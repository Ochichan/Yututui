//! Semantic owners of rendered scroll tracks.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollSurface {
    Library,
    Search,
    LocalFind,
    /// The search results-filter popup's list.
    SearchFilter,
    /// The artist detail screen's top-songs list.
    ArtistSongs,
    /// The artist detail screen's albums/singles list.
    ArtistAlbums,
    AiTranscript,
    AiSuggestions,
    Settings,
    Queue,
    /// The radio "now playing" (지듣노) card's title line — marquee-only, no scrollbar.
    NowPlaying,
    /// The player/mini/docked title row — marquee-only, no scrollbar.
    PlayerTitle,
}
