//! Message, command, and view-state type definitions for the app reducer.
//!
//! Split out of the former monolithic `app.rs` (behaviour-preserving move). These are
//! re-exported from `crate::app` (`pub use types::*`) so existing `crate::app::Msg` /
//! `crate::app::Cmd` / `crate::app::Mode` paths keep resolving for actors and views.

use super::*;

/// Everything that can change the application state.
pub enum Msg {
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
    /// A right-click at a cell — adds the song row under the pointer to the queue (the mouse
    /// equivalent of `\`). Acts only on Search/Library list rows; ignored elsewhere.
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
    /// for lists, and volume-up over the player volume cluster.
    MouseScroll {
        up: bool,
        col: u16,
        row: u16,
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
    /// The current track reached its end.
    PlayerEof,
    /// mpv reported a playback error.
    PlayerError(String),
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
    /// Related tracks returned by the non-DJ Gem radio fallback, each tagged with the source it
    /// came from (real YTM watch-playlist vs anonymous yt-dlp search) so the local engine can
    /// weight provenance and prefer the better source on dedup.
    RadioResults {
        seed_video_id: String,
        candidates: Vec<(Song, CandidateSource)>,
    },
    /// Final radio picks after the API actor has run any needed metadata preflight. This is the
    /// last gate before enqueueing; it can drop risky public-YouTube candidates and top up from
    /// fallback picks.
    RadioPreflighted {
        seed_video_id: String,
        songs: Vec<Song>,
    },
    /// The non-DJ Gem radio fallback failed to fetch related tracks.
    RadioError {
        seed_video_id: String,
        error: String,
    },
    /// The DJ Gem reranker's chosen picks (best-first), or empty on any failure. Each pick is an
    /// opaque pack `cid`; the reducer resolves cids→tracks via the stashed `cid_map`, validates
    /// against the shortlist, and tops up from the local pick.
    RadioAiPicks {
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
    /// Append these tracks to the queue (add_to_queue/start_radio).
    AiEnqueue(Vec<Song>),
    /// Populate the pickable related-tracks list (get_suggestions).
    AiSuggestions(Vec<Song>),
    /// Turn autoplay/radio on or off (start_radio/stop_radio).
    AiSetAutoplay(bool),
    /// Shape the active station from a free-text vibe (start_radio with explore/avoid hints):
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
}

/// Side effects the reducer asks the run loop to perform.
pub enum Cmd {
    Player(PlayerCmd),
    Search {
        query: String,
        source: SearchSource,
        config: SearchConfig,
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
    /// Persist the active natural-language station profile to disk (after a vibe-shaped radio).
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
    /// Ask the anonymous API/search actor for related tracks to keep radio going without DJ Gem.
    RadioFallback {
        seed: String,
        seed_video_id: String,
        exclude_ids: Vec<String>,
        mode: RadioMode,
    },
    /// Ask the API actor to run a final metadata preflight on radio picks before enqueueing.
    /// Only risky candidates trigger full yt-dlp extraction; clean picks pass through.
    RadioPreflight {
        seed_video_id: String,
        picks: Vec<Song>,
        fallback: Vec<Song>,
        mode: RadioMode,
        config: radio::RadioConfig,
    },
    /// Hand a local candidate shortlist to the DJ Gem actor to rerank (ids only). The result
    /// returns as [`Msg::RadioAiPicks`]; failure degrades to the stashed local pick.
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
    /// Open/close the radio-mode dropdown on the player status line (clicking the `radio:` label).
    RadioMenu,
    /// Pick a radio mode from the open dropdown.
    RadioSelect(RadioMode),
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
    /// The `N/M` queue-position label on the player status line — opens the queue window.
    QueuePos,
    /// A row in the open queue window, by order position. Single-click selects; double-click
    /// jumps playback to it.
    QueueRow(usize),
    /// The `✗` delete button on a queue-window row, by order position.
    QueueDel(usize),
    /// The `✗` delete button on a Library list row, by row index in the current tab.
    LibraryDel(usize),
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

/// A radio rerank handed to the DJ Gem actor, kept until its `Msg::RadioAiPicks` returns. The
/// `shortlist` is the exact set the model was shown — every returned id is validated against
/// it (so a hallucinated id is dropped) — and `local_pick` is the guaranteed fallback ordering
/// the engine produced, used to top up any slots the DJ Gem left empty.
pub(crate) struct PendingRerank {
    pub(crate) seed_video_id: String,
    pub(crate) mode: RadioMode,
    pub(crate) shortlist: Vec<Song>,
    pub(crate) local_pick: Vec<Song>,
    /// Maps each pack `cid` shown to the model back to its track's video id, so the DJ Gem's chosen
    /// cids can be resolved to playable tracks before validation.
    pub(crate) cid_map: Vec<crate::radio::PackedCand>,
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

/// The resolved, human-readable explanation of the last DJ Gem radio rerank, shown by the "Why DJ Gem"
/// overlay (the `w` key). Built when [`Msg::RadioAiPicks`] resolves — the model's opaque cids are
/// mapped back to real tracks (title + artist) while [`PendingRerank`] is still in hand — so the
/// overlay can render it long after the pending rerank has been consumed.
#[derive(Debug, Clone, Default)]
pub(crate) struct RadioAiExplain {
    /// The model's self-reported confidence in [0,1], if any.
    pub(crate) conf: Option<f32>,
    /// The picks the model chose, in its best-first order (hallucinated cids already dropped).
    pub(crate) picks: Vec<ExplainPick>,
}

/// One resolved pick in a [`RadioAiExplain`]: the track it landed on plus the model's stated
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
/// [`crate::app::RadioRuntime::session_events`]). Feeds the DJ Gem reranker's *recovery context* — a
/// skip → widen and avoid the skipped artist, a like → stay close — so the model reacts to the
/// arc of the session, not just the aggregate per-track signals the engine already folds in.
#[derive(Debug, Clone)]
pub(crate) struct SessionEvent {
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
}

impl LibraryTab {
    pub const NORMAL: [Self; 4] = [Self::All, Self::Favorites, Self::History, Self::Downloads];

    pub const RADIO_MODE: [Self; 2] = [Self::RadioFavorites, Self::Radio];

    pub fn label(self) -> &'static str {
        match self {
            LibraryTab::All => t!("All", "전체"),
            LibraryTab::Favorites => t!("Favorites", "즐겨찾기"),
            LibraryTab::History => t!("History", "기록"),
            LibraryTab::RadioFavorites => t!("Radio Likes", "라디오 좋아요"),
            LibraryTab::Radio => t!("Radio History", "라디오 히스토리"),
            LibraryTab::Downloads => t!("Downloads", "다운로드"),
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

/// Within the search screen, whether the query box or the results list has focus.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum SearchFocus {
    #[default]
    Input,
    Results,
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
