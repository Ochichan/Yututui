use super::*;

#[test]
fn eof_auto_advances_to_next_track() {
    let mut app = app_playing(3, 0);
    let mut cmds = app.update(PlayerMsg::Eof);
    admit_player_transition(&mut app, &mut cmds);
    assert_loads_video(&cmds, "id1");
    assert_eq!(current(&app), "id1");
}

#[test]
fn eof_at_end_with_repeat_off_stops() {
    let mut app = app_playing(2, 1); // already on the last track
    let mut cmds = app.update(PlayerMsg::Eof);
    admit_player_transition(&mut app, &mut cmds);
    // Playback closes (no load/advance), though the finished track is still recorded and its
    // queue metadata remains selected for the idle projection.
    assert_no_load(&cmds);
    assert!(cmds.iter().any(|cmd| matches!(
        cmd,
        Cmd::PlayerControl(PlayerControl::Intent(intent))
            if intent.commands.iter().any(|command| matches!(command, PlayerCmd::Stop))
    )));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Signals)))
    );
    assert_eq!(current(&app), "id1");
}

#[test]
fn eof_with_repeat_one_replays_same_track() {
    let mut app = app_playing(3, 0);
    app.queue.repeat = crate::queue::Repeat::One;
    let mut cmds = app.update(PlayerMsg::Eof);
    admit_player_transition(&mut app, &mut cmds);
    assert_loads_video(&cmds, "id0");
    assert_eq!(current(&app), "id0");
}

#[test]
fn player_error_auto_skips_to_next_track() {
    let mut app = app_playing(3, 0);
    let mut cmds = app.update(PlayerMsg::Error("boom".to_owned()));
    admit_player_transition(&mut app, &mut cmds);
    // The unplayable track is skipped: cursor + title move to the next track.
    assert_loads_video(&cmds, "id1");
    assert_eq!(current(&app), "id1");
    assert!(app.status.text.contains("skipped") || app.status.text.contains("unavailable"));
}

#[test]
fn prefetched_stream_error_retries_same_track_via_watch_url_once() {
    let mut app = app_playing(3, 0);
    app.prefetch.resolved.insert(
        "id0".to_owned(),
        "https://cdn.example/stale-id0.m4a".to_owned(),
    );
    app.prefetch.last_load_prefetched = true;

    let mut cmds = app.update(PlayerMsg::Error(
        "mpv could not play this track (HTTP error 403 Forbidden)".to_owned(),
    ));
    admit_player_transition(&mut app, &mut cmds);
    assert_loads_video(&cmds, "id0");
    assert_eq!(current(&app), "id0", "first failure retries the same track");
    assert!(!app.prefetch.resolved.contains_fresh("id0"));
    assert!(app.prefetch.watch_retry_attempted.contains("id0"));
    assert!(!app.prefetch.last_load_prefetched);
    assert_eq!(
        app.consecutive_play_errors, 0,
        "prefetched URL retry is not a playback strike"
    );

    let mut cmds = app.update(PlayerMsg::Error(
        "mpv could not play this track (HTTP error 403 Forbidden)".to_owned(),
    ));
    admit_player_transition(&mut app, &mut cmds);
    assert_loads_video(&cmds, "id1");
    assert_eq!(
        current(&app),
        "id1",
        "second failure uses the existing skip path"
    );
    assert_eq!(app.consecutive_play_errors, 1);
}

