use super::*;

impl SettingsDraft {
    /// Render the current value of `field` for display.
    pub fn value_display(&self, field: Field) -> String {
        match field {
            Field::BeginnerMode
            | Field::Language
            | Field::CookiesFile
            | Field::DownloadDir
            | Field::LocalIncludeDownloadDir
            | Field::LocalMusicRoot
            | Field::LocalMusicRootRecursive
            | Field::Mouse
            | Field::AlbumArt
            | Field::PlayerBarPosition
            | Field::AutoplayOnStart
            | Field::EnqueueNext
            | Field::UpdateCheck
            | Field::ExportPersonalData
            | Field::ResetKeybindings
            | Field::ResetAll
            | Field::BigText => self.value_display_general(field),
            Field::SearchSource
            | Field::StreamingSource
            | Field::SearchYoutube
            | Field::SearchSoundCloud
            | Field::SearchAudius
            | Field::AudiusAppName
            | Field::SearchJamendo
            | Field::JamendoClientId
            | Field::SearchInternetArchive
            | Field::SearchRadioBrowser => self.value_display_search(field),
            Field::Speed
            | Field::SeekInterval
            | Field::MouseWheelVolume
            | Field::Gapless
            | Field::MediaControls
            | Field::AutoContinueVideos
            | Field::VideoLayout
            | Field::AlbumArtQuality
            | Field::RadioRecording
            | Field::AudioBackend
            | Field::AudioOutput
            | Field::AudioMpvOutput
            | Field::AudioMpvDevice
            | Field::LongFormSeekOptimization
            | Field::AudioMpvCacheForward
            | Field::AudioMpvCacheBack
            | Field::EqPreset
            | Field::Band(_)
            | Field::Normalize => self.value_display_playback(field),
            Field::AiEnabled
            | Field::GeminiModel
            | Field::ApiKey
            | Field::DjGemLanguage
            | Field::RomanizedTitles
            | Field::ClearRomanizedTitleCache
            | Field::AutoplayStreaming
            | Field::CuratingMode
            | Field::StreamingMode => self.value_display_ai(field),
            Field::RetroMode
            | Field::ThemePreset
            | Field::BackgroundNone
            | Field::ThemeColor(_)
            | Field::AnimMaster
            | Field::AnimFps
            | Field::AnimPauseUnfocused
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
            | Field::AnimPlasma => self.value_display_graphics(field),
            Field::LastfmEnabled
            | Field::LastfmConnect
            | Field::LastfmLoveSync
            | Field::ListenBrainzEnabled
            | Field::ListenBrainzToken
            | Field::ScrobbleLocalFiles
            | Field::SpotifyClientId
            | Field::SpotifyRedirectPort
            | Field::SpotifyConnect
            | Field::SpotifyImportMode
            | Field::SpotifyImport => self.value_display_accounts(field),
        }
    }

    fn value_display_general(&self, field: Field) -> String {
        match field {
            Field::BeginnerMode => toggle_str(self.beginner_mode),
            // Each language names itself, so this value is the same regardless of the active
            // UI language (English / 한국어 / 日本語).
            Field::Language => self.language.native_name().to_owned(),
            Field::CookiesFile => {
                if self.cookies_file.is_empty() {
                    default_cookies_file()
                        .map(|p| {
                            format!(
                                "({}: {})",
                                t!("default", "기본값", "デフォルト"),
                                display_path(&p)
                            )
                        })
                        .unwrap_or_else(|| t!("(none)", "(없음)", "(なし)").to_owned())
                } else {
                    self.cookies_file.clone()
                }
            }
            Field::DownloadDir => {
                if self.download_dir.is_empty() {
                    format!(
                        "({}: {})",
                        t!("default", "기본값", "デフォルト"),
                        display_path(&default_download_dir())
                    )
                } else {
                    self.download_dir.clone()
                }
            }
            Field::LocalIncludeDownloadDir => toggle_str(self.local_include_download_dir),
            Field::LocalMusicRoot => {
                if self.local_music_root.is_empty() {
                    t!("(none)", "(없음)", "(なし)").to_owned()
                } else {
                    self.local_music_root.clone()
                }
            }
            Field::LocalMusicRootRecursive => toggle_str(self.local_music_root_recursive),
            Field::Mouse => toggle_str(self.mouse),
            Field::AlbumArt => toggle_str(self.album_art),
            Field::PlayerBarPosition => self.player_bar_position.label().to_owned(),
            Field::AutoplayOnStart => toggle_str(self.autoplay_on_start),
            Field::EnqueueNext => toggle_str(self.enqueue_next),
            Field::UpdateCheck => toggle_str(self.update_check_enabled),
            Field::BigText => toggle_str(self.big_text),
            // Buttons, not values: these rows show how to trigger them.
            Field::ExportPersonalData => PersonalDataExportStatus::Idle.value_display(),
            Field::ResetKeybindings | Field::ResetAll => {
                t!("↵ press Enter", "↵ Enter로 실행", "↵ Enterで実行").to_owned()
            }
            _ => unreachable!("non-General field delegated to General display"),
        }
    }

