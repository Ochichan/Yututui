use crate::search_source::SearchConfig;

/// A local playlist's identity for an AI context snapshot (no track payload).
#[derive(Debug, Clone)]
pub struct PlaylistInfo {
    pub id: String,
    pub name: String,
    pub count: usize,
}

/// A read-only owner-state snapshot handed to the DJ Gem actor with each prompt.
///
/// Keeping this DTO in the AI domain lets both the interactive app and the daemon
/// construct snapshots without making the leaf actor depend on either owner.
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
    /// Whether repeat is currently enabled. Mutation tools use this snapshot to reject an
    /// incompatible `start_streaming` before doing network or queue work; owners revalidate on
    /// reduction in case playback mode changed while the AI turn was in flight.
    pub repeat_on: bool,
}

/// One reranked pick the DJ Gem returned: the opaque pack `cid` it chose, plus optional
/// explanation surfaced by the "Why DJ Gem" overlay.
#[derive(Debug, Clone)]
pub struct AiPick {
    pub cid: String,
    pub role: Option<String>,
    pub reasons: Vec<String>,
}
