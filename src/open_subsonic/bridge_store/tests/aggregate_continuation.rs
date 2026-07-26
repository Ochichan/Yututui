use super::*;
use crate::open_subsonic::history::AggregatePlayRange;
use crate::open_subsonic::model::OpenSubsonicItemRef;

fn range(first: u64, last: u64, counter_epoch: u64) -> PendingAggregateRange {
    PendingAggregateRange {
        track: portable("song"),
        artist_key: "artist".to_owned(),
        counter_epoch,
        range: AggregatePlayRange {
            item: OpenSubsonicItemRef::new(
                BackendId::new("backend").unwrap(),
                AccountScopeId::new("account").unwrap(),
                ItemId::new("song").unwrap(),
            ),
            first_ordinal: first,
            last_ordinal: last,
            played_at_unix: 42,
            has_server_time: true,
            has_server_ordinal: true,
        },
    }
}

fn bridge_with_shadow(play_count: u64, counter_epoch: u64) -> OpenSubsonicBridgeState {
    let mut state = bridge();
    state
        .upsert_aggregate_play_shadow(
            ItemId::new("song").unwrap(),
            AggregatePlayShadow {
                play_count,
                played_at: Some("1970-01-01T00:00:42Z".to_owned()),
                observed_at_unix: 42,
                counter_epoch,
            },
        )
        .unwrap();
    state
}

#[test]
fn twenty_thousand_plus_one_is_compact_durable_and_refills_after_ack() {
    let mut state = bridge_with_shadow(20_001, 0);
    state.queue_aggregate_range(range(1, 20_001, 0)).unwrap();
    state.materialize_pending_aggregate_ranges().unwrap();

    assert_eq!(state.pending_engagement_imports().len(), 20_000);
    let tail = state.pending_aggregate_ranges().front().unwrap();
    assert_eq!(tail.range.first_ordinal, 20_001);
    assert_eq!(tail.range.last_ordinal, 20_001);

    let encoded = encode_bridge(&state).unwrap();
    assert!((encoded.len() as u64) < MAX_BRIDGE_BYTES);
    let mut restarted = decode_bridge(&encoded).unwrap();
    assert_eq!(restarted, state);
    let mut frozen = restarted.clone();
    let item_id = ItemId::new("song").unwrap();
    frozen
        .reserve_outbound_exact_history_credit(item_id.clone(), 0)
        .unwrap();
    frozen
        .queue_outbound_scrobble(PendingOutboundScrobble {
            event_id: "uncertain".to_owned(),
            item_id: item_id.clone(),
            played_at_unix: 42,
            kind: OutboundScrobbleKind::Submission,
            delivery: OutboundScrobbleDelivery::Uncertain,
            baseline_captured: true,
            baseline_play_count: Some(20_000),
            baseline_played_at: Some("1970-01-01T00:00:42Z".to_owned()),
            exact_credit_recorded: true,
            exact_credit_epoch: Some(0),
            uncertain_readbacks: 0,
            source_marker_acknowledged: false,
        })
        .unwrap();
    let frozen_ack = frozen
        .pending_engagement_imports()
        .keys()
        .next()
        .cloned()
        .unwrap();
    frozen.remove_engagement_import(&frozen_ack);
    frozen.materialize_pending_aggregate_ranges().unwrap();
    assert_eq!(frozen.pending_engagement_imports().len(), 19_999);
    assert_eq!(
        frozen
            .pending_aggregate_ranges()
            .front()
            .unwrap()
            .range
            .first_ordinal,
        20_001
    );

    let acknowledged = restarted
        .pending_engagement_imports()
        .keys()
        .next()
        .cloned()
        .unwrap();
    restarted.remove_engagement_import(&acknowledged);
    restarted.materialize_pending_aggregate_ranges().unwrap();
    assert_eq!(restarted.pending_engagement_imports().len(), 20_000);
    assert!(restarted.pending_aggregate_ranges().is_empty());
    assert_eq!(
        restarted
            .history_dedupe_credits()
            .get(&ItemId::new("song").unwrap())
            .unwrap()
            .get(&0)
            .unwrap()
            .aggregate_unmatched,
        20_001
    );
}

#[test]
fn corrupt_ranges_and_payloads_over_sixteen_mib_are_rejected() {
    let mut state = bridge_with_shadow(20_001, 0);
    state.queue_aggregate_range(range(1, 20_001, 0)).unwrap();
    state.materialize_pending_aggregate_ranges().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&encode_bridge(&state).unwrap()).unwrap();

    value["pending_aggregate_ranges"][0]["counter_epoch"] = serde_json::Value::from(1);
    assert_eq!(
        decode_bridge(&serde_json::to_vec(&value).unwrap()),
        Err(StoreError::InvalidState)
    );

    let oversized = vec![b' '; usize::try_from(MAX_BRIDGE_BYTES).unwrap() + 1];
    assert_eq!(decode_bridge(&oversized), Err(StoreError::PayloadTooLarge));
}
