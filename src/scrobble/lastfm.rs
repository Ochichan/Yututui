//! Last.fm web-service client (scrobble API 2.0): desktop auth flow,
//! `track.updateNowPlaying`, batched `track.scrobble` (≤50), `track.love`/`unlove`.
//!
//! Every call is a signed POST to the one endpoint: parameters are sorted by name,
//! concatenated `name``value`, the shared secret appended, and the md5 hex of that UTF-8
//! string travels as `api_sig` (`format=json` is added *after* signing — the spec excludes
//! `format`/`callback` from the signature).

use std::collections::BTreeMap;
use std::time::Duration;

use md5::{Digest, Md5};

use super::service::{ScrobbleError, ScrobbleService, ScrobbleTrack, ServiceKind};
use crate::util::http::json_limited;
use crate::util::sanitize::sanitize_error_text;

const API_URL: &str = "https://ws.audioscrobbler.com/2.0/";
const BODY_MAX: usize = 512 * 1024;
/// Last.fm caps `track.scrobble` batches at 50 tracks.
pub const SCROBBLE_BATCH_MAX: usize = 50;

/// Embedded application credentials, registered once by the project at
/// <https://www.last.fm/api/account/create>. Shipping them in the source is the normal
/// open-source scrobbler practice (rescrobbled, mpdscribble, …) — the per-user secret is
/// the *session key*, never these. Self-builders can override both via
/// `config.scrobble.lastfm.api_key`/`api_secret` without touching the source.
const EMBEDDED_API_KEY: &str = "1a3d611fb758ebb5e2aafb8116edb1b8";
const EMBEDDED_API_SECRET: &str = "d59da78c05b8636c71895820bbd14a8d";

/// Resolve the application credentials: config overrides win, embedded values otherwise.
/// Returns empty strings when neither is configured — callers treat that as "Last.fm
/// unavailable" rather than an error.
pub fn app_credentials(
    key_override: Option<&str>,
    secret_override: Option<&str>,
) -> (String, String) {
    let key = key_override
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .unwrap_or(EMBEDDED_API_KEY);
    let secret = secret_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(EMBEDDED_API_SECRET);
    (key.to_owned(), secret.to_owned())
}

