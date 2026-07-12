use super::*;

#[test]
fn transport_recovery_reloads_current_without_duplicate_history_or_signals() {
    let mut app = app_playing(2, 0);
    app.playback.paused = true;
    app.playback.time_pos = Some(42.0);
    app.playback.time_pos_at = Some(Instant::now());
    app.playback.duration = Some(180.0);
    app.playback.cache_time = Some(50.0);
    app.playback.cache_time_at = Some(Instant::now());
    app.playback.audio_codec = Some("aac".to_owned());
    app.playback.file_format = Some("mp4".to_owned());

    let history_len = app.library.history.len();
    let session_plays = app.session.plays;
    let signal_play_count = app.signals.play_count("id0");
    let artist_weight = app.signals.artist_weight("a");
    let epoch = app.playback.position_epoch;

    let cmds = app.update(PlayerMsg::TransportClosed(
        "read failed: access_token=secret1".to_owned(),
    ));

    let restore = cmds
        .iter()
        .find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Restart { restore }) => Some(restore.as_slice()),
            _ => None,
        })
        .expect("restart request");
    assert!(matches!(restore.first(), Some(PlayerCmd::Load(_))));
    assert!(matches!(restore.last(), Some(PlayerCmd::CyclePause)));
    assert_loads_video(&cmds, "id0");
    assert!(
        !cmds.iter().any(|cmd| matches!(cmd, Cmd::Persist(_))),
        "transport recovery must not persist duplicate play/signal effects"
    );

    assert_eq!(current(&app), "id0");
    assert!(app.playback.paused);
    assert_eq!(app.playback.time_pos, None);
    assert_eq!(app.playback.duration, None);
    assert_eq!(app.playback.cache_time, None);
    assert_eq!(app.playback.audio_codec, None);
    assert_eq!(app.playback.file_format, None);
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert_eq!(app.library.history.len(), history_len);
    assert_eq!(app.session.plays, session_plays);
    assert_eq!(app.signals.play_count("id0"), signal_play_count);
    assert_eq!(app.signals.artist_weight("a"), artist_weight);
    assert!(!app.status.text.contains("secret1"));
}

#[test]
fn transport_recovery_does_not_pause_a_previously_playing_track() {
    let mut app = app_playing(1, 0);
    app.playback.paused = false;

    let cmds = app.update(PlayerMsg::TransportClosed("unexpected EOF".to_owned()));

    assert_loads_video(&cmds, "id0");
    assert!(!app.playback.paused);
    assert!(
        !cmds
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|command| matches!(command, PlayerCmd::CyclePause))
    );
}

#[test]
fn transport_recovery_without_a_current_track_restarts_with_an_empty_restore_batch() {
    let mut app = App::new(50);

    let cmds = app.update(PlayerMsg::TransportClosed("unexpected EOF".to_owned()));

    assert!(matches!(
        cmds.as_slice(),
        [Cmd::PlayerControl(PlayerControl::Restart { restore })] if restore.is_empty()
    ));
    assert_no_load(&cmds);
}
