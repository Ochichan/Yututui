//! Application state and the TEA-style reducer.
//!
//! All mutable state lives in [`App`] on the main task. Inbound events and actor
//! results arrive as [`Msg`]; `update` is the single place that mutates state and
//! returns the [`Cmd`]s the run loop should dispatch to actors. Keeping `update` pure
//! (state in, `Cmd`s out — no IO) makes it directly unit-testable.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use image::DynamicImage;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::thread::{ResizeRequest, ResizeResponse, ThreadProtocol};

use crate::ai::GeminiModel;
use crate::api::{ApiMode, Song};
use crate::artwork::ArtSource;
use crate::config::{Config, SPEED_MAX, SPEED_MIN};
use crate::downloads::DownloadStore;
use crate::eq::{self, EqPreset};
use crate::keymap::{Action, Chord, Conflict, KeyContext, KeyMap};
use crate::library::Library;
use crate::lyrics::LyricLine;
use crate::player::PlayerCmd;
use crate::playlists::Playlists;
use crate::queue::{Queue, QueueSnapshot};
use crate::romanize::{RomanizeItem, RomanizedResult};
use crate::search_source::{SearchConfig, SearchSource};
use crate::settings::{
    self, Field, FieldKind, SettingsConfirm, SettingsDraft, SettingsState, SettingsTab,
};
use crate::signals::{self, Signals};
use crate::station::StationStore;
use crate::streaming::{self, CandidateSource, Cooc, StationState, StreamingMode};
use crate::t;
use crate::theme::{ThemeConfig, ThemeRole};
use crate::util::process;

mod types;
pub use types::*;

mod bootstrap;
mod feedback;
mod helpers;
pub(crate) use helpers::open_in_browser;
pub(in crate::app) use helpers::{
    fetch_lyrics_cmd, rect_contains, song_label, spawn_video_overlay,
};
mod mode_switch;
mod navigation;
mod reducer;
mod session_restore;

mod state;
pub use state::*;

mod ai_reducer;
pub use ai_reducer::AiMsg;
mod artwork;
mod clipboard;
mod download;
mod keys;
mod library;
mod library_reducer;
mod local;
mod local_format;
mod local_import;
mod media_reducer;
mod mouse;
pub use mouse::HitMap;
mod now_playing;
mod now_playing_reducer;
mod player;
pub use player::PlayerMsg;
mod playlists_reducer;
mod queue;
mod recorder_reducer;
mod remote_reducer;
mod romanize;
mod scrobble_reducer;
mod search;
mod settings_reducer;
mod stream_metadata;
mod streaming_reducer;
pub(in crate::app) use clipboard::{copy_to_clipboard, spotify_auth_url_status};
pub use streaming_reducer::StreamingMsg;

