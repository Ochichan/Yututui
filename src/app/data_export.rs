//! Owner-lane orchestration for the portable personal-data exporter.
//!
//! The reducer captures owned source state and marks the export busy; the runtime projects and
//! writes it on a blocking worker. This keeps draft settings current without letting projection or
//! file IO stall input/rendering, and keeps one shared busy guard for TUI and remote requests.

use super::*;
#[cfg(test)]
use crate::data_export::live::{
    NESTED_TEXT_ITEMS_LIMIT, PLAYLIST_TRACK_LIMIT, checked_playlist_track_count,
};
use crate::data_export::live::{SizeError, validate_source_clone};

#[cfg(not(test))]
fn personal_export_download_directory() -> Result<PathBuf, crate::data_export::ExportError> {
    crate::data_export::default_export_directory()
}

// Settings interaction tests must not depend on a desktop Downloads directory being configured
// on the host (headless Linux runners intentionally have none). The public resolver itself keeps
// its platform-specific coverage in `data_export` tests.
#[cfg(test)]
fn personal_export_download_directory() -> Result<PathBuf, crate::data_export::ExportError> {
    Ok(std::env::temp_dir())
}

impl App {
    pub(in crate::app) fn personal_export_status(&self) -> PersonalDataExportStatus {
        PersonalDataExportStatus::from_busy(self.personal_export.in_progress)
    }

    /// Build the config view the user can currently see, including an open Settings draft and
    /// live playback values that may not have reached the debounced config store yet.
    fn personal_export_config(&self) -> Config {
        let mut config = self.config.clone();

        if let Some(settings) = self.settings.as_ref() {
            settings.draft.apply_to(&mut config);
            config.keybindings = settings.keymap.to_overrides();
            config.mouse_bindings = settings.mousemap.to_overrides();

            // In dedicated Radio mode the visible draft edits the radio-only theme slot. Preserve
            // the normal theme exactly as close_settings does instead of exporting it as radio.
            if self.radio_dedicated_mode {
                config.radio_theme = Some(settings.draft.theme.normalized());
                config.theme = self
                    .radio_mode
                    .normal_mode_theme
                    .clone()
                    .unwrap_or_else(|| self.config.effective_theme());
            } else if self.local_dedicated_mode {
                config.local_theme = Some(settings.draft.theme.normalized());
                config.theme = self
                    .local_mode
                    .normal_mode_theme
                    .clone()
                    .unwrap_or_else(|| self.config.effective_theme());
            } else {
                config.theme = settings.draft.theme.normalized();
            }
        } else {
            // These controls are mutable during playback and persist asynchronously. Export the
            // owner state, not a possibly stale on-disk/config snapshot.
            config.speed = Some(self.playback.speed);
            config.seek_seconds = Some(self.audio.seek_seconds);
            config.eq_preset = self.audio.preset;
            config.eq_bands = if self.audio.bands == self.audio.preset.gains() {
                None
            } else {
                Some(self.audio.bands)
            };
            config.normalize = Some(self.audio.normalize);
            config.autoplay_streaming = Some(self.autoplay_streaming);
            config.keybindings = self.keymap.to_overrides();
            config.mouse_bindings = self.mousemap.to_overrides();
            if self.radio_dedicated_mode {
                config.radio_theme = Some(self.theme.clone());
            } else if self.local_dedicated_mode {
                config.local_theme = Some(self.theme.clone());
                config.theme = self
                    .local_mode
                    .normal_mode_theme
                    .clone()
                    .unwrap_or_else(|| self.config.effective_theme());
            } else {
                config.theme = self.theme.clone();
            }
        }

        config.volume = self.playback.volume;
        config.shuffle = Some(self.queue.shuffle);
        config.repeat = self.queue.repeat;
        config
    }

