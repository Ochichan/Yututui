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
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::ai::GeminiModel;
use crate::api::Song;
use crate::artwork::ArtSource;
use crate::config::{Config, SPEED_MAX, SPEED_MIN};
use crate::eq::{self, EqPreset};
use crate::keymap::{Action, Chord, Conflict, KeyContext, KeyMap};
use crate::t;
use crate::library::Library;
use crate::lyrics::LyricLine;
use crate::player::PlayerCmd;
use crate::playlists::Playlists;
use crate::queue::Queue;
use crate::radio::{self, CandidateSource, Cooc, RadioMode, StationState};
use crate::settings::{self, Field, FieldKind, SettingsDraft, SettingsState, SettingsTab};
use crate::signals::{self, Signals};
use crate::theme::{ThemeConfig, ThemeRole};

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
mod mouse;
mod player;
mod queue;
mod radio_reducer;
mod search;
mod settings_reducer;

/// Queue length at or below which the autoplay/radio hook tops up the queue.
const AUTOPLAY_THRESHOLD: usize = 3;
/// Number of related tracks to request from the non-AI radio fallback.
pub(crate) const RADIO_FALLBACK_COUNT: usize = 8;
/// Size of the raw candidate pool fetched for the local radio engine to rank. Larger than
/// the final pick count so scoring/diversity/cooldown have real choice.
pub(crate) const RADIO_POOL_COUNT: usize = 40;
/// How many recent history artists feed the radio cooldown window.
const RADIO_RECENT_ARTISTS: usize = 12;
/// Minimum gap between autoplay top-up requests (avoids a request storm).
const AUTOPLAY_COOLDOWN: Duration = Duration::from_secs(60);
/// Consecutive empty radio extends before autoplay disables itself (circuit breaker).
const AUTOPLAY_MAX_FAILURES: u8 = 3;
/// How long a transient `status` notification covers the song title before it auto-clears
/// (on the Player screen the status line replaces the title, so it must not linger).
const STATUS_TTL: Duration = Duration::from_secs(3);
/// Cap on AI chat transcript lines kept in memory (bounded memory).
const AI_HISTORY_MAX: usize = 999;

/// Rows the cursor moves per mouse-wheel notch in the Library / Search lists — enough to
/// read as scrolling, small enough to stay controllable.
const MOUSE_SCROLL_LINES: usize = 3;
/// Page size used by PageUp/PageDown before the first render records the real list
/// viewport height (e.g. in tests that never draw a frame).
const DEFAULT_PAGE_ROWS: usize = 10;

