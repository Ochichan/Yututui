use super::*;

fn install_closed_player(engine: &mut DaemonEngine) {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    engine.player = Some(PlayerRuntime {
        handle: PlayerHandle::test_handle(tx),
        _guard: None,
    });
}

#[tokio::test]
async fn rejected_transport_commands_do_not_commit_optimistic_state() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    engine.playback.volume = 40;
    engine.playback.time_pos = Some(12.0);
    engine.playback.duration = Some(90.0);
    let epoch = engine.playback.position_epoch;
    install_closed_player(&mut engine);

    for command in [
        RemoteCommand::TogglePause,
        RemoteCommand::SetVolume { percent: 80 },
        RemoteCommand::SeekTo { ms: 45_000 },
    ] {
        let (response, shutdown, effects) = engine.handle_remote(command).await;
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some("mpv_unavailable"));
        assert!(!shutdown);
        assert!(effects.is_empty());
    }

    assert!(!engine.playback.paused);
    assert_eq!(engine.playback.volume, 40);
    assert_eq!(engine.playback.time_pos, Some(12.0));
    assert_eq!(engine.playback.position_epoch, epoch);
}

#[tokio::test]
async fn rejected_track_load_restores_queue_cursor_and_membership() {
    let mut engine = super::tests::engine_with_queue(&["current", "next"]);
    engine.loaded_video_id = Some("current".to_owned());
    install_closed_player(&mut engine);

    let response = engine.queue_play(1).await;
    assert!(!response.ok);
    assert_eq!(engine.queue.current().unwrap().video_id, "current");
    assert_eq!(engine.queue.len(), 2);

    let response = engine.queue_remove(0).await;
    assert!(!response.ok);
    assert_eq!(engine.queue.current().unwrap().video_id, "current");
    assert_eq!(engine.queue.len(), 2);
    assert_eq!(engine.loaded_video_id.as_deref(), Some("current"));
}

#[tokio::test]
async fn rejected_next_restores_queue_and_does_not_record_outgoing_side_effects() {
    let mut engine = super::tests::engine_with_queue(&["current", "next"]);
    engine.loaded_video_id = Some("current".to_owned());
    engine.playback.paused = false;
    engine.playback.time_pos = Some(5.0);
    engine.playback.duration = Some(100.0);
    install_closed_player(&mut engine);

    let before_queue = engine.queue.snapshot();
    let before_epoch = engine.playback.position_epoch;
    let before_history = engine.library.history.len();
    let before_sessions = engine.session_events.len();
    let before_play_count = engine.signals.play_count("current");
    let before_artist_weight = engine.signals.artist_weight("artist");

    let (response, shutdown, effects) = engine.handle_remote(RemoteCommand::Next).await;

    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("mpv_unavailable"));
    assert!(!shutdown);
    assert!(effects.is_empty());
    let after_queue = engine.queue.snapshot();
    assert_eq!(
        serde_json::to_value(&after_queue.songs).unwrap(),
        serde_json::to_value(&before_queue.songs).unwrap()
    );
    assert_eq!(after_queue.order, before_queue.order);
    assert_eq!(after_queue.cursor, before_queue.cursor);
    assert_eq!(after_queue.shuffle, before_queue.shuffle);
    assert_eq!(after_queue.repeat, before_queue.repeat);
    assert_eq!(engine.loaded_video_id.as_deref(), Some("current"));
    assert_eq!(engine.playback.position_epoch, before_epoch);
    assert_eq!(engine.library.history.len(), before_history);
    assert_eq!(engine.session_events.len(), before_sessions);
    assert_eq!(engine.signals.play_count("current"), before_play_count);
    assert_eq!(engine.signals.artist_weight("artist"), before_artist_weight);
}

#[test]
fn rejected_load_does_not_advance_epoch_history_or_loaded_track() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    let epoch = engine.playback.position_epoch;
    let history_len = engine.library.history.len();
    install_closed_player(&mut engine);

    assert!(engine.load_current_loaded().is_err());
    assert_eq!(engine.loaded_video_id, None);
    assert!(engine.playback.paused);
    assert_eq!(engine.playback.position_epoch, epoch);
    assert_eq!(engine.library.history.len(), history_len);
}

#[test]
fn rejected_live_settings_roll_back_config_and_playback_state() {
    let mut engine = super::tests::engine_with_queue(&["seed"]);
    install_closed_player(&mut engine);
    let speed = engine.playback.speed;
    let configured_speed = engine.config.speed;
    let normalize = engine.config.normalize;
    let preset = engine.config.eq_preset;
    let bands = engine.config.eq_bands;

    let (response, effects) = engine.set_setting(RemoteSettingChange::Speed { tenths: 15 });
    assert!(!response.ok);
    assert!(effects.is_empty());
    assert_eq!(engine.playback.speed, speed);
    assert_eq!(engine.config.speed, configured_speed);

    let (response, effects) = engine.set_setting(RemoteSettingChange::Normalize { value: true });
    assert!(!response.ok);
    assert!(effects.is_empty());
    assert_eq!(engine.config.normalize, normalize);

    let (response, effects) = engine.apply_gui_setting(crate::remote::proto::GuiSettingChange {
        group: "eq".to_owned(),
        field: "preset".to_owned(),
        value: serde_json::json!("bass_boost"),
    });
    assert!(!response.ok);
    assert!(effects.is_empty());
    assert_eq!(engine.config.eq_preset, preset);
    assert_eq!(engine.config.eq_bands, bands);
}
