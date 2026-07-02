//! Grouped sub-states of [`App`] (Stage 2 of the re-architecture).
//!
//! The reducer historically kept ~70 flat fields on `App`; these structs gather the
//! cohesive per-domain groups so ownership reads clearly and future changes stay local.
//! Behaviour-preserving: the fields are the same, just nested (`app.search.input`).

use super::*;

/// Live audio-processing settings: the active EQ preset and its per-band gains, loudness
/// normalization, and the seek step. The in-session working copy mpv's filter chain is built
/// from — distinct from the persisted defaults in [`Config`].
pub struct AudioEq {
    /// Selected equalizer preset (drives `bands` when chosen via `e`).
    pub preset: EqPreset,
    /// Current per-band gains (dB); editable live from the settings screen.
    pub bands: [f64; eq::BANDS],
    /// Loudness normalization (`dynaudnorm`) on/off.
    pub normalize: bool,
    /// Seconds jumped per seek-back/-forward key (configurable; default 10s).
    pub seek_seconds: f64,
}

impl Default for AudioEq {
    fn default() -> Self {
        // Matches the historical flat-field init in `App::new` (apply_config overlays the
        // persisted values afterwards); seek_seconds is non-zero so this can't derive.
        Self {
            preset: EqPreset::default(),
            bands: [0.0; eq::BANDS],
            normalize: false,
            seek_seconds: crate::config::SEEK_SECONDS_DEFAULT,
        }
    }
}

/// Render→reducer bridges: state the render pass (which only has `&App`) writes so the reducer
/// can read it on the next event. The plan keeps these together so the one place render reaches
/// back into the reducer is visible at a glance and never split across domains.
#[derive(Default)]
pub struct RenderBridges {
    /// Screen rect of the seekbar, written by the player view each render so a mouse click can
    /// be hit-tested against it. `Cell` because render only has `&App`.
    pub seekbar_rect: Cell<Option<Rect>>,
    /// Viewport height (rows) of the active Library / Search list, written each render so
    /// PageUp/PageDown can move by a screenful. `Cell` because render only has `&App`.
    pub list_viewport_rows: Cell<u16>,
    /// Clickable button rects written by views each render. `RefCell` because render only has
    /// `&App`, but the reducer needs the last rendered hit map.
    pub mouse_buttons: RefCell<Vec<MouseButtonRegion>>,
    /// Decoupled wheel-scroll offset for each browse list (see [`crate::ui::scroll`]). The mouse
    /// wheel moves these directly; the render pass nudges them to keep the keyboard selection
    /// on-screen with a margin. One per list so each keeps its own place.
    pub library_scroll: crate::ui::scroll::ScrollState,
    pub search_scroll: crate::ui::scroll::ScrollState,
    pub ai_transcript_scroll: crate::ui::scroll::ScrollState,
    /// Last rendered DJ Gem transcript visual lines, after wrapping and prefix indentation.
    /// Mouse-drag copy uses these exact rows so the copied text matches what was selected.
    pub ai_transcript_copy_lines: RefCell<Vec<String>>,
    pub ai_scroll: crate::ui::scroll::ScrollState,
    /// The Settings field list keeps its own persistent offset too, so a mouse click on a
    /// visible row focuses it in place instead of letting ratatui re-derive the offset from 0
    /// each frame (which snapped the clicked row across the viewport).
    pub settings_scroll: crate::ui::scroll::ScrollState,
    /// One offset per column of the two-column Keys tab; only the focused column re-anchors.
    pub settings_keys_scroll: [crate::ui::scroll::ScrollState; 2],
}

impl RenderBridges {
    /// Reset every Settings list's scroll offset (the field list and both Keys columns). Called
    /// when the Settings screen opens or its tab changes, so a stale offset from the previous
    /// tab/session can't carry over onto a different, shorter set of rows.
    pub fn reset_settings_scroll(&self) {
        self.settings_scroll.reset();
        self.settings_keys_scroll[0].reset();
        self.settings_keys_scroll[1].reset();
    }
}

