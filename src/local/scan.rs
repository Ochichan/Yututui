//! Blocking filesystem scanner for Local Deck.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use super::index::LocalIndex;
use super::metadata;
use super::model::{FileFingerprint, LocalTrack, LocalTrackId};

const AUDIO_EXTENSIONS: &[&str] = &["aac", "flac", "m4a", "mp3", "ogg", "opus", "wav", "wma"];

/// Max directory depth for the download-folder scan root (`Artist/Album/track` = 2).
pub const DOWNLOAD_SCAN_MAX_DEPTH: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalScanRoot {
    pub path: PathBuf,
    pub recursive: bool,
    /// When set, descend at most this many directory levels below the root
    /// (files at depth 0..=max_depth are indexed). Ignored when `recursive` is true
    /// after merge with a full music root.
    #[serde(default)]
    pub max_depth: Option<u32>,
}

impl LocalScanRoot {
    pub fn download(path: PathBuf) -> Self {
        Self {
            path,
            recursive: false,
            max_depth: Some(DOWNLOAD_SCAN_MAX_DEPTH),
        }
    }

    pub fn recursive(path: PathBuf) -> Self {
        Self {
            path,
            recursive: true,
            max_depth: None,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalScanProgress {
    pub seen: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub current: Option<PathBuf>,
}

pub fn scan_roots(roots: &[LocalScanRoot], previous: &LocalIndex) -> LocalScanResult {
    scan_roots_with_progress(roots, previous, |_| {})
}

pub fn scan_roots_with_progress(
    roots: &[LocalScanRoot],
    previous: &LocalIndex,
    progress: impl FnMut(LocalScanProgress),
) -> LocalScanResult {
    let mut scanner = Scanner {
        previous,
        tracks: Vec::new(),
        seen_ids: Vec::new(),
        summary: LocalScanSummary::default(),
        errors: Vec::new(),
        progress,
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

struct Scanner<'a, F>
where
    F: FnMut(LocalScanProgress),
{
    previous: &'a LocalIndex,
    tracks: Vec<LocalTrack>,
    seen_ids: Vec<LocalTrackId>,
    summary: LocalScanSummary,
    errors: Vec<ScanError>,
    progress: F,
}

impl<F> Scanner<'_, F>
where
    F: FnMut(LocalScanProgress),
{
    fn scan_root(&mut self, root: &LocalScanRoot) {
        let root_path = canonical_or_original(&root.path);
        if !root_path.is_dir() {
            self.errors.push(ScanError {
                path: root.path.clone(),
                message: "root is not a readable directory".to_owned(),
            });
            self.emit_progress(Some(root.path.clone()));
            return;
        }
        self.scan_dir(&root_path, &root_path, 0, root.recursive, root.max_depth);
    }

    fn scan_dir(
        &mut self,
        root: &Path,
        dir: &Path,
        depth: u32,
        recursive: bool,
        max_depth: Option<u32>,
    ) {
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
                self.emit_progress(Some(path));
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    self.errors.push(ScanError {
                        path,
                        message: error.to_string(),
                    });
                    self.emit_progress(None);
                    continue;
                }
            };
            if file_type.is_symlink() {
                self.summary.skipped += 1;
                self.emit_progress(Some(path));
                continue;
            }
            if file_type.is_dir() {
                let can_descend = if recursive {
                    true
                } else {
                    max_depth.is_some_and(|max| depth < max)
                };
                if can_descend {
                    self.scan_dir(root, &path, depth + 1, recursive, max_depth);
                } else {
                    self.summary.skipped += 1;
                    self.emit_progress(Some(path));
                }
                continue;
            }
            if !file_type.is_file() {
                self.summary.skipped += 1;
                self.emit_progress(Some(path));
                continue;
            }
            if !is_supported_audio_path(&path) {
                self.summary.skipped += 1;
                self.emit_progress(Some(path));
                continue;
            }
            self.scan_file(root, path);
        }
    }

    fn scan_file(&mut self, root: &Path, path: PathBuf) {
        self.summary.seen += 1;
        let canonical = canonical_or_original(&path);
        let metadata = match fs::metadata(&canonical) {
            Ok(metadata) => metadata,
            Err(error) => {
                self.errors.push(ScanError {
                    path: canonical.clone(),
                    message: error.to_string(),
                });
                self.emit_progress(Some(canonical));
                return;
            }
        };
        let modified_at = metadata_modified_unix(&metadata);
        let fingerprint = FileFingerprint::path_mtime_size(&canonical, modified_at, metadata.len());
        if let Some(track) = self.previous.find_unchanged(&fingerprint) {
            let mut track = track.clone();
            apply_path_metadata_fallback(&mut track, root, &canonical);
            self.seen_ids.push(track.id.clone());
            self.tracks.push(track);
            self.summary.reused += 1;
            self.emit_progress(Some(canonical));
            return;
        }

        let was_known_path = self.previous.contains_path(&canonical);
        let mut read = metadata::read_track(canonical.clone(), metadata.len(), modified_at);
        if let Some(warning) = read.warning {
            self.errors.push(ScanError {
                path: canonical.clone(),
                message: warning,
            });
        }
        match crate::downloads::read_sidecar(&canonical) {
            Ok(Some(sidecar)) => apply_download_sidecar(&mut read.track, &sidecar),
            Ok(None) => {}
            Err(error) => self.errors.push(ScanError {
                path: crate::downloads::sidecar_path(&canonical),
                message: format!("download sidecar could not be read: {error}"),
            }),
        }
        apply_path_metadata_fallback(&mut read.track, root, &canonical);
        if was_known_path {
            self.summary.changed += 1;
        } else {
            self.summary.added += 1;
        }
        self.seen_ids.push(read.track.id.clone());
        self.tracks.push(read.track);
        self.emit_progress(Some(canonical));
    }

    fn emit_progress(&mut self, current: Option<PathBuf>) {
        (self.progress)(LocalScanProgress {
            seen: self.summary.seen,
            indexed: self.tracks.len(),
            skipped: self.summary.skipped,
            errors: self.errors.len(),
            current,
        });
    }
}

fn apply_download_sidecar(track: &mut LocalTrack, sidecar: &crate::downloads::DownloadSidecar) {
    if let Some(title) = clean_sidecar_text(&sidecar.title) {
        track.title = title;
    }
    if !sidecar.artists.is_empty() {
        track.artist = sidecar
            .artists
            .iter()
            .filter_map(|artist| clean_sidecar_text(artist))
            .collect();
    } else if let Some(artist) = clean_sidecar_text(&sidecar.artist)
        && !artist.eq_ignore_ascii_case("local file")
    {
        track.artist = split_sidecar_people(&artist);
    }
    track.album = sidecar.album.as_deref().and_then(clean_sidecar_text);
    track.album_artist = if sidecar.album_artists.is_empty() {
        sidecar.album_artist.as_deref().and_then(clean_sidecar_text)
    } else {
        Some(sidecar.album_artists.join(", "))
    };
    if track.year.is_none() {
        track.year = sidecar
            .album_release_date
            .as_deref()
            .and_then(|date| date.get(..4))
            .and_then(|year| year.parse::<i32>().ok());
    }
    track.disc_no = sidecar.disc_number;
    track.track_no = sidecar.track_number;
    track.isrc = sidecar.isrc.as_deref().and_then(clean_sidecar_text);
    if track.duration_ms.is_none() {
        track.duration_ms = sidecar.duration_secs.map(|secs| u64::from(secs) * 1000);
    }
    if let Some(id) = sidecar.linked_youtube_id() {
        track.linked_video_id = Some(id.to_owned());
    }
    track.origin_key = sidecar.origin_key.as_deref().and_then(clean_sidecar_text);
    track.origin_url = sidecar.origin_url.as_deref().and_then(clean_sidecar_text);
    track.import_session_id = sidecar
        .import_session_id
        .as_deref()
        .and_then(clean_sidecar_text);
    track.import_source_order = sidecar.import_source_order.filter(|order| *order > 0);
}

/// Fill empty artist/album fields from parent folder names relative to `root`.
///
/// - 0 dirs under root → no hints
/// - 1 dir → artist = that folder
/// - 2+ dirs → artist = first component, album = last component
fn apply_path_metadata_fallback(track: &mut LocalTrack, root: &Path, file: &Path) {
    let Some((artist_hint, album_hint)) = path_metadata_hints(root, file) else {
        return;
    };
    let artist_empty = track.artist.is_empty()
        || track
            .artist
            .iter()
            .all(|a| a.eq_ignore_ascii_case("local file"));
    if artist_empty {
        track.artist = vec![artist_hint.clone()];
    }
    if track
        .album_artist
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        track.album_artist = Some(artist_hint);
    }
    if track
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
        && let Some(album) = album_hint
    {
        track.album = Some(album);
    }
}

fn path_metadata_hints(root: &Path, file: &Path) -> Option<(String, Option<String>)> {
    let parent = file.parent()?;
    let relative = parent.strip_prefix(root).ok()?;
    let components: Vec<String> = relative
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(name) => {
                let s = name.to_string_lossy();
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            }
            _ => None,
        })
        .collect();
    match components.as_slice() {
        [] => None,
        [artist] => Some((artist.clone(), None)),
        [artist, ..] => {
            let album = components.last().cloned();
            Some((artist.clone(), album))
        }
    }
}

