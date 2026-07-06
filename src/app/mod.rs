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

mod state;
pub use state::*;

mod ai_reducer;
mod artwork;
mod download;
mod keys;
mod library;
mod library_reducer;
mod media_reducer;
mod mouse;
pub use mouse::HitMap;
mod now_playing;
mod now_playing_reducer;
mod player;
mod playlists_reducer;
mod queue;
mod recorder_reducer;
mod remote_reducer;
mod romanize;
mod search;
mod settings_reducer;
mod stream_metadata;
mod streaming_reducer;

/// Autoplay/streaming top-up policy and the play-error breaker threshold — single-sourced
/// with the headless daemon in [`crate::playback_policy`] so no bound can drift between the
/// two playback owners. Re-exported so this module's submodules keep resolving the names.
pub(crate) use crate::playback_policy::{
    AUTOPLAY_COOLDOWN, AUTOPLAY_MAX_FAILURES, AUTOPLAY_THRESHOLD, MAX_CONSECUTIVE_PLAY_ERRORS,
    STREAMING_FALLBACK_COUNT, STREAMING_POOL_COUNT,
};
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
/// Cap on cached prefetched stream URLs (bounded memory; we only look a step ahead).
const RESOLVED_MAX: usize = 999;
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
    /// The normal-mode theme to restore after leaving dedicated Radio mode.
    normal_mode_theme: Option<ThemeConfig>,
    /// The Radio-mode theme to restore on the next dedicated Radio entry. Defaults to Dario
    /// until the user edits the theme while Radio mode is active.
    radio_mode_theme: Option<ThemeConfig>,
    /// The normal-mode queue to restore when leaving dedicated Radio mode.
    normal_mode_queue: Option<QueueSnapshot>,
    /// The Radio-mode queue to restore when entering dedicated Radio mode again.
    radio_mode_queue: Option<QueueSnapshot>,
    /// A pending confirmation before entering or leaving dedicated Radio mode.
    pub pending_radio_mode_confirm: Option<RadioModeConfirm>,
    /// Whether the `?` help / cheat-sheet overlay is shown.
    pub help_visible: bool,
    /// Whether the mouse cheat-sheet overlay is shown. Opened only from the footer mouse icon.
    pub mouse_help_visible: bool,
    /// A pending keybinding-conflict warning (Keys tab). When set, a modal popup is shown
    /// and the next key/click dismisses it; the attempted rebind is left unchanged.
    pub key_conflict: Option<Conflict>,
    /// A pending destructive/settings-wide confirmation. Enter/`y` confirms; Esc/`n` or the
    /// Cancel button backs out before the key can leak through to the settings list.
    pub pending_settings_confirm: Option<SettingsConfirm>,
    /// The "Import from Spotify" playlist picker overlay (Settings › Accounts). ↑/↓
    /// select, Enter imports, Esc closes.
    pub spotify_picker: Option<SpotifyPicker>,
    /// The radio-recording settings popup (over the Playback settings tab).
    pub recording_settings: Option<RecordingSettingsPopup>,
    /// The recordings browser (Decide-mode save/discard/play), opened from the popup or a key.
    pub recordings_browser: Option<RecordingsBrowser>,
    /// A transfer job is running (guards double-starts; progress rides the status line).
    pub transfer_running: bool,
    /// Whether the About card overlay is showing. Opened by clicking the `ytm-tui` brand in the
    /// nav bar or via `Action::ToggleAbout` (F1); any key/click (other than the GitHub link)
    /// dismisses it.
    pub about_visible: bool,
    /// The app icon as a render-ready protocol for the About card, decoded from the embedded PNG
    /// and cached with the popup background it was composited against. Native image-capable
    /// terminals reuse the detected picker for pixel quality; everything else falls back to
    /// half-blocks so the card still draws everywhere. `RefCell` because render only has `&App`.
    pub about_icon: RefCell<
        Option<(
            ratatui::style::Color,
            Option<ProtocolType>,
            StatefulProtocol,
        )>,
    >,
    /// Result of the background app-update check (`None` until it completes / when disabled).
    /// When `available`, the About card shows an upgrade notice and the nav brand gets a dot.
    pub update_status: Option<crate::update::UpdateStatus>,
    /// Whether the "Why DJ Gem" overlay is showing. Opened by `Action::WhyAi` (`w`) when the last
    /// autoplay-streaming refill went through the DJ Gem reranker; lists why each track was chosen (slot
    /// role + reason codes + confidence). Esc / `w` / Back dismiss it, like the About card.
    pub why_ai_visible: bool,
    /// The "what's playing" (지듣노) overlay — the radio identify card with favorite /
    /// ask-DJ Gem actions. `None` = closed. Opened by `Action::IdentifyNowPlaying` (`i`).
    pub now_playing_overlay: Option<NowPlayingOverlay>,
    /// Short-TTL cache of identify results keyed on (station, title), so re-opening on
    /// the same song and the "tell me more" handoff never re-spend an API call.
    pub(in crate::app) now_playing_cache: now_playing::NowPlayingCache,
    /// Identify epoch: replies must carry the open overlay's snapshot of this counter or
    /// they're stale (overlay closed / stream title moved on).
    now_playing_seq: u64,

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

    /// Last whole second we redrew for, so sub-second `time-pos` spam is coalesced.
    last_shown_sec: i64,
    /// Same coalescing for `demuxer-cache-time` (`-1` = none shown yet).
    last_shown_cache_sec: i64,
    /// When the last radio re-sync (seek-to-live or reconnect) was issued. A second
    /// re-sync inside [`crate::app::player::RESYNC_RETRY_WINDOW`] while still behind
    /// means the seek didn't take, so the action escalates to a stream reconnect.
    pub(in crate::app) radio_resync_at: Option<Instant>,

    /// Monotonic animation frame counter, bumped on each [`Msg::AnimTick`] (~30 fps) while
    /// animations are active. Drives every effect's phase; wraps harmlessly. `0` at rest.
    anim_frame: u64,
    /// One-shot animation feedback: start frames for event-driven effects (toast reveal, track
    /// intro, volume flash, …) plus the last-observed values `update` diffs to fire them.
    /// See [`FxState`]; all of it is inert until the matching animation flags are enabled.
    pub fx: FxState,
    /// Fractional redraw scheduler for animation frames. The phase can advance at the configured
    /// FPS while heavyweight effects redraw at a lower cadence, preserving motion timing without
    /// forcing the terminal compositor to repaint every logical tick.
    anim_draw_credit: u16,
    /// Last draw cadence used to interpret [`Self::anim_draw_credit`]. Reset when the active effect
    /// mix moves between cheap element effects, canvas effects, and the DJ Gem mascot.
    anim_last_draw_fps: u16,

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
    pub fn new(volume: i64) -> Self {
        Self {
            should_quit: false,
            dirty: true,
            mode: Mode::Player,
            authenticated: false,
            keymap: KeyMap::default(),
            theme: ThemeConfig::default(),
            radio_dedicated_mode: false,
            normal_mode_theme: None,
            radio_mode_theme: None,
            normal_mode_queue: None,
            radio_mode_queue: None,
            pending_radio_mode_confirm: None,
            help_visible: false,
            mouse_help_visible: false,
            key_conflict: None,
            pending_settings_confirm: None,
            spotify_picker: None,
            recording_settings: None,
            recordings_browser: None,
            transfer_running: false,
            about_visible: false,
            about_icon: RefCell::new(None),
            update_status: None,
            why_ai_visible: false,
            now_playing_overlay: None,
            now_playing_cache: now_playing::NowPlayingCache::default(),
            now_playing_seq: 0,
            playback: Playback {
                volume: volume.clamp(0, VOLUME_MAX),
                speed: 1.0,
                ..Default::default()
            },
            recorder: crate::recorder::RecorderState::default(),
            media_art: None,
            queue: Queue::default(),
            status: Status::default(),
            status_text_prev: String::new(),
            video: Video::default(),
            anim_frame: 0,
            fx: FxState::new(volume.clamp(0, VOLUME_MAX)),
            audio: AudioEq::default(),
            autoplay_streaming: false,
            dropdowns: Dropdowns::default(),
            queue_popup: QueuePopup::default(),
            search_filter: SearchFilterPopup::default(),
            config: Config::default(),
            settings: None,
            ai: AiState {
                available: false,
                model: GeminiModel::default(),
                messages: Vec::new(),
                input: String::new(),
                select_all: false,
                thinking: false,
                suggestions: Vec::new(),
                suggestions_selected: 0,
                focus: AiFocus::Input,
            },
            romanization: RomanizationRuntime::default(),
            streaming: StreamingRuntime::default(),
            consecutive_play_errors: 0,
            playlists: Playlists::default(),
            station: StationStore::default(),
            search: SearchState {
                input: String::new(),
                source: SearchSource::Youtube,
                select_all: false,
                focus: SearchFocus::Input,
                results: Vec::new(),
                selected: 0,
                kind: SearchKind::default(),
                searching: false,
                request_id: 0,
            },
            library: Library::default(),
            signals: Signals::default(),
            session: Session::default(),
            library_ui: LibraryView::default(),
            library_rows_cache: RefCell::new(None),
            all_count_cache: Cell::new(None),
            yid_scan_memo: RefCell::new(None),
            playlist_picker: None,
            interaction: Interaction::default(),
            lyrics: Lyrics::default(),
            art: ArtState::default(),
            downloads: Downloads::default(),
            download_store: DownloadStore::default(),
            prefetch: Prefetch::default(),
            heal: YtdlpHeal::default(),
            bridges: RenderBridges::default(),
            hits: HitMap::default(),
            last_shown_sec: -1,
            last_shown_cache_sec: -1,
            radio_resync_at: None,
            anim_draw_credit: 0,
            anim_last_draw_fps: 0,
            focused: true,
            zoom: crate::zoom::ZoomHandle::default(),
        }
    }

    /// Push persisted playback/EQ settings into the app after construction. Kept separate
    /// from `new` (whose `volume`-only signature many tests rely on) so `main` can apply
    /// the full config without churning those call sites.
    pub fn apply_config(&mut self, cfg: &Config) {
        self.audio.preset = cfg.eq_preset;
        self.audio.bands = cfg.effective_eq_bands();
        self.audio.normalize = cfg.effective_normalize();
        self.playback.speed = cfg.effective_speed();
        self.audio.seek_seconds = cfg.effective_seek_seconds();
        self.queue.set_shuffle(cfg.effective_shuffle());
        self.queue.repeat = cfg.effective_repeat();
        // Music-mode invariant (single-sourced with the daemon): streaming and repeat can't
        // both be on; a legacy/hand-edited config carrying both drops streaming, keeping the
        // more deliberate repeat. `self.queue.repeat` was just set from the same config above.
        self.autoplay_streaming = crate::playback_policy::streaming_enabled_with_repeat(
            cfg.effective_autoplay_streaming(),
            self.queue.repeat,
        );
        self.ai.available = cfg.effective_ai_key().is_some();
        self.ai.model = cfg.effective_gemini_model();
        self.keymap = KeyMap::from_config(cfg);
        let normal_theme = cfg.effective_theme();
        // Seed the radio-mode theme stash from its persisted slot. Guarded so a config
        // without one never clobbers a theme picked live earlier in this session.
        if let Some(radio_theme) = cfg.effective_radio_theme() {
            self.radio_mode_theme = Some(radio_theme);
        }
        if self.radio_dedicated_mode {
            self.normal_mode_theme = Some(normal_theme);
            self.theme = self
                .radio_mode_theme
                .clone()
                .unwrap_or_else(ThemeConfig::dario);
        } else {
            self.theme = normal_theme;
        }
        let search =
            Self::search_config_for_radio_mode(cfg.effective_search(), self.radio_dedicated_mode);
        self.search.source = search.normalized_source(search.source);
        // Keep the process-wide UI language in sync with the applied config (this is the
        // central apply path, called at startup and after a settings save).
        crate::i18n::set_language(cfg.effective_language());
        // Same for the DJ Gem reply language (resolved: retro → English, `Auto` → the UI
        // language). The AI actor reads this global when building its system prompt.
        crate::i18n::set_dj_gem_language(cfg.effective_dj_gem_language());
        // Restore the persisted text zoom, but only on terminals with a working zoom
        // mechanism — a config written under kitty must not garble a later tmux session.
        // (`set` snaps to the mode's levels, so kitty's 150% reads as 200% on a
        // double-size-line terminal rather than getting lost.)
        if self.zoom.supported() {
            self.zoom.set(cfg.effective_text_zoom());
        }
        // Keep the full config so the settings screen can persist the whole file.
        self.config = cfg.clone();
        self.ensure_radio_mode_constraints();
    }

    pub fn search_config_for_mode(&self) -> SearchConfig {
        Self::search_config_for_radio_mode(
            self.config.effective_search(),
            self.radio_dedicated_mode,
        )
    }

    fn search_config_for_radio_mode(
        mut search: SearchConfig,
        streaming_mode: bool,
    ) -> SearchConfig {
        if streaming_mode {
            search.youtube = false;
            search.soundcloud = false;
            search.audius = false;
            search.jamendo = false;
            search.internet_archive = false;
            search.radio_browser = true;
            search.source = SearchSource::RadioBrowser;
        } else {
            search.radio_browser = false;
            if search.source == SearchSource::RadioBrowser {
                search.source = SearchSource::Youtube;
            }
        }
        search.normalized()
    }

    pub fn library_tabs(&self) -> &'static [LibraryTab] {
        if self.radio_dedicated_mode {
            &LibraryTab::RADIO_MODE
        } else {
            &LibraryTab::NORMAL
        }
    }

    pub fn library_tab_available(&self, tab: LibraryTab) -> bool {
        self.library_tabs().contains(&tab)
    }

    pub(in crate::app) fn next_library_tab(
        &self,
        current: LibraryTab,
        forward: bool,
    ) -> LibraryTab {
        let tabs = self.library_tabs();
        let i = tabs.iter().position(|&tab| tab == current).unwrap_or(0);
        let n = tabs.len();
        if n == 0 {
            return LibraryTab::All;
        }
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        tabs[j]
    }

    pub(in crate::app) fn ensure_radio_mode_constraints(&mut self) {
        if !self.library_tab_available(self.library_ui.tab) {
            self.library_ui.tab = self.library_tabs()[0];
            self.reset_playlist_ui_state();
            self.clear_library_filter();
        }
        let search = self.search_config_for_mode();
        self.search.source = search.normalized_source(self.search.source);
        if self.radio_dedicated_mode {
            self.dropdowns.search_source_open = false;
            self.why_ai_visible = false;
        }
    }

    pub(in crate::app) fn request_radio_mode_switch(&mut self) -> Vec<Cmd> {
        self.pending_radio_mode_confirm = Some(if self.radio_dedicated_mode {
            RadioModeConfirm::Exit
        } else {
            RadioModeConfirm::Enter
        });
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.queue_popup.open = false;
        self.search_filter.close();
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn apply_radio_mode_confirm(
        &mut self,
        confirm: RadioModeConfirm,
    ) -> Vec<Cmd> {
        self.pending_radio_mode_confirm = None;
        match confirm {
            RadioModeConfirm::Enter => self.enter_radio_dedicated_mode(),
            RadioModeConfirm::Exit => self.exit_radio_dedicated_mode(),
        }
    }

    fn enter_radio_dedicated_mode(&mut self) -> Vec<Cmd> {
        if self.radio_dedicated_mode {
            return Vec::new();
        }
        self.normal_mode_theme = Some(self.theme.clone());
        self.normal_mode_queue = Some(self.queue.snapshot());
        self.activate_radio_dedicated_mode_ui();
        let restore = self.radio_mode_queue.take();
        let cmds = self.stop_clear_and_restore_queue_for_mode_switch(restore);
        self.status.kind = StatusKind::Info;
        self.status.text = t!("Radio mode enabled", "라디오 모드 켜짐").to_owned();
        self.dirty = true;
        cmds
    }

    fn exit_radio_dedicated_mode(&mut self) -> Vec<Cmd> {
        if !self.radio_dedicated_mode {
            return Vec::new();
        }
        self.radio_mode_theme = Some(self.theme.clone());
        self.radio_mode_queue = Some(self.queue.snapshot());
        self.radio_dedicated_mode = false;
        self.theme = self
            .normal_mode_theme
            .take()
            .unwrap_or_else(|| self.config.effective_theme());
        if !self.library_tab_available(self.library_ui.tab) {
            self.library_ui.tab = LibraryTab::All;
            self.clear_library_filter();
        }
        let search = self.search_config_for_mode();
        self.search.source = search.normalized_source(self.config.effective_search().source);
        self.search.searching = false;
        self.search.results.clear();
        self.search.selected = 0;
        self.dropdowns.search_source_open = false;
        let restore = self.normal_mode_queue.take();
        let cmds = self.stop_clear_and_restore_queue_for_mode_switch(restore);
        self.status.kind = StatusKind::Info;
        self.status.text = t!("Radio mode disabled", "라디오 모드 꺼짐").to_owned();
        self.dirty = true;
        cmds
    }

    fn activate_radio_dedicated_mode_ui(&mut self) {
        self.radio_dedicated_mode = true;
        self.theme = self
            .radio_mode_theme
            .clone()
            .unwrap_or_else(ThemeConfig::dario);
        self.search.source = SearchSource::RadioBrowser;
        self.search.searching = false;
        self.search.results.clear();
        self.search.selected = 0;
        self.search.focus = SearchFocus::Input;
        self.bridges.search_scroll.reset();
        self.library_ui.tab = LibraryTab::RadioFavorites;
        self.clear_library_filter();
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.ensure_radio_mode_constraints();
        self.dirty = true;
    }

    fn stop_clear_and_restore_queue_for_mode_switch(
        &mut self,
        restore: Option<QueueSnapshot>,
    ) -> Vec<Cmd> {
        self.queue.set(Vec::new(), 0);
        self.queue_popup.open = false;
        self.queue_popup.cursor = 0;
        self.queue_popup.anchor = 0;
        self.search_filter.close();
        self.streaming.pending = false;
        self.streaming.pending_rerank = None;
        self.ai.thinking = false;
        let mut cmds = self.load_song(None);
        self.art.force_clear_next_frame = true;
        cmds.push(Cmd::Player(PlayerCmd::Stop));
        if let Some(snapshot) = restore {
            self.queue.restore_snapshot(snapshot);
            if let Some(song) = self.queue.current().cloned() {
                self.playback.paused = false;
                cmds.extend(self.load_song(Some(song)));
            }
        }
        cmds
    }

    pub(in crate::app) fn sync_playback_modes_to_config(&mut self) {
        self.config.shuffle = Some(self.queue.shuffle);
        self.config.repeat = self.queue.repeat;
        self.config.autoplay_streaming = Some(self.autoplay_streaming);
    }

    pub(in crate::app) fn save_playback_modes_cmd(&mut self) -> Cmd {
        self.sync_playback_modes_to_config();
        Cmd::SaveConfig(Box::new(self.config.clone()))
    }

    /// Live retro-mode flag. While Settings is open, the draft is what the user is looking at,
    /// so render from it before the value is committed to config on close.
    pub fn retro_mode(&self) -> bool {
        self.settings.as_ref().map_or_else(
            || self.config.effective_retro_mode(),
            |s| s.draft.retro_mode,
        )
    }

    /// Seed the player with the last locally recorded track, without starting playback.
    /// This gives a fresh launch something useful to show while keeping autoplay opt-in.
    pub fn restore_last_session_from_library(&mut self, radio_mode: bool) {
        if radio_mode {
            self.restore_last_radio_from_library();
        } else {
            self.restore_last_played_from_library();
        }
    }

    pub fn restore_last_played_from_library(&mut self) {
        if !self.queue.is_empty() {
            return;
        }
        let Some(song) = self.library.history.front().cloned() else {
            return;
        };
        self.seed_restored_queue(song);
    }

    /// Restore dedicated Radio mode and seed the last played radio station, without starting
    /// playback. The station itself comes from the persisted radio history.
    pub fn restore_last_radio_from_library(&mut self) {
        if !self.radio_dedicated_mode {
            self.normal_mode_theme = Some(self.theme.clone());
        }
        self.activate_radio_dedicated_mode_ui();
        if !self.queue.is_empty() {
            return;
        }
        let Some(station) = self.library.radios.front().cloned() else {
            return;
        };
        self.seed_restored_queue(station);
    }

    fn seed_restored_queue(&mut self, song: Song) {
        self.queue.set(vec![song], 0);
        self.seed_restored_playback_state();
    }

    /// Build the persisted session cache from the active queue plus the inactive mode's stashed
    /// queue. This is the handoff used by both the next TUI launch and the headless daemon.
    pub fn session_cache_snapshot(&self) -> crate::session::SessionCache {
        let mut cache = crate::session::SessionCache::from_radio_mode(self.radio_dedicated_mode);
        if self.radio_dedicated_mode {
            cache.radio_queue = Some(self.queue.snapshot());
            cache.normal_queue = self.normal_mode_queue.clone();
        } else {
            cache.normal_queue = Some(self.queue.snapshot());
            cache.radio_queue = self.radio_mode_queue.clone();
        }
        cache
    }

    /// Restore an exact queue snapshot when one exists; fall back to the legacy library-history
    /// restore path for old session files.
    pub fn restore_last_session_from_cache(&mut self, cache: &crate::session::SessionCache) {
        self.normal_mode_queue = cache.normal_queue.clone();
        self.radio_mode_queue = cache.radio_queue.clone();

        if cache.was_radio_mode() {
            self.activate_radio_dedicated_mode_ui();
        }

        if let Some(snapshot) = cache.active_queue().cloned() {
            self.queue.restore_snapshot(snapshot);
            self.seed_restored_playback_state();
            return;
        }

        self.restore_last_session_from_library(cache.was_radio_mode());
    }

    fn seed_restored_playback_state(&mut self) {
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.playback.position_epoch = self.playback.position_epoch.wrapping_add(1);
        self.playback.duration = None;
        self.playback.paused = true;
        self.playback.stream_now_playing = None;
        self.last_shown_sec = -1;
        self.prefetch.loaded_video_id = None;
        self.status.text.clear();
        self.dirty = true;
    }

    /// Opt-in: when "autoplay on launch" is enabled and [`restore_last_played_from_library`]
    /// seeded a track, start playing it at launch — the same path pressing play would take
    /// (load → record → prefetch). Returns no commands when the setting is off or nothing was
    /// restored, leaving the queue paused and idle (the default). Called once at startup.
    ///
    /// [`restore_last_played_from_library`]: Self::restore_last_played_from_library
    pub fn autoplay_on_start_cmds(&mut self) -> Vec<Cmd> {
        if !self.config.effective_autoplay_on_start() || !self.current_needs_load() {
            return Vec::new();
        }
        // Optimistic: mpv will confirm via a `pause` property-change once the track opens.
        self.playback.paused = false;
        let song = self.queue.current().cloned();
        self.load_song(song)
    }

    /// The reducer: apply one message, returning effects for the run loop to dispatch.
    /// Reducer entry point. Wraps [`Self::dispatch`] to centrally track when a transient
    /// `status` notification is set or cleared (any of the ~40 `self.status.text = …` sites), so
    /// the main loop can expire it after [`STATUS_TTL`] and bring the song title back —
    /// without each call site having to remember to arm a timer. See [`Self::status_visible`].
    pub fn update(&mut self, msg: Msg) -> Vec<Cmd> {
        let mut status_before = std::mem::take(&mut self.status_text_prev);
        status_before.clear();
        status_before.push_str(&self.status.text);
        let kind_before = self.status.kind;
        let paused_before = self.playback.paused;
        // Default this turn's status to the error styling; the few positive handlers override
        // it to `Info` while they run. This keeps the kind in lock-step with the status text:
        // an error set by one of the ~40 plain `self.status.text = …` sites can never inherit a
        // leftover `Info` color from a previous green toast.
        self.status.kind = StatusKind::Error;
        let cmds = self.dispatch(msg);
        let status_changed = self.status.text != status_before;
        if status_changed {
            self.status.set_at = if self.status.text.is_empty() {
                None
            } else {
                Some(Instant::now())
            };
        } else {
            // Text unchanged this turn — keep the color the still-showing message already had.
            self.status.kind = kind_before;
        }
        // Media-session position clock, kept centrally so no seek/pause site can forget it:
        // any seek command emitted this turn is a position discontinuity (bump the epoch so
        // the OS session re-announces the position), and any pause/resume flip rebases the
        // interpolation anchor so a long pause never reads as elapsed progress.
        let seeked = cmds.iter().any(|cmd| {
            matches!(
                cmd,
                Cmd::Player(PlayerCmd::SeekRelative(_) | PlayerCmd::SeekAbsolute(_))
            )
        });
        if seeked {
            self.playback.position_epoch = self.playback.position_epoch.wrapping_add(1);
        }
        if self.playback.paused != paused_before {
            self.playback.time_pos_at = Some(Instant::now());
        }
        // One-shot animation feedback, detected centrally for the same reason as the status
        // TTL above: every input path (key, mouse, remote, DJ Gem) changes the same state, so
        // diffing it here means no call site can forget to trigger the matching effect.
        self.detect_fx(status_changed, seeked);
        self.sync_art_overlay_state();
        self.status_text_prev = status_before; // return the buffer's capacity for next turn
        cmds
    }

    /// Whether a transient status is currently covering the title (drives the main loop's
    /// expiry tick — see [`Msg::StatusTick`]).
    pub fn status_visible(&self) -> bool {
        self.status.set_at.is_some()
    }

    /// Step the text zoom one notch up or down (Ctrl+wheel / Ctrl+-/=). On terminals
    /// without the text sizing protocol this explains itself in a toast instead of
    /// silently doing nothing — the keys are advertised in the cheat-sheet, so a dead
    /// key would read as a bug.
    pub(in crate::app) fn zoom_step(&mut self, zoom_in: bool) -> Vec<Cmd> {
        self.status.kind = StatusKind::Info;
        if !self.zoom.supported() {
            self.status.text = t!(
                "This terminal can't scale text (kitty 0.40+, Windows Terminal, …)",
                "이 터미널은 글자 확대를 지원하지 않아요 (kitty 0.40+, Windows Terminal 등 가능)"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        let current = self.zoom.percent();
        let next = self.zoom.step(zoom_in);
        if next == current {
            self.status.text = if zoom_in {
                let max = self.zoom.max_percent();
                if crate::i18n::is_korean() {
                    format!("이미 최대 글자 크기예요 ({max}%)")
                } else {
                    format!("Text is already at its largest ({max}%)")
                }
            } else {
                t!(
                    "Text is back to its normal size (100%)",
                    "기본 글자 크기예요 (100%)"
                )
                .to_owned()
            };
            self.dirty = true;
            return Vec::new();
        }
        self.zoom.set(next);
        self.status.text = if crate::i18n::is_korean() {
            format!("글자 크기 {next}%")
        } else {
            format!("Text size {next}%")
        };
        // The virtual grid just changed size: force the full VT-clear redraw path so no
        // scaled multicells (or native-image placements) from the old grid survive.
        self.request_native_image_clear();
        self.config.text_zoom = Some(next);
        vec![Cmd::SaveConfig(Box::new(self.config.clone()))]
    }

    /// Toggle the Ctrl+wheel zoom lock (`ToggleZoomWheelLock`). Persisted, so a
    /// deliberately frozen gesture stays frozen across sessions.
    pub(in crate::app) fn toggle_zoom_wheel_lock(&mut self) -> Vec<Cmd> {
        let locked = !self.config.effective_zoom_wheel_lock();
        self.config.zoom_wheel_lock = Some(locked);
        self.status.kind = StatusKind::Info;
        self.status.text = if locked {
            t!(
                "Ctrl+wheel zoom locked (Ctrl+-/= still zoom)",
                "Ctrl+휠 확대 잠금 켜짐 (Ctrl+-/= 는 그대로 동작)"
            )
        } else {
            t!("Ctrl+wheel zoom unlocked", "Ctrl+휠 확대 잠금 꺼짐")
        }
        .to_owned();
        self.dirty = true;
        vec![Cmd::SaveConfig(Box::new(self.config.clone()))]
    }

    fn dispatch(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::Noop => return Vec::new(),
            Msg::Key(k) => return self.on_key(k),
            Msg::MouseClick { col, row } => return self.on_mouse_click(col, row),
            Msg::MouseDoubleClick { col, row } => return self.on_mouse_double_click(col, row),
            Msg::MouseRightClick { col, row } => return self.on_mouse_right_click(col, row),
            Msg::MouseDrag { col, row } => return self.on_mouse_drag(col, row),
            Msg::MouseLeftUp => return self.on_mouse_left_up(),
            Msg::MouseScroll { up, col, row, ctrl } => {
                return self.on_mouse_scroll(up, col, row, ctrl);
            }
            Msg::Resize => self.dirty = true,
            Msg::Quit => self.should_quit = true,
            Msg::Remote(cmd, reply) => {
                let (resp, cmds) = self.apply_remote(cmd);
                let _ = reply.send(resp);
                return cmds;
            }
            Msg::Media(cmd) => return self.apply_media(cmd),
            Msg::MediaArtworkReady(ready) => {
                // No redraw: this only feeds the OS media session, not the TUI.
                self.media_art = Some(ready);
            }
            Msg::Autoplay => return self.autoplay_on_start_cmds(),
            Msg::ApiModeResolved { mode, had_cookie } => {
                self.authenticated = mode == ApiMode::Authenticated;
                if mode == ApiMode::Anonymous && had_cookie {
                    self.status.text = crate::t!(
                        "Cookie rejected — anonymous mode (search & play only)",
                        "쿠키가 거부됨 — 익명 모드 (검색·재생만 가능)"
                    )
                    .to_owned();
                }
                self.dirty = true;
                let results = self.search.results.clone();
                return self.request_romanization_for_songs(&results);
            }
            Msg::StatusTick => {
                // The status has been covering the title long enough — clear it so the
                // wrapper above nulls `status.set_at` and the next frame redraws the title.
                if matches!(self.status.set_at, Some(t) if t.elapsed() >= STATUS_TTL) {
                    self.status.text.clear();
                    self.dirty = true;
                }
            }
            Msg::AnimTick => {
                // Advance the logical animation phase on every configured tick, but only request
                // an actual terminal redraw when the active effect mix is due. This keeps visual
                // timing stable while cutting the expensive render/terminal/compositor path.
                self.advance_animation();
            }
            Msg::Focus(f) => {
                // Terminal focus toggled. `animation_active()` reads `focused` to park the ~30 fps
                // tick while we're hidden; one redraw repaints cleanly on the transition (freeze a
                // tidy frame on blur, resume instantly on focus). The seekbar keeps advancing via
                // `PlayerTimePos`, which is independent of this tick.
                self.focused = f;
                self.dirty = true;
            }
            Msg::PlayerTimePos(t) => {
                // Normalize at the mpv trust boundary: a NaN/inf/negative time-pos must not
                // reach the interpolation clock, the OS media session, or the seekbar gauge.
                let t = crate::playback_policy::norm_position(t);
                self.playback.time_pos = Some(t);
                self.playback.time_pos_at = Some(Instant::now());
                // Real progress means the current track opened and is playing, so the
                // auto-skip streak is broken — clear it.
                if t > 0.0 {
                    self.consecutive_play_errors = 0;
                }
                // Redraw at most once per second; mpv emits `time-pos` far more often.
                let sec = t as i64;
                if sec != self.last_shown_sec {
                    self.last_shown_sec = sec;
                    self.dirty = true;
                    tracing::debug!(time_pos = t, "progress");
                }
            }
            Msg::PlayerDuration(d) => {
                self.playback.duration = Some(crate::playback_policy::norm_duration(d));
                self.dirty = true;
            }
            Msg::PlayerCacheTime(t) => {
                let t = t.map(crate::playback_policy::norm_position);
                let had = self.playback.cache_time.is_some();
                self.playback.cache_time = t;
                self.playback.cache_time_at = t.map(|_| Instant::now());
                // Redraw at most once per second (mpv reports far more often), plus on
                // Some↔None transitions so the live-sync glyph never shows stale state.
                let sec = t.map_or(-1, |v| v as i64);
                if sec != self.last_shown_cache_sec || had != t.is_some() {
                    self.last_shown_cache_sec = sec;
                    self.dirty = true;
                }
            }
            Msg::PlayerAudioCodec(codec) => {
                // Passthrough container hint for the recorder; no redraw needed.
                self.playback.audio_codec = codec;
            }
            Msg::PlayerFileFormat(format) => {
                self.playback.file_format = format;
            }
            Msg::RecordingTick => {
                return self.recorder_on_tick();
            }
            Msg::Recorder(event) => {
                return self.on_recorder_event(event);
            }
            Msg::PlayerPaused(p) => {
                self.playback.paused = p;
                self.dirty = true;
            }
            Msg::PlayerVolume(v) => {
                // A non-finite report is ignored (leave the current level) rather than
                // muting (`NaN.round() as i64` == 0) or storing a garbage level.
                if let Some(volume) = crate::playback_policy::norm_volume_event(v) {
                    self.playback.volume = volume;
                    self.dirty = true;
                    tracing::info!(volume = self.playback.volume, "volume");
                }
            }
            Msg::PlayerMetadata(metadata) => {
                let parsed = self.queue.current().cloned().and_then(|song| {
                    if !song.is_radio_station() {
                        return None;
                    }
                    let station_label = self.display_song_label(&song);
                    stream_metadata::parse_stream_now_playing(
                        &metadata,
                        &[song.title.as_str(), station_label.as_str()],
                    )
                });
                if self.playback.stream_now_playing != parsed {
                    self.playback.stream_now_playing = parsed.clone();
                    self.dirty = true;
                    // Rotate the recorder first (finalize the ended track, start the next),
                    // then let the overlay re-populate from the fresh ICY title (a
                    // favorite-resolve in flight for the old title is now stale).
                    let mut cmds = self.recorder_on_title(parsed.as_ref());
                    cmds.extend(self.on_stream_title_changed());
                    return cmds;
                }
            }
            Msg::TrackResolved { seq, result } => {
                return self.on_track_resolved(seq, result);
            }
            Msg::PlayerEof => {
                tracing::info!("track ended (eof)");
                // The just-finished track played to its end → a full-play signal, then advance.
                let mut cmds = self.record_outgoing(true);
                cmds.extend(self.advance(true));
                return cmds;
            }
            Msg::VideoOverlay { generation, event } => {
                return self.on_video_overlay_event(generation, event);
            }
            Msg::PlaylistTracks {
                title,
                intent,
                songs,
            } => {
                return self.on_playlist_tracks(title, intent, songs);
            }
            Msg::PlaylistTracksError { title, error } => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!("{title}: {error}");
                self.dirty = true;
            }
            Msg::PlayerError(e) => {
                // Log *which* track failed and whether it came from a (possibly stale)
                // prefetched URL. `e` already carries mpv's own reason (its `file_error`
                // end-file field — the closest thing to a "why": HTTP 403, unsupported, …).
                let failed = self
                    .queue
                    .current()
                    .map(|s| format!("{} — {}", s.title, s.artist));
                tracing::warn!(
                    error = %e,
                    track = failed.as_deref().unwrap_or("?"),
                    prefetched = self.prefetch.last_load_prefetched,
                    "playback error"
                );
                let extraction = crate::tools::looks_like_extraction_failure(&e);
                // Self-heal: an extraction-shaped failure on a yt-dlp-resolved track is
                // the stale-yt-dlp signature. Update it in the background and retry this
                // track ONCE — via a resolver-resolved direct URL, because the session
                // mpv keeps its spawn-time ytdl_path (see player::mpv::spawn docs).
                // Deliberately does not touch `consecutive_play_errors`: the heal is an
                // extra chance, not a substitute for the circuit breaker.
                if extraction
                    && self.heal.pending_video_id.is_none()
                    && let Some(song) = self.queue.current()
                    && song.prefetch_target().is_some()
                    && !self.heal.attempted.contains(&song.video_id)
                    && self
                        .heal
                        .last_check
                        .is_none_or(|at| at.elapsed() >= crate::tools::HEAL_COOLDOWN)
                {
                    let video_id = song.video_id.clone();
                    // Bound the per-track guard set: after enough distinct healed tracks in one
                    // long session, reset it (a re-heal is at worst one wasted retry).
                    if self.heal.attempted.len() >= crate::playback_policy::HEAL_ATTEMPTED_MAX {
                        self.heal.attempted.clear();
                    }
                    self.heal.attempted.insert(video_id.clone());
                    self.heal.last_check = Some(Instant::now());
                    self.heal.pending_video_id = Some(video_id.clone());
                    // The failed load may itself have been a stale prefetched URL —
                    // drop it so the retry resolves fresh with the updated binary.
                    self.prefetch.resolved.remove(&video_id);
                    self.status.kind = StatusKind::Info;
                    self.status.text = t!(
                        "Stream resolution failed — updating yt-dlp…",
                        "스트림 해석 실패 — yt-dlp 업데이트 중…"
                    )
                    .to_owned();
                    self.dirty = true;
                    return vec![Cmd::YtdlpSelfHeal {
                        video_id,
                        tools: self.config.tools.clone(),
                    }];
                }
                self.consecutive_play_errors = self.consecutive_play_errors.saturating_add(1);
                // A single bad track shouldn't strand the user: skip it and play on. The
                // cursor moves, so the title refreshes to the next track. Bail out once too
                // many fail in a row (offline / bad cookie) so we don't skip-storm.
                if self.consecutive_play_errors <= MAX_CONSECUTIVE_PLAY_ERRORS
                    && self.queue.peek_next().is_some()
                {
                    // `advance(false)` always moves on (ignores repeat-one), unlike an EOF.
                    let cmds = self.advance(false);
                    self.status.text = if extraction {
                        t!(
                            "⚠ Couldn't resolve the stream (yt-dlp may be outdated) — skipped",
                            "⚠ 스트림 해석 실패 (yt-dlp가 오래됐을 수 있음) — 건너뜀"
                        )
                    } else {
                        t!(
                            "⚠ Track unavailable — skipped to next",
                            "⚠ 재생할 수 없는 곡 — 다음 곡으로 건너뜀"
                        )
                    }
                    .to_owned();
                    self.dirty = true;
                    return cmds;
                }
                self.status.text = if self.consecutive_play_errors > MAX_CONSECUTIVE_PLAY_ERRORS {
                    if extraction {
                        t!(
                            "Several tracks failed — run `ytt tools reset --playback`, then `ytt doctor --verbose` if it continues.",
                            "여러 곡 재생 실패 — `ytt tools reset --playback` 실행 후 계속되면 `ytt doctor --verbose`를 확인하세요."
                        ).to_owned()
                    } else {
                        t!(
                            "Several tracks failed to play — stopped. Check your connection, or sign in (cookies) for gated tracks.",
                            "여러 곡 재생에 실패해서 중단했어요. 연결을 확인하거나, 제한된 곡은 로그인(쿠키)하세요."
                        ).to_owned()
                    }
                } else {
                    format!("{}: {e}", t!("Playback error", "재생 오류"))
                };
                self.dirty = true;
            }
            Msg::YtdlpHealResult { video_id, updated } => {
                if self.heal.pending_video_id.as_deref() != Some(video_id.as_str()) {
                    return Vec::new(); // stale: the user already moved on
                }
                let still_current = self.queue.current().is_some_and(|s| s.video_id == video_id);
                if updated && still_current {
                    // A fresh binary landed. Resolve a direct URL with it (Msg::Resolved
                    // below finishes the retry); Msg::ResolveFailed ends the heal.
                    let watch_url = self.queue.current().and_then(Song::prefetch_target);
                    if let Some(watch_url) = watch_url {
                        return vec![Cmd::Resolve {
                            video_id,
                            watch_url,
                        }];
                    }
                }
                // No update available / track changed — give up on this heal and skip
                // like the plain error path would have.
                self.heal.pending_video_id = None;
                if !still_current {
                    return Vec::new();
                }
                self.consecutive_play_errors = self.consecutive_play_errors.saturating_add(1);
                let cmds = if self.queue.peek_next().is_some() {
                    self.advance(false)
                } else {
                    Vec::new()
                };
                self.status.kind = StatusKind::Error;
                self.status.text = t!(
                    "⚠ Couldn't resolve the stream (yt-dlp may be outdated) — skipped",
                    "⚠ 스트림 해석 실패 (yt-dlp가 오래됐을 수 있음) — 건너뜀"
                )
                .to_owned();
                self.dirty = true;
                return cmds;
            }
            Msg::ResolveFailed { video_id } => {
                // Only meaningful while a self-heal retry waits on this exact resolve;
                // ordinary prefetch failures were already logged by the resolver.
                if self.heal.pending_video_id.as_deref() != Some(video_id.as_str()) {
                    return Vec::new();
                }
                self.heal.pending_video_id = None;
                if self.queue.current().is_none_or(|s| s.video_id != video_id) {
                    return Vec::new();
                }
                self.consecutive_play_errors = self.consecutive_play_errors.saturating_add(1);
                let cmds = if self.queue.peek_next().is_some() {
                    self.advance(false)
                } else {
                    Vec::new()
                };
                self.status.kind = StatusKind::Error;
                self.status.text = t!(
                    "⚠ Couldn't resolve the stream (yt-dlp may be outdated) — skipped",
                    "⚠ 스트림 해석 실패 (yt-dlp가 오래됐을 수 있음) — 건너뜀"
                )
                .to_owned();
                self.dirty = true;
                return cmds;
            }
            Msg::SearchResults {
                request_id,
                query,
                songs,
                timed_out,
                ..
            } => {
                // Drop results from a superseded search: a slow older response must never
                // overwrite a newer one, even after the newer one already cleared `searching`.
                // The request id is authoritative — comparing the live `input`/`source` would
                // wrongly reject the current search's results the moment the user types more
                // (or changes the source) without submitting, since those change without a
                // new request.
                if request_id != self.search.request_id {
                    return Vec::new();
                }
                self.search.searching = false;
                // The filter popup indexes into the rows it opened over; a fresh result
                // set makes those stale, so it closes rather than filtering the new list.
                self.search_filter.close();
                if songs.is_empty() {
                    self.status.text = if crate::i18n::is_korean() {
                        format!("\"{query}\" 검색 결과 없음")
                    } else {
                        format!("No results for \"{query}\"")
                    };
                    self.search.results.clear();
                } else {
                    // A partial result set (the operation deadline dropped a slow source) gets a
                    // subtle note so it doesn't read as the complete set; a full result clears it.
                    self.status.text = if timed_out {
                        t!("Some sources timed out", "일부 소스 시간 초과").to_string()
                    } else {
                        String::new()
                    };
                    self.search.results = songs;
                    self.search.selected = 0;
                    self.bridges.search_scroll.reset();
                    self.search.focus = SearchFocus::Results;
                }
                self.dirty = true;
            }
            Msg::SearchError {
                request_id, error, ..
            } => {
                // Same stale-guard as SearchResults: a failed older search must not clear the
                // status or `searching` flag of a newer one still in flight.
                if request_id != self.search.request_id {
                    return Vec::new();
                }
                self.search.searching = false;
                self.status.text = format!("{}: {error}", t!("Search error", "검색 오류"));
                self.dirty = true;
            }
            Msg::DownloadsScanned(songs) => {
                self.library_ui.downloaded_rev = self.library_ui.downloaded_rev.wrapping_add(1);
                self.library_ui.downloaded = self.enrich_downloads(songs);
                let len = self.library_len();
                if self.library_ui.selected >= len {
                    self.library_ui.selected = len.saturating_sub(1);
                }
                self.dirty = true;
                let downloaded = self.library_ui.downloaded.clone();
                return self.request_romanization_for_songs(&downloaded);
            }
            Msg::LyricsResult { video_id, lines } => {
                self.lyrics.loading = false;
                // Ignore stale results for a track we've already skipped past.
                if self.queue.current().is_some_and(|s| s.video_id == video_id) {
                    self.lyrics.track = Some(TrackLyrics { video_id, lines });
                    self.dirty = true;
                }
            }
            Msg::ArtworkResult { video_id, image } => {
                self.art.loading = false;
                // Drop results for a track we've already skipped past.
                if self.queue.current().is_some_and(|s| s.video_id == video_id) {
                    self.set_artwork(video_id, image);
                    self.dirty = true;
                }
            }
            Msg::ArtworkResized(response) => self.apply_artwork_resize(response),
            Msg::DownloadProgress { video_id, percent } => {
                let percent = percent.round() as u8;
                let changed = !matches!(
                    self.downloads.active.get(&video_id),
                    Some(DownloadState::Running(prev)) if *prev == percent
                );
                if changed {
                    self.downloads
                        .active
                        .insert(video_id, DownloadState::Running(percent));
                    self.dirty = true;
                }
            }
            Msg::DownloadDone { video_id, path } => {
                self.downloads
                    .active
                    .insert(video_id.clone(), DownloadState::Done);
                self.downloads.dispatched = self.downloads.dispatched.saturating_sub(1);
                let saved = !path.trim().is_empty();
                if saved {
                    let local = self
                        .downloads
                        .sources
                        .remove(&video_id)
                        .map(|source| source.with_local_path(PathBuf::from(&path)))
                        .unwrap_or_else(|| Song::local_file(PathBuf::from(&path)));
                    self.add_downloaded_track(local);
                }
                // Success toast — opt out of this turn's default error styling.
                self.status.kind = StatusKind::Info;
                self.status.text = format!("{}: {path}", t!("Saved", "저장됨"));
                self.dirty = true;
                // A finished slot lets the next bulk-queued download start.
                let mut cmds = self.pump_downloads();
                if saved {
                    // Persist the manifest so the recovered YouTube id survives a restart.
                    cmds.push(Cmd::SaveDownloads);
                }
                return cmds;
            }
            Msg::DownloadError { video_id, error } => {
                self.downloads
                    .active
                    .insert(video_id.clone(), DownloadState::Failed);
                self.downloads.sources.remove(&video_id);
                self.downloads.dispatched = self.downloads.dispatched.saturating_sub(1);
                self.status.text = format!("{}: {error}", t!("Download failed", "다운로드 실패"));
                self.dirty = true;
                // Keep the batch flowing even when one track fails.
                return self.pump_downloads();
            }
            Msg::Resolved {
                video_id,
                stream_url,
            } => {
                // Bounded prefetch cache; no redraw (purely a skip-latency optimization).
                if self.prefetch.resolved.len() >= RESOLVED_MAX {
                    self.prefetch.resolved.clear();
                }
                self.prefetch.resolved.insert(video_id.clone(), stream_url);
                // A pending self-heal retry: the freshly-updated yt-dlp resolved the
                // failed track — reload it now through the direct CDN URL just cached
                // (bypassing the session mpv's stale spawn-time ytdl_hook).
                if self.heal.pending_video_id.as_deref() == Some(video_id.as_str()) {
                    self.heal.pending_video_id = None;
                    if self.queue.current().is_some_and(|s| s.video_id == video_id) {
                        return self.load_song(self.queue.current().cloned());
                    }
                }
            }
            Msg::StreamingResults {
                seed_video_id,
                candidates,
            } => {
                self.streaming.pending = false;
                if self.autoplay_streaming && self.queue.contains_video_id(&seed_video_id) {
                    // With a key + reranker enabled, hand the model a diverse local shortlist to
                    // reorder (ids only); otherwise rank the pool purely locally. Either way the
                    // pool went through scoring + MMR + cooldown — never taken verbatim.
                    if self.ai.available && self.config.streaming.ai.enabled {
                        return self.start_ai_rerank(&seed_video_id, candidates);
                    }
                    let picks = self.plan_local_streaming(&seed_video_id, candidates);
                    return self.extend_sanitized_streaming(&seed_video_id, picks, &[]);
                } else {
                    self.dirty = true;
                }
            }
            Msg::StreamingPreflighted {
                seed_video_id,
                songs,
            } => {
                self.streaming.pending = false;
                if self.autoplay_streaming && self.queue.contains_video_id(&seed_video_id) {
                    return self.extend_queue_from_streaming(songs);
                }
                self.dirty = true;
            }
            Msg::StreamingAiPicks {
                seed_video_id,
                picks,
                conf,
            } => {
                self.ai.thinking = false;
                self.dirty = true;
                // Only consume `pending_rerank` when this result is for it (a stale/duplicate
                // message for some other seed leaves the current rerank untouched). When it does
                // match but the seed is no longer queued (the user skipped/cleared mid-think),
                // the chain still drops the stale rerank without enqueuing.
                let ours = self
                    .streaming
                    .pending_rerank
                    .as_ref()
                    .is_some_and(|p| p.seed_video_id == seed_video_id);
                if ours
                    && let Some(pending) = self.streaming.pending_rerank.take()
                    && self.autoplay_streaming
                    && self.queue.contains_video_id(&seed_video_id)
                {
                    if let Some(conf) = conf {
                        tracing::debug!(
                            conf,
                            picks = picks.len(),
                            "streaming DJ Gem rerank confidence"
                        );
                    }
                    // Resolve the model's opaque cids back to real tracks once, keeping its order. A
                    // cid that isn't in the pack (a hallucinated id) is dropped here; `merge_ai_picks`
                    // then re-validates against the shortlist and tops up from the local pick. The
                    // same resolution feeds the "Why DJ Gem" overlay (title + role + reasons), which must
                    // outlive the `pending_rerank` we're about to drop.
                    let resolved: Vec<(String, ExplainPick)> = picks
                        .iter()
                        .filter_map(|p| {
                            let vid = pending
                                .cid_map
                                .iter()
                                .find(|m| m.cid == p.cid)?
                                .video_id
                                .clone();
                            let song = pending.shortlist.iter().find(|s| s.video_id == vid)?;
                            let pick = ExplainPick {
                                title: self.display_title(song).into_owned(),
                                artist: self.display_artist(song).into_owned(),
                                role: p.role.clone(),
                                reasons: p.reasons.clone(),
                            };
                            Some((vid, pick))
                        })
                        .collect();
                    let ids: Vec<String> = resolved.iter().map(|(vid, _)| vid.clone()).collect();
                    let roles: Vec<Option<String>> =
                        resolved.iter().map(|(_, pick)| pick.role.clone()).collect();
                    let recipe_ok = streaming::ai_roles_match_recipe(
                        &roles,
                        pending.mode,
                        &self.config.streaming,
                    );
                    let effective_conf = if recipe_ok {
                        conf
                    } else {
                        Some(conf.unwrap_or(0.35).min(0.40))
                    };
                    if !resolved.is_empty() {
                        self.streaming.last_explain = Some(StreamingAiExplain {
                            conf: effective_conf,
                            picks: resolved.into_iter().map(|(_, p)| p).collect(),
                        });
                    }
                    let picks = streaming::merge_ai_picks_with_confidence(
                        &ids,
                        &pending.shortlist,
                        &pending.local_pick,
                        self.config.streaming.ai.picks,
                        effective_conf,
                    );
                    // Cache the validated ordering so a rapid identical refill replays it without a
                    // second call. Skip empty results (a failed rerank) so the next refill retries.
                    if !ids.is_empty() && recipe_ok && effective_conf.unwrap_or(0.0) >= 0.45 {
                        self.ai_cache_store(pending.cache_key, ids);
                    }
                    return self.extend_sanitized_streaming(
                        &seed_video_id,
                        picks,
                        &pending.local_pick,
                    );
                }
            }
            Msg::StreamingError {
                seed_video_id,
                error,
            } => {
                self.streaming.pending = false;
                if self.autoplay_streaming && self.queue.contains_video_id(&seed_video_id) {
                    return self.note_streaming_failure(format!(
                        "{}: {error}",
                        t!("Autoplay failed", "자동재생 실패")
                    ));
                } else {
                    self.dirty = true;
                }
            }

            // --- DJ Gem assistant intents ---------------------------------------
            Msg::AiThinking(on) => {
                self.ai.thinking = on;
                self.bridges.ai_transcript_scroll.scroll_to_end();
                self.dirty = true;
            }
            Msg::AiChat(text) => {
                // Skip empty replies (e.g. a silent autoplay top-up that only ran tools).
                if !text.trim().is_empty() {
                    self.push_ai_message(AiRole::Ai, text);
                    self.dirty = true;
                }
            }
            Msg::AiError(text) => {
                self.ai.thinking = false;
                self.push_ai_message(AiRole::Error, text);
                self.dirty = true;
            }
            Msg::AiPlayTracks(songs) => {
                if !songs.is_empty() {
                    let requested_songs = songs.clone();
                    self.queue.set(songs, 0);
                    self.status.text.clear();
                    let song = self.queue.current().cloned();
                    let mut cmds = self.load_song(song);
                    cmds.extend(self.request_romanization_for_songs(&requested_songs));
                    return cmds;
                }
            }
            Msg::AiEnqueue(songs) => {
                return self.extend_queue_from_streaming(songs);
            }
            Msg::AiSuggestions(songs) => {
                self.ai.suggestions = songs;
                self.ai.suggestions_selected = 0;
                self.bridges.ai_scroll.reset();
                self.dirty = true;
                let suggestions = self.ai.suggestions.clone();
                return self.request_romanization_for_songs(&suggestions);
            }
            Msg::AiSetAutoplay(on) => {
                // Music-mode invariant: DJ Gem can't enable autoplay while repeat is on.
                let on = on && self.queue.repeat == crate::queue::Repeat::Off;
                self.set_autoplay_streaming(on);
                self.dirty = true;
                let mut cmds = vec![self.save_playback_modes_cmd()];
                if on {
                    // Same proactive top-up as the manual toggle (see Action::ToggleStreaming).
                    cmds.extend(self.maybe_autoplay_extend());
                }
                return cmds;
            }
            Msg::AiSetStationProfile {
                query,
                explore,
                avoid_artists,
            } => {
                // Distill the vibe into engine knobs the local streaming can actually act on:
                // adventurousness (→ mode) and artists to keep out (→ banned_artist_keys, read
                // live in `build_station_state`). Persist both so the station survives a restart.
                let profile = crate::station::StationProfile::from_intent(
                    &query,
                    explore.as_deref(),
                    &avoid_artists,
                );
                self.config.streaming.mode = profile.explore.to_mode();
                self.station.active = Some(profile);
                self.dirty = true;
                return vec![
                    Cmd::SaveStationProfile,
                    Cmd::SaveConfig(Box::new(self.config.clone())),
                ];
            }
            Msg::AiCreatePlaylist(name) => {
                if self.playlists.create(&name).is_some() {
                    self.dirty = true;
                    return vec![Cmd::SavePlaylists];
                }
            }
            Msg::AiAddToPlaylist { playlist, songs } => {
                let mut any = false;
                for song in songs {
                    if matches!(
                        self.playlists.add(&playlist, song),
                        crate::playlists::AddResult::Added
                    ) {
                        any = true;
                    }
                }
                if any {
                    self.dirty = true;
                    return vec![Cmd::SavePlaylists];
                }
            }
            Msg::AiPlayPlaylist(key) => {
                if let Some(songs) = self.playlists.find(&key).map(|p| p.songs.clone())
                    && !songs.is_empty()
                {
                    let requested_songs = songs.clone();
                    self.queue.set(songs, 0);
                    self.status.text.clear();
                    let song = self.queue.current().cloned();
                    let mut cmds = self.load_song(song);
                    cmds.extend(self.request_romanization_for_songs(&requested_songs));
                    return cmds;
                }
            }
            Msg::StationPatch {
                down_artists,
                boost_artists,
            } => {
                // The off-path feedback summary landed (possibly empty on failure) — always clear
                // the in-flight guard so the next streak can trigger again. Fold the avoid/boost
                // into the active station and persist only when the avoid list actually changed.
                self.streaming.feedback_in_flight = false;
                if let Some(profile) = self.station.active.as_mut()
                    && profile.apply_feedback(&down_artists, &boost_artists)
                {
                    self.dirty = true;
                    return vec![Cmd::SaveStationProfile];
                }
            }
            Msg::RomanizedTitles {
                request_id,
                keys,
                entries,
            } => {
                return self.apply_romanized_titles(request_id, keys, entries);
            }
            Msg::Scrobble(event) => return self.on_scrobble_event(event),
            Msg::UpdateChecked(status) => {
                // One-time toast the first time a newer release is seen (the check task
                // sets `first_seen` and persists the toasted tag so it fires once per
                // version). The persistent surfaces — About notice + brand dot — read
                // `update_status` directly on every frame.
                if status.available && status.first_seen {
                    self.status.kind = StatusKind::Info;
                    self.status.text = if crate::i18n::is_korean() {
                        format!("새 버전 v{} 사용 가능 — About(F1)", status.latest_display())
                    } else {
                        format!(
                            "Update available: v{} — see About (F1)",
                            status.latest_display()
                        )
                    };
                    self.dirty = true;
                }
                self.update_status = Some(status);
            }
            Msg::Tools(event) => match event {
                crate::tools::ToolsEvent::Progress { channel, percent } => {
                    self.status.kind = StatusKind::Info;
                    let label = channel.label();
                    let head = t!("Downloading yt-dlp", "yt-dlp 다운로드 중");
                    self.status.text = match percent {
                        Some(p) => format!("{head} ({label})… {p}%"),
                        None => format!("{head} ({label})…"),
                    };
                    self.dirty = true;
                }
                crate::tools::ToolsEvent::Installed { version } => {
                    self.status.kind = StatusKind::Info;
                    self.status.text = if crate::i18n::is_korean() {
                        format!("yt-dlp {version} 준비 완료")
                    } else {
                        format!("yt-dlp {version} ready")
                    };
                    self.dirty = true;
                }
                crate::tools::ToolsEvent::Failed { error } => {
                    // A failed background refresh of a *working* setup stays log-only
                    // (check_and_update already traced it); only an app with no usable
                    // yt-dlp at all needs the user's attention.
                    if crate::tools::ytdlp_selection().is_none() {
                        self.status.kind = StatusKind::Error;
                        self.status.text =
                            format!("{}: {error}", t!("yt-dlp unavailable", "yt-dlp 사용 불가"));
                        self.dirty = true;
                    }
                }
            },
            Msg::Transfer(event) => return self.on_transfer_event(event),
        }
        Vec::new()
    }

    /// The single footer hint shown across every view: just the live chord that opens the
    /// `?` cheat-sheet (which already lists every binding for every screen). Built from the
    /// keymap so remapping "toggle help" updates the hint in lock-step.
    pub fn help_footer(&self) -> String {
        format!(
            "{}  keybindings",
            self.keymap.label_for_display(
                KeyContext::Global,
                Action::ToggleHelp,
                self.retro_mode()
            )
        )
    }

    /// Return to the player/home screen from any mode. Settings use the normal close path
    /// so draft values and keybinding changes are not silently discarded.
    fn go_home(&mut self) -> Vec<Cmd> {
        self.help_visible = false;
        self.mouse_help_visible = false;
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.queue_popup.open = false;
        self.search_filter.close();
        self.library_ui.confirm_delete = None;
        self.library_ui.confirm_download = None;
        self.playlist_picker = None;
        // These three render as top-level overlays but route input only inside Settings-mode
        // dispatch, so leaving the screen must drop them explicitly or they'd paint on top of
        // the Player, unreachable. (`spotify_picker` shares the same shape.)
        self.recording_settings = None;
        self.recordings_browser = None;
        self.spotify_picker = None;
        self.reset_playlist_ui_state();
        // Leaving the screen drops any pending text selection so it can't reappear highlighted
        // when the input is re-entered later.
        self.search.select_all = false;
        self.ai.select_all = false;
        if self.mode == Mode::Settings {
            self.finish_settings_text_edit();
            return self.close_settings();
        }
        self.mode = Mode::Player;
        self.dirty = true;
        Vec::new()
    }

    fn quit_app(&mut self) -> Vec<Cmd> {
        self.help_visible = false;
        self.mouse_help_visible = false;
        let cmds = if self.mode == Mode::Settings {
            self.finish_settings_text_edit();
            self.close_settings()
        } else {
            Vec::new()
        };
        self.should_quit = true;
        cmds
    }

    /// How many rows a PageUp/PageDown moves: a screenful of the active list less one row
    /// of context overlap. Falls back to [`DEFAULT_PAGE_ROWS`] before the first render
    /// records the viewport height.
    fn page_step(&self) -> usize {
        let rows = self.bridges.list_viewport_rows.get() as usize;
        if rows <= 1 {
            DEFAULT_PAGE_ROWS
        } else {
            rows - 1
        }
    }

    /// How many rows a single `MoveUp`/`MoveDown`-style step should advance, given how long
    /// the key has been held. Ramps up the longer the same direction repeats so holding an
    /// arrow flies through a long list while a tap still moves exactly one row. See
    /// [`NavRepeat`]; the timing core is split into [`Self::nav_repeat_step_at`] for tests.
    fn nav_repeat_step(&mut self, action: Action) -> usize {
        self.nav_repeat_step_at(Instant::now(), action)
    }

    /// Timing core of [`Self::nav_repeat_step`], split out so tests can supply a clock.
    /// Consecutive same-direction events within [`NAV_REPEAT_GAP`] extend the hold; a gap or
    /// a direction change restarts it. (The OS initial-repeat delay exceeds the gap, so the
    /// ramp naturally begins once the fast auto-repeat stream kicks in.)
    fn nav_repeat_step_at(&mut self, now: Instant, action: Action) -> usize {
        let held_on = self.interaction.nav_repeat.action == Some(action)
            && self
                .interaction
                .nav_repeat
                .last
                .is_some_and(|t| now.duration_since(t) <= NAV_REPEAT_GAP);
        if !held_on {
            self.interaction.nav_repeat.started = Some(now);
        }
        self.interaction.nav_repeat.action = Some(action);
        self.interaction.nav_repeat.last = Some(now);
        let held = self
            .interaction
            .nav_repeat
            .started
            .map_or(Duration::ZERO, |s| now.duration_since(s));
        nav_step_for_hold(held)
    }

    /// Switch screens from a nav-bar click — the mouse equivalent of the `Open*` keys, but
    /// reachable from any screen. Leaving Settings commits the draft via the normal close
    /// path so edits aren't lost; transient overlays are cleared.
    fn navigate_to(&mut self, mode: Mode) -> Vec<Cmd> {
        self.help_visible = false;
        self.mouse_help_visible = false;
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.queue_popup.open = false;
        self.search_filter.close();
        self.library_ui.confirm_delete = None;
        self.library_ui.confirm_download = None;
        // Popup-like playlist surfaces dismiss on any navigation (the drill-down itself is
        // content state — it resets only on a fresh Library entry below).
        self.library_ui.create_input = None;
        self.library_ui.confirm_playlist_delete = None;
        self.playlist_picker = None;
        // Any navigation deselects: a Ctrl+A highlight must not survive a screen change.
        self.search.select_all = false;
        self.ai.select_all = false;
        if self.mode == mode {
            self.dirty = true;
            return Vec::new();
        }
        let cmds = if self.mode == Mode::Settings {
            self.finish_settings_text_edit();
            self.close_settings() // sets mode = Player; overridden below if needed
        } else {
            Vec::new()
        };
        match mode {
            Mode::Player => self.mode = Mode::Player,
            Mode::Search => {
                self.mode = Mode::Search;
                self.search.focus = SearchFocus::Input;
                let search = self.search_config_for_mode();
                self.search.source = search.normalized_source(self.search.source);
            }
            Mode::Library => {
                self.mode = Mode::Library;
                if !self.library_tab_available(self.library_ui.tab) {
                    self.library_ui.tab = self.library_tabs()[0];
                }
                // Start each library visit clean (cursor, anchor, scroll, filter, and any
                // playlist drill-down or popup left from the previous visit).
                self.reset_playlist_ui_state();
                self.clear_library_filter();
                if self.effective_library_tab() == LibraryTab::Playlists {
                    self.hint_playlist_create();
                }
            }
            Mode::Settings => self.open_settings(),
            Mode::Ai => self.enter_ai(),
        }
        self.dirty = true;
        cmds
    }

    /// Select a Settings tab by index into [`SettingsTab::ALL`] (from a tab click).
    fn settings_select_tab(&mut self, index: usize) {
        if let Some(st) = self.settings.as_mut()
            && let Some(&tab) = SettingsTab::ALL.get(index)
        {
            st.tab = tab;
            st.row = 0;
            st.editing_text = false;
            st.capturing = None;
            // The new tab has a different row set; drop the old offset so it starts at the top.
            self.bridges.reset_settings_scroll();
            self.dirty = true;
        }
    }

    /// Move the field cursor to `row` before a mouse-driven change/activate, so the shared
    /// keyboard handlers (`settings_change`/`settings_activate`) act on the clicked row.
    fn settings_focus_row(&mut self, row: usize) {
        // Commit any in-progress text edit *before* moving focus: a secret editor (API key) is
        // opened with its buffer cleared and the prior key stashed in `secret_restore`. Clicking
        // straight to another control would otherwise leave that buffer empty with `editing_text`
        // cleared, orphaning the stash — and `close_settings` would then erase the saved key.
        // `finish_settings_text_edit` runs while the cursor is still on the secret row, so it
        // restores into the right buffer.
        self.finish_settings_text_edit();
        if let Some(st) = self.settings.as_mut() {
            // The Keys tab has no `Field`s; leave its binding-selection cursor untouched (no
            // field controls register there, so this is defensive).
            let fields = st.fields();
            if !fields.is_empty() {
                st.row = row.min(fields.len() - 1);
            }
            st.editing_text = false;
            self.dirty = true;
        }
    }
}