#[test]
fn repeated_prefetched_stream_failures_pause_prefetch_temporarily() {
    let mut app = app_playing(4, 0);
    app.prefetch.resolved.insert(
        "id0".to_owned(),
        "https://cdn.example/stale-id0.m4a".to_owned(),
    );
    app.prefetch.last_load_prefetched = true;

    let mut cmds = app.update(PlayerMsg::Error(
        "mpv could not play this track (HTTP error 403 Forbidden)".to_owned(),
    ));
    assert_loads_video(&cmds, "id0");
    assert!(app.prefetch.disabled_until.is_none());
    admit_player_transition(&mut app, &mut cmds);

    app.queue.next(false);
    app.prefetch.loaded_video_id = Some("id1".to_owned());
    app.prefetch.resolved.insert(
        "id1".to_owned(),
        "https://cdn.example/stale-id1.m4a".to_owned(),
    );
    app.prefetch.last_load_prefetched = true;

    let mut cmds = app.update(PlayerMsg::Error(
        "mpv could not play this track (HTTP error 403 Forbidden)".to_owned(),
    ));
    assert_loads_video(&cmds, "id1");
    admit_player_transition(&mut app, &mut cmds);
    assert!(
        app.prefetch.disabled_until.is_some(),
        "second direct URL failure pauses prefetch"
    );
    assert_eq!(
        app.prefetch.resolved.len(),
        0,
        "cooldown clears direct URLs"
    );

    let mut cmds = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        resolve_cmd_id(&cmds),
        None,
        "new prefetch requests are blocked during cooldown"
    );
    app.update(StreamingMsg::Resolved {
        video_id: "id2".to_owned(),
        stream_url: "https://cdn.example/late-id2.m4a".to_owned(),
        self_heal: false,
    });
    assert!(
        !app.prefetch.resolved.contains_fresh("id2"),
        "late resolver results are not cached during cooldown"
    );

    app.prefetch.disabled_until = Some(Instant::now() - Duration::from_secs(1));
    let mut cmds = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        resolve_cmd_id(&cmds),
        Some("id2"),
        "prefetch resumes after the cooldown expires"
    );
}

#[test]
fn rejected_prefetched_watch_retry_does_not_consume_retry_or_cooldown() {
    for error in [
        crate::util::delivery::DeliveryError::Busy,
        crate::util::delivery::DeliveryError::Closed,
    ] {
        let mut app = app_playing(3, 0);
        app.prefetch.resolved.insert(
            "id0".to_owned(),
            "https://cdn.example/stale-id0.m4a".to_owned(),
        );
        app.prefetch.last_load_prefetched = true;
        app.playback.time_pos = Some(42.0);
        let epoch = app.playback.position_epoch;
        let loaded = app.prefetch.loaded_video_id.clone();

        let cmds = app.update(PlayerMsg::Error(
            "mpv could not play this track (HTTP error 403 Forbidden)".to_owned(),
        ));
        assert!(resume_load(&cmds).is_some());
        assert!(reject_player_transition(&mut app, cmds, error).is_empty());

        assert!(app.prefetch.resolved.contains_fresh("id0"));
        assert!(!app.prefetch.watch_retry_attempted.contains("id0"));
        assert!(app.prefetch.last_load_prefetched);
        assert!(app.prefetch.recent_failures.is_empty());
        assert!(app.prefetch.disabled_until.is_none());
        assert_eq!(app.prefetch.loaded_video_id, loaded);
        assert_eq!(app.playback.time_pos, Some(42.0));
        assert_eq!(app.playback.position_epoch, epoch);

        let mut retry = app.update(PlayerMsg::Error(
            "mpv could not play this track (HTTP error 403 Forbidden)".to_owned(),
        ));
        assert!(resume_load(&retry).is_some());
        admit_player_transition(&mut app, &mut retry);
        assert!(app.prefetch.watch_retry_attempted.contains("id0"));
        assert!(!app.prefetch.last_load_prefetched);
        assert_eq!(app.playback.time_pos, Some(42.0));
        assert_eq!(app.playback.position_epoch, epoch.wrapping_add(1));
    }
}

