//! Settings-screen reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;

impl App {
    // --- Settings screen ----------------------------------------------------

    /// The live settings draft, mutable. Valid **only** in `Mode::Settings`, where the reducer
    /// upholds the invariant that `self.settings` is `Some`: `open_settings` sets it on entry and
    /// `close_settings` clears it on exit, and every caller below is reached through a
    /// `Mode::Settings` key/mouse route. `#[track_caller]` so a broken invariant blames the
    /// offending reducer arm, not this accessor.
    #[track_caller]
    pub(in crate::app) fn settings_mut(&mut self) -> &mut SettingsState {
        self.settings
            .as_deref_mut()
            .expect("settings draft present in Mode::Settings")
    }

    /// Open the settings screen, snapshotting the current persisted + live state into an
    /// editable draft.
    pub(in crate::app) fn open_settings(&mut self) {
        self.dropdowns.eq_open = false;
        self.dropdowns.radio_open = false;
        self.dropdowns.search_source_open = false;
        let path_str = |p: &Option<std::path::PathBuf>| {
            p.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        };
        let draft = SettingsDraft {
            cookies_file: path_str(&self.config.cookies_file),
            download_dir: path_str(&self.config.download_dir),
            search: self.config.effective_search(),
            mouse: self.config.effective_mouse(),
            album_art: self.config.effective_album_art(),
            autoplay_on_start: self.config.effective_autoplay_on_start(),
            speed: self.playback.speed,
            seek_seconds: self.audio.seek_seconds,
            mouse_wheel_volume: self.config.effective_mouse_wheel_volume(),
            gapless: self.config.effective_gapless(),
            autoplay_radio: self.autoplay_radio,
            radio_mode: self.config.radio.mode,
            eq_preset: self.audio.preset,
            eq_bands: self.audio.bands,
            normalize: self.audio.normalize,
            gemini_model: self.ai.model,
            // Deliberately the *raw* config key, not `effective_gemini_api_key()`: seeding the
            // env-provided value would let a save copy it into config.json (persisting a key
            // the user chose to keep only in the environment). The cost is that an env-only
            // key shows "(none)" here; the AI still works and README documents env-wins.
            gemini_api_key: self.config.gemini_api_key.clone().unwrap_or_default(),
            ai_enabled: self.config.effective_ai_enabled(),
            theme: self.theme.clone(),
            retro_mode: self.config.effective_retro_mode(),
            language: self.config.effective_language(),
            animations: self.config.animations,
        };
        self.settings = Some(Box::new(SettingsState {
            tab: SettingsTab::General,
            row: 0,
            draft,
            editing_text: false,
            secret_restore: None,
            keymap: self.keymap.clone(),
            capturing: None,
        }));
        self.mode = Mode::Settings;
        self.pending_settings_confirm = None;
        self.status.text.clear();
        // Start every Settings session at the top; clear any offset left from a prior session.
        self.bridges.reset_settings_scroll();
        self.dirty = true;
    }

