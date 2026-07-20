//! Artist detail screen ([`Mode::Artist`]) reducer methods. Reached only from a Search
//! artist row; Back returns to Search with the results and cursor untouched.

use super::*;

impl App {
    /// Handle artist-row double-click activation without flattening artist logic into mouse.rs.
    pub(in crate::app) fn artist_mouse_double_click(
        &mut self,
        target: Option<&MouseTarget>,
    ) -> Option<Vec<Cmd>> {
        if self.mode != Mode::Artist {
            return None;
        }
        let (section, index) = match target {
            Some(MouseTarget::ArtistSongRow(index)) => (ArtistSection::Songs, *index),
            Some(MouseTarget::ArtistAlbumRow(index)) => (ArtistSection::Albums, *index),
            _ => return None,
        };
        self.artist_select(section, index);
        Some(self.artist_activate_selected())
    }

    /// Scroll the artist section under the pointer, falling back to the focused section.
    pub(in crate::app) fn artist_mouse_scroll(&mut self, up: bool, col: u16, row: u16, n: usize) {
        let hovered = match self.mouse_target_at(col, row) {
            Some(
                MouseTarget::ArtistSongRow(_) | MouseTarget::Scrollbar(ScrollSurface::ArtistSongs),
            ) => Some(ArtistSection::Songs),
            Some(
                MouseTarget::ArtistAlbumRow(_)
                | MouseTarget::Scrollbar(ScrollSurface::ArtistAlbums),
            ) => Some(ArtistSection::Albums),
            _ => None,
        };
        if let Some(state) = self.search.artist.as_ref() {
            match hovered.unwrap_or(state.section) {
                ArtistSection::Songs => {
                    state.songs_scroll.wheel(up, n, state.page.songs.len());
                }
                ArtistSection::Albums => {
                    state.albums_scroll.wheel(up, n, state.page.albums.len());
                }
            }
            self.dirty = true;
        }
    }

