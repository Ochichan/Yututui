use super::*;
use crate::open_subsonic::bridge_store::PendingRatingProjectionStage;
use crate::open_subsonic::rating::{RawServerRating, canonical_server_rating};

type ExpectedRequest = (&'static str, Option<String>, String);

async fn rating_server(expected: Vec<ExpectedRequest>) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for (endpoint, query, body) in expected {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let request_line = request.lines().next().unwrap_or_default();
            assert!(request_line.contains(endpoint), "{request_line}");
            if let Some(query) = query {
                assert!(request_line.contains(&query), "{request_line}");
            }
            write_json(&mut stream, &body).await;
        }
    });
    (port, server)
}

fn ok() -> String {
    r#"{"subsonic-response":{"status":"ok"}}"#.to_owned()
}

fn failed() -> String {
    r#"{"subsonic-response":{"status":"failed","error":{"code":50,"message":"failed"}}}"#.to_owned()
}

fn song_readback(item_id: &str, raw: RawServerRating) -> String {
    let mut song = serde_json::json!({
        "id": item_id,
        "title": format!("Server song {item_id}"),
        "artist": "Server artist"
    });
    if let Some(rating) = raw.user_rating {
        song["userRating"] = serde_json::json!(rating);
    }
    if raw.starred {
        song["starred"] = serde_json::json!("2026-07-26T00:00:00Z");
    }
    serde_json::json!({
        "subsonic-response": {
            "status": "ok",
            "song": song
        }
    })
    .to_string()
}

fn local_winner(
    store_set: &OpenSubsonicStoreSet,
    operation_id: &str,
    rating: Rating,
) -> OpenSubsonicRatingWinner {
    let mut winner = rating_winner(store_set, "song-1", operation_id);
    winner.rating = rating;
    winner
}

async fn assert_unstar_failure_survives_restart(target: Rating) {
    let raw = canonical_server_rating(target);
    assert!(!raw.starred);
    let rating = raw.user_rating.unwrap();
    let (port, server) = rating_server(vec![
        (
            "/rest/setRating.view?",
            Some(format!("rating={rating}")),
            ok(),
        ),
        ("/rest/unstar.view?", None, failed()),
        ("/rest/unstar.view?", None, ok()),
        ("/rest/getSong.view?", None, song_readback("song-1", raw)),
    ])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let winner = local_winner(&store_set, "local-rating", target);
    runtime
        .reconcile_ratings(&mut store_set, vec![winner])
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
    let failed_stage = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        failed_stage
            .bridge_state
            .pending_rating_projections()
            .values()
            .next()
            .unwrap()
            .stage,
        PendingRatingProjectionStage::SetStarred
    );

    let mut restarted_store = load_store_set(&paths).unwrap().unwrap();
    let restarted_client = OpenSubsonicClient::connect(&restarted_store.profile)
        .await
        .unwrap();
    let restarted = BridgeRuntime::writable(paths.clone(), None);
    restarted
        .retry_network(&mut restarted_store, &restarted_client)
        .await
        .unwrap();
    assert_eq!(
        restarted_store
            .bridge_state
            .pending_rating_projections()
            .values()
            .next()
            .unwrap()
            .stage,
        PendingRatingProjectionStage::Readback
    );
    restarted
        .retry_network(&mut restarted_store, &restarted_client)
        .await
        .unwrap();

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(durable.bridge_state.pending_rating_projections().is_empty());
    let shadow = durable
        .bridge_state
        .rating_shadow(&ItemId::new("song-1").unwrap())
        .unwrap();
    assert_eq!(shadow.raw, raw);
    assert_eq!(
        shadow.confirmed_operation_id.as_deref(),
        Some("local-rating")
    );
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn disliked_and_neutral_unstar_failures_resume_after_restart() {
    assert_unstar_failure_survives_restart(Rating::Disliked).await;
    assert_unstar_failure_survives_restart(Rating::Neutral).await;
}

