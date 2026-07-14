use super::*;

fn open_audio_output_picker(app: &mut App) {
    app.open_settings();
    let settings = app.settings.as_mut().expect("settings open");
    settings.tab = SettingsTab::Playback;
    settings.row = settings
        .fields()
        .iter()
        .position(|field| *field == Field::AudioOutput)
        .expect("audio output row");
    let _ = app.settings_activate();
    assert!(app.overlays.audio_output_picker.is_some());
}

fn device(name: &str, description: &str) -> crate::player::AudioDevice {
    crate::player::AudioDevice {
        name: name.to_owned(),
        description: description.to_owned(),
    }
}

#[test]
fn playback_settings_replaces_raw_route_fields_with_one_picker_action() {
    let fields = SettingsTab::Playback.fields();
    assert!(fields.contains(&Field::AudioOutput));
    assert!(!fields.contains(&Field::AudioMpvOutput));
    assert!(!fields.contains(&Field::AudioMpvDevice));
    assert_eq!(Field::AudioOutput.kind(), FieldKind::Button);
}

#[test]
fn picker_renders_auto_detected_unavailable_and_manual_rows_responsively() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config.audio.mpv.device = Some("wasapi/missing-guid".to_owned());
    app.audio_devices.loading = false;
    app.audio_devices.devices = vec![
        device("coreaudio/42", "Studio Speakers"),
        device("coreaudio/77", "Studio Speakers"),
        device("pipewire/9", &"매우 긴 스피커 이름 ".repeat(20)),
    ];
    open_audio_output_picker(&mut app);

    let rows = app.audio_output_rows();
    assert!(matches!(&rows[0].kind, AudioOutputRowKind::SystemDefault));
    assert!(rows[1].label.contains("Studio Speakers · coreaudio"));
    assert!(rows.iter().any(|row| matches!(
        row.kind,
        AudioOutputRowKind::SavedUnavailable(ref name) if name == "wasapi/missing-guid"
    )));
    assert!(matches!(
        &rows.last().unwrap().kind,
        AudioOutputRowKind::Manual
    ));

    let buffer = render_app_buffer(&app, 80, 24);
    assert!(buffer_contains(&buffer, "System default"));
    assert!(buffer_contains(&buffer, "Studio Speakers"));
    let _ = render_app_buffer(&app, 30, 30);
    let popup = app
        .overlays
        .audio_output_picker
        .as_ref()
        .unwrap()
        .rect
        .get()
        .expect("popup rect");
    assert!(popup.right() <= 30 && popup.bottom() <= 30);
    assert!(
        app.hits
            .regions()
            .iter()
            .any(|region| matches!(region.target, MouseTarget::AudioOutputRow(_)))
    );
}

#[test]
fn narrow_picker_keeps_route_state_and_core_actions_visible() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.config
        .audio
        .mpv
        .set_detected_device("wasapi/saved".to_owned());
    app.audio_devices.loading = false;
    app.audio_devices.current_device = Some("wasapi/current".to_owned());
    app.audio_devices.devices = vec![
        device(
            "wasapi/current",
            "Current speakers with a deliberately very long display name",
        ),
        device(
            "wasapi/saved",
            "Saved speakers with a deliberately very long display name",
        ),
    ];
    open_audio_output_picker(&mut app);

    let buffer = render_app_buffer(&app, 70, 24);
    assert!(buffer_contains(&buffer, "[now]"));
    assert!(buffer_contains(&buffer, "[saved]"));
    assert!(buffer_contains(&buffer, "2 outputs · default available"));
    assert!(buffer_contains(&buffer, "r refresh · Esc"));
}

#[test]
fn manual_editor_bounds_input_rejects_invisible_controls_and_esc_returns_then_closes() {
    let mut app = App::new(100);
    app.audio_devices.loading = false;
    open_audio_output_picker(&mut app);
    let manual = app.audio_output_rows().len() - 1;
    app.overlays.audio_output_picker.as_mut().unwrap().selected = manual;

    let _ = app.audio_output_picker_key(key(KeyCode::Enter));
    assert!(
        app.overlays
            .audio_output_picker
            .as_ref()
            .unwrap()
            .editing_manual
    );
    for _ in 0..600 {
        let _ = app.audio_output_picker_key(key(KeyCode::Char('a')));
    }
    let before = app
        .overlays
        .audio_output_picker
        .as_ref()
        .unwrap()
        .manual_input
        .clone();
    let _ = app.audio_output_picker_key(key(KeyCode::Char('\u{202e}')));
    assert_eq!(
        app.overlays
            .audio_output_picker
            .as_ref()
            .unwrap()
            .manual_input,
        before
    );
    assert_eq!(before.len(), 512);

    let _ = app.audio_output_picker_key(key(KeyCode::Esc));
    assert!(
        !app.overlays
            .audio_output_picker
            .as_ref()
            .unwrap()
            .editing_manual
    );
    let _ = app.audio_output_picker_key(key(KeyCode::Esc));
    assert!(app.overlays.audio_output_picker.is_none());
    assert_eq!(app.mode, Mode::Settings);
}

