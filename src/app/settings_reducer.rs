//! Settings-screen reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).
use super::*;

mod commit;
mod persist_text;
mod recording;
mod transfer;

/// Decide the Spotify account row's state from the saved token's embedded Client ID
/// (`None` = no token) and the configured Client ID. Returns
/// `(connected, stale, effective_client_id)`:
/// - `connected`: a token exists at all.
/// - `stale`: a token exists but the connection is *orphaned* — config has no Client ID,
///   or it no longer matches the token's. The row then offers a browser reconnect
///   instead of disconnect. (A merely-expired-but-matching token is *not* stale: it
///   refreshes on its own, and forcing re-auth there would drop the disconnect action.)
/// - `effective_client_id`: the configured Client ID, or — when config lost it — the one
///   recovered from the token, so a one-press reconnect has an ID to authorize with.
fn spotify_row_state(token_client_id: Option<&str>, cfg_client_id: &str) -> (bool, bool, String) {
    let cfg = cfg_client_id.trim();
    let connected = token_client_id.is_some();
    let effective = if cfg.is_empty() {
        token_client_id.unwrap_or_default().trim().to_owned()
    } else {
        cfg_client_id.to_owned()
    };
    let stale = token_client_id.is_some_and(|tok| {
        let tok = tok.trim();
        cfg.is_empty() || (!tok.is_empty() && cfg != tok)
    });
    (connected, stale, effective)
}

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
        let local_root = self.config.local.first_root();
        // Spotify connection state, decided once (one token-file read). See
        // `spotify_row_state`: recovers a lost Client ID from the token and flags an
        // orphaned connection so the row can offer a browser reconnect.
        let spotify_token = crate::spotify::auth::SpotifyToken::load();
        let cfg_spotify_client_id = self.config.spotify.client_id.clone().unwrap_or_default();
        let (spotify_connected, spotify_stale, spotify_client_id) = spotify_row_state(
            spotify_token.as_ref().map(|t| t.client_id.as_str()),
            &cfg_spotify_client_id,
        );
        let draft = SettingsDraft {
            beginner_mode: self.config.beginner_mode,
            restart_beginner_tutorial: false,
            cookies_file: path_str(&self.config.cookies_file),
            download_dir: path_str(&self.config.download_dir),
            local_include_download_dir: self.config.local.include_download_dir(),
            local_music_root: local_root
                .map(|root| root.path.display().to_string())
                .unwrap_or_default(),
            local_music_root_recursive: local_root
                .map(crate::config::LocalRootConfig::recursive)
                .unwrap_or(true),
            search: self.config.effective_search(),
            mouse: self.config.effective_mouse(),
            album_art: self.config.effective_album_art(),
            album_art_quality: self.config.album_art_quality,
            autoplay_on_start: self.config.effective_autoplay_on_start(),
            enqueue_next: self.config.effective_enqueue_next(),
            update_check_enabled: self.config.update_check_enabled,
            speed: self.playback.speed,
            seek_seconds: self.audio.seek_seconds,
            big_text: self.config.effective_text_zoom() > 100,
            big_text_percent: self.zoom.mode().big_percent(),
            mouse_wheel_volume: self.config.effective_mouse_wheel_volume(),
            gapless: self.config.effective_gapless(),
            media_controls: self.config.effective_media_controls(),
            auto_continue_videos: self.config.effective_auto_continue_videos(),
            video_layout: self.config.video_layout,
            player_bar_position: self.config.effective_player_bar_position(),
            audio_backend: self.config.audio.backend,
            audio_mpv_output: self.config.audio.mpv.output.clone().unwrap_or_default(),
            audio_mpv_device: self.config.audio.mpv.device.clone().unwrap_or_default(),
            long_form_seek_optimization: self.config.audio.mpv.long_form_seek_optimization,
            audio_mpv_cache_forward: self.config.audio.mpv.cache_forward.clone(),
            audio_mpv_cache_back: self.config.audio.mpv.cache_back.clone(),
            autoplay_streaming: self.autoplay_streaming,
            curating_mode: crate::streaming::CuratingMode::from_ai(
                self.config.streaming.ai.enabled,
            ),
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
            // The raw pick (incl. `Auto`), so the picker shows the user's actual choice; retro's
            // English override is applied at read time, not baked into the draft.
            dj_gem_language: self.config.dj_gem_language,
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
            spotify_client_id,
            spotify_redirect_port: self
                .config
                .spotify
                .redirect_port
                .map(|p| p.to_string())
                .unwrap_or_default(),
            spotify_import_mode: self.config.spotify.import_mode,
            // Connection state computed above; the display name arrives only when a
            // connect flow completes in this session.
            spotify_connected,
            spotify_stale,
            spotify_username: String::new(),
            recording_mode: self.config.recording.mode,
            recording_min_seconds: self.config.effective_recording_min(),
            recording_max_seconds: self.config.effective_recording_max(),
            recording_dir: path_str(&self.config.recording.track_directory),
            recording_past_tracks: self.config.effective_recording_past_tracks(),
            recording_notify: self.config.recording.notify,
        };
        self.settings = Some(Box::new(SettingsState {
            tab: SettingsTab::General,
            row: 0,
            draft,
            editing_text: false,
            text_cursor: TextCursor::default(),
            color_picker: None,
            secret_restore: None,
            keymap: self.keymap.clone(),
            mousemap: self.mousemap.clone(),
            capturing: None,
            spotify_import_mode_dropdown: None,
            personal_data_export: self.personal_export_status(),
            // Show the radio-recording item whenever the user is in a radio context —
            // dedicated Radio mode OR a radio station is currently loaded/playing (recording
            // runs on any station, not only in the dedicated UI), so it's never hidden when
            // it's actually usable.
            radio_mode: self.radio_dedicated_mode || self.current_is_radio_stream(),
        }));
        self.mode = Mode::Settings;
        self.overlays.pending_settings_confirm = None;
        self.status.text.clear();
        // Start every Settings session at the top; clear any offset left from a prior session.
        self.bridges.reset_settings_scroll();
        self.dirty = true;
    }

    pub(in crate::app) fn on_key_settings(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // The recording-settings popup fully owns input while open (checked before the base
        // text-edit path so the popup's own output-folder editor works). The recordings browser
        // is intercepted higher up in `on_key`, since it can also open over the player.
        if self.overlays.recording_settings.is_some() {
            return self.recording_settings_key(k);
        }
        if self
            .settings
            .as_ref()
            .is_some_and(|s| s.spotify_import_mode_dropdown.is_some())
        {
            return self.settings_spotify_import_mode_dropdown_key(k);
        }
        // While editing a text field, keys feed the buffer until Enter/Esc commits it.
        if self.settings.as_ref().is_some_and(|s| s.editing_text) {
            return self.settings_edit_text(k);
        }
        // The Spotify playlist picker overlay swallows navigation keys while open.
        if self.overlays.spotify_picker.is_some() {
            return self.spotify_picker_key(k);
        }
        let on_keys_tab = self
            .settings
            .as_ref()
            .is_some_and(|s| s.tab == SettingsTab::Keys);
        let on_mouse_binding = self.settings_current_mouse_binding().is_some();
        // A color row can now live anywhere (the Graphics tab), so the "reset color" key gates
        // on the focused field's type rather than a dedicated Colors tab.
        let on_color_field = self
            .settings
            .as_ref()
            .is_some_and(|s| matches!(s.current_field(), Some(Field::ThemeColor(_))));
        if on_color_field && k.code == KeyCode::Char(' ') && k.modifiers == KeyModifiers::NONE {
            self.settings_open_color_picker();
            return Vec::new();
        }
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
            Some(Action::ChangeDecrease) if on_mouse_binding => {
                self.settings_change_mouse_binding(-1);
                Vec::new()
            }
            Some(Action::ChangeIncrease) if on_mouse_binding => {
                self.settings_change_mouse_binding(1);
                Vec::new()
            }
            Some(Action::ChangeDecrease) if !on_keys_tab => self.settings_change(-1),
            Some(Action::ChangeIncrease) if !on_keys_tab => self.settings_change(1),
            Some(Action::Confirm) => {
                if on_mouse_binding {
                    self.settings_change_mouse_binding(1);
                    Vec::new()
                } else if on_keys_tab {
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
        if ctx == KeyContext::MpvOverlay && crate::keymap::chord_to_mpv_input(chord).is_none() {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "That key cannot be installed in mpv",
                "이 키는 mpv에 설정할 수 없어요"
            )
            .to_owned();
            return Vec::new();
        }
        if ctx == KeyContext::MpvOverlay
            && let Some(fixed_action) = crate::keymap::mpv_overlay_fixed_alias(chord)
            && fixed_action != action
        {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "That mpv compatibility key is reserved",
                "이 mpv 호환 키는 예약되어 있어요"
            )
            .to_owned();
            return Vec::new();
        }
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
                self.overlays.key_conflict = Some(conflict);
            }
        }
        Vec::new()
    }

    /// Reset the highlighted binding (Keys tab) to its built-in default.
    pub(in crate::app) fn settings_reset_binding(&mut self) {
        if self.settings_reset_mouse_binding() {
            return;
        }
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
                    self.overlays.key_conflict = Some(conflict);
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
            st.spotify_import_mode_dropdown = None;
            // The new tab has a different row set; drop the old offset so it starts at the top.
            self.bridges.reset_settings_scroll();
            self.dirty = true;
        }
    }

    pub(in crate::app) fn settings_move_row(&mut self, delta: i32) {
        if let Some(st) = self.settings.as_mut() {
            // The Keys tab is a list of remappable bindings rather than `Field`s.
            let n = match st.tab {
                SettingsTab::Keys => {
                    (crate::keymap::editable_entries().len()
                        + crate::mousemap::MouseContext::ALL.len()
                            * crate::mousemap::MouseGesture::ALL.len()) as i32
                }
                _ => st.fields().len() as i32,
            };
            st.row = (st.row as i32 + delta).clamp(0, n.max(1) - 1) as usize;
            st.editing_text = false;
            st.spotify_import_mode_dropdown = None;
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
            Field::BeginnerMode => {
                let s = self.settings_mut();
                let was_enabled = s.draft.beginner_mode;
                s.draft.beginner_mode = !was_enabled;
                if !was_enabled {
                    s.draft.restart_beginner_tutorial = true;
                }
                Vec::new()
            }
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
            Field::AutoContinueVideos => {
                let s = self.settings_mut();
                s.draft.auto_continue_videos = !s.draft.auto_continue_videos;
                Vec::new()
            }
            Field::UpdateCheck => {
                let s = self.settings_mut();
                s.draft.update_check_enabled = !s.draft.update_check_enabled;
                Vec::new()
            }
            Field::LocalIncludeDownloadDir => {
                let s = self.settings_mut();
                s.draft.local_include_download_dir = !s.draft.local_include_download_dir;
                Vec::new()
            }
            Field::LocalMusicRootRecursive => {
                let s = self.settings_mut();
                s.draft.local_music_root_recursive = !s.draft.local_music_root_recursive;
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
                // Local Deck is offline. The draft carries the saved normal-mode preference, so
                // leave it untouched and explain why this control is inactive in Local mode.
                if self.local_dedicated_mode {
                    self.status.text = t!(
                        "Autoplay stays off in Local Deck",
                        "로컬 덱에서는 자동재생이 꺼져 있어요"
                    )
                    .to_owned();
                    self.dirty = true;
                    return Vec::new();
                }
                let current = self.settings_mut().draft.autoplay_streaming;
                let transition = PlaybackModeState::new(self.queue.repeat, current)
                    .transition(PlaybackModeAction::SetStreaming(!current));
                let Ok(transition) = transition else {
                    self.show_streaming_repeat_conflict();
                    return Vec::new();
                };
                let s = self.settings_mut();
                s.draft.autoplay_streaming = transition.state.autoplay_streaming;
                Vec::new()
            }
            Field::CuratingMode => {
                let s = self.settings_mut();
                let next = s.draft.curating_mode.cycled(dir >= 0);
                s.draft.curating_mode = next;
                self.status.text =
                    format!("{}: {}", t!("Curating mode", "큐레이팅 방식"), next.label());
                Vec::new()
            }
            Field::StreamingMode => {
                let s = self.settings_mut();
                let next = s.draft.streaming_mode.cycled(dir >= 0);
                s.draft.streaming_mode = next;
                self.status.text = format!(
                    "{}: {}",
                    t!("Curating style", "큐레이팅 스타일"),
                    next.label()
                );
                Vec::new()
            }
            Field::SpotifyImportMode => {
                let s = self.settings_mut();
                let next = s.draft.spotify_import_mode.cycled(dir >= 0);
                s.draft.spotify_import_mode = next;
                s.spotify_import_mode_dropdown = None;
                self.status.text = format!(
                    "{}: {}",
                    t!("Spotify import mode", "Spotify 가져오기 모드"),
                    next.label()
                );
                Vec::new()
            }
            Field::VideoLayout => {
                let s = self.settings_mut();
                // Only the open default; `Shift+V` still cycles the live window. Persists on save
                // via `apply_to`, like every other Select field.
                let next = s.draft.video_layout.cycled(dir >= 0);
                s.draft.video_layout = next;
                self.status.text = format!("{}: {}", t!("Video window", "영상 창"), next.label());
                Vec::new()
            }
            Field::AlbumArtQuality => {
                let s = self.settings_mut();
                let next = s.draft.album_art_quality.cycled(dir >= 0);
                s.draft.album_art_quality = next;
                self.status.text = format!(
                    "{}: {}",
                    t!("Album art quality", "앨범 아트 화질"),
                    next.label()
                );
                Vec::new()
            }
            Field::PlayerBarPosition => {
                let s = self.settings_mut();
                // Two states, so both cycle directions agree. The draft previews live
                // (`App::player_bar_position` reads it), so moving the bar relocates the
                // album-art rect immediately — ask for a native-image clear or the old
                // anchor cells keep the stale kitty/sixel bytes.
                let next = s.draft.player_bar_position.toggled();
                s.draft.player_bar_position = next;
                if self.art_active() {
                    self.request_native_image_clear();
                }
                self.status.text = format!(
                    "{}: {}",
                    t!("Player bar position", "플레이어 바 위치"),
                    next.label()
                );
                Vec::new()
            }
            Field::AudioBackend => {
                self.status.text = t!("Audio backend: mpv", "오디오 백엔드: mpv").to_owned();
                Vec::new()
            }
            Field::LongFormSeekOptimization => {
                let s = self.settings_mut();
                s.draft.long_form_seek_optimization =
                    s.draft.long_form_seek_optimization.cycled(dir >= 0);
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
            Field::DjGemLanguage => {
                let s = self.settings_mut();
                // Retro pins replies to English; keep the underlying pick so disabling retro
                // later restores it, and just explain the lock.
                if s.draft.retro_mode {
                    self.status.text = t!(
                        "Retro mode replies in English",
                        "레트로 모드는 영어로 답변합니다"
                    )
                    .to_owned();
                    return Vec::new();
                }
                let next = s.draft.dj_gem_language.cycled(dir >= 0);
                s.draft.dj_gem_language = next;
                // The resolved value is pushed to the AI actor on save (close_settings); no DJ Gem
                // request can fire while Settings is open, so there's nothing to update live here.
                self.status.text = format!(
                    "{}: {}",
                    t!("Reply language", "답변 언어"),
                    next.picker_label()
                );
                Vec::new()
            }
            Field::Normalize => self.settings_preview_normalize(),
            Field::Speed => self.settings_preview_speed(dir),
            Field::SeekInterval => {
                let s = self.settings_mut();
                s.draft.seek_seconds = settings::clamp_seek_seconds(
                    s.draft.seek_seconds + f64::from(dir) * settings::SEEK_SECONDS_STEP,
                );
                // Stored only — affects the next seek key, nothing to push to mpv now.
                Vec::new()
            }
            Field::BigText => {
                let s = self.settings_mut();
                s.draft.big_text = !s.draft.big_text;
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
            Field::EqPreset => self.settings_preview_eq_preset(dir),
            Field::Band(i) => self.settings_preview_band(i, dir),
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
            // mapping keeps these 41 flags (master + 40 effects) in lock-step across
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
            | Field::AnimTrackIntro
            | Field::AnimLyrics
            | Field::AnimToast
            | Field::AnimVolumeFlash
            | Field::AnimLikeBurst
            | Field::AnimSeekFlash
            | Field::AnimPauseFlash
            | Field::AnimErrorShake
            | Field::AnimSelection
            | Field::AnimStagger
            | Field::AnimCaret
            | Field::AnimTabs
            | Field::AnimPopupFade
            | Field::AnimActivity
            | Field::AnimAboutFx
            | Field::AnimTimeGlow
            | Field::AnimProgressSparkle
            | Field::AnimBorderChase
            | Field::AnimRain
            | Field::AnimDonut
            | Field::AnimVisualizer
            | Field::AnimStarfield
            | Field::AnimBounce
            | Field::AnimComets
            | Field::AnimSnow
            | Field::AnimFireflies
            | Field::AnimCube
            | Field::AnimAquarium
            | Field::AnimWaves
            | Field::AnimFireworks
            | Field::AnimLife
            | Field::AnimPipes
            | Field::AnimPlasma => {
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
            Field::CookiesFile
            | Field::DownloadDir
            | Field::LocalMusicRoot
            | Field::AudioMpvOutput
            | Field::AudioMpvDevice
            | Field::AudioMpvCacheForward
            | Field::AudioMpvCacheBack
            | Field::AudiusAppName
            | Field::JamendoClientId
            | Field::ApiKey
            | Field::ListenBrainzToken
            | Field::SpotifyClientId
            | Field::SpotifyRedirectPort
            | Field::ThemeColor(_)
            | Field::ExportPersonalData
            | Field::ResetKeybindings
            | Field::ResetAll
            | Field::ClearRomanizedTitleCache
            | Field::RadioRecording
            | Field::AudioOutput
            | Field::LastfmConnect
            | Field::SpotifyConnect
            | Field::SpotifyImport => Vec::new(),
        }
    }

    /// Enter (Enter key): start editing a text field, or flip a toggle.
    pub(in crate::app) fn settings_activate(&mut self) -> Vec<Cmd> {
        let Some(field) = self.settings.as_ref().and_then(|s| s.current_field()) else {
            return Vec::new();
        };
        match field.kind() {
            FieldKind::Text => {
                // The Gemini API key is a masked secret: activating it clears the buffer for a
                // fresh key, so gate edit-mode entry behind an explicit confirmation (a stray
                // Enter/click must not blank the saved key). The apply arm re-enters edit mode.
                if field == Field::ApiKey {
                    self.settings_request_confirm(SettingsConfirm::EditApiKey);
                    return Vec::new();
                }
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
                st.text_cursor = Self::settings_text_buf(st)
                    .map_or_else(TextCursor::default, |buf| TextCursor::at_end(buf));
                st.editing_text = true;
                self.dirty = true;
                Vec::new()
            }
            FieldKind::Toggle => self.settings_change(1),
            FieldKind::Select if field == Field::SpotifyImportMode => {
                self.settings_open_spotify_import_mode_dropdown();
                Vec::new()
            }
            FieldKind::Select if field == Field::LongFormSeekOptimization => {
                self.settings_change(1)
            }
            FieldKind::Button => match field {
                Field::AudioOutput => self.open_audio_output_picker(),
                Field::ExportPersonalData => self.start_personal_export_to_downloads(),
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
                        vec![Cmd::Scrobble(ScrobbleCmd::AuthStart)]
                    }
                }
                Field::SpotifyConnect => {
                    let (connected, stale, client_id, port) = {
                        let st = self.settings_mut();
                        (
                            st.draft.spotify_connected,
                            st.draft.spotify_stale,
                            st.draft.spotify_client_id.trim().to_owned(),
                            st.draft
                                .spotify_redirect_port
                                .trim()
                                .parse::<u16>()
                                .unwrap_or(crate::config::SPOTIFY_REDIRECT_PORT_DEFAULT),
                        )
                    };
                    // A healthy connection disconnects. Everything else — no token, or an
                    // orphaned/stale one — takes the browser (re)connect path instead, so
                    // the user always has a way to open the approval page.
                    if connected && !stale {
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
                    self.status.text = if stale {
                        t!("Reconnecting Spotify…", "Spotify 재연결 중…").to_owned()
                    } else {
                        t!(
                            "Starting Spotify authorization…",
                            "Spotify 인증을 시작합니다…"
                        )
                        .to_owned()
                    };
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
                    // Re-derive from the token file, not the draft snapshot: a CLI `ytt auth
                    // spotify` run while this screen was open never refreshed the draft.
                    let token = crate::spotify::auth::SpotifyToken::load();
                    let connected = token.is_some();
                    let cfg_cid = self.config.spotify.client_id.clone().unwrap_or_default();
                    let stale =
                        spotify_row_state(token.as_ref().map(|t| t.client_id.as_str()), &cfg_cid).1;
                    if let Some(st) = self.settings.as_mut() {
                        st.draft.spotify_connected = connected;
                        st.draft.spotify_stale = stale;
                    }
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
                Field::RadioRecording => {
                    self.overlays.recording_settings = Some(RecordingSettingsPopup::default());
                    self.dirty = true;
                    Vec::new()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod spotify_state_tests {
    use super::spotify_row_state;

    #[test]
    fn no_token_is_not_connected() {
        let (connected, stale, cid) = spotify_row_state(None, "");
        assert!(!connected);
        assert!(!stale);
        assert_eq!(cid, "");
        // A configured Client ID with no token is still "not connected", not stale.
        let (connected, stale, cid) = spotify_row_state(None, "cfg-id");
        assert!(!connected);
        assert!(!stale);
        assert_eq!(cid, "cfg-id");
    }

    #[test]
    fn matching_client_id_is_healthy_connected() {
        // Config knows the same Client ID the token was minted with → disconnect, not reconnect.
        let (connected, stale, cid) = spotify_row_state(Some("app-123"), "app-123");
        assert!(connected);
        assert!(!stale);
        assert_eq!(cid, "app-123");
        // Whitespace differences don't count as a mismatch.
        let (_, stale, _) = spotify_row_state(Some("app-123"), "  app-123  ");
        assert!(!stale);
    }

    #[test]
    fn orphaned_client_id_is_stale_and_recovers_from_token() {
        // The reported bug: config lost the Client ID but the token still embeds it.
        let (connected, stale, cid) = spotify_row_state(Some("app-123"), "");
        assert!(connected);
        assert!(stale, "orphaned connection should offer reconnect");
        assert_eq!(cid, "app-123", "Client ID recovered from the token");
    }

    #[test]
    fn mismatched_client_id_is_stale_but_keeps_configured_value() {
        // Config points at a different app than the saved token → reconnect.
        let (connected, stale, cid) = spotify_row_state(Some("token-app"), "config-app");
        assert!(connected);
        assert!(stale);
        assert_eq!(cid, "config-app");
    }

    #[test]
    fn token_without_embedded_id_is_not_stale_when_config_has_one() {
        // Legacy token with no embedded Client ID: nothing to mismatch against.
        let (connected, stale, cid) = spotify_row_state(Some(""), "config-app");
        assert!(connected);
        assert!(!stale);
        assert_eq!(cid, "config-app");
    }
}
