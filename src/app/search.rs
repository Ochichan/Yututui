//! Search-screen reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    /// Move the search-results cursor up/down by `lines`, clamped.
    pub(in crate::app) fn move_search_cursor(&mut self, up: bool, lines: usize) {
        let len = self.search_results.len();
        if len == 0 {
            return;
        }
        self.search_selected = if up {
            self.search_selected.saturating_sub(lines)
        } else {
            (self.search_selected + lines).min(len - 1)
        };
        self.dirty = true;
    }

    pub(in crate::app) fn on_key_search(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match self.search_focus {
            SearchFocus::Input => {
                // Ctrl+A selects the whole query (desktop-style); idempotent re-select.
                if matches!(self.keymap.action(KeyContext::SearchInput, k.into()), Some(Action::SelectAll)) {
                    self.search_select_all = !self.search_input.is_empty();
                    self.dirty = true;
                    return Vec::new();
                }
                // With the query selected, the next key consumes the selection: a character
                // replaces it, Backspace clears it, anything else just deselects + falls through.
                if std::mem::take(&mut self.search_select_all) {
                    self.dirty = true;
                    let chord = Chord::from(k);
                    if chord.is_typeable()
                        && let KeyCode::Char(c) = k.code
                    {
                        self.search_input.clear();
                        self.search_input.push(c);
                        return Vec::new();
                    }
                    if matches!(self.keymap.action(KeyContext::SearchInput, k.into()), Some(Action::DeleteChar)) {
                        self.search_input.clear();
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
                    self.search_input.push(c);
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
                        self.search_input.pop();
                        self.dirty = true;
                        return Vec::new();
                    }
                    Some(Action::MoveDown) if !self.search_results.is_empty() => {
                        self.search_focus = SearchFocus::Results;
                        self.dirty = true;
                        return Vec::new();
                    }
                    _ => {}
                }
                Vec::new()
            }
            SearchFocus::Results if k.code == KeyCode::Enter => self.play_selected(),
            SearchFocus::Results => match self.keymap.action(KeyContext::SearchResults, k.into()) {
                Some(Action::Back) => {
                    self.mode = Mode::Player;
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveUp) => {
                    if self.search_selected == 0 {
                        self.search_focus = SearchFocus::Input;
                    } else {
                        self.search_selected -= 1;
                    }
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::MoveDown) => {
                    if self.search_selected + 1 < self.search_results.len() {
                        self.search_selected += 1;
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
                    self.search_selected = 0;
                    self.dirty = true;
                    Vec::new()
                }
                Some(Action::JumpBottom) => {
                    self.search_selected = self.search_results.len().saturating_sub(1);
                    self.dirty = true;
                    Vec::new()
                }
                // Favorite the highlighted result (♥ appears on the row).
                Some(Action::Favorite) => {
                    if let Some(song) = self.search_results.get(self.search_selected).cloned() {
                        self.library.toggle_favorite(&song);
                        self.dirty = true;
                        return vec![Cmd::SaveLibrary];
                    }
                    Vec::new()
                }
                Some(Action::Download) => {
                    match self.search_results.get(self.search_selected).cloned() {
                        Some(song) => self.start_download(song),
                        None => Vec::new(),
                    }
                }
                Some(Action::FocusInput) => {
                    self.search_focus = SearchFocus::Input;
                    self.dirty = true;
                    Vec::new()
                }
                _ => Vec::new(),
            },
        }
    }

    pub(in crate::app) fn submit_search_query(&mut self) -> Vec<Cmd> {
        self.search_select_all = false;
        let q = self.search_input.trim().to_owned();
        self.dirty = true;
        if q.is_empty() {
            Vec::new()
        } else {
            self.searching = true;
            self.status.clear();
            vec![Cmd::Search(q)]
        }
    }
}
