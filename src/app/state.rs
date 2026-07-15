//! Grouped sub-states of [`App`] (Stage 2 of the re-architecture).
//!
//! The reducer historically kept ~70 flat fields on `App`; these structs gather the
//! cohesive per-domain groups so ownership reads clearly and future changes stay local.
//! Behaviour-preserving: the fields are the same, just nested (`app.search.input`).

use super::*;
use crate::lyrics::LyricDelay;

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
    /// Viewport height (rows) of the active Library / Search list, written each render so
    /// PageUp/PageDown can move by a screenful. `Cell` because render only has `&App`.
    pub list_viewport_rows: Cell<u16>,
    /// The responsive tier the last frame rendered at (see [`crate::ui::layout::tier`]).
    /// Bridged rather than derived from resize events because text zoom rescales the
    /// virtual grid without one; the reducer reads it for key routing and art geometry.
    pub ui_tier: Cell<crate::ui::layout::UiTier>,
    /// Decoupled wheel-scroll offset for each browse list (see [`crate::ui::scroll`]). The mouse
    /// wheel moves these directly; the render pass nudges them to keep the keyboard selection
    /// on-screen with a margin. One per list so each keeps its own place.
    pub library_scroll: crate::ui::scroll::ScrollState,
    pub search_scroll: crate::ui::scroll::ScrollState,
    /// Local Find owns its own result offset. It deliberately does not reuse the online
    /// Search offset because both surfaces keep their cursor/viewport across mode round-trips.
    pub local_find_scroll: crate::ui::scroll::ScrollState,
    pub ai_transcript_scroll: crate::ui::scroll::ScrollState,
    /// Last rendered DJ Gem transcript visual lines, after wrapping and prefix indentation.
    /// Mouse-drag copy uses these exact rows so the copied text matches what was selected.
    pub ai_transcript_copy_lines: RefCell<Arc<[Arc<str>]>>,
    pub ai_scroll: crate::ui::scroll::ScrollState,
    /// The Settings field list keeps its own persistent offset too, so a mouse click on a
    /// visible row focuses it in place instead of letting ratatui re-derive the offset from 0
    /// each frame (which snapped the clicked row across the viewport).
    pub settings_scroll: crate::ui::scroll::ScrollState,
    /// One offset per column of the two-column Keys tab; only the focused column re-anchors.
    pub settings_keys_scroll: [crate::ui::scroll::ScrollState; 2],
    /// Wheel / arrow-key offset for the help and mouse cheat-sheet overlays (one state is
    /// enough — they never show together). Reset when either overlay opens; the render pass
    /// records the viewport and clamps to the sheet's real length like every other list.
    pub help_scroll: crate::ui::scroll::ScrollState,
    /// Selected-row marquee bridge (see `ui::anim::selected_marquee`): which list row is
    /// crawling (`surface + index`, to restart the phase when the cursor moves), the
    /// anim-frame its phase started at, and whether *this* frame's render actually produced
    /// a scrolling row. `marquee_ran` is reset at the top of `ui::render` and read by
    /// `App::animation_active`, which keeps the clock ticking for it independently of every
    /// animation toggle — that independence is the feature, not an accident.
    pub marquee_key: Cell<Option<(ScrollSurface, usize)>>,
    pub marquee_origin: Cell<u64>,
    pub marquee_ran: Cell<bool>,
    pub marquee_cache: RefCell<crate::ui::marquee::MarqueeCache>,
    /// Whether the last render actually placed at least one canvas effect. Unlike raw config,
    /// this excludes effects hidden by the responsive layout or focal safe-area constraints.
    pub canvas_active: Cell<bool>,
    /// Whether any actually placed canvas effect uses the heavy redraw cadence. This keeps the
    /// 20 fps cap and synchronized terminal updates active even when lyrics are in the foreground.
    pub canvas_heavy_active: Cell<bool>,
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

/// One-shot animation feedback ("fx") state: the frame each event-driven effect started, plus
/// the last-observed values [`App::update`] diffs against to fire them. Centralizing detection
/// in the reducer wrapper means every input path — key, mouse, remote, DJ Gem — triggers the
/// same feedback without ~40 call sites having to remember to. Start frames are read by
/// `crate::ui::anim`; the `last_*` anchors are reducer-only. While `anim_frame < until` the
/// animation clock stays awake even where it would otherwise sleep (paused, non-player views),
/// so a one-shot always gets to finish; afterwards the clock re-sleeps and, with every flag
/// off, nothing here is ever armed and rendering is byte-for-byte unchanged.
pub struct FxState {
    /// The clock stays armed while `anim_frame < until` (the max deadline of all live
    /// one-shots). Comparison-based, so stale values are harmless once passed.
    pub(in crate::app) until: u64,
    /// New status text was set → typewriter reveal (all views' status bands).
    pub toast: Option<u64>,
    /// The current track changed → title letter-cascade intro.
    pub track_intro: Option<u64>,
    /// The volume changed → transient gauge under the transport strip.
    pub volume: Option<u64>,
    /// The current track flipped to liked → heart burst around the title.
    pub like: Option<u64>,
    /// A seek command was issued → ripple at the seekbar head.
    pub seek: Option<u64>,
    /// The screen switched → nav-tab pop. Carries the mode switched *to*.
    pub switch: Option<(u64, Mode)>,
    /// An in-view tab bar changed (Library tabs / Settings tabs) → tab pop.
    pub tabbar: Option<u64>,
    /// A list got fresh content (view/tab switch, search results, playlist drill-down) →
    /// row cascade. Carries the mode whose list should cascade.
    pub list: Option<(u64, Mode)>,
    /// A popup/dropdown opened → fade-in materialize (applied in `seal_popup_background`).
    pub popup: Option<u64>,
    /// The current synced-lyric line advanced → flash on the newly-current line.
    pub lyric: Option<u64>,
    /// Play/pause toggled on a current track → light wave across the transport controls.
    pub pause: Option<u64>,

