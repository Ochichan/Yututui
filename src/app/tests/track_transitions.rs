use super::*;

fn player_batch(cmds: &[Cmd]) -> Vec<&PlayerCmd> {
    cmds.iter().flat_map(Cmd::player_commands).collect()
}

fn take_player_intent(cmds: Vec<Cmd>) -> PlayerIntent {
    cmds.into_iter()
        .find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(*intent),
            _ => None,
        })
        .expect("track transition intent")
}

#[test]
fn busy_and_closed_next_leave_transition_state_unchanged() {
    use crate::util::delivery::DeliveryError;

    for (error, reason) in [
        (DeliveryError::Busy, "player_busy"),
        (DeliveryError::Closed, "player_unavailable"),
    ] {
        let mut app = app_playing(3, 0);
        app.art.video_id = Some("art-id0".to_owned());
        app.lyrics.track = Some(TrackLyrics {
            video_id: "id0".into(),
            lines: Vec::new().into(),
        });
        let cursor = app.queue.cursor_pos();
        let history: Vec<_> = app
            .library
            .history
            .iter()
            .map(|song| song.video_id.clone())
            .collect();
        let signal_count = app.signals.play_count("id0");
        let session_events = app.streaming.session_events.len();
        let epoch = app.playback.position_epoch;
        let loaded = app.prefetch.loaded_video_id.clone();
        let session_plays = app.session.plays;
        let art = app.art.video_id.clone();
        let lyrics = app
            .lyrics
            .track
            .as_ref()
            .map(|track| track.video_id.clone());
        let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
        let cmds = app.update(Msg::Remote(
            crate::remote::proto::RemoteCommand::Next,
            reply.into(),
        ));

        let effects = crate::runtime::player_delivery::settle_player_intent(
            &mut app,
            take_player_intent(cmds),
            Err(error),
        );

        assert!(effects.is_empty());
        assert_eq!(app.queue.cursor_pos(), cursor);
        assert_eq!(current(&app), "id0");
        assert_eq!(
            app.library
                .history
                .iter()
                .map(|song| song.video_id.clone())
                .collect::<Vec<_>>(),
            history
        );
        assert_eq!(app.signals.play_count("id0"), signal_count);
        assert_eq!(app.streaming.session_events.len(), session_events);
        assert_eq!(app.playback.position_epoch, epoch);
        assert_eq!(app.prefetch.loaded_video_id, loaded);
        assert_eq!(app.session.plays, session_plays);
        assert_eq!(app.art.video_id, art);
        assert_eq!(
            app.lyrics
                .track
                .as_ref()
                .map(|track| track.video_id.clone()),
            lyrics
        );
        let response = reply_rx.try_recv().expect("correlated rejection response");
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some(reason));
    }
}

#[test]
fn next_preparation_leaves_all_transition_state_unchanged() {
    let mut app = app_playing(3, 0);
    app.art.video_id = Some("art-id0".to_owned());
    app.lyrics.track = Some(TrackLyrics {
        video_id: "id0".into(),
        lines: Vec::new().into(),
    });
    let cursor = app.queue.cursor_pos();
    let history: Vec<_> = app
        .library
        .history
        .iter()
        .map(|song| song.video_id.clone())
        .collect();
    let signal_count = app.signals.play_count("id0");
    let session_events = app.streaming.session_events.len();
    let epoch = app.playback.position_epoch;
    let loaded = app.prefetch.loaded_video_id.clone();
    let session_plays = app.session.plays;
    let art = app.art.video_id.clone();
    let lyrics = app
        .lyrics
        .track
        .as_ref()
        .map(|track| track.video_id.clone());

    let cmds = app.on_player_action(Action::NextTrack);

    assert_loads_video(&cmds, "id1");
    assert_eq!(app.queue.cursor_pos(), cursor);
    assert_eq!(current(&app), "id0");
    assert_eq!(
        app.library
            .history
            .iter()
            .map(|song| song.video_id.clone())
            .collect::<Vec<_>>(),
        history
    );
    assert_eq!(app.signals.play_count("id0"), signal_count);
    assert_eq!(app.streaming.session_events.len(), session_events);
    assert_eq!(app.playback.position_epoch, epoch);
    assert_eq!(app.prefetch.loaded_video_id, loaded);
    assert_eq!(app.session.plays, session_plays);
    assert_eq!(app.art.video_id, art);
    assert_eq!(
        app.lyrics
            .track
            .as_ref()
            .map(|track| track.video_id.clone()),
        lyrics
    );
    assert!(
        !cmds.iter().any(|cmd| matches!(cmd, Cmd::Persist(_))),
        "persistence is a post-admission effect"
    );
}

