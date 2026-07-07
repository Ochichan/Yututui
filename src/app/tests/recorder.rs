use super::*;

#[test]
fn recorder_first_track_is_incomplete_and_dropped() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "Artist A - Track 1");
    let seg = app.recorder.current.as_ref().expect("segment open");
    assert!(
        seg.incomplete,
        "the first track after tuning in is incomplete"
    );
    assert!(app.recorder.saw_first_title);
    assert!(app.recorder.history.is_empty());

    // Second title: the incomplete first track is dropped; a complete second opens.
    feed_title(&mut app, "Artist B - Track 2");
    assert!(
        app.recorder.history.is_empty(),
        "incomplete first stays out of history"
    );
    let seg = app.recorder.current.as_ref().expect("second segment open");
    assert!(!seg.incomplete);
    assert_eq!(seg.raw, "Artist B - Track 2");
}

#[test]
fn recorder_keeps_a_long_enough_track_on_the_next_boundary() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One"); // incomplete first
    feed_title(&mut app, "B - Two"); // opens complete "Two"
    backdate_current(&mut app, 60); // pretend "Two" played a full minute
    feed_title(&mut app, "C - Three"); // finalize "Two" -> kept
    assert_eq!(app.recorder.history.len(), 1);
    let t = &app.recorder.history[0];
    assert_eq!(t.raw, "B - Two");
    assert_eq!(t.title.as_deref(), Some("Two"));
    assert_eq!(t.artist.as_deref(), Some("B"));
    assert!(matches!(t.state, crate::recorder::RecordingState::Recorded));
}

#[test]
fn recorder_drops_a_track_below_min_duration() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two"); // complete, but ~0s long
    feed_title(&mut app, "C - Three"); // finalize "Two": below min -> dropped
    assert!(app.recorder.history.is_empty());
}

#[test]
fn recorder_everything_mode_auto_saves_kept_tracks() {
    let mut app = recording_app(crate::recorder::RecordingMode::Everything);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    let cmds = feed_title(&mut app, "C - Three");
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Cmd::Recorder(crate::recorder::job::RecorderJob::Save { .. })
        )),
        "a save job is emitted for the kept track"
    );
    assert_eq!(app.recorder.history.len(), 1);
    assert!(matches!(
        app.recorder.history[0].state,
        crate::recorder::RecordingState::Saved
    ));
}

#[test]
fn recorder_pauses_through_ad_and_resumes_complete() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One"); // incomplete
    feed_title(&mut app, "B - Two"); // complete "Two"
    backdate_current(&mut app, 60);
    // Metadata goes junk (ad / station id) -> parsed None -> finalize "Two", open nothing.
    app.update(PlayerMsg::Metadata(serde_json::json!({ "icy-title": "" })));
    assert!(
        app.recorder.current.is_none(),
        "recording paused during the ad"
    );
    assert_eq!(app.recorder.history.len(), 1, "\"Two\" kept");
    // A later real title opens a *complete* segment (saw_first_title is already set).
    feed_title(&mut app, "D - Four");
    assert!(!app.recorder.current.as_ref().expect("resumed").incomplete);
}

#[test]
fn recorder_teardown_on_stop_drops_in_progress_and_clears_stream_record() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    assert!(app.recorder.current.is_some());
    let cmds = app.load_song(None); // stop / clear playback
    assert!(app.recorder.current.is_none());
    assert!(!app.recorder.saw_first_title);
    assert!(emits_stream_record_clear(&cmds));
}

#[test]
fn media_stop_clears_radio_recording_and_video_pause_latch() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "A - One");
    feed_title(&mut app, "B - Two");
    backdate_current(&mut app, 60);
    app.playback.paused = false;
    app.video.paused_audio = true;

    let cmds = app.update(Msg::Media(crate::media::MediaCommand::Stop));

    assert!(app.playback.paused);
    assert!(app.prefetch.loaded_video_id.is_none());
    assert!(!app.video.paused_audio);
    assert!(app.recorder.current.is_none());
    assert!(!app.recorder.saw_first_title);
    let clear_pos = cmds
        .iter()
        .position(|cmd| emits_stream_record_clear(std::slice::from_ref(cmd)))
        .expect("stream-record cleared");
    let stop_pos = cmds
        .iter()
        .position(|cmd| matches!(cmd, Cmd::Player(crate::player::PlayerCmd::Stop)))
        .expect("mpv stop emitted");
    assert!(
        clear_pos < stop_pos,
        "stream-record must clear before mpv stops"
    );
}

#[test]
fn recorder_off_mode_records_nothing() {
    let mut app = recording_app(crate::recorder::RecordingMode::Nothing);
    let cmds = feed_title(&mut app, "A - One");
    assert!(app.recorder.current.is_none());
    assert!(
        !cmds.iter().any(|c| matches!(
            c,
            Cmd::Player(crate::player::PlayerCmd::SetProperty { name, .. }) if name == "stream-record"
        )),
        "no stream-record command when recording is off"
    );
}

#[test]
fn radio_recording_item_hidden_outside_radio_mode() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = false;
    app.open_settings();
    app.settings.as_mut().unwrap().tab = crate::settings::SettingsTab::Playback;
    assert!(
        !app.settings
            .as_ref()
            .unwrap()
            .fields()
            .contains(&crate::settings::Field::RadioRecording)
    );
}

