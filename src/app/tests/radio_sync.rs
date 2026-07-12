use super::*;

#[test]
fn cache_time_updates_playback_and_coalesces_redraws() {
    let mut app = radio_playing("groove");
    app.dirty = false;
    app.update(PlayerMsg::CacheTime(Some(100.2)));
    assert_eq!(app.playback.cache_time, Some(100.2));
    assert!(app.playback.cache_time_at.is_some());
    assert!(app.dirty);

    app.dirty = false;
    app.update(PlayerMsg::CacheTime(Some(100.7)));
    assert!(!app.dirty, "same whole second must not redraw");
    app.update(PlayerMsg::CacheTime(Some(101.3)));
    assert!(app.dirty);

    app.dirty = false;
    app.update(PlayerMsg::CacheTime(None));
    assert_eq!(app.playback.cache_time, None);
    assert!(app.playback.cache_time_at.is_none());
    assert!(
        app.dirty,
        "Some→None must redraw so the glyph can't go stale"
    );
}

#[test]
fn radio_live_sync_verdict_follows_behind_distance() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(105.0)));
    assert_eq!(app.radio_behind_secs().map(|b| b as i64), Some(5));
    assert_eq!(app.radio_live_synced(), Some(true));

    app.update(PlayerMsg::CacheTime(Some(117.0)));
    assert_eq!(app.radio_behind_secs().map(|b| b as i64), Some(17));
    assert_eq!(app.radio_live_synced(), Some(true));

    app.update(PlayerMsg::CacheTime(Some(120.0)));
    assert_eq!(app.radio_behind_secs().map(|b| b as i64), Some(20));
    assert_eq!(app.radio_live_synced(), Some(true));

    app.update(PlayerMsg::CacheTime(Some(130.0)));
    assert_eq!(app.radio_live_synced(), Some(false));

    // A regular track never gets a verdict, even with cache reports flowing.
    let mut music = app_playing(1, 0);
    music.update(PlayerMsg::TimePos(10.0));
    music.update(PlayerMsg::CacheTime(Some(60.0)));
    assert_eq!(music.radio_live_synced(), None);
}

#[test]
fn stale_cache_time_degrades_synced_to_unknown_but_keeps_behind() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(117.0)));
    // An old at-edge report proves nothing while playing (mpv freezes the property
    // once the forward buffer saturates) → unknown.
    app.playback.cache_time_at = Some(Instant::now() - Duration::from_secs(30));
    assert_eq!(app.radio_live_synced(), None);
    // But an old report showing a big gap is still a valid lower bound → behind.
    app.playback.cache_time = Some(200.0);
    assert_eq!(app.radio_live_synced(), Some(false));
    // While paused the frozen report stays authoritative (behind only grows).
    app.playback.cache_time = Some(117.0);
    app.playback.paused = true;
    assert_eq!(app.radio_live_synced(), Some(true));
}

#[test]
fn radio_repeat_key_at_normal_live_buffer_does_not_seek_or_reconnect() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(117.0)));

    let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));

    assert!(
        cmds.is_empty(),
        "normal live buffer should need no player command"
    );
    assert!(app.radio_resync_at.is_none());
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(
        app.status.text.contains("live edge") || app.status.text.contains("실시간"),
        "status should report live, got {:?}",
        app.status.text
    );
}

