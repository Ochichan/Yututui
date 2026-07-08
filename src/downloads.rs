//! Persisted manifest of downloaded tracks' YouTube identity and rich metadata.
//!
//! [`crate::library::scan_downloads`] rebuilds the on-disk download list from bare files on
//! every launch, which loses the YouTube video id (and the real artist/duration). This
//! manifest — persisted to `<data dir>/downloads.json`, mirroring [`crate::library::Library`]'s
//! atomic write — remembers each download's enriched [`Song`] so a downloaded-and-online track
//! can still produce its YouTube share URL after a restart. Records are keyed by the
//! `local:{canonical path}` `video_id` that both [`Song::local_file`] and
//! [`Song::with_local_path`] derive identically, so a scanned file and its remembered record
//! match exactly. The id-in-filename scheme ([`crate::download`]) covers files this manifest
//! doesn't know (e.g. copied from another machine); this manifest adds the richer metadata.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::api::{Song, is_youtube_video_id};
use crate::search_source::SearchSource;
use crate::util::safe_fs;

/// Cap on remembered downloads (bounded memory; matches the scan cap).
const STORE_MAX: usize = 999;
/// Cap the on-disk read so a bloated/corrupt/synced `downloads.json` can't be slurped whole
/// at startup; an oversize file is moved to `*.too-large.bak` (never destroyed) and the store
/// falls back to empty. `STORE_MAX` records of paths/titles stay far below this.
const STORE_MAX_BYTES: u64 = 50 * 1024 * 1024;
const SIDECAR_SCHEMA_VERSION: u32 = 1;
const SIDECAR_MAX_BYTES: u64 = 64 * 1024;

/// Per-file metadata handoff written next to downloaded audio.
///
/// The global download manifest is still the main app-level memory. This sidecar travels with
/// the audio file, so a Local Deck rescan can recover the YouTube origin and catalog metadata
/// even if the file is moved within a scanned root or the central manifest is missing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadSidecar {
    pub schema_version: u32,
    pub video_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub youtube_id: Option<String>,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    pub source: SearchSource,
}

impl Default for DownloadSidecar {
    fn default() -> Self {
        Self {
            schema_version: SIDECAR_SCHEMA_VERSION,
            video_id: String::new(),
            youtube_id: None,
            title: String::new(),
            artist: String::new(),
            album: None,
            duration_secs: None,
            source: SearchSource::Youtube,
        }
    }
}

impl DownloadSidecar {
    pub fn from_song(song: &Song) -> Self {
        Self {
            schema_version: SIDECAR_SCHEMA_VERSION,
            video_id: song.video_id.clone(),
            youtube_id: song.youtube_id().map(str::to_owned),
            title: song.title.clone(),
            artist: song.artist.clone(),
            album: song.album.clone(),
            duration_secs: song.duration_secs,
            source: song.source,
        }
    }

    pub fn linked_youtube_id(&self) -> Option<&str> {
        self.youtube_id
            .as_deref()
            .or_else(|| is_youtube_video_id(&self.video_id).then_some(self.video_id.as_str()))
    }
}

/// The download manifest, persisted to `<data dir>/downloads.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadStore {
    /// Enriched download records, most-recent first, de-duplicated by `video_id`.
    tracks: Vec<Song>,
}

impl DownloadStore {
    /// Load from disk, falling back to an empty store if absent or unreadable.
    pub fn load() -> Self {
        let Some(path) = store_path() else {
            return DownloadStore::default();
        };
        // Schema-drift tolerant: one changed field no longer discards download history.
        let mut store =
            safe_fs::load_json_or_default_limited::<DownloadStore>(&path, STORE_MAX_BYTES);
        store.tracks.truncate(STORE_MAX);
        store
    }

