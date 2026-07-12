//! OS media-session integration: macOS Now Playing, Windows SMTC, Linux MPRIS.
//!
//! This is the platform-independent layer. The core (TUI reducer or headless daemon
//! engine) stays the single source of truth: it builds a [`MediaSnapshot`] after every
//! state change and hands it to [`MediaSession::publish`], which diffs against the last
//! published snapshot and forwards only the changed facets to the platform backend.
//! Inbound OS events (media keys, widget buttons, scrubber drags, AirPods gestures)
//! arrive as [`MediaCommand`]s through the sink given at construction and flow through
//! the normal reducer/engine paths — the backend never mutates state optimistically.
//!
//! Backend init failure (no session bus, WinRT unavailable, …) is non-fatal: the app
//! keeps running with media controls disabled and one warning line in the log.

pub mod artwork;
pub mod identity;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod mpris;
// Named `smtc` (not `windows`) so paths inside never shadow the `windows` crate.
#[cfg(windows)]
mod smtc;
#[cfg(any(windows, test))]
mod smtc_lifecycle;

#[cfg(target_os = "macos")]
use macos as platform_backend;
#[cfg(target_os = "linux")]
use mpris as platform_backend;
#[cfg(windows)]
use smtc as platform_backend;

use std::path::PathBuf;
use std::sync::Arc;
#[cfg(any(target_os = "linux", windows, test))]
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::queue::Repeat;
use crate::util::delivery::{CallbackCancellation, DeliveryResult};

/// A control command originating from the OS media session, normalized across
/// platforms. Applied through the same reducer/engine paths a keypress or `ytt -r`
/// command uses, so it works regardless of the TUI's input mode.
// Each backend constructs its own subset (e.g. only MPRIS emits SetVolume/OpenUri,
// only macOS emits Like/Dislike), so per-platform builds see "unused" variants.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum MediaCommand {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    /// Relative seek in seconds (negative = backward).
    SeekBy(f64),
    /// Absolute seek to a position in seconds (scrubber drag / `SetPosition`).
    SeekTo(f64),
    SetShuffle(bool),
    SetRepeat(Repeat),
    /// App volume, `0.0..=1.0` (Linux MPRIS `Volume` writes only).
    SetVolume(f64),
    /// Playback-rate change (MPRIS `Rate` writes). `0.0` means pause per the MPRIS spec.
    SetRate(f64),
    /// Open a YouTube / YouTube Music URL (MPRIS `OpenUri`).
    OpenUri(String),
    /// Toggle the current track's like (favorite) state (macOS `likeCommand`).
    Like,
    /// Toggle the current track's dislike state (macOS `dislikeCommand`).
    Dislike,
    /// Quit the player (MPRIS `Quit`).
    Quit,
}

/// The coarse transport state reported to the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

/// Which transport actions are currently possible; drives button enablement
/// (SMTC `Is*Enabled`, MPRIS `Can*`, macOS `isEnabled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MediaCaps {
    pub can_next: bool,
    pub can_previous: bool,
    pub can_seek: bool,
    pub can_play: bool,
    pub can_pause: bool,
}

/// The current track's metadata, as shown by OS media surfaces.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaTrack {
    /// Stable per-track key (the queue `video_id`; also keys the artwork cache).
    pub key: String,
    pub title: String,
    /// Display artist; empty when unknown (adapters omit the field then).
    pub artist: String,
    /// Album name when the catalog knows it (`xesam:album`; scrobble metadata).
    pub album: Option<String>,
    /// Track length in seconds; `None` for live streams (radio).
    pub duration: Option<f64>,
    pub is_live: bool,
    /// Shareable `https://` URL (`xesam:url`), when the track has a YouTube origin.
    pub url: Option<String>,
    /// Remote artwork URL fallback (MPRIS clients accept `https://` art URLs).
    pub art_remote_url: Option<String>,
    /// Cache-resolved local artwork file, once the async fetch has landed.
    pub art_file: Option<PathBuf>,
    /// Where the artwork cache should fetch this track's art from.
    pub art_query: Option<artwork::ArtQuery>,
    /// Current like/dislike state (drives macOS feedback-command `isActive`).
    pub liked: bool,
    pub disliked: bool,
}