#[test]
fn player_error_stops_after_repeated_failures() {
    let mut app = app_playing(6, 0);
    // First MAX failures auto-skip...
    let expected_skips = ["id1", "id2", "id3"];
    assert_eq!(
        MAX_CONSECUTIVE_PLAY_ERRORS as usize,
        expected_skips.len(),
        "update expected skip sequence when the breaker budget changes"
    );
    for expected in expected_skips {
        let mut cmds = app.update(PlayerMsg::Error("boom".to_owned()));
        admit_player_transition(&mut app, &mut cmds);
        assert_loads_video(&cmds, expected);
    }
    // ...the next one gives up instead of skip-storming the whole queue.
    let cmds = app.update(PlayerMsg::Error("boom".to_owned()));
    assert_no_load(&cmds);
    assert!(app.status.text.contains("stopped") || app.status.text.contains("failed"));
}

#[test]
fn successful_playback_resets_the_error_streak() {
    let mut app = app_playing(5, 0);
    let mut cmds = app.update(PlayerMsg::Error("boom".to_owned())); // skip to id1 (streak = 1)
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(current(&app), "id1");
    app.update(PlayerMsg::TimePos(3.0)); // id1 actually plays → streak cleared
    // A later failure starts a fresh streak, so it skips again rather than giving up.
    let mut cmds = app.update(PlayerMsg::Error("boom".to_owned()));
    admit_player_transition(&mut app, &mut cmds);
    assert_loads_video(&cmds, "id2");
    assert_eq!(current(&app), "id2");
}

/// The mpv `file_error` signature of a failed ytdl_hook resolution (stale yt-dlp),
/// exactly as `player::ipc` wraps it.

#[test]
fn extraction_error_triggers_ytdlp_self_heal_instead_of_skipping() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    assert_eq!(
        heal_cmd_id(&cmds),
        Some("id0"),
        "runs a yt-dlp update check"
    );
    assert_no_load(&cmds);
    assert_eq!(current(&app), "id0", "cursor stays on the failed track");
    assert!(app.status.text.contains("yt-dlp"));
}

#[test]
fn heal_success_resolves_and_reloads_the_same_track() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    // A new binary landed → the track re-resolves through the resolver (the session
    // mpv keeps its stale spawn-time ytdl_hook, so a watch-URL reload wouldn't help).
    let cmds = app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: true,
    });
    assert_eq!(resolve_cmd_id(&cmds), Some("id0"));
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::ResolveForSelfHeal { video_id, .. } if video_id == "id0"
        )),
        "the post-update request must force a new resolver invocation"
    );
    assert_no_load(&cmds);
    // A result from an ordinary prefetch that started before the update is not allowed
    // to consume the self-heal latch, even though its semantic key is identical.
    let stale_cmds = app.update(StreamingMsg::Resolved {
        video_id: "id0".to_owned(),
        stream_url: "https://cdn.example/pre-update-id0".to_owned(),
        self_heal: false,
    });
    assert_no_load(&stale_cmds);
    assert_eq!(app.heal.pending_video_id.as_deref(), Some("id0"));
    // The fresh binary's direct CDN URL arrives → the SAME track reloads from it.
    let epoch = app.playback.position_epoch;
    let mut cmds = app.update(StreamingMsg::Resolved {
        video_id: "id0".to_owned(),
        stream_url: "https://cdn.example/id0".to_owned(),
        self_heal: true,
    });
    assert_eq!(load_url(&cmds), Some("https://cdn.example/id0"));
    assert_eq!(app.heal.pending_video_id.as_deref(), Some("id0"));
    assert_eq!(current(&app), "id0");
    assert_eq!(app.playback.position_epoch, epoch);
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.heal.pending_video_id, None);
    assert_eq!(app.playback.position_epoch, epoch.wrapping_add(1));
}

