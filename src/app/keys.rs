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

        // The "reset all settings" confirmation is modal: Enter or `y` confirms, anything
        // else cancels. Handle it here so the key can't leak through to the settings list.
        if self.confirm_reset_all {
            self.confirm_reset_all = false;
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            return if confirmed { self.settings_reset_all() } else { Vec::new() };
        }

        // Deleting downloaded files is irreversible, so it's gated behind the same modal:
        // Enter or `y` confirms the on-disk delete, anything else backs out. Handled here so
        // the key can't fall through to the library list underneath.
        if self.confirm_delete_files.is_some() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            if confirmed {
                return self.confirm_delete_files_apply();
            }
            self.confirm_delete_files = None;
            return Vec::new();
        }

        // Search submit/select is fixed to the physical Enter key. Handle it before
        // remappable globals so Enter stays local to Search while every other screen keeps
        // using the user's keymap.
        if self.mode == Mode::Search && !self.help_visible && k.code == KeyCode::Enter {
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

        // While the help overlay is up, swallow input; help-toggle / Esc / Back dismiss it.
        if self.help_visible {
            if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
                return self.quit_app();
            }
            let close = matches!(self.keymap.global_action(chord), Some(Action::ToggleHelp))
                || k.code == KeyCode::Esc
                || matches!(self.keymap.action(KeyContext::Common, chord), Some(Action::Back));
            if close {
                self.help_visible = false;
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
                || matches!(self.keymap.action(KeyContext::Common, chord), Some(Action::Back));
            if close {
                self.about_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // Global shortcuts (help, radio). Suppressed only when a *typeable* key would feed
        // a focused text field — so `?` types into the search box but opens help elsewhere,
        // while Ctrl-based globals (radio) keep working everywhere as before.
        if !(self.in_text_entry() && chord.is_typeable())
            && let Some(action) = self.keymap.global_action(chord)
        {
            match action {
                Action::ToggleHelp => {
                    self.help_visible = true;
                    self.dirty = true;
                    return Vec::new();
                }
                Action::ToggleAbout => {
                    self.about_visible = true;
                    self.dirty = true;
                    return Vec::new();
                }
                Action::ToggleRadio => {
                    self.autoplay_radio = !self.autoplay_radio;
                    self.status = format!(
                        "{}: {}",
                        t!("Autoplay radio", "자동재생 라디오"),
                        if self.autoplay_radio { "✓" } else { "✗" }
                    );
                    self.dirty = true;
                    // Kick off the top-up now (not at end-of-track) so a low/single-song queue
                    // has the next tracks queued before the current one ends — no silent gap.
                    if self.autoplay_radio {
                        return self.maybe_autoplay_extend();
                    }
                    return Vec::new();
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
        if self.queue_popup_open {
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

    /// Whether a focused text field is currently capturing typed characters (so command
    /// keys and the `?` help shortcut must not fire — they'd be typed instead).
    pub(in crate::app) fn in_text_entry(&self) -> bool {
        match self.mode {
            Mode::Search => self.search_focus == SearchFocus::Input,
            Mode::Ai => self.ai_focus == AiFocus::Input,
            Mode::Settings => self.settings.as_ref().is_some_and(|s| s.editing_text),
            _ => false,
        }
    }

    pub fn should_scrub_ime_preedit(&self) -> bool {
        !self.in_text_entry()
    }
}