#[test]
fn accepted_next_commits_state_and_effects_exactly_once() {
    let mut app = app_playing(3, 0);
    let epoch = app.playback.position_epoch;
    let history_len = app.library.history.len();
    let signal_count = app.signals.play_count("id0");
    let session_events = app.streaming.session_events.len();
    let session_plays = app.session.plays;
    let mut cmds = app.on_player_action(Action::NextTrack);

    let follow_ups = app.admit_player_intents_with_followups_for_test(&cmds);
    cmds.extend(follow_ups);

    assert_eq!(current(&app), "id1");
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id1"));
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert_eq!(app.signals.play_count("id0"), signal_count + 1);
    assert_eq!(app.streaming.session_events.len(), session_events + 1);
    assert_eq!(app.session.plays, session_plays + 1);
    assert_eq!(app.library.history.len(), history_len + 1);
    assert_eq!(
        app.library
            .history
            .iter()
            .filter(|song| song.video_id == "id1")
            .count(),
        1
    );
    assert_eq!(
        player_batch(&cmds)
            .iter()
            .filter(|command| matches!(command, PlayerCmd::Load(_)))
            .count(),
        1
    );
    assert_eq!(
        cmds.iter()
            .filter(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Library)))
            .count(),
        1
    );
    assert_eq!(
        cmds.iter()
            .filter(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Signals)))
            .count(),
        1
    );
}

#[test]
fn admitted_next_uses_the_exact_prefetched_url_selected_during_prepare() {
    let mut app = app_playing(3, 0);
    let direct = "https://cdn.example/id1.m4a";
    app.prefetch
        .resolved
        .insert("id1".to_owned(), direct.to_owned());

    let mut cmds = app.on_player_action(Action::NextTrack);

    assert_eq!(load_url(&cmds), Some(direct));
    assert_eq!(current(&app), "id0", "prefetch selection is still pure");
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(current(&app), "id1");
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id1"));
    assert!(app.prefetch.last_load_prefetched);
}

#[test]
fn recorder_clear_load_and_filter_share_one_ordered_batch() {
    let mut app = recording_app(crate::recorder::RecordingMode::Decide);
    feed_title(&mut app, "Artist - Live Track");
    assert!(app.recorder.current.is_some());
    app.queue
        .extend(vec![Song::remote("next", "Next", "Artist", "3:00")]);
    app.audio.bands[0] = 1.0;

    let cmds = app.on_player_action(Action::NextTrack);
    let batch = player_batch(&cmds);

    assert_eq!(batch.len(), 3);
    assert!(batch[0].property().is_some_and(|(name, value)| {
        name == "stream-record" && value == &serde_json::Value::from("")
    }));
    assert!(matches!(batch[1], PlayerCmd::Load(_)));
    assert!(matches!(batch[2], PlayerCmd::SetAudioFilter(_)));
    assert!(
        app.recorder.current.is_some(),
        "recorder state waits for whole-batch admission"
    );

    let follow_ups = app.admit_player_intents_with_followups_for_test(&cmds);
    assert!(app.recorder.current.is_none());
    assert_eq!(
        follow_ups
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::Recorder(_)))
            .count(),
        1
    );
    assert!(
        follow_ups
            .iter()
            .all(|cmd| cmd.player_commands().next().is_none()),
        "required player commands must not be resent after commit"
    );
}

