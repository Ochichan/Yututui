use super::*;

#[test]
fn failed_exact_append_retention_uses_its_independent_twenty_thousand_event_cap() {
    let (dir, queue) = temp_queue("open-subsonic-append-cap");
    queue.fail_next_appends(1);
    let (events, emit) = captured_events();
    let mut actor = test_actor(inactive_settings(), Some(queue), emit);
    for index in 0..OPEN_SUBSONIC_QUEUE_CAP {
        let mut pending =
            QueueEntry::from_track(&open_subsonic_track(1_000 + index as i64), Vec::new());
        pending.id = format!("exact-pending-{index}");
        actor.pending_appends.push_back(pending);
    }
    actor.pending_open_subsonic_appends = OPEN_SUBSONIC_QUEUE_CAP;
    let oldest_id = actor.pending_appends.front().unwrap().id.clone();

    actor.enqueue_scrobble(open_subsonic_track(30_000));

    assert_eq!(actor.pending_appends.len(), OPEN_SUBSONIC_QUEUE_CAP);
    assert_eq!(actor.pending_open_subsonic_appends, OPEN_SUBSONIC_QUEUE_CAP);
    assert_eq!(actor.pending_appends.front().unwrap().id, oldest_id);
    assert_eq!(
        actor.queue.as_ref().unwrap().drop_audit_counts().unwrap(),
        (0, 1, 1)
    );
    let captured = events.lock().unwrap();
    assert!(matches!(
        captured.first(),
        Some(ScrobbleEvent::QueueDropped { dropped: 1 })
    ));
    assert!(matches!(
        captured.get(1),
        Some(ScrobbleEvent::QueueStalled {
            pending: OPEN_SUBSONIC_QUEUE_CAP
        })
    ));
    drop(captured);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn exact_in_memory_overflow_remains_retained_until_its_loss_audit_is_durable() {
    let (dir, queue) = temp_queue("open-subsonic-append-audit-failure");
    let audit_path = queue.path().with_extension("drops.json");
    std::fs::create_dir_all(audit_path.parent().unwrap()).unwrap();
    std::fs::write(&audit_path, b"{invalid").unwrap();
    let (events, emit) = captured_events();
    let mut actor = test_actor(inactive_settings(), Some(queue), emit);
    for index in 0..OPEN_SUBSONIC_QUEUE_CAP {
        let mut pending =
            QueueEntry::from_track(&open_subsonic_track(1_000 + index as i64), Vec::new());
        pending.id = format!("retained-{index}");
        actor.pending_appends.push_back(pending);
    }
    actor.pending_open_subsonic_appends = OPEN_SUBSONIC_QUEUE_CAP;

    actor.enqueue_scrobble(open_subsonic_track(30_000));

    assert_eq!(
        actor.pending_open_subsonic_appends,
        OPEN_SUBSONIC_QUEUE_CAP + 1
    );
    assert!(
        !actor.accepts_command_ingress(),
        "cap+1 closes the bounded command inbox until the audit retry finishes"
    );
    assert_eq!(actor.pending_appends.len(), OPEN_SUBSONIC_QUEUE_CAP + 1);
    assert!(actor.pending_appends.back().unwrap().open_subsonic_pending);
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|event| !matches!(event, ScrobbleEvent::QueueDropped { .. }))
    );

    std::fs::remove_file(audit_path).unwrap();
    actor.queue.as_ref().unwrap().fail_next_appends(1);
    assert_eq!(actor.retry_pending_appends(), Err(DeliveryError::Saturated));
    assert_eq!(actor.pending_open_subsonic_appends, OPEN_SUBSONIC_QUEUE_CAP);
    assert!(actor.accepts_command_ingress());
    assert_eq!(actor.pending_appends.len(), OPEN_SUBSONIC_QUEUE_CAP);
    assert_eq!(
        actor.queue.as_ref().unwrap().drop_audit_counts().unwrap(),
        (0, 1, 1)
    );
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, ScrobbleEvent::QueueDropped { dropped: 1 }))
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn owner_acknowledgement_handoff_is_bounded_without_retiring_accepted_markers() {
    let (dir, queue) = temp_queue("open-subsonic-ack-cap");
    let (_events, emit) = captured_events();
    let mut actor = test_actor(inactive_settings(), Some(queue), emit);
    for index in 0..OPEN_SUBSONIC_QUEUE_CAP {
        actor.pending_open_subsonic_acks.insert(
            format!("accepted-{index}"),
            PendingOpenSubsonicMarkerAcks::default(),
        );
    }
    let durability = Arc::new(AtomicU8::new(MARKER_ACK_QUEUED));
    actor.confirm_open_subsonic_submission(OpenSubsonicMarkerAckRequest {
        event_id: "overflow".to_owned(),
        stage: OpenSubsonicMarkerAckStage::BridgeQueued,
        durability: Arc::clone(&durability),
    });

    assert_eq!(
        actor.pending_open_subsonic_acks.len(),
        OPEN_SUBSONIC_QUEUE_CAP
    );
    assert!(!actor.pending_open_subsonic_acks.contains_key("overflow"));
    assert!(actor.pending_open_subsonic_acks.contains_key("accepted-0"));
    assert_eq!(durability.load(Ordering::Acquire), MARKER_ACK_IDLE);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "current_thread")]
