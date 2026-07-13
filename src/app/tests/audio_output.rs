use super::*;

fn device(name: &str) -> crate::player::AudioDevice {
    crate::player::AudioDevice {
        name: name.to_owned(),
        description: format!("Output {name}"),
    }
}

fn selection_command(cmds: &[Cmd]) -> (u64, Option<String>) {
    cmds.iter()
        .flat_map(Cmd::player_commands)
        .find_map(|command| match command {
            PlayerCmd::SelectAudioDevice {
                correlation_id,
                device,
            } => Some((*correlation_id, device.clone())),
            _ => None,
        })
        .expect("audio-device selection command")
}

fn request_and_admit(
    app: &mut App,
    target: Option<&str>,
    source: AudioOutputSelectionSource,
) -> u64 {
    let mut commands = app.request_audio_output_selection(target.map(str::to_owned), source);
    let (correlation_id, command_target) = selection_command(&commands);
    assert_eq!(command_target.as_deref(), target);
    assert!(save_config(&commands).is_none());
    admit_player_transition(app, &mut commands);
    assert!(save_config(&commands).is_none());
    assert_eq!(
        app.audio_devices
            .pending
            .as_ref()
            .map(|pending| pending.correlation_id),
        Some(correlation_id)
    );
    correlation_id
}

#[test]
fn detected_selection_persists_device_and_marker_only_after_mpv_success() {
    let mut app = App::new(100);
    app.config.audio.mpv.output = Some("wasapi".to_owned());
    app.open_settings();
    let original_mpv = app.config.audio.mpv.clone();
    let original_draft_output = app
        .settings
        .as_ref()
        .unwrap()
        .draft
        .audio_mpv_output
        .clone();
    let original_draft_device = app
        .settings
        .as_ref()
        .unwrap()
        .draft
        .audio_mpv_device
        .clone();

    let correlation_id = request_and_admit(
        &mut app,
        Some("wasapi/{new-device}"),
        AudioOutputSelectionSource::Detected,
    );

    assert_eq!(app.config.audio.mpv, original_mpv);
    let draft = &app.settings.as_ref().unwrap().draft;
    assert_eq!(draft.audio_mpv_output, original_draft_output);
    assert_eq!(draft.audio_mpv_device, original_draft_device);

    let effects = app.update(PlayerMsg::AudioDeviceSelectionResult {
        correlation_id,
        device: Some("wasapi/{new-device}".to_owned()),
        result: Ok(()),
    });

    assert_eq!(
        app.config.audio.mpv.device.as_deref(),
        Some("wasapi/{new-device}")
    );
    assert_eq!(app.config.audio.mpv.output, None);
    assert!(app.config.audio.mpv.device_is_detected());
    let draft = &app.settings.as_ref().unwrap().draft;
    assert!(draft.audio_mpv_output.is_empty());
    assert_eq!(draft.audio_mpv_device, "wasapi/{new-device}");
    let persisted = save_config(&effects).expect("successful selection persists config");
    assert_eq!(
        persisted.audio.mpv.device.as_deref(),
        Some("wasapi/{new-device}")
    );
    assert!(persisted.audio.mpv.device_is_detected());
}

#[test]
fn mpv_selection_failure_leaves_config_and_draft_unchanged_without_persisting() {
    let mut app = App::new(100);
    app.config.audio.mpv.output = Some("pipewire".to_owned());
    app.config.audio.mpv.device = Some("pipewire/old".to_owned());
    app.open_settings();
    let original_mpv = app.config.audio.mpv.clone();
    let original_draft_output = app
        .settings
        .as_ref()
        .unwrap()
        .draft
        .audio_mpv_output
        .clone();
    let original_draft_device = app
        .settings
        .as_ref()
        .unwrap()
        .draft
        .audio_mpv_device
        .clone();

    let correlation_id = request_and_admit(
        &mut app,
        Some("pipewire/new"),
        AudioOutputSelectionSource::Detected,
    );
    let effects = app.update(PlayerMsg::AudioDeviceSelectionResult {
        correlation_id,
        device: Some("pipewire/new".to_owned()),
        result: Err("device open failed".to_owned()),
    });

    assert!(save_config(&effects).is_none());
    assert_eq!(app.config.audio.mpv, original_mpv);
    let draft = &app.settings.as_ref().unwrap().draft;
    assert_eq!(draft.audio_mpv_output, original_draft_output);
    assert_eq!(draft.audio_mpv_device, original_draft_device);
    assert!(app.audio_devices.pending.is_none());
}

#[test]
fn missing_detected_device_falls_back_for_session_and_does_not_auto_return_on_hotplug() {
    let mut app = App::new(100);
    app.config
        .audio
        .mpv
        .set_detected_device("coreaudio/saved".to_owned());
    let saved_mpv = app.config.audio.mpv.clone();

    let mut fallback = app.update(PlayerMsg::AudioDeviceList(vec![device("coreaudio/other")]));
    let (correlation_id, target) = selection_command(&fallback);
    assert_eq!(target, None, "missing detected preference must select auto");
    assert_eq!(app.config.audio.mpv, saved_mpv);
    assert!(save_config(&fallback).is_none());

    admit_player_transition(&mut app, &mut fallback);
    assert_eq!(app.audio_devices.session_fallback_for, None);
    let effects = app.update(PlayerMsg::AudioDeviceSelectionResult {
        correlation_id,
        device: None,
        result: Ok(()),
    });
    assert!(effects.is_empty());
    assert_eq!(app.audio_devices.current_device, None);
    assert_eq!(app.config.audio.mpv, saved_mpv);
    assert!(app.config.audio.mpv.device_is_detected());

    let hotplug = app.update(PlayerMsg::AudioDeviceList(vec![
        device("coreaudio/other"),
        device("coreaudio/saved"),
    ]));
    assert!(
        hotplug.is_empty(),
        "hotplug must not switch output mid-session"
    );
    assert!(app.audio_devices.pending.is_none());
    assert_eq!(
        app.audio_devices.session_fallback_for.as_deref(),
        Some("coreaudio/saved")
    );
    assert_eq!(app.config.audio.mpv, saved_mpv);
}

