//! Library-input reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// Move the library cursor up/down by `lines`, clamped, collapsing the multi-select
    /// range onto the cursor (like keyboard nav).
    pub(in crate::app) fn move_library_cursor(&mut self, up: bool, lines: usize) {
        let len = self.library_len();
        if len == 0 {
            return;
        }
        self.library_ui.selected = if up {
            self.library_ui.selected.saturating_sub(lines)
        } else {
            (self.library_ui.selected + lines).min(len - 1)
        };
        self.library_ui.anchor = self.library_ui.selected;
        self.dirty = true;
    }

    /// Stop capturing filter input, drop the query, and snap the cursor/scroll back to the top
    /// of the now-unfiltered list. Also used when switching tabs.
    pub(in crate::app) fn clear_library_filter(&mut self) {
        self.library_ui.filter_query.clear();
        self.library_ui.filter_editing = false;
        self.library_ui.selected = 0;
        self.library_ui.anchor = 0;
        self.bridges.library_scroll.reset();
    }

    /// After the filter text changes, jump the cursor to the first match and reset the scroll so
    /// the filtered list reads from the top (fzf-style).
    fn after_library_filter_change(&mut self) {
        self.library_ui.selected = 0;
        self.library_ui.anchor = 0;
        self.bridges.library_scroll.reset();
        self.dirty = true;
    }

    /// Keystrokes while the filter input is focused: type to narrow, Backspace to edit, Enter
    /// to commit (keep the filter, return to list nav), Esc to cancel (clear the filter), and
    /// the arrows to move the cursor within the filtered rows.
    fn on_key_library_filter(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match k.code {
            KeyCode::Esc => {
                self.clear_library_filter();
                self.dirty = true;
            }
            KeyCode::Enter => {
                self.library_ui.filter_editing = false;
                self.dirty = true;
            }
            KeyCode::Backspace => {
                self.library_ui.filter_query.pop();
                self.after_library_filter_change();
            }
            KeyCode::Up => {
                self.library_ui.selected = self.library_ui.selected.saturating_sub(1);
                self.library_ui.anchor = self.library_ui.selected;
                self.dirty = true;
            }
            KeyCode::Down => {
                let len = self.library_len();
                if self.library_ui.selected + 1 < len {
                    self.library_ui.selected += 1;
                }
                self.library_ui.anchor = self.library_ui.selected;
                self.dirty = true;
            }
            // Plain typed characters extend the query; ignore Ctrl/Alt combos (e.g. Ctrl+R for
            // radio is already handled as a global before we get here).
            KeyCode::Char(c)
                if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.library_ui.filter_query.push(c);
                self.after_library_filter_change();
            }
            _ => {}
        }
        Vec::new()
    }

    pub(in crate::app) fn on_key_library(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // While the filter box is capturing, typed characters edit the query (the list narrows
        // live) and the arrows still move within the filtered rows — see `on_key_library_filter`.
        if self.library_ui.filter_editing {
            return self.on_key_library_filter(k);
        }
        // Esc isn't a Library keybinding, but with a filter applied it should clear it — the
        // natural "get me out of this filtered view" gesture, matching the box's Esc-to-cancel.
        if k.code == KeyCode::Esc && !self.library_ui.filter_query.is_empty() {
            self.clear_library_filter();
            self.dirty = true;
            return Vec::new();
        }
        let len = self.library_len();
        match self.keymap.action(KeyContext::Library, k.into()) {
            Some(Action::LibraryFilter) => {
                // Open the filter input (re-opens with the current query if one is applied).
                self.library_ui.filter_editing = true;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::Back) => {
                // `q` always closes the Library in one press. Esc is the lighter gesture that
                // just clears an applied filter (handled above); the filter is reset on the
                // next Library open regardless, so leaving with one applied is harmless.
                self.mode = Mode::Player;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::Quit) => {
                self.should_quit = true;
                Vec::new()
            }
            Some(Action::FocusNext) => {
                self.library_ui.tab = self.library_ui.tab.next();
                self.clear_library_filter();
                self.dirty = true;
                Vec::new()
            }
            Some(Action::FocusPrev) => {
                self.library_ui.tab = self.library_ui.tab.prev();
                self.clear_library_filter();
                self.dirty = true;
                Vec::new()
            }
            Some(Action::MoveUp) => {
                self.library_ui.selected = self.library_ui.selected.saturating_sub(1);
                // Keyboard nav collapses the range to the cursor (like the queue window).
                self.library_ui.anchor = self.library_ui.selected;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::MoveDown) => {
                if self.library_ui.selected + 1 < len {
                    self.library_ui.selected += 1;
                }
                self.library_ui.anchor = self.library_ui.selected;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::PageUp) => {
                self.move_library_cursor(true, self.page_step());
                Vec::new()
            }
            Some(Action::PageDown) => {
                self.move_library_cursor(false, self.page_step());
                Vec::new()
            }
            Some(Action::JumpTop) => {
                self.library_ui.selected = 0;
                self.library_ui.anchor = 0;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::JumpBottom) => {
                self.library_ui.selected = len.saturating_sub(1);
                self.library_ui.anchor = self.library_ui.selected;
                self.dirty = true;
                Vec::new()
            }
            // Delete the selected range (mouse-drag or single row), per-tab semantics.
            Some(Action::LibraryRemove) => self.library_delete_selection(),
            Some(Action::Confirm) => self.play_from_library(),
            Some(Action::OpenAi) => {
                self.enter_ai();
                Vec::new()
            }
            Some(Action::Download) => match self.selected_library_song() {
                Some(song) => self.start_download(song),
                None => Vec::new(),
            },
            // Un/favorite the highlighted track (removing shifts selection up).
            Some(Action::Favorite) => {
                if let Some(song) = self.selected_library_song() {
                    self.library.toggle_favorite(&song);
                    let new_len = self.library_len();
                    if self.library_ui.selected >= new_len {
                        self.library_ui.selected = new_len.saturating_sub(1);
                    }
                    self.dirty = true;
                    return vec![Cmd::SaveLibrary];
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}
