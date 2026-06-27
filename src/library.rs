//! The local, on-disk library: favorite tracks and recently-played history.
//!
//! This is *our* library, persisted to the data dir — independent of any YouTube
//! account, so it works in anonymous mode. (Account-sourced views — liked songs,
//! your YTM playlists, home recommendations — need a cookie and layer on later.)
//!
//! Both collections are bounded on load/write paths (priority #1: flat memory over long
//! sessions) and de-duplicated by `video_id`. Persistence mirrors [`crate::config`]:
//! pretty JSON written atomically (temp file + rename).

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::api::Song;

/// Caps on the two collections (bounded memory).
const FAVORITES_MAX: usize = 999;
const HISTORY_MAX: usize = 999;
const DOWNLOADS_MAX: usize = 999;
const AUDIO_EXTENSIONS: &[&str] = &["aac", "flac", "m4a", "mp3", "ogg", "opus", "wav", "wma"];

/// Saved tracks and play history, persisted to `<data dir>/library.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    /// Saved tracks, most-recently-favorited first.
    pub favorites: Vec<Song>,
    /// Recently played, most-recent first; capped at [`HISTORY_MAX`].
    pub history: VecDeque<Song>,
}

impl Library {
    /// Load from disk, falling back to an empty library if absent or unreadable.
    pub fn load() -> Self {
        if let Some(path) = library_path()
            && let Ok(text) = fs::read_to_string(&path)
            && let Ok(mut lib) = serde_json::from_str::<Library>(&text)
        {
            lib.trim_to_caps();
            return lib;
        }
        Library::default()
    }

    /// Persist atomically (temp file + rename). A missing data dir is a no-op.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = library_path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)
    }

    pub fn is_favorite(&self, video_id: &str) -> bool {
        self.favorites.iter().any(|s| s.video_id == video_id)
    }

    /// Toggle `song`'s favorite status. Returns `true` if it is now a favorite.
    pub fn toggle_favorite(&mut self, song: &Song) -> bool {
        if let Some(pos) = self
            .favorites
            .iter()
            .position(|s| s.video_id == song.video_id)
        {
            self.favorites.remove(pos);
            false
        } else {
            self.favorites.insert(0, song.clone());
            self.favorites.truncate(FAVORITES_MAX);
            true
        }
    }

    /// Record that `song` is being played: move it to the front of history (de-duping
    /// by id) and trim to the cap.
    pub fn record_play(&mut self, song: &Song) {
        self.history.retain(|s| s.video_id != song.video_id);
        self.history.push_front(song.clone());
        while self.history.len() > HISTORY_MAX {
            self.history.pop_back();
        }
    }

    fn trim_to_caps(&mut self) {
        self.favorites.truncate(FAVORITES_MAX);
        while self.history.len() > HISTORY_MAX {
            self.history.pop_back();
        }
    }
}

fn library_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui").map(|d| d.data_dir().join("library.json"))
}

/// Scan the configured download directory for directly playable local audio files.
///
/// The downloader writes files directly under this folder, so this intentionally stays
/// non-recursive and bounded. Missing/unreadable directories simply produce an empty list.
pub fn scan_downloads(dir: &Path) -> Vec<Song> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let is_file = e.file_type().ok().is_some_and(|t| t.is_file());
            (is_file && is_audio_file(&path)).then_some(path)
        })
        .collect();
    paths.sort_by_key(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()));
    paths.truncate(DOWNLOADS_MAX);
    paths.into_iter().map(Song::local_file).collect()
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            AUDIO_EXTENSIONS
                .iter()
                .any(|known| e.eq_ignore_ascii_case(known))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn song(id: &str) -> Song {
        Song::remote(id, format!("t-{id}"), "a", "1:00")
    }

    #[test]
    fn toggle_favorite_adds_then_removes() {
        let mut lib = Library::default();
        assert!(!lib.is_favorite("x"));
        assert!(lib.toggle_favorite(&song("x"))); // now favorited
        assert!(lib.is_favorite("x"));
        assert_eq!(lib.favorites.len(), 1);
        assert!(!lib.toggle_favorite(&song("x"))); // removed
        assert!(!lib.is_favorite("x"));
        assert!(lib.favorites.is_empty());
    }

    #[test]
    fn newest_favorite_is_first() {
        let mut lib = Library::default();
        lib.toggle_favorite(&song("a"));
        lib.toggle_favorite(&song("b"));
        assert_eq!(lib.favorites[0].video_id, "b");
        assert_eq!(lib.favorites[1].video_id, "a");
    }

    #[test]
    fn history_dedups_and_moves_to_front() {
        let mut lib = Library::default();
        lib.record_play(&song("a"));
        lib.record_play(&song("b"));
        lib.record_play(&song("a")); // replay 'a' -> back to front, no dupe
        let ids: Vec<&str> = lib.history.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn history_is_capped() {
        let mut lib = Library::default();
        for i in 0..(HISTORY_MAX + 25) {
            lib.record_play(&song(&i.to_string()));
        }
        assert_eq!(lib.history.len(), HISTORY_MAX);
        // The most recent play is at the front.
        assert_eq!(
            lib.history.front().unwrap().video_id,
            (HISTORY_MAX + 24).to_string()
        );
    }

    #[test]
    fn loaded_library_is_trimmed_to_caps() {
        let mut lib = Library {
            favorites: (0..(FAVORITES_MAX + 25))
                .map(|i| song(&format!("fav-{i}")))
                .collect(),
            history: (0..(HISTORY_MAX + 25))
                .map(|i| song(&format!("hist-{i}")))
                .collect(),
        };
        lib.trim_to_caps();
        assert_eq!(lib.favorites.len(), FAVORITES_MAX);
        assert_eq!(lib.history.len(), HISTORY_MAX);
    }

    #[test]
    fn json_round_trips() {
        let mut lib = Library::default();
        lib.toggle_favorite(&song("fav"));
        lib.record_play(&song("hist"));
        let s = serde_json::to_string(&lib).unwrap();
        let back: Library = serde_json::from_str(&s).unwrap();
        assert!(back.is_favorite("fav"));
        assert_eq!(back.history.front().unwrap().video_id, "hist");
    }

    #[test]
    fn missing_fields_default() {
        let back: Library = serde_json::from_str("{}").unwrap();
        assert!(back.favorites.is_empty());
        assert!(back.history.is_empty());
    }

    #[test]
    fn scan_downloads_finds_supported_audio_files() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("b.MP3"), b"").unwrap();
        fs::write(dir.join("a.m4a"), b"").unwrap();
        fs::write(dir.join("note.txt"), b"").unwrap();
        fs::create_dir_all(dir.join("album")).unwrap();
        fs::write(dir.join("album").join("nested.flac"), b"").unwrap();

        let songs = scan_downloads(&dir);
        let titles: Vec<&str> = songs.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b"]);
        assert!(songs.iter().all(Song::is_local));
        assert!(songs.iter().all(|s| s.video_id.starts_with("local:")));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_downloads_ignores_missing_dirs() {
        assert!(scan_downloads(&temp_dir().join("missing")).is_empty());
    }

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ytm-tui-library-test-{}-{nanos}",
            std::process::id()
        ))
    }
}
