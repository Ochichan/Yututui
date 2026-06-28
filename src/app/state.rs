//! Grouped sub-states of [`App`] (Stage 2 of the re-architecture).
//!
//! The reducer historically kept ~70 flat fields on `App`; these structs gather the
//! cohesive per-domain groups so ownership reads clearly and future changes stay local.
//! Behaviour-preserving: the fields are the same, just nested (`app.search.input`).

use super::*;

/// Live playback transport: position, track length, pause state, output volume, and speed.
/// These mirror mpv's current state (distinct from the persisted defaults in [`Config`]).
#[derive(Default)]
pub struct Playback {
    /// Playback position in seconds, if known.
    pub time_pos: Option<f64>,
    /// Track duration in seconds, if known.
    pub duration: Option<f64>,
    /// Whether playback is currently paused.
    pub paused: bool,
    /// Output volume, 0-100.
    pub volume: i64,
    /// Playback speed multiplier (1.0 = normal).
    pub speed: f64,
}

/// Download state, keyed by `video_id`: the in-flight/finished progress map shown in the UI,
/// plus the original catalog metadata held while a download runs.
#[derive(Default)]
pub struct Downloads {
    /// In-flight / finished downloads, keyed by `video_id`, for the UI indicator.
    pub active: HashMap<String, DownloadState>,
    /// Original catalog metadata for in-flight downloads, keyed by `video_id`.
    pub sources: HashMap<String, Song>,
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

/// AI-assistant state: availability, model, the chat transcript, the prompt being
/// typed, and the pickable suggestions list with its focus.
#[derive(Default)]
pub struct AiState {
    /// Whether a Gemini API key is configured (gates the assistant; `false` → onboarding).
    pub available: bool,
    /// The Gemini model the assistant uses (shown in the AI view header).
    pub model: GeminiModel,
    /// The chat transcript (user prompts, assistant replies, errors).
    pub messages: Vec<AiMessage>,
    /// The AI prompt being typed.
    pub input: String,
    /// Whether Ctrl+A has selected the whole AI prompt (next edit replaces/clears it).
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

/// Album-art state: the terminal graphics picker, the held render protocol, its decoded
/// source image and dimensions, the owning track id, and the in-flight flag.
#[derive(Default)]
pub struct ArtState {
    /// The terminal graphics picker (font size + detected protocol), built once at startup
    /// when album art is enabled. `None` → feature off, or the terminal couldn't be probed
    /// (no art is fetched or drawn in that case).
    pub picker: Option<Picker>,
    /// The current track's art as a render-ready, resizable protocol. `RefCell` because
    /// `StatefulImage` needs `&mut` during render, which only has `&App` (mirrors
    /// [`App::mouse_buttons`]).
    pub protocol: RefCell<Option<StatefulProtocol>>,
    /// The decoded source image kept alongside the protocol so [`App::refresh_art`] can
    /// rebuild a fresh protocol (new graphics-protocol id) on demand — see that method for why.
    pub source: Option<DynamicImage>,
    /// Source pixel dimensions of the held art, for centering it within its panel.
    pub dims: (u32, u32),
    /// `video_id` the held art belongs to (guards against a stale image lingering).
    pub video_id: Option<String>,
    /// True between requesting art and the result arriving.
    pub loading: bool,
}

/// Radio autoplay runtime: the cooldown clock, the in-flight pool flag, a handed-off AI
/// rerank awaiting its picks, and the empty-extend circuit-breaker counter.
#[derive(Default)]
pub struct RadioRuntime {
    /// When the autoplay hook last fired a top-up request (for the cooldown).
    pub last_extend: Option<Instant>,
    /// True while the radio candidate-pool fetch is in flight (both the AI and non-AI paths
    /// fetch the same pool first).
    pub pending: bool,
    /// An AI rerank handed off to the assistant actor, awaiting its `Msg::RadioAiPicks`. Holds
    /// the shortlist (to validate the returned ids against) and the local pick (the fallback).
    pub pending_rerank: Option<PendingRerank>,
    /// Consecutive empty radio extends, for the autoplay circuit breaker.
    pub consecutive_failures: u8,
}

/// Library-screen state: the active tab, the list cursor and its multi-select anchor, the
/// local download-folder rows, and the pending file-delete confirmation.
#[derive(Default)]
pub struct LibraryView {
    /// Which library list is shown (All / Favorites / History / Downloads).
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
}
