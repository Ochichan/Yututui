//! Persisted Local Deck index.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::model::{FileFingerprint, LocalAlbum, LocalArtist, LocalTrack, LocalTrackId};
use crate::util::safe_fs;

const INDEX_SCHEMA_VERSION: u32 = 1;
const INDEX_MAX_BYTES: u64 = 128 * 1024 * 1024;

/// Read-only Local Deck catalog built from on-disk audio files.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalIndex {
    pub schema_version: u32,
    pub tracks: Vec<LocalTrack>,
    pub updated_at: i64,
}

impl Default for LocalIndex {
    fn default() -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            tracks: Vec::new(),
            updated_at: 0,
        }
    }
}

impl LocalIndex {
    pub fn load(path: &Path) -> Self {
        let mut index = safe_fs::load_json_or_default_limited::<LocalIndex>(path, INDEX_MAX_BYTES);
        index.normalize();
        index
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        safe_fs::write_private_atomic_json(path, self)
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn tracks(&self) -> &[LocalTrack] {
        &self.tracks
    }

    pub fn set_tracks(&mut self, tracks: Vec<LocalTrack>) {
        self.tracks = tracks;
        self.updated_at = unix_now();
        self.normalize();
    }

    pub fn upsert_track(&mut self, track: LocalTrack) {
        if let Some(existing) = self.tracks.iter_mut().find(|t| t.id == track.id) {
            *existing = track;
        } else {
            self.tracks.push(track);
        }
        self.updated_at = unix_now();
        self.normalize();
    }

    pub fn remove_missing(&mut self, ids: &[LocalTrackId]) {
        let keep: std::collections::BTreeSet<_> = ids.iter().cloned().collect();
        self.tracks.retain(|track| keep.contains(&track.id));
        self.updated_at = unix_now();
    }

    pub fn find_unchanged(&self, fingerprint: &FileFingerprint) -> Option<&LocalTrack> {
        let id = LocalTrackId::from_fingerprint(fingerprint);
        self.tracks
            .iter()
            .find(|track| track.id == id && &track.fingerprint == fingerprint)
    }

    pub fn contains_path(&self, path: &Path) -> bool {
        self.tracks.iter().any(|track| track.path == path)
    }

    pub fn albums(&self) -> Vec<LocalAlbum> {
        super::query::albums_from_tracks(&self.tracks)
    }

    pub fn artists(&self) -> Vec<LocalArtist> {
        let albums = self.albums();
        super::query::artists_from_tracks(&self.tracks, &albums)
    }

    fn normalize(&mut self) {
        self.schema_version = INDEX_SCHEMA_VERSION;
        let mut by_id = BTreeMap::<LocalTrackId, LocalTrack>::new();
        for track in self.tracks.drain(..) {
            by_id.insert(track.id.clone(), track);
        }
        self.tracks = by_id.into_values().collect();
        self.tracks.sort_by(|a, b| {
            a.path
                .to_string_lossy()
                .to_lowercase()
                .cmp(&b.path.to_string_lossy().to_lowercase())
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
    }
}

pub fn default_index_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui")
        .map(|dirs| dirs.data_dir().join("local-index.json"))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ytm-tui-local-index-test-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn index_round_trips_tracks() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("local-index.json");
        let mut index = LocalIndex::default();
        index.upsert_track(LocalTrack::untagged(dir.join("b.mp3"), 10, 20));
        index.upsert_track(LocalTrack::untagged(dir.join("a.flac"), 11, 21));

        index.save(&path).unwrap();
        let loaded = LocalIndex::load(&path);

        assert_eq!(loaded.schema_version, INDEX_SCHEMA_VERSION);
        assert_eq!(loaded.tracks.len(), 2);
        assert_eq!(loaded.tracks[0].title, "a");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_index_loads_empty() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("local-index.json");
        std::fs::write(&path, b"{not json").unwrap();

        let loaded = LocalIndex::load(&path);

        assert!(loaded.tracks.is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_schema_version_loads_as_current_version() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("local-index.json");
        std::fs::write(&path, br#"{"tracks":[],"updated_at":1}"#).unwrap();

        let loaded = LocalIndex::load(&path);

        assert_eq!(loaded.schema_version, INDEX_SCHEMA_VERSION);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn find_unchanged_matches_fingerprint() {
        let track = LocalTrack::untagged(PathBuf::from("/music/a.mp3"), 10, 20);
        let mut index = LocalIndex::default();
        index.upsert_track(track.clone());

        assert_eq!(
            index.find_unchanged(&track.fingerprint).map(|t| &t.id),
            Some(&track.id)
        );
    }

    #[test]
    fn remove_missing_keeps_only_seen_ids() {
        let keep = LocalTrack::untagged(PathBuf::from("/music/keep.mp3"), 10, 20);
        let drop = LocalTrack::untagged(PathBuf::from("/music/drop.mp3"), 11, 21);
        let mut index = LocalIndex::default();
        index.set_tracks(vec![keep.clone(), drop]);

        index.remove_missing(std::slice::from_ref(&keep.id));

        assert_eq!(index.tracks.len(), 1);
        assert_eq!(index.tracks[0].id, keep.id);
    }
}