#[test]
fn rejected_heal_reload_preserves_pending_state_and_can_retry() {
    for error in [
        crate::util::delivery::DeliveryError::Busy,
        crate::util::delivery::DeliveryError::Closed,
    ] {
        let mut app = app_playing(3, 0);
        app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
        app.update(Msg::YtdlpHealResult {
            video_id: "id0".to_owned(),
            updated: true,
        });
        let epoch = app.playback.position_epoch;
        let loaded = app.prefetch.loaded_video_id.clone();
        let history_len = app.library.history.len();
        let session_plays = app.session.plays;
        let queue_rev = app.queue.rev();

        let cmds = app.update(StreamingMsg::Resolved {
            video_id: "id0".to_owned(),
            stream_url: "https://cdn.example/id0".to_owned(),
            self_heal: true,
        });
        assert_eq!(load_url(&cmds), Some("https://cdn.example/id0"));
        assert!(reject_player_transition(&mut app, cmds, error).is_empty());

        assert_eq!(app.heal.pending_video_id.as_deref(), Some("id0"));
        assert!(app.prefetch.resolved.contains_fresh("id0"));
        assert_eq!(app.prefetch.loaded_video_id, loaded);
        assert_eq!(app.playback.position_epoch, epoch);
        assert_eq!(app.library.history.len(), history_len);
        assert_eq!(app.session.plays, session_plays);
        assert_eq!(app.queue.rev(), queue_rev);
        assert_eq!(current(&app), "id0");

        let mut retry = app.update(StreamingMsg::Resolved {
            video_id: "id0".to_owned(),
            stream_url: "https://cdn.example/id0".to_owned(),
            self_heal: true,
        });
        admit_player_transition(&mut app, &mut retry);
        assert_eq!(app.heal.pending_video_id, None);
        assert_eq!(app.playback.position_epoch, epoch.wrapping_add(1));
    }
}

#[test]
fn heal_without_update_falls_back_to_skip() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    let mut cmds = app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: false,
    });
    admit_player_transition(&mut app, &mut cmds);
    assert_loads_video(&cmds, "id1");
    assert_eq!(current(&app), "id1");
    assert!(
        app.status.text.contains("yt-dlp"),
        "message names the cause"
    );
}

#[test]
fn heal_resolve_failure_falls_back_to_skip() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: true,
    });
    // The updated binary STILL can't resolve it (region lock, deleted video…).
    let mut cmds = app.update(Msg::ResolveFailed {
        video_id: "id0".to_owned(),
    });
    admit_player_transition(&mut app, &mut cmds);
    assert_loads_video(&cmds, "id1");
    assert_eq!(current(&app), "id1");
}

#[test]
fn heal_runs_once_per_track_then_plain_skip() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    let mut cmds = app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: false,
    });
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(current(&app), "id1");
    // Back on the same track later: no second heal (and the 30-min cooldown also
    // bars other tracks from re-checking) — the plain skip path runs instead.
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char(','))));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(current(&app), "id0");
    let mut cmds = app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    admit_player_transition(&mut app, &mut cmds);
    assert!(
        heal_cmd_id(&cmds).is_none(),
        "one heal per track per session"
    );
    assert_loads_video(&cmds, "id1");
}

#[test]
fn stale_heal_result_is_ignored_after_user_moved_on() {
    let mut app = app_playing(3, 0);
    app.update(PlayerMsg::Error(EXTRACTION_ERR.to_owned()));
    // The user skips manually while the update check is still running.
    let mut cmds = app.update(Msg::Key(key(KeyCode::Char('.'))));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(current(&app), "id1");
    let cmds = app.update(Msg::YtdlpHealResult {
        video_id: "id0".to_owned(),
        updated: true,
    });
    assert!(cmds.is_empty(), "stale heal result is dropped");
    assert_eq!(current(&app), "id1");
}

#[test]
fn non_extraction_error_never_triggers_a_heal() {
    let mut app = app_playing(3, 0);
    let cmds = app.update(PlayerMsg::Error(
        "mpv could not play this track (HTTP error 403 Forbidden)".to_owned(),
    ));
    assert!(
        heal_cmd_id(&cmds).is_none(),
        "network errors skip as before"
    );
    assert_loads_video(&cmds, "id1");
}
