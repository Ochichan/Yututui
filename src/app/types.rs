//! Message, command, and view-state type definitions for the app reducer.
//!
//! Split out of the former monolithic `app.rs` (behaviour-preserving move). These are
//! re-exported from `crate::app` (`pub use types::*`) so existing `crate::app::Msg` /
//! `crate::app::Cmd` / `crate::app::Mode` paths keep resolving for actors and views.

use super::*;

/// Pending owner-mediated transfer commit. The candidate is intentionally not installed into
/// live App state until this exact persistence generation is confirmed.
pub struct TransferPlaylistCommit {
    pub(crate) request: crate::transfer::actor::LocalPlaylistRequest,
    pub(crate) owner_base_revision: u64,
    pub(crate) candidate: crate::playlists::Playlists,
    pub(crate) kind: TransferPlaylistCommitKind,
}

pub(crate) enum TransferPlaylistCommitKind {
    Apply {
        patch: crate::transfer::local_playlist::LocalPlaylistPatch,
        outcome: crate::transfer::local_playlist::LocalPlaylistWriteOutcome,
    },
    /// Reassert the latest live owner snapshot after a stale transfer candidate reached disk but
    /// could no longer be safely rebased. The original error is replied only after exact restore.
    RestoreThenFail {
        error: crate::transfer::local_playlist::LocalPlaylistStoreError,
        retry_attempt: u8,
    },
}

/// Opaque owner-lane result keeps internal persistence identities out of the public `Msg` shape.
pub struct TransferPlaylistPersistence {
    pub(crate) commit: Box<TransferPlaylistCommit>,
    pub(crate) persistence: crate::persist::TargetFlushOutcome,
}

pub enum DownloadMsg {
    Progress {
        video_id: String,
        percent: f64,
    },
    ImportProgress {
        context: crate::download::ImportDownloadContext,
        percent: f64,
    },
    Done {
        video_id: String,
        path: String,
    },
    ImportDone {
        context: crate::download::ImportDownloadContext,
        path: String,
    },
    Error {
        video_id: String,
        error: String,
    },
    ImportError {
        context: crate::download::ImportDownloadContext,
        error: String,
    },
    Rejected {
        tracking_key: String,
        error: String,
    },
    DirError {
        error: String,
    },
}

/// Blocking download-domain effects share one top-level command bucket.
pub enum DownloadCmd {
    /// Download a track to disk (best audio + tags + cover art).
    Start(Box<Song>),
    /// Refresh the local downloads list from this folder.
    Scan(PathBuf),
    /// Point the download actor at a new folder for future downloads.
    SetDir(PathBuf),
    /// Delete confirmed files off the owner thread after runtime mutation admission.
    Delete { paths: Vec<PathBuf>, root: PathBuf },
}

/// Everything that can change the application state.
pub enum Msg {
    /// Inert. Runtime events with no standalone-TUI meaning (e.g. daemon-only
    /// GUI-session answers) still need a total `RuntimeEvent → Msg` mapping.
    Noop,
    Key(KeyEvent),
    /// A left-click at a terminal cell (1-based crossterm coords); may hit the seekbar.
    /// `multi` is set when the multi-select modifier was held (Ctrl, or Cmd on macOS):
    /// on a Library/Search list row that toggles the row in/out of the selection instead
    /// of re-anchoring it; everywhere else the click behaves as plain. The input
    /// translator never chains a modifier click into a double-click.
    MouseClick {
        col: u16,
        row: u16,
        multi: bool,
    },
    /// A left double-click at a cell — plays a song row / queue entry (vs. single-click,
    /// which selects). Falls back to single-click behavior off a list row.
    MouseDoubleClick {
        col: u16,
        row: u16,
    },
    /// A right-click at a cell. Supported semantic rows resolve their configured safe mouse
    /// action (the default opens the in-TUI context menu); ignored elsewhere.
    MouseRightClick {
        col: u16,
        row: u16,
    },
    /// A right double-click at a cell, inferred by the input translator from two presses at
    /// the same zoom-adjusted virtual cell within the double-click window.
    MouseRightDoubleClick {
        col: u16,
        row: u16,
    },
    /// The pointer was dragged with the left button held — extends the queue window's
    /// multi-select range. Ignored outside that window.
    MouseDrag {
        col: u16,
        row: u16,
    },
    /// The left mouse button was released. Used to end the current drag-selection
    /// session so a later drag starts a fresh range instead of extending stale state.
    MouseLeftUp,
    /// The mouse wheel was scrolled at a terminal cell. `up` means toward earlier items
    /// for lists, and volume-up over the player volume cluster. With `ctrl` held the
    /// wheel steps the text zoom instead (browser-style).
    MouseScroll {
        up: bool,
        col: u16,
        row: u16,
        ctrl: bool,
    },
    /// The terminal was resized; ratatui auto-resizes on draw, we just redraw.
    Resize,
    /// A real terminal resize with its zoom-adjusted logical grid. Unlike the test/internal
    /// redraw hint above, this lets the reducer leave Mini synchronously before the next frame.
    TerminalResize {
        width: u16,
        height: u16,
    },
    /// Terminal focus changed (DECSET ?1004). `false` while the window is unfocused
    /// (minimized / behind another window); the main loop then parks the ~30 fps animation
    /// tick (see [`App::animation_active`]). Unsupported terminals never send this, so the
    /// app stays at its `focused: true` default and behaves exactly as before.
    Focus(bool),
    /// A termination signal asked us to shut down.
    Quit,
    /// Startup-only: begin playing the restored last track (sent once at launch when the
    /// "autoplay on launch" setting is on). A no-op otherwise.
    Autoplay,
    /// The API actor finished startup authentication and selected its live mode.
    ApiModeResolved {
        mode: crate::api::ApiMode,
        had_cookie: bool,
    },
    /// Periodic wake-up while transient status or lyric-sync OSD state is showing. Lets the
    /// reducer expire the status after [`STATUS_TTL`] and collapse the OSD after three seconds.
    StatusTick,
    /// 100 ms synced-lyrics clock. The runtime arms it only while the full Player lyric panel is
    /// visible and actively playing; the reducer redraws only when the stored active row changes.
    LyricsTick,
    /// Animation frame tick (~30 fps), driven by the main loop **only** while
    /// [`App::animation_active`] holds — i.e. on the player view, master on, a track playing,
    /// and at least one effect enabled. Advances `anim_frame` and forces a redraw. When all
    /// animation toggles are off the main loop never arms this, so it costs literally nothing.
    AnimTick,
    /// A player/playback runtime message — mpv property changes (position, duration, pause,
    /// volume, metadata, cache-time, codec, format), EOF, playback errors, and video-overlay
    /// IPC events. See [`PlayerMsg`].
    Player(PlayerMsg),
    /// 1 Hz tick while a radio recording is in progress; drives the max-duration force-split.
    RecordingTick,
    /// A radio recorder disk job finished (a track was saved, or saving failed).
    Recorder(crate::recorder::job::RecorderEvent),
    /// Search returned results (possibly empty) for `query`.
    SearchResults {
        request_id: u64,
        query: String,
        source: SearchSource,
        songs: Vec<Song>,
        /// The multi-source operation deadline dropped one or more sources (partial results).
        timed_out: bool,
    },
    /// Search failed.
    SearchError {
        request_id: u64,
        source: SearchSource,
        error: String,
    },
    /// A remote playlist's tracks arrived (answering [`Cmd::FetchPlaylistTracks`]).
    PlaylistTracks {
        title: String,
        intent: crate::api::PlaylistIntent,
        songs: Vec<Song>,
    },
    /// Fetching a remote playlist's tracks failed.
    PlaylistTracksError {
        title: String,
        error: String,
    },
    /// Local-data work completed: a download scan or portable personal-data export.
    Data(DataMsg),
    /// Local Deck index load/scan result.
    Local(LocalMsg),
    /// Synced lyrics for `video_id` (empty `lines` = none found).
    LyricsResult {
        video_id: String,
        lines: std::sync::Arc<[LyricLine]>,
    },
    /// Decoded album art / thumbnail for `video_id` (`None` = none found / fetch failed).
    ArtworkResult {
        video_id: String,
        quality: Option<crate::config::AlbumArtQuality>,
        image: Option<DynamicImage>,
    },
    /// Album-art protocol resize/encode finished off the UI thread.
    ArtworkResized(ResizeResponse),
    /// Download actor progress and terminal results, including stable import-row correlation.
    Download(DownloadMsg),
    /// Runtime completion of a confirmed, path-validated downloaded-file deletion batch.
    DownloadsDeleted {
        root: PathBuf,
        deleted: Vec<PathBuf>,
        failed: usize,
    },
    /// A background persistence write failed and remains queued for retry.
    PersistFailed {
        store: crate::persist::StoreKind,
        error: String,
    },
    /// A streaming/autoplay pipeline message — a prefetched/resolved direct URL, related-track
    /// candidates, the metadata-preflighted picks, a fallback failure, or the DJ Gem reranker's
    /// chosen picks. See [`StreamingMsg`].
    Streaming(StreamingMsg),

