//! Scrobbling: Last.fm + ListenBrainz.
//!
//! A pure snapshot-diff state machine ([`monitor`]) decides *when* a listen counts, a
//! durable JSONL queue ([`queue`]) makes scrobbles crash-safe, and a network actor
//! ([`actor`]) owns the clients ([`lastfm`], [`listenbrainz`]) behind the [`service`]
//! trait. Both run loops (TUI and daemon) feed the actor the same
//! [`crate::media::MediaSnapshot`] they already publish to the OS media session — the one
//! place the two playback owners converge — via [`ScrobbleHandle::observe`], which
//! derives an [`Observation`] and rate-gates the channel traffic.

pub mod actor;
pub mod auth_cli;
pub mod lastfm;
pub mod listenbrainz;
pub mod monitor;
pub mod queue;
pub mod service;

use std::time::Instant;

pub use actor::{ScrobbleCmd, ScrobbleEvent, spawn};
pub use monitor::{Observation, ObservedTrack};

use tokio::sync::mpsc::UnboundedSender;

/// Runtime snapshot the actor works from, resolved by
/// [`crate::config::Config::scrobble_settings`] (embedded credentials + config overrides
/// + enabled gates already applied).
// No `Debug`: carries the app secret, session key, and token.
#[derive(Clone)]
pub struct ScrobbleSettings {
    /// Application credentials (embedded or config override). `None` → Last.fm wholly
    /// unavailable, including the connect flow.
    pub lastfm_app: Option<LastfmApp>,
    /// The connected, enabled Last.fm session. `None` = disconnected or switched off.
    pub lastfm: Option<LastfmSession>,
    pub listenbrainz: Option<ListenBrainzSession>,
    /// Scrobble local files too (when they carry title + artist metadata).
    pub local_files: bool,
}

impl ScrobbleSettings {
    /// Whether any service would receive scrobbles — when false the actor idles.
    pub fn any_active(&self) -> bool {
        self.lastfm.is_some() || self.listenbrainz.is_some()
    }
}

// No `Debug`: `api_secret` is the app secret.
#[derive(Clone)]
pub struct LastfmApp {
    pub api_key: String,
    pub api_secret: String,
}

// No `Debug`: `session_key` is a secret.
#[derive(Clone)]
pub struct LastfmSession {
    pub session_key: String,
    /// Mirror in-app like/unlike to `track.love`/`track.unlove`.
    pub love_sync: bool,
}

// No `Debug`: `token` is a secret.
#[derive(Clone)]
pub struct ListenBrainzSession {
    pub token: String,
    /// Base API URL (self-hosted friendly); default [`listenbrainz::DEFAULT_API_URL`].
    pub api_url: String,
}

impl Observation {
    /// Derive an observation from the snapshot both run loops already build. Injects the
    /// clocks here so the monitor stays deterministic under test.
    pub fn from_media(snapshot: &crate::media::MediaSnapshot) -> Self {
        use crate::media::MediaPlaybackStatus;
        let track = snapshot.track.as_ref().map(|t| {
            let is_local = t.key.starts_with("local:");
            ObservedTrack {
                key: t.key.clone(),
                title: t.title.clone(),
                // `Song::local_file` fills a "Local file" placeholder for untagged files;
                // that is display text, not an artist — scrobbling treats it as absent.
                artist: if is_local && t.artist == "Local file" {
                    String::new()
                } else {
                    t.artist.clone()
                },
                album: t.album.clone(),
                duration: t.duration,
                is_live: t.is_live,
                is_local,
                origin_url: t.url.clone(),
                liked: t.liked,
            }
        });
        Self {
            playing: snapshot.status == MediaPlaybackStatus::Playing,
            stopped: snapshot.status == MediaPlaybackStatus::Stopped,
            position: snapshot.position_now(),
            position_epoch: snapshot.position_epoch,
            rate: snapshot.rate,
            at: Instant::now(),
            wall_unix: crate::signals::unix_now(),
            track,
        }
    }
}

/// The run loops' handle: derives + rate-gates observations, forwards commands.
///
/// The TUI publishes a snapshot after *every* reducer message (scrolling included), so
/// `observe` only forwards when the scrobble-relevant fingerprint changed or a ~1s
/// heartbeat is due while playing — the actor sees ~1 Hz, the reducer path stays free of
/// channel traffic.
pub struct ScrobbleHandle {
    tx: UnboundedSender<ScrobbleCmd>,
    last_fingerprint: Option<Fingerprint>,
    last_sent: Option<Instant>,
}

#[derive(PartialEq)]
struct Fingerprint {
    key: Option<String>,
    playing: bool,
    stopped: bool,
    epoch: u64,
    liked: bool,
    has_duration: bool,
}

impl ScrobbleHandle {
    pub(crate) fn new(tx: UnboundedSender<ScrobbleCmd>) -> Self {
        Self {
            tx,
            last_fingerprint: None,
            last_sent: None,
        }
    }

    pub fn observe(&mut self, snapshot: &crate::media::MediaSnapshot) {
        let obs = Observation::from_media(snapshot);
        let fingerprint = Fingerprint {
            key: obs.track.as_ref().map(|t| t.key.clone()),
            playing: obs.playing,
            stopped: obs.stopped,
            epoch: obs.position_epoch,
            liked: obs.track.as_ref().is_some_and(|t| t.liked),
            has_duration: obs.track.as_ref().is_some_and(|t| t.duration.is_some()),
        };
        let heartbeat_due = self
            .last_sent
            .is_none_or(|t| t.elapsed().as_secs_f64() >= 1.0);
        if self.last_fingerprint.as_ref() != Some(&fingerprint) || (obs.playing && heartbeat_due) {
            self.last_fingerprint = Some(fingerprint);
            self.last_sent = Some(Instant::now());
            let _ = self.tx.send(ScrobbleCmd::Observe(Box::new(obs)));
        }
    }

    pub fn reconfigure(&self, settings: ScrobbleSettings) {
        let _ = self.tx.send(ScrobbleCmd::Reconfigure(Box::new(settings)));
    }

    /// Kick the Last.fm browser authorization flow (events come back via the sink).
    pub fn auth_start(&self) {
        let _ = self.tx.send(ScrobbleCmd::AuthStart);
    }

    /// Ask for a final best-effort queue flush; await the receiver (with a timeout) on quit.
    pub fn shutdown_flush(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (done, rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(ScrobbleCmd::Shutdown { done });
        rx
    }
}
