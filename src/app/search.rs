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
        if self.dropdowns.search_source_open {
            return self.on_key_search_source_menu(k);
        }
        match self.search.focus {
            SearchFocus::Input => {
                // Ctrl+A selects the whole query (desktop-style); idempotent re-select.
                if matches!(
                    self.keymap.action(KeyContext::SearchInput, k.into()),
                    Some(Action::SelectAll)
                ) {
                    self.search.select_all = !self.search.input.is_empty();
                    self.dirty = true;
                    return Vec::new();
                }
                if matches!(
                    self.keymap.action(KeyContext::SearchInput, k.into()),
                    Some(Action::ToggleSearchSourceMenu)
                ) {
                    return self.toggle_search_source_menu();
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
                    if matches!(
                        self.keymap.action(KeyContext::SearchInput, k.into()),
                        Some(Action::DeleteChar)
                    ) {
                        self.search.input.clear();
                        return Vec::new();
                    }
                }
                let chord = Chord::from(k);
                if matches!(
                    self.keymap.context_action(KeyContext::SearchInput, chord),
                    Some(Action::FocusPrev)
                ) && !self.search.results.is_empty()
                {
                    self.search.focus = SearchFocus::Results;
                    self.dirty = true;
                    return Vec::new();
                }
                if k.code == KeyCode::Enter {
                    return self.submit_search_query();
                }
                if chord.is_typeable()
                    && let KeyCode::Char(c) = k.code
                {
                    self.search.input.push(c);
                    self.dirty = true;
                    return Vec::new();
                }
                match self.keymap.action(KeyContext::SearchInput, k.into()) {
                    Some(Action::ToggleSearchSourceMenu) => {
                        return self.toggle_search_source_menu();
                    }
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
            SearchFocus::Results
                if matches!(
                    self.keymap
                        .context_action(KeyContext::SearchResults, k.into()),
                    Some(Action::FocusPrev)
                ) =>
            {
                self.search.focus = SearchFocus::Input;
                self.dirty = true;
                Vec::new()
            }
            SearchFocus::Results => match self.keymap.action(KeyContext::SearchResults, k.into()) {
                Some(Action::ToggleSearchSourceMenu) => self.toggle_search_source_menu(),
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

    fn on_key_search_source_menu(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let chord = Chord::from(k);
        let ctx = match self.search.focus {
            SearchFocus::Input => KeyContext::SearchInput,
            SearchFocus::Results => KeyContext::SearchResults,
        };
        let action = self
            .keymap
            .action(ctx, chord)
            .or_else(|| self.keymap.action(KeyContext::Common, chord));

        match action {
            Some(Action::ToggleSearchSourceMenu)
            | Some(Action::Confirm)
            | Some(Action::Back)
            | Some(Action::FocusInput) => {
                self.dropdowns.search_source_open = false;
                self.dirty = true;
                Vec::new()
            }
            Some(Action::MoveUp) | Some(Action::FocusPrev) => self.cycle_search_source(false),
            Some(Action::MoveDown) | Some(Action::FocusNext) => self.cycle_search_source(true),
            _ => match k.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.dropdowns.search_source_open = false;
                    self.dirty = true;
                    Vec::new()
                }
                KeyCode::Left | KeyCode::Up | KeyCode::BackTab => self.cycle_search_source(false),
                KeyCode::Right | KeyCode::Down | KeyCode::Tab => self.cycle_search_source(true),
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
            let config = self.search_config_for_mode();
            let source = config.normalized_source(self.search.source);
            self.search.source = source;
            self.search.searching = true;
            self.status.text.clear();
            vec![Cmd::Search {
                query: q,
                source,
                config,
            }]
        }
    }

    pub(in crate::app) fn select_search_source(&mut self, source: SearchSource) -> Vec<Cmd> {
        self.set_search_source(source, true)
    }

    pub(in crate::app) fn toggle_search_source_menu(&mut self) -> Vec<Cmd> {
        let config = self.search_config_for_mode();
        self.search.source = config.normalized_source(self.search.source);
        self.dropdowns.search_source_open = !self.dropdowns.search_source_open;
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn cycle_search_source(&mut self, forward: bool) -> Vec<Cmd> {
        let search = self.search_config_for_mode();
        let source = search.cycled_source(self.search.source, forward);
        self.set_search_source(source, false)
    }

    fn set_search_source(&mut self, source: SearchSource, close_menu: bool) -> Vec<Cmd> {
        let mut search = self.search_config_for_mode();
        let source = search.normalized_source(source);
        search.source = source;
        self.search.source = source;
        if close_menu {
            self.dropdowns.search_source_open = false;
        }
        self.status.kind = StatusKind::Info;
        self.status.text = format!("{}: {}", t!("Search source", "검색 소스"), source.label());
        self.dirty = true;
        if self.radio_dedicated_mode {
            Vec::new()
        } else {
            self.config.search = search;
            vec![Cmd::SaveConfig(Box::new(self.config.clone()))]
        }
    }
}