    // DJ Gem assistant: intents emitted by the DJ Gem actor, applied here by `update()`.
    /// A DJ Gem assistant intent or off-path result — thinking, chat, errors, play/enqueue,
    /// suggestions, autoplay, station-profile shaping, playlist mutations, the feedback-summary
    /// station patch, and batch romanized titles. See [`AiMsg`].
    Ai(AiMsg),
    /// A command from a one-shot or persistent remote client. Applied through the same reducer
    /// path as a keypress (see [`App::apply_remote`]) so it is independent of the current input
    /// mode. For v8 sessions, sending this reply directly enqueues it before the run loop's
    /// same-turn publisher observation.
    Remote(
        crate::remote::proto::RemoteCommand,
        crate::remote::server::RemoteReply,
    ),
    /// A command from the OS media session (media keys, Now Playing / SMTC / MPRIS
    /// surfaces, Bluetooth AVRCP). Applied through the same reducer paths as a keypress
    /// (see [`App::apply_media`]), so it is independent of the current input mode.
    Media(crate::media::MediaCommand),
    /// The media-session artwork cache resolved a local file for a track. Stored on the
    /// app so the next media snapshot carries it (the OS artwork then refreshes).
    MediaArtworkReady(crate::media::artwork::MediaArtworkReady),
    /// Best-match tracks answering [`Cmd::ResolveTrack`] (possibly empty). Dropped
    /// unless `seq` matches the overlay's pending resolve epoch.
    TrackResolved {
        seq: u64,
        result: Result<Vec<Song>, String>,
    },
    /// An event from the scrobble actor: auth-flow progress or a service-health notice.
    /// Scrobbling itself is fire-and-forget and never surfaces here.
    Scrobble(crate::scrobble::ScrobbleEvent),
    /// Managed yt-dlp maintenance: download progress / installed / failed. Progress and
    /// success are informational; a failure is an error only when no usable yt-dlp
    /// exists at all (a failed background refresh of a working setup stays log-only).
    Tools(crate::tools::ToolsEvent),
    /// The background app-update check finished: whether a newer YuTuTui! release exists,
    /// how this build was installed, and whether this is the first sighting (toast gate).
    UpdateChecked(crate::update::UpdateStatus),
    /// A [`Cmd::YtdlpSelfHeal`] update check finished. `updated` means a new binary was
    /// installed (a retry is worth it); anything else falls through to the skip path.
    YtdlpHealResult {
        video_id: String,
        updated: bool,
    },
    /// The prefetch resolver failed to resolve `video_id`. Only meaningful while a
    /// self-heal retry is pending for that track (normal prefetch failures just mean
    /// a slower skip later and stay log-only).
    ResolveFailed {
        video_id: String,
    },
    /// An event from the transfer actor: Spotify auth, playlist listings, job progress.
    Transfer(crate::transfer::actor::TransferEvent),
}

/// Results from local-data workers that are applied on the owner lane.
pub enum DataMsg {
    /// Download folder scan completed.
    DownloadsScanned(crate::library::DownloadScan),
    /// A portable personal-data export worker event.
    PersonalDataExport(PersonalDataExportMsg),
    /// Targeted persistence confirmation for an owner-mediated transfer playlist patch.
    TransferPlaylistPersisted(TransferPlaylistPersistence),
}

/// Events produced by the portable personal-data export worker.
pub enum PersonalDataExportMsg {
    /// The off-loop writer completed.
    Finished {
        result: Result<PathBuf, String>,
        /// Kept on the owner-lane completion event so busy state is cleared before a remote
        /// success/failure becomes observable and an immediate retry can arrive.
        reply: Option<crate::remote::RemoteReply>,
    },
}

/// Small owner-lane guard shared by Settings and authenticated remote exports.
#[derive(Debug, Default)]
pub(crate) struct PersonalDataExportState {
    pub(crate) in_progress: bool,
}

