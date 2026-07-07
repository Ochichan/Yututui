//! The minimal Spotify Web API client. One central [`SpotifyClient::request_json`] path
//! carries all policy: self-pacing, proactive + reactive token refresh (rotation-safe),
//! 429 handling with a per-job wait budget, dev-mode 403 detection, body caps, and
//! secret-safe errors. Exclusive `&mut self` ownership (one task per client) is what
//! makes refresh single-flight — documented invariant, no mutex.

use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use super::auth::{self, SpotifyToken};
use super::models::{
    Paging, RawPlaylist, RawTrackItem, SpotifyPlaylist, SpotifyTrack, SpotifyUser, simplify,
};
use super::{API_BASE, BODY_MAX};
use crate::util::http::json_limited;
use crate::util::sanitize::sanitize_error_text;

/// Hard ceiling on paginated fetches. Far above any real Spotify library (20k pages × 50–100
/// items = 1–2M entries), it exists only to bound a buggy or hostile `next` link that would
/// otherwise loop forever and grow the result Vec without limit.
const MAX_PAGES: u32 = 20_000;

/// Minimum interval between calls (gentle, well under any documented limit).
const PACE: Duration = Duration::from_millis(250);
/// A single 429 wait is capped here (a bigger Retry-After aborts resumably)…
const RATE_WAIT_MAX: Duration = Duration::from_secs(60);
/// …and the cumulative 429 waiting a single client (= job) will absorb.
const RATE_BUDGET: Duration = Duration::from_secs(300);
/// Transient network/5xx retries (short — jobs checkpoint and resume anyway).
const TRANSIENT_RETRIES: u32 = 2;

#[derive(Debug, Clone)]
pub enum SpotifyError {
    /// 401 after a refresh attempt, or the refresh itself failed: reconnect.
    Auth(String),
    /// 403 from a Development Mode app: the account isn't on the app's allowlist.
    NotAllowlisted,
    /// The per-job 429 budget ran out; the job should checkpoint and abort resumably.
    RateLimited,
    Network(String),
    Api {
        status: u16,
        message: String,
    },
    Decode(String),
}

impl std::fmt::Display for SpotifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpotifyError::Auth(m) => write!(
                f,
                "Spotify session expired ({m}) — reconnect with `ytt auth spotify` or Settings › Accounts"
            ),
            SpotifyError::NotAllowlisted => write!(
                f,
                "Spotify rejected the request (403). Your Spotify app is in Development Mode: open \
                 the Developer Dashboard → your app → User Management and add your own Spotify \
                 account, and double-check the Client ID"
            ),
            SpotifyError::RateLimited => {
                write!(
                    f,
                    "Spotify rate limit exhausted — resume the job in a few minutes"
                )
            }
            SpotifyError::Network(m) => write!(f, "network: {m}"),
            SpotifyError::Api { status, message } => write!(f, "Spotify HTTP {status}: {message}"),
            SpotifyError::Decode(m) => write!(f, "unexpected Spotify response: {m}"),
        }
    }
}

impl std::error::Error for SpotifyError {}

// No `Debug`: holds the bearer token.
pub struct SpotifyClient {
    http: reqwest::Client,
    token: SpotifyToken,
    user: Option<SpotifyUser>,
    last_call: Option<Instant>,
    rate_waited: Duration,
}

impl SpotifyClient {
    /// Build from the saved token. `client_id_hint` (the config value) guards against a
    /// token minted by a different app registration.
    pub fn from_saved(client_id_hint: Option<&str>) -> Result<Self, SpotifyError> {
        let token = SpotifyToken::load()
            .ok_or_else(|| SpotifyError::Auth("not connected (no saved token)".to_owned()))?;
        if let Some(hint) = client_id_hint.map(str::trim).filter(|h| !h.is_empty())
            && !token.client_id.is_empty()
            && token.client_id != hint
        {
            return Err(SpotifyError::Auth(
                "saved token belongs to a different Client ID".to_owned(),
            ));
        }
        Ok(Self::with_token(token))
    }

