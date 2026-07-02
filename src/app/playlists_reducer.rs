//! Playlists-tab reducer methods: the root list of local playlists, the opened-playlist
//! drill-down, the create-playlist popup, and the delete-playlist confirmation.
//!
//! The tab has two levels sharing the Library cursor/scroll state: the *root* lists the
//! playlists themselves (`open_playlist == None`); opening one drills down into its songs,
//! which then flow through the ordinary Library row paths (play / enqueue / favorite /
//! download / remove). Every mutation persists immediately via [`Cmd::SavePlaylists`] —
//! the transfer engine writes `playlists.json` directly and the app reloads it on job
//! completion, so unsaved in-memory edits would be silently dropped.

use super::*;
use crate::playlists::Playlist;

impl App {
    /// The tab actually shown, honoring the radio-mode fallback (mirrors `library_rows`).
    pub fn effective_library_tab(&self) -> LibraryTab {
        if self.library_tab_available(self.library_ui.tab) {
            self.library_ui.tab
        } else {
            self.library_tabs()[0]
        }
    }

    /// Whether the Playlists tab's *root* level (the list of playlists) is showing.
    pub fn playlists_root(&self) -> bool {
        self.effective_library_tab() == LibraryTab::Playlists
            && self.library_ui.open_playlist.is_none()
    }

    /// Root-level rows: playlists narrowed by the in-library filter (name match, mirroring
    /// `apply_library_filter`'s case-insensitive substring semantics).
    pub fn filtered_playlists(&self) -> Vec<&Playlist> {
        let needle = self.library_ui.filter_query.trim().to_lowercase();
        self.playlists
            .list()
            .iter()
            .filter(|p| needle.is_empty() || p.name.to_lowercase().contains(&needle))
            .collect()
    }

    /// The playlist under the root-level cursor, if any.
    fn selected_root_playlist(&self) -> Option<&Playlist> {
        self.filtered_playlists()
            .get(self.library_ui.selected)
            .copied()
    }

    /// Enter the drill-down for the playlist under the cursor: its songs become the
    /// Library rows. Clears the filter (like a tab switch) and resets cursor/scroll.
    pub(in crate::app) fn open_selected_playlist(&mut self) -> Vec<Cmd> {
        let Some((id, songs)) = self
            .selected_root_playlist()
            .map(|p| (p.id.clone(), p.songs.clone()))
        else {
            return Vec::new();
        };
        self.clear_library_filter();
        self.library_ui.open_playlist = Some(id);
        self.dirty = true;
        self.request_romanization_for_songs(&songs)
    }

    /// Leave the drill-down back to the playlist list, restoring the cursor to the row of
    /// the playlist that was just closed.
    pub(in crate::app) fn close_open_playlist(&mut self) {
        let Some(id) = self.library_ui.open_playlist.take() else {
            return;
        };
        self.clear_library_filter();
        if let Some(pos) = self.playlists.list().iter().position(|p| p.id == id) {
            self.library_ui.selected = pos;
            self.library_ui.anchor = pos;
        }
        self.dirty = true;
    }

    /// Play the playlist under the root cursor as a fresh queue (mirrors
    /// `Msg::AiPlayPlaylist`).
    pub(in crate::app) fn play_selected_playlist(&mut self) -> Vec<Cmd> {
        let Some(songs) = self.selected_root_playlist().map(|p| p.songs.clone()) else {
            return Vec::new();
        };
        if songs.is_empty() {
            self.status.kind = StatusKind::Info;
            self.status.text = t!("Playlist is empty", "플레이리스트가 비어 있어요").to_string();
            self.dirty = true;
            return Vec::new();
        }
        let requested_songs = songs.clone();
        self.queue.set(songs, 0);
        self.mode = Mode::Player;
        self.status.text.clear();
        let song = self.queue.current().cloned();
        let mut cmds = self.load_song(song);
        cmds.extend(self.request_romanization_for_songs(&requested_songs));
        cmds
    }