fn clean_sidecar_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

fn split_sidecar_people(text: &str) -> Vec<String> {
    text.split(&[';', '/', ','][..])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
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

    fn temp_dir() -> PathBuf {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!(
            "ytm-tui-local-scan-test-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn download_root_walks_two_directory_levels() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("Artist").join("Album").join("extra")).unwrap();
        fs::write(dir.join("a.mp3"), b"not audio").unwrap();
        fs::write(dir.join("Artist").join("b.flac"), b"not audio").unwrap();
        fs::write(dir.join("Artist").join("Album").join("c.m4a"), b"not audio").unwrap();
        fs::write(
            dir.join("Artist").join("Album").join("extra").join("d.wav"),
            b"not audio",
        )
        .unwrap();

        let result = scan_roots(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
        );

        let mut titles: Vec<_> = result
            .index
            .tracks()
            .iter()
            .map(|track| track.title.as_str())
            .collect();
        titles.sort_unstable();
        assert_eq!(titles, vec!["a", "b", "c"]);
        assert_eq!(result.summary.seen, 3);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn path_metadata_fallback_fills_empty_artist_and_album() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("Isegye Rockstar").join("Debut")).unwrap();
        fs::write(
            dir.join("Isegye Rockstar").join("Debut").join("song.mp3"),
            b"not audio",
        )
        .unwrap();
        fs::write(dir.join("Isegye Rockstar").join("cover.flac"), b"not audio").unwrap();

        let result = scan_roots(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
        );

        let album_track = result
            .index
            .tracks()
            .iter()
            .find(|t| t.title == "song")
            .expect("album track");
        assert_eq!(album_track.artist, vec!["Isegye Rockstar"]);
        assert_eq!(album_track.album.as_deref(), Some("Debut"));
        assert_eq!(album_track.album_artist.as_deref(), Some("Isegye Rockstar"));

        let cover = result
            .index
            .tracks()
            .iter()
            .find(|t| t.title == "cover")
            .expect("artist-level track");
        assert_eq!(cover.artist, vec!["Isegye Rockstar"]);
        assert!(cover.album.is_none());
        assert_eq!(cover.album_artist.as_deref(), Some("Isegye Rockstar"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn path_metadata_fallback_does_not_override_sidecar() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("Folder Artist").join("Folder Album")).unwrap();
        let audio = dir
            .join("Folder Artist")
            .join("Folder Album")
            .join("Sidecar Track [abc123def45].m4a");
        fs::write(&audio, b"not audio").unwrap();
        let mut song = crate::api::Song::from_search(
            "abc123def45",
            "Canonical Title",
            "Tag Artist",
            "3:03",
            Some("Tag Album".to_owned()),
        );
        song = song.with_catalog_metadata(
            Some("Tag Album Artist".to_owned()),
            None,
            None,
            None,
            None,
            None,
        );
        crate::downloads::write_sidecar(&song, &audio).unwrap();

        let result = scan_roots(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
        );

        assert_eq!(result.index.tracks().len(), 1);
        let track = &result.index.tracks()[0];
        assert_eq!(track.artist, vec!["Tag Artist"]);
        assert_eq!(track.album.as_deref(), Some("Tag Album"));
        assert_eq!(track.album_artist.as_deref(), Some("Tag Album Artist"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reused_tracks_apply_path_metadata_fallback() {
        let dir = temp_dir();
        let album_dir = dir.join("Folder Artist").join("Folder Album");
        fs::create_dir_all(&album_dir).unwrap();
        fs::write(album_dir.join("song.mp3"), b"not audio").unwrap();

        let first = scan_roots(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
        );
        let mut legacy_track = first.index.tracks()[0].clone();
        legacy_track.artist.clear();
        legacy_track.album = None;
        legacy_track.album_artist = None;
        let mut previous = LocalIndex::default();
        previous.set_tracks(vec![legacy_track]);

        let second = scan_roots(&[LocalScanRoot::download(dir.clone())], &previous);

        assert_eq!(second.summary.reused, 1);
        let track = &second.index.tracks()[0];
        assert_eq!(track.artist, vec!["Folder Artist"]);
        assert_eq!(track.album.as_deref(), Some("Folder Album"));
        assert_eq!(track.album_artist.as_deref(), Some("Folder Artist"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scanner_applies_download_sidecar_metadata() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let audio = dir.join("Sidecar Track [abc123def45].m4a");
        fs::write(&audio, b"not audio").unwrap();
        let mut song = crate::api::Song::from_search(
            "abc123def45",
            "Canonical Title",
            "Artist A, Artist B",
            "3:03",
            Some("Canonical Album".to_owned()),
        );
        song.duration_secs = Some(183);
        song = song
            .with_catalog_metadata(
                Some("Album Artist".to_owned()),
                Some(1),
                Some(4),
                Some("ISRC123".to_owned()),
                Some("spotify:track:abc".to_owned()),
                Some("https://open.spotify.com/track/abc".to_owned()),
            )
            .with_import_metadata(crate::api::SongImportMetadata {
                artists: vec!["Artist A".to_owned(), "Artist B".to_owned()],
                album_artists: vec!["Album Artist".to_owned()],
                album_release_date: Some("2026-07-08".to_owned()),
                album_release_date_precision: Some("day".to_owned()),
                album_total_tracks: Some(10),
                album_type: Some("album".to_owned()),
                album_art_url: Some("https://i.scdn.co/image/cover".to_owned()),
                explicit: Some(false),
            });
        song = song.with_import_session(Some("sp2yt-20260708-abcd".to_owned()), Some(7));
        crate::downloads::write_sidecar(&song, &audio).unwrap();

        let result = scan_roots(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
        );

        assert_eq!(result.index.tracks().len(), 1);
        let track = &result.index.tracks()[0];
        assert_eq!(track.title, "Canonical Title");
        assert_eq!(track.artist, vec!["Artist A", "Artist B"]);
        assert_eq!(track.album.as_deref(), Some("Canonical Album"));
        assert_eq!(track.album_artist.as_deref(), Some("Album Artist"));
        assert_eq!(track.year, Some(2026));
        assert_eq!(track.disc_no, Some(1));
        assert_eq!(track.track_no, Some(4));
        assert_eq!(track.duration_ms, Some(183_000));
        assert_eq!(track.isrc.as_deref(), Some("ISRC123"));
        assert_eq!(track.linked_video_id.as_deref(), Some("abc123def45"));
        assert_eq!(track.origin_key.as_deref(), Some("spotify:track:abc"));
        assert_eq!(
            track.origin_url.as_deref(),
            Some("https://open.spotify.com/track/abc")
        );
        assert_eq!(
            track.import_session_id.as_deref(),
            Some("sp2yt-20260708-abcd")
        );
        assert_eq!(track.import_source_order, Some(7));

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
    fn scanner_reports_progress_while_walking_files() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.mp3"), b"not audio").unwrap();
        fs::write(dir.join("b.flac"), b"not audio").unwrap();
        let mut progress = Vec::new();

        let result = scan_roots_with_progress(
            &[LocalScanRoot::download(dir.clone())],
            &LocalIndex::default(),
            |update| progress.push(update),
        );

        assert_eq!(result.summary.seen, 2);
        assert!(
            progress
                .iter()
                .any(|update| update.seen == 1 && update.indexed == 1),
            "missing first-file progress in {progress:?}"
        );
        assert!(
            progress
                .iter()
                .any(|update| update.seen == 2 && update.indexed == 2),
            "missing second-file progress in {progress:?}"
        );
        assert!(progress.iter().any(|update| {
            update
                .current
                .as_ref()
                .is_some_and(|path| path.ends_with("a.mp3"))
        }));

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