    /// An artist page arrived ([`SearchMsg::ArtistPage`]): open the detail screen. An empty
    /// page (no songs and no albums) stays on Search with a status line instead of
    /// presenting a blank screen.
    pub(in crate::app) fn on_artist_page(&mut self, page: crate::api::ArtistPage) -> Vec<Cmd> {
        self.dirty = true;
        if page.songs.is_empty() && page.albums.is_empty() {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "Artist page is empty or unavailable",
                "아티스트 페이지가 비어 있거나 사용할 수 없어요",
                "アーティストページが空か利用できません"
            )
            .to_owned();
            return Vec::new();
        }
        self.status.text.clear();
        self.search.artist = Some(ArtistPageState::new(page));
        self.mode = Mode::Artist;
        Vec::new()
    }

    /// Close the artist screen back to Search (results, cursor, and query untouched).
    pub(in crate::app) fn close_artist(&mut self) {
        self.search.artist = None;
        if self.mode == Mode::Artist {
            self.mode = Mode::Search;
        }
        self.dirty = true;
    }

    pub(in crate::app) fn on_key_artist(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // Section hop and close use raw keycodes (like the popups) so a rebind can never
        // strand the screen; everything else reuses the Search-results action map — the
        // artist lists deliberately share that muscle memory instead of a new context.
        match k.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.artist_toggle_section();
                return Vec::new();
            }
            KeyCode::Esc => {
                self.close_artist();
                return Vec::new();
            }
            KeyCode::Enter => return self.artist_activate_selected(),
            _ => {}
        }
        match self.keymap.action(KeyContext::SearchResults, k.into()) {
            Some(Action::Back) => {
                self.close_artist();
                Vec::new()
            }
            Some(Action::MoveUp) => {
                let step = self.nav_repeat_step(Action::MoveUp);
                self.artist_move_cursor(true, step);
                Vec::new()
            }
            Some(Action::MoveDown) => {
                let step = self.nav_repeat_step(Action::MoveDown);
                self.artist_move_cursor(false, step);
                Vec::new()
            }
            Some(Action::PageUp) => {
                self.artist_move_cursor(true, self.page_step());
                Vec::new()
            }
            Some(Action::PageDown) => {
                self.artist_move_cursor(false, self.page_step());
                Vec::new()
            }
            Some(Action::JumpTop) => {
                self.artist_move_cursor(true, usize::MAX);
                Vec::new()
            }
            Some(Action::JumpBottom) => {
                self.artist_move_cursor(false, usize::MAX);
                Vec::new()
            }
            Some(Action::Enqueue) => match self.artist_selected_row() {
                Some(row) if row.youtube_playlist_id().is_some() => {
                    self.fetch_playlist_tracks(&row, crate::api::PlaylistIntent::Enqueue)
                }
                Some(row) => self.enqueue(row),
                None => Vec::new(),
            },
            Some(Action::AddToPlaylist) => match self.artist_selected_row() {
                Some(row) if row.youtube_playlist_id().is_some() => {
                    self.fetch_playlist_tracks(&row, crate::api::PlaylistIntent::Import)
                }
                Some(row) => {
                    self.open_playlist_picker(vec![row]);
                    Vec::new()
                }
                None => Vec::new(),
            },
            Some(Action::Favorite) => match self.artist_selected_row() {
                Some(row) if row.youtube_playlist_id().is_some() => self.playlist_row_hint(),
                Some(row) => {
                    self.library_mut().toggle_favorite(&row);
                    self.dirty = true;
                    vec![Cmd::Persist(PersistCmd::Library)]
                }
                None => Vec::new(),
            },
            Some(Action::Download) => match self.artist_selected_row() {
                Some(row) if row.youtube_playlist_id().is_some() => self.playlist_row_hint(),
                Some(row) => self.start_download(row),
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// Flip the cursor between the top-songs and albums sections; a section with no rows
    /// never takes it.
    pub(in crate::app) fn artist_toggle_section(&mut self) {
        let Some(st) = self.search.artist.as_mut() else {
            return;
        };
        let target = match st.section {
            ArtistSection::Songs => ArtistSection::Albums,
            ArtistSection::Albums => ArtistSection::Songs,
        };
        let target_len = match target {
            ArtistSection::Songs => st.page.songs.len(),
            ArtistSection::Albums => st.page.albums.len(),
        };
        if target_len > 0 {
            st.section = target;
            self.dirty = true;
        }
    }

    /// Move the focused section's cursor up/down by `lines`, clamped (saturating, so
    /// `usize::MAX` doubles as jump-to-top/bottom).
    fn artist_move_cursor(&mut self, up: bool, lines: usize) {
        let Some(st) = self.search.artist.as_mut() else {
            return;
        };
        let len = st.section_rows().len();
        if len == 0 {
            return;
        }
        let sel = match st.section {
            ArtistSection::Songs => &mut st.songs_selected,
            ArtistSection::Albums => &mut st.albums_selected,
        };
        let cur = (*sel).min(len - 1);
        *sel = if up {
            cur.saturating_sub(lines)
        } else {
            cur.saturating_add(lines).min(len - 1)
        };
        self.dirty = true;
    }

    /// The row under the focused section's cursor.
    fn artist_selected_row(&self) -> Option<Song> {
        let st = self.search.artist.as_ref()?;
        st.section_rows().get(st.section_selected()).cloned()
    }

    /// Land the cursor on `(section, idx)` (a row click); no-op past the end.
    pub(in crate::app) fn artist_select(&mut self, section: ArtistSection, idx: usize) {
        let Some(st) = self.search.artist.as_mut() else {
            return;
        };
        let len = match section {
            ArtistSection::Songs => st.page.songs.len(),
            ArtistSection::Albums => st.page.albums.len(),
        };
        if idx >= len {
            return;
        }
        st.section = section;
        match section {
            ArtistSection::Songs => st.songs_selected = idx,
            ArtistSection::Albums => st.albums_selected = idx,
        }
        self.dirty = true;
    }

    /// Activate the focused row (Enter / double-click): a song plays right now, an album
    /// row fetches its tracks and replaces the queue — the Search playlist row's exact
    /// semantics.
    pub(in crate::app) fn artist_activate_selected(&mut self) -> Vec<Cmd> {
        let Some(row) = self.artist_selected_row() else {
            return Vec::new();
        };
        self.dirty = true;
        match row.youtube_playlist_id() {
            Some(_) => self.fetch_playlist_tracks(&row, crate::api::PlaylistIntent::Play),
            None => self.play_now(row),
        }
    }
}
