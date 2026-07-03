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
                    Some(Action::ToggleSearchKind) => {
                        return self.toggle_search_kind();
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
            // Enter plays the highlighted result right now, keeping the existing queue
            // intact. A playlist row fetches its tracks first, then replaces the queue.
            SearchFocus::Results if k.code == KeyCode::Enter => match self.selected_search_song() {
                Some(song) => match song.youtube_playlist_id() {
                    Some(_) => self.fetch_playlist_tracks(&song, crate::api::PlaylistIntent::Play),
                    None => self.play_now(song),
                },
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
                Some(Action::ToggleSearchKind) => self.toggle_search_kind(),
                // `\` adds the highlighted result to the queue without interrupting playback.
                // A playlist row appends its whole track list.
                Some(Action::Enqueue) => match self.selected_search_song() {
                    Some(song) => match song.youtube_playlist_id() {
                        Some(_) => {
                            self.fetch_playlist_tracks(&song, crate::api::PlaylistIntent::Enqueue)
                        }
                        None => self.enqueue(song),
                    },
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
                        if song.youtube_playlist_id().is_some() {
                            return self.playlist_row_hint();
                        }
                        self.library.toggle_favorite(&song);
                        self.dirty = true;
                        return vec![Cmd::SaveLibrary];
                    }
                    Vec::new()
                }
                Some(Action::Download) => {
                    match self.search.results.get(self.search.selected).cloned() {
                        Some(song) if song.youtube_playlist_id().is_some() => {
                            self.playlist_row_hint()
                        }
                        Some(song) => self.start_download(song),
                        None => Vec::new(),
                    }
                }
                // `p` opens the add-to-playlist picker for the highlighted result. A
                // playlist row instead imports the whole playlist as a local one.
                Some(Action::AddToPlaylist) => {
                    if let Some(song) = self.search.results.get(self.search.selected).cloned() {
                        if song.youtube_playlist_id().is_some() {
                            return self
                                .fetch_playlist_tracks(&song, crate::api::PlaylistIntent::Import);
                        }
                        self.open_playlist_picker(vec![song]);
                    }
                    Vec::new()
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
            self.search.searching = true;
            self.status.text.clear();
            // Playlist kind is YouTube-only (no other provider has a playlist catalog),
            // so it bypasses the source selection entirely.
            if self.search.kind == SearchKind::Playlists && !self.radio_dedicated_mode {
                return vec![Cmd::SearchPlaylists { query: q }];
            }
            let config = self.search_config_for_mode();
            let source = config.normalized_source(self.search.source);
            self.search.source = source;
            vec![Cmd::Search {
                query: q,
                source,
                config,
            }]
        }
    }

    /// `Ctrl+P`: flip the search box between tracks and public YouTube playlists.
    pub(in crate::app) fn toggle_search_kind(&mut self) -> Vec<Cmd> {
        self.search.kind = match self.search.kind {
            SearchKind::Songs => SearchKind::Playlists,
            SearchKind::Playlists => SearchKind::Songs,
        };
        self.status.kind = StatusKind::Info;
        self.status.text = match self.search.kind {
            SearchKind::Songs => t!("Search: songs", "검색: 곡").to_owned(),
            SearchKind::Playlists => {
                t!("Search: YouTube playlists", "검색: 유튜브 플레이리스트").to_owned()
            }
        };
        self.dirty = true;
        Vec::new()
    }

    /// Kick the track fetch for a playlist row; `intent` decides what happens when the
    /// tracks arrive ([`Msg::PlaylistTracks`] → [`Self::on_playlist_tracks`]).
    fn fetch_playlist_tracks(
        &mut self,
        row: &Song,
        intent: crate::api::PlaylistIntent,
    ) -> Vec<Cmd> {
        let Some(id) = row.youtube_playlist_id() else {
            return Vec::new();
        };
        self.status.kind = StatusKind::Info;
        self.status.text = t!("Fetching playlist…", "플레이리스트 불러오는 중…").to_owned();
        self.dirty = true;
        vec![Cmd::FetchPlaylistTracks {
            playlist_id: id.to_owned(),
            title: row.title.clone(),
            intent,
        }]
    }

    /// Status hint for per-track actions that don't apply to a playlist row.
    fn playlist_row_hint(&mut self) -> Vec<Cmd> {
        self.status.kind = StatusKind::Info;
        self.status.text = t!(
            "Playlist row: Enter plays, \\ enqueues, p imports",
            "플레이리스트 행: Enter 재생, \\ 큐 추가, p 가져오기"
        )
        .to_owned();
        self.dirty = true;
        Vec::new()
    }

    /// A remote playlist's tracks arrived: play, enqueue, or import per the intent the
    /// user picked on the row.
    pub(in crate::app) fn on_playlist_tracks(
        &mut self,
        title: String,
        intent: crate::api::PlaylistIntent,
        songs: Vec<Song>,
    ) -> Vec<Cmd> {
        use crate::api::PlaylistIntent;
        self.dirty = true;
        if songs.is_empty() {
            self.status.text = t!(
                "Playlist is empty or unavailable",
                "플레이리스트가 비어 있거나 사용할 수 없어요"
            )
            .to_owned();
            return Vec::new();
        }
        match intent {
            PlaylistIntent::Play => {
                // Mirror `Msg::AiPlayTracks`: replace the queue and start at the top.
                let requested = songs.clone();
                self.queue.set(songs, 0);
                let song = self.queue.current().cloned();
                let mut cmds = self.load_song(song);
                cmds.extend(self.request_romanization_for_songs(&requested));
                // After load_song, which clears the transient status.
                self.status.kind = StatusKind::Info;
                self.status.text = if crate::i18n::is_korean() {
                    format!("플레이리스트 재생: {title} ({}곡)", requested.len())
                } else {
                    format!("Playing playlist: {title} ({} tracks)", requested.len())
                };
                cmds
            }
            PlaylistIntent::Enqueue => {
                let added = self.queue.extend(songs);
                self.status.kind = StatusKind::Info;
                self.status.text = if crate::i18n::is_korean() {
                    format!("{added}곡을 대기열에 추가함")
                } else {
                    format!("Queued {added} track(s)")
                };
                Vec::new()
            }
            PlaylistIntent::Import => {
                let Some(id) = self.playlists.create(&title) else {
                    self.status.text = t!(
                        "Could not create the playlist (name in use or limit reached)",
                        "플레이리스트를 만들 수 없어요 (이름 중복 또는 한도 초과)"
                    )
                    .to_owned();
                    return Vec::new();
                };
                let mut added = 0usize;
                for song in songs {
                    if matches!(
                        self.playlists.add(&id, song),
                        crate::playlists::AddResult::Added
                    ) {
                        added += 1;
                    }
                }
                self.status.kind = StatusKind::Info;
                self.status.text = if crate::i18n::is_korean() {
                    format!("플레이리스트 가져옴: {title} ({added}곡)")
                } else {
                    format!("Imported playlist: {title} ({added} tracks)")
                };
                vec![Cmd::SavePlaylists]
            }
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
