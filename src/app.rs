//! Application state and the TEA-style reducer.
//!
//! All mutable state lives in [`App`] on the main task. Inbound events and actor
//! results arrive as [`Msg`]; `update` is the single place that mutates state and
//! returns the [`Cmd`]s the run loop should dispatch to actors. Keeping `update` pure
//! (state in, `Cmd`s out — no IO) makes it directly unit-testable.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::ai::GeminiModel;
use crate::api::Song;
use crate::config::{Config, SPEED_MAX, SPEED_MIN};
use crate::eq::{self, EqPreset};
use crate::keymap::{Action, Chord, KeyContext, KeyMap};
use crate::library::Library;
use crate::lyrics::LyricLine;
use crate::player::PlayerCmd;
use crate::playlists::Playlists;
use crate::queue::Queue;
use crate::settings::{self, Field, FieldKind, SettingsDraft, SettingsState, SettingsTab};

/// Queue length at or below which the autoplay/radio hook tops up the queue.
const AUTOPLAY_THRESHOLD: usize = 3;
/// Minimum gap between autoplay top-up requests (avoids a request storm).
const AUTOPLAY_COOLDOWN: Duration = Duration::from_secs(60);
/// Consecutive empty radio extends before autoplay disables itself (circuit breaker).
const AUTOPLAY_MAX_FAILURES: u8 = 3;
/// Cap on AI chat transcript lines kept in memory (bounded memory).
const AI_HISTORY_MAX: usize = 200;

/// Seconds jumped per ←/→ press.
const SEEK_STEP: f64 = 10.0;
/// Percentage points changed per volume keypress.
const VOLUME_STEP: i64 = 5;
/// Highest volume the UI sets (mpv would allow more, but 100 is a sane v1 ceiling).
const VOLUME_MAX: i64 = 100;
/// Cap on cached prefetched stream URLs (bounded memory; we only look a step ahead).
const RESOLVED_MAX: usize = 16;
/// How many tracks in a row may fail before we stop auto-skipping and surface the error.
/// A single unplayable track (expired URL, region/age-gated, throttled) shouldn't halt
/// the session, but a systemic failure (offline, bad cookie) shouldn't skip-storm the
/// whole queue either — so we skip a few, then stop and explain.
const MAX_CONSECUTIVE_PLAY_ERRORS: u8 = 3;
/// Playback-speed change per `>`/`<` press.
const SPEED_STEP: f64 = 0.1;

/// Everything that can change the application state.
pub enum Msg {
    Key(KeyEvent),
    /// A left-click at a terminal cell (1-based crossterm coords); may hit the seekbar.
    MouseClick { col: u16, row: u16 },
    /// The terminal was resized; ratatui auto-resizes on draw, we just redraw.
    Resize,
    /// A termination signal asked us to shut down.
    Quit,
    /// mpv playback position, in seconds.
    PlayerTimePos(f64),
    /// Current track duration, in seconds.
    PlayerDuration(f64),
    /// mpv pause state changed.
    PlayerPaused(bool),
    /// mpv volume changed (0-100, but mpv can report fractional/over-100 values).
    PlayerVolume(f64),
    /// The current track reached its end.
    PlayerEof,
    /// mpv reported a playback error.
    PlayerError(String),
    /// Search returned results (possibly empty) for `query`.
    SearchResults { query: String, songs: Vec<Song> },
    /// Search failed.
    SearchError(String),
    /// Synced lyrics for `video_id` (empty `lines` = none found).
    LyricsResult { video_id: String, lines: Vec<LyricLine> },
    /// Download progress for `video_id` (0-100).
    DownloadProgress { video_id: String, percent: f64 },
    /// A download finished, saved at `path`.
    DownloadDone { video_id: String, path: String },
    /// A download failed.
    DownloadError { video_id: String, error: String },
    /// A track's direct stream URL was prefetched (for instant skip).
    Resolved { video_id: String, stream_url: String },

    // AI assistant: intents emitted by the AI actor, applied here by `update()`.
    /// The assistant started/finished a turn (drives the thinking spinner).
    AiThinking(bool),
    /// Assistant chat text to append to the transcript.
    AiChat(String),
    /// An AI error to surface in the transcript (also clears the spinner).
    AiError(String),
    /// Replace the queue with these tracks and start playing (play_music/play_playlist).
    AiPlayTracks(Vec<Song>),
    /// Append these tracks to the queue (add_to_queue/start_radio).
    AiEnqueue(Vec<Song>),
    /// Populate the pickable related-tracks list (get_suggestions).
    AiSuggestions(Vec<Song>),
    /// Turn autoplay/radio on or off (start_radio/stop_radio).
    AiSetAutoplay(bool),
    /// Create a local playlist with this name (create_playlist).
    AiCreatePlaylist(String),
    /// Add these tracks to a local playlist by id or name (add_to_playlist).
    AiAddToPlaylist { playlist: String, songs: Vec<Song> },
    /// Play a local playlist by id or name (play_playlist).
    AiPlayPlaylist(String),
}

/// Side effects the reducer asks the run loop to perform.
pub enum Cmd {
    Player(PlayerCmd),
    Search(String),
    /// Persist the library (favorites/history) to disk.
    SaveLibrary,
    /// Fetch synced lyrics for a track.
    FetchLyrics { video_id: String, artist: String, title: String },
    /// Download a track to disk (best audio + tags + cover art).
    Download(Song),
    /// Prefetch a track's direct stream URL for instant skip.
    Resolve { video_id: String, watch_url: String },
    /// Persist the given config to disk (settings screen, on save).
    SaveConfig(Box<Config>),
    /// Persist the local playlists to disk (after an AI playlist mutation).
    SavePlaylists,
    /// Ask the AI assistant to handle a prompt, given a read-only state snapshot.
    AskAi { prompt: String, context: Box<AiContext> },
    /// Switch the running AI actor's model (settings save). No effect without a key.
    SetAiModel(GeminiModel),
    /// (Re)build the AI actor with a new key + model (settings save, key changed). A
    /// `None` key tears the assistant down; a valid key brings it up live — so a key
    /// entered at runtime takes effect immediately, with no relaunch.
    ReloadAi { key: Option<String>, model: GeminiModel },
}

/// A clickable terminal region's semantic target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseTarget {
    Global(Action),
    Player(Action),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseButtonRegion {
    pub rect: Rect,
    pub target: MouseTarget,
}

/// Who authored a line in the AI chat transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRole {
    User,
    Ai,
    Error,
}

/// One line in the AI chat transcript.
pub struct AiMessage {
    pub role: AiRole,
    pub text: String,
}

/// Within the AI screen, whether the input box or the suggestions list has focus.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AiFocus {
    Input,
    Suggestions,
}

/// A local playlist's identity, for the AI context snapshot (no track payload).
#[derive(Debug, Clone)]
pub struct PlaylistInfo {
    pub id: String,
    pub name: String,
    pub count: usize,
}

/// A read-only snapshot of app state handed to the AI actor with each prompt, so its
/// read tools (get_queue, get_user_favorites, …) can answer without touching `App`.
#[derive(Debug, Clone)]
pub struct AiContext {
    /// "Title — Artist" of the current track, if any.
    pub current_track: Option<String>,
    /// Up to a few upcoming queue entries, "Title — Artist".
    pub queue_upcoming: Vec<String>,
    pub queue_len: usize,
    pub queue_remaining: usize,
    /// A few recently-played tracks, most-recent first.
    pub recent_history: Vec<String>,
    /// A sample of favorited tracks.
    pub favorites: Vec<String>,
    /// The user's local playlists (names + counts; tracks fetched on demand).
    pub playlists: Vec<PlaylistInfo>,
    /// Whether a YTM cookie is configured (gates authenticated related-tracks).
    pub authenticated: bool,
    pub autoplay_radio: bool,
}

/// Per-track download state, for the UI indicator.
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadState {
    Running(u8),
    Done,
    Failed,
}

/// Which screen is active.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Mode {
    Player,
    Search,
    Library,
    Settings,
    Ai,
}

/// Synced lyrics for one track (held while it's the current track).
pub struct TrackLyrics {
    pub video_id: String,
    pub lines: Vec<LyricLine>,
}

/// The two lists in the library view.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum LibraryTab {
    Favorites,
    History,
}

impl LibraryTab {
    fn toggled(self) -> Self {
        match self {
            LibraryTab::Favorites => LibraryTab::History,
            LibraryTab::History => LibraryTab::Favorites,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LibraryTab::Favorites => "Favorites",
            LibraryTab::History => "History",
        }
    }
}

/// Within the search screen, whether the query box or the results list has focus.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SearchFocus {
    Input,
    Results,
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
    /// Whether the `?` help / cheat-sheet overlay is shown.
    pub help_visible: bool,

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

    // Audio / EQ --------------------------------------------------------------
    /// Selected equalizer preset (drives `eq_bands` when chosen via `e`).
    pub eq_preset: EqPreset,
    /// Current per-band gains (dB); editable live from the settings screen.
    pub eq_bands: [f64; eq::BANDS],
    /// Loudness normalization (`dynaudnorm`) on/off.
    pub normalize: bool,
    /// Playback speed multiplier (1.0 = normal).
    pub speed: f64,
    /// Auto-extend the queue with related tracks when it runs low (radio mode).
    pub autoplay_radio: bool,

    // Settings ----------------------------------------------------------------
    /// The persisted config, kept so the settings screen can save the full file.
    pub config: Config,
    /// The settings screen state, present only while `Mode::Settings` is active.
    pub settings: Option<Box<SettingsState>>,

    // AI assistant ------------------------------------------------------------
    /// Whether a Gemini API key is configured (gates the assistant; `false` → onboarding).
    pub ai_available: bool,
    /// The Gemini model the assistant uses (shown in the AI view header).
    pub gemini_model: GeminiModel,
    /// The chat transcript (user prompts, assistant replies, errors).
    pub ai_messages: Vec<AiMessage>,
    /// The AI prompt being typed.
    pub ai_input: String,
    /// True while a request is in flight (drives the spinner; blocks a second request).
    pub ai_thinking: bool,
    /// The pickable related-tracks list (get_suggestions).
    pub ai_suggestions: Vec<Song>,
    pub ai_suggestions_selected: usize,
    /// Whether the input box or the suggestions list has focus in the AI view.
    pub ai_focus: AiFocus,
    /// When the autoplay hook last fired a top-up request (for the cooldown).
    ai_last_extend: Option<Instant>,
    /// Consecutive empty radio extends, for the autoplay circuit breaker.
    consecutive_radio_failures: u8,
    /// Consecutive mpv playback errors with no track playing in between, for the
    /// auto-skip circuit breaker (see [`MAX_CONSECUTIVE_PLAY_ERRORS`]).
    consecutive_play_errors: u8,
    /// The user's local playlists (the AI playlist tools read/write these).
    pub playlists: Playlists,

    // Search ------------------------------------------------------------------
    pub search_input: String,
    pub search_focus: SearchFocus,
    pub search_results: Vec<Song>,
    pub search_selected: usize,
    pub searching: bool,

    // Library -----------------------------------------------------------------
    /// Favorites + play history, persisted to disk. Loaded by `main` after `new`.
    pub library: Library,
    pub library_tab: LibraryTab,
    pub library_selected: usize,

    // Lyrics ------------------------------------------------------------------
    /// Whether the lyrics panel is shown in the player view.
    pub lyrics_visible: bool,
    /// True between requesting lyrics and the result arriving.
    pub lyrics_loading: bool,
    /// Lyrics for the current track, if fetched.
    pub lyrics: Option<TrackLyrics>,

    // Downloads ---------------------------------------------------------------
    /// In-flight / finished downloads, keyed by `video_id`, for the UI indicator.
    pub downloads: HashMap<String, DownloadState>,

    // Prefetch ----------------------------------------------------------------
    /// Pre-resolved direct stream URLs, keyed by `video_id` (for instant skip).
    resolved: HashMap<String, String>,
    /// Whether the current track was loaded from a prefetched direct URL (vs the watch
    /// URL mpv resolves itself). Recorded so a playback error can note the likelier cause
    /// (a stale prefetched CDN URL) in the log.
    last_load_prefetched: bool,

    /// Screen rect of the seekbar, written by the player view each render so a mouse
    /// click can be hit-tested against it. `Cell` because render only has `&App`.
    pub seekbar_rect: Cell<Option<Rect>>,
    /// Clickable button rects written by views each render. `RefCell` because render only
    /// has `&App`, but the reducer needs the last rendered hit map.
    pub mouse_buttons: RefCell<Vec<MouseButtonRegion>>,

    /// Last whole second we redrew for, so sub-second `time-pos` spam is coalesced.
    last_shown_sec: i64,
}