    /// Append the playlist under the root cursor to the queue without interrupting playback.
    pub(in crate::app) fn enqueue_selected_playlist(&mut self) -> Vec<Cmd> {
        let Some(songs) = self.selected_root_playlist().map(|p| p.songs.clone()) else {
            return Vec::new();
        };
        if songs.is_empty() {
            self.status.kind = StatusKind::Info;
            self.status.text = t!("Playlist is empty", "플레이리스트가 비어 있어요").to_string();
            self.dirty = true;
            return Vec::new();
        }
        self.enqueue_many(songs)
    }

    /// Ask before deleting the playlist at root row `row` (removes the whole list at once).
    pub(in crate::app) fn request_playlist_delete(&mut self, row: usize) {
        let id = self.filtered_playlists().get(row).map(|p| p.id.clone());
        if let Some(id) = id {
            self.library_ui.confirm_playlist_delete = Some(id);
            self.dirty = true;
        }
    }

    /// Carry out a confirmed playlist deletion. Tolerates the playlist having vanished
    /// under the modal (e.g. a finished import job reloaded the store).
    pub(in crate::app) fn confirm_playlist_delete_apply(&mut self) -> Vec<Cmd> {
        let Some(key) = self.library_ui.confirm_playlist_delete.take() else {
            return Vec::new();
        };
        self.dirty = true;
        let Some(removed) = self.playlists.delete(&key) else {
            return Vec::new();
        };
        if self.library_ui.open_playlist.as_deref() == Some(key.as_str()) {
            self.library_ui.open_playlist = None;
        }
        self.clamp_library_selection();
        self.status.kind = StatusKind::Info;
        self.status.text = format!(
            "{}: {}",
            t!("Deleted playlist", "플레이리스트 삭제"),
            removed.name
        );
        vec![Cmd::SavePlaylists]
    }

    /// Open the create-playlist popup with an empty name buffer.
    pub(in crate::app) fn open_playlist_create(&mut self) {
        self.library_ui.create_input = Some(String::new());
        self.dirty = true;
    }

