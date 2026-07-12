use super::*;

#[test]
fn load_song_reapplies_active_eq_chain() {
    let mut app = app_playing(3, 0);
    app.audio.bands = EqPreset::BassBoost.gains();
    // A manual skip reloads the track and must re-send the EQ chain (gapless rebuild
    // can otherwise drop the labeled bands).
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert!(
        af(&cmds)
            .expect("a SetAudioFilter cmd")
            .contains("equalizer")
    );
}

#[test]
fn apply_config_pushes_playback_settings() {
    let cfg = crate::config::Config {
        eq_preset: EqPreset::Vocal,
        normalize: Some(true),
        speed: Some(1.5),
        seek_seconds: Some(30.0),
        shuffle: Some(true),
        repeat: crate::queue::Repeat::One,
        autoplay_streaming: Some(true),
        ..crate::config::Config::default()
    };
    let mut app = App::new(100);
    app.apply_config(&cfg);
    assert_eq!(app.audio.preset, EqPreset::Vocal);
    assert_eq!(app.audio.bands, EqPreset::Vocal.gains());
    assert!(app.audio.normalize);
    assert!((app.playback.speed - 1.5).abs() < 1e-9);
    assert!((app.audio.seek_seconds - 30.0).abs() < 1e-9);
    assert!(app.queue.shuffle);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::One);
    // Music-mode invariant: this config carries both repeat and autoplay on, which can't both be
    // on — apply_config keeps the deliberate repeat and drops streaming.
    assert!(!app.autoplay_streaming);
}

#[test]
fn apply_config_with_autoplay_and_no_repeat_keeps_streaming() {
    // The reconcile only fires when both are on: autoplay alone still pushes through.
    let cfg = crate::config::Config {
        repeat: crate::queue::Repeat::Off,
        autoplay_streaming: Some(true),
        ..crate::config::Config::default()
    };
    let mut app = App::new(100);
    app.apply_config(&cfg);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
    assert!(app.autoplay_streaming);
    assert!(app.streaming_active());
}

#[test]
fn cannot_enable_streaming_while_repeat_on() {
    let mut app = app_playing(3, 0);
    app.queue.repeat = crate::queue::Repeat::All;
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(!app.autoplay_streaming, "streaming stays off");
    assert!(!app.status.text.is_empty(), "a message is shown");
    assert!(save_config(&cmds).is_none(), "nothing persisted");
    assert_eq!(
        app.queue.repeat,
        crate::queue::Repeat::All,
        "repeat untouched"
    );
}

#[test]
fn cannot_enable_repeat_while_streaming_on() {
    let mut app = app_playing(3, 0);
    app.autoplay_streaming = true;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert_eq!(
        app.queue.repeat,
        crate::queue::Repeat::Off,
        "repeat stays off"
    );
    assert!(app.autoplay_streaming, "streaming untouched");
    assert!(!app.status.text.is_empty(), "a message is shown");
    assert!(save_config(&cmds).is_none(), "nothing persisted");
}

#[test]
fn streaming_toggle_in_radio_mode_keeps_preference() {
    let mut app = app_playing(3, 0);
    app.autoplay_streaming = true; // a real preference carried from music mode
    app.radio_dedicated_mode = true;
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(app.autoplay_streaming, "stored preference is preserved");
    assert!(
        !app.streaming_active(),
        "but streaming is effectively off in radio mode"
    );
    assert!(!app.status.text.is_empty(), "a message explains why");
    assert!(
        save_config(&cmds).is_none(),
        "no persist — the preference is untouched"
    );
}

#[test]
fn streaming_active_false_in_radio_and_on_a_station() {
    let mut app = app_playing(3, 0);
    app.autoplay_streaming = true;
    assert!(app.streaming_active());
    app.radio_dedicated_mode = true;
    assert!(!app.streaming_active(), "off in dedicated Radio mode");
    app.radio_dedicated_mode = false;
    assert!(app.streaming_active());
    // A live station playing in normal mode also suppresses it.
    let mut radio = radio_playing("groove");
    radio.autoplay_streaming = true;
    assert!(radio.current_is_radio_stream());
    assert!(!radio.streaming_active(), "off while a live station plays");
}

#[test]
fn settings_cannot_enable_autoplay_while_repeat_on() {
    let mut app = app_playing(3, 0);
    app.queue.repeat = crate::queue::Repeat::All;
    app.open_settings();
    {
        let s = app.settings.as_mut().unwrap();
        s.tab = crate::settings::SettingsTab::Ai;
        s.row = s
            .fields()
            .iter()
            .position(|f| *f == Field::AutoplayStreaming)
            .expect("an AutoplayStreaming field");
    }
    assert_eq!(
        app.settings.as_ref().unwrap().current_field(),
        Some(Field::AutoplayStreaming)
    );
    assert!(!app.settings.as_ref().unwrap().draft.autoplay_streaming);
    app.settings_change(1);
    assert!(
        !app.settings.as_ref().unwrap().draft.autoplay_streaming,
        "draft not flipped while repeat is on"
    );
    assert!(!app.status.text.is_empty(), "a message is shown");
}

#[test]
fn seek_keys_use_the_configured_interval() {
    let mut app = app_playing(1, 0);
    app.apply_config(&crate::config::Config {
        seek_seconds: Some(30.0),
        ..Default::default()
    });
    // Forward (→) jumps +interval, backward (←) jumps −interval.
    let cmds = app.update(Msg::Key(key(KeyCode::Right)));
    match cmds.as_slice() {
        [cmd] => match cmd.player_command() {
            Some(PlayerCmd::SeekRelative(s)) => assert!((*s - 30.0).abs() < 1e-9),
            _ => panic!("expected a single SeekRelative(+30) cmd"),
        },
        _ => panic!("expected a single SeekRelative(+30) cmd"),
    }
    app.admit_player_intents_for_test(&cmds);

    let cmds = app.update(Msg::Key(key(KeyCode::Left)));
    match cmds.as_slice() {
        [cmd] => match cmd.player_command() {
            Some(PlayerCmd::SeekRelative(s)) => assert!((*s + 30.0).abs() < 1e-9),
            _ => panic!("expected a single SeekRelative(-30) cmd"),
        },
        _ => panic!("expected a single SeekRelative(-30) cmd"),
    }
    app.admit_player_intents_for_test(&cmds);
}

// --- D: settings screen -------------------------------------------------
