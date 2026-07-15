//! Library-input reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;
use crate::util::query::{MAX_FILTER_QUERY_BYTES, try_insert_query_char};

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
        self.library_ui.picked.clear();
        self.dirty = true;
    }

    /// Move the library cursor up/down by `lines`, clamped, **without** collapsing the
    /// multi-select anchor — the keyboard mirror of a mouse drag-select (Shift+nav). At the
    /// Playlists root, which has no multi-select (see `mouse.rs` drag handling), it falls
    /// back to a plain collapsing move.
    pub(in crate::app) fn extend_library_cursor(&mut self, up: bool, lines: usize) {
        if self.playlists_root() {
            self.move_library_cursor(up, lines);
            return;
        }
        let len = self.library_len();
        if len == 0 {
            return;
        }
        self.library_ui.selected = if up {
            self.library_ui.selected.saturating_sub(lines)
        } else {
            (self.library_ui.selected + lines).min(len - 1)
        };
        // Extending is a range gesture — discontiguous picks yield to the range.
        self.library_ui.picked.clear();
        self.dirty = true;
    }

    /// Ctrl/Cmd+click on a library row: toggle it in/out of the discontiguous selection.
    /// The first toggle seeds the picked set from the current range selection (so a
    /// modifier click *adds to* what is already highlighted, desktop-style); emptying the
    /// set collapses the selection back onto the cursor. The Playlists root has no
    /// multi-select (its actions are single-row), so the modifier click degrades to a
    /// plain click there.
    pub(in crate::app) fn library_toggle_pick(&mut self, index: usize) -> Vec<Cmd> {
        let len = self.library_len();
        if index >= len {
            return Vec::new();
        }
        if self.playlists_root() {
            return self.on_list_row_click(index);
        }
        if self.library_ui.picked.is_empty() {
            let lo = self
                .library_ui
                .selected
                .min(self.library_ui.anchor)
                .min(len - 1);
            let hi = self
                .library_ui
                .selected
                .max(self.library_ui.anchor)
                .min(len - 1);
            self.library_ui.picked.extend(lo..=hi);
        }
        if !self.library_ui.picked.remove(&index) {
            self.library_ui.picked.insert(index);
        }
        self.library_ui.selected = index;
        self.library_ui.anchor = index;
        // A toggle click never starts a drag range; the next drag anchors freshly.
        self.interaction.drag_selection = None;
        self.dirty = true;
        Vec::new()
    }

    /// Stop capturing filter input, drop the query, and snap the cursor/scroll back to the top
    /// of the now-unfiltered list. Also used when switching tabs.
    pub(in crate::app) fn clear_library_filter(&mut self) {
        self.library_ui.filter_query.clear();
        self.library_ui.filter_cursor = TextCursor::default();
        self.library_ui.filter_editing = false;
        self.library_ui.selected = 0;
        self.library_ui.anchor = 0;
        self.library_ui.picked.clear();
        self.bridges.library_scroll.reset();
    }

    /// After the filter text changes, jump the cursor to the first match and reset the scroll so
    /// the filtered list reads from the top (fzf-style).
    fn after_library_filter_change(&mut self) {
        self.library_ui.selected = 0;
        self.library_ui.anchor = 0;
        self.library_ui.picked.clear();
        self.bridges.library_scroll.reset();
        self.dirty = true;
    }

    /// Keystrokes while the filter input is focused: type to narrow, Backspace to edit, Enter
    /// to commit (keep the filter, return to list nav), Esc to cancel (clear the filter), and
    /// the arrows to move the cursor within the filtered rows.
    fn on_key_library_filter(&mut self, k: KeyEvent) -> Vec<Cmd> {
        if let Some(result) = self.keymap.text_edit_action(k.into()).and_then(|action| {
            apply_text_edit_action(
                action,
                &mut self.library_ui.filter_cursor,
                &mut self.library_ui.filter_query,
            )
        }) {
            match result {
                TextEditResult::BufferChanged(true) => self.after_library_filter_change(),
                TextEditResult::CursorMoved(true) => self.dirty = true,
                TextEditResult::BufferChanged(false) | TextEditResult::CursorMoved(false) => {}
            }
            return Vec::new();
        }
        match k.code {
            KeyCode::Esc => {
                self.clear_library_filter();
                self.dirty = true;
            }
            KeyCode::Enter => {
                self.library_ui.filter_editing = false;
                self.dirty = true;
            }
            KeyCode::Up => {
                self.library_ui.selected = self.library_ui.selected.saturating_sub(1);
                self.library_ui.anchor = self.library_ui.selected;
                self.library_ui.picked.clear();
                self.dirty = true;
            }
            KeyCode::Down => {
                let len = self.library_len();
                if self.library_ui.selected + 1 < len {
                    self.library_ui.selected += 1;
                }
                self.library_ui.anchor = self.library_ui.selected;
                self.library_ui.picked.clear();
                self.dirty = true;
            }
            // Plain typed characters extend the query; ignore Ctrl/Alt combos (e.g. Ctrl+R for
            // radio is already handled as a global before we get here).
            KeyCode::Char(c)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match try_insert_query_char(
                    &mut self.library_ui.filter_query,
                    &mut self.library_ui.filter_cursor,
                    c,
                    MAX_FILTER_QUERY_BYTES,
                ) {
                    Ok(()) => self.after_library_filter_change(),
                    Err(reason) => self.set_query_reject_status(reason),
                }
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
        let chord = crate::keymap::Chord::from(k);
        if self.library_ui.filter_editing && self.keymap.text_edit_action(chord).is_some() {
            return self.on_key_library_filter(k);
        }
        // A non-text Local Deck toggle should still work while the Library filter is focused.
        // If the user remaps it to a typeable key, the filter keeps owning that key.
        if !chord.is_typeable()
            && matches!(
                self.keymap.action(KeyContext::Library, chord),
                Some(Action::ToggleLocalMode)
            )
        {
            return self.request_local_mode_switch();
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
        if matches!(
            self.keymap.action(KeyContext::Library, chord),
            Some(Action::ToggleLocalMode)
        ) {
            return self.request_local_mode_switch();
        }
        let len = self.library_len();
        // The Playlists tab resolves against its own context so its bindings (and the
        // cheat-sheet group) can differ from the song tabs; Common still supplies shared nav.
        let ctx = if playlists_tab {
            KeyContext::Playlists
        } else {
            KeyContext::Library
        };
        match self.keymap.action(ctx, chord) {
            Some(Action::LibraryFilter) => {
                // Open the filter input (re-opens with the current query if one is applied).
                self.library_ui.filter_editing = true;
                self.library_ui.filter_cursor = TextCursor::at_end(&self.library_ui.filter_query);
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
                // `move_library_cursor` collapses the range to the cursor (like the queue
                // window). `nav_repeat_step` accelerates the move while the key is held.
                let step = self.nav_repeat_step(Action::MoveUp);
                self.move_library_cursor(true, step);
                Vec::new()
            }
            Some(Action::MoveDown) => {
                let step = self.nav_repeat_step(Action::MoveDown);
                self.move_library_cursor(false, step);
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
                self.library_ui.picked.clear();
                self.dirty = true;
                Vec::new()
            }
            Some(Action::JumpBottom) => {
                self.library_ui.selected = len.saturating_sub(1);
                self.library_ui.anchor = self.library_ui.selected;
                self.library_ui.picked.clear();
                self.dirty = true;
                Vec::new()
            }
            // Shift+nav extends the multi-select range (keyboard mirror of a mouse drag):
            // move the cursor but leave the anchor fixed. See `extend_library_cursor`.
            Some(Action::SelectUp) => {
                let step = self.nav_repeat_step(Action::SelectUp);
                self.extend_library_cursor(true, step);
                Vec::new()
            }
            Some(Action::SelectDown) => {
                let step = self.nav_repeat_step(Action::SelectDown);
                self.extend_library_cursor(false, step);
                Vec::new()
            }
            Some(Action::SelectPageUp) => {
                self.extend_library_cursor(true, self.page_step());
                Vec::new()
            }
            Some(Action::SelectPageDown) => {
                self.extend_library_cursor(false, self.page_step());
                Vec::new()
            }
            Some(Action::SelectToTop) => {
                self.extend_library_cursor(true, len);
                Vec::new()
            }
            Some(Action::SelectToBottom) => {
                self.extend_library_cursor(false, len);
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
            // `d` downloads the selection: a lone track immediately (unchanged), a drag-selected
            // range behind the "Download N songs?" confirm popup. No-op at the Playlists root
            // (no song rows there — that's `Shift+D`'s job).
            Some(Action::Download) => {
                if self.playlists_root() {
                    Vec::new()
                } else {
                    let songs = self.selected_library_songs();
                    match songs.len() {
                        0 => Vec::new(),
                        1 => self.start_download(songs.into_iter().next().unwrap()),
                        _ => self.open_confirm_download(songs),
                    }
                }
            }
            // `Shift+D` downloads a whole list at once (deduped, behind the confirm popup): the
            // highlighted playlist at the Playlists root, otherwise every row of the current tab.
            Some(Action::DownloadAll) => {
                let songs = if self.playlists_root() {
                    self.selected_root_playlist()
                        .map(|p| p.songs.clone())
                        .unwrap_or_default()
                } else {
                    self.library_songs()
                };
                self.open_confirm_download(songs)
            }
            // Un/favorite the highlighted track (removing shifts selection up).
            Some(Action::Favorite) => {
                if let Some(song) = self.selected_library_song() {
                    let rows_before = self.library_len();
                    self.library.toggle_favorite(&song);
                    // Un-favoriting can remove the row (Favorites/All tab): re-clamp and
                    // drop the now-stale picks. Tabs where the row list is unchanged
                    // (e.g. History) keep the selection.
                    if self.library_len() != rows_before {
                        self.clamp_library_selection();
                    }
                    self.dirty = true;
                    return vec![Cmd::Persist(PersistCmd::Library)];
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}
