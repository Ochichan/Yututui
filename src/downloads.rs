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
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::api::Song;
use crate::util::safe_fs;

/// Cap on remembered downloads (bounded memory; matches the scan cap).
const STORE_MAX: usize = 999;
/// Cap the on-disk read so a bloated/corrupt/synced `downloads.json` can't be slurped whole
/// at startup; an oversize file is moved to `*.too-large.bak` (never destroyed) and the store
/// falls back to empty. `STORE_MAX` records of paths/titles stay far below this.
const STORE_MAX_BYTES: u64 = 50 * 1024 * 1024;

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

fn store_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "yututui").map(|d| d.data_dir().join("downloads.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn downloaded(path: &str, yt_id: &str) -> Song {
        Song::remote(yt_id, "Title", "Real Artist", "3:00").with_local_path(PathBuf::from(path))
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
}
