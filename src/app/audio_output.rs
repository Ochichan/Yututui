//! Runtime integration for the Settings audio-output picker.
//!
//! User choices pass through the ordinary player-admission boundary, then wait for mpv's
//! correlated IPC reply. Persistence happens only after that terminal success. A detected saved
//! endpoint which disappears is preserved in config while this process uses system routing.

use super::*;

impl App {
    pub(in crate::app) fn request_audio_output_refresh(&mut self) -> Vec<Cmd> {
        if self.audio_devices.loading && self.audio_devices.inventory_observed {
            return Vec::new();
        }
        self.player_intent(
            "refresh_audio_devices",
            PlayerCmd::RefreshAudioDevices,
            PlayerCommit::AudioOutputRefresh,
        )
    }

    pub(in crate::app) fn request_audio_output_selection(
        &mut self,
        target: Option<String>,
        source: AudioOutputSelectionSource,
    ) -> Vec<Cmd> {
        if self.audio_devices.pending.is_some() {
            self.audio_output_selection_failed(
                t!(
                    "An audio output change is already in progress",
                    "오디오 출력 변경이 이미 진행 중입니다"
                )
                .to_owned(),
            );
            return Vec::new();
        }

        let target = normalize_audio_output_target(target);
        if matches!(
            source,
            AudioOutputSelectionSource::Detected | AudioOutputSelectionSource::Manual
        ) && target.is_none()
        {
            self.audio_output_selection_failed(
                t!("Enter a valid device ID", "올바른 장치 ID를 입력하세요").to_owned(),
            );
            return Vec::new();
        }

        let correlation_id = self.audio_devices.allocate_correlation_id();
        self.player_intent(
            "select_audio_device",
            PlayerCmd::SelectAudioDevice {
                correlation_id,
                device: target.clone(),
            },
            PlayerCommit::AudioOutputSelection {
                correlation_id,
                target,
                source,
            },
        )
    }

    pub(in crate::app) fn audio_output_selection_is_current(&self, correlation_id: u64) -> bool {
        self.audio_devices.pending.is_none()
            && self.audio_devices.correlation_is_latest(correlation_id)
    }

