use super::*;

#[test]
fn stale_prefetch_retry_is_rejected_before_load_reaches_player() {
    let mut app = app_playing(2, 0);
    let video_id = app.queue.current().unwrap().video_id.clone();
    app.prefetch.last_load_prefetched = true;
    let cmds = app.prefetch_watch_retry_intent(
        video_id.clone(),
        format!("https://www.youtube.com/watch?v={video_id}"),
    );

    app.queue.toggle_shuffle();
    assert_rejected_before_send(&mut app, cmds);

    assert!(app.prefetch.last_load_prefetched);
    assert!(!app.prefetch.watch_retry_attempted.contains(&video_id));
}

#[test]
fn stale_radio_live_seek_is_rejected_before_resume_or_seek_reaches_player() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(200.0)));
    app.playback.paused = true;
    app.video.paused_audio = true;
    let epoch = app.playback.position_epoch;
    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));

    app.queue.toggle_shuffle();
    assert_rejected_before_send(&mut app, cmds);

    assert!(app.playback.paused);
    assert!(app.video.paused_audio);
    assert!(app.radio_resync_at.is_none());
    assert_eq!(app.playback.position_epoch, epoch);
}

#[test]
fn stale_recorder_plan_is_rejected_before_stream_record_reaches_player() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    let cmds = app.update(PlayerMsg::Metadata(
        serde_json::json!({ "icy-title": "Artist A - Track 1" }),
    ));

    app.recorder.temp_seq += 1;
    assert_rejected_before_send(&mut app, cmds);

    assert!(app.recorder.current.is_none());
    assert!(!app.recorder.saw_first_title);
    assert_eq!(app.recorder.temp_seq, 1);
}

#[test]
fn stale_media_stop_is_rejected_before_stop_reaches_player() {
    let mut app = app_playing(2, 0);
    let loaded = app.prefetch.loaded_video_id.clone();
    let epoch = app.playback.position_epoch;
    let cmds = app.update(Msg::Media(crate::media::MediaCommand::Stop));

    app.queue.toggle_shuffle();
    assert_rejected_before_send(&mut app, cmds);

    assert!(!app.playback.paused);
    assert_eq!(app.prefetch.loaded_video_id, loaded);
    assert_eq!(app.playback.position_epoch, epoch);
}

#[test]
fn stale_settings_preview_and_save_are_rejected_before_audio_reaches_player() {
    let mut preview_app = App::new(100);
    preview_app.open_settings();
    let preview = preview_app.settings_preview_speed(1);
    preview_app.playback.speed = 1.25;

    assert_rejected_before_send(&mut preview_app, preview);
    assert_eq!(preview_app.settings.as_deref().unwrap().draft.speed, 1.0);

    let mut save_app = App::new(100);
    save_app.open_settings();
    let save = save_app.close_settings();
    save_app.settings.as_deref_mut().unwrap().draft.download_dir = "newer-dir".to_owned();

    assert_rejected_before_send(&mut save_app, save);
    assert_eq!(save_app.mode, Mode::Settings);
    assert_eq!(
        save_app.settings.as_deref().unwrap().draft.download_dir,
        "newer-dir"
    );

    let mut reset_app = App::new(100);
    reset_app.open_settings();
    let reset = reset_app.settings_reset_all();
    reset_app
        .settings
        .as_deref_mut()
        .unwrap()
        .draft
        .download_dir = "keep-me".to_owned();

    assert_rejected_before_send(&mut reset_app, reset);
    assert_eq!(
        reset_app.settings.as_deref().unwrap().draft.download_dir,
        "keep-me"
    );
}

#[test]
fn stale_video_open_is_rejected_before_pause_reaches_player() {
    let mut app = app_playing(2, 0);
    let cmds = app.toggle_video_overlay_with_fake_spawn(true);

    app.config.video_layout = app.config.video_layout.toggled();
    assert_rejected_before_send(&mut app, cmds);

    assert!(app.video.proc.is_none());
    assert!(!app.playback.paused);
    assert!(!app.video.paused_audio);
}

#[test]
fn stale_video_finish_cannot_close_a_replacement_overlay_or_unpause_audio() {
    let mut app = app_playing(2, 0);
    app.video.proc = Some(fake_overlay_proc());
    app.playback.paused = true;
    app.video.paused_audio = true;
    let stale_finish = app.toggle_video_overlay();

    let respawn_effects = app.toggle_video_layout_with_fake_spawn(true);
    assert!(
        respawn_effects
            .iter()
            .all(|cmd| !matches!(cmd, Cmd::PlayerControl(_)))
    );
    let replacement_id = app.video.proc.as_ref().map(|child| child.id());

    assert_rejected_before_send(&mut app, stale_finish);

    assert_eq!(
        app.video.proc.as_ref().map(|child| child.id()),
        replacement_id
    );
    assert!(app.playback.paused);
    assert!(app.video.paused_audio);
    app.close_video();
}

#[test]
fn stale_track_plan_is_rejected_before_load_reaches_player() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));

    app.queue.toggle_shuffle();
    assert_rejected_before_send(&mut app, cmds);

    assert_eq!(current(&app), "id0");
}
