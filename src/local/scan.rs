//! Blocking filesystem scanner for Local Deck.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use super::index::LocalIndex;
use super::metadata;
use super::model::{FileFingerprint, LocalTrack, LocalTrackId};

const AUDIO_EXTENSIONS: &[&str] = &["aac", "flac", "m4a", "mp3", "ogg", "opus", "wav", "wma"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalScanRoot {
    pub path: PathBuf,
    pub recursive: bool,
}

impl LocalScanRoot {
    pub fn download(path: PathBuf) -> Self {
        Self {
            path,
            recursive: false,
        }
    }

    pub fn recursive(path: PathBuf) -> Self {
        Self {
            path,
            recursive: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalScanSummary {
    pub seen: usize,
    pub indexed: usize,
    pub reused: usize,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub skipped: usize,
    pub errors: usize,
}

#[derive(Debug, Clone)]
pub struct LocalScanResult {
    pub index: LocalIndex,
    pub summary: LocalScanSummary,
    pub errors: Vec<ScanError>,
}

pub fn scan_roots(roots: &[LocalScanRoot], previous: &LocalIndex) -> LocalScanResult {
    let mut scanner = Scanner {
        previous,
        tracks: Vec::new(),
        seen_ids: Vec::new(),
        summary: LocalScanSummary::default(),
        errors: Vec::new(),
    };
    for root in roots {
        scanner.scan_root(root);
    }

    let previous_ids: std::collections::BTreeSet<_> = previous
        .tracks()
        .iter()
        .map(|track| track.id.clone())
        .collect();
    let seen_ids: std::collections::BTreeSet<_> = scanner.seen_ids.iter().cloned().collect();
    scanner.summary.removed = previous_ids.difference(&seen_ids).count();
    scanner.summary.indexed = scanner.tracks.len();
    scanner.summary.errors = scanner.errors.len();

    let mut index = LocalIndex::default();
    index.set_tracks(scanner.tracks);
    LocalScanResult {
        index,
        summary: scanner.summary,
        errors: scanner.errors,
    }
}

pub fn is_supported_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|known| ext.eq_ignore_ascii_case(known))
        })
        .unwrap_or(false)
}

struct Scanner<'a> {
    previous: &'a LocalIndex,
    tracks: Vec<LocalTrack>,
    seen_ids: Vec<LocalTrackId>,
    summary: LocalScanSummary,
    errors: Vec<ScanError>,
}

impl Scanner<'_> {
    fn scan_root(&mut self, root: &LocalScanRoot) {
        let root_path = canonical_or_original(&root.path);
        if !root_path.is_dir() {
            self.errors.push(ScanError {
                path: root.path.clone(),
                message: "root is not a readable directory".to_owned(),
            });
            return;
        }
        self.scan_dir(&root_path, root.recursive);
    }

    fn scan_dir(&mut self, dir: &Path, recursive: bool) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                self.errors.push(ScanError {
                    path: dir.to_path_buf(),
                    message: error.to_string(),
                });
                return;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.errors.push(ScanError {
                        path: dir.to_path_buf(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let path = entry.path();
            if is_hidden_path(&path) {
                self.summary.skipped += 1;
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    self.errors.push(ScanError {
                        path,
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            if file_type.is_symlink() {
                self.summary.skipped += 1;
                continue;
            }
            if file_type.is_dir() {
                if recursive {
                    self.scan_dir(&path, recursive);
                } else {
                    self.summary.skipped += 1;
                }
                continue;
            }
            if !file_type.is_file() {
                self.summary.skipped += 1;
                continue;
            }
            if !is_supported_audio_path(&path) {
                self.summary.skipped += 1;
                continue;
            }
            self.scan_file(path);
        }
    }

    fn scan_file(&mut self, path: PathBuf) {
        self.summary.seen += 1;
        let canonical = canonical_or_original(&path);
        let metadata = match fs::metadata(&canonical) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.errors.push(ScanError {
                    path: canonical,
                    message: error.to_string(),
                });
                return;
            }
        };
        let modified_at = metadata_modified_unix(&metadata);
        let fingerprint = FileFingerprint::path_mtime_size(&canonical, modified_at, metadata.len());
        if let Some(track) = self.previous.find_unchanged(&fingerprint) {
            self.seen_ids.push(track.id.clone());
            self.tracks.push(track.clone());
            self.summary.reused += 1;
            return;
        }

        let was_known_path = self.previous.contains_path(&canonical);
        let read = metadata::read_track(canonical.clone(), metadata.len(), modified_at);
        if let Some(warning) = read.warning {
            self.errors.push(ScanError {
                path: canonical,
                message: warning,
            });
        }
        if was_known_path {
            self.summary.changed += 1;
        } else {
            self.summary.added += 1;
        }
        self.seen_ids.push(read.track.id.clone());
        self.tracks.push(read.track);
    }
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn is_hidden_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn metadata_modified_unix(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ytm-tui-local-scan-test-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn download_root_is_non_recursive() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("a.mp3"), b"not audio").unwrap();
        fs::write(dir.join("nested").join("b.flac"), b"not audio").unwrap();

        let result = scan_roots(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
        );

        assert_eq!(result.index.tracks().len(), 1);
        assert_eq!(result.index.tracks()[0].title, "a");
        assert_eq!(result.summary.seen, 1);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn recursive_root_walks_nested_audio() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("nested")).unwrap();
        fs::write(dir.join("a.mp3"), b"not audio").unwrap();
        fs::write(dir.join("nested").join("b.flac"), b"not audio").unwrap();

        let result = scan_roots(
            &[LocalScanRoot::recursive(dir.clone())],
            &LocalIndex::default(),
        );

        let titles: Vec<_> = result
            .index
            .tracks()
            .iter()
            .map(|track| track.title.as_str())
            .collect();
        assert_eq!(titles, vec!["a", "b"]);
        assert_eq!(result.summary.seen, 2);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scanner_skips_hidden_and_unsupported_files() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join(".hidden")).unwrap();
        fs::write(dir.join(".hidden").join("a.mp3"), b"not audio").unwrap();
        fs::write(dir.join("note.txt"), b"not audio").unwrap();
        fs::write(dir.join("song.wav"), b"not audio").unwrap();

        let result = scan_roots(
            &[LocalScanRoot::recursive(dir.clone())],
            &LocalIndex::default(),
        );

        assert_eq!(result.index.tracks().len(), 1);
        assert!(result.summary.skipped >= 2);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scanner_reuses_unchanged_tracks() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.mp3"), b"not audio").unwrap();

        let first = scan_roots(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
        );
        let second = scan_roots(&[LocalScanRoot::download(dir.clone())], &first.index);

        assert_eq!(second.summary.reused, 1);
        assert_eq!(second.summary.added, 0);
        assert_eq!(second.index.tracks()[0].id, first.index.tracks()[0].id);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scanner_reports_missing_files_as_removed() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.mp3");
        fs::write(&file, b"not audio").unwrap();
        let first = scan_roots(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
        );
        fs::remove_file(file).unwrap();

        let second = scan_roots(&[LocalScanRoot::download(dir.clone())], &first.index);

        assert_eq!(second.summary.removed, 1);
        assert!(second.index.tracks().is_empty());

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn scanner_does_not_follow_symlinked_audio_files() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.mp3");
        let link = dir.join("linked.mp3");
        fs::write(&target, b"not audio").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let result = scan_roots(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
        );

        assert_eq!(result.index.tracks().len(), 1);
        assert_eq!(result.index.tracks()[0].title, "target");

        let _ = fs::remove_dir_all(dir);
    }
}