#[test]
fn radio_repeat_key_resyncs_with_a_seek_to_the_live_edge() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(200.0)));
    app.playback.paused = true;
    app.video.paused_audio = true;
    let epoch_before = app.playback.position_epoch;

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));

    // Preparation is pure: the absolute resume + seek batch must be admitted together.
    assert!(app.playback.paused);
    assert!(app.video.paused_audio);
    assert!(app.radio_resync_at.is_none());
    assert_eq!(app.playback.position_epoch, epoch_before);
    assert!(
        cmds.iter()
            .flat_map(Cmd::player_commands)
            .any(|command| matches!(
                command,
                PlayerCmd::SetProperty { name, value }
                    if name == "pause" && value == &serde_json::Value::Bool(false)
            ))
    );
    let target = cmds
        .iter()
        .flat_map(Cmd::player_commands)
        .find_map(|command| match command {
            PlayerCmd::SeekAbsolute(seconds) => Some(*seconds),
            _ => None,
        });
    assert_eq!(target, Some(198.0));
    admit_player_transition(&mut app, &mut cmds);
    assert!(!app.playback.paused);
    assert!(!app.video.paused_audio);
    assert_eq!(app.playback.time_pos, Some(100.0));
    assert_eq!(app.playback.position_epoch, epoch_before.wrapping_add(1));
    // The queue's repeat mode is untouched and nothing is persisted.
    assert!(save_config(&cmds).is_none());
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(app.radio_resync_at.is_some());
}

#[test]
fn radio_repeat_key_reconnects_when_no_cache_info() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));

    assert_eq!(load_url(&cmds), Some("https://example.com/groove.mp3"));
    assert_eq!(app.playback.time_pos, Some(100.0));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.playback.time_pos, None);
    assert!(app.status.text.contains("Reconnected") || app.status.text.contains("다시 연결"));
    assert_eq!(app.queue.repeat, crate::queue::Repeat::Off);
}

#[test]
fn radio_resync_escalates_to_reconnect_when_seek_does_not_take() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(200.0)));

    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert_no_load(&cmds);
    admit_player_transition(&mut app, &mut cmds);

    // Seconds later the playhead hasn't moved (an unseekable live cache) — the retry
    // inside the window escalates to a stream reconnect.
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
    assert_eq!(load_url(&cmds), Some("https://example.com/groove.mp3"));
    admit_player_transition(&mut app, &mut cmds);
}

#[test]
fn rejected_radio_live_seek_keeps_state_and_does_not_escalate_retry() {
    for error in [
        crate::util::delivery::DeliveryError::Busy,
        crate::util::delivery::DeliveryError::Closed,
    ] {
        let mut app = radio_playing("groove");
        app.update(PlayerMsg::TimePos(100.0));
        app.update(PlayerMsg::CacheTime(Some(200.0)));
        app.playback.paused = true;
        app.video.paused_audio = true;
        let epoch = app.playback.position_epoch;
        let position_at = app.playback.time_pos_at;
        let loaded = app.prefetch.loaded_video_id.clone();

        let cmds = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(reject_player_transition(&mut app, cmds, error).is_empty());

        assert!(app.playback.paused);
        assert!(app.video.paused_audio);
        assert_eq!(app.playback.time_pos, Some(100.0));
        assert_eq!(app.playback.time_pos_at, position_at);
        assert_eq!(app.playback.position_epoch, epoch);
        assert!(app.radio_resync_at.is_none());
        assert_eq!(app.prefetch.loaded_video_id, loaded);

        let retry = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert_no_load(&retry);
        assert!(
            retry
                .iter()
                .flat_map(Cmd::player_commands)
                .any(|command| matches!(command, PlayerCmd::SeekAbsolute(198.0)))
        );
    }
}

#[test]
fn radio_shift_s_reports_sync_state_without_touching_shuffle() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::TimePos(100.0));
    app.update(PlayerMsg::CacheTime(Some(103.0)));

    let cmds = app.update(Msg::Key(key(KeyCode::Char('S'))));

    assert!(cmds.is_empty(), "the sync note is read-only");
    assert!(!app.queue.shuffle);
    assert_eq!(app.status.kind, StatusKind::Info);
    assert!(!app.status.text.is_empty());
}

#[test]
fn loading_a_new_track_clears_cache_time() {
    let mut app = radio_playing("groove");
    app.update(PlayerMsg::CacheTime(Some(50.0)));
    assert!(app.playback.cache_time.is_some());

    app.queue.set(songs(1), 0);
    let mut cmds = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut cmds);

    assert!(app.playback.cache_time.is_none());
    assert!(app.playback.cache_time_at.is_none());
    assert!(app.radio_resync_at.is_none());
}