/// The transient status/notification line shown to the user: its text, when it was last set
/// (for TTL auto-expiry), and its semantic kind (which drives its color).
#[derive(Default)]
pub struct Status {
    /// A status/error line shown to the user (empty = normal; the track title shows instead).
    pub text: String,
    /// When `text` was last set non-empty, used to auto-expire it after [`STATUS_TTL`] (set
    /// centrally in [`App::update`]; `None` while the title is showing normally). Reducer-only
    /// (was a private App field) — `pub(in crate::app)` keeps it off the render-facing surface.
    pub(in crate::app) set_at: Option<Instant>,
    /// Semantic kind of the current status (drives its color); reset to `Error` on clear.
    pub kind: StatusKind,
}

/// Click-to-open dropdowns. The player status-line menus are mutually exclusive; the search
/// source menu is separate but shares the same modal dismissal path.
#[derive(Default)]
pub struct Dropdowns {
    /// Whether the EQ-preset dropdown is showing (toggled by clicking the `eq:` label,
    /// dismissed by picking a preset or clicking elsewhere).
    pub eq_open: bool,
    /// Whether the streaming-mode dropdown is showing. Mutually exclusive with `eq_open`.
    pub streaming_open: bool,
    /// Whether the search-source dropdown is showing.
    pub search_source_open: bool,
}

/// Listening-session tracking for skip-confidence: how many tracks have started this session
/// (reset after a long idle gap) and when the last one started (to detect that gap).
#[derive(Default)]
pub struct Session {
    /// Tracks started in the current listening session (reset after a long idle gap). Used to
    /// down-weight skip→dislike learning early in / in short sessions (noisier signal).
    pub plays: u32,
    /// Unix time of the last track start, for detecting session boundaries (idle gaps).
    pub last_activity_at: Option<i64>,
}

/// Video-overlay state: the detached mpv process (if open) and whether opening it is what
/// paused the audio (so closing only resumes playback the overlay paused).
#[derive(Default)]
pub struct Video {
    /// The detached mpv video-overlay process, if one is open. Tracked so a second `v` (or a
    /// `Shift+V` layout switch) closes/respawns it instead of stacking windows.
    pub proc: Option<std::process::Child>,
    /// Whether opening the video overlay is what paused the audio, so closing it only resumes
    /// playback we paused (not audio the user had paused themselves).
    pub paused_audio: bool,
}

/// Live playback transport: position, track length, pause state, output volume, and speed.
/// These mirror mpv's current state (distinct from the persisted defaults in [`Config`]).
#[derive(Default)]
pub struct Playback {
    /// Playback position in seconds, if known.
    pub time_pos: Option<f64>,
    /// When `time_pos` was last (re)based, so the OS media session can interpolate the
    /// live position between mpv reports (`time_pos + elapsed × speed` while playing).
    /// Rebasing also happens on pause/resume so a long pause never reads as progress.
    pub time_pos_at: Option<Instant>,
    /// Bumped on every position discontinuity — a seek or a track (re)start — so the
    /// media session knows to re-announce the position (MPRIS `Seeked`, SMTC timeline
    /// reset, macOS elapsed update). Ordinary playback progress never bumps it.
    pub position_epoch: u64,
    /// Track duration in seconds, if known.
    pub duration: Option<f64>,
    /// Whether playback is currently paused.
    pub paused: bool,
    /// Output volume, 0-100.
    pub volume: i64,
    /// Playback speed multiplier (1.0 = normal).
    pub speed: f64,
    /// Current live-radio now-playing metadata, when the active stream exposes it.
    pub stream_now_playing: Option<StreamNowPlaying>,
}

/// Prefetch / load tracking: the pre-resolved stream-URL cache, whether the current track
/// loaded from that cache, and the `video_id` actually loaded into mpv.
#[derive(Default)]
pub struct Prefetch {
    /// Pre-resolved direct stream URLs, keyed by `video_id` (for instant skip).
    pub resolved: HashMap<String, String>,
    /// Whether the current track was loaded from a prefetched direct URL (vs the watch
    /// URL mpv resolves itself). Recorded so a playback error can note the likelier cause
    /// (a stale prefetched CDN URL) in the log.
    pub last_load_prefetched: bool,
    /// `video_id` of the track actually loaded into mpv. A cached/restored queue entry can
    /// be visible before it is loaded; the first play action then loads it instead of only
    /// toggling mpv's pause property.
    pub loaded_video_id: Option<String>,
}

