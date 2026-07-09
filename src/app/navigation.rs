//! Cross-screen navigation helpers.

use super::*;

impl App {
    /// The single footer hint shown across every view: just the live chord that opens the
    /// `?` cheat-sheet (which already lists every binding for every screen). Built from the
    /// keymap so remapping "toggle help" updates the hint in lock-step.
    pub fn help_footer(&self) -> String {
        format!(
            "{}  keybindings",
            self.keymap.label_for_display(
                KeyContext::Global,
                Action::ToggleHelp,
                self.retro_mode()
            )
        )
    }

    /// Return to the player/home screen from any mode. Settings use the normal close path
    /// so draft values and keybinding changes are not silently discarded.
    pub(in crate::app) fn go_home(&mut self) -> Vec<Cmd> {
        self.overlays.help_visible = false;
        self.overlays.mouse_help_visible = false;
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.queue_popup.open = false;
        self.search_filter.close();
        self.library_ui.confirm_delete = None;
        self.library_ui.confirm_download = None;
        self.local_mode.pending_organize_confirm = None;
        self.playlist_picker = None;
        // These three render as top-level overlays but route input only inside Settings-mode
        // dispatch, so leaving the screen must drop them explicitly or they'd paint on top of
        // the Player, unreachable. (`spotify_picker` shares the same shape.)
        self.overlays.recording_settings = None;
        self.overlays.recordings_browser = None;
        self.overlays.spotify_picker = None;
        self.reset_playlist_ui_state();
        // Leaving the screen drops any pending text selection so it can't reappear highlighted
        // when the input is re-entered later.
        self.search.select_all = false;
        self.ai.select_all = false;
        if self.mode == Mode::Settings {
            self.finish_settings_text_edit();
            return self.close_settings();
        }
        self.mode = Mode::Player;
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn quit_app(&mut self) -> Vec<Cmd> {
        self.overlays.help_visible = false;
        self.overlays.mouse_help_visible = false;
        let cmds = if self.mode == Mode::Settings {
            self.finish_settings_text_edit();
            self.close_settings()
        } else {
            Vec::new()
        };
        self.should_quit = true;
        cmds
    }

    /// How many rows a PageUp/PageDown moves: a screenful of the active list less one row
    /// of context overlap. Falls back to [`DEFAULT_PAGE_ROWS`] before the first render
    /// records the viewport height.
    pub(in crate::app) fn page_step(&self) -> usize {
        let rows = self.bridges.list_viewport_rows.get() as usize;
        if rows <= 1 {
            DEFAULT_PAGE_ROWS
        } else {
            rows - 1
        }
    }

    /// How many rows a single `MoveUp`/`MoveDown`-style step should advance, given how long
    /// the key has been held. Ramps up the longer the same direction repeats so holding an
    /// arrow flies through a long list while a tap still moves exactly one row. See
    /// [`NavRepeat`]; the timing core is split into [`Self::nav_repeat_step_at`] for tests.
    pub(in crate::app) fn nav_repeat_step(&mut self, action: Action) -> usize {
        self.nav_repeat_step_at(Instant::now(), action)
    }

    /// Timing core of [`Self::nav_repeat_step`], split out so tests can supply a clock.
    /// Consecutive same-direction events within [`NAV_REPEAT_GAP`] extend the hold; a gap or
    /// a direction change restarts it. (The OS initial-repeat delay exceeds the gap, so the
    /// ramp naturally begins once the fast auto-repeat stream kicks in.)
    pub(in crate::app) fn nav_repeat_step_at(&mut self, now: Instant, action: Action) -> usize {
        let held_on = self.interaction.nav_repeat.action == Some(action)
            && self
                .interaction
                .nav_repeat
                .last
                .is_some_and(|t| now.duration_since(t) <= NAV_REPEAT_GAP);
        if !held_on {
            self.interaction.nav_repeat.started = Some(now);
        }
        self.interaction.nav_repeat.action = Some(action);
        self.interaction.nav_repeat.last = Some(now);
        let held = self
            .interaction
            .nav_repeat
            .started
            .map_or(Duration::ZERO, |s| now.duration_since(s));
        nav_step_for_hold(held)
    }

    /// Switch screens from a nav-bar click - the mouse equivalent of the `Open*` keys, but
    /// reachable from any screen. Leaving Settings commits the draft via the normal close
    /// path so edits aren't lost; transient overlays are cleared.
    pub(in crate::app) fn navigate_to(&mut self, mode: Mode) -> Vec<Cmd> {
        self.overlays.help_visible = false;
        self.overlays.mouse_help_visible = false;
        self.dropdowns.eq_open = false;
        self.dropdowns.streaming_open = false;
        self.dropdowns.search_source_open = false;
        self.queue_popup.open = false;
        self.search_filter.close();
        self.library_ui.confirm_delete = None;
        self.library_ui.confirm_download = None;
        self.local_mode.pending_organize_confirm = None;
        // Popup-like playlist surfaces dismiss on any navigation (the drill-down itself is
        // content state - it resets only on a fresh Library entry below).
        self.library_ui.create_input = None;
        self.library_ui.confirm_playlist_delete = None;
        self.playlist_picker = None;
        // Any navigation deselects: a Ctrl+A highlight must not survive a screen change.
        self.search.select_all = false;
        self.ai.select_all = false;
        if self.mode == mode {
            self.dirty = true;
            return Vec::new();
        }
        let cmds = if self.mode == Mode::Settings {
            self.finish_settings_text_edit();
            self.close_settings() // sets mode = Player; overridden below if needed
        } else {
            Vec::new()
        };
        match mode {
            Mode::Player => self.mode = Mode::Player,
            Mode::Search => {
                self.mode = Mode::Search;
                self.search.focus = SearchFocus::Input;
                let search = self.search_config_for_mode();
                self.search.source = search.normalized_source(self.search.source);
            }
            Mode::Library => {
                self.mode = Mode::Library;
                if !self.library_tab_available(self.library_ui.tab) {
                    self.library_ui.tab = self.library_tabs()[0];
                }
                // Start each library visit clean (cursor, anchor, scroll, filter, and any
                // playlist drill-down or popup left from the previous visit).
                self.reset_playlist_ui_state();
                self.clear_library_filter();
                if self.effective_library_tab() == LibraryTab::Playlists {
                    self.hint_playlist_create();
                }
            }
            Mode::Settings => self.open_settings(),
            Mode::Ai => self.enter_ai(),
        }
        self.dirty = true;
        cmds
    }

    /// Select a Settings tab by index into [`SettingsTab::ALL`] (from a tab click).
    pub(in crate::app) fn settings_select_tab(&mut self, index: usize) {
        if let Some(st) = self.settings.as_mut()
            && let Some(&tab) = SettingsTab::ALL.get(index)
        {
            st.tab = tab;
            st.row = 0;
            st.editing_text = false;
            st.capturing = None;
            // The new tab has a different row set; drop the old offset so it starts at the top.
            self.bridges.reset_settings_scroll();
            self.dirty = true;
        }
    }

    /// Move the field cursor to `row` before a mouse-driven change/activate, so the shared
    /// keyboard handlers (`settings_change`/`settings_activate`) act on the clicked row.
    pub(in crate::app) fn settings_focus_row(&mut self, row: usize) {
        // Commit any in-progress text edit *before* moving focus: a secret editor (API key) is
        // opened with its buffer cleared and the prior key stashed in `secret_restore`. Clicking
        // straight to another control would otherwise leave that buffer empty with `editing_text`
        // cleared, orphaning the stash - and `close_settings` would then erase the saved key.
        // `finish_settings_text_edit` runs while the cursor is still on the secret row, so it
        // restores into the right buffer.
        self.finish_settings_text_edit();
        if let Some(st) = self.settings.as_mut() {
            // The Keys tab has no `Field`s; leave its binding-selection cursor untouched (no
            // field controls register there, so this is defensive).
            let fields = st.fields();
            if !fields.is_empty() {
                st.row = row.min(fields.len() - 1);
            }
            st.editing_text = false;
            self.dirty = true;
        }
    }
}
