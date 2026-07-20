use super::*;

fn ordered_ids(queue: &crate::queue::Queue) -> Vec<String> {
    queue
        .ordered()
        .into_iter()
        .map(|song| song.video_id.clone())
        .collect()
}

fn history_count(app: &App, video_id: &str) -> usize {
    app.library
        .history
        .iter()
        .filter(|song| song.video_id == video_id)
        .count()
}

fn take_player_intent(cmds: Vec<Cmd>) -> PlayerIntent {
    cmds.into_iter()
        .find_map(|cmd| match cmd {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(*intent),
            _ => None,
        })
        .expect("queue mutation player intent")
}

#[test]
fn play_now_busy_and_closed_leave_state_retryable_and_commit_once() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = app_playing(3, 0);
        app.mode = Mode::Search;
        app.status.kind = StatusKind::Info;
        app.status.text = "keep until admission".to_owned();
        let before_order = ordered_ids(&app.queue);
        let before_rev = app.queue.rev();
        let before_epoch = app.playback.position_epoch;
        let before_history = app.library.history.len();
        let inserted = Song::remote("retry000001", "Retry", "Artist", "3:00");

        let cmds = app.play_now_many(vec![inserted.clone()]);

        assert_loads_video(&cmds, &inserted.video_id);
        assert_eq!(ordered_ids(&app.queue), before_order);
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.status.text, "keep until admission");
        assert_eq!(app.playback.position_epoch, before_epoch);
        assert_eq!(app.library.history.len(), before_history);

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        assert_eq!(ordered_ids(&app.queue), before_order);
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.playback.position_epoch, before_epoch);
        assert_eq!(app.library.history.len(), before_history);

        let mut retry = app.play_now_many(vec![inserted.clone()]);
        assert_loads_video(&retry, &inserted.video_id);
        assert_eq!(ordered_ids(&app.queue), before_order);
        admit_player_transition(&mut app, &mut retry);

        assert_eq!(current(&app), inserted.video_id);
        assert_eq!(app.queue.len(), 4, "the rejected attempt was not inserted");
        assert_ne!(app.queue.rev(), before_rev);
        assert_eq!(app.mode, Mode::Player);
        assert!(app.status.text.is_empty());
        assert_eq!(app.playback.position_epoch, before_epoch + 1);
        assert_eq!(app.library.history.len(), before_history + 1);
        assert_eq!(history_count(&app, "retry000001"), 1);
    }
}

#[test]
fn idle_enqueue_busy_and_closed_leave_append_retryable_and_commit_once() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = App::new(100);
        app.queue.set(songs(2), 0);
        app.mode = Mode::Library;
        app.status.text = "idle".to_owned();
        let before_order = ordered_ids(&app.queue);
        let before_rev = app.queue.rev();
        let before_epoch = app.playback.position_epoch;
        let appended = Song::remote("idle0000001", "Idle", "Artist", "3:00");

        let cmds = app.enqueue_many(vec![appended.clone()]);

        assert_loads_video(&cmds, &appended.video_id);
        assert_eq!(ordered_ids(&app.queue), before_order);
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(app.mode, Mode::Library);
        assert_eq!(app.status.text, "idle");
        assert_eq!(app.playback.position_epoch, before_epoch);

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        assert_eq!(ordered_ids(&app.queue), before_order);
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(app.mode, Mode::Library);

        let mut retry = app.enqueue_many(vec![appended.clone()]);
        assert_eq!(ordered_ids(&app.queue), before_order);
        admit_player_transition(&mut app, &mut retry);

        assert_eq!(current(&app), appended.video_id);
        assert_eq!(app.queue.len(), 3, "the rejected append was not retained");
        assert_ne!(app.queue.rev(), before_rev);
        assert_eq!(app.mode, Mode::Player);
        assert_eq!(app.playback.position_epoch, before_epoch + 1);
        assert_eq!(history_count(&app, "idle0000001"), 1);
    }
}