/// Autoplay/streaming top-up policy and the play-error breaker threshold — single-sourced
/// with the headless daemon in [`crate::playback_policy`] so no bound can drift between the
/// two playback owners. Re-exported so this module's submodules keep resolving the names.
pub(crate) use crate::playback_policy::{
    AUTOPLAY_COOLDOWN, AUTOPLAY_MAX_FAILURES, AUTOPLAY_THRESHOLD, MAX_CONSECUTIVE_PLAY_ERRORS,
    STREAMING_FALLBACK_COUNT, STREAMING_POOL_COUNT,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum PositionEpochReason {
    RestoreSession,
    Seek,
    TrackRestart,
    PlaybackCleared,
    Stop,
}
/// How many ordered session outcomes (plays/skips/likes/dislikes) to retain for the DJ Gem
/// reranker's recovery context. Small: the model only needs the recent arc.
const SESSION_EVENTS_CAP: usize = 20;
/// How long a resolved DJ Gem rerank ordering stays replayable in [`StreamingRuntime::ai_cache`]. Short:
/// it only needs to catch rapid identical refills (e.g. skipping through a few tracks) before the
/// candidate pool drifts and a fresh call is warranted anyway.
const AI_CACHE_TTL: Duration = Duration::from_secs(600);
/// Trailing skip streak that triggers an off-path feedback summary (the listener is clearly
/// rejecting the station's direction). Matches the reranker's recovery threshold.
const FEEDBACK_STREAK: usize = 3;
/// Minimum gap between feedback summaries, so a long skip streak can't fire one every track.
const FEEDBACK_COOLDOWN: Duration = Duration::from_secs(120);
/// How long a transient `status` notification covers the song title before it auto-clears
/// (on the Player screen the status line replaces the title, so it must not linger).
const STATUS_TTL: Duration = Duration::from_secs(3);
/// Cap on DJ Gem chat transcript lines kept in memory (bounded memory).
const AI_HISTORY_MAX: usize = 999;

/// Rows the cursor moves per mouse-wheel notch in the Library / Search lists — enough to
/// read as scrolling, small enough to stay controllable.
const MOUSE_SCROLL_LINES: usize = 3;
/// Page size used by PageUp/PageDown before the first render records the real list
/// viewport height (e.g. in tests that never draw a frame).
const DEFAULT_PAGE_ROWS: usize = 10;

/// Volume step / ceiling, single-sourced with the headless daemon in
/// [`crate::playback_policy`] so the bound can't drift between the two owners.
pub(crate) use crate::playback_policy::{VOLUME_MAX, VOLUME_STEP};
/// Cap on local download-folder rows held in memory.
const DOWNLOADED_TRACKS_MAX: usize = 999;
/// Playback-speed change per `>`/`<` press.
const SPEED_STEP: f64 = 0.1;
/// Idle gap (seconds) that ends a listening session, resetting the skip-confidence counter.
const SESSION_GAP_SECS: i64 = 20 * 60;
/// Max gap between two list-nav events still counted as one continuous hold. Wider than any
/// key-repeat interval but far under the OS initial-repeat delay, so deliberate taps restart
/// the ramp while a held key keeps it climbing. See [`NavRepeat`] / [`nav_step_for_hold`].
const NAV_REPEAT_GAP: Duration = Duration::from_millis(180);

/// Rows to advance per list-nav step as a function of how long the key has been held: one
/// row for a tap or the first moments of a hold (precise), accelerating to a fast sweep the
/// longer it's held. Per-view length clamping bounds the top tier on short lists.
fn nav_step_for_hold(held: Duration) -> usize {
    match held.as_millis() {
        0..=399 => 1,
        400..=999 => 2,
        1000..=1999 => 4,
        _ => 8,
    }
}

/// Tracks a run of consecutive same-direction list-nav events (OS `Press` auto-repeats on
/// plain terminals, enhanced-terminal `Repeat` events on kitty-class ones) so held
/// navigation accelerates. `action` disambiguates directions — plain nav and Shift
/// range-select each ramp on their own — while `started`/`last` drive a hold-duration ramp
/// with no free-running timer (which would need key-release events plain terminals lack).
#[derive(Debug, Default)]
struct NavRepeat {
    action: Option<Action>,
    started: Option<Instant>,
    last: Option<Instant>,
}

/// The whole application state.
pub struct App {
    pub should_quit: bool,
    /// Set whenever visible state changes; the run loop redraws only when true.
    pub dirty: bool,
    pub mode: Mode,
    /// Whether the API client is signed in (vs anonymous: search + public play only).
    pub authenticated: bool,
    /// The resolved keybindings (defaults overlaid with user overrides from config).
    pub keymap: KeyMap,
    /// Resolved color theme (preset plus user overrides).
    pub theme: ThemeConfig,
    /// Dedicated Radio UI mode: swaps to a cached Radio theme, Radio Browser-only search,
    /// and radio-only Library tabs until normal mode is restored.
    pub radio_dedicated_mode: bool,
    /// Dedicated-Radio-mode theme+queue stash and the pending enter/leave confirmation — see
    /// [`RadioMode`]. The `radio_dedicated_mode` flag above stays flat (read pervasively).
    pub radio_mode: RadioMode,
    /// Dedicated Local Deck shell under the Library mode. Phase 1 only swaps the rendered
    /// shell; queue/session stashing lands with the later Local queue milestone.
    pub local_dedicated_mode: bool,
    /// Local Deck UI state and pending enter/leave confirmation.
    pub local_mode: LocalMode,
    /// All transient modal/overlay state — help/mouse-help/about/why-DJ-Gem toggles, the
    /// key-conflict + settings confirmations, the Spotify picker, the recording popups, the
    /// update-check result, and the identify overlay with its cache/epoch. See [`Overlays`].
    /// `transfer_running` below stays flat (a job guard, not overlay state).
    pub overlays: Overlays,
    /// A transfer job is running (guards double-starts; progress rides the status line).
    pub transfer_running: bool,

    // Playback ----------------------------------------------------------------
    /// Live playback transport: position, duration, pause state, volume, and speed
    /// (mirrors mpv's current state, distinct from the persisted defaults in `config`).
    pub playback: Playback,
    /// Radio recorder (a Shortwave-style feature): the open segment, the bounded browser
    /// history, and the mpv-support probe. All volatile — only [`crate::config::RecordingConfig`]
    /// persists. See [`crate::recorder`].
    pub recorder: crate::recorder::RecorderState,
    /// The media-session artwork cache's resolved local file for the current (or a
    /// recent) track, keyed by `video_id`. Set by [`Msg::MediaArtworkReady`]; read by
    /// [`App::media_snapshot`], which only surfaces it while the keys still match.
    pub(crate) media_art: Option<crate::media::artwork::MediaArtworkReady>,
    /// The play queue: ordering, shuffle, repeat, and the current track.
    pub queue: Queue,
    /// The transient status/notification line: its text, last-set time (for TTL expiry), and
    /// semantic kind (see [`Status`]).
    pub status: Status,
    /// Scratch buffer for [`Self::update`]'s before/after status comparison, reused across
    /// turns so the per-message clone doesn't allocate. Private — only `update` touches it.
    status_text_prev: String,
    /// Video-overlay state: the detached mpv process (if open) and whether opening it paused
    /// the audio (see [`Video`]). Private — render never reads it.
    video: Video,

    // Audio / EQ --------------------------------------------------------------
    /// Live audio-processing settings (EQ preset + per-band gains, loudness normalization, and
    /// the seek step) — the in-session working copy mpv's filter chain is built from, mirrored
    /// from the persisted `config` (see [`AudioEq`]).
    pub audio: AudioEq,
    /// Auto-extend the queue with related tracks when it runs low (streaming mode).
    pub autoplay_streaming: bool,
    /// The two mutually-exclusive player status-line dropdowns (EQ preset + streaming mode); both
    /// player-only and session-ephemeral (see [`Dropdowns`]).
    pub dropdowns: Dropdowns,
    /// Queue-window overlay state: open flag, selection cursor + anchor, on-screen rect
    /// bridge, and wheel-scroll offset (see [`QueuePopup`]).
    pub queue_popup: QueuePopup,
    /// Search results-filter popup state: open flag, live query, cursor, on-screen rect
    /// bridge, and wheel-scroll offset (see [`SearchFilterPopup`]).
    pub search_filter: SearchFilterPopup,

    // Settings ----------------------------------------------------------------
    /// The persisted config, kept so the settings screen can save the full file.
    pub config: Config,
    /// The settings screen state, present only while `Mode::Settings` is active.
    pub settings: Option<Box<SettingsState>>,

    // DJ Gem assistant ------------------------------------------------------------
    /// DJ Gem assistant state: availability, model, chat transcript, prompt, suggestions.
    pub ai: AiState,
    /// Latin-script title display overlay cache and in-flight requests.
    pub romanization: RomanizationRuntime,

    // Streaming runtime -------------------------------------------------------
    /// Streaming autoplay runtime: cooldown clock, in-flight pool flag, a handed-off DJ Gem rerank,
    /// and the empty-extend circuit-breaker counter.
    pub streaming: StreamingRuntime,
    /// Consecutive mpv playback errors with no track playing in between, for the
    /// auto-skip circuit breaker (see [`MAX_CONSECUTIVE_PLAY_ERRORS`]).
    consecutive_play_errors: u8,
    /// The user's local playlists (the DJ Gem playlist tools read/write these).
    pub playlists: Playlists,
    /// The active natural-language station profile (explore level + avoided artists), distilled
    /// from a `start_streaming` vibe and persisted. Read live by [`App::build_station_state`].
    pub station: StationStore,

    // Search ------------------------------------------------------------------
    /// Search query, results, selection, focus, and in-flight flag.
    pub search: SearchState,

    // Library -----------------------------------------------------------------
    /// Favorites + play history, persisted to disk. Loaded by `main` after `new`.
    pub library: Library,
    /// Per-track preference signals (plays/skips/dislikes + raw play log + artist affinity),
    /// persisted separately from the library so `Song`'s shape stays unchanged. Loaded by
    /// `main` after `new`; drives streaming ranking and the ♥/✗ status-line toggles.
    pub signals: Signals,
    /// Listening-session tracking (play count + last-start time) for skip-confidence; reset
    /// after a long idle gap (see [`Session`]).
    session: Session,
    /// Library-screen state: active tab, list cursor + multi-select anchor, local
    /// download-folder rows, and the pending file-delete confirmation.
    pub library_ui: LibraryView,
    /// Cross-frame cache of the visible library rows (dedup + filter are O(library) and
    /// used to run on every frame *and* every navigation event). Keyed on the source
    /// revisions/lengths + tab + filter; see `library_reducer`. Interior mutability so
    /// `library_rows(&self)` can refresh it.
    library_rows_cache: RefCell<Option<library_reducer::LibraryRowsCache>>,
    /// Same idea for the All-tab dedup count shown in the tab bar every frame.
    all_count_cache: Cell<Option<(library_reducer::AllCountKey, usize)>>,
    /// Memo for `recover_youtube_id`'s library title scan (per current track + library
    /// state) — media_snapshot re-asks it every second while playing a local/radio track.
    yid_scan_memo: RefCell<Option<player::YidMemo>>,
    /// The "add to playlist" picker popup, when open (from Library rows, Search results,
    /// or the Player's current track).
    pub playlist_picker: Option<PlaylistPicker>,
    /// Live pointer-interaction sessions (drag selections, seekbar/recording scrubs) and the
    /// held-key nav accelerator — see [`Interaction`]. All transient; cleared on button release.
    pub interaction: Interaction,

    // Lyrics ------------------------------------------------------------------
    /// Lyrics-panel state: visibility, in-flight flag, and the fetched track lyrics.
    pub lyrics: Lyrics,

    // Album art ---------------------------------------------------------------
    /// Album-art state: graphics picker, held render protocol, decoded source + dims,
    /// owning track id, and the in-flight flag.
    pub art: ArtState,

    // Downloads ---------------------------------------------------------------
    /// Download progress + source metadata, keyed by `video_id` (see [`Downloads`]).
    pub downloads: Downloads,
    /// Persisted manifest of completed downloads' YouTube identity + rich metadata, so a
    /// downloaded-and-online track keeps its share URL across restarts (see [`DownloadStore`]).
    /// Loaded by `main` after `new`.
    pub download_store: DownloadStore,

    // Prefetch ----------------------------------------------------------------
    /// Prefetch / load tracking: stream-URL cache, last-load-was-prefetched flag, and the
    /// `video_id` currently loaded into mpv (see [`Prefetch`]).
    prefetch: Prefetch,
    /// yt-dlp self-heal bookkeeping: the in-flight healed track, per-track one-shot
    /// guard, and the update-check cooldown clock (see [`YtdlpHeal`]).
    heal: YtdlpHeal,

    /// Render→reducer bridges: the active list viewport height and the per-list wheel-scroll
    /// offsets — all written by render (`&App`) for the reducer to read on the next event
    /// (see [`RenderBridges`]).
    pub bridges: RenderBridges,

    /// Last-rendered mouse hit map: the clickable button rects and the seekbar rect views
    /// publish each frame, kept behind [`HitMap`]'s method API so the reducer and views never
    /// touch the raw cells (extracted from [`RenderBridges`]).
    pub hits: HitMap,

    /// When the last radio re-sync (seek-to-live or reconnect) was issued. A second
    /// re-sync inside [`crate::app::player::RESYNC_RETRY_WINDOW`] while still behind
    /// means the seek didn't take, so the action escalates to a stream reconnect.
    pub(in crate::app) radio_resync_at: Option<Instant>,

    /// Animation clock and redraw-coalescing counters (frame counter, fractional draw credit +
    /// its cadence, and the last-drawn whole second / cache second) — see [`Animation`]. The
    /// one-shot `fx` feedback and the `focused` gate stay flat.
    pub anim: Animation,
    /// One-shot animation feedback: start frames for event-driven effects (toast reveal, track
    /// intro, volume flash, …) plus the last-observed values `update` diffs to fire them.
    /// See [`FxState`]; all of it is inert until the matching animation flags are enabled.
    pub fx: FxState,

    /// Whether the terminal currently holds input focus (DECSET ?1004, fed by [`Msg::Focus`]).
    /// Defaults to `true`, so terminals/multiplexers that never report focus animate exactly as
    /// before — the pause is strictly additive. Gates [`App::animation_active`].
    pub focused: bool,

    /// Shared text-zoom state (this handle is a clone of the one inside the terminal
    /// backend and the event translator — setting it rescales the next draw). Carries
    /// the mechanism detected at startup; when that is `None` the zoom actions explain
    /// themselves in a toast instead of silently doing nothing.
    pub zoom: crate::zoom::ZoomHandle,
}

impl App {
    // INVARIANT(PLAY-EPOCH-001): every position discontinuity bumps through this helper.
    pub(in crate::app) fn bump_position_epoch(&mut self, _reason: PositionEpochReason) {
        self.playback.position_epoch = self.playback.position_epoch.wrapping_add(1);
    }
}

#[cfg(test)]
mod hardening_tests;
#[cfg(test)]
mod tests;
