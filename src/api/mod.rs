//! YouTube Music data layer. Search is served by ytmapi-rs when authenticated and by
//! yt-dlp (`ytsearch`) when anonymous — see [`ytmusic`]. A `MusicApi` trait + raw-Innertube
//! fallback arrive when home/charts need endpoints ytmapi-rs lacks.

mod gui_search;
pub mod ytmusic;

#[cfg(test)]
mod hardening_tests;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, Receiver, Sender, error::TrySendError};

use crate::search_source::{SearchConfig, SearchSource};
use crate::streaming::{CandidateSource, StreamingConfig, StreamingMode};
use crate::util::sanitize;

pub use crate::playback_target::{
    PlayableUrlError, validate_playable_url, validate_playable_url_destination,
    validate_playback_target_for_handoff,
};
pub(crate) use gui_search::gui_search_row_id;
pub use gui_search::{GUI_SEARCH_ROW_ID_MAX_BYTES, GuiSearchGroup, GuiSearchRequestId};

#[cfg(test)]
use crate::playback_target::MAX_PLAYABLE_URL_BYTES;

const STREAMING_YTDLP_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const STREAMING_YTDLP_CACHE_MAX: usize = 512;
const API_INTERACTIVE_QUEUE: usize = 256;
const API_BULK_QUEUE: usize = 256;
pub const MAX_TITLE_CHARS: usize = 300;
pub const MAX_ARTIST_CHARS: usize = 200;
pub const MAX_ALBUM_CHARS: usize = 200;
pub const MAX_DURATION_CHARS: usize = 32;
pub const MAX_PROVIDER_ID_CHARS: usize = 256;
pub const MAX_ORIGIN_URL_CHARS: usize = 512;

pub fn is_youtube_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn is_definitely_non_video_youtube_ref(id: &str) -> bool {
    if is_youtube_video_id(id) {
        return false;
    }
    let known_prefix = id.starts_with(PLAYLIST_ID_PREFIX)
        || id.starts_with("UC")
        || id.starts_with("UU")
        || id.starts_with("PL")
        || id.starts_with("VL")
        || id.starts_with("RD")
        || id.starts_with("MPRE")
        || id.starts_with("OLAK5uy");
    known_prefix
        || id.bytes().any(|b| {
            !(b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b':' || b == b'.')
        })
}

pub fn sanitize_title(raw: &str) -> String {
    sanitize_metadata_text(raw, MAX_TITLE_CHARS)
}

pub fn sanitize_artist(raw: &str) -> String {
    sanitize_metadata_text(raw, MAX_ARTIST_CHARS)
}

pub fn sanitize_album(raw: &str) -> String {
    sanitize_metadata_text(raw, MAX_ALBUM_CHARS)
}

pub fn sanitize_duration(raw: &str) -> String {
    sanitize_metadata_text(raw, MAX_DURATION_CHARS)
}

pub fn sanitize_provider_id(raw: &str) -> String {
    sanitize_metadata_text(raw, MAX_PROVIDER_ID_CHARS)
}

fn sanitize_optional_metadata(raw: Option<String>, max_chars: usize) -> Option<String> {
    raw.map(|value| sanitize_metadata_text(&value, max_chars))
        .filter(|value| !value.trim().is_empty())
}

fn sanitize_metadata_vec(raw: Vec<String>, max_chars: usize) -> Vec<String> {
    raw.into_iter()
        .map(|value| sanitize_metadata_text(&value, max_chars))
        .filter(|value| !value.trim().is_empty())
        .collect()
}

pub fn sanitize_metadata_text(raw: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut kept = 0;
    for ch in raw.trim().chars() {
        if kept >= max_chars {
            break;
        }
        if is_forbidden_metadata_char(ch) {
            continue;
        }
        out.push(ch);
        kept += 1;
    }
    out.trim().to_owned()
}