    /// Persist atomically (temp file + rename). A missing data dir is a no-op.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = store_path() else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
    }

    /// Whether any remembered download shares this YouTube id — the bulk-download dedup uses
    /// it to skip tracks already fetched in a previous session.
    pub fn contains_youtube_id(&self, yt: &str) -> bool {
        self.tracks.iter().any(|s| s.youtube_id() == Some(yt))
    }

    /// Remember an enriched downloaded track (dedup by `video_id`, newest first).
    pub fn record(&mut self, song: &Song) {
        self.tracks.retain(|s| s.video_id != song.video_id);
        self.tracks.insert(0, song.clone());
        self.tracks.truncate(STORE_MAX);
    }

    /// Drop records whose on-disk file is among `paths` (after a confirmed delete).
    pub fn remove_paths(&mut self, paths: &[PathBuf]) {
        let doomed: HashSet<&PathBuf> = paths.iter().collect();
        self.tracks
            .retain(|s| s.local_path.as_ref().is_none_or(|p| !doomed.contains(p)));
    }

    /// Fold the remembered enriched metadata (real artist/duration + `yt_video_id`, and the
    /// clean catalog title) into each bare scanned song when the `video_id` matches; otherwise
    /// keep the scanned song unchanged. The scan's own `video_id`/`local_path` are kept — the
    /// on-disk scan is the source of truth for *where each file actually is* (the stored record's
    /// raw path can be stale, e.g. moved/symlinked dirs), while the manifest only lends the
    /// richer fields a bare scan can't know. A remembered record for a deleted file never matches.
    pub fn enrich(&self, scanned: Vec<Song>) -> Vec<Song> {
        let by_id: HashMap<&str, &Song> = self
            .tracks
            .iter()
            .map(|s| (s.video_id.as_str(), s))
            .collect();
        scanned
            .into_iter()
            .map(|song| match by_id.get(song.video_id.as_str()) {
                Some(rec) => Song {
                    title: rec.title.clone(),
                    artist: rec.artist.clone(),
                    duration: rec.duration.clone(),
                    album: rec.album.clone(),
                    duration_secs: rec.duration_secs,
                    yt_video_id: rec.yt_video_id.clone(),
                    ..song
                },
                None => song,
            })
            .collect()
    }
}

pub fn sidecar_path(audio_path: &Path) -> PathBuf {
    let file_name = audio_path
        .file_name()
        .map(OsString::from)
        .unwrap_or_else(|| OsString::from("track"));
    let mut sidecar_name = file_name;
    sidecar_name.push(".ytt.json");
    audio_path.with_file_name(sidecar_name)
}

pub fn write_sidecar(song: &Song, audio_path: &Path) -> io::Result<()> {
    let path = sidecar_path(audio_path);
    let json =
        serde_json::to_vec_pretty(&DownloadSidecar::from_song(song)).map_err(io::Error::other)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = sidecar_temp_path(&path);
    let write_result = (|| {
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

pub fn write_audio_tags(song: &Song, audio_path: &Path) -> lofty::error::Result<()> {
    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Tag;

    let mut tagged = lofty::read_from_path(audio_path)?;
    let tag_type = tagged.primary_tag_type();
    if tagged.primary_tag_mut().is_none() {
        tagged.insert_tag(Tag::new(tag_type));
    }
    let Some(tag) = tagged.primary_tag_mut() else {
        return Ok(());
    };
    apply_song_tags(tag, song);
    tagged.save_to_path(audio_path, WriteOptions::default())
}

pub fn read_sidecar(audio_path: &Path) -> io::Result<Option<DownloadSidecar>> {
    let path = sidecar_path(audio_path);
    let bytes = match safe_fs::read_no_symlink_limited(&path, SIDECAR_MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let sidecar: DownloadSidecar = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if sidecar.schema_version != SIDECAR_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "download sidecar schema version {} is not supported",
                sidecar.schema_version
            ),
        ));
    }
    Ok(Some(sidecar))
}

fn apply_song_tags(tag: &mut lofty::tag::Tag, song: &Song) {
    use lofty::tag::{Accessor, ItemKey};

    if !song.title.trim().is_empty() {
        tag.set_title(song.title.clone());
    }
    if !song.artist.trim().is_empty() {
        tag.set_artist(song.artist.clone());
        tag.insert_text(ItemKey::TrackArtists, song.artist.clone());
    }
    if let Some(album) = song
        .album
        .as_deref()
        .filter(|album| !album.trim().is_empty())
    {
        tag.set_album(album.to_owned());
    }
    if let Some(url) = song.share_url() {
        tag.insert_text(ItemKey::Comment, format!("YouTube: {url}"));
    }
}

fn sidecar_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download.ytt.json");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp.{}-{nanos}", std::process::id()))
}