    pub fn with_token(token: SpotifyToken) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(format!("ytm-tui/{}", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            token,
            user: None,
            last_call: None,
            rate_waited: Duration::ZERO,
        }
    }

    /// Fresh 429 budget for a new job on a long-lived client (the TUI actor).
    pub fn reset_rate_budget(&mut self) {
        self.rate_waited = Duration::ZERO;
    }

    // Endpoints -----------------------------------------------------------------

    pub async fn me(&mut self) -> Result<SpotifyUser, SpotifyError> {
        if let Some(user) = &self.user {
            return Ok(user.clone());
        }
        let user: SpotifyUser = self
            .request_json(reqwest::Method::GET, format!("{API_BASE}/me"), None)
            .await?;
        self.user = Some(user.clone());
        Ok(user)
    }

    pub async fn my_playlists(&mut self) -> Result<Vec<SpotifyPlaylist>, SpotifyError> {
        let mut out = Vec::new();
        let mut url = format!("{API_BASE}/me/playlists?limit=50");
        let mut pages = 0u32;
        loop {
            pages += 1;
            let page: Paging<RawPlaylist> =
                self.request_json(reqwest::Method::GET, url, None).await?;
            out.extend(page.items.into_iter().map(SpotifyPlaylist::from));
            match page.next {
                Some(next) if pages < MAX_PAGES => url = next,
                Some(_) => {
                    tracing::warn!(
                        pages,
                        "Spotify my-playlists pagination hit the page ceiling; result truncated"
                    );
                    return Ok(out);
                }
                None => return Ok(out),
            }
        }
    }

    pub async fn playlist_meta(&mut self, id: &str) -> Result<SpotifyPlaylist, SpotifyError> {
        // Both generations of the contents-ref name are requested; whichever the server
        // knows survives the fields filter (unknown names are silently omitted).
        let raw: RawPlaylist = self
            .request_json(
                reqwest::Method::GET,
                format!(
                    "{API_BASE}/playlists/{id}?fields=id,name,collaborative,owner(display_name,id),items(total),tracks(total)"
                ),
                None,
            )
            .await?;
        Ok(raw.into())
    }

