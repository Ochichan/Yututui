//! Settings-screen reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).
use super::*;

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

fn transfer_done_status(report: &crate::transfer::checkpoint::TransferReport) -> String {
    if crate::i18n::is_korean() {
        format!(
            "가져오기 완료: {} · Library > Playlists에 저장됨 · 검토: Local Deck > Import Sessions 또는 ytt transfer session {}",
            report.render_text(),
            report.job_id
        )
    } else {
        format!(
            "Import finished: {} · saved in Library > Playlists · review: Local Deck > Import Sessions or ytt transfer session {}",
            report.render_text(),
            report.job_id
        )
    }
}

fn local_accept_write_done_status(
    report: &crate::transfer::checkpoint::TransferReport,
    accepted_count: u32,
) -> String {
    let review_left = report.ambiguous.len() as u32;
    let missing_left = report.not_found.len() as u32;
    if crate::i18n::is_korean() {
        format!(
            "임포트 세션 작성 완료: 후보 {}개 수락 · 준비 행 {}개 작성 · 검토 {}개 남음 · 누락 {}개 남음 · Library > Playlists",
            accepted_count, report.written, review_left, missing_left
        )
    } else {
        format!(
            "Import session written: {} candidate{} accepted · {} ready row{} written · {} review left · {} missing left · Library > Playlists",
            accepted_count,
            if accepted_count == 1 { "" } else { "s" },
            report.written,
            if report.written == 1 { "" } else { "s" },
            review_left,
            missing_left
        )
    }
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
                // Music-mode invariant: can't enable autoplay while repeat is on.
                let repeat_on = self.queue.repeat.is_on();
                if !self.settings_mut().draft.autoplay_streaming && repeat_on {
                    self.status.text = t!(
                        "Can't use autoplay while repeat is on",
                        "반복 재생 중에는 자동재생을 켤 수 없어요"
                    )
                    .to_owned();
                    self.dirty = true;
                    return Vec::new();
                }
                let s = self.settings_mut();
                s.draft.autoplay_streaming = !s.draft.autoplay_streaming;
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
            // mapping keeps these 26 flags (master + 25 effects) in lock-step across
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
            | Field::AnimSelection
            | Field::AnimStagger
            | Field::AnimCaret
            | Field::AnimTabs
            | Field::AnimPopupFade
            | Field::AnimActivity
            | Field::AnimAboutFx
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
                st.editing_text = true;
                self.dirty = true;
                Vec::new()
            }
            FieldKind::Toggle => self.settings_change(1),
            FieldKind::Select if field == Field::SpotifyImportMode => {
                self.settings_open_spotify_import_mode_dropdown();
                Vec::new()
            }
            FieldKind::Button => match field {
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

    /// Keys while the radio-recording settings popup is open. Rows: 0 mode · 1 min · 2 max ·
    /// 3 folder · 4 past-tracks · 5 notify · 6 browse. Edits go straight into the draft.
    pub(in crate::app) fn recording_settings_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        // The output-folder text field captures every key until Enter/Esc commits it.
        if self
            .overlays
            .recording_settings
            .as_ref()
            .is_some_and(|p| p.editing_dir)
        {
            return self.recording_dir_edit(k);
        }
        let action = self
            .keymap
            .action(KeyContext::Settings, k.into())
            .or_else(|| Self::settings_safety_action(k));
        if self.overlays.recording_settings.is_none() {
            return Vec::new();
        }
        self.dirty = true;
        match action {
            Some(Action::MoveUp) => {
                if let Some(p) = self.overlays.recording_settings.as_mut() {
                    p.row = p.row.saturating_sub(1);
                }
                Vec::new()
            }
            Some(Action::MoveDown) => {
                if let Some(p) = self.overlays.recording_settings.as_mut() {
                    p.row = (p.row + 1).min(RECORDING_POPUP_ROWS - 1);
                }
                Vec::new()
            }
            Some(Action::ChangeDecrease) => self.recording_settings_adjust(-1),
            Some(Action::ChangeIncrease) => self.recording_settings_adjust(1),
            Some(Action::Confirm) => self.recording_settings_confirm(),
            Some(Action::SettingsCancel | Action::Back) => {
                self.overlays.recording_settings = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Adjust the focused popup knob by one step (`dir >= 0` = increase). `pub(in crate::app)`
    /// so the mouse handler can reuse it for the `‹`/`›` arrow clicks.
    pub(in crate::app) fn recording_settings_adjust(&mut self, dir: i32) -> Vec<Cmd> {
        use crate::config::{
            RECORDING_MAX_SECONDS_MAX, RECORDING_MAX_SECONDS_MIN, RECORDING_MIN_SECONDS_MAX,
            RECORDING_MIN_SECONDS_MIN, RECORDING_PAST_TRACKS_MAX, RECORDING_PAST_TRACKS_MIN,
        };
        let row = self
            .overlays
            .recording_settings
            .as_ref()
            .map(|p| p.row)
            .unwrap_or(0);
        let up = dir >= 0;
        let Some(st) = self.settings.as_mut() else {
            return Vec::new();
        };
        let d = &mut st.draft;
        match row {
            0 => d.recording_mode = d.recording_mode.cycled(up),
            1 => {
                let step: i64 = 5;
                let v = d.recording_min_seconds as i64 + if up { step } else { -step };
                d.recording_min_seconds = v.clamp(
                    RECORDING_MIN_SECONDS_MIN as i64,
                    RECORDING_MIN_SECONDS_MAX as i64,
                ) as u32;
                // Keep max strictly above min.
                if d.recording_max_seconds <= d.recording_min_seconds {
                    d.recording_max_seconds =
                        (d.recording_min_seconds + 60).min(RECORDING_MAX_SECONDS_MAX);
                }
            }
            2 => {
                let step: i64 = 60; // one minute
                let v = d.recording_max_seconds as i64 + if up { step } else { -step };
                let floor = (d.recording_min_seconds + 1) as i64;
                d.recording_max_seconds = v
                    .clamp(
                        RECORDING_MAX_SECONDS_MIN as i64,
                        RECORDING_MAX_SECONDS_MAX as i64,
                    )
                    .max(floor) as u32;
            }
            4 => {
                let v = d.recording_past_tracks as i64 + if up { 1 } else { -1 };
                d.recording_past_tracks = v.clamp(
                    RECORDING_PAST_TRACKS_MIN as i64,
                    RECORDING_PAST_TRACKS_MAX as i64,
                ) as usize;
            }
            5 => d.recording_notify = !d.recording_notify,
            _ => {}
        }
        self.dirty = true;
        Vec::new()
    }

    /// Map a pointer column over a numeric row's bar track to a value and apply it, snapped to
    /// the row's step and clamped to its bounds. Reuses the seekbar's fraction math but over
    /// `width - 1` divisions so both track ends are reachable (the rendered `bar` spreads its
    /// thumb across `WIDTH - 1` the same way). No-ops when the value is unchanged so a drag that
    /// stays in one cell's value doesn't spam redraws. `rect` is the track rect captured at
    /// press, so mapping keeps working after the pointer leaves the track.
    pub(in crate::app) fn recording_slider_set(
        &mut self,
        row: usize,
        col: u16,
        rect: Rect,
    ) -> Vec<Cmd> {
        use crate::config::{
            RECORDING_MAX_SECONDS_MAX, RECORDING_MAX_SECONDS_MIN, RECORDING_MIN_SECONDS_MAX,
            RECORDING_MIN_SECONDS_MIN, RECORDING_PAST_TRACKS_MAX, RECORDING_PAST_TRACKS_MIN,
        };
        if rect.width == 0 {
            return Vec::new();
        }
        let clamped = col.clamp(rect.x, rect.right().saturating_sub(1));
        let denom = f64::from(rect.width.saturating_sub(1).max(1));
        let frac = f64::from(clamped - rect.x) / denom;
        // Fraction → value in `[min, max]`, snapped to `step`, then clamped.
        let snap = |min: i64, max: i64, step: i64| -> i64 {
            let raw = min as f64 + frac * (max - min) as f64;
            (((raw / step as f64).round() as i64) * step).clamp(min, max)
        };
        let mut changed = false;
        if let Some(st) = self.settings.as_mut() {
            let d = &mut st.draft;
            match row {
                1 => {
                    let v = snap(
                        RECORDING_MIN_SECONDS_MIN as i64,
                        RECORDING_MIN_SECONDS_MAX as i64,
                        5,
                    ) as u32;
                    if d.recording_min_seconds != v {
                        d.recording_min_seconds = v;
                        // Keep max strictly above min (same invariant as the ±step path).
                        if d.recording_max_seconds <= d.recording_min_seconds {
                            d.recording_max_seconds =
                                (d.recording_min_seconds + 60).min(RECORDING_MAX_SECONDS_MAX);
                        }
                        changed = true;
                    }
                }
                2 => {
                    let floor = i64::from(d.recording_min_seconds + 1);
                    let v = snap(
                        RECORDING_MAX_SECONDS_MIN as i64,
                        RECORDING_MAX_SECONDS_MAX as i64,
                        60,
                    )
                    .max(floor) as u32;
                    if d.recording_max_seconds != v {
                        d.recording_max_seconds = v;
                        changed = true;
                    }
                }
                4 => {
                    let v = snap(
                        RECORDING_PAST_TRACKS_MIN as i64,
                        RECORDING_PAST_TRACKS_MAX as i64,
                        1,
                    ) as usize;
                    if d.recording_past_tracks != v {
                        d.recording_past_tracks = v;
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        if changed {
            self.dirty = true;
        }
        Vec::new()
    }

    /// Enter/Confirm on a popup row. `pub(in crate::app)` so the mouse handler can reuse it
    /// (a row click = focus that row, then Confirm).
    pub(in crate::app) fn recording_settings_confirm(&mut self) -> Vec<Cmd> {
        let row = self
            .overlays
            .recording_settings
            .as_ref()
            .map(|p| p.row)
            .unwrap_or(0);
        match row {
            3 => {
                if let Some(p) = self.overlays.recording_settings.as_mut() {
                    p.editing_dir = true;
                }
                self.dirty = true;
                Vec::new()
            }
            6 => {
                // Open the recordings browser (the popup stays behind it).
                self.overlays.recordings_browser = Some(RecordingsBrowser::default());
                self.dirty = true;
                Vec::new()
            }
            // Mode / sliders / toggle: Enter nudges like ChangeIncrease.
            _ => self.recording_settings_adjust(1),
        }
    }

    /// Feed one key into the output-folder buffer while `editing_dir` is set.
    fn recording_dir_edit(&mut self, k: KeyEvent) -> Vec<Cmd> {
        self.dirty = true;
        match k.code {
            KeyCode::Enter | KeyCode::Esc => {
                if let Some(p) = self.overlays.recording_settings.as_mut() {
                    p.editing_dir = false;
                }
            }
            KeyCode::Backspace => {
                if let Some(st) = self.settings.as_mut() {
                    st.draft.recording_dir.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(st) = self.settings.as_mut() {
                    st.draft.recording_dir.push(c);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Keys while the recordings browser is open: ↑/↓ move, `s` save, `d` discard/cancel,
    /// Enter play/reveal, its toggle / Esc / Back close it.
    pub(in crate::app) fn recordings_browser_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let chord = k.into();
        let close = k.code == KeyCode::Esc
            || matches!(
                self.keymap.action(KeyContext::Common, chord),
                Some(Action::Back)
            )
            || matches!(
                self.keymap.action(KeyContext::Player, chord),
                Some(Action::ToggleRecordings)
            );
        if close {
            self.overlays.recordings_browser = None;
            self.dirty = true;
            return Vec::new();
        }
        let action = self
            .keymap
            .action(KeyContext::Settings, chord)
            .or_else(|| Self::settings_safety_action(k));
        let ids = self.recordings_browser_ids();
        let selected = self
            .overlays
            .recordings_browser
            .as_ref()
            .map(|b| b.selected.min(ids.len().saturating_sub(1)))
            .unwrap_or(0);
        self.dirty = true;
        match action {
            Some(Action::MoveUp) => {
                if let Some(b) = self.overlays.recordings_browser.as_mut() {
                    b.selected = selected.saturating_sub(1);
                }
                Vec::new()
            }
            Some(Action::MoveDown) => {
                if let Some(b) = self.overlays.recordings_browser.as_mut() {
                    b.selected = (selected + 1).min(ids.len().saturating_sub(1));
                }
                Vec::new()
            }
            _ => {
                let id = ids.get(selected).copied();
                match k.code {
                    KeyCode::Char('s') => id.map(|id| self.recorder_save(id)).unwrap_or_default(),
                    KeyCode::Char('d') => {
                        id.map(|id| self.recorder_discard(id)).unwrap_or_default()
                    }
                    KeyCode::Enter => {
                        if let Some(id) = id {
                            self.recorder_reveal(id);
                        }
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
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
                let saved_url_path = crate::spotify::auth::save_pending_auth_url(&url)
                    .ok()
                    .flatten();
                let opened = crate::util::browser::open_in_browser_checked(&url);
                // Also copy the URL: xdg-open can fail silently (e.g. a Flatpak
                // browser the cleared env can't resolve), and this is the only
                // path that would otherwise leave the user no way to reach the
                // approval page.
                let copied = copy_to_clipboard(&url);
                self.status.text =
                    spotify_auth_url_status(opened.launched(), copied, saved_url_path.as_deref());
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::AuthDone { display_name } => {
                let _ = crate::spotify::auth::clear_pending_auth_url();
                let mut used_client_id = None;
                if let Some(st) = self.settings.as_mut() {
                    st.draft.spotify_connected = true;
                    st.draft.spotify_stale = false;
                    st.draft.spotify_username = display_name.clone();
                    let cid = st.draft.spotify_client_id.trim().to_owned();
                    if !cid.is_empty() {
                        used_client_id = Some(cid);
                    }
                }
                self.status.text = if crate::i18n::is_korean() {
                    format!("Spotify 연결됨: {display_name}")
                } else {
                    format!("Spotify connected as {display_name}")
                };
                self.status.kind = StatusKind::Info;
                // Repair config if it had lost or mismatched the Client ID (recovered
                // from the token for this reconnect), so the orphaned "needs reconnect"
                // state doesn't come back on the next launch.
                if let Some(cid) = used_client_id
                    && self.config.spotify.client_id.as_deref() != Some(cid.as_str())
                {
                    self.config.spotify.client_id = Some(cid);
                    return vec![Cmd::Persist(PersistCmd::Config(Box::new(
                        self.config.clone(),
                    )))];
                }
            }
            TransferEvent::AuthError(error) => {
                let _ = crate::spotify::auth::clear_pending_auth_url();
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
                    st.draft.spotify_stale = false;
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
                    self.overlays.spotify_picker =
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
                        "Spotify 가져오기: {} {}/{} · 맞춤 {} · 자동 {} · 검토 {} · 누락 {} · 작성 {} · {}",
                        p.stage.label(),
                        p.done,
                        p.total,
                        p.matched,
                        p.auto_accepted,
                        p.ambiguous,
                        p.not_found,
                        p.written,
                        p.current
                    )
                } else {
                    format!(
                        "Spotify import: {} {}/{} · matched {} · auto {} · review {} · missing {} · written {} · {}",
                        p.stage.label(),
                        p.done,
                        p.total,
                        p.matched,
                        p.auto_accepted,
                        p.ambiguous,
                        p.not_found,
                        p.written,
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
                if let Some(accepted_count) = self
                    .local_mode
                    .pending_accept_write_summaries
                    .remove(&report.job_id)
                {
                    self.status.text = local_accept_write_done_status(&report, accepted_count);
                } else {
                    self.status.text = transfer_done_status(&report);
                }
                self.status.kind = StatusKind::Info;
            }
            TransferEvent::JobRejected { job_id, error } => {
                // The actor still owns a different active job. Clear only bookkeeping for the
                // rejected attempt; its terminal will never arrive, while the active job's guard
                // must remain set until that job emits JobDone/JobFailed.
                self.local_mode
                    .pending_accept_write_summaries
                    .remove(&job_id);
                let error = crate::util::sanitize::sanitize_error_text(error);
                self.status.text = format!(
                    "{}: {error}",
                    t!("Import request rejected", "가져오기 요청 거부")
                );
                self.status.kind = StatusKind::Error;
            }
            TransferEvent::JobFailed {
                job_id,
                error,
                resumable,
            } => {
                self.transfer_running = false;
                self.local_mode
                    .pending_accept_write_summaries
                    .remove(&job_id);
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

    pub(in crate::app) fn settings_request_confirm(&mut self, confirm: SettingsConfirm) {
        self.overlays.pending_settings_confirm = Some(confirm);
        self.status.text.clear();
        self.dirty = true;
    }

    pub(in crate::app) fn settings_apply_confirm(&mut self, confirm: SettingsConfirm) -> Vec<Cmd> {
        self.overlays.pending_settings_confirm = None;
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
                    Cmd::Persist(PersistCmd::Config(Box::new(self.config.clone()))),
                    Cmd::Scrobble(ScrobbleCmd::Reconfigure(Box::new(
                        self.config.scrobble_settings(),
                    ))),
                ]
            }
            SettingsConfirm::SpotifyDisconnect => {
                // The actor deletes the token file and answers with `Disconnected`,
                // which flips the draft's display state.
                vec![Cmd::Transfer(
                    crate::transfer::actor::TransferCmd::Disconnect,
                )]
            }
            SettingsConfirm::EditApiKey => {
                // Confirmed: enter edit mode on the API-key row exactly as the (non-secret) text
                // path would, including the secret-clear. The field cursor is still on `ApiKey`
                // (the confirm is modal), so `settings_text_buf` targets `draft.gemini_api_key`.
                // Clearing the masked buffer avoids blind append-corruption; the prior value is
                // remembered in `secret_restore` so committing empty restores it (no accidental wipe).
                let st = self.settings_mut();
                st.secret_restore = Self::settings_text_buf(st).map(|buf| {
                    let prev = buf.clone();
                    buf.clear();
                    prev
                });
                st.editing_text = true;
                self.dirty = true;
                Vec::new()
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
        st.mousemap.reset_all();
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
        vec![Cmd::Persist(PersistCmd::ClearRomanizedTitles)]
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
                let old_roots = self.local_scan_roots();
                self.config.download_dir =
                    settings::blank_to_none(&value).map(std::path::PathBuf::from);
                let new_dir = self.config.effective_download_dir();
                if new_dir != old_dir {
                    cmds.push(Cmd::Download(DownloadCmd::SetDir(new_dir.clone())));
                    cmds.push(Cmd::Download(DownloadCmd::Scan(new_dir)));
                }
                if self.local_dedicated_mode && self.local_scan_roots() != old_roots {
                    cmds.extend(self.request_local_scan(false));
                }
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::LocalMusicRoot => {
                let recursive = self
                    .settings
                    .as_ref()
                    .map(|s| s.draft.local_music_root_recursive)
                    .unwrap_or(true);
                let old_roots = self.local_scan_roots();
                settings::set_first_local_root(&mut self.config, &value, recursive);
                if self.local_dedicated_mode && self.local_scan_roots() != old_roots {
                    cmds.extend(self.request_local_scan(false));
                }
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::AudioMpvOutput => {
                self.config.audio.mpv.output = settings::blank_to_none(&value);
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::AudioMpvDevice => {
                self.config.audio.mpv.device = settings::blank_to_none(&value);
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::AudioMpvCacheForward => {
                self.config.audio.mpv.cache_forward = settings::blank_to_none(&value)
                    .unwrap_or_else(|| crate::config::MPV_CACHE_FORWARD_DEFAULT.to_owned());
                self.status.text = t!("Settings saved", "설정을 저장했어요").to_owned();
            }
            Field::AudioMpvCacheBack => {
                self.config.audio.mpv.cache_back = settings::blank_to_none(&value)
                    .unwrap_or_else(|| crate::config::MPV_CACHE_BACK_DEFAULT.to_owned());
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
                cmds.push(Cmd::Scrobble(ScrobbleCmd::Reconfigure(Box::new(
                    self.config.scrobble_settings(),
                ))));
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
        cmds.push(Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        ))));
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
            Field::LocalMusicRoot => Some(&mut st.draft.local_music_root),
            Field::AudioMpvOutput => Some(&mut st.draft.audio_mpv_output),
            Field::AudioMpvDevice => Some(&mut st.draft.audio_mpv_device),
            Field::AudioMpvCacheForward => Some(&mut st.draft.audio_mpv_cache_forward),
            Field::AudioMpvCacheBack => Some(&mut st.draft.audio_mpv_cache_back),
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

    /// Apply a Settings snapshot whose complete player-audio batch was already admitted.
    pub(in crate::app) fn apply_settings_save(&mut self, st: SettingsState) -> Vec<Cmd> {
        self.overlays.pending_settings_confirm = None;
        // Drop the top-level overlays that live over the Settings screen so leaving it (Esc/q,
        // or any early-return path below) can't strand them painting on top of the Player.
        self.overlays.recording_settings = None;
        self.overlays.recordings_browser = None;
        self.overlays.spotify_picker = None;
        self.settings = None;
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
        let old_local_roots = self.local_scan_roots();
        let normal_theme = if self.radio_dedicated_mode {
            Some(
                self.radio_mode
                    .normal_mode_theme
                    .clone()
                    .unwrap_or_else(|| self.config.effective_theme()),
            )
        } else {
            None
        };
        let old_zoom = self.zoom.percent();
        d.apply_to(&mut self.config);
        // Push the resolved DJ Gem reply language to the AI actor. The UI language was set live
        // as the user cycled it; this global isn't, so it's resolved and applied here on save
        // (retro → English, `Auto` → the UI language, else the concrete pick).
        crate::i18n::set_dj_gem_language(self.config.effective_dj_gem_language());
        // Push the (possibly toggled) large-text level to the renderer. The handle snaps
        // to what this terminal's zoom mode can draw; a change forces the full-clear
        // redraw path so nothing from the old grid survives.
        if self.zoom.supported() && self.zoom.set(self.config.effective_text_zoom()) != old_zoom {
            self.request_native_image_clear();
        }
        self.search.source = self.config.effective_search().source;
        // Commit the edited keybindings (live + persisted as compact overrides).
        self.keymap = st.keymap.clone();
        self.config.keybindings = self.keymap.to_overrides();
        self.mousemap = st.mousemap.clone();
        self.config.mouse_bindings = self.mousemap.to_overrides();
        self.theme = st.draft.theme.normalized();
        if self.radio_dedicated_mode {
            self.radio_mode.radio_mode_theme = Some(self.theme.clone());
            // Persist the radio theme in its own slot; `config.theme` stays the normal
            // theme (below) so a radio-mode save can't clobber it.
            self.config.radio_theme = Some(self.theme.clone());
            if let Some(normal_theme) = normal_theme {
                self.radio_mode.normal_mode_theme = Some(normal_theme.clone());
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
        // Turning "large text" on in a terminal that can't render it deserves the why,
        // not a silent no-op — override the generic saved-toast with the explanation.
        if !self.zoom.supported() && st.draft.big_text && old_zoom <= 100 {
            self.status.text = t!(
                "Large text saved, but this terminal can't scale text (kitty 0.40+, Windows Terminal, …)",
                "큰 글자 설정은 저장됐지만 이 터미널은 글자 확대를 지원하지 않아요 (kitty 0.40+, Windows Terminal 등 가능)"
            )
            .to_owned();
        }
        let mut cmds = vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))];
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
            cmds.push(Cmd::Download(DownloadCmd::SetDir(new_download_dir.clone())));
            cmds.push(Cmd::Download(DownloadCmd::Scan(new_download_dir)));
        }
        if self.local_dedicated_mode && self.local_scan_roots() != old_local_roots {
            cmds.extend(self.request_local_scan(false));
        }
        cmds.extend(self.reconcile_recorder());
        // Hand the scrobble actor the committed account settings. Unconditional: the
        // snapshot is a few strings and the actor's swap is trivial, so this is cheaper
        // than tracking which of the scrobble fields changed.
        cmds.push(Cmd::Scrobble(ScrobbleCmd::Reconfigure(Box::new(
            self.config.scrobble_settings(),
        ))));
        // React to an album-art toggle. Turning it off drops the held image (frees RAM).
        // Turning it on fetches the current track's art live: the graphics protocol is probed
        // unconditionally at startup, so the picker is always present and `artwork_source`
        // resolves as soon as the flag flips — no relaunch needed.
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