/// A point-in-time copy of everything the OS media session cares about.
#[derive(Debug, Clone)]
pub struct MediaSnapshot {
    /// `None` = idle (empty queue): metadata cleared, status `Stopped`.
    pub track: Option<MediaTrack>,
    pub status: MediaPlaybackStatus,
    /// Playback position in seconds, valid as of `captured_at`.
    pub position: f64,
    /// When `position` was sampled; backends interpolate `position +
    /// elapsed × rate` while `status == Playing` (the spec's position clock).
    pub captured_at: Instant,
    /// Playback speed multiplier (mpv `speed`; interpolation + MPRIS `Rate`).
    pub rate: f64,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// App volume `0.0..=1.0` (MPRIS `Volume`).
    pub volume: f64,
    pub caps: MediaCaps,
    /// Bumped by the core on every position discontinuity (seek, track (re)start),
    /// so backends know to re-announce the position (`Seeked` signal, timeline
    /// reset, `ElapsedPlaybackTime` update) — per the spec, playback progress
    /// itself never triggers an update.
    pub position_epoch: u64,
}

impl MediaSnapshot {
    /// An idle snapshot (empty queue, nothing loaded). Seeds the Linux/Windows
    /// backends' local state, so it reads as dead code on macOS builds.
    #[allow(dead_code)]
    pub fn idle() -> Self {
        Self {
            track: None,
            status: MediaPlaybackStatus::Stopped,
            position: 0.0,
            captured_at: Instant::now(),
            rate: 1.0,
            shuffle: false,
            repeat: Repeat::Off,
            volume: 1.0,
            caps: MediaCaps::default(),
            position_epoch: 0,
        }
    }

    /// The interpolated position "now": frozen while paused/stopped, advancing at
    /// `rate` while playing, clamped to the track length when known.
    pub fn position_now(&self) -> f64 {
        let mut pos = self.position;
        if self.status == MediaPlaybackStatus::Playing {
            pos += self.captured_at.elapsed().as_secs_f64() * self.rate;
        }
        if let Some(len) = self.track.as_ref().and_then(|t| t.duration) {
            pos = pos.min(len);
        }
        pos.max(0.0)
    }
}

/// Which facets of the snapshot changed since the last publish. Backends apply only
/// what changed (diff-based: no `PropertiesChanged` spam, no `Update()` churn).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MediaChanges {
    /// Track identity or its displayed metadata changed → full metadata rebuild.
    pub track: bool,
    /// Artwork became available / changed for the *same* track → art-only refresh.
    pub artwork: bool,
    pub status: bool,
    /// Position discontinuity (seek / track restart) → re-announce position.
    pub position: bool,
    /// Shuffle / repeat / rate / volume changed.
    pub options: bool,
    pub caps: bool,
    /// Like/dislike state changed (macOS feedback-button highlight).
    pub feedback: bool,
}

impl MediaChanges {
    fn all() -> Self {
        Self {
            track: true,
            artwork: true,
            status: true,
            position: true,
            options: true,
            caps: true,
            feedback: true,
        }
    }

    /// Preserve every changed facet when several snapshots collapse into one
    /// platform update. The newest snapshot supplies values; this union says which
    /// values must be re-announced so an intermediate edge is never forgotten.
    #[allow(dead_code)] // used by Linux MPRIS and Windows SMTC, not the macOS build
    pub(crate) fn merge(&mut self, newer: Self) {
        self.track |= newer.track;
        self.artwork |= newer.artwork;
        self.status |= newer.status;
        self.position |= newer.position;
        self.options |= newer.options;
        self.caps |= newer.caps;
        self.feedback |= newer.feedback;
    }
}

/// One platform update slot shared by the asynchronous Linux and Windows backends.
/// The newest snapshot owns all values while changed facets are unioned across every
/// collapsed publish, so a full platform queue cannot erase an intermediate edge.
#[derive(Default)]
#[cfg(any(target_os = "linux", windows, test))]
pub(crate) struct LatestMediaUpdate {
    pending: Mutex<Option<(MediaSnapshot, MediaChanges)>>,
}

