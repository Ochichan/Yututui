use super::*;

#[test]
fn audio_device_properties_emit_sanitized_cross_platform_state() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();

    dispatch_incoming(
        r#"{"event":"property-change","id":11,"name":"audio-device-list","data":[{"name":"auto","description":"Autoselect"},{"name":"wasapi/{usb}","description":"USB\nSpeakers"},{"name":"wasapi/{usb}","description":"duplicate"},{"name":"bad\u001b[2J","description":"unsafe"}]}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","id":12,"name":"audio-device","data":"auto"}"#,
        &emit,
        &mut state,
    );
    dispatch_incoming(
        r#"{"event":"property-change","id":13,"name":"current-ao","data":"wasapi"}"#,
        &emit,
        &mut state,
    );

    match rx.try_recv().expect("audio device list") {
        PlayerEvent::AudioDeviceList(devices) => {
            assert_eq!(devices.len(), 2);
            assert_eq!(devices[1].name, "wasapi/{usb}");
            assert_eq!(devices[1].description, "USB Speakers");
        }
        _ => panic!("expected audio device list"),
    }
    assert!(matches!(
        rx.try_recv(),
        Ok(PlayerEvent::AudioDeviceChanged(None))
    ));
    assert!(matches!(
        rx.try_recv(),
        Ok(PlayerEvent::CurrentAudioOutput(Some(output))) if output == "wasapi"
    ));
}

#[test]
fn audio_device_selection_inspects_then_clears_forced_ao_before_setting_device() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    let selection = PendingAudioDeviceSelection {
        correlation_id: 41,
        device: Some("coreaudio/builtin".to_owned()),
        phase: AudioDeviceSelectionPhase::InspectOutput,
    };
    remember_pending_audio_device_selection(&mut state, 20, selection).unwrap();
    state.audio_output.selection_request_id = Some(20);

    dispatch_incoming(
        r#"{"error":"success","request_id":20,"data":"coreaudio"}"#,
        &emit,
        &mut state,
    );

    let mut followup = state
        .audio_output
        .followup
        .take()
        .expect("successful AO inspection schedules the clear command");
    assert_eq!(followup.correlation_id, 41);
    assert_eq!(followup.device.as_deref(), Some("coreaudio/builtin"));
    assert_eq!(followup.phase, AudioDeviceSelectionPhase::ClearOutput);
    assert_eq!(
        state.audio_output.prior_output,
        Some(serde_json::Value::from("coreaudio"))
    );
    assert!(rx.try_recv().is_err());

    remember_pending_audio_device_selection(&mut state, 21, followup).unwrap();
    state.audio_output.selection_request_id = Some(21);
    dispatch_incoming(r#"{"error":"success","request_id":21}"#, &emit, &mut state);

    followup = state
        .audio_output
        .followup
        .take()
        .expect("successful AO clear schedules the device command");
    assert_eq!(followup.phase, AudioDeviceSelectionPhase::SetDevice);
    assert!(rx.try_recv().is_err());

    remember_pending_audio_device_selection(&mut state, 22, followup).unwrap();
    state.audio_output.selection_request_id = Some(22);
    dispatch_incoming(r#"{"error":"success","request_id":22}"#, &emit, &mut state);

    match rx.try_recv().expect("selection result") {
        PlayerEvent::AudioDeviceSelectionResult {
            correlation_id,
            device,
            result,
        } => {
            assert_eq!(correlation_id, 41);
            assert_eq!(device.as_deref(), Some("coreaudio/builtin"));
            assert_eq!(result, Ok(()));
        }
        _ => panic!("expected audio device selection result"),
    }
    assert!(state.audio_output.selection_request_id.is_none());
}

#[test]
fn failed_ao_clear_ends_selection_without_scheduling_device_command() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    let selection = PendingAudioDeviceSelection {
        correlation_id: 42,
        device: Some("pipewire/usb".to_owned()),
        phase: AudioDeviceSelectionPhase::ClearOutput,
    };
    state.audio_output.prior_output = Some(serde_json::Value::from("pipewire"));
    remember_pending_audio_device_selection(&mut state, 30, selection).unwrap();
    state.audio_output.selection_request_id = Some(30);

    dispatch_incoming(
        r#"{"error":"property unavailable","request_id":30}"#,
        &emit,
        &mut state,
    );

    assert!(state.audio_output.followup.is_none());
    assert!(state.audio_output.selection_request_id.is_none());
    assert!(matches!(
        rx.try_recv(),
        Ok(PlayerEvent::AudioDeviceSelectionResult {
            correlation_id: 42,
            device: Some(device),
            result: Err(error),
        }) if device == "pipewire/usb" && error.contains("clear")
    ));
}

