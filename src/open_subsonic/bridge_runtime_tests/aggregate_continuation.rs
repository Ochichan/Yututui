use super::*;
use crate::open_subsonic::bridge_store::{
    MAX_UNCERTAIN_SCROBBLE_READBACKS, PendingOutboundScrobble,
};

fn mark_speculative_exact(
    store_set: &mut OpenSubsonicStoreSet,
    event_id: &str,
    delivery: OutboundScrobbleDelivery,
) {
    let mut pending: PendingOutboundScrobble = store_set
        .bridge_state
        .outbound_scrobbles()
        .iter()
        .find(|pending| pending.event_id == event_id)
        .cloned()
        .unwrap();
    let epoch = store_set
        .bridge_state
        .aggregate_play_shadows()
        .get(&pending.item_id)
        .unwrap()
        .counter_epoch;
    store_set
        .bridge_state
        .reserve_outbound_exact_history_credit(pending.item_id.clone(), epoch)
        .unwrap();
    pending.delivery = delivery;
    pending.exact_credit_recorded = true;
    pending.exact_credit_epoch = Some(epoch);
    pending.uncertain_readbacks = if delivery == OutboundScrobbleDelivery::NeedsAttention {
        MAX_UNCERTAIN_SCROBBLE_READBACKS
    } else {
        0
    };
    store_set
        .bridge_state
        .replace_outbound_scrobble(pending)
        .unwrap();
}

