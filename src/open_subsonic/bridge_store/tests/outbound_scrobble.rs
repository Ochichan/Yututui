use super::*;

#[test]
fn completed_or_echoed_owner_event_replay_is_a_no_op() {
    let mut bridge = bridge();
    let item = ItemId::new("song").unwrap();
    let pending = PendingOutboundScrobble {
        event_id: "stable-owner-event".to_owned(),
        item_id: item.clone(),
        played_at_unix: 42,
        kind: OutboundScrobbleKind::Submission,
        delivery: OutboundScrobbleDelivery::Queued,
        baseline_captured: false,
        baseline_play_count: None,
        baseline_played_at: None,
        exact_credit_recorded: false,
        exact_credit_epoch: None,
        uncertain_readbacks: 0,
        source_marker_acknowledged: false,
    };
    bridge.queue_outbound_scrobble(pending.clone()).unwrap();
    bridge
        .complete_outbound_scrobble(&pending.event_id)
        .unwrap();
    bridge
        .record_outbound_echo(OutboundScrobbleEcho {
            event_id: pending.event_id.clone(),
            item_id: item,
            played_at_unix: 42,
        })
        .unwrap();

    bridge.queue_outbound_scrobble(pending.clone()).unwrap();
    assert!(bridge.outbound_scrobbles().is_empty());
    assert_eq!(bridge.completed_outbound_scrobbles().len(), 1);

    let mut conflicting = pending;
    conflicting.played_at_unix += 1;
    assert_eq!(
        bridge.queue_outbound_scrobble(conflicting),
        Err(BridgeMutationError::ConflictingEntry)
    );
}

#[test]
fn unacknowledged_completion_window_backpressures_and_survives_restart() {
    let mut bridge = bridge();
    let item = ItemId::new("song").unwrap();
    let pending = |index: usize| PendingOutboundScrobble {
        event_id: format!("event-{index}"),
        item_id: item.clone(),
        played_at_unix: index as i64,
        kind: OutboundScrobbleKind::Submission,
        delivery: OutboundScrobbleDelivery::Uncertain,
        baseline_captured: true,
        baseline_play_count: Some(index as u64),
        baseline_played_at: None,
        exact_credit_recorded: true,
        exact_credit_epoch: Some(0),
        uncertain_readbacks: 0,
        source_marker_acknowledged: false,
    };
    for index in 0..MAX_COMPLETED_OUTBOUND_SCROBBLES {
        let event = pending(index);
        bridge.queue_outbound_scrobble(event.clone()).unwrap();
        bridge.complete_outbound_scrobble(&event.event_id).unwrap();
    }

    let mut restarted = decode_bridge(&encode_bridge(&bridge).unwrap()).unwrap();
    let oldest = pending(0);
    restarted.queue_outbound_scrobble(oldest.clone()).unwrap();
    assert!(restarted.outbound_scrobbles().is_empty());
    assert_eq!(
        restarted.completed_outbound_scrobbles().len(),
        MAX_COMPLETED_OUTBOUND_SCROBBLES
    );

    let overflow = pending(MAX_COMPLETED_OUTBOUND_SCROBBLES);
    restarted.queue_outbound_scrobble(overflow.clone()).unwrap();
    assert_eq!(
        restarted.complete_outbound_scrobble(&overflow.event_id),
        Err(BridgeMutationError::CapacityExceeded)
    );
    assert!(
        restarted
            .outbound_scrobbles()
            .iter()
            .any(|event| event.event_id == overflow.event_id)
    );

    restarted
        .acknowledge_outbound_source(&oldest.event_id)
        .unwrap();
    restarted
        .complete_outbound_scrobble(&overflow.event_id)
        .unwrap();
    assert_eq!(
        restarted.completed_outbound_scrobbles().len(),
        MAX_COMPLETED_OUTBOUND_SCROBBLES
    );
}

#[test]
fn outbound_reserve_never_consumes_preexisting_aggregate_evidence() {
    let mut bridge = bridge();
    let item = ItemId::new("song").unwrap();
    assert_eq!(
        bridge
            .record_aggregate_history_evidence(item.clone(), 0, 1)
            .unwrap(),
        1
    );

    bridge
        .reserve_outbound_exact_history_credit(item.clone(), 0)
        .unwrap();
    let credits = bridge
        .history_dedupe_credits()
        .get(&item)
        .unwrap()
        .get(&0)
        .unwrap();
    assert_eq!(credits.aggregate_unmatched, 1);
    assert_eq!(credits.exact_unmatched, 1);

    bridge.discard_exact_history_evidence(&item, 0);
    let credits = bridge
        .history_dedupe_credits()
        .get(&item)
        .unwrap()
        .get(&0)
        .unwrap();
    assert_eq!(credits.aggregate_unmatched, 1);
    assert_eq!(credits.exact_unmatched, 0);
}