#[test]
fn failed_session_fallback_remains_retryable() {
    let mut app = App::new(100);
    app.config
        .audio
        .mpv
        .set_detected_device("pipewire/saved".to_owned());

    let mut fallback = app.update(PlayerMsg::AudioDeviceList(vec![device("pipewire/other")]));
    let (correlation_id, target) = selection_command(&fallback);
    assert_eq!(target, None);
    admit_player_transition(&mut app, &mut fallback);
    assert_eq!(app.audio_devices.session_fallback_for, None);

    let effects = app.update(PlayerMsg::AudioDeviceSelectionResult {
        correlation_id,
        device: None,
        result: Err("temporary audio backend failure".to_owned()),
    });
    assert!(effects.is_empty());
    assert!(app.audio_devices.pending.is_none());
    assert_eq!(app.audio_devices.session_fallback_for, None);

    let retry = app.update(PlayerMsg::AudioDeviceList(vec![device("pipewire/other")]));
    let (retry_id, retry_target) = selection_command(&retry);
    assert_ne!(retry_id, correlation_id);
    assert_eq!(retry_target, None);
}

#[test]
fn missing_legacy_or_manual_device_does_not_trigger_automatic_fallback() {
    let mut legacy = App::new(100);
    legacy.config.audio.mpv.device = Some("wasapi/legacy".to_owned());
    assert!(!legacy.config.audio.mpv.device_is_detected());
    let legacy_effects = legacy.update(PlayerMsg::AudioDeviceList(vec![device("wasapi/other")]));
    assert!(legacy_effects.is_empty());
    assert!(legacy.audio_devices.pending.is_none());
    assert_eq!(
        legacy.config.audio.mpv.device.as_deref(),
        Some("wasapi/legacy")
    );

    let mut manual = App::new(100);
    manual
        .config
        .audio
        .mpv
        .set_manual_device(Some("alsa/manual".to_owned()));
    assert!(!manual.config.audio.mpv.device_is_detected());
    let manual_effects = manual.update(PlayerMsg::AudioDeviceList(vec![device("alsa/other")]));
    assert!(manual_effects.is_empty());
    assert!(manual.audio_devices.pending.is_none());
    assert_eq!(
        manual.config.audio.mpv.device.as_deref(),
        Some("alsa/manual")
    );
}

#[test]
fn stale_selection_result_cannot_complete_or_overwrite_the_current_request() {
    let mut app = App::new(100);
    app.open_settings();

    let stale_id = request_and_admit(
        &mut app,
        Some("pipewire/stale"),
        AudioOutputSelectionSource::Detected,
    );
    let failed = app.update(PlayerMsg::AudioDeviceSelectionResult {
        correlation_id: stale_id,
        device: Some("pipewire/stale".to_owned()),
        result: Err("first request failed".to_owned()),
    });
    assert!(save_config(&failed).is_none());

    let current_id = request_and_admit(
        &mut app,
        Some("pipewire/current"),
        AudioOutputSelectionSource::Detected,
    );
    assert_ne!(stale_id, current_id);
    let before = app.config.audio.mpv.clone();
    let stale_effects = app.update(PlayerMsg::AudioDeviceSelectionResult {
        correlation_id: stale_id,
        device: Some("pipewire/stale".to_owned()),
        result: Ok(()),
    });

    assert!(stale_effects.is_empty());
    assert_eq!(app.config.audio.mpv, before);
    assert_eq!(
        app.audio_devices
            .pending
            .as_ref()
            .map(|pending| pending.correlation_id),
        Some(current_id)
    );

    let current_effects = app.update(PlayerMsg::AudioDeviceSelectionResult {
        correlation_id: current_id,
        device: Some("pipewire/current".to_owned()),
        result: Ok(()),
    });
    assert!(save_config(&current_effects).is_some());
    assert_eq!(
        app.config.audio.mpv.device.as_deref(),
        Some("pipewire/current")
    );
    assert!(app.config.audio.mpv.device_is_detected());
}

#[test]
fn actor_validation_failure_clears_the_matching_request_even_without_a_device_echo() {
    let mut app = App::new(100);
    let correlation_id = request_and_admit(
        &mut app,
        Some("bad\ndevice"),
        AudioOutputSelectionSource::Manual,
    );

    let effects = app.update(PlayerMsg::AudioDeviceSelectionResult {
        correlation_id,
        // The player cannot normalize an invalid ID, so its failure deliberately echoes None.
        device: None,
        result: Err("audio device ID contains unsupported control characters".to_owned()),
    });

    assert!(effects.is_empty());
    assert!(app.audio_devices.pending.is_none());
    assert_eq!(app.config.audio.mpv.device, None);
}