fn store_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("downloads.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn downloaded(path: &str, yt_id: &str) -> Song {
        Song::remote(yt_id, "Title", "Real Artist", "3:00").with_local_path(PathBuf::from(path))
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!(
            "yututui-downloads-{tag}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn enrich_restores_id_and_artist_by_video_id() {
        let mut store = DownloadStore::default();
        let rich = downloaded("/tmp/Title [dQw4w9WgXcQ].m4a", "dQw4w9WgXcQ");
        store.record(&rich);

        // A bare scanned song with the SAME local video_id but no metadata.
        let scanned = Song::local_file(PathBuf::from("/tmp/plain.m4a"));
        let scanned = Song {
            video_id: rich.video_id.clone(),
            ..scanned
        };
        let out = store.enrich(vec![scanned]);
        assert_eq!(out[0].artist, "Real Artist");
        assert_eq!(out[0].youtube_id(), Some("dQw4w9WgXcQ"));
        assert!(out[0].share_url().is_some());
    }

    #[test]
    fn enrich_keeps_scanned_local_path_over_stored() {
        // The manifest's stored raw path can go stale (file moved, dir re-symlinked) while the
        // canonical `video_id` still agrees. Enrich must take metadata from the manifest but keep
        // the scan's on-disk `local_path` so playback points at where the file actually is.
        let mut store = DownloadStore::default();
        let rich = downloaded("/tmp/stale/Title [dQw4w9WgXcQ].m4a", "dQw4w9WgXcQ");
        store.record(&rich);

        let scanned = Song::local_file(PathBuf::from("/tmp/fresh/plain.m4a"));
        let scanned = Song {
            video_id: rich.video_id.clone(),
            ..scanned
        };
        let out = store.enrich(vec![scanned]);
        assert_eq!(out[0].artist, "Real Artist");
        assert_eq!(out[0].youtube_id(), Some("dQw4w9WgXcQ"));
        assert_eq!(
            out[0].local_path,
            Some(PathBuf::from("/tmp/fresh/plain.m4a"))
        );
    }

    #[test]
    fn enrich_keeps_unknown_scanned_songs() {
        let store = DownloadStore::default();
        let scanned = Song::local_file(PathBuf::from("/tmp/unknown.m4a"));
        let out = store.enrich(vec![scanned.clone()]);
        assert_eq!(out[0].video_id, scanned.video_id);
        assert_eq!(out[0].artist, "Local file");
    }

    #[test]
    fn record_dedups_by_video_id_newest_first() {
        let mut store = DownloadStore::default();
        let a = downloaded("/tmp/a.m4a", "aaaaaaaaaaa");
        let b = downloaded("/tmp/b.m4a", "bbbbbbbbbbb");
        store.record(&a);
        store.record(&b);
        store.record(&a); // re-record a -> back to front, no dupe
        assert_eq!(store.tracks.len(), 2);
        assert_eq!(store.tracks[0].video_id, a.video_id);
    }

    #[test]
    fn remove_paths_drops_matching_records() {
        let mut store = DownloadStore::default();
        let a = downloaded("/tmp/a.m4a", "aaaaaaaaaaa");
        store.record(&a);
        store.remove_paths(&[PathBuf::from("/tmp/a.m4a")]);
        assert!(store.tracks.is_empty());
    }

    #[test]
    fn json_round_trips_and_missing_defaults_empty() {
        let mut store = DownloadStore::default();
        store.record(&downloaded("/tmp/a.m4a", "aaaaaaaaaaa"));
        let s = serde_json::to_string(&store).unwrap();
        let back: DownloadStore = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tracks.len(), 1);
        let empty: DownloadStore = serde_json::from_str("{}").unwrap();
        assert!(empty.tracks.is_empty());
    }

    #[test]
    fn sidecar_path_appends_to_audio_file_name() {
        assert_eq!(
            sidecar_path(&PathBuf::from("/tmp/Track.m4a")),
            PathBuf::from("/tmp/Track.m4a.ytt.json")
        );
    }

    #[test]
    fn sidecar_round_trips_download_metadata() {
        let dir = temp_dir("sidecar");
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Track [abc123def45].m4a");
        let mut song = Song::from_search(
            "abc123def45",
            "Track",
            "Artist A, Artist B",
            "3:03",
            Some("Album".to_owned()),
        );
        song.duration_secs = Some(183);

        write_sidecar(&song, &audio).unwrap();
        let sidecar = read_sidecar(&audio).unwrap().expect("sidecar exists");

        assert_eq!(sidecar.video_id, "abc123def45");
        assert_eq!(sidecar.linked_youtube_id(), Some("abc123def45"));
        assert_eq!(sidecar.title, "Track");
        assert_eq!(sidecar.artist, "Artist A, Artist B");
        assert_eq!(sidecar.album.as_deref(), Some("Album"));
        assert_eq!(sidecar.duration_secs, Some(183));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tag_writer_populates_common_catalog_fields() {
        use lofty::tag::{Accessor, ItemKey, Tag, TagType};

        let mut song = Song::from_search(
            "abc123def45",
            "Track",
            "Artist A, Artist B",
            "3:03",
            Some("Album".to_owned()),
        );
        song.duration_secs = Some(183);
        let mut tag = Tag::new(TagType::Mp4Ilst);

        apply_song_tags(&mut tag, &song);

        assert_eq!(tag.title().as_deref(), Some("Track"));
        assert_eq!(tag.artist().as_deref(), Some("Artist A, Artist B"));
        assert_eq!(tag.album().as_deref(), Some("Album"));
        assert_eq!(
            tag.get_string(ItemKey::Comment),
            Some("YouTube: https://www.youtube.com/watch?v=abc123def45")
        );
    }
}
