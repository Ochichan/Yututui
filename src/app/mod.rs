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
use crate::downloads::DownloadStore;
use crate::eq::{self, EqPreset};
use crate::keymap::{Action, Chord, Conflict, KeyContext, KeyMap};
use crate::t;
use crate::library::Library;
use crate::lyrics::LyricLine;
use crate::player::PlayerCmd;
use crate::playlists::Playlists;
use crate::station::StationStore;
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
mod remote_reducer;
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
/// How many ordered session outcomes (plays/skips/likes/dislikes) to retain for the AI
/// reranker's recovery context. Small: the model only needs the recent arc.
const SESSION_EVENTS_CAP: usize = 20;
/// Minimum gap between autoplay top-up requests (avoids a request storm).
const AUTOPLAY_COOLDOWN: Duration = Duration::from_secs(60);
/// Consecutive empty radio extends before autoplay disables itself (circuit breaker).
const AUTOPLAY_MAX_FAILURES: u8 = 3;
/// How long a resolved AI rerank ordering stays replayable in [`RadioRuntime::ai_cache`]. Short:
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
    /// Whether the "Why AI" overlay is showing. Opened by `Action::WhyAi` (`w`) when the last
    /// autoplay-radio refill went through the AI reranker; lists why each track was chosen (slot
    /// role + reason codes + confidence). Esc / `w` / Back dismiss it, like the About card.
    pub why_ai_visible: bool,

    // Playback ----------------------------------------------------------------
    /// Live playback transport: position, duration, pause state, volume, and speed
    /// (mirrors mpv's current state, distinct from the persisted defaults in `config`).
    pub playback: Playback,
    /// The play queue: ordering, shuffle, repeat, and the current track.
    pub queue: Queue,
    /// The transient status/notification line: its text, last-set time (for TTL expiry), and
    /// semantic kind (see [`Status`]).
    pub status: Status,
    /// Video-overlay state: the detached mpv process (if open) and whether opening it paused
    /// the audio (see [`Video`]). Private — render never reads it.
    video: Video,

    // Audio / EQ --------------------------------------------------------------
    /// Live audio-processing settings (EQ preset + per-band gains, loudness normalization, and
    /// the seek step) — the in-session working copy mpv's filter chain is built from, mirrored
    /// from the persisted `config` (see [`AudioEq`]).
    pub audio: AudioEq,
    /// Auto-extend the queue with related tracks when it runs low (radio mode).
    pub autoplay_radio: bool,
    /// The two mutually-exclusive player status-line dropdowns (EQ preset + radio mode); both
    /// player-only and session-ephemeral (see [`Dropdowns`]).
    pub dropdowns: Dropdowns,
    /// Queue-window overlay state: open flag, selection cursor + anchor, on-screen rect
    /// bridge, and wheel-scroll offset (see [`QueuePopup`]).
    pub queue_popup: QueuePopup,

    // Settings ----------------------------------------------------------------
    /// The persisted config, kept so the settings screen can save the full file.
    pub config: Config,
    /// The settings screen state, present only while `Mode::Settings` is active.
    pub settings: Option<Box<SettingsState>>,

    // AI assistant ------------------------------------------------------------
    /// AI-assistant state: availability, model, chat transcript, prompt, suggestions.
    pub ai: AiState,

    // Radio runtime -----------------------------------------------------------
    /// Radio autoplay runtime: cooldown clock, in-flight pool flag, a handed-off AI rerank,
    /// and the empty-extend circuit-breaker counter.
    pub radio: RadioRuntime,
    /// Consecutive mpv playback errors with no track playing in between, for the
    /// auto-skip circuit breaker (see [`MAX_CONSECUTIVE_PLAY_ERRORS`]).
    consecutive_play_errors: u8,
    /// The user's local playlists (the AI playlist tools read/write these).
    pub playlists: Playlists,
    /// The active natural-language station profile (explore level + avoided artists), distilled
    /// from a `start_radio` vibe and persisted. Read live by [`App::build_station_state`].
    pub station: StationStore,

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
    /// Listening-session tracking (play count + last-start time) for skip-confidence; reset
    /// after a long idle gap (see [`Session`]).
    session: Session,
    /// Library-screen state: active tab, list cursor + multi-select anchor, local
    /// download-folder rows, and the pending file-delete confirmation.
    pub library_ui: LibraryView,

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

    /// Render→reducer bridges: hit-test rects, the active list viewport height, the clickable
    /// button map, and the per-list wheel-scroll offsets — all written by render (`&App`) for
    /// the reducer to read on the next event (see [`RenderBridges`]).
    pub bridges: RenderBridges,

    /// Last whole second we redrew for, so sub-second `time-pos` spam is coalesced.
    last_shown_sec: i64,

    /// Monotonic animation frame counter, bumped on each [`Msg::AnimTick`] (~30 fps) while
    /// animations are active. Drives every effect's phase; wraps harmlessly. `0` at rest.
    anim_frame: u64,

    /// Whether the terminal currently holds input focus (DECSET ?1004, fed by [`Msg::Focus`]).
    /// Defaults to `true`, so terminals/multiplexers that never report focus animate exactly as
    /// before — the pause is strictly additive. Gates [`App::animation_active`].
    pub focused: bool,
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
            why_ai_visible: false,
            playback: Playback {
                volume: volume.clamp(0, VOLUME_MAX),
                speed: 1.0,
                ..Default::default()
            },
            queue: Queue::default(),
            status: Status::default(),
            video: Video::default(),
            anim_frame: 0,
            audio: AudioEq::default(),
            autoplay_radio: false,
            dropdowns: Dropdowns::default(),
            queue_popup: QueuePopup::default(),
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
            radio: RadioRuntime::default(),
            consecutive_play_errors: 0,
            playlists: Playlists::default(),
            station: StationStore::default(),
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
            session: Session::default(),
            library_ui: LibraryView::default(),
            lyrics: Lyrics::default(),
            art: ArtState::default(),
            downloads: Downloads::default(),
            download_store: DownloadStore::default(),
            prefetch: Prefetch::default(),
            bridges: RenderBridges::default(),
            last_shown_sec: -1,
            focused: true,
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
        self.playback.time_pos = None;
        self.playback.duration = None;
        self.playback.paused = true;
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
        let status_before = self.status.text.clone();
        let kind_before = self.status.kind;
        // Default this turn's status to the error styling; the few positive handlers override
        // it to `Info` while they run. This keeps the kind in lock-step with the status text:
        // an error set by one of the ~40 plain `self.status.text = …` sites can never inherit a
        // leftover `Info` color from a previous green toast.
        self.status.kind = StatusKind::Error;
        let cmds = self.dispatch(msg);
        if self.status.text != status_before {
            self.status.set_at =
                if self.status.text.is_empty() { None } else { Some(Instant::now()) };
        } else {
            // Text unchanged this turn — keep the color the still-showing message already had.
            self.status.kind = kind_before;
        }
        cmds
    }

    /// Whether a transient status is currently covering the title (drives the main loop's
    /// expiry tick — see [`Msg::StatusTick`]).
    pub fn status_visible(&self) -> bool {
        self.status.set_at.is_some()
    }

    fn dispatch(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::Key(k) => return self.on_key(k),
            Msg::MouseClick { col, row } => return self.on_mouse_click(col, row),
            Msg::MouseDoubleClick { col, row } => return self.on_mouse_double_click(col, row),
            Msg::MouseRightClick { col, row } => return self.on_mouse_right_click(col, row),
            Msg::MouseDrag { col, row } => return self.on_mouse_drag(col, row),
            Msg::MouseScroll { up } => return self.on_mouse_scroll(up),
            Msg::Resize => self.dirty = true,
            Msg::Quit => self.should_quit = true,
            Msg::Remote(cmd, reply) => {
                let (resp, cmds) = self.apply_remote(cmd);
                let _ = reply.send(resp);
                return cmds;
            }
            Msg::Autoplay => return self.autoplay_on_start_cmds(),
            Msg::StatusTick => {
                // The status has been covering the title long enough — clear it so the
                // wrapper above nulls `status.set_at` and the next frame redraws the title.
                if matches!(self.status.set_at, Some(t) if t.elapsed() >= STATUS_TTL) {
                    self.status.text.clear();
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
            Msg::Focus(f) => {
                // Terminal focus toggled. `animation_active()` reads `focused` to park the ~30 fps
                // tick while we're hidden; one redraw repaints cleanly on the transition (freeze a
                // tidy frame on blur, resume instantly on focus). The seekbar keeps advancing via
                // `PlayerTimePos`, which is independent of this tick.
                self.focused = f;
                self.dirty = true;
            }
            Msg::PlayerTimePos(t) => {
                self.playback.time_pos = Some(t);
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
                self.playback.duration = Some(d);
                self.dirty = true;
            }
            Msg::PlayerPaused(p) => {
                self.playback.paused = p;
                self.dirty = true;
            }
            Msg::PlayerVolume(v) => {
                self.playback.volume = v.round() as i64;
                self.dirty = true;
                tracing::info!(volume = self.playback.volume, "volume");
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
                    prefetched = self.prefetch.last_load_prefetched,
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
                    self.status.text = t!("⚠ Track unavailable — skipped to next", "⚠ 재생할 수 없는 곡 — 다음 곡으로 건너뜀").to_owned();
                    self.dirty = true;
                    return cmds;
                }
                self.status.text = if self.consecutive_play_errors > MAX_CONSECUTIVE_PLAY_ERRORS {
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
                    self.status.text = if crate::i18n::is_korean() {
                        format!("\"{query}\" 검색 결과 없음")
                    } else {
                        format!("No results for \"{query}\"")
                    };
                    self.search.results.clear();
                } else {
                    self.status.text.clear();
                    self.search.results = songs;
                    self.search.selected = 0;
                    self.bridges.search_scroll.reset();
                    self.search.focus = SearchFocus::Results;
                }
                self.dirty = true;
            }
            Msg::SearchError(e) => {
                self.search.searching = false;
                self.status.text = format!("{}: {e}", t!("Search error", "검색 오류"));
                self.dirty = true;
            }
            Msg::DownloadsScanned(songs) => {
                self.library_ui.downloaded = self.enrich_downloads(songs);
                let len = self.library_len();
                if self.library_ui.selected >= len {
                    self.library_ui.selected = len.saturating_sub(1);
                }
                self.dirty = true;
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
            Msg::DownloadProgress { video_id, percent } => {
                self.downloads.active
                    .insert(video_id, DownloadState::Running(percent.round() as u8));
                self.dirty = true;
            }
            Msg::DownloadDone { video_id, path } => {
                self.downloads.active.insert(video_id.clone(), DownloadState::Done);
                let saved = !path.trim().is_empty();
                if saved {
                    let local = self
                        .downloads.sources
                        .remove(&video_id)
                        .map(|source| source.with_local_path(PathBuf::from(&path)))
                        .unwrap_or_else(|| Song::local_file(PathBuf::from(&path)));
                    self.add_downloaded_track(local);
                }
                // Success toast — opt out of this turn's default error styling.
                self.status.kind = StatusKind::Info;
                self.status.text = format!("{}: {path}", t!("Saved", "저장됨"));
                self.dirty = true;
                if saved {
                    // Persist the manifest so the recovered YouTube id survives a restart.
                    return vec![Cmd::SaveDownloads];
                }
            }
            Msg::DownloadError { video_id, error } => {
                self.downloads.active
                    .insert(video_id.clone(), DownloadState::Failed);
                self.downloads.sources.remove(&video_id);
                self.status.text = format!("{}: {error}", t!("Download failed", "다운로드 실패"));
                self.dirty = true;
            }
            Msg::Resolved {
                video_id,
                stream_url,
            } => {
                // Bounded prefetch cache; no redraw (purely a skip-latency optimization).
                if self.prefetch.resolved.len() >= RESOLVED_MAX {
                    self.prefetch.resolved.clear();
                }
                self.prefetch.resolved.insert(video_id, stream_url);
            }
            Msg::RadioResults {
                seed_video_id,
                candidates,
            } => {
                self.radio.pending = false;
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
            Msg::RadioAiPicks { seed_video_id, picks, conf } => {
                self.ai.thinking = false;
                self.dirty = true;
                // Only consume `pending_rerank` when this result is for it (a stale/duplicate
                // message for some other seed leaves the current rerank untouched). When it does
                // match but the seed is no longer queued (the user skipped/cleared mid-think),
                // the chain still drops the stale rerank without enqueuing.
                let ours = self
                    .radio.pending_rerank
                    .as_ref()
                    .is_some_and(|p| p.seed_video_id == seed_video_id);
                if ours
                    && let Some(pending) = self.radio.pending_rerank.take()
                    && self.autoplay_radio
                    && self.queue.contains_video_id(&seed_video_id)
                {
                    if let Some(conf) = conf {
                        tracing::debug!(conf, picks = picks.len(), "radio AI rerank confidence");
                    }
                    // Resolve the model's opaque cids back to real tracks once, keeping its order. A
                    // cid that isn't in the pack (a hallucinated id) is dropped here; `merge_ai_picks`
                    // then re-validates against the shortlist and tops up from the local pick. The
                    // same resolution feeds the "Why AI" overlay (title + role + reasons), which must
                    // outlive the `pending_rerank` we're about to drop.
                    let resolved: Vec<(String, ExplainPick)> = picks
                        .iter()
                        .filter_map(|p| {
                            let vid = pending.cid_map.iter().find(|m| m.cid == p.cid)?.video_id.clone();
                            let song = pending.shortlist.iter().find(|s| s.video_id == vid)?;
                            let pick = ExplainPick {
                                title: song.title.clone(),
                                artist: song.artist.clone(),
                                role: p.role.clone(),
                                reasons: p.reasons.clone(),
                            };
                            Some((vid, pick))
                        })
                        .collect();
                    let ids: Vec<String> = resolved.iter().map(|(vid, _)| vid.clone()).collect();
                    if !resolved.is_empty() {
                        self.radio.last_explain = Some(RadioAiExplain {
                            conf,
                            picks: resolved.into_iter().map(|(_, p)| p).collect(),
                        });
                    }
                    let picks = radio::merge_ai_picks(
                        &ids,
                        &pending.shortlist,
                        &pending.local_pick,
                        self.config.radio.ai.picks,
                    );
                    // Cache the validated ordering so a rapid identical refill replays it without a
                    // second call. Skip empty results (a failed rerank) so the next refill retries.
                    if !ids.is_empty() {
                        self.ai_cache_store(pending.cache_key, ids);
                    }
                    return self.extend_queue_from_radio(picks);
                }
            }
            Msg::RadioError {
                seed_video_id,
                error,
            } => {
                self.radio.pending = false;
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
                    self.status.text.clear();
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
                self.bridges.ai_scroll.reset();
                self.dirty = true;
            }
            Msg::AiSetAutoplay(on) => {
                self.autoplay_radio = on;
                self.dirty = true;
                if on {
                    self.radio.consecutive_failures = 0;
                    // Same proactive top-up as the manual toggle (see Action::ToggleRadio).
                    return self.maybe_autoplay_extend();
                }
            }
            Msg::AiSetStationProfile {
                query,
                explore,
                avoid_artists,
            } => {
                // Distill the vibe into engine knobs the local radio can actually act on:
                // adventurousness (→ mode) and artists to keep out (→ banned_artist_keys, read
                // live in `build_station_state`). Persist both so the station survives a restart.
                let profile =
                    crate::station::StationProfile::from_intent(&query, explore.as_deref(), &avoid_artists);
                self.config.radio.mode = profile.explore.to_mode();
                self.station.active = Some(profile);
                self.dirty = true;
                return vec![Cmd::SaveStationProfile, Cmd::SaveConfig(Box::new(self.config.clone()))];
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
                    self.status.text.clear();
                    let song = self.queue.current().cloned();
                    return self.load_song(song);
                }
            }
            Msg::StationPatch { down_artists, boost_artists } => {
                // The off-path feedback summary landed (possibly empty on failure) — always clear
                // the in-flight guard so the next streak can trigger again. Fold the avoid/boost
                // into the active station and persist only when the avoid list actually changed.
                self.radio.feedback_in_flight = false;
                if let Some(profile) = self.station.active.as_mut()
                    && profile.apply_feedback(&down_artists, &boost_artists)
                {
                    self.dirty = true;
                    return vec![Cmd::SaveStationProfile];
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
        self.dropdowns.eq_open = false;
        self.dropdowns.radio_open = false;
        self.queue_popup.open = false;
        self.library_ui.confirm_delete = None;
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
        let rows = self.bridges.list_viewport_rows.get() as usize;
        if rows <= 1 { DEFAULT_PAGE_ROWS } else { rows - 1 }
    }


    /// Switch screens from a nav-bar click — the mouse equivalent of the `Open*` keys, but
    /// reachable from any screen. Leaving Settings commits the draft via the normal close
    /// path so edits aren't lost; transient overlays are cleared.
    fn navigate_to(&mut self, mode: Mode) -> Vec<Cmd> {
        self.dropdowns.eq_open = false;
        self.dropdowns.radio_open = false;
        self.queue_popup.open = false;
        self.library_ui.confirm_delete = None;
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
                // Start each library visit clean (cursor, anchor, scroll, and any filter).
                self.clear_library_filter();
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
