use super::*;

fn generic_entry(index: usize) -> QueueEntry {
    entry(index, vec![ServiceKind::ListenBrainz])
}

fn queue_drop_values(events: &[ScrobbleEvent]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|event| match event {
            ScrobbleEvent::QueueDropped { dropped } => Some(*dropped),
            _ => None,
        })
        .collect()
}

#[test]
fn generic_in_memory_overflow_waits_for_a_durable_loss_audit() {
    let (dir, queue) = temp_queue("generic-append-audit-failure");
    let audit_path = queue.path().with_extension("drops.json");
    std::fs::create_dir_all(audit_path.parent().unwrap()).unwrap();
    std::fs::write(&audit_path, b"{invalid").unwrap();
    let (events, emit) = captured_events();
    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);
    for index in 0..QUEUE_CAP {
        actor.pending_appends.push_back(generic_entry(index));
    }
    actor.pending_generic_appends = QUEUE_CAP;
    let oldest_id = actor.pending_appends.front().unwrap().id.clone();

    actor.enqueue_scrobble(track(9_999));

    assert_eq!(actor.pending_generic_appends, QUEUE_CAP + 1);
    assert_eq!(actor.pending_appends.len(), QUEUE_CAP + 1);
    assert_eq!(actor.pending_appends.front().unwrap().id, oldest_id);
    assert!(!actor.accepts_command_ingress());
    assert!(queue_drop_values(&events.lock().unwrap()).is_empty());

    std::fs::remove_file(audit_path).unwrap();
    actor.queue.as_ref().unwrap().fail_next_appends(1);
    assert_eq!(actor.retry_pending_appends(), Err(DeliveryError::Saturated));

    assert_eq!(actor.pending_generic_appends, QUEUE_CAP);
    assert_eq!(actor.pending_appends.len(), QUEUE_CAP);
    assert_ne!(actor.pending_appends.front().unwrap().id, oldest_id);
    assert!(actor.accepts_command_ingress());
    assert_eq!(queue_drop_values(&events.lock().unwrap()), vec![1]);
    assert_eq!(
        actor.queue.as_ref().unwrap().drop_audit_counts().unwrap(),
        (1, 0, 1)
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn generic_in_memory_cap_preserves_the_same_entrys_exact_marker() {
    let (dir, queue) = temp_queue("generic-cap-preserves-exact");
    queue.fail_next_appends(1);
    let (events, emit) = captured_events();
    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);
    let mut mixed =
        QueueEntry::from_track(&open_subsonic_track(1_000), vec![ServiceKind::ListenBrainz]);
    mixed.id = "oldest-mixed-marker".to_owned();
    actor.pending_appends.push_back(mixed);
    for index in 1..QUEUE_CAP {
        actor.pending_appends.push_back(generic_entry(index));
    }
    actor.pending_generic_appends = QUEUE_CAP;
    actor.pending_open_subsonic_appends = 1;

    actor.enqueue_scrobble(track(9_999));

    let retained = actor
        .pending_appends
        .iter()
        .find(|entry| entry.id == "oldest-mixed-marker")
        .unwrap();
    assert!(retained.pending.is_empty());
    assert!(retained.open_subsonic_pending);
    assert_eq!(actor.pending_generic_appends, QUEUE_CAP);
    assert_eq!(actor.pending_open_subsonic_appends, 1);
    assert_eq!(queue_drop_values(&events.lock().unwrap()), vec![1]);
    assert_eq!(
        actor.queue.as_ref().unwrap().drop_audit_counts().unwrap(),
        (1, 0, 1)
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn exact_in_memory_cap_preserves_the_same_entrys_generic_marker() {
    let (dir, queue) = temp_queue("exact-cap-preserves-generic");
    queue.fail_next_appends(1);
    let (events, emit) = captured_events();
    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);
    for index in 0..OPEN_SUBSONIC_QUEUE_CAP {
        let mut pending =
            QueueEntry::from_track(&open_subsonic_track(1_000 + index as i64), Vec::new());
        pending.id = format!("exact-pending-{index}");
        actor.pending_appends.push_back(pending);
    }
    actor.pending_open_subsonic_appends = OPEN_SUBSONIC_QUEUE_CAP;

    actor.enqueue_scrobble(open_subsonic_track(30_000));

    let retained = actor.pending_appends.back().unwrap();
    assert!(!retained.pending.is_empty());
    assert!(!retained.open_subsonic_pending);
    assert_eq!(actor.pending_generic_appends, 1);
    assert_eq!(actor.pending_open_subsonic_appends, OPEN_SUBSONIC_QUEUE_CAP);
    assert_eq!(queue_drop_values(&events.lock().unwrap()), vec![1]);
    assert_eq!(
        actor.queue.as_ref().unwrap().drop_audit_counts().unwrap(),
        (0, 1, 1)
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn pre_replace_cap_failure_notifies_once_after_confirmed_retry() {
    assert_cap_rewrite_notification(false).await;
}

#[tokio::test]
async fn post_replace_cap_ambiguity_notifies_once_after_readback() {
    assert_cap_rewrite_notification(true).await;
}

async fn assert_cap_rewrite_notification(after_replace: bool) {
    let label = if after_replace {
        "cap-notification-post-replace"
    } else {
        "cap-notification-pre-replace"
    };
    let (dir, queue) = temp_queue(label);
    let entries = (0..=QUEUE_CAP).map(generic_entry).collect::<Vec<_>>();
    queue.rewrite(&entries).unwrap();
    if after_replace {
        queue.fail_next_rewrites_after_replace(1);
    } else {
        queue.fail_next_rewrites(1);
    }
    let (events, emit) = captured_events();
    let mut actor = test_actor(inactive_settings(), Some(queue), emit);

    assert_eq!(actor.flush().await, Err(DeliveryError::Saturated));
    assert!(queue_drop_values(&events.lock().unwrap()).is_empty());
    assert!(actor.pending_queue_drop_outcome.is_some());
    assert_eq!(
        actor.queue.as_ref().unwrap().load().entries.len(),
        if after_replace {
            QUEUE_CAP
        } else {
            QUEUE_CAP + 1
        }
    );

    assert_eq!(actor.flush().await, Ok(()));
    assert_eq!(queue_drop_values(&events.lock().unwrap()), vec![1]);
    assert!(actor.pending_queue_drop_outcome.is_none());
    assert_eq!(
        actor.queue.as_ref().unwrap().load().entries.len(),
        QUEUE_CAP
    );

    assert_eq!(actor.flush().await, Ok(()));
    assert_eq!(queue_drop_values(&events.lock().unwrap()), vec![1]);

    actor
        .queue
        .as_ref()
        .unwrap()
        .append(&generic_entry(QUEUE_CAP + 2))
        .unwrap();
    assert_eq!(actor.flush().await, Ok(()));
    assert_eq!(queue_drop_values(&events.lock().unwrap()), vec![1, 2]);
    assert_eq!(actor.dropped_total, 2);
    let _ = std::fs::remove_dir_all(dir);
}
