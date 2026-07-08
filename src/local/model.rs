use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::Song;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LocalTrackId(String);

impl LocalTrackId {
    pub fn from_fingerprint(fingerprint: &FileFingerprint) -> Self {
        Self(format!("lt:{:016x}", fingerprint.stable_hash()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LocalAlbumId(String);

impl LocalAlbumId {
    pub fn from_parts(title: &str, album_artist: &str) -> Self {
        Self(format!(
            "la:{:016x}",
            stable_hash_segments(&[
                normalize_key(title).as_bytes(),
                normalize_key(album_artist).as_bytes(),
            ])
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LocalArtistId(String);

impl LocalArtistId {
    pub fn from_name(name: &str) -> Self {
        Self(format!(
            "lar:{:016x}",
            stable_hash_segments(&[normalize_key(name).as_bytes()])
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioFormat {
    Aac,
    Flac,
    M4a,
    Mp3,
    Ogg,
    Opus,
    Wav,
    Wma,
    Other(String),
}

impl AudioFormat {
    pub fn from_extension(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.trim().to_ascii_lowercase();
        Some(match ext.as_str() {
            "aac" => Self::Aac,
            "flac" => Self::Flac,
            "m4a" | "mp4" => Self::M4a,
            "mp3" => Self::Mp3,
            "ogg" | "oga" => Self::Ogg,
            "opus" => Self::Opus,
            "wav" | "wave" => Self::Wav,
            "wma" => Self::Wma,
            _ => Self::Other(ext),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FileFingerprint {
    /// Phase-3 fallback identity: stable for the same path/mtime/size, but not
    /// intended to survive renames. Scanner phases can upgrade this when they
    /// can read platform file identity cheaply.
    PathMtimeSize {
        path_hash: u64,
        mtime: i64,
        size: u64,
    },
    /// Reserved model shape for Unix scanners that can capture device/inode.
    UnixInode {
        dev: u64,
        ino: u64,
        mtime: i64,
        size: u64,
    },
    /// Reserved model shape for Windows scanners that can capture file IDs.
    WindowsFileId {
        volume: u64,
        file_id: u128,
        mtime: i64,
        size: u64,
    },
}

impl FileFingerprint {
    pub fn path_mtime_size(path: &Path, mtime: i64, size: u64) -> Self {
        Self::PathMtimeSize {
            path_hash: stable_hash_segments(&[path.to_string_lossy().as_bytes()]),
            mtime,
            size,
        }
    }

    pub fn stable_hash(&self) -> u64 {
        match self {
            Self::PathMtimeSize {
                path_hash,
                mtime,
                size,
            } => stable_hash_u64s(0x10, &[*path_hash, *mtime as u64, *size]),
            Self::UnixInode {
                dev,
                ino,
                mtime,
                size,
            } => stable_hash_u64s(0x20, &[*dev, *ino, *mtime as u64, *size]),
            Self::WindowsFileId {
                volume,
                file_id,
                mtime,
                size,
            } => stable_hash_u64s(
                0x30,
                &[
                    *volume,
                    (*file_id >> 64) as u64,
                    *file_id as u64,
                    *mtime as u64,
                    *size,
                ],
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTrack {
    pub id: LocalTrackId,
    pub path: PathBuf,
    pub title: String,
    pub artist: Vec<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Vec<String>,
    pub year: Option<i32>,
    pub disc_no: Option<u32>,
    pub track_no: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    pub duration_ms: Option<u64>,
    pub format: Option<AudioFormat>,
    pub bitrate: Option<u32>,
    pub sample_rate: Option<u32>,
    pub file_size: u64,
    pub modified_at: i64,
    pub fingerprint: FileFingerprint,
    pub embedded_art_key: Option<String>,
    pub linked_video_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_source_order: Option<u32>,
}

impl LocalTrack {
    pub fn untagged(path: PathBuf, file_size: u64, modified_at: i64) -> Self {
        let fingerprint = FileFingerprint::path_mtime_size(&path, modified_at, file_size);
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| path.display().to_string());
        Self {
            id: LocalTrackId::from_fingerprint(&fingerprint),
            path: path.clone(),
            title,
            artist: Vec::new(),
            album: None,
            album_artist: None,
            genre: Vec::new(),
            year: None,
            disc_no: None,
            track_no: None,
            isrc: None,
            duration_ms: None,
            format: AudioFormat::from_extension(&path),
            bitrate: None,
            sample_rate: None,
            file_size,
            modified_at,
            fingerprint,
            embedded_art_key: None,
            linked_video_id: None,
            origin_key: None,
            origin_url: None,
            import_session_id: None,
            import_source_order: None,
        }
    }

    pub fn to_song(&self) -> Song {
        let mut song = Song::local_file(self.path.clone());
        song.title = self.display_title();
        song.artist = self.display_artist();
        song.album = self.album.clone();
        song.duration = self.duration_ms.map(format_duration_ms).unwrap_or_default();
        song.duration_secs = self.duration_ms.map(|ms| (ms / 1000) as u32);
        if let Some(id) = &self.linked_video_id {
            song = song.with_yt_id(id.clone());
        }
        song.with_catalog_metadata(
            self.album_artist.clone(),
            self.disc_no,
            self.track_no,
            self.isrc.clone(),
            self.origin_key.clone(),
            self.origin_url.clone(),
        )
        .with_import_session(self.import_session_id.clone(), self.import_source_order)
    }

    pub fn display_title(&self) -> String {
        if self.title.trim().is_empty() {
            self.path
                .file_stem()
                .and_then(|s| s.to_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| self.path.display().to_string())
        } else {
            self.title.clone()
        }
    }

    pub fn display_artist(&self) -> String {
        if self.artist.is_empty() {
            "Local file".to_owned()
        } else {
            self.artist.join(", ")
        }
    }

    pub fn grouping_album_artist(&self) -> String {
        self.album_artist
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| self.artist.first().cloned())
            .unwrap_or_else(|| "Unknown Artist".to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAlbum {
    pub id: LocalAlbumId,
    pub title: String,
    pub album_artist: String,
    pub year: Option<i32>,
    pub track_ids: Vec<LocalTrackId>,
    pub track_count: usize,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalArtist {
    pub id: LocalArtistId,
    pub name: String,
    pub album_ids: Vec<LocalAlbumId>,
    pub track_ids: Vec<LocalTrackId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocalSmartList {
    RecentlyAdded,
    DownloadedFromYoutubeMusic,
    LocalOnly,
    MissingArtist,
    MissingAlbum,
    NoEmbeddedCover,
    LargeFiles,
    Lossless,
}

impl LocalSmartList {
    pub const ALL: [Self; 8] = [
        Self::RecentlyAdded,
        Self::DownloadedFromYoutubeMusic,
        Self::LocalOnly,
        Self::MissingArtist,
        Self::MissingAlbum,
        Self::NoEmbeddedCover,
        Self::LargeFiles,
        Self::Lossless,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::RecentlyAdded => "Recently Added",
            Self::DownloadedFromYoutubeMusic => "Downloaded from YouTube Music",
            Self::LocalOnly => "Local-only",
            Self::MissingArtist => "Missing Artist",
            Self::MissingAlbum => "Missing Album",
            Self::NoEmbeddedCover => "No Embedded Cover",
            Self::LargeFiles => "Large Files",
            Self::Lossless => "Lossless",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalRowId {
    Track(LocalTrackId),
    DownloadSeed(usize),
    Album(LocalAlbumId),
    Artist(LocalArtistId),
    Genre(String),
    Folder(PathBuf),
    Smart(LocalSmartList),
    ImportSession(String),
    ImportSessionRow {
        session_id: String,
        source_order: u32,
    },
    ScanError(usize),
}

pub(crate) fn normalize_key(s: &str) -> String {
    s.trim().to_lowercase()
}

fn format_duration_ms(ms: u64) -> String {
    let total = ms / 1000;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn stable_hash_u64s(tag: u64, values: &[u64]) -> u64 {
    let mut segments = Vec::with_capacity(values.len() + 1);
    segments.push(tag.to_le_bytes());
    for value in values {
        segments.push(value.to_le_bytes());
    }
    let refs: Vec<&[u8]> = segments.iter().map(|bytes| bytes.as_slice()).collect();
    stable_hash_segments(&refs)
}

pub(crate) fn stable_hash_segments(segments: &[&[u8]]) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for segment in segments {
        for byte in *segment {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untagged_track_uses_filename_and_extension() {
        let track = LocalTrack::untagged(PathBuf::from("/music/Alpha.flac"), 1234, 77);

        assert_eq!(track.title, "Alpha");
        assert_eq!(track.format, Some(AudioFormat::Flac));
        assert_eq!(track.file_size, 1234);
        assert_eq!(track.modified_at, 77);
    }

    #[test]
    fn to_song_preserves_local_path_and_metadata() {
        let mut track = LocalTrack::untagged(PathBuf::from("/music/Alpha.m4a"), 1234, 77);
        track.title = "Real Title".to_owned();
        track.artist = vec!["A".to_owned(), "B".to_owned()];
        track.album = Some("Album".to_owned());
        track.album_artist = Some("Album Artist".to_owned());
        track.disc_no = Some(1);
        track.track_no = Some(3);
        track.isrc = Some("ISRC123".to_owned());
        track.origin_key = Some("spotify:track:abc".to_owned());
        track.origin_url = Some("https://open.spotify.com/track/abc".to_owned());
        track.import_session_id = Some("sp2yt-20260708-abcd".to_owned());
        track.import_source_order = Some(7);
        track.duration_ms = Some(185_000);
        track.linked_video_id = Some("abcdefghijk".to_owned());

        let song = track.to_song();

        assert_eq!(song.title, "Real Title");
        assert_eq!(song.artist, "A, B");
        assert_eq!(song.album.as_deref(), Some("Album"));
        assert_eq!(song.album_artist.as_deref(), Some("Album Artist"));
        assert_eq!(song.disc_number, Some(1));
        assert_eq!(song.track_number, Some(3));
        assert_eq!(song.isrc.as_deref(), Some("ISRC123"));
        assert_eq!(song.origin_key.as_deref(), Some("spotify:track:abc"));
        assert_eq!(
            song.origin_url.as_deref(),
            Some("https://open.spotify.com/track/abc")
        );
        assert_eq!(
            song.import_session_id.as_deref(),
            Some("sp2yt-20260708-abcd")
        );
        assert_eq!(song.import_source_order, Some(7));
        assert_eq!(song.duration, "3:05");
        assert_eq!(song.duration_secs, Some(185));
        assert_eq!(
            song.local_path.as_deref(),
            Some(Path::new("/music/Alpha.m4a"))
        );
        assert_eq!(song.youtube_id(), Some("abcdefghijk"));
        assert_eq!(song.playback_target(), "/music/Alpha.m4a");
    }

    #[test]
    fn to_song_falls_back_for_untagged_files() {
        let track = LocalTrack::untagged(PathBuf::from("/music/Untitled.mp3"), 10, 20);

        let song = track.to_song();

        assert_eq!(song.title, "Untitled");
        assert_eq!(song.artist, "Local file");
        assert_eq!(song.playback_target(), "/music/Untitled.mp3");
    }

    #[test]
    fn path_fingerprint_is_stable_and_changes_with_mtime_or_size() {
        let a = FileFingerprint::path_mtime_size(Path::new("/music/a.flac"), 10, 20);
        let same = FileFingerprint::path_mtime_size(Path::new("/music/a.flac"), 10, 20);
        let changed_mtime = FileFingerprint::path_mtime_size(Path::new("/music/a.flac"), 11, 20);
        let changed_size = FileFingerprint::path_mtime_size(Path::new("/music/a.flac"), 10, 21);

        assert_eq!(a, same);
        assert_eq!(
            LocalTrackId::from_fingerprint(&a),
            LocalTrackId::from_fingerprint(&same)
        );
        assert_ne!(
            LocalTrackId::from_fingerprint(&a),
            LocalTrackId::from_fingerprint(&changed_mtime)
        );
        assert_ne!(
            LocalTrackId::from_fingerprint(&a),
            LocalTrackId::from_fingerprint(&changed_size)
        );
    }

    #[test]
    fn duration_formats_hour_tracks() {
        assert_eq!(format_duration_ms(3_723_000), "1:02:03");
    }
}