#[test]
fn play_now_partial_capacity_and_romanization_commit_only_after_admission() {
    let mut app = app_playing(998, 0);
    app.config.romanized_titles = Some(true);
    app.why_gem.upsert(
        "id500".to_owned(),
        why_gem::streaming_origin_model(crate::streaming::StreamingMode::Balanced),
    );
    let requested = vec![
        Song::remote("cap00000001", "첫 번째", "가수", "3:00"),
        Song::remote("id500", "두 번째", "가수", "3:00"),
        Song::remote("cap00000003", "세 번째", "가수", "3:00"),
    ];
    let before_rev = app.queue.rev();
    let before_epoch = app.playback.position_epoch;

    let mut cmds = app.play_now_many(requested.clone());

    assert_eq!(app.queue.len(), 998);
    assert_eq!(app.queue.rev(), before_rev);
    assert_eq!(app.playback.position_epoch, before_epoch);
    assert!(
        requested
            .iter()
            .all(|song| app.romanization.cache.entry_for(song).is_none())
    );
    assert!(
        cmds.iter()
            .all(|cmd| !matches!(cmd, Cmd::Persist(PersistCmd::RomanizedTitles)))
    );
    admit_player_transition(&mut app, &mut cmds);

    assert_eq!(app.queue.len(), 999);
    assert_eq!(current(&app), "cap00000001");
    assert!(app.queue.video_ids().any(|id| id == "cap00000001"));
    assert_eq!(app.queue.video_ids().filter(|id| *id == "id500").count(), 1);
    assert!(!app.queue.video_ids().any(|id| id == "cap00000003"));
    assert!(
        app.why_gem.contains("id500"),
        "a cap-rejected duplicate must not erase the existing queue row's provenance"
    );
    assert_ne!(app.queue.rev(), before_rev);
    assert_eq!(app.playback.position_epoch, before_epoch + 1);
    assert!(
        requested
            .iter()
            .all(|song| app.romanization.cache.entry_for(song).is_some()),
        "legacy semantics romanize the complete request, including cap-truncated songs"
    );
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::RomanizedTitles)))
    );

    let full_rev = app.queue.rev();
    let rejected = app.play_now(Song::remote("cap00000004", "가득 참", "가수", "3:00"));
    assert!(rejected.is_empty());
    assert_eq!(app.queue.rev(), full_rev);
    assert_eq!(app.status.kind, StatusKind::Error);
}

#[test]
fn idle_enqueue_selects_the_same_shuffled_append_and_installs_advanced_rng() {
    let mut app = App::new(100);
    app.queue.set(songs(5), 2);
    app.queue.seed_rng(0x5151);
    app.queue.set_shuffle(true);
    app.queue.seed_rng(0x8181);
    app.queue.repeat = crate::queue::Repeat::All;

    let mut expected = crate::queue::Queue::default();
    expected.set(songs(5), 2);
    expected.seed_rng(0x5151);
    expected.set_shuffle(true);
    expected.seed_rng(0x8181);
    expected.repeat = crate::queue::Repeat::All;

    let appended = vec![
        Song::remote("shuffle0001", "One", "A", "3:00"),
        Song::remote("shuffle0002", "Two", "A", "3:00"),
        Song::remote("shuffle0003", "Three", "A", "3:00"),
        Song::remote("shuffle0004", "Four", "A", "3:00"),
    ];
    expected.extend(appended.clone());
    expected.goto(5);
    let selected = expected.current().unwrap().video_id.clone();
    let before_order = ordered_ids(&app.queue);
    let before_rev = app.queue.rev();

    let mut cmds = app.enqueue_many(appended);

    assert_loads_video(&cmds, &selected);
    assert_eq!(ordered_ids(&app.queue), before_order);
    assert_eq!(app.queue.rev(), before_rev);
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(ordered_ids(&app.queue), ordered_ids(&expected));
    assert_eq!(current(&app), selected);
    assert!(app.queue.shuffle);
    assert_eq!(app.queue.repeat, crate::queue::Repeat::All);

    let tail = vec![
        Song::remote("shuffle0005", "Five", "A", "3:00"),
        Song::remote("shuffle0006", "Six", "A", "3:00"),
        Song::remote("shuffle0007", "Seven", "A", "3:00"),
    ];
    app.queue.extend(tail.clone());
    expected.extend(tail);
    assert_eq!(
        ordered_ids(&app.queue),
        ordered_ids(&expected),
        "commit installs the RNG state advanced during preparation"
    );
}

