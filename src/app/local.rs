//! Local Deck reducer helpers.

use super::*;

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
        self.local_dedicated_mode = true;
        self.mode = Mode::Library;
        self.local_mode.ui.section = if self.library_ui.downloaded.is_empty() {
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
        self.status.kind = StatusKind::Info;
        self.status.text = t!("Local Player mode enabled", "로컬 플레이어 모드 켜짐").to_owned();
        self.dirty = true;
        Vec::new()
    }

    fn exit_local_dedicated_mode(&mut self) -> Vec<Cmd> {
        if !self.local_dedicated_mode {
            return Vec::new();
        }
        self.local_dedicated_mode = false;
        self.local_mode.pending_confirm = None;
        self.bridges.library_scroll.reset();
        self.status.kind = StatusKind::Info;
        self.status.text = t!("Local Player mode disabled", "로컬 플레이어 모드 꺼짐").to_owned();
        self.dirty = true;
        Vec::new()
    }

    pub fn local_rows_len(&self) -> usize {
        match self.local_mode.ui.section {
            LocalSection::Home => 0,
            LocalSection::Tracks => self.library_ui.downloaded.len(),
        }
    }

    pub(in crate::app) fn on_key_local(&mut self, k: KeyEvent) -> Vec<Cmd> {
        if k.code == KeyCode::Esc {
            return self.request_local_mode_switch();
        }

        match self.keymap.action(KeyContext::Library, k.into()) {
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
        let Some(song) = self
            .library_ui
            .downloaded
            .get(self.local_mode.ui.selected)
            .cloned()
        else {
            return Vec::new();
        };
        self.play_now(song)
    }
}