fn is_forbidden_metadata_char(ch: char) -> bool {
    ch.is_control()
        || matches!(
            ch,
            '\u{200b}'
                | '\u{200c}'
                | '\u{200d}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

/// A search result track, trimmed to what the UI needs. Serializable so the local
/// library (favorites/history) can persist tracks verbatim. `local_path` is set only for
/// downloaded/local audio files; old persisted JSON omits it and still deserializes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Song {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<String>,
    /// Pre-formatted duration string (e.g. "3:45").
    pub duration: String,
    /// Album name, when the provider exposes it (YT Music search does). Feeds scrobble
    /// metadata and Spotify↔YTM matching; old persisted JSON omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Album artist from the source catalog, when available. Local Deck uses this for
    /// album grouping after downloaded tracks are scanned back from disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub album_artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_release_date_precision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_total_tracks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_art_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    /// Source catalog key/URI that produced this playable song, such as a Spotify URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_key: Option<String>,
    /// Human-openable source URL, when the import source provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
    /// Transfer/import session that produced this row, if it came through the import flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_source_order: Option<u32>,
    /// Numeric duration in seconds, when known exactly (search results, imports). Display
    /// still uses `duration`; consumers needing seconds fall back to parsing that string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    /// The provider this result came from. Defaults to YouTube for old persisted JSON.
    #[serde(default, skip_serializing_if = "SearchSource::is_youtube")]
    pub source: SearchSource,
    /// Non-YouTube playback target. Old/pure YouTube tracks omit it and use `video_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playable: Option<PlayableRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<PathBuf>,
    /// The original YouTube video ID, preserved when a catalog track is downloaded and its
    /// `video_id` becomes a `local:` identity. `None` for pure local files and for remote
    /// tracks (whose `video_id` is already the YouTube ID). Old persisted JSON omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yt_video_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SongImportMetadata {
    pub artists: Vec<String>,
    pub album_artists: Vec<String>,
    pub album_release_date: Option<String>,
    pub album_release_date_precision: Option<String>,
    pub album_total_tracks: Option<u32>,
    pub album_type: Option<String>,
    pub album_art_url: Option<String>,
    pub explicit: Option<bool>,
}

/// How a non-local track should be handed to mpv/yt-dlp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PlayableRef {
    YoutubeVideo {
        id: String,
    },
    DirectUrl {
        source: SearchSource,
        url: String,
    },
    YtdlpUrl {
        source: SearchSource,
        url: String,
    },
    AudiusTrackId {
        id: String,
        app_name: String,
    },
    JamendoTrackId {
        id: String,
        url: String,
    },
    ArchiveFile {
        identifier: String,
        file: String,
        url: String,
    },
    RadioStream {
        url: String,
    },
}

impl Song {
    pub fn remote(
        video_id: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
        duration: impl Into<String>,
    ) -> Self {
        let video_id = video_id.into();
        let title = title.into();
        let artist = artist.into();
        let duration = duration.into();
        Self {
            video_id: sanitize_provider_id(&video_id),
            title: sanitize_title(&title),
            artist: sanitize_artist(&artist),
            artists: Vec::new(),
            duration: sanitize_duration(&duration),
            album: None,
            album_artist: None,
            album_artists: Vec::new(),
            album_release_date: None,
            album_release_date_precision: None,
            album_total_tracks: None,
            album_type: None,
            album_art_url: None,
            disc_number: None,
            track_number: None,
            explicit: None,
            isrc: None,
            origin_key: None,
            origin_url: None,
            import_session_id: None,
            import_source_order: None,
            duration_secs: None,
            source: SearchSource::Youtube,
            playable: None,
            local_path: None,
            yt_video_id: None,
        }
    }

    /// A YT Music search result with the richer fields the catalog exposes. Seconds are
    /// derived from the display string once, here, so downstream consumers (matching,
    /// scrobbling) don't re-parse.
    pub fn from_search(
        video_id: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
        duration: impl Into<String>,
        album: Option<String>,
    ) -> Self {
        let mut song = Self::remote(video_id, title, artist, duration);
        song.duration_secs = crate::streaming::candidate::parse_duration_secs(&song.duration);
        song.album = album
            .map(|a| sanitize_album(&a))
            .filter(|a| !a.trim().is_empty());
        song
    }

    pub fn from_source(
        source: SearchSource,
        source_id: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
        duration: impl Into<String>,
        playable: PlayableRef,
    ) -> Self {
        let source_id = sanitize_provider_id(&source_id.into());
        let title = title.into();
        let artist = artist.into();
        let duration = duration.into();
        let video_id = if source == SearchSource::Youtube {
            source_id.clone()
        } else {
            format!("{}:{source_id}", source.id_prefix())
        };
        Self {
            video_id,
            title: sanitize_title(&title),
            artist: sanitize_artist(&artist),
            artists: Vec::new(),
            duration: sanitize_duration(&duration),
            album: None,
            album_artist: None,
            album_artists: Vec::new(),
            album_release_date: None,
            album_release_date_precision: None,
            album_total_tracks: None,
            album_type: None,
            album_art_url: None,
            disc_number: None,
            track_number: None,
            explicit: None,
            isrc: None,
            origin_key: None,
            origin_url: None,
            import_session_id: None,
            import_source_order: None,
            duration_secs: None,
            source,
            playable: Some(playable),
            local_path: None,
            yt_video_id: None,
        }
    }