#[test]
fn repeat_one_eof_reloads_once_and_bumps_epoch_once() {
    let mut app = app_playing(2, 0);
    app.queue.repeat = crate::queue::Repeat::One;
    let epoch = app.playback.position_epoch;
    let mut cmds = app.update(PlayerMsg::Eof);

    assert_eq!(current(&app), "id0");
    assert_eq!(app.playback.position_epoch, epoch);
    assert_eq!(load_watch_video_id(&cmds).as_deref(), Some("id0"));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(current(&app), "id0");
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert_eq!(
        player_batch(&cmds)
            .iter()
            .filter(|command| matches!(command, PlayerCmd::Load(_)))
            .count(),
        1
    );
}

#[test]
fn repeat_all_eof_wraps_only_after_admission() {
    let mut app = app_playing(2, 1);
    app.queue.repeat = crate::queue::Repeat::All;
    let epoch = app.playback.position_epoch;
    let mut cmds = app.update(PlayerMsg::Eof);

    assert_eq!(current(&app), "id1");
    assert_loads_video(&cmds, "id0");
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(current(&app), "id0");
    assert_eq!(app.playback.position_epoch, epoch + 1);
}

#[test]
fn repeat_off_queue_end_admits_stop_without_a_fake_load() {
    let mut app = app_playing(2, 1);
    let epoch = app.playback.position_epoch;
    let cmds = app.on_player_action(Action::NextTrack);

    assert_no_load(&cmds);
    assert!(has_stop(&cmds));
    assert_eq!(app.playback.position_epoch, epoch);
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id1"));
    let follow_ups = app.admit_player_intents_with_followups_for_test(&cmds);
    assert!(
        follow_ups
            .iter()
            .all(|cmd| cmd.player_commands().next().is_none())
    );
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert!(app.prefetch.loaded_video_id.is_none());
    assert_eq!(current(&app), "id1", "queue cursor stays at its final item");
}

#[test]
fn queue_play_success_commits_cursor_popup_and_load_exactly_once() {
    use crate::util::delivery::DeliveryReceipt;

    let mut app = app_playing(3, 0);
    app.open_queue_popup();
    app.queue_popup.cursor = 1;
    app.queue_popup.anchor = 1;
    let epoch = app.playback.position_epoch;
    let history_len = app.library.history.len();
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();

    let cmds = app.update(Msg::Remote(
        crate::remote::proto::RemoteCommand::QueuePlay { position: 1 },
        reply.into(),
    ));

    assert_eq!(current(&app), "id0");
    assert!(app.queue_popup.open);
    assert_eq!(app.queue_popup.cursor, 1);
    assert_eq!(app.library.history.len(), history_len);
    assert_eq!(app.playback.position_epoch, epoch);
    let intent = take_player_intent(cmds);
    assert_eq!(
        intent
            .commands
            .iter()
            .filter(|command| matches!(command, PlayerCmd::Load(_)))
            .count(),
        1
    );

    let effects = crate::runtime::player_delivery::settle_player_intent(
        &mut app,
        intent,
        Ok(DeliveryReceipt::Deferred),
    );

    assert_eq!(current(&app), "id1");
    assert!(!app.queue_popup.open);
    assert_eq!(app.queue_popup.cursor, 1);
    assert_eq!(app.queue_popup.anchor, 1);
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert_eq!(app.library.history.len(), history_len + 1);
    assert_eq!(
        app.library
            .history
            .iter()
            .filter(|song| song.video_id == "id1")
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Library)))
            .count(),
        1
    );
    let response = reply_rx.try_recv().expect("correlated success response");
    assert!(response.ok);
    assert_eq!(response.status.expect("status snapshot").position, 2);
    assert!(matches!(
        reply_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
}

#[test]
fn queue_play_busy_and_closed_leave_cursor_popup_and_load_state_unchanged() {
    use crate::util::delivery::DeliveryError;

    for (error, reason) in [
        (DeliveryError::Busy, "player_busy"),
        (DeliveryError::Closed, "player_unavailable"),
    ] {
        let mut app = app_playing(3, 0);
        app.open_queue_popup();
        app.queue_popup.cursor = 1;
        app.queue_popup.anchor = 1;
        let epoch = app.playback.position_epoch;
        let history_len = app.library.history.len();
        let loaded = app.prefetch.loaded_video_id.clone();
        let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
        let cmds = app.update(Msg::Remote(
            crate::remote::proto::RemoteCommand::QueuePlay { position: 1 },
            reply.into(),
        ));

        let effects = crate::runtime::player_delivery::settle_player_intent(
            &mut app,
            take_player_intent(cmds),
            Err(error),
        );

        assert!(effects.is_empty());
        assert_eq!(current(&app), "id0");
        assert!(app.queue_popup.open);
        assert_eq!(app.queue_popup.cursor, 1);
        assert_eq!(app.queue_popup.anchor, 1);
        assert_eq!(app.prefetch.loaded_video_id, loaded);
        assert_eq!(app.library.history.len(), history_len);
        assert_eq!(app.playback.position_epoch, epoch);
        let response = reply_rx.try_recv().expect("correlated rejection response");
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some(reason));
    }
}

