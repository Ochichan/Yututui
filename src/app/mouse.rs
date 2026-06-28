//! Mouse/region reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    pub fn clear_mouse_regions(&self) {
        self.seekbar_rect.set(None);
        self.mouse_buttons.borrow_mut().clear();
    }

    pub fn register_mouse_button(&self, rect: Rect, target: MouseTarget) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.mouse_buttons
            .borrow_mut()
            .push(MouseButtonRegion { rect, target });
    }

    pub(in crate::app) fn mouse_target_at(&self, col: u16, row: u16) -> Option<MouseTarget> {
        self.mouse_buttons
            .borrow()
            .iter()
            .rev()
            .find(|b| rect_contains(b.rect, col, row))
            .map(|b| b.target)
    }

    /// A left-click at `(col, row)`: buttons fire their mapped action; the player's
    /// seekbar seeks to the matching fraction of the track. Hit rects are published by
    /// views each render.
    pub(in crate::app) fn on_mouse_click(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        // A click dismisses the modal conflict warning, same as a keypress.
        if self.key_conflict.take().is_some() {
            self.dirty = true;
            return Vec::new();
        }
        // A click cancels the reset-all confirmation (never confirms — that needs Enter/`y`).
        if self.confirm_reset_all {
            self.confirm_reset_all = false;
            self.dirty = true;
            return Vec::new();
        }
        // The download-delete confirmation is modal: only its own Delete/Cancel buttons act;
        // a click anywhere else backs out without touching any files.
        if self.library_ui.confirm_delete.is_some() {
            match self.mouse_target_at(col, row) {
                Some(t @ (MouseTarget::ConfirmDelete | MouseTarget::CancelDelete)) => {
                    return self.on_mouse_target(t);
                }
                _ => {
                    self.library_ui.confirm_delete = None;
                    self.dirty = true;
                    return Vec::new();
                }
            }
        }
        if self.help_visible {
            self.help_visible = false;
            self.dirty = true;
            return Vec::new();
        }
        // The About card is modal: clicking its GitHub link opens the browser (and keeps the card
        // up); a click anywhere else dismisses it.
        if self.about_visible {
            if let Some(t @ MouseTarget::AboutLink) = self.mouse_target_at(col, row) {
                return self.on_mouse_target(t);
            }
            self.about_visible = false;
            self.dirty = true;
            return Vec::new();
        }
        // The queue window is modal: a click outside it closes it ("창 밖을 클릭하면 꺼지고"),
        // and inside it only its own rows / `✗` buttons act — underlying player buttons are
        // ignored so a click landing on the player beneath the popup doesn't leak through.
        if self.queue_popup_open {
            let inside = self.queue_popup_rect.get().is_some_and(|r| rect_contains(r, col, row));
            if !inside {
                self.queue_popup_open = false;
                self.dirty = true;
                return Vec::new();
            }
            match self.mouse_target_at(col, row) {
                Some(t @ (MouseTarget::QueueRow(_) | MouseTarget::QueueDel(_))) => {
                    return self.on_mouse_target(t);
                }
                _ => return Vec::new(),
            }
        }
        if let Some(target) = self.mouse_target_at(col, row) {
            return self.on_mouse_target(target);
        }
        // A click that missed every button dismisses an open status-line dropdown (modal-style),
        // so the same click doesn't also seek.
        if self.eq_dropdown_open || self.radio_dropdown_open {
            self.eq_dropdown_open = false;
            self.radio_dropdown_open = false;
            self.dirty = true;
            return Vec::new();
        }
        if self.mode != Mode::Player {
            return Vec::new();
        }
        if let Some(area) = self.seekbar_rect.get()
            && let Some(dur) = self.playback.duration
            && dur > 0.0
            && area.width > 0
            && rect_contains(area, col, row)
        {
            let frac = f64::from(col - area.x) / f64::from(area.width);
            let target = (frac * dur).clamp(0.0, dur);
            tracing::info!(secs = target, "click seek");
            self.dirty = true;
            return vec![Cmd::Player(PlayerCmd::SeekAbsolute(target))];
        }
        Vec::new()
    }

    pub(in crate::app) fn on_mouse_target(&mut self, target: MouseTarget) -> Vec<Cmd> {
        match target {
            MouseTarget::Global(Action::ToggleHelp) => {
                self.help_visible = true;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::Global(_) => Vec::new(),
            MouseTarget::Player(action) if self.mode == Mode::Player => {
                self.on_player_action(action)
            }
            MouseTarget::Player(_) => Vec::new(),
            // Toggle the EQ dropdown by clicking its `eq:` label (closes the radio one).
            MouseTarget::EqMenu if self.mode == Mode::Player => {
                self.radio_dropdown_open = false;
                self.eq_dropdown_open = !self.eq_dropdown_open;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::EqMenu => Vec::new(),
            // Pick a preset from the open dropdown.
            MouseTarget::EqSelect(preset) if self.mode == Mode::Player => {
                self.select_eq_preset(preset)
            }
            MouseTarget::EqSelect(_) => Vec::new(),
            // Toggle the radio-mode dropdown by clicking its `radio:` label (closes the EQ one).
            MouseTarget::RadioMenu if self.mode == Mode::Player => {
                self.eq_dropdown_open = false;
                self.radio_dropdown_open = !self.radio_dropdown_open;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::RadioMenu => Vec::new(),
            // Pick a radio mode from the open dropdown.
            MouseTarget::RadioSelect(mode) if self.mode == Mode::Player => {
                self.select_radio_mode(mode)
            }
            MouseTarget::RadioSelect(_) => Vec::new(),
            // Nav bar: switch screens from anywhere.
            MouseTarget::Nav(mode) => self.navigate_to(mode),
            // Search bar submit button.
            MouseTarget::SearchSubmit if self.mode == Mode::Search => self.submit_search_query(),
            MouseTarget::SearchSubmit => Vec::new(),
            // Library tab header.
            MouseTarget::LibraryTab(tab) if self.mode == Mode::Library => {
                self.library_ui.tab = tab;
                self.library_ui.selected = 0;
                self.library_scroll.reset();
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::LibraryTab(_) => Vec::new(),
            // Settings tab header.
            MouseTarget::SettingsTab(i) if self.mode == Mode::Settings => {
                self.settings_select_tab(i);
                Vec::new()
            }
            MouseTarget::SettingsTab(_) => Vec::new(),
            // Click a checkbox or `<`/`>` arrow: focus that row, then nudge it like ←/→.
            MouseTarget::SettingsChange { row, delta } if self.mode == Mode::Settings => {
                self.settings_focus_row(row);
                self.settings_change(delta)
            }
            MouseTarget::SettingsChange { .. } => Vec::new(),
            // Click a button or text value: focus that row, then activate it like Enter.
            MouseTarget::SettingsActivate(row) if self.mode == Mode::Settings => {
                self.settings_focus_row(row);
                self.settings_activate()
            }
            MouseTarget::SettingsActivate(_) => Vec::new(),
            // Single-click a list row: select it (double-click plays — see double-click path).
            MouseTarget::ListRow(i) => self.on_list_row_click(i),
            // Open the queue window from the `N/M` position label.
            MouseTarget::QueuePos if self.mode == Mode::Player => {
                self.open_queue_popup();
                Vec::new()
            }
            MouseTarget::QueuePos => Vec::new(),
            // Single-click a queue row: select it (and anchor a drag range here).
            MouseTarget::QueueRow(i) if self.queue_popup_open => {
                self.queue_popup_cursor = i;
                self.queue_popup_anchor = i;
                self.dirty = true;
                Vec::new()
            }
            MouseTarget::QueueRow(_) => Vec::new(),
            // The `✗` button on a queue row removes just that track.
            MouseTarget::QueueDel(i) if self.queue_popup_open => self.remove_queue_range(i, i),
            MouseTarget::QueueDel(_) => Vec::new(),
            // The `✗` button on a library row removes just that track (per-tab semantics).
            MouseTarget::LibraryDel(i) if self.mode == Mode::Library => {
                self.library_delete_rows(i, i)
            }
            MouseTarget::LibraryDel(_) => Vec::new(),
            // The "delete downloaded files" confirmation buttons.
            MouseTarget::ConfirmDelete => self.confirm_delete_files_apply(),
            MouseTarget::CancelDelete => {
                self.library_ui.confirm_delete = None;
                self.dirty = true;
                Vec::new()
            }
            // Click the `ytm-tui` brand to open the About card.
            MouseTarget::AboutTitle => {
                self.about_visible = true;
                self.dirty = true;
                Vec::new()
            }
            // Click the GitHub link inside the About card to open the repo in the browser.
            MouseTarget::AboutLink => {
                open_in_browser(crate::ui::views::about::GITHUB_URL);
                self.status = t!("Opening GitHub in your browser…", "브라우저에서 GitHub을 여는 중…").to_owned();
                self.dirty = true;
                Vec::new()
            }
        }
    }

    /// A left double-click: play a song/queue row (vs. single-click, which selects). Falls
    /// back to single-click behavior anywhere else so buttons, tabs, and the seekbar still
    /// respond to the first press of a double-click.
    pub(in crate::app) fn on_mouse_double_click(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        // Modal overlays treat a double-click like a single click.
        if self.help_visible
            || self.about_visible
            || self.key_conflict.is_some()
            || self.confirm_reset_all
            || self.library_ui.confirm_delete.is_some()
        {
            return self.on_mouse_click(col, row);
        }
        if self.queue_popup_open {
            let inside = self.queue_popup_rect.get().is_some_and(|r| rect_contains(r, col, row));
            if inside {
                if let Some(MouseTarget::QueueRow(i)) = self.mouse_target_at(col, row) {
                    return self.queue_popup_play(i);
                }
                return Vec::new();
            }
            return self.on_mouse_click(col, row); // outside -> close, same as single click
        }
        match self.mouse_target_at(col, row) {
            Some(MouseTarget::ListRow(i)) => self.on_list_row_activate(i),
            _ => self.on_mouse_click(col, row),
        }
    }

    /// A left-drag: extend a multi-select range to the row under the pointer (the anchor end
    /// stays fixed). Works in the queue window and, identically, in the Library list.
    pub(in crate::app) fn on_mouse_drag(&mut self, col: u16, row: u16) -> Vec<Cmd> {
        if self.queue_popup_open {
            if let Some(MouseTarget::QueueRow(i) | MouseTarget::QueueDel(i)) =
                self.mouse_target_at(col, row)
                && self.queue_popup_cursor != i
            {
                self.queue_popup_cursor = i;
                self.dirty = true;
            }
            return Vec::new();
        }
        if self.mode == Mode::Library
            && let Some(MouseTarget::ListRow(i) | MouseTarget::LibraryDel(i)) =
                self.mouse_target_at(col, row)
            && self.library_ui.selected != i
        {
            self.library_ui.selected = i;
            self.dirty = true;
        }
        Vec::new()
    }

    /// Wheel scroll moves the *viewport* of whichever list is on top by
    /// [`MOUSE_SCROLL_LINES`] rows — decoupled from the selection, which stays put (it may
    /// scroll out of view; the render pass keeps it visible only for keyboard nav). An open
    /// overlay (the queue window) wins over the active screen.
    pub(in crate::app) fn on_mouse_scroll(&mut self, up: bool) -> Vec<Cmd> {
        let n = MOUSE_SCROLL_LINES;
        if self.queue_popup_open {
            self.queue_popup_scroll.wheel(up, n, self.queue.len());
            self.dirty = true;
            return Vec::new();
        }
        match self.mode {
            Mode::Library => {
                self.library_scroll.wheel(up, n, self.library_len());
                self.dirty = true;
            }
            Mode::Search => {
                self.search_scroll.wheel(up, n, self.search.results.len());
                self.dirty = true;
            }
            Mode::Ai => {
                self.ai_scroll.wheel(up, n, self.ai.suggestions.len());
                self.dirty = true;
            }
            // Settings is an interactive form, not a browse list, so the wheel keeps walking
            // the focused field (which the render then keeps on-screen with a margin).
            Mode::Settings => {
                let delta = if up { -1 } else { 1 } * n as i32;
                self.settings_move_row(delta);
            }
            _ => {}
        }
        Vec::new()
    }

    /// Single-click select on the active screen's list. `index` is the logical item index
    /// the view published (song index, or a Settings row index).
    pub(in crate::app) fn on_list_row_click(&mut self, index: usize) -> Vec<Cmd> {
        match self.mode {
            Mode::Search if index < self.search.results.len() => {
                self.search.selected = index;
                self.search.focus = SearchFocus::Results;
                self.dirty = true;
            }
            Mode::Library if index < self.library_len() => {
                self.library_ui.selected = index;
                // A fresh single click re-anchors the multi-select range here.
                self.library_ui.anchor = index;
                self.dirty = true;
            }
            Mode::Settings => {
                if let Some(st) = self.settings.as_mut() {
                    st.row = index;
                    st.editing_text = false;
                    self.dirty = true;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Double-click activate on the active screen's list: play the song (Search/Library);
    /// Settings rows have no "play", so a double-click just selects.
    pub(in crate::app) fn on_list_row_activate(&mut self, index: usize) -> Vec<Cmd> {
        match self.mode {
            Mode::Search if index < self.search.results.len() => {
                self.search.selected = index;
                self.play_selected()
            }
            Mode::Library if index < self.library_len() => {
                self.library_ui.selected = index;
                self.play_from_library()
            }
            _ => self.on_list_row_click(index),
        }
    }
}
