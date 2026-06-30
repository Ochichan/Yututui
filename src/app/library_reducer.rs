//! Library data/delete reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// Number of rows currently shown in the active library tab — after the in-library
    /// filter, so selection/navigation bounds track what's actually on screen.
    pub(in crate::app) fn library_len(&self) -> usize {
        self.library_rows().len()
    }

    pub fn library_counts(&self) -> [usize; 6] {
        [
            self.all_library_count(),
            self.library
                .favorites
                .iter()
                .filter(|s| !s.is_radio_station())
                .count(),
            self.library
                .history
                .iter()
                .filter(|s| !s.is_radio_station())
                .count(),
            self.radio_favorites_library_count(),
            self.radio_recent_library_count(),
            self.library_ui.downloaded.len(),
        ]
    }

    pub fn library_rows(&self) -> Vec<&Song> {
        let rows = self.library_rows_for(self.library_ui.tab);
        self.apply_library_filter(rows)
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
            .filter(|s| {
                s.title.to_lowercase().contains(&needle)
                    || s.artist.to_lowercase().contains(&needle)
                    || self.display_title(s).to_lowercase().contains(&needle)
                    || self.display_artist(s).to_lowercase().contains(&needle)
            })
            .collect()
    }

    pub(in crate::app) fn library_rows_for(&self, tab: LibraryTab) -> Vec<&Song> {
        match tab {
            LibraryTab::All => self.all_library_rows(),
            LibraryTab::Favorites => self
                .library
                .favorites
                .iter()
                .filter(|s| !s.is_radio_station())
                .collect(),
            LibraryTab::History => self
                .library
                .history
                .iter()
                .filter(|s| !s.is_radio_station())
                .collect(),
            LibraryTab::RadioFavorites => self.radio_favorites_library_rows(),
            LibraryTab::Radio => self.radio_recent_library_rows(),
            LibraryTab::Downloads => self.library_ui.downloaded.iter().collect(),
        }
    }

    pub(in crate::app) fn all_library_rows(&self) -> Vec<&Song> {
        let mut rows = Vec::new();
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
            // Collapse a track that lives in several collections to one row. The exact id
            // catches a favorite that's also in history; the normalized title additionally
            // catches a downloaded file (saved as `<title>.m4a`, so its title matches the
            // catalog title) that duplicates a remote favorite/history entry. First in the
            // chain wins, so the richer catalog entry is preferred over the local file.
            let title_key = song.title.trim().to_lowercase();
            let fresh_id = seen_ids.insert(song.video_id.clone());
            let fresh_title = seen_titles.insert(title_key);
            if fresh_id && fresh_title {
                rows.push(song);
            }
        }
        rows
    }

    fn all_library_count(&self) -> usize {
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
        count
    }

    pub(in crate::app) fn radio_favorites_library_rows(&self) -> Vec<&Song> {
        self.library
            .radio_favorites
            .iter()
            .filter(|song| song.is_radio_station())
            .collect()
    }

    fn radio_favorites_library_count(&self) -> usize {
        self.library
            .radio_favorites
            .iter()
            .filter(|song| song.is_radio_station())
            .count()
    }

    pub(in crate::app) fn radio_recent_library_rows(&self) -> Vec<&Song> {
        let mut rows = Vec::new();
        let mut seen_ids = HashSet::new();
        for song in self.library.radios.iter().filter(|song| {
            song.is_radio_station() && !self.library.is_radio_favorite(&song.video_id)
        }) {
            if seen_ids.insert(song.video_id.as_str()) {
                rows.push(song);
            }
        }
        rows
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
        }
    }

    /// Carry out a confirmed download deletion: remove each file from disk, drop the matching
    /// rows for instant feedback, then rescan the folder as the source of truth. A failed
    /// delete is logged but doesn't abort the rest.
    pub(in crate::app) fn confirm_delete_files_apply(&mut self) -> Vec<Cmd> {
        let Some(paths) = self.library_ui.confirm_delete.take() else {
            return Vec::new();
        };
        for path in &paths {
            if let Err(err) = std::fs::remove_file(path) {
                tracing::warn!(?path, error = %err, "failed to delete downloaded file");
            }
        }
        self.library_ui
            .downloaded
            .retain(|song| song.local_path.as_ref().is_none_or(|p| !paths.contains(p)));
        // Forget the deleted files in the persisted manifest too, so they don't linger.
        self.download_store.remove_paths(&paths);
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