    // Last-observed values the central diff compares against (reducer-only) ----
    pub(in crate::app) last_track_id: Option<String>,
    pub(in crate::app) last_volume: i64,
    pub(in crate::app) last_liked: bool,
    pub(in crate::app) last_mode: Mode,
    /// The responsive tier the reducer last acted on (diffed against the render-side
    /// bridge to run one-shot mini-entry hygiene; see `App::sync_ui_tier`).
    pub(in crate::app) last_ui_tier: crate::ui::layout::UiTier,
    pub(in crate::app) last_library_tab: LibraryTab,
    pub(in crate::app) last_settings_tab: Option<SettingsTab>,
    pub(in crate::app) last_open_playlist: Option<String>,
    pub(in crate::app) last_searching: bool,
    /// Overlay-open bitmask (the art overlay mask's popup bits plus the two it doesn't
    /// track); a bit turning on means "a popup just opened".
    pub(in crate::app) last_popup_mask: u32,
    pub(in crate::app) last_lyric_index: Option<usize>,
    /// Diff anchor for the pause flash (`false`, matching a fresh `Playback`); the arm is
    /// additionally gated on a live `time_pos` so loader-driven flips stay quiet.
    pub(in crate::app) last_paused: bool,
}

impl FxState {
    /// Anchors seeded from launch state so the first frames don't replay a phantom "change"
    /// (e.g. the initial volume reading as a volume change).
    pub(in crate::app) fn new(volume: i64) -> Self {
        Self {
            until: 0,
            toast: None,
            track_intro: None,
            volume: None,
            like: None,
            seek: None,
            switch: None,
            tabbar: None,
            list: None,
            popup: None,
            lyric: None,
            pause: None,
            last_track_id: None,
            last_volume: volume,
            last_liked: false,
            last_mode: Mode::Player,
            last_ui_tier: crate::ui::layout::UiTier::default(),
            last_library_tab: LibraryTab::default(),
            last_settings_tab: None,
            last_open_playlist: None,
            last_searching: false,
            last_popup_mask: 0,
            last_lyric_index: None,
            last_paused: false,
        }
    }

