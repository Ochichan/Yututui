use super::*;

#[test]
fn long_form_seek_row_renders_and_exposes_controls_at_supported_widths_in_both_languages() {
    let _guard = crate::i18n::lock_for_test();
    let contains_compact_row = |buffer: &ratatui::buffer::Buffer, needle: &str| {
        let needle: String = needle
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        (0..buffer.area.height).any(|y| {
            buffer_row(buffer, y)
                .chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>()
                .contains(&needle)
        })
    };
    let mut app = App::new(100);
    app.open_settings();
    focus_settings_field(
        &mut app,
        SettingsTab::Playback,
        Field::LongFormSeekOptimization,
    );
    let row = SettingsTab::Playback
        .fields()
        .iter()
        .position(|field| *field == Field::LongFormSeekOptimization)
        .expect("long-form seek field");

    for (language, label, value) in [
        (crate::i18n::Language::English, "Long-form seek", "Off"),
        (crate::i18n::Language::Korean, "긴 미디어 탐색", "끔"),
    ] {
        crate::i18n::set_language(language);
        for width in [40, 60, 80] {
            let buffer = render_app_buffer(&app, width, 40);
            assert!(
                contains_compact_row(&buffer, label),
                "{label} missing at width {width}"
            );
            assert!(
                contains_compact_row(&buffer, value),
                "{value} missing at width {width}"
            );
            assert!(
                app.hits.regions().iter().any(|region| {
                    region.target == MouseTarget::SettingsChange { row, delta: -1 }
                }),
                "decrease control missing for {label} at width {width}"
            );
            assert!(
                app.hits.regions().iter().any(|region| {
                    region.target == MouseTarget::SettingsChange { row, delta: 1 }
                }),
                "increase control missing for {label} at width {width}"
            );
        }
    }
}

#[test]
fn long_form_seek_selector_cycles_forward_reverse_and_enter() {
    use crate::config::LongFormSeekOptimization::{Auto, Off, On};

    let mut app = App::new(100);
    app.open_settings();
    focus_settings_field(
        &mut app,
        SettingsTab::Playback,
        Field::LongFormSeekOptimization,
    );
    app.settings
        .as_mut()
        .unwrap()
        .draft
        .long_form_seek_optimization = Auto;

    assert!(app.settings_change(1).is_empty());
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .long_form_seek_optimization,
        Off
    );
    assert!(app.settings_change(1).is_empty());
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .long_form_seek_optimization,
        On
    );
    assert!(app.settings_change(1).is_empty());
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .long_form_seek_optimization,
        Auto
    );

    assert!(app.settings_change(-1).is_empty());
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .long_form_seek_optimization,
        On
    );

    app.settings
        .as_mut()
        .unwrap()
        .draft
        .long_form_seek_optimization = Auto;
    assert!(app.settings_activate().is_empty());
    assert_eq!(
        app.settings
            .as_ref()
            .unwrap()
            .draft
            .long_form_seek_optimization,
        Off,
        "Enter must match forward cycling"
    );
}

#[test]
fn reset_all_preserves_tui_mpv_output_device_and_cache_like_origin_main() {
    // This is intentionally different from the daemon GUI's full-Config reset. The terminal
    // Settings reset has never included the mpv output/device/cache draft fields; keep that exact
    // behavior unless the user explicitly approves the visible settings-semantics change.
    let mut app = app_playing(1, 0);
    app.config.audio.mpv.output = Some("pipewire".to_owned());
    app.config.audio.mpv.device = Some("alsa/custom".to_owned());
    app.config.audio.mpv.cache_forward = "64MiB".to_owned();
    app.config.audio.mpv.cache_back = "12MiB".to_owned();
    app.config.audio.mpv.long_form_seek_optimization = crate::config::LongFormSeekOptimization::On;
    app.config.audio.mpv.cache_defaults_revision = u64::MAX;

    app.update(Msg::Key(key(KeyCode::Char('o'))));
    let mut reset = app.settings_reset_all();
    assert!(reset.iter().flat_map(Cmd::player_commands).any(|command| {
        matches!(
            command,
            PlayerCmd::SetLongFormSeekOptimization(crate::config::LongFormSeekOptimization::Off)
        )
    }));
    admit_player_transition(&mut app, &mut reset);

    let draft = &app.settings.as_ref().unwrap().draft;
    assert_eq!(draft.audio_mpv_output, "pipewire");
    assert_eq!(draft.audio_mpv_device, "alsa/custom");
    assert_eq!(draft.audio_mpv_cache_forward, "64MiB");
    assert_eq!(draft.audio_mpv_cache_back, "12MiB");
    assert_eq!(
        draft.long_form_seek_optimization,
        crate::config::LongFormSeekOptimization::Off
    );

    let cmds = super::settings_ui::update_and_admit(&mut app, Msg::Key(key(KeyCode::Char('q'))));
    let saved = save_config(&cmds).unwrap();
    assert_eq!(saved.audio.mpv.output.as_deref(), Some("pipewire"));
    assert_eq!(saved.audio.mpv.device.as_deref(), Some("alsa/custom"));
    assert_eq!(saved.audio.mpv.cache_forward, "64MiB");
    assert_eq!(saved.audio.mpv.cache_back, "12MiB");
    assert_eq!(
        saved.audio.mpv.long_form_seek_optimization,
        crate::config::LongFormSeekOptimization::Off
    );
    assert_eq!(saved.audio.mpv.cache_defaults_revision, u64::MAX);
}

#[test]
fn settings_save_updates_live_long_form_policy_only_after_batch_admission() {
    use crate::config::LongFormSeekOptimization::{Off, On};

    let mut app = app_playing(1, 0);
    app.open_settings();
    app.settings
        .as_mut()
        .expect("settings")
        .draft
        .long_form_seek_optimization = On;

    let rejected = app.close_settings();
    assert!(
        rejected
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|command| { matches!(command, PlayerCmd::SetLongFormSeekOptimization(On)) })
    );
    assert!(
        reject_player_transition(
            &mut app,
            rejected,
            crate::util::delivery::DeliveryError::Closed,
        )
        .is_empty()
    );
    assert_eq!(app.config.audio.mpv.long_form_seek_optimization, Off);
    assert!(app.settings.is_some());

    let mut accepted = app.close_settings();
    admit_player_transition(&mut app, &mut accepted);
    assert_eq!(app.config.audio.mpv.long_form_seek_optimization, On);
    assert!(app.settings.is_none());
}

#[test]
fn direct_tui_cache_edits_mark_current_without_lowering_future_revision() {
    let mut app = app_playing(1, 0);
    app.config.audio.mpv.cache_defaults_revision = 0;
    app.update(Msg::Key(key(KeyCode::Char('o'))));

    app.settings.as_mut().unwrap().draft.audio_mpv_cache_forward = "48MiB".to_owned();
    app.settings_persist_text_field(Field::AudioMpvCacheForward);
    assert_eq!(app.config.audio.mpv.cache_forward, "48MiB");
    assert_eq!(
        app.config.audio.mpv.cache_defaults_revision,
        crate::config::MPV_CACHE_DEFAULTS_REVISION
    );

    app.config.audio.mpv.cache_defaults_revision = u64::MAX;
    app.settings.as_mut().unwrap().draft.audio_mpv_cache_back = "6MiB".to_owned();
    app.settings_persist_text_field(Field::AudioMpvCacheBack);
    assert_eq!(app.config.audio.mpv.cache_back, "6MiB");
    assert_eq!(app.config.audio.mpv.cache_defaults_revision, u64::MAX);
}
