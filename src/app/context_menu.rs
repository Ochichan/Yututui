//! Row-scoped context menus and configurable right-button gestures.
//!
//! A terminal can consume a right click before the PTY ever sees it; once crossterm delivers
//! the gesture, however, this module keeps the interaction inside the TUI.  Menu commands carry
//! an identity snapshot of the row(s) that opened them, so an asynchronous list refresh cannot
//! redirect a delayed click to a different song.

use super::*;
use crate::mousemap::{MouseAction, MouseContext, MouseGesture};

/// A visible context menu. The render pass publishes its clamped terminal rectangle through
/// `rect`, while the reducer owns the selected item and immutable target snapshot.
pub struct ContextMenuState {
    pub(crate) anchor_col: u16,
    pub(crate) anchor_row: u16,
    pub(crate) selected: usize,
    pub(crate) items: Vec<ContextMenuItem>,
    pub(crate) rect: Cell<Option<Rect>>,
    target: ContextTarget,
}

impl ContextMenuState {
    fn new(
        anchor_col: u16,
        anchor_row: u16,
        target: ContextTarget,
        items: Vec<ContextMenuItem>,
    ) -> Self {
        Self {
            anchor_col,
            anchor_row,
            selected: 0,
            items,
            rect: Cell::new(None),
            target,
        }
    }

    pub(crate) fn target_count(&self) -> usize {
        self.target.count()
    }
}

#[derive(Clone)]
enum ContextTarget {
    Search {
        /// Ascending result rows the menu acts on — the clicked row, or the whole
        /// effective multi-selection when the click landed inside it.
        rows: Vec<usize>,
        /// Identity snapshot parallel to `rows`.
        video_ids: Vec<String>,
        filter_row: Option<usize>,
    },
    LibrarySongs {
        /// Ascending library rows the menu acts on — the clicked row, or the whole
        /// effective (range or Ctrl/Cmd-picked) selection when clicked inside it.
        rows: Vec<usize>,
        video_ids: Vec<String>,
        tab: LibraryTab,
        open_playlist: Option<String>,
    },
    LibraryPlaylist {
        index: usize,
        playlist_id: String,
    },
    Queue {
        lo: usize,
        hi: usize,
        revision: u64,
        video_ids: Vec<String>,
    },
    Local {
        index: usize,
        row: crate::local::LocalRowId,
        song_ids: Vec<(String, Option<PathBuf>)>,
        download_ids: Vec<(String, Option<PathBuf>)>,
    },
}