#[tokio::test]
async fn stable_mismatch_becomes_one_external_observation_and_stops_local_rewrites() {
    let mismatch = RawServerRating {
        user_rating: Some(1),
        starred: false,
    };
    let (port, server) = rating_server(vec![
        ("/rest/setRating.view?", Some("rating=5".to_owned()), ok()),
        ("/rest/star.view?", None, ok()),
        (
            "/rest/getSong.view?",
            None,
            song_readback("song-1", mismatch),
        ),
        (
            "/rest/getSong.view?",
            None,
            song_readback("song-1", mismatch),
        ),
    ])
    .await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let sink: OpenSubsonicBridgeSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, Some(sink)).await;
    let winner = local_winner(&store_set, "local-liked", Rating::Liked);
    runtime
        .reconcile_ratings(&mut store_set, vec![winner])
        .unwrap();

    for _ in 0..4 {
        runtime
            .retry_network(&mut store_set, &client)
            .await
            .unwrap();
    }

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(durable.bridge_state.pending_rating_projections().is_empty());
    assert_eq!(durable.bridge_state.pending_rating_imports().len(), 1);
    let (operation_id, pending) = durable
        .bridge_state
        .pending_rating_imports()
        .first_key_value()
        .unwrap();
    assert_eq!(pending.raw, mismatch);
    assert_eq!(pending.mapped, Rating::Disliked);
    assert_eq!(
        durable
            .bridge_state
            .rating_shadow(&ItemId::new("song-1").unwrap())
            .unwrap()
            .raw,
        mismatch
    );
    let operation_id = operation_id.clone();
    let imported = events.lock().unwrap().clone();
    assert_eq!(imported.len(), 1);
    assert!(matches!(
        &imported[0],
        OpenSubsonicBridgeImport::Rating {
            operation_id: emitted,
            rating: Rating::Disliked,
            ..
        } if emitted == &operation_id
    ));

    runtime
        .acknowledge_import(&mut store_set, &operation_id)
        .unwrap();
    let repeated_observation = song(&store_set, 1, 0);
    runtime
        .observe_songs(&mut store_set, &[repeated_observation])
        .unwrap();
    assert!(store_set.bridge_state.pending_rating_imports().is_empty());
    assert_eq!(events.lock().unwrap().len(), 1);

    let older_local = local_winner(&store_set, "older-local-retry", Rating::Liked);
    runtime
        .reconcile_ratings(&mut store_set, vec![older_local])
        .unwrap();
    assert_eq!(store_set.bridge_state.pending_rating_projections().len(), 1);
    let mut server_winner = local_winner(&store_set, "server-ledger-winner", Rating::Disliked);
    server_winner.origin = OperationOrigin::OpenSubsonic {
        backend_id: store_set.profile.backend_id().as_str().to_owned(),
    };
    runtime
        .reconcile_ratings(&mut store_set, vec![server_winner])
        .unwrap();
    assert!(
        store_set
            .bridge_state
            .pending_rating_projections()
            .is_empty()
    );

    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn changing_mismatch_must_repeat_before_it_becomes_external() {
    let first = RawServerRating {
        user_rating: Some(1),
        starred: false,
    };
    let second = RawServerRating {
        user_rating: Some(0),
        starred: false,
    };
    let (port, server) = rating_server(vec![
        ("/rest/setRating.view?", Some("rating=5".to_owned()), ok()),
        ("/rest/star.view?", None, ok()),
        ("/rest/getSong.view?", None, song_readback("song-1", first)),
        ("/rest/getSong.view?", None, song_readback("song-1", second)),
        ("/rest/getSong.view?", None, song_readback("song-1", second)),
    ])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let winner = local_winner(&store_set, "local-liked", Rating::Liked);
    runtime
        .reconcile_ratings(&mut store_set, vec![winner])
        .unwrap();
    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();

    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    let after_first = load_store_set(&paths).unwrap().unwrap();
    let pending = after_first
        .bridge_state
        .pending_rating_projections()
        .values()
        .next()
        .unwrap();
    assert_eq!(pending.stage, PendingRatingProjectionStage::Readback);
    assert_eq!(pending.last_readback, Some(first));
    assert!(after_first.bridge_state.pending_rating_imports().is_empty());

    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    let after_change = load_store_set(&paths).unwrap().unwrap();
    let pending = after_change
        .bridge_state
        .pending_rating_projections()
        .values()
        .next()
        .unwrap();
    assert_eq!(pending.stage, PendingRatingProjectionStage::Readback);
    assert_eq!(pending.last_readback, Some(second));
    assert!(
        after_change
            .bridge_state
            .pending_rating_imports()
            .is_empty()
    );

    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    let stable = load_store_set(&paths).unwrap().unwrap();
    assert!(stable.bridge_state.pending_rating_projections().is_empty());
    let imported = stable
        .bridge_state
        .pending_rating_imports()
        .values()
        .next()
        .unwrap();
    assert_eq!(imported.raw, second);
    assert_eq!(imported.mapped, Rating::Neutral);
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn stable_noncanonical_readback_with_same_mapping_is_preserved_without_normalizing_loop() {
    let noncanonical_like = RawServerRating {
        user_rating: Some(3),
        starred: true,
    };
    let (port, server) = rating_server(vec![
        ("/rest/setRating.view?", Some("rating=5".to_owned()), ok()),
        ("/rest/star.view?", None, ok()),
        (
            "/rest/getSong.view?",
            None,
            song_readback("song-1", noncanonical_like),
        ),
        (
            "/rest/getSong.view?",
            None,
            song_readback("song-1", noncanonical_like),
        ),
    ])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let winner = local_winner(&store_set, "local-liked", Rating::Liked);
    runtime
        .reconcile_ratings(&mut store_set, vec![winner])
        .unwrap();
    for _ in 0..4 {
        runtime
            .retry_network(&mut store_set, &client)
            .await
            .unwrap();
    }

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(durable.bridge_state.pending_rating_projections().is_empty());
    let (operation_id, imported) = durable
        .bridge_state
        .pending_rating_imports()
        .first_key_value()
        .unwrap();
    assert_eq!(imported.raw, noncanonical_like);
    assert_eq!(imported.mapped, Rating::Liked);
    let operation_id = operation_id.clone();

    let mut repeated_observation = song(&store_set, 3, 0);
    repeated_observation.starred = true;
    runtime
        .observe_songs(&mut store_set, &[repeated_observation])
        .unwrap();
    let repeated = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(repeated.bridge_state.pending_rating_imports().len(), 1);
    assert_eq!(
        repeated
            .bridge_state
            .pending_rating_imports()
            .first_key_value()
            .unwrap()
            .0,
        &operation_id
    );
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}