/// Owned source state for a personal-data export. This deliberately carries the original
/// secret-bearing types only as far as the blocking worker; projection there produces the
/// allowlisted [`crate::data_export::ExportSnapshot`] before anything is written.
pub struct PersonalDataExportSources {
    pub(crate) config: Config,
    pub(crate) library: Library,
    pub(crate) playlists: Playlists,
    pub(crate) signals: Signals,
    pub(crate) station: StationStore,
}

/// Local-data effects dispatched by the runtime.
pub enum DataCmd {
    /// Refresh the local downloads list from this folder.
    ScanDownloads(PathBuf),
    /// Run one portable personal-data export.
    PersonalDataExport(PersonalDataExportCmd),
}

/// Effects in the portable personal-data export domain.
pub enum PersonalDataExportCmd {
    /// Project owned source state and write its sanitized snapshot off the UI loop. A remote
    /// reply channel is carried only for `ytt data export`; the in-TUI button uses `None`.
    Export {
        directory: PathBuf,
        sources: Box<PersonalDataExportSources>,
        reply: Option<crate::remote::RemoteReply>,
    },
}

/// Side effects the reducer asks the run loop to perform.
pub enum Cmd {
    /// Admission-sensitive player work. Intents commit projected UI state only after the
    /// runtime accepts the whole command batch; transport recovery carries its ordered restore
    /// batch through the same bounded admission path.
    PlayerControl(PlayerControl),
    /// Off-loop disk work for the radio recorder (copy/tag a saved track, delete a temp,
    /// wipe the temp dir). Run via `spawn_blocking`; a `Save` reports back as `Msg::Recorder`.
    Recorder(crate::recorder::job::RecorderJob),
    /// Connect the IPC client for a freshly spawned video-overlay mpv.
    VideoConnect {
        ipc_path: String,
        generation: u64,
        bindings: Vec<crate::player::video::VideoKeyBinding>,
    },
    /// `loadfile <url> replace` into the live overlay window (auto-continue).
    VideoLoad(String),
    /// Toggle the external video overlay's own pause state.
    VideoTogglePause,
    /// Toggle the external video overlay's fullscreen state.
    VideoToggleFullscreen,
    /// Toggle the external video overlay's mute state.
    VideoToggleMute,
    /// Mark a newer release tag as accepted by the reducer and queued for notification.
    UpdateSeen { tag: String },
    Search {
        request_id: u64,
        query: String,
        source: SearchSource,
        config: SearchConfig,
    },
    /// Search public YouTube playlists by name (the search box's playlist kind).
    SearchPlaylists { request_id: u64, query: String },
    /// Fetch a remote playlist's full track list, then apply `intent` to it.
    FetchPlaylistTracks {
        playlist_id: String,
        title: String,
        intent: crate::api::PlaylistIntent,
    },
    /// Persist a store to disk (or clear one) via the debounced persistence actor. The
    /// [`PersistCmd`] payload selects which store; for the marker variants the runtime clones
    /// the live snapshot from `App` at dispatch time (`Config` carries its own owned snapshot).
    Persist(PersistCmd),
    /// Local-data work, including portable personal-data exports.
    Data(DataCmd),
    /// Download actor and blocking download-store/file operations.
    Download(DownloadCmd),
    /// Load or rebuild the Local Deck index off the UI thread.
    Local(LocalCmd),
    /// Fetch synced lyrics for a track.
    FetchLyrics {
        video_id: String,
        artist: String,
        title: String,
    },
    /// Fetch + decode album art for a track (only when album art is enabled).
    FetchArtwork { video_id: String, source: ArtSource },
    /// Prefetch a track's direct stream URL for instant skip.
    Resolve { video_id: String, watch_url: String },
    /// Resolve the current self-healing track after yt-dlp was updated. Unlike ordinary
    /// prefetch, an identical request already in flight must not satisfy this command.
    ResolveForSelfHeal { video_id: String, watch_url: String },
    /// Playback self-heal: run a yt-dlp update check now (extraction-shaped failure on
    /// `video_id`). Answered by [`Msg::YtdlpHealResult`]; carries the tools config so
    /// the runtime needs no config plumbing of its own.
    YtdlpSelfHeal {
        video_id: String,
        tools: crate::config::ToolsConfig,
    },
    /// Fire a desktop notification (radio-recording saved). Handled in the main loop where the
    /// terminal is owned: emits an OSC 9/777 escape when the terminal supports it, else a native
    /// `notify-rust` toast off-thread. Best-effort; the in-app status toast is the final fallback.
    DesktopNotify { title: String, body: String },
    /// Off-path: ask the assistant to distill a recent-feedback digest into artists to avoid /
    /// re-allow for the active station. The result returns as [`AiMsg::StationPatch`].
    SummarizeFeedback { digest: String },
    /// Ask Gemini to upgrade local CJK title/artist romanization for a visible batch.
    RomanizeTitles {
        request_id: u64,
        items: Vec<RomanizeItem>,
    },
    /// Ask the DJ Gem assistant to handle a prompt, given a read-only state snapshot.
    AskAi {
        prompt: String,
        context: Box<AiContext>,
    },
    /// Resolve free-text artist/title to real YouTube tracks (favorites need a
    /// `video_id`), off the Search screen. Answers as [`Msg::TrackResolved`].
    ResolveTrack {
        seq: u64,
        query: String,
        config: SearchConfig,
    },
    /// Ask the anonymous API/search actor for related tracks to keep streaming going without DJ Gem.
    StreamingFallback {
        seed: String,
        seed_video_id: String,
        exclude_ids: Vec<String>,
        mode: StreamingMode,
        config: SearchConfig,
    },
    /// Ask the API actor to run a final metadata preflight on streaming picks before enqueueing.
    /// Only risky candidates trigger full yt-dlp extraction; clean picks pass through.
    StreamingPreflight {
        seed_video_id: String,
        picks: Vec<Song>,
        fallback: Vec<Song>,
        mode: StreamingMode,
        config: streaming::StreamingConfig,
    },
    /// Hand a local candidate shortlist to the DJ Gem actor to rerank (ids only). The result
    /// returns as [`StreamingMsg::AiPicks`]; failure degrades to the stashed local pick.
    AiRerank {
        seed_video_id: String,
        prompt: String,
    },
    /// Switch the running DJ Gem actor's model (settings save). No effect without a key.
    SetAiModel(GeminiModel),
    /// (Re)build the DJ Gem actor with a new key + model (settings save, key changed). A
    /// `None` key tears the assistant down; a valid key brings it up live — so a key
    /// entered at runtime takes effect immediately, with no relaunch.
    ReloadAi {
        key: Option<String>,
        model: GeminiModel,
        assistant_enabled: bool,
    },
    /// Last.fm owner controls (browser auth and live settings reconfiguration).
    Scrobble(ScrobbleCmd),
    /// A command for the transfer actor (Spotify auth / playlist listing / jobs).
    Transfer(crate::transfer::actor::TransferCmd),
}