#[tokio::test]
async fn baseline_ten_to_one_hundred_eleven_is_lossless_and_retry_stable() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let baseline = song(&store_set, 5, 10);
    runtime.observe_songs(&mut store_set, &[baseline]).unwrap();

    let growth = song(&store_set, 5, 111);
    runtime
        .observe_songs(&mut store_set, std::slice::from_ref(&growth))
        .unwrap();
    let ids = store_set
        .bridge_state
        .pending_engagement_imports()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 101);
    assert!(store_set.bridge_state.pending_aggregate_ranges().is_empty());

    runtime
        .observe_songs(&mut store_set, std::slice::from_ref(&growth))
        .unwrap();
    assert_eq!(
        store_set
            .bridge_state
            .pending_engagement_imports()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ids
    );
    let mut restarted = load_store_set(&paths).unwrap().unwrap();
    runtime.observe_songs(&mut restarted, &[growth]).unwrap();
    assert_eq!(
        restarted
            .bridge_state
            .pending_engagement_imports()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ids
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn twenty_thousand_boundary_and_acknowledged_tail_survive_restart() {
    let (root, _paths, mut exact_capacity, _client, runtime) = fixture(9, None).await;
    let baseline = song(&exact_capacity, 5, 0);
    runtime
        .observe_songs(&mut exact_capacity, &[baseline])
        .unwrap();
    let growth = song(&exact_capacity, 5, 20_000);
    runtime
        .observe_songs(&mut exact_capacity, &[growth])
        .unwrap();
    assert_eq!(
        exact_capacity
            .bridge_state
            .pending_engagement_imports()
            .len(),
        20_000
    );
    assert!(
        exact_capacity
            .bridge_state
            .pending_aggregate_ranges()
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(root);

    let (tail_root, paths, mut with_tail, _client, tail_runtime) = fixture(9, None).await;
    let baseline = song(&with_tail, 5, 0);
    tail_runtime
        .observe_songs(&mut with_tail, &[baseline])
        .unwrap();
    let growth = song(&with_tail, 5, 20_001);
    tail_runtime
        .observe_songs(&mut with_tail, &[growth])
        .unwrap();
    assert_eq!(
        with_tail.bridge_state.pending_engagement_imports().len(),
        20_000
    );
    assert_eq!(
        with_tail
            .bridge_state
            .pending_aggregate_ranges()
            .front()
            .unwrap()
            .range
            .first_ordinal,
        20_001
    );

    let mut restarted = load_store_set(&paths).unwrap().unwrap();
    let acknowledged = restarted
        .bridge_state
        .pending_engagement_imports()
        .keys()
        .next()
        .cloned()
        .unwrap();
    tail_runtime
        .acknowledge_import(&mut restarted, &acknowledged)
        .unwrap();
    assert_eq!(
        restarted.bridge_state.pending_engagement_imports().len(),
        20_000
    );
    assert!(restarted.bridge_state.pending_aggregate_ranges().is_empty());
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(durable.bridge_state, restarted.bridge_state);
    let _ = std::fs::remove_dir_all(tail_root);
}

#[tokio::test]
async fn exact_first_and_reset_generations_do_not_consume_each_other() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let baseline = song(&store_set, 5, 0);
    runtime.observe_songs(&mut store_set, &[baseline]).unwrap();
    let item_id = ItemId::new("song-1").unwrap();
    assert!(
        store_set
            .bridge_state
            .record_exact_history_evidence(item_id.clone(), 0)
            .unwrap()
    );
    let growth = song(&store_set, 5, 20_002);
    runtime.observe_songs(&mut store_set, &[growth]).unwrap();
    assert_eq!(
        store_set.bridge_state.pending_engagement_imports().len(),
        20_000
    );
    assert_eq!(
        store_set
            .bridge_state
            .pending_aggregate_ranges()
            .front()
            .unwrap()
            .range
            .first_ordinal,
        20_002,
        "the epoch-zero exact credit suppresses only the first logical ordinal"
    );

    let mut reset = song(&store_set, 5, 0);
    reset.played_at = Some("2026-07-26T00:01:00Z".to_owned());
    runtime.observe_songs(&mut store_set, &[reset]).unwrap();
    assert_eq!(
        store_set
            .bridge_state
            .aggregate_play_shadows()
            .get(&item_id)
            .unwrap()
            .counter_epoch,
        1
    );
    assert!(
        store_set
            .bridge_state
            .record_exact_history_evidence(item_id.clone(), 1)
            .unwrap()
    );

    let acknowledged = store_set
        .bridge_state
        .pending_engagement_imports()
        .keys()
        .next()
        .cloned()
        .unwrap();
    runtime
        .acknowledge_import(&mut store_set, &acknowledged)
        .unwrap();
    assert!(store_set.bridge_state.pending_aggregate_ranges().is_empty());
    let epochs = store_set
        .bridge_state
        .history_dedupe_credits()
        .get(&item_id)
        .unwrap();
    assert_eq!(epochs.get(&0).unwrap().aggregate_unmatched, 20_001);
    assert_eq!(epochs.get(&1).unwrap().exact_unmatched, 1);

    let mut post_reset = song(&store_set, 5, 1);
    post_reset.played_at = Some("2026-07-26T00:02:00Z".to_owned());
    runtime
        .observe_songs(&mut store_set, &[post_reset])
        .unwrap();
    assert!(store_set.bridge_state.pending_aggregate_ranges().is_empty());
    let epochs = store_set
        .bridge_state
        .history_dedupe_credits()
        .get(&item_id)
        .unwrap();
    assert_eq!(epochs.get(&0).unwrap().aggregate_unmatched, 20_001);
    assert!(epochs.get(&1).is_none());
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .history_dedupe_credits(),
        store_set.bridge_state.history_dedupe_credits()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn ambiguous_aggregate_observation_freezes_until_retry_or_mark_sent() {
    for resolution in [
        OutboundScrobbleResolution::Retry,
        OutboundScrobbleResolution::MarkSent,
    ] {
        let (root, _paths, mut store_set, _client, runtime) = fixture(9, None).await;
        let baseline = song(&store_set, 5, 10);
        runtime.observe_songs(&mut store_set, &[baseline]).unwrap();
        let track = scrobble_track(&store_set);
        runtime
            .queue_scrobble(
                &mut store_set,
                "freeze-owner-event",
                OpenSubsonicScrobbleKind::Submission,
                track,
            )
            .unwrap();
        let event_id = store_set
            .bridge_state
            .outbound_scrobbles()
            .front()
            .unwrap()
            .event_id
            .clone();
        mark_speculative_exact(
            &mut store_set,
            &event_id,
            OutboundScrobbleDelivery::NeedsAttention,
        );

        let mobile_growth = song(&store_set, 1, 12);
        runtime
            .observe_songs(&mut store_set, std::slice::from_ref(&mobile_growth))
            .unwrap();
        assert_eq!(
            store_set
                .bridge_state
                .aggregate_play_shadows()
                .get(mobile_growth.item.item_id())
                .unwrap()
                .play_count,
            10
        );
        assert!(
            store_set
                .bridge_state
                .pending_engagement_imports()
                .is_empty()
        );
        assert!(store_set.bridge_state.pending_aggregate_ranges().is_empty());
        assert_eq!(
            store_set
                .bridge_state
                .rating_shadow(mobile_growth.item.item_id())
                .unwrap()
                .raw
                .user_rating,
            Some(1),
            "rating observations stay live while aggregate history is frozen"
        );

        runtime
            .resolve_outbound_scrobble(&mut store_set, &event_id, resolution)
            .unwrap();
        runtime
            .observe_songs(&mut store_set, &[mobile_growth])
            .unwrap();
        assert_eq!(
            store_set
                .bridge_state
                .aggregate_play_shadows()
                .get(&ItemId::new("song-1").unwrap())
                .unwrap()
                .play_count,
            12
        );
        let expected_imports = if resolution == OutboundScrobbleResolution::Retry {
            2
        } else {
            1
        };
        assert_eq!(
            store_set.bridge_state.pending_engagement_imports().len(),
            expected_imports,
            "mark-sent reserves one aggregate increment for the local event"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn native_aggregate_baseline_stays_frozen_until_exact_echo_resolves_uncertainty() {
    let (root, _paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let initial = song(&store_set, 5, 10);
    runtime.observe_songs(&mut store_set, &[initial]).unwrap();
    let track = scrobble_track(&store_set);
    runtime
        .queue_scrobble(
            &mut store_set,
            "native-freeze-owner-event",
            OpenSubsonicScrobbleKind::Submission,
            track.clone(),
        )
        .unwrap();
    let event_id = store_set
        .bridge_state
        .outbound_scrobbles()
        .front()
        .unwrap()
        .event_id
        .clone();
    mark_speculative_exact(
        &mut store_set,
        &event_id,
        OutboundScrobbleDelivery::Uncertain,
    );
    let item_id = ItemId::new("song-1").unwrap();
    let backend_id = store_set.profile.backend_id().clone();
    let account_scope_id = store_set.profile.account_scope_id().clone();
    let baseline = |rows| HistoryRefreshResult {
        backend_id: backend_id.clone(),
        account_scope_id: account_scope_id.clone(),
        base_cursor: None,
        native: Ok(Some(NativeHistoryBatch {
            rows,
            aggregate_baselines: std::iter::once((
                item_id.clone(),
                (11, Some("2026-07-26T00:01:00Z".to_owned())),
            ))
            .collect(),
            next_cursor: None,
            truncated: false,
            metadata_retry_pending: false,
        })),
        standard: Ok(Vec::new()),
    };

    runtime
        .apply_history_refresh(&mut store_set, baseline(Vec::new()))
        .unwrap();
    assert_eq!(
        store_set
            .bridge_state
            .aggregate_play_shadows()
            .get(&item_id)
            .unwrap()
            .play_count,
        10
    );

    let exact_track = portable_server_track(&song(&store_set, 5, 11));
    runtime
        .apply_history_refresh(
            &mut store_set,
            baseline(vec![NativeHistoryObservation {
                row_id: 9001,
                item_id: item_id.clone(),
                track: exact_track,
                observed_at_unix: track.started_unix,
            }]),
        )
        .unwrap();
    assert!(
        !store_set
            .bridge_state
            .has_unresolved_outbound_submission(&item_id)
    );
    assert_eq!(
        store_set
            .bridge_state
            .aggregate_play_shadows()
            .get(&item_id)
            .unwrap()
            .play_count,
        11
    );
    let _ = std::fs::remove_dir_all(root);
}
