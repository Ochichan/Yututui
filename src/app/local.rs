//! Local Deck reducer helpers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::local_format::*;
use super::*;
use crate::util::query::{MAX_FILTER_QUERY_BYTES, try_push_query_char};

impl App {
    pub(in crate::app) fn request_local_mode_switch(&mut self) -> Vec<Cmd> {
        if !self.local_dedicated_mode && self.radio_dedicated_mode {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "Leave Radio mode before entering Local Player.",
                "로컬 플레이어로 들어가기 전에 라디오 모드를 먼저 나가세요."
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }

        self.local_mode.pending_confirm = Some(if self.local_dedicated_mode {
            LocalModeConfirm::Exit
        } else {
            LocalModeConfirm::Enter
        });
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.queue_popup.open = false;
        self.search_filter.close();
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn apply_local_mode_confirm(
        &mut self,
        confirm: LocalModeConfirm,
    ) -> Vec<Cmd> {
        self.local_mode.pending_confirm = None;
        match confirm {
            LocalModeConfirm::Enter => self.enter_local_dedicated_mode(),
            LocalModeConfirm::Exit => self.exit_local_dedicated_mode(),
        }
    }

    fn enter_local_dedicated_mode(&mut self) -> Vec<Cmd> {
        if self.local_dedicated_mode || self.radio_dedicated_mode {
            return Vec::new();
        }
        self.local_mode.normal_mode_queue = Some(self.queue.snapshot());
        self.activate_local_dedicated_mode_ui();
        let restore = self.local_mode.local_mode_queue.take();
        let mut cmds = self.stop_clear_and_restore_queue_for_mode_switch(restore);
        cmds.extend(self.ensure_local_index_ready());
        self.status.kind = StatusKind::Info;
        self.status.text = t!("Local Player mode enabled", "로컬 플레이어 모드 켜짐").to_owned();
        self.dirty = true;
        cmds
    }

    pub(in crate::app) fn activate_local_dedicated_mode_ui(&mut self) {
        self.local_dedicated_mode = true;
        self.mode = Mode::Library;
        self.local_mode.ui.section = if self.local_track_rows_len() == 0 {
            LocalSection::Home
        } else {
            LocalSection::Tracks
        };
        self.local_mode.ui.selected = self
            .local_mode
            .ui
            .selected
            .min(self.local_rows_len().saturating_sub(1));
        self.local_mode.ui.anchor = self.local_mode.ui.selected;
        self.bridges.library_scroll.reset();
        self.clear_library_filter();
        self.reset_playlist_ui_state();
        self.dirty = true;
    }

    fn exit_local_dedicated_mode(&mut self) -> Vec<Cmd> {
        if !self.local_dedicated_mode {
            return Vec::new();
        }
        self.local_mode.local_mode_queue = Some(self.queue.snapshot());
        self.local_dedicated_mode = false;
        self.local_mode.pending_confirm = None;
        self.bridges.library_scroll.reset();
        let restore = self.local_mode.normal_mode_queue.take();
        let cmds = self.stop_clear_and_restore_queue_for_mode_switch(restore);
        self.status.kind = StatusKind::Info;
        self.status.text = t!("Local Player mode disabled", "로컬 플레이어 모드 꺼짐").to_owned();
        self.dirty = true;
        cmds
    }

    pub fn local_rows_len(&self) -> usize {
        self.local_visible_rows().len()
    }

    pub(in crate::app) fn on_key_local(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let chord = crate::keymap::Chord::from(k);
        // Keep the remappable Local Deck toggle available for non-text keys while the
        // Local Deck filter is focused. Typeable remaps still belong to the filter input.
        if self.local_mode.ui.filter_editing
            && !chord.is_typeable()
            && matches!(
                self.keymap.action(KeyContext::Library, chord),
                Some(Action::ToggleLocalMode)
            )
        {
            return self.request_local_mode_switch();
        }
        if self.local_mode.ui.filter_editing {
            return self.on_key_local_filter(k);
        }
        if k.code == KeyCode::Esc && !self.local_mode.ui.filter_query.is_empty() {
            self.clear_local_filter();
            self.dirty = true;
            return Vec::new();
        }
        if k.code == KeyCode::Esc {
            return self.local_back_or_exit();
        }
        if k.code == KeyCode::Tab && k.modifiers.is_empty() {
            self.cycle_local_pane();
            return Vec::new();
        }
        if k.modifiers.is_empty() {
            match k.code {
                KeyCode::Char(' ') => return self.on_player_action(Action::TogglePause),
                KeyCode::Char('a') => return self.local_enqueue_selected(),
                KeyCode::Char('c') => {
                    self.open_queue_popup();
                    return Vec::new();
                }
                KeyCode::Char('s') => return self.local_shuffle_visible(),
                _ => {}
            }
        }
        if matches!(k.code, KeyCode::Char('A'))
            && (k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT)
        {
            return self.local_enqueue_visible();
        }
        if matches!(k.code, KeyCode::Char('P'))
            && (k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT)
        {
            return self.local_play_selected_now();
        }
        if let KeyCode::Char(ch) = k.code
            && k.modifiers.is_empty()
            && let Some(section) = LocalSection::from_digit(ch)
        {
            self.switch_local_section(section);
            return Vec::new();
        }
        if k.modifiers.is_empty() && matches!(k.code, KeyCode::Char('r')) {
            return self.request_local_scan(false);
        }
        if matches!(k.code, KeyCode::Char('R'))
            && (k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT)
        {
            return self.request_local_scan(true);
        }

        match self.keymap.action(KeyContext::Library, chord) {
            Some(Action::ToggleLocalMode) => self.request_local_mode_switch(),
            Some(Action::LibraryFilter) => {
                self.local_mode.ui.filter_editing = true;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::Back) => self.local_back_or_exit(),
            Some(Action::MoveUp) => {
                let step = self.nav_repeat_step(Action::MoveUp);
                if self.local_mode.ui.pane == LocalPane::Sidebar {
                    self.move_local_sidebar(true);
                } else {
                    self.move_local_cursor(true, step);
                }
                Vec::new()
            }
            Some(Action::MoveDown) => {
                let step = self.nav_repeat_step(Action::MoveDown);
                if self.local_mode.ui.pane == LocalPane::Sidebar {
                    self.move_local_sidebar(false);
                } else {
                    self.move_local_cursor(false, step);
                }
                Vec::new()
            }
            Some(Action::PageUp) => {
                self.move_local_cursor(true, self.page_step());
                Vec::new()
            }
            Some(Action::PageDown) => {
                self.move_local_cursor(false, self.page_step());
                Vec::new()
            }
            Some(Action::JumpTop) => {
                self.local_mode.ui.selected = 0;
                self.local_mode.ui.anchor = 0;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::JumpBottom) => {
                self.local_mode.ui.selected = self.local_rows_len().saturating_sub(1);
                self.local_mode.ui.anchor = self.local_mode.ui.selected;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::Confirm) => self.local_activate_selected(),
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn local_row_click(&mut self, index: usize) -> Vec<Cmd> {
        if index < self.local_rows_len() {
            self.local_mode.ui.selected = index;
            self.local_mode.ui.anchor = index;
            self.dirty = true;
        }
        Vec::new()
    }

    pub(in crate::app) fn local_row_activate(&mut self, index: usize) -> Vec<Cmd> {
        if index >= self.local_rows_len() {
            return Vec::new();
        }
        self.local_mode.ui.selected = index;
        self.local_mode.ui.anchor = index;
        self.local_activate_selected()
    }

    pub(in crate::app) fn local_enqueue_row_index(&mut self, index: usize) -> Vec<Cmd> {
        let Some(row) = self.local_visible_rows().get(index).cloned() else {
            return Vec::new();
        };
        self.local_mode.ui.selected = index;
        self.local_mode.ui.anchor = index;
        self.enqueue_many(self.local_songs_for_row(&row))
    }

    fn move_local_cursor(&mut self, up: bool, step: usize) {
        let len = self.local_rows_len();
        if len == 0 {
            self.local_mode.ui.selected = 0;
            self.local_mode.ui.anchor = 0;
            self.dirty = true;
            return;
        }
        let step = step.max(1);
        self.local_mode.ui.selected = if up {
            self.local_mode.ui.selected.saturating_sub(step)
        } else {
            self.local_mode
                .ui
                .selected
                .saturating_add(step)
                .min(len - 1)
        };
        self.local_mode.ui.anchor = self.local_mode.ui.selected;
        self.dirty = true;
    }

    fn local_activate_selected(&mut self) -> Vec<Cmd> {
        if self.local_mode.ui.section == LocalSection::Home {
            self.switch_local_section(LocalSection::Tracks);
            return Vec::new();
        }
        let Some(row) = self
            .local_visible_rows()
            .get(self.local_mode.ui.selected)
            .cloned()
        else {
            return Vec::new();
        };
        self.activate_local_row(row)
    }

    fn local_enqueue_selected(&mut self) -> Vec<Cmd> {
        self.enqueue_many(self.local_selected_songs())
    }

    fn local_enqueue_visible(&mut self) -> Vec<Cmd> {
        let (songs, _) = self.local_visible_songs_with_start();
        self.enqueue_many(songs)
    }

    fn local_play_selected_now(&mut self) -> Vec<Cmd> {
        self.play_now_many(self.local_selected_songs())
    }

    fn local_shuffle_visible(&mut self) -> Vec<Cmd> {
        let (songs, start) = self.local_visible_songs_with_start();
        if songs.is_empty() {
            return Vec::new();
        }
        let requested_songs = songs.clone();
        let shuffle_changed = !self.queue.shuffle;
        self.queue.set(songs, start);
        self.queue.set_shuffle(true);
        self.mode = Mode::Player;
        self.status.text.clear();
        let song = self.queue.current().cloned();
        let mut cmds = self.load_song(song);
        cmds.extend(self.request_romanization_for_songs(&requested_songs));
        if shuffle_changed {
            cmds.push(self.save_playback_modes_cmd());
        }
        cmds
    }

    pub(in crate::app) fn apply_local_msg(&mut self, msg: LocalMsg) -> Vec<Cmd> {
        match msg {
            LocalMsg::IndexLoaded {
                index_path,
                index,
                warnings,
            } => {
                self.local_mode.index.index_path = index_path;
                self.local_mode.index.index = index;
                self.local_mode.index.loaded = true;
                self.local_mode.index.loading = false;
                self.local_mode.index.scanning = false;
                self.local_mode.index.progress = None;
                self.local_mode.index.load_errors = warnings;
                self.local_mode.index.errors.clear();
                self.clamp_local_after_index_change();
                self.dirty = true;
                if !self.local_mode.index.load_errors.is_empty() {
                    self.status.kind = StatusKind::Error;
                    self.status.text = format!(
                        "{}: {} {}",
                        t!("Local index recovered", "로컬 인덱스 복구됨"),
                        self.local_mode.index.load_errors.len(),
                        t!("issues", "문제")
                    );
                }
                if self.local_dedicated_mode && self.local_mode.index.index.is_empty() {
                    return self.request_local_scan(false);
                }
                Vec::new()
            }
            LocalMsg::ScanFinished { index_path, result } => {
                self.local_mode.index.index_path = index_path;
                self.local_mode.index.index = result.index;
                self.local_mode.index.loaded = true;
                self.local_mode.index.loading = false;
                self.local_mode.index.scanning = false;
                self.local_mode.index.progress = None;
                self.local_mode.index.last_summary = Some(result.summary.clone());
                self.local_mode.index.errors = result.errors;
                self.clamp_local_after_index_change();
                self.status.kind = if self.local_mode.index.errors.is_empty() {
                    StatusKind::Info
                } else {
                    StatusKind::Error
                };
                self.status.text = format!(
                    "{}: {} {}",
                    t!("Local scan finished", "로컬 스캔 완료"),
                    result.summary.indexed,
                    t!("tracks", "곡")
                );
                self.dirty = true;
                Vec::new()
            }
            LocalMsg::ScanProgress(progress) => {
                if self.local_mode.index.scanning {
                    self.local_mode.index.progress = Some(progress.clone());
                    self.status.kind = StatusKind::Info;
                    self.status.text = format_local_scan_progress(&progress);
                    self.dirty = true;
                }
                Vec::new()
            }
            LocalMsg::ScanFailed { error } => {
                self.local_mode.index.loading = false;
                self.local_mode.index.scanning = false;
                self.local_mode.index.progress = None;
                self.status.kind = StatusKind::Error;
                self.status.text =
                    format!("{}: {error}", t!("Local scan failed", "로컬 스캔 실패"));
                self.dirty = true;
                Vec::new()
            }
        }
    }

    fn ensure_local_index_ready(&mut self) -> Vec<Cmd> {
        if self.local_mode.index.loading || self.local_mode.index.scanning {
            return Vec::new();
        }
        if !self.local_mode.index.loaded {
            let index_path = crate::local::default_index_path();
            self.local_mode.index.index_path = index_path.clone();
            self.local_mode.index.loading = true;
            self.dirty = true;
            return vec![Cmd::Local(LocalCmd::LoadIndex { index_path })];
        }
        if self.local_mode.index.index.is_empty() {
            return self.request_local_scan(false);
        }
        Vec::new()
    }

    pub(in crate::app) fn request_local_scan(&mut self, full: bool) -> Vec<Cmd> {
        if self.local_mode.index.scanning {
            return Vec::new();
        }
        let roots = self.local_scan_roots();
        if roots.is_empty() {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "No local music roots configured",
                "설정된 로컬 음악 폴더가 없습니다."
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        self.local_mode.index.loaded = true;
        self.local_mode.index.loading = false;
        self.local_mode.index.scanning = true;
        self.local_mode.index.progress = Some(crate::local::LocalScanProgress::default());
        self.local_mode.index.errors.clear();
        self.status.kind = StatusKind::Info;
        self.status.text = t!("Scanning local music...", "로컬 음악 스캔 중...").to_owned();
        self.dirty = true;
        let previous = if full {
            crate::local::LocalIndex::default()
        } else {
            self.local_mode.index.index.clone()
        };
        let index_path = self
            .local_mode
            .index
            .index_path
            .clone()
            .or_else(crate::local::default_index_path);
        self.local_mode.index.index_path = index_path.clone();
        vec![Cmd::Local(LocalCmd::ScanRoots {
            roots,
            index_path,
            previous,
        })]
    }

    pub(in crate::app) fn local_scan_roots(&self) -> Vec<crate::local::LocalScanRoot> {
        let mut roots = Vec::new();
        if self.config.local.include_download_dir() {
            push_local_scan_root(
                &mut roots,
                crate::local::LocalScanRoot::download(self.config.effective_download_dir()),
            );
        }
        for root in &self.config.local.roots {
            if !root.enabled() {
                continue;
            }
            let Some(path) = root.normalized_path() else {
                continue;
            };
            push_local_scan_root(
                &mut roots,
                crate::local::LocalScanRoot {
                    path,
                    recursive: root.recursive(),
                },
            );
        }
        roots
    }

    fn local_track_rows_len(&self) -> usize {
        self.local_track_rows_for_query(self.local_mode.ui.filter_query.as_str())
            .len()
    }

    fn clamp_local_after_index_change(&mut self) {
        if !self.local_mode.index.index.is_empty()
            && self.local_mode.ui.section == LocalSection::Home
        {
            self.local_mode.ui.section = LocalSection::Tracks;
        }
        self.local_mode.ui.drill.clear();
        let len = self.local_rows_len();
        self.local_mode.ui.selected = self.local_mode.ui.selected.min(len.saturating_sub(1));
        self.local_mode.ui.anchor = self.local_mode.ui.selected;
        self.bridges.library_scroll.reset();
    }

    pub(in crate::app) fn switch_local_section(&mut self, section: LocalSection) {
        self.local_mode.ui.section = section;
        self.local_mode.ui.drill.clear();
        self.local_mode.ui.selected = 0;
        self.local_mode.ui.anchor = 0;
        self.local_mode.ui.pane = LocalPane::List;
        self.bridges.library_scroll.reset();
        self.dirty = true;
    }

    fn cycle_local_pane(&mut self) {
        self.local_mode.ui.pane = match self.local_mode.ui.pane {
            LocalPane::Sidebar => LocalPane::List,
            LocalPane::List => LocalPane::Sidebar,
        };
        self.dirty = true;
    }

    fn move_local_sidebar(&mut self, up: bool) {
        let current = LocalSection::ALL
            .iter()
            .position(|section| *section == self.local_mode.ui.section)
            .unwrap_or_default();
        let next = if up {
            current.saturating_sub(1)
        } else {
            (current + 1).min(LocalSection::ALL.len().saturating_sub(1))
        };
        if let Some(section) = LocalSection::ALL.get(next).copied() {
            self.switch_local_section(section);
            self.local_mode.ui.pane = LocalPane::Sidebar;
        }
    }

    fn local_back_or_exit(&mut self) -> Vec<Cmd> {
        if self.local_mode.ui.drill.pop().is_some() {
            self.local_mode.ui.selected = 0;
            self.local_mode.ui.anchor = 0;
            self.bridges.library_scroll.reset();
            self.dirty = true;
            return Vec::new();
        }
        self.request_local_mode_switch()
    }

    pub(crate) fn local_visible_rows(&self) -> Vec<crate::local::LocalRowId> {
        self.local_rows_for_query(self.local_mode.ui.filter_query.as_str())
    }

    pub(crate) fn local_total_rows_len(&self) -> usize {
        self.local_rows_for_query("").len()
    }

    pub(crate) fn local_scan_progress_text(&self) -> Option<String> {
        self.local_mode
            .index
            .progress
            .as_ref()
            .map(format_local_scan_progress)
    }

    fn local_rows_for_query(&self, query: &str) -> Vec<crate::local::LocalRowId> {
        if let Some(drill) = self.local_mode.ui.drill.last() {
            return self.local_drill_rows(drill, query);
        }
        match self.local_mode.ui.section {
            LocalSection::Home => Vec::new(),
            LocalSection::Tracks => self.local_track_rows_for_query(query),
            LocalSection::Albums => self.local_album_rows_for_query(query),
            LocalSection::Artists => self.local_artist_rows_for_query(query),
            LocalSection::Genres => self.local_genre_rows_for_query(query),
            LocalSection::Folders => self.local_folder_rows_for_query(query),
            LocalSection::SmartLists => self.local_smart_rows_for_query(query),
            LocalSection::ScanErrors => self.local_scan_error_rows_for_query(query),
            LocalSection::ImportSessions => self.local_import_session_rows_for_query(query),
        }
    }

    fn local_drill_rows(&self, drill: &LocalDrill, query: &str) -> Vec<crate::local::LocalRowId> {
        match drill {
            LocalDrill::Album(album_id) => self
                .local_tracks_for_album(album_id)
                .into_iter()
                .filter(|track| crate::local::query::track_matches_filter(track, query))
                .map(|track| crate::local::LocalRowId::Track(track.id.clone()))
                .collect(),
            LocalDrill::Artist(artist_id) => {
                let albums = self.local_albums_for_artist(artist_id);
                if albums.is_empty() {
                    return self
                        .local_tracks_for_artist(artist_id)
                        .into_iter()
                        .filter(|track| crate::local::query::track_matches_filter(track, query))
                        .map(|track| crate::local::LocalRowId::Track(track.id.clone()))
                        .collect();
                }
                albums
                    .into_iter()
                    .filter(|album| local_album_matches_filter(album, query))
                    .map(|album| crate::local::LocalRowId::Album(album.id))
                    .collect()
            }
            LocalDrill::Genre(genre) => self
                .local_tracks_for_genre(genre)
                .into_iter()
                .filter(|track| crate::local::query::track_matches_filter(track, query))
                .map(|track| crate::local::LocalRowId::Track(track.id.clone()))
                .collect(),
            LocalDrill::Folder(folder) => self
                .local_tracks_for_folder(folder)
                .into_iter()
                .filter(|track| crate::local::query::track_matches_filter(track, query))
                .map(|track| crate::local::LocalRowId::Track(track.id.clone()))
                .collect(),
            LocalDrill::Smart(smart) => self
                .local_tracks_for_smart(*smart)
                .into_iter()
                .filter(|track| crate::local::query::track_matches_filter(track, query))
                .map(|track| crate::local::LocalRowId::Track(track.id.clone()))
                .collect(),
            LocalDrill::ImportSession(session_id) => self
                .local_tracks_for_import_session(session_id)
                .into_iter()
                .filter(|track| crate::local::query::track_matches_filter(track, query))
                .map(|track| crate::local::LocalRowId::Track(track.id.clone()))
                .collect(),
        }
    }

    fn local_track_rows_for_query(&self, query: &str) -> Vec<crate::local::LocalRowId> {
        if !self.local_mode.index.index.is_empty() {
            return self
                .local_mode
                .index
                .index
                .tracks()
                .iter()
                .filter(|track| crate::local::query::track_matches_filter(track, query))
                .map(|track| crate::local::LocalRowId::Track(track.id.clone()))
                .collect();
        }
        self.library_ui
            .downloaded
            .iter()
            .enumerate()
            .filter(|(_, song)| {
                crate::local::query::fields_match_query(
                    [
                        song.title.as_str(),
                        song.artist.as_str(),
                        song.album.as_deref().unwrap_or_default(),
                        song.local_path
                            .as_ref()
                            .and_then(|path| path.file_name())
                            .and_then(|name| name.to_str())
                            .unwrap_or_default(),
                    ],
                    query,
                )
            })
            .map(|(index, _)| crate::local::LocalRowId::DownloadSeed(index))
            .collect()
    }

    fn local_album_rows_for_query(&self, query: &str) -> Vec<crate::local::LocalRowId> {
        self.local_mode
            .index
            .index
            .albums()
            .into_iter()
            .filter(|album| local_album_matches_filter(album, query))
            .map(|album| crate::local::LocalRowId::Album(album.id))
            .collect()
    }

    fn local_artist_rows_for_query(&self, query: &str) -> Vec<crate::local::LocalRowId> {
        self.local_mode
            .index
            .index
            .artists()
            .into_iter()
            .filter(|artist| {
                let albums = artist.album_ids.len().to_string();
                let tracks = artist.track_ids.len().to_string();
                crate::local::query::fields_match_query(
                    [artist.name.as_str(), albums.as_str(), tracks.as_str()],
                    query,
                )
            })
            .map(|artist| crate::local::LocalRowId::Artist(artist.id))
            .collect()
    }

    fn local_genre_rows_for_query(&self, query: &str) -> Vec<crate::local::LocalRowId> {
        let mut genres = BTreeSet::new();
        for track in self.local_mode.index.index.tracks() {
            for genre in &track.genre {
                let genre = genre.trim();
                if !genre.is_empty() {
                    genres.insert(genre.to_owned());
                }
            }
        }
        genres
            .into_iter()
            .filter(|genre| crate::local::query::fields_match_query([genre.as_str()], query))
            .map(crate::local::LocalRowId::Genre)
            .collect()
    }

    fn local_folder_rows_for_query(&self, query: &str) -> Vec<crate::local::LocalRowId> {
        let mut folders = BTreeSet::<PathBuf>::new();
        for track in self.local_mode.index.index.tracks() {
            if let Some(parent) = track.path.parent() {
                folders.insert(parent.to_path_buf());
            }
        }
        folders
            .into_iter()
            .filter(|folder| {
                let name = folder
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default();
                let path = folder.to_string_lossy();
                crate::local::query::fields_match_query([name, path.as_ref()], query)
            })
            .map(crate::local::LocalRowId::Folder)
            .collect()
    }

    fn local_smart_rows_for_query(&self, query: &str) -> Vec<crate::local::LocalRowId> {
        crate::local::LocalSmartList::ALL
            .into_iter()
            .filter(|smart| crate::local::query::fields_match_query([smart.label()], query))
            .map(crate::local::LocalRowId::Smart)
            .collect()
    }

    fn local_import_session_rows_for_query(&self, query: &str) -> Vec<crate::local::LocalRowId> {
        let mut sessions = BTreeMap::<
            String,
            (
                usize,
                i64,
                Option<crate::transfer::session::ImportSessionSummary>,
            ),
        >::new();
        for track in self.local_mode.index.index.tracks() {
            let Some(session_id) = track
                .import_session_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            let entry =
                sessions
                    .entry(session_id.to_owned())
                    .or_insert((0, track.modified_at, None));
            entry.0 += 1;
            entry.1 = entry.1.max(track.modified_at);
        }
        for summary in crate::transfer::session::ImportSession::list_all() {
            let entry =
                sessions
                    .entry(summary.session_id.clone())
                    .or_insert((0, summary.updated_at, None));
            entry.1 = entry.1.max(summary.updated_at);
            entry.2 = Some(summary);
        }
        let mut rows: Vec<_> = sessions.into_iter().collect();
        rows.sort_by(|a, b| b.1.1.cmp(&a.1.1).then_with(|| a.0.cmp(&b.0)));
        rows.into_iter()
            .filter(|(session_id, (count, _, summary))| {
                let count = count.to_string();
                let total = summary
                    .as_ref()
                    .map(|summary| summary.counts.total.to_string())
                    .unwrap_or_default();
                let review = summary
                    .as_ref()
                    .map(|summary| summary.counts.ambiguous.to_string())
                    .unwrap_or_default();
                let missing = summary
                    .as_ref()
                    .map(|summary| summary.counts.not_found.to_string())
                    .unwrap_or_default();
                crate::local::query::fields_match_query(
                    [
                        session_id.as_str(),
                        count.as_str(),
                        total.as_str(),
                        review.as_str(),
                        missing.as_str(),
                    ],
                    query,
                )
            })
            .map(|(session_id, _)| crate::local::LocalRowId::ImportSession(session_id))
            .collect()
    }

    fn local_scan_error_rows_for_query(&self, query: &str) -> Vec<crate::local::LocalRowId> {
        self.local_mode
            .index
            .load_errors
            .iter()
            .chain(self.local_mode.index.errors.iter())
            .enumerate()
            .filter(|(_, error)| {
                let path = error.path.to_string_lossy();
                crate::local::query::fields_match_query(
                    [path.as_ref(), error.message.as_str()],
                    query,
                )
            })
            .map(|(index, _)| crate::local::LocalRowId::ScanError(index))
            .collect()
    }

    fn local_scan_issue(&self, index: usize) -> Option<&crate::local::ScanError> {
        self.local_mode
            .index
            .load_errors
            .iter()
            .chain(self.local_mode.index.errors.iter())
            .nth(index)
    }

    pub(crate) fn local_scan_issue_count(&self) -> usize {
        self.local_mode.index.load_errors.len() + self.local_mode.index.errors.len()
    }

    fn activate_local_row(&mut self, row: crate::local::LocalRowId) -> Vec<Cmd> {
        match row {
            crate::local::LocalRowId::Track(id) => {
                let Some(song) = self.local_track_by_id(&id).map(|track| track.to_song()) else {
                    return Vec::new();
                };
                self.play_now(song)
            }
            crate::local::LocalRowId::DownloadSeed(index) => {
                let Some(song) = self.library_ui.downloaded.get(index).cloned() else {
                    return Vec::new();
                };
                self.play_now(song)
            }
            crate::local::LocalRowId::Album(id) => {
                self.local_mode.ui.drill.push(LocalDrill::Album(id));
                self.reset_local_list_cursor();
                Vec::new()
            }
            crate::local::LocalRowId::Artist(id) => {
                self.local_mode.ui.drill.push(LocalDrill::Artist(id));
                self.reset_local_list_cursor();
                Vec::new()
            }
            crate::local::LocalRowId::Genre(genre) => {
                self.local_mode.ui.drill.push(LocalDrill::Genre(genre));
                self.reset_local_list_cursor();
                Vec::new()
            }
            crate::local::LocalRowId::Folder(folder) => {
                self.local_mode.ui.drill.push(LocalDrill::Folder(folder));
                self.reset_local_list_cursor();
                Vec::new()
            }
            crate::local::LocalRowId::Smart(smart) => {
                self.local_mode.ui.drill.push(LocalDrill::Smart(smart));
                self.reset_local_list_cursor();
                Vec::new()
            }
            crate::local::LocalRowId::ImportSession(session_id) => {
                self.local_mode
                    .ui
                    .drill
                    .push(LocalDrill::ImportSession(session_id));
                self.reset_local_list_cursor();
                Vec::new()
            }
            crate::local::LocalRowId::ScanError(_) => Vec::new(),
        }
    }

    fn local_selected_songs(&self) -> Vec<Song> {
        let Some(row) = self
            .local_visible_rows()
            .get(self.local_mode.ui.selected)
            .cloned()
        else {
            return Vec::new();
        };
        self.local_songs_for_row(&row)
    }

    fn local_visible_songs_with_start(&self) -> (Vec<Song>, usize) {
        let rows = self.local_visible_rows();
        if rows.is_empty() {
            return (Vec::new(), 0);
        }
        let cursor = self.local_mode.ui.selected.min(rows.len() - 1);
        let mut songs = Vec::new();
        let mut seen = BTreeSet::new();
        let mut start = None;
        for (index, row) in rows.iter().enumerate() {
            for song in self.local_songs_for_row(row) {
                if seen.insert(song.video_id.clone()) {
                    if index == cursor && start.is_none() {
                        start = Some(songs.len());
                    }
                    songs.push(song);
                }
            }
        }
        (songs, start.unwrap_or(0))
    }

    fn local_songs_for_row(&self, row: &crate::local::LocalRowId) -> Vec<Song> {
        match row {
            crate::local::LocalRowId::Track(id) => self
                .local_track_by_id(id)
                .map(|track| vec![track.to_song()])
                .unwrap_or_default(),
            crate::local::LocalRowId::DownloadSeed(index) => self
                .library_ui
                .downloaded
                .get(*index)
                .cloned()
                .into_iter()
                .collect(),
            crate::local::LocalRowId::Album(id) => self
                .local_tracks_for_album(id)
                .into_iter()
                .map(|track| track.to_song())
                .collect(),
            crate::local::LocalRowId::Artist(id) => self
                .local_tracks_for_artist(id)
                .into_iter()
                .map(|track| track.to_song())
                .collect(),
            crate::local::LocalRowId::Genre(genre) => self
                .local_tracks_for_genre(genre)
                .into_iter()
                .map(|track| track.to_song())
                .collect(),
            crate::local::LocalRowId::Folder(folder) => self
                .local_tracks_for_folder(folder)
                .into_iter()
                .map(|track| track.to_song())
                .collect(),
            crate::local::LocalRowId::Smart(smart) => self
                .local_tracks_for_smart(*smart)
                .into_iter()
                .map(|track| track.to_song())
                .collect(),
            crate::local::LocalRowId::ImportSession(session_id) => self
                .local_tracks_for_import_session(session_id)
                .into_iter()
                .map(|track| track.to_song())
                .collect(),
            crate::local::LocalRowId::ScanError(_) => Vec::new(),
        }
    }

    fn reset_local_list_cursor(&mut self) {
        self.local_mode.ui.selected = 0;
        self.local_mode.ui.anchor = 0;
        self.local_mode.ui.pane = LocalPane::List;
        self.bridges.library_scroll.reset();
        self.dirty = true;
    }

    pub(crate) fn local_section_title(&self) -> String {
        let mut title = self.local_mode.ui.section.label().to_owned();
        for drill in &self.local_mode.ui.drill {
            let label = self.local_drill_label(drill);
            if !label.is_empty() {
                title.push_str(" / ");
                title.push_str(&label);
            }
        }
        title
    }

    fn local_drill_label(&self, drill: &LocalDrill) -> String {
        match drill {
            LocalDrill::Album(id) => self
                .local_album_by_id(id)
                .map(|album| album.title)
                .unwrap_or_default(),
            LocalDrill::Artist(id) => self
                .local_artist_by_id(id)
                .map(|artist| artist.name)
                .unwrap_or_default(),
            LocalDrill::Genre(genre) => genre.clone(),
            LocalDrill::Folder(folder) => folder
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .unwrap_or_else(|| folder.display().to_string()),
            LocalDrill::Smart(smart) => smart.label().to_owned(),
            LocalDrill::ImportSession(session_id) => session_id.clone(),
        }
    }

    pub(crate) fn local_row_text(&self, row: &crate::local::LocalRowId) -> String {
        match row {
            crate::local::LocalRowId::Track(id) => self
                .local_track_by_id(id)
                .map(|track| local_track_text(self, track))
                .unwrap_or_else(|| t!("Missing track", "없는 곡").to_owned()),
            crate::local::LocalRowId::DownloadSeed(index) => self
                .library_ui
                .downloaded
                .get(*index)
                .map(|song| local_song_text(self, song))
                .unwrap_or_else(|| t!("Missing track", "없는 곡").to_owned()),
            crate::local::LocalRowId::Album(id) => self
                .local_album_by_id(id)
                .map(|album| {
                    let duration = album.duration_ms.map(format_local_duration_ms);
                    let year = album.year.map(|year| year.to_string()).unwrap_or_default();
                    let suffix = match (year.is_empty(), duration) {
                        (false, Some(duration)) => format!("  {year} - {duration}"),
                        (false, None) => format!("  {year}"),
                        (true, Some(duration)) => format!("  {duration}"),
                        (true, None) => String::new(),
                    };
                    format!(
                        "{} - {}  ({} {}){}",
                        album.title,
                        album.album_artist,
                        album.track_count,
                        t!("tracks", "곡"),
                        suffix
                    )
                })
                .unwrap_or_else(|| t!("Missing album", "없는 앨범").to_owned()),
            crate::local::LocalRowId::Artist(id) => self
                .local_artist_by_id(id)
                .map(|artist| {
                    format!(
                        "{}  ({} {}, {} {})",
                        artist.name,
                        artist.album_ids.len(),
                        t!("albums", "앨범"),
                        artist.track_ids.len(),
                        t!("tracks", "곡")
                    )
                })
                .unwrap_or_else(|| t!("Missing artist", "없는 아티스트").to_owned()),
            crate::local::LocalRowId::Genre(genre) => {
                let count = self.local_tracks_for_genre(genre).len();
                format!("{genre}  ({count} {})", t!("tracks", "곡"))
            }
            crate::local::LocalRowId::Folder(folder) => {
                let count = self.local_tracks_for_folder(folder).len();
                format!("{}  ({count} {})", folder.display(), t!("tracks", "곡"))
            }
            crate::local::LocalRowId::Smart(smart) => {
                let count = self.local_tracks_for_smart(*smart).len();
                format!("{}  ({count} {})", smart.label(), t!("tracks", "곡"))
            }
            crate::local::LocalRowId::ImportSession(session_id) => local_import_session_text(
                session_id,
                self.local_tracks_for_import_session(session_id).len(),
            ),
            crate::local::LocalRowId::ScanError(index) => self
                .local_scan_issue(*index)
                .map(|error| format!("{} - {}", error.path.display(), error.message))
                .unwrap_or_else(|| t!("Missing scan error", "없는 스캔 오류").to_owned()),
        }
    }

    pub(crate) fn local_details_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(t!("Selected", "선택").to_owned());
        let selected = self
            .local_visible_rows()
            .get(self.local_mode.ui.selected)
            .cloned();
        if let Some(row) = selected {
            self.push_local_row_details(&mut lines, &row);
        } else {
            lines.push(t!("No local item selected.", "선택된 로컬 항목이 없습니다.").to_owned());
        }

        lines.push(String::new());
        self.push_local_queue_details(&mut lines);
        lines
    }

    pub(crate) fn local_details_summary(&self) -> String {
        let selected = self
            .local_visible_rows()
            .get(self.local_mode.ui.selected)
            .map(|row| self.local_row_text(row))
            .unwrap_or_else(|| t!("No selection", "선택 없음").to_owned());
        let Some(current) = self.queue.current() else {
            return format!("{}: {selected}", t!("Selected", "선택"));
        };
        format!(
            "{}: {selected}  |  {}: {}",
            t!("Selected", "선택"),
            t!("Now", "재생 중"),
            local_song_text(self, current)
        )
    }

    fn push_local_row_details(&self, lines: &mut Vec<String>, row: &crate::local::LocalRowId) {
        match row {
            crate::local::LocalRowId::Track(id) => {
                if let Some(track) = self.local_track_by_id(id) {
                    self.push_local_track_details(lines, track);
                }
            }
            crate::local::LocalRowId::DownloadSeed(index) => {
                if let Some(song) = self.library_ui.downloaded.get(*index) {
                    self.push_local_song_details(lines, song);
                }
            }
            crate::local::LocalRowId::Album(id) => {
                if let Some(album) = self.local_album_by_id(id) {
                    push_detail_line(lines, t!("Album", "앨범"), album.title);
                    push_detail_line(lines, t!("Artist", "아티스트"), album.album_artist);
                    if let Some(year) = album.year {
                        push_detail_line(lines, t!("Year", "연도"), year.to_string());
                    }
                    push_detail_line(
                        lines,
                        t!("Tracks", "곡"),
                        format!("{} {}", album.track_count, t!("tracks", "곡")),
                    );
                    if let Some(duration) = album.duration_ms {
                        push_detail_line(
                            lines,
                            t!("Duration", "길이"),
                            format_local_duration_ms(duration),
                        );
                    }
                    let cover_count = self
                        .local_tracks_for_album(id)
                        .into_iter()
                        .filter(|track| track.embedded_art_key.is_some())
                        .count();
                    push_detail_line(
                        lines,
                        t!("Cover", "커버"),
                        format_embedded_cover_count(cover_count),
                    );
                }
            }
            crate::local::LocalRowId::Artist(id) => {
                if let Some(artist) = self.local_artist_by_id(id) {
                    push_detail_line(lines, t!("Artist", "아티스트"), artist.name);
                    push_detail_line(
                        lines,
                        t!("Albums", "앨범"),
                        format!("{} {}", artist.album_ids.len(), t!("albums", "앨범")),
                    );
                    push_detail_line(
                        lines,
                        t!("Tracks", "곡"),
                        format!("{} {}", artist.track_ids.len(), t!("tracks", "곡")),
                    );
                }
            }
            crate::local::LocalRowId::Genre(genre) => {
                let tracks = self.local_tracks_for_genre(genre);
                push_detail_line(lines, t!("Genre", "장르"), genre.clone());
                push_detail_line(
                    lines,
                    t!("Tracks", "곡"),
                    format!("{} {}", tracks.len(), t!("tracks", "곡")),
                );
            }
            crate::local::LocalRowId::Folder(folder) => {
                let tracks = self.local_tracks_for_folder(folder);
                push_detail_line(lines, t!("Folder", "폴더"), folder.display().to_string());
                push_detail_line(
                    lines,
                    t!("Tracks", "곡"),
                    format!("{} {}", tracks.len(), t!("tracks", "곡")),
                );
            }
            crate::local::LocalRowId::Smart(smart) => {
                let tracks = self.local_tracks_for_smart(*smart);
                push_detail_line(lines, t!("Smart list", "스마트 목록"), smart.label());
                push_detail_line(
                    lines,
                    t!("Tracks", "곡"),
                    format!("{} {}", tracks.len(), t!("tracks", "곡")),
                );
            }
            crate::local::LocalRowId::ImportSession(session_id) => {
                let tracks = self.local_tracks_for_import_session(session_id);
                push_detail_line(
                    lines,
                    t!("Import session", "임포트 세션"),
                    session_id.clone(),
                );
                push_import_session_summary_details(lines, session_id);
                push_detail_line(
                    lines,
                    t!("Tracks", "곡"),
                    format!("{} {}", tracks.len(), t!("tracks", "곡")),
                );
                let first_order = tracks
                    .iter()
                    .filter_map(|track| track.import_source_order)
                    .min();
                let last_order = tracks
                    .iter()
                    .filter_map(|track| track.import_source_order)
                    .max();
                if let (Some(first), Some(last)) = (first_order, last_order) {
                    let value = if first == last {
                        format!("#{first}")
                    } else {
                        format!("#{first}-#{last}")
                    };
                    push_detail_line(lines, t!("Source order", "원본 순서"), value);
                }
            }
            crate::local::LocalRowId::ScanError(index) => {
                if let Some(error) = self.local_scan_issue(*index) {
                    push_detail_line(lines, t!("Path", "경로"), error.path.display().to_string());
                    push_detail_line(lines, t!("Error", "오류"), error.message.clone());
                }
            }
        }
    }

    fn push_local_track_details(&self, lines: &mut Vec<String>, track: &crate::local::LocalTrack) {
        push_detail_line(lines, t!("Title", "제목"), track.display_title());
        push_detail_line(lines, t!("Artist", "아티스트"), track.display_artist());
        if let Some(album) = format_album_year(track.album.as_deref(), track.year) {
            push_detail_line(lines, t!("Album", "앨범"), album);
        }
        if let Some(number) = format_disc_track(track.disc_no, track.track_no) {
            push_detail_line(lines, t!("Track", "트랙"), number);
        }
        if let Some(duration) = track.duration_ms {
            push_detail_line(
                lines,
                t!("Duration", "길이"),
                format_local_duration_ms(duration),
            );
        }
        if let Some(format) = &track.format {
            push_detail_line(lines, t!("Format", "포맷"), format_audio_format(format));
        }
        if let Some(sample_rate) = track.sample_rate {
            push_detail_line(
                lines,
                t!("Sample rate", "샘플레이트"),
                format_sample_rate(sample_rate),
            );
        }
        if let Some(bitrate) = track.bitrate {
            push_detail_line(lines, t!("Bitrate", "비트레이트"), format_bitrate(bitrate));
        }
        push_detail_line(
            lines,
            t!("Cover", "커버"),
            if track.embedded_art_key.is_some() {
                t!("embedded cover", "내장 커버")
            } else {
                t!("no embedded cover", "내장 커버 없음")
            },
        );
        if let Some(name) = track.path.file_name().and_then(|name| name.to_str()) {
            push_detail_line(lines, t!("File", "파일"), name);
        }
        if let Some(session_id) = track.import_session_id.as_deref() {
            push_detail_line(lines, t!("Import session", "임포트 세션"), session_id);
        }
        if let Some(order) = track.import_source_order {
            push_detail_line(lines, t!("Source order", "원본 순서"), format!("#{order}"));
        }
        push_detail_line(
            lines,
            t!("Path", "경로"),
            self.local_path_for_display(&track.path),
        );
    }

    fn push_local_song_details(&self, lines: &mut Vec<String>, song: &Song) {
        push_detail_line(lines, t!("Title", "제목"), self.display_title(song));
        push_detail_line(lines, t!("Artist", "아티스트"), self.display_artist(song));
        if let Some(album) = song
            .album
            .as_deref()
            .map(str::trim)
            .filter(|album| !album.is_empty())
        {
            push_detail_line(lines, t!("Album", "앨범"), album);
        }
        if !song.duration.trim().is_empty() {
            push_detail_line(lines, t!("Duration", "길이"), song.duration.as_str());
        }
        if let Some(path) = &song.local_path {
            push_detail_line(
                lines,
                t!("Cover", "커버"),
                t!("local file artwork source", "로컬 파일 아트워크 소스"),
            );
            if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
                push_detail_line(lines, t!("File", "파일"), name);
            }
            push_detail_line(lines, t!("Path", "경로"), self.local_path_for_display(path));
        }
    }

    fn push_local_queue_details(&self, lines: &mut Vec<String>) {
        lines.push(t!("Now playing", "재생 중").to_owned());
        if let Some(current) = self.queue.current() {
            let (position, total) = self.queue.position();
            lines.push(local_song_text(self, current));
            push_detail_line(lines, t!("Queue", "큐"), format!("{position}/{total}"));
        } else {
            lines.push(t!("Queue is empty.", "큐가 비어 있습니다.").to_owned());
        }

        lines.push(String::new());
        lines.push(t!("Up next", "다음 곡").to_owned());
        let upcoming = self.queue.upcoming(3);
        if upcoming.is_empty() {
            lines.push(t!("End of queue.", "큐의 끝입니다.").to_owned());
        } else {
            for (index, song) in upcoming.into_iter().enumerate() {
                lines.push(format!("{}. {}", index + 1, local_song_text(self, song)));
            }
        }
    }

    fn local_path_for_display(&self, path: &Path) -> String {
        for root in self.local_detail_roots() {
            if let Ok(relative) = path.strip_prefix(&root)
                && !relative.as_os_str().is_empty()
            {
                return relative.display().to_string();
            }
        }
        path.display().to_string()
    }

    fn local_detail_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if self.config.local.include_download_dir() {
            roots.push(self.config.effective_download_dir());
        }
        for root in &self.config.local.roots {
            if root.enabled()
                && let Some(path) = root.normalized_path()
            {
                roots.push(path);
            }
        }
        roots
    }

    fn local_track_by_id(
        &self,
        id: &crate::local::LocalTrackId,
    ) -> Option<&crate::local::LocalTrack> {
        self.local_mode
            .index
            .index
            .tracks()
            .iter()
            .find(|track| &track.id == id)
    }

    fn local_album_by_id(
        &self,
        id: &crate::local::LocalAlbumId,
    ) -> Option<crate::local::LocalAlbum> {
        self.local_mode
            .index
            .index
            .albums()
            .into_iter()
            .find(|album| &album.id == id)
    }

    fn local_artist_by_id(
        &self,
        id: &crate::local::LocalArtistId,
    ) -> Option<crate::local::LocalArtist> {
        self.local_mode
            .index
            .index
            .artists()
            .into_iter()
            .find(|artist| &artist.id == id)
    }

    fn local_tracks_for_album(
        &self,
        album_id: &crate::local::LocalAlbumId,
    ) -> Vec<&crate::local::LocalTrack> {
        let Some(album) = self.local_album_by_id(album_id) else {
            return Vec::new();
        };
        let track_ids: BTreeSet<_> = album.track_ids.into_iter().collect();
        let mut tracks: Vec<_> = self
            .local_mode
            .index
            .index
            .tracks()
            .iter()
            .filter(|track| track_ids.contains(&track.id))
            .collect();
        sort_local_tracks(&mut tracks);
        tracks
    }

    fn local_albums_for_artist(
        &self,
        artist_id: &crate::local::LocalArtistId,
    ) -> Vec<crate::local::LocalAlbum> {
        let Some(artist) = self.local_artist_by_id(artist_id) else {
            return Vec::new();
        };
        let album_ids: BTreeSet<_> = artist.album_ids.into_iter().collect();
        self.local_mode
            .index
            .index
            .albums()
            .into_iter()
            .filter(|album| album_ids.contains(&album.id))
            .collect()
    }

    fn local_tracks_for_artist(
        &self,
        artist_id: &crate::local::LocalArtistId,
    ) -> Vec<&crate::local::LocalTrack> {
        let Some(artist) = self.local_artist_by_id(artist_id) else {
            return Vec::new();
        };
        let track_ids: BTreeSet<_> = artist.track_ids.into_iter().collect();
        let mut tracks: Vec<_> = self
            .local_mode
            .index
            .index
            .tracks()
            .iter()
            .filter(|track| track_ids.contains(&track.id))
            .collect();
        sort_local_tracks(&mut tracks);
        tracks
    }

    fn local_tracks_for_genre(&self, genre: &str) -> Vec<&crate::local::LocalTrack> {
        let key = crate::local::model::normalize_key(genre);
        let mut tracks: Vec<_> = self
            .local_mode
            .index
            .index
            .tracks()
            .iter()
            .filter(|track| {
                track
                    .genre
                    .iter()
                    .any(|g| crate::local::model::normalize_key(g) == key)
            })
            .collect();
        sort_local_tracks(&mut tracks);
        tracks
    }

    fn local_tracks_for_folder(&self, folder: &Path) -> Vec<&crate::local::LocalTrack> {
        let mut tracks: Vec<_> = self
            .local_mode
            .index
            .index
            .tracks()
            .iter()
            .filter(|track| track.path.parent() == Some(folder))
            .collect();
        sort_local_tracks(&mut tracks);
        tracks
    }

    fn local_tracks_for_smart(
        &self,
        smart: crate::local::LocalSmartList,
    ) -> Vec<&crate::local::LocalTrack> {
        let download_dir = self.config.effective_download_dir();
        let mut tracks: Vec<_> = self
            .local_mode
            .index
            .index
            .tracks()
            .iter()
            .filter(|track| match smart {
                crate::local::LocalSmartList::RecentlyAdded => true,
                crate::local::LocalSmartList::DownloadedFromYoutubeMusic => {
                    track.linked_video_id.is_some() || track.path.starts_with(&download_dir)
                }
                crate::local::LocalSmartList::LocalOnly => track.linked_video_id.is_none(),
                crate::local::LocalSmartList::MissingArtist => track.artist.is_empty(),
                crate::local::LocalSmartList::MissingAlbum => track
                    .album
                    .as_deref()
                    .map(str::trim)
                    .filter(|album| !album.is_empty())
                    .is_none(),
                crate::local::LocalSmartList::NoEmbeddedCover => track.embedded_art_key.is_none(),
                crate::local::LocalSmartList::LargeFiles => track.file_size >= 50 * 1024 * 1024,
                crate::local::LocalSmartList::Lossless => matches!(
                    track.format,
                    Some(crate::local::AudioFormat::Flac | crate::local::AudioFormat::Wav)
                ),
            })
            .collect();
        if smart == crate::local::LocalSmartList::RecentlyAdded {
            tracks.sort_by(|a, b| {
                b.modified_at
                    .cmp(&a.modified_at)
                    .then_with(|| a.path.cmp(&b.path))
            });
        } else {
            sort_local_tracks(&mut tracks);
        }
        tracks
    }

    fn local_tracks_for_import_session(&self, session_id: &str) -> Vec<&crate::local::LocalTrack> {
        let mut tracks: Vec<_> = self
            .local_mode
            .index
            .index
            .tracks()
            .iter()
            .filter(|track| track.import_session_id.as_deref() == Some(session_id))
            .collect();
        tracks.sort_by(|a, b| {
            a.import_source_order
                .unwrap_or(u32::MAX)
                .cmp(&b.import_source_order.unwrap_or(u32::MAX))
                .then_with(|| a.path.cmp(&b.path))
        });
        tracks
    }

    fn clear_local_filter(&mut self) {
        self.local_mode.ui.filter_query.clear();
        self.local_mode.ui.filter_editing = false;
        self.local_mode.ui.selected = 0;
        self.local_mode.ui.anchor = 0;
        self.bridges.library_scroll.reset();
    }

    fn after_local_filter_change(&mut self) {
        self.local_mode.ui.selected = 0;
        self.local_mode.ui.anchor = 0;
        self.bridges.library_scroll.reset();
        self.dirty = true;
    }

    fn on_key_local_filter(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match k.code {
            KeyCode::Esc => {
                self.clear_local_filter();
                self.dirty = true;
            }
            KeyCode::Enter => {
                self.local_mode.ui.filter_editing = false;
                self.dirty = true;
            }
            KeyCode::Backspace => {
                self.local_mode.ui.filter_query.pop();
                self.after_local_filter_change();
            }
            KeyCode::Up => {
                self.local_mode.ui.selected = self.local_mode.ui.selected.saturating_sub(1);
                self.local_mode.ui.anchor = self.local_mode.ui.selected;
                self.dirty = true;
            }
            KeyCode::Down => {
                let len = self.local_rows_len();
                if self.local_mode.ui.selected + 1 < len {
                    self.local_mode.ui.selected += 1;
                }
                self.local_mode.ui.anchor = self.local_mode.ui.selected;
                self.dirty = true;
            }
            KeyCode::Char(c)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match try_push_query_char(
                    &mut self.local_mode.ui.filter_query,
                    c,
                    MAX_FILTER_QUERY_BYTES,
                ) {
                    Ok(()) => self.after_local_filter_change(),
                    Err(reason) => self.set_query_reject_status(reason),
                }
            }
            _ => {}
        }
        Vec::new()
    }
}
