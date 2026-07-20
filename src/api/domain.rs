//! Track domain model and metadata sanitization.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::playback_target::{PlayableUrlError, validate_playable_url};
use crate::search_source::SearchSource;

/// Marker prefix for *playlist* rows in search results — their `video_id` is
/// `"ytpl:<playlist id>"`. Never sent to the wire; strip via [`Song::youtube_playlist_id`].
pub const PLAYLIST_ID_PREFIX: &str = "ytpl:";

/// Marker prefix for *artist* rows in search results — their `video_id` is
/// `"ytar:<channel id>"`. Never sent to the wire; strip via [`Song::youtube_artist_id`].
pub const ARTIST_ID_PREFIX: &str = "ytar:";

/// An artist's browse page, trimmed to what the TUI shows: top songs plus album/single
/// rows (the latter as `ytpl:` playlist rows, playable via the playlist machinery).
#[derive(Debug, Clone)]
pub struct ArtistPage {
    pub channel_id: String,
    pub name: String,
    pub subscribers: Option<String>,
    /// Top songs — ordinary playable track rows.
    pub songs: Vec<Song>,
    /// Albums and singles as `ytpl:` rows.
    pub albums: Vec<Song>,
    /// The artist's full "Songs" playlist, when the page exposes one.
    pub songs_playlist_id: Option<String>,
}

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
        || id.starts_with(ARTIST_ID_PREFIX)
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

pub(super) fn is_forbidden_metadata_char(ch: char) -> bool {
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

    /// The YouTube Music channel id when this row is an artist search result (see
    /// [`ARTIST_ID_PREFIX`]); `None` for ordinary tracks.
    pub fn youtube_artist_id(&self) -> Option<&str> {
        self.video_id.strip_prefix(ARTIST_ID_PREFIX)
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
