//! Persisted Local Deck index.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{FileFingerprint, LocalAlbum, LocalArtist, LocalTrack, LocalTrackId};
use crate::util::safe_fs;

const INDEX_SCHEMA_VERSION: u32 = 1;
const INDEX_MAX_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct LocalIndexLoad {
    pub index: LocalIndex,
    pub warnings: Vec<LocalIndexLoadWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIndexLoadWarning {
    pub path: PathBuf,
    pub message: String,
    pub backup_path: Option<PathBuf>,
}

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
        Self::load_with_diagnostics(path).index
    }

    pub fn load_with_diagnostics(path: &Path) -> LocalIndexLoad {
        Self::load_with_diagnostics_limited(path, INDEX_MAX_BYTES)
    }

    fn load_with_diagnostics_limited(path: &Path, max_bytes: u64) -> LocalIndexLoad {
        let bytes = match safe_fs::read_no_symlink_limited(path, max_bytes) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return LocalIndexLoad::default();
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let backup_path = safe_fs::backup_too_large(path).ok();
                return LocalIndexLoad {
                    index: LocalIndex::default(),
                    warnings: vec![load_warning(
                        path,
                        format!("local index was too large and was rebuilt: {error}"),
                        backup_path,
                    )],
                };
            }
            Err(error) => {
                return LocalIndexLoad {
                    index: LocalIndex::default(),
                    warnings: vec![load_warning(
                        path,
                        format!("local index could not be read and was rebuilt: {error}"),
                        None,
                    )],
                };
            }
        };

        let text = match String::from_utf8(bytes.clone()) {
            Ok(text) => text,
            Err(error) => {
                let backup_path = write_bad_copy(path, &bytes).ok();
                return LocalIndexLoad {
                    index: LocalIndex::default(),
                    warnings: vec![load_warning(
                        path,
                        format!("local index was not valid UTF-8 and was rebuilt: {error}"),
                        backup_path,
                    )],
                };
            }
        };

        let value = match serde_json::from_str::<Value>(&text) {
            Ok(value) => value,
            Err(error) => {
                let backup_path = write_bad_copy(path, &bytes).ok();
                return LocalIndexLoad {
                    index: LocalIndex::default(),
                    warnings: vec![load_warning(
                        path,
                        format!("local index JSON was corrupt and was rebuilt: {error}"),
                        backup_path,
                    )],
                };
            }
        };

        if let Some(schema_version) = unsupported_schema_version(&value) {
            let backup_path = write_bad_copy(path, &bytes).ok();
            return LocalIndexLoad {
                index: LocalIndex::default(),
                warnings: vec![load_warning(
                    path,
                    format!(
                        "local index schema version {schema_version} is newer than supported version {INDEX_SCHEMA_VERSION}; rebuilt empty index"
                    ),
                    backup_path,
                )],
            };
        }

        let mut index = serde_json::from_value::<LocalIndex>(value.clone())
            .unwrap_or_else(|_| safe_fs::recover_lenient::<LocalIndex>(value));
        index.normalize();
        LocalIndexLoad {
            index,
            warnings: Vec::new(),
        }
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

fn unsupported_schema_version(value: &Value) -> Option<u64> {
    value
        .get("schema_version")
        .and_then(Value::as_u64)
        .filter(|version| *version > u64::from(INDEX_SCHEMA_VERSION))
}

fn load_warning(
    path: &Path,
    mut message: String,
    backup_path: Option<PathBuf>,
) -> LocalIndexLoadWarning {
    if let Some(backup_path) = &backup_path {
        message.push_str(&format!("; preserved copy: {}", backup_path.display()));
    }
    LocalIndexLoadWarning {
        path: path.to_path_buf(),
        message,
        backup_path,
    }
}

fn write_bad_copy(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("local-index.json");
    for n in 0..1000 {
        let backup_name = if n == 0 {
            format!("{file_name}.bad")
        } else {
            format!("{file_name}.{n}.bad")
        };
        let backup_path = path.with_file_name(backup_name);
        if backup_path.exists() {
            continue;
        }
        safe_fs::write_private_atomic(&backup_path, bytes)?;
        return Ok(backup_path);
    }
    Err(io::Error::other("too many local index backup copies"))
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
    fn corrupt_index_load_preserves_bad_copy() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("local-index.json");
        std::fs::write(&path, b"{not json").unwrap();

        let loaded = LocalIndex::load_with_diagnostics(&path);

        assert!(loaded.index.tracks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        let backup = loaded.warnings[0]
            .backup_path
            .as_ref()
            .expect("corrupt index should preserve a bad copy");
        assert_eq!(std::fs::read(backup).unwrap(), b"{not json");
        assert!(backup.ends_with("local-index.json.bad"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_schema_version_loads_empty_and_preserves_bad_copy() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("local-index.json");
        let contents = br#"{"schema_version":999,"tracks":[],"updated_at":1}"#;
        std::fs::write(&path, contents).unwrap();

        let loaded = LocalIndex::load_with_diagnostics(&path);

        assert!(loaded.index.tracks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].message.contains("newer than supported"));
        let backup = loaded.warnings[0]
            .backup_path
            .as_ref()
            .expect("unsupported index should preserve a bad copy");
        assert_eq!(std::fs::read(backup).unwrap(), contents);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_index_loads_empty_and_moves_original_aside() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("local-index.json");
        let contents = br#"{"schema_version":1,"tracks":[],"updated_at":1}"#;
        std::fs::write(&path, contents).unwrap();

        let loaded = LocalIndex::load_with_diagnostics_limited(&path, 8);

        assert!(loaded.index.tracks.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].message.contains("too large"));
        let backup = loaded.warnings[0]
            .backup_path
            .as_ref()
            .expect("oversized index should be moved aside");
        assert_eq!(std::fs::read(backup).unwrap(), contents);
        assert!(!path.exists());

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
