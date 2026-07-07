use super::*;

#[test]
fn next_and_prev_keys_move_through_queue() {
    let mut app = app_playing(3, 0);
    app.update(Msg::Key(key(KeyCode::Char('.'))));
    assert_eq!(current(&app), "id1");
    app.update(Msg::Key(key(KeyCode::Char(','))));
    assert_eq!(current(&app), "id0");
}

#[test]
fn delete_on_player_removes_current_and_loads_next() {
    let mut app = app_playing(3, 0);

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));

    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "id1");
    assert!(
        app.queue.ordered().iter().all(|s| s.video_id != "id0"),
        "deleted current track should be removed from the queue"
    );
    assert_loads_video(&cmds, "id1");
}

#[test]
fn r_cycles_repeat_and_persists() {
    let mut app = app_playing(3, 0);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert_eq!(app.queue.repeat, crate::queue::Repeat::All);
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert_eq!(saved.repeat, crate::queue::Repeat::All);
}

#[test]
fn s_enters_search_and_shift_s_toggles_shuffle() {
    let mut app = app_playing(3, 0);
    assert!(!app.queue.shuffle);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    assert_eq!(app.mode, Mode::Search);
    assert!(!app.queue.shuffle);

    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('S'))));
    assert!(app.queue.shuffle);
    let saved = save_config(&cmds).expect("a SaveConfig cmd");
    assert_eq!(saved.shuffle, Some(true));
    // Shuffle keeps the current track current.
    assert_eq!(current(&app), "id0");
}

// --- B+C: EQ / normalize / speed / autoplay -----------------------------

#[test]
fn e_cycles_eq_preset_and_emits_filter() {
    let mut app = app_playing(3, 0);
    assert_eq!(app.audio.preset, EqPreset::Flat);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('e'))));
    assert_eq!(app.audio.preset, EqPreset::BassBoost);
    assert!(
        af(&cmds)
            .expect("a SetAudioFilter cmd")
            .contains("equalizer")
    );
    // Cycle the rest of the way back to Flat → the chain is cleared (empty string).
    let mut last = Vec::new();
    for _ in 0..(EqPreset::CYCLE.len() - 1) {
        last = app.update(Msg::Key(key(KeyCode::Char('e'))));
    }
    assert_eq!(app.audio.preset, EqPreset::Flat);
    assert_eq!(af(&last), Some(""));
}

#[test]
fn shift_n_toggles_normalization() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('N'))));
    assert!(app.audio.normalize);
    assert!(
        af(&cmds)
            .expect("a SetAudioFilter cmd")
            .contains("dynaudnorm")
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char('N'))));
    assert!(!app.audio.normalize);
    assert_eq!(af(&cmds), Some(""));
}

#[test]
fn speed_up_and_down_clamp_and_emit() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char(']'))));
    assert!((app.playback.speed - 1.1).abs() < 1e-9);
    assert!(cmds.iter().any(|c| matches!(c,
        Cmd::Player(PlayerCmd::SetProperty { name, .. }) if name == "speed")));
    // Floor at SPEED_MIN no matter how many times we press down.
    for _ in 0..50 {
        app.update(Msg::Key(key(KeyCode::Char('['))));
    }
    assert!((app.playback.speed - SPEED_MIN).abs() < 1e-9);
}

#[test]
fn ctrl_r_toggles_autoplay_streaming() {
    let mut app = app_playing(3, 0);
    assert!(!app.autoplay_streaming);
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(app.autoplay_streaming);
    assert_eq!(
        save_config(&cmds)
            .expect("a SaveConfig cmd")
            .autoplay_streaming,
        Some(true)
    );
    // Plain `r` while autoplay is on is now refused (they're mutually exclusive in music mode):
    // repeat stays Off, a status message is shown, and autoplay is untouched.
    app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert!(app.autoplay_streaming);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
    assert!(!app.status.text.is_empty());
    let cmds = app.update(Msg::Key(ctrl(KeyCode::Char('r'))));
    assert!(!app.autoplay_streaming);
    assert_eq!(
        save_config(&cmds)
            .expect("a SaveConfig cmd")
            .autoplay_streaming,
        Some(false)
    );
}