/// Download state, keyed by `video_id`: the in-flight/finished progress map shown in the UI,
/// plus the original catalog metadata held while a download runs.
#[derive(Default)]
pub struct Downloads {
    /// In-flight / finished downloads, keyed by `video_id`, for the UI indicator.
    pub active: HashMap<String, DownloadState>,
    /// Original catalog metadata for in-flight downloads, keyed by `video_id`. Reducer-only
    /// (was a private App field) — `pub(in crate::app)` keeps it off the render-facing surface.
    pub(in crate::app) sources: HashMap<String, Song>,
}

/// The Spotify playlist picker overlay (Settings › Accounts › Import from Spotify…):
/// "Liked Songs" first, then the account's playlists.
pub struct SpotifyPicker {
    pub items: Vec<crate::transfer::actor::PickerPlaylist>,
    pub selected: usize,
}

/// Lyrics-panel state: whether the panel is shown, the in-flight flag, and the fetched
/// lyrics for the current track.
#[derive(Default)]
pub struct Lyrics {
    /// Whether the lyrics panel is shown in the player view.
    pub visible: bool,
    /// True between requesting lyrics and the result arriving.
    pub loading: bool,
    /// Lyrics for the current track, if fetched.
    pub track: Option<TrackLyrics>,
}

/// Search-screen state: the query, its results, selection, focus, and in-flight flag.
#[derive(Default)]
pub struct SearchState {
    /// The search query being typed.
    pub input: String,
    /// The source currently selected in the search box.
    pub source: SearchSource,
    /// Whether Ctrl+A has selected the whole query (desktop-style: the next edit
    /// replaces or clears it). Reset on any consuming keypress.
    pub select_all: bool,
    /// Whether the input box or the results list has focus.
    pub focus: SearchFocus,
    /// The current search results.
    pub results: Vec<Song>,
    /// The highlighted result row.
    pub selected: usize,
    /// True between issuing a search request and its results arriving.
    pub searching: bool,
}

/// DJ Gem assistant state: availability, model, the chat transcript, the prompt being
/// typed, and the pickable suggestions list with its focus.
#[derive(Default)]
pub struct AiState {
    /// Whether a Gemini API key is configured (gates the assistant; `false` → onboarding).
    pub available: bool,
    /// The Gemini model the assistant uses (shown in the DJ Gem view header).
    pub model: GeminiModel,
    /// The chat transcript (user prompts, assistant replies, errors).
    pub messages: Vec<AiMessage>,
    /// The DJ Gem prompt being typed.
    pub input: String,
    /// Whether Ctrl+A has selected the whole DJ Gem prompt (next edit replaces/clears it).
    pub select_all: bool,
    /// True while a request is in flight (drives the spinner; blocks a second request).
    pub thinking: bool,
    /// The pickable related-tracks list (get_suggestions).
    pub suggestions: Vec<Song>,
    /// The highlighted suggestion row.
    pub suggestions_selected: usize,
    /// Whether the input box or the suggestions list has focus.
    pub focus: AiFocus,
}

/// Runtime state for Latin-script title overlays. The cache is persisted separately from the
/// source library so source metadata stays untouched.
#[derive(Default)]
pub struct RomanizationRuntime {
    pub cache: crate::romanize::RomanizeCache,
    pub pending: HashSet<String>,
    pub next_request_id: u64,
    pub min_valid_request_id: u64,
}