impl App {
    pub fn new(volume: i64) -> Self {
        Self {
            should_quit: false,
            dirty: true,
            mode: Mode::Player,
            authenticated: false,
            keymap: KeyMap::default(),
            help_visible: false,
            time_pos: None,
            duration: None,
            paused: false,
            volume: volume.clamp(0, VOLUME_MAX),
            queue: Queue::default(),
            status: String::new(),
            eq_preset: EqPreset::default(),
            eq_bands: [0.0; eq::BANDS],
            normalize: false,
            speed: 1.0,
            autoplay_radio: false,
            config: Config::default(),
            settings: None,
            ai_available: false,
            gemini_model: GeminiModel::default(),
            ai_messages: Vec::new(),
            ai_input: String::new(),
            ai_thinking: false,
            ai_suggestions: Vec::new(),
            ai_suggestions_selected: 0,
            ai_focus: AiFocus::Input,
            ai_last_extend: None,
            consecutive_radio_failures: 0,
            consecutive_play_errors: 0,
            playlists: Playlists::default(),
            search_input: String::new(),
            search_focus: SearchFocus::Input,
            search_results: Vec::new(),
            search_selected: 0,
            searching: false,
            library: Library::default(),
            library_tab: LibraryTab::Favorites,
            library_selected: 0,
            lyrics_visible: false,
            lyrics_loading: false,
            lyrics: None,
            downloads: HashMap::new(),
            resolved: HashMap::new(),
            seekbar_rect: Cell::new(None),
            mouse_buttons: RefCell::new(Vec::new()),
            last_shown_sec: -1,
            last_load_prefetched: false,
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
        self.autoplay_radio = cfg.effective_autoplay_radio();
        self.ai_available = cfg.effective_gemini_api_key().is_some();
        self.gemini_model = cfg.effective_gemini_model();
        self.keymap = KeyMap::from_config(cfg);
        // Keep the full config so the settings screen can persist the whole file.
        self.config = cfg.clone();
    }

    /// The mpv `af` filter chain for the current EQ + normalization state, or `None` when
    /// nothing is active (the caller then clears `af`).
    fn current_af(&self) -> Option<String> {
        eq::build_af_string(&self.eq_bands, self.normalize)
    }

    /// Change playback speed by `delta`, clamped and rounded to one decimal, and emit the
    /// `set_property speed` command.
    fn adjust_speed(&mut self, delta: f64) -> Vec<Cmd> {
        self.speed = (((self.speed + delta) * 10.0).round() / 10.0).clamp(SPEED_MIN, SPEED_MAX);
        self.status = format!("Speed: {:.1}x", self.speed);
        self.dirty = true;
        vec![Cmd::Player(PlayerCmd::SetProperty {
            name: "speed".to_owned(),
            value: serde_json::Value::from(self.speed),
        })]
    }

    /// The reducer: apply one message, returning effects for the run loop to dispatch.
    pub fn update(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::Key(k) => return self.on_key(k),
            Msg::MouseClick { col, row } => return self.on_mouse_click(col, row),
            Msg::Resize => self.dirty = true,
            Msg::Quit => self.should_quit = true,
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
                return self.advance(true);
            }
            Msg::PlayerError(e) => {
                // Log *which* track failed and whether it came from a (possibly stale)
                // prefetched URL. `e` already carries mpv's own reason (its `file_error`
                // end-file field — the closest thing to a "why": HTTP 403, unsupported, …).
                let failed = self.queue.current().map(|s| format!("{} — {}", s.title, s.artist));
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
                    self.status = "⚠ Track unavailable — skipped to next".to_owned();
                    self.dirty = true;
                    return cmds;
                }
                self.status = if self.consecutive_play_errors > MAX_CONSECUTIVE_PLAY_ERRORS {
                    "Several tracks failed to play — stopped. Check your connection, or sign in (cookies) for gated tracks.".to_owned()
                } else {
                    format!("Playback error: {e}")
                };
                self.dirty = true;
            }
            Msg::SearchResults { query, songs } => {
                self.searching = false;
                if songs.is_empty() {
                    self.status = format!("No results for \"{query}\"");
                    self.search_results.clear();
                } else {
                    self.status.clear();
                    self.search_results = songs;
                    self.search_selected = 0;
                    self.search_focus = SearchFocus::Results;
                }
                self.dirty = true;
            }
            Msg::SearchError(e) => {
                self.searching = false;
                self.status = format!("Search error: {e}");
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
            Msg::DownloadProgress { video_id, percent } => {
                self.downloads.insert(video_id, DownloadState::Running(percent.round() as u8));
                self.dirty = true;
            }
            Msg::DownloadDone { video_id, path } => {
                self.downloads.insert(video_id, DownloadState::Done);
                self.status = format!("Saved: {path}");
                self.dirty = true;
            }
            Msg::DownloadError { video_id, error } => {
                self.downloads.insert(video_id, DownloadState::Failed);
                self.status = format!("Download failed: {error}");
                self.dirty = true;
            }
            Msg::Resolved { video_id, stream_url } => {
                // Bounded prefetch cache; no redraw (purely a skip-latency optimization).
                if self.resolved.len() >= RESOLVED_MAX {
                    self.resolved.clear();
                }
                self.resolved.insert(video_id, stream_url);
            }

            // --- AI assistant intents ---------------------------------------
            Msg::AiThinking(on) => {
                self.ai_thinking = on;
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
                self.ai_thinking = false;
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
                let added = self.queue.extend(songs);
                if added == 0 {
                    self.consecutive_radio_failures = self.consecutive_radio_failures.saturating_add(1);
                    // Circuit breaker: stop a radio that keeps coming up empty.
                    if self.autoplay_radio && self.consecutive_radio_failures >= AUTOPLAY_MAX_FAILURES {
                        self.autoplay_radio = false;
                        self.status = "Autoplay radio stopped (no related tracks found)".to_owned();
                    }
                } else {
                    self.consecutive_radio_failures = 0;
                    self.status = format!("Queued {added} track(s)");
                }
                self.dirty = true;
            }
            Msg::AiSuggestions(songs) => {
                self.ai_suggestions = songs;
                self.ai_suggestions_selected = 0;
                self.dirty = true;
            }
            Msg::AiSetAutoplay(on) => {
                self.autoplay_radio = on;
                if on {
                    self.consecutive_radio_failures = 0;
                }
                self.dirty = true;
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
                    if matches!(self.playlists.add(&playlist, song), crate::playlists::AddResult::Added) {
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

    fn on_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // Ctrl+C always quits, regardless of mode or remapping (a hard safety key that is
        // never part of the keymap, so the user can't lock themselves out).
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Vec::new();
        }
        let chord = Chord::from(k);

        // The keybinding editor's capture mode grabs the next key verbatim (except Esc),
        // so it must run before the global/help shortcuts could swallow it.
        if self.mode == Mode::Settings && self.settings.as_ref().is_some_and(|s| s.capturing.is_some())
        {
            return self.settings_capture_key(k);
        }

        // While the help overlay is up, swallow input; help-toggle / Esc / q dismiss it.
        if self.help_visible {
            let close = matches!(self.keymap.global_action(chord), Some(Action::ToggleHelp))
                || k.code == KeyCode::Esc
                || k.code == KeyCode::Char('q');
            if close {
                self.help_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // Global shortcuts (help, radio). Suppressed only when a *typeable* key would feed
        // a focused text field — so `?` types into the search box but opens help elsewhere,
        // while Ctrl-based globals (radio) keep working everywhere as before.
        if !(self.in_text_entry() && chord.is_typeable())
            && let Some(action) = self.keymap.global_action(chord)
        {
            match action {
                Action::ToggleHelp => {
                    self.help_visible = true;
                    self.dirty = true;
                    return Vec::new();
                }
                Action::ToggleRadio => {
                    self.autoplay_radio = !self.autoplay_radio;
                    self.status =
                        format!("Autoplay radio: {}", if self.autoplay_radio { "on" } else { "off" });
                    self.dirty = true;
                    return Vec::new();
                }
                _ => {}
            }
        }

        match self.mode {
            Mode::Player => self.on_key_player(k),
            Mode::Search => self.on_key_search(k),
            Mode::Library => self.on_key_library(k),
            Mode::Settings => self.on_key_settings(k),
            Mode::Ai => self.on_key_ai(k),
        }
    }

    /// The single footer hint shown across every view: just the live chord that opens the
    /// `?` cheat-sheet (which already lists every binding for every screen). Built from the
    /// keymap so remapping "toggle help" updates the hint in lock-step.
    pub fn help_footer(&self) -> String {
        format!("{}  keybindings", self.keymap.label(KeyContext::Global, Action::ToggleHelp))
    }

    pub fn clear_mouse_regions(&self) {
        self.seekbar_rect.set(None);
        self.mouse_buttons.borrow_mut().clear();
    }

    pub fn register_mouse_button(&self, rect: Rect, target: MouseTarget) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.mouse_buttons.borrow_mut().push(MouseButtonRegion { rect, target });
    }

    fn mouse_target_at(&self, col: u16, row: u16) -> Option<MouseTarget> {
        self.mouse_buttons
            .borrow()
            .iter()
            .rev()
            .find(|b| rect_contains(b.rect, col, row))
            .map(|b| b.target)
    }

    /// Whether a focused text field is currently capturing typed characters (so command
    /// keys and the `?` help shortcut must not fire — they'd be typed instead).
    fn in_text_entry(&self) -> bool {
        match self.mode {
            Mode::Search => self.search_focus == SearchFocus::Input,
            Mode::Ai => self.ai_focus == AiFocus::Input,
            Mode::Settings => self.settings.as_ref().is_some_and(|s| s.editing_text),
            _ => false,
        }
    }

    /// A left-click at `(col, row)`: buttons fire their mapped action; the player's
    /// seekbar seeks to the matching fraction of the track. Hit rects are published by
    /// views each render.
    fn on_mouse_click(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        if self.help_visible {
            self.help_visible = false;
            self.dirty = true;
            return Vec::new();
        }
        if let Some(target) = self.mouse_target_at(col, row) {
            return self.on_mouse_target(target);
        }
        if self.mode != Mode::Player {
            return Vec::new();
        }
        if let Some(area) = self.seekbar_rect.get()
            && let Some(dur) = self.duration
            && dur > 0.0
            && area.width > 0
            && rect_contains(area, col, row)
        {
            let frac = f64::from(col - area.x) / f64::from(area.width);
            let target = (frac * dur).clamp(0.0, dur);
            tracing::info!(secs = target, "click seek");
            self.dirty = true;
            return vec![Cmd::Player(PlayerCmd::SeekAbsolute(target))];
        }
        Vec::new()
    }

    fn on_mouse_target(&mut self, target: MouseTarget) -> Vec<Cmd> {
        match target {
            MouseTarget::Global(Action::ToggleHelp) => {
                self.help_visible = true;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::Global(_) => Vec::new(),
            MouseTarget::Player(action) if self.mode == Mode::Player => self.on_player_action(action),
            MouseTarget::Player(_) => Vec::new(),
        }
    }

    fn on_key_player(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match self.keymap.action(KeyContext::Player, k.into()) {
            Some(action) => self.on_player_action(action),
            None => Vec::new(),
        }
    }

    fn on_player_action(&mut self, action: Action) -> Vec<Cmd> {
        match action {
            Action::Quit | Action::Back => {
                self.should_quit = true;
                Vec::new()
            }
            Action::TogglePause => {
                // Optimistic toggle; mpv confirms via a `pause` property-change.
                self.paused = !self.paused;
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::CyclePause)]
            }
            Action::SeekBack => vec![Cmd::Player(PlayerCmd::SeekRelative(-SEEK_STEP))],
            Action::SeekForward => vec![Cmd::Player(PlayerCmd::SeekRelative(SEEK_STEP))],
            Action::VolUp => {
                self.volume = (self.volume + VOLUME_STEP).min(VOLUME_MAX);
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetVolume(self.volume))]
            }
            Action::VolDown => {
                self.volume = (self.volume - VOLUME_STEP).max(0);
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetVolume(self.volume))]
            }
            // Manual next: always moves on, even under repeat-one.
            Action::NextTrack => self.advance(false),
            Action::PrevTrack => {
                let song = self.queue.prev().cloned();
                self.load_song(song)
            }
            // Favorite the current track (the ♥ marker in the title is the feedback).
            Action::Favorite => {
                if let Some(song) = self.queue.current().cloned() {
                    self.library.toggle_favorite(&song);
                    self.dirty = true;
                    return vec![Cmd::SaveLibrary];
                }
                Vec::new()
            }
            Action::OpenLibrary => {
                self.mode = Mode::Library;
                self.library_selected = 0;
                self.dirty = true;
                Vec::new()
            }
            // Toggle the lyrics panel; fetch on first open for the current track.
            Action::ToggleLyrics => {
                self.lyrics_visible = !self.lyrics_visible;
                self.dirty = true;
                if self.lyrics_visible
                    && self.lyrics_stale()
                    && let Some(song) = self.queue.current().cloned()
                {
                    self.lyrics_loading = true;
                    return vec![fetch_lyrics_cmd(&song)];
                }
                Vec::new()
            }
            Action::Download => match self.queue.current().cloned() {
                Some(song) => self.start_download(song),
                None => Vec::new(),
            },
            Action::ToggleShuffle => {
                self.queue.toggle_shuffle();
                self.dirty = true;
                Vec::new()
            }
            Action::CycleRepeat => {
                self.queue.cycle_repeat();
                self.dirty = true;
                Vec::new()
            }
            // Cycle the EQ preset and apply it immediately.
            Action::CycleEq => {
                self.eq_preset = self.eq_preset.cycled();
                self.eq_bands = self.eq_preset.gains();
                self.status = format!("EQ: {}", self.eq_preset.label());
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetAudioFilter(self.current_af().unwrap_or_default()))]
            }
            Action::ToggleNormalize => {
                self.normalize = !self.normalize;
                self.status = format!("Normalize: {}", if self.normalize { "on" } else { "off" });
                self.dirty = true;
                vec![Cmd::Player(PlayerCmd::SetAudioFilter(self.current_af().unwrap_or_default()))]
            }
            Action::SpeedUp => self.adjust_speed(SPEED_STEP),
            Action::SpeedDown => self.adjust_speed(-SPEED_STEP),
            Action::OpenSettings => {
                self.open_settings();
                Vec::new()
            }
            Action::OpenAi => {
                self.enter_ai();
                Vec::new()
            }
            Action::OpenSearch => {
                self.mode = Mode::Search;
                self.search_focus = SearchFocus::Input;
                self.dirty = true;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_key_search(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match self.search_focus {
            SearchFocus::Input => {
                match self.keymap.action(KeyContext::SearchInput, k.into()) {
                    Some(Action::Back) => {
                        self.mode = Mode::Player;
                        self.dirty = true;
                        return Vec::new();
                    }
                    Some(Action::Confirm) => {
                        let q = self.search_input.trim().to_owned();
                        self.dirty = true;
                        return if q.is_empty() {
                            Vec::new()
                        } else {
                            self.searching = true;
                            self.status.clear();
                            vec![Cmd::Search(q)]
                        };
                    }
                    Some(Action::DeleteChar) => {
                        self.search_input.pop();
                        self.dirty = true;
                        return Vec::new();
                    }
                    Some(Action::MoveDown) if !self.search_results.is_empty() => {
                        self.search_focus = SearchFocus::Results;
                        self.dirty = true;
                        return Vec::new();
                    }
                    _ => {}
                }
                // Anything else types into the query box (player keys never leak here).
                if let KeyCode::Char(c) = k.code
                    && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    self.search_input.push(c);
                    self.dirty = true;
                }
                Vec::new()
            }
            SearchFocus::Results => match self.keymap.action(KeyContext::SearchResults, k.into()) {
                Some(Action::Back) => {
                    self.mode = Mode::Player;
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveUp) => {
                    if self.search_selected == 0 {
                        self.search_focus = SearchFocus::Input;
                    } else {
                        self.search_selected -= 1;
                    }
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveDown) => {
                    if self.search_selected + 1 < self.search_results.len() {
                        self.search_selected += 1;
                    }
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::Confirm) => self.play_selected(),
                // Favorite the highlighted result (♥ appears on the row).
                Some(Action::Favorite) => {
                    if let Some(song) = self.search_results.get(self.search_selected).cloned() {
                        self.library.toggle_favorite(&song);
                        self.dirty = true;
                        return vec![Cmd::SaveLibrary];
                    }
                    Vec::new()
                }
                Some(Action::Download) => match self.search_results.get(self.search_selected).cloned() {
                    Some(song) => self.start_download(song),
                    None => Vec::new(),
                },
                Some(Action::FocusInput) => {
                    self.search_focus = SearchFocus::Input;
                    self.dirty = true;
                    Vec::new()
                }
                _ => Vec::new(),
            },
        }
    }

    fn on_key_library(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let len = self.library_len();
        match self.keymap.action(KeyContext::Library, k.into()) {
            Some(Action::Back) => {
                self.mode = Mode::Player;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::Quit) => {
                self.should_quit = true;
                Vec::new()
            }
            Some(Action::FocusNext | Action::FocusPrev) => {
                self.library_tab = self.library_tab.toggled();
                self.library_selected = 0;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::MoveUp) => {
                self.library_selected = self.library_selected.saturating_sub(1);
                self.dirty = true;
                Vec::new()
            }
            Some(Action::MoveDown) => {
                if self.library_selected + 1 < len {
                    self.library_selected += 1;
                }
                self.dirty = true;
                Vec::new()
            }
            Some(Action::Confirm) => self.play_from_library(),
            Some(Action::OpenAi) => {
                self.enter_ai();
                Vec::new()
            }
            Some(Action::Download) => match self.selected_library_song() {
                Some(song) => self.start_download(song),
                None => Vec::new(),
            },
            // Un/favorite the highlighted track (removing shifts selection up).
            Some(Action::Favorite) => {
                if let Some(song) = self.selected_library_song() {
                    self.library.toggle_favorite(&song);
                    let new_len = self.library_len();
                    if self.library_selected >= new_len {
                        self.library_selected = new_len.saturating_sub(1);
                    }
                    self.dirty = true;
                    return vec![Cmd::SaveLibrary];
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    // --- Settings screen ----------------------------------------------------

    /// Open the settings screen, snapshotting the current persisted + live state into an
    /// editable draft.
    fn open_settings(&mut self) {
        let path_str = |p: &Option<std::path::PathBuf>| {
            p.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        };
        let draft = SettingsDraft {
            cookies_file: path_str(&self.config.cookies_file),
            download_dir: path_str(&self.config.download_dir),
            mouse: self.config.effective_mouse(),
            speed: self.speed,
            gapless: self.config.effective_gapless(),
            autoplay_radio: self.autoplay_radio,
            eq_preset: self.eq_preset,
            eq_bands: self.eq_bands,
            normalize: self.normalize,
            gemini_model: self.gemini_model,
            // Deliberately the *raw* config key, not `effective_gemini_api_key()`: seeding the
            // env-provided value would let a save copy it into config.json (persisting a key
            // the user chose to keep only in the environment). The cost is that an env-only
            // key shows "(none)" here; the AI still works and README documents env-wins.
            gemini_api_key: self.config.gemini_api_key.clone().unwrap_or_default(),
        };
        self.settings = Some(Box::new(SettingsState {
            tab: SettingsTab::General,
            row: 0,
            draft,
            editing_text: false,
            secret_restore: None,
            keymap: self.keymap.clone(),
            capturing: None,
        }));
        self.mode = Mode::Settings;
        self.status.clear();
        self.dirty = true;
    }

    fn on_key_settings(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // While editing a text field, keys feed the buffer until Enter/Esc commits it.
        if self.settings.as_ref().is_some_and(|s| s.editing_text) {
            return self.settings_edit_text(k);
        }
        let on_keys_tab = self.settings.as_ref().is_some_and(|s| s.tab == SettingsTab::Keys);
        // The editor must stay operable no matter how keys are remapped, so the literal
        // arrows / Enter / Esc / Tab are always honored here, on top of the configured ones.
        let action = self
            .keymap
            .action(KeyContext::Settings, k.into())
            .or_else(|| Self::settings_safety_action(k));
        match action {
            // `q`/Esc and `s` both commit the draft before leaving the screen. The key
            // name stays SettingsCancel for compatibility with existing keybinding ids.
            Some(Action::SettingsCancel | Action::Back | Action::SettingsSave) => self.close_settings(),
            Some(Action::FocusNext) => {
                self.settings_switch_tab(true);
                Vec::new()
            }
            Some(Action::FocusPrev) => {
                self.settings_switch_tab(false);
                Vec::new()
            }
            Some(Action::MoveUp) => {
                self.settings_move_row(-1);
                Vec::new()
            }
            Some(Action::MoveDown) => {
                self.settings_move_row(1);
                Vec::new()
            }
            Some(Action::ChangeDecrease) if !on_keys_tab => self.settings_change(-1),
            Some(Action::ChangeIncrease) if !on_keys_tab => self.settings_change(1),
            Some(Action::Confirm) => {
                if on_keys_tab {
                    self.settings_begin_capture();
                    Vec::new()
                } else {
                    self.settings_activate()
                }
            }
            // Reset the highlighted binding to its default (Keys tab only).
            Some(Action::DeleteChar) if on_keys_tab => {
                self.settings_reset_binding();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Literal navigation keys the settings editor always accepts, so a user can never
    /// remap themselves out of the screen that edits keybindings.
    fn settings_safety_action(k: KeyEvent) -> Option<Action> {
        match k.code {
            KeyCode::Up => Some(Action::MoveUp),
            KeyCode::Down => Some(Action::MoveDown),
            KeyCode::Left => Some(Action::ChangeDecrease),
            KeyCode::Right => Some(Action::ChangeIncrease),
            KeyCode::Enter => Some(Action::Confirm),
            KeyCode::Esc => Some(Action::Back),
            KeyCode::Tab => Some(Action::FocusNext),
            KeyCode::BackTab => Some(Action::FocusPrev),
            KeyCode::Backspace => Some(Action::DeleteChar),
            _ => None,
        }
    }

    /// The `(context, action)` the Keys-tab cursor is on, if the Keys tab is active.
    fn settings_current_binding(&self) -> Option<(KeyContext, Action)> {
        let st = self.settings.as_ref()?;
        if st.tab != SettingsTab::Keys {
            return None;
        }
        crate::keymap::editable_entries().get(st.row).copied()
    }

    /// Enter key-capture mode for the highlighted binding (Keys tab). The next keypress
    /// becomes the new chord (handled in [`Self::settings_capture_key`]).
    fn settings_begin_capture(&mut self) {
        if let Some(entry) = self.settings_current_binding()
            && let Some(st) = self.settings.as_mut()
        {
            st.capturing = Some(entry);
            self.status = "Press a key to bind (Esc to cancel)…".to_owned();
            self.dirty = true;
        }
    }

    /// Consume the captured keypress as the new chord for the binding being edited. Esc
    /// cancels; a conflict is rejected with a status message (the old binding is kept).
    fn settings_capture_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let Some((ctx, action)) = self.settings.as_mut().and_then(|s| s.capturing.take()) else {
            return Vec::new();
        };
        self.dirty = true;
        if k.code == KeyCode::Esc {
            self.status = "Rebinding cancelled".to_owned();
            return Vec::new();
        }
        let chord = Chord::from(k);
        let Some(st) = self.settings.as_mut() else {
            return Vec::new();
        };
        match st.keymap.rebind(ctx, action, chord) {
            Ok(()) => {
                self.status =
                    format!("Bound {} to {}", action.human_label(), crate::keymap::format_chord(chord));
            }
            Err(existing) => {
                self.status = format!(
                    "{} is already bound to {}",
                    crate::keymap::format_chord(chord),
                    existing.human_label()
                );
            }
        }
        Vec::new()
    }

    /// Reset the highlighted binding (Keys tab) to its built-in default.
    fn settings_reset_binding(&mut self) {
        let Some((ctx, action)) = self.settings_current_binding() else {
            return;
        };
        if let Some(st) = self.settings.as_mut() {
            match st.keymap.reset(ctx, action) {
                Ok(()) => self.status = format!("Reset {} to default", action.human_label()),
                Err(existing) => {
                    self.status =
                        format!("Default is taken by {} — unbind it first", existing.human_label());
                }
            }
            self.dirty = true;
        }
    }

    fn settings_switch_tab(&mut self, forward: bool) {
        if let Some(st) = self.settings.as_mut() {
            st.tab = st.tab.stepped(forward);
            st.row = 0;
            st.editing_text = false;
            st.capturing = None;
            self.dirty = true;
        }
    }

    fn settings_move_row(&mut self, delta: i32) {
        if let Some(st) = self.settings.as_mut() {
            // The Keys tab is a list of remappable bindings rather than `Field`s.
            let n = match st.tab {
                SettingsTab::Keys => crate::keymap::editable_entries().len() as i32,
                _ => st.tab.fields().len() as i32,
            };
            st.row = (st.row as i32 + delta).clamp(0, n.max(1) - 1) as usize;
            st.editing_text = false;
            self.dirty = true;
        }
    }

    /// Change the focused field's value with ←/→. Audio fields apply to mpv immediately.
    fn settings_change(&mut self, dir: i32) -> Vec<Cmd> {
        let Some(field) = self.settings.as_ref().map(|s| s.current_field()) else {
            return Vec::new();
        };
        self.dirty = true;
        match field {
            Field::Mouse => {
                let s = self.settings.as_mut().unwrap();
                s.draft.mouse = !s.draft.mouse;
                Vec::new()
            }
            Field::Gapless => {
                let s = self.settings.as_mut().unwrap();
                s.draft.gapless = !s.draft.gapless;
                Vec::new()
            }
            Field::AutoplayRadio => {
                let s = self.settings.as_mut().unwrap();
                s.draft.autoplay_radio = !s.draft.autoplay_radio;
                Vec::new()
            }
            Field::Normalize => {
                let s = self.settings.as_mut().unwrap();
                s.draft.normalize = !s.draft.normalize;
                self.settings_apply_af()
            }
            Field::Speed => {
                let s = self.settings.as_mut().unwrap();
                s.draft.speed = settings::clamp_speed(s.draft.speed + f64::from(dir) * settings::SPEED_STEP);
                self.settings_apply_speed()
            }
            Field::EqPreset => {
                let s = self.settings.as_mut().unwrap();
                // `Custom` isn't in CYCLE; rather than jump to a surprising neighbour,
                // the first ←/→ from a hand-tuned state snaps back to Flat (a clean,
                // known preset), and subsequent presses cycle normally.
                s.draft.eq_preset = if s.draft.eq_preset == EqPreset::Custom {
                    EqPreset::Flat
                } else {
                    let cur = EqPreset::CYCLE.iter().position(|&p| p == s.draft.eq_preset).unwrap_or(0);
                    let n = EqPreset::CYCLE.len();
                    let next = if dir >= 0 { (cur + 1) % n } else { (cur + n - 1) % n };
                    EqPreset::CYCLE[next]
                };
                s.draft.eq_bands = s.draft.eq_preset.gains();
                self.settings_apply_af()
            }
            Field::Band(i) => self.settings_change_band(i, dir),
            Field::GeminiModel => {
                let s = self.settings.as_mut().unwrap();
                s.draft.gemini_model = s.draft.gemini_model.cycled(dir >= 0);
                Vec::new()
            }
            // Text fields ignore ←/→; Enter starts editing instead.
            Field::CookiesFile | Field::DownloadDir | Field::ApiKey => Vec::new(),
        }
    }

    /// Adjust one EQ band. Uses a glitch-free `af-command` when the labeled chain already
    /// exists; otherwise rebuilds the chain (which creates or clears the `@eqN` labels).
    fn settings_change_band(&mut self, i: usize, dir: i32) -> Vec<Cmd> {
        let Some(st) = self.settings.as_mut() else {
            return Vec::new();
        };
        let was_active = st.draft.eq_bands.iter().any(|g| g.abs() > f64::EPSILON);
        let gain = settings::clamp_band(st.draft.eq_bands[i] + f64::from(dir) * settings::BAND_GAIN_STEP);
        st.draft.eq_bands[i] = gain;
        st.draft.eq_preset = EqPreset::Custom;
        let bands = st.draft.eq_bands;
        let normalize = st.draft.normalize;
        let now_active = bands.iter().any(|g| g.abs() > f64::EPSILON);
        if was_active && now_active {
            vec![Cmd::Player(PlayerCmd::AfCommand {
                label: eq::band_label(i),
                param: "gain".to_owned(),
                value: format!("{gain}"),
            })]
        } else {
            vec![Cmd::Player(PlayerCmd::SetAudioFilter(
                eq::build_af_string(&bands, normalize).unwrap_or_default(),
            ))]
        }
    }

    /// Rebuild and apply the EQ/normalization chain from the current draft.
    fn settings_apply_af(&self) -> Vec<Cmd> {
        let Some(st) = self.settings.as_ref() else {
            return Vec::new();
        };
        vec![Cmd::Player(PlayerCmd::SetAudioFilter(
            eq::build_af_string(&st.draft.eq_bands, st.draft.normalize).unwrap_or_default(),
        ))]
    }

    /// Apply the draft's playback speed.
    fn settings_apply_speed(&self) -> Vec<Cmd> {
        let Some(st) = self.settings.as_ref() else {
            return Vec::new();
        };
        vec![Cmd::Player(PlayerCmd::SetProperty {
            name: "speed".to_owned(),
            value: serde_json::Value::from(st.draft.speed),
        })]
    }

    /// Enter (Enter key): start editing a text field, or flip a toggle.
    fn settings_activate(&mut self) -> Vec<Cmd> {
        let Some(field) = self.settings.as_ref().map(|s| s.current_field()) else {
            return Vec::new();
        };
        match field.kind() {
            FieldKind::Text => {
                let st = self.settings.as_mut().unwrap();
                // A secret field (the API key) is masked, so editing in place is blind —
                // appending to the hidden value silently corrupts it. Start fresh: clear
                // the buffer so the user types/pastes a whole new key, but remember the
                // prior value so committing without typing restores it (no accidental wipe).
                if field.is_secret() {
                    st.secret_restore = Self::settings_text_buf(st).map(|buf| {
                        let prev = buf.clone();
                        buf.clear();
                        prev
                    });
                }
                st.editing_text = true;
                self.dirty = true;
                Vec::new()
            }
            FieldKind::Toggle => self.settings_change(1),
            _ => Vec::new(),
        }
    }

    /// Feed one key into the focused text field's buffer. Committing the edit (Enter/Esc)
    /// also persists free-text config fields immediately, so a typed value — notably the
    /// Gemini API key — can never be lost by leaving the screen via Esc/q instead of `s`.
    fn settings_edit_text(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let Some(field) = self.settings.as_ref().map(|s| s.current_field()) else {
            return Vec::new();
        };
        self.dirty = true;
        match k.code {
            KeyCode::Enter | KeyCode::Esc => {
                if let Some(st) = self.settings.as_mut() {
                    st.editing_text = false;
                    // Secret editor opened but left empty (no new key typed): restore the
                    // prior value rather than wiping the saved key.
                    if let Some(prev) = st.secret_restore.take()
                        && let Some(buf) = Self::settings_text_buf(st)
                        && buf.is_empty()
                    {
                        *buf = prev;
                    }
                }
                self.settings_persist_text_field(field)
            }
            KeyCode::Char(c) => {
                if let Some(st) = self.settings.as_mut()
                    && let Some(buf) = Self::settings_text_buf(st)
                {
                    buf.push(c);
                }
                Vec::new()
            }
            KeyCode::Backspace => {
                if let Some(st) = self.settings.as_mut()
                    && let Some(buf) = Self::settings_text_buf(st)
                {
                    buf.pop();
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Persist a free-text config field (cookies path, download dir, API key) to disk the
    /// moment its edit is committed. Other draft fields persist when the settings screen
    /// closes. A changed key also rebuilds the AI actor so it takes effect immediately.
    fn settings_persist_text_field(&mut self, field: Field) -> Vec<Cmd> {
        let value = match self.settings.as_ref().and_then(|s| s.draft.text_value(field)) {
            Some(v) => v.to_owned(),
            None => return Vec::new(),
        };
        let mut cmds = Vec::new();
        match field {
            Field::CookiesFile => {
                self.config.cookies_file = settings::blank_to_none(&value).map(std::path::PathBuf::from);
                self.status = "Settings saved".to_owned();
            }
            Field::DownloadDir => {
                self.config.download_dir = settings::blank_to_none(&value).map(std::path::PathBuf::from);
                self.status = "Settings saved".to_owned();
            }
            Field::ApiKey => {
                let old_key = self.config.gemini_api_key.clone();
                self.config.gemini_api_key = settings::blank_to_none(&value);
                if self.config.gemini_api_key != old_key {
                    cmds.push(Cmd::ReloadAi {
                        key: self.config.effective_gemini_api_key(),
                        model: self.gemini_model,
                    });
                }
                self.status = "API key saved".to_owned();
            }
            // Non-text fields never reach here (only Field::kind()==Text enters edit mode).
            _ => return Vec::new(),
        }
        cmds.push(Cmd::SaveConfig(Box::new(self.config.clone())));
        cmds
    }

    /// The draft string backing the focused text field, if it is a text field.
    fn settings_text_buf(st: &mut SettingsState) -> Option<&mut String> {
        match st.current_field() {
            Field::CookiesFile => Some(&mut st.draft.cookies_file),
            Field::DownloadDir => Some(&mut st.draft.download_dir),
            Field::ApiKey => Some(&mut st.draft.gemini_api_key),
            _ => None,
        }
    }

    /// Leave the settings screen, copying the draft into live state + config and
    /// persisting it. This keeps `q`/Esc from silently discarding changed settings.
    fn close_settings(&mut self) -> Vec<Cmd> {
        let Some(st) = self.settings.take() else {
            self.mode = Mode::Player;
            self.dirty = true;
            return Vec::new();
        };
        self.mode = Mode::Player;
        self.dirty = true;
        let d = &st.draft;
        self.speed = d.speed;
        self.eq_bands = d.eq_bands;
        self.eq_preset = d.eq_preset;
        self.normalize = d.normalize;
        self.autoplay_radio = d.autoplay_radio;
        let model_changed = self.gemini_model != d.gemini_model;
        self.gemini_model = d.gemini_model;
        let old_key = self.config.gemini_api_key.clone();
        d.apply_to(&mut self.config);
        // Commit the edited keybindings (live + persisted as compact overrides).
        self.keymap = st.keymap.clone();
        self.config.keybindings = self.keymap.to_overrides();
        let key_changed = self.config.gemini_api_key != old_key;
        // Volume controls change the live value in place; fold it in so a save
        // doesn't persist the stale startup value.
        self.config.volume = self.volume;
        self.status = "Settings saved".to_owned();
        // Re-assert the committed audio chain before persisting: the draft was
        // previewing live, but a track change mid-edit (EOF auto-advance) would have
        // rebuilt mpv's chain from the *old* committed bands, so push the now-committed
        // chain to guarantee the current track matches what was just saved.
        let mut cmds = vec![
            Cmd::SaveConfig(Box::new(self.config.clone())),
            Cmd::Player(PlayerCmd::SetAudioFilter(self.current_af().unwrap_or_default())),
        ];
        // A changed key rebuilds the AI actor live (the client is otherwise built once
        // at spawn) — so a key entered at runtime takes effect now, no relaunch. The
        // rebuild already adopts the current model, so only hot-swap the model on the
        // running actor when the key itself didn't change.
        if key_changed {
            cmds.push(Cmd::ReloadAi {
                key: self.config.effective_gemini_api_key(),
                model: self.gemini_model,
            });
        } else if model_changed {
            cmds.push(Cmd::SetAiModel(self.gemini_model));
        }
        cmds
    }

    // --- AI assistant -------------------------------------------------------

    /// Enter the AI assistant screen (input focused).
    fn enter_ai(&mut self) {
        self.mode = Mode::Ai;
        self.ai_focus = AiFocus::Input;
        self.status.clear();
        self.dirty = true;
    }

    fn on_key_ai(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match self.ai_focus {
            AiFocus::Input => {
                match self.keymap.action(KeyContext::AiInput, k.into()) {
                    Some(Action::Back) => {
                        self.mode = Mode::Player;
                        self.dirty = true;
                        return Vec::new();
                    }
                    Some(Action::Confirm) => return self.submit_ai_prompt(),
                    Some(Action::DeleteChar) => {
                        self.ai_input.pop();
                        self.dirty = true;
                        return Vec::new();
                    }
                    // Drop into the suggestions list (if any) to pick a track.
                    Some(Action::MoveDown | Action::FocusNext) if !self.ai_suggestions.is_empty() => {
                        self.ai_focus = AiFocus::Suggestions;
                        self.dirty = true;
                        return Vec::new();
                    }
                    _ => {}
                }
                // Every other char feeds the prompt — player keys never leak while typing.
                if let KeyCode::Char(c) = k.code
                    && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    self.ai_input.push(c);
                    self.dirty = true;
                }
                Vec::new()
            }
            AiFocus::Suggestions => match self.keymap.action(KeyContext::AiSuggestions, k.into()) {
                Some(Action::Back) => {
                    self.mode = Mode::Player;
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveUp) => {
                    if self.ai_suggestions_selected == 0 {
                        self.ai_focus = AiFocus::Input;
                    } else {
                        self.ai_suggestions_selected -= 1;
                    }
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveDown) => {
                    if self.ai_suggestions_selected + 1 < self.ai_suggestions.len() {
                        self.ai_suggestions_selected += 1;
                    }
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::FocusNext) => {
                    self.ai_focus = AiFocus::Input;
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::Confirm) => self.play_ai_suggestion(),
                _ => Vec::new(),
            },
        }
    }

    /// Submit the typed prompt to the assistant (or show onboarding if no key).
    fn submit_ai_prompt(&mut self) -> Vec<Cmd> {
        let prompt = self.ai_input.trim().to_owned();
        if prompt.is_empty() {
            return Vec::new();
        }
        self.ai_input.clear();
        self.push_ai_message(AiRole::User, prompt.clone());
        self.dirty = true;
        if !self.ai_available {
            self.push_ai_message(
                AiRole::Error,
                // Saving a key in Settings now brings the assistant up live (no restart).
                "No Gemini API key. Add one in Settings (press ,) or set GEMINI_API_KEY.".to_owned(),
            );
            return Vec::new();
        }
        // Ignore a new prompt while one is in flight (the spinner is showing).
        if self.ai_thinking {
            return Vec::new();
        }
        self.ai_thinking = true;
        vec![Cmd::AskAi { prompt, context: Box::new(self.build_ai_context()) }]
    }

    /// Play the highlighted suggestion, queuing the whole list from that point.
    fn play_ai_suggestion(&mut self) -> Vec<Cmd> {
        if self.ai_suggestions.is_empty() {
            return Vec::new();
        }
        let start = self.ai_suggestions_selected.min(self.ai_suggestions.len() - 1);
        self.queue.set(self.ai_suggestions.clone(), start);
        self.status.clear();
        let song = self.queue.current().cloned();
        self.load_song(song)
    }

    /// Append a line to the AI transcript, bounding its length.
    fn push_ai_message(&mut self, role: AiRole, text: String) {
        self.ai_messages.push(AiMessage { role, text });
        if self.ai_messages.len() > AI_HISTORY_MAX {
            let overflow = self.ai_messages.len() - AI_HISTORY_MAX;
            self.ai_messages.drain(0..overflow);
        }
    }

    /// Snapshot the read-only state the AI actor needs to answer its read tools.
    fn build_ai_context(&self) -> AiContext {
        let fmt = |s: &Song| format!("{} — {}", s.title, s.artist);
        AiContext {
            current_track: self.queue.current().map(fmt),
            queue_upcoming: self.queue.upcoming(10).into_iter().map(fmt).collect(),
            queue_len: self.queue.len(),
            queue_remaining: self.queue.remaining(),
            recent_history: self.library.history.iter().take(5).map(fmt).collect(),
            favorites: self.library.favorites.iter().take(20).map(fmt).collect(),
            playlists: self
                .playlists
                .list()
                .iter()
                .map(|p| PlaylistInfo { id: p.id.clone(), name: p.name.clone(), count: p.songs.len() })
                .collect(),
            authenticated: self.authenticated,
            autoplay_radio: self.autoplay_radio,
        }
    }

    /// If autoplay/radio is on and the queue is running low, ask the assistant to top it
    /// up — rate-limited by a cooldown and guarded against overlapping requests.
    fn maybe_autoplay_extend(&mut self) -> Vec<Cmd> {
        if !self.autoplay_radio || !self.ai_available || self.ai_thinking {
            return Vec::new();
        }
        if self.queue.remaining() > AUTOPLAY_THRESHOLD {
            return Vec::new();
        }
        let cooled = match self.ai_last_extend {
            Some(t) => t.elapsed() >= AUTOPLAY_COOLDOWN,
            None => true,
        };
        if !cooled {
            return Vec::new();
        }
        let Some(cur) = self.queue.current() else {
            return Vec::new();
        };
        let seed = format!("{} — {}", cur.title, cur.artist);
        self.ai_last_extend = Some(Instant::now());
        self.ai_thinking = true;
        self.dirty = true;
        let prompt = format!(
            "The play queue is running low. Using the add_to_queue tool, add 5 to 8 tracks similar to \"{seed}\" to keep the music going. Reply with no text."
        );
        vec![Cmd::AskAi { prompt, context: Box::new(self.build_ai_context()) }]
    }

    /// Number of rows in the currently selected library tab.
    fn library_len(&self) -> usize {
        match self.library_tab {
            LibraryTab::Favorites => self.library.favorites.len(),
            LibraryTab::History => self.library.history.len(),
        }
    }

    /// The track under the library cursor, if any.
    fn selected_library_song(&self) -> Option<Song> {
        match self.library_tab {
            LibraryTab::Favorites => self.library.favorites.get(self.library_selected).cloned(),
            LibraryTab::History => self.library.history.get(self.library_selected).cloned(),
        }
    }

    /// Queue the current library tab (starting at the cursor) and start playing.
    fn play_from_library(&mut self) -> Vec<Cmd> {
        let songs: Vec<Song> = match self.library_tab {
            LibraryTab::Favorites => self.library.favorites.clone(),
            LibraryTab::History => self.library.history.iter().cloned().collect(),
        };
        if songs.is_empty() {
            return Vec::new();
        }
        self.queue.set(songs, self.library_selected);
        self.mode = Mode::Player;
        self.status.clear();
        let song = self.queue.current().cloned();
        self.load_song(song)
    }

    /// Make the whole results list the queue, starting at the selected track, and play.
    fn play_selected(&mut self) -> Vec<Cmd> {
        if self.search_results.is_empty() {
            return Vec::new();
        }
        self.queue.set(self.search_results.clone(), self.search_selected);
        self.mode = Mode::Player;
        self.status.clear();
        let song = self.queue.current().cloned();
        self.load_song(song)
    }

    /// Move to the next queue track (auto = end-of-track) and load it, or stop. Also runs
    /// the autoplay/radio top-up check now that the queue has advanced.
    fn advance(&mut self, auto: bool) -> Vec<Cmd> {
        let song = self.queue.next(auto).cloned();
        let mut cmds = self.load_song(song);
        cmds.extend(self.maybe_autoplay_extend());
        cmds
    }

    /// Given an optional track, record it in history, reset progress, and emit a load
    /// command (or nothing when the queue produced no track). Always marks the UI dirty.
    fn load_song(&mut self, song: Option<Song>) -> Vec<Cmd> {
        self.dirty = true;
        match song {
            Some(song) => {
                self.reset_progress();
                // A new track is a clean slate: drop any stale status (e.g. a prior
                // "Playback error" / "Track unavailable") so the UI matches what's loading.
                self.status.clear();
                self.library.record_play(&song);
                // Drop the previous track's lyrics; refresh if the panel is open.
                self.lyrics = None;
                // Use a prefetched direct URL if we have one (instant skip); else hand mpv
                // the watch URL and let it resolve.
                let prefetched = self.resolved.contains_key(&song.video_id);
                self.last_load_prefetched = prefetched;
                let url = self.resolved.get(&song.video_id).cloned().unwrap_or_else(|| song.watch_url());
                tracing::info!(url = %url, prefetched, "load track");
                let mut cmds = vec![Cmd::Player(PlayerCmd::Load(url)), Cmd::SaveLibrary];
                // Re-apply the EQ/normalization chain: a gapless graph rebuild on track
                // change can drop the labeled `@eqN` filters, so push it after every load.
                // While the settings screen is open the *draft* is the source of truth (it's
                // been previewing live), so a track change mid-edit keeps mpv matching what
                // the user sees — and leaves the labels in place for the next `af-command`.
                let af = match self.settings.as_deref() {
                    Some(st) => eq::build_af_string(&st.draft.eq_bands, st.draft.normalize),
                    None => self.current_af(),
                };
                if let Some(af) = af {
                    cmds.push(Cmd::Player(PlayerCmd::SetAudioFilter(af)));
                }
                if self.lyrics_visible {
                    self.lyrics_loading = true;
                    cmds.push(fetch_lyrics_cmd(&song));
                }
                // Prefetch the upcoming track's stream so the next skip is instant.
                if let Some(next) = self.queue.peek_next() {
                    let video_id = next.video_id.clone();
                    let watch_url = next.watch_url();
                    if !self.resolved.contains_key(&video_id) {
                        cmds.push(Cmd::Resolve { video_id, watch_url });
                    }
                }
                cmds
            }
            None => Vec::new(),
        }
    }

    /// Mark a download as starting and emit the effect to run it.
    fn start_download(&mut self, song: Song) -> Vec<Cmd> {
        self.downloads.insert(song.video_id.clone(), DownloadState::Running(0));
        self.status = format!("Downloading: {} — {}", song.title, song.artist);
        self.dirty = true;
        vec![Cmd::Download(song)]
    }

    /// Whether we lack lyrics for the current track (so a fetch is warranted).
    fn lyrics_stale(&self) -> bool {
        match (&self.lyrics, self.queue.current()) {
            (Some(l), Some(cur)) => l.video_id != cur.video_id,
            (None, Some(_)) => true,
            _ => false,
        }
    }

    /// Clear per-track playback state before loading a new track.
    fn reset_progress(&mut self) {
        self.time_pos = None;
        self.duration = None;
        self.paused = false;
        self.last_shown_sec = -1;
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

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// The `af` chain set by a `SetAudioFilter` command among `cmds`, if any.
    fn af(cmds: &[Cmd]) -> Option<&str> {
        cmds.iter().find_map(|c| match c {
            Cmd::Player(PlayerCmd::SetAudioFilter(s)) => Some(s.as_str()),
            _ => None,
        })
    }

    /// The URL of the `Load` command among `cmds`, if any. (A load now also emits
    /// `SaveLibrary`, so tests look for the Load rather than an exact one-element match.)
    fn load_url(cmds: &[Cmd]) -> Option<&str> {
        cmds.iter().find_map(|c| match c {
            Cmd::Player(PlayerCmd::Load(u)) => Some(u.as_str()),
            _ => None,
        })
    }

    #[test]
    fn q_quits_in_player_mode() {
        let mut app = App::new(100);
        app.update(Msg::Key(key(KeyCode::Char('q'))));
        assert!(app.should_quit);
    }

    #[test]
    fn space_toggles_pause_and_emits_cmd() {
        let mut app = App::new(100);
        let cmds = app.update(Msg::Key(key(KeyCode::Char(' '))));
        assert!(app.paused);
        assert!(matches!(cmds.as_slice(), [Cmd::Player(PlayerCmd::CyclePause)]));
    }

    #[test]
    fn up_down_adjust_volume_in_player_mode() {
        let mut app = App::new(50);
        let cmds = app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(app.volume, 55);
        assert!(matches!(cmds.as_slice(), [Cmd::Player(PlayerCmd::SetVolume(55))]));

        let cmds = app.update(Msg::Key(key(KeyCode::Down)));
        assert_eq!(app.volume, 50);
        assert!(matches!(cmds.as_slice(), [Cmd::Player(PlayerCmd::SetVolume(50))]));
    }

    #[test]
    fn time_pos_redraws_only_on_second_change() {
        let mut app = App::new(100);
        app.dirty = false;
        app.update(Msg::PlayerTimePos(1.1));
        assert!(app.dirty);
        app.dirty = false;
        app.update(Msg::PlayerTimePos(1.9)); // same whole second
        assert!(!app.dirty);
        app.update(Msg::PlayerTimePos(2.0)); // new second
        assert!(app.dirty);
    }

    #[test]
    fn slash_enters_search_and_q_is_typed_not_quit() {
        let mut app = App::new(100);
        app.update(Msg::Key(key(KeyCode::Char('/'))));
        assert_eq!(app.mode, Mode::Search);
        app.update(Msg::Key(key(KeyCode::Char('q'))));
        assert!(!app.should_quit);
        assert_eq!(app.search_input, "q");
    }

    #[test]
    fn enter_in_search_emits_search_cmd() {
        let mut app = App::new(100);
        app.update(Msg::Key(key(KeyCode::Char('/'))));
        for c in "lofi".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.searching);
        match cmds.as_slice() {
            [Cmd::Search(q)] => assert_eq!(q, "lofi"),
            _ => panic!("expected a Search cmd"),
        }
    }

    #[test]
    fn results_then_enter_plays_and_returns_to_player() {
        let mut app = App::new(100);
        app.mode = Mode::Search;
        app.update(Msg::SearchResults {
            query: "x".to_owned(),
            songs: vec![Song {
                video_id: "abc123".to_owned(),
                title: "Song".to_owned(),
                artist: "Artist".to_owned(),
                duration: "3:00".to_owned(),
            }],
        });
        assert_eq!(app.search_focus, SearchFocus::Results);
        let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.mode, Mode::Player);
        assert!(load_url(&cmds).expect("a Load cmd").contains("abc123"));
    }

    #[test]
    fn ctrl_q_quits_search_results_without_quitting_app() {
        let mut app = App::new(100);
        app.mode = Mode::Search;
        app.search_focus = SearchFocus::Results;
        app.search_results = songs(1);
        app.update(Msg::Key(ctrl(KeyCode::Char('q'))));
        assert_eq!(app.mode, Mode::Player);
        assert!(!app.should_quit);
    }

    // --- M4: queue / shuffle / repeat / auto-advance ------------------------

    fn songs(n: usize) -> Vec<Song> {
        (0..n)
            .map(|i| Song {
                video_id: format!("id{i}"),
                title: format!("t{i}"),
                artist: "a".to_owned(),
                duration: "0:10".to_owned(),
            })
            .collect()
    }

    /// An app whose queue is the search results, playing track `start`.
    fn app_playing(n: usize, start: usize) -> App {
        let mut app = App::new(100);
        app.search_results = songs(n);
        app.search_selected = start;
        app.search_focus = SearchFocus::Results;
        app.mode = Mode::Search;
        app.update(Msg::Key(key(KeyCode::Enter)));
        app
    }

    fn current(app: &App) -> &str {
        app.queue.current().unwrap().video_id.as_str()
    }

    #[test]
    fn enter_queues_whole_results_list() {
        let app = app_playing(5, 2);
        assert_eq!(app.queue.len(), 5);
        assert_eq!(current(&app), "id2");
        assert_eq!(app.mode, Mode::Player);
    }

    #[test]
    fn eof_auto_advances_to_next_track() {
        let mut app = app_playing(3, 0);
        let cmds = app.update(Msg::PlayerEof);
        assert!(load_url(&cmds).expect("load of next track").contains("id1"));
        assert_eq!(current(&app), "id1");
    }

    #[test]
    fn eof_at_end_with_repeat_off_stops() {
        let mut app = app_playing(2, 1); // already on the last track
        let cmds = app.update(Msg::PlayerEof);
        assert!(cmds.is_empty());
        assert_eq!(current(&app), "id1");
    }

    #[test]
    fn eof_with_repeat_one_replays_same_track() {
        let mut app = app_playing(3, 0);
        app.queue.repeat = crate::queue::Repeat::One;
        let cmds = app.update(Msg::PlayerEof);
        assert!(load_url(&cmds).expect("replay of same track").contains("id0"));
        assert_eq!(current(&app), "id0");
    }

    #[test]
    fn player_error_auto_skips_to_next_track() {
        let mut app = app_playing(3, 0);
        let cmds = app.update(Msg::PlayerError("boom".to_owned()));
        // The unplayable track is skipped: cursor + title move to the next track.
        assert!(load_url(&cmds).expect("load of next track").contains("id1"));
        assert_eq!(current(&app), "id1");
        assert!(app.status.contains("skipped") || app.status.contains("unavailable"));
    }

    #[test]
    fn player_error_stops_after_repeated_failures() {
        let mut app = app_playing(6, 0);
        // First MAX failures auto-skip...
        for _ in 0..MAX_CONSECUTIVE_PLAY_ERRORS {
            let cmds = app.update(Msg::PlayerError("boom".to_owned()));
            assert!(load_url(&cmds).is_some(), "still skipping within budget");
        }
        // ...the next one gives up instead of skip-storming the whole queue.
        let cmds = app.update(Msg::PlayerError("boom".to_owned()));
        assert!(load_url(&cmds).is_none(), "stops skipping after the budget");
        assert!(app.status.contains("stopped") || app.status.contains("failed"));
    }

    #[test]
    fn successful_playback_resets_the_error_streak() {
        let mut app = app_playing(5, 0);
        app.update(Msg::PlayerError("boom".to_owned())); // skip to id1 (streak = 1)
        assert_eq!(current(&app), "id1");
        app.update(Msg::PlayerTimePos(3.0)); // id1 actually plays → streak cleared
        // A later failure starts a fresh streak, so it skips again rather than giving up.
        let cmds = app.update(Msg::PlayerError("boom".to_owned()));
        assert!(load_url(&cmds).expect("skips again after a clean play").contains("id2"));
        assert_eq!(current(&app), "id2");
    }

    #[test]
    fn n_advances_and_p_goes_back() {
        let mut app = app_playing(3, 0);
        app.update(Msg::Key(key(KeyCode::Char('n'))));
        assert_eq!(current(&app), "id1");
        app.update(Msg::Key(key(KeyCode::Char('p'))));
        assert_eq!(current(&app), "id0");
    }

    #[test]
    fn r_cycles_repeat_and_s_toggles_shuffle() {
        let mut app = app_playing(3, 0);
        assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
        app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert_eq!(app.queue.repeat, crate::queue::Repeat::All);
        assert!(!app.queue.shuffle);
        app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert!(app.queue.shuffle);
        // Shuffle keeps the current track current.
        assert_eq!(current(&app), "id0");
    }

    // --- B+C: EQ / normalize / speed / autoplay -----------------------------

    #[test]
    fn e_cycles_eq_preset_and_emits_filter() {
        let mut app = app_playing(3, 0);
        assert_eq!(app.eq_preset, EqPreset::Flat);
        let cmds = app.update(Msg::Key(key(KeyCode::Char('e'))));
        assert_eq!(app.eq_preset, EqPreset::BassBoost);
        assert!(af(&cmds).expect("a SetAudioFilter cmd").contains("equalizer"));
        // Cycle the rest of the way back to Flat → the chain is cleared (empty string).
        let mut last = Vec::new();
        for _ in 0..(EqPreset::CYCLE.len() - 1) {
            last = app.update(Msg::Key(key(KeyCode::Char('e'))));
        }
        assert_eq!(app.eq_preset, EqPreset::Flat);
        assert_eq!(af(&last), Some(""));
    }

    #[test]
    fn shift_n_toggles_normalization() {
        let mut app = app_playing(3, 0);
        let cmds = app.update(Msg::Key(key(KeyCode::Char('N'))));
        assert!(app.normalize);
        assert!(af(&cmds).expect("a SetAudioFilter cmd").contains("dynaudnorm"));
        let cmds = app.update(Msg::Key(key(KeyCode::Char('N'))));
        assert!(!app.normalize);
        assert_eq!(af(&cmds), Some(""));
    }

    #[test]
    fn speed_up_and_down_clamp_and_emit() {
        let mut app = app_playing(3, 0);
        let cmds = app.update(Msg::Key(key(KeyCode::Char('>'))));
        assert!((app.speed - 1.1).abs() < 1e-9);
        assert!(cmds.iter().any(|c| matches!(c,
            Cmd::Player(PlayerCmd::SetProperty { name, .. }) if name == "speed")));
        // Floor at SPEED_MIN no matter how many times we press down.
        for _ in 0..50 {
            app.update(Msg::Key(key(KeyCode::Char('<'))));
        }
        assert!((app.speed - SPEED_MIN).abs() < 1e-9);
    }

    #[test]
    fn ctrl_r_toggles_autoplay_radio() {
        let mut app = app_playing(3, 0);
        assert!(!app.autoplay_radio);
        app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
        assert!(app.autoplay_radio);
        // Plain `r` still cycles repeat (not the autoplay toggle).
        app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(app.autoplay_radio);
        assert_eq!(app.queue.repeat, crate::queue::Repeat::All);
        app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
        assert!(!app.autoplay_radio);
    }

    #[test]
    fn load_song_reapplies_active_eq_chain() {
        let mut app = app_playing(3, 0);
        app.eq_bands = EqPreset::BassBoost.gains();
        // A manual skip reloads the track and must re-send the EQ chain (gapless rebuild
        // can otherwise drop the labeled bands).
        let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
        assert!(af(&cmds).expect("a SetAudioFilter cmd").contains("equalizer"));
    }

    #[test]
    fn apply_config_pushes_playback_settings() {
        let cfg = crate::config::Config {
            eq_preset: EqPreset::Vocal,
            normalize: Some(true),
            speed: Some(1.5),
            autoplay_radio: Some(true),
            ..crate::config::Config::default()
        };
        let mut app = App::new(100);
        app.apply_config(&cfg);
        assert_eq!(app.eq_preset, EqPreset::Vocal);
        assert_eq!(app.eq_bands, EqPreset::Vocal.gains());
        assert!(app.normalize);
        assert!((app.speed - 1.5).abs() < 1e-9);
        assert!(app.autoplay_radio);
    }

    // --- D: settings screen -------------------------------------------------

    fn save_config(cmds: &[Cmd]) -> Option<&Config> {
        cmds.iter().find_map(|c| match c {
            Cmd::SaveConfig(c) => Some(c.as_ref()),
            _ => None,
        })
    }

    #[test]
    fn comma_opens_settings_and_q_closes_without_quitting() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(','))));
        assert_eq!(app.mode, Mode::Settings);
        assert!(app.settings.is_some());
        app.update(Msg::Key(key(KeyCode::Char('q'))));
        assert_eq!(app.mode, Mode::Player);
        assert!(!app.should_quit);
        assert!(app.settings.is_none());
    }

    #[test]
    fn settings_tab_cycles_through_all_tabs() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Playback);
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Eq);
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Ai);
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General); // wraps
    }

    #[test]
    fn settings_key_capture_accepts_ctrl_chords() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open settings
        for _ in 0..4 {
            app.update(Msg::Key(key(KeyCode::Tab)));
        }
        assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::Keys);
        app.update(Msg::Key(key(KeyCode::Enter))); // capture first binding: player.toggle_pause
        assert_eq!(
            app.settings.as_ref().unwrap().capturing,
            Some((KeyContext::Player, Action::TogglePause))
        );
        app.update(Msg::Key(ctrl(KeyCode::Char('X'))));
        assert_eq!(
            app.settings
                .as_ref()
                .unwrap()
                .keymap
                .action(KeyContext::Player, crate::keymap::parse_chord("ctrl+x").unwrap()),
            Some(Action::TogglePause)
        );
        assert!(app.status.contains("^X"));

        let cmds = app.update(Msg::Key(key(KeyCode::Char('s'))));
        let saved = save_config(&cmds).expect("a SaveConfig cmd");
        assert_eq!(saved.keybindings.get("player.toggle_pause").map(String::as_str), Some("ctrl+x"));
    }

    #[test]
    fn settings_save_applies_and_persists() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open (General)
        app.update(Msg::Key(key(KeyCode::Tab))); // Playback tab; row 0 = Speed
        app.update(Msg::Key(key(KeyCode::Right))); // speed 1.0 -> 1.1 (draft)
        assert!((app.speed - 1.0).abs() < 1e-9, "committed speed unchanged while editing");
        let cmds = app.update(Msg::Key(key(KeyCode::Char('s')))); // save
        assert_eq!(app.mode, Mode::Player);
        assert!((app.speed - 1.1).abs() < 1e-9, "speed applied on save");
        let saved = save_config(&cmds).expect("a SaveConfig cmd");
        assert_eq!(saved.speed, Some(1.1));
    }

    #[test]
    fn settings_close_persists_live_audio() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open
        app.update(Msg::Key(key(KeyCode::Tab))); // Playback; Speed
        app.update(Msg::Key(key(KeyCode::Right))); // draft speed -> 1.1
        let cmds = app.update(Msg::Key(key(KeyCode::Esc))); // save+close
        assert_eq!(app.mode, Mode::Player);
        assert!((app.speed - 1.1).abs() < 1e-9, "speed persisted on close");
        assert_eq!(save_config(&cmds).expect("a SaveConfig cmd").speed, Some(1.1));
        // Closing re-asserts the committed filter chain so the running track matches the
        // now-persisted settings.
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Player(PlayerCmd::SetAudioFilter(_)))));
    }

    #[test]
    fn settings_band_edit_sets_custom_and_emits_filter() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab))); // EQ tab; row 0 = preset
        app.update(Msg::Key(key(KeyCode::Down))); // row 1 = first band (31 Hz)
        let cmds = app.update(Msg::Key(key(KeyCode::Right))); // raise the band
        let st = app.settings.as_ref().unwrap();
        assert_eq!(st.draft.eq_preset, EqPreset::Custom);
        assert!(st.draft.eq_bands[0] > 0.0);
        // First non-zero band → full rebuild (creates the labels).
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Player(PlayerCmd::SetAudioFilter(s)) if s.contains("equalizer"))));
        // A second nudge with labels present uses the glitch-free af-command path.
        let cmds = app.update(Msg::Key(key(KeyCode::Right)));
        assert!(cmds.iter().any(|c| matches!(c,
            Cmd::Player(PlayerCmd::AfCommand { label, param, .. }) if label == "eq0" && param == "gain")));
    }

    #[test]
    fn settings_save_reasserts_audio_and_persists_volume() {
        let mut app = app_playing(1, 0);
        app.volume = 55; // a `=`/`-` change during the session
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab))); // EQ tab; row 0 = preset
        app.update(Msg::Key(key(KeyCode::Down))); // first band
        app.update(Msg::Key(key(KeyCode::Right))); // raise it (draft = Custom)
        let cmds = app.update(Msg::Key(key(KeyCode::Char('s')))); // save
        // Save re-asserts the committed chain so the current track matches what was saved
        // even if an EOF rebuilt mpv from the old bands mid-edit.
        assert!(cmds.iter().any(|c| matches!(c,
            Cmd::Player(PlayerCmd::SetAudioFilter(s)) if s.contains("equalizer"))));
        // The session volume is folded into the persisted config (not the startup value).
        assert_eq!(save_config(&cmds).expect("a SaveConfig cmd").volume, 55);
    }

    #[test]
    fn settings_preset_selector_snaps_from_custom_to_flat() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab))); // EQ tab; row 0 = preset
        app.update(Msg::Key(key(KeyCode::Down))); // first band
        app.update(Msg::Key(key(KeyCode::Right))); // hand-tune → Custom
        assert_eq!(app.settings.as_ref().unwrap().draft.eq_preset, EqPreset::Custom);
        app.update(Msg::Key(key(KeyCode::Up))); // back to the preset row
        // From Custom, the first ←/→ snaps to Flat rather than jumping to a neighbour.
        app.update(Msg::Key(key(KeyCode::Right)));
        assert_eq!(app.settings.as_ref().unwrap().draft.eq_preset, EqPreset::Flat);
        // Then it cycles normally.
        app.update(Msg::Key(key(KeyCode::Right)));
        assert_eq!(app.settings.as_ref().unwrap().draft.eq_preset, EqPreset::BassBoost);
    }

    #[test]
    fn settings_text_field_edits_path_buffer() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open (General); row 0 = cookies file
        app.update(Msg::Key(key(KeyCode::Enter))); // enter text-edit mode
        assert!(app.settings.as_ref().unwrap().editing_text);
        for c in "/x.txt".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        // `q` is typed, not treated as close, while editing.
        assert_eq!(app.mode, Mode::Settings);
        app.update(Msg::Key(key(KeyCode::Enter))); // commit edit mode
        assert!(!app.settings.as_ref().unwrap().editing_text);
        let cmds = app.update(Msg::Key(key(KeyCode::Char('s')))); // save
        assert_eq!(save_config(&cmds).unwrap().cookies_file, Some(std::path::PathBuf::from("/x.txt")));
    }

    #[test]
    fn settings_ai_tab_switches_model_live_and_persists() {
        let mut app = app_playing(1, 0);
        let start = app.gemini_model;
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open (General)
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab))); // AI tab; row 0 = model
        app.update(Msg::Key(key(KeyCode::Right))); // cycle model (draft only)
        let drafted = app.settings.as_ref().unwrap().draft.gemini_model;
        assert_ne!(drafted, start, "← /→ cycles the model in the draft");
        assert_eq!(app.gemini_model, start, "committed model unchanged while editing");
        let cmds = app.update(Msg::Key(key(KeyCode::Char('s')))); // save
        assert_eq!(app.gemini_model, drafted, "model committed on save");
        // The running actor is told to hot-swap; config persists the choice.
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SetAiModel(m) if *m == drafted)));
        assert_eq!(save_config(&cmds).unwrap().gemini_model, drafted);
    }

    #[test]
    fn settings_ai_tab_edits_masked_api_key() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open
        for _ in 0..3 {
            app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab
        }
        app.update(Msg::Key(key(KeyCode::Down))); // model -> API key row
        app.update(Msg::Key(key(KeyCode::Enter))); // start editing the key
        assert!(app.settings.as_ref().unwrap().editing_text);
        for c in "AIzaKey".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        // Committing the edit (Enter) persists the key immediately — it must NOT depend on
        // the user also pressing `s`, which is the trap that lost keys before.
        let cmds = app.update(Msg::Key(key(KeyCode::Enter))); // commit edit
        assert_eq!(save_config(&cmds).unwrap().gemini_api_key.as_deref(), Some("AIzaKey"));
        // A new key rebuilds the assistant live (no relaunch), not just persists it.
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ReloadAi { key: Some(k), .. } if k == "AIzaKey")),
            "committing a changed key must reload the AI actor"
        );
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::SetAiModel(_))));
        // The committed value is now in config, so a later `s`-save doesn't double-reload.
        let save_cmds = app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(save_config(&save_cmds).unwrap().gemini_api_key.as_deref(), Some("AIzaKey"));
        assert!(
            !save_cmds.iter().any(|c| matches!(c, Cmd::ReloadAi { .. })),
            "an unchanged key shouldn't rebuild the actor again on save"
        );
    }

    #[test]
    fn api_key_persists_when_leaving_settings_via_close() {
        // The reported bug: type a key, then leave with Esc/q (the intuitive move) — the
        // key must survive.
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open
        for _ in 0..3 {
            app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab
        }
        app.update(Msg::Key(key(KeyCode::Down))); // model -> API key row
        app.update(Msg::Key(key(KeyCode::Enter))); // start editing
        for c in "AIzaPersist".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        // Esc commits the field (and persists it) rather than discarding the typed key.
        let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(save_config(&cmds).unwrap().gemini_api_key.as_deref(), Some("AIzaPersist"));
        // Esc again leaves the screen; config already holds the key.
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.config.gemini_api_key.as_deref(), Some("AIzaPersist"));
    }

    #[test]
    fn opening_then_leaving_key_editor_empty_keeps_existing_key() {
        // Entering the masked editor clears the buffer; backing out without typing must
        // restore the saved key, not wipe it.
        let mut app = app_playing(1, 0);
        app.config.gemini_api_key = Some("KEEPME".to_owned());
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open (draft seeds from config)
        for _ in 0..3 {
            app.update(Msg::Key(key(KeyCode::Tab)));
        }
        app.update(Msg::Key(key(KeyCode::Down))); // → API key row
        app.update(Msg::Key(key(KeyCode::Enter))); // start editing -> buffer cleared
        let cmds = app.update(Msg::Key(key(KeyCode::Esc))); // leave editor without typing
        assert_eq!(
            save_config(&cmds).unwrap().gemini_api_key.as_deref(),
            Some("KEEPME"),
            "an untouched secret edit must not wipe the saved key"
        );
    }

    #[test]
    fn editing_existing_api_key_starts_fresh_not_appended() {
        let mut app = app_playing(1, 0);
        app.config.gemini_api_key = Some("OLDKEY".to_owned());
        app.update(Msg::Key(key(KeyCode::Char(',')))); // open (draft seeds from config)
        for _ in 0..3 {
            app.update(Msg::Key(key(KeyCode::Tab))); // → AI tab
        }
        app.update(Msg::Key(key(KeyCode::Down))); // model -> API key row
        app.update(Msg::Key(key(KeyCode::Enter))); // start editing -> masked buffer cleared
        assert_eq!(
            app.settings.as_ref().unwrap().draft.gemini_api_key, "",
            "editing a secret field clears it rather than appending blindly"
        );
        for c in "NEWKEY".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        app.update(Msg::Key(key(KeyCode::Enter))); // commit
        let cmds = app.update(Msg::Key(key(KeyCode::Char('s')))); // save
        // Replaces, not "OLDKEYNEWKEY".
        assert_eq!(save_config(&cmds).unwrap().gemini_api_key.as_deref(), Some("NEWKEY"));
    }

    // --- A: AI assistant ----------------------------------------------------

    /// The prompt of the `AskAi` command among `cmds`, if any.
    fn ask_ai(cmds: &[Cmd]) -> Option<&str> {
        cmds.iter().find_map(|c| match c {
            Cmd::AskAi { prompt, .. } => Some(prompt.as_str()),
            _ => None,
        })
    }

    #[test]
    fn a_enters_ai_from_player_and_library() {
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        assert_eq!(app.mode, Mode::Ai);
        assert_eq!(app.ai_focus, AiFocus::Input);
        // And from the library view.
        let mut app = app_playing(1, 0);
        app.update(Msg::Key(key(KeyCode::Char('l'))));
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        assert_eq!(app.mode, Mode::Ai);
    }

    #[test]
    fn ai_submit_without_key_shows_onboarding_error() {
        let mut app = app_playing(1, 0); // ai_available defaults to false
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        for c in "play jazz".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(ask_ai(&cmds).is_none(), "no AskAi without a key");
        assert!(!app.ai_thinking);
        // Transcript holds the user prompt plus an error line.
        assert_eq!(app.ai_messages.last().unwrap().role, AiRole::Error);
        assert!(app.ai_messages.iter().any(|m| m.role == AiRole::User && m.text == "play jazz"));
    }

    #[test]
    fn ai_submit_with_key_emits_ask_and_sets_thinking() {
        let mut app = app_playing(1, 0);
        app.ai_available = true;
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        for c in "play lofi".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(ask_ai(&cmds), Some("play lofi"));
        assert!(app.ai_thinking);
        assert!(app.ai_input.is_empty());
        // A second submit while thinking is ignored (no duplicate request).
        for c in "more".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(ask_ai(&cmds).is_none());
    }

    #[test]
    fn ai_play_tracks_on_empty_queue_starts_playback() {
        let mut app = App::new(100);
        assert!(app.queue.is_empty());
        let cmds = app.update(Msg::AiPlayTracks(songs(3)));
        assert_eq!(current(&app), "id0");
        assert!(load_url(&cmds).expect("a Load cmd").contains("id0"));
    }

    #[test]
    fn ai_enqueue_reports_count_and_extends() {
        let mut app = app_playing(2, 0); // queue has id0, id1
        app.update(Msg::AiEnqueue(songs(3)));
        assert_eq!(app.queue.len(), 5);
        assert!(app.status.contains("Queued"));
    }

    #[test]
    fn ai_error_clears_thinking() {
        let mut app = app_playing(1, 0);
        app.ai_thinking = true;
        app.update(Msg::AiError("boom".to_owned()));
        assert!(!app.ai_thinking);
        assert_eq!(app.ai_messages.last().unwrap().role, AiRole::Error);
    }

    #[test]
    fn ai_empty_chat_is_not_appended() {
        let mut app = app_playing(1, 0);
        app.update(Msg::AiChat("   ".to_owned()));
        assert!(app.ai_messages.is_empty());
        app.update(Msg::AiChat("here you go".to_owned()));
        assert_eq!(app.ai_messages.len(), 1);
    }

    #[test]
    fn ai_radio_circuit_breaker_disables_after_repeated_empties() {
        let mut app = app_playing(1, 0);
        app.autoplay_radio = true;
        for _ in 0..AUTOPLAY_MAX_FAILURES {
            app.update(Msg::AiEnqueue(Vec::new())); // resolves nothing
        }
        assert!(!app.autoplay_radio, "radio disabled after repeated empty extends");
        assert!(app.status.contains("Autoplay radio stopped"));
    }

    #[test]
    fn autoplay_extends_when_queue_runs_low() {
        let mut app = app_playing(2, 0); // remaining = 1 (<= threshold)
        app.ai_available = true;
        app.autoplay_radio = true;
        // A manual next advances and should trigger a top-up AskAi.
        let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
        assert!(ask_ai(&cmds).is_some(), "autoplay should ask for more tracks");
        assert!(app.ai_thinking);
        // The cooldown blocks an immediate second request.
        let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
        assert!(ask_ai(&cmds).is_none());
    }

    #[test]
    fn ai_create_and_play_playlist_roundtrip() {
        let mut app = App::new(100);
        let cmds = app.update(Msg::AiCreatePlaylist("Focus".to_owned()));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SavePlaylists)));
        app.update(Msg::AiAddToPlaylist { playlist: "Focus".to_owned(), songs: songs(2) });
        assert_eq!(app.playlists.find("Focus").unwrap().songs.len(), 2);
        let cmds = app.update(Msg::AiPlayPlaylist("Focus".to_owned()));
        assert_eq!(current(&app), "id0");
        assert!(load_url(&cmds).is_some());
    }

    // --- M5: library (favorites + history) ----------------------------------

    #[test]
    fn f_toggles_favorite_of_current_track() {
        let mut app = app_playing(3, 0); // playing "id0"
        assert!(!app.library.is_favorite("id0"));
        let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert!(app.library.is_favorite("id0"));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
        app.update(Msg::Key(key(KeyCode::Char('f')))); // toggle off
        assert!(!app.library.is_favorite("id0"));
    }

    #[test]
    fn playing_records_history_most_recent_first() {
        let mut app = app_playing(3, 0); // loads id0 -> history [id0]
        app.update(Msg::Key(key(KeyCode::Char('n')))); // id1 -> [id1, id0]
        let hist: Vec<&str> = app.library.history.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(hist, vec!["id1", "id0"]);
    }

    #[test]
    fn favorite_from_search_results() {
        let mut app = App::new(100);
        app.search_results = songs(3);
        app.search_selected = 1;
        app.search_focus = SearchFocus::Results;
        app.mode = Mode::Search;
        let cmds = app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert!(app.library.is_favorite("id1"));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SaveLibrary)));
    }

    #[test]
    fn l_opens_library_and_enter_plays_selected() {
        let mut app = app_playing(3, 0);
        // favorites become [id0, id1] (most-recent-first insertion).
        app.library.toggle_favorite(&songs(2)[1]);
        app.library.toggle_favorite(&songs(2)[0]);
        app.update(Msg::Key(key(KeyCode::Char('l'))));
        assert_eq!(app.mode, Mode::Library);
        assert_eq!(app.library_tab, LibraryTab::Favorites);
        app.update(Msg::Key(key(KeyCode::Down))); // select favorites[1] = id1
        let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.mode, Mode::Player);
        assert_eq!(current(&app), "id1");
        assert!(load_url(&cmds).expect("a Load cmd").contains("id1"));
    }

    #[test]
    fn library_tab_toggles_and_unfavorite_fixes_selection() {
        let mut app = app_playing(1, 0);
        app.library.toggle_favorite(&songs(1)[0]); // favorites = [id0]
        app.update(Msg::Key(key(KeyCode::Char('l'))));
        assert_eq!(app.library_tab, LibraryTab::Favorites);
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.library_tab, LibraryTab::History);
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.library_tab, LibraryTab::Favorites);
        // Unfavorite the only entry: selection clamps to 0, list empties.
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert_eq!(app.library_selected, 0);
        assert!(app.library.favorites.is_empty());
    }

    // --- M6: lyrics ---------------------------------------------------------

    fn lyric_lines() -> Vec<LyricLine> {
        vec![
            LyricLine { time: 0.0, text: "one".to_owned() },
            LyricLine { time: 5.0, text: "two".to_owned() },
        ]
    }

    #[test]
    fn shift_l_toggles_lyrics_and_fetches_on_open() {
        let mut app = app_playing(3, 0); // playing id0
        let cmds = app.update(Msg::Key(key(KeyCode::Char('L'))));
        assert!(app.lyrics_visible);
        assert!(app.lyrics_loading);
        match cmds.as_slice() {
            [Cmd::FetchLyrics { video_id, .. }] => assert_eq!(video_id, "id0"),
            _ => panic!("expected a FetchLyrics cmd"),
        }
        // Toggling off issues no fetch.
        let cmds = app.update(Msg::Key(key(KeyCode::Char('L'))));
        assert!(!app.lyrics_visible);
        assert!(cmds.is_empty());
    }

    #[test]
    fn lyrics_result_stored_only_for_current_track() {
        let mut app = app_playing(3, 0); // current id0
        app.update(Msg::LyricsResult { video_id: "id0".to_owned(), lines: lyric_lines() });
        assert!(app.lyrics.as_ref().is_some_and(|l| l.lines.len() == 2));
        // A late result for a different track is ignored.
        app.update(Msg::LyricsResult { video_id: "stale".to_owned(), lines: lyric_lines() });
        assert_eq!(app.lyrics.as_ref().unwrap().video_id, "id0");
    }

    #[test]
    fn advancing_track_clears_lyrics_and_refetches_when_open() {
        let mut app = app_playing(3, 0);
        app.lyrics_visible = true;
        app.update(Msg::LyricsResult { video_id: "id0".to_owned(), lines: lyric_lines() });
        assert!(app.lyrics.is_some());
        let cmds = app.update(Msg::Key(key(KeyCode::Char('n')))); // -> id1
        assert!(app.lyrics.is_none());
        assert!(app.lyrics_loading);
        assert!(cmds.iter().any(|c| matches!(c, Cmd::FetchLyrics { video_id, .. } if video_id == "id1")));
    }

    // --- M7: downloads ------------------------------------------------------

    #[test]
    fn shift_d_starts_download_of_current_track() {
        let mut app = app_playing(3, 0); // playing id0
        let cmds = app.update(Msg::Key(key(KeyCode::Char('D'))));
        match cmds.as_slice() {
            [Cmd::Download(song)] => assert_eq!(song.video_id, "id0"),
            _ => panic!("expected a Download cmd"),
        }
        assert_eq!(app.downloads.get("id0"), Some(&DownloadState::Running(0)));
    }

    #[test]
    fn download_progress_and_done_update_state() {
        let mut app = app_playing(1, 0);
        app.update(Msg::DownloadProgress { video_id: "id0".to_owned(), percent: 42.6 });
        assert_eq!(app.downloads.get("id0"), Some(&DownloadState::Running(43)));
        app.update(Msg::DownloadDone { video_id: "id0".to_owned(), path: "/tmp/x.m4a".to_owned() });
        assert_eq!(app.downloads.get("id0"), Some(&DownloadState::Done));
        assert!(app.status.contains("/tmp/x.m4a"));
    }

    #[test]
    fn download_error_marks_failed() {
        let mut app = app_playing(1, 0);
        app.update(Msg::DownloadError { video_id: "id0".to_owned(), error: "boom".to_owned() });
        assert_eq!(app.downloads.get("id0"), Some(&DownloadState::Failed));
        assert!(app.status.contains("boom"));
    }

    // --- M8: prefetch / instant skip ----------------------------------------

    fn resolve_cmd<'a>(cmds: &'a [Cmd], id: &str) -> Option<&'a str> {
        cmds.iter().find_map(|c| match c {
            Cmd::Resolve { video_id, watch_url } if video_id == id => Some(watch_url.as_str()),
            _ => None,
        })
    }

    #[test]
    fn loading_prefetches_the_next_track() {
        // Playing id0 → should request a resolve for id1 (the next track).
        let mut app = App::new(100);
        app.search_results = songs(3);
        app.search_selected = 0;
        app.search_focus = SearchFocus::Results;
        app.mode = Mode::Search;
        let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(resolve_cmd(&cmds, "id1").is_some_and(|u| u.contains("id1")));
    }

    #[test]
    fn skip_uses_prefetched_url_when_available() {
        let mut app = app_playing(3, 0); // playing id0, prefetch requested for id1
        app.update(Msg::Resolved {
            video_id: "id1".to_owned(),
            stream_url: "https://cdn.example/stream-id1".to_owned(),
        });
        // Skip: id1 should load via the prefetched direct URL, not its watch URL.
        let cmds = app.update(Msg::Key(key(KeyCode::Char('n'))));
        let url = load_url(&cmds).expect("a Load cmd");
        assert_eq!(url, "https://cdn.example/stream-id1");
        // And it should now prefetch id2.
        assert!(resolve_cmd(&cmds, "id2").is_some());
    }

    #[test]
    fn skip_without_prefetch_falls_back_to_watch_url() {
        let mut app = app_playing(3, 0);
        let cmds = app.update(Msg::Key(key(KeyCode::Char('n')))); // no Resolved arrived
        let url = load_url(&cmds).expect("a Load cmd");
        assert!(url.contains("music.youtube.com/watch") && url.contains("id1"));
    }

    // --- M9: mouse controls -------------------------------------------------

    #[test]
    fn click_on_seekbar_seeks_to_fraction() {
        let mut app = app_playing(1, 0);
        app.duration = Some(200.0);
        app.seekbar_rect.set(Some(Rect { x: 0, y: 5, width: 100, height: 1 }));
        // Column 50 of a 100-wide bar → 50% of 200 s → ~100 s.
        let cmds = app.update(Msg::MouseClick { col: 50, row: 5 });
        match cmds.as_slice() {
            [Cmd::Player(PlayerCmd::SeekAbsolute(t))] => assert!((*t - 100.0).abs() < 1.0),
            _ => panic!("expected a SeekAbsolute cmd"),
        }
    }

    #[test]
    fn click_off_seekbar_is_ignored() {
        let mut app = app_playing(1, 0);
        app.duration = Some(200.0);
        app.seekbar_rect.set(Some(Rect { x: 0, y: 5, width: 100, height: 1 }));
        assert!(app.update(Msg::MouseClick { col: 50, row: 9 }).is_empty()); // wrong row
        assert!(app.update(Msg::MouseClick { col: 200, row: 5 }).is_empty()); // past the bar
    }

    #[test]
    fn click_does_nothing_outside_player_mode() {
        let mut app = app_playing(1, 0);
        app.duration = Some(200.0);
        app.seekbar_rect.set(Some(Rect { x: 0, y: 5, width: 100, height: 1 }));
        app.mode = Mode::Search;
        assert!(app.update(Msg::MouseClick { col: 50, row: 5 }).is_empty());
    }

    #[test]
    fn click_player_buttons_dispatch_actions() {
        let mut app = app_playing(3, 0);
        app.register_mouse_button(
            Rect { x: 10, y: 4, width: 9, height: 1 },
            MouseTarget::Player(Action::TogglePause),
        );
        let cmds = app.update(Msg::MouseClick { col: 12, row: 4 });
        assert!(app.paused);
        assert!(matches!(cmds.as_slice(), [Cmd::Player(PlayerCmd::CyclePause)]));

        app.volume = 40;
        app.register_mouse_button(
            Rect { x: 22, y: 4, width: 8, height: 1 },
            MouseTarget::Player(Action::VolUp),
        );
        let cmds = app.update(Msg::MouseClick { col: 25, row: 4 });
        assert_eq!(app.volume, 45);
        assert!(matches!(cmds.as_slice(), [Cmd::Player(PlayerCmd::SetVolume(45))]));
    }

    #[test]
    fn click_next_button_loads_next_track() {
        let mut app = app_playing(3, 0);
        app.register_mouse_button(
            Rect { x: 0, y: 1, width: 8, height: 1 },
            MouseTarget::Player(Action::NextTrack),
        );
        let cmds = app.update(Msg::MouseClick { col: 3, row: 1 });
        assert_eq!(current(&app), "id1");
        assert!(load_url(&cmds).expect("a Load cmd").contains("id1"));
    }

    #[test]
    fn click_help_button_opens_cheatsheet() {
        let mut app = app_playing(1, 0);
        app.register_mouse_button(
            Rect { x: 0, y: 9, width: 16, height: 1 },
            MouseTarget::Global(Action::ToggleHelp),
        );
        assert!(app.update(Msg::MouseClick { col: 4, row: 9 }).is_empty());
        assert!(app.help_visible);
    }

    #[test]
    fn click_closes_help_overlay_before_buttons() {
        let mut app = app_playing(1, 0);
        app.help_visible = true;
        app.volume = 40;
        app.register_mouse_button(
            Rect { x: 0, y: 1, width: 8, height: 1 },
            MouseTarget::Player(Action::VolUp),
        );
        assert!(app.update(Msg::MouseClick { col: 3, row: 1 }).is_empty());
        assert!(!app.help_visible);
        assert_eq!(app.volume, 40);
    }

    #[test]
    fn rendering_player_registers_control_buttons() {
        let app = app_playing(2, 0);
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

        let buttons = app.mouse_buttons.borrow();
        assert!(buttons.iter().any(|b| b.target == MouseTarget::Player(Action::TogglePause)));
        assert!(buttons.iter().any(|b| b.target == MouseTarget::Player(Action::PrevTrack)));
        assert!(buttons.iter().any(|b| b.target == MouseTarget::Player(Action::NextTrack)));
        assert!(buttons.iter().any(|b| b.target == MouseTarget::Player(Action::VolDown)));
        assert!(buttons.iter().any(|b| b.target == MouseTarget::Player(Action::VolUp)));
        assert!(buttons.iter().any(|b| b.target == MouseTarget::Global(Action::ToggleHelp)));
        assert!(app.seekbar_rect.get().is_some());
    }
}