#[test]
fn resume_success_loads_current_track_and_replies_exactly_once() {
    use crate::util::delivery::DeliveryReceipt;

    let mut app = App::new(100);
    app.queue.set(songs(2), 1);
    app.playback.paused = true;
    let epoch = app.playback.position_epoch;
    let session_plays = app.session.plays;
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    let cmds = app.update(Msg::Remote(
        crate::remote::proto::RemoteCommand::ResumeSession,
        reply.into(),
    ));

    assert_eq!(current(&app), "id1");
    assert!(app.playback.paused);
    assert!(app.prefetch.loaded_video_id.is_none());
    assert!(app.library.history.is_empty());
    assert_eq!(app.playback.position_epoch, epoch);
    let intent = take_player_intent(cmds);
    assert_eq!(
        intent
            .commands
            .iter()
            .filter(|command| matches!(command, PlayerCmd::Load(_)))
            .count(),
        1
    );

    let effects = crate::runtime::player_delivery::settle_player_intent(
        &mut app,
        intent,
        Ok(DeliveryReceipt::Deferred),
    );

    assert_eq!(current(&app), "id1");
    assert!(!app.playback.paused);
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id1"));
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert_eq!(app.session.plays, session_plays + 1);
    assert_eq!(
        app.library
            .history
            .iter()
            .filter(|song| song.video_id == "id1")
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Library)))
            .count(),
        1
    );
    let response = reply_rx.try_recv().expect("correlated success response");
    assert!(response.ok);
    let status = response.status.expect("status snapshot");
    assert_eq!(status.position, 2);
    assert!(!status.paused);
    assert!(matches!(
        reply_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
}

#[test]
fn resume_busy_and_closed_leave_current_track_state_unchanged() {
    use crate::util::delivery::DeliveryError;

    for (error, reason) in [
        (DeliveryError::Busy, "player_busy"),
        (DeliveryError::Closed, "player_unavailable"),
    ] {
        let mut app = App::new(100);
        app.queue.set(songs(2), 1);
        app.playback.paused = true;
        let epoch = app.playback.position_epoch;
        let session_plays = app.session.plays;
        let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
        let cmds = app.update(Msg::Remote(
            crate::remote::proto::RemoteCommand::ResumeSession,
            reply.into(),
        ));

        let effects = crate::runtime::player_delivery::settle_player_intent(
            &mut app,
            take_player_intent(cmds),
            Err(error),
        );

        assert!(effects.is_empty());
        assert_eq!(current(&app), "id1");
        assert!(app.playback.paused);
        assert!(app.prefetch.loaded_video_id.is_none());
        assert!(app.library.history.is_empty());
        assert_eq!(app.session.plays, session_plays);
        assert_eq!(app.playback.position_epoch, epoch);
        let response = reply_rx.try_recv().expect("correlated rejection response");
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some(reason));
    }
}

