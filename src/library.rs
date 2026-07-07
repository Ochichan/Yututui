//! The local, on-disk library: favorite tracks, recently-played history, and radio stations.
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
use crate::util::safe_fs;

/// Caps on the collections (bounded memory).
const FAVORITES_MAX: usize = 999;
const HISTORY_MAX: usize = 999;
const RADIO_FAVORITES_MAX: usize = 999;
const RADIOS_MAX: usize = 999;
const DOWNLOADS_MAX: usize = 999;
const AUDIO_EXTENSIONS: &[&str] = &["aac", "flac", "m4a", "mp3", "ogg", "opus", "wav", "wma"];

#[derive(Debug, Clone, Default)]
pub struct DownloadScan {
    pub songs: Vec<Song>,
    pub truncated: bool,
    pub limit: usize,
}

/// Saved tracks and play history, persisted to `<data dir>/library.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    /// Saved tracks, most-recently-favorited first.
    pub favorites: Vec<Song>,
    /// Recently played, most-recent first; capped at [`HISTORY_MAX`].
    pub history: VecDeque<Song>,
    /// Favorite live radio stations, most-recently-favorited first; capped separately from songs.
    pub radio_favorites: Vec<Song>,
    /// Recently played live radio stations, most-recent first; capped at [`RADIOS_MAX`].
    pub radios: VecDeque<Song>,
    /// Mutation counter for downstream caches (library row cache, id-recovery memo).
    /// Every `&mut self` method bumps it; direct field mutation (tests) is covered by the
    /// caches also keying on collection lengths.
    #[serde(skip)]
    pub(crate) rev: u64,
}

impl Library {
    /// Load from disk, falling back to an empty library if absent or unreadable.
    pub fn load() -> Self {
        let Some(path) = library_path() else {
            return Library::default();
        };
        // Schema-drift tolerant: one changed field no longer discards saved tracks/history.
        // Size-capped like the sibling Playlists load: a corrupt/oversized library.json is set
        // aside instead of being read wholesale into memory at startup.
        const MAX_BYTES: u64 = 50 * 1024 * 1024;
        let mut lib = safe_fs::load_json_or_default_limited::<Library>(&path, MAX_BYTES);
        lib.trim_to_caps();
        lib
    }