impl ContextTarget {
    const fn mouse_context(&self) -> MouseContext {
        match self {
            Self::Search { .. } => MouseContext::Search,
            Self::LibrarySongs { .. } | Self::LibraryPlaylist { .. } => MouseContext::Library,
            Self::Queue { .. } => MouseContext::Queue,
            Self::Local { .. } => MouseContext::Local,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ContextCommand {
    Activate,
    PlayNow,
    PlayFromHere,
    Enqueue,
    ToggleFavorite,
    AddToPlaylist,
    Download,
    ImportPlaylist,
    OpenPlaylist,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContextMenuItem {
    command: ContextCommand,
}

impl ContextMenuItem {
    const fn new(command: ContextCommand) -> Self {
        Self { command }
    }

    pub(crate) fn label(self, count: usize) -> String {
        match self.command {
            ContextCommand::Activate => t!("Activate", "실행").to_owned(),
            ContextCommand::PlayNow if count > 1 => {
                format!("{} ({count})", t!("Play selected", "선택 항목 재생"))
            }
            ContextCommand::PlayNow => t!("Play now", "지금 재생").to_owned(),
            ContextCommand::PlayFromHere => t!("Play from here", "여기서부터 재생").to_owned(),
            ContextCommand::Enqueue if count > 1 => {
                format!(
                    "{} ({count})",
                    t!("Add selected to queue", "선택 항목 큐에 추가")
                )
            }
            ContextCommand::Enqueue => t!("Add to queue", "대기열에 추가").to_owned(),
            ContextCommand::ToggleFavorite => {
                t!("Favorite / unfavorite", "즐겨찾기 추가 / 해제").to_owned()
            }
            ContextCommand::AddToPlaylist if count > 1 => {
                format!(
                    "{} ({count})",
                    t!("Add selected to playlist", "선택 항목 플레이리스트에 추가")
                )
            }
            ContextCommand::AddToPlaylist => {
                t!("Add to playlist", "플레이리스트에 추가").to_owned()
            }
            ContextCommand::Download if count > 1 => {
                format!(
                    "{} ({count})",
                    t!("Download selected", "선택 항목 다운로드")
                )
            }
            ContextCommand::Download => t!("Download", "다운로드").to_owned(),
            ContextCommand::ImportPlaylist => {
                t!("Import playlist", "플레이리스트 가져오기").to_owned()
            }
            ContextCommand::OpenPlaylist => t!("Open playlist", "플레이리스트 열기").to_owned(),
            ContextCommand::Remove if count > 1 => {
                format!("{} ({count})", t!("Remove selected", "선택 항목 제거"))
            }
            ContextCommand::Remove => t!("Remove", "제거").to_owned(),
        }
    }
}

impl ContextTarget {
    fn count(&self) -> usize {
        match self {
            Self::LibrarySongs { video_ids, .. }
            | Self::Queue { video_ids, .. }
            | Self::Search { video_ids, .. } => video_ids.len(),
            _ => 1,
        }
    }
}

impl App {
    /// Whether another modal owns input. The queue and search-filter popups are deliberately
    /// excluded: their semantic rows are valid context-menu targets.
    fn context_menu_blocked(&self) -> bool {
        self.overlays.help_visible
            || self.overlays.mouse_help_visible
            || self.overlays.about_visible
            || self.overlays.why_ai_visible
            || self.overlays.now_playing_overlay.is_some()
            || self.overlays.key_conflict.is_some()
            || self.overlays.pending_settings_confirm.is_some()
            || self.overlays.spotify_picker.is_some()
            || self.overlays.recordings_browser.is_some()
            || self.overlays.recording_settings.is_some()
            || self.radio_mode.pending_radio_mode_confirm.is_some()
            || self.local_mode.pending_confirm.is_some()
            || self.local_import_confirmation_open()
            || self.library_ui.confirm_delete.is_some()
            || self.library_ui.confirm_download.is_some()
            || self.library_ui.confirm_playlist_delete.is_some()
            || self.library_ui.create_input.is_some()
            || self.playlist_picker.is_some()
            || self.dropdowns.eq_open
            || self.dropdowns.streaming_open
            || self.dropdowns.search_source_open
    }

    /// A right press either opens the in-TUI menu or runs the configured safe direct action.
    pub(in crate::app) fn on_mouse_right_click(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        if let Some(menu) = self.overlays.context_menu.as_ref()
            && menu.anchor_col == col
            && menu.anchor_row == row
        {
            let action = self
                .mousemap
                .action(menu.target.mouse_context(), MouseGesture::RightClick);
            if matches!(action, MouseAction::ContextMenu | MouseAction::Disabled) {
                return Vec::new();
            }
            let target = menu.target.clone();
            self.overlays.context_menu = None;
            self.dirty = true;
            return self.execute_mouse_action(target, action);
        }
        if self.overlays.context_menu.take().is_some() {
            self.dirty = true;
        }
        if self.context_menu_blocked() {
            return Vec::new();
        }
        let Some(target) = self.context_target_at(col, row) else {
            return Vec::new();
        };
        let action = self
            .mousemap
            .action(target.mouse_context(), MouseGesture::RightClick);
        match action {
            MouseAction::ContextMenu => self.open_context_target_menu(col, row, target),
            MouseAction::Activate | MouseAction::Enqueue => {
                self.execute_mouse_action(target, action)
            }
            MouseAction::Disabled => Vec::new(),
        }
    }

    /// The second press of a right double-click must act on the original row, not the menu item
    /// now covering that cell. If single-click is configured as a direct action no menu exists,
    /// so the still-current underlying hit map supplies the snapshot instead.
    pub(in crate::app) fn on_mouse_right_double_click(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        if let Some(menu) = self.overlays.context_menu.as_ref()
            && menu.anchor_col == col
            && menu.anchor_row == row
        {
            let action = self
                .mousemap
                .action(menu.target.mouse_context(), MouseGesture::RightDoubleClick);
            if action == MouseAction::Disabled {
                return Vec::new();
            }
            let target = menu.target.clone();
            self.overlays.context_menu = None;
            self.dirty = true;
            return self.execute_mouse_action(target, action);
        }
        let target = match self.overlays.context_menu.take() {
            Some(menu) if menu.anchor_col == col && menu.anchor_row == row => Some(menu.target),
            Some(_) => None,
            None if !self.context_menu_blocked() => self.context_target_at(col, row),
            None => None,
        };
        self.dirty = true;
        let Some(target) = target else {
            return Vec::new();
        };
        let action = self
            .mousemap
            .action(target.mouse_context(), MouseGesture::RightDoubleClick);
        match action {
            MouseAction::Activate | MouseAction::Enqueue => {
                self.execute_mouse_action(target, action)
            }
            MouseAction::ContextMenu | MouseAction::Disabled => Vec::new(),
        }
    }

    /// Keyboard accessibility fallback (normally Shift+F10): anchor a menu beside the selected
    /// visible row. Hidden/off-screen rows intentionally do nothing because there is no honest
    /// place to attach a spatial menu.
    pub(in crate::app) fn open_context_menu_for_keyboard(&mut self) -> Vec<Cmd> {
        if self.context_menu_blocked() {
            return Vec::new();
        }
        let target = if self.search_filter.open {
            MouseTarget::SearchFilterRow(self.search_filter.cursor)
        } else if self.queue_popup.open {
            MouseTarget::QueueRow(self.queue_popup.cursor)
        } else {
            match self.mode {
                Mode::Search if self.search.focus == SearchFocus::Results => {
                    MouseTarget::ListRow(self.search.selected)
                }
                Mode::Library if self.local_dedicated_mode => {
                    MouseTarget::LocalRow(self.local_mode.ui.selected)
                }
                Mode::Library => MouseTarget::ListRow(self.library_ui.selected),
                _ => return Vec::new(),
            }
        };
        let Some(rect) = self.hits.rect_of_target(target) else {
            self.status.kind = StatusKind::Info;
            self.status.text =
                t!("Selected row is off-screen", "선택한 행이 화면 밖에 있어요").to_owned();
            self.dirty = true;
            return Vec::new();
        };
        let col = rect.x.saturating_add(1);
        let row = rect.y;
        let Some(target) = self.context_target_at(col, row) else {
            return Vec::new();
        };
        self.open_context_target_menu(col, row, target)
    }

    /// While open, the menu consumes every key. Navigation follows the remappable Common
    /// bindings; Enter/Space activate and Esc/Back close.
    pub(in crate::app) fn on_key_context_menu(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let chord = Chord::from(k);
        if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
            return self.quit_app();
        }
        if k.code == KeyCode::Esc
            || matches!(
                self.keymap.action(KeyContext::Common, chord),
                Some(Action::Back)
            )
            || matches!(
                self.keymap.global_action(chord),
                Some(Action::OpenContextMenu)
            )
        {
            self.overlays.context_menu = None;
            self.dirty = true;
            return Vec::new();
        }

        let common = self.keymap.action(KeyContext::Common, chord);
        let Some(menu) = self.overlays.context_menu.as_mut() else {
            return Vec::new();
        };
        let last = menu.items.len().saturating_sub(1);
        match common {
            Some(Action::MoveUp) => menu.selected = menu.selected.saturating_sub(1),
            Some(Action::MoveDown) => menu.selected = menu.selected.saturating_add(1).min(last),
            Some(Action::JumpTop) => menu.selected = 0,
            Some(Action::JumpBottom) => menu.selected = last,
            Some(Action::Confirm) if !menu.items.is_empty() => {
                let index = menu.selected;
                return self.activate_context_menu_item(index);
            }
            _ if k.code == KeyCode::Char(' ') && !menu.items.is_empty() => {
                let index = menu.selected;
                return self.activate_context_menu_item(index);
            }
            _ => return Vec::new(),
        }
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn activate_context_menu_item(&mut self, index: usize) -> Vec<Cmd> {
        let Some(menu) = self.overlays.context_menu.take() else {
            return Vec::new();
        };
        let Some(item) = menu.items.get(index).copied() else {
            self.dirty = true;
            return Vec::new();
        };
        self.dirty = true;
        self.execute_context_command(menu.target, item.command)
    }

    pub(in crate::app) fn move_context_menu_selection(&mut self, up: bool) {
        let Some(menu) = self.overlays.context_menu.as_mut() else {
            return;
        };
        let last = menu.items.len().saturating_sub(1);
        menu.selected = if up {
            menu.selected.saturating_sub(1)
        } else {
            menu.selected.saturating_add(1).min(last)
        };
        self.dirty = true;
    }

    fn context_target_at(&mut self, col: u16, row: u16) -> Option<ContextTarget> {
        if self.search_filter.open {
            let inside = self
                .search_filter
                .rect
                .get()
                .is_some_and(|rect| rect_contains(rect, col, row));
            if !inside {
                return None;
            }
            let MouseTarget::SearchFilterRow(filter_row) = self.mouse_target_at(col, row)? else {
                return None;
            };
            let &index = self.search_filter.matches.get(filter_row)?;
            let song = self.search.results.get(index)?;
            return Some(ContextTarget::Search {
                rows: vec![index],
                video_ids: vec![song.video_id.clone()],
                filter_row: Some(filter_row),
            });
        }

        if self.queue_popup.open {
            let inside = self
                .queue_popup
                .rect
                .get()
                .is_some_and(|rect| rect_contains(rect, col, row));
            if !inside {
                return None;
            }
            let index = match self.mouse_target_at(col, row)? {
                MouseTarget::QueueRow(index) | MouseTarget::QueueDel(index) => index,
                _ => return None,
            };
            if index >= self.queue.len() {
                return None;
            }
            let current_lo = self.queue_popup.cursor.min(self.queue_popup.anchor);
            let current_hi = self.queue_popup.cursor.max(self.queue_popup.anchor);
            let (lo, hi) = if current_lo <= index && index <= current_hi {
                (current_lo, current_hi)
            } else {
                (index, index)
            };
            let video_ids = self
                .queue
                .ordered_iter()
                .skip(lo)
                .take(hi - lo + 1)
                .map(|song| song.video_id.clone())
                .collect();
            return Some(ContextTarget::Queue {
                lo,
                hi,
                revision: self.queue.rev(),
                video_ids,
            });
        }

        let target = self.mouse_target_at(col, row)?;
        match self.mode {
            Mode::Search => {
                let MouseTarget::ListRow(index) = target else {
                    return None;
                };
                let song = self.search.results.get(index)?;
                // A click inside the current multi-selection targets the whole selection;
                // playlist rows keep their dedicated single-row menu, and any playlist
                // rows inside the selection are dropped here so the "(N)" labels count
                // exactly the songs the commands will act on.
                let selection = self.search_selection_indices();
                let rows = if selection.len() > 1
                    && selection.contains(&index)
                    && song.youtube_playlist_id().is_none()
                {
                    let songs_only: Vec<usize> = selection
                        .into_iter()
                        .filter(|&i| {
                            self.search
                                .results
                                .get(i)
                                .is_some_and(|s| s.youtube_playlist_id().is_none())
                        })
                        .collect();
                    if songs_only.len() > 1 {
                        songs_only
                    } else {
                        vec![index]
                    }
                } else {
                    vec![index]
                };
                let video_ids = rows
                    .iter()
                    .filter_map(|&i| self.search.results.get(i))
                    .map(|s| s.video_id.clone())
                    .collect();
                Some(ContextTarget::Search {
                    rows,
                    video_ids,
                    filter_row: None,
                })
            }
            Mode::Library if self.local_dedicated_mode => {
                let MouseTarget::LocalRow(index) = target else {
                    return None;
                };
                let row = self.local_visible_rows().get(index)?.clone();
                let song_ids = Self::context_song_ids(self.local_songs_for_row(&row));
                let download_ids =
                    Self::context_song_ids(self.local_downloadable_songs_for_row(&row));
                Some(ContextTarget::Local {
                    index,
                    row,
                    song_ids,
                    download_ids,
                })
            }
            Mode::Library => {
                let index = match target {
                    MouseTarget::ListRow(index) | MouseTarget::LibraryDel(index) => index,
                    _ => return None,
                };
                if self.playlists_root() {
                    let playlist_id = self.filtered_playlists().get(index)?.id.clone();
                    return Some(ContextTarget::LibraryPlaylist { index, playlist_id });
                }
                let songs = self.library_songs();
                if index >= songs.len() {
                    return None;
                }
                // A click inside the current effective selection (drag range or
                // Ctrl/Cmd-picked rows) targets the whole selection.
                let selection = self.library_selection_indices();
                let rows = if selection.contains(&index) {
                    selection
                } else {
                    vec![index]
                };
                let video_ids = rows
                    .iter()
                    .filter_map(|&i| songs.get(i))
                    .map(|song| song.video_id.clone())
                    .collect();
                Some(ContextTarget::LibrarySongs {
                    rows,
                    video_ids,
                    tab: self.effective_library_tab(),
                    open_playlist: self.library_ui.open_playlist.clone(),
                })
            }
            _ => None,
        }
    }

    /// Make the target visible as the current selection when a menu actually opens. A Disabled
    /// binding never calls this, so disabling a gesture also removes its selection side effect.
    fn select_context_target(&mut self, target: &ContextTarget) {
        match target {
            ContextTarget::Search {
                rows, filter_row, ..
            } => {
                // A click inside the current selection may target a filtered subset
                // (song rows only, or a playlist row). Leave the selection untouched
                // so cancelling the menu changes nothing.
                if let Some((&first, &last)) = rows.first().zip(rows.last())
                    && {
                        let selection = self.search_selection_indices();
                        *rows != selection && !rows.iter().all(|row| selection.contains(row))
                    }
                {
                    if rows.len() > 1 {
                        self.search.picked = rows.iter().copied().collect();
                    } else {
                        self.search.picked.clear();
                    }
                    self.search.selected = last;
                    self.search.anchor = first;
                }
                self.search.focus = SearchFocus::Results;
                if let Some(row) = filter_row {
                    self.search_filter.cursor = *row;
                }
            }
            ContextTarget::LibrarySongs { rows, .. } => {
                if let Some((&first, &last)) = rows.first().zip(rows.last())
                    && *rows != self.library_selection_indices()
                {
                    if rows.len() > 1 {
                        self.library_ui.picked = rows.iter().copied().collect();
                    } else {
                        self.library_ui.picked.clear();
                    }
                    self.library_ui.selected = last;
                    self.library_ui.anchor = first;
                }
            }
            ContextTarget::LibraryPlaylist { index, .. } => {
                self.library_ui.selected = *index;
                self.library_ui.anchor = *index;
            }
            ContextTarget::Queue { lo, hi, .. } => {
                let current_lo = self.queue_popup.cursor.min(self.queue_popup.anchor);
                let current_hi = self.queue_popup.cursor.max(self.queue_popup.anchor);
                if (current_lo, current_hi) != (*lo, *hi) {
                    self.queue_popup.cursor = *hi;
                    self.queue_popup.anchor = *lo;
                }
            }
            ContextTarget::Local { index, .. } => {
                self.local_mode.ui.selected = *index;
                self.local_mode.ui.anchor = *index;
            }
        }
        self.dirty = true;
    }

    fn open_context_target_menu(&mut self, col: u16, row: u16, target: ContextTarget) -> Vec<Cmd> {
        let items = self.context_items(&target);
        if !items.is_empty() {
            self.select_context_target(&target);
            self.overlays.context_menu = Some(ContextMenuState::new(col, row, target, items));
            self.dirty = true;
        }
        Vec::new()
    }

    fn context_song_ids(songs: Vec<Song>) -> Vec<(String, Option<PathBuf>)> {
        songs
            .into_iter()
            .map(|song| (song.video_id, song.local_path))
            .collect()
    }

    fn context_items(&self, target: &ContextTarget) -> Vec<ContextMenuItem> {
        use ContextCommand as C;
        let commands: Vec<C> = match target {
            // A multi-selection only ever holds song rows (playlist rows keep their
            // dedicated single-row menu), so the bulk commands apply directly.
            ContextTarget::Search { rows, .. } if rows.len() > 1 => {
                vec![C::PlayNow, C::Enqueue, C::AddToPlaylist, C::Download]
            }
            ContextTarget::Search {
                rows, video_ids, ..
            } => match rows.first().and_then(|&i| self.search.results.get(i)) {
                Some(song)
                    if video_ids.first() == Some(&song.video_id)
                        && song.youtube_playlist_id().is_some() =>
                {
                    vec![C::PlayNow, C::Enqueue, C::ImportPlaylist]
                }
                Some(song) if video_ids.first() == Some(&song.video_id) => vec![
                    C::PlayNow,
                    C::Enqueue,
                    C::ToggleFavorite,
                    C::AddToPlaylist,
                    C::Download,
                ],
                _ => Vec::new(),
            },
            ContextTarget::LibrarySongs { tab, .. } => {
                let mut commands = vec![C::PlayNow, C::Enqueue];
                if target.count() == 1 {
                    commands.push(C::ToggleFavorite);
                }
                commands.extend([C::AddToPlaylist, C::Download]);
                if *tab != LibraryTab::All {
                    commands.push(C::Remove);
                }
                commands
            }
            ContextTarget::LibraryPlaylist { .. } => vec![
                C::OpenPlaylist,
                C::PlayNow,
                C::Enqueue,
                C::Download,
                C::Remove,
            ],
            ContextTarget::Queue { .. } => vec![C::PlayFromHere, C::Remove],
            ContextTarget::Local {
                row,
                song_ids,
                download_ids,
                ..
            } => {
                if matches!(row, crate::local::LocalRowId::ScanError(_)) {
                    return Vec::new();
                }
                let mut commands = vec![C::Activate];
                if !song_ids.is_empty() {
                    commands.push(C::Enqueue);
                }
                if !download_ids.is_empty() {
                    commands.push(C::Download);
                }
                commands
            }
        };
        commands.into_iter().map(ContextMenuItem::new).collect()
    }

    fn execute_mouse_action(&mut self, target: ContextTarget, action: MouseAction) -> Vec<Cmd> {
        let command = match action {
            MouseAction::Activate => ContextCommand::Activate,
            MouseAction::Enqueue => ContextCommand::Enqueue,
            MouseAction::ContextMenu | MouseAction::Disabled => return Vec::new(),
        };
        self.execute_context_command(target, command)
    }

    fn execute_context_command(
        &mut self,
        target: ContextTarget,
        command: ContextCommand,
    ) -> Vec<Cmd> {
        match target {
            ContextTarget::Search {
                rows,
                video_ids,
                filter_row,
            } => {
                let Some((&first, &last)) = rows.first().zip(rows.last()) else {
                    return self.context_target_stale();
                };
                let valid_filter = filter_row.is_none_or(|row| {
                    self.search_filter.open
                        && self.search_filter.matches.get(row).copied() == Some(first)
                });
                // Identity snapshot: every targeted row must still hold the song it did
                // when the menu opened.
                let songs: Vec<Song> = rows
                    .iter()
                    .filter_map(|&i| self.search.results.get(i).cloned())
                    .collect();
                if songs.len() != rows.len()
                    || !songs
                        .iter()
                        .map(|s| s.video_id.as_str())
                        .eq(video_ids.iter().map(String::as_str))
                    || !valid_filter
                {
                    return self.context_target_stale();
                }
                if songs.len() > 1 {
                    self.search.picked = rows.iter().copied().collect();
                    self.search.selected = last;
                    self.search.anchor = first;
                    self.search.focus = SearchFocus::Results;
                    self.dirty = true;
                    // Multi targets are built from song rows only; keep the filter as a
                    // guard against a hypothetically stale playlist row sneaking in.
                    let songs: Vec<Song> = songs
                        .into_iter()
                        .filter(|s| s.youtube_playlist_id().is_none())
                        .collect();
                    if songs.is_empty() {
                        return Vec::new();
                    }
                    return match command {
                        ContextCommand::Activate | ContextCommand::PlayNow => {
                            self.play_now_many(songs)
                        }
                        ContextCommand::Enqueue => self.enqueue_many(songs),
                        ContextCommand::AddToPlaylist => {
                            self.open_playlist_picker(songs);
                            Vec::new()
                        }
                        ContextCommand::Download => self.open_confirm_download(songs),
                        _ => Vec::new(),
                    };
                }
                let index = first;
                let Some(song) = songs.into_iter().next() else {
                    return self.context_target_stale();
                };
                self.search.selected = index;
                self.collapse_search_selection();
                self.search.focus = SearchFocus::Results;
                self.dirty = true;
                match command {
                    ContextCommand::Activate | ContextCommand::PlayNow => {
                        if filter_row.is_some() {
                            self.search_filter.close();
                        }
                        self.activate_search_index(index)
                    }
                    ContextCommand::Enqueue => match song.youtube_playlist_id() {
                        Some(_) => {
                            self.fetch_playlist_tracks(&song, crate::api::PlaylistIntent::Enqueue)
                        }
                        None => self.enqueue(song),
                    },
                    ContextCommand::ToggleFavorite if song.youtube_playlist_id().is_none() => {
                        self.library.toggle_favorite(&song);
                        vec![Cmd::Persist(PersistCmd::Library)]
                    }
                    ContextCommand::AddToPlaylist if song.youtube_playlist_id().is_none() => {
                        self.open_playlist_picker(vec![song]);
                        Vec::new()
                    }
                    ContextCommand::Download if song.youtube_playlist_id().is_none() => {
                        self.start_download(song)
                    }
                    ContextCommand::ImportPlaylist if song.youtube_playlist_id().is_some() => {
                        self.fetch_playlist_tracks(&song, crate::api::PlaylistIntent::Import)
                    }
                    _ => Vec::new(),
                }
            }
            ContextTarget::LibrarySongs {
                rows,
                video_ids,
                tab,
                open_playlist,
            } => {
                if self.effective_library_tab() != tab
                    || self.library_ui.open_playlist != open_playlist
                {
                    return self.context_target_stale();
                }
                let Some((&first, &last)) = rows.first().zip(rows.last()) else {
                    return self.context_target_stale();
                };
                let songs = self.library_songs();
                let selected: Vec<Song> =
                    rows.iter().filter_map(|&i| songs.get(i).cloned()).collect();
                if selected.len() != rows.len()
                    || !selected
                        .iter()
                        .map(|song| song.video_id.as_str())
                        .eq(video_ids.iter().map(String::as_str))
                {
                    return self.context_target_stale();
                }
                if rows.len() > 1 {
                    self.library_ui.picked = rows.iter().copied().collect();
                } else {
                    self.library_ui.picked.clear();
                }
                self.library_ui.selected = last;
                self.library_ui.anchor = first;
                self.dirty = true;
                match command {
                    ContextCommand::Activate | ContextCommand::PlayNow => {
                        self.play_now_many(selected)
                    }
                    ContextCommand::Enqueue => self.enqueue_many(selected),
                    ContextCommand::ToggleFavorite if selected.len() == 1 => {
                        let rows_before = self.library_len();
                        self.library.toggle_favorite(&selected[0]);
                        // Un-favoriting can remove the row (Favorites/All tab): re-clamp
                        // and drop the now-stale picks; unchanged tabs keep the selection.
                        if self.library_len() != rows_before {
                            self.clamp_library_selection();
                        }
                        vec![Cmd::Persist(PersistCmd::Library)]
                    }
                    ContextCommand::AddToPlaylist => {
                        self.open_playlist_picker(selected);
                        Vec::new()
                    }
                    ContextCommand::Download => match selected.as_slice() {
                        [song] => self.start_download(song.clone()),
                        _ => self.open_confirm_download(selected),
                    },
                    ContextCommand::Remove if tab != LibraryTab::All => {
                        self.library_delete_selection()
                    }
                    _ => Vec::new(),
                }
            }
            ContextTarget::LibraryPlaylist { index, playlist_id } => {
                if !self.playlists_root()
                    || self
                        .filtered_playlists()
                        .get(index)
                        .is_none_or(|playlist| playlist.id != playlist_id)
                {
                    return self.context_target_stale();
                }
                self.library_ui.selected = index;
                self.library_ui.anchor = index;
                self.dirty = true;
                match command {
                    ContextCommand::Activate | ContextCommand::OpenPlaylist => {
                        self.open_selected_playlist()
                    }
                    ContextCommand::PlayNow => self.play_selected_playlist(),
                    ContextCommand::Enqueue => self.enqueue_selected_playlist(),
                    ContextCommand::Download => {
                        let songs = self
                            .selected_root_playlist()
                            .map(|playlist| playlist.songs.clone())
                            .unwrap_or_default();
                        self.open_confirm_download(songs)
                    }
                    ContextCommand::Remove => {
                        self.request_playlist_delete(index);
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            ContextTarget::Queue {
                lo,
                hi,
                revision,
                video_ids,
            } => {
                let ids_match = self
                    .queue
                    .ordered_iter()
                    .skip(lo)
                    .take(hi - lo + 1)
                    .map(|song| song.video_id.as_str())
                    .eq(video_ids.iter().map(String::as_str));
                if self.queue.rev() != revision || !ids_match {
                    return self.context_target_stale();
                }
                self.queue_popup.cursor = hi;
                self.queue_popup.anchor = lo;
                self.dirty = true;
                match command {
                    ContextCommand::Activate
                    | ContextCommand::PlayNow
                    | ContextCommand::PlayFromHere => self.queue_popup_play(lo),
                    ContextCommand::Remove => self.remove_queue_range(lo, hi),
                    _ => Vec::new(),
                }
            }
            ContextTarget::Local {
                index,
                row,
                song_ids,
                download_ids,
            } => {
                let current_song_ids = Self::context_song_ids(self.local_songs_for_row(&row));
                let current_download_ids =
                    Self::context_song_ids(self.local_downloadable_songs_for_row(&row));
                if self.local_visible_rows().get(index) != Some(&row)
                    || current_song_ids != song_ids
                    || current_download_ids != download_ids
                {
                    return self.context_target_stale();
                }
                self.local_mode.ui.selected = index;
                self.local_mode.ui.anchor = index;
                self.dirty = true;
                match command {
                    ContextCommand::Activate | ContextCommand::PlayNow => {
                        self.local_row_activate(index)
                    }
                    ContextCommand::Enqueue => self.local_enqueue_row_index(index),
                    ContextCommand::Download => self.local_download_selected(),
                    _ => Vec::new(),
                }
            }
        }
    }

    fn context_target_stale(&mut self) -> Vec<Cmd> {
        self.status.kind = StatusKind::Info;
        self.status.text = t!(
            "The list changed — open the menu again",
            "목록이 바뀌었어요 — 메뉴를 다시 열어 주세요"
        )
        .to_owned();
        self.dirty = true;
        Vec::new()
    }
}