    /// All of a playlist's playable tracks, in order; `on_page(done, total)` reports
    /// pagination progress. Episodes/local/null items are silently skipped (the caller
    /// can compare the returned length with the playlist total to report them).
    ///
    /// Uses the post-March-2026 `/items` endpoint — the old `/tracks` one is 403 for
    /// Development Mode apps.
    pub async fn playlist_tracks(
        &mut self,
        id: &str,
        on_page: &mut (dyn FnMut(u32, u32) + Send),
    ) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        let fields = "items(is_local,item(type,uri,name,duration_ms,explicit,is_local,artists(name),album(name),external_ids(isrc))),total,next";
        let mut url = format!("{API_BASE}/playlists/{id}/items?limit=100&fields={fields}");
        let mut out = Vec::new();
        let mut seen = 0u32;
        let mut pages = 0u32;
        loop {
            pages += 1;
            let page: Paging<RawTrackItem> =
                self.request_json(reqwest::Method::GET, url, None).await?;
            seen += page.items.len() as u32;
            out.extend(page.items.iter().filter_map(simplify));
            on_page(seen.min(page.total.max(seen)), page.total);
            match page.next {
                Some(next) if pages < MAX_PAGES => url = next,
                Some(_) => {
                    tracing::warn!(
                        pages,
                        "Spotify playlist pagination hit the page ceiling; result truncated"
                    );
                    return Ok(out);
                }
                None => return Ok(out),
            }
        }
    }

    /// Liked songs. Spotify returns newest-first; callers reverse when they need
    /// oldest-first (like-order-preserving import).
    pub async fn liked_tracks(
        &mut self,
        on_page: &mut (dyn FnMut(u32, u32) + Send),
    ) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        let mut url = format!("{API_BASE}/me/tracks?limit=50");
        let mut out = Vec::new();
        let mut seen = 0u32;
        let mut pages = 0u32;
        loop {
            pages += 1;
            let page: Paging<RawTrackItem> =
                self.request_json(reqwest::Method::GET, url, None).await?;
            seen += page.items.len() as u32;
            out.extend(page.items.iter().filter_map(simplify));
            on_page(seen.min(page.total.max(seen)), page.total);
            match page.next {
                Some(next) if pages < MAX_PAGES => url = next,
                Some(_) => {
                    tracing::warn!(
                        pages,
                        "Spotify liked-tracks pagination hit the page ceiling; result truncated"
                    );
                    return Ok(out);
                }
                None => return Ok(out),
            }
        }
    }

    /// Create a private playlist; returns its id.
    pub async fn create_playlist(
        &mut self,
        name: &str,
        description: &str,
    ) -> Result<String, SpotifyError> {
        let uid = self.me().await?.id;
        let body: serde_json::Value = self
            .request_json(
                reqwest::Method::POST,
                format!("{API_BASE}/users/{uid}/playlists"),
                Some(serde_json::json!({
                    "name": name, "description": description, "public": false,
                })),
            )
            .await?;
        body.get("id")
            .and_then(|i| i.as_str())
            .map(str::to_owned)
            .ok_or_else(|| SpotifyError::Decode("create playlist: no id".to_owned()))
    }

    /// Append ≤100 URIs (caller pre-chunks; order within a call is preserved).
    /// Post-migration endpoint; works on existing own playlists even in Development Mode.
    pub async fn add_tracks(
        &mut self,
        playlist_id: &str,
        uris: &[String],
    ) -> Result<(), SpotifyError> {
        debug_assert!(uris.len() <= 100);
        let _: serde_json::Value = self
            .request_json(
                reqwest::Method::POST,
                format!("{API_BASE}/playlists/{playlist_id}/items"),
                Some(serde_json::json!({ "uris": uris })),
            )
            .await?;
        Ok(())
    }

    /// Track search for the YTM→Spotify direction, top 10.
    pub async fn search_track(
        &mut self,
        query: &str,
        market: Option<&str>,
    ) -> Result<Vec<SpotifyTrack>, SpotifyError> {
        let mut url = reqwest::Url::parse(&format!("{API_BASE}/search"))
            .map_err(|e| SpotifyError::Decode(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("type", "track")
            .append_pair("limit", "10")
            .append_pair("q", query);
        if let Some(market) = market.map(str::trim).filter(|m| !m.is_empty()) {
            url.query_pairs_mut().append_pair("market", market);
        }
        let body: serde_json::Value = self
            .request_json(reqwest::Method::GET, url.to_string(), None)
            .await?;
        let items: Vec<RawTrackItem> = body
            .pointer("/tracks/items")
            .and_then(|i| i.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|t| RawTrackItem {
                        track: Some(t.clone()),
                        ..RawTrackItem::default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(items.iter().filter_map(simplify).collect())
    }

    // The one central request path --------------------------------------------------

    async fn request_json<T: DeserializeOwned>(
        &mut self,
        method: reqwest::Method,
        url: String,
        body: Option<serde_json::Value>,
    ) -> Result<T, SpotifyError> {
        let mut refreshed = false;
        let mut transient_left = TRANSIENT_RETRIES;
        loop {
            // Self-pace every call.
            if let Some(last) = self.last_call {
                let since = last.elapsed();
                if since < PACE {
                    tokio::time::sleep(PACE - since).await;
                }
            }
            // Proactive refresh keeps the reactive 401 path a rarity.
            if self.token.is_expired() && !refreshed {
                self.refresh().await?;
                refreshed = true;
            }
            self.last_call = Some(Instant::now());

            let mut bearer = reqwest::header::HeaderValue::from_str(&format!(
                "Bearer {}",
                self.token.access_token
            ))
            .map_err(|_| SpotifyError::Auth("token has invalid characters".to_owned()))?;
            bearer.set_sensitive(true);
            let mut req = self
                .http
                .request(method.clone(), &url)
                .header(reqwest::header::AUTHORIZATION, bearer);
            if let Some(body) = &body {
                req = req.json(body);
            }
            let resp = match req.send().await {
                Ok(resp) => resp,
                Err(_) if transient_left > 0 => {
                    transient_left -= 1;
                    tokio::time::sleep(Duration::from_millis(1200)).await;
                    continue;
                }
                Err(e) => {
                    return Err(SpotifyError::Network(sanitize_error_text(format!("{e}"))));
                }
            };
            let status = resp.status();

            if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                self.refresh().await?;
                refreshed = true;
                continue;
            }
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map_or(Duration::from_secs(2), Duration::from_secs);
                let wait = wait.min(RATE_WAIT_MAX).max(Duration::from_secs(1));
                if self.rate_waited + wait > RATE_BUDGET {
                    return Err(SpotifyError::RateLimited);
                }
                self.rate_waited += wait;
                tracing::info!(secs = wait.as_secs(), "spotify 429; waiting");
                tokio::time::sleep(wait).await;
                continue;
            }
            if status.is_server_error() && transient_left > 0 {
                transient_left -= 1;
                tokio::time::sleep(Duration::from_millis(1200)).await;
                continue;
            }
            if !status.is_success() {
                let body: serde_json::Value =
                    json_limited(resp, BODY_MAX).await.unwrap_or_default();
                let message = body
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown")
                    .to_owned();
                return Err(match status.as_u16() {
                    401 => SpotifyError::Auth(message),
                    // Dev-mode allowlist rejection is *the* predictable first-run failure;
                    // the error text does the support work.
                    403 => SpotifyError::NotAllowlisted,
                    code => SpotifyError::Api {
                        status: code,
                        message: sanitize_error_text(message),
                    },
                });
            }
            // 201/200 with body; DELETE/PUT snapshots also return small JSON.
            return json_limited(resp, BODY_MAX)
                .await
                .map_err(|e| SpotifyError::Decode(sanitize_error_text(format!("{e:#}"))));
        }
    }

    /// Rotation-safe refresh: `auth::refresh` persists the successor before returning.
    async fn refresh(&mut self) -> Result<(), SpotifyError> {
        match auth::refresh(&self.http, &self.token).await {
            Ok(fresh) => {
                self.token = fresh;
                Ok(())
            }
            Err(e) => Err(SpotifyError::Auth(sanitize_error_text(format!("{e:#}")))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn token() -> SpotifyToken {
        SpotifyToken {
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            expires_at: crate::signals::unix_now() + 3600,
            scopes: "scope-a scope-b".to_owned(),
            client_id: "client-id".to_owned(),
        }
    }

    #[test]
    fn spotify_error_display_gives_actionable_messages() {
        assert!(
            SpotifyError::Auth("expired".to_owned())
                .to_string()
                .contains("ytt auth spotify")
        );
        assert!(
            SpotifyError::NotAllowlisted
                .to_string()
                .contains("User Management")
        );
        assert!(
            SpotifyError::RateLimited
                .to_string()
                .contains("resume the job")
        );
        assert_eq!(
            SpotifyError::Network("timeout".to_owned()).to_string(),
            "network: timeout"
        );
        assert_eq!(
            SpotifyError::Api {
                status: 418,
                message: "teapot".to_owned(),
            }
            .to_string(),
            "Spotify HTTP 418: teapot"
        );
        assert_eq!(
            SpotifyError::Decode("bad json".to_owned()).to_string(),
            "unexpected Spotify response: bad json"
        );
    }

    #[test]
    fn client_with_token_starts_without_cached_user_or_rate_debt() {
        let client = SpotifyClient::with_token(token());

        assert!(client.user.is_none());
        assert!(client.last_call.is_none());
        assert_eq!(client.rate_waited, Duration::ZERO);
        assert_eq!(client.token.client_id, "client-id");
    }

    #[test]
    fn reset_rate_budget_clears_cumulative_wait_without_touching_token() {
        let mut client = SpotifyClient::with_token(token());
        client.rate_waited = Duration::from_secs(42);

        client.reset_rate_budget();

        assert_eq!(client.rate_waited, Duration::ZERO);
        assert_eq!(client.token.refresh_token, "refresh");
    }

    async fn serve_spotify_once(
        status: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind spotify test server");
        let addr = listener.local_addr().expect("spotify test server address");
        let status = status.to_owned();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        let body = body.to_owned();
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept spotify request");
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = stream.read(&mut buf).await.expect("read spotify request");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                if let Some(head_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..head_end + 4]);
                    let content_len = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + content_len {
                        break;
                    }
                }
            }

            let mut response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            response.push_str(&body);
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write spotify response");
            let _ = stream.shutdown().await;
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{addr}/v1/test"), handle)
    }

    #[tokio::test]
    async fn request_json_sends_bearer_json_and_decodes_success() {
        let (url, request) = serve_spotify_once(
            "201 Created",
            &[("Content-Type", "application/json")],
            r#"{"ok":true,"id":"new-playlist"}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());

        let body: serde_json::Value = client
            .request_json(
                reqwest::Method::POST,
                url,
                Some(serde_json::json!({"name": "Roadtrip"})),
            )
            .await
            .expect("successful response decodes");

        assert_eq!(body["id"], "new-playlist");
        assert!(client.last_call.is_some());
        let request = request.await.expect("captured request");
        assert!(request.starts_with("POST /v1/test HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer access")
        );
        assert!(request.contains(r#""name":"Roadtrip""#));
    }

    #[tokio::test]
    async fn request_json_maps_forbidden_api_decode_and_rate_errors() {
        let (url, request) = serve_spotify_once(
            "403 Forbidden",
            &[("Content-Type", "application/json")],
            r#"{"error":{"message":"dev mode"}}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());
        let err = client
            .request_json::<serde_json::Value>(reqwest::Method::GET, url, None)
            .await
            .expect_err("403 maps to not-allowlisted");
        assert!(matches!(err, SpotifyError::NotAllowlisted));
        assert!(request.await.expect("captured 403 request").contains("GET"));

        let (url, _) = serve_spotify_once(
            "418 I'm a teapot",
            &[("Content-Type", "application/json")],
            r#"{"error":{"message":"short and stout"}}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());
        let err = client
            .request_json::<serde_json::Value>(reqwest::Method::GET, url, None)
            .await
            .expect_err("non-special HTTP error maps to Api");
        assert!(matches!(
            err,
            SpotifyError::Api {
                status: 418,
                ref message
            } if message == "short and stout"
        ));

        let (url, _) = serve_spotify_once(
            "200 OK",
            &[("Content-Type", "application/json")],
            "not-json",
        )
        .await;
        let mut client = SpotifyClient::with_token(token());
        let err = client
            .request_json::<serde_json::Value>(reqwest::Method::GET, url, None)
            .await
            .expect_err("bad JSON maps to Decode");
        assert!(matches!(err, SpotifyError::Decode(_)));

        let (url, _) = serve_spotify_once(
            "429 Too Many Requests",
            &[("Content-Type", "application/json"), ("Retry-After", "1")],
            r#"{"error":{"message":"slow down"}}"#,
        )
        .await;
        let mut client = SpotifyClient::with_token(token());
        client.rate_waited = RATE_BUDGET;
        let err = client
            .request_json::<serde_json::Value>(reqwest::Method::GET, url, None)
            .await
            .expect_err("exhausted rate budget returns immediately");
        assert!(matches!(err, SpotifyError::RateLimited));
    }
}
