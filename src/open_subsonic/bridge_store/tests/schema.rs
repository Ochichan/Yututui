use super::*;

#[test]
fn bridge_round_trip_preserves_every_queue_and_invalid_raw_rating() {
    let mut bridge = bridge();
    bridge.set_native_history_health(NativeHistoryHealth::Detailed);
    let item = ItemId::new("song").unwrap();
    bridge
        .upsert_rating_shadow(item.clone(), shadow(Some(99), true, 10))
        .unwrap();
    bridge
        .queue_rating_projection(
            item.clone(),
            PendingRatingProjection {
                operation_id: "local-op".to_owned(),
                target: Rating::Disliked,
                stage: PendingRatingProjectionStage::SetStarred,
                last_readback: Some(RawServerRating {
                    user_rating: Some(99),
                    starred: true,
                }),
                queued_at_unix: 11,
            },
        )
        .unwrap();
    bridge
        .queue_rating_import(
            "server-observation".to_owned(),
            PendingRatingImport {
                item_id: item.clone(),
                track: portable("song"),
                raw: RawServerRating {
                    user_rating: Some(-9),
                    starred: false,
                },
                mapped: Rating::Neutral,
                observed_at_unix: 12,
            },
        )
        .unwrap();
    bridge
        .queue_engagement_import("history-row".to_owned(), engagement("song"))
        .unwrap();
    bridge
        .upsert_aggregate_play_shadow(
            item.clone(),
            AggregatePlayShadow {
                play_count: 7,
                played_at: Some("2026-07-26T10:00:00Z".to_owned()),
                observed_at_unix: 13,
                counter_epoch: 2,
            },
        )
        .unwrap();
    bridge
        .set_history_cursor(
            "navidrome-native".to_owned(),
            HistoryCursor {
                high_water_id: Some("row-7".to_owned()),
                overlap_started_at_unix: Some(14),
                updated_at_unix: 15,
                continuation: None,
                pending_metadata_rows: Vec::new(),
            },
        )
        .unwrap();
    bridge
        .queue_outbound_scrobble(PendingOutboundScrobble {
            event_id: "engagement-1".to_owned(),
            item_id: item,
            played_at_unix: 16,
            kind: OutboundScrobbleKind::Submission,
            delivery: OutboundScrobbleDelivery::Queued,
            baseline_captured: false,
            baseline_play_count: None,
            baseline_played_at: None,
            exact_credit_recorded: false,
            exact_credit_epoch: None,
            uncertain_readbacks: 0,
            source_marker_acknowledged: false,
        })
        .unwrap();
    bridge
        .record_outbound_echo(OutboundScrobbleEcho {
            event_id: "echo-1".to_owned(),
            item_id: ItemId::new("song").unwrap(),
            played_at_unix: 17,
        })
        .unwrap();

    let decoded = decode_bridge(&encode_bridge(&bridge).unwrap()).unwrap();
    assert_eq!(decoded, bridge);
    assert_eq!(
        decoded
            .rating_shadow(&ItemId::new("song").unwrap())
            .unwrap()
            .raw
            .user_rating,
        Some(99)
    );
}

#[test]
fn schema_one_empty_bridge_migrates_explicitly_to_current_schema() {
    let legacy = br#"{
        "kind":"yututui_open_subsonic_bridge",
        "schema_version":1,
        "revision":8,
        "backend_id":"backend",
        "account_scope_id":"account"
    }"#;
    let migrated = decode_bridge(legacy).unwrap();
    assert_eq!(migrated.revision(), 8);
    assert!(migrated.rating_shadows().is_empty());
    assert!(migrated.pending_rating_projections().is_empty());
    assert!(migrated.pending_rating_imports().is_empty());
    assert!(migrated.pending_engagement_imports().is_empty());
    assert!(migrated.aggregate_play_shadows().is_empty());
    assert!(migrated.history_dedupe_credits().is_empty());
    assert!(migrated.history_cursors().is_empty());
    assert!(migrated.outbound_scrobbles().is_empty());
    assert!(migrated.outbound_echoes().is_empty());
    assert!(migrated.completed_outbound_scrobbles().is_empty());
    assert_eq!(migrated.native_history_health(), NativeHistoryHealth::Off);
    let encoded: serde_json::Value =
        serde_json::from_slice(&encode_bridge(&migrated).unwrap()).unwrap();
    assert_eq!(
        encoded["schema_version"],
        serde_json::Value::from(BRIDGE_SCHEMA_VERSION)
    );
}

#[test]
fn aggregate_shadow_without_counter_epoch_defaults_to_legacy_ids() {
    let mut state = bridge();
    state
        .upsert_aggregate_play_shadow(
            ItemId::new("song").unwrap(),
            AggregatePlayShadow {
                play_count: 4,
                played_at: None,
                observed_at_unix: 12,
                counter_epoch: 3,
            },
        )
        .unwrap();
    let mut encoded: serde_json::Value =
        serde_json::from_slice(&encode_bridge(&state).unwrap()).unwrap();
    encoded["aggregate_play_shadows"]["song"]
        .as_object_mut()
        .unwrap()
        .remove("counter_epoch");
    encoded
        .as_object_mut()
        .unwrap()
        .remove("native_history_health");

    let decoded = decode_bridge(&serde_json::to_vec(&encoded).unwrap()).unwrap();
    assert_eq!(
        decoded
            .aggregate_play_shadows()
            .get(&ItemId::new("song").unwrap())
            .unwrap()
            .counter_epoch,
        0
    );
    assert_eq!(decoded.native_history_health(), NativeHistoryHealth::Off);
}