/// Build the lyrics-fetch effect for `song`.
fn fetch_lyrics_cmd(song: &Song) -> Cmd {
    Cmd::FetchLyrics {
        video_id: song.video_id.clone(),
        artist: song.artist.clone(),
        title: song.title.clone(),
    }
}

fn song_label(song: &Song) -> String {
    if song.artist.trim().is_empty() {
        song.title.clone()
    } else {
        format!("{} — {}", song.title, song.artist)
    }
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

pub(crate) use crate::util::browser::open_in_browser;

/// Copy `text` to the system clipboard, fire-and-forget. Mirrors `open_in_browser`: spawns the
/// platform clipboard tool with stdio nulled and pipes `text` to its stdin, so no native
/// clipboard crate is needed. macOS uses `pbcopy`, Windows `clip`; Linux tries `wl-copy`
/// (Wayland) then `xclip`/`xsel` (X11), each of which self-daemonizes to keep the selection
/// alive after we return. Any failure (tool absent) is silently ignored.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn pipe(cmd: &mut Command, text: &str) -> bool {
        match cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                true
            }
            Err(_) => false,
        }
    }

    if cfg!(target_os = "macos") {
        pipe(
            &mut process::std_command("pbcopy", process::ProcessProfile::Clipboard),
            text,
        );
    } else if cfg!(target_os = "windows") {
        pipe(
            &mut process::std_command("clip", process::ProcessProfile::Clipboard),
            text,
        );
    } else if !pipe(
        &mut process::std_command("wl-copy", process::ProcessProfile::Clipboard),
        text,
    ) && !pipe(
        process::std_command("xclip", process::ProcessProfile::Clipboard)
            .args(["-selection", "clipboard"]),
        text,
    ) {
        pipe(
            process::std_command("xsel", process::ProcessProfile::Clipboard).arg("-ib"),
            text,
        );
    }
}

