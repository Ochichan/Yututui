//! Actor command/event protocol and bounded request handle.

use tokio::sync::mpsc::{Sender, error::TrySendError};

use crate::search_source::{SearchConfig, SearchSource};
use crate::streaming::{CandidateSource, StreamingConfig, StreamingMode};

use super::domain::ArtistPage;
use super::{GuiSearchGroup, GuiSearchRequestId, Song};

/// Which auth mode the live API client ended up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiMode {
    Authenticated,
    Anonymous,
}

/// What the reducer wants done with a remote playlist's tracks once they arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistIntent {
    /// Replace the queue with the playlist and start playing.
    Play,
    /// Append the playlist's tracks to the queue.
    Enqueue,
    /// Save the playlist as a local playlist (named after it).
    Import,
}

/// What the reducer wants done with an artist row once its page arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtistIntent {
    /// Open the artist detail screen.
    Open,
    /// Append the artist's songs playlist to the queue.
    Enqueue,
    /// Save the artist's songs playlist as a local playlist (named after the artist).
    Import,
}

/// Commands the reducer sends to the API actor.
pub enum ApiCmd {
    Search {
        request_id: u64,
        query: String,
        source: SearchSource,
        config: SearchConfig,
    },
    /// A GUI-session search (`RemoteCommand::RunSearch`): per-catalog result groups,
    /// answered as [`ApiEvent::GuiSearchCompleted`] through an opaque request id. Requester
    /// routing remains in the daemon owner, and this independent lane never disturbs the TUI
    /// Search screen's [`ApiEvent::SearchResults`].
    GuiSearch {
        request_id: GuiSearchRequestId,
        query: String,
        source: SearchSource,
        config: SearchConfig,
    },
    Streaming {
        request_id: u64,
        seed: String,
        seed_video_id: String,
        exclude_ids: Vec<String>,
        limit: usize,
        mode: StreamingMode,
        config: SearchConfig,
    },
    StreamingPreflight {
        request_id: u64,
        seed_video_id: String,
        picks: Vec<Song>,
        fallback: Vec<Song>,
        mode: StreamingMode,
        config: StreamingConfig,
    },
    /// Resolve free-text artist/title (e.g. an AI-identified radio song) to real YouTube
    /// tracks, WITHOUT touching the Search screen — the reply is its own event, keyed by
    /// `seq`, not [`ApiEvent::SearchResults`].
    ResolveTrack {
        seq: u64,
        query: String,
        config: SearchConfig,
    },
    /// Search public YouTube playlists by name (YouTube-only; other providers have no
    /// playlist catalog here). Results return as ordinary [`ApiEvent::SearchResults`]
    /// rows whose ids carry [`crate::api::PLAYLIST_ID_PREFIX`].
    SearchPlaylists { request_id: u64, query: String },
    /// Search YouTube Music artists by name (YouTube-only). Results return as ordinary
    /// [`ApiEvent::SearchResults`] rows whose ids carry [`crate::api::ARTIST_ID_PREFIX`].
    SearchArtists { request_id: u64, query: String },
    /// Fetch a remote playlist's full track list; `title` and `intent` ride along so the
    /// reducer knows what to do with the answer.
    PlaylistTracks {
        playlist_id: String,
        title: String,
        intent: PlaylistIntent,
    },
    /// Fetch an artist's page. `Open` answers as [`ApiEvent::ArtistPage`]; `Enqueue`/`Import`
    /// chain into the artist's songs playlist and answer as [`ApiEvent::PlaylistTracks`],
    /// reusing the playlist-row reducer path.
    ArtistPage {
        channel_id: String,
        title: String,
        intent: ArtistIntent,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiCommandKind {
    Search,
    GuiSearch,
    Streaming,
    StreamingPreflight,
    ResolveTrack,
    SearchPlaylists,
    SearchArtists,
    PlaylistTracks,
    ArtistPage,
}

impl ApiCommandKind {
    fn label(self) -> &'static str {
        match self {
            ApiCommandKind::Search => "search",
            ApiCommandKind::GuiSearch => "GUI search",
            ApiCommandKind::Streaming => "streaming",
            ApiCommandKind::StreamingPreflight => "streaming preflight",
            ApiCommandKind::ResolveTrack => "track resolve",
            ApiCommandKind::SearchPlaylists => "playlist search",
            ApiCommandKind::SearchArtists => "artist search",
            ApiCommandKind::PlaylistTracks => "playlist tracks",
            ApiCommandKind::ArtistPage => "artist page",
        }
    }

    pub(super) fn lane(self) -> ApiLane {
        match self {
            ApiCommandKind::Search
            | ApiCommandKind::GuiSearch
            | ApiCommandKind::ResolveTrack
            | ApiCommandKind::SearchPlaylists
            | ApiCommandKind::SearchArtists => ApiLane::Interactive,
            ApiCommandKind::Streaming
            | ApiCommandKind::StreamingPreflight
            | ApiCommandKind::PlaylistTracks
            | ApiCommandKind::ArtistPage => ApiLane::Bulk,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ApiLane {
    Interactive,
    Bulk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiEnqueueError {
    Full { kind: ApiCommandKind },
    Closed { kind: ApiCommandKind },
}

impl ApiEnqueueError {
    pub fn kind(self) -> ApiCommandKind {
        match self {
            ApiEnqueueError::Full { kind } | ApiEnqueueError::Closed { kind } => kind,
        }
    }
}

impl std::fmt::Display for ApiEnqueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            ApiEnqueueError::Full { kind } => {
                write!(
                    f,
                    "API {} queue is full; try again in a moment.",
                    kind.label()
                )
            }
            ApiEnqueueError::Closed { kind } => {
                write!(f, "API {} worker is not running.", kind.label())
            }
        }
    }
}

impl std::error::Error for ApiEnqueueError {}

impl ApiCmd {
    pub(super) fn kind(&self) -> ApiCommandKind {
        match self {
            ApiCmd::Search { .. } => ApiCommandKind::Search,
            ApiCmd::GuiSearch { .. } => ApiCommandKind::GuiSearch,
            ApiCmd::Streaming { .. } => ApiCommandKind::Streaming,
            ApiCmd::StreamingPreflight { .. } => ApiCommandKind::StreamingPreflight,
            ApiCmd::ResolveTrack { .. } => ApiCommandKind::ResolveTrack,
            ApiCmd::SearchPlaylists { .. } => ApiCommandKind::SearchPlaylists,
            ApiCmd::SearchArtists { .. } => ApiCommandKind::SearchArtists,
            ApiCmd::PlaylistTracks { .. } => ApiCommandKind::PlaylistTracks,
            ApiCmd::ArtistPage { .. } => ApiCommandKind::ArtistPage,
        }
    }
}

/// Events emitted by the API actor.
pub enum ApiEvent {
    ModeResolved {
        mode: ApiMode,
        had_cookie: bool,
    },
    SearchResults {
        request_id: u64,
        query: String,
        source: SearchSource,
        songs: Vec<Song>,
        /// The multi-source operation deadline dropped one or more sources; the Search screen
        /// shows a subtle indicator so partial results don't look like the full set.
        timed_out: bool,
    },
    SearchError {
        request_id: u64,
        source: SearchSource,
        error: String,
    },
    StreamingResults {
        request_id: u64,
        seed_video_id: String,
        candidates: Vec<(Song, CandidateSource)>,
    },
    StreamingPreflighted {
        request_id: u64,
        seed_video_id: String,
        songs: Vec<Song>,
    },
    StreamingError {
        request_id: u64,
        seed_video_id: String,
        error: String,
    },
    /// Best-match tracks answering [`ApiCmd::ResolveTrack`] (possibly empty).
    TrackResolved {
        seq: u64,
        result: Result<Vec<Song>, String>,
    },
    /// A remote playlist's tracks, answering [`ApiCmd::PlaylistTracks`].
    PlaylistTracks {
        title: String,
        intent: PlaylistIntent,
        songs: Vec<Song>,
    },
    /// Fetching a remote playlist's tracks failed.
    PlaylistTracksError {
        title: String,
        error: String,
    },
    /// An artist's page, answering [`ApiCmd::ArtistPage`] with [`ArtistIntent::Open`].
    ArtistPage {
        page: ArtistPage,
    },
    /// Fetching an artist's page (or its songs playlist) failed.
    ArtistPageError {
        title: String,
        error: String,
    },
    /// Per-catalog groups answering [`ApiCmd::GuiSearch`], correlated only by an opaque id.
    GuiSearchCompleted {
        request_id: GuiSearchRequestId,
        groups: Vec<GuiSearchGroup>,
    },
}

/// Handle for issuing API requests; results return as [`ApiEvent`]s.
pub struct ApiHandle {
    pub(super) interactive_tx: Sender<ApiCmd>,
    pub(super) bulk_tx: Sender<ApiCmd>,
}

impl ApiHandle {
    #[cfg(test)]
    pub(crate) fn from_test_senders(
        interactive_tx: Sender<ApiCmd>,
        bulk_tx: Sender<ApiCmd>,
    ) -> Self {
        Self {
            interactive_tx,
            bulk_tx,
        }
    }

    fn enqueue(&self, cmd: ApiCmd) -> Result<(), ApiEnqueueError> {
        let tx = match cmd.kind().lane() {
            ApiLane::Interactive => &self.interactive_tx,
            ApiLane::Bulk => &self.bulk_tx,
        };
        tx.try_send(cmd).map_err(|err| match err {
            TrySendError::Full(cmd) => ApiEnqueueError::Full { kind: cmd.kind() },
            TrySendError::Closed(cmd) => ApiEnqueueError::Closed { kind: cmd.kind() },
        })
    }

    pub fn search(
        &self,
        request_id: u64,
        query: impl Into<String>,
        source: SearchSource,
        config: SearchConfig,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::Search {
            request_id,
            query: query.into(),
            source,
            config,
        })
    }

    pub fn gui_search(
        &self,
        request_id: GuiSearchRequestId,
        query: impl Into<String>,
        source: SearchSource,
        config: SearchConfig,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::GuiSearch {
            request_id,
            query: query.into(),
            source,
            config,
        })
    }

