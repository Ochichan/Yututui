//! YouTube Music data layer. Search is served by ytmapi-rs when authenticated and by
//! yt-dlp (`ytsearch`) when anonymous — see [`ytmusic`]. A `MusicApi` trait + raw-Innertube
//! fallback arrive when home/charts need endpoints ytmapi-rs lacks.

mod actor;
mod domain;
mod gui_search;
mod protocol;
pub mod ytmusic;

#[cfg(test)]
mod hardening_tests;

pub use crate::playback_target::{
    PlayableUrlError, validate_playable_url, validate_playable_url_destination,
    validate_playback_target_for_handoff,
};
pub use actor::spawn;
pub use domain::{
    ARTIST_ID_PREFIX, ArtistPage, MAX_ALBUM_CHARS, MAX_ARTIST_CHARS, MAX_DURATION_CHARS,
    MAX_ORIGIN_URL_CHARS, MAX_PROVIDER_ID_CHARS, MAX_TITLE_CHARS, PLAYLIST_ID_PREFIX, PlayableRef,
    Song, SongImportMetadata, is_youtube_video_id, sanitize_album, sanitize_artist,
    sanitize_duration, sanitize_metadata_text, sanitize_provider_id, sanitize_title,
};
pub(crate) use gui_search::gui_search_row_id;
pub use gui_search::{GUI_SEARCH_ROW_ID_MAX_BYTES, GuiSearchGroup, GuiSearchRequestId};
pub use protocol::{
    ApiCmd, ApiCommandKind, ApiEnqueueError, ApiEvent, ApiHandle, ApiMode, ArtistIntent,
    PlaylistIntent,
};

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::time::{Duration, Instant};

#[cfg(test)]
use tokio::sync::mpsc::{self, Receiver};

#[cfg(test)]
use crate::playback_target::MAX_PLAYABLE_URL_BYTES;
#[cfg(test)]
use crate::search_source::{SearchConfig, SearchSource};
#[cfg(test)]
use crate::streaming::{StreamingConfig, StreamingMode};

#[cfg(test)]
use actor::{STREAMING_YTDLP_CACHE_MAX, enforce_streaming_cache_cap};
#[cfg(test)]
use domain::is_forbidden_metadata_char;
#[cfg(test)]
use protocol::ApiLane;

#[cfg(test)]
mod tests;
