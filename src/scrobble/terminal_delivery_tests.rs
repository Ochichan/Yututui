use super::*;

fn track(duration: Option<f64>, liked: bool) -> ObservedTrack {
    ObservedTrack {
        key: "repeat-a".to_owned(),
        title: "Repeat A".to_owned(),
        artist: "Artist".to_owned(),
        album: None,
        duration,
        is_live: false,
        is_local: false,
        origin_url: None,
        liked,
    }
}

fn paused_snapshot(duration: Option<f64>, artist: &str, epoch: u64) -> crate::media::MediaSnapshot {
    let mut snapshot = crate::media::MediaSnapshot::idle();
    snapshot.status = crate::media::MediaPlaybackStatus::Paused;
    snapshot.position = 20.0;
    snapshot.position_epoch = epoch;
    snapshot.track = Some(crate::media::MediaTrack {
        key: "repeat-a".to_owned(),
        title: "Repeat A".to_owned(),
        artist: artist.to_owned(),
        album: None,
        duration,
        is_live: false,
        url: None,
        art_remote_url: None,
        art_file: None,
        art_query: None,
        liked: false,
        disliked: false,
    });
    snapshot
}

fn apply_batch(
    monitor: &mut monitor::ScrobbleMonitor,
    batch: ObservationBatch,
) -> Vec<monitor::ScrobbleAction> {
    let (first, tail) = batch.into_parts();
    let mut actions = monitor.observe(&first, false);
    if let Some((latest, preserved_credit)) = tail {
        actions.extend(monitor.observe_with_preserved_credit(&latest, false, preserved_credit));
    }
    actions
}

#[test]
fn terminal_fifo_preserves_same_track_restart_rearm_pivot() {
    let started = Instant::now();
    let observation = |seconds: u64, playing: bool, epoch: u64, position: f64| Observation {
        track: Some(track(Some(40.0), false)),
        playing,
        stopped: false,
        position,
        position_epoch: epoch,
        rate: 1.0,
        at: started + std::time::Duration::from_secs(seconds),
        wall_unix: 1_751_400_000 + seconds as i64,
    };
    let mut monitor = monitor::ScrobbleMonitor::new();
    let mut first_listen = Vec::new();
    for seconds in [0, 5, 10, 15, 20] {
        first_listen.extend(monitor.observe(&observation(seconds, true, 1, seconds as f64), false));
    }
    assert_eq!(scrobble_count(&first_listen), 1);

    let mut pending = PendingState::default();
    for edge in [
        observation(20, false, 1, 20.0),
        observation(21, false, 2, 0.0),
        observation(22, false, 3, 10.0),
    ] {
        assert!(
            stage_terminal_observation(&mut pending, Box::new(ObservationBatch::single(edge)))
                .is_ok()
        );
    }
    assert_eq!(pending.terminal_observations.len(), 3);
    while let Some(batch) = pending.terminal_observations.pop_front() {
        let _ = apply_batch(&mut monitor, *batch);
    }

    let mut second_listen = Vec::new();
    for elapsed in [0, 5, 10, 15, 20] {
        second_listen.extend(monitor.observe(
            &observation(22 + elapsed, true, 3, 10.0 + elapsed as f64),
            false,
        ));
    }
    assert_eq!(
        scrobble_count(&second_listen),
        1,
        "the at-zero restart must re-arm an already-scrobbled listen"
    );
}

#[test]
fn terminal_fifo_is_bounded_and_never_overwrites_an_accepted_edge() {
    let started = Instant::now();
    let paused = |epoch: u64, position: f64| Observation {
        track: Some(track(Some(40.0), false)),
        playing: false,
        stopped: false,
        position,
        position_epoch: epoch,
        rate: 1.0,
        at: started + std::time::Duration::from_secs(epoch),
        wall_unix: 1_751_400_000 + epoch as i64,
    };
    let mut pending = PendingState::default();
    for epoch in 0..PENDING_TERMINAL_CAPACITY as u64 {
        assert_eq!(
            stage_terminal_observation(
                &mut pending,
                Box::new(ObservationBatch::single(paused(epoch, epoch as f64)))
            ),
            Ok(false)
        );
    }
    assert_eq!(
        stage_terminal_observation(
            &mut pending,
            Box::new(ObservationBatch::single(paused(999, 999.0)))
        ),
        Err(DeliveryError::Busy)
    );
    assert_eq!(
        pending.terminal_observations.len(),
        PENDING_TERMINAL_CAPACITY
    );
    assert_eq!(
        pending
            .terminal_observations
            .front()
            .unwrap()
            .latest()
            .position_epoch,
        0
    );
    assert_eq!(
        pending
            .terminal_observations
            .back()
            .unwrap()
            .latest()
            .position_epoch,
        PENDING_TERMINAL_CAPACITY as u64 - 1
    );

    let tail_epoch = PENDING_TERMINAL_CAPACITY as u64 - 1;
    assert_eq!(
        stage_terminal_observation(
            &mut pending,
            Box::new(ObservationBatch::single(paused(tail_epoch, 321.0)))
        ),
        Ok(true),
        "a compatible steady-state tail may still coalesce at capacity"
    );
    assert_eq!(
        pending
            .terminal_observations
            .back()
            .unwrap()
            .latest()
            .position,
        321.0
    );
}