#[test]
fn radio_recording_popup_opens_edits_and_persists() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = true;
    app.open_settings();
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = crate::settings::SettingsTab::Playback;
        st.row = st
            .fields()
            .iter()
            .position(|f| *f == crate::settings::Field::RadioRecording)
            .expect("radio item present in radio mode");
    }
    let _ = app.settings_activate();
    assert!(
        app.overlays.recording_settings.is_some(),
        "the button opens the popup"
    );

    // Row 0 = mode; Right cycles Off -> Decide.
    app.recording_settings_key(key(KeyCode::Right));
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_mode,
        crate::recorder::RecordingMode::Decide
    );

    // Last row = "Browse recordings…"; Enter opens the recordings browser.
    for _ in 0..(crate::app::RECORDING_POPUP_ROWS - 1) {
        app.recording_settings_key(key(KeyCode::Down));
    }
    app.recording_settings_key(key(KeyCode::Enter));
    assert!(
        app.overlays.recordings_browser.is_some(),
        "browse row opens the browser"
    );

    // Closing Settings commits the draft to config.
    app.overlays.recordings_browser = None;
    app.overlays.recording_settings = None;
    let _ = app.close_settings();
    assert_eq!(
        app.config.recording.mode,
        crate::recorder::RecordingMode::Decide
    );
}

#[test]
fn recording_slider_drag_maps_column_to_value() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = true;
    app.open_settings();
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = crate::settings::SettingsTab::Playback;
        st.row = st
            .fields()
            .iter()
            .position(|f| *f == crate::settings::Field::RadioRecording)
            .expect("radio item present in radio mode");
    }
    let _ = app.settings_activate();
    assert!(app.overlays.recording_settings.is_some());

    // An 11-cell track; the mapping uses width-1 divisions so both ends are reachable.
    let track = Rect {
        x: 20,
        y: 5,
        width: 11,
        height: 1,
    };

    // Keep-recent (row 4, 1..=50 step 1): leftmost cell → min, rightmost cell → max.
    app.recording_slider_set(4, track.x, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_past_tracks,
        1
    );
    app.recording_slider_set(4, track.x + 10, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_past_tracks,
        50
    );

    // Min duration (row 1, 5..=600 step 5): a column past the right end clamps to the max,
    // and one before the left end clamps to the min — proof the drag works both directions.
    app.recording_slider_set(1, track.right() + 5, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_min_seconds,
        600
    );
    app.recording_slider_set(1, 0, track);
    assert_eq!(
        app.settings.as_ref().unwrap().draft.recording_min_seconds,
        5
    );
}

#[test]
fn recorder_saved_emits_desktop_notify_when_enabled() {
    let _guard = crate::i18n::lock_for_test();
    use crate::recorder::job::RecorderEvent;
    let mut app = App::new(100);

    // notify on → a saved recording returns a DesktopNotify command (the filename is the body),
    // in addition to the in-app toast.
    app.config.recording.notify = true;
    let cmds = app.on_recorder_event(RecorderEvent::Saved {
        id: 1,
        final_path: std::path::PathBuf::from("/tmp/Artist - Track.mp3"),
    });
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Cmd::DesktopNotify { body, .. } if body == "Artist - Track.mp3"
        )),
        "a saved recording fires a desktop notification with the filename as the body"
    );

    // notify off → no desktop notification.
    app.config.recording.notify = false;
    let cmds = app.on_recorder_event(RecorderEvent::Saved {
        id: 2,
        final_path: std::path::PathBuf::from("/tmp/Other.mp3"),
    });
    assert!(
        !cmds.iter().any(|c| matches!(c, Cmd::DesktopNotify { .. })),
        "with notifications off, no desktop notification is fired"
    );
}

#[test]
fn recording_settings_popup_closed_by_navigation() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);
    app.radio_dedicated_mode = true;
    app.open_settings();
    {
        let st = app.settings.as_mut().unwrap();
        st.tab = crate::settings::SettingsTab::Playback;
        st.row = st
            .fields()
            .iter()
            .position(|f| *f == crate::settings::Field::RadioRecording)
            .expect("radio item present in radio mode");
    }
    let _ = app.settings_activate();
    assert!(
        app.overlays.recording_settings.is_some(),
        "the button opens the popup"
    );

    // Navigating Home must drop the top-level overlay so it can't strand over the Player
    // (regression: it used to keep painting on top, unreachable).
    let _ = app.go_home();
    assert!(
        app.overlays.recording_settings.is_none(),
        "go_home clears the recording-settings popup"
    );
    assert_eq!(app.mode, Mode::Player);
}

#[test]
fn radio_stream_metadata_updates_dj_gem_context() {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station("groove")], 0);
    app.load_song(app.queue.current().cloned());

    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));

    assert_eq!(
        app.playback
            .stream_now_playing
            .as_ref()
            .map(StreamNowPlaying::label)
            .as_deref(),
        Some("Track — Artist")
    );
    let ctx = app.build_ai_context();
    assert_eq!(
        ctx.current_radio_station.as_deref(),
        Some("Station groove — KR / MP3")
    );
    assert_eq!(
        ctx.current_radio_now_playing.as_deref(),
        Some("Track — Artist")
    );

    app.dirty = false;
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));
    assert!(!app.dirty, "unchanged stream metadata should not redraw");
}

#[test]
fn stream_metadata_is_ignored_for_regular_tracks() {
    let mut app = app_playing(1, 0);

    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));

    assert!(app.playback.stream_now_playing.is_none());
    let ctx = app.build_ai_context();
    assert!(ctx.current_radio_station.is_none());
    assert!(ctx.current_radio_now_playing.is_none());
}

#[test]
fn loading_a_new_track_clears_stale_stream_metadata() {
    let mut app = App::new(100);
    app.queue.set(vec![radio_station("groove")], 0);
    app.load_song(app.queue.current().cloned());
    app.update(PlayerMsg::Metadata(serde_json::json!({
        "icy-title": "Artist - Track"
    })));
    assert!(app.playback.stream_now_playing.is_some());

    app.queue.set(songs(1), 0);
    app.load_song(app.queue.current().cloned());

    assert!(app.playback.stream_now_playing.is_none());
}