#[test]
fn play_now_skips_an_invalid_first_candidate_in_the_plan_before_commit() {
    let mut app = app_playing(2, 0);
    let before_order = ordered_ids(&app.queue);
    let before_rev = app.queue.rev();
    let invalid = Song::remote(
        "UCfLdIEPs1tYj4ieEdJnyNyw",
        "Channel, not a video",
        "Owner",
        "",
    );
    let valid = Song::remote("valid000001", "Playable", "Artist", "3:00");

    let mut cmds = app.play_now_many(vec![invalid.clone(), valid.clone()]);

    assert_loads_video(&cmds, &valid.video_id);
    assert_eq!(ordered_ids(&app.queue), before_order);
    assert_eq!(app.queue.rev(), before_rev);
    assert_eq!(current(&app), "id0");
    admit_player_transition(&mut app, &mut cmds);

    assert_eq!(current(&app), valid.video_id);
    assert_eq!(
        ordered_ids(&app.queue),
        vec![
            "id0".to_owned(),
            invalid.video_id,
            valid.video_id,
            "id1".to_owned(),
        ]
    );
    assert_ne!(app.queue.rev(), before_rev);
}

#[test]
fn non_current_range_removal_commits_immediately_without_player_work() {
    let mut app = app_playing(5, 0);
    app.open_queue_popup();
    let loaded = app.prefetch.loaded_video_id.clone();
    let epoch = app.playback.position_epoch;
    let before_bumps = app.queue.revision_bumps();

    let cmds = app.remove_queue_range(2, 4);

    assert!(cmds.is_empty());
    assert_eq!(ordered_ids(&app.queue), vec!["id0", "id1"]);
    assert_eq!(current(&app), "id0");
    assert_eq!(app.queue.revision_bumps(), before_bumps + 1);
    assert_eq!(app.prefetch.loaded_video_id, loaded);
    assert_eq!(app.playback.position_epoch, epoch);
    assert!(app.queue_popup.open);
    assert_eq!((app.queue_popup.cursor, app.queue_popup.anchor), (1, 1));
}

#[test]
fn current_range_busy_and_closed_preserve_queue_popup_and_playback_until_retry() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = app_playing(5, 1);
        app.open_queue_popup();
        app.queue_popup.anchor = 1;
        app.queue_popup.cursor = 3;
        app.art.video_id = Some("art-id1".to_owned());
        app.lyrics.track = Some(TrackLyrics {
            video_id: "id1".into(),
            lines: Vec::new().into(),
        });
        let before_ids = ordered_ids(&app.queue);
        let before_rev = app.queue.rev();
        let before_bumps = app.queue.revision_bumps();
        let before_epoch = app.playback.position_epoch;
        let before_loaded = app.prefetch.loaded_video_id.clone();
        let before_history = app.library.history.len();

        let cmds = app.remove_queue_range(1, 3);

        assert_loads_video(&cmds, "id4");
        assert_eq!(ordered_ids(&app.queue), before_ids);
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(app.queue.revision_bumps(), before_bumps);
        assert_eq!((app.queue_popup.cursor, app.queue_popup.anchor), (3, 1));
        assert_eq!(app.prefetch.loaded_video_id, before_loaded);
        assert_eq!(app.playback.position_epoch, before_epoch);
        assert_eq!(app.art.video_id.as_deref(), Some("art-id1"));
        assert_eq!(
            app.lyrics
                .track
                .as_ref()
                .map(|lyrics| lyrics.video_id.as_ref()),
            Some("id1")
        );

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        assert_eq!(ordered_ids(&app.queue), before_ids);
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(app.queue.revision_bumps(), before_bumps);
        assert_eq!((app.queue_popup.cursor, app.queue_popup.anchor), (3, 1));
        assert_eq!(app.prefetch.loaded_video_id, before_loaded);
        assert_eq!(app.playback.position_epoch, before_epoch);
        assert_eq!(app.library.history.len(), before_history);
        assert_eq!(app.art.video_id.as_deref(), Some("art-id1"));

        let mut retry = app.remove_queue_range(1, 3);
        admit_player_transition(&mut app, &mut retry);
        assert_eq!(ordered_ids(&app.queue), vec!["id0", "id4"]);
        assert_eq!(current(&app), "id4");
        assert_eq!(app.queue.revision_bumps(), before_bumps + 1);
        assert_eq!((app.queue_popup.cursor, app.queue_popup.anchor), (1, 1));
        assert!(app.queue_popup.open);
        assert_eq!(app.playback.position_epoch, before_epoch + 1);
        assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id4"));
    }
}

