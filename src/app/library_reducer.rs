//! Library data/delete reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// Number of rows currently shown in the active library tab — after the in-library
    /// filter, so selection/navigation bounds track what's actually on screen. At the
    /// Playlists root the rows are playlists, not songs.
    pub(in crate::app) fn library_len(&self) -> usize {
        if self.playlists_root() {
            return self.filtered_playlists().len();
        }
        self.library_rows_len()
    }

    /// Row count for the active tab *without* materializing the `Vec<&Song>` that
    /// [`Self::library_rows`] allocates — rows are 1:1 with the cached slots (each slot
    /// resolves to exactly one row; see [`Self::rows_from_slots`]). Called on every library
    /// nav key and wheel-scroll, so this must stay O(1) on a cache hit rather than rebuilding
    /// and throwing away a full row vector just to read its length.
    fn library_rows_len(&self) -> usize {
        let tab = self.effective_library_tab();
        // Playlist drill-down rows are built uncached (an open playlist is small); count them
        // the same way `library_rows` builds them, so the two never disagree.
        if matches!(tab, LibraryTab::Playlists) {
            return self.apply_library_filter(self.open_playlist_rows()).len();
        }
        let key = self.library_rows_key(tab);
        if let Some(cache) = self.library_rows_cache.borrow().as_ref()
            && cache.key == key
        {
            return cache.slots.len();
        }
        // Cache miss: build + store the slots exactly as `library_rows` would (so a following
        // `library_rows` call in the same frame hits the cache), and return the slot count.
        let slots = self.build_library_slots(tab);
        let n = slots.len();
        *self.library_rows_cache.borrow_mut() = Some(LibraryRowsCache { key, slots });
        n
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

    /// Row indices of the effective library selection: the Ctrl/Cmd-picked rows when any
    /// exist, else the inclusive drag/selection range between the anchor and the cursor.
    pub(in crate::app) fn library_selection_indices(&self) -> Vec<usize> {
        effective_selection_indices(
            &self.library_ui.picked,
            self.library_ui.selected,
            self.library_ui.anchor,
            self.library_len(),
        )
    }

    /// Tracks in the current library selection (picked rows or the drag range), in
    /// visible row order.
    pub(in crate::app) fn selected_library_songs(&self) -> Vec<Song> {
        let songs = self.library_songs();
        self.library_selection_indices()
            .into_iter()
            .filter_map(|i| songs.get(i).cloned())
            .collect()
    }

    /// Queue the current library tab (starting at the cursor) and start playing.
    pub(in crate::app) fn play_from_library(&mut self) -> Vec<Cmd> {
        let songs = self.library_songs();
        if songs.is_empty() {
            return Vec::new();
        }
        self.replace_queue_and_load(
            songs,
            self.library_ui.selected,
            None,
            QueueReplacementOptions {
                player_mode: true,
                romanize_all: true,
                ..QueueReplacementOptions::default()
            },
        )
    }

    /// Delete the library list's current selection — the Ctrl/Cmd-picked rows, or the
    /// inclusive range between the drag anchor and the cursor — using the active tab's
    /// delete semantics.
    pub(in crate::app) fn library_delete_selection(&mut self) -> Vec<Cmd> {
        let rows = self.library_selection_indices();
        self.library_delete_indices(&rows)
    }

    /// Delete library rows `lo..=hi` (positions in the current tab); see
    /// [`Self::library_delete_indices`].
    pub(in crate::app) fn library_delete_rows(&mut self, lo: usize, hi: usize) -> Vec<Cmd> {
        let rows: Vec<usize> = (lo..=hi).collect();
        self.library_delete_indices(&rows)
    }

    /// Delete the library rows at `indices` (ascending positions in the current tab) with
    /// per-tab meaning: Favorites un-favorites, History forgets, Radio Favorites un-favorites
    /// radio stations, Radio forgets recently played stations, Downloads asks before deleting
    /// the files on disk, and All is an aggregate view so it's read-only. Clamps the
    /// selection afterward.
    fn library_delete_indices(&mut self, indices: &[usize]) -> Vec<Cmd> {
        // The Playlists root lists playlists, not songs — the song-target resolution below
        // would bail on its empty song list, so route to the delete-confirm modal first.
        // Deleting is deliberately single-row (the first index): dropping several whole
        // playlists in one keypress is too destructive for a range gesture.
        if self.playlists_root() {
            if let Some(&first) = indices.first() {
                self.request_playlist_delete(first);
            }
            return Vec::new();
        }
        // Resolve the displayed (possibly filtered) rows to concrete songs first, then delete
        // by identity. Under an active filter the row positions no longer map to the raw
        // collection indices, so an index-based removal would hit the wrong tracks.
        let songs = self.library_songs();
        let targets: Vec<Song> = indices
            .iter()
            .filter_map(|&i| songs.get(i).cloned())
            .collect();
        if targets.is_empty() {
            return Vec::new();
        }
        let targets = &targets[..];
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
                vec![Cmd::Persist(PersistCmd::Library)]
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
                vec![Cmd::Persist(PersistCmd::Library)]
            }
            LibraryTab::RadioFavorites => {
                let mut any = false;
                for song in targets {
                    any |= self.library.remove_radio_favorite_by_id(&song.video_id);
                }
                if any {
                    self.clamp_library_selection();
                    self.dirty = true;
                    vec![Cmd::Persist(PersistCmd::Library)]
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
                    vec![Cmd::Persist(PersistCmd::Library)]
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
                    vec![Cmd::Persist(PersistCmd::Playlists)]
                } else {
                    Vec::new()
                }
            }
        }
    }

    /// Submit a confirmed deletion as an owned runtime effect. The reducer must never unlink a
    /// file before the process read-only/recovery gate has admitted the mutation.
    pub(in crate::app) fn confirm_delete_files_apply(&mut self) -> Vec<Cmd> {
        let Some(paths) = self.library_ui.confirm_delete.take() else {
            return Vec::new();
        };
        let root = self.config.effective_download_dir();
        self.dirty = true;
        vec![Cmd::Download(DownloadCmd::Delete { paths, root })]
    }

    pub(in crate::app) fn apply_deleted_downloads(
        &mut self,
        root: PathBuf,
        deleted: Vec<PathBuf>,
        failed: usize,
    ) -> Vec<Cmd> {
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
        if failed > 0 {
            self.set_status_error(if crate::i18n::is_korean() {
                format!("다운로드 파일 {failed}개를 삭제하지 못했습니다")
            } else {
                format!("Could not delete {failed} downloaded file(s)")
            });
        }
        let mut commands = vec![Cmd::Download(DownloadCmd::Scan(root))];
        if !deleted.is_empty() {
            commands.push(Cmd::Persist(PersistCmd::Downloads));
        }
        commands
    }

    /// Clamp the library cursor and the drag anchor into the current tab's row count, and
    /// drop any Ctrl/Cmd-picked rows — the list just mutated, so their indices no longer
    /// name the rows the user picked.
    pub(in crate::app) fn clamp_library_selection(&mut self) {
        let last = self.library_len().saturating_sub(1);
        self.library_ui.selected = self.library_ui.selected.min(last);
        self.library_ui.anchor = self.library_ui.anchor.min(last);
        self.library_ui.picked.clear();
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