/// Album-art state: the terminal graphics picker, the held render protocol, its decoded
/// source image and dimensions, the owning track id, and the in-flight flag.
#[derive(Default)]
pub struct ArtState {
    /// The terminal graphics picker (font size + detected protocol), built once at startup
    /// when album art is enabled. `None` → feature off, or the terminal couldn't be probed
    /// (no art is fetched or drawn in that case).
    pub picker: Option<Picker>,
    /// The current track's art as a render-ready, threaded resize protocol. `RefCell` because
    /// `StatefulImage` needs `&mut` during render, which only has `&App` (mirrors the
    /// [`RenderBridges`] fields). Resize/encode work is sent off-thread through `resize_tx`.
    pub protocol: RefCell<Option<ThreadProtocol>>,
    /// Last rendered album-art cell rect. Used by popup renderers to make Kitty rows that were
    /// overdrawn in the middle repaint cleanly when the popup closes.
    pub rect: Cell<Option<Rect>>,
    /// Background resize/encode request channel for [`ThreadProtocol`].
    pub(in crate::app) resize_tx: Option<tokio::sync::mpsc::UnboundedSender<ResizeRequest>>,
    /// The decoded source image kept alongside the protocol for stale-result checks and future
    /// resize/protocol rebuilds. Reducer-only (was a private App field) — `pub(in crate::app)`.
    pub(in crate::app) source: Option<DynamicImage>,
    /// Source pixel dimensions of the held art, for centering it within its panel.
    pub dims: (u32, u32),
    /// `video_id` the held art belongs to (guards against a stale image lingering).
    /// Reducer-only (was a private App field) — `pub(in crate::app)`.
    pub(in crate::app) video_id: Option<String>,
    /// True between requesting art and the result arriving.
    pub loading: bool,
    /// Last visible-overlay bitmask observed by the reducer. When this changes while a native
    /// terminal image can be visible, the next draw clears the terminal before repainting so
    /// Sixel / Kitty / iTerm2 state cannot keep popup residue outside ratatui's diff buffer.
    pub(in crate::app) overlay_mask: u16,
    /// One-shot request for the render loop to call `Terminal::clear()` before the next frame.
    /// Kept in art state because the expensive clear is only needed to resync native terminal
    /// graphics after an overlay or screen transition has covered album art or the About icon.
    pub(in crate::app) force_clear_next_frame: bool,
    /// Extra clear/redraw frames after native album art is refreshed while an overlay is visible.
    /// Windows Terminal can composite the graphics payload one frame after the text overlay, so the
    /// overlay needs a short reinforcement burst rather than a single clear.
    pub(in crate::app) overlay_refresh_clear_frames: u8,
}

/// Streaming autoplay runtime: the cooldown clock, the in-flight pool flag, a handed-off DJ Gem
/// rerank awaiting its picks, and the empty-extend circuit-breaker counter.
#[derive(Default)]
pub struct StreamingRuntime {
    /// When the autoplay hook last fired a top-up request (for the cooldown).
    pub last_extend: Option<Instant>,
    /// True while the streaming candidate-pool fetch is in flight (both the DJ Gem and non-DJ Gem paths
    /// fetch the same pool first).
    pub pending: bool,
    /// An DJ Gem rerank handed off to the assistant actor, awaiting its `Msg::StreamingAiPicks`. Holds
    /// the shortlist (to validate the returned ids against) and the local pick (the fallback).
    pub pending_rerank: Option<PendingRerank>,
    /// Consecutive empty streaming extends, for the autoplay circuit breaker.
    pub consecutive_failures: u8,
    /// The last DJ Gem rerank's resolved explanation (picks → role + reasons + confidence), stashed
    /// when `Msg::StreamingAiPicks` resolves so the "Why DJ Gem" overlay (`w`) can show why these tracks
    /// were chosen. `None` until the first DJ Gem rerank lands.
    pub last_explain: Option<StreamingAiExplain>,
    /// Ordered recent listening outcomes (plays / skips / likes / dislikes), newest at the back,
    /// bounded to the last [`SESSION_EVENTS_CAP`]. Drives the reranker's recovery context.
    pub session_events: std::collections::VecDeque<SessionEvent>,
    /// TTL cache of resolved DJ Gem rerank orderings, keyed by [`streaming::ai_cache_key`]. Each value is
    /// the DJ Gem's chosen `video_id` ordering plus when it was stored; a rapid identical refill
    /// replays it instead of spending another call. Pruned by TTL on every insert (stays tiny).
    pub ai_cache: HashMap<u64, (Vec<String>, Instant)>,
    /// Cached co-occurrence graph keyed by [`Signals::play_log_generation`], so streaming refills don't
    /// rebuild the same nested HashMap when listening history has not changed.
    pub cooc_cache: Option<(u64, Cooc)>,
    /// True while an off-path feedback summary is handed off to the assistant actor, awaiting its
    /// `Msg::StationPatch`. A single-flight guard so a skip streak can't fan out duplicate calls.
    pub feedback_in_flight: bool,
    /// When the last feedback summary was dispatched, for the cooldown between summaries (a skip
    /// streak shouldn't trigger one every track). `None` until the first summary fires.
    pub last_feedback_at: Option<Instant>,
}