    /// Build a locally playable track from a file path. Rich metadata parsing is intentionally
    /// avoided, but a YouTube id embedded in the filename by our downloader (`Title [<id>].m4a`,
    /// see [`crate::download`]) is recovered into `yt_video_id` and stripped from the displayed
    /// title — this is what lets a downloaded-and-online track still produce its share URL after
    /// a restart. Plain/foreign filenames are unaffected. Session downloads can additionally
    /// preserve richer metadata via [`Self::with_local_path`].
    pub fn local_file(path: PathBuf) -> Self {
        let stem = path.file_stem().and_then(|s| s.to_str());
        let (title, yt_video_id) = match stem.and_then(Self::parse_embedded_id) {
            Some((title, id)) => (title.to_owned(), Some(id.to_owned())),
            None => (
                stem.filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| path.display().to_string()),
                None,
            ),
        };
        Self {
            video_id: Self::local_id(&path),
            title: sanitize_title(&title),
            artist: "Local file".to_owned(),
            artists: Vec::new(),
            duration: String::new(),
            album: None,
            album_artist: None,
            album_artists: Vec::new(),
            album_release_date: None,
            album_release_date_precision: None,
            album_total_tracks: None,
            album_type: None,
            album_art_url: None,
            disc_number: None,
            track_number: None,
            explicit: None,
            isrc: None,
            origin_key: None,
            origin_url: None,
            import_session_id: None,
            import_source_order: None,
            duration_secs: None,
            source: SearchSource::Youtube,
            playable: None,
            local_path: Some(path),
            yt_video_id,
        }
    }

    /// Parse a trailing ` [<id>]` YouTube-id tag off a file stem, returning
    /// `(title_without_tag, id)`. The id must be exactly 11 URL-safe base64 chars (the YouTube
    /// video-id shape), which makes accidental matches on bracketed titles like `Mix [Vol. 3]`
    /// effectively impossible. Returns `None` when there is no tag or no real title before it.
    pub(crate) fn parse_embedded_id(stem: &str) -> Option<(&str, &str)> {
        let rest = stem.trim_end().strip_suffix(']')?;
        let open = rest.rfind('[')?;
        let id = &rest[open + 1..];
        if !is_youtube_video_id(id) {
            return None;
        }
        let title = rest[..open].trim_end();
        (!title.is_empty()).then_some((title, id))
    }

    /// Preserve known catalog metadata while making this entry play from `path`. The
    /// original YouTube ID is kept in `yt_video_id` so the track still knows its online
    /// URL even though `video_id` becomes a `local:` identity.
    pub fn with_local_path(&self, path: PathBuf) -> Self {
        Self {
            video_id: Self::local_id(&path),
            title: self.title.clone(),
            artist: self.artist.clone(),
            artists: self.artists.clone(),
            duration: self.duration.clone(),
            album: self.album.clone(),
            album_artist: self.album_artist.clone(),
            album_artists: self.album_artists.clone(),
            album_release_date: self.album_release_date.clone(),
            album_release_date_precision: self.album_release_date_precision.clone(),
            album_total_tracks: self.album_total_tracks,
            album_type: self.album_type.clone(),
            album_art_url: self.album_art_url.clone(),
            disc_number: self.disc_number,
            track_number: self.track_number,
            explicit: self.explicit,
            isrc: self.isrc.clone(),
            origin_key: self.origin_key.clone(),
            origin_url: self.origin_url.clone(),
            import_session_id: self.import_session_id.clone(),
            import_source_order: self.import_source_order,
            duration_secs: self.duration_secs,
            source: self.source,
            playable: self.playable.clone(),
            local_path: Some(path),
            yt_video_id: self.youtube_id().map(str::to_owned),
        }
    }

    /// Attach a recovered YouTube id to an otherwise id-less local track (used when the id is
    /// restored from the download manifest or a title match rather than the filename).
    pub fn with_yt_id(mut self, id: String) -> Self {
        self.yt_video_id = Some(id);
        self
    }

    pub fn with_catalog_metadata(
        mut self,
        album_artist: Option<String>,
        disc_number: Option<u32>,
        track_number: Option<u32>,
        isrc: Option<String>,
        origin_key: Option<String>,
        origin_url: Option<String>,
    ) -> Self {
        self.album_artist = sanitize_optional_metadata(album_artist, MAX_ARTIST_CHARS);
        self.album_artists = self.album_artist.iter().cloned().collect::<Vec<_>>();
        self.disc_number = disc_number.filter(|number| *number > 0);
        self.track_number = track_number.filter(|number| *number > 0);
        self.isrc = sanitize_optional_metadata(isrc, MAX_PROVIDER_ID_CHARS);
        self.origin_key = sanitize_optional_metadata(origin_key, MAX_PROVIDER_ID_CHARS);
        self.origin_url = sanitize_optional_metadata(origin_url, MAX_ORIGIN_URL_CHARS);
        self
    }

    pub fn with_import_metadata(mut self, metadata: SongImportMetadata) -> Self {
        self.artists = sanitize_metadata_vec(metadata.artists, MAX_ARTIST_CHARS);
        self.album_artists = sanitize_metadata_vec(metadata.album_artists, MAX_ARTIST_CHARS);
        self.album_release_date =
            sanitize_optional_metadata(metadata.album_release_date, MAX_DURATION_CHARS);
        self.album_release_date_precision =
            sanitize_optional_metadata(metadata.album_release_date_precision, MAX_DURATION_CHARS);
        self.album_total_tracks = metadata.album_total_tracks.filter(|number| *number > 0);
        self.album_type = sanitize_optional_metadata(metadata.album_type, MAX_PROVIDER_ID_CHARS);
        self.album_art_url =
            sanitize_optional_metadata(metadata.album_art_url, MAX_ORIGIN_URL_CHARS);
        self.explicit = metadata.explicit;
        self
    }

    pub fn with_import_session(
        mut self,
        session_id: Option<String>,
        source_order: Option<u32>,
    ) -> Self {
        self.import_session_id = sanitize_optional_metadata(session_id, MAX_PROVIDER_ID_CHARS);
        self.import_source_order = source_order.filter(|order| *order > 0);
        self
    }

    pub fn is_local(&self) -> bool {
        self.local_path.is_some()
    }

    /// True for live radio stations from Radio Browser. These are streams, not tracks, so they
    /// stay out of song history and recommendation signals.
    pub fn is_radio_station(&self) -> bool {
        self.source == SearchSource::RadioBrowser
            || matches!(&self.playable, Some(PlayableRef::RadioStream { .. }))
    }

    /// The real YouTube video ID for this track, if it originated from YouTube. Pure local
    /// files (dropped into the library, never sourced online) return `None`. Downloaded
    /// catalog tracks keep their original ID via `yt_video_id` even though `video_id`
    /// becomes a `local:` identity. Playlist rows (`ytpl:` ids) are not videos.
    pub fn youtube_id(&self) -> Option<&str> {
        if let Some(id) = self.yt_video_id.as_deref() {
            return (!is_definitely_non_video_youtube_ref(id)).then_some(id);
        }
        if self.local_path.is_some() || self.source != SearchSource::Youtube {
            return None;
        }
        (!self.video_id.starts_with("local:")
            && !self.video_id.starts_with(PLAYLIST_ID_PREFIX)
            && !is_definitely_non_video_youtube_ref(&self.video_id))
        .then_some(self.video_id.as_str())
    }

    pub fn unplayable_youtube_ref_reason(&self) -> Option<String> {
        if self.local_path.is_some() || self.source != SearchSource::Youtube {
            return None;
        }
        match &self.playable {
            Some(PlayableRef::YoutubeVideo { id }) if is_definitely_non_video_youtube_ref(id) => {
                Some(format!("not a YouTube video id: {id}"))
            }
            None if is_definitely_non_video_youtube_ref(&self.video_id) => {
                Some(format!("not a YouTube video id: {}", self.video_id))
            }
            _ => None,
        }
    }

    /// The YouTube playlist id when this row is a playlist search result (see
    /// [`PLAYLIST_ID_PREFIX`]); `None` for ordinary tracks.
    pub fn youtube_playlist_id(&self) -> Option<&str> {
        self.video_id.strip_prefix(PLAYLIST_ID_PREFIX)
    }

    /// A shareable public URL for this track, if it has a YouTube origin.
    pub fn share_url(&self) -> Option<String> {
        self.youtube_id()
            .map(|id| format!("https://www.youtube.com/watch?v={id}"))
    }

    /// The string passed to mpv: either a local file path or a YouTube Music watch URL.
    pub fn playback_target(&self) -> String {
        self.playback_target_checked().unwrap_or_default()
    }

    pub fn playback_target_checked(&self) -> Result<String, PlayableUrlError> {
        self.local_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .map(Ok)
            .unwrap_or_else(|| self.watch_url_checked())
    }

    /// A URL mpv/yt-dlp can resolve and play for catalog tracks.
    pub fn watch_url(&self) -> String {
        self.watch_url_checked().unwrap_or_default()
    }

    pub fn watch_url_checked(&self) -> Result<String, PlayableUrlError> {
        match &self.playable {
            Some(PlayableRef::YoutubeVideo { id }) if !is_definitely_non_video_youtube_ref(id) => {
                validate_playable_url(
                    SearchSource::Youtube,
                    &format!("https://music.youtube.com/watch?v={id}"),
                )
            }
            Some(PlayableRef::YoutubeVideo { id }) => validate_playable_url(
                SearchSource::Youtube,
                &format!("https://music.youtube.com/channel/{id}"),
            ),
            Some(PlayableRef::DirectUrl { source, url })
            | Some(PlayableRef::YtdlpUrl { source, url }) => validate_playable_url(*source, url),
            Some(PlayableRef::JamendoTrackId { url, .. }) => {
                validate_playable_url(SearchSource::Jamendo, url)
            }
            Some(PlayableRef::ArchiveFile { url, .. }) => {
                validate_playable_url(SearchSource::InternetArchive, url)
            }
            Some(PlayableRef::RadioStream { url }) => {
                validate_playable_url(SearchSource::RadioBrowser, url)
            }
            Some(PlayableRef::AudiusTrackId { id, app_name }) => {
                validate_playable_url(SearchSource::Audius, &audius_stream_url(id, app_name))
            }
            None if !is_definitely_non_video_youtube_ref(&self.video_id) => validate_playable_url(
                SearchSource::Youtube,
                &format!("https://music.youtube.com/watch?v={}", self.video_id),
            ),
            None => validate_playable_url(
                SearchSource::Youtube,
                &format!("https://music.youtube.com/channel/{}", self.video_id),
            ),
        }
    }

    /// Direct streams/radio URLs do not benefit from yt-dlp pre-resolution. YouTube and
    /// yt-dlp-backed sources do, so the skip-ahead resolver can ask only for those.
    pub fn prefetch_target(&self) -> Option<String> {
        if self.is_local() {
            return None;
        }
        match &self.playable {
            Some(PlayableRef::DirectUrl { .. })
            | Some(PlayableRef::JamendoTrackId { .. })
            | Some(PlayableRef::ArchiveFile { .. })
            | Some(PlayableRef::RadioStream { .. }) => None,
            Some(PlayableRef::YoutubeVideo { .. })
            | Some(PlayableRef::YtdlpUrl { .. })
            | Some(PlayableRef::AudiusTrackId { .. })
            | None => self
                .unplayable_youtube_ref_reason()
                .is_none()
                .then(|| self.watch_url_checked().ok())
                .flatten(),
        }
    }

    fn local_id(path: &Path) -> String {
        let stable = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        format!("local:{}", stable.to_string_lossy())
    }
}

