//! Settings navigation: the always-honored keys, moving the selection per tab or Sync area,
//! scrolling the view past the selection, the mouse wheel, and bringing a scrolled-off cursor
//! back before an action key acts on it.

use super::super::*;

impl App {
    /// Literal navigation keys the settings editor always accepts, so a user can never
    /// remap themselves out of the screen that edits keybindings.
    pub(in crate::app) fn settings_safety_action(k: KeyEvent) -> Option<Action> {
        match k.code {
            KeyCode::Up => Some(Action::MoveUp),
            KeyCode::Down => Some(Action::MoveDown),
            KeyCode::Left => Some(Action::ChangeDecrease),
            KeyCode::Right => Some(Action::ChangeIncrease),
            KeyCode::Enter => Some(Action::Confirm),
            KeyCode::Esc => Some(Action::Back),
            KeyCode::Backspace => Some(Action::DeleteChar),
            _ => None,
        }
    }

    pub(in crate::app) fn settings_move_row(&mut self, delta: i32) {
        if let Some(st) = self.settings.as_mut() {
            if st.tab == SettingsTab::Sync {
                self.personal_state.sync_ui.move_row(delta);
                self.dirty = true;
                return;
            }
            // The Keys tab is a list of remappable bindings rather than `Field`s.
            let n = match st.tab {
                SettingsTab::Keys => {
                    (crate::keymap::editable_entries().len()
                        + crate::mousemap::MouseContext::ALL.len()
                            * crate::mousemap::MouseGesture::ALL.len()) as i32
                }
                _ => st.fields().len() as i32,
            };
            st.row = (st.row as i32 + delta).clamp(0, n.max(1) - 1) as usize;
            st.editing_text = false;
            st.spotify_import_mode_dropdown = None;
            self.dirty = true;
        }
    }

    /// Move the selection one step within whatever the Settings screen is showing: the field
    /// list, the Keys list, or the active Sync area's actions. Sync Status has nothing to select.
    pub(in crate::app) fn settings_step_selection(&mut self, delta: i32) {
        let on_sync = self
            .settings
            .as_ref()
            .is_some_and(|st| st.tab == SettingsTab::Sync);
        if !on_sync {
            self.settings_move_row(delta);
            return;
        }
        match self.server.settings.area {
            crate::app::SyncArea::MusicServer => {
                let last = self.server.settings.row_count().saturating_sub(1);
                let selected = self.server.settings.selected as i32 + delta;
                self.server.settings.selected = selected.clamp(0, last as i32) as usize;
                self.dirty = true;
            }
            crate::app::SyncArea::PersonalState | crate::app::SyncArea::DevicesRecovery => {
                self.settings_move_row(delta);
            }
            crate::app::SyncArea::Status => {}
        }
    }

    /// Scroll the Settings view without moving the selection, for reading detail rows and Sync
    /// text that run past the screen. PageUp/PageDown move a page (keeping one row of overlap),
    /// Home/End jump to either end; Up/Down move one row where nothing is selectable. Returns
    /// whether `action` was one of these.
    pub(in crate::app) fn settings_scroll_view(&mut self, action: Action) -> bool {
        let scroll = &self.bridges.settings_scroll;
        let Some(len) = self.bridges.settings_list_len.get() else {
            return false;
        };
        let page = scroll.viewport().saturating_sub(1).max(1);
        match action {
            Action::MoveUp => scroll.wheel(true, 1, len),
            Action::MoveDown => scroll.wheel(false, 1, len),
            Action::PageUp => scroll.wheel(true, page, len),
            Action::PageDown => scroll.wheel(false, page, len),
            Action::JumpTop => scroll.set_offset(0, len),
            Action::JumpBottom => scroll.set_offset(len, len),
            _ => return false,
        }
        self.dirty = true;
        true
    }

    /// One wheel notch over Settings. The wheel walks the selection as before, except that it
    /// first scrolls through anything the selection alone cannot bring on screen: the rest of
    /// the focused row's detail below the window, a focused row scrolled off the top, and the
    /// text above the first selectable row. A pane with nothing to select just scrolls.
    pub(in crate::app) fn settings_wheel(&mut self, up: bool, notches: usize) {
        let on_keys = self
            .settings
            .as_ref()
            .is_some_and(|st| st.tab == SettingsTab::Keys);
        // A wheel event carries several rows; on a pane only a few rows tall that jump would
        // skip text, so the view never scrolls more than a page (less one row) per event.
        let max_scroll = self
            .bridges
            .settings_scroll
            .viewport()
            .saturating_sub(1)
            .max(1);
        let mut scrolled = 0;
        for _ in 0..notches {
            let len = self.bridges.settings_list_len.get();
            let focus = self.bridges.settings_focus.get();
            let (Some(len), false) = (len, on_keys) else {
                self.settings_move_row(if up { -1 } else { 1 });
                continue;
            };
            let scroll = &self.bridges.settings_scroll;
            let offset = scroll.offset();
            let bottom = offset + scroll.viewport();
            let scroll_first = match focus {
                None => true,
                Some((first, focused, _)) if up => {
                    focused < offset || (focused <= first && offset > 0)
                }
                Some((_, _, block_end)) => block_end >= bottom && bottom < len,
            };
            if scroll_first {
                if scrolled == max_scroll {
                    break;
                }
                scroll.wheel(up, 1, len);
                scrolled += 1;
                self.dirty = true;
            } else {
                self.settings_step_selection(if up { -1 } else { 1 });
            }
        }
    }

    /// When a scroll left the selected row off screen, an action key only brings it back into
    /// view, so ←/→/Enter never act on a row the user cannot see. Returns whether it did.
    pub(in crate::app) fn settings_reveal_hidden_cursor(&mut self) -> bool {
        let (Some(len), Some((_, focused, _))) = (
            self.bridges.settings_list_len.get(),
            self.bridges.settings_focus.get(),
        ) else {
            return false;
        };
        let scroll = &self.bridges.settings_scroll;
        let offset = scroll.offset();
        if scroll.viewport() == 0 || (offset..offset + scroll.viewport()).contains(&focused) {
            return false;
        }
        scroll.set_offset(focused, len);
        self.dirty = true;
        true
    }
}