    /// Keystrokes while the create-playlist popup is open: type/Backspace edit the name,
    /// Enter creates, Esc cancels. Mirrors the library filter's plain-char gate.
    pub(in crate::app) fn on_key_playlist_create(&mut self, k: KeyEvent) -> Vec<Cmd> {
        match k.code {
            KeyCode::Esc => {
                self.library_ui.create_input = None;
                self.dirty = true;
            }
            KeyCode::Enter => return self.playlist_create_commit(),
            KeyCode::Backspace => {
                if let Some(buf) = self.library_ui.create_input.as_mut() {
                    buf.pop();
                    self.dirty = true;
                }
            }
            KeyCode::Char(c)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(buf) = self.library_ui.create_input.as_mut()
                    && buf.chars().count() < PLAYLIST_NAME_MAX
                {
                    buf.push(c);
                    self.dirty = true;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Commit the create-playlist popup: create, select the new row (root level), persist.
    pub(in crate::app) fn playlist_create_commit(&mut self) -> Vec<Cmd> {
        let name = self
            .library_ui
            .create_input
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if name.is_empty() {
            self.status.kind = StatusKind::Info;
            self.status.text =
                t!("Enter a playlist name", "플레이리스트 이름을 입력하세요").to_string();
            self.dirty = true;
            return Vec::new();
        }
        self.dirty = true;
        match self.playlists.create(&name) {
            Some(id) => {
                self.library_ui.create_input = None;
                self.status.kind = StatusKind::Info;
                self.status.text =
                    format!("{}: {name}", t!("Created playlist", "플레이리스트 생성"));
                // Land the cursor on the new playlist so Enter/`a` act on it right away.
                if self.playlists_root() {
                    self.clear_library_filter();
                    if let Some(pos) = self.playlists.list().iter().position(|p| p.id == id) {
                        self.library_ui.selected = pos;
                        self.library_ui.anchor = pos;
                    }
                }
                vec![Cmd::SavePlaylists]
            }
            // Blank is pre-checked above, so `None` here means the playlist cap.
            None => {
                self.status.kind = StatusKind::Error;
                self.status.text =
                    t!("Playlists are full", "플레이리스트가 가득 찼어요").to_string();
                Vec::new()
            }
        }
    }

    /// Drop every Playlists-tab surface (drill-down, create popup, delete confirm) — used
    /// when leaving the Library or switching tabs so stale state can't act on the wrong list.
    pub(in crate::app) fn reset_playlist_ui_state(&mut self) {
        self.library_ui.open_playlist = None;
        self.library_ui.create_input = None;
        self.library_ui.confirm_playlist_delete = None;
    }

    /// A transient onboarding nudge shown on entering the Playlists tab: the live chord
    /// bound to [`Action::PlaylistCreate`] (so a rebind updates the message in lock-step),
    /// riding the shared status line, which auto-expires after `STATUS_TTL`.
    pub(in crate::app) fn hint_playlist_create(&mut self) {
        let key = self.keymap.label_for_display(
            KeyContext::Playlists,
            Action::PlaylistCreate,
            self.retro_mode(),
        );
        self.status.kind = StatusKind::Info;
        self.status.text = if crate::i18n::is_korean() {
            format!("{key} 키로 새 플레이리스트를 만들어 보세요")
        } else {
            format!("Press {key} to create a new playlist")
        };
        self.dirty = true;
    }

    /// After an external rewrite of the playlists store (a finished transfer job), drop a
    /// drill-down or pending delete whose playlist no longer exists and re-clamp the cursor.
    pub(in crate::app) fn reconcile_playlists_reload(&mut self) {
        if let Some(id) = self.library_ui.open_playlist.as_deref()
            && self.playlists.find(id).is_none()
        {
            self.library_ui.open_playlist = None;
        }
        if let Some(id) = self.library_ui.confirm_playlist_delete.as_deref()
            && self.playlists.find(id).is_none()
        {
            self.library_ui.confirm_playlist_delete = None;
        }
        self.clamp_library_selection();
        self.bridges.library_scroll.reset();
    }

    // --- "Add to playlist" picker ------------------------------------------------

    /// Open the picker over the current screen for `songs` (a library selection, one
    /// search result, or the current track). No-op when there's nothing to add.
    pub(in crate::app) fn open_playlist_picker(&mut self, songs: Vec<Song>) {
        if songs.is_empty() {
            return;
        }
        self.playlist_picker = Some(PlaylistPicker {
            songs,
            cursor: 0,
            naming: None,
        });
        self.dirty = true;
    }

    /// Keystrokes while the picker is open. List phase: ↑/↓ move, Enter chooses (the
    /// trailing row switches to name entry), `n` jumps straight to name entry, Esc/`q`
    /// close. Naming phase: type/Backspace edit, Enter creates-and-adds, Esc backs out
    /// to the list.
    pub(in crate::app) fn on_key_playlist_picker(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let Some(picker) = self.playlist_picker.as_mut() else {
            return Vec::new();
        };
        if picker.naming.is_some() {
            match k.code {
                KeyCode::Esc => {
                    picker.naming = None;
                    self.dirty = true;
                }
                KeyCode::Enter => return self.picker_create_commit(),
                KeyCode::Backspace => {
                    if let Some(buf) = picker.naming.as_mut() {
                        buf.pop();
                        self.dirty = true;
                    }
                }
                KeyCode::Char(c)
                    if !k
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if let Some(buf) = picker.naming.as_mut()
                        && buf.chars().count() < PLAYLIST_NAME_MAX
                    {
                        buf.push(c);
                        self.dirty = true;
                    }
                }
                _ => {}
            }
            return Vec::new();
        }
        let last = self.playlists.list().len(); // the "New playlist…" row
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.playlist_picker = None;
                self.dirty = true;
            }
            KeyCode::Up => {
                picker.cursor = picker.cursor.saturating_sub(1);
                self.dirty = true;
            }
            KeyCode::Down => {
                picker.cursor = (picker.cursor + 1).min(last);
                self.dirty = true;
            }
            KeyCode::Char('n') => {
                picker.naming = Some(String::new());
                self.dirty = true;
            }
            KeyCode::Enter => {
                let row = picker.cursor;
                return self.picker_choose(row);
            }
            _ => {}
        }
        Vec::new()
    }