#[test]
fn manual_editor_keeps_long_id_tail_and_caret_visible() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = App::new(100);
    app.audio_devices.loading = false;
    open_audio_output_picker(&mut app);
    let manual = app.audio_output_rows().len() - 1;
    let picker = app.overlays.audio_output_picker.as_mut().unwrap();
    picker.selected = manual;
    picker.manual_input = format!("wasapi/{{{}}}-visible-tail", "long-prefix".repeat(12));
    let _ = app.audio_output_picker_key(key(KeyCode::Enter));

    let buffer = render_app_buffer(&app, 70, 24);
    assert!(buffer_contains(&buffer, "…"));
    assert!(buffer_contains(&buffer, "visible-tail▏"));
}

#[test]
fn leaving_manual_editor_by_navigation_or_click_makes_enter_apply_highlighted_device() {
    let mut keyboard = App::new(100);
    keyboard.audio_devices.loading = false;
    keyboard.audio_devices.devices = vec![device("coreaudio/42", "Speakers")];
    open_audio_output_picker(&mut keyboard);
    let manual = keyboard.audio_output_rows().len() - 1;
    keyboard
        .overlays
        .audio_output_picker
        .as_mut()
        .unwrap()
        .selected = manual;
    let _ = keyboard.audio_output_picker_key(key(KeyCode::Enter));
    let _ = keyboard.audio_output_picker_key(key(KeyCode::Up));
    assert!(
        !keyboard
            .overlays
            .audio_output_picker
            .as_ref()
            .unwrap()
            .editing_manual
    );
    let commands = keyboard.audio_output_picker_key(key(KeyCode::Enter));
    assert!(commands.iter().flat_map(Cmd::player_commands).any(|command| {
        matches!(
            command,
            PlayerCmd::SelectAudioDevice { device: Some(name), .. } if name == "coreaudio/42"
        )
    }));

    let mut mouse = App::new(100);
    mouse.audio_devices.loading = false;
    mouse.audio_devices.devices = vec![device("wasapi/7", "Headphones")];
    open_audio_output_picker(&mut mouse);
    let manual = mouse.audio_output_rows().len() - 1;
    mouse
        .overlays
        .audio_output_picker
        .as_mut()
        .unwrap()
        .selected = manual;
    let _ = mouse.audio_output_picker_key(key(KeyCode::Enter));
    let _ = click_target(&mut mouse, MouseTarget::AudioOutputRow(1));
    assert!(
        !mouse
            .overlays
            .audio_output_picker
            .as_ref()
            .unwrap()
            .editing_manual
    );
    let commands = mouse.audio_output_picker_key(key(KeyCode::Enter));
    assert!(
        commands
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|command| {
                matches!(
                    command,
                    PlayerCmd::SelectAudioDevice { device: Some(name), .. } if name == "wasapi/7"
                )
            })
    );
}

#[test]
fn hotplug_reorder_preserves_cursor_by_device_id() {
    let mut app = App::new(100);
    app.audio_devices.devices = vec![device("wasapi/a", "A"), device("wasapi/b", "B")];
    open_audio_output_picker(&mut app);
    app.overlays.audio_output_picker.as_mut().unwrap().selected = 2;

    app.replace_audio_output_devices(vec![device("wasapi/b", "B"), device("wasapi/a", "A")]);
    let picker = app.overlays.audio_output_picker.as_ref().unwrap();
    assert!(matches!(
        app.audio_output_rows()[picker.selected].kind,
        AudioOutputRowKind::Device(ref name) if name == "wasapi/b"
    ));
}