fn audius_stream_url(id: &str, app_name: &str) -> String {
    reqwest::Url::parse_with_params(
        &format!("https://discoveryprovider.audius.co/v1/tracks/{id}/stream"),
        &[("app_name", app_name)],
    )
    .map(|u| u.to_string())
    .unwrap_or_else(|_| {
        format!("https://discoveryprovider.audius.co/v1/tracks/{id}/stream?app_name={app_name}")
    })
}

/// Which auth mode the live API client ended up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiMode {
    Authenticated,
    Anonymous,
}

/// Marker prefix for *playlist* rows in search results — their `video_id` is
/// `"ytpl:<playlist id>"`. Never sent to the wire; strip via [`Song::youtube_playlist_id`].
pub const PLAYLIST_ID_PREFIX: &str = "ytpl:";

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
        seed: String,
        seed_video_id: String,
        exclude_ids: Vec<String>,
        limit: usize,
        mode: StreamingMode,
        config: SearchConfig,
    },
    StreamingPreflight {
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
    /// rows whose ids carry [`PLAYLIST_ID_PREFIX`].
    SearchPlaylists { request_id: u64, query: String },
    /// Fetch a remote playlist's full track list; `title` and `intent` ride along so the
    /// reducer knows what to do with the answer.
    PlaylistTracks {
        playlist_id: String,
        title: String,
        intent: PlaylistIntent,
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
    PlaylistTracks,
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
            ApiCommandKind::PlaylistTracks => "playlist tracks",
        }
    }

    fn lane(self) -> ApiLane {
        match self {
            ApiCommandKind::Search
            | ApiCommandKind::GuiSearch
            | ApiCommandKind::ResolveTrack
            | ApiCommandKind::SearchPlaylists => ApiLane::Interactive,
            ApiCommandKind::Streaming
            | ApiCommandKind::StreamingPreflight
            | ApiCommandKind::PlaylistTracks => ApiLane::Bulk,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiLane {
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
    fn kind(&self) -> ApiCommandKind {
        match self {
            ApiCmd::Search { .. } => ApiCommandKind::Search,
            ApiCmd::GuiSearch { .. } => ApiCommandKind::GuiSearch,
            ApiCmd::Streaming { .. } => ApiCommandKind::Streaming,
            ApiCmd::StreamingPreflight { .. } => ApiCommandKind::StreamingPreflight,
            ApiCmd::ResolveTrack { .. } => ApiCommandKind::ResolveTrack,
            ApiCmd::SearchPlaylists { .. } => ApiCommandKind::SearchPlaylists,
            ApiCmd::PlaylistTracks { .. } => ApiCommandKind::PlaylistTracks,
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
        seed_video_id: String,
        candidates: Vec<(Song, CandidateSource)>,
    },
    StreamingPreflighted {
        seed_video_id: String,
        songs: Vec<Song>,
    },
    StreamingError {
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
    /// Per-catalog groups answering [`ApiCmd::GuiSearch`], correlated only by an opaque id.
    GuiSearchCompleted {
        request_id: GuiSearchRequestId,
        groups: Vec<GuiSearchGroup>,
    },
}

/// Handle for issuing API requests; results return as [`ApiEvent`]s.
pub struct ApiHandle {
    interactive_tx: Sender<ApiCmd>,
    bulk_tx: Sender<ApiCmd>,
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

    pub fn streaming(
        &self,
        seed: impl Into<String>,
        seed_video_id: impl Into<String>,
        exclude_ids: Vec<String>,
        limit: usize,
        mode: StreamingMode,
        config: SearchConfig,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::Streaming {
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
        seed_video_id: impl Into<String>,
        picks: Vec<Song>,
        fallback: Vec<Song>,
        mode: StreamingMode,
        config: StreamingConfig,
    ) -> Result<(), ApiEnqueueError> {
        self.enqueue(ApiCmd::StreamingPreflight {
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
}

/// Spawn the API actor, returning its handle immediately.
///
/// A configured cookie is tried first; if it's rejected we fall back to anonymous
/// (yt-dlp) search so search + public playback still work. With no cookie we go straight
/// to anonymous. Commands sent before authentication settles are buffered by the channel.
pub fn spawn<F>(cookie: Option<String>, emit: F) -> ApiHandle
where
    F: Fn(ApiEvent) + Send + Sync + 'static,
{
    let had_cookie = cookie.is_some();
    // Bounded with a generous cap; the human-rate UI producer never fills it in normal use.
    // A stalled network + command burst rejects new commands through `ApiEnqueueError`, which
    // every owner maps to a visible terminal result instead of silently losing the request.
    let (interactive_tx, interactive_rx) = mpsc::channel(API_INTERACTIVE_QUEUE);
    let (bulk_tx, bulk_rx) = mpsc::channel(API_BULK_QUEUE);
    tokio::spawn(async move {
        let (api, mode) = init_api(cookie).await;
        emit(ApiEvent::ModeResolved { mode, had_cookie });
        let api = Arc::new(api);
        let emit = Arc::new(emit);
        tokio::spawn(run_interactive_actor(
            Arc::clone(&api),
            interactive_rx,
            Arc::clone(&emit),
        ));
        run_bulk_actor(api, bulk_rx, emit).await;
    });
    ApiHandle {
        interactive_tx,
        bulk_tx,
    }
}

/// Run one GUI search: per-catalog groups. A concrete `source` yields one group; `All`
/// fans out over the enabled catalogs (like the TUI's merged path) but keeps results
/// separated and failures per-source. A pasted YouTube URL short-circuits to a single
/// youtube group — resolving it once, not once per catalog.
async fn gui_search_groups(
    api: &ytmusic::YtMusicApi,
    query: &str,
    source: SearchSource,
    config: &SearchConfig,
) -> Vec<GuiSearchGroup> {
    let url_like = crate::media::parse_youtube_playlist_id(query).is_some()
        || crate::media::parse_youtube_video_id(query).is_some();
    let targets: Vec<SearchSource> = if source == SearchSource::All && !url_like {
        let enabled = config.enabled_sources();
        if enabled.is_empty() {
            vec![SearchSource::Youtube]
        } else {
            enabled
        }
    } else if source == SearchSource::All {
        vec![SearchSource::Youtube]
    } else {
        vec![source]
    };
    let mut groups = Vec::with_capacity(targets.len());
    for target in targets {
        match api.search_songs(query, target, config).await {
            Ok(songs) => groups.push(GuiSearchGroup {
                source: target,
                songs,
                error: None,
            }),
            Err(e) => groups.push(GuiSearchGroup {
                source: target,
                songs: Vec::new(),
                error: Some(sanitize::sanitize_error_text(format!("{e:#}"))),
            }),
        }
    }
    groups
}

async fn init_api(cookie: Option<String>) -> (ytmusic::YtMusicApi, ApiMode) {
    let (api, mode) = match cookie {
        Some(c) => match ytmusic::YtMusicApi::from_cookie(&c).await {
            Ok(api) => (api, ApiMode::Authenticated),
            Err(e) => {
                tracing::warn!(error = %sanitize::sanitize_error_text(format!("{e:#}")), "cookie auth failed; using anonymous search");
                (ytmusic::YtMusicApi::Anonymous, ApiMode::Anonymous)
            }
        },
        None => (ytmusic::YtMusicApi::Anonymous, ApiMode::Anonymous),
    };
    (api, mode)
}

async fn run_interactive_actor<F>(
    api: Arc<ytmusic::YtMusicApi>,
    mut rx: Receiver<ApiCmd>,
    emit: Arc<F>,
) where
    F: Fn(ApiEvent) + Send + Sync + 'static,
{
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ApiCmd::Search {
                request_id,
                query,
                source,
                config,
            } => {
                let event = match api.search_songs_reported(&query, source, &config).await {
                    Ok((songs, timed_out)) => {
                        let query_log = crate::util::query::query_log_preview(&query);
                        tracing::info!(
                            count = songs.len(),
                            query_bytes = query_log.bytes,
                            query_chars = query_log.chars,
                            query_preview = %query_log.preview,
                            query_truncated = query_log.truncated,
                            source = %source.code(),
                            timed_out,
                            "search results"
                        );
                        ApiEvent::SearchResults {
                            request_id,
                            query,
                            source,
                            songs,
                            timed_out,
                        }
                    }
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(source = %source.code(), error = %error, "search failed");
                        ApiEvent::SearchError {
                            request_id,
                            source,
                            error,
                        }
                    }
                };
                emit(event);
            }
            ApiCmd::GuiSearch {
                request_id,
                query,
                source,
                config,
            } => {
                let groups = gui_search_groups(&api, &query, source, &config).await;
                let query_log = crate::util::query::query_log_preview(&query);
                tracing::info!(
                    request_id = ?request_id,
                    query_bytes = query_log.bytes,
                    query_chars = query_log.chars,
                    query_preview = %query_log.preview,
                    query_truncated = query_log.truncated,
                    source = %source.code(),
                    groups = groups.len(),
                    "gui search completed"
                );
                emit(ApiEvent::GuiSearchCompleted { request_id, groups });
            }
            ApiCmd::ResolveTrack { seq, query, config } => {
                // The same innertube→yt-dlp search as the Search screen, but the answer
                // stays out of screen state — the caller matches it back up by `seq`.
                let result = api
                    .search_songs(&query, SearchSource::Youtube, &config)
                    .await
                    .map_err(|e| sanitize::sanitize_error_text(format!("{e:#}")));
                match &result {
                    Ok(songs) => {
                        let query_log = crate::util::query::query_log_preview(&query);
                        tracing::info!(
                            count = songs.len(),
                            query_bytes = query_log.bytes,
                            query_chars = query_log.chars,
                            query_preview = %query_log.preview,
                            query_truncated = query_log.truncated,
                            "track resolved"
                        )
                    }
                    Err(error) => tracing::warn!(error = %error, "track resolve failed"),
                }
                emit(ApiEvent::TrackResolved { seq, result });
            }
            ApiCmd::SearchPlaylists { request_id, query } => {
                let event = match api.search_playlists(&query).await {
                    Ok(songs) => {
                        let query_log = crate::util::query::query_log_preview(&query);
                        tracing::info!(
                            count = songs.len(),
                            query_bytes = query_log.bytes,
                            query_chars = query_log.chars,
                            query_preview = %query_log.preview,
                            query_truncated = query_log.truncated,
                            "playlist search results"
                        );
                        ApiEvent::SearchResults {
                            request_id,
                            query,
                            source: SearchSource::Youtube,
                            songs,
                            // Playlist search is a single provider; no multi-source deadline.
                            timed_out: false,
                        }
                    }
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(error = %error, "playlist search failed");
                        ApiEvent::SearchError {
                            request_id,
                            source: SearchSource::Youtube,
                            error,
                        }
                    }
                };
                emit(event);
            }
            ApiCmd::PlaylistTracks { .. }
            | ApiCmd::Streaming { .. }
            | ApiCmd::StreamingPreflight { .. } => {
                tracing::warn!(
                    kind = ?cmd.kind(),
                    "bulk API command arrived on interactive lane"
                );
            }
        }
    }
}

async fn run_bulk_actor<F>(api: Arc<ytmusic::YtMusicApi>, mut rx: Receiver<ApiCmd>, emit: Arc<F>)
where
    F: Fn(ApiEvent) + Send + Sync + 'static,
{
    let mut streaming_ytdlp_cache: HashMap<
        (String, StreamingMode, SearchSource),
        (Instant, Vec<Song>),
    > = HashMap::new();
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ApiCmd::PlaylistTracks {
                playlist_id,
                title,
                intent,
            } => {
                let event = match api.playlist_tracks(&playlist_id).await {
                    Ok(songs) => {
                        tracing::info!(count = songs.len(), id = %playlist_id, "playlist tracks fetched");
                        ApiEvent::PlaylistTracks {
                            title,
                            intent,
                            songs,
                        }
                    }
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(id = %playlist_id, error = %error, "playlist tracks fetch failed");
                        ApiEvent::PlaylistTracksError { title, error }
                    }
                };
                emit(event);
            }
            ApiCmd::Streaming {
                seed,
                seed_video_id,
                exclude_ids,
                limit,
                mode,
                config,
            } => {
                // Build one pool from the configured streaming source(s), tagged by provenance so
                // the local engine can weight them. YouTube gets the strongest source first:
                // YTM's own watch-playlist continuation, then search-based top-up. Other providers
                // use their Search-screen backends with the same seed query variants.
                let config = config.normalized();
                let streaming_source = config.normalized_streaming_source(config.streaming_source);
                let selected_sources = if streaming_source == SearchSource::All {
                    config.streaming_enabled_sources()
                } else {
                    vec![streaming_source]
                };
                let per_source_limit = if streaming_source == SearchSource::All {
                    (limit / selected_sources.len().max(1)).max(4).min(limit)
                } else {
                    limit
                };
                let mut candidate_ids: HashSet<String> = exclude_ids.into_iter().collect();
                let mut candidates: Vec<(Song, CandidateSource)> = Vec::new();
                let mut errors = Vec::new();

                if selected_sources.contains(&SearchSource::Youtube) {
                    let mut yt_added = 0usize;
                    match api.streaming_continuation(&seed_video_id).await {
                        Ok(songs) => {
                            for s in songs {
                                if candidate_ids.insert(s.video_id.clone()) {
                                    candidates.push((s, CandidateSource::WatchPlaylist));
                                    yt_added += 1;
                                    if yt_added >= per_source_limit || candidates.len() >= limit {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %sanitize::sanitize_error_text(format!("{e:#}")), "watch-playlist streaming unavailable; using search top-up");
                        }
                    }

                    let want = if streaming_source == SearchSource::All {
                        per_source_limit.saturating_sub(yt_added)
                    } else {
                        limit.saturating_sub(candidates.len())
                    };
                    if want > 0 && candidates.len() < limit {
                        match cached_related_tracks(
                            &mut streaming_ytdlp_cache,
                            &seed,
                            SearchSource::Youtube,
                            &config,
                            want,
                            mode,
                        )
                        .await
                        {
                            Ok(songs) => {
                                let mut added = 0usize;
                                for s in songs {
                                    if candidate_ids.insert(s.video_id.clone()) {
                                        candidates.push((s, CandidateSource::YtdlpStreaming));
                                        added += 1;
                                        if added >= want || candidates.len() >= limit {
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(e) => errors.push(format!(
                                "{}: {}",
                                SearchSource::Youtube.code(),
                                sanitize::sanitize_error_text(format!("{e:#}"))
                            )),
                        }
                    }
                }

                for source in selected_sources
                    .iter()
                    .copied()
                    .filter(|source| *source != SearchSource::Youtube)
                {
                    if candidates.len() >= limit {
                        break;
                    }
                    let source_limit = if streaming_source == SearchSource::All {
                        per_source_limit
                    } else {
                        limit.saturating_sub(candidates.len())
                    };
                    if source_limit == 0 {
                        continue;
                    }
                    match cached_related_tracks(
                        &mut streaming_ytdlp_cache,
                        &seed,
                        source,
                        &config,
                        source_limit,
                        mode,
                    )
                    .await
                    {
                        Ok(songs) => {
                            let mut added = 0usize;
                            for s in songs {
                                if candidate_ids.insert(s.video_id.clone()) {
                                    candidates.push((s, CandidateSource::YtdlpStreaming));
                                    added += 1;
                                    if added >= source_limit || candidates.len() >= limit {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => errors.push(format!(
                            "{}: {}",
                            source.code(),
                            sanitize::sanitize_error_text(format!("{e:#}"))
                        )),
                    }
                }

                let event = if candidates.is_empty() {
                    let error = if errors.is_empty() {
                        "no related tracks found".to_owned()
                    } else {
                        errors.join("; ")
                    };
                    tracing::warn!(seed = %seed, %error, "streaming search yielded nothing");
                    ApiEvent::StreamingError {
                        seed_video_id,
                        error,
                    }
                } else {
                    tracing::info!(count = candidates.len(), seed = %seed, "streaming results");
                    ApiEvent::StreamingResults {
                        seed_video_id,
                        candidates,
                    }
                };
                emit(event);
            }
            ApiCmd::StreamingPreflight {
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => {
                let songs =
                    ytmusic::preflight_streaming_picks(picks, fallback, mode, &config).await;
                emit(ApiEvent::StreamingPreflighted {
                    seed_video_id,
                    songs,
                });
            }
            ApiCmd::Search { .. }
            | ApiCmd::GuiSearch { .. }
            | ApiCmd::ResolveTrack { .. }
            | ApiCmd::SearchPlaylists { .. } => {
                tracing::warn!(
                    kind = ?cmd.kind(),
                    "interactive API command arrived on bulk lane"
                );
            }
        }
    }
}

async fn cached_related_tracks(
    cache: &mut HashMap<(String, StreamingMode, SearchSource), (Instant, Vec<Song>)>,
    seed: &str,
    source: SearchSource,
    config: &SearchConfig,
    limit: usize,
    mode: StreamingMode,
) -> anyhow::Result<Vec<Song>> {
    let now = Instant::now();
    cache.retain(|_, (stored, _)| now.duration_since(*stored) < STREAMING_YTDLP_CACHE_TTL);
    let cache_key = (seed.to_owned(), mode, source);
    if let Some(songs) = cache
        .get(&cache_key)
        .filter(|(stored, _)| now.duration_since(*stored) < STREAMING_YTDLP_CACHE_TTL)
        .map(|(_, songs)| songs.clone())
    {
        return Ok(songs);
    }
    let empty = HashSet::new();
    let songs =
        ytmusic::related_tracks_from_source(seed, source, config, limit, &empty, mode).await?;
    cache.insert(cache_key, (now, songs.clone()));
    enforce_streaming_cache_cap(cache);
    Ok(songs)
}

fn enforce_streaming_cache_cap(
    cache: &mut HashMap<(String, StreamingMode, SearchSource), (Instant, Vec<Song>)>,
) {
    while cache.len() > STREAMING_YTDLP_CACHE_MAX {
        let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (stored, _))| *stored)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        cache.remove(&oldest);
    }
}

#[cfg(test)]
mod tests;