/// Scrobble-owner controls share one top-level effect bucket so authentication and live
/// reconfiguration cannot grow the already-broad [`Cmd`] surface independently.
pub enum ScrobbleCmd {
    AuthStart,
    Reconfigure(Box<crate::scrobble::ScrobbleSettings>),
}

/// Which persisted store a [`Cmd::Persist`] targets. The marker variants carry no data — the
/// runtime clones the live snapshot from `App` at dispatch time — while `Config` carries its
/// own owned snapshot (settings save) and `ClearRomanizedTitles` deletes rather than saves.
pub enum PersistCmd {
    /// Persist the library (song favorites/history and radio stations) to disk.
    Library,
    /// Persist the downloads manifest (completed downloads' YouTube identity) to disk.
    Downloads,
    /// Persist the per-track preference signals (plays/skips/dislikes) to disk.
    Signals,
    /// Persist the Latin-script title display cache to disk.
    RomanizedTitles,
    /// Delete the persisted Latin-script title display cache from disk.
    ClearRomanizedTitles,
    /// Persist the given config to disk (settings screen, on save).
    Config(Box<Config>),
    /// Persist the local playlists to disk (after a DJ Gem playlist mutation).
    Playlists,
    /// Persist the active natural-language station profile to disk (after vibe-shaped streaming).
    StationProfile,
    /// Persist one transfer candidate under an exact target generation. Live playlists stay
    /// unchanged until the completion returns to the reducer.
    TransferPlaylistCommit(Box<TransferPlaylistCommit>),
}

/// Blocking Local Deck work requested by the reducer.
#[derive(Debug, Clone)]
pub enum LocalCmd {
    LoadIndex {
        index_path: Option<PathBuf>,
    },
    ScanRoots {
        roots: Vec<crate::local::LocalScanRoot>,
        index_path: Option<PathBuf>,
        previous: crate::local::LocalIndex,
    },
    ReviewImport {
        op_id: u64,
        session_id: String,
        source_order: u32,
        action: ImportReviewAction,
    },
    ReviewImportAcceptAll {
        op_id: u64,
        session_id: String,
    },
    /// Rebuild the immutable Local Find corpus after either owned source generation changes.
    BuildFindCorpus {
        generation: u64,
        tracks: Vec<crate::local::LocalTrack>,
        playlists: Vec<crate::local::find::LocalFindPlaylistInput>,
        revision: crate::local::find::LocalFindCorpusRevision,
        options: crate::local::find::LocalFindCorpusOptions,
    },
    /// Evaluate one parsed query against a shared immutable corpus off the UI thread.
    EvaluateFind {
        request_id: u64,
        generation: u64,
        corpus: Arc<crate::local::find::LocalFindCorpus>,
        query: crate::local::find::LocalFindQuery,
        scope: crate::local::find::LocalFindScope,
        sort: crate::local::find::LocalFindSort,
    },
    /// Retire any in-flight Local Find evaluation after a blank or invalid live edit.
    CancelFindEvaluations,
}

/// Local Deck worker results.
#[derive(Debug, Clone)]
pub enum LocalMsg {
    IndexLoaded {
        index_path: Option<PathBuf>,
        index: crate::local::LocalIndex,
        warnings: Vec<crate::local::ScanError>,
    },
    ScanFinished {
        index_path: Option<PathBuf>,
        result: crate::local::LocalScanResult,
    },
    ScanProgress(crate::local::LocalScanProgress),
    ScanFailed {
        error: String,
    },
    ImportReviewFinished {
        op_id: u64,
        session_id: String,
        source_order: u32,
        action: ImportReviewAction,
        result: Result<crate::transfer::review_action::ReviewActionSummary, String>,
        elapsed_ms: u128,
    },
    ImportReviewAcceptAllFinished {
        op_id: u64,
        session_id: String,
        result: Result<crate::transfer::review_action::ReviewBatchSummary, String>,
        elapsed_ms: u128,
    },
    FindCorpusReady {
        generation: u64,
        corpus: Arc<crate::local::find::LocalFindCorpus>,
    },
    FindResultsReady {
        request_id: u64,
        generation: u64,
        snapshot: crate::local::find::LocalFindSnapshot,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportReviewAction {
    AcceptFirst,
    ChooseNext,
    Reject,
    Skip,
}

/// A button or blocker on the interactive Beginner Mode coach card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingAction {
    Noop,
    Primary,
    Back,
    Skip,
    ConfirmSkip,
    CancelSkip,
}

/// Exact Local Find view rendered into a pointer hit map. A result snapshot and its drill-down
/// share the same query generation, so the drill owner is part of the identity rather than only
/// relying on a numeric generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalFindPointerView {
    Launchpad,
    Recovery,
    Results,
    Drill(crate::local::find::LocalFindHitId),
}

/// Identity stamped onto Local Find targets whose meaning depends on the rendered row set.
/// Both generations are required: a corpus rebuild can retain the query request, while a query
/// edit can retain the corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFindPointerStamp {
    pub corpus_generation: u64,
    pub result_generation: u64,
    pub view: LocalFindPointerView,
    /// Recovery rows and refreshed drills can change length without changing their broad view
    /// kind; include the exact rendered geometry so an old scrollbar drag cannot target it.
    pub rows_len: usize,
}

