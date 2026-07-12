//! The service abstraction the actor flushes through: Last.fm and ListenBrainz both
//! implement [`ScrobbleService`], so queue draining, backoff, and health tracking are
//! written once. Async-fn-in-trait is fine here — the actor holds concrete clients
//! (static dispatch), never `dyn`.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Which backend a queue entry still owes a delivery to. Serialized into the JSONL queue
/// (`"lastfm"` / `"listenbrainz"`), so the names are a stable on-disk format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Lastfm,
    ListenBrainz,
}

impl ServiceKind {
    pub fn label(self) -> &'static str {
        match self {
            ServiceKind::Lastfm => "Last.fm",
            ServiceKind::ListenBrainz => "ListenBrainz",
        }
    }
}

/// One listen's metadata, as submitted to the services. No secrets — `Debug` is fine and
/// useful in queue logs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScrobbleTrack {
    /// Stable track identity (the app's `video_id`); keys queue entries and dedupe.
    pub key: String,
    pub artist: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Whole seconds (Last.fm `duration`; ListenBrainz `duration_ms` derives from it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    /// Shareable track URL (ListenBrainz `origin_url`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
    /// Listen START time, UTC unix seconds — the scrobble timestamp per the 2.0 rules.
    pub started_unix: i64,
}

/// Delivery failure classes; they drive retry policy, not user-facing text (which the
/// actor builds via `t!` when it surfaces anything at all).
#[derive(Debug, Clone)]
pub enum ScrobbleError {
    /// Bad/revoked session key or token. Latches the service unhealthy until reconfigured;
    /// never retried automatically.
    Auth(String),
    /// 429-class throttling; retry after the hint (or the default backoff when absent).
    RateLimited(Option<Duration>),
    /// Transport / 5xx / timeout. Retried with exponential backoff, indefinitely.
    Network(String),
    /// The service accepted the request but rejected the content (Last.fm "ignored"
    /// statuses, LB 400). Retrying is pointless — treated as delivered.
    Invalid(String),
}

impl std::fmt::Display for ScrobbleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScrobbleError::Auth(m) => write!(f, "auth: {m}"),
            ScrobbleError::RateLimited(Some(d)) => write!(f, "rate limited ({}s)", d.as_secs()),
            ScrobbleError::RateLimited(None) => write!(f, "rate limited"),
            ScrobbleError::Network(m) => write!(f, "network: {m}"),
            ScrobbleError::Invalid(m) => write!(f, "rejected: {m}"),
        }
    }
}

impl std::error::Error for ScrobbleError {}

/// What every scrobbling backend can do. Batch size is the caller's job (Last.fm caps at
/// 50 per `track.scrobble`; the queue flusher chunks accordingly).
// Internal trait with in-crate impls only; callers never name the returned futures, so
// plain `async fn` (without explicit auto-trait bounds) is fine here.
#[allow(async_fn_in_trait)]
pub trait ScrobbleService {
    fn kind(&self) -> ServiceKind;
    async fn now_playing(&self, track: &ScrobbleTrack) -> Result<(), ScrobbleError>;
    async fn scrobble_batch(&self, tracks: &[ScrobbleTrack]) -> Result<(), ScrobbleError>;
    async fn love(&self, artist: &str, title: &str, love: bool) -> Result<(), ScrobbleError>;
}