    fn value_display_search(&self, field: Field) -> String {
        match field {
            Field::SearchSource => self.search.source.label().to_owned(),
            Field::StreamingSource => self
                .search
                .normalized_streaming_source(self.search.streaming_source)
                .label()
                .to_owned(),
            Field::SearchYoutube => toggle_str(self.search.youtube),
            Field::SearchSoundCloud => toggle_str(self.search.soundcloud),
            Field::SearchAudius => toggle_str(self.search.audius),
            Field::AudiusAppName => self.search.audius_app_name.clone().unwrap_or_else(|| {
                t!(
                    "(default: yututui)",
                    "(기본값: yututui)",
                    "(デフォルト: yututui)"
                )
                .to_owned()
            }),
            Field::SearchJamendo => toggle_str(self.search.jamendo),
            Field::JamendoClientId => self
                .search
                .jamendo_client_id
                .clone()
                .unwrap_or_else(|| t!("(none)", "(없음)", "(なし)").to_owned()),
            Field::SearchInternetArchive => toggle_str(self.search.internet_archive),
            Field::SearchRadioBrowser => toggle_str(self.search.radio_browser),
            _ => unreachable!("non-Search field delegated to Search display"),
        }
    }

    fn value_display_playback(&self, field: Field) -> String {
        match field {
            Field::RadioRecording => self.recording_mode.label(),
            Field::Speed => format!("{:.1}x", self.speed),
            Field::SeekInterval => format!("{:.0}s", self.seek_seconds),
            Field::MouseWheelVolume => toggle_str(self.mouse_wheel_volume),
            Field::Gapless => toggle_str(self.gapless),
            Field::MediaControls => toggle_str(self.media_controls),
            Field::AutoContinueVideos => toggle_str(self.auto_continue_videos),
            Field::VideoLayout => self.video_layout.label().to_owned(),
            Field::AlbumArtQuality => self.album_art_quality.label().to_owned(),
            Field::AudioBackend => self.audio_backend.id().to_owned(),
            Field::AudioOutput => audio_optional_display(&self.audio_mpv_device),
            Field::AudioMpvOutput => audio_optional_display(&self.audio_mpv_output),
            Field::AudioMpvDevice => audio_optional_display(&self.audio_mpv_device),
            Field::LongFormSeekOptimization => match self.long_form_seek_optimization {
                LongFormSeekOptimization::Auto => {
                    t!("Auto (experimental)", "자동 (실험적)", "自動 (実験的)").to_owned()
                }
                LongFormSeekOptimization::Off => t!("Off", "끔", "オフ").to_owned(),
                LongFormSeekOptimization::On => t!("On", "켬", "オン").to_owned(),
            },
            Field::AudioMpvCacheForward => {
                cache_display(&self.audio_mpv_cache_forward, MPV_CACHE_FORWARD_DEFAULT)
            }
            Field::AudioMpvCacheBack => {
                cache_display(&self.audio_mpv_cache_back, MPV_CACHE_BACK_DEFAULT)
            }
            Field::EqPreset => self.eq_preset.label().to_owned(),
            Field::Band(i) => format!("{:+.0} dB", self.eq_bands[i]),
            Field::Normalize => toggle_str(self.normalize),
            _ => unreachable!("non-Playback field delegated to Playback display"),
        }
    }

    fn value_display_ai(&self, field: Field) -> String {
        match field {
            Field::AutoplayStreaming => toggle_str(self.autoplay_streaming),
            Field::CuratingMode => self.curating_mode.label().to_owned(),
            Field::StreamingMode => self.streaming_mode.label().to_owned(),
            Field::AiEnabled => toggle_str(self.ai_enabled),
            Field::GeminiModel => self.gemini_model.label().to_owned(),
            // Retro mode forces English replies, so the row says so plainly instead of showing
            // the (preserved-underneath) picked value that retro would ignore.
            Field::DjGemLanguage => {
                if self.retro_mode {
                    t!(
                        "English (Retro mode)",
                        "영어 (레트로 모드)",
                        "英語 (レトロモード)"
                    )
                    .to_owned()
                } else {
                    self.dj_gem_language.picker_label().to_owned()
                }
            }
            Field::RomanizedTitles => toggle_str(self.romanized_titles),
            // Never echo the key. Editing shows a masked buffer (handled in the view); this
            // is the at-rest summary.
            Field::ApiKey => {
                if self.gemini_api_key.trim().is_empty() {
                    t!("(none)", "(없음)", "(なし)").to_owned()
                } else {
                    t!("***configured***", "***저장됨***", "***設定済み***").to_owned()
                }
            }
            Field::ClearRomanizedTitleCache => {
                t!("↵ press Enter", "↵ Enter로 실행", "↵ Enterで実行").to_owned()
            }
            _ => unreachable!("non-AI field delegated to AI display"),
        }
    }