/// A clickable terminal region's semantic target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseTarget {
    /// An action row in the open TUI context menu.
    ContextMenuItem(usize),
    ToolSetupCopy,
    ToolSetupGuide,
    ToolSetupRetry,
    ToolSetupLater,
    /// A control on the Beginner Mode coach card. `Noop` seals the card body against
    /// click-through; the remaining actions are rendered as explicit buttons on top.
    Onboarding(OnboardingAction),
    Global(Action),
    Player(Action),
    /// A visible synced-lyric row. The owning track ID and original LRC index make stale frame
    /// targets fail closed instead of seeking a newly loaded track.
    LyricsLine {
        video_id: Arc<str>,
        line_index: usize,
    },
    /// The collapsed `[±]` handle. Carries its rendered track ID for the same stale-frame guard.
    LyricsDelayHandle {
        video_id: Arc<str>,
    },
    /// Expanded lyric-delay buttons. Keeping the rendered track ID prevents an old frame's OSD
    /// from adjusting a newly loaded song before the next frame replaces the hit map.
    LyricsDelayEarlier {
        video_id: Arc<str>,
    },
    LyricsDelayLater {
        video_id: Arc<str>,
    },
    /// Inert coverage for the expanded lyric-delay OSD's value and spacing. Action buttons are
    /// registered after it and win hit-testing; everything else is deliberately consumed.
    LyricsDelayBlock,
    /// Open/close the EQ preset dropdown on the player status line (clicking the `eq:` label).
    EqMenu,
    /// Pick an EQ preset from the open dropdown.
    EqSelect(EqPreset),
    /// Open/close the streaming-mode dropdown on the player status line (clicking the `streaming:` label).
    StreamingMenu,
    /// Pick a streaming mode from the open dropdown.
    StreamingSelect(StreamingMode),
    /// The player volume cluster (`vol - 50% +`). Clicks are ignored on the label/value,
    /// but wheel events over the cluster nudge volume.
    VolumeArea,
    /// A nav-bar item — switch to that screen from any screen.
    Nav(Mode),
    /// The search bar's submit button.
    SearchSubmit,
    /// The search query input box.
    SearchInput,
    /// The `⌕ Filter` button next to the search bar — opens the results-filter popup.
    SearchFilterOpen,
    /// A row in the results-filter popup, by *display* index into the filtered rows.
    /// Single-click selects; double-click plays; right-click enqueues.
    SearchFilterRow(usize),
    /// Open/close the search-source dropdown.
    SearchSourceMenu,
    /// Pick a source from the search-source dropdown.
    SearchSourceSelect(SearchSource),
    /// A Library tab header.
    LibraryTab(LibraryTab),
    /// A Local Deck sidebar section, by index into [`LocalSection::ALL`].
    LocalNav(usize),
    /// A row in the Local Deck list, by display index.
    LocalRow(usize),
    /// A generation-stamped Local Find result/drill row. Old frames cannot redirect actions.
    LocalFindRow {
        index: usize,
        stamp: LocalFindPointerStamp,
    },
    LocalFindInput,
    LocalFindSubmit,
    LocalFindRefineOpen,
    LocalFindRefineRow(usize),
    LocalFindLaunchpad {
        index: usize,
        stamp: LocalFindPointerStamp,
    },
    /// A Local Find scrollbar is separate from generic scrollbars so a delayed press or drag
    /// cannot move a newer query, corpus, or drill view.
    LocalFindScrollbar {
        stamp: LocalFindPointerStamp,
    },
    ConfirmLocalFindBulk,
    CancelLocalFindBulk,
    ConfirmLocalFindRebuild,
    CancelLocalFindRebuild,
    /// The trailing `✗` on a saved Local Deck import-session row. Carries the exact persisted
    /// job id so a re-sort between render and click can never redirect the destructive action.
    LocalImportDel(String),
    /// The footer mouse-help icon. Mouse-only: no keybinding maps to this overlay.
    MouseHelp,
    /// A Settings tab header, by index into [`SettingsTab::ALL`].
    SettingsTab(usize),
    /// A clickable value control on a Settings field row — the checkbox of a toggle or the
    /// `<` / `>` arrow of a Select/Slider. Carries the field-row index and the nudge direction,
    /// so a click is the mouse equivalent of ←/→ on that row.
    SettingsChange {
        row: usize,
        delta: i32,
    },
    /// A clickable Settings button or text value, by field-row index — enters edit mode (text)
    /// or fires the action (button); the mouse equivalent of Enter on that row.
    SettingsActivate(usize),
    /// The two-cell color swatch on a Settings theme row — opens the full color picker.
    SettingsColorSwatch(usize),
    /// Modal picker backdrop/chrome. It captures clicks inside the popup that are not choices.
    SettingsColorPickerSurface,
    /// The lossless current-value row in the modal color picker.
    SettingsColorPickerCurrent,
    /// A picker-grid choice: transparent at zero, then the 240 xterm colors.
    SettingsColorPickerChoice(usize),
    /// Open/close the Settings Spotify import-mode dropdown.
    SettingsSpotifyImportModeMenu,
    /// Pick a Settings Spotify import-mode dropdown option.
    SettingsSpotifyImportModeSelect(crate::config::SpotifyImportMode),
    /// A row in the Settings audio-output picker.
    AudioOutputRow(usize),
    /// A list row, by absolute item index (interpreted per the active screen). Single-click
    /// selects; double-click plays.
    ListRow(usize),
    /// A vertical list scrollbar track/thumb. Clicking or dragging maps the pointer row to
    /// the matching viewport offset for the owning scroll state.
    Scrollbar(ScrollSurface),
    /// A rendered visual row in the DJ Gem transcript, after wrapping. Dragging across these
    /// rows copies the selected chat text.
    AiTranscriptRow(usize),
    /// The DJ Gem prompt input box.
    AiInput,
    /// The DJ Gem prompt submit button.
    AiSubmit,
    /// The DJ Gem model label under the prompt — cycles the active model.
    AiModel,
    /// A pickable DJ Gem suggestion row.
    AiSuggestionRow(usize),
    /// The `N/M` queue-position label on the player status line — opens the queue window.
    QueuePos,
    /// A row in the open queue window, by order position. Single-click selects; double-click
    /// jumps playback to it.
    QueueRow(usize),
    /// The `✗` delete button on a queue-window row, by order position.
    QueueDel(usize),
    /// The `✗` delete button on a Library list row, by row index in the current tab.
    LibraryDel(usize),
    /// The breadcrumb of an opened playlist (Playlists tab drill-down) — returns to the
    /// playlist list.
    PlaylistBack,
    /// Confirm button on the "delete playlist" modal.
    ConfirmPlaylistDelete,
    /// Cancel button on the "delete playlist" modal.
    CancelPlaylistDelete,
    /// Create button on the "new playlist" popup.
    ConfirmPlaylistCreate,
    /// Cancel button on the "new playlist" popup.
    CancelPlaylistCreate,
    /// A row in the "add to playlist" picker: `0..len` choose a playlist, `len` is the
    /// trailing "New playlist…" row.
    PlaylistPickRow(usize),
    /// Create button on the picker's inline new-playlist name entry.
    ConfirmPickerCreate,
    /// Back button on the picker's inline new-playlist name entry (returns to the list).
    CancelPickerCreate,
    /// A row in the "Import from Spotify" picker, by item index. Single-click selects;
    /// clicking the already-selected row (or double-click) starts the import.
    SpotifyPickRow(usize),
    /// Confirm button on the "delete downloaded files" modal.
    ConfirmDelete,
    /// Cancel button on the "delete downloaded files" modal.
    CancelDelete,
    /// Confirm button on the bulk "download N songs" modal.
    ConfirmDownload,
    /// Cancel button on the bulk "download N songs" modal.
    CancelDownload,
    /// Confirm button on a Settings confirmation modal.
    ConfirmSettings,
    /// Cancel button on a Settings confirmation modal.
    CancelSettings,
    /// Confirm button on the radio-mode confirmation modal.
    ConfirmRadioMode,
    /// Cancel button on the radio-mode confirmation modal.
    CancelRadioMode,
    /// Confirm button on the local-player confirmation modal.
    ConfirmLocalMode,
    /// Cancel button on the local-player confirmation modal.
    CancelLocalMode,
    /// Confirm button on the local import organize modal.
    ConfirmLocalOrganize,
    /// Cancel button on the local import organize modal.
    CancelLocalOrganize,
    /// Confirm button on the local import accept-all modal.
    ConfirmLocalAcceptAll,
    /// Cancel button on the local import accept-all modal.
    CancelLocalAcceptAll,
    /// Buttons on the local import-history delete confirmation.
    ConfirmLocalImportDelete,
    CancelLocalImportDelete,
    /// "Save to favorites" on the "what's playing" overlay (resolves a real YT track first).
    NowPlayingFavorite,
    /// "Tell me more" on the "what's playing" overlay — hands off to the DJ Gem view.
    NowPlayingAskAi,
    /// Close button on the "what's playing" overlay.
    CloseNowPlaying,
    /// The `yututui` brand label at the top-left of the nav bar — opens the About card.
    AboutTitle,
    /// The GitHub link inside the About card — opens the repo in the system browser.
    AboutLink,
    /// The "Releases" link inside the About card's update notice — opens the latest release
    /// page in the system browser (only present when an update is available).
    AboutUpdateLink,
    /// A row in the radio-recording settings popup, by row index (`0..7`). Clicking focuses the
    /// row; for the folder (edit) and browse (open list) rows it also activates them — the mouse
    /// equivalent of moving there and pressing Enter. Value rows (mode / sliders / notify) only
    /// focus here; their arrows and track publish [`MouseTarget::RecordingChange`] /
    /// [`MouseTarget::RecordingSlider`] rects on top so a bare row click never changes a value.
    RecordingRow(usize),
    /// A `‹`/`›` arrow (or the mode `< >`, or the notify `[x]`) on a radio-recording popup row —
    /// carries the row index and the nudge direction, so a click is the mouse equivalent of
    /// ←/→ on that row.
    RecordingChange {
        row: usize,
        delta: i32,
    },
    /// The draggable bar track of a numeric radio-recording row (min / max / keep-recent). A
    /// press maps pointer-x to a value and arms a drag that keeps mapping as the pointer moves,
    /// exactly like the player seekbar.
    RecordingSlider(usize),
    /// A row in the radio-recordings browser, by item index. Single-click selects the row.
    RecordingBrowseRow(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseButtonRegion {
    pub rect: Rect,
    pub target: MouseTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSurface {
    Library,
    Search,
    LocalFind,
    /// The search results-filter popup's list.
    SearchFilter,
    AiTranscript,
    AiSuggestions,
    Settings,
    Queue,
    /// The radio "now playing" (지듣노) card's title line — marquee-only, no scrollbar.
    NowPlaying,
    /// The player/mini/docked title row — marquee-only, no scrollbar.
    PlayerTitle,
}

/// Who authored a line in the DJ Gem chat transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiRole {
    User,
    Ai,
    Error,
}

/// One line in the DJ Gem chat transcript.
pub struct AiMessage {
    pub role: AiRole,
    pub text: String,
}

/// Within the DJ Gem screen, whether the input box or the suggestions list has focus.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum AiFocus {
    #[default]
    Input,
    Suggestions,
}