/// Spawn a borderless, always-on-top mpv overlay window for `url`, returning the child so the
/// caller can track and later close it. Stdio is nulled so mpv can't touch the TUI's terminal.
/// Cookies are forwarded to mpv's bundled yt-dlp (same option as the audio instance) when set;
/// `--no-config` is intentionally omitted so the user's own mpv config applies to the video.
/// With `ipc_path`, the window exposes a JSON IPC endpoint and gets `--keep-open=yes`, so a
/// natural end pauses on the last frame (observable, and re-loadable) instead of exiting.
fn spawn_video_overlay(
    url: &str,
    cookies: Option<&std::path::Path>,
    layout: crate::config::VideoOverlay,
    ipc_path: Option<&str>,
) -> Option<std::process::Child> {
    use std::process::Stdio;
    let mut cmd =
        process::std_command(&crate::tools::mpv_program(), process::ProcessProfile::Media);
    cmd.arg(url);
    // The audio instance already owns the OS media session; without this the
    // overlay mpv would register a second, duplicate entry (mpv ≥ 0.39 does so
    // by default even for a plain window). As a CLI option it wins over the
    // user's mpv config, which is the point — the override lever for the audio
    // instance (`YTM_MPV_EXTRA`) intentionally doesn't reach the overlay.
    if crate::player::mpv::media_controls_flag_supported() {
        cmd.arg("--media-controls=no");
    }
    for arg in layout.mpv_window_args() {
        cmd.arg(arg);
    }
    if let Some(path) = ipc_path {
        cmd.arg(format!("--input-ipc-server={path}"));
        cmd.arg("--keep-open=yes");
    }
    if let Some(path) = cookies {
        cmd.arg(format!(
            "--ytdl-raw-options-append=cookies={}",
            path.display()
        ));
    }
    if let Some(arg) = crate::tools::mpv_ytdl_js_runtime_arg() {
        cmd.arg(arg);
    }
    // Pin ytdl_hook to the selected yt-dlp (managed/override), like the audio
    // instance — but with `-append`: this spawn honors the user's mpv config, and
    // plain `--script-opts=` would wipe their other script options.
    if let Some(sel) = crate::tools::ytdlp_selection()
        && let Some(pin) = sel.pin_for_mpv()
    {
        let pin = pin.canonicalize().unwrap_or_else(|_| pin.to_path_buf());
        cmd.arg(format!(
            "--script-opts-append=ytdl_hook-ytdl_path={}",
            pin.display()
        ));
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    cmd.spawn().ok()
}

#[cfg(test)]
mod tests;
