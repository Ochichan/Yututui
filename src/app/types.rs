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
    /// mpv playback position, in seconds.
    PlayerTimePos(f64),
    /// Current track duration, in seconds.
    PlayerDuration(f64),
    /// mpv pause state changed.
    PlayerPaused(bool),
    /// mpv volume changed (0-100, but mpv can report fractional/over-100 values).
    PlayerVolume(f64),
    /// mpv stream metadata changed. Live radio streams often expose ICY now-playing titles here.
    PlayerMetadata(serde_json::Value),
    /// mpv `demuxer-cache-time`: the newest demuxed timestamp (≈ the live edge on a radio
    /// stream), or `None` when the property became unavailable.
    PlayerCacheTime(Option<f64>),
    /// The current track reached its end.
    PlayerEof,
    /// mpv reported a playback error.
    PlayerError(String),
    /// An event from the video-overlay mpv's IPC client, tagged with the spawn
    /// generation it was connected for — the reducer drops events from a window it
    /// already closed (`v`) or respawned (`Shift+V`).
    VideoOverlay {
        generation: u64,
        event: crate::player::video::VideoEvent,
    },
    /// Search returned results (possibly empty) for `query`.
    SearchResults {
        query: String,
        source: SearchSource,
        songs: Vec<Song>,
    },
    /// Search failed.
    SearchError {
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
    /// A track's direct stream URL was prefetched (for instant skip).
    Resolved {
        video_id: String,
        stream_url: String,
    },
    /// Related tracks returned by the non-DJ Gem streaming fallback, each tagged with the source it
    /// came from (real YTM watch-playlist vs anonymous yt-dlp search) so the local engine can
    /// weight provenance and prefer the better source on dedup.
    StreamingResults {
        seed_video_id: String,
        candidates: Vec<(Song, CandidateSource)>,
    },
    /// Final streaming picks after the API actor has run any needed metadata preflight. This is the
    /// last gate before enqueueing; it can drop risky public-YouTube candidates and top up from
    /// fallback picks.
    StreamingPreflighted {
        seed_video_id: String,
        songs: Vec<Song>,
    },
    /// The non-DJ Gem streaming fallback failed to fetch related tracks.
    StreamingError {
        seed_video_id: String,
        error: String,
    },
    /// The DJ Gem reranker's chosen picks (best-first), or empty on any failure. Each pick is an
    /// opaque pack `cid`; the reducer resolves cids→tracks via the stashed `cid_map`, validates
    /// against the shortlist, and tops up from the local pick.
    StreamingAiPicks {
        seed_video_id: String,
        picks: Vec<AiPick>,
        /// The model's self-reported confidence in [0,1], if it returned one.
        conf: Option<f32>,
    },

    // DJ Gem assistant: intents emitted by the DJ Gem actor, applied here by `update()`.
    /// The assistant started/finished a turn (drives the thinking spinner).
    AiThinking(bool),
    /// Assistant chat text to append to the transcript.
    AiChat(String),
    /// An DJ Gem error to surface in the transcript (also clears the spinner).
    AiError(String),
    /// Replace the queue with these tracks and start playing (play_music/play_playlist).
    AiPlayTracks(Vec<Song>),
    /// Append these tracks to the queue (add_to_queue/start_streaming).
    AiEnqueue(Vec<Song>),
    /// Populate the pickable related-tracks list (get_suggestions).
    AiSuggestions(Vec<Song>),
    /// Turn autoplay/streaming on or off (start_streaming/stop_streaming).
    AiSetAutoplay(bool),
    /// Shape the active station from a free-text vibe (start_streaming with explore/avoid hints):
    /// set the adventurousness and the artists to keep out. `explore` is the model's raw string
    /// (tight/balanced/wide or a synonym), parsed leniently.
    AiSetStationProfile {
        query: String,
        explore: Option<String>,
        avoid_artists: Vec<String>,
    },
    /// Create a local playlist with this name (create_playlist).
    AiCreatePlaylist(String),
    /// Add these tracks to a local playlist by id or name (add_to_playlist).
    AiAddToPlaylist {
        playlist: String,
        songs: Vec<Song>,
    },
    /// Play a local playlist by id or name (play_playlist).
    AiPlayPlaylist(String),
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
    /// Result of an off-path feedback summary (see [`Cmd::SummarizeFeedback`]): artists the
    /// listener kept skipping vs. warmed to, folded into the active station's avoid list. Always
    /// delivered (empty on failure) so the in-flight guard clears.
    StationPatch {
        down_artists: Vec<String>,
        boost_artists: Vec<String>,
    },
    /// Result of a batch title/artist romanization request. Empty `entries` means Gemini failed or
    /// produced nothing usable; `keys` still clears the reducer's in-flight guard for those tracks.
    RomanizedTitles {
        request_id: u64,
        keys: Vec<String>,
        entries: Vec<RomanizedResult>,
    },
    /// Result of a "what's playing" identify one-shot (see [`Cmd::IdentifyNowPlaying`]).
    /// Always delivered — `Err` carries a short user-safe message — so the overlay can
    /// never stick on Loading. Dropped unless `seq` matches the open overlay's epoch.
    NowPlayingIdentified {
        seq: u64,
        result: Result<IdentifiedNowPlaying, String>,
    },
    /// Best-match tracks answering [`Cmd::ResolveTrack`] (possibly empty). Dropped
    /// unless `seq` matches the overlay's pending resolve epoch.
    TrackResolved {
        seq: u64,
        result: Result<Vec<Song>, String>,
    },
    /// An event from the scrobble actor: auth-flow progress or a service-health notice.
    /// Scrobbling itself is fire-and-forget and never surfaces here.
    Scrobble(crate::scrobble::ScrobbleEvent),
    /// An event from the transfer actor: Spotify auth, playlist listings, job progress.
    Transfer(crate::transfer::actor::TransferEvent),
}