/// A live radio stream's current ICY/metadata title, as exposed by mpv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamNowPlaying {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub raw: String,
}

impl StreamNowPlaying {
    pub fn label(&self) -> String {
        match (&self.title, &self.artist) {
            (Some(title), Some(artist)) => format!("{title} — {artist}"),
            (Some(title), None) => title.clone(),
            _ => self.raw.clone(),
        }
    }
}

/// A streaming rerank handed to the DJ Gem actor, kept until its `StreamingMsg::AiPicks` returns. The
/// `shortlist` is the exact set the model was shown — every returned id is validated against
/// it (so a hallucinated id is dropped) — and `local_pick` is the guaranteed fallback ordering
/// the engine produced, used to top up any slots the DJ Gem left empty.
pub struct PendingRerank {
    pub(crate) seed_video_id: String,
    pub(crate) mode: StreamingMode,
    pub(crate) shortlist: Vec<Song>,
    pub(crate) local_pick: Vec<Song>,
    /// Maps each pack `cid` shown to the model back to its track's video id, so the DJ Gem's chosen
    /// cids can be resolved to playable tracks before validation.
    pub(crate) cid_map: Vec<crate::streaming::PackedCand>,
    /// Cache key for this rerank (hash of seed artist / mode / recent ids / candidate set), so the
    /// resolved ordering can be stored on return and replayed for a rapid identical refill.
    pub(crate) cache_key: u64,
}

/// The resolved, human-readable explanation of the last DJ Gem streaming rerank, shown by the "Why DJ Gem"
/// overlay (the `w` key). Built when [`StreamingMsg::AiPicks`] resolves — the model's opaque cids are
/// mapped back to real tracks (title + artist) while [`PendingRerank`] is still in hand — so the
/// overlay can render it long after the pending rerank has been consumed.
#[derive(Debug, Clone, Default)]
pub struct StreamingAiExplain {
    /// The model's self-reported confidence in [0,1], if any.
    pub(crate) conf: Option<f32>,
    /// The picks the model chose, in its best-first order (hallucinated cids already dropped).
    pub(crate) picks: Vec<ExplainPick>,
}

/// One resolved pick in a [`StreamingAiExplain`]: the track it landed on plus the model's stated
/// slot role and reason codes.
#[derive(Debug, Clone)]
pub(crate) struct ExplainPick {
    pub(crate) title: String,
    pub(crate) artist: String,
    /// The model's slot role for this pick (core/bridge/adjacent/discovery/…), if it gave one.
    pub(crate) role: Option<String>,
    /// The model's reason codes (the evidence scores it leaned on, e.g. `tr`, `u`).
    pub(crate) reasons: Vec<String>,
}