/// Library-screen state: the active tab, the list cursor and its multi-select anchor, the
/// local download-folder rows, and the pending file-delete confirmation.
#[derive(Default)]
pub struct LibraryView {
    /// Which library list is shown (All / Favorites / History / Radio Favorites / Radio / Downloads).
    pub tab: LibraryTab,
    /// The highlighted row in the active library list.
    pub selected: usize,
    /// The fixed end of the library list's multi-select range (drag start / last single
    /// click). The selection is the inclusive span between this and `selected`, mirroring
    /// the queue window's drag-to-select.
    pub anchor: usize,
    /// Local audio files found in the configured download directory.
    pub downloaded: Vec<Song>,
    /// Pending "delete downloaded files" confirmation: the on-disk paths queued for deletion
    /// (file removal is irreversible, so it's gated behind an explicit yes/no). `None` when no
    /// modal is open.
    pub confirm_delete: Option<Vec<PathBuf>>,
    /// In-library incremental filter query (`/`). When non-empty, the active list narrows to
    /// rows whose title or artist contains it (case-insensitive). Empty = no filter.
    pub filter_query: String,
    /// Whether the filter input box is capturing keystrokes (typed chars edit `filter_query`
    /// and the list narrows live). Committed with Enter (keeps the filter, returns to list
    /// navigation); cleared with Esc.
    pub filter_editing: bool,
    /// Drill-down state of the Playlists tab: the id of the opened playlist whose songs are
    /// listed. `None` = the root level (the playlist list itself).
    pub open_playlist: Option<String>,
    /// The create-playlist popup's name buffer. `Some` while the popup is open and capturing
    /// keystrokes (Enter creates, Esc cancels).
    pub create_input: Option<String>,
    /// Pending "delete playlist" confirmation: the id of the playlist queued for deletion
    /// (removes the whole list at once, so it's gated behind an explicit yes/no like the
    /// download-file delete). `None` when no modal is open.
    pub confirm_playlist_delete: Option<String>,
}

/// The "add to playlist" picker popup: which songs are being added, the highlighted row,
/// and the optional inline new-playlist name entry. Opens over any screen (Library rows,
/// Search results, the Player's current track), so it lives on [`App`] rather than a view.
pub struct PlaylistPicker {
    /// The song(s) to add — a library multi-select range, one search result, or the
    /// current track.
    pub songs: Vec<Song>,
    /// The highlighted picker row: `0..len` are playlists, `len` is "New playlist…".
    pub cursor: usize,
    /// `Some` while the trailing "New playlist…" row is capturing a name (phase two of
    /// the popup). Enter creates the playlist and adds the songs; Esc returns to the list.
    pub naming: Option<String>,
}

/// Queue-window overlay state: whether it's open, the selection cursor + anchor, its
/// on-screen rect (a render→reducer bridge), and its wheel-scroll offset. Grouping the
/// `Cell`/scroll bridges here keeps them next to the overlay state they belong to.
#[derive(Default)]
pub struct QueuePopup {
    /// Whether the queue window (opened by clicking the `N/M` position label) is showing.
    /// Player-only overlay; while open it captures the keyboard (nav / Delete / Enter).
    pub open: bool,
    /// The highlighted row in the queue window (order position) — the active end of the
    /// drag/range selection.
    pub cursor: usize,
    /// The fixed end of the queue window's multi-select range (drag start / last single
    /// click). The selection is the inclusive span between this and `cursor`.
    pub anchor: usize,
    /// Screen rect of the open queue window, written each render so a click outside it can
    /// be detected (which closes it). `Cell` because render only has `&App`.
    pub rect: Cell<Option<Rect>>,
    /// Decoupled wheel-scroll offset for the queue window (see [`crate::ui::scroll`]).
    pub scroll: crate::ui::scroll::ScrollState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragSurface {
    Queue,
    Library,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DragSelection {
    pub surface: DragSurface,
    pub anchor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AiTranscriptDrag {
    pub anchor: usize,
    pub cursor: usize,
    pub moved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScrollbarDrag {
    pub surface: ScrollSurface,
    pub rect: Rect,
    pub content_len: usize,
    pub viewport: usize,
    pub grab: u16,
}