    pub(in crate::app) fn commit_audio_output_refresh(&mut self) {
        self.audio_devices.loading = true;
        self.audio_devices.error = None;
        if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
            picker.error = None;
        }
    }

    pub(in crate::app) fn commit_audio_output_selection(
        &mut self,
        correlation_id: u64,
        target: Option<String>,
        source: AudioOutputSelectionSource,
    ) {
        self.audio_output_selection_admitted(correlation_id, target, source);
    }

    pub(in crate::app) fn on_audio_device_list(
        &mut self,
        devices: Vec<crate::player::AudioDevice>,
    ) -> Vec<Cmd> {
        self.replace_audio_output_devices(devices);

        let Some(preferred) = configured_audio_device(self) else {
            return Vec::new();
        };
        let detected_preference = self.config.audio.mpv.device_is_detected();
        let preferred_missing = !self
            .audio_devices
            .devices
            .iter()
            .any(|device| device.name == preferred);
        if detected_preference
            && preferred_missing
            && self.audio_devices.pending.is_none()
            && self.audio_devices.session_fallback_for.is_none()
        {
            return self
                .request_audio_output_selection(None, AudioOutputSelectionSource::SessionFallback);
        }
        Vec::new()
    }

    pub(in crate::app) fn on_audio_device_refresh_failed(&mut self, error: String) -> Vec<Cmd> {
        let error = crate::util::sanitize::sanitize_error_text(error);
        self.audio_devices.loading = false;
        self.audio_devices.error = Some(error.clone());
        self.audio_output_selection_failed(error.clone());
        self.status.kind = StatusKind::Error;
        self.status.text = format!(
            "{}: {error}",
            t!(
                "Could not refresh audio outputs",
                "오디오 출력 목록을 새로 고치지 못했습니다"
            )
        );
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn on_audio_device_changed(&mut self, device: Option<String>) -> Vec<Cmd> {
        self.audio_devices.current_device = normalize_audio_output_target(device);
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn on_current_audio_output(&mut self, output: Option<String>) -> Vec<Cmd> {
        let output = output.and_then(|output| {
            let output = output.trim();
            (!output.is_empty()).then(|| output.to_owned())
        });
        let fell_back_to_null = output
            .as_deref()
            .is_some_and(|output| output.eq_ignore_ascii_case("null"));
        self.audio_devices.current_output = output;
        self.dirty = true;

        if fell_back_to_null
            && self.audio_devices.inventory_observed
            && self.audio_devices.pending.is_none()
            && self.audio_devices.session_fallback_for.is_none()
            && configured_audio_device(self).is_some()
        {
            return self
                .request_audio_output_selection(None, AudioOutputSelectionSource::SessionFallback);
        }
        Vec::new()
    }

    pub(in crate::app) fn finish_audio_device_selection(
        &mut self,
        correlation_id: u64,
        device: Option<String>,
        result: Result<(), String>,
    ) -> Vec<Cmd> {
        let device = normalize_audio_output_target(device);
        let Some(pending) = self.audio_devices.pending.as_ref() else {
            tracing::debug!(
                correlation_id,
                "ignored stale audio-output selection result"
            );
            return Vec::new();
        };
        if pending.correlation_id != correlation_id {
            tracing::warn!(
                correlation_id,
                expected_correlation_id = pending.correlation_id,
                "ignored mismatched audio-output selection result"
            );
            return Vec::new();
        }
        if result.is_ok() && pending.target != device {
            tracing::warn!(
                correlation_id,
                ?device,
                expected_device = ?pending.target,
                "ignored successful audio-output result for the wrong device"
            );
            return Vec::new();
        }
        let pending = self
            .audio_devices
            .pending
            .take()
            .expect("matched pending audio-output selection");

        if let Err(error) = result {
            let error = crate::util::sanitize::sanitize_error_text(error);
            self.audio_devices.error = Some(error.clone());
            self.audio_output_selection_failed(error.clone());
            self.status.kind = StatusKind::Error;
            self.status.text = format!(
                "{}: {error}",
                t!(
                    "Could not change audio output",
                    "오디오 출력을 변경하지 못했습니다"
                )
            );
            self.dirty = true;
            return Vec::new();
        }

        self.audio_devices.current_device = pending.target.clone();
        self.audio_devices.error = None;
        match pending.source {
            AudioOutputSelectionSource::SessionFallback => {
                self.audio_devices.session_fallback_for = configured_audio_device(self);
                if let Some(picker) = self.overlays.audio_output_picker.as_mut() {
                    picker.applying = false;
                    picker.error = Some(
                        t!(
                            "Saved output is unavailable; using the system default for this session",
                            "저장된 출력을 찾을 수 없어 이번 세션에는 시스템 기본값을 사용합니다"
                        )
                        .to_owned(),
                    );
                }
                self.status.kind = StatusKind::Error;
                self.status.text = t!(
                    "Saved audio output unavailable; using system default for this session",
                    "저장된 오디오 출력을 찾을 수 없어 이번 세션에는 시스템 기본값을 사용합니다"
                )
                .to_owned();
                self.dirty = true;
                Vec::new()
            }
            source => self.persist_selected_audio_output(source, pending.target),
        }
    }

    fn persist_selected_audio_output(
        &mut self,
        source: AudioOutputSelectionSource,
        target: Option<String>,
    ) -> Vec<Cmd> {
        match source {
            AudioOutputSelectionSource::SystemDefault => {
                self.config.audio.mpv.set_system_default_device();
            }
            AudioOutputSelectionSource::Detected => self
                .config
                .audio
                .mpv
                .set_detected_device(target.clone().expect("detected selection has a device")),
            AudioOutputSelectionSource::Manual => {
                self.config.audio.mpv.set_manual_device(target.clone());
            }
            AudioOutputSelectionSource::SessionFallback => {
                unreachable!("session fallback is never persisted")
            }
        }
        if let Some(settings) = self.settings.as_mut() {
            settings.draft.audio_mpv_output.clear();
            settings.draft.audio_mpv_device = target.clone().unwrap_or_default();
        }
        self.audio_devices.session_fallback_for = None;
        let label = audio_output_label(self, target.as_deref());
        self.audio_output_selection_succeeded();
        self.status.kind = StatusKind::Info;
        self.status.text = format!(
            "{}: {label}",
            t!("Audio output changed", "오디오 출력을 변경했습니다")
        );
        self.dirty = true;
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    /// Clear transport-owned observations. If this process had fallen back, the fresh player
    /// must make that same decision again from its first complete inventory.
    pub(in crate::app) fn audio_output_transport_closed(&mut self, reason: &str) {
        if self.audio_devices.pending.take().is_some() {
            self.audio_output_selection_failed(format!(
                "{}: {}",
                t!(
                    "Audio output change was interrupted",
                    "오디오 출력 변경이 중단되었습니다"
                ),
                crate::util::sanitize::sanitize_error_text(reason)
            ));
        }
        self.audio_devices.loading = true;
        self.audio_devices.inventory_observed = false;
        self.audio_devices.current_device = None;
        self.audio_devices.current_output = None;
        self.audio_devices.session_fallback_for = None;
    }
}

fn normalize_audio_output_target(target: Option<String>) -> Option<String> {
    target.and_then(|target| {
        let target = target.trim();
        (!target.is_empty() && !target.eq_ignore_ascii_case("auto")).then(|| target.to_owned())
    })
}

fn configured_audio_device(app: &App) -> Option<String> {
    normalize_audio_output_target(app.config.audio.mpv.device.clone())
}

fn audio_output_label(app: &App, target: Option<&str>) -> String {
    let Some(target) = target else {
        return t!("System default", "시스템 기본값").to_owned();
    };
    app.audio_devices
        .devices
        .iter()
        .find(|device| device.name == target)
        .map(|device| device.description.clone())
        .unwrap_or_else(|| target.to_owned())
}
