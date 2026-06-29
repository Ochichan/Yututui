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

    pub(in crate::app) fn on_key_library(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let len = self.library_len();
        match self.keymap.action(KeyContext::Library, k.into()) {
            Some(Action::Back) => {
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
                self.library_ui.selected = 0;
                self.library_ui.anchor = 0;
                self.bridges.library_scroll.reset();
                self.dirty = true;
                Vec::new()
            }
            Some(Action::FocusPrev) => {
                self.library_ui.tab = self.library_ui.tab.prev();
                self.library_ui.selected = 0;
                self.library_ui.anchor = 0;
                self.bridges.library_scroll.reset();
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
