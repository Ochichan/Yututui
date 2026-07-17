//! Immediate persistence for free-text settings fields, split out of the parent reducer
//! to keep its size budget for the field dispatch tables.

use super::super::*;

impl App {
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
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
            }
            Field::AudiusAppName => {
                self.config.search.audius_app_name = settings::blank_to_none(&value);
                if let Some(st) = self.settings.as_mut() {
                    st.draft.search.audius_app_name = self.config.search.audius_app_name.clone();
                    st.draft.search = st.draft.search.clone().normalized();
                }
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
            }
            Field::JamendoClientId => {
                self.config.search.jamendo_client_id = settings::blank_to_none(&value);
                if let Some(st) = self.settings.as_mut() {
                    st.draft.search.jamendo_client_id =
                        self.config.search.jamendo_client_id.clone();
                    st.draft.search = st.draft.search.clone().normalized();
                }
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
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
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
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
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
            }
            Field::AudioMpvOutput => {
                self.config.audio.mpv.output = settings::blank_to_none(&value);
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
            }
            Field::AudioMpvDevice => {
                self.config
                    .audio
                    .mpv
                    .set_manual_device(settings::blank_to_none(&value));
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
            }
            Field::AudioMpvCacheForward => {
                let mpv = &mut self.config.audio.mpv;
                mpv.set_cache_forward(settings::blank_to_none(&value));
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
            }
            Field::AudioMpvCacheBack => {
                let mpv = &mut self.config.audio.mpv;
                mpv.set_cache_back(settings::blank_to_none(&value));
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
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
                self.status.text = t!(
                    "API key saved",
                    "API 키를 저장했어요",
                    "APIキーを保存しました"
                )
                .to_owned();
            }
            Field::ListenBrainzToken => {
                self.config.scrobble.listenbrainz.token = settings::blank_to_none(&value);
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
                cmds.push(Cmd::Scrobble(ScrobbleCmd::Reconfigure(Box::new(
                    self.config.scrobble_settings(),
                ))));
            }
            Field::SpotifyClientId => {
                self.config.spotify.client_id = settings::blank_to_none(&value);
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
            }
            Field::SpotifyRedirectPort => {
                self.config.spotify.redirect_port = value.trim().parse::<u16>().ok();
                self.status.text =
                    t!("Settings saved", "설정을 저장했어요", "設定を保存しました").to_owned();
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
}