    /// Persist atomically (temp file + rename). A missing data dir is a no-op.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = library_path() else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
    }

    pub fn is_favorite(&self, video_id: &str) -> bool {
        self.favorites.iter().any(|s| s.video_id == video_id) || self.is_radio_favorite(video_id)
    }

    pub fn is_radio_favorite(&self, video_id: &str) -> bool {
        self.radio_favorites.iter().any(|s| s.video_id == video_id)
    }

    fn touch(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }

    /// Toggle `song`'s favorite status. Returns `true` if it is now a favorite.
    pub fn toggle_favorite(&mut self, song: &Song) -> bool {
        self.touch();
        if song.is_radio_station() {
            return self.toggle_radio_favorite(song);
        }
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

    /// Toggle a live radio station's favorite status, separate from track favorites.
    pub fn toggle_radio_favorite(&mut self, song: &Song) -> bool {
        self.touch();
        if !song.is_radio_station() {
            return false;
        }
        self.remove_from_song_lists(&song.video_id);
        if let Some(pos) = self
            .radio_favorites
            .iter()
            .position(|s| s.video_id == song.video_id)
        {
            self.radio_favorites.remove(pos);
            false
        } else {
            self.radio_favorites.insert(0, song.clone());
            self.radio_favorites.truncate(RADIO_FAVORITES_MAX);
            true
        }
    }

    /// Remove the favorite at `index` (position in [`Self::favorites`]). Returns whether a
    /// track was removed — `false` for an out-of-range index. Powers the library's delete.
    pub fn remove_favorite_at(&mut self, index: usize) -> bool {
        self.touch();
        if index < self.favorites.len() {
            self.favorites.remove(index);
            true
        } else {
            false
        }
    }

    /// Remove the history entry at `index` (position in [`Self::history`]). Returns whether a
    /// track was removed — `false` for an out-of-range index. Powers the library's delete.
    pub fn remove_history_at(&mut self, index: usize) -> bool {
        self.touch();
        self.history.remove(index).is_some()
    }

    /// Remove a station from radio favorites only, leaving its recent-play entry intact.
    pub fn remove_radio_favorite_by_id(&mut self, video_id: &str) -> bool {
        self.touch();
        let before = self.radio_favorites.len();
        self.radio_favorites.retain(|s| s.video_id != video_id);
        self.radio_favorites.len() != before
    }

    /// Remove a station from recently played radio only, leaving its favorite status intact.
    pub fn remove_radio_recent_by_id(&mut self, video_id: &str) -> bool {
        self.touch();
        let before = self.radios.len();
        self.radios.retain(|s| s.video_id != video_id);
        self.radios.len() != before
    }

    /// Record that `song` is being played. Normal tracks move to the front of history; live radio
    /// stations move to the Radio tab instead so they never feed song history.
    pub fn record_play(&mut self, song: &Song) {
        self.touch();
        if song.is_radio_station() {
            self.record_radio(song);
            return;
        }
        self.history.retain(|s| s.video_id != song.video_id);
        self.history.push_front(song.clone());
        while self.history.len() > HISTORY_MAX {
            self.history.pop_back();
        }
    }

    /// Record a live radio station in its own list, de-duped by id and kept out of song lists.
    pub fn record_radio(&mut self, song: &Song) {
        self.touch();
        if !song.is_radio_station() {
            return;
        }
        self.remove_from_song_lists(&song.video_id);
        self.radios.retain(|s| s.video_id != song.video_id);
        self.radios.push_front(song.clone());
        while self.radios.len() > RADIOS_MAX {
            self.radios.pop_back();
        }
    }

    fn trim_to_caps(&mut self) {
        // Bound the raw lists BEFORE normalize: on a bloated/corrupt/synced library.json,
        // normalize_radio_entries is O(n²) (a `retain`/`insert` per moved radio), which would
        // burn startup CPU. A generous multiple of the caps leaves every realistic library
        // untouched while capping the worst case.
        self.favorites.truncate(FAVORITES_MAX.saturating_mul(4));
        self.radio_favorites
            .truncate(RADIO_FAVORITES_MAX.saturating_mul(4));
        self.history.truncate(HISTORY_MAX.saturating_mul(4));
        self.radios.truncate(RADIOS_MAX.saturating_mul(4));
        self.normalize_radio_entries();
        self.favorites.truncate(FAVORITES_MAX);
        while self.history.len() > HISTORY_MAX {
            self.history.pop_back();
        }
        self.radio_favorites.truncate(RADIO_FAVORITES_MAX);
        while self.radios.len() > RADIOS_MAX {
            self.radios.pop_back();
        }
    }

    fn normalize_radio_entries(&mut self) {
        let mut moved_radio_favorites = Vec::new();
        let mut moved_radios = Vec::new();

        let mut favorites = Vec::with_capacity(self.favorites.len());
        for song in self.favorites.drain(..) {
            if song.is_radio_station() {
                moved_radio_favorites.push(song);
            } else {
                favorites.push(song);
            }
        }
        self.favorites = favorites;

        let mut history = VecDeque::with_capacity(self.history.len());
        while let Some(song) = self.history.pop_front() {
            if song.is_radio_station() {
                moved_radios.push(song);
            } else {
                history.push_back(song);
            }
        }
        self.history = history;

        for song in self.radio_favorites.drain(..) {
            if song.is_radio_station() {
                moved_radio_favorites.push(song);
            }
        }

        while let Some(song) = self.radios.pop_front() {
            if song.is_radio_station() {
                moved_radios.push(song);
            }
        }

        for song in moved_radio_favorites.into_iter().rev() {
            self.add_radio_favorite_front(song);
        }
        for song in moved_radios.into_iter().rev() {
            self.record_radio(&song);
        }
    }

    fn add_radio_favorite_front(&mut self, song: Song) {
        self.radio_favorites.retain(|s| s.video_id != song.video_id);
        self.radio_favorites.insert(0, song);
        self.radio_favorites.truncate(RADIO_FAVORITES_MAX);
    }

    fn remove_from_song_lists(&mut self, video_id: &str) {
        self.favorites.retain(|s| s.video_id != video_id);
        self.history.retain(|s| s.video_id != video_id);
    }
}

fn library_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("library.json"))
}