#[test]
fn eligibility_metadata_changes_are_semantic_edges_not_heartbeats() {
    let started = Instant::now();
    let paused = |duration, seconds| Observation {
        track: Some(track(duration, false)),
        playing: false,
        stopped: false,
        position: 20.0,
        position_epoch: 7,
        rate: 1.0,
        at: started + std::time::Duration::from_secs(seconds),
        wall_unix: 1_751_400_000 + seconds as i64,
    };
    let mut pending = PendingState::default();
    for edge in [paused(None, 0), paused(Some(40.0), 1), paused(None, 2)] {
        assert!(
            stage_terminal_observation(&mut pending, Box::new(ObservationBatch::single(edge)))
                .is_ok()
        );
    }
    assert_eq!(
        pending.terminal_observations.len(),
        3,
        "transient eligibility must reach the monitor instead of disappearing in first/latest"
    );
}

#[test]
fn paused_love_toggles_reach_the_monitor_in_order() {
    let started = Instant::now();
    let paused = |liked, seconds| Observation {
        track: Some(track(Some(40.0), liked)),
        playing: false,
        stopped: false,
        position: 10.0,
        position_epoch: 1,
        rate: 1.0,
        at: started + std::time::Duration::from_secs(seconds),
        wall_unix: 1_751_400_000 + seconds as i64,
    };
    let mut monitor = monitor::ScrobbleMonitor::new();
    let _ = monitor.observe(&paused(false, 0), false);
    let mut pending = PendingState::default();
    for edge in [paused(true, 1), paused(false, 2), paused(true, 3)] {
        assert!(
            stage_terminal_observation(&mut pending, Box::new(ObservationBatch::single(edge)))
                .is_ok()
        );
    }
    let mut love = Vec::new();
    while let Some(batch) = pending.terminal_observations.pop_front() {
        love.extend(apply_batch(&mut monitor, *batch).into_iter().filter_map(
            |action| match action {
                monitor::ScrobbleAction::Love { love, .. } => Some(love),
                _ => None,
            },
        ));
    }
    assert_eq!(love, vec![true, false, true]);
}

#[test]
fn paused_metadata_change_passes_the_public_handle_rate_gate() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let (shutdown_tx, _shutdown_rx) = tokio::sync::mpsc::channel(1);
    let mut handle = ScrobbleHandle::new(tx, shutdown_tx);
    assert_eq!(
        handle.observe(&paused_snapshot(Some(29.0), "", 7)),
        Ok(DeliveryReceipt::Enqueued)
    );
    assert!(matches!(rx.try_recv(), Ok(ScrobbleCmd::Observe(_))));

    assert_eq!(
        handle.observe(&paused_snapshot(Some(40.0), "Artist", 7)),
        Ok(DeliveryReceipt::Enqueued),
        "duration and artist eligibility changes must not look like a duplicate pause"
    );
    let Ok(ScrobbleCmd::Observe(batch)) = rx.try_recv() else {
        panic!("updated paused metadata must reach the actor")
    };
    let updated = batch.latest().track.as_ref().unwrap();
    assert_eq!(updated.duration, Some(40.0));
    assert_eq!(updated.artist, "Artist");
}

#[test]
fn public_handle_marks_terminal_overflow_for_heartbeat_independent_retry() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let (shutdown_tx, _shutdown_rx) = tokio::sync::mpsc::channel(1);
    let mut handle = ScrobbleHandle::new(tx, shutdown_tx);
    {
        let mut state = lock_pending(&handle.pending.state);
        state.drainer_running = true;
        for epoch in 0..PENDING_TERMINAL_CAPACITY as u64 {
            state
                .terminal_observations
                .push_back(Box::new(ObservationBatch::single(Observation::from_media(
                    &paused_snapshot(Some(40.0), "Artist", epoch),
                ))));
        }
    }

    let final_pause = paused_snapshot(Some(40.0), "Artist", 999);
    assert_eq!(handle.observe(&final_pause), Err(DeliveryError::Busy));
    assert!(handle.retry_needed());
    assert!(handle.last_fingerprint.is_none());
    assert_eq!(
        lock_pending(&handle.pending.state)
            .terminal_retry
            .as_ref()
            .map(|batch| batch.latest().position_epoch),
        Some(999),
        "the actor-shared retry slot owns the rejected terminal edge"
    );

    let latest_pause = paused_snapshot(Some(40.0), "Artist", 1_000);
    assert_eq!(handle.observe(&latest_pause), Err(DeliveryError::Busy));
    assert_eq!(
        lock_pending(&handle.pending.state)
            .terminal_retry
            .as_ref()
            .map(|batch| batch.latest().position_epoch),
        Some(1_000),
        "the bounded retry slot retains only the latest rejected terminal snapshot"
    );

    lock_pending(&handle.pending.state)
        .terminal_observations
        .pop_front();
    assert_eq!(handle.observe(&latest_pause), Ok(DeliveryReceipt::Deferred));
    assert!(!handle.retry_needed());
    assert!(
        lock_pending(&handle.pending.state).terminal_retry.is_none(),
        "a successful explicit retry supersedes the shutdown-only copy"
    );
    assert_eq!(
        handle.last_fingerprint.as_ref().map(|value| value.epoch),
        Some(1_000)
    );
}

fn scrobble_count(actions: &[monitor::ScrobbleAction]) -> usize {
    actions
        .iter()
        .filter(|action| matches!(action, monitor::ScrobbleAction::Scrobble(_)))
        .count()
}