#[cfg(any(target_os = "linux", windows, test))]
impl LatestMediaUpdate {
    pub(crate) fn store(&self, snapshot: MediaSnapshot, changes: MediaChanges) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let replaced_existing = pending.is_some();
        let mut accumulated = pending
            .as_ref()
            .map_or(changes, |(_, accumulated)| *accumulated);
        accumulated.merge(changes);
        *pending = Some((snapshot, accumulated));
        replaced_existing
    }

    pub(crate) fn take(&self) -> Option<(MediaSnapshot, MediaChanges)> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn clear(&self) {
        let _ = self.take();
    }
}

/// Compute which facets differ between two snapshots. Position *values* are
/// deliberately ignored — only an epoch bump (a discontinuity) counts, so ordinary
/// playback progress never causes an OS update.
pub fn diff(old: &MediaSnapshot, new: &MediaSnapshot) -> MediaChanges {
    let track = match (&old.track, &new.track) {
        (None, None) => false,
        (Some(a), Some(b)) => {
            a.key != b.key
                || a.title != b.title
                || a.artist != b.artist
                || a.album != b.album
                || a.duration != b.duration
                || a.is_live != b.is_live
                || a.url != b.url
        }
        _ => true,
    };
    let artwork = track
        || match (&old.track, &new.track) {
            (Some(a), Some(b)) => a.art_file != b.art_file || a.art_remote_url != b.art_remote_url,
            _ => false,
        };
    let feedback = match (&old.track, &new.track) {
        (Some(a), Some(b)) => a.liked != b.liked || a.disliked != b.disliked,
        (None, None) => false,
        _ => true,
    };
    MediaChanges {
        track,
        artwork,
        status: old.status != new.status,
        position: old.position_epoch != new.position_epoch || track,
        options: old.shuffle != new.shuffle
            || old.repeat != new.repeat
            || (old.rate - new.rate).abs() > 1e-3
            || (old.volume - new.volume).abs() > 5e-3,
        caps: old.caps != new.caps,
        feedback,
    }
}

/// Shared type for the command sink handed to backends.
pub type CommandSink = Arc<dyn Fn(MediaCommand) -> DeliveryResult + Send + Sync>;
type CancellableCommandSink =
    Arc<dyn Fn(MediaCommand, &CallbackCancellation) -> DeliveryResult + Send + Sync>;

/// Extract a YouTube video id from a watch/share URL for MPRIS `OpenUri` and the search-box
/// paste shortcut. Recognizes `…/watch?v=…`, `youtu.be/…`, `…/shorts/…`, `…/embed/…`,
/// `…/live/…`, and `youtube-nocookie.com/embed/…`. Host is matched case-insensitively with a
/// trailing dot stripped. A `watch?v=X&list=Y` link resolves to the *video* X (a pasted watch
/// link means that track, not the playlist it was opened from).
pub fn parse_youtube_video_id(uri: &str) -> Option<String> {
    let uri = uri.trim();
    // Scheme is case-insensitive (RFC 3986).
    let rest = strip_prefix_ci(uri, "https://").or_else(|| strip_prefix_ci(uri, "http://"))?;
    let id = if let Some(after_host) = rest.strip_prefix("youtu.be/") {
        after_host
    } else {
        let (host, path) = rest.split_once('/')?;
        // Normalize the host: strip a trailing dot (`youtube.com.`) and lowercase it.
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        match host.as_str() {
            "music.youtube.com" | "www.youtube.com" | "youtube.com" | "m.youtube.com" => {
                if let Some(query) = path.strip_prefix("watch?") {
                    query
                        .split('&')
                        .find_map(|param| param.strip_prefix("v="))?
                } else {
                    path.strip_prefix("shorts/")
                        .or_else(|| path.strip_prefix("embed/"))
                        .or_else(|| path.strip_prefix("live/"))?
                }
            }
            // Privacy-enhanced player only carries ids via /embed/.
            "www.youtube-nocookie.com" | "youtube-nocookie.com" => path.strip_prefix("embed/")?,
            _ => return None,
        }
    };
    // YouTube ids are 11 chars today; accept a small range to stay future-proof while rejecting
    // obviously-truncated fragments. `leading_id` also rejects a concatenated second-URL paste.
    leading_id(id, 8, 16)
}