/// Percentage points changed per volume keypress.
const VOLUME_STEP: i64 = 5;
/// Highest volume the UI sets (mpv would allow more, but 100 is a sane v1 ceiling).
const VOLUME_MAX: i64 = 100;
/// Cap on cached prefetched stream URLs (bounded memory; we only look a step ahead).
const RESOLVED_MAX: usize = 999;
/// Cap on local download-folder rows held in memory.
const DOWNLOADED_TRACKS_MAX: usize = 999;
/// How many tracks in a row may fail before we stop auto-skipping and surface the error.
/// A single unplayable track (expired URL, region/age-gated, throttled) shouldn't halt
/// the session, but a systemic failure (offline, bad cookie) shouldn't skip-storm the
/// whole queue either — so we skip a few, then stop and explain.
const MAX_CONSECUTIVE_PLAY_ERRORS: u8 = 3;
/// Playback-speed change per `>`/`<` press.
const SPEED_STEP: f64 = 0.1;
/// Idle gap (seconds) that ends a listening session, resetting the skip-confidence counter.
const SESSION_GAP_SECS: i64 = 20 * 60;

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
    /// Whether the `?` help / cheat-sheet overlay is shown.
    pub help_visible: bool,
    /// A pending keybinding-conflict warning (Keys tab). When set, a modal popup is shown
    /// and the next key/click dismisses it; the attempted rebind is left unchanged.
    pub key_conflict: Option<Conflict>,
    /// Whether the "reset all settings" confirmation modal (General tab) is showing. Enter/`y`
    /// confirms (resets the draft to defaults); any other key / a click cancels.
    pub confirm_reset_all: bool,
    /// Whether the About card overlay is showing. Opened by clicking the `ytm-tui` brand in the
    /// nav bar or via `Action::ToggleAbout` (F1); any key/click (other than the GitHub link)
    /// dismisses it.
    pub about_visible: bool,
    /// The app icon as a render-ready half-blocks protocol for the About card, decoded from the
    /// embedded PNG and built once on first open. Half-blocks (not the graphics protocol used for
    /// album art) so it draws in any terminal and repaints like plain text — leaving no residue
    /// when the card closes over the view beneath. `RefCell` because render only has `&App`.
    pub about_icon: RefCell<Option<StatefulProtocol>>,

    // Playback ----------------------------------------------------------------
    /// Playback position in seconds, if known.
    pub time_pos: Option<f64>,
    /// Track duration in seconds, if known.
    pub duration: Option<f64>,
    /// Whether playback is currently paused.
    pub paused: bool,
    /// Output volume, 0-100.
    pub volume: i64,
    /// The play queue: ordering, shuffle, repeat, and the current track.
    pub queue: Queue,
    /// A status/error line shown to the user (empty = normal).
    pub status: String,
    /// When `status` was last set non-empty, used to auto-expire it after [`STATUS_TTL`]
    /// (set centrally in [`Self::update`]; `None` while the title is showing normally).
    status_set_at: Option<Instant>,
    /// Semantic kind of the current `status` (drives its color); reset to `Error` on clear.
    pub status_kind: StatusKind,
    /// The detached mpv video-overlay process, if one is open. Tracked so a second `v` (or a
    /// `Shift+V` layout switch) closes/respawns it instead of stacking windows.
    video_proc: Option<std::process::Child>,
    /// Whether opening the video overlay is what paused the audio, so closing it only resumes
    /// playback we paused (not audio the user had paused themselves).
    video_paused_audio: bool,

    // Audio / EQ --------------------------------------------------------------
    /// Selected equalizer preset (drives `eq_bands` when chosen via `e`).
    pub eq_preset: EqPreset,
    /// Current per-band gains (dB); editable live from the settings screen.
    pub eq_bands: [f64; eq::BANDS],
    /// Loudness normalization (`dynaudnorm`) on/off.
    pub normalize: bool,
    /// Playback speed multiplier (1.0 = normal).
    pub speed: f64,
    /// Seconds jumped per seek-back/-forward key (configurable; default 10s).
    pub seek_seconds: f64,
    /// Auto-extend the queue with related tracks when it runs low (radio mode).
    pub autoplay_radio: bool,
    /// Whether the click-to-open EQ preset dropdown is showing on the player status line.
    /// Player-only and session-ephemeral: toggled by clicking the `eq:` label, dismissed
    /// by picking a preset or clicking elsewhere.
    pub eq_dropdown_open: bool,
    /// Same as [`Self::eq_dropdown_open`] but for the `radio:` mode dropdown. Mutually
    /// exclusive with it (opening one closes the other).
    pub radio_dropdown_open: bool,
    /// Whether the queue window (opened by clicking the `N/M` position label) is showing.
    /// Player-only overlay; while open it captures the keyboard (nav / Delete / Enter).
    pub queue_popup_open: bool,
    /// The highlighted row in the queue window (order position) — the active end of the
    /// drag/range selection.
    pub queue_popup_cursor: usize,
    /// The fixed end of the queue window's multi-select range (drag start / last single
    /// click). The selection is the inclusive span between this and `queue_popup_cursor`.
    pub queue_popup_anchor: usize,
    /// Screen rect of the open queue window, written each render so a click outside it can
    /// be detected (which closes it). `Cell` because render only has `&App`.
    pub queue_popup_rect: Cell<Option<Rect>>,

    // Settings ----------------------------------------------------------------
    /// The persisted config, kept so the settings screen can save the full file.
    pub config: Config,
    /// The settings screen state, present only while `Mode::Settings` is active.
    pub settings: Option<Box<SettingsState>>,

    // AI assistant ------------------------------------------------------------
    /// AI-assistant state: availability, model, chat transcript, prompt, suggestions.
    pub ai: AiState,
    /// When the autoplay hook last fired a top-up request (for the cooldown).
    radio_last_extend: Option<Instant>,
    /// True while the radio candidate-pool fetch is in flight (both the AI and non-AI paths
    /// fetch the same pool first).
    radio_pending: bool,
    /// An AI rerank handed off to the assistant actor, awaiting its `Msg::RadioAiPicks`. Holds
    /// the shortlist (to validate the returned ids against) and the local pick (the fallback).
    pending_rerank: Option<PendingRerank>,
    /// Consecutive empty radio extends, for the autoplay circuit breaker.
    consecutive_radio_failures: u8,
    /// Consecutive mpv playback errors with no track playing in between, for the
    /// auto-skip circuit breaker (see [`MAX_CONSECUTIVE_PLAY_ERRORS`]).
    consecutive_play_errors: u8,
    /// The user's local playlists (the AI playlist tools read/write these).
    pub playlists: Playlists,

    // Search ------------------------------------------------------------------
    /// Search query, results, selection, focus, and in-flight flag.
    pub search: SearchState,

    // Library -----------------------------------------------------------------
    /// Favorites + play history, persisted to disk. Loaded by `main` after `new`.
    pub library: Library,
    /// Per-track preference signals (plays/skips/dislikes + raw play log + artist affinity),
    /// persisted separately from the library so `Song`'s shape stays unchanged. Loaded by
    /// `main` after `new`; drives radio ranking and the ♥/✗ status-line toggles.
    pub signals: Signals,
    /// Tracks started in the current listening session (reset after a long idle gap). Used
    /// to down-weight skip→dislike learning early in / in short sessions (noisier signal).
    session_plays: u32,
    /// Unix time of the last track start, for detecting session boundaries (idle gaps).
    last_activity_at: Option<i64>,
    /// Local audio files found in the configured download directory.
    pub downloaded_tracks: Vec<Song>,
    pub library_tab: LibraryTab,
    pub library_selected: usize,
    /// The fixed end of the library list's multi-select range (drag start / last single
    /// click). The selection is the inclusive span between this and `library_selected`,
    /// mirroring the queue window's drag-to-select.
    pub library_anchor: usize,
    /// Pending "delete downloaded files" confirmation: the on-disk paths queued for deletion
    /// (file removal is irreversible, so it's gated behind an explicit yes/no). `None` when no
    /// modal is open.
    pub confirm_delete_files: Option<Vec<PathBuf>>,

    // Lyrics ------------------------------------------------------------------
    /// Whether the lyrics panel is shown in the player view.
    pub lyrics_visible: bool,
    /// True between requesting lyrics and the result arriving.
    pub lyrics_loading: bool,
    /// Lyrics for the current track, if fetched.
    pub lyrics: Option<TrackLyrics>,

    // Album art ---------------------------------------------------------------
    /// The terminal graphics picker (font size + detected protocol), built once at startup
    /// when album art is enabled. `None` → feature off, or the terminal couldn't be probed
    /// (no art is fetched or drawn in that case).
    pub art_picker: Option<Picker>,
    /// The current track's art as a render-ready, resizable protocol. `RefCell` because
    /// `StatefulImage` needs `&mut` during render, which only has `&App` (mirrors
    /// [`Self::mouse_buttons`]).
    pub art: RefCell<Option<StatefulProtocol>>,
    /// The decoded source image kept alongside the protocol so [`Self::refresh_art`] can
    /// rebuild a fresh protocol (new graphics-protocol id) on demand — see that method for why.
    art_source: Option<DynamicImage>,
    /// Source pixel dimensions of the held art, for centering it within its panel.
    pub art_dims: (u32, u32),
    /// `video_id` the held art belongs to (guards against a stale image lingering).
    art_video_id: Option<String>,
    /// True between requesting art and the result arriving.
    pub art_loading: bool,

    // Downloads ---------------------------------------------------------------
    /// In-flight / finished downloads, keyed by `video_id`, for the UI indicator.
    pub downloads: HashMap<String, DownloadState>,
    /// Original catalog metadata for in-flight downloads, keyed by `video_id`.
    download_sources: HashMap<String, Song>,

    // Prefetch ----------------------------------------------------------------
    /// Pre-resolved direct stream URLs, keyed by `video_id` (for instant skip).
    resolved: HashMap<String, String>,
    /// Whether the current track was loaded from a prefetched direct URL (vs the watch
    /// URL mpv resolves itself). Recorded so a playback error can note the likelier cause
    /// (a stale prefetched CDN URL) in the log.
    last_load_prefetched: bool,
    /// `video_id` of the track actually loaded into mpv. A cached/restored queue entry can
    /// be visible before it is loaded; the first play action then loads it instead of only
    /// toggling mpv's pause property.
    loaded_video_id: Option<String>,

    /// Screen rect of the seekbar, written by the player view each render so a mouse
    /// click can be hit-tested against it. `Cell` because render only has `&App`.
    pub seekbar_rect: Cell<Option<Rect>>,
    /// Viewport height (rows) of the active Library / Search list, written each render so
    /// PageUp/PageDown can move by a screenful. `Cell` because render only has `&App`.
    pub list_viewport_rows: Cell<u16>,
    /// Decoupled wheel-scroll offset for each browse list (see [`crate::ui::scroll`]). The
    /// mouse wheel moves these directly; the render pass nudges them to keep the keyboard
    /// selection on-screen with a margin. One per list so each keeps its own place.
    pub library_scroll: crate::ui::scroll::ScrollState,
    pub search_scroll: crate::ui::scroll::ScrollState,
    pub queue_popup_scroll: crate::ui::scroll::ScrollState,
    pub ai_scroll: crate::ui::scroll::ScrollState,
    /// Clickable button rects written by views each render. `RefCell` because render only
    /// has `&App`, but the reducer needs the last rendered hit map.
    pub mouse_buttons: RefCell<Vec<MouseButtonRegion>>,

    /// Last whole second we redrew for, so sub-second `time-pos` spam is coalesced.
    last_shown_sec: i64,

    /// Monotonic animation frame counter, bumped on each [`Msg::AnimTick`] (~30 fps) while
    /// animations are active. Drives every effect's phase; wraps harmlessly. `0` at rest.
    anim_frame: u64,
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
            help_visible: false,
            key_conflict: None,
            confirm_reset_all: false,
            about_visible: false,
            about_icon: RefCell::new(None),
            time_pos: None,
            duration: None,
            paused: false,
            volume: volume.clamp(0, VOLUME_MAX),
            queue: Queue::default(),
            status: String::new(),
            status_set_at: None,
            status_kind: StatusKind::Error,
            video_proc: None,
            video_paused_audio: false,
            anim_frame: 0,
            eq_preset: EqPreset::default(),
            eq_bands: [0.0; eq::BANDS],
            normalize: false,
            speed: 1.0,
            seek_seconds: crate::config::SEEK_SECONDS_DEFAULT,
            autoplay_radio: false,
            eq_dropdown_open: false,
            radio_dropdown_open: false,
            queue_popup_open: false,
            queue_popup_cursor: 0,
            queue_popup_anchor: 0,
            queue_popup_rect: Cell::new(None),
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
            radio_last_extend: None,
            radio_pending: false,
            pending_rerank: None,
            consecutive_radio_failures: 0,
            consecutive_play_errors: 0,
            playlists: Playlists::default(),
            search: SearchState {
                input: String::new(),
                select_all: false,
                focus: SearchFocus::Input,
                results: Vec::new(),
                selected: 0,
                searching: false,
            },
            library: Library::default(),
            signals: Signals::default(),
            session_plays: 0,
            last_activity_at: None,
            downloaded_tracks: Vec::new(),
            library_tab: LibraryTab::All,
            library_selected: 0,
            library_anchor: 0,
            confirm_delete_files: None,
            lyrics_visible: false,
            lyrics_loading: false,
            lyrics: None,
            art_picker: None,
            art: RefCell::new(None),
            art_source: None,
            art_dims: (0, 0),
            art_video_id: None,
            art_loading: false,
            downloads: HashMap::new(),
            download_sources: HashMap::new(),
            resolved: HashMap::new(),
            seekbar_rect: Cell::new(None),
            list_viewport_rows: Cell::new(0),
            library_scroll: crate::ui::scroll::ScrollState::default(),
            search_scroll: crate::ui::scroll::ScrollState::default(),
            queue_popup_scroll: crate::ui::scroll::ScrollState::default(),
            ai_scroll: crate::ui::scroll::ScrollState::default(),
            mouse_buttons: RefCell::new(Vec::new()),
            last_shown_sec: -1,
            last_load_prefetched: false,
            loaded_video_id: None,
        }
    }

    /// Push persisted playback/EQ settings into the app after construction. Kept separate
    /// from `new` (whose `volume`-only signature many tests rely on) so `main` can apply
    /// the full config without churning those call sites.
    pub fn apply_config(&mut self, cfg: &Config) {
        self.eq_preset = cfg.eq_preset;
        self.eq_bands = cfg.effective_eq_bands();
        self.normalize = cfg.effective_normalize();
        self.speed = cfg.effective_speed();
        self.seek_seconds = cfg.effective_seek_seconds();
        self.autoplay_radio = cfg.effective_autoplay_radio();
        self.ai.available = cfg.effective_ai_key().is_some();
        self.ai.model = cfg.effective_gemini_model();
        self.keymap = KeyMap::from_config(cfg);
        self.theme = cfg.effective_theme();
        // Keep the process-wide UI language in sync with the applied config (this is the
        // central apply path, called at startup and after a settings save).
        crate::i18n::set_language(cfg.effective_language());
        // Keep the full config so the settings screen can persist the whole file.
        self.config = cfg.clone();
    }

    /// Seed the player with the last locally recorded track, without starting playback.
    /// This gives a fresh launch something useful to show while keeping autoplay opt-in.
    pub fn restore_last_played_from_library(&mut self) {
        if !self.queue.is_empty() {
            return;
        }
        let Some(song) = self.library.history.front().cloned() else {
            return;
        };
        self.queue.set(vec![song], 0);
        self.time_pos = None;
        self.duration = None;
        self.paused = true;
        self.last_shown_sec = -1;
        self.loaded_video_id = None;
        self.status.clear();
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
        self.paused = false;
        let song = self.queue.current().cloned();
        self.load_song(song)
    }


    /// The reducer: apply one message, returning effects for the run loop to dispatch.
    /// Reducer entry point. Wraps [`Self::dispatch`] to centrally track when a transient
    /// `status` notification is set or cleared (any of the ~40 `self.status = …` sites), so
    /// the main loop can expire it after [`STATUS_TTL`] and bring the song title back —
    /// without each call site having to remember to arm a timer. See [`Self::status_visible`].
    pub fn update(&mut self, msg: Msg) -> Vec<Cmd> {
        let status_before = self.status.clone();
        let kind_before = self.status_kind;
        // Default this turn's status to the error styling; the few positive handlers override
        // it to `Info` while they run. This keeps the kind in lock-step with the status text:
        // an error set by one of the ~40 plain `self.status = …` sites can never inherit a
        // leftover `Info` color from a previous green toast.
        self.status_kind = StatusKind::Error;
        let cmds = self.dispatch(msg);
        if self.status != status_before {
            self.status_set_at =
                if self.status.is_empty() { None } else { Some(Instant::now()) };
        } else {
            // Text unchanged this turn — keep the color the still-showing message already had.
            self.status_kind = kind_before;
        }
        cmds
    }

    /// Whether a transient status is currently covering the title (drives the main loop's
    /// expiry tick — see [`Msg::StatusTick`]).
    pub fn status_visible(&self) -> bool {
        self.status_set_at.is_some()
    }

    fn dispatch(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::Key(k) => return self.on_key(k),
            Msg::MouseClick { col, row } => return self.on_mouse_click(col, row),
            Msg::MouseDoubleClick { col, row } => return self.on_mouse_double_click(col, row),
            Msg::MouseDrag { col, row } => return self.on_mouse_drag(col, row),
            Msg::MouseScroll { up } => return self.on_mouse_scroll(up),
            Msg::Resize => self.dirty = true,
            Msg::Quit => self.should_quit = true,
            Msg::Autoplay => return self.autoplay_on_start_cmds(),
            Msg::StatusTick => {
                // The status has been covering the title long enough — clear it so the
                // wrapper above nulls `status_set_at` and the next frame redraws the title.
                if matches!(self.status_set_at, Some(t) if t.elapsed() >= STATUS_TTL) {
                    self.status.clear();
                    self.dirty = true;
                }
            }
            Msg::AnimTick => {
                // Advance the animation phase and request a frame. The main loop only delivers
                // this while `animation_active()` is true, so the wrapping `wrapping_add` and a
                // single redraw are the entire per-frame cost; idle when animations are off.
                self.anim_frame = self.anim_frame.wrapping_add(1);
                self.dirty = true;
            }
            Msg::PlayerTimePos(t) => {
                self.time_pos = Some(t);
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
                    tracing::info!(time_pos = t, "progress");
                }
            }
            Msg::PlayerDuration(d) => {
                self.duration = Some(d);
                self.dirty = true;
            }
            Msg::PlayerPaused(p) => {
                self.paused = p;
                self.dirty = true;
            }
            Msg::PlayerVolume(v) => {
                self.volume = v.round() as i64;
                self.dirty = true;
                tracing::info!(volume = self.volume, "volume");
            }
            Msg::PlayerEof => {
                tracing::info!("track ended (eof)");
                // The just-finished track played to its end → a full-play signal, then advance.
                let mut cmds = self.record_outgoing(true);
                cmds.extend(self.advance(true));
                return cmds;
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
                    prefetched = self.last_load_prefetched,
                    "playback error"
                );
                self.consecutive_play_errors = self.consecutive_play_errors.saturating_add(1);
                // A single bad track shouldn't strand the user: skip it and play on. The
                // cursor moves, so the title refreshes to the next track. Bail out once too
                // many fail in a row (offline / bad cookie) so we don't skip-storm.
                if self.consecutive_play_errors <= MAX_CONSECUTIVE_PLAY_ERRORS
                    && self.queue.peek_next().is_some()
                {
                    // `advance(false)` always moves on (ignores repeat-one), unlike an EOF.
                    let cmds = self.advance(false);
                    self.status = t!("⚠ Track unavailable — skipped to next", "⚠ 재생할 수 없는 곡 — 다음 곡으로 건너뜀").to_owned();
                    self.dirty = true;
                    return cmds;
                }
                self.status = if self.consecutive_play_errors > MAX_CONSECUTIVE_PLAY_ERRORS {
                    t!(
                        "Several tracks failed to play — stopped. Check your connection, or sign in (cookies) for gated tracks.",
                        "여러 곡 재생에 실패해서 중단했어요. 연결을 확인하거나, 제한된 곡은 로그인(쿠키)하세요."
                    ).to_owned()
                } else {
                    format!("{}: {e}", t!("Playback error", "재생 오류"))
                };
                self.dirty = true;
            }
            Msg::SearchResults { query, songs } => {
                self.search.searching = false;
                if songs.is_empty() {
                    self.status = if crate::i18n::is_korean() {
                        format!("\"{query}\" 검색 결과 없음")
                    } else {
                        format!("No results for \"{query}\"")
                    };
                    self.search.results.clear();
                } else {
                    self.status.clear();
                    self.search.results = songs;
                    self.search.selected = 0;
                    self.search_scroll.reset();
                    self.search.focus = SearchFocus::Results;
                }
                self.dirty = true;
            }
            Msg::SearchError(e) => {
                self.search.searching = false;
                self.status = format!("{}: {e}", t!("Search error", "검색 오류"));
                self.dirty = true;
            }
            Msg::DownloadsScanned(songs) => {
                self.downloaded_tracks = songs;
                let len = self.library_len();
                if self.library_selected >= len {
                    self.library_selected = len.saturating_sub(1);
                }
                self.dirty = true;
            }
            Msg::LyricsResult { video_id, lines } => {
                self.lyrics_loading = false;
                // Ignore stale results for a track we've already skipped past.
                if self.queue.current().is_some_and(|s| s.video_id == video_id) {
                    self.lyrics = Some(TrackLyrics { video_id, lines });
                    self.dirty = true;
                }
            }
            Msg::ArtworkResult { video_id, image } => {
                self.art_loading = false;
                // Drop results for a track we've already skipped past.
                if self.queue.current().is_some_and(|s| s.video_id == video_id) {
                    self.set_artwork(video_id, image);
                    self.dirty = true;
                }
            }
            Msg::DownloadProgress { video_id, percent } => {
                self.downloads
                    .insert(video_id, DownloadState::Running(percent.round() as u8));
                self.dirty = true;
            }
            Msg::DownloadDone { video_id, path } => {
                self.downloads.insert(video_id.clone(), DownloadState::Done);
                if !path.trim().is_empty() {
                    let local = self
                        .download_sources
                        .remove(&video_id)
                        .map(|source| source.with_local_path(PathBuf::from(&path)))
                        .unwrap_or_else(|| Song::local_file(PathBuf::from(&path)));
                    self.add_downloaded_track(local);
                }
                self.status = format!("{}: {path}", t!("Saved", "저장됨"));
                self.dirty = true;
            }
            Msg::DownloadError { video_id, error } => {
                self.downloads
                    .insert(video_id.clone(), DownloadState::Failed);
                self.download_sources.remove(&video_id);
                self.status = format!("{}: {error}", t!("Download failed", "다운로드 실패"));
                self.dirty = true;
            }
            Msg::Resolved {
                video_id,
                stream_url,
            } => {
                // Bounded prefetch cache; no redraw (purely a skip-latency optimization).
                if self.resolved.len() >= RESOLVED_MAX {
                    self.resolved.clear();
                }
                self.resolved.insert(video_id, stream_url);
            }
            Msg::RadioResults {
                seed_video_id,
                candidates,
            } => {
                self.radio_pending = false;
                if self.autoplay_radio && self.queue.contains_video_id(&seed_video_id) {
                    // With a key + reranker enabled, hand the model a diverse local shortlist to
                    // reorder (ids only); otherwise rank the pool purely locally. Either way the
                    // pool went through scoring + MMR + cooldown — never taken verbatim.
                    if self.ai.available && self.config.radio.ai.enabled {
                        return self.start_ai_rerank(&seed_video_id, candidates);
                    }
                    let picks = self.plan_local_radio(&seed_video_id, candidates);
                    return self.extend_queue_from_radio(picks);
                } else {
                    self.dirty = true;
                }
            }
            Msg::RadioAiPicks { seed_video_id, ids } => {
                self.ai.thinking = false;
                self.dirty = true;
                // Only consume `pending_rerank` when this result is for it (a stale/duplicate
                // message for some other seed leaves the current rerank untouched). When it does
                // match but the seed is no longer queued (the user skipped/cleared mid-think),
                // the chain still drops the stale rerank without enqueuing.
                let ours = self
                    .pending_rerank
                    .as_ref()
                    .is_some_and(|p| p.seed_video_id == seed_video_id);
                if ours
                    && let Some(pending) = self.pending_rerank.take()
                    && self.autoplay_radio
                    && self.queue.contains_video_id(&seed_video_id)
                {
                    let picks = radio::merge_ai_picks(
                        &ids,
                        &pending.shortlist,
                        &pending.local_pick,
                        self.config.radio.ai.picks,
                    );
                    return self.extend_queue_from_radio(picks);
                }
            }
            Msg::RadioError {
                seed_video_id,
                error,
            } => {
                self.radio_pending = false;
                if self.autoplay_radio && self.queue.contains_video_id(&seed_video_id) {
                    self.note_radio_failure(format!("{}: {error}", t!("Autoplay radio failed", "자동재생 라디오 실패")));
                } else {
                    self.dirty = true;
                }
            }

            // --- AI assistant intents ---------------------------------------
            Msg::AiThinking(on) => {
                self.ai.thinking = on;
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
                    self.queue.set(songs, 0);
                    self.status.clear();
                    let song = self.queue.current().cloned();
                    return self.load_song(song);
                }
            }
            Msg::AiEnqueue(songs) => {
                return self.extend_queue_from_radio(songs);
            }
            Msg::AiSuggestions(songs) => {
                self.ai.suggestions = songs;
                self.ai.suggestions_selected = 0;
                self.ai_scroll.reset();
                self.dirty = true;
            }
            Msg::AiSetAutoplay(on) => {
                self.autoplay_radio = on;
                self.dirty = true;
                if on {
                    self.consecutive_radio_failures = 0;
                    // Same proactive top-up as the manual toggle (see Action::ToggleRadio).
                    return self.maybe_autoplay_extend();
                }
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
                    self.queue.set(songs, 0);
                    self.status.clear();
                    let song = self.queue.current().cloned();
                    return self.load_song(song);
                }
            }
        }
        Vec::new()
    }


    /// The single footer hint shown across every view: just the live chord that opens the
    /// `?` cheat-sheet (which already lists every binding for every screen). Built from the
    /// keymap so remapping "toggle help" updates the hint in lock-step.
    pub fn help_footer(&self) -> String {
        format!(
            "{}  keybindings",
            self.keymap.label(KeyContext::Global, Action::ToggleHelp)
        )
    }


    /// Return to the player/home screen from any mode. Settings use the normal close path
    /// so draft values and keybinding changes are not silently discarded.
    fn go_home(&mut self) -> Vec<Cmd> {
        self.help_visible = false;
        self.eq_dropdown_open = false;
        self.radio_dropdown_open = false;
        self.queue_popup_open = false;
        self.confirm_delete_files = None;
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
        let rows = self.list_viewport_rows.get() as usize;
        if rows <= 1 { DEFAULT_PAGE_ROWS } else { rows - 1 }
    }


    /// Switch screens from a nav-bar click — the mouse equivalent of the `Open*` keys, but
    /// reachable from any screen. Leaving Settings commits the draft via the normal close
    /// path so edits aren't lost; transient overlays are cleared.
    fn navigate_to(&mut self, mode: Mode) -> Vec<Cmd> {
        self.eq_dropdown_open = false;
        self.radio_dropdown_open = false;
        self.queue_popup_open = false;
        self.confirm_delete_files = None;
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
            }
            Mode::Library => {
                self.mode = Mode::Library;
                self.library_selected = 0;
                self.library_anchor = 0;
                self.library_scroll.reset();
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
            let fields = st.tab.fields();
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

/// Open `url` in the system's default browser, fire-and-forget. Spawns the platform opener
/// (`open` / `xdg-open` / `cmd start`) detached with stdio nulled so it can't touch the TUI's
/// terminal; any failure (no opener installed) is ignored — the URL is also shown in the card.
fn open_in_browser(url: &str) {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        // `start` is a cmd builtin; the empty "" is its (ignored) window-title argument.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    let _ = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// Copy `text` to the system clipboard, fire-and-forget. Mirrors `open_in_browser`: spawns the
/// platform clipboard tool with stdio nulled and pipes `text` to its stdin, so no native
/// clipboard crate is needed. macOS uses `pbcopy`, Windows `clip`; Linux tries `wl-copy`
/// (Wayland) then `xclip`/`xsel` (X11), each of which self-daemonizes to keep the selection
/// alive after we return. Any failure (tool absent) is silently ignored.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn pipe(cmd: &mut Command, text: &str) -> bool {
        match cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
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
        pipe(&mut Command::new("pbcopy"), text);
    } else if cfg!(target_os = "windows") {
        pipe(&mut Command::new("clip"), text);
    } else if !pipe(&mut Command::new("wl-copy"), text)
        && !pipe(Command::new("xclip").args(["-selection", "clipboard"]), text)
    {
        pipe(Command::new("xsel").arg("-ib"), text);
    }
}

/// Spawn a borderless, always-on-top mpv overlay window for `url`, returning the child so the
/// caller can track and later close it. Stdio is nulled so mpv can't touch the TUI's terminal.
/// Cookies are forwarded to mpv's bundled yt-dlp (same option as the audio instance) when set;
/// `--no-config` is intentionally omitted so the user's own mpv config applies to the video.
fn spawn_video_overlay(
    url: &str,
    cookies: Option<&std::path::Path>,
    layout: crate::config::VideoOverlay,
) -> Option<std::process::Child> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("mpv");
    cmd.arg(url);
    for arg in layout.mpv_window_args() {
        cmd.arg(arg);
    }
    if let Some(path) = cookies {
        cmd.arg(format!("--ytdl-raw-options-append=cookies={}", path.display()));
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
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
