use super::*;
use crate::open_subsonic::bridge_store::MAX_UNCERTAIN_SCROBBLE_READBACKS;

fn unchanged_song_body(count: u64, played: &str) -> String {
    format!(
        r#"{{"subsonic-response":{{"status":"ok","song":{{"id":"song-1","title":"Server song","artist":"Server artist","playCount":{count},"played":"{played}"}}}}}}"#
    )
}

#[tokio::test]
async fn redirected_submission_uses_bounded_attention_never_false_confirmation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut baseline, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut baseline)
                .await
                .contains("/rest/getSong.view?")
        );
        write_json(
            &mut baseline,
            &unchanged_song_body(10, "2026-03-25T00:00:00Z"),
        )
        .await;

        let (mut lost, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut lost)
                .await
                .contains("/rest/scrobble.view?")
        );
        lost.write_all(
            b"HTTP/1.1 302 Found\r\nLocation: /rest/scrobble-again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();

        for (count, played) in [
            (10, "2026-03-25T00:00:00Z"),
            (11, "2026-03-27T00:00:00Z"),
            (12, "2026-03-28T00:00:00Z"),
        ] {
            let (mut readback, _) = listener.accept().await.unwrap();
            assert!(
                read_request(&mut readback)
                    .await
                    .contains("/rest/getSong.view?")
            );
            write_json(&mut readback, &unchanged_song_body(count, played)).await;
        }
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let track = scrobble_track(&store_set);
    runtime
        .queue_scrobble(
            &mut store_set,
            "count-only-ambiguous",
            OpenSubsonicScrobbleKind::Submission,
            track,
        )
        .unwrap();

    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    assert!(
        runtime
            .retry_network(&mut store_set, &client)
            .await
            .is_err()
    );
    for _ in 0..MAX_UNCERTAIN_SCROBBLE_READBACKS {
        runtime
            .retry_network(&mut store_set, &client)
            .await
            .unwrap();
    }
    // No active lane remains, so another automatic retry neither GETs nor resubmits.
    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();

    let durable = load_store_set(&paths).unwrap().unwrap();
    let pending = durable.bridge_state.outbound_scrobbles().front().unwrap();
    assert_eq!(pending.delivery, OutboundScrobbleDelivery::NeedsAttention);
    assert_eq!(
        pending.uncertain_readbacks,
        MAX_UNCERTAIN_SCROBBLE_READBACKS
    );
    assert_eq!(
        read_status(&paths).unwrap().kind,
        OpenSubsonicStatusKind::NeedsAttention
    );
    assert_eq!(
        read_status(&paths)
            .unwrap()
            .outbound_scrobbles_needing_attention,
        1
    );
    let attention_ids = runtime.outbound_scrobble_attention_ids(&durable);
    assert_eq!(attention_ids.len(), 1);
    assert!(attention_ids[0].starts_with("sub-scrobble-"));
    for forbidden in ["count-only-ambiguous", "song-1", "test-api-key", "http://"] {
        assert!(!attention_ids[0].contains(forbidden));
    }

    let item_id = ItemId::new("song-1").unwrap();
    let exact_echo = HistoryRefreshResult {
        backend_id: store_set.profile.backend_id().clone(),
        account_scope_id: store_set.profile.account_scope_id().clone(),
        base_cursor: None,
        native: Ok(Some(NativeHistoryBatch {
            rows: vec![NativeHistoryObservation {
                row_id: 999,
                item_id: item_id.clone(),
                track: portable_server_track(&song(&store_set, 5, 12)),
                observed_at_unix: 1_774_483_200,
            }],
            aggregate_baselines: std::collections::BTreeMap::new(),
            next_cursor: None,
            truncated: false,
            metadata_retry_pending: false,
        })),
        standard: Ok(Vec::new()),
    };
    runtime
        .apply_history_refresh(&mut store_set, exact_echo)
        .unwrap();
    assert!(store_set.bridge_state.outbound_scrobbles().is_empty());
    assert_eq!(
        read_status(&paths).unwrap().kind,
        OpenSubsonicStatusKind::UpToDate
    );
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn definite_connect_failure_restores_queue_and_credit_before_restart() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut baseline, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut baseline)
                .await
                .contains("/rest/getSong.view?")
        );
        write_json(
            &mut baseline,
            &unchanged_song_body(10, "2026-03-25T00:00:00Z"),
        )
        .await;
        // Dropping the listener makes the following connection fail before request bytes exist.
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let track = scrobble_track(&store_set);
    runtime
        .queue_scrobble(
            &mut store_set,
            "connect-failed-before-send",
            OpenSubsonicScrobbleKind::Submission,
            track,
        )
        .unwrap();
    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();
    let item_id = ItemId::new("song-1").unwrap();
    let counter_epoch = store_set
        .bridge_state
        .aggregate_play_shadows()
        .get(&item_id)
        .unwrap()
        .counter_epoch;
    assert_eq!(
        store_set
            .bridge_state
            .record_aggregate_history_evidence(item_id.clone(), counter_epoch, 1)
            .unwrap(),
        1
    );
    let expected = store_set.revisions();
    commit_store_set(&paths, expected, &mut store_set).unwrap();
    assert!(
        runtime
            .retry_network(&mut store_set, &client)
            .await
            .is_err()
    );

    let mut restarted = load_store_set(&paths).unwrap().unwrap();
    let pending = restarted.bridge_state.outbound_scrobbles().front().unwrap();
    assert_eq!(pending.delivery, OutboundScrobbleDelivery::Queued);
    assert!(!pending.exact_credit_recorded);
    assert_eq!(pending.uncertain_readbacks, 0);
    let credits = restarted
        .bridge_state
        .history_dedupe_credits()
        .get(&item_id)
        .unwrap()
        .get(&counter_epoch)
        .unwrap();
    assert_eq!(credits.aggregate_unmatched, 1);
    assert_eq!(credits.exact_unmatched, 0);

    let retry_listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let retry_server = tokio::spawn(async move {
        let (mut stream, _) = retry_listener.accept().await.unwrap();
        assert!(
            read_request(&mut stream)
                .await
                .contains("/rest/scrobble.view?")
        );
        write_json(&mut stream, r#"{"subsonic-response":{"status":"ok"}}"#).await;
    });
    let retry_client = OpenSubsonicClient::connect(&restarted.profile)
        .await
        .unwrap();
    let restarted_runtime = BridgeRuntime::writable(paths.clone(), None);
    restarted_runtime
        .retry_network(&mut restarted, &retry_client)
        .await
        .unwrap();
    assert!(restarted.bridge_state.outbound_scrobbles().is_empty());
    let credits = restarted
        .bridge_state
        .history_dedupe_credits()
        .get(&item_id)
        .unwrap()
        .get(&counter_epoch)
        .unwrap();
    assert_eq!(credits.aggregate_unmatched, 1);
    assert_eq!(
        credits.exact_unmatched, 1,
        "a successful outbound report must reserve future aggregate growth without consuming older evidence"
    );
    retry_server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn pre_wire_crash_boundary_is_ambiguous_bounded_and_explicitly_resolvable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (root, paths, mut store_set, _client, runtime) = fixture(port, None).await;
    let mut initial = song(&store_set, 5, 10);
    initial.played_at = Some("2026-03-25T00:00:00Z".to_owned());
    runtime.observe_songs(&mut store_set, &[initial]).unwrap();
    let track = scrobble_track(&store_set);
    runtime
        .queue_scrobble(
            &mut store_set,
            "irreducible-pre-wire-crash-boundary",
            OpenSubsonicScrobbleKind::Submission,
            track,
        )
        .unwrap();
    // The fsync-to-wire boundary cannot atomically prove whether request bytes escaped before a
    // process crash. Recovery must therefore preserve ambiguity, never infer a safe resend.
    let mut pending = store_set.bridge_state.outbound_scrobbles()[0].clone();
    pending.delivery = OutboundScrobbleDelivery::Uncertain;
    pending.baseline_captured = true;
    pending.baseline_play_count = Some(10);
    pending.baseline_played_at = Some("2026-03-25T00:00:00Z".to_owned());
    store_set
        .bridge_state
        .reserve_outbound_exact_history_credit(pending.item_id.clone(), 0)
        .unwrap();
    pending.exact_credit_recorded = true;
    pending.exact_credit_epoch = Some(0);
    store_set
        .bridge_state
        .replace_outbound_scrobble(pending)
        .unwrap();
    let expected = store_set.revisions();
    commit_store_set(&paths, expected, &mut store_set).unwrap();
    drop(store_set);

    let server = tokio::spawn(async move {
        for _ in 0..MAX_UNCERTAIN_SCROBBLE_READBACKS {
            let (mut readback, _) = listener.accept().await.unwrap();
            assert!(
                read_request(&mut readback)
                    .await
                    .contains("/rest/getSong.view?"),
                "restart recovery must never blindly resubmit an ambiguous marker"
            );
            write_json(
                &mut readback,
                &unchanged_song_body(10, "2026-03-25T00:00:00Z"),
            )
            .await;
        }
    });
    let mut restarted = load_store_set(&paths).unwrap().unwrap();
    let client = OpenSubsonicClient::connect(&restarted.profile)
        .await
        .unwrap();
    let restarted_runtime = BridgeRuntime::writable(paths.clone(), None);
    for _ in 0..MAX_UNCERTAIN_SCROBBLE_READBACKS {
        restarted_runtime
            .retry_network(&mut restarted, &client)
            .await
            .unwrap();
    }
    server.await.unwrap();

    let event_id = restarted_runtime
        .outbound_scrobble_attention_ids(&restarted)
        .into_iter()
        .next()
        .unwrap();
    restarted_runtime
        .resolve_outbound_scrobble(&mut restarted, &event_id, OutboundScrobbleResolution::Retry)
        .unwrap();
    let queued = restarted
        .bridge_state
        .outbound_scrobbles()
        .front()
        .unwrap()
        .clone();
    assert_eq!(queued.delivery, OutboundScrobbleDelivery::Queued);
    assert!(!queued.exact_credit_recorded);
    assert!(!queued.baseline_captured);
    assert!(restarted.bridge_state.history_dedupe_credits().is_empty());

    // Recreate a bounded attention marker to exercise the explicit "treat as sent" choice.
    restarted
        .bridge_state
        .reserve_outbound_exact_history_credit(queued.item_id.clone(), 0)
        .unwrap();
    let mut attention = queued;
    attention.delivery = OutboundScrobbleDelivery::NeedsAttention;
    attention.baseline_captured = true;
    attention.exact_credit_recorded = true;
    attention.exact_credit_epoch = Some(0);
    attention.uncertain_readbacks = MAX_UNCERTAIN_SCROBBLE_READBACKS;
    restarted
        .bridge_state
        .replace_outbound_scrobble(attention)
        .unwrap();
    let expected = restarted.revisions();
    commit_store_set(&paths, expected, &mut restarted).unwrap();
    restarted_runtime
        .resolve_outbound_scrobble(
            &mut restarted,
            &event_id,
            OutboundScrobbleResolution::MarkSent,
        )
        .unwrap();
    assert!(restarted.bridge_state.outbound_scrobbles().is_empty());
    assert_eq!(restarted.bridge_state.outbound_echoes().len(), 1);
    assert_eq!(
        restarted
            .bridge_state
            .history_dedupe_credits()
            .get(&ItemId::new("song-1").unwrap())
            .unwrap()
            .get(&0)
            .unwrap()
            .exact_unmatched,
        1
    );

    let exact_echo = HistoryRefreshResult {
        backend_id: restarted.profile.backend_id().clone(),
        account_scope_id: restarted.profile.account_scope_id().clone(),
        base_cursor: None,
        native: Ok(Some(NativeHistoryBatch {
            rows: vec![NativeHistoryObservation {
                row_id: 777,
                item_id: ItemId::new("song-1").unwrap(),
                track: portable_server_track(&song(&restarted, 5, 11)),
                observed_at_unix: 1_774_483_200,
            }],
            aggregate_baselines: std::collections::BTreeMap::new(),
            next_cursor: None,
            truncated: false,
            metadata_retry_pending: false,
        })),
        standard: Ok(Vec::new()),
    };
    restarted_runtime
        .apply_history_refresh(&mut restarted, exact_echo)
        .unwrap();
    assert!(
        restarted
            .bridge_state
            .pending_engagement_imports()
            .is_empty(),
        "a late exact echo must link to the marked-sent local event"
    );
    assert!(restarted.bridge_state.outbound_echoes().is_empty());

    let mut aggregate_echo = song(&restarted, 5, 11);
    aggregate_echo.played_at = Some("2026-03-26T00:00:00Z".to_owned());
    restarted_runtime
        .observe_songs(&mut restarted, &[aggregate_echo])
        .unwrap();
    assert!(
        restarted
            .bridge_state
            .pending_engagement_imports()
            .is_empty(),
        "the reserved outbound credit must suppress the late aggregate echo"
    );
    assert!(restarted.bridge_state.history_dedupe_credits().is_empty());
    let _ = std::fs::remove_dir_all(root);
}
