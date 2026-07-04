//! Library data/delete reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;
use crate::util::sanitize;

impl App {
    /// Number of rows currently shown in the active library tab — after the in-library
    /// filter, so selection/navigation bounds track what's actually on screen. At the
    /// Playlists root the rows are playlists, not songs.
    pub(in crate::app) fn library_len(&self) -> usize {
        if self.playlists_root() {
            return self.filtered_playlists().len();
        }
        self.library_rows().len()
    }

    pub fn library_count_for(&self, tab: LibraryTab) -> usize {
        match tab {
            LibraryTab::All => self.all_library_count(),
            LibraryTab::Favorites => self
                .library
                .favorites
                .iter()
                .filter(|s| !s.is_radio_station())
                .count(),
            LibraryTab::History => self
                .library
                .history
                .iter()
                .filter(|s| !s.is_radio_station())
                .count(),
            LibraryTab::RadioFavorites => self.radio_favorites_library_count(),
            LibraryTab::Radio => self.radio_recent_library_count(),
            LibraryTab::Downloads => self.library_ui.downloaded.len(),
            LibraryTab::Playlists => self.playlists.list().len(),
        }
    }

    pub fn library_rows(&self) -> Vec<&Song> {
        let tab = self.effective_library_tab();
        // Playlist drill-down rows borrow from the playlists store, whose mutation
        // surface (create/rename/add/remove/reorder) is too broad to key cheaply —
        // and an open playlist is small. Build those uncached.
        if matches!(tab, LibraryTab::Playlists) {
            let rows = self.open_playlist_rows();
            return self.apply_library_filter(rows);
        }
        let key = self.library_rows_key(tab);
        if let Some(cache) = self.library_rows_cache.borrow().as_ref()
            && cache.key == key
        {
            return self.rows_from_slots(&cache.slots);
        }
        let slots = self.build_library_slots(tab);
        let rows = self.rows_from_slots(&slots);
        *self.library_rows_cache.borrow_mut() = Some(LibraryRowsCache { key, slots });
        rows
    }

