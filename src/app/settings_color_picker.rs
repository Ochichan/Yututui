//! Settings-owned color-picker reducer. Keeping modal behavior here avoids growing the already
//! size-tight Settings view and reducer while giving every input path one capture point.

use super::*;
use crate::settings::ColorPickerState;

impl App {
    pub(in crate::app) fn settings_color_picker_is_open(&self) -> bool {
        self.mode == Mode::Settings
            && self
                .settings
                .as_ref()
                .is_some_and(|state| state.color_picker.is_some())
    }

    pub(in crate::app) fn settings_open_color_picker(&mut self) {
        let Some(Field::ThemeColor(role)) = self.settings.as_ref().and_then(|s| s.current_field())
        else {
            return;
        };
        let Some(state) = self.settings.as_mut() else {
            return;
        };
        let current = state.draft.theme.effective_hex(role);
        state.editing_text = false;
        state.spotify_import_mode_dropdown = None;
        state.color_picker = Some(ColorPickerState::new(role, current));
        self.dirty = true;
    }

    pub(in crate::app) fn settings_color_swatch_click(&mut self, row: usize) -> Vec<Cmd> {
        if self.mode != Mode::Settings || self.settings_close_spotify_import_mode_dropdown() {
            return Vec::new();
        }
        self.settings_focus_row(row);
        self.settings_open_color_picker();
        Vec::new()
    }

    pub(in crate::app) fn settings_cancel_color_picker(&mut self) {
        if let Some(state) = self.settings.as_mut()
            && state.color_picker.take().is_some()
        {
            self.dirty = true;
        }
    }

    pub(in crate::app) fn settings_color_picker_key(&mut self, key: KeyEvent) -> Vec<Cmd> {
        match key.code {
            KeyCode::Esc => self.settings_cancel_color_picker(),
            KeyCode::Enter => return self.settings_commit_color_picker(),
            KeyCode::Left => self.settings_move_color_picker(|picker| picker.move_left()),
            KeyCode::Right => self.settings_move_color_picker(|picker| picker.move_right()),
            KeyCode::Up => self.settings_move_color_picker(|picker| picker.move_up()),
            KeyCode::Down => self.settings_move_color_picker(|picker| picker.move_down()),
            _ => {}
        }
        Vec::new()
    }

    pub(in crate::app) fn settings_color_picker_scroll(&mut self, up: bool, rows: usize) {
        self.settings_move_color_picker(|picker| picker.wheel(up, rows));
    }

    fn settings_move_color_picker(&mut self, move_picker: impl FnOnce(&mut ColorPickerState)) {
        let Some(picker) = self
            .settings
            .as_mut()
            .and_then(|state| state.color_picker.as_mut())
        else {
            return;
        };
        move_picker(picker);
        self.dirty = true;
    }

    pub(in crate::app) fn settings_color_picker_mouse_click(
        &mut self,
        col: u16,
        row: u16,
    ) -> Vec<Cmd> {
        // Retain ownership of the complete click gesture even when this press closes the modal;
        // a translator-emitted double-click must not hit the Settings form underneath.
        self.interaction.color_picker_click = Some((col, row));
        match self.mouse_target_at(col, row) {
            Some(MouseTarget::SettingsColorPickerCurrent) => {
                self.settings_move_color_picker(ColorPickerState::select_current);
                self.settings_commit_color_picker()
            }
            Some(MouseTarget::SettingsColorPickerChoice(index)) => {
                self.settings_move_color_picker(|picker| picker.select_choice(index));
                self.settings_commit_color_picker()
            }
            Some(MouseTarget::SettingsColorPickerSurface) => Vec::new(),
            _ => {
                self.settings_cancel_color_picker();
                Vec::new()
            }
        }
    }

    fn settings_commit_color_picker(&mut self) -> Vec<Cmd> {
        let Some((role, value)) = self.settings.as_ref().and_then(|state| {
            state
                .color_picker
                .as_ref()
                .map(|picker| (picker.role, picker.selected_value()))
        }) else {
            return Vec::new();
        };
        let Some(state) = self.settings.as_mut() else {
            return Vec::new();
        };
        match state.draft.theme.set_override(role, &value) {
            Ok(()) => {
                state.color_picker = None;
                self.theme = state.draft.theme.normalized();
                let hex = state.draft.theme.effective_hex(role);
                self.status.kind = StatusKind::Info;
                self.status.text = if crate::i18n::is_korean() {
                    format!("{} 을(를) {hex} 로 설정함", role.label())
                } else {
                    format!("Set {} to {hex}", role.label())
                };
            }
            Err(message) => {
                self.status.kind = StatusKind::Error;
                self.status.text = message;
            }
        }
        self.dirty = true;
        Vec::new()
    }
}