#[test]
fn mouse_single_click_only_focuses_double_click_applies_and_outside_closes() {
    let mut app = App::new(100);
    app.audio_devices.loading = false;
    app.audio_devices.devices = vec![device("coreaudio/42", "Speakers")];
    open_audio_output_picker(&mut app);

    let commands = click_target(&mut app, MouseTarget::AudioOutputRow(1));
    assert!(commands.is_empty(), "single click must only focus");
    assert_eq!(
        app.overlays.audio_output_picker.as_ref().unwrap().selected,
        1
    );
    let commands = double_click_target(&mut app, MouseTarget::AudioOutputRow(1));
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Cmd::PlayerControl(_))),
        "double click must request a correlated player selection"
    );

    let _ = render_app_buffer(&app, 80, 24);
    let commands = app.update(Msg::MouseClick {
        col: 0,
        row: 0,
        multi: false,
    });
    assert!(commands.is_empty());
    assert!(app.overlays.audio_output_picker.is_none());
}

#[test]
fn detected_selection_persists_only_after_correlated_success_without_position_change() {
    let mut app = App::new(100);
    app.config.audio.mpv.output = Some("coreaudio".to_owned());
    app.audio_devices.devices = vec![device("coreaudio/42", "Speakers")];
    open_audio_output_picker(&mut app);
    let epoch = app.playback.position_epoch;

    let mut commands = app.request_audio_output_selection(
        Some("coreaudio/42".to_owned()),
        AudioOutputSelectionSource::Detected,
    );
    assert_eq!(app.config.audio.mpv.device, None);
    assert!(app.audio_devices.pending.is_none());
    admit_player_transition(&mut app, &mut commands);
    let pending = app.audio_devices.pending.clone().expect("admitted request");
    assert!(app.overlays.audio_output_picker.as_ref().unwrap().applying);
    assert_eq!(app.config.audio.mpv.device, None);

    let commands =
        app.finish_audio_device_selection(pending.correlation_id, pending.target, Ok(()));
    assert_eq!(app.config.audio.mpv.device.as_deref(), Some("coreaudio/42"));
    assert!(app.config.audio.mpv.output.is_none());
    assert!(app.config.audio.mpv.device_is_detected());
    assert!(app.overlays.audio_output_picker.is_none());
    assert_eq!(app.playback.position_epoch, epoch);
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn failed_selection_keeps_saved_and_draft_values_for_retry() {
    let mut app = App::new(100);
    app.config
        .audio
        .mpv
        .set_manual_device(Some("wasapi/old".to_owned()));
    open_audio_output_picker(&mut app);

    let mut commands = app.request_audio_output_selection(
        Some("wasapi/new".to_owned()),
        AudioOutputSelectionSource::Manual,
    );
    admit_player_transition(&mut app, &mut commands);
    let pending = app.audio_devices.pending.clone().expect("admitted request");
    let commands = app.finish_audio_device_selection(
        pending.correlation_id,
        pending.target,
        Err("device open failed".to_owned()),
    );

    assert!(commands.is_empty());
    assert_eq!(app.config.audio.mpv.device.as_deref(), Some("wasapi/old"));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.audio_mpv_device,
        "wasapi/old"
    );
    let picker = app.overlays.audio_output_picker.as_ref().unwrap();
    assert!(!picker.applying);
    assert!(
        picker
            .error
            .as_deref()
            .is_some_and(|error| error.contains("device open failed"))
    );
}

#[test]
fn missing_detected_preference_falls_back_for_session_without_overwriting_config() {
    let mut app = App::new(100);
    app.config
        .audio
        .mpv
        .set_detected_device("pipewire/usb".to_owned());
    let mut commands = app.on_audio_device_list(vec![device("pipewire/builtin", "Built-in")]);
    assert_eq!(app.config.audio.mpv.device.as_deref(), Some("pipewire/usb"));
    admit_player_transition(&mut app, &mut commands);
    let pending = app
        .audio_devices
        .pending
        .clone()
        .expect("fallback admitted");
    assert_eq!(pending.source, AudioOutputSelectionSource::SessionFallback);

    let commands =
        app.finish_audio_device_selection(pending.correlation_id, pending.target, Ok(()));
    assert!(commands.is_empty(), "session fallback is not persisted");
    assert_eq!(app.config.audio.mpv.device.as_deref(), Some("pipewire/usb"));
    assert_eq!(
        app.audio_devices.session_fallback_for.as_deref(),
        Some("pipewire/usb")
    );

    let commands = app.on_audio_device_list(vec![device("pipewire/usb", "USB DAC")]);
    assert!(
        commands.is_empty(),
        "hotplug must not surprise-switch mid-session"
    );
}