/// Scan the configured download directory for directly playable local audio files.
///
/// The downloader writes files directly under this folder, so this intentionally stays
/// non-recursive and bounded. Missing/unreadable directories simply produce an empty list.
pub fn scan_downloads(dir: &Path) -> DownloadScan {
    let Ok(entries) = fs::read_dir(dir) else {
        return DownloadScan {
            limit: DOWNLOADS_MAX,
            ..DownloadScan::default()
        };
    };
    // Bounded top-K by lowercased filename: keep at most DOWNLOADS_MAX entries while streaming
    // the directory (a max-heap drops the current largest once full), so a folder with far more
    // files than the cap uses O(cap) memory instead of collecting + sorting the whole listing.
    // Output is the alphabetically-first DOWNLOADS_MAX — the same set/order as before.
    use std::collections::BinaryHeap;
    let mut heap: BinaryHeap<(String, PathBuf)> = BinaryHeap::new();
    let mut truncated = false;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let is_file = entry.file_type().ok().is_some_and(|t| t.is_file());
        if !(is_file && is_audio_file(&path)) {
            continue;
        }
        let key = path
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        heap.push((key, path));
        if heap.len() > DOWNLOADS_MAX {
            truncated = true;
            heap.pop(); // drop the largest key, keeping the smallest DOWNLOADS_MAX
        }
    }
    let mut kept = heap.into_vec();
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    DownloadScan {
        songs: kept.into_iter().map(|(_, p)| Song::local_file(p)).collect(),
        truncated,
        limit: DOWNLOADS_MAX,
    }
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

    fn radio(id: &str) -> Song {
        Song::from_source(
            crate::search_source::SearchSource::RadioBrowser,
            id,
            format!("radio-{id}"),
            "KR / MP3",
            "",
            crate::api::PlayableRef::RadioStream {
                url: format!("https://example.com/{id}.mp3"),
            },
        )
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
    fn radio_play_and_favorite_stay_out_of_song_lists() {
        let mut lib = Library::default();
        let station = radio("station-a");

        lib.record_play(&station);
        assert!(lib.history.is_empty());
        assert_eq!(
            lib.radios.front().map(|s| s.video_id.as_str()),
            Some("rad:station-a")
        );

        assert!(lib.toggle_favorite(&station));
        assert!(lib.favorites.is_empty());
        assert!(lib.is_favorite("rad:station-a"));
        assert!(lib.is_radio_favorite("rad:station-a"));
        assert_eq!(lib.radio_favorites[0].video_id, "rad:station-a");

        assert!(!lib.toggle_favorite(&station));
        assert!(!lib.is_favorite("rad:station-a"));
        assert!(lib.favorites.is_empty());
        assert_eq!(lib.radios.len(), 1);
    }

    #[test]
    fn remove_favorite_at_drops_the_indexed_track() {
        let mut lib = Library::default();
        lib.toggle_favorite(&song("a"));
        lib.toggle_favorite(&song("b")); // b is now first (newest-first)
        assert!(lib.remove_favorite_at(0)); // removes b
        let ids: Vec<&str> = lib.favorites.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(ids, vec!["a"]);
        assert!(!lib.remove_favorite_at(5)); // out of range
        assert_eq!(lib.favorites.len(), 1);
    }

    #[test]
    fn remove_history_at_drops_the_indexed_track() {
        let mut lib = Library::default();
        lib.record_play(&song("a"));
        lib.record_play(&song("b")); // history front-to-back: b, a
        assert!(lib.remove_history_at(1)); // removes a
        let ids: Vec<&str> = lib.history.iter().map(|s| s.video_id.as_str()).collect();
        assert_eq!(ids, vec!["b"]);
        assert!(!lib.remove_history_at(9)); // out of range
        assert_eq!(lib.history.len(), 1);
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
            ..Library::default()
        };
        lib.trim_to_caps();
        assert_eq!(lib.favorites.len(), FAVORITES_MAX);
        assert_eq!(lib.history.len(), HISTORY_MAX);
    }

    #[test]
    fn trim_moves_legacy_radio_entries_out_of_song_lists() {
        let fav_radio = radio("fav-radio");
        let hist_radio = radio("hist-radio");
        let mut lib = Library {
            favorites: vec![fav_radio],
            history: VecDeque::from(vec![hist_radio]),
            ..Library::default()
        };

        lib.trim_to_caps();

        assert!(lib.favorites.is_empty());
        assert!(lib.history.is_empty());
        assert_eq!(
            lib.radio_favorites
                .iter()
                .map(|s| s.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["rad:fav-radio"]
        );
        assert_eq!(
            lib.radios
                .iter()
                .map(|s| s.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["rad:hist-radio"]
        );
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
        assert!(back.radio_favorites.is_empty());
        assert!(back.radios.is_empty());
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

        let scan = scan_downloads(&dir);
        let titles: Vec<&str> = scan.songs.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["a", "b"]);
        assert!(!scan.truncated);
        assert_eq!(scan.limit, DOWNLOADS_MAX);
        assert!(scan.songs.iter().all(Song::is_local));
        assert!(scan.songs.iter().all(|s| s.video_id.starts_with("local:")));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_downloads_ignores_missing_dirs() {
        let scan = scan_downloads(&temp_dir().join("missing"));
        assert!(scan.songs.is_empty());
        assert!(!scan.truncated);
        assert_eq!(scan.limit, DOWNLOADS_MAX);
    }

    #[test]
    fn scan_downloads_reports_when_cap_hides_files() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        for i in 0..=DOWNLOADS_MAX {
            fs::write(dir.join(format!("track{i:04}.mp3")), b"").unwrap();
        }

        let scan = scan_downloads(&dir);

        assert!(scan.truncated);
        assert_eq!(scan.songs.len(), DOWNLOADS_MAX);
        assert_eq!(
            scan.songs.first().map(|s| s.title.as_str()),
            Some("track0000")
        );
        assert_eq!(
            scan.songs.last().map(|s| s.title.as_str()),
            Some("track0998")
        );

        let _ = fs::remove_dir_all(dir);
    }

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "yututui-library-test-{}-{nanos}",
            std::process::id()
        ))
    }
}