/// One ordered listening-session outcome (newest pushed to the back of
/// [`crate::app::StreamingRuntime::session_events`]). Feeds the DJ Gem reranker's *recovery context* — a
/// skip → widen and avoid the skipped artist, a like → stay close — so the model reacts to the
/// arc of the session, not just the aggregate per-track signals the engine already folds in.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    /// Normalized artist key (the recovery context keys off the artist, not the track id — the
    /// engine already excludes the just-played track via history/cooldown).
    pub(crate) artist_key: String,
    pub(crate) outcome: Outcome,
    /// Fraction of the track that played, [0,1] (meaningful for plays/skips; ~current position
    /// for likes/dislikes).
    pub(crate) completion: f32,
}

/// The kind of a [`SessionEvent`]. `QuickSkip` is a skip below [`crate::signals::STRONG_SKIP_FRAC`]
/// (bailed almost immediately) — a stronger "wrong direction" signal than an ordinary `Skip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    FullPlay,
    Skip,
    QuickSkip,
    Like,
    Dislike,
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

/// Contextual owner of the shared top-level Search/Find navigation slot.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ActiveSearchSurface {
    Normal,
    Radio,
    Local,
}

/// Synced lyrics for one track (held while it's the current track).
pub struct TrackLyrics {
    pub video_id: Arc<str>,
    pub lines: std::sync::Arc<[LyricLine]>,
}

/// The lists in the library view.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum LibraryTab {
    #[default]
    All,
    Favorites,
    History,
    RadioFavorites,
    Radio,
    Downloads,
    Playlists,
}

impl LibraryTab {
    pub const NORMAL: [Self; 5] = [
        Self::All,
        Self::Favorites,
        Self::History,
        Self::Downloads,
        Self::Playlists,
    ];

    pub const RADIO_MODE: [Self; 2] = [Self::RadioFavorites, Self::Radio];

    pub fn label(self) -> &'static str {
        match self {
            LibraryTab::All => t!("All", "전체"),
            LibraryTab::Favorites => t!("Favorites", "즐겨찾기"),
            LibraryTab::History => t!("History", "기록"),
            LibraryTab::RadioFavorites => t!("Radio Likes", "라디오 좋아요"),
            LibraryTab::Radio => t!("Radio History", "라디오 히스토리"),
            LibraryTab::Downloads => t!("Downloads", "다운로드"),
            LibraryTab::Playlists => t!("Playlists", "플레이리스트"),
        }
    }

    pub fn compact_label(self) -> &'static str {
        match self {
            LibraryTab::All => t!("All", "전체"),
            LibraryTab::Favorites => t!("Fav", "즐겨찾기"),
            LibraryTab::History => t!("Hist", "기록"),
            LibraryTab::RadioFavorites => t!("R-Like", "라디오 좋아요"),
            LibraryTab::Radio => t!("R-Hist", "라디오 기록"),
            LibraryTab::Downloads => t!("Down", "다운"),
            LibraryTab::Playlists => t!("Lists", "플리"),
        }
    }
}

/// Pending confirmation for entering or leaving the dedicated Radio UI mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioModeConfirm {
    Enter,
    Exit,
}

impl RadioModeConfirm {
    pub fn title(self) -> &'static str {
        match self {
            RadioModeConfirm::Enter => t!(" Confirm radio mode ", " 라디오 모드 확인 "),
            RadioModeConfirm::Exit => t!(" Confirm normal mode ", " 일반 모드 확인 "),
        }
    }

    pub fn prompt(self) -> &'static str {
        match self {
            RadioModeConfirm::Enter => {
                t!(
                    "Switch to dedicated Radio mode?",
                    "라디오 전용 모드로 전환할까요?"
                )
            }
            RadioModeConfirm::Exit => t!("Leave Radio mode?", "라디오 모드에서 나갈까요?"),
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            RadioModeConfirm::Enter => t!(
                "Search becomes Radio Browser only; Library shows radio favorites and history.",
                "검색은 Radio Browser만, 라이브러리는 라디오 좋아요와 히스토리만 표시됩니다."
            ),
            RadioModeConfirm::Exit => t!(
                "Your normal theme, Search sources, Library tabs, and queue return.",
                "일반 테마, 검색 소스, 라이브러리 탭, 큐가 돌아옵니다."
            ),
        }
    }
}

/// Pending confirmation for entering or leaving the Library-owned Local Deck shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalModeConfirm {
    Enter,
    Exit,
}

/// Single-use handoff for the sole intentional online transition from Local Deck: a manually
/// confirmed Import Sessions candidate search. The normal Search state is not touched until the
/// dedicated-mode exit has committed and this stable origin has been revalidated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalImportSearchContinuation {
    /// Local-exit confirmation that explicitly authorized this one online search.
    pub confirmation_token: u64,
    pub query: String,
    pub session_id: String,
    pub row_id: String,
    pub source_order: u32,
    pub source_revision: u64,
}

impl LocalModeConfirm {
    pub fn title(self) -> &'static str {
        match self {
            LocalModeConfirm::Enter => t!(" Confirm local player ", " 로컬 플레이어 확인 "),
            LocalModeConfirm::Exit => t!(" Confirm library mode ", " 라이브러리 모드 확인 "),
        }
    }

    pub fn prompt(self) -> &'static str {
        match self {
            LocalModeConfirm::Enter => t!(
                "Switch to Local Player mode?",
                "로컬 플레이어 모드로 전환할까요?"
            ),
            LocalModeConfirm::Exit => {
                t!(
                    "Leave Local Player mode?",
                    "로컬 플레이어 모드에서 나갈까요?"
                )
            }
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            LocalModeConfirm::Enter => t!(
                "Browse downloaded local audio in an immersive Library shell.",
                "라이브러리 안에서 다운로드된 로컬 오디오를 전용 화면으로 탐색합니다."
            ),
            LocalModeConfirm::Exit => t!(
                "Return to the normal Library tabs.",
                "일반 라이브러리 탭으로 돌아갑니다."
            ),
        }
    }
}

/// Pending confirmation before moving an import session's inbox files into the local library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalOrganizeConfirm {
    pub session_id: String,
    pub root: PathBuf,
    pub move_count: u32,
    pub already_count: u32,
    pub skipped_count: u32,
}

