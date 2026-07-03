//! Key-routing reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    pub(in crate::app) fn on_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // Some terminals render IME preedit text even in raw alternate-screen apps. Always
        // redraw after a key press so committed Korean jamo used as shortcuts are covered.
        self.dirty = true;
        let chord = Chord::from(k);
        // Ctrl+C always quits, regardless of mode or remapping (a hard safety key that is
        // never part of the keymap, so the user can't lock themselves out).
        if chord == Chord::new(KeyCode::Char('c'), KeyModifiers::CONTROL) {
            return self.quit_app();
        }

        // A keybinding-conflict warning is modal: the next keypress just dismisses it (the
        // rejected rebind already left the binding untouched), so it never leaks through to
        // the screen underneath.
        if self.key_conflict.take().is_some() {
            self.dirty = true;
            return Vec::new();
        }

        // Radio mode switching is modal: Enter or `y` confirms, anything else cancels.
        // It sits outside Settings so the shortcut works from the Player/Search/Library tabs too.
        if let Some(confirm) = self.pending_radio_mode_confirm.take() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            return if confirmed {
                self.apply_radio_mode_confirm(confirm)
            } else {
                Vec::new()
            };
        }

        // Settings confirmations are modal: Enter or `y` confirms, anything else cancels.
        // Handle it here so the key can't leak through to the settings list.
        if let Some(confirm) = self.pending_settings_confirm.take() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            return if confirmed {
                self.settings_apply_confirm(confirm)
            } else {
                Vec::new()
            };
        }

        // Deleting downloaded files is irreversible, so it's gated behind the same modal:
        // Enter or `y` confirms the on-disk delete, anything else backs out. Handled here so
        // the key can't fall through to the library list underneath.
        if self.library_ui.confirm_delete.is_some() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            if confirmed {
                return self.confirm_delete_files_apply();
            }
            self.library_ui.confirm_delete = None;
            return Vec::new();
        }

        // Deleting a playlist drops the whole list at once, so it's gated behind the same
        // modal: Enter or `y` confirms, anything else backs out.
        if self.library_ui.confirm_playlist_delete.is_some() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            if confirmed {
                return self.confirm_playlist_delete_apply();
            }
            self.library_ui.confirm_playlist_delete = None;
            return Vec::new();
        }

        // The "add to playlist" picker captures the keyboard while open (list nav + the
        // inline name entry) — before Search-Enter and globals so keys can't leak through.
        if self.playlist_picker.is_some() {
            return self.on_key_playlist_picker(k);
        }

        // Search submit/select is fixed to the physical Enter key. Handle it before
        // remappable globals so Enter stays local to Search while every other screen keeps
        // using the user's keymap.
        if self.mode == Mode::Search
            && !self.help_visible
            && !self.mouse_help_visible
            && k.code == KeyCode::Enter
        {
            return self.on_key_search(k);
        }

        // Home is intentionally a hard global action: it should work even while a text
        // field or key-capture prompt is focused.
        if matches!(self.keymap.global_action(chord), Some(Action::Home)) {
            return self.go_home();
        }

        // The keybinding editor's capture mode grabs the next key verbatim (except Esc),
        // so it must run before the global/help shortcuts could swallow it.
        if self.mode == Mode::Settings
            && self
                .settings
                .as_ref()
                .is_some_and(|s| s.capturing.is_some())
        {
            return self.settings_capture_key(k);
        }

        // Text zoom is resolved ahead of the overlay blocks below so it keeps working
        // while help / about / the mouse sheet are up — the cheat-sheet itself advertises
        // Ctrl+-/= , so the natural first place users try it is with that overlay open.
        if !(self.in_text_entry() && chord.is_typeable())
            && let Some(action @ (Action::TextZoomIn | Action::TextZoomOut)) =
                self.keymap.global_action(chord)
        {
            return self.zoom_step(matches!(action, Action::TextZoomIn));
        }

        // While the help overlay is up, swallow input; help-toggle / Esc / Back dismiss it,
        // and the navigation keys scroll the sheet (it rarely fits whole on small grids).
        if self.help_visible {
            if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
                return self.quit_app();
            }
            if self.scroll_help_overlay(chord) {
                return Vec::new();
            }
            let close = matches!(self.keymap.global_action(chord), Some(Action::ToggleHelp))
                || k.code == KeyCode::Esc
                || matches!(
                    self.keymap.action(KeyContext::Common, chord),
                    Some(Action::Back)
                );
            if close {
                self.help_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // The mouse cheat-sheet is opened by a mouse-only footer icon. While up, swallow input;
        // Esc / Back dismiss it, Quit still works, and the navigation keys scroll it.
        if self.mouse_help_visible {
            if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
                return self.quit_app();
            }
            if self.scroll_help_overlay(chord) {
                return Vec::new();
            }
            let close = k.code == KeyCode::Esc
                || matches!(
                    self.keymap.action(KeyContext::Common, chord),
                    Some(Action::Back)
                );
            if close {
                self.mouse_help_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // The About card behaves like the help overlay: while it's up, swallow input; its own
        // toggle (F1) / Esc / Back dismiss it, and Quit still works.
        if self.about_visible {
            if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
                return self.quit_app();
            }
            let close = matches!(self.keymap.global_action(chord), Some(Action::ToggleAbout))
                || k.code == KeyCode::Esc
                || matches!(
                    self.keymap.action(KeyContext::Common, chord),
                    Some(Action::Back)
                );
            if close {
                self.about_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // The "Why DJ Gem" overlay behaves like the About card: while it's up, swallow input; its own
        // toggle (`w`) / Esc / Back dismiss it, and Quit still works.
        if self.why_ai_visible {
            if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
                return self.quit_app();
            }
            let close = matches!(self.keymap.global_action(chord), Some(Action::WhyAi))
                || k.code == KeyCode::Esc
                || matches!(
                    self.keymap.action(KeyContext::Common, chord),
                    Some(Action::Back)
                );
            if close {
                self.why_ai_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // Global shortcuts (help, streaming). Suppressed only when a *typeable* key would feed
        // a focused text field — so `?` types into the search box but opens help elsewhere,
        // while Ctrl-based globals (streaming) keep working everywhere as before.
        if !(self.in_text_entry() && chord.is_typeable())
            && let Some(action) = self.keymap.global_action(chord)
        {
            match action {
                Action::ToggleHelp => {
                    self.help_visible = true;
                    self.bridges.help_scroll.reset();
                    self.dirty = true;
                    return Vec::new();
                }
                Action::ToggleAbout => {
                    self.about_visible = true;
                    self.dirty = true;
                    return Vec::new();
                }
                Action::WhyAi => {
                    // Only worth an overlay if a prior DJ Gem rerank left something to explain;
                    // otherwise nudge the user with a transient note instead of an empty card.
                    if self.streaming.last_explain.is_some() {
                        self.why_ai_visible = true;
                    } else {
                        self.status.kind = StatusKind::Info;
                        self.status.text = t!(
                            "No DJ Gem streaming picks to explain yet.",
                            "아직 설명할 DJ Gem 라디오 선곡이 없어요."
                        )
                        .to_owned();
                    }
                    self.dirty = true;
                    return Vec::new();
                }
                Action::ToggleAnimations => return self.toggle_animations(),
                Action::ToggleStreaming => {
                    self.autoplay_streaming = !self.autoplay_streaming;
                    self.status.text = format!(
                        "{}: {}",
                        t!("Autoplay streaming", "자동 스트리밍"),
                        if self.autoplay_streaming {
                            "✓"
                        } else {
                            "✗"
                        }
                    );
                    self.dirty = true;
                    let mut cmds = vec![self.save_playback_modes_cmd()];
                    // Kick off the top-up now (not at end-of-track) so a low/single-song queue
                    // has the next tracks queued before the current one ends — no silent gap.
                    if self.autoplay_streaming {
                        cmds.extend(self.maybe_autoplay_extend());
                    }
                    return cmds;
                }
                Action::Quit => {
                    return self.quit_app();
                }
                Action::Home => return self.go_home(),
                _ => {}
            }
        }

        // The queue window is a player overlay that captures the keyboard while open (after
        // the global shortcuts above, so Quit/Home/Help still work).
        if self.queue_popup.open {
            return self.on_key_queue(k);
        }

        match self.mode {
            Mode::Player => self.on_key_player(k),
            Mode::Search => self.on_key_search(k),
            Mode::Library => self.on_key_library(k),
            Mode::Settings => self.on_key_settings(k),
            Mode::Ai => self.on_key_ai(k),
        }
    }

    /// Scroll the open help / mouse cheat-sheet with the shared navigation chords. Returns
    /// whether the chord was a scroll key (the overlay swallows it either way; this just
    /// tells the caller not to treat it as anything else). The sheet length is unknown here
    /// — render clamps the offset to the real content every frame, so `usize::MAX` simply
    /// means "no reducer-side ceiling".
    fn scroll_help_overlay(&mut self, chord: crate::keymap::Chord) -> bool {
        let scroll = &self.bridges.help_scroll;
        let page = scroll.viewport().max(1);
        match self.keymap.action(KeyContext::Common, chord) {
            Some(Action::MoveUp) => scroll.wheel(true, 1, usize::MAX),
            Some(Action::MoveDown) => scroll.wheel(false, 1, usize::MAX),
            Some(Action::PageUp) => scroll.wheel(true, page, usize::MAX),
            Some(Action::PageDown) => scroll.wheel(false, page, usize::MAX),
            Some(Action::JumpTop) => scroll.set_offset(0, usize::MAX),
            Some(Action::JumpBottom) => scroll.wheel(false, usize::MAX / 2, usize::MAX),
            _ => return false,
        }
        self.dirty = true;
        true
    }

    /// Whether a focused text field is currently capturing typed characters (so command
    /// keys and the `?` help shortcut must not fire — they'd be typed instead).
    pub(in crate::app) fn in_text_entry(&self) -> bool {
        // The picker's inline name entry captures text regardless of the mode it opened over.
        if self
            .playlist_picker
            .as_ref()
            .is_some_and(|p| p.naming.is_some())
        {
            return true;
        }
        match self.mode {
            Mode::Search => self.search.focus == SearchFocus::Input,
            Mode::Ai => self.ai.focus == AiFocus::Input,
            Mode::Settings => self.settings.as_ref().is_some_and(|s| s.editing_text),
            Mode::Library => {
                self.library_ui.filter_editing || self.library_ui.create_input.is_some()
            }
            _ => false,
        }
    }

    pub fn should_scrub_ime_preedit(&self) -> bool {
        !self.in_text_entry()
    }
}
