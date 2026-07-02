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
        self.dropdowns.streaming_open = false;
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
            enqueue_next: self.config.effective_enqueue_next(),
            speed: self.playback.speed,
            seek_seconds: self.audio.seek_seconds,
            mouse_wheel_volume: self.config.effective_mouse_wheel_volume(),
            gapless: self.config.effective_gapless(),
            media_controls: self.config.effective_media_controls(),
            autoplay_streaming: self.autoplay_streaming,
            streaming_mode: self.config.streaming.mode,
            eq_preset: self.audio.preset,
            eq_bands: self.audio.bands,
            normalize: self.audio.normalize,
            gemini_model: self.ai.model,
            // Deliberately the *raw* config key, not `effective_gemini_api_key()`: seeding the
            // env-provided value would let a save copy it into config.json (persisting a key
            // the user chose to keep only in the environment). The cost is that an env-only
            // key shows "(none)" here; the DJ Gem still works and README documents env-wins.
            gemini_api_key: self.config.gemini_api_key.clone().unwrap_or_default(),
            ai_enabled: self.config.effective_ai_enabled(),
            romanized_titles: self.config.effective_romanized_titles(),
            theme: self.theme.clone(),
            retro_mode: self.config.effective_retro_mode(),
            language: self.config.effective_language(),
            animations: self.config.animations,
            lastfm_enabled: self.config.scrobble.lastfm.enabled.unwrap_or(true),
            lastfm_love_sync: self.config.scrobble.lastfm.love_sync.unwrap_or(true),
            lastfm_session_key: self
                .config
                .scrobble
                .lastfm
                .session_key
                .clone()
                .unwrap_or_default(),
            lastfm_username: self
                .config
                .scrobble
                .lastfm
                .username
                .clone()
                .unwrap_or_default(),
            listenbrainz_enabled: self.config.scrobble.listenbrainz.enabled.unwrap_or(true),
            listenbrainz_token: self
                .config
                .scrobble
                .listenbrainz
                .token
                .clone()
                .unwrap_or_default(),
            scrobble_local_files: self.config.effective_scrobble_local_files(),
            spotify_client_id: self.config.spotify.client_id.clone().unwrap_or_default(),
            spotify_redirect_port: self
                .config
                .spotify
                .redirect_port
                .map(|p| p.to_string())
                .unwrap_or_default(),
            // Connection state = "a token file exists"; the display name arrives only
            // when a connect flow completes in this session.
            spotify_connected: crate::spotify::auth::SpotifyToken::load().is_some(),
            spotify_username: String::new(),
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
        // The Spotify playlist picker overlay swallows navigation keys while open.
        if self.spotify_picker.is_some() {
            return self.spotify_picker_key(k);
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
        let retro = self.retro_mode();
        let Some(st) = self.settings.as_mut() else {
            return Vec::new();
        };
        match st.keymap.rebind(ctx, action, chord) {
            Ok(()) => {
                let label = action.human_label();
                let chord = crate::keymap::format_chord_for_display(chord, retro);
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
                _ => st.fields().len() as i32,
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
            Field::MediaControls => {
                let s = self.settings_mut();
                s.draft.media_controls = !s.draft.media_controls;
                Vec::new()
            }
            Field::AutoplayOnStart => {
                let s = self.settings_mut();
                s.draft.autoplay_on_start = !s.draft.autoplay_on_start;
                Vec::new()
            }
            Field::EnqueueNext => {
                let s = self.settings_mut();
                s.draft.enqueue_next = !s.draft.enqueue_next;
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
            Field::StreamingSource => {
                let s = self.settings_mut();
                s.draft.search.streaming_source = s
                    .draft
                    .search
                    .cycled_streaming_source(s.draft.search.streaming_source, dir >= 0);
                self.status.text = format!(
                    "{}: {}",
                    t!("Streaming source", "추천 소스"),
                    s.draft
                        .search
                        .normalized_streaming_source(s.draft.search.streaming_source)
                        .label()
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
            Field::AutoplayStreaming => {
                let s = self.settings_mut();
                s.draft.autoplay_streaming = !s.draft.autoplay_streaming;
                Vec::new()
            }
            Field::StreamingMode => {
                let s = self.settings_mut();
                let next = s.draft.streaming_mode.cycled(dir >= 0);
                s.draft.streaming_mode = next;
                self.status.text = format!(
                    "{}: {}",
                    t!("Streaming mode", "스트리밍 모드"),
                    next.label()
                );
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
            Field::RomanizedTitles => {
                let s = self.settings_mut();
                s.draft.romanized_titles = !s.draft.romanized_titles;
                Vec::new()
            }
            Field::ThemePreset => {
                let s = self.settings_mut();
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
            Field::LastfmEnabled => {
                let s = self.settings_mut();
                s.draft.lastfm_enabled = !s.draft.lastfm_enabled;
                Vec::new()
            }
            Field::LastfmLoveSync => {
                let s = self.settings_mut();
                s.draft.lastfm_love_sync = !s.draft.lastfm_love_sync;
                Vec::new()
            }
            Field::ListenBrainzEnabled => {
                let s = self.settings_mut();
                s.draft.listenbrainz_enabled = !s.draft.listenbrainz_enabled;
                Vec::new()
            }
            Field::ScrobbleLocalFiles => {
                let s = self.settings_mut();
                s.draft.scrobble_local_files = !s.draft.scrobble_local_files;
                Vec::new()
            }
            // Text fields ignore ←/→; Enter starts editing instead. The reset button has no
            // value to nudge — Enter activates it (see `settings_activate`).
            Field::CookiesFile
            | Field::DownloadDir
            | Field::AudiusAppName
            | Field::JamendoClientId
            | Field::ApiKey
            | Field::ListenBrainzToken
            | Field::SpotifyClientId
            | Field::SpotifyRedirectPort
            | Field::ThemeColor(_)
            | Field::ResetKeybindings
            | Field::ResetAll
            | Field::ClearRomanizedTitleCache
            | Field::LastfmConnect
            | Field::SpotifyConnect
            | Field::SpotifyImport => Vec::new(),
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
                Field::ClearRomanizedTitleCache => {
                    self.settings_request_confirm(SettingsConfirm::ClearRomanizedTitleCache);
                    Vec::new()
                }
                Field::LastfmConnect => {
                    let connected = self
                        .settings
                        .as_ref()
                        .is_some_and(|s| !s.draft.lastfm_session_key.trim().is_empty());
                    if connected {
                        self.settings_request_confirm(SettingsConfirm::LastfmDisconnect);
                        Vec::new()
                    } else {
                        self.status.text =
                            t!("Requesting Last.fm authorization…", "Last.fm 인증 요청 중…")
                                .to_owned();
                        self.status.kind = StatusKind::Info;
                        self.dirty = true;
                        vec![Cmd::ScrobbleAuthStart]
                    }
                }
                Field::SpotifyConnect => {
                    let (connected, client_id, port) = {
                        let st = self.settings_mut();
                        (
                            st.draft.spotify_connected,
                            st.draft.spotify_client_id.trim().to_owned(),
                            st.draft
                                .spotify_redirect_port
                                .trim()
                                .parse::<u16>()
                                .unwrap_or(crate::config::SPOTIFY_REDIRECT_PORT_DEFAULT),
                        )
                    };
                    if connected {
                        self.settings_request_confirm(SettingsConfirm::SpotifyDisconnect);
                        return Vec::new();
                    }
                    self.dirty = true;
                    if client_id.is_empty() {
                        self.status.text = t!(
                            "Set a Client ID first (create an app at developer.spotify.com)",
                            "먼저 클라이언트 ID를 입력하세요 (developer.spotify.com에서 앱 생성)"
                        )
                        .to_owned();
                        self.status.kind = StatusKind::Error;
                        return Vec::new();
                    }
                    self.status.text = t!(
                        "Starting Spotify authorization…",
                        "Spotify 인증을 시작합니다…"
                    )
                    .to_owned();
                    self.status.kind = StatusKind::Info;
                    vec![Cmd::Transfer(
                        crate::transfer::actor::TransferCmd::AuthStart { client_id, port },
                    )]
                }
                Field::SpotifyImport => {
                    self.dirty = true;
                    // While a job runs, the same button cancels it (the checkpoint
                    // survives — `ytt transfer resume` can pick it back up).
                    if self.transfer_running {
                        self.transfer_running = false;
                        self.status.text =
                            t!("Cancelling the import…", "가져오기를 취소하는 중…").to_owned();
                        self.status.kind = StatusKind::Info;
                        return vec![Cmd::Transfer(
                            crate::transfer::actor::TransferCmd::CancelJob,
                        )];
                    }
                    let connected = self
                        .settings
                        .as_ref()
                        .is_some_and(|s| s.draft.spotify_connected);
                    if !connected {
                        self.status.text =
                            t!("Connect Spotify first", "먼저 Spotify를 연결해 주세요").to_owned();
                        self.status.kind = StatusKind::Error;
                        return Vec::new();
                    }
                    self.status.text = t!(
                        "Loading Spotify playlists…",
                        "Spotify 플레이리스트 불러오는 중…"
                    )
                    .to_owned();
                    self.status.kind = StatusKind::Info;
                    vec![Cmd::Transfer(
                        crate::transfer::actor::TransferCmd::ListSpotifyPlaylists,
                    )]
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// Keys while the Spotify playlist picker overlay is open (↑/↓/Enter/Esc).
    pub(in crate::app) fn spotify_picker_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let action = self
            .keymap
            .action(KeyContext::Settings, k.into())
            .or_else(|| Self::settings_safety_action(k));
        let Some(picker) = self.spotify_picker.as_mut() else {
            return Vec::new();
        };
        self.dirty = true;
        match action {
            Some(Action::MoveUp) => {
                picker.selected = picker.selected.saturating_sub(1);
                Vec::new()
            }
            Some(Action::MoveDown) => {
                picker.selected = (picker.selected + 1).min(picker.items.len().saturating_sub(1));
                Vec::new()
            }
            Some(Action::Confirm) => {
                let Some(item) = picker.items.get(picker.selected).cloned() else {
                    return Vec::new();
                };
                self.spotify_picker = None;
                self.transfer_running = true;
                // The TUI can't browse account playlists, so the picker lands imports in
                // the app's own Library playlists — playable the moment the job finishes.
                // (`ytt transfer` still targets the YTM account by default.)
                let dest = match item.source {
                    crate::transfer::TransferSource::SpotifyLiked => {
                        crate::transfer::TransferDest::YtmLikes
                    }
                    _ => crate::transfer::TransferDest::LocalPlaylist { name: None },
                };
                let spec = crate::transfer::JobSpec {
                    source: item.source,
                    dest,
                    dry_run: false,
                    min_score: 0.80,
                    take_best: false,
                    rematch: false,
                };
                self.status.text = if crate::i18n::is_korean() {
                    format!("가져오는 중: {}", item.label)
                } else {
                    format!("Importing: {}", item.label)
                };
                self.status.kind = StatusKind::Info;
                vec![Cmd::Transfer(
                    crate::transfer::actor::TransferCmd::StartJob(Box::new(spec)),
                )]
            }
            Some(Action::SettingsCancel | Action::Back) => {
                self.spotify_picker = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Auth/listing/job events from the transfer actor.
    pub(in crate::app) fn on_transfer_event(
        &mut self,
        event: crate::transfer::actor::TransferEvent,
    ) -> Vec<Cmd> {
        use crate::transfer::actor::TransferEvent;
        self.dirty = true;
        match event {
            TransferEvent::AuthUrl(url) => {
                open_in_browser(&url);
                self.status.text = t!(
                    "Approve ytm-tui in the browser…",
                    "브라우저에서 ytm-tui를 승인해 주세요…"
                )
                .to_owned();
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::AuthDone { display_name } => {
                if let Some(st) = self.settings.as_mut() {
                    st.draft.spotify_connected = true;
                    st.draft.spotify_username = display_name.clone();
                }
                self.status.text = if crate::i18n::is_korean() {
                    format!("Spotify 연결됨: {display_name}")
                } else {
                    format!("Spotify connected as {display_name}")
                };
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::AuthError(error) => {
                self.status.text = format!(
                    "{}: {}",
                    t!("Spotify authorization failed", "Spotify 인증 실패"),
                    crate::util::sanitize::sanitize_error_text(error)
                );
                self.status.kind = StatusKind::Error;
            }
            TransferEvent::Disconnected => {
                if let Some(st) = self.settings.as_mut() {
                    st.draft.spotify_connected = false;
                    st.draft.spotify_username.clear();
                }
                self.status.text =
                    t!("Spotify disconnected", "Spotify 연결을 해제했어요").to_owned();
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::SpotifyPlaylists(Ok(items)) => {
                if items.is_empty() {
                    self.status.text =
                        t!("No Spotify playlists", "Spotify 플레이리스트 없음").to_owned();
                    self.status.kind = StatusKind::Info;
                } else {
                    self.status.text.clear();
                    self.spotify_picker =
                        Some(crate::app::state::SpotifyPicker { items, selected: 0 });
                }
            }
            TransferEvent::SpotifyPlaylists(Err(error)) => {
                self.status.text = format!(
                    "{}: {}",
                    t!(
                        "Could not list Spotify playlists",
                        "Spotify 플레이리스트 조회 실패"
                    ),
                    crate::util::sanitize::sanitize_error_text(error)
                );
                self.status.kind = StatusKind::Error;
            }
            TransferEvent::Progress(p) => {
                self.transfer_running = true;
                self.status.text = if crate::i18n::is_korean() {
                    format!(
                        "Spotify 가져오기: {} {}/{} · {}",
                        p.stage.label(),
                        p.done,
                        p.total,
                        p.current
                    )
                } else {
                    format!(
                        "Spotify import: {} {}/{} · {}",
                        p.stage.label(),
                        p.done,
                        p.total,
                        p.current
                    )
                };
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::JobDone(report) => {
                self.transfer_running = false;
                // A local-dest job wrote playlists.json from the actor; reload so the
                // Library shows it now and a later in-app save can't clobber it. (The
                // app persists its own mutations immediately, so disk is the union — which
                // also means a just-deleted playlist reappears if the job re-created it.)
                self.playlists = crate::playlists::Playlists::load();
                // The store changed under the Playlists tab: drop a drill-down or pending
                // delete whose playlist vanished and re-clamp the cursor into the new rows.
                self.reconcile_playlists_reload();
                self.status.text = format!(
                    "{}: {}",
                    t!("Import finished", "가져오기 완료"),
                    report.render_text()
                );
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::JobFailed {
                job_id,
                error,
                resumable,
            } => {
                self.transfer_running = false;
                let error = crate::util::sanitize::sanitize_error_text(error);
                self.status.text = if resumable && !job_id.is_empty() {
                    format!(
                        "{}: {error} · ytt transfer resume {job_id}",
                        t!("Import interrupted", "가져오기 중단")
                    )
                } else {
                    format!("{}: {error}", t!("Import failed", "가져오기 실패"))
                };
                self.status.kind = StatusKind::Error;
            }
        }
        Vec::new()
    }

    /// Auth-flow progress and service-health notices from the scrobble actor.
    pub(in crate::app) fn on_scrobble_event(
        &mut self,
        event: crate::scrobble::ScrobbleEvent,
    ) -> Vec<Cmd> {
        use crate::scrobble::ScrobbleEvent;
        self.dirty = true;
        match event {
            ScrobbleEvent::AuthUrl(url) => {
                open_in_browser(&url);
                self.status.text = t!(
                    "Approve ytm-tui in the browser…",
                    "브라우저에서 ytm-tui를 승인해 주세요…"
                )
                .to_owned();
                self.status.kind = StatusKind::Info;
                Vec::new()
            }
            ScrobbleEvent::AuthDone {
                username,
                session_key,
            } => {
                self.config.scrobble.lastfm.session_key = Some(session_key.clone());
                self.config.scrobble.lastfm.username = Some(username.clone());
                // Mirror into the open draft too, or closing settings would clobber the
                // fresh session with the stale pre-connect values.
                if let Some(st) = self.settings.as_mut() {
                    st.draft.lastfm_session_key = session_key;
                    st.draft.lastfm_username = username.clone();
                }
                self.status.text = if crate::i18n::is_korean() {
                    format!("Last.fm 연결됨: {username}")
                } else {
                    format!("Last.fm connected as {username}")
                };
                self.status.kind = StatusKind::Info;
                vec![
                    Cmd::SaveConfig(Box::new(self.config.clone())),
                    Cmd::ScrobbleReconfigure(Box::new(self.config.scrobble_settings())),
                ]
            }
            ScrobbleEvent::AuthFailed(error) => {
                let error = crate::util::sanitize::sanitize_error_text(error);
                self.status.text = format!(
                    "{}: {error}",
                    t!("Last.fm authorization failed", "Last.fm 인증 실패")
                );
                self.status.kind = StatusKind::Error;
                Vec::new()
            }
            ScrobbleEvent::SessionInvalid(kind) => {
                self.status.text = if crate::i18n::is_korean() {
                    format!(
                        "{} 세션이 만료되었어요 — 설정 › 계정에서 다시 연결해 주세요",
                        kind.label()
                    )
                } else {
                    format!(
                        "{} session expired — reconnect in Settings › Accounts",
                        kind.label()
                    )
                };
                self.status.kind = StatusKind::Error;
                Vec::new()
            }
            ScrobbleEvent::QueueStalled { pending } => {
                self.status.text = if crate::i18n::is_korean() {
                    format!("스크로블 {pending}건이 전송 대기 중이에요")
                } else {
                    format!("{pending} scrobbles waiting to be delivered")
                };
                self.status.kind = StatusKind::Info;
                Vec::new()
            }
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
            SettingsConfirm::ClearRomanizedTitleCache => {
                self.settings_clear_romanized_title_cache()
            }
            SettingsConfirm::LastfmDisconnect => {
                if let Some(st) = self.settings.as_mut() {
                    st.draft.lastfm_session_key.clear();
                    st.draft.lastfm_username.clear();
                }
                self.config.scrobble.lastfm.session_key = None;
                self.config.scrobble.lastfm.username = None;
                self.status.text =
                    t!("Last.fm disconnected", "Last.fm 연결을 해제했어요").to_owned();
                self.status.kind = StatusKind::Info;
                vec![
                    Cmd::SaveConfig(Box::new(self.config.clone())),
                    Cmd::ScrobbleReconfigure(Box::new(self.config.scrobble_settings())),
                ]
            }
            SettingsConfirm::SpotifyDisconnect => {
                // The actor deletes the token file and answers with `Disconnected`,
                // which flips the draft's display state.
                vec![Cmd::Transfer(
                    crate::transfer::actor::TransferCmd::Disconnect,
                )]
            }
        }
    }

    pub(in crate::app) fn settings_toggle_retro_mode(&mut self) {
        let Some(st) = self.settings.as_mut() else {
            return;
        };
        st.draft.retro_mode = !st.draft.retro_mode;
        if st.draft.retro_mode {
            // Seed the Retro preset as a starting point (keeping any color overrides) —
            // unlike before, this is a one-time default: preset and colors stay freely
            // editable while retro mode is on, and turning retro off keeps whatever
            // theme is current instead of snapping back.
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

    /// Clear only the generated Latin-script display overlays. Source metadata, library rows,
    /// settings, and the Gemini API key are left untouched.
    pub(in crate::app) fn settings_clear_romanized_title_cache(&mut self) -> Vec<Cmd> {
        self.romanization.cache.clear();
        self.romanization.pending.clear();
        self.romanization.min_valid_request_id =
            self.romanization.next_request_id.saturating_add(1);
        self.status.text = t!(
            "Romanized title cache cleared",
            "로마자 제목 캐시를 삭제했어요"
        )
        .to_owned();
        self.dirty = true;
        vec![Cmd::ClearRomanizedTitles]
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
            d.enqueue_next = def.effective_enqueue_next();
            d.speed = def.effective_speed();
            d.seek_seconds = def.effective_seek_seconds();
            d.gapless = def.effective_gapless();
            d.autoplay_streaming = def.effective_autoplay_streaming();
            d.streaming_mode = def.streaming.mode;
            d.eq_preset = def.eq_preset;
            d.eq_bands = def.effective_eq_bands();
            d.normalize = def.effective_normalize();
            d.gemini_model = def.effective_gemini_model();
            d.gemini_api_key = String::new();
            d.ai_enabled = def.effective_ai_enabled(); // back to on (don't strand DJ Gem off)
            d.romanized_titles = def.effective_romanized_titles();
            d.theme = def.effective_theme();
            d.retro_mode = def.effective_retro_mode();
            d.language = def.effective_language();
            d.animations = def.animations; // all effects off (the lightweight default)
            // Accounts: behavior flags reset, but the connections themselves survive —
            // "reset settings" should not disconnect Last.fm or wipe the LB token.
            d.lastfm_enabled = true;
            d.lastfm_love_sync = true;
            d.listenbrainz_enabled = true;
            d.scrobble_local_files = true;
            d.spotify_redirect_port = String::new();
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
    /// closes. A changed key also rebuilds the DJ Gem actor so it takes effect immediately.
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
                    // Gate the live DJ Gem rebuild on the *draft's* enable toggle: it commits to
                    // `config.ai_enabled` only on close, so a key saved while DJ Gem is draft-disabled
                    // must not spawn the actor (and a key saved right after re-enabling DJ Gem in the
                    // draft should spawn it now). `close_settings` reconciles the final state.
                    let (ai_on, romanized_on) = self
                        .settings
                        .as_ref()
                        .map(|s| (s.draft.ai_enabled, s.draft.romanized_titles))
                        .unwrap_or_else(|| {
                            (
                                self.config.effective_ai_enabled(),
                                self.config.effective_romanized_titles(),
                            )
                        });
                    cmds.push(Cmd::ReloadAi {
                        key: if ai_on || romanized_on {
                            self.config.effective_gemini_api_key()
                        } else {
                            None
                        },
                        model: self.ai.model,
                        assistant_enabled: ai_on,
                    });
                    cmds.extend(self.request_current_surfaces_romanization());
                }
                self.status.text = t!("API key saved", "API 키를 저장했어요").to_owned();
            }
            Field::ListenBrainzToken => {
                self.config.scrobble.listenbrainz.token = settings::blank_to_none(&value);
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
                cmds.push(Cmd::ScrobbleReconfigure(Box::new(
                    self.config.scrobble_settings(),
                )));
            }
            Field::SpotifyClientId => {
                self.config.spotify.client_id = settings::blank_to_none(&value);
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::SpotifyRedirectPort => {
                self.config.spotify.redirect_port = value.trim().parse::<u16>().ok();
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
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
            Field::ListenBrainzToken => Some(&mut st.draft.listenbrainz_token),
            Field::SpotifyClientId => Some(&mut st.draft.spotify_client_id),
            Field::SpotifyRedirectPort => Some(&mut st.draft.spotify_redirect_port),
            Field::ThemeColor(role) => st.draft.theme.override_value_mut(role),
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
        self.autoplay_streaming = d.autoplay_streaming;
        let model_changed = self.ai.model != d.gemini_model;
        self.ai.model = d.gemini_model;
        let old_key = self.config.gemini_api_key.clone();
        let old_ai_enabled = self.config.effective_ai_enabled();
        let old_romanized_titles = self.config.effective_romanized_titles();
        let old_download_dir = self.config.effective_download_dir();
        let normal_theme = if self.radio_dedicated_mode {
            Some(
                self.normal_mode_theme
                    .clone()
                    .unwrap_or_else(|| self.config.effective_theme()),
            )
        } else {
            None
        };
        d.apply_to(&mut self.config);
        self.search.source = self.config.effective_search().source;
        // Commit the edited keybindings (live + persisted as compact overrides).
        self.keymap = st.keymap.clone();
        self.config.keybindings = self.keymap.to_overrides();
        self.theme = st.draft.theme.normalized();
        if self.radio_dedicated_mode {
            self.radio_mode_theme = Some(self.theme.clone());
            if let Some(normal_theme) = normal_theme {
                self.normal_mode_theme = Some(normal_theme.clone());
                self.config.theme = normal_theme;
            }
        } else {
            self.config.theme = self.theme.clone();
        }
        self.ensure_radio_mode_constraints();
        let key_changed = self.config.gemini_api_key != old_key;
        let ai_enabled_changed = self.config.effective_ai_enabled() != old_ai_enabled;
        let romanized_changed = self.config.effective_romanized_titles() != old_romanized_titles;
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
        // A changed key rebuilds the DJ Gem actor live (the client is otherwise built once
        // at spawn) — so a key entered at runtime takes effect now, no relaunch. The
        // rebuild already adopts the current model, so only hot-swap the model on the
        // running actor when the key itself didn't change.
        // A changed key *or* a flipped DJ Gem on/off switch rebuilds the actor: `effective_ai_key`
        // returns `None` when DJ Gem is off, so turning it off tears the actor down (and back on
        // respawns it) without discarding the saved key.
        if key_changed || ai_enabled_changed || romanized_changed {
            cmds.push(Cmd::ReloadAi {
                key: self.config.effective_ai_service_key(),
                model: self.ai.model,
                assistant_enabled: self.config.effective_ai_enabled(),
            });
            if key_changed || romanized_changed {
                cmds.extend(self.request_current_surfaces_romanization());
            }
        } else if model_changed {
            cmds.push(Cmd::SetAiModel(self.ai.model));
        }
        let new_download_dir = self.config.effective_download_dir();
        if new_download_dir != old_download_dir {
            cmds.push(Cmd::SetDownloadDir(new_download_dir.clone()));
            cmds.push(Cmd::ScanDownloads(new_download_dir));
        }
        // Hand the scrobble actor the committed account settings. Unconditional: the
        // snapshot is a few strings and the actor's swap is trivial, so this is cheaper
        // than tracking which of the scrobble fields changed.
        cmds.push(Cmd::ScrobbleReconfigure(Box::new(
            self.config.scrobble_settings(),
        )));
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