    /// Act on picker row `row`: add the pending songs to that playlist, or open the
    /// inline name entry when it's the trailing "New playlist…" row.
    pub(in crate::app) fn picker_choose(&mut self, row: usize) -> Vec<Cmd> {
        let Some(picker) = self.playlist_picker.as_mut() else {
            return Vec::new();
        };
        if row >= self.playlists.playlists.len() {
            picker.naming = Some(String::new());
            picker.cursor = self.playlists.playlists.len();
            self.dirty = true;
            return Vec::new();
        }
        let Some(picker) = self.playlist_picker.take() else {
            return Vec::new();
        };
        let key = self.playlists.playlists[row].id.clone();
        self.picker_add_songs(&key, picker.songs)
    }

    /// Commit the picker's inline name entry: create the playlist, then add the songs.
    pub(in crate::app) fn picker_create_commit(&mut self) -> Vec<Cmd> {
        let Some(name) = self
            .playlist_picker
            .as_ref()
            .and_then(|p| p.naming.as_deref())
        else {
            return Vec::new();
        };
        let name = name.trim().to_owned();
        self.dirty = true;
        if name.is_empty() {
            self.status.kind = StatusKind::Info;
            self.status.text =
                t!("Enter a playlist name", "플레이리스트 이름을 입력하세요").to_string();
            return Vec::new();
        }
        match self.playlists.create(&name) {
            Some(id) => {
                let songs = self
                    .playlist_picker
                    .take()
                    .map_or_else(Vec::new, |p| p.songs);
                self.picker_add_songs(&id, songs)
            }
            // Blank is pre-checked above, so `None` here means the playlist cap.
            None => {
                self.status.kind = StatusKind::Error;
                self.status.text =
                    t!("Playlists are full", "플레이리스트가 가득 찼어요").to_string();
                Vec::new()
            }
        }
    }

    /// Add `songs` to the playlist at `key` and report the outcome in the status line.
    /// Saves when anything was actually added (idempotent under duplicates).
    fn picker_add_songs(&mut self, key: &str, songs: Vec<Song>) -> Vec<Cmd> {
        let name = self
            .playlists
            .find(key)
            .map_or_else(|| key.to_owned(), |p| p.name.clone());
        let (mut added, mut dupes, mut full) = (0usize, 0usize, 0usize);
        for song in songs {
            match self.playlists.add(key, song) {
                crate::playlists::AddResult::Added => added += 1,
                crate::playlists::AddResult::Duplicate => dupes += 1,
                crate::playlists::AddResult::Full => full += 1,
                crate::playlists::AddResult::NotFound => {}
            }
        }
        self.dirty = true;
        if full > 0 && added == 0 {
            self.status.kind = StatusKind::Error;
            self.status.text = t!("Playlist is full", "플레이리스트가 가득 찼어요").to_string();
            return Vec::new();
        }
        if added == 0 {
            self.status.kind = StatusKind::Info;
            self.status.text = format!(
                "{}: {name}",
                t!("Already in playlist", "이미 플레이리스트에 있어요")
            );
            return Vec::new();
        }
        self.status.kind = StatusKind::Info;
        self.status.text = if crate::i18n::is_korean() {
            if dupes > 0 {
                format!("{name}에 {added}곡 추가 ({dupes}곡은 이미 있음)")
            } else {
                format!("{name}에 {added}곡 추가")
            }
        } else {
            let noun = if added == 1 { "track" } else { "tracks" };
            if dupes > 0 {
                format!("Added {added} {noun} to {name} ({dupes} already there)")
            } else {
                format!("Added {added} {noun} to {name}")
            }
        };
        vec![Cmd::SavePlaylists]
    }
}

/// Create-popup name length bound (bounded memory; far beyond any sensible name).
const PLAYLIST_NAME_MAX: usize = 100;