    /// Narrow `rows` to the active in-library filter — a case-insensitive substring match on
    /// the title or artist. Returns `rows` unchanged when no filter is set. The single choke
    /// point so the displayed list, selection bounds, and every row operation (play /
    /// favorite / download / delete) all see the same filtered set.
    fn apply_library_filter<'a>(&self, rows: Vec<&'a Song>) -> Vec<&'a Song> {
        let needle = self.library_ui.filter_query.trim().to_lowercase();
        if needle.is_empty() {
            return rows;
        }
        rows.into_iter()
            .filter(|s| self.song_matches_filter(s, &needle))
            .collect()
    }

    /// Shared by the Library filter and the Search results-filter popup so both narrow
    /// with identical semantics (`needle` is expected pre-trimmed and lowercased).
    pub(in crate::app) fn song_matches_filter(&self, s: &Song, needle: &str) -> bool {
        s.title.to_lowercase().contains(needle)
            || s.artist.to_lowercase().contains(needle)
            || self.display_title(s).to_lowercase().contains(needle)
            || self.display_artist(s).to_lowercase().contains(needle)
    }

    /// Song rows of the opened playlist; the Playlists root level lists the playlists
    /// themselves (see `filtered_playlists`).
    fn open_playlist_rows(&self) -> Vec<&Song> {
        self.library_ui
            .open_playlist
            .as_ref()
            .and_then(|key| self.playlists.find(key))
            .map(|p| p.songs.iter().collect())
            .unwrap_or_default()
    }

    fn library_rows_key(&self, tab: LibraryTab) -> RowsKey {
        RowsKey {
            library_rev: self.library.rev,
            downloaded_rev: self.library_ui.downloaded_rev,
            romanize_rev: self.romanization.cache.rev(),
            fav_len: self.library.favorites.len(),
            hist_len: self.library.history.len(),
            radio_fav_len: self.library.radio_favorites.len(),
            radios_len: self.library.radios.len(),
            down_len: self.library_ui.downloaded.len(),
            tab,
            filter: self.library_ui.filter_query.clone(),
            romanized_on: self.config.effective_romanized_titles(),
        }
    }

    /// Resolve cached slots back to rows. `.get` + debug_assert so a hypothetically
    /// missed invalidation degrades to a dropped row in release, never a render panic.
    fn rows_from_slots(&self, slots: &[RowSlot]) -> Vec<&Song> {
        slots
            .iter()
            .filter_map(|slot| {
                let song = match *slot {
                    RowSlot::Fav(i) => self.library.favorites.get(i as usize),
                    RowSlot::Hist(i) => self.library.history.get(i as usize),
                    RowSlot::RadioFav(i) => self.library.radio_favorites.get(i as usize),
                    RowSlot::RadioRecent(i) => self.library.radios.get(i as usize),
                    RowSlot::Down(i) => self.library_ui.downloaded.get(i as usize),
                };
                debug_assert!(song.is_some(), "stale library row slot {slot:?}");
                song
            })
            .collect()
    }

    /// The uncached row computation: per-tab source selection (+ the All-tab dedup),
    /// then the in-library filter, producing stable (collection, index) slots.
    fn build_library_slots(&self, tab: LibraryTab) -> Vec<RowSlot> {
        let mut slots: Vec<RowSlot> = match tab {
            LibraryTab::All => {
                let mut slots = Vec::new();
                let mut seen_ids = HashSet::new();
                let mut seen_titles = HashSet::new();
                let favs = self
                    .library
                    .favorites
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (RowSlot::Fav(i as u32), s));
                let hist = self
                    .library
                    .history
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (RowSlot::Hist(i as u32), s));
                let down = self
                    .library_ui
                    .downloaded
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (RowSlot::Down(i as u32), s));
                for (slot, song) in favs
                    .chain(hist)
                    .chain(down)
                    .filter(|(_, song)| !song.is_radio_station())
                {
                    // Collapse a track that lives in several collections to one row. The exact id
                    // catches a favorite that's also in history; the normalized title additionally
                    // catches a downloaded file (saved as `<title>.m4a`, so its title matches the
                    // catalog title) that duplicates a remote favorite/history entry. First in the
                    // chain wins, so the richer catalog entry is preferred over the local file.
                    let title_key = song.title.trim().to_lowercase();
                    let fresh_id = seen_ids.insert(song.video_id.as_str());
                    let fresh_title = seen_titles.insert(title_key);
                    if fresh_id && fresh_title {
                        slots.push(slot);
                    }
                }
                slots
            }
            LibraryTab::Favorites => self
                .library
                .favorites
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.is_radio_station())
                .map(|(i, _)| RowSlot::Fav(i as u32))
                .collect(),
            LibraryTab::History => self
                .library
                .history
                .iter()
                .enumerate()
                .filter(|(_, s)| !s.is_radio_station())
                .map(|(i, _)| RowSlot::Hist(i as u32))
                .collect(),
            LibraryTab::RadioFavorites => self
                .library
                .radio_favorites
                .iter()
                .enumerate()
                .filter(|(_, s)| s.is_radio_station())
                .map(|(i, _)| RowSlot::RadioFav(i as u32))
                .collect(),
            LibraryTab::Radio => {
                let mut seen_ids = HashSet::new();
                self.library
                    .radios
                    .iter()
                    .enumerate()
                    .filter(|(_, song)| {
                        song.is_radio_station()
                            && !self.library.is_radio_favorite(&song.video_id)
                            && seen_ids.insert(song.video_id.as_str())
                    })
                    .map(|(i, _)| RowSlot::RadioRecent(i as u32))
                    .collect()
            }
            LibraryTab::Downloads => (0..self.library_ui.downloaded.len())
                .map(|i| RowSlot::Down(i as u32))
                .collect(),
            // Handled (uncached) in `library_rows` before we get here.
            LibraryTab::Playlists => Vec::new(),
        };
        let needle = self.library_ui.filter_query.trim().to_lowercase();
        if !needle.is_empty() {
            let matches = |slot: &RowSlot| -> bool {
                let song = match *slot {
                    RowSlot::Fav(i) => self.library.favorites.get(i as usize),
                    RowSlot::Hist(i) => self.library.history.get(i as usize),
                    RowSlot::RadioFav(i) => self.library.radio_favorites.get(i as usize),
                    RowSlot::RadioRecent(i) => self.library.radios.get(i as usize),
                    RowSlot::Down(i) => self.library_ui.downloaded.get(i as usize),
                };
                song.is_some_and(|s| self.song_matches_filter(s, &needle))
            };
            slots.retain(matches);
        }
        slots
    }

    fn all_library_count(&self) -> usize {
        let key: AllCountKey = (
            self.library.rev,
            self.library_ui.downloaded_rev,
            self.library.favorites.len(),
            self.library.history.len(),
            self.library_ui.downloaded.len(),
        );
        if let Some((cached_key, count)) = self.all_count_cache.get()
            && cached_key == key
        {
            return count;
        }
        let mut count = 0usize;
        let mut seen_ids = HashSet::new();
        let mut seen_titles = HashSet::new();
        for song in self
            .library
            .favorites
            .iter()
            .chain(self.library.history.iter())
            .chain(self.library_ui.downloaded.iter())
            .filter(|song| !song.is_radio_station())
        {
            let title_key = song.title.trim().to_lowercase();
            let fresh_id = seen_ids.insert(song.video_id.as_str());
            let fresh_title = seen_titles.insert(title_key);
            if fresh_id && fresh_title {
                count += 1;
            }
        }
        self.all_count_cache.set(Some((key, count)));
        count
    }

    fn radio_favorites_library_count(&self) -> usize {
        self.library
            .radio_favorites
            .iter()
            .filter(|song| song.is_radio_station())
            .count()
    }

    fn radio_recent_library_count(&self) -> usize {
        let mut seen_ids = HashSet::new();
        self.library
            .radios
            .iter()
            .filter(|song| song.is_radio_station())
            .filter(|song| !self.library.is_radio_favorite(&song.video_id))
            .filter(|song| seen_ids.insert(song.video_id.as_str()))
            .count()
    }

    pub(in crate::app) fn library_songs(&self) -> Vec<Song> {
        self.library_rows().into_iter().cloned().collect()
    }

    /// The track under the library cursor, if any.
    pub(in crate::app) fn selected_library_song(&self) -> Option<Song> {
        self.library_songs().get(self.library_ui.selected).cloned()
    }

    /// Tracks in the current library drag/selection range, in visible row order.
    pub(in crate::app) fn selected_library_songs(&self) -> Vec<Song> {
        let songs = self.library_songs();
        if songs.is_empty() {
            return Vec::new();
        }
        let lo = self.library_ui.selected.min(self.library_ui.anchor);
        if lo >= songs.len() {
            return Vec::new();
        }
        let hi = self
            .library_ui
            .selected
            .max(self.library_ui.anchor)
            .min(songs.len() - 1);
        songs[lo..=hi].to_vec()
    }

    /// Queue the current library tab (starting at the cursor) and start playing.
    pub(in crate::app) fn play_from_library(&mut self) -> Vec<Cmd> {
        let songs = self.library_songs();
        if songs.is_empty() {
            return Vec::new();
        }
        let requested_songs = songs.clone();
        self.queue.set(songs, self.library_ui.selected);
        self.mode = Mode::Player;
        self.status.text.clear();
        let song = self.queue.current().cloned();
        let mut cmds = self.load_song(song);
        cmds.extend(self.request_romanization_for_songs(&requested_songs));
        cmds
    }

    /// Delete the library list's current selection — the inclusive range between the drag
    /// anchor and the cursor — using the active tab's delete semantics.
    pub(in crate::app) fn library_delete_selection(&mut self) -> Vec<Cmd> {
        let lo = self.library_ui.selected.min(self.library_ui.anchor);
        let hi = self.library_ui.selected.max(self.library_ui.anchor);
        self.library_delete_rows(lo, hi)
    }

    /// Delete library rows `lo..=hi` (positions in the current tab) with per-tab meaning:
    /// Favorites un-favorites, History forgets, Radio Favorites un-favorites radio stations,
    /// Radio forgets recently played stations, Downloads asks before deleting the files on disk,
    /// and All is an aggregate view so it's read-only. Clamps the selection afterward.
    pub(in crate::app) fn library_delete_rows(&mut self, lo: usize, hi: usize) -> Vec<Cmd> {
        // The Playlists root lists playlists, not songs — the song-target resolution below
        // would bail on its empty song list, so route to the delete-confirm modal first.
        // Deleting is deliberately single-row (`lo`): dropping several whole playlists in
        // one keypress is too destructive for a range gesture.
        if self.playlists_root() {
            self.request_playlist_delete(lo);
            return Vec::new();
        }
        // Resolve the displayed (possibly filtered) rows to concrete songs first, then delete
        // by identity. Under an active filter the row positions no longer map to the raw
        // collection indices, so an index-based removal would hit the wrong tracks.
        let targets = self.library_songs();
        if lo >= targets.len() {
            return Vec::new();
        }
        let hi = hi.min(targets.len() - 1);
        let targets = &targets[lo..=hi];
        match self.library_ui.tab {
            // Aggregate view — a row may live in several tabs, so deleting from here is
            // ambiguous. Manage tracks from their own tab instead.
            LibraryTab::All => Vec::new(),
            LibraryTab::Favorites => {
                for song in targets {
                    if let Some(pos) = self
                        .library
                        .favorites
                        .iter()
                        .position(|s| s.video_id == song.video_id)
                    {
                        self.library.remove_favorite_at(pos);
                    }
                }
                self.clamp_library_selection();
                self.dirty = true;
                vec![Cmd::SaveLibrary]
            }
            LibraryTab::History => {
                for song in targets {
                    if let Some(pos) = self
                        .library
                        .history
                        .iter()
                        .position(|s| s.video_id == song.video_id)
                    {
                        self.library.remove_history_at(pos);
                    }
                }
                self.clamp_library_selection();
                self.dirty = true;
                vec![Cmd::SaveLibrary]
            }
            LibraryTab::RadioFavorites => {
                let mut any = false;
                for song in targets {
                    any |= self.library.remove_radio_favorite_by_id(&song.video_id);
                }
                if any {
                    self.clamp_library_selection();
                    self.dirty = true;
                    vec![Cmd::SaveLibrary]
                } else {
                    Vec::new()
                }
            }
            LibraryTab::Radio => {
                let mut any = false;
                for song in targets {
                    any |= self.library.remove_radio_recent_by_id(&song.video_id);
                }
                if any {
                    self.clamp_library_selection();
                    self.dirty = true;
                    vec![Cmd::SaveLibrary]
                } else {
                    Vec::new()
                }
            }
            LibraryTab::Downloads => {
                // Deleting real files is irreversible — gather the paths and ask first.
                let paths: Vec<PathBuf> = targets
                    .iter()
                    .filter_map(|song| song.local_path.clone())
                    .collect();
                if !paths.is_empty() {
                    self.library_ui.confirm_delete = Some(paths);
                    self.dirty = true;
                }
                Vec::new()
            }
            // Drill-down rows of an opened playlist: remove the selected tracks from it
            // (the playlist itself stays; root-level deletion is handled above).
            LibraryTab::Playlists => {
                let Some(key) = self.library_ui.open_playlist.clone() else {
                    return Vec::new();
                };
                let mut any = false;
                for song in targets {
                    any |= self.playlists.remove_song(&key, &song.video_id);
                }
                if any {
                    self.clamp_library_selection();
                    self.dirty = true;
                    vec![Cmd::SavePlaylists]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Carry out a confirmed download deletion: remove each file from disk, drop the matching
    /// rows for instant feedback, then rescan the folder as the source of truth. A failed
    /// delete is logged but doesn't abort the rest.
    pub(in crate::app) fn confirm_delete_files_apply(&mut self) -> Vec<Cmd> {
        let Some(paths) = self.library_ui.confirm_delete.take() else {
            return Vec::new();
        };
        let root = self.config.effective_download_dir();
        let mut deleted = Vec::new();
        for path in &paths {
            match remove_download_file_if_safe(path, &root) {
                Ok(()) => deleted.push(path.clone()),
                Err(err) => {
                    tracing::warn!(
                        path = %sanitize::sanitize_error_text(path.display().to_string()),
                        error = %sanitize::sanitize_error_text(err.to_string()),
                        "refused or failed to delete downloaded file"
                    );
                }
            }
        }
        self.library_ui.downloaded_rev = self.library_ui.downloaded_rev.wrapping_add(1);
        self.library_ui.downloaded.retain(|song| {
            song.local_path
                .as_ref()
                .is_none_or(|p| !deleted.contains(p))
        });
        // Forget the deleted files in the persisted manifest too, so they don't linger.
        self.download_store.remove_paths(&deleted);
        self.clamp_library_selection();
        self.dirty = true;
        vec![
            Cmd::ScanDownloads(self.config.effective_download_dir()),
            Cmd::SaveDownloads,
        ]
    }

    /// Clamp the library cursor and the drag anchor into the current tab's row count.
    pub(in crate::app) fn clamp_library_selection(&mut self) {
        let last = self.library_len().saturating_sub(1);
        self.library_ui.selected = self.library_ui.selected.min(last);
        self.library_ui.anchor = self.library_ui.anchor.min(last);
    }
}

/// A visible library row, addressed as (source collection, index) — small, `Copy`, and
/// resolvable back to `&Song` in O(1). Cached across frames in `App::library_rows_cache`.
#[derive(Clone, Copy, Debug)]
pub(in crate::app) enum RowSlot {
    Fav(u32),
    Hist(u32),
    RadioFav(u32),
    RadioRecent(u32),
    Down(u32),
}

/// Everything the row computation reads. Revs cover in-place mutation through the store
/// methods; the lengths are belt-and-suspenders for direct collection pushes (tests).
#[derive(PartialEq, Eq)]
pub(in crate::app) struct RowsKey {
    library_rev: u64,
    downloaded_rev: u64,
    romanize_rev: u64,
    fav_len: usize,
    hist_len: usize,
    radio_fav_len: usize,
    radios_len: usize,
    down_len: usize,
    tab: LibraryTab,
    filter: String,
    romanized_on: bool,
}

pub(in crate::app) struct LibraryRowsCache {
    key: RowsKey,
    slots: Vec<RowSlot>,
}

/// (library_rev, downloaded_rev, favorites len, history len, downloaded len) — the All-tab
/// count reads nothing else.
pub(in crate::app) type AllCountKey = (u64, u64, usize, usize, usize);

fn remove_download_file_if_safe(
    path: &std::path::Path,
    download_dir: &std::path::Path,
) -> std::io::Result<()> {
    let root = download_dir.canonicalize()?;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to delete symlink",
        ));
    }
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to delete non-regular file",
        ));
    }
    let target = path.canonicalize()?;
    if !target.starts_with(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to delete outside download directory",
        ));
    }
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "download file changed before delete",
        ));
    }
    std::fs::remove_file(path)
}