#[test]
fn full_queue_delete_waits_for_stop_admission_and_commits_once() {
    let mut app = app_playing(3, 1);
    app.open_queue_popup();
    app.queue_popup.anchor = 0;
    app.queue_popup.cursor = 2;
    let before_bumps = app.queue.revision_bumps();
    let before_epoch = app.playback.position_epoch;

    let mut cmds = app.remove_queue_range(0, usize::MAX);

    assert!(has_stop(&cmds));
    assert_eq!(app.queue.len(), 3);
    assert!(app.queue_popup.open);
    assert_eq!(app.playback.position_epoch, before_epoch);
    admit_player_transition(&mut app, &mut cmds);
    assert!(app.queue.is_empty());
    assert_eq!(app.queue.revision_bumps(), before_bumps + 1);
    assert!(!app.queue_popup.open);
    assert_eq!((app.queue_popup.cursor, app.queue_popup.anchor), (0, 0));
    assert!(app.playback.paused);
    assert!(app.prefetch.loaded_video_id.is_none());
    assert_eq!(app.playback.position_epoch, before_epoch + 1);
}

#[test]
fn remote_current_remove_refills_and_replies_only_after_successful_commit() {
    use crate::util::delivery::DeliveryReceipt;

    let mut app = app_playing(2, 0);
    app.autoplay_streaming = true;
    let before_bumps = app.queue.revision_bumps();
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    let cmds = app.update(Msg::Remote(
        crate::remote::proto::RemoteCommand::QueueRemove { position: 0 },
        reply.into(),
    ));

    assert_eq!(app.queue.len(), 2);
    assert!(!app.streaming.pending);
    assert!(
        cmds.iter()
            .all(|cmd| !matches!(cmd, Cmd::StreamingFallback { .. }))
    );
    assert!(matches!(
        reply_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    let effects = crate::runtime::player_delivery::settle_player_intent(
        &mut app,
        take_player_intent(cmds),
        Ok(DeliveryReceipt::Deferred),
    );

    assert_eq!(ordered_ids(&app.queue), vec!["id1"]);
    assert_eq!(app.queue.revision_bumps(), before_bumps + 1);
    assert!(app.streaming.pending);
    assert!(
        effects
            .iter()
            .any(|cmd| matches!(cmd, Cmd::StreamingFallback { .. }))
    );
    let response = reply_rx.try_recv().expect("correlated success response");
    assert!(response.ok);
    assert_eq!(response.status.expect("status snapshot").queue.len(), 1);
}

#[test]
fn remote_current_remove_busy_and_closed_reply_with_error_and_preserve_state() {
    use crate::util::delivery::DeliveryError;

    for (error, reason) in [
        (DeliveryError::Busy, "player_busy"),
        (DeliveryError::Closed, "player_unavailable"),
    ] {
        let mut app = app_playing(2, 0);
        app.open_queue_popup();
        let before_ids = ordered_ids(&app.queue);
        let before_rev = app.queue.rev();
        let before_bumps = app.queue.revision_bumps();
        let before_epoch = app.playback.position_epoch;
        let before_loaded = app.prefetch.loaded_video_id.clone();
        let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
        let cmds = app.update(Msg::Remote(
            crate::remote::proto::RemoteCommand::QueueRemove { position: 0 },
            reply.into(),
        ));

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        assert_eq!(ordered_ids(&app.queue), before_ids);
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(app.queue.revision_bumps(), before_bumps);
        assert_eq!(app.playback.position_epoch, before_epoch);
        assert_eq!(app.prefetch.loaded_video_id, before_loaded);
        assert!(app.queue_popup.open);
        let response = reply_rx.try_recv().expect("correlated rejection response");
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some(reason));
    }
}