/// The signature input: parameters sorted by name (BTreeMap order), `name``value` pairs
/// concatenated, secret appended, md5 hex over the UTF-8 bytes. Pure — golden-tested.
fn api_sig(params: &BTreeMap<String, String>, secret: &str) -> String {
    let mut src = String::new();
    for (k, v) in params {
        src.push_str(k);
        src.push_str(v);
    }
    src.push_str(secret);
    let digest = Md5::digest(src.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// What `auth.getSession` said while the user is off approving in the browser.
pub enum SessionPoll {
    /// Error 14: token not yet authorized — keep polling.
    Pending,
    /// Approved: `(session_key, username)`.
    Granted { key: String, username: String },
}

// No `Debug`: holds the shared secret and the user's session key.
#[derive(Clone)]
pub struct LastfmClient {
    http: reqwest::Client,
    api_key: String,
    api_secret: String,
    session_key: Option<String>,
}

impl LastfmClient {
    /// `http` is shared with the rest of the actor (reqwest clients are cheap handles).
    pub fn new(
        http: reqwest::Client,
        api_key: String,
        api_secret: String,
        session_key: Option<String>,
    ) -> Self {
        Self {
            http,
            api_key,
            api_secret,
            session_key: session_key.filter(|k| !k.is_empty()),
        }
    }

    /// Base parameter set for `method`, pre-authenticated when a session exists.
    fn params(&self, method: &str) -> BTreeMap<String, String> {
        let mut p = BTreeMap::new();
        p.insert("method".to_owned(), method.to_owned());
        p.insert("api_key".to_owned(), self.api_key.clone());
        if let Some(sk) = &self.session_key {
            p.insert("sk".to_owned(), sk.clone());
        }
        p
    }

    /// Sign and POST; decode the JSON reply. Last.fm signals errors as
    /// `{"error": <code>, "message": …}` with assorted HTTP statuses — classify by code
    /// first, transport problems as `Network`.
    async fn call(
        &self,
        mut params: BTreeMap<String, String>,
    ) -> Result<serde_json::Value, ScrobbleError> {
        params.insert("api_sig".to_owned(), api_sig(&params, &self.api_secret));
        params.insert("format".to_owned(), "json".to_owned()); // excluded from the signature
        let resp = self
            .http
            .post(API_URL)
            .form(&params)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| ScrobbleError::Network(sanitize_error_text(format!("{e}"))))?;
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let status = resp.status();
        let body: serde_json::Value = json_limited(resp, BODY_MAX)
            .await
            .map_err(|e| ScrobbleError::Network(sanitize_error_text(format!("{e:#}"))))?;
        if let Some(code) = body.get("error").and_then(|e| e.as_u64()) {
            let message = body
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_owned();
            return Err(classify(code, message, retry_after));
        }
        if !status.is_success() {
            return Err(ScrobbleError::Network(format!("HTTP {status}")));
        }
        Ok(body)
    }

    // Auth flow ---------------------------------------------------------------

    /// Step 1: an unauthorized request token (valid 60 minutes, single use).
    pub async fn get_token(&self) -> Result<String, ScrobbleError> {
        let body = self.call(self.unauthed_params("auth.getToken")).await?;
        body.get("token")
            .and_then(|t| t.as_str())
            .map(str::to_owned)
            .ok_or_else(|| ScrobbleError::Invalid("auth.getToken: no token".to_owned()))
    }

    /// Step 2: where the user approves the app.
    pub fn auth_url(&self, token: &str) -> String {
        format!(
            "https://www.last.fm/api/auth/?api_key={}&token={token}",
            self.api_key
        )
    }

    /// Step 3: poll until the browser approval lands. Error 14 = still pending.
    pub async fn get_session(&self, token: &str) -> Result<SessionPoll, ScrobbleError> {
        let mut params = self.unauthed_params("auth.getSession");
        params.insert("token".to_owned(), token.to_owned());
        match self.call(params).await {
            Ok(body) => {
                let session = body
                    .get("session")
                    .ok_or_else(|| ScrobbleError::Invalid("auth.getSession: no session".into()))?;
                let key = session
                    .get("key")
                    .and_then(|k| k.as_str())
                    .unwrap_or_default()
                    .to_owned();
                if key.is_empty() {
                    return Err(ScrobbleError::Invalid("auth.getSession: empty key".into()));
                }
                let username = session
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_owned();
                Ok(SessionPoll::Granted { key, username })
            }
            Err(ScrobbleError::Invalid(m)) if m.starts_with("lastfm 14:") => {
                Ok(SessionPoll::Pending)
            }
            Err(e) => Err(e),
        }
    }

    /// Auth-flow calls run before a session exists — never send a stale `sk`.
    fn unauthed_params(&self, method: &str) -> BTreeMap<String, String> {
        let mut p = BTreeMap::new();
        p.insert("method".to_owned(), method.to_owned());
        p.insert("api_key".to_owned(), self.api_key.clone());
        p
    }
}

/// Map a Last.fm error code onto a retry class. Codes per the API docs: 4/9/10/26 are
/// credential problems (latch, don't retry), 29 is the rate limit, 8/11/16 are their
/// "try again later" family, everything else is a content/programming rejection.
fn classify(code: u64, message: String, retry_after: Option<Duration>) -> ScrobbleError {
    let text = format!("lastfm {code}: {message}");
    match code {
        4 | 9 | 10 | 26 => ScrobbleError::Auth(text),
        29 => ScrobbleError::RateLimited(retry_after),
        8 | 11 | 16 => ScrobbleError::Network(text),
        _ => ScrobbleError::Invalid(text),
    }
}

impl ScrobbleService for LastfmClient {
    fn kind(&self) -> ServiceKind {
        ServiceKind::Lastfm
    }

    async fn now_playing(&self, track: &ScrobbleTrack) -> Result<(), ScrobbleError> {
        let mut p = self.params("track.updateNowPlaying");
        p.insert("artist".to_owned(), track.artist.clone());
        p.insert("track".to_owned(), track.title.clone());
        if let Some(album) = &track.album {
            p.insert("album".to_owned(), album.clone());
        }
        if let Some(d) = track.duration_secs {
            p.insert("duration".to_owned(), d.to_string());
        }
        self.call(p).await.map(|_| ())
    }

    async fn scrobble_batch(&self, tracks: &[ScrobbleTrack]) -> Result<(), ScrobbleError> {
        if tracks.is_empty() {
            return Ok(());
        }
        let mut p = self.params("track.scrobble");
        for (i, t) in tracks.iter().take(SCROBBLE_BATCH_MAX).enumerate() {
            p.insert(format!("artist[{i}]"), t.artist.clone());
            p.insert(format!("track[{i}]"), t.title.clone());
            p.insert(format!("timestamp[{i}]"), t.started_unix.to_string());
            if let Some(album) = &t.album {
                p.insert(format!("album[{i}]"), album.clone());
            }
            if let Some(d) = t.duration_secs {
                p.insert(format!("duration[{i}]"), d.to_string());
            }
            // We choose the track (it's a music player), per the scrobble spec.
            p.insert(format!("chosenByUser[{i}]"), "1".to_owned());
        }
        let body = self.call(p).await?;
        // Per-track "ignored" statuses are content rejections — delivered as far as
        // retry policy goes, but worth a log line.
        if let Some(ignored) = body
            .pointer("/scrobbles/@attr/ignored")
            .and_then(|v| v.as_u64())
            .filter(|n| *n > 0)
        {
            tracing::info!(ignored, "last.fm ignored some scrobbles (content rules)");
        }
        Ok(())
    }

    async fn love(&self, artist: &str, title: &str, love: bool) -> Result<(), ScrobbleError> {
        let mut p = self.params(if love { "track.love" } else { "track.unlove" });
        p.insert("artist".to_owned(), artist.to_owned());
        p.insert("track".to_owned(), title.to_owned());
        self.call(p).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vectors (precomputed with the `md5` CLI over the exact signature input) —
    /// this is the wire format, so it must never drift.
    #[test]
    fn api_sig_matches_known_digests() {
        let mut p = BTreeMap::new();
        p.insert("method".to_owned(), "auth.getToken".to_owned());
        p.insert("api_key".to_owned(), "abc123".to_owned());
        // sorted concat: "api_keyabc123methodauth.getToken" + "mysecret"
        assert_eq!(api_sig(&p, "mysecret"), "bbf45b6c683f3b074ed7e9052b099645");

        // UTF-8 params sign over their UTF-8 bytes (K-pop metadata is the common case).
        let mut p = BTreeMap::new();
        p.insert("method".to_owned(), "track.love".to_owned());
        p.insert("api_key".to_owned(), "k".to_owned());
        p.insert("sk".to_owned(), "sess".to_owned());
        p.insert("artist".to_owned(), "아이유".to_owned());
        p.insert("track".to_owned(), "Love wins all".to_owned());
        assert_eq!(api_sig(&p, "s"), "b65cafbccf11e2d236341fbe9dff65da");
    }

    #[test]
    fn error_codes_classify_into_retry_classes() {
        assert!(matches!(
            classify(9, "Invalid session key".into(), None),
            ScrobbleError::Auth(_)
        ));
        assert!(matches!(
            classify(
                29,
                "Rate limit exceeded".into(),
                Some(Duration::from_secs(5))
            ),
            ScrobbleError::RateLimited(Some(_))
        ));
        assert!(matches!(
            classify(16, "temporarily unavailable".into(), None),
            ScrobbleError::Network(_)
        ));
        assert!(matches!(
            classify(6, "Invalid parameters".into(), None),
            ScrobbleError::Invalid(_)
        ));
    }

    #[test]
    fn embedded_credentials_can_be_overridden() {
        let (k, s) = app_credentials(Some("  key  "), Some("secret"));
        assert_eq!((k.as_str(), s.as_str()), ("key", "secret"));
        let (k, s) = app_credentials(None, Some("   "));
        assert_eq!(k, EMBEDDED_API_KEY);
        assert_eq!(s, EMBEDDED_API_SECRET);
    }
}
