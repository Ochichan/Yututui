//! Settings confirmations, text editing, resets, and committed-save reduction.

use super::super::*;

impl App {
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
                st.text_cursor = TextCursor::default();
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
        if let Some(action) = self.keymap.text_edit_action(k.into()) {
            if let Some(st) = self.settings.as_mut() {
                let mut cursor = st.text_cursor;
                if let Some(buf) = Self::settings_text_buf(st) {
                    let _ = apply_text_edit_action(action, &mut cursor, buf);
                }
                st.text_cursor = cursor;
            }
            return Vec::new();
        }
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
                if let Some(st) = self.settings.as_mut() {
                    let mut cursor = st.text_cursor;
                    if let Some(buf) = Self::settings_text_buf(st) {
                        cursor.insert_char(buf, c);
                    }
                    st.text_cursor = cursor;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
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
        self.overlays.audio_output_picker = None;
        self.settings = None;
        self.mode = Mode::Player;
        self.dirty = true;
        let d = &st.draft;
        self.playback.speed = d.speed;
        self.audio.seek_seconds = d.seek_seconds;
        self.audio.bands = d.eq_bands;
        self.audio.preset = d.eq_preset;
        self.audio.normalize = d.normalize;
        // This is the post-admission commit boundary only: do not reset the streaming failure
        // counter or start a refill from here.
        self.autoplay_streaming = d.autoplay_streaming;
        let model_changed = self.ai.model != d.gemini_model;
        self.ai.model = d.gemini_model;
        let old_key = self.config.gemini_api_key.clone();
        let old_beginner_mode = self.config.beginner_mode;
        let old_ai_enabled = self.config.effective_ai_enabled();
        let old_romanized_titles = self.config.effective_romanized_titles();
        let old_album_art_quality = self.config.album_art_quality;
        let old_download_dir = self.config.effective_download_dir();
        let old_local_roots = self.local_scan_roots();
        let normal_theme = if self.radio_dedicated_mode {
            Some(
                self.radio_mode
                    .normal_mode_theme
                    .clone()
                    .unwrap_or_else(|| self.config.effective_theme()),
            )
        } else if self.local_dedicated_mode {
            Some(
                self.local_mode
                    .normal_mode_theme
                    .clone()
                    .unwrap_or_else(|| self.config.effective_theme()),
            )
        } else {
            None
        };
        let old_zoom = self.zoom.percent();
        d.apply_to(&mut self.config);
        let album_art_quality_changed = self.config.album_art_quality != old_album_art_quality;
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
        } else if self.local_dedicated_mode {
            self.local_mode.local_mode_theme = Some(self.theme.clone());
            // Persist the Local theme in its own slot. `SettingsDraft::apply_to` writes the
            // visible draft into `config.theme`, so restore the stashed normal slot afterward.
            self.config.local_theme = Some(self.theme.clone());
            if let Some(normal_theme) = normal_theme {
                self.local_mode.normal_mode_theme = Some(normal_theme.clone());
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
        // The transition owns tutorial completion/restart and its user-facing toast. Keep it
        // after the generic save message but before cloning the one persisted Config snapshot.
        self.apply_beginner_mode_settings_transition(
            old_beginner_mode,
            d.restart_beginner_tutorial,
        );
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
        // React to an album-art toggle or quality change. Turning it off drops the held image
        // (frees RAM). Turning it on or selecting a different quality fetches the current track's
        // art live: the graphics protocol is probed unconditionally at startup, so the picker is
        // always present and `artwork_source` resolves without a relaunch.
        if !self.config.effective_album_art() {
            self.clear_artwork();
        } else if let Some(song) = self.queue.current().cloned() {
            // Local embedded covers keep the legacy 768px cap, so a remote-quality change must
            // not re-read and decode the same local file.
            if album_art_quality_changed && song.local_path.is_none() {
                self.clear_artwork();
            }
            if self.art.video_id.as_deref() != Some(song.video_id.as_str())
                && let Some(source) = self.artwork_source(&song)
            {
                self.art.loading = true;
                cmds.push(Cmd::FetchArtwork {
                    video_id: song.video_id.clone(),
                    source,
                });
            }
        }
        cmds
    }
}