#[test]
fn audio_device_selection_commands_preserve_exact_phase_order_and_restore_value() {
    let mut state = DispatchState::default();
    let mut selection = PendingAudioDeviceSelection {
        correlation_id: 43,
        device: Some("wasapi/{usb}".to_owned()),
        phase: AudioDeviceSelectionPhase::InspectOutput,
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&audio_device_selection_command(
            &state, &selection, 60,
        ))
        .unwrap(),
        serde_json::json!({"command":["get_property","options/ao"],"request_id":60})
    );

    state.audio_output.prior_output = Some(serde_json::json!([{"name":"wasapi"}]));
    selection.phase = AudioDeviceSelectionPhase::ClearOutput;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&audio_device_selection_command(
            &state, &selection, 61,
        ))
        .unwrap(),
        serde_json::json!({"command":["set_property","options/ao",""],"request_id":61})
    );
    selection.phase = AudioDeviceSelectionPhase::SetDevice;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&audio_device_selection_command(
            &state, &selection, 62,
        ))
        .unwrap(),
        serde_json::json!({"command":["set_property","audio-device","wasapi/{usb}"],"request_id":62})
    );
    selection.phase = AudioDeviceSelectionPhase::RestoreOutput;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&audio_device_selection_command(
            &state, &selection, 63,
        ))
        .unwrap(),
        serde_json::json!({"command":["set_property","options/ao",[{"name":"wasapi"}]],"request_id":63})
    );
}

#[test]
fn rejected_device_restores_forced_output_before_emitting_one_terminal_result() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    state.audio_output.prior_output = Some(serde_json::Value::from("pipewire"));
    remember_pending_audio_device_selection(
        &mut state,
        70,
        PendingAudioDeviceSelection {
            correlation_id: 44,
            device: Some("pipewire/missing".to_owned()),
            phase: AudioDeviceSelectionPhase::SetDevice,
        },
    )
    .unwrap();
    state.audio_output.selection_request_id = Some(70);

    dispatch_incoming(
        r#"{"error":"audio device error","request_id":70}"#,
        &emit,
        &mut state,
    );
    let rollback = state
        .audio_output
        .followup
        .take()
        .expect("device rejection schedules forced-output rollback");
    assert_eq!(rollback.phase, AudioDeviceSelectionPhase::RestoreOutput);
    assert!(
        rx.try_recv().is_err(),
        "rollback must precede terminal result"
    );

    remember_pending_audio_device_selection(&mut state, 71, rollback).unwrap();
    state.audio_output.selection_request_id = Some(71);
    dispatch_incoming(r#"{"error":"success","request_id":71}"#, &emit, &mut state);

    assert!(matches!(
        rx.try_recv(),
        Ok(PlayerEvent::AudioDeviceSelectionResult {
            correlation_id: 44,
            device: Some(device),
            result: Err(error),
        }) if device == "pipewire/missing" && error.contains("audio device error")
    ));
    assert!(rx.try_recv().is_err(), "selection has one terminal result");
    assert!(state.audio_output.selection_request_id.is_none());
    assert!(state.audio_output.prior_output.is_none());
    assert!(state.audio_output.selection_error.is_none());
}

#[test]
fn set_device_timeout_emits_once_and_does_not_start_a_late_rollback() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    state.audio_output.prior_output = Some(serde_json::Value::from("wasapi"));
    remember_pending_audio_device_selection(
        &mut state,
        72,
        PendingAudioDeviceSelection {
            correlation_id: 45,
            device: Some("wasapi/{slow}".to_owned()),
            phase: AudioDeviceSelectionPhase::SetDevice,
        },
    )
    .unwrap();
    state.audio_output.selection_request_id = Some(72);
    state.audio_output.selection_deadline = Some(Instant::now());

    timeout_audio_device_selection(&mut state, &emit);

    assert!(matches!(
        rx.try_recv(),
        Ok(PlayerEvent::AudioDeviceSelectionResult {
            correlation_id: 45,
            result: Err(error),
            ..
        }) if error.contains("timed out")
    ));
    assert!(rx.try_recv().is_err());
    assert!(state.pending.is_empty());
    assert!(state.audio_output.followup.is_none());
    assert!(state.audio_output.prior_output.is_none());
}

#[test]
fn refresh_reply_emits_the_normal_device_list_event() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    assert!(remember_pending_audio_device_refresh(&mut state, 50));

    dispatch_incoming(
        r#"{"error":"success","request_id":50,"data":[{"name":"alsa/default","description":"Default ALSA"}]}"#,
        &emit,
        &mut state,
    );

    assert!(matches!(
        rx.try_recv(),
        Ok(PlayerEvent::AudioDeviceList(devices))
            if devices == vec![crate::player::AudioDevice {
                name: "alsa/default".to_owned(),
                description: "Default ALSA".to_owned(),
            }]
    ));
}

#[test]
fn refresh_failure_uses_a_non_playback_error_event_and_is_sanitized() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let emit: EventSink = std::sync::Arc::new(move |event| {
        let _ = tx.try_send(event);
    });
    let mut state = DispatchState::default();
    assert!(remember_pending_audio_device_refresh(&mut state, 51));

    dispatch_incoming(
        r#"{"error":"access_token=secret","request_id":51}"#,
        &emit,
        &mut state,
    );

    assert!(matches!(
        rx.try_recv(),
        Ok(PlayerEvent::AudioDeviceRefreshFailed(error))
            if error.contains("<redacted>") && !error.contains("secret")
    ));
}