    fn personal_export_sources(&self, config: Config) -> PersonalDataExportSources {
        PersonalDataExportSources {
            personal_state: self.personal_state.clone(),
            config,
            library: self.library.as_ref().clone(),
            playlists: self.playlists.as_ref().clone(),
            signals: self.signals.as_ref().clone(),
            station: self.station.clone(),
        }
    }

    fn personal_export_size_preflight(&self, config: &Config) -> Result<(), SizeError> {
        validate_source_clone(
            config,
            &self.library,
            Some(&self.playlists),
            &self.signals,
            &self.station,
        )
        .map(|_| ())
    }

    fn reject_oversized_personal_export(
        &mut self,
        error: SizeError,
        reply: Option<crate::remote::RemoteReply>,
    ) -> Vec<Cmd> {
        let detail = error.detail();
        let remote_message = format!(
            "personal-data export is too large or complex to copy safely while ytt is running: {detail}. Close the running ytt instance, then run `ytt data export`, or reduce the saved metadata."
        );
        let status = match crate::i18n::current() {
            crate::i18n::Language::Korean => format!(
                "실행 중 복사하기에는 개인 데이터가 너무 크거나 복잡합니다. 실행 중인 ytt를 닫은 뒤 `ytt data export`를 실행하거나 저장된 메타데이터를 줄이세요. ({detail})"
            ),
            crate::i18n::Language::Japanese => format!(
                "実行中にコピーするには個人データが大きすぎるか複雑すぎます。実行中のyttを閉じてから `ytt data export` を実行するか、保存済みメタデータを減らしてください。({detail})"
            ),
            _ => remote_message.clone(),
        };

        if let Some(settings) = self.settings.as_mut() {
            settings.personal_data_export = PersonalDataExportStatus::Failed;
        }
        self.personal_export.in_progress = false;
        self.set_status_error(status);
        if let Some(reply) = reply {
            let _ = reply.send(crate::remote::proto::RemoteResponse::err_with_message(
                "personal_export_too_large",
                remote_message,
            ));
        }
        Vec::new()
    }

    /// Settings-button entrypoint: resolve the platform Downloads directory without a cwd
    /// fallback. Resolution failure is surfaced in place and no worker is started.
    pub(in crate::app) fn start_personal_export_to_downloads(&mut self) -> Vec<Cmd> {
        match personal_export_download_directory() {
            Ok(directory) => self.start_personal_export(directory, 2, None),
            Err(error) => {
                let error = crate::util::sanitize::sanitize_error_text(error.to_string());
                if let Some(settings) = self.settings.as_mut() {
                    settings.personal_data_export = PersonalDataExportStatus::Failed;
                }
                self.set_status_error(format!(
                    "{}: {error}",
                    t!(
                        "Could not find the Downloads folder",
                        "다운로드 폴더를 찾을 수 없음",
                        "ダウンロードフォルダーが見つかりません"
                    )
                ));
                Vec::new()
            }
        }
    }

    /// Start one export from either surface. Remote replies remain open until the runtime worker
    /// publishes the final file; duplicate remote requests receive a stable busy rejection.
    pub(in crate::app) fn start_personal_export(
        &mut self,
        directory: PathBuf,
        schema: u32,
        reply: Option<crate::remote::RemoteReply>,
    ) -> Vec<Cmd> {
        if self.personal_export.in_progress {
            if let Some(reply) = reply {
                let _ = reply.send(crate::remote::proto::RemoteResponse::err_with_message(
                    "personal_export_busy",
                    "another personal-data export is already running".to_owned(),
                ));
            }
            return Vec::new();
        }
        let config = self.personal_export_config();
        if let Err(error) = self.personal_export_size_preflight(&config) {
            return self.reject_oversized_personal_export(error, reply);
        }

        // Publish busy before capturing potentially large stores. The capture is clone-only;
        // allowlist projection and serialization happen in the blocking runtime worker.
        self.personal_export.in_progress = true;
        if let Some(settings) = self.settings.as_mut() {
            settings.personal_data_export = PersonalDataExportStatus::Exporting;
        }
        self.status.text = t!(
            "Exporting personal data…",
            "개인 데이터를 내보내는 중…",
            "個人データをエクスポート中…"
        )
        .to_owned();
        self.status.kind = StatusKind::Info;
        self.dirty = true;
        let sources = self.personal_export_sources(config);

        vec![Cmd::Data(DataCmd::PersonalDataExport(
            PersonalDataExportCmd::Export {
                directory,
                schema,
                sources: Box::new(sources),
                reply,
            },
        ))]
    }

