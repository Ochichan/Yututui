//! Key-routing reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    pub(in crate::app) fn on_key(&mut self, mut k: KeyEvent) -> Vec<Cmd> {
        // Character repeat is opt-in. The translator forwards only lyric-delay mappings, but
        // tests and alternate event sources can call the reducer directly; reject those repeats
        // before they dirty the frame or dismiss a modal that currently owns keyboard input.
        if k.kind == crossterm::event::KeyEventKind::Repeat
            && !crate::event::is_autorepeat_nav_key(k.code)
            && !self.lyrics_repeat_has_keyboard(k)
        {
            return Vec::new();
        }
        // Some terminals render IME preedit text even in raw alternate-screen apps. Always
        // redraw after a key press so committed Korean jamo used as shortcuts are covered.
        self.dirty = true;
        let mut chord = Chord::from(k);
        // Ctrl+C always quits, regardless of mode or remapping (a hard safety key that is
        // never part of the keymap, so the user can't lock themselves out).
        if chord == Chord::new(KeyCode::Char('c'), KeyModifiers::CONTROL) {
            return self.quit_app();
        }

        // The keybinding editor owns the next raw chord. It must run before Home and before
        // Legacy Ctrl+H normalization so ambiguous terminals record what they actually sent;
        // Ctrl+C above remains the one non-capturable emergency exit.
        if self.mode == Mode::Settings
            && self
                .settings
                .as_ref()
                .is_some_and(|s| s.capturing.is_some())
        {
            return self.settings_capture_key(k);
        }
        if self.tool_setup.is_some() {
            return self.on_key_tool_setup(k);
        }
        // Beginner Mode's F6 focus bridge runs before ordinary surface routing, while every
        // established overlay keeps precedence. Unowned keys continue through unchanged.
        if !self.beginner_higher_overlay_open()
            && let Some(cmds) = self.on_key_beginner(k)
        {
            return cmds;
        }

        // The Settings color picker is a mode-owned modal (also rendered in Mini). Capture every
        // key before global shortcuts or the hidden Settings form can act on it.
        if self.settings_color_picker_is_open() {
            return self.settings_color_picker_key(k);
        }

        // A keybinding-conflict warning is modal: the next keypress just dismisses it (the
        // rejected rebind already left the binding untouched), so it never leaks through to
        // the screen underneath.
        if self.overlays.key_conflict.take().is_some() {
            self.dirty = true;
            return Vec::new();
        }

        // Radio mode switching is modal: Enter or `y` confirms, anything else cancels.
        // It sits outside Settings so the shortcut works from the Player/Search/Library tabs too.
        if let Some(confirm) = self.radio_mode.pending_radio_mode_confirm {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            if confirmed {
                return self.apply_radio_mode_confirm(confirm);
            }
            self.radio_mode.pending_radio_mode_confirm = None;
            return Vec::new();
        }

        // Local Player mode switching is modal and follows the Radio confirmation rules.
        if let Some(confirm) = self.local_mode.pending_confirm {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            if confirmed {
                return self.apply_local_mode_confirm(confirm);
            }
            self.cancel_local_mode_switch();
            return Vec::new();
        }

        // A Local Find bulk action that would exceed the bounded queue is explicit: Enter/`y`
        // accepts the displayed prefix, every other key cancels without a partial mutation.
        if self.local_mode.find.pending_bulk_confirm.is_some() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            return if confirmed {
                self.confirm_local_find_bulk()
            } else {
                self.local_mode.find.pending_bulk_confirm = None;
                Vec::new()
            };
        }

        // Local import organize is modal because it moves files on disk. Enter/`y` applies
        // the latest organize plan; any other key backs out.
        if let Some(confirm) = self.local_mode.pending_organize_confirm.take() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            return if confirmed {
                self.apply_local_import_organize_confirm(confirm)
            } else {
                Vec::new()
            };
        }

        // Local import accept-all is modal because it changes many review decisions at once.
        if let Some(confirm) = self.local_mode.pending_accept_all_confirm.take() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            return if confirmed {
                self.apply_local_import_accept_all_confirm(confirm)
            } else {
                Vec::new()
            };
        }

        // Import history deletion removes resumability artifacts, so it requires confirmation.
        if let Some(session_id) = self.local_mode.pending_import_record_delete.take() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            return if confirmed {
                self.apply_local_import_record_delete(session_id)
            } else {
                Vec::new()
            };
        }

        // Settings confirmations are modal: Enter or `y` confirms, anything else cancels.
        // Handle it here so the key can't leak through to the settings list.
        if let Some(confirm) = self.overlays.pending_settings_confirm.take() {
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

        // The bulk-download confirmation is modal the same way: Enter or `y` starts the batch,
        // anything else backs out. Handled here so the key can't leak to the library list.
        if self.library_ui.confirm_download.is_some() {
            self.dirty = true;
            let confirmed = k.code == KeyCode::Enter
                || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
            if confirmed {
                return self.confirm_download_apply();
            }
            self.library_ui.confirm_download = None;
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

        // The row context menu is modal once open. Its own handler preserves Quit, consumes
        // everything else, and resolves actions against the row snapshot that opened it.
        if self.overlays.context_menu.is_some() {
            return self.on_key_context_menu(k);
        }

        // Legacy terminals can report both physical Ctrl+Backspace and Ctrl+H as `^H`. While
        // DeleteWord still owns its default chord, prefer safe editing in a focused text field
        // and consume the ambiguous report everywhere else. Higher-priority non-editor modals
        // have already captured it; exact keyboard modes never enter this branch.
        if self.terminal_keyboard_mode == crate::terminal_keyboard::KeyboardInputMode::Legacy
            && chord == Chord::new(KeyCode::Char('h'), KeyModifiers::CONTROL)
            && self.keymap.legacy_ctrl_backspace_fallback_active()
        {
            if self.in_text_entry() {
                k.code = KeyCode::Backspace;
                chord = Chord::from(k);
            } else {
                return Vec::new();
            }
        }

        // The "add to playlist" picker captures the keyboard while open (list nav + the
        // inline name entry) — before Search-Enter and globals so keys can't leak through.
        if self.playlist_picker.is_some() {
            return self.on_key_playlist_picker(k);
        }

        // Shift+F10-style accessibility fallback. Resolve it before the search-filter input
        // captures non-typeable keys; `open_context_menu_for_keyboard` still rejects every
        // higher-priority modal and unsupported surface.
        if !(self.in_text_entry() && chord.is_typeable())
            && matches!(
                self.keymap.global_action(chord),
                Some(Action::OpenContextMenu)
            )
        {
            return self.open_context_menu_for_keyboard();
        }

        // The search results-filter popup captures the keyboard while open (its query
        // input is always live, so every typeable key belongs to it) — before Search-Enter
        // and globals so nothing leaks into the list underneath.
        if self.local_mode.find.refine_popup.open
            && self.active_search_surface() == ActiveSearchSurface::Local
        {
            return self.on_key_local_find_refine(k);
        }
        if self.search_filter.open && self.active_search_surface() != ActiveSearchSurface::Local {
            return self.on_key_search_filter(k);
        }

        // Search submit/select is fixed to the physical Enter key. Handle it before
        // remappable globals so Enter stays local to Search while every other screen keeps
        // using the user's keymap.
        if self.mode == Mode::Search
            && !self.overlays.help_visible
            && !self.overlays.mouse_help_visible
            && k.code == KeyCode::Enter
        {
            return if self.active_search_surface() == ActiveSearchSurface::Local {
                self.on_key_local_find(k)
            } else {
                self.on_key_search(k)
            };
        }

        // Home is intentionally a hard global action in exact keyboard modes: it works even
        // while a text field is focused. Settings capture and the Legacy safety alias above
        // receive first refusal because their raw input would otherwise be lost here.
        if matches!(self.keymap.global_action(chord), Some(Action::Home)) {
            return self.go_home();
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
        if self.overlays.help_visible {
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
                self.overlays.help_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // The mouse cheat-sheet is opened by a mouse-only footer icon. While up, swallow input;
        // Esc / Back dismiss it, Quit still works, and the navigation keys scroll it.
        if self.overlays.mouse_help_visible {
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
                self.overlays.mouse_help_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // The About card behaves like the help overlay: while it's up, swallow input; its own
        // toggle (F1) / Esc / Back dismiss it, and Quit still works.
        if self.overlays.about_visible {
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
                self.overlays.about_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // The "what's playing" identify overlay is modal: its own remappable actions
        // (`KeyContext::NowPlaying` — favorite / ask DJ Gem, listed in the cheat sheet
        // and editable in Settings › Keys) work, its toggle (`i`) / Esc / Enter / Back
        // close it, Quit still works, and everything else is swallowed so nothing leaks
        // into the player underneath.
        if self.overlays.now_playing_overlay.is_some() {
            if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
                return self.quit_app();
            }
            match self.keymap.context_action(KeyContext::NowPlaying, chord) {
                Some(Action::NowPlayingFavorite) => return self.now_playing_favorite(),
                Some(Action::NowPlayingAskAi) => return self.now_playing_ask_ai(),
                _ => {}
            }
            let close = matches!(
                self.keymap.action(KeyContext::Player, chord),
                Some(Action::IdentifyNowPlaying)
            ) || k.code == KeyCode::Esc
                || k.code == KeyCode::Enter
                || matches!(
                    self.keymap.action(KeyContext::Common, chord),
                    Some(Action::Back)
                );
            if close {
                self.close_now_playing_overlay();
            }
            return Vec::new();
        }

        // The recordings browser (Decide-mode save/discard/play) is modal wherever it opens —
        // over the player, or on top of the recording-settings popup. Quit still works; its own
        // keys (↑/↓, s/d/Enter) act on the selected track; its toggle / Esc / Back close it.
        if self.overlays.recordings_browser.is_some() {
            if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
                return self.quit_app();
            }
            return self.recordings_browser_key(k);
        }

        if self.overlays.audio_output_picker.is_some() {
            return self.audio_output_picker_key(k);
        }

        // The recording-settings popup renders as a top-level overlay (`ui::mod`), so it must
        // capture input here too — not only inside Settings-mode dispatch. Otherwise a global
        // shortcut (`?`/`w`) or Home would open/enter another window *behind* it and strand the
        // popup painting on top. Quit still works; the recordings browser (checked above) can
        // still open on top of it; everything else routes to its own handler.
        if self.overlays.recording_settings.is_some() {
            if matches!(self.keymap.global_action(chord), Some(Action::Quit)) {
                return self.quit_app();
            }
            return self.recording_settings_key(k);
        }

        // The "Why DJ Gem" overlay behaves like the About card: while it's up, swallow input; its own
        // toggle (`w`) / Esc / Back dismiss it, and Quit still works.
        if self.overlays.why_ai_visible {
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
                self.overlays.why_ai_visible = false;
                self.dirty = true;
            }
            return Vec::new();
        }

        // Local Deck owns Shift+A/`A` for import accept-all and, outside import review rows,
        // "enqueue the current local result set". That chord overlaps the global animation
        // toggle, so route it before globals only while the Local Deck list is active; filter
        // text entry still receives typeable characters normally.
        if self.mode == Mode::Library
            && self.local_dedicated_mode
            && !self.local_mode.ui.filter_editing
            && matches!(k.code, KeyCode::Char('A'))
            && (k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT)
        {
            return self.on_key_local(k);
        }

        // Global shortcuts (help, streaming). Suppressed only when a *typeable* key would feed
        // a focused text field — so `?` types into the search box but opens help elsewhere,
        // while Ctrl-based globals (streaming) keep working everywhere as before.
        if !(self.in_text_entry() && chord.is_typeable())
            && let Some(action) = self.keymap.global_action(chord)
        {
            match action {
                Action::ToggleHelp => {
                    self.overlays.help_visible = true;
                    self.bridges.help_scroll.reset();
                    self.dirty = true;
                    return Vec::new();
                }
                Action::ToggleAbout => {
                    self.overlays.about_visible = true;
                    self.dirty = true;
                    return Vec::new();
                }
                Action::WhyAi => {
                    // Only worth an overlay if a prior DJ Gem rerank left something to explain;
                    // otherwise nudge the user with a transient note instead of an empty card.
                    if self.streaming.last_explain.is_some() {
                        self.overlays.why_ai_visible = true;
                    } else {
                        self.status.kind = StatusKind::Info;
                        self.status.text = t!(
                            "No DJ Gem streaming picks to explain yet.",
                            "아직 설명할 DJ Gem 라디오 선곡이 없어요.",
                            "説明できるDJ Gemの選曲はまだありません。"
                        )
                        .to_owned();
                    }
                    self.dirty = true;
                    return Vec::new();
                }
                Action::ToggleAnimations => return self.toggle_animations(),
                Action::ToggleControlBox => return self.toggle_control_box(),
                Action::ToggleZoomWheelLock => return self.toggle_zoom_wheel_lock(),
                Action::ToggleStreaming => {
                    // Local Deck is deliberately offline. Preserve the normal-mode preference so
                    // it resumes on exit, but never let a Local key press rewrite that preference.
                    if self.local_dedicated_mode {
                        self.status.text = t!(
                            "Autoplay stays off in Local Deck",
                            "로컬 덱에서는 자동재생이 꺼져 있어요",
                            "ローカルデッキでは自動再生はオフのままです"
                        )
                        .to_owned();
                        self.dirty = true;
                        return Vec::new();
                    }
                    // Radio mode: autoplay is meaningless — keep whatever the stored preference
                    // is (so it survives the round-trip) and just say why nothing changed.
                    if self.radio_dedicated_mode {
                        self.status.text = t!(
                            "Autoplay stays off in Radio mode",
                            "라디오 모드에서는 자동재생이 꺼져 있어요",
                            "ラジオモードでは自動再生はオフのままです"
                        )
                        .to_owned();
                        self.dirty = true;
                        return Vec::new();
                    }
                    let transition =
                        PlaybackModeState::new(self.queue.repeat, self.autoplay_streaming)
                            .transition(PlaybackModeAction::SetStreaming(!self.autoplay_streaming));
                    let Ok(transition) = transition else {
                        self.show_streaming_repeat_conflict();
                        return Vec::new();
                    };
                    let enabling = transition.state.autoplay_streaming;
                    self.set_autoplay_streaming(enabling);
                    self.status.text = format!(
                        "{}: {}",
                        t!("Autoplay", "자동재생", "自動再生"),
                        if enabling { "✓" } else { "✗" }
                    );
                    self.dirty = true;
                    let mut cmds = vec![self.save_playback_modes_cmd()];
                    // Kick off the top-up now (not at end-of-track) so a low/single-song queue
                    // has the next tracks queued before the current one ends — no silent gap.
                    if enabling {
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

        // Under the miniplayer tier only the transport is on screen, so keys route to the
        // Player context regardless of the retained mode — a suppressed screen must not
        // eat keystrokes into an invisible list or input. Two modals render
        // mode-independently but are keyed inside their owning screen's handler (the
        // Spotify picker and the create-playlist input); while one is open the normal
        // dispatch stays so it can be operated and dismissed.
        if self.bridges.ui_tier.get() == crate::ui::layout::UiTier::Mini
            && !self.mini_mode_owns_modal()
        {
            return self.on_key_player(k);
        }

        // Dedicated Local Deck owns a separate, remappable collection-wide Find action. Higher
        // overlays, key-capture sessions, and the miniplayer have already had first refusal.
        if self.local_dedicated_mode
            && !(self.in_text_entry() && chord.is_typeable())
            && matches!(
                self.keymap.action(KeyContext::LocalDeck, chord),
                Some(Action::OpenLocalFind)
            )
        {
            return self.navigate_to(Mode::Search);
        }

        match self.mode {
            Mode::Player => self.on_key_player(k),
            Mode::Search if self.active_search_surface() == ActiveSearchSurface::Local => {
                self.on_key_local_find(k)
            }
            Mode::Search => self.on_key_search(k),
            Mode::Library if self.local_dedicated_mode => self.on_key_local(k),
            Mode::Library => self.on_key_library(k),
            Mode::Settings => self.on_key_settings(k),
            Mode::Ai => self.on_key_ai(k),
        }
    }

    /// Whether the retained (hidden) screen owns an open modal that renders
    /// mode-independently — the only cases where mini-tier key routing must fall through
    /// to the screen's own handler (see the mini guard in [`Self::on_key`]).
    fn mini_mode_owns_modal(&self) -> bool {
        match self.mode {
            Mode::Settings => {
                self.overlays.spotify_picker.is_some()
                    || self.settings.as_ref().is_some_and(|s| {
                        s.spotify_import_mode_dropdown.is_some() || s.color_picker.is_some()
                    })
            }
            Mode::Library => !self.local_dedicated_mode && self.library_ui.create_input.is_some(),
            _ => false,
        }
    }

    fn lyrics_repeat_has_keyboard(&self, key: KeyEvent) -> bool {
        self.mode == Mode::Player
            && self.bridges.ui_tier.get() != crate::ui::layout::UiTier::Mini
            && self.art_overlay_mask() == 0
            && self.local_mode.pending_confirm.is_none()
            && self.overlays.spotify_picker.is_none()
            && self.overlays.now_playing_overlay.is_none()
            && self.overlays.recording_settings.is_none()
            && self.overlays.recordings_browser.is_none()
            && matches!(
                self.keymap
                    .context_action(KeyContext::Player, Chord::from(key)),
                Some(Action::LyricsDelayEarlier | Action::LyricsDelayLater)
            )
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
        if self
            .overlays
            .audio_output_picker
            .as_ref()
            .is_some_and(|picker| picker.editing_manual)
            || self
                .overlays
                .recording_settings
                .as_ref()
                .is_some_and(|settings| settings.editing_dir)
        {
            return true;
        }
        // The picker's inline name entry captures text regardless of the mode it opened over.
        if self
            .playlist_picker
            .as_ref()
            .is_some_and(|p| p.naming.is_some())
        {
            return true;
        }
        // The results-filter popup's query input is always live while it's open.
        if self.search_filter.open {
            return true;
        }
        match self.mode {
            Mode::Search if self.active_search_surface() == ActiveSearchSurface::Local => {
                self.local_mode.find.focus == LocalFindFocus::Input
            }
            Mode::Search => self.search.focus == SearchFocus::Input,
            Mode::Ai => self.ai.focus == AiFocus::Input,
            Mode::Settings => self.settings.as_ref().is_some_and(|s| s.editing_text),
            Mode::Library => {
                if self.local_dedicated_mode {
                    self.local_mode.ui.filter_editing
                } else {
                    self.library_ui.filter_editing || self.library_ui.create_input.is_some()
                }
            }
            _ => false,
        }
    }

    /// Whether the owner loop's IME scrub arm may run this turn. Text entry always suppresses it
    /// (the focused editor owns the cursor there); outside text entry it runs only while the
    /// event-armed burst is live, so a static idle screen has zero scrub wakeups.
    pub fn should_scrub_ime_preedit(&self) -> bool {
        !self.in_text_entry() && self.ime_scrub_burst > 0
    }

    /// Re-arm the IME scrub burst. The owner loop calls this for every received terminal event
    /// (keys, focus changes, resize, paste — including events that translate to no message):
    /// terminal activity is what creates terminal-owned preedit ghosts.
    pub fn arm_ime_scrub_burst(&mut self) {
        self.ime_scrub_burst = super::IME_SCRUB_BURST_TICKS;
    }

    /// Account one delivered scrub tick; at zero the select arm parks until the next event.
    pub fn consume_ime_scrub_tick(&mut self) {
        self.ime_scrub_burst = self.ime_scrub_burst.saturating_sub(1);
    }
}