    pub(in crate::app) fn on_key_settings(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // While editing a text field, keys feed the buffer until Enter/Esc commits it.
        if self.settings.as_ref().is_some_and(|s| s.editing_text) {
            return self.settings_edit_text(k);
        }
        let on_keys_tab = self
            .settings
            .as_ref()
            .is_some_and(|s| s.tab == SettingsTab::Keys);
        // A color row can now live anywhere (the Graphics tab), so the "reset color" key gates
        // on the focused field's type rather than a dedicated Colors tab.
        let on_color_field = self
            .settings
            .as_ref()
            .is_some_and(|s| matches!(s.current_field(), Some(Field::ThemeColor(_))));
        // The editor must stay operable no matter how keys are remapped, so the literal
        // arrows / Enter / Esc / Backspace are always honored here, on top of the configured
        // ones. Tab switching deliberately comes only from the keymap's FocusNext/FocusPrev
        // bindings so Library and Settings secondary tabs stay tied to the same setting.
        let action = self
            .keymap
            .action(KeyContext::Settings, k.into())
            .or_else(|| Self::settings_safety_action(k));
        match action {
            // `q`/Esc commit the draft before leaving the screen. The action name stays
            // SettingsCancel for compatibility with existing keybinding ids.
            Some(Action::SettingsCancel | Action::Back) => self.close_settings(),
            Some(Action::FocusNext) => {
                self.settings_switch_tab(true);
                Vec::new()
            }
            Some(Action::FocusPrev) => {
                self.settings_switch_tab(false);
                Vec::new()
            }
            Some(Action::MoveUp) => {
                self.settings_move_row(-1);
                Vec::new()
            }
            Some(Action::MoveDown) => {
                self.settings_move_row(1);
                Vec::new()
            }
            Some(Action::ChangeDecrease) if !on_keys_tab => self.settings_change(-1),
            Some(Action::ChangeIncrease) if !on_keys_tab => self.settings_change(1),
            Some(Action::Confirm) => {
                if on_keys_tab {
                    self.settings_begin_capture();
                    Vec::new()
                } else {
                    self.settings_activate()
                }
            }
            // Reset the highlighted binding to its default (Keys tab only).
            Some(Action::DeleteChar) if on_keys_tab => {
                self.settings_reset_binding();
                Vec::new()
            }
            // Reset the highlighted color override to the selected theme preset default.
            Some(Action::DeleteChar) if on_color_field => {
                self.settings_reset_color();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Literal navigation keys the settings editor always accepts, so a user can never
    /// remap themselves out of the screen that edits keybindings.
    pub(in crate::app) fn settings_safety_action(k: KeyEvent) -> Option<Action> {
        match k.code {
            KeyCode::Up => Some(Action::MoveUp),
            KeyCode::Down => Some(Action::MoveDown),
            KeyCode::Left => Some(Action::ChangeDecrease),
            KeyCode::Right => Some(Action::ChangeIncrease),
            KeyCode::Enter => Some(Action::Confirm),
            KeyCode::Esc => Some(Action::Back),
            KeyCode::Backspace => Some(Action::DeleteChar),
            _ => None,
        }
    }

    /// The `(context, action)` the Keys-tab cursor is on, if the Keys tab is active.
    pub(in crate::app) fn settings_current_binding(&self) -> Option<(KeyContext, Action)> {
        let st = self.settings.as_ref()?;
        if st.tab != SettingsTab::Keys {
            return None;
        }
        crate::keymap::editable_entries().get(st.row).copied()
    }

    /// Enter key-capture mode for the highlighted binding (Keys tab). The next keypress
    /// becomes the new chord (handled in [`Self::settings_capture_key`]).
    pub(in crate::app) fn settings_begin_capture(&mut self) {
        if let Some(entry) = self.settings_current_binding()
            && let Some(st) = self.settings.as_mut()
        {
            st.capturing = Some(entry);
            self.status.text = t!(
                "Press a key to bind (Esc to cancel)…",
                "바인딩할 키를 누르세요 (Esc로 취소)…"
            )
            .to_owned();
            self.dirty = true;
        }
    }

    /// Consume the captured keypress as the new chord for the binding being edited. Esc
    /// cancels; a conflict is rejected with a status message (the old binding is kept).
    pub(in crate::app) fn settings_capture_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let Some((ctx, action)) = self.settings.as_mut().and_then(|s| s.capturing.take()) else {
            return Vec::new();
        };
        self.dirty = true;
        if k.code == KeyCode::Esc {
            self.status.text = t!("Rebinding cancelled", "단축키 변경을 취소했어요").to_owned();
            return Vec::new();
        }
        let chord = Chord::from(k);
        let Some(st) = self.settings.as_mut() else {
            return Vec::new();
        };
        match st.keymap.rebind(ctx, action, chord) {
            Ok(()) => {
                let label = action.human_label();
                let chord = crate::keymap::format_chord(chord);
                self.status.text = if crate::i18n::is_korean() {
                    format!("{label} → {chord} 으로 바인딩됨")
                } else {
                    format!("Bound {label} to {chord}")
                };
            }
            Err(conflict) => {
                // Surface the clash as a modal warning rather than a quiet status line, so
                // the rebind visibly fails instead of silently keeping the old key.
                self.status.text.clear();
                self.key_conflict = Some(conflict);
            }
        }
        Vec::new()
    }

    /// Reset the highlighted binding (Keys tab) to its built-in default.
    pub(in crate::app) fn settings_reset_binding(&mut self) {
        let Some((ctx, action)) = self.settings_current_binding() else {
            return;
        };
        if let Some(st) = self.settings.as_mut() {
            match st.keymap.reset(ctx, action) {
                Ok(()) => {
                    let label = action.human_label();
                    self.status.text = if crate::i18n::is_korean() {
                        format!("{label} 을(를) 기본값으로 되돌림")
                    } else {
                        format!("Reset {label} to default")
                    };
                }
                Err(conflict) => {
                    // Same modal treatment as a manual rebind clash.
                    self.status.text.clear();
                    self.key_conflict = Some(conflict);
                }
            }
            self.dirty = true;
        }
    }

    pub(in crate::app) fn settings_reset_color(&mut self) {
        let Some(Field::ThemeColor(role)) = self.settings.as_ref().and_then(|s| s.current_field())
        else {
            return;
        };
        if let Some(st) = self.settings.as_mut() {
            st.draft.theme.reset_role(role);
            self.theme = st.draft.theme.normalized();
            let label = role.label();
            self.status.text = if crate::i18n::is_korean() {
                format!("{label} 색상을 되돌림")
            } else {
                format!("Reset {label} color")
            };
            self.dirty = true;
        }
    }

    pub(in crate::app) fn settings_switch_tab(&mut self, forward: bool) {
        if let Some(st) = self.settings.as_mut() {
            st.tab = st.tab.stepped(forward);
            st.row = 0;
            st.editing_text = false;
            st.capturing = None;
            // The new tab has a different row set; drop the old offset so it starts at the top.
            self.bridges.reset_settings_scroll();
            self.dirty = true;
        }
    }

    pub(in crate::app) fn settings_move_row(&mut self, delta: i32) {
        if let Some(st) = self.settings.as_mut() {
            // The Keys tab is a list of remappable bindings rather than `Field`s.
            let n = match st.tab {
                SettingsTab::Keys => crate::keymap::editable_entries().len() as i32,
                _ => st.tab.fields().len() as i32,
            };
            st.row = (st.row as i32 + delta).clamp(0, n.max(1) - 1) as usize;
            st.editing_text = false;
            self.dirty = true;
        }
    }

    /// Change the focused field's value with ←/→. Audio fields apply to mpv immediately.
    pub(in crate::app) fn settings_change(&mut self, dir: i32) -> Vec<Cmd> {
        let Some(field) = self.settings.as_ref().and_then(|s| s.current_field()) else {
            return Vec::new();
        };
        self.dirty = true;
        match field {
            Field::Mouse => {
                let s = self.settings_mut();
                s.draft.mouse = !s.draft.mouse;
                Vec::new()
            }
            Field::AlbumArt => {
                let s = self.settings_mut();
                s.draft.album_art = !s.draft.album_art;
                Vec::new()
            }
            Field::AutoplayOnStart => {
                let s = self.settings_mut();
                s.draft.autoplay_on_start = !s.draft.autoplay_on_start;
                Vec::new()
            }
            Field::SearchSource => {
                let s = self.settings_mut();
                s.draft.search.source = s
                    .draft
                    .search
                    .cycled_source(s.draft.search.source, dir >= 0);
                self.status.text = format!(
                    "{}: {}",
                    t!("Search source", "검색 소스"),
                    s.draft.search.source.label()
                );
                Vec::new()
            }
            Field::SearchYoutube => {
                let s = self.settings_mut();
                let next = !s.draft.search.youtube;
                s.draft.search.set_enabled(SearchSource::Youtube, next);
                Vec::new()
            }
            Field::SearchSoundCloud => {
                let s = self.settings_mut();
                let next = !s.draft.search.soundcloud;
                s.draft.search.set_enabled(SearchSource::SoundCloud, next);
                Vec::new()
            }
            Field::SearchAudius => {
                let s = self.settings_mut();
                let next = !s.draft.search.audius;
                s.draft.search.set_enabled(SearchSource::Audius, next);
                Vec::new()
            }
            Field::SearchJamendo => {
                let s = self.settings_mut();
                let next = !s.draft.search.jamendo;
                s.draft.search.set_enabled(SearchSource::Jamendo, next);
                Vec::new()
            }
            Field::SearchInternetArchive => {
                let s = self.settings_mut();
                let next = !s.draft.search.internet_archive;
                s.draft
                    .search
                    .set_enabled(SearchSource::InternetArchive, next);
                Vec::new()
            }
            Field::SearchRadioBrowser => {
                let s = self.settings_mut();
                let next = !s.draft.search.radio_browser;
                s.draft.search.set_enabled(SearchSource::RadioBrowser, next);
                Vec::new()
            }
            Field::RetroMode => {
                self.settings_request_confirm(SettingsConfirm::RetroMode);
                Vec::new()
            }
            Field::Gapless => {
                let s = self.settings_mut();
                s.draft.gapless = !s.draft.gapless;
                Vec::new()
            }
            Field::AutoplayRadio => {
                let s = self.settings_mut();
                s.draft.autoplay_radio = !s.draft.autoplay_radio;
                Vec::new()
            }
            Field::RadioMode => {
                let s = self.settings_mut();
                let next = s.draft.radio_mode.cycled(dir >= 0);
                s.draft.radio_mode = next;
                self.status.text = format!("{}: {}", t!("Radio mode", "라디오 모드"), next.label());
                Vec::new()
            }
            Field::Language => {
                let s = self.settings_mut();
                if s.draft.retro_mode {
                    s.draft.language = crate::i18n::Language::English;
                    crate::i18n::set_language(crate::i18n::Language::English);
                    self.status.text = t!(
                        "Retro mode keeps the UI in English",
                        "레트로 모드는 UI를 영어로 유지합니다"
                    )
                    .to_owned();
                    return Vec::new();
                }
                let next = s.draft.language.cycled(dir >= 0);
                s.draft.language = next;
                // Apply live so the whole UI — including this settings screen — re-renders in
                // the new language on the next frame; `close_settings` persists it.
                crate::i18n::set_language(next);
                self.status.text = format!("{}: {}", t!("Language", "언어"), next.native_name());
                Vec::new()
            }
            Field::Normalize => {
                let s = self.settings_mut();
                s.draft.normalize = !s.draft.normalize;
                self.settings_apply_af()
            }
            Field::Speed => {
                let s = self.settings_mut();
                s.draft.speed =
                    settings::clamp_speed(s.draft.speed + f64::from(dir) * settings::SPEED_STEP);
                self.settings_apply_speed()
            }
            Field::SeekInterval => {
                let s = self.settings_mut();
                s.draft.seek_seconds = settings::clamp_seek_seconds(
                    s.draft.seek_seconds + f64::from(dir) * settings::SEEK_SECONDS_STEP,
                );
                // Stored only — affects the next seek key, nothing to push to mpv now.
                Vec::new()
            }
            Field::MouseWheelVolume => {
                let s = self.settings_mut();
                s.draft.mouse_wheel_volume = !s.draft.mouse_wheel_volume;
                Vec::new()
            }
            Field::AnimFps => {
                let s = self.settings_mut();
                let next = (i32::from(s.draft.animations.fps)
                    + dir * i32::from(settings::ANIM_FPS_STEP))
                .clamp(
                    i32::from(crate::config::FPS_MIN),
                    i32::from(crate::config::FPS_MAX),
                );
                s.draft.animations.fps = next as u16;
                // Stored only — the main loop rebuilds the animation-tick interval from the saved
                // rate when Settings closes (it reads `config.animations.effective_fps()`).
                Vec::new()
            }
            Field::AnimPauseUnfocused => {
                let s = self.settings_mut();
                s.draft.animations.pause_unfocused = !s.draft.animations.pause_unfocused;
                Vec::new()
            }
            Field::EqPreset => {
                let s = self.settings_mut();
                // `Custom` isn't in CYCLE; rather than jump to a surprising neighbour,
                // the first ←/→ from a hand-tuned state snaps back to Flat (a clean,
                // known preset), and subsequent presses cycle normally.
                s.draft.eq_preset = if s.draft.eq_preset == EqPreset::Custom {
                    EqPreset::Flat
                } else {
                    let cur = EqPreset::CYCLE
                        .iter()
                        .position(|&p| p == s.draft.eq_preset)
                        .unwrap_or(0);
                    let n = EqPreset::CYCLE.len();
                    let next = if dir >= 0 {
                        (cur + 1) % n
                    } else {
                        (cur + n - 1) % n
                    };
                    EqPreset::CYCLE[next]
                };
                s.draft.eq_bands = s.draft.eq_preset.gains();
                self.settings_apply_af()
            }
            Field::Band(i) => self.settings_change_band(i, dir),
            Field::GeminiModel => {
                let s = self.settings_mut();
                s.draft.gemini_model = s.draft.gemini_model.cycled(dir >= 0);
                Vec::new()
            }
            Field::AiEnabled => {
                let s = self.settings_mut();
                s.draft.ai_enabled = !s.draft.ai_enabled;
                Vec::new()
            }
            Field::ThemePreset => {
                let s = self.settings_mut();
                if s.draft.retro_mode {
                    s.draft.theme.set_preset(crate::theme::ThemePreset::Retro);
                    self.theme = s.draft.theme.normalized();
                    self.status.text = t!(
                        "Retro mode keeps the Retro theme",
                        "레트로 모드는 레트로 테마를 유지합니다"
                    )
                    .to_owned();
                    return Vec::new();
                }
                let next = s.draft.theme.preset_enum().stepped(dir);
                s.draft.theme.set_preset(next);
                self.theme = s.draft.theme.normalized();
                self.status.text = format!("{}: {}", t!("Theme", "테마"), next.label());
                Vec::new()
            }
            // Toggle the background between the preset's color and "no color" (transparent).
            // Mirrors the color editor's live-preview path so the change shows immediately.
            Field::BackgroundNone => {
                let s = self.settings_mut();
                if s.draft.theme.is_role_transparent(ThemeRole::Background) {
                    s.draft.theme.reset_role(ThemeRole::Background);
                } else {
                    let _ = s
                        .draft
                        .theme
                        .set_override(ThemeRole::Background, crate::theme::TRANSPARENT);
                }
                self.theme = s.draft.theme.normalized();
                Vec::new()
            }
            // Animation toggles: flip the mapped flag in the draft. The single `anim_flag`
            // mapping keeps these 13 flags (master + 12 effects) in lock-step across
            // display/toggle/persist; the UI
            // takes effect immediately because the player reads `config.animations` each frame
            // and the draft is what's live while the screen is open (committed on close).
            Field::AnimMaster
            | Field::AnimTitle
            | Field::AnimHeart
            | Field::AnimSeekbar
            | Field::AnimSpinner
            | Field::AnimEqBars
            | Field::AnimControls
            | Field::AnimBorder
            | Field::AnimRain
            | Field::AnimDonut
            | Field::AnimVisualizer
            | Field::AnimStarfield
            | Field::AnimBounce => {
                let s = self.settings_mut();
                if let Some(flag) = field.anim_flag(&mut s.draft.animations) {
                    *flag = !*flag;
                }
                Vec::new()
            }
            // Text fields ignore ←/→; Enter starts editing instead. The reset button has no
            // value to nudge — Enter activates it (see `settings_activate`).
            Field::CookiesFile
            | Field::DownloadDir
            | Field::AudiusAppName
            | Field::JamendoClientId
            | Field::ApiKey
            | Field::ThemeColor(_)
            | Field::ResetKeybindings
            | Field::ResetAll => Vec::new(),
        }
    }

    /// Adjust one EQ band. Uses a glitch-free `af-command` when the labeled chain already
    /// exists; otherwise rebuilds the chain (which creates or clears the `@eqN` labels).
    pub(in crate::app) fn settings_change_band(&mut self, i: usize, dir: i32) -> Vec<Cmd> {
        let Some(st) = self.settings.as_mut() else {
            return Vec::new();
        };
        let was_active = st.draft.eq_bands.iter().any(|g| g.abs() > f64::EPSILON);
        let gain =
            settings::clamp_band(st.draft.eq_bands[i] + f64::from(dir) * settings::BAND_GAIN_STEP);
        st.draft.eq_bands[i] = gain;
        st.draft.eq_preset = EqPreset::Custom;
        let bands = st.draft.eq_bands;
        let normalize = st.draft.normalize;
        let now_active = bands.iter().any(|g| g.abs() > f64::EPSILON);
        if was_active && now_active {
            vec![Cmd::Player(PlayerCmd::AfCommand {
                label: eq::band_label(i),
                param: "gain".to_owned(),
                value: format!("{gain}"),
            })]
        } else {
            vec![Cmd::Player(PlayerCmd::SetAudioFilter(
                eq::build_af_string(&bands, normalize).unwrap_or_default(),
            ))]
        }
    }

    /// Rebuild and apply the EQ/normalization chain from the current draft.
    pub(in crate::app) fn settings_apply_af(&self) -> Vec<Cmd> {
        let Some(st) = self.settings.as_ref() else {
            return Vec::new();
        };
        vec![Cmd::Player(PlayerCmd::SetAudioFilter(
            eq::build_af_string(&st.draft.eq_bands, st.draft.normalize).unwrap_or_default(),
        ))]
    }

    /// Apply the draft's playback speed.
    pub(in crate::app) fn settings_apply_speed(&self) -> Vec<Cmd> {
        let Some(st) = self.settings.as_ref() else {
            return Vec::new();
        };
        vec![Cmd::Player(PlayerCmd::SetProperty {
            name: "speed".to_owned(),
            value: serde_json::Value::from(st.draft.speed),
        })]
    }

    /// Enter (Enter key): start editing a text field, or flip a toggle.
    pub(in crate::app) fn settings_activate(&mut self) -> Vec<Cmd> {
        let Some(field) = self.settings.as_ref().and_then(|s| s.current_field()) else {
            return Vec::new();
        };
        match field.kind() {
            FieldKind::Text => {
                let st = self.settings_mut();
                if let Field::ThemeColor(role) = field {
                    st.draft.theme.ensure_override_for_edit(role);
                }
                if field == Field::AudiusAppName && st.draft.search.audius_app_name.is_none() {
                    st.draft.search.audius_app_name = Some(String::new());
                }
                if field == Field::JamendoClientId && st.draft.search.jamendo_client_id.is_none() {
                    st.draft.search.jamendo_client_id = Some(String::new());
                }
                // A secret field (the API key) is masked, so editing in place is blind —
                // appending to the hidden value silently corrupts it. Start fresh: clear
                // the buffer so the user types/pastes a whole new key, but remember the
                // prior value so committing without typing restores it (no accidental wipe).
                if field.is_secret() {
                    st.secret_restore = Self::settings_text_buf(st).map(|buf| {
                        let prev = buf.clone();
                        buf.clear();
                        prev
                    });
                }
                st.editing_text = true;
                self.dirty = true;
                Vec::new()
            }
            FieldKind::Toggle => self.settings_change(1),
            FieldKind::Button => match field {
                Field::ResetKeybindings => {
                    self.settings_request_confirm(SettingsConfirm::ResetKeybindings);
                    Vec::new()
                }
                Field::ResetAll => {
                    self.settings_request_confirm(SettingsConfirm::ResetAll);
                    Vec::new()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn settings_request_confirm(&mut self, confirm: SettingsConfirm) {
        self.pending_settings_confirm = Some(confirm);
        self.status.text.clear();
        self.dirty = true;
    }

    pub(in crate::app) fn settings_apply_confirm(&mut self, confirm: SettingsConfirm) -> Vec<Cmd> {
        self.pending_settings_confirm = None;
        match confirm {
            SettingsConfirm::RetroMode => {
                self.settings_toggle_retro_mode();
                Vec::new()
            }
            SettingsConfirm::ResetKeybindings => self.settings_reset_keybindings(),
            SettingsConfirm::ResetAll => self.settings_reset_all(),
        }
    }

    pub(in crate::app) fn settings_toggle_retro_mode(&mut self) {
        let Some(st) = self.settings.as_mut() else {
            return;
        };
        st.draft.retro_mode = !st.draft.retro_mode;
        if st.draft.retro_mode {
            st.draft.theme = crate::theme::ThemeConfig::default();
            st.draft.theme.set_preset(crate::theme::ThemePreset::Retro);
            st.draft.language = crate::i18n::Language::English;
            self.theme = st.draft.theme.normalized();
            crate::i18n::set_language(crate::i18n::Language::English);
            self.status.text = t!(
                "Retro mode enabled: English + Retro theme",
                "레트로 모드 켜짐: 영어 + 레트로 테마"
            )
            .to_owned();
        } else {
            self.status.text = t!("Retro mode disabled", "레트로 모드 꺼짐").to_owned();
        }
        self.dirty = true;
    }

    /// Restore only the working keymap in Settings to built-in defaults. Like individual
    /// key edits, this is committed and persisted when the settings screen closes.
    pub(in crate::app) fn settings_reset_keybindings(&mut self) -> Vec<Cmd> {
        let Some(st) = self.settings.as_mut() else {
            return Vec::new();
        };
        st.keymap = KeyMap::default();
        st.capturing = None;
        self.status.text = t!(
            "Keybindings reset to defaults",
            "단축키를 기본값으로 되돌렸어요"
        )
        .to_owned();
        self.dirty = true;
        Vec::new()
    }

    /// Reset every editable setting (and the Keys-tab keymap draft) back to its built-in
    /// default. Mutates only the draft / working keymap — like any other settings edit, it
    /// is committed and persisted when the screen closes. Live audio (speed, EQ, normalize)
    /// is pushed to mpv immediately so the change is audible right away.
    pub(in crate::app) fn settings_reset_all(&mut self) -> Vec<Cmd> {
        {
            let Some(st) = self.settings.as_mut() else {
                return Vec::new();
            };
            let def = Config::default();
            let d = &mut st.draft;
            d.cookies_file = String::new();
            d.download_dir = String::new();
            d.search = def.effective_search();
            d.mouse = def.effective_mouse();
            d.album_art = def.effective_album_art();
            d.autoplay_on_start = def.effective_autoplay_on_start();
            d.speed = def.effective_speed();
            d.seek_seconds = def.effective_seek_seconds();
            d.gapless = def.effective_gapless();
            d.autoplay_radio = def.effective_autoplay_radio();
            d.radio_mode = def.radio.mode;
            d.eq_preset = def.eq_preset;
            d.eq_bands = def.effective_eq_bands();
            d.normalize = def.effective_normalize();
            d.gemini_model = def.effective_gemini_model();
            d.gemini_api_key = String::new();
            d.ai_enabled = def.effective_ai_enabled(); // back to on (don't strand AI off)
            d.theme = def.effective_theme();
            d.retro_mode = def.effective_retro_mode();
            d.language = def.effective_language();
            d.animations = def.animations; // all effects off (the lightweight default)
            st.keymap = KeyMap::default();
            st.editing_text = false;
        }
        // Reflect the reset theme + language live so the open settings screen re-colors and
        // re-translates immediately (the reset also restores the default English UI).
        if let Some(st) = self.settings.as_ref() {
            self.theme = st.draft.theme.normalized();
            crate::i18n::set_language(st.draft.language);
        }
        self.status.text = t!(
            "All settings reset to defaults",
            "모든 설정을 기본값으로 되돌렸어요"
        )
        .to_owned();
        self.dirty = true;
        let mut cmds = self.settings_apply_speed();
        cmds.extend(self.settings_apply_af());
        cmds
    }

    /// Feed one key into the focused text field's buffer. Committing the edit (Enter/Esc)
    /// also persists free-text config fields immediately, so a typed value — notably the
    /// Gemini API key — can never be lost by leaving the screen via Esc/q instead of `s`.
    pub(in crate::app) fn settings_edit_text(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let Some(field) = self.settings.as_ref().and_then(|s| s.current_field()) else {
            return Vec::new();
        };
        self.dirty = true;
        match k.code {
            KeyCode::Enter | KeyCode::Esc => {
                if let Field::ThemeColor(role) = field {
                    return self.settings_commit_color(role);
                }
                if let Some(st) = self.settings.as_mut() {
                    st.editing_text = false;
                    // Secret editor opened but left empty (no new key typed): restore the
                    // prior value rather than wiping the saved key.
                    if let Some(prev) = st.secret_restore.take()
                        && let Some(buf) = Self::settings_text_buf(st)
                        && buf.is_empty()
                    {
                        *buf = prev;
                    }
                }
                self.settings_persist_text_field(field)
            }
            KeyCode::Char(c) => {
                if let Some(st) = self.settings.as_mut()
                    && let Some(buf) = Self::settings_text_buf(st)
                {
                    buf.push(c);
                }
                Vec::new()
            }
            KeyCode::Backspace => {
                if let Some(st) = self.settings.as_mut()
                    && let Some(buf) = Self::settings_text_buf(st)
                {
                    buf.pop();
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Persist a free-text config field (cookies path, download dir, API key) to disk the
    /// moment its edit is committed. Other draft fields persist when the settings screen
    /// closes. A changed key also rebuilds the AI actor so it takes effect immediately.
    pub(in crate::app) fn settings_persist_text_field(&mut self, field: Field) -> Vec<Cmd> {
        let value = match self
            .settings
            .as_ref()
            .and_then(|s| s.draft.text_value(field))
        {
            Some(v) => v.to_owned(),
            None => return Vec::new(),
        };
        let mut cmds = Vec::new();
        match field {
            Field::CookiesFile => {
                self.config.cookies_file =
                    settings::blank_to_none(&value).map(std::path::PathBuf::from);
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::AudiusAppName => {
                self.config.search.audius_app_name = settings::blank_to_none(&value);
                if let Some(st) = self.settings.as_mut() {
                    st.draft.search.audius_app_name = self.config.search.audius_app_name.clone();
                    st.draft.search = st.draft.search.clone().normalized();
                }
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::JamendoClientId => {
                self.config.search.jamendo_client_id = settings::blank_to_none(&value);
                if let Some(st) = self.settings.as_mut() {
                    st.draft.search.jamendo_client_id =
                        self.config.search.jamendo_client_id.clone();
                    st.draft.search = st.draft.search.clone().normalized();
                }
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::DownloadDir => {
                let old_dir = self.config.effective_download_dir();
                self.config.download_dir =
                    settings::blank_to_none(&value).map(std::path::PathBuf::from);
                let new_dir = self.config.effective_download_dir();
                if new_dir != old_dir {
                    cmds.push(Cmd::SetDownloadDir(new_dir.clone()));
                    cmds.push(Cmd::ScanDownloads(new_dir));
                }
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::ApiKey => {
                let old_key = self.config.gemini_api_key.clone();
                self.config.gemini_api_key = settings::blank_to_none(&value);
                if self.config.gemini_api_key != old_key {
                    // Gate the live AI rebuild on the *draft's* enable toggle: it commits to
                    // `config.ai_enabled` only on close, so a key saved while AI is draft-disabled
                    // must not spawn the actor (and a key saved right after re-enabling AI in the
                    // draft should spawn it now). `close_settings` reconciles the final state.
                    let ai_on = self.settings.as_ref().is_some_and(|s| s.draft.ai_enabled);
                    cmds.push(Cmd::ReloadAi {
                        key: if ai_on {
                            self.config.effective_gemini_api_key()
                        } else {
                            None
                        },
                        model: self.ai.model,
                    });
                }
                self.status.text = t!("API key saved", "API 키를 저장했어요").to_owned();
            }
            Field::ThemeColor(_) => return Vec::new(),
            // Non-text fields never reach here (only Field::kind()==Text enters edit mode).
            _ => return Vec::new(),
        }
        cmds.push(Cmd::SaveConfig(Box::new(self.config.clone())));
        cmds
    }

    pub(in crate::app) fn settings_commit_color(&mut self, role: ThemeRole) -> Vec<Cmd> {
        let value = self
            .settings
            .as_ref()
            .and_then(|s| s.draft.text_value(Field::ThemeColor(role)))
            .unwrap_or_default()
            .to_owned();
        let Some(st) = self.settings.as_mut() else {
            return Vec::new();
        };
        match st.draft.theme.set_override(role, &value) {
            Ok(()) => {
                st.editing_text = false;
                self.theme = st.draft.theme.normalized();
                let label = role.label();
                let hex = st.draft.theme.effective_hex(role);
                self.status.text = if crate::i18n::is_korean() {
                    format!("{label} 을(를) {hex} 로 설정함")
                } else {
                    format!("Set {label} to {hex}")
                };
            }
            Err(msg) => {
                st.editing_text = true;
                self.status.text = msg;
            }
        }
        self.dirty = true;
        Vec::new()
    }

    /// The draft string backing the focused text field, if it is a text field.
    pub(in crate::app) fn settings_text_buf(st: &mut SettingsState) -> Option<&mut String> {
        match st.current_field()? {
            Field::CookiesFile => Some(&mut st.draft.cookies_file),
            Field::DownloadDir => Some(&mut st.draft.download_dir),
            Field::AudiusAppName => st.draft.search.audius_app_name.as_mut(),
            Field::JamendoClientId => st.draft.search.jamendo_client_id.as_mut(),
            Field::ApiKey => Some(&mut st.draft.gemini_api_key),
            Field::ThemeColor(role) => st.draft.theme.overrides.get_mut(role.id()),
            _ => None,
        }
    }

    pub(in crate::app) fn finish_settings_text_edit(&mut self) {
        let Some(st) = self.settings.as_mut() else {
            return;
        };
        if !st.editing_text {
            return;
        }
        st.editing_text = false;
        if let Some(prev) = st.secret_restore.take()
            && let Some(buf) = Self::settings_text_buf(st)
            && buf.is_empty()
        {
            *buf = prev;
        }
    }

    /// Leave the settings screen, copying the draft into live state + config and
    /// persisting it. This keeps `q`/Esc from silently discarding changed settings.
    pub(in crate::app) fn close_settings(&mut self) -> Vec<Cmd> {
        self.pending_settings_confirm = None;
        let Some(st) = self.settings.take() else {
            self.mode = Mode::Player;
            self.dirty = true;
            return Vec::new();
        };
        self.mode = Mode::Player;
        self.dirty = true;
        let d = &st.draft;
        self.playback.speed = d.speed;
        self.audio.seek_seconds = d.seek_seconds;
        self.audio.bands = d.eq_bands;
        self.audio.preset = d.eq_preset;
        self.audio.normalize = d.normalize;
        self.autoplay_radio = d.autoplay_radio;
        let model_changed = self.ai.model != d.gemini_model;
        self.ai.model = d.gemini_model;
        let old_key = self.config.gemini_api_key.clone();
        let old_ai_enabled = self.config.effective_ai_enabled();
        let old_download_dir = self.config.effective_download_dir();
        d.apply_to(&mut self.config);
        self.search.source = self.config.effective_search().source;
        // Commit the edited keybindings (live + persisted as compact overrides).
        self.keymap = st.keymap.clone();
        self.config.keybindings = self.keymap.to_overrides();
        self.theme = st.draft.theme.normalized();
        self.config.theme = self.theme.clone();
        let key_changed = self.config.gemini_api_key != old_key;
        let ai_enabled_changed = self.config.effective_ai_enabled() != old_ai_enabled;
        // Volume controls change the live value in place; fold it in so a save
        // doesn't persist the stale startup value.
        self.config.volume = self.playback.volume;
        self.sync_playback_modes_to_config();
        self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
        // Re-assert the committed audio chain before persisting: the draft was
        // previewing live, but a track change mid-edit (EOF auto-advance) would have
        // rebuilt mpv's chain from the *old* committed bands, so push the now-committed
        // chain to guarantee the current track matches what was just saved.
        let mut cmds = vec![
            Cmd::SaveConfig(Box::new(self.config.clone())),
            Cmd::Player(PlayerCmd::SetAudioFilter(
                self.current_af().unwrap_or_default(),
            )),
        ];
        // A changed key rebuilds the AI actor live (the client is otherwise built once
        // at spawn) — so a key entered at runtime takes effect now, no relaunch. The
        // rebuild already adopts the current model, so only hot-swap the model on the
        // running actor when the key itself didn't change.
        // A changed key *or* a flipped AI on/off switch rebuilds the actor: `effective_ai_key`
        // returns `None` when AI is off, so turning it off tears the actor down (and back on
        // respawns it) without discarding the saved key.
        if key_changed || ai_enabled_changed {
            cmds.push(Cmd::ReloadAi {
                key: self.config.effective_ai_key(),
                model: self.ai.model,
            });
        } else if model_changed {
            cmds.push(Cmd::SetAiModel(self.ai.model));
        }
        let new_download_dir = self.config.effective_download_dir();
        if new_download_dir != old_download_dir {
            cmds.push(Cmd::SetDownloadDir(new_download_dir.clone()));
            cmds.push(Cmd::ScanDownloads(new_download_dir));
        }
        // React to an album-art toggle. Turning it off drops the held image (frees RAM).
        // Turning it on fetches the current track's art live — but only when a protocol was
        // detected at startup (`artwork_source` gates on the picker); a first-time enable
        // with no picker takes effect next launch, as the field label says.
        if !self.config.effective_album_art() {
            self.clear_artwork();
        } else if let Some(song) = self.queue.current().cloned()
            && self.art.video_id.as_deref() != Some(song.video_id.as_str())
            && let Some(source) = self.artwork_source(&song)
        {
            self.art.loading = true;
            cmds.push(Cmd::FetchArtwork {
                video_id: song.video_id.clone(),
                source,
            });
        }
        cmds
    }
}
