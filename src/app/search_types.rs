//! Search-domain messages and effects kept behind one top-level reducer wrapper.

use crate::api::{ArtistIntent, ArtistPage, PlaylistIntent, Song};
use crate::search_source::{SearchConfig, SearchSource};

/// Search-domain results delivered to the app reducer.
pub enum SearchMsg {
    /// Search returned results (possibly empty) for `query`.
    Results {
        request_id: u64,
        query: String,
        source: SearchSource,
        songs: Vec<Song>,
        /// The multi-source operation deadline dropped one or more sources (partial results).
        timed_out: bool,
    },
    /// Search failed.
    Error {
        request_id: u64,
        source: SearchSource,
        error: String,
    },
    /// A remote playlist's tracks arrived (answering [`SearchCmd::PlaylistTracks`]).
    PlaylistTracks {
        title: String,
        intent: PlaylistIntent,
        songs: Vec<Song>,
    },
    /// Fetching a remote playlist's tracks failed.
    PlaylistTracksError { title: String, error: String },
    /// An artist's page arrived (answering [`SearchCmd::ArtistPage`] with intent `Open`).
    ArtistPage { page: ArtistPage },
    /// Fetching an artist's page (or its songs playlist) failed.
    ArtistPageError { title: String, error: String },
}

/// Search-domain effects dispatched by the runtime.
pub enum SearchCmd {
    /// Search tracks using the selected source and provider configuration.
    Query {
        request_id: u64,
        query: String,
        source: SearchSource,
        config: SearchConfig,
    },
    /// Search public YouTube playlists by name.
    Playlists { request_id: u64, query: String },
    /// Search YouTube Music artists by name.
    Artists { request_id: u64, query: String },
    /// Fetch a remote playlist's full track list, then apply `intent` to it.
    PlaylistTracks {
        playlist_id: String,
        title: String,
        intent: PlaylistIntent,
    },
    /// Fetch an artist's page (top songs + albums), then apply `intent` to it.
    ArtistPage {
        channel_id: String,
        title: String,
        intent: ArtistIntent,
    },
}