    /// Fold the worker result back into both the compact Settings row and the detailed status
    /// line. The success notice calls out that history is private even though credentials/media
    /// were deliberately omitted.
    pub(in crate::app) fn finish_personal_export(
        &mut self,
        result: Result<PathBuf, String>,
        reply: Option<crate::remote::RemoteReply>,
    ) -> Vec<Cmd> {
        let remote_response = reply.map(|reply| {
            let response = match &result {
                Ok(path) => crate::remote::proto::RemoteResponse::ok(path.display().to_string()),
                Err(message) => crate::remote::proto::RemoteResponse::err_with_message(
                    "personal_export_failed",
                    message.clone(),
                ),
            };
            (reply, response)
        });
        self.personal_export.in_progress = false;
        match result {
            Ok(path) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.personal_data_export = PersonalDataExportStatus::Succeeded;
                }
                self.status.text = match crate::i18n::current() {
                    crate::i18n::Language::Korean => format!(
                        "내보내기 완료: {} · 인증정보/미디어 제외 · 청취 기록이 포함된 개인 파일",
                        path.display()
                    ),
                    crate::i18n::Language::Japanese => format!(
                        "エクスポート完了: {} · 認証情報/メディア除外 · 聴取履歴を含む個人ファイル",
                        path.display()
                    ),
                    _ => format!(
                        "Exported: {} · credentials/media omitted · private listening history included",
                        path.display()
                    ),
                };
                self.status.kind = StatusKind::Info;
            }
            Err(error) => {
                if let Some(settings) = self.settings.as_mut() {
                    settings.personal_data_export = PersonalDataExportStatus::Failed;
                }
                self.status.text = format!(
                    "{}: {}",
                    t!(
                        "Personal-data export failed",
                        "개인 데이터 내보내기 실패",
                        "個人データのエクスポートに失敗"
                    ),
                    crate::util::sanitize::sanitize_error_text(error)
                );
                self.status.kind = StatusKind::Error;
            }
        }
        self.dirty = true;
        if let Some((reply, response)) = remote_response {
            let _ = reply.send(response);
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;
    use crate::playlists::Playlist;

    #[test]
    fn personal_export_config_uses_settings_draft_or_closed_live_autoplay_streaming() {
        let mut app = App::new(100);
        app.config.autoplay_streaming = Some(false);
        app.autoplay_streaming = true;

        assert_eq!(app.personal_export_config().autoplay_streaming, Some(true));

        app.open_settings();
        app.settings.as_mut().unwrap().draft.autoplay_streaming = false;

        assert_eq!(app.personal_export_config().autoplay_streaming, Some(false));
    }

    #[test]
    fn personal_export_config_separates_live_and_draft_local_themes() {
        let mut app = App::new(100);
        app.theme.set_preset(crate::theme::ThemePreset::Midnight);
        app.config.theme = app.theme.clone();
        app.local_mode.normal_mode_theme = Some(app.theme.clone());
        app.local_dedicated_mode = true;
        app.theme = ThemeConfig::local_launch();
        app.theme
            .set_override(crate::theme::ThemeRole::Accent, "#123456")
            .unwrap();

        let live = app.personal_export_config();
        assert_eq!(live.theme.preset, "midnight");
        assert_eq!(
            live.local_theme
                .as_ref()
                .and_then(|theme| theme.overrides.get("accent"))
                .map(String::as_str),
            Some("#123456")
        );

        app.open_settings();
        let draft = &mut app.settings.as_mut().unwrap().draft.theme;
        draft.set_preset(crate::theme::ThemePreset::Custom);
        draft
            .set_override(crate::theme::ThemeRole::Accent, "#ABCDEF")
            .unwrap();

        let projected = app.personal_export_config();
        assert_eq!(projected.theme.preset, "midnight");
        assert_eq!(
            projected
                .local_theme
                .unwrap()
                .custom_overrides
                .get("accent")
                .map(String::as_str),
            Some("#ABCDEF")
        );
    }

    #[test]
    fn live_export_preflight_accepts_boundary_and_rejects_overflow() {
        assert_eq!(
            checked_playlist_track_count([4_000, 6_000]),
            Ok(PLAYLIST_TRACK_LIMIT)
        );
        assert_eq!(
            checked_playlist_track_count([PLAYLIST_TRACK_LIMIT, 1]),
            Err(SizeError::TooManyPlaylistTracks(PLAYLIST_TRACK_LIMIT + 1))
        );
        assert_eq!(
            checked_playlist_track_count([usize::MAX, 1]),
            Err(SizeError::Overflow)
        );
    }

    #[test]
    fn live_export_rejects_nested_text_heap_amplification_before_clone() {
        let mut app = App::new(100);
        let mut song = Song::remote("track", "Track", "Artist", "1:00");
        song.artists = vec![String::new(); NESTED_TEXT_ITEMS_LIMIT + 1];
        app.playlists_mut().playlists.push(Playlist {
            id: "nested".to_owned(),
            name: "Nested".to_owned(),
            songs: vec![song],
        });
        let config = app.personal_export_config();

        assert_eq!(
            app.personal_export_size_preflight(&config),
            Err(SizeError::TooManyNestedTextItems(
                NESTED_TEXT_ITEMS_LIMIT + 1
            ))
        );
    }

    #[test]
    fn oversized_live_export_rejects_before_busy_or_source_clone_and_replies() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.open_settings();
        app.playlists_mut().playlists.push(Playlist {
            id: "too-large".to_owned(),
            name: "Too large".to_owned(),
            songs: vec![Song::remote("track", "Track", "Artist", "1:00"); PLAYLIST_TRACK_LIMIT + 1],
        });
        crate::i18n::set_language(crate::i18n::Language::Korean);
        let (reply, mut response) = tokio::sync::oneshot::channel();

        let cmds = app.start_personal_export(PathBuf::from("/unused"), 2, Some(reply.into()));

        assert!(cmds.is_empty(), "rejection must not carry cloned sources");
        assert!(!app.personal_export.in_progress);
        assert_eq!(
            app.settings.as_ref().unwrap().personal_data_export,
            PersonalDataExportStatus::Failed
        );
        assert!(app.status.text.contains("실행 중인 ytt를 닫은 뒤"));
        assert!(app.status.text.contains("ytt data export"));
        let response = response.try_recv().expect("immediate rejection reply");
        assert!(!response.ok);
        assert_eq!(
            response.reason.as_deref(),
            Some("personal_export_too_large")
        );
        assert!(
            response
                .message
                .as_deref()
                .is_some_and(|message| message.contains("Close the running ytt instance"))
        );
        crate::i18n::set_language(crate::i18n::Language::English);
    }

    #[test]
    fn remote_completion_clears_busy_before_reply_is_observable() {
        let mut app = App::new(100);
        app.personal_export.in_progress = true;
        let (reply, mut response) = tokio::sync::oneshot::channel();

        app.finish_personal_export(Ok(PathBuf::from("/tmp/export.json")), Some(reply.into()));

        assert!(!app.personal_export.in_progress);
        let response = response.try_recv().expect("owner-lane completion reply");
        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("/tmp/export.json"));
    }
}