    // This actor-boundary method mirrors the typed command field-for-field so correlation and
    // seed context cannot be accidentally dropped at call sites.
    #[allow(clippy::too_many_arguments)]
    pub fn streaming(
        &self,
        request_id: u64,
        seed: impl Into<String>,
        seed_video_id: impl Into<String>,
        exclude_ids: Vec<String>,
        limit: usize,
        mode: StreamingMode,
        config: SearchConfig,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::Streaming {
            request_id,
            seed: seed.into(),
            seed_video_id: seed_video_id.into(),
            exclude_ids,
            limit,
            mode,
            config,
        })
    }

    pub fn streaming_preflight(
        &self,
        request_id: u64,
        seed_video_id: impl Into<String>,
        picks: Vec<Song>,
        fallback: Vec<Song>,
        mode: StreamingMode,
        config: StreamingConfig,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::StreamingPreflight {
            request_id,
            seed_video_id: seed_video_id.into(),
            picks,
            fallback,
            mode,
            config,
        })
    }

    pub fn resolve_track(
        &self,
        seq: u64,
        query: impl Into<String>,
        config: SearchConfig,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::ResolveTrack {
            seq,
            query: query.into(),
            config,
        })
    }

    pub fn search_playlists(
        &self,
        request_id: u64,
        query: impl Into<String>,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::SearchPlaylists {
            request_id,
            query: query.into(),
        })
    }

    pub fn playlist_tracks(
        &self,
        playlist_id: impl Into<String>,
        title: impl Into<String>,
        intent: PlaylistIntent,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::PlaylistTracks {
            playlist_id: playlist_id.into(),
            title: title.into(),
            intent,
        })
    }

    pub fn search_artists(
        &self,
        request_id: u64,
        query: impl Into<String>,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::SearchArtists {
            request_id,
            query: query.into(),
        })
    }

    pub fn artist_page(
        &self,
        channel_id: impl Into<String>,
        title: impl Into<String>,
        intent: ArtistIntent,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::ArtistPage {
            channel_id: channel_id.into(),
            title: title.into(),
            intent,
        })
    }
}
