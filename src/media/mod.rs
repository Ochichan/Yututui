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

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod mpris;
// Named `smtc` (not `windows`) so paths inside never shadow the `windows` crate.
#[cfg(windows)]
mod smtc;

#[cfg(target_os = "macos")]
use macos as platform_backend;
#[cfg(target_os = "linux")]
use mpris as platform_backend;
#[cfg(windows)]
use smtc as platform_backend;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::queue::Repeat;

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
pub type CommandSink = Arc<dyn Fn(MediaCommand) + Send + Sync>;

/// Extract a YouTube video id from a watch/share URL (`music.youtube.com/watch?v=…`,
/// `www.youtube.com/watch?v=…`, `youtu.be/…`) for MPRIS `OpenUri`.
pub fn parse_youtube_video_id(uri: &str) -> Option<String> {
    let uri = uri.trim();
    let rest = uri
        .strip_prefix("https://")
        .or_else(|| uri.strip_prefix("http://"))?;
    let id = if let Some(after_host) = rest.strip_prefix("youtu.be/") {
        after_host
    } else {
        let (host, path) = rest.split_once('/')?;
        if !matches!(
            host,
            "music.youtube.com" | "www.youtube.com" | "youtube.com" | "m.youtube.com"
        ) {
            return None;
        }
        path.strip_prefix("watch?")?
            .split('&')
            .find_map(|param| param.strip_prefix("v="))?
    };
    let id: String = id
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    // YouTube ids are 11 chars today; accept a small range to stay future-proof
    // while rejecting obviously-truncated fragments.
    (id.len() >= 8 && id.len() <= 16).then_some(id)
}

/// The facade owned by the run loop (TUI or daemon). Construction never fails: a
/// backend that can't initialize just leaves the session inert (A-2 in the spec).
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
    cmd_sink: CommandSink,
    artwork: artwork::ArtworkCache,
    /// Last artwork key requested, so a track change requests its art exactly once.
    art_requested: Option<String>,
}

impl MediaSession {
    pub fn new(
        enabled: bool,
        cmd_sink: impl Fn(MediaCommand) + Send + Sync + 'static,
        art_sink: impl Fn(artwork::MediaArtworkReady) + Send + Sync + 'static,
    ) -> Self {
        let cmd_sink: CommandSink = Arc::new(cmd_sink);
        Self {
            enabled,
            failed: false,
            activated: false,
            backend: None,
            last: None,
            cmd_sink,
            artwork: artwork::ArtworkCache::spawn(art_sink),
            art_requested: None,
        }
    }

    /// Live enable/disable (the Settings toggle). Disabling tears the OS session down
    /// (no ghost entry); re-enabling brings it back with the next published snapshot.
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        self.activated = false;
        self.last = None;
        // An explicit user toggle gets a fresh init attempt even after a failure.
        self.failed = false;
        if !enabled {
            self.backend = None; // Drop tears down the platform session.
        }
    }

    /// Whether the run loop should drive [`Self::pump`] on a short interval.
    /// Only macOS needs it (main-thread run-loop delivery of remote commands).
    pub fn wants_pump(&self) -> bool {
        cfg!(target_os = "macos") && self.backend.is_some()
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

        // Hold back until the first actually-playing snapshot on platforms where a
        // single Now Playing slot exists (macOS) or a blank session would show (SMTC).
        if !self.activated {
            if snapshot.status != MediaPlaybackStatus::Playing && !platform_backend::EAGER {
                return;
            }
            self.activated = true;
            if self.backend.is_none() {
                match platform_backend::Backend::new(Arc::clone(&self.cmd_sink)) {
                    Ok(backend) => self.backend = Some(backend),
                    Err(e) => {
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
            backend.apply(&snapshot, changes);
        }
        self.last = Some(snapshot);
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
            self.art_requested = Some(track.key.clone());
            self.artwork.request(track.key.clone(), query.clone());
        }
    }
}

/// Fallback backend for platforms without a media-session integration.
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod fallback {
    use super::{CommandSink, MediaChanges, MediaSnapshot};

    pub const EAGER: bool = false;

    pub struct Backend;

    impl Backend {
        pub fn new(_sink: CommandSink) -> anyhow::Result<Self> {
            anyhow::bail!("no media-session backend for this platform")
        }

        pub fn apply(&mut self, _snapshot: &MediaSnapshot, _changes: MediaChanges) {}
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
use fallback as platform_backend;

#[cfg(test)]
mod tests {
    use super::*;

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