/// Side effects the reducer asks the run loop to perform.
pub enum Cmd {
    Player(PlayerCmd),
    /// Connect the IPC client for a freshly spawned video-overlay mpv.
    VideoConnect {
        ipc_path: String,
        generation: u64,
    },
    /// `loadfile <url> replace` into the live overlay window (auto-continue).
    VideoLoad(String),
    Search {
        query: String,
        source: SearchSource,
        config: SearchConfig,
    },
    /// Search public YouTube playlists by name (the search box's playlist kind).
    SearchPlaylists {
        query: String,
    },
    /// Fetch a remote playlist's full track list, then apply `intent` to it.
    FetchPlaylistTracks {
        playlist_id: String,
        title: String,
        intent: crate::api::PlaylistIntent,
    },
    /// Persist the library (song favorites/history and radio stations) to disk.
    SaveLibrary,
    /// Persist the downloads manifest (completed downloads' YouTube identity) to disk.
    SaveDownloads,
    /// Persist the per-track preference signals (plays/skips/dislikes) to disk.
    SaveSignals,
    /// Persist the Latin-script title display cache to disk.
    SaveRomanizedTitles,
    /// Delete the persisted Latin-script title display cache from disk.
    ClearRomanizedTitles,
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
    /// Persist the given config to disk (settings screen, on save).
    SaveConfig(Box<Config>),
    /// Persist the local playlists to disk (after a DJ Gem playlist mutation).
    SavePlaylists,
    /// Persist the active natural-language station profile to disk (after vibe-shaped streaming).
    SaveStationProfile,
    /// Off-path: ask the assistant to distill a recent-feedback digest into artists to avoid /
    /// re-allow for the active station. The result returns as [`Msg::StationPatch`].
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
    /// One-shot "what's playing" identification of a radio stream title, pinned to the
    /// LOWEST model tier (Flash-Lite, no fallback), no tools, JSON-only. The result
    /// returns as [`Msg::NowPlayingIdentified`] with the same `seq`.
    IdentifyNowPlaying {
        seq: u64,
        station: String,
        raw_title: String,
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
    /// returns as [`Msg::StreamingAiPicks`]; failure degrades to the stashed local pick.
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
    /// Confirm button on the "delete downloaded files" modal.
    ConfirmDelete,
    /// Cancel button on the "delete downloaded files" modal.
    CancelDelete,
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
    AiTranscript,
    AiSuggestions,
    Settings,
    Queue,
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

/// A streaming rerank handed to the DJ Gem actor, kept until its `Msg::StreamingAiPicks` returns. The
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
/// overlay (the `w` key). Built when [`Msg::StreamingAiPicks`] resolves — the model's opaque cids are
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

/// What one radio stream title turned out to be, per the identify one-shot's `kind`
/// classification (the model's finer jingle/station-id classes fold into `Ad`: station
/// content, not a song).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifiedKind {
    Song,
    Ad,
    Unknown,
}

/// The identify one-shot's input-clarity confidence. An enum, not a probability — small
/// models verbalize numeric confidence badly (systematic overconfidence), so the rubric
/// classifies the *input* instead: high = clean `Artist - Title`, medium = a song is
/// present but the split/order is ambiguous, low = fragmentary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifyConfidence {
    High,
    Medium,
    Low,
}

/// A "what's playing" identification extracted from a radio stream's ICY title by the
/// FlashLite one-shot ([`Cmd::IdentifyNowPlaying`]). Both name fields are extractions
/// from the (untrusted) stream text — never model recall — and stay untrusted data
/// wherever they flow (overlay, DJ Gem seed, search query).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifiedNowPlaying {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub kind: IdentifiedKind,
    pub confidence: IdentifyConfidence,
    /// One short model sentence about ambiguity/anomalies (shown muted), if any.
    pub note: Option<String>,
}

/// What the "what's playing" overlay is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NowPlayingOverlayState {
    /// The identify call is in flight.
    Loading,
    /// The station exposes no usable ICY title — nothing to identify, no API call made.
    NoMetadata,
    Identified(IdentifiedNowPlaying),
    /// The identify call failed (short user-safe message). Never cached as a result.
    Error(String),
}

/// The "what's playing" (지듣노) overlay: a compact card over the player identifying the
/// current radio song, with favorite / ask-DJ Gem actions. `None` on [`crate::app::App`]
/// = closed. Snapshots the station + sanitized title it was opened for, so replies and
/// actions stay pinned to that moment even if the stream moves on underneath.
#[derive(Debug, Clone)]
pub struct NowPlayingOverlay {
    /// Identify epoch — a reply must match [`crate::app::App`]'s live counter via this
    /// snapshot or it's stale (overlay closed / title changed).
    pub seq: u64,
    /// The station `Song::video_id`, keying the identify cache.
    pub station_id: String,
    /// The station's display label (also fed to the identify prompt).
    pub station_label: String,
    /// The sanitized ICY title this overlay is about.
    pub raw_title: String,
    pub state: NowPlayingOverlayState,
    /// The YouTube track the favorite action resolved to (attached after the first
    /// search so a repeat favorite / re-open never re-searches).
    pub resolved: Option<Song>,
    /// A favorite resolve is in flight (debounces the button).
    pub resolving: bool,
    /// Resolve epoch, same staleness contract as `seq`.
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