    fn value_display_graphics(&self, field: Field) -> String {
        match field {
            Field::RetroMode => toggle_str(self.retro_mode),
            Field::ThemePreset => self.theme.preset_enum().label().to_owned(),
            Field::BackgroundNone => {
                toggle_str(self.theme.is_role_transparent(ThemeRole::Background))
            }
            Field::ThemeColor(role) => self.theme.effective_hex(role),
            // The lone numeric animation field: shown as "<n> fps" (clamped to the valid range).
            Field::AnimFps => format!("{} fps", self.animations.effective_fps()),
            // Behaviour knob, rendered as a checkbox (handled explicitly, not via `anim_flag`).
            Field::AnimPauseUnfocused => toggle_str(self.animations.pause_unfocused),
            // All 41 animation toggles render as a checkbox; one mapping (`anim_flag`) reads
            // the live value out of the draft's `animations`, so display never drifts from
            // the toggle/persist paths. (`field` is the value being matched here.)
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
                let mut a = self.animations;
                toggle_str(field.anim_flag(&mut a).map(|b| *b).unwrap_or(false))
            }
            _ => unreachable!("non-Graphics field delegated to Graphics display"),
        }
    }

    fn value_display_accounts(&self, field: Field) -> String {
        match field {
            Field::LastfmEnabled => toggle_str(self.lastfm_enabled),
            // The Connect button doubles as the connection status line.
            Field::LastfmConnect => {
                if self.lastfm_session_key.trim().is_empty() {
                    t!(
                        "↵ connect in browser",
                        "↵ Enter로 브라우저 연결",
                        "↵ ブラウザで連携"
                    )
                    .to_owned()
                } else if self.lastfm_username.trim().is_empty() {
                    t!(
                        "connected · ↵ disconnect",
                        "연결됨 · ↵ 연결 해제",
                        "連携済み · ↵ 連携解除"
                    )
                    .to_owned()
                } else {
                    match crate::i18n::current() {
                        Language::Korean => {
                            format!("{} 연결됨 · ↵ 연결 해제", self.lastfm_username)
                        }
                        Language::Japanese => {
                            format!("{} として連携済み · ↵ 連携解除", self.lastfm_username)
                        }
                        _ => format!("connected as {} · ↵ disconnect", self.lastfm_username),
                    }
                }
            }
            Field::LastfmLoveSync => toggle_str(self.lastfm_love_sync),
            Field::ListenBrainzEnabled => toggle_str(self.listenbrainz_enabled),
            Field::ListenBrainzToken => {
                if self.listenbrainz_token.trim().is_empty() {
                    t!("(none)", "(없음)", "(なし)").to_owned()
                } else {
                    t!("***configured***", "***저장됨***", "***設定済み***").to_owned()
                }
            }
            Field::ScrobbleLocalFiles => toggle_str(self.scrobble_local_files),
            Field::SpotifyClientId => {
                if self.spotify_client_id.trim().is_empty() {
                    t!(
                        "(none — create an app at developer.spotify.com)",
                        "(없음 — developer.spotify.com에서 앱 생성)",
                        "(なし — developer.spotify.comでアプリ作成)"
                    )
                    .to_owned()
                } else {
                    self.spotify_client_id.clone()
                }
            }
            Field::SpotifyRedirectPort => {
                if self.spotify_redirect_port.trim().is_empty() {
                    format!(
                        "({}: {})",
                        t!("default", "기본값", "デフォルト"),
                        crate::config::SPOTIFY_REDIRECT_PORT_DEFAULT
                    )
                } else {
                    self.spotify_redirect_port.clone()
                }
            }
            Field::SpotifyConnect => {
                if !self.spotify_connected {
                    t!(
                        "↵ connect in browser",
                        "↵ Enter로 브라우저 연결",
                        "↵ ブラウザで連携"
                    )
                    .to_owned()
                } else if self.spotify_stale {
                    t!(
                        "needs reconnect · ↵ reconnect in browser",
                        "재연결 필요 · ↵ Enter로 브라우저 재연결",
                        "再連携が必要 · ↵ ブラウザで再連携"
                    )
                    .to_owned()
                } else if self.spotify_username.trim().is_empty() {
                    t!(
                        "connected · ↵ disconnect",
                        "연결됨 · ↵ 연결 해제",
                        "連携済み · ↵ 連携解除"
                    )
                    .to_owned()
                } else {
                    match crate::i18n::current() {
                        Language::Korean => {
                            format!("{} 연결됨 · ↵ 연결 해제", self.spotify_username)
                        }
                        Language::Japanese => {
                            format!("{} として連携済み · ↵ 連携解除", self.spotify_username)
                        }
                        _ => format!("connected as {} · ↵ disconnect", self.spotify_username),
                    }
                }
            }
            Field::SpotifyImportMode => self.spotify_import_mode.label().to_owned(),
            Field::SpotifyImport => t!(
                "↵ pick a playlist (↵ again cancels a running import)",
                "↵ 플레이리스트 선택 (실행 중엔 ↵로 취소)",
                "↵ プレイリスト選択 (実行中は↵でキャンセル)"
            )
            .to_owned(),
            _ => unreachable!("non-Accounts field delegated to Accounts display"),
        }
    }
}
