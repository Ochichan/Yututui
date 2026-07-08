//! Local Deck reducer helpers.

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
        match self.local_mode.ui.section {
            LocalSection::Home => 0,
            LocalSection::Tracks => self.local_track_rows_len(),
        }
    }

    pub(in crate::app) fn on_key_local(&mut self, k: KeyEvent) -> Vec<Cmd> {
        if self.local_mode.ui.filter_editing {
            return self.on_key_local_filter(k);
        }
        if k.code == KeyCode::Esc && !self.local_mode.ui.filter_query.is_empty() {
            self.clear_local_filter();
            self.dirty = true;
            return Vec::new();
        }
        if k.code == KeyCode::Esc {
            return self.request_local_mode_switch();
        }
        if k.modifiers.is_empty() && matches!(k.code, KeyCode::Char('r')) {
            return self.request_local_scan(false);
        }
        if matches!(k.code, KeyCode::Char('R'))
            && (k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT)
        {
            return self.request_local_scan(true);
        }

        match self.keymap.action(KeyContext::Library, k.into()) {
            Some(Action::LibraryFilter) => {
                self.local_mode.ui.filter_editing = true;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::Back) => self.request_local_mode_switch(),
            Some(Action::MoveUp) => {
                let step = self.nav_repeat_step(Action::MoveUp);
                self.move_local_cursor(true, step);
                Vec::new()
            }
            Some(Action::MoveDown) => {
                let step = self.nav_repeat_step(Action::MoveDown);
                self.move_local_cursor(false, step);
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
            self.local_mode.ui.section = LocalSection::Tracks;
            self.local_mode.ui.selected = self
                .local_mode
                .ui
                .selected
                .min(self.local_rows_len().saturating_sub(1));
            self.local_mode.ui.anchor = self.local_mode.ui.selected;
            self.dirty = true;
            return Vec::new();
        }
        let Some(song) = self.local_song_at_display_index(self.local_mode.ui.selected) else {
            return Vec::new();
        };
        self.play_now(song)
    }

    pub(in crate::app) fn apply_local_msg(&mut self, msg: LocalMsg) -> Vec<Cmd> {
        match msg {
            LocalMsg::IndexLoaded { index_path, index } => {
                self.local_mode.index.index_path = index_path;
                self.local_mode.index.index = index;
                self.local_mode.index.loaded = true;
                self.local_mode.index.loading = false;
                self.local_mode.index.scanning = false;
                self.local_mode.index.errors.clear();
                self.clamp_local_after_index_change();
                self.dirty = true;
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
            LocalMsg::ScanFailed { error } => {
                self.local_mode.index.loading = false;
                self.local_mode.index.scanning = false;
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

    fn request_local_scan(&mut self, full: bool) -> Vec<Cmd> {
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

    fn local_scan_roots(&self) -> Vec<crate::local::LocalScanRoot> {
        vec![crate::local::LocalScanRoot::download(
            self.config.effective_download_dir(),
        )]
    }

    fn local_track_rows_len(&self) -> usize {
        if !self.local_mode.index.index.is_empty() {
            self.local_visible_index_tracks().len()
        } else {
            self.local_visible_download_seed_rows().len()
        }
    }

    fn clamp_local_after_index_change(&mut self) {
        if !self.local_mode.index.index.is_empty() {
            self.local_mode.ui.section = LocalSection::Tracks;
        }
        let len = self.local_rows_len();
        self.local_mode.ui.selected = self.local_mode.ui.selected.min(len.saturating_sub(1));
        self.local_mode.ui.anchor = self.local_mode.ui.selected;
        self.bridges.library_scroll.reset();
    }

    pub(crate) fn local_visible_index_tracks(&self) -> Vec<&crate::local::LocalTrack> {
        let query = self.local_mode.ui.filter_query.as_str();
        self.local_mode
            .index
            .index
            .tracks()
            .iter()
            .filter(|track| crate::local::query::track_matches_filter(track, query))
            .collect()
    }

    pub(crate) fn local_visible_download_seed_rows(&self) -> Vec<&Song> {
        let query = self.local_mode.ui.filter_query.as_str();
        self.library_ui
            .downloaded
            .iter()
            .filter(|song| {
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
            .collect()
    }

    fn local_song_at_display_index(&self, index: usize) -> Option<Song> {
        if !self.local_mode.index.index.is_empty() {
            self.local_visible_index_tracks()
                .get(index)
                .map(|track| track.to_song())
        } else {
            self.local_visible_download_seed_rows()
                .get(index)
                .cloned()
                .cloned()
        }
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
