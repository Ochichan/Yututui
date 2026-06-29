//! Search-screen reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// The track under the search-results cursor, if any.
    pub(in crate::app) fn selected_search_song(&self) -> Option<Song> {
        self.search.results.get(self.search.selected).cloned()
    }

    /// Move the search-results cursor up/down by `lines`, clamped.
    pub(in crate::app) fn move_search_cursor(&mut self, up: bool, lines: usize) {
        let len = self.search.results.len();
        if len == 0 {
            return;
        }
        self.search.selected = if up {
            self.search.selected.saturating_sub(lines)
        } else {
            (self.search.selected + lines).min(len - 1)
        };
        self.dirty = true;
    }

    pub(in crate::app) fn on_key_search(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match self.search.focus {
            SearchFocus::Input => {
                // Ctrl+A selects the whole query (desktop-style); idempotent re-select.
                if matches!(self.keymap.action(KeyContext::SearchInput, k.into()), Some(Action::SelectAll)) {
                    self.search.select_all = !self.search.input.is_empty();
                    self.dirty = true;
                    return Vec::new();
                }
                // With the query selected, the next key consumes the selection: a character
                // replaces it, Backspace clears it, anything else just deselects + falls through.
                if std::mem::take(&mut self.search.select_all) {
                    self.dirty = true;
                    let chord = Chord::from(k);
                    if chord.is_typeable()
                        && let KeyCode::Char(c) = k.code
                    {
                        self.search.input.clear();
                        self.search.input.push(c);
                        return Vec::new();
                    }
                    if matches!(self.keymap.action(KeyContext::SearchInput, k.into()), Some(Action::DeleteChar)) {
                        self.search.input.clear();
                        return Vec::new();
                    }
                }
                if k.code == KeyCode::Enter {
                    return self.submit_search_query();
                }
                let chord = Chord::from(k);
                if chord.is_typeable()
                    && let KeyCode::Char(c) = k.code
                {
                    self.search.input.push(c);
                    self.dirty = true;
                    return Vec::new();
                }
                match self.keymap.action(KeyContext::SearchInput, k.into()) {
                    Some(Action::Back) => {
                        self.mode = Mode::Player;
                        self.dirty = true;
                        return Vec::new();
                    }
                    Some(Action::DeleteChar) => {
                        self.search.input.pop();
                        self.dirty = true;
                        return Vec::new();
                    }
                    Some(Action::MoveDown) if !self.search.results.is_empty() => {
                        self.search.focus = SearchFocus::Results;
                        self.dirty = true;
                        return Vec::new();
                    }
                    _ => {}
                }
                Vec::new()
            }
            // Enter plays the highlighted result right now, keeping the existing queue intact.
            SearchFocus::Results if k.code == KeyCode::Enter => match self.selected_search_song() {
                Some(song) => self.play_now(song),
                None => Vec::new(),
            },
            SearchFocus::Results => match self.keymap.action(KeyContext::SearchResults, k.into()) {
                // `\` adds the highlighted result to the queue without interrupting playback.
                Some(Action::Enqueue) => match self.selected_search_song() {
                    Some(song) => self.enqueue(song),
                    None => Vec::new(),
                },
                Some(Action::Back) => {
                    self.mode = Mode::Player;
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveUp) => {
                    if self.search.selected == 0 {
                        self.search.focus = SearchFocus::Input;
                    } else {
                        self.search.selected -= 1;
                    }
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveDown) => {
                    if self.search.selected + 1 < self.search.results.len() {
                        self.search.selected += 1;
                    }
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::PageUp) => {
                    self.move_search_cursor(true, self.page_step());
                    Vec::new()
                }
                Some(Action::PageDown) => {
                    self.move_search_cursor(false, self.page_step());
                    Vec::new()
                }
                Some(Action::JumpTop) => {
                    self.search.selected = 0;
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::JumpBottom) => {
                    self.search.selected = self.search.results.len().saturating_sub(1);
                    self.dirty = true;
                    Vec::new()
                }
                // Favorite the highlighted result (♥ appears on the row).
                Some(Action::Favorite) => {
                    if let Some(song) = self.search.results.get(self.search.selected).cloned() {
                        self.library.toggle_favorite(&song);
                        self.dirty = true;
                        return vec![Cmd::SaveLibrary];
                    }
                    Vec::new()
                }
                Some(Action::Download) => {
                    match self.search.results.get(self.search.selected).cloned() {
                        Some(song) => self.start_download(song),
                        None => Vec::new(),
                    }
                }
                Some(Action::FocusInput) => {
                    self.search.focus = SearchFocus::Input;
                    self.dirty = true;
                    Vec::new()
                }
                _ => Vec::new(),
            },
        }
    }

    pub(in crate::app) fn submit_search_query(&mut self) -> Vec<Cmd> {
        self.search.select_all = false;
        let q = self.search.input.trim().to_owned();
        self.dirty = true;
        if q.is_empty() {
            Vec::new()
        } else {
            self.search.searching = true;
            self.status.text.clear();
            vec![Cmd::Search(q)]
        }
    }
}