    /// Drop every in-flight visual while retaining the reducer's change-detection anchors.
    pub(in crate::app) fn cancel(&mut self) {
        self.until = 0;
        self.toast = None;
        self.track_intro = None;
        self.volume = None;
        self.like = None;
        self.seek = None;
        self.switch = None;
        self.tabbar = None;
        self.list = None;
        self.popup = None;
        self.lyric = None;
        self.pause = None;
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
    pub proc: Option<crate::util::process_tree::OwnedProcessTree>,
    /// Whether opening the video overlay is what paused the audio, so closing it only resumes
    /// playback we paused (not audio the user had paused themselves).
    pub paused_audio: bool,
    /// Monotonic spawn counter. Overlay IPC events carry the generation they were connected
    /// for, so events from a window that was already closed (`v`) or respawned (`Shift+V`)
    /// are recognized as stale and ignored.
    pub generation: u64,
    /// The overlay's IPC endpoint, kept so closing can unlink the Unix socket file
    /// (Windows named pipes self-clean).
    pub ipc_path: Option<String>,
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
    /// Volume to restore on unmute. `Some(prev)` means muted (volume is held at 0); `None`
    /// means not muted. Cleared whenever the user changes volume directly, so a later unmute
    /// never restores a stale level.
    pub pre_mute_volume: Option<i64>,
    /// Playback speed multiplier (1.0 = normal).
    pub speed: f64,
    /// Current live-radio now-playing metadata, when the active stream exposes it.
    pub stream_now_playing: Option<StreamNowPlaying>,
    /// mpv `audio-codec-name` for the active stream (e.g. `mp3`, `aac`); the radio recorder
    /// maps it to the passthrough container extension.
    pub audio_codec: Option<String>,
    /// mpv `file-format` (container) for the active stream; fallback/HLS signal for the
    /// recorder's extension choice.
    pub file_format: Option<String>,
    /// mpv `demuxer-cache-time`: the newest demuxed timestamp. On a live radio stream this
    /// approximates the live edge, so `cache_time − time_pos` is how far behind live the
    /// playhead sits (the timeshift depth).
    pub cache_time: Option<f64>,
    /// When `cache_time` last updated. mpv stops updating it once the forward buffer
    /// saturates, so the live-sync verdict treats an old report as unknown rather than
    /// trusting it (see `App::radio_behind_secs`).
    pub cache_time_at: Option<Instant>,
}

/// Prefetch / load tracking: the pre-resolved stream-URL cache, whether the current track
/// loaded from that cache, and the `video_id` actually loaded into mpv.
#[derive(Default)]
pub struct Prefetch {
    /// Pre-resolved direct stream URLs, keyed by `video_id` (for instant skip).
    pub resolved: super::prefetch::PrefetchCache,
    /// Whether the current track was loaded from a prefetched direct URL (vs the watch
    /// URL mpv resolves itself). Recorded so a playback error can note the likelier cause
    /// (a stale prefetched CDN URL) in the log.
    pub last_load_prefetched: bool,
    /// `video_id` of the track actually loaded into mpv. A cached/restored queue entry can
    /// be visible before it is loaded; the first play action then loads it instead of only
    /// toggling mpv's pause property.
    pub loaded_video_id: Option<String>,
    /// Tracks whose prefetched direct URL already failed once and was retried via the watch URL.
    pub watch_retry_attempted: HashSet<String>,
    /// Timestamps of recent prefetched direct-URL playback failures. Session-only; used to dampen
    /// prefetch when YouTube/CDN URLs are being rejected repeatedly.
    pub recent_failures: VecDeque<Instant>,
    /// When set to a future instant, ordinary skip-ahead prefetch is paused. Self-heal resolves
    /// still run because they are not latency prefetches.
    pub disabled_until: Option<Instant>,
    /// One-shot, same-item stale-source recovery. The logical generation advances only for an
    /// ordinary admitted load; its replacement advances the file generation without rearming
    /// the per-item attempt latch.
    pub source_recovery: crate::player::recovery::RecoveryPlanner,
    pub source_logical_generation: u64,
    pub source_file_generation: u64,
}

/// Playback self-heal driven by extraction-shaped errors (the stale-yt-dlp signature):
/// update yt-dlp in the background, then retry the failed track exactly once via a
/// resolver-resolved direct URL (the session mpv keeps its spawn-time `ytdl_path`, so a
/// watch-URL reload through mpv would still use the old binary).
#[derive(Default)]
pub struct YtdlpHeal {
    /// The track a heal is in flight for; cleared when its retry lands or fails.
    pub pending_video_id: Option<String>,
    /// Tracks that already got their one retry this session — no retry loops.
    pub attempted: HashSet<String>,
    /// When the last heal-triggered update check ran ([`crate::tools::HEAL_COOLDOWN`]).
    pub last_check: Option<Instant>,
}

/// Download state, keyed by ordinary `video_id` or a stable import-session row key: the
/// in-flight/finished progress map shown in the UI, plus the original catalog metadata held while
/// a download runs.
#[derive(Default)]
pub struct Downloads {
    /// In-flight / finished downloads, keyed by download owner, for the UI indicator.
    pub active: HashMap<String, DownloadState>,
    /// Original catalog metadata for in-flight downloads, keyed by download owner. Reducer-only
    /// (was a private App field) — `pub(in crate::app)` keeps it off the render-facing surface.
    pub(in crate::app) sources: HashMap<String, Song>,
    /// Bulk-download overflow queue: deduped songs accepted for download but not yet handed to
    /// the actor. `pump_downloads` drains it as in-flight slots free, so a large playlist can't
    /// overrun the bounded command channel (`backpressure::DOWNLOAD_QUEUE`).
    pub(in crate::app) pending: std::collections::VecDeque<Song>,
    /// Downloads handed to the actor but not yet done/failed. Gates `pump_downloads` under the
    /// channel bound; decremented as each `DownloadDone`/`DownloadError` arrives.
    pub(in crate::app) dispatched: usize,
    /// Import rows admitted during the current runtime batch, grouped by session. The terminal
    /// reducer consumes a group only after that session has no queued or running work, then uses
    /// it to keep automatic organization from sweeping up unrelated, pre-existing inbox files.
    pub(in crate::app) auto_organize_rows: HashMap<String, HashSet<u32>>,
}

/// The Spotify playlist picker overlay (Settings › Accounts › Import from Spotify…):
/// "Liked Songs" first, then the account's playlists.
pub struct SpotifyPicker {
    pub items: Vec<crate::transfer::actor::PickerPlaylist>,
    pub selected: usize,
}

/// The radio-recording settings popup (opened from the Playback tab's one radio item). Edits
/// the live `SettingsDraft.recording_*` fields directly, so its values commit with the rest of
/// the draft when Settings closes.
#[derive(Default)]
pub struct RecordingSettingsPopup {
    /// Focused row: 0 mode · 1 min · 2 max · 3 folder · 4 past-tracks · 5 notify · 6 browse.
    pub row: usize,
    /// True while the output-folder text field is being typed into.
    pub editing_dir: bool,
    /// Caret within the output-folder draft while [`Self::editing_dir`] is active.
    pub dir_cursor: TextCursor,
    /// Screen rect of the popup, written each render so a click outside it can be detected
    /// (which closes it) and clicks inside can be hit-tested. `Cell` because render only has
    /// `&App`.
    pub rect: Cell<Option<Rect>>,
}

/// The recordings browser (Decide-mode save/discard/play over `recorder.history`).
#[derive(Default)]
pub struct RecordingsBrowser {
    pub selected: usize,
    /// Screen rect of the browser, written each render (same role as
    /// [`RecordingSettingsPopup::rect`]).
    pub rect: Cell<Option<Rect>>,
}

/// Row count of the recording-settings popup: mode · min · max · folder · past-tracks ·
/// notify · browse.
pub const RECORDING_POPUP_ROWS: usize = 7;

/// Lyrics-panel state: fetched lines plus session-only sync controls for the current track.
#[derive(Default)]
pub struct Lyrics {
    /// Whether the lyrics panel is shown in the player view.
    pub visible: bool,
    /// True between requesting lyrics and the result arriving.
    pub loading: bool,
    /// Lyrics for the current track, if fetched.
    pub track: Option<TrackLyrics>,
    /// Session-only timing adjustment. Positive values display lyrics later.
    pub delay: LyricDelay,
    /// Video id the timing adjustment belongs to, retained across same-track reloads.
    pub delay_video_id: Option<String>,
    /// Current line shared by the lyric renderer and lyric-transition animation.
    pub active_index: Option<usize>,
    /// A fresh non-empty payload is waiting for its first visible full-Player frame. Keeping this
    /// separate from the wall-clock deadline prevents an off-screen load from spending the OSD's
    /// three-second exposure before the user can see it.
    pub initial_osd_pending: bool,
    /// Wall-clock deadline while the expanded delay control is visible.
    pub delay_osd_until: Option<Instant>,
}

/// Search-screen state: the query, its results, selection, focus, and in-flight flag.
#[derive(Default)]
pub struct SearchState {
    /// The search query being typed.
    pub input: String,
    /// Caret within [`Self::input`], stored as a grapheme-safe UTF-8 byte offset.
    pub input_cursor: TextCursor,
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
    /// The fixed end of the results list's multi-select range (drag start / last single
    /// click). The selection is the inclusive span between this and `selected`,
    /// mirroring the Library list's drag-to-select.
    pub anchor: usize,
    /// Discontiguous multi-select rows toggled with Ctrl/Cmd+click. When non-empty it
    /// IS the effective selection (the anchor..=selected range is ignored); cleared by
    /// any plain click/nav/drag and whenever the result list changes. Consumers clamp
    /// stale indices, never panic.
    pub picked: BTreeSet<usize>,
    /// True between issuing a search request and its results arriving.
    pub searching: bool,
    /// Monotonic id of the most recently *submitted* search. Stamped on the request and echoed
    /// back on its results/error so a slow older response can't overwrite a newer search — the
    /// id is stable while the user keeps typing, unlike the live `input`.
    pub request_id: u64,
    /// Whether the box searches tracks or public YouTube playlists (session-scoped).
    pub kind: SearchKind,
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
    /// Production mutation generation for the wrapped-transcript render cache. Every reducer
    /// append (including history trimming) advances it exactly once.
    pub(crate) transcript_revision: u64,
    /// Stable owner identity prevents a thread-local cache entry from one `App` being reused by a
    /// different instance that happens to have the same revision and presentation settings.
    pub(crate) transcript_cache_token: Arc<()>,
    /// The DJ Gem prompt being typed.
    pub input: String,
    /// Caret within [`Self::input`], stored as a grapheme-safe UTF-8 byte offset.
    pub input_cursor: TextCursor,
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
    pub(in crate::app) resize_tx: Option<tokio::sync::mpsc::Sender<ResizeRequest>>,
    /// The decoded source image kept alongside the protocol for stale-result checks and future
    /// resize/protocol rebuilds. Reducer-only (was a private App field) — `pub(in crate::app)`.
    pub(in crate::app) source: Option<Arc<DynamicImage>>,
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
    pub(in crate::app) overlay_mask: u32,
    /// One-shot request for the render loop to call `Terminal::clear()` before the next frame.
    /// Kept in art state because the expensive clear is only needed to resync native terminal
    /// graphics after an overlay or screen transition has covered album art or the About icon.
    pub(in crate::app) force_clear_next_frame: bool,
    /// Extra clear/redraw frames after native album art is refreshed while an overlay is visible.
    /// Windows Terminal can composite the graphics payload one frame after the text overlay, so the
    /// overlay needs a short reinforcement burst rather than a single clear.
    pub(in crate::app) overlay_refresh_clear_frames: u8,
    /// Last layout-geometry key observed by the reducer (bar position, lyrics visibility) —
    /// the inputs that MOVE the art rect within the Player screen. A separate key rather
    /// than overlay-mask bits: the mask budget is nearly exhausted, and geometry is a
    /// different axis than occlusion. `None` until the first sync.
    pub(in crate::app) geometry_key: Option<(
        crate::config::PlayerBarPosition,
        bool,
        crate::ui::layout::UiTier,
    )>,
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
    /// An DJ Gem rerank handed off to the assistant actor, awaiting its `StreamingMsg::AiPicks`. Holds
    /// the shortlist (to validate the returned ids against) and the local pick (the fallback).
    pub pending_rerank: Option<PendingRerank>,
    /// Consecutive empty streaming extends, for the autoplay circuit breaker.
    pub consecutive_failures: u8,
    /// The last DJ Gem rerank's resolved explanation (picks → role + reasons + confidence), stashed
    /// when `StreamingMsg::AiPicks` resolves so the "Why DJ Gem" overlay (`w`) can show why these tracks
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
    /// `AiMsg::StationPatch`. A single-flight guard so a skip streak can't fan out duplicate calls.
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
    /// Discontiguous multi-select rows toggled with Ctrl/Cmd+click. When non-empty it
    /// IS the effective selection (the anchor..=selected range is ignored); cleared by
    /// any plain click/nav/drag and by every selection-aware mutation (delete, clamp,
    /// filter/tab change). Like `anchor`, it can drift if the list shifts underneath
    /// (e.g. history growing while off-screen) — consumers clamp, never panic.
    pub picked: BTreeSet<usize>,
    /// Local audio files found in the configured download directory.
    pub downloaded: Vec<Song>,
    /// Mutation counter for `downloaded` (row cache / id-recovery memo key). Bumped at
    /// every prod mutation site; a rescan can swap same-length different-content lists,
    /// which is why length alone can't key the caches.
    pub downloaded_rev: u64,
    /// Pending "delete downloaded files" confirmation: the on-disk paths queued for deletion
    /// (file removal is irreversible, so it's gated behind an explicit yes/no). `None` when no
    /// modal is open.
    pub confirm_delete: Option<Vec<PathBuf>>,
    /// In-library incremental filter query (`/`). When non-empty, the active list narrows to
    /// rows whose title or artist contains it (case-insensitive). Empty = no filter.
    pub filter_query: String,
    /// Caret within [`Self::filter_query`] while the live filter is focused.
    pub filter_cursor: TextCursor,
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
    /// Caret within the create-playlist name buffer.
    pub create_cursor: TextCursor,
    /// Pending "delete playlist" confirmation: the id of the playlist queued for deletion
    /// (removes the whole list at once, so it's gated behind an explicit yes/no like the
    /// download-file delete). `None` when no modal is open.
    pub confirm_playlist_delete: Option<String>,
    /// Pending bulk-download confirmation: the already-deduped batch queued behind a
    /// "Download N songs?" yes/no (drag-selected range, or a whole list/playlist). `None`
    /// when no modal is open. Shares the album-art overlay bit with `confirm_delete` — the
    /// two Library confirm modals are mutually exclusive.
    pub confirm_download: Option<Vec<Song>>,
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
    /// Caret within the inline new-playlist name buffer.
    pub naming_cursor: TextCursor,
}

/// The search results-filter popup ("추가 창"): a transient fzf-style window over the
/// Search view that narrows the current results as you type. Fresh (empty query) on each
/// open — it is a picker, not a persistent filter like the Library's. Grouping the
/// `Cell`/scroll bridges here keeps them next to the overlay state they belong to,
/// mirroring [`QueuePopup`].
#[derive(Default)]
pub struct SearchFilterPopup {
    /// Whether the popup is showing. Search-only overlay; while open it captures the
    /// keyboard (typed chars edit `query`, arrows move `cursor`, Enter plays, Esc closes).
    pub open: bool,
    /// The live filter text; the popup's row list narrows to matches as it changes.
    pub query: String,
    /// Caret within [`Self::query`]; distinct from the highlighted result-row cursor.
    pub input_cursor: TextCursor,
    /// The highlighted row, as a *display* index into [`Self::matches`].
    pub cursor: usize,
    /// Cached original-`results` indices of the rows matching `query`, in results order.
    /// Recomputed only when the query (or the result set on open) changes — while the popup
    /// is open nothing else mutates `results` (a fresh search closes it) — so the render,
    /// nav, and hit-test paths read it in O(1) instead of re-filtering every frame/event.
    pub(in crate::app) matches: Vec<usize>,
    /// Screen rect of the open popup, written each render so a click outside it can be
    /// detected (which closes it). `Cell` because render only has `&App`.
    pub rect: Cell<Option<Rect>>,
    /// Decoupled wheel-scroll offset for the popup's list (see [`crate::ui::scroll`]).
    pub scroll: crate::ui::scroll::ScrollState,
}

impl SearchFilterPopup {
    /// Reset to a fresh, open popup (empty query, cursor at the top). The caller refreshes
    /// [`Self::matches`] afterwards (it needs `App` context to filter).
    pub(in crate::app) fn open_fresh(&mut self) {
        self.open = true;
        self.query.clear();
        self.input_cursor = TextCursor::default();
        self.cursor = 0;
        self.matches.clear();
        self.scroll.reset();
    }

