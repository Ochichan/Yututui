//! Message, command, and view-state type definitions for the app reducer.
//!
//! Split out of the former monolithic `app.rs` (behaviour-preserving move). These are
//! re-exported from `crate::app` (`pub use types::*`) so existing `crate::app::Msg` /
//! `crate::app::Cmd` / `crate::app::Mode` paths keep resolving for actors and views.

use super::*;

/// Everything that can change the application state.
pub enum Msg {
    /// Inert. Runtime events with no standalone-TUI meaning (e.g. daemon-only
    /// GUI-session answers) still need a total `RuntimeEvent → Msg` mapping.
    Noop,
    Key(KeyEvent),
    /// A left-click at a terminal cell (1-based crossterm coords); may hit the seekbar.
    MouseClick {
        col: u16,
        row: u16,
    },
    /// A left double-click at a cell — plays a song row / queue entry (vs. single-click,
    /// which selects). Falls back to single-click behavior off a list row.
    MouseDoubleClick {
        col: u16,
        row: u16,
    },
    /// A right-click at a cell. Search/Library rows add the song to the queue; queue-window
    /// rows remove that entry. Ignored elsewhere.
    MouseRightClick {
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
    /// Periodic wake-up (driven by the main loop only while a transient `status` is showing)
    /// that lets the reducer expire the status after [`STATUS_TTL`] and restore the title.
    StatusTick,
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
    /// Download folder scan completed.
    DownloadsScanned(Vec<Song>),
    /// Synced lyrics for `video_id` (empty `lines` = none found).
    LyricsResult {
        video_id: String,
        lines: Vec<LyricLine>,
    },
    /// Decoded album art / thumbnail for `video_id` (`None` = none found / fetch failed).
    ArtworkResult {
        video_id: String,
        image: Option<DynamicImage>,
    },
    /// Album-art protocol resize/encode finished off the UI thread.
    ArtworkResized(ResizeResponse),
    /// Download progress for `video_id` (0-100).
    DownloadProgress {
        video_id: String,
        percent: f64,
    },
    /// A download finished, saved at `path`.
    DownloadDone {
        video_id: String,
        path: String,
    },
    /// A download failed.
    DownloadError {
        video_id: String,
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
    /// A command from a `ytt -r <cmd>` client, with a oneshot channel to reply on. Applied
    /// through the same reducer path as a keypress (see [`App::apply_remote`]) so it is
    /// independent of the current input mode; the computed response is sent back over the
    /// channel for the control socket to write to the client.
    Remote(
        crate::remote::proto::RemoteCommand,
        tokio::sync::oneshot::Sender<crate::remote::proto::RemoteResponse>,
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
    /// The background app-update check finished: whether a newer ytm-tui release exists,
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

/// Side effects the reducer asks the run loop to perform.
pub enum Cmd {
    Player(PlayerCmd),
    /// Off-loop disk work for the radio recorder (copy/tag a saved track, delete a temp,
    /// wipe the temp dir). Run via `spawn_blocking`; a `Save` reports back as `Msg::Recorder`.
    Recorder(crate::recorder::job::RecorderJob),
    /// Connect the IPC client for a freshly spawned video-overlay mpv.
    VideoConnect {
        ipc_path: String,
        generation: u64,
    },
    /// `loadfile <url> replace` into the live overlay window (auto-continue).
    VideoLoad(String),
    Search {
        request_id: u64,
        query: String,
        source: SearchSource,
        config: SearchConfig,
    },
    /// Search public YouTube playlists by name (the search box's playlist kind).
    SearchPlaylists {
        request_id: u64,
        query: String,
    },
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
    /// Refresh the local downloads list from this folder.
    ScanDownloads(PathBuf),
    /// Fetch synced lyrics for a track.
    FetchLyrics {
        video_id: String,
        artist: String,
        title: String,
    },
    /// Fetch + decode album art for a track (only when album art is enabled).
    FetchArtwork {
        video_id: String,
        source: ArtSource,
    },
    /// Download a track to disk (best audio + tags + cover art).
    Download(Song),
    /// Point the download actor at a new folder for future downloads.
    SetDownloadDir(PathBuf),
    /// Prefetch a track's direct stream URL for instant skip.
    Resolve {
        video_id: String,
        watch_url: String,
    },
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
    DesktopNotify {
        title: String,
        body: String,
    },
    /// Off-path: ask the assistant to distill a recent-feedback digest into artists to avoid /
    /// re-allow for the active station. The result returns as [`AiMsg::StationPatch`].
    SummarizeFeedback {
        digest: String,
    },
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
    /// Kick the Last.fm browser authorization flow (Settings › Accounts › connect).
    ScrobbleAuthStart,
    /// Hand the scrobble actor a fresh settings snapshot (settings save, connect,
    /// disconnect) — takes effect live, no relaunch.
    ScrobbleReconfigure(Box<crate::scrobble::ScrobbleSettings>),
    /// A command for the transfer actor (Spotify auth / playlist listing / jobs).
    Transfer(crate::transfer::actor::TransferCmd),
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
}

/// A clickable terminal region's semantic target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseTarget {
    Global(Action),
    Player(Action),
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
    /// "Save to favorites" on the "what's playing" overlay (resolves a real YT track first).
    NowPlayingFavorite,
    /// "Tell me more" on the "what's playing" overlay — hands off to the DJ Gem view.
    NowPlayingAskAi,
    /// Close button on the "what's playing" overlay.
    CloseNowPlaying,
    /// The `ytm-tui` brand label at the top-left of the nav bar — opens the About card.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseButtonRegion {
    pub rect: Rect,
    pub target: MouseTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSurface {
    Library,
    Search,
    /// The search results-filter popup's list.
    SearchFilter,
    AiTranscript,
    AiSuggestions,
    Settings,
    Queue,
    /// The radio "now playing" (지듣노) card's title line — marquee-only, no scrollbar.
    NowPlaying,
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

/// A local playlist's identity, for the DJ Gem context snapshot (no track payload).
#[derive(Debug, Clone)]
pub struct PlaylistInfo {
    pub id: String,
    pub name: String,
    pub count: usize,
}

/// A read-only snapshot of app state handed to the DJ Gem actor with each prompt, so its
/// read tools (get_queue, get_user_favorites, …) can answer without touching `App`.
#[derive(Debug, Clone)]
pub struct AiContext {
    /// "Title — Artist" of the current track, if any.
    pub current_track: Option<String>,
    /// The currently loaded live radio station, if the current queue item is a station.
    pub current_radio_station: Option<String>,
    /// The stream's own now-playing metadata for the current radio station, if mpv has seen it.
    pub current_radio_now_playing: Option<String>,
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
    /// Search/source settings used by DJ Gem streaming tools.
    pub search: SearchConfig,
    /// Whether a YTM cookie is configured (gates authenticated related-tracks).
    pub authenticated: bool,
    pub autoplay_streaming: bool,
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

/// One reranked pick the DJ Gem returned: the opaque pack `cid` it chose, plus optional explanation
/// (slot role + reason codes) surfaced by the "Why DJ Gem" overlay.
#[derive(Debug, Clone)]
pub struct AiPick {
    pub cid: String,
    pub role: Option<String>,
    pub reasons: Vec<String>,
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

/// Synced lyrics for one track (held while it's the current track).
pub struct TrackLyrics {
    pub video_id: String,
    pub lines: Vec<LyricLine>,
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