impl LocalOrganizeConfirm {
    pub fn title(&self) -> &'static str {
        t!(" Commit import files ", " 임포트 파일 커밋 ")
    }

    pub fn prompt(&self) -> String {
        if crate::i18n::is_korean() {
            format!("{}개 파일을 로컬 라이브러리로 이동할까요?", self.move_count)
        } else {
            format!(
                "Move {} import file{} into the local library?",
                self.move_count,
                if self.move_count == 1 { "" } else { "s" }
            )
        }
    }

    pub fn detail(&self) -> String {
        if crate::i18n::is_korean() {
            format!(
                "{} -> {}  (이미 정리됨 {}, 건너뜀 {})",
                self.session_id,
                self.root.display(),
                self.already_count,
                self.skipped_count
            )
        } else {
            format!(
                "{} -> {}  ({} already, {} skipped)",
                self.session_id,
                self.root.display(),
                self.already_count,
                self.skipped_count
            )
        }
    }
}

/// Pending confirmation before marking every safely reviewable candidate Ready.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalImportAcceptAllConfirm {
    pub session_id: String,
    pub candidate_count: u32,
    pub ready_count: u32,
    pub total_count: u32,
    pub local_count: u32,
    pub review_left: u32,
    pub missing_left: u32,
}

impl LocalImportAcceptAllConfirm {
    pub fn title(&self) -> &'static str {
        t!(" Mark all Ready ", " 전체 준비 완료 표시 ")
    }

    pub fn prompt(&self) -> String {
        if crate::i18n::is_korean() {
            format!(
                "안전한 후보 {}개를 준비 완료로 표시할까요?",
                self.candidate_count
            )
        } else {
            format!(
                "Mark {} safe candidate{} Ready for this playlist?",
                self.candidate_count,
                if self.candidate_count == 1 { "" } else { "s" },
            )
        }
    }

    pub fn detail(&self) -> String {
        if crate::i18n::is_korean() {
            format!(
                "준비 {}/{} · 로컬 {}/{} · 남을 검토 {} · 누락 {}",
                self.ready_count,
                self.total_count,
                self.local_count,
                self.total_count,
                self.review_left,
                self.missing_left
            )
        } else {
            format!(
                "Ready {}/{} · Local {}/{} · review left {} · missing {}",
                self.ready_count,
                self.total_count,
                self.local_count,
                self.total_count,
                self.review_left,
                self.missing_left
            )
        }
    }
}

/// The primary section visible in the Local Deck shell.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LocalSection {
    #[default]
    Home,
    Tracks,
    Albums,
    Artists,
    Genres,
    Folders,
    SmartLists,
    ScanErrors,
    ImportSessions,
    Inbox,
}

impl LocalSection {
    pub const ALL: [Self; 10] = [
        Self::Home,
        Self::Tracks,
        Self::Albums,
        Self::Artists,
        Self::Genres,
        Self::Folders,
        Self::SmartLists,
        Self::ScanErrors,
        Self::ImportSessions,
        Self::Inbox,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Home => t!("Home", "홈"),
            Self::Tracks => t!("Tracks", "곡"),
            Self::Albums => t!("Albums", "앨범"),
            Self::Artists => t!("Artists", "아티스트"),
            Self::Genres => t!("Genres", "장르"),
            Self::Folders => t!("Folders", "폴더"),
            Self::SmartLists => t!("Smart Lists", "스마트 목록"),
            Self::ScanErrors => t!("Scan Errors", "스캔 오류"),
            Self::ImportSessions => t!("Import Sessions", "임포트 세션"),
            Self::Inbox => t!("Inbox", "인박스"),
        }
    }

    pub fn from_digit(ch: char) -> Option<Self> {
        let digit = ch.to_digit(10)?;
        let index = if digit == 0 {
            9
        } else {
            digit.checked_sub(1)? as usize
        };
        Self::ALL.get(index).copied()
    }
}

/// Focused pane inside the Local Deck shell.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LocalPane {
    Sidebar,
    #[default]
    List,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalDrill {
    Album(crate::local::LocalAlbumId),
    Artist(crate::local::LocalArtistId),
    Genre(String),
    Folder(PathBuf),
    Smart(crate::local::LocalSmartList),
    ImportSession(String),
}

/// What the "what's playing" (지듣노) card is showing — populated synchronously from the
/// live radio stream's own ICY metadata, no identification call. The name fields are
/// (untrusted) stream text — never recall — and stay untrusted data wherever they flow
/// (overlay, DJ Gem seed, search query).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NowPlayingOverlayState {
    /// The stream is exposing a usable title (sanitized). `artist` is present only when
    /// the raw ICY title split cleanly into artist + title; otherwise `title` carries the
    /// whole cleaned string.
    Playing {
        artist: Option<String>,
        title: String,
    },
    /// The title looks like an advertisement, jingle, or station id — station content,
    /// not a song (a light local heuristic, no AI).
    StationContent,
    /// No usable stream title right now: the station exposes none, or (right after tuning
    /// in) mpv hasn't surfaced the first ICY tick yet.
    NoMetadata,
}

/// The "what's playing" (지듣노) overlay: a compact card over the player showing the
/// current radio song straight from the stream's ICY metadata, with favorite / ask-DJ Gem
/// actions. `None` on [`crate::app::App`] = closed. Snapshots the station + sanitized
/// title it was opened for, so the favorite action stays pinned to that moment even if the
/// stream moves on underneath.
#[derive(Debug, Clone)]
pub struct NowPlayingOverlay {
    /// The station `Song::video_id`, keying the favorite-resolution cache.
    pub station_id: String,
    /// The station's display label.
    pub station_label: String,
    /// The sanitized ICY title this overlay is about.
    pub raw_title: String,
    pub state: NowPlayingOverlayState,
    /// The YouTube track the favorite action resolved to (attached after the first
    /// search so a repeat favorite / re-open never re-searches).
    pub resolved: Option<Song>,
    /// A favorite resolve is in flight (debounces the button).
    pub resolving: bool,
    /// Resolve epoch — a reply must match [`crate::app::App`]'s live counter via this
    /// snapshot or it's stale (overlay closed / title changed).
    pub resolve_seq: u64,
}

/// Within the search screen, whether the query box or the results list has focus.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum SearchFocus {
    #[default]
    Input,
    Results,
}

/// What the search box looks for: tracks (default) or public YouTube playlists.
/// Session-scoped; toggled with [`Action::ToggleSearchKind`].
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum SearchKind {
    #[default]
    Songs,
    Playlists,
}

/// The semantic kind of the transient `status` line, controlling its color in the player
/// view. Defaults to `Error` (red) so the existing `self.status.text = …` sites keep their styling;
/// only positive confirmations opt into `Info` (green).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StatusKind {
    #[default]
    Error,
    Info,
}
