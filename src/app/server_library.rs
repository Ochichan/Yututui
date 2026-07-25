//! Read-only OpenSubsonic library state and reducer.
//!
//! The local YuTuTui library remains authoritative and fully usable when this surface is
//! unavailable. Server pages are bounded, generation-stamped snapshots; no server rating or
//! playlist mutation is reachable from this module.

use crossterm::event::{KeyCode, KeyEvent};

use super::{App, Cmd, Mode, MouseTarget, StatusKind};
use crate::api::Song;
use crate::keymap::{Action, Chord, KeyContext};
use crate::open_subsonic::model::{
    AlbumId, ArtistId, ServerLibraryDetail, ServerLibraryPage, ServerLibraryRow,
    ServerLibrarySection, ServerPlaylistId, ServerSong,
};

pub const SERVER_LIBRARY_PAGE_LIMIT: u32 = 50;
const SERVER_LIBRARY_MAX_OFFSET: u32 = 20_000;
const SERVER_LIBRARY_MAX_PAGE_HISTORY: usize = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LibrarySource {
    #[default]
    Yututui,
    OpenSubsonic,
}

impl LibrarySource {
    pub fn label(self, server_name: &str) -> &str {
        match self {
            Self::Yututui => "YuTuTui",
            Self::OpenSubsonic => server_name,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerLibraryFailure {
    Offline,
    Authentication,
    Unsupported,
    InvalidResponse,
    Unavailable,
}

impl ServerLibraryFailure {
    pub fn label(self) -> &'static str {
        match self {
            Self::Offline => crate::t!(
                "Server is offline. Your local library still works.",
                "서버가 오프라인이에요. 로컬 보관함은 계속 사용할 수 있어요.",
                "サーバーはオフラインです。ローカルライブラリは引き続き使えます。"
            ),
            Self::Authentication => crate::t!(
                "Server sign-in needs attention.",
                "서버 로그인을 확인해 주세요.",
                "サーバーへのログインを確認してください。"
            ),
            Self::Unsupported => crate::t!(
                "This server does not support this section.",
                "이 서버는 이 섹션을 지원하지 않아요.",
                "このサーバーはこのセクションに対応していません。"
            ),
            Self::InvalidResponse => crate::t!(
                "The server returned an unreadable response.",
                "서버 응답을 읽을 수 없어요.",
                "サーバーの応答を読み取れません。"
            ),
            Self::Unavailable => crate::t!(
                "Set up a music server in Settings → Sync.",
                "설정 → 동기화에서 음악 서버를 설정해 주세요.",
                "設定 → 同期で音楽サーバーを設定してください。"
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerLibraryDetailTarget {
    Album(AlbumId),
    Artist(ArtistId),
    Playlist(ServerPlaylistId),
}

pub enum ServerLibraryCommand {
    LoadPage {
        generation: u64,
        section: ServerLibrarySection,
        offset: u32,
        limit: u32,
    },
    LoadDetail {
        generation: u64,
        target: ServerLibraryDetailTarget,
    },
}

pub enum ServerLibraryEvent {
    PageLoaded {
        generation: u64,
        offset: u32,
        result: Result<ServerLibraryPage, ServerLibraryFailure>,
    },
    DetailLoaded {
        generation: u64,
        result: Result<ServerLibraryDetail, ServerLibraryFailure>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerLibraryBusy {
    Page,
    Detail,
}

pub struct ServerLibraryState {
    pub source: LibrarySource,
    pub section: ServerLibrarySection,
    pub selected: usize,
    pub offset: u32,
    pub previous_offsets: Vec<u32>,
    pub page: Option<ServerLibraryPage>,
    pub detail: Option<ServerLibraryDetail>,
    pub generation: u64,
    pub busy: Option<ServerLibraryBusy>,
    pub failure: Option<ServerLibraryFailure>,
}

impl Default for ServerLibraryState {
    fn default() -> Self {
        Self {
            source: LibrarySource::Yututui,
            section: ServerLibrarySection::RecentlyPlayed,
            selected: 0,
            offset: 0,
            previous_offsets: Vec::new(),
            page: None,
            detail: None,
            generation: 0,
            busy: None,
            failure: None,
        }
    }
}

impl ServerLibraryState {
    fn next_generation(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.generation
    }

    pub fn rows_len(&self) -> usize {
        match self.detail.as_ref() {
            Some(ServerLibraryDetail::AlbumSongs { songs, .. }) => songs.len(),
            Some(ServerLibraryDetail::ArtistAlbums { albums, .. }) => albums.len(),
            Some(ServerLibraryDetail::PlaylistEntries(playlist)) => playlist.entries.len(),
            None => self.page.as_ref().map_or(0, |page| page.rows.len()),
        }
    }

    pub fn row_song(&self, index: usize) -> Option<&ServerSong> {
        match self.detail.as_ref() {
            Some(ServerLibraryDetail::AlbumSongs { songs, .. }) => songs.get(index),
            Some(ServerLibraryDetail::ArtistAlbums { .. }) => None,
            Some(ServerLibraryDetail::PlaylistEntries(playlist)) => playlist.entries.get(index),
            None => self
                .page
                .as_ref()
                .and_then(|page| page.rows.get(index))
                .and_then(|row| match row {
                    ServerLibraryRow::Song(song) => Some(song),
                    ServerLibraryRow::Album(_)
                    | ServerLibraryRow::Artist(_)
                    | ServerLibraryRow::Playlist(_) => None,
                }),
        }
    }

    pub fn visible_songs(&self) -> Vec<Song> {
        match self.detail.as_ref() {
            Some(ServerLibraryDetail::AlbumSongs { songs, .. }) => songs
                .iter()
                .cloned()
                .map(Song::from_open_subsonic)
                .collect(),
            Some(ServerLibraryDetail::ArtistAlbums { .. }) => Vec::new(),
            Some(ServerLibraryDetail::PlaylistEntries(playlist)) => playlist
                .entries
                .iter()
                .cloned()
                .map(Song::from_open_subsonic)
                .collect(),
            None => self
                .page
                .iter()
                .flat_map(|page| &page.rows)
                .filter_map(|row| match row {
                    ServerLibraryRow::Song(song) => Some(Song::from_open_subsonic(song.clone())),
                    ServerLibraryRow::Album(_)
                    | ServerLibraryRow::Artist(_)
                    | ServerLibraryRow::Playlist(_) => None,
                })
                .collect(),
        }
    }

    pub fn next_offset(&self) -> Option<u32> {
        self.detail
            .is_none()
            .then(|| self.page.as_ref().and_then(|page| page.next_offset))
            .flatten()
            .filter(|offset| *offset > self.offset && *offset <= SERVER_LIBRARY_MAX_OFFSET)
    }

    pub fn reset_after_profile_removal(&mut self) {
        self.source = LibrarySource::Yututui;
        self.invalidate_after_profile_change();
    }

    pub fn invalidate_after_profile_change(&mut self) {
        self.page = None;
        self.detail = None;
        self.previous_offsets.clear();
        self.offset = 0;
        self.selected = 0;
        self.failure = None;
        self.busy = None;
        self.next_generation();
    }
}

impl App {
    pub(in crate::app) fn server_library_mouse_double_click(
        &mut self,
        target: Option<&MouseTarget>,
    ) -> Option<Vec<Cmd>> {
        let Some(MouseTarget::ServerLibraryRow { generation, index }) = target else {
            return None;
        };
        (self.mode == Mode::Library && self.server.library.source == LibrarySource::OpenSubsonic)
            .then(|| self.activate_server_library_row(*generation, *index))
    }

    pub(in crate::app) fn library_rows_len_for_wheel(&self) -> usize {
        if self.local_dedicated_mode {
            self.local_rows_len()
        } else if self.server.library.source == LibrarySource::OpenSubsonic {
            self.server.library.rows_len()
        } else {
            self.library_len()
        }
    }

    pub(in crate::app) fn select_library_source(&mut self, source: LibrarySource) -> Vec<Cmd> {
        if self.local_dedicated_mode {
            return Vec::new();
        }
        // The local filter editor is not rendered on the server surface. End its capture before
        // switching so typed server shortcuts cannot silently mutate the hidden local query.
        if source == LibrarySource::OpenSubsonic && self.library_ui.filter_editing {
            self.library_ui.filter_editing = false;
            self.dirty = true;
        }
        if self.server.library.source == source {
            return Vec::new();
        }
        // Fence any page/detail completion owned by the source we are leaving. In particular,
        // returning to the local library while a server request is in flight must not leave a
        // stale `busy` latch that prevents a later fresh server request.
        self.server.library.next_generation();
        self.server.library.busy = None;
        self.server.library.source = source;
        self.server.library.selected = 0;
        self.bridges.library_scroll.reset();
        self.interaction.drag_selection = None;
        self.interaction.drag_scrollbar = None;
        self.dirty = true;
        match source {
            LibrarySource::Yututui => Vec::new(),
            LibrarySource::OpenSubsonic => self.request_server_library_page(0, false),
        }
    }

    pub(in crate::app) fn select_server_library_section(
        &mut self,
        section: ServerLibrarySection,
    ) -> Vec<Cmd> {
        if self.server.library.source != LibrarySource::OpenSubsonic
            || self.server.library.busy.is_some()
            || self.server.library.section == section
        {
            return Vec::new();
        }
        self.server.library.section = section;
        self.server.library.previous_offsets.clear();
        self.server.library.detail = None;
        self.server.library.selected = 0;
        self.bridges.library_scroll.reset();
        self.request_server_library_page(0, false)
    }

    fn step_server_library_section(&mut self, forward: bool) -> Vec<Cmd> {
        let sections = [
            ServerLibrarySection::RecentlyPlayed,
            ServerLibrarySection::Albums,
            ServerLibrarySection::Artists,
            ServerLibrarySection::Songs,
            ServerLibrarySection::Playlists,
        ];
        let index = sections
            .iter()
            .position(|section| *section == self.server.library.section)
            .unwrap_or(0);
        let next = if forward {
            (index + 1) % sections.len()
        } else {
            (index + sections.len() - 1) % sections.len()
        };
        self.select_server_library_section(sections[next])
    }

    pub(in crate::app) fn request_server_library_page(
        &mut self,
        offset: u32,
        remember_current: bool,
    ) -> Vec<Cmd> {
        if self.server.library.busy.is_some() || offset > SERVER_LIBRARY_MAX_OFFSET {
            return Vec::new();
        }
        if remember_current {
            if self.server.library.previous_offsets.len() == SERVER_LIBRARY_MAX_PAGE_HISTORY {
                self.server.library.previous_offsets.remove(0);
            }
            self.server
                .library
                .previous_offsets
                .push(self.server.library.offset);
        }
        let generation = self.server.library.next_generation();
        self.server.library.busy = Some(ServerLibraryBusy::Page);
        self.server.library.failure = None;
        // A page belongs to the generation and section that produced it. Drop it before
        // dispatch so a failed section/page request cannot leave stale rows reachable through
        // keyboard or mouse activation.
        self.server.library.page = None;
        self.server.library.detail = None;
        self.server.library.selected = 0;
        self.bridges.library_scroll.reset();
        self.dirty = true;
        vec![Cmd::ServerLibrary(ServerLibraryCommand::LoadPage {
            generation,
            section: self.server.library.section,
            offset,
            limit: SERVER_LIBRARY_PAGE_LIMIT,
        })]
    }

    pub(in crate::app) fn next_server_library_page(&mut self) -> Vec<Cmd> {
        let Some(offset) = self.server.library.next_offset() else {
            return Vec::new();
        };
        self.request_server_library_page(offset, true)
    }

    pub(in crate::app) fn previous_server_library_page(&mut self) -> Vec<Cmd> {
        if self.server.library.busy.is_some() {
            return Vec::new();
        }
        let Some(offset) = self.server.library.previous_offsets.pop() else {
            return Vec::new();
        };
        self.request_server_library_page(offset, false)
    }

    pub(in crate::app) fn select_server_library_row(&mut self, generation: u64, index: usize) {
        if self.server.library.source == LibrarySource::OpenSubsonic
            && generation == self.server.library.generation
            && index < self.server.library.rows_len()
        {
            self.server.library.selected = index;
            self.dirty = true;
        }
    }

    pub(in crate::app) fn activate_server_library_row(
        &mut self,
        generation: u64,
        index: usize,
    ) -> Vec<Cmd> {
        if generation != self.server.library.generation
            || self.server.library.busy.is_some()
            || index >= self.server.library.rows_len()
        {
            return Vec::new();
        }
        self.server.library.selected = index;
        if let Some(song) = self.server.library.row_song(index).cloned() {
            return self.play_now_many(vec![Song::from_open_subsonic(song)]);
        }
        let target = match self.server.library.detail.as_ref() {
            Some(ServerLibraryDetail::ArtistAlbums { albums, .. }) => albums
                .get(index)
                .map(|album| ServerLibraryDetailTarget::Album(album.id.clone())),
            Some(
                ServerLibraryDetail::AlbumSongs { .. } | ServerLibraryDetail::PlaylistEntries(_),
            ) => None,
            None => self
                .server
                .library
                .page
                .as_ref()
                .and_then(|page| page.rows.get(index))
                .and_then(|row| match row {
                    ServerLibraryRow::Song(_) => None,
                    ServerLibraryRow::Album(album) => {
                        Some(ServerLibraryDetailTarget::Album(album.id.clone()))
                    }
                    ServerLibraryRow::Artist(artist) => {
                        Some(ServerLibraryDetailTarget::Artist(artist.id.clone()))
                    }
                    ServerLibraryRow::Playlist(playlist) => {
                        Some(ServerLibraryDetailTarget::Playlist(playlist.id.clone()))
                    }
                }),
        };
        let Some(target) = target else {
            return Vec::new();
        };
        let generation = self.server.library.next_generation();
        self.server.library.busy = Some(ServerLibraryBusy::Detail);
        self.server.library.failure = None;
        self.dirty = true;
        vec![Cmd::ServerLibrary(ServerLibraryCommand::LoadDetail {
            generation,
            target,
        })]
    }

    pub(in crate::app) fn back_server_library(&mut self) -> Vec<Cmd> {
        if self.server.library.busy.is_some() {
            return Vec::new();
        }
        if self.server.library.detail.take().is_some() {
            self.server.library.selected = 0;
            self.bridges.library_scroll.reset();
            self.server.library.next_generation();
            self.dirty = true;
            return Vec::new();
        }
        if !self.server.library.previous_offsets.is_empty() {
            return self.previous_server_library_page();
        }
        self.select_library_source(LibrarySource::Yututui)
    }

    fn move_server_library_cursor(&mut self, up: bool, step: usize) {
        let len = self.server.library.rows_len();
        self.server.library.selected = if up {
            self.server.library.selected.saturating_sub(step)
        } else {
            self.server
                .library
                .selected
                .saturating_add(step)
                .min(len.saturating_sub(1))
        };
        self.dirty = true;
    }

    fn enqueue_selected_server_library_song(&mut self) -> Vec<Cmd> {
        self.server
            .library
            .row_song(self.server.library.selected)
            .cloned()
            .map(Song::from_open_subsonic)
            .map_or_else(Vec::new, |song| self.enqueue_many(vec![song]))
    }

    fn add_selected_server_library_song_to_playlist(&mut self) -> Vec<Cmd> {
        if let Some(song) = self
            .server
            .library
            .row_song(self.server.library.selected)
            .cloned()
            .map(Song::from_open_subsonic)
        {
            self.open_playlist_picker(vec![song]);
        }
        Vec::new()
    }

    fn handle_server_library_action(&mut self, action: Action) -> Option<Vec<Cmd>> {
        let commands = match action {
            Action::Back => {
                if self.server.library.detail.is_some()
                    || !self.server.library.previous_offsets.is_empty()
                {
                    self.back_server_library()
                } else {
                    self.mode = Mode::Player;
                    self.dirty = true;
                    Vec::new()
                }
            }
            Action::FocusNext => self.step_server_library_section(true),
            Action::FocusPrev => self.step_server_library_section(false),
            Action::MoveUp => {
                let step = self.nav_repeat_step(Action::MoveUp);
                self.move_server_library_cursor(true, step);
                Vec::new()
            }
            Action::MoveDown => {
                let step = self.nav_repeat_step(Action::MoveDown);
                self.move_server_library_cursor(false, step);
                Vec::new()
            }
            Action::PageUp => {
                self.move_server_library_cursor(true, self.page_step());
                Vec::new()
            }
            Action::PageDown => {
                self.move_server_library_cursor(false, self.page_step());
                Vec::new()
            }
            Action::JumpTop => {
                self.server.library.selected = 0;
                self.dirty = true;
                Vec::new()
            }
            Action::JumpBottom => {
                self.server.library.selected = self.server.library.rows_len().saturating_sub(1);
                self.dirty = true;
                Vec::new()
            }
            Action::Confirm => self.activate_server_library_row(
                self.server.library.generation,
                self.server.library.selected,
            ),
            Action::Enqueue => self.enqueue_selected_server_library_song(),
            Action::PlayAll => self.play_now_many(self.server.library.visible_songs()),
            Action::AddToPlaylist => self.add_selected_server_library_song_to_playlist(),
            _ => return None,
        };
        Some(commands)
    }

    pub(in crate::app) fn on_key_server_library(&mut self, key: KeyEvent) -> Vec<Cmd> {
        // Resolve the same semantic Library/Common actions as the local Library. Unsupported
        // local mutations (favorite, remove, filter, download) deliberately fall through to a
        // no-op on this read-only surface.
        let action = self.keymap.action(KeyContext::Library, Chord::from(key));
        if let Some(commands) = action.and_then(|action| self.handle_server_library_action(action))
        {
            return commands;
        }

        // Server-only navigation stays fixed and intentionally uses otherwise-unclaimed default
        // Library chords. Configured supported actions above win if a user assigns one of them.
        match key.code {
            KeyCode::Left if key.modifiers.is_empty() => {
                self.select_library_source(LibrarySource::Yututui)
            }
            KeyCode::Esc => self.back_server_library(),
            KeyCode::Char('1') if key.modifiers.is_empty() => {
                self.select_server_library_section(ServerLibrarySection::RecentlyPlayed)
            }
            KeyCode::Char('2') if key.modifiers.is_empty() => {
                self.select_server_library_section(ServerLibrarySection::Albums)
            }
            KeyCode::Char('3') if key.modifiers.is_empty() => {
                self.select_server_library_section(ServerLibrarySection::Artists)
            }
            KeyCode::Char('4') if key.modifiers.is_empty() => {
                self.select_server_library_section(ServerLibrarySection::Songs)
            }
            KeyCode::Char('5') if key.modifiers.is_empty() => {
                self.select_server_library_section(ServerLibrarySection::Playlists)
            }
            KeyCode::Char('[') if key.modifiers.is_empty() => self.previous_server_library_page(),
            KeyCode::Char(']') if key.modifiers.is_empty() => self.next_server_library_page(),
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn finish_server_library_event(
        &mut self,
        event: ServerLibraryEvent,
    ) -> Vec<Cmd> {
        let event_generation = match &event {
            ServerLibraryEvent::PageLoaded { generation, .. }
            | ServerLibraryEvent::DetailLoaded { generation, .. } => *generation,
        };
        if event_generation != self.server.library.generation
            || self.server.library.source != LibrarySource::OpenSubsonic
        {
            return Vec::new();
        }
        self.server.library.busy = None;
        match event {
            ServerLibraryEvent::PageLoaded { offset, result, .. } => match result {
                Ok(mut page) => {
                    page.rows.truncate(SERVER_LIBRARY_PAGE_LIMIT as usize);
                    if page.section != self.server.library.section {
                        return Vec::new();
                    }
                    self.server.library.offset = offset;
                    self.server.library.page = Some(page);
                    self.server.library.detail = None;
                    self.server.library.failure = None;
                    self.server.library.selected = 0;
                }
                Err(failure) => {
                    self.server.library.page = None;
                    self.server.library.failure = Some(failure);
                    self.status.kind = StatusKind::Error;
                    self.status.text = failure.label().to_owned();
                }
            },
            ServerLibraryEvent::DetailLoaded { result, .. } => match result {
                Ok(detail) => {
                    // Detail endpoints are already bounded to 20,000 rows by the catalog
                    // decoder. Retain the complete occurrence-preserving result so rows after
                    // the first page remain browsable and playable.
                    self.server.library.detail = Some(detail);
                    self.server.library.failure = None;
                    self.server.library.selected = 0;
                    self.bridges.library_scroll.reset();
                }
                Err(failure) => {
                    self.server.library.failure = Some(failure);
                    self.status.kind = StatusKind::Error;
                    self.status.text = failure.label().to_owned();
                }
            },
        }
        self.dirty = true;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::keymap::KeyMap;
    use crate::open_subsonic::model::{AccountScopeId, BackendId, ItemId, OpenSubsonicItemRef};

    fn song(id: &str) -> ServerSong {
        ServerSong {
            item: OpenSubsonicItemRef::new(
                BackendId::new("backend").unwrap(),
                AccountScopeId::new("account").unwrap(),
                ItemId::new(id).unwrap(),
            ),
            title: format!("Song {id}"),
            artist: "Artist".to_owned(),
            artists: vec!["Artist".to_owned()],
            album: None,
            album_id: None,
            album_artist: None,
            duration_secs: Some(125),
            track_number: None,
            disc_number: None,
            year: None,
            cover_art_id: None,
            content_type: None,
            suffix: None,
            starred: false,
            user_rating: None,
        }
    }

    fn server_song_page(ids: &[&str], next_offset: Option<u32>) -> ServerLibraryPage {
        ServerLibraryPage {
            section: ServerLibrarySection::Songs,
            rows: ids
                .iter()
                .map(|id| ServerLibraryRow::Song(song(id)))
                .collect(),
            next_offset,
            warning: None,
        }
    }

    fn server_library_app(ids: &[&str]) -> App {
        let mut app = App::new(50);
        app.mode = Mode::Library;
        app.server.library.source = LibrarySource::OpenSubsonic;
        app.server.library.section = ServerLibrarySection::Songs;
        app.server.library.generation = 7;
        app.server.library.page = Some(server_song_page(ids, None));
        app
    }

    #[test]
    fn stale_page_is_ignored_after_source_switch() {
        let mut app = App::new(50);
        app.server.library.source = LibrarySource::OpenSubsonic;
        let commands = app.request_server_library_page(0, false);
        assert_eq!(commands.len(), 1);
        let stale_generation = app.server.library.generation;
        app.select_library_source(LibrarySource::Yututui);
        app.finish_server_library_event(ServerLibraryEvent::PageLoaded {
            generation: stale_generation,
            offset: 0,
            result: Ok(ServerLibraryPage {
                section: ServerLibrarySection::RecentlyPlayed,
                rows: vec![ServerLibraryRow::Song(song("one"))],
                next_offset: None,
                warning: None,
            }),
        });
        assert!(app.server.library.page.is_none());
    }

    #[test]
    fn source_round_trip_clears_in_flight_busy_and_starts_a_fresh_request() {
        let mut app = App::new(50);
        app.server.library.source = LibrarySource::OpenSubsonic;
        let commands = app.request_server_library_page(0, false);
        assert_eq!(commands.len(), 1);
        let stale_generation = app.server.library.generation;
        assert_eq!(app.server.library.busy, Some(ServerLibraryBusy::Page));

        assert!(app.select_library_source(LibrarySource::Yututui).is_empty());
        assert!(app.server.library.busy.is_none());
        let fresh = app.select_library_source(LibrarySource::OpenSubsonic);
        assert_eq!(fresh.len(), 1);
        assert!(app.server.library.generation > stale_generation);

        app.finish_server_library_event(ServerLibraryEvent::PageLoaded {
            generation: stale_generation,
            offset: 0,
            result: Ok(ServerLibraryPage {
                section: ServerLibrarySection::RecentlyPlayed,
                rows: vec![ServerLibraryRow::Song(song("stale"))],
                next_offset: None,
                warning: None,
            }),
        });
        assert!(app.server.library.page.is_none());
        assert_eq!(app.server.library.busy, Some(ServerLibraryBusy::Page));
    }

    #[test]
    fn page_results_are_bounded_again_on_the_owner_lane() {
        let mut app = App::new(50);
        app.server.library.source = LibrarySource::OpenSubsonic;
        let generation = app.server.library.next_generation();
        app.server.library.busy = Some(ServerLibraryBusy::Page);
        app.finish_server_library_event(ServerLibraryEvent::PageLoaded {
            generation,
            offset: 0,
            result: Ok(ServerLibraryPage {
                section: ServerLibrarySection::RecentlyPlayed,
                rows: (0..80)
                    .map(|index| ServerLibraryRow::Song(song(&index.to_string())))
                    .collect(),
                next_offset: Some(80),
                warning: None,
            }),
        });
        assert_eq!(
            app.server.library.page.as_ref().unwrap().rows.len(),
            SERVER_LIBRARY_PAGE_LIMIT as usize
        );
    }

    #[test]
    fn detail_keeps_rows_and_duplicate_occurrences_after_the_first_page() {
        let mut app = App::new(50);
        app.server.library.source = LibrarySource::OpenSubsonic;
        let generation = app.server.library.next_generation();
        app.server.library.busy = Some(ServerLibraryBusy::Detail);
        let mut entries = (0..60)
            .map(|index| song(&format!("song-{index}")))
            .collect::<Vec<_>>();
        entries[50] = entries[49].clone();
        app.finish_server_library_event(ServerLibraryEvent::DetailLoaded {
            generation,
            result: Ok(ServerLibraryDetail::PlaylistEntries(
                crate::open_subsonic::ServerPlaylist {
                    summary: crate::open_subsonic::ServerPlaylistSummary {
                        id: ServerPlaylistId::new("playlist").unwrap(),
                        name: "Long playlist".to_owned(),
                        owner: None,
                        song_count: Some(60),
                        duration_secs: None,
                        public: None,
                        cover_art_id: None,
                    },
                    entries,
                },
            )),
        });

        assert_eq!(app.server.library.rows_len(), 60);
        assert_eq!(
            app.server.library.row_song(49).unwrap().item,
            app.server.library.row_song(50).unwrap().item
        );
        assert!(app.server.library.row_song(59).is_some());
    }

    #[test]
    fn server_errors_do_not_change_local_library() {
        let mut app = App::new(50);
        let favorites_before = app.library.favorites.clone();
        let history_before = app.library.history.clone();
        app.server.library.source = LibrarySource::OpenSubsonic;
        let generation = app.server.library.next_generation();
        app.server.library.busy = Some(ServerLibraryBusy::Page);
        app.finish_server_library_event(ServerLibraryEvent::PageLoaded {
            generation,
            offset: 0,
            result: Err(ServerLibraryFailure::Offline),
        });
        assert_eq!(app.library.favorites, favorites_before);
        assert_eq!(app.library.history, history_before);
        assert_eq!(
            app.server.library.failure,
            Some(ServerLibraryFailure::Offline)
        );
    }

    #[test]
    fn failed_section_load_cannot_expose_or_activate_previous_rows() {
        let mut app = App::new(50);
        app.server.library.source = LibrarySource::OpenSubsonic;
        app.server.library.section = ServerLibrarySection::Songs;
        app.server.library.page = Some(ServerLibraryPage {
            section: ServerLibrarySection::Songs,
            rows: vec![ServerLibraryRow::Song(song("old"))],
            next_offset: None,
            warning: None,
        });

        let commands = app.select_server_library_section(ServerLibrarySection::Albums);
        assert_eq!(commands.len(), 1);
        let generation = app.server.library.generation;
        assert!(app.server.library.page.is_none());
        assert!(app.server.library.row_song(0).is_none());
        assert!(app.activate_server_library_row(generation, 0).is_empty());

        app.finish_server_library_event(ServerLibraryEvent::PageLoaded {
            generation,
            offset: 0,
            result: Err(ServerLibraryFailure::Offline),
        });
        assert!(app.server.library.page.is_none());
        assert_eq!(app.server.library.rows_len(), 0);
        assert!(app.activate_server_library_row(generation, 0).is_empty());
    }

    #[test]
    fn page_history_keeps_latest_hundred_and_back_does_not_skip() {
        let mut app = App::new(50);
        app.server.library.source = LibrarySource::OpenSubsonic;
        app.server.library.section = ServerLibrarySection::Songs;
        app.server.library.page = Some(ServerLibraryPage {
            section: ServerLibrarySection::Songs,
            rows: vec![ServerLibraryRow::Song(song("page-0"))],
            next_offset: Some(SERVER_LIBRARY_PAGE_LIMIT),
            warning: None,
        });

        for page_number in 1..=101 {
            let expected_offset = page_number * SERVER_LIBRARY_PAGE_LIMIT;
            let commands = app.next_server_library_page();
            assert_eq!(commands.len(), 1);
            assert!(matches!(
                &commands[0],
                Cmd::ServerLibrary(ServerLibraryCommand::LoadPage { offset, .. })
                    if *offset == expected_offset
            ));
            let generation = app.server.library.generation;
            app.finish_server_library_event(ServerLibraryEvent::PageLoaded {
                generation,
                offset: expected_offset,
                result: Ok(ServerLibraryPage {
                    section: ServerLibrarySection::Songs,
                    rows: vec![ServerLibraryRow::Song(song(&format!("page-{page_number}")))],
                    next_offset: Some(expected_offset + SERVER_LIBRARY_PAGE_LIMIT),
                    warning: None,
                }),
            });
        }

        assert_eq!(
            app.server.library.previous_offsets.len(),
            SERVER_LIBRARY_MAX_PAGE_HISTORY
        );
        assert_eq!(
            app.server.library.previous_offsets.first(),
            Some(&SERVER_LIBRARY_PAGE_LIMIT)
        );
        assert_eq!(
            app.server.library.previous_offsets.last(),
            Some(&(100 * SERVER_LIBRARY_PAGE_LIMIT))
        );

        let commands = app.previous_server_library_page();
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            &commands[0],
            Cmd::ServerLibrary(ServerLibraryCommand::LoadPage { offset, .. })
                if *offset == 100 * SERVER_LIBRARY_PAGE_LIMIT
        ));
    }

    #[test]
    fn switching_to_server_ends_hidden_local_filter_editing() {
        let mut app = App::new(50);
        app.mode = Mode::Library;
        app.library_ui.filter_editing = true;
        app.library_ui.filter_query = "kept for local".to_owned();

        let commands = app.select_library_source(LibrarySource::OpenSubsonic);

        assert_eq!(commands.len(), 1);
        assert!(!app.library_ui.filter_editing);
        assert_eq!(app.library_ui.filter_query, "kept for local");
        assert_eq!(app.server.library.source, LibrarySource::OpenSubsonic);
    }

    #[test]
    fn configured_library_and_common_actions_drive_server_rows() {
        let mut overrides = BTreeMap::new();
        overrides.insert("common.move_down".to_owned(), "j".to_owned());
        overrides.insert("common.focus_next".to_owned(), "x".to_owned());
        overrides.insert("library.enqueue".to_owned(), "e".to_owned());
        overrides.insert("library.add_to_playlist".to_owned(), "t".to_owned());
        overrides.insert("library.back".to_owned(), "b".to_owned());
        let mut app = server_library_app(&["one", "two"]);
        app.keymap = KeyMap::from_overrides(&overrides);

        assert!(
            app.on_key_server_library(KeyEvent::from(KeyCode::Char('j')))
                .is_empty()
        );
        assert_eq!(app.server.library.selected, 1);

        assert!(
            !app.on_key_server_library(KeyEvent::from(KeyCode::Char('e')))
                .is_empty(),
            "the remapped enqueue action should reach the selected server song"
        );

        app.on_key_server_library(KeyEvent::from(KeyCode::Char('t')));
        assert_eq!(
            app.playlist_picker.as_ref().expect("playlist picker").songs[0].title,
            "Song two"
        );
        app.playlist_picker = None;

        app.server.library.busy = None;
        app.on_key_server_library(KeyEvent::from(KeyCode::Char('x')));
        assert_eq!(app.server.library.section, ServerLibrarySection::Playlists);

        app.server.library.busy = None;
        app.server.library.detail = None;
        app.server.library.previous_offsets.clear();
        app.on_key_server_library(KeyEvent::from(KeyCode::Char('b')));
        assert_eq!(app.mode, Mode::Player);
    }

    #[test]
    fn default_add_to_playlist_no_longer_collides_with_page_navigation() {
        let mut app = server_library_app(&["one"]);
        app.server.library.page.as_mut().unwrap().next_offset = Some(SERVER_LIBRARY_PAGE_LIMIT);

        assert!(
            app.on_key_server_library(KeyEvent::from(KeyCode::Char('p')))
                .is_empty()
        );
        assert!(app.playlist_picker.is_some());
        assert_eq!(app.server.library.offset, 0);
        assert!(app.server.library.busy.is_none());

        app.playlist_picker = None;
        let commands = app.on_key_server_library(KeyEvent::from(KeyCode::Char(']')));
        assert!(matches!(
            commands.as_slice(),
            [Cmd::ServerLibrary(ServerLibraryCommand::LoadPage { offset, .. })]
                if *offset == SERVER_LIBRARY_PAGE_LIMIT
        ));
    }

    #[test]
    fn fixed_section_numbers_reach_every_server_section() {
        let cases = [
            ('1', ServerLibrarySection::RecentlyPlayed),
            ('2', ServerLibrarySection::Albums),
            ('3', ServerLibrarySection::Artists),
            ('4', ServerLibrarySection::Songs),
            ('5', ServerLibrarySection::Playlists),
        ];
        for (key, expected) in cases {
            let mut app = server_library_app(&["one"]);
            app.on_key_server_library(KeyEvent::from(KeyCode::Char(key)));
            assert_eq!(app.server.library.section, expected);
        }

        let mut app = server_library_app(&["one"]);
        app.on_key_server_library(KeyEvent::new(
            KeyCode::Char('1'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(
            app.server.library.section,
            ServerLibrarySection::Songs,
            "modified digits are not fixed server shortcuts"
        );
    }
}