#[test]
fn empty_queue_resume_restores_history_only_after_admission() {
    use crate::util::delivery::DeliveryReceipt;

    let mut app = App::new(100);
    app.library_mut()
        .record_play(&Song::remote("history", "History", "A", "3:00"));
    let history_len = app.library.history.len();
    let rev = app.queue.rev();
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();

    let cmds = app.update(Msg::Remote(
        crate::remote::proto::RemoteCommand::ResumeSession,
        reply.into(),
    ));

    assert_loads_video(&cmds, "history");
    assert!(app.queue.is_empty());
    assert_eq!(app.queue.rev(), rev);
    assert_eq!(app.library.history.len(), history_len);
    assert!(app.prefetch.loaded_video_id.is_none());
    assert!(matches!(
        reply_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    let effects = crate::runtime::player_delivery::settle_player_intent(
        &mut app,
        take_player_intent(cmds),
        Ok(DeliveryReceipt::Deferred),
    );

    assert_eq!(current(&app), "history");
    assert_ne!(app.queue.rev(), rev);
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("history"));
    assert_eq!(app.library.history.len(), history_len);
    assert!(!effects.is_empty());
    let response = reply_rx.try_recv().expect("correlated success response");
    assert!(response.ok);
    assert_eq!(response.status.expect("status snapshot").queue.len(), 1);
}

#[test]
fn rejected_empty_queue_resume_keeps_history_seed_uncommitted() {
    use crate::util::delivery::DeliveryError;

    for (error, reason) in [
        (DeliveryError::Busy, "player_busy"),
        (DeliveryError::Closed, "player_unavailable"),
    ] {
        let mut app = App::new(100);
        app.library_mut()
            .record_play(&Song::remote("history", "History", "A", "3:00"));
        let history_len = app.library.history.len();
        let rev = app.queue.rev();
        let epoch = app.playback.position_epoch;
        let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
        let cmds = app.update(Msg::Remote(
            crate::remote::proto::RemoteCommand::ResumeSession,
            reply.into(),
        ));

        let effects = crate::runtime::player_delivery::settle_player_intent(
            &mut app,
            take_player_intent(cmds),
            Err(error),
        );

        assert!(effects.is_empty());
        assert!(app.queue.is_empty());
        assert_eq!(app.queue.rev(), rev);
        assert_eq!(app.playback.position_epoch, epoch);
        assert_eq!(app.library.history.len(), history_len);
        assert!(app.prefetch.loaded_video_id.is_none());
        let response = reply_rx.try_recv().expect("correlated rejection response");
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some(reason));
    }
}

#[test]
fn cold_toggle_pause_and_startup_autoplay_wait_for_load_admission() {
    use crate::util::delivery::{DeliveryError, DeliveryReceipt};

    let mut cold = App::new(100);
    cold.queue.set(songs(1), 0);
    cold.playback.paused = true;
    let cold_epoch = cold.playback.position_epoch;
    let cmds = cold.on_player_action(Action::TogglePause);
    assert_loads_video(&cmds, "id0");
    assert!(cold.playback.paused);
    assert!(cold.prefetch.loaded_video_id.is_none());
    assert_eq!(cold.playback.position_epoch, cold_epoch);
    let effects = crate::runtime::player_delivery::settle_player_intent(
        &mut cold,
        take_player_intent(cmds),
        Err(DeliveryError::Busy),
    );
    assert!(effects.is_empty());
    assert!(cold.playback.paused);
    assert!(cold.prefetch.loaded_video_id.is_none());
    assert_eq!(cold.playback.position_epoch, cold_epoch);

    let mut startup = App::new(100);
    startup.queue.set(songs(1), 0);
    startup.playback.paused = true;
    startup.config.autoplay_on_start = Some(true);
    let startup_epoch = startup.playback.position_epoch;
    let cmds = startup.update(Msg::Autoplay);
    assert_loads_video(&cmds, "id0");
    assert!(startup.playback.paused);
    assert!(startup.prefetch.loaded_video_id.is_none());
    assert_eq!(startup.playback.position_epoch, startup_epoch);
    let effects = crate::runtime::player_delivery::settle_player_intent(
        &mut startup,
        take_player_intent(cmds),
        Ok(DeliveryReceipt::Deferred),
    );
    assert!(!startup.playback.paused);
    assert_eq!(startup.prefetch.loaded_video_id.as_deref(), Some("id0"));
    assert_eq!(startup.playback.position_epoch, startup_epoch + 1);
    assert_eq!(
        effects
            .iter()
            .filter(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Library)))
            .count(),
        1
    );
}
