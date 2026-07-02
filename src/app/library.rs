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
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.library_ui.filter_query.push(c);
                self.after_library_filter_change();
            }
            _ => {}
        }
        Vec::new()
    }

    pub(in crate::app) fn on_key_library(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // The create-playlist popup captures every key while open — checked before the
        // filter so its Esc closes the popup, not the filter.
        if self.library_ui.create_input.is_some() {
            return self.on_key_playlist_create(k);
        }
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
        let playlists_tab = self.effective_library_tab() == LibraryTab::Playlists;
        // With no filter to clear, Esc backs out of an opened playlist — the second stage
        // of the same "get me out" gesture.
        if k.code == KeyCode::Esc && playlists_tab && self.library_ui.open_playlist.is_some() {
            self.close_open_playlist();
            return Vec::new();
        }
        let len = self.library_len();
        // The Playlists tab resolves against its own context so its bindings (and the
        // cheat-sheet group) can differ from the song tabs; Common still supplies shared nav.
        let ctx = if playlists_tab {
            KeyContext::Playlists
        } else {
            KeyContext::Library
        };
        match self.keymap.action(ctx, k.into()) {
            Some(Action::LibraryFilter) => {
                // Open the filter input (re-opens with the current query if one is applied).
                self.library_ui.filter_editing = true;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::Back) => {
                // Inside an opened playlist, Back first returns to the playlist list; from
                // the root (and every other tab) `q` closes the Library in one press. Esc is
                // the lighter gesture that just clears an applied filter (handled above);
                // the filter is reset on the next Library open regardless, so leaving with
                // one applied is harmless.
                if playlists_tab && self.library_ui.open_playlist.is_some() {
                    self.close_open_playlist();
                } else {
                    self.mode = Mode::Player;
                    self.dirty = true;
                }
                Vec::new()
            }
            Some(Action::Quit) => {
                self.should_quit = true;
                Vec::new()
            }
            Some(Action::FocusNext) => {
                self.library_ui.tab = self.next_library_tab(self.library_ui.tab, true);
                self.reset_playlist_ui_state();
                self.clear_library_filter();
                if self.library_ui.tab == LibraryTab::Playlists {
                    self.hint_playlist_create();
                }
                self.dirty = true;
                Vec::new()
            }
            Some(Action::FocusPrev) => {
                self.library_ui.tab = self.next_library_tab(self.library_ui.tab, false);
                self.reset_playlist_ui_state();
                self.clear_library_filter();
                if self.library_ui.tab == LibraryTab::Playlists {
                    self.hint_playlist_create();
                }
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
            // At the Playlists root this asks before deleting the playlist under the cursor.
            Some(Action::LibraryRemove) => self.library_delete_selection(),
            // Enter opens the playlist under the cursor at the Playlists root; on song rows
            // it plays the highlighted track right now, keeping the existing queue intact.
            Some(Action::Confirm) => {
                if self.playlists_root() {
                    self.open_selected_playlist()
                } else {
                    self.play_now_many(self.selected_library_songs())
                }
            }
            // `\` enqueues without interrupting playback: the whole playlist under the
            // cursor at the Playlists root, the highlighted track(s) on song rows.
            Some(Action::Enqueue) => {
                if self.playlists_root() {
                    self.enqueue_selected_playlist()
                } else {
                    self.enqueue_many(self.selected_library_songs())
                }
            }
            // `a` plays the playlist under the cursor at the Playlists root; on song rows it
            // plays the whole current tab as a fresh queue (the old Enter behavior).
            Some(Action::PlayAll) => {
                if self.playlists_root() {
                    self.play_selected_playlist()
                } else {
                    self.play_from_library()
                }
            }
            // `n` opens the create-playlist popup (bound only in the Playlists context).
            Some(Action::PlaylistCreate) => {
                self.open_playlist_create();
                Vec::new()
            }
            // `p` opens the add-to-playlist picker for the selected song(s). The Playlists
            // root has no song rows, so it's a no-op there (like `f`/`d`).
            Some(Action::AddToPlaylist) => {
                if !self.playlists_root() {
                    let songs = self.selected_library_songs();
                    self.open_playlist_picker(songs);
                }
                Vec::new()
            }
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