/// Case-insensitive `strip_prefix`, char-boundary-safe (`prefix` is ASCII here).
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Take the leading id-charset run off `raw`, requiring it to end at a real URL
/// boundary (`?`, `&`, `#`, `/`, or the end of the string). A run stopped by anything
/// else — e.g. the `:` of a second URL pasted right behind the first — is a mangled
/// paste, not an id.
fn leading_id(raw: &str, min: usize, max: usize) -> Option<String> {
    let end = raw
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_'))
        .map_or(raw.len(), |(i, _)| i);
    let (id, rest) = raw.split_at(end);
    if !matches!(rest.chars().next(), None | Some('?' | '&' | '#' | '/')) {
        return None;
    }
    (id.len() >= min && id.len() <= max).then(|| id.to_owned())
}

/// Extract a YouTube playlist id from a playlist URL
/// (`{music,www,m,}.youtube.com/playlist?list=…`). Watch URLs carrying a `list=` param
/// are deliberately *not* matched — a pasted watch link means that video, not the
/// playlist it was opened from.
pub fn parse_youtube_playlist_id(uri: &str) -> Option<String> {
    let uri = uri.trim();
    let rest = uri
        .strip_prefix("https://")
        .or_else(|| uri.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/')?;
    if !matches!(
        host,
        "music.youtube.com" | "www.youtube.com" | "youtube.com" | "m.youtube.com"
    ) {
        return None;
    }
    let id = path
        .strip_prefix("playlist?")?
        .split('&')
        .find_map(|param| param.strip_prefix("list="))?;
    // Playlist ids vary by kind ("PL…" 34, "OLAK5uy_…" 41, mixes longer); bound loosely.
    leading_id(id, 8, 64)
}

/// The facade owned by the run loop (TUI or daemon). Construction never fails: a
/// backend that can't initialize just leaves the session inert (A-2 in the spec).
const BACKEND_RETRY_DELAY: Duration = Duration::from_millis(250);

pub struct MediaSession {
    enabled: bool,
    /// Backend init failed (no session bus, WinRT unavailable, …). Latched so the
    /// one warning isn't retried/re-logged on every message; an explicit off→on
    /// toggle in Settings clears it and allows one fresh attempt.
    failed: bool,
    /// Whether the session has claimed the OS media surface yet. On macOS/Windows we
    /// wait for the first *playing* snapshot so merely launching the app never steals
    /// the system Now Playing slot from another player; MPRIS registers eagerly so
    /// desktop widgets can offer "resume" right away.
    activated: bool,
    backend: Option<platform_backend::Backend>,
    last: Option<MediaSnapshot>,
    cmd_sink: CancellableCommandSink,
    callback_cancellation: CallbackCancellation,
    /// A prior Windows initialization worker timed out and is still retiring. Retry at a bounded
    /// cadence without latching a permanent failure or spawning a concurrent SMTC worker.
    backend_retry_after: Option<Instant>,
    artwork: artwork::ArtworkCache,
    /// Last artwork key requested, so a track change requests its art exactly once.
    art_requested: Option<String>,
}

impl MediaSession {
    pub fn new(
        enabled: bool,
        cmd_sink: impl Fn(MediaCommand) -> DeliveryResult + Send + Sync + 'static,
        art_sink: impl Fn(artwork::MediaArtworkReady) + Send + Sync + 'static,
    ) -> Self {
        Self::new_cancellable(
            enabled,
            move |command, _cancellation| cmd_sink(command),
            art_sink,
        )
    }

    /// Construct a media session whose platform callback can release bounded backpressure when
    /// that backend generation is retired. Kept crate-private so the public constructor retains
    /// its existing source-compatible callback contract.
    pub(crate) fn new_cancellable(
        enabled: bool,
        cmd_sink: impl Fn(MediaCommand, &CallbackCancellation) -> DeliveryResult + Send + Sync + 'static,
        art_sink: impl Fn(artwork::MediaArtworkReady) + Send + Sync + 'static,
    ) -> Self {
        let cmd_sink: CancellableCommandSink = Arc::new(cmd_sink);
        Self {
            enabled,
            failed: false,
            activated: false,
            backend: None,
            last: None,
            cmd_sink,
            callback_cancellation: CallbackCancellation::new(),
            backend_retry_after: None,
            artwork: artwork::ArtworkCache::spawn(art_sink),
            art_requested: None,
        }
    }

    /// Live enable/disable (the Settings toggle). Disabling tears the OS session down
    /// (no ghost entry); re-enabling brings it back with the next published snapshot.
    pub fn set_enabled(&mut self, enabled: bool) -> bool {
        if enabled == self.enabled {
            return false;
        }
        self.enabled = enabled;
        self.activated = false;
        self.last = None;
        self.backend_retry_after = None;
        // An explicit user toggle gets a fresh init attempt even after a failure.
        self.failed = false;
        if !enabled {
            // Release a callback retaining an exact command before the platform backend joins
            // its producer thread. The application-wide owner ingress intentionally stays open.
            self.callback_cancellation.cancel();
            self.backend = None; // Drop tears down the platform session.
        } else {
            // A retired producer must never poison a newly enabled backend generation.
            self.callback_cancellation = CallbackCancellation::new();
        }
        true
    }

    /// Whether the run loop should drive [`Self::pump`] on a short interval.
    /// Only macOS needs it (main-thread run-loop delivery of remote commands).
    pub fn wants_pump(&self) -> bool {
        cfg!(target_os = "macos") && self.backend.is_some()
    }

    /// Whether a transient platform failure is due for another publish attempt. Owners that
    /// fingerprint snapshots use this to bypass their usual no-change fast path at a bounded
    /// cadence.
    pub(crate) fn retry_due(&self) -> bool {
        self.retry_deadline()
            .is_some_and(|retry_after| Instant::now() >= retry_after)
    }

    pub(crate) fn retry_deadline(&self) -> Option<Instant> {
        if self.enabled && !self.failed {
            self.backend_retry_after
        } else {
            None
        }
    }

    /// Service the platform event loop, if this platform needs manual pumping.
    pub fn pump(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(backend) = self.backend.as_mut() {
            backend.pump();
        }
    }

    /// Publish the core's current state. Diffs against the previous snapshot and
    /// forwards only the changed facets; a no-change publish costs one comparison.
    pub fn publish(&mut self, snapshot: MediaSnapshot) {
        if !self.enabled || self.failed {
            return;
        }
        if let Some(retry_after) = self.backend_retry_after {
            if Instant::now() < retry_after {
                return;
            }
            self.backend_retry_after = None;
        }

        // Hold back until the first actually-playing snapshot on platforms where a
        // single Now Playing slot exists (macOS) or a blank session would show (SMTC).
        if !self.activated {
            if snapshot.status != MediaPlaybackStatus::Playing && !platform_backend::EAGER {
                return;
            }
            self.activated = true;
            if self.backend.is_none() {
                let cmd_sink = Arc::clone(&self.cmd_sink);
                let cancellation = self.callback_cancellation.clone();
                let backend_sink: CommandSink =
                    Arc::new(move |command| cmd_sink(command, &cancellation));
                match platform_backend::Backend::new(backend_sink) {
                    Ok(backend) => {
                        self.backend_retry_after = None;
                        self.backend = Some(backend);
                    }
                    Err(e) => {
                        if retryable_backend_init_error(&e) {
                            self.defer_backend_init_retry();
                            tracing::debug!(
                                error = %e,
                                retry_ms = BACKEND_RETRY_DELAY.as_millis(),
                                "media backend is still retiring; initialization will retry"
                            );
                            return;
                        }
                        // A timed-out platform constructor may still be retiring off-thread.
                        // Fence any late callback before allowing a future off→on retry.
                        self.callback_cancellation.cancel();
                        tracing::warn!(error = %e, "media controls disabled: platform session init failed");
                        self.failed = true;
                        return;
                    }
                }
            }
        }

        self.request_artwork(&snapshot);
        let changes = match &self.last {
            Some(last) => diff(last, &snapshot),
            None => MediaChanges::all(),
        };
        if let Some(backend) = self.backend.as_mut() {
            // Always hand the backend the fresh snapshot (it re-bases its position
            // clock from it), but only make OS calls for the changed facets.
            match backend.apply(&snapshot, changes) {
                Ok(receipt) => {
                    tracing::trace!(?receipt, "media snapshot accepted");
                    self.backend_retry_after = None;
                    self.last = Some(snapshot);
                }
                Err(error) => {
                    tracing::warn!(%error, "media snapshot was not accepted");
                    if error == crate::util::delivery::DeliveryError::Closed {
                        self.callback_cancellation.cancel();
                        self.backend = None;
                        self.failed = true;
                    } else {
                        self.arm_backend_retry();
                    }
                }
            }
        } else {
            self.last = Some(snapshot);
        }
    }

    fn arm_backend_retry(&mut self) {
        self.backend_retry_after = Some(Instant::now() + BACKEND_RETRY_DELAY);
    }

    fn defer_backend_init_retry(&mut self) {
        self.activated = false;
        self.arm_backend_retry();
    }

    /// Kick the async artwork fetch when a new track appears; the result comes back
    /// through the core (as a message) and lands in a later snapshot's `art_file`.
    fn request_artwork(&mut self, snapshot: &MediaSnapshot) {
        let Some(track) = &snapshot.track else {
            self.art_requested = None;
            return;
        };
        if track.art_file.is_some() || self.art_requested.as_deref() == Some(track.key.as_str()) {
            return;
        }
        if let Some(query) = &track.art_query {
            match self.artwork.request(track.key.clone(), query.clone()) {
                Ok(receipt) => {
                    tracing::trace!(?receipt, key = %track.key, "media artwork request accepted");
                    self.art_requested = Some(track.key.clone());
                }
                Err(error) => {
                    // Leave `art_requested` untouched so the next publish retries.
                    tracing::warn!(%error, key = %track.key, "media artwork request was not accepted");
                }
            }
        }
    }
}

fn retryable_backend_init_error(error: &anyhow::Error) -> bool {
    #[cfg(windows)]
    {
        platform_backend::is_worker_retiring(error)
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        // Rust drops fields after `Drop::drop`; take the backend explicitly so cancellation is
        // always visible before a platform destructor performs a synchronous worker join.
        self.callback_cancellation.cancel();
        self.backend.take();
    }
}

/// Fallback backend for platforms without a media-session integration.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod fallback {
    use super::{CommandSink, MediaChanges, MediaSnapshot};
    use crate::util::delivery::{DeliveryReceipt, DeliveryResult};

    pub const EAGER: bool = false;

    pub struct Backend;

    impl Backend {
        pub fn new(_sink: CommandSink) -> anyhow::Result<Self> {
            anyhow::bail!("no media-session backend for this platform")
        }

        pub fn apply(
            &mut self,
            _snapshot: &MediaSnapshot,
            _changes: MediaChanges,
        ) -> DeliveryResult {
            Ok(DeliveryReceipt::Enqueued)
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
use fallback as platform_backend;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabling_media_cancels_a_blocked_callback_before_worker_join() {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let mut media = MediaSession::new_cancellable(
            true,
            move |_command, cancellation| {
                started_tx.send(()).expect("announce fake callback");
                while !cancellation.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(crate::util::delivery::DeliveryError::Closed)
            },
            |_| {},
        );
        let sink = Arc::clone(&media.cmd_sink);
        let callback_cancellation = media.callback_cancellation.clone();
        let worker = std::thread::spawn(move || sink(MediaCommand::Next, &callback_cancellation));

        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("fake callback entered");
        assert!(media.set_enabled(false));
        assert_eq!(
            worker.join().expect("join fake SMTC callback"),
            Err(crate::util::delivery::DeliveryError::Closed)
        );
    }

    #[tokio::test]
    async fn reenabled_media_uses_a_fresh_callback_generation() {
        let mut media = MediaSession::new_cancellable(
            true,
            |_command, _cancellation| Ok(crate::util::delivery::DeliveryReceipt::Enqueued),
            |_| {},
        );
        let retired = media.callback_cancellation.clone();

        assert!(media.set_enabled(false));
        assert!(retired.is_cancelled());
        assert!(media.set_enabled(true));
        assert!(!media.callback_cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn retiring_backend_defers_without_latching_the_fresh_generation() {
        let mut media = MediaSession::new_cancellable(
            true,
            |_command, _cancellation| Ok(crate::util::delivery::DeliveryReceipt::Enqueued),
            |_| {},
        );
        media.activated = true;
        media.defer_backend_init_retry();

        assert!(!media.failed);
        assert!(!media.activated);
        assert!(!media.callback_cancellation.is_cancelled());
        assert!(media.backend_retry_after.is_some());
        assert!(!media.retry_due());
        media.backend_retry_after = Some(Instant::now());
        assert!(media.retry_due());
        assert!(media.retry_deadline().is_some());
    }

    #[test]
    fn parse_playlist_id_variants() {
        for url in [
            "https://www.youtube.com/playlist?list=PLabcdefgh1234",
            "https://music.youtube.com/playlist?list=PLabcdefgh1234",
            "http://m.youtube.com/playlist?list=PLabcdefgh1234&si=xyz",
        ] {
            assert_eq!(
                parse_youtube_playlist_id(url).as_deref(),
                Some("PLabcdefgh1234"),
                "{url}"
            );
        }
        // A watch URL with a list param means the video, not the playlist.
        assert_eq!(
            parse_youtube_playlist_id(
                "https://www.youtube.com/watch?v=abc12345678&list=PLabcdefgh1234"
            ),
            None
        );
        // Non-YouTube hosts, schemeless strings, and truncated ids are rejected.
        assert_eq!(
            parse_youtube_playlist_id("https://example.com/playlist?list=PLabcdefgh1234"),
            None
        );
        assert_eq!(
            parse_youtube_playlist_id("youtube.com/playlist?list=PLabcdefgh1234"),
            None
        );
        assert_eq!(
            parse_youtube_playlist_id("https://www.youtube.com/playlist?list=PL1"),
            None
        );
    }

    #[test]
    fn concatenated_urls_are_not_ids() {
        // Two URLs pasted back to back: the id run ends at the second URL's `:` — a
        // mangled paste, not an id. (Regression: this used to yield "-UfI1X-MSighttps".)
        assert_eq!(
            parse_youtube_video_id(
                "https://youtu.be/-UfI1X-MSighttps://www.youtube.com/watch?v=dQw4w9WgXcQ"
            ),
            None
        );
        assert_eq!(
            parse_youtube_video_id(
                "https://www.youtube.com/watch?v=dQw4w9WgXcQhttps://youtu.be/-UfI1X-MSig"
            ),
            None
        );
        assert_eq!(
            parse_youtube_playlist_id(
                "https://www.youtube.com/playlist?list=PLabcdefgh1234https://youtu.be/x"
            ),
            None
        );
        // Legitimate boundaries still parse: query params, fragments, trailing slashes.
        assert_eq!(
            parse_youtube_video_id("https://youtu.be/dQw4w9WgXcQ#t=10").as_deref(),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            parse_youtube_video_id("https://youtu.be/dQw4w9WgXcQ/").as_deref(),
            Some("dQw4w9WgXcQ")
        );
    }

    fn track(key: &str) -> MediaTrack {
        MediaTrack {
            key: key.to_owned(),
            title: format!("title-{key}"),
            artist: "artist".to_owned(),
            album: None,
            duration: Some(180.0),
            is_live: false,
            url: None,
            art_remote_url: None,
            art_file: None,
            art_query: None,
            liked: false,
            disliked: false,
        }
    }

    fn snap(key: Option<&str>) -> MediaSnapshot {
        MediaSnapshot {
            track: key.map(track),
            status: MediaPlaybackStatus::Playing,
            position: 10.0,
            captured_at: Instant::now(),
            rate: 1.0,
            shuffle: false,
            repeat: Repeat::Off,
            volume: 0.5,
            caps: MediaCaps::default(),
            position_epoch: 1,
        }
    }

    #[test]
    fn identical_snapshots_produce_no_changes() {
        let a = snap(Some("a"));
        let mut b = a.clone();
        // Progress alone (position value + capture time) is not a change.
        b.position = 42.0;
        b.captured_at = Instant::now();
        assert_eq!(diff(&a, &b), MediaChanges::default());
    }

    #[test]
    fn track_change_marks_track_artwork_and_position() {
        let a = snap(Some("a"));
        let b = snap(Some("b"));
        let d = diff(&a, &b);
        assert!(d.track && d.artwork && d.position);
        assert!(!d.status && !d.options && !d.caps);
    }

    #[test]
    fn epoch_bump_marks_position_only() {
        let a = snap(Some("a"));
        let mut b = a.clone();
        b.position_epoch += 1;
        let d = diff(&a, &b);
        assert!(d.position);
        assert!(!d.track && !d.status && !d.options);
    }

    #[test]
    fn artwork_arrival_marks_artwork_only() {
        let a = snap(Some("a"));
        let mut b = a.clone();
        b.track.as_mut().unwrap().art_file = Some(std::path::PathBuf::from("/tmp/x.jpg"));
        let d = diff(&a, &b);
        assert!(d.artwork);
        assert!(!d.track && !d.position);
    }

    #[test]
    fn status_and_options_flag_independently() {
        let a = snap(Some("a"));
        let mut b = a.clone();
        b.status = MediaPlaybackStatus::Paused;
        b.volume = 0.8;
        b.shuffle = true;
        let d = diff(&a, &b);
        assert!(d.status && d.options);
        assert!(!d.track && !d.position);
    }

    #[test]
    fn coalesced_media_changes_preserve_every_intermediate_facet() {
        let mut accumulated = MediaChanges {
            status: true,
            position: true,
            ..MediaChanges::default()
        };
        accumulated.merge(MediaChanges {
            artwork: true,
            options: true,
            ..MediaChanges::default()
        });

        assert!(accumulated.status);
        assert!(accumulated.position);
        assert!(accumulated.artwork);
        assert!(accumulated.options);
        assert!(!accumulated.track);
    }

    #[test]
    fn latest_media_slot_keeps_newest_snapshot_and_unions_all_facets() {
        let slot = LatestMediaUpdate::default();
        let first = snap(Some("first"));
        let mut newest = snap(Some("newest"));
        newest.volume = 0.25;

        assert!(!slot.store(
            first,
            MediaChanges {
                track: true,
                artwork: true,
                status: true,
                ..MediaChanges::default()
            },
        ));
        assert!(slot.store(
            newest,
            MediaChanges {
                position: true,
                options: true,
                caps: true,
                feedback: true,
                ..MediaChanges::default()
            },
        ));

        let (stored, changes) = slot.take().expect("latest update");
        assert_eq!(
            stored.track.as_ref().map(|track| track.key.as_str()),
            Some("newest")
        );
        assert_eq!(stored.volume, 0.25);
        assert!(changes.track);
        assert!(changes.artwork);
        assert!(changes.status);
        assert!(changes.position);
        assert!(changes.options);
        assert!(changes.caps);
        assert!(changes.feedback);
        assert!(slot.take().is_none());
    }

    #[test]
    fn latest_media_slot_clear_discards_pending_state() {
        let slot = LatestMediaUpdate::default();
        assert!(!slot.store(snap(Some("pending")), MediaChanges::all()));
        slot.clear();
        assert!(slot.take().is_none());
    }

    #[test]
    fn like_state_marks_feedback() {
        let a = snap(Some("a"));
        let mut b = a.clone();
        b.track.as_mut().unwrap().liked = true;
        let d = diff(&a, &b);
        assert!(d.feedback);
        assert!(!d.track);
    }

    #[test]
    fn queue_end_marks_track_change() {
        let a = snap(Some("a"));
        let b = snap(None);
        assert!(diff(&a, &b).track);
    }

    #[test]
    fn position_now_freezes_while_paused() {
        let mut s = snap(Some("a"));
        s.status = MediaPlaybackStatus::Paused;
        s.position = 30.0;
        assert!((s.position_now() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn position_now_clamps_to_duration() {
        let mut s = snap(Some("a"));
        s.position = 500.0; // past the 180s duration
        assert!((s.position_now() - 180.0).abs() < 1e-9);
    }
}
