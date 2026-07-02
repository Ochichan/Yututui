//! ListenBrainz client: `submit-listens` (playing_now / import) + `validate-token`.
//!
//! Token auth only — the user copies their token from listenbrainz.org/settings. Love
//! sync is deliberately not implemented: LB feedback wants a MusicBrainz recording
//! MBID/MSID, which YouTube Music metadata doesn't carry.

use std::time::Duration;

use serde_json::json;

use super::service::{ScrobbleError, ScrobbleService, ScrobbleTrack, ServiceKind};
use crate::util::http::json_limited;
use crate::util::sanitize::sanitize_error_text;

/// Public instance; `config.scrobble.listenbrainz.api_url` overrides for self-hosted setups.
pub const DEFAULT_API_URL: &str = "https://api.listenbrainz.org";
const BODY_MAX: usize = 512 * 1024;

/// One-shot token check for the auth CLI / settings validation. `Ok(Some(name))` when
/// the instance reports the account name.
pub async fn validate_token(api_url: &str, token: &str) -> Result<Option<String>, ScrobbleError> {
    let http = reqwest::Client::builder()
        .user_agent(format!("ytm-tui/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let resp = http
        .get(format!(
            "{}/1/validate-token",
            api_url.trim_end_matches('/')
        ))
        .header(reqwest::header::AUTHORIZATION, token_header(token)?)
        .send()
        .await
        .map_err(|e| ScrobbleError::Network(sanitize_error_text(format!("{e}"))))?;
    let status = resp.status();
    let body: serde_json::Value = json_limited(resp, BODY_MAX)
        .await
        .map_err(|e| ScrobbleError::Network(sanitize_error_text(format!("{e:#}"))))?;
    if !status.is_success() {
        return Err(classify_status(status.as_u16(), None, &body));
    }
    if body.get("valid").and_then(|v| v.as_bool()) != Some(true) {
        return Err(ScrobbleError::Auth("token rejected".to_owned()));
    }
    Ok(body
        .get("user_name")
        .and_then(|n| n.as_str())
        .map(str::to_owned))
}

fn token_header(token: &str) -> Result<reqwest::header::HeaderValue, ScrobbleError> {
    let mut value = reqwest::header::HeaderValue::from_str(&format!("Token {token}"))
        .map_err(|_| ScrobbleError::Auth("token contains invalid characters".to_owned()))?;
    value.set_sensitive(true);
    Ok(value)
}

fn classify_status(
    status: u16,
    retry_after: Option<Duration>,
    body: &serde_json::Value,
) -> ScrobbleError {
    let message = body
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or("unknown")
        .to_owned();
    match status {
        401 | 403 => ScrobbleError::Auth(format!("listenbrainz {status}: {message}")),
        429 => ScrobbleError::RateLimited(retry_after),
        400..=499 => ScrobbleError::Invalid(format!("listenbrainz {status}: {message}")),
        _ => ScrobbleError::Network(format!("listenbrainz {status}: {message}")),
    }
}

// No `Debug`: holds the user token.
#[derive(Clone)]
pub struct ListenBrainzClient {
    http: reqwest::Client,
    token: String,
    api_url: String,
}

impl ListenBrainzClient {
    pub fn new(http: reqwest::Client, token: String, api_url: String) -> Self {
        Self {
            http,
            token,
            api_url: api_url.trim_end_matches('/').to_owned(),
        }
    }

    fn track_metadata(track: &ScrobbleTrack) -> serde_json::Value {
        let mut additional = json!({
            "media_player": "ytm-tui",
            "submission_client": "ytm-tui",
            "submission_client_version": env!("CARGO_PKG_VERSION"),
        });
        if let Some(d) = track.duration_secs {
            additional["duration_ms"] = json!(u64::from(d) * 1000);
        }
        if let Some(url) = &track.origin_url {
            additional["origin_url"] = json!(url);
        }
        let mut meta = json!({
            "artist_name": track.artist,
            "track_name": track.title,
            "additional_info": additional,
        });
        if let Some(album) = &track.album {
            meta["release_name"] = json!(album);
        }
        meta
    }

    async fn submit(&self, payload: serde_json::Value) -> Result<(), ScrobbleError> {
        let resp = self
            .http
            .post(format!("{}/1/submit-listens", self.api_url))
            .header(reqwest::header::AUTHORIZATION, token_header(&self.token)?)
            .json(&payload)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| ScrobbleError::Network(sanitize_error_text(format!("{e}"))))?;
        let status = resp.status();
        // LB rate-limit hints live in X-RateLimit-Reset-In (seconds) or Retry-After.
        let retry_after = ["retry-after", "x-ratelimit-reset-in"]
            .iter()
            .find_map(|h| resp.headers().get(*h))
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        if status.is_success() {
            return Ok(());
        }
        let body: serde_json::Value = json_limited(resp, BODY_MAX).await.unwrap_or_default();
        Err(classify_status(status.as_u16(), retry_after, &body))
    }
}

impl ScrobbleService for ListenBrainzClient {
    fn kind(&self) -> ServiceKind {
        ServiceKind::ListenBrainz
    }

    async fn now_playing(&self, track: &ScrobbleTrack) -> Result<(), ScrobbleError> {
        self.submit(json!({
            "listen_type": "playing_now",
            "payload": [ { "track_metadata": Self::track_metadata(track) } ],
        }))
        .await
    }

    async fn scrobble_batch(&self, tracks: &[ScrobbleTrack]) -> Result<(), ScrobbleError> {
        if tracks.is_empty() {
            return Ok(());
        }
        let payload: Vec<serde_json::Value> = tracks
            .iter()
            .map(|t| {
                json!({
                    "listened_at": t.started_unix,
                    "track_metadata": Self::track_metadata(t),
                })
            })
            .collect();
        self.submit(json!({ "listen_type": "import", "payload": payload }))
            .await
    }

    /// LB feedback needs a MusicBrainz recording id we don't have — documented no-op.
    async fn love(&self, _artist: &str, _title: &str, _love: bool) -> Result<(), ScrobbleError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_payload_shape() {
        let track = ScrobbleTrack {
            key: "abc".to_owned(),
            artist: "IU".to_owned(),
            title: "Blueming".to_owned(),
            album: Some("Love poem".to_owned()),
            duration_secs: Some(217),
            origin_url: Some("https://music.youtube.com/watch?v=abc".to_owned()),
            started_unix: 1_751_400_000,
        };
        let meta = ListenBrainzClient::track_metadata(&track);
        assert_eq!(meta["artist_name"], "IU");
        assert_eq!(meta["release_name"], "Love poem");
        assert_eq!(meta["additional_info"]["duration_ms"], 217_000);
        assert_eq!(meta["additional_info"]["media_player"], "ytm-tui");
    }

    #[test]
    fn statuses_classify() {
        let empty = serde_json::Value::Null;
        assert!(matches!(
            classify_status(401, None, &empty),
            ScrobbleError::Auth(_)
        ));
        assert!(matches!(
            classify_status(429, Some(Duration::from_secs(3)), &empty),
            ScrobbleError::RateLimited(Some(_))
        ));
        assert!(matches!(
            classify_status(400, None, &empty),
            ScrobbleError::Invalid(_)
        ));
        assert!(matches!(
            classify_status(503, None, &empty),
            ScrobbleError::Network(_)
        ));
    }
}