async fn fresh_terminal_owner_observation_crosses_threshold_before_actor_shutdown() {
    let (dir, queue) = temp_queue("open-subsonic-final-owner-observation");
    let queue_path = queue.path().to_path_buf();
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    let (_events, emit) = captured_events();
    let mut handle = ScrobbleHandle::new(tx, shutdown_tx);
    let pending = Arc::clone(&handle.pending);
    let actor = tokio::spawn(run_actor(
        rx,
        shutdown_rx,
        pending,
        inactive_settings(),
        emit,
        Some(queue),
    ));
    let item = crate::open_subsonic::OpenSubsonicItemRef::new(
        crate::open_subsonic::BackendId::new("server-backend").unwrap(),
        crate::open_subsonic::AccountScopeId::new("account-scope").unwrap(),
        crate::open_subsonic::ItemId::new("final-song").unwrap(),
    );
    let observed_track = ObservedTrack {
        key: "final-song".to_owned(),
        open_subsonic_item: Some(item.clone()),
        title: "Final Song".to_owned(),
        artist: "Artist".to_owned(),
        album: Some("Album".to_owned()),
        duration: Some(40.0),
        is_live: false,
        is_local: false,
        origin_url: None,
        liked: false,
    };
    let started_at = Instant::now()
        .checked_sub(Duration::from_secs(20))
        .expect("monotonic clock represents a short prior listen");
    let started_unix = crate::signals::unix_now() - 20;
    for step in 0..4_u64 {
        assert!(
            handle
                .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                    Observation {
                        track: Some(observed_track.clone()),
                        playing: true,
                        stopped: false,
                        position: (step * 5) as f64,
                        position_epoch: 1,
                        rate: 1.0,
                        at: started_at + Duration::from_secs(step * 5),
                        wall_unix: started_unix + (step * 5) as i64,
                    },
                ))))
                .is_ok()
        );
    }
    let mut final_snapshot = crate::media::MediaSnapshot::idle();
    final_snapshot.status = crate::media::MediaPlaybackStatus::Playing;
    final_snapshot.position = 20.0;
    final_snapshot.track = Some(crate::media::MediaTrack {
        key: "final-song".to_owned(),
        open_subsonic_item: Some(item),
        title: "Final Song".to_owned(),
        artist: "Artist".to_owned(),
        album: Some("Album".to_owned()),
        duration: Some(40.0),
        is_live: false,
        url: None,
        art_remote_url: None,
        art_file: None,
        art_query: None,
        liked: false,
        disliked: false,
    });

    assert!(handle.observe_shutdown(&final_snapshot).is_ok());
    assert_eq!(handle.shutdown_flush().await, Ok(()));
    actor.await.expect("scrobble actor joins");

    let loaded = QueueFile::at(queue_path).load();
    assert!(!loaded.read_failed);
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].track_key, "final-song");
    assert!(loaded.entries[0].open_subsonic_pending);
    assert!(loaded.entries[0].open_subsonic_handoff_started);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn same_second_server_submissions_keep_distinct_shared_journal_ids() {
    let (dir, queue) = temp_queue("open-subsonic-same-second");
    let (events, emit) = captured_events();
    let mut actor = test_actor(inactive_settings(), Some(queue), emit);
    let first = open_subsonic_track(1_000);
    let second = open_subsonic_track(1_000);

    actor.record_durable_actions(vec![
        ScrobbleAction::Scrobble(first),
        ScrobbleAction::Scrobble(second),
    ]);
    assert_eq!(actor.flush().await, Ok(()));

    let loaded = actor.queue.as_ref().unwrap().load();
    assert_eq!(loaded.entries.len(), 2);
    assert_ne!(loaded.entries[0].id, loaded.entries[1].id);
    let captured = events.lock().unwrap();
    let event_ids = captured
        .iter()
        .map(|event| match event {
            ScrobbleEvent::OpenSubsonic {
                event_id,
                kind: crate::open_subsonic::OpenSubsonicScrobbleKind::Submission,
                confirmation: Some(_),
                ..
            } => event_id.as_str(),
            _ => panic!("only durable server submissions are expected"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        event_ids,
        loaded
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>()
    );
    drop(captured);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn owner_backpressure_reannounces_only_the_deferred_event_in_the_same_actor_run() {
    let (dir, queue) = temp_queue("open-subsonic-same-run-defer");
    let (events, emit) = captured_events();
    let (mut actor, _acknowledgement_rx) =
        test_actor_with_ack(inactive_settings(), Some(queue), emit);
    actor.record_durable_actions(vec![ScrobbleAction::Scrobble(open_subsonic_track(1_000))]);
    assert_eq!(actor.flush().await, Ok(()));

    let first = events.lock().unwrap().pop().unwrap();
    let (event_id, confirmation) = match first {
        ScrobbleEvent::OpenSubsonic {
            event_id,
            kind: crate::open_subsonic::OpenSubsonicScrobbleKind::Submission,
            confirmation: Some(confirmation),
            ..
        } => (event_id, confirmation),
        _ => panic!("expected one exact music-server submission"),
    };
    confirmation.defer_submission();
    assert!(confirmation.submission_is_deferred());

    assert_eq!(actor.flush().await, Ok(()));
    let retry = events.lock().unwrap().pop().unwrap();
    assert!(matches!(
        retry,
        ScrobbleEvent::OpenSubsonic {
            event_id: retried,
            kind: crate::open_subsonic::OpenSubsonicScrobbleKind::Submission,
            confirmation: Some(_),
            ..
        } if retried == event_id
    ));
    assert!(!confirmation.submission_is_deferred());
    assert!(
        actor.next_flush.is_some(),
        "an exact marker keeps the 30-second owner retry poll alive without external services"
    );

    assert_eq!(actor.flush().await, Ok(()));
    assert!(
        events.lock().unwrap().is_empty(),
        "the shared defer bit authorizes exactly one additional emission"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn cap_rewrite_failure_never_emits_or_evicts_a_started_handoff() {
    let (dir, queue) = temp_queue("open-subsonic-protected-cap-rewrite");
    let queue_path = queue.path().to_path_buf();
    let mut entries = (0..OPEN_SUBSONIC_QUEUE_CAP)
        .map(|index| {
            let mut entry =
                QueueEntry::from_track(&open_subsonic_track(1_000 + index as i64), Vec::new());
            entry.id = format!("protected-{index}");
            entry.open_subsonic_handoff_started = true;
            entry
        })
        .collect::<Vec<_>>();
    entries[0].open_subsonic_bridge_durable = true;
    let mut overflow = QueueEntry::from_track(&open_subsonic_track(30_000), Vec::new());
    overflow.id = "newest-never-handed-off".to_owned();
    entries.push(overflow);
    queue.rewrite(&entries).unwrap();
    queue.fail_next_rewrites(1);
    let (failed_events, failed_emit) = captured_events();
    let mut failed = test_actor(inactive_settings(), Some(queue), failed_emit);

    assert_eq!(failed.flush().await, Err(DeliveryError::Saturated));
    assert!(failed_events.lock().unwrap().is_empty());
    let unchanged = QueueFile::at(queue_path.clone()).load();
    assert_eq!(unchanged.entries.len(), OPEN_SUBSONIC_QUEUE_CAP + 1);
    assert!(
        unchanged
            .entries
            .iter()
            .any(|entry| entry.id == "protected-0"
                && entry.open_subsonic_handoff_started
                && entry.open_subsonic_bridge_durable)
    );
    drop(failed);

    let emitted = Arc::new(Mutex::new((0usize, false)));
    let emitted_for_sink = Arc::clone(&emitted);
    let emit: EventSink = Arc::new(move |event| {
        if let ScrobbleEvent::OpenSubsonic { event_id, .. } = event {
            let mut emitted = emitted_for_sink.lock().unwrap();
            emitted.0 += 1;
            emitted.1 |= event_id == "newest-never-handed-off";
        }
    });
    let mut restarted = test_actor(
        inactive_settings(),
        Some(QueueFile::at(queue_path.clone())),
        emit,
    );
    assert_eq!(restarted.flush().await, Ok(()));
    assert_eq!(*emitted.lock().unwrap(), (OPEN_SUBSONIC_QUEUE_CAP, false));
    let recovered = QueueFile::at(queue_path).load();
    assert_eq!(recovered.entries.len(), OPEN_SUBSONIC_QUEUE_CAP);
    assert!(
        recovered
            .entries
            .iter()
            .all(|entry| entry.open_subsonic_handoff_started)
    );
    assert!(
        recovered
            .entries
            .iter()
            .any(|entry| entry.id == "protected-0" && entry.open_subsonic_bridge_durable),
        "an AwaitingSourceAck marker cannot be orphaned by exact-event compaction"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn scope_retirement_requires_audit_and_rewrite_before_preserving_generic_delivery() {
    let (dir, queue) = temp_queue("open-subsonic-scope-retirement");
    let queue_path = queue.path().to_path_buf();
    let mut marker =
        QueueEntry::from_track(&open_subsonic_track(1_000), vec![ServiceKind::ListenBrainz]);
    marker.open_subsonic_handoff_started = true;
    marker.open_subsonic_bridge_durable = true;
    queue.rewrite(std::slice::from_ref(&marker)).unwrap();
    let (events, emit) = captured_events();
    let (mut actor, mut acknowledgement_rx) =
        test_actor_with_ack(inactive_settings(), Some(queue), emit);
    assert_eq!(actor.flush().await, Ok(()));
    let confirmation = match events.lock().unwrap().pop().unwrap() {
        ScrobbleEvent::OpenSubsonic {
            confirmation: Some(confirmation),
            ..
        } => confirmation,
        _ => panic!("expected an awaiting-source-ack marker"),
    };
    assert!(confirmation.bridge_marker_is_durable());
    confirmation.retire_account_scope().unwrap();
    actor.confirm_open_subsonic_submission(acknowledgement_rx.try_recv().unwrap());

    let audit_path = queue_path.with_extension("drops.json");
    std::fs::write(&audit_path, b"{invalid").unwrap();
    assert_eq!(actor.flush().await, Err(DeliveryError::Saturated));
    let after_audit_failure = QueueFile::at(queue_path.clone()).load().entries.remove(0);
    assert!(after_audit_failure.open_subsonic_pending);
    assert!(after_audit_failure.open_subsonic_handoff_started);
    assert!(after_audit_failure.open_subsonic_bridge_durable);
    assert_eq!(after_audit_failure.pending, vec![ServiceKind::ListenBrainz]);
    assert!(!confirmation.account_scope_is_retired());

    std::fs::remove_file(audit_path).unwrap();
    actor.queue.as_ref().unwrap().fail_next_rewrites(1);
    assert_eq!(actor.flush().await, Err(DeliveryError::Saturated));
    let after_rewrite_failure = QueueFile::at(queue_path.clone()).load().entries.remove(0);
    assert!(after_rewrite_failure.open_subsonic_pending);
    assert!(after_rewrite_failure.open_subsonic_handoff_started);
    assert!(after_rewrite_failure.open_subsonic_bridge_durable);
    assert_eq!(
        after_rewrite_failure.pending,
        vec![ServiceKind::ListenBrainz]
    );
    assert!(!confirmation.account_scope_is_retired());

    assert_eq!(actor.flush().await, Ok(()));
    let retired = QueueFile::at(queue_path).load().entries.remove(0);
    assert!(!retired.open_subsonic_pending);
    assert!(!retired.open_subsonic_handoff_started);
    assert!(!retired.open_subsonic_bridge_durable);
    assert_eq!(retired.pending, vec![ServiceKind::ListenBrainz]);
    assert!(confirmation.account_scope_is_retired());
    assert_eq!(
        actor
            .queue
            .as_ref()
            .unwrap()
            .scope_retirement_audit_count()
            .unwrap(),
        1
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn server_submission_three_stage_journal_survives_both_crash_boundaries() {
    let (dir, queue) = temp_queue("open-subsonic-crash-replay");
    let queue_path = queue.path().to_path_buf();
    let (first_events, first_emit) = captured_events();
    let mut producer = test_actor(inactive_settings(), Some(queue), first_emit);
    producer.record_durable_actions(vec![ScrobbleAction::Scrobble(open_subsonic_track(1_000))]);
    assert_eq!(producer.flush().await, Ok(()));
    let event_id = producer.queue.as_ref().unwrap().load().entries[0]
        .id
        .clone();
    assert!(matches!(
        &first_events.lock().unwrap()[0],
        ScrobbleEvent::OpenSubsonic {
            event_id: emitted,
            confirmation: Some(_),
            ..
        } if emitted == &event_id
    ));
    drop(producer);

    // A fresh actor replays the journal marker with the original owner-event identity.
    let (replay_events, replay_emit) = captured_events();
    let (mut replay, mut acknowledgement_rx) = test_actor_with_ack(
        inactive_settings(),
        Some(QueueFile::at(queue_path.clone())),
        replay_emit,
    );
    assert_eq!(replay.flush().await, Ok(()));
    let replay_event = replay_events
        .lock()
        .unwrap()
        .pop()
        .expect("restart emits the pending submission");
    let confirmation = match replay_event {
        ScrobbleEvent::OpenSubsonic {
            event_id: replayed,
            kind: crate::open_subsonic::OpenSubsonicScrobbleKind::Submission,
            confirmation: Some(confirmation),
            ..
        } => {
            assert_eq!(replayed, event_id);
            confirmation
        }
        _ => panic!("restart emitted an unexpected event"),
    };
    assert!(replay.announced_open_subsonic.contains_key(&event_id));

    // Boundary 1: bridge durability is known, but the Pending -> AwaitingSourceAck rewrite fails.
    confirmation.confirm_bridge_durable().unwrap();
    let acknowledgement = acknowledgement_rx
        .try_recv()
        .expect("confirmation reaches the independent acknowledgement lane");
    assert_eq!(acknowledgement.event_id, event_id);
    replay.confirm_open_subsonic_submission(acknowledgement);
    replay.queue.as_ref().unwrap().fail_next_rewrites(1);
    assert_eq!(replay.flush().await, Err(DeliveryError::Saturated));
    let failed = QueueFile::at(queue_path.clone()).load().entries.remove(0);
    assert!(failed.open_subsonic_pending);
    assert!(failed.open_subsonic_handoff_started);
    assert!(!failed.open_subsonic_bridge_durable);
    drop(replay);

    // Restart still emits a real submission phase. A successful retry persists the intermediate
    // state without removing the only source marker.
    let (second_replay_events, second_replay_emit) = captured_events();
    let (mut second_replay, mut second_ack_rx) = test_actor_with_ack(
        inactive_settings(),
        Some(QueueFile::at(queue_path.clone())),
        second_replay_emit,
    );
    assert_eq!(second_replay.flush().await, Ok(()));
    let second_confirmation = match second_replay_events.lock().unwrap().pop().unwrap() {
        ScrobbleEvent::OpenSubsonic {
            event_id: replayed,
            confirmation: Some(confirmation),
            ..
        } => {
            assert_eq!(replayed, event_id);
            confirmation
        }
        _ => panic!("restart emitted an unexpected event"),
    };
    assert!(!second_confirmation.bridge_marker_is_durable());
    second_confirmation.confirm_bridge_durable().unwrap();
    second_replay.confirm_open_subsonic_submission(second_ack_rx.try_recv().unwrap());
    assert_eq!(second_replay.flush().await, Ok(()));
    let intermediate = QueueFile::at(queue_path.clone()).load().entries.remove(0);
    assert!(intermediate.open_subsonic_pending);
    assert!(intermediate.open_subsonic_handoff_started);
    assert!(intermediate.open_subsonic_bridge_durable);
    drop(second_replay);

    // Boundary 2: the bridge source-ack is durable, but final marker removal fails. Restart sees
    // AwaitingSourceAck and therefore must never submit the event again.
    let (third_events, third_emit) = captured_events();
    let (mut third, mut third_ack_rx) = test_actor_with_ack(
        inactive_settings(),
        Some(QueueFile::at(queue_path.clone())),
        third_emit,
    );
    assert_eq!(third.flush().await, Ok(()));
    let third_confirmation = match third_events.lock().unwrap().pop().unwrap() {
        ScrobbleEvent::OpenSubsonic {
            event_id: replayed,
            confirmation: Some(confirmation),
            ..
        } => {
            assert_eq!(replayed, event_id);
            confirmation
        }
        _ => panic!("restart emitted an unexpected event"),
    };
    assert!(third_confirmation.bridge_marker_is_durable());
    third_confirmation.confirm_source_acknowledged().unwrap();
    third.confirm_open_subsonic_submission(third_ack_rx.try_recv().unwrap());
    third.queue.as_ref().unwrap().fail_next_rewrites(1);
    assert_eq!(third.flush().await, Err(DeliveryError::Saturated));
    let still_intermediate = QueueFile::at(queue_path.clone()).load().entries.remove(0);
    assert!(still_intermediate.open_subsonic_pending);
    assert!(still_intermediate.open_subsonic_handoff_started);
    assert!(still_intermediate.open_subsonic_bridge_durable);
    drop(third);

    let (fourth_events, fourth_emit) = captured_events();
    let (mut fourth, mut fourth_ack_rx) = test_actor_with_ack(
        inactive_settings(),
        Some(QueueFile::at(queue_path.clone())),
        fourth_emit,
    );
    assert_eq!(fourth.flush().await, Ok(()));
    let fourth_confirmation = match fourth_events.lock().unwrap().pop().unwrap() {
        ScrobbleEvent::OpenSubsonic {
            confirmation: Some(confirmation),
            ..
        } => confirmation,
        _ => panic!("restart emitted an unexpected event"),
    };
    assert!(fourth_confirmation.bridge_marker_is_durable());
    fourth_confirmation.confirm_source_acknowledged().unwrap();
    fourth.confirm_open_subsonic_submission(fourth_ack_rx.try_recv().unwrap());
    assert_eq!(fourth.flush().await, Ok(()));
    assert!(QueueFile::at(queue_path).load().entries.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn external_service_compaction_preserves_server_marker_until_owner_ack() {
    let (dir, queue) = temp_queue("open-subsonic-service-compaction");
    let marker = QueueEntry::from_track(&open_subsonic_track(1_000), vec![ServiceKind::Lastfm]);
    let event_id = marker.id.clone();
    queue.rewrite(std::slice::from_ref(&marker)).unwrap();
    let mut entries = vec![marker];
    let service = FakeService::new(ServiceKind::Lastfm, Vec::new());
    let mut health = Health::default();
    let (_events, emit) = captured_events();
    let lock = queue.try_lock().expect("test owns queue");

    assert!(
        flush_service(&service, &mut entries, &queue, &lock, &mut health, &emit,)
            .await
            .is_none()
    );
    drop(lock);
    let loaded = queue.load();
    assert_eq!(loaded.entries.len(), 1);
    assert!(loaded.entries[0].pending.is_empty());
    assert!(loaded.entries[0].open_subsonic_pending);

    let (mut actor, mut acknowledgement_rx) =
        test_actor_with_ack(inactive_settings(), Some(queue), emit);
    let confirmation = OpenSubsonicSubmissionAck::new(event_id, actor.open_subsonic_ack_tx.clone());
    confirmation.confirm_bridge_durable().unwrap();
    actor.confirm_open_subsonic_submission(acknowledgement_rx.try_recv().unwrap());
    assert_eq!(actor.flush().await, Ok(()));
    confirmation.confirm_source_acknowledged().unwrap();
    actor.confirm_open_subsonic_submission(acknowledgement_rx.try_recv().unwrap());
    assert_eq!(actor.flush().await, Ok(()));
    assert!(actor.queue.as_ref().unwrap().load().entries.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}
