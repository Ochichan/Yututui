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

#[test]
fn cache_emergency_preserves_position_once_and_requests_a_forced_ram_resume() {
    let mut app = app_playing(1, 0);
    app.playback.paused = true;
    app.playback.time_pos = Some(3_600.25);
    app.playback.duration = Some(7_200.0);
    let epoch = app.playback.position_epoch;
    let history_len = app.library.history.len();

    let cmds = app.update(PlayerMsg::CacheEmergency {
        position_secs: 3_600.25,
        paused: true,
        reason: crate::player::long_form_seek::CacheReason::DisableFailed,
    });

    let request = cmds
        .iter()
        .find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Restart { restore }) => {
                restore.iter().find_map(|command| match command {
                    PlayerCmd::LoadWithResume(request) => Some(request),
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("cache emergency restart must carry a correlated resume");
    assert!((request.position_secs - 3_600.25).abs() < f64::EPSILON);
    assert!(request.paused);
    assert!(request.force_ram_only);
    assert_eq!(
        request.source_context,
        crate::player::MediaSourceContext::OnDemand
    );
    assert_eq!(app.playback.time_pos, Some(3_600.25));
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert_eq!(app.library.history.len(), history_len);
    assert!(!cmds.iter().any(|cmd| matches!(cmd, Cmd::Persist(_))));
}

#[test]
fn cache_emergency_cannot_overwrite_a_newer_same_generation_seek_or_pause() {
    let mut app = app_playing(1, 0);
    // Model an intent which the runtime admitted and committed while the actor's cache
    // emergency was already queued ahead of that command.
    let original_epoch = app.playback.position_epoch;
    app.playback.time_pos = Some(3_630.0);
    app.playback.paused = true;
    app.bump_position_epoch(PositionEpochReason::Seek);
    let admitted_epoch = app.playback.position_epoch;
    assert_eq!(admitted_epoch, original_epoch + 1);

    let cmds = app.update(PlayerMsg::CacheEmergency {
        position_secs: 3_600.0,
        paused: false,
        reason: crate::player::long_form_seek::CacheReason::DisableFailed,
    });

    let request = cmds
        .iter()
        .find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Restart { restore }) => {
                restore.iter().find_map(|command| match command {
                    PlayerCmd::LoadWithResume(request) => Some(request),
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("cache emergency restart must retain the newest owner transport");
    assert!((request.position_secs - 3_630.0).abs() < f64::EPSILON);
    assert!(request.paused);
    assert!(request.force_ram_only);
    assert_eq!(app.playback.time_pos, Some(3_630.0));
    assert!(app.playback.paused);
    assert_eq!(app.playback.position_epoch, admitted_epoch + 1);
    assert_eq!(app.playback.position_epoch, original_epoch + 2);
}

#[test]
fn replacement_cache_emergency_replays_new_item_ram_only_without_old_position() {
    let mut app = app_playing(2, 1);
    app.playback.time_pos = None;
    app.playback.paused = false;

    let cmds = app.update(PlayerMsg::CacheReplacementEmergency {
        reason: crate::player::long_form_seek::CacheReason::DisableFailed,
    });

    let request = cmds
        .iter()
        .find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Restart { restore }) => {
                restore.iter().find_map(|command| match command {
                    PlayerCmd::LoadWithResume(request) => Some(request),
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("replacement cache emergency must replay the admitted destination");
    assert_eq!(current(&app), "id1");
    assert_eq!(request.position_secs, 0.0);
    assert!(!request.paused);
    assert!(request.force_ram_only);
    assert!(request.url.contains("id1"), "must replay the new item");
}

#[test]
fn stop_cache_emergency_retires_actor_without_replaying_queue_current() {
    let mut app = app_playing(1, 0);
    app.commit_playback_cleared();

    let cmds = app.update(PlayerMsg::CacheReplacementEmergency {
        reason: crate::player::long_form_seek::CacheReason::DisableFailed,
    });

    assert!(matches!(
        cmds.as_slice(),
        [Cmd::PlayerControl(PlayerControl::Restart { restore })] if restore.is_empty()
    ));
    assert_no_load(&cmds);
    assert_eq!(app.playback.time_pos, None);
    assert_eq!(app.prefetch.loaded_video_id, None);
}