    /// Close and drop the transient state so a later open starts fresh.
    pub(in crate::app) fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.input_cursor = TextCursor::default();
        self.cursor = 0;
        self.matches.clear();
        self.rect.set(None);
        self.scroll.reset();
    }
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

/// Row indices of a list's effective multi-selection, ascending and bounded to `len`:
/// the Ctrl/Cmd-picked set when non-empty, else the inclusive `anchor..=selected` range.
/// Shared by the Library and Search lists so both resolve selection identically.
pub(in crate::app) fn effective_selection_indices(
    picked: &std::collections::BTreeSet<usize>,
    selected: usize,
    anchor: usize,
    len: usize,
) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    if !picked.is_empty() {
        // BTreeSet iterates ascending; drop indices a stale set might hold past the end.
        return picked.iter().copied().filter(|&i| i < len).collect();
    }
    let lo = selected.min(anchor);
    if lo >= len {
        return Vec::new();
    }
    let hi = selected.max(anchor).min(len - 1);
    (lo..=hi).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragSurface {
    Queue,
    Library,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DragSelection {
    pub surface: DragSurface,
    pub anchor: usize,
}

/// A multi-selection collapsed by the first press of a possible double-click. Plain clicks
/// stay collapsed; only the translator's matching second press may restore these rows before
/// activation. Keying the snapshot by surface and row prevents a later click elsewhere from
/// reviving an unrelated selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingDoubleClickSelection {
    pub surface: DragSurface,
    pub row: usize,
    pub indices: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AiTranscriptDrag {
    pub anchor: usize,
    pub cursor: usize,
    pub moved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScrollbarDrag {
    pub surface: ScrollSurface,
    pub rect: Rect,
    pub content_len: usize,
    pub viewport: usize,
    pub grab: u16,
    /// Present only for Local Find. Generic surfaces have stable list ownership rules; Local
    /// Find can replace the row set asynchronously while the mouse button remains held.
    pub local_find_stamp: Option<LocalFindPointerStamp>,
}

/// Live pointer-interaction sessions: the in-flight drag/scrub selections and the held-key
/// navigation accelerator. All transient — each is cleared on button release (or a gap, for
/// nav) — so grouping them keeps the mouse reducer's working set in one place.
#[derive(Default)]
pub struct Interaction {
    /// A left press that dismissed/activated the context menu owns the entire gesture until
    /// button-up, so a following drag cannot leak through to the covered list.
    pub(in crate::app) context_menu_press: bool,
    /// Coordinates of a context-menu left press. Kept across its button-up so the translator's
    /// paired `MouseDoubleClick` can be swallowed instead of leaking into a modal opened by the
    /// first press; the next ordinary single click resets it.
    pub(in crate::app) context_menu_click: Option<(u16, u16)>,
    /// Coordinates of a picker-opening or picker-owned press, retained until a paired
    /// double-click arrives so the second press cannot be reinterpreted by a newly opened popup
    /// or click through after the first press closes it.
    pub(in crate::app) color_picker_click: Option<(u16, u16)>,
    /// Multi-selection hidden by the latest plain Search/Library row press while the input
    /// translator waits to learn whether that press is the first half of a double-click.
    pub(in crate::app) pending_double_click_selection: Option<PendingDoubleClickSelection>,
    /// Active mouse drag-selection session. Cleared on left-button release so a later
    /// drag starts from its own first row, not whatever was selected before.
    pub(in crate::app) drag_selection: Option<DragSelection>,
    /// Active scrollbar drag session. Kept separate from row range selection so dragging a
    /// scrollbar never extends a Library/Queue multi-select range.
    pub(in crate::app) drag_scrollbar: Option<ScrollbarDrag>,
    /// Active DJ Gem transcript drag-copy selection. Stores rendered visual row indexes,
    /// not message indexes, so wrapping and copy behavior line up exactly. `pub(crate)` (not
    /// `pub`) to match [`AiTranscriptDrag`]'s visibility — still reachable from the `ui` render.
    pub(crate) ai_transcript_drag: Option<AiTranscriptDrag>,
    /// Preview-only seekbar scrub. Authoritative playback state changes only after its single
    /// release command is admitted by the player lane.
    pub(in crate::app) seekbar_scrub: Option<SeekbarScrub>,
    /// Active radio-recording slider drag: the focused slider row (1 min · 2 max · 4 keep) and
    /// the track rect captured at press, so pointer-x maps to a value even after the pointer
    /// leaves the track. `None` = not dragging; cleared on the next press like the seekbar scrub.
    pub(in crate::app) recording_drag: Option<(usize, Rect)>,
    /// Held-key auto-repeat accelerator for list navigation (see [`NavRepeat`]). Idle at rest.
    pub(in crate::app) nav_repeat: NavRepeat,
}

#[derive(Clone)]
pub(in crate::app) struct SeekbarScrub {
    pub(in crate::app) queue_revision: u64,
    pub(in crate::app) track_id: Option<String>,
    pub(in crate::app) position_epoch: u64,
    pub(in crate::app) duration: f64,
    pub(in crate::app) area: Rect,
    pub(in crate::app) column: u16,
    pub(in crate::app) target: f64,
    pub(in crate::app) awaiting_admission: bool,
}

/// Dedicated-Radio-mode stash: the normal- and radio-mode themes and queues that swap in and
/// out as the user enters/leaves Radio mode, plus the pending enter/leave confirmation. The
/// active `radio_dedicated_mode` flag itself stays flat on [`App`] — it gates behavior in many
/// places and reads as a mode, not stashed state.
#[derive(Default)]
pub struct RadioMode {
    /// The normal-mode theme to restore after leaving dedicated Radio mode.
    pub(in crate::app) normal_mode_theme: Option<ThemeConfig>,
    /// The Radio-mode theme to restore on the next dedicated Radio entry. Defaults to Radio
    /// until the user edits the theme while Radio mode is active.
    pub(in crate::app) radio_mode_theme: Option<ThemeConfig>,
    /// The normal-mode queue to restore when leaving dedicated Radio mode.
    pub(in crate::app) normal_mode_queue: Option<QueueSnapshot>,
    /// The Radio-mode queue to restore when entering dedicated Radio mode again.
    pub(in crate::app) radio_mode_queue: Option<QueueSnapshot>,
    /// A pending confirmation before entering or leaving dedicated Radio mode.
    pub pending_radio_mode_confirm: Option<RadioModeConfirm>,
}

/// View state for the Library-owned Local Deck shell.
#[derive(Default)]
pub struct LocalUi {
    /// Which Local Deck section is shown.
    pub section: LocalSection,
    /// Which Local Deck pane has keyboard focus.
    pub pane: LocalPane,
    /// The highlighted row in the current Local Deck list.
    pub selected: usize,
    /// Fixed end of a future multi-select range. Kept from the first shell so row
    /// selection semantics can follow Library/Queue when range select lands.
    pub anchor: usize,
    /// Active drill-down path for section rows such as album -> tracks.
    pub drill: Vec<LocalDrill>,
    /// Current Local Deck live-filter query.
    pub filter_query: String,
    /// Caret within [`Self::filter_query`] while the live filter is focused.
    pub filter_cursor: TextCursor,
    /// Whether typed keys edit `filter_query`.
    pub filter_editing: bool,
}

/// Loaded Local Deck index plus transient scan/load status.
#[derive(Default)]
pub struct LocalIndexRuntime {
    pub index: crate::local::LocalIndex,
    /// Monotonic generation advanced only when a loaded/scanned index is committed.
    pub revision: u64,
    pub index_path: Option<PathBuf>,
    pub loaded: bool,
    pub loading: bool,
    pub scanning: bool,
    /// A scan requested while another scan owns the worker. `Some(true)` upgrades the queued
    /// follow-up to a full rebuild; otherwise one incremental pass runs after settlement.
    pub(in crate::app) pending_rescan: Option<bool>,
    pub progress: Option<crate::local::LocalScanProgress>,
    pub last_summary: Option<crate::local::LocalScanSummary>,
    pub load_errors: Vec<crate::local::ScanError>,
    pub errors: Vec<crate::local::ScanError>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LocalFindFocus {
    #[default]
    Input,
    Results,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindDrill {
    pub title: String,
    pub source: crate::local::find::LocalFindHitId,
    pub track_ids: Vec<crate::local::LocalTrackId>,
    pub corpus_revision: crate::local::find::LocalFindCorpusRevision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalFindBulkAction {
    Play,
    Enqueue,
    ShufflePlay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindBulkConfirm {
    pub action: LocalFindBulkAction,
    pub track_ids: Vec<crate::local::LocalTrackId>,
    pub accepted_count: usize,
    pub result_generation: u64,
    pub corpus_revision: crate::local::find::LocalFindCorpusRevision,
    pub queue_revision: u64,
    pub capacity_recalculated: bool,
}

/// Transactional scope/sort editor. Draft values do not affect the visible result until Apply.
pub struct LocalFindRefine {
    pub open: bool,
    pub row: usize,
    pub draft_scope: crate::local::find::LocalFindScope,
    pub draft_sort: crate::local::find::LocalFindSort,
    pub rect: Cell<Option<Rect>>,
    pub help_scroll: crate::ui::scroll::ScrollState,
}

impl Default for LocalFindRefine {
    fn default() -> Self {
        Self {
            open: false,
            row: 0,
            draft_scope: crate::local::find::LocalFindScope::All,
            draft_sort: crate::local::find::LocalFindSort::Relevance,
            rect: Cell::new(None),
            help_scroll: crate::ui::scroll::ScrollState::default(),
        }
    }
}

/// Local-only search state. No field is shared with the online Search reducer, which prevents
/// a Local query/source/result from leaking into normal or dedicated Radio mode.
pub struct LocalFindState {
    pub query: String,
    pub select_all: bool,
    pub focus: LocalFindFocus,
    pub scope: crate::local::find::LocalFindScope,
    pub sort: crate::local::find::LocalFindSort,
    pub selected: usize,
    pub return_section: LocalSection,
    pub parse_error: Option<String>,
    pub searching: bool,
    /// An explicit Enter/Find-button commit selects the first row when its current result lands;
    /// live edits leave this false so stable-ID selection retention remains active.
    pub reset_selection_on_result: bool,
    pub request_id: u64,
    pub corpus_generation: u64,
    pub(in crate::app) building_revision: Option<crate::local::find::LocalFindCorpusRevision>,
    pub(in crate::app) corpus: Option<Arc<crate::local::find::LocalFindCorpus>>,
    pub snapshot: Option<crate::local::find::LocalFindSnapshot>,
    pub drill: Option<LocalFindDrill>,
    pub refine_popup: LocalFindRefine,
    pub pending_bulk_confirm: Option<LocalFindBulkConfirm>,
    /// Full index rebuild discards incremental reuse, so the typed command requires an explicit
    /// confirmation before admitting scanner work.
    pub pending_rebuild_confirm: bool,
}

impl Default for LocalFindState {
    fn default() -> Self {
        Self {
            query: String::new(),
            select_all: false,
            focus: LocalFindFocus::Input,
            scope: crate::local::find::LocalFindScope::All,
            sort: crate::local::find::LocalFindSort::Relevance,
            selected: 0,
            return_section: LocalSection::Home,
            parse_error: None,
            searching: false,
            reset_selection_on_result: false,
            request_id: 0,
            corpus_generation: 0,
            building_revision: None,
            corpus: None,
            snapshot: None,
            drill: None,
            refine_popup: LocalFindRefine::default(),
            pending_bulk_confirm: None,
            pending_rebuild_confirm: false,
        }
    }
}

impl LocalFindState {
    /// Clear only the Local Find interaction session after an admitted Local exit. The immutable
    /// corpus remains a reusable cache for the unchanged index/playlists, while advancing both
    /// worker tokens makes any corpus build or query result already in flight stale.
    pub(in crate::app) fn clear_transient_for_exit(&mut self) {
        let corpus = self.corpus.take();
        let request_id = self.request_id.wrapping_add(1).max(1);
        let corpus_generation = self.corpus_generation.wrapping_add(1).max(1);
        *self = Self::default();
        self.request_id = request_id;
        self.corpus_generation = corpus_generation;
        self.corpus = corpus;
    }
}

/// One immutable view of the Local Deck rows used by a render pass. The backing row slice and
/// its compact derived metadata are shared with the single-entry cache, so the header, body, and
/// details panes never rebuild (or independently clone) the same row set within a frame.
#[derive(Clone)]
pub(crate) struct LocalRowsSnapshot {
    pub(in crate::app) data: std::rc::Rc<super::local::LocalRowsData>,
    pub(in crate::app) total_len: usize,
}

impl LocalRowsSnapshot {
    pub(crate) fn rows(&self) -> &[crate::local::LocalRowId] {
        self.data.rows.as_ref()
    }

    pub(crate) fn total_len(&self) -> usize {
        self.total_len
    }
}

/// Dedicated Local Deck state. The active `local_dedicated_mode` flag stays flat on
/// [`App`], mirroring Radio mode, while this struct owns shell-local UI state and the
/// pending enter/leave confirmation.
#[derive(Default)]
pub struct LocalMode {
    pub ui: LocalUi,
    pub index: LocalIndexRuntime,
    pub find: LocalFindState,
    /// The normal-mode theme to restore after leaving Local Deck.
    pub(in crate::app) normal_mode_theme: Option<ThemeConfig>,
    /// The Local-Deck theme to restore on the next entry. Falls back to Local Launch until the
    /// user saves a Local theme.
    pub(in crate::app) local_mode_theme: Option<ThemeConfig>,
    /// Explicit production mutation generation for the Local Deck derived-row cache.
    pub(in crate::app) rows_revision: Cell<u64>,
    /// A single entry is enough: only the active section/query/drill path is rendered, and
    /// replacing it promptly releases potentially large row/id and derived-metadata arrays.
    pub(in crate::app) rows_cache: RefCell<Option<super::local::LocalRowsCache>>,
    /// Recognized import-artifact paths and metadata used to invalidate the row cache without
    /// opening persisted JSON or rescanning unchanged directories from the render path.
    pub(in crate::app) import_files_fingerprint_cache:
        RefCell<super::local::LocalImportFilesFingerprintCache>,
    pub(in crate::app) normal_mode_queue: Option<QueueSnapshot>,
    pub(in crate::app) local_mode_queue: Option<QueueSnapshot>,
    pub pending_confirm: Option<LocalModeConfirm>,
    /// Monotonic namespace shared by Local confirmation and player-intent tokens. Tokens make a
    /// cancelled confirmation distinct from a later confirmation with the same `Enter`/`Exit`
    /// enum value, closing the deferred-admission ABA window.
    pub(in crate::app) next_transition_token: u64,
    /// Identity of the confirmation currently painted to the user. Import-search continuations
    /// bind to this token so an older deferred exit can never consume a newer confirmation.
    pub(in crate::app) pending_confirm_token: Option<u64>,
    /// Identity of the one player intent currently claiming the confirmation. A second Enter is
    /// ignored until this intent commits or rejects, so its rejection cannot steal the first
    /// intent's single-use continuation.
    pub(in crate::app) pending_intent_token: Option<u64>,
    pub pending_organize_confirm: Option<LocalOrganizeConfirm>,
    pub pending_accept_all_confirm: Option<LocalImportAcceptAllConfirm>,
    /// Import-history artifact selected for confirmed deletion. Imported songs are retained.
    pub pending_import_record_delete: Option<String>,
    pub(in crate::app) pending_import_reviews: HashMap<String, u64>,
    pub(in crate::app) next_import_review_op_id: u64,
    /// Confirmed manual online-search handoff, consumed only after a successful Local exit.
    pub(in crate::app) pending_import_search: Option<LocalImportSearchContinuation>,
}

/// Animation clock and redraw-coalescing counters: the monotonic frame counter that drives every
/// effect's phase, the fractional draw-credit scheduler and its last cadence, and the last whole
/// second / cache second rendered (so sub-second mpv position spam is coalesced). The one-shot
/// [`FxState`] feedback and the `focused` gate live separately on [`App`].
pub struct Animation {
    /// Monotonic animation frame counter, bumped on each [`Msg::AnimTick`] (~30 fps) while
    /// animations are active. Drives every effect's phase; wraps harmlessly. `0` at rest.
    pub(in crate::app) anim_frame: u64,
    /// Fractional redraw scheduler for animation frames. The phase can advance at the configured
    /// FPS while heavyweight effects redraw at a lower cadence, preserving motion timing without
    /// forcing the terminal compositor to repaint every logical tick.
    pub(in crate::app) anim_draw_credit: u16,
    /// Last draw cadence used to interpret [`Self::anim_draw_credit`]. Reset when the active effect
    /// mix moves between cheap element effects, canvas effects, and the DJ Gem mascot.
    pub(in crate::app) anim_last_draw_fps: u16,
    /// Last whole second we redrew for, so sub-second `time-pos` spam is coalesced.
    pub(in crate::app) last_shown_sec: i64,
    /// Same coalescing for `demuxer-cache-time` (`-1` = none shown yet).
    pub(in crate::app) last_shown_cache_sec: i64,
}

impl Default for Animation {
    fn default() -> Self {
        // Matches the historical flat-field init in `App::new`: the frame/credit counters start
        // at 0, but the last-shown seconds start at -1 ("nothing shown yet"), so this can't derive.
        Self {
            anim_frame: 0,
            anim_draw_credit: 0,
            anim_last_draw_fps: 0,
            last_shown_sec: -1,
            last_shown_cache_sec: -1,
        }
    }
}

/// The transient modal/overlay layer: which cheat-sheets and cards are open, the pending
/// confirmations, the popup payloads (Spotify picker, recording settings/browser), the
/// background update-check result, and the "what's playing" identify overlay with its cache and
/// epoch. Grouping them keeps the overlay-visibility surface — which render and the modal
/// dismissal paths both read — in one place. The `transfer_running` job guard stays on [`App`].
#[derive(Default)]
pub struct Overlays {
    /// Row-scoped TUI context menu opened by right click or the keyboard fallback.
    pub context_menu: Option<ContextMenuState>,
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
    /// Hotplug-aware local audio-output chooser opened from Settings › Playback.
    pub audio_output_picker: Option<AudioOutputPicker>,
    /// The radio-recording settings popup (over the Playback settings tab).
    pub recording_settings: Option<RecordingSettingsPopup>,
    /// The recordings browser (Decide-mode save/discard/play), opened from the popup or a key.
    pub recordings_browser: Option<RecordingsBrowser>,
    /// Whether the About card overlay is showing. Opened by clicking the `yututui` brand in the
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
    pub(in crate::app) now_playing_cache: super::now_playing::NowPlayingCache,
    /// Identify epoch: replies must carry the open overlay's snapshot of this counter or
    /// they're stale (overlay closed / stream title moved on).
    pub(in crate::app) now_playing_seq: u64,
}
