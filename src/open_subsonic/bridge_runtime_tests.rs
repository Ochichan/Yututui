use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use age::secrecy::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::bridge_event::portable_server_track;
use super::bridge_runtime::{
    BridgeRuntime, HistoryRefreshResult, NativeHistoryBatch, NativeHistoryObservation,
    completed_history_overlap,
};
use super::bridge_store::{
    HistoryContinuation, HistoryCursor, OutboundScrobbleDelivery, OutboundScrobbleKind,
    PendingNativeMetadataRow, PendingRatingProjectionStage,
};
use super::*;
use crate::personal_state::{
    OpenSubsonicRatingWinner, OperationOrigin, PortableTrack, PortableTrackKey, Rating,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

mod aggregate_continuation;
mod rating_projection;
mod scrobble_delivery;

async fn fixture(
    port: u16,
    sink: Option<OpenSubsonicBridgeSink>,
) -> (
    std::path::PathBuf,
    OpenSubsonicPaths,
    OpenSubsonicStoreSet,
    OpenSubsonicClient,
    BridgeRuntime,
) {
    let backend_id = BackendId::new("bridge-backend").unwrap();
    let account_scope_id = AccountScopeId::new("bridge-account").unwrap();
    let profile = OpenSubsonicProfile::with_ids(
        0,
        backend_id.clone(),
        account_scope_id.clone(),
        "Bridge server",
        ConfiguredPrivateOrigin::new(&format!("http://127.0.0.1:{port}/"), true).unwrap(),
        None,
    )
    .unwrap();
    let private_state = OpenSubsonicPrivateState::new(
        backend_id.clone(),
        account_scope_id.clone(),
        ServerCredential::api_key(SecretString::from("test-api-key".to_owned())).unwrap(),
    );
    let bridge_state = OpenSubsonicBridgeState::new(backend_id, account_scope_id);
    let mut store_set = OpenSubsonicStoreSet::new(profile, private_state, bridge_state).unwrap();
    let root = std::env::temp_dir().join(format!(
        "yututui-bridge-runtime-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ));
    let paths = OpenSubsonicPaths::for_data_root(root.clone());
    commit_store_set(&paths, StoreRevisions::MISSING, &mut store_set).unwrap();
    let client = OpenSubsonicClient::connect(&store_set.profile)
        .await
        .unwrap();
    let runtime = BridgeRuntime::writable(paths.clone(), sink);
    (root, paths, store_set, client, runtime)
}

fn song(store_set: &OpenSubsonicStoreSet, rating: i64, play_count: u64) -> ServerSong {
    ServerSong {
        item: OpenSubsonicItemRef::new(
            store_set.profile.backend_id().clone(),
            store_set.profile.account_scope_id().clone(),
            ItemId::new("song-1").unwrap(),
        ),
        title: "Server song".to_owned(),
        artist: "Server artist".to_owned(),
        artists: Vec::new(),
        album: Some("Server album".to_owned()),
        album_id: None,
        album_artist: None,
        duration_secs: Some(180),
        track_number: None,
        disc_number: None,
        year: None,
        cover_art_id: None,
        content_type: None,
        suffix: None,
        starred: rating == 5,
        user_rating: Some(rating),
        play_count: Some(play_count),
        played_at: Some("2026-07-26T00:00:00Z".to_owned()),
    }
}

fn scrobble_track(store_set: &OpenSubsonicStoreSet) -> crate::scrobble::ScrobbleTrack {
    scrobble_track_for(store_set, "song-1", 1_774_483_200)
}

fn scrobble_track_for(
    store_set: &OpenSubsonicStoreSet,
    item_id: &str,
    started_unix: i64,
) -> crate::scrobble::ScrobbleTrack {
    crate::scrobble::ScrobbleTrack {
        key: item_id.to_owned(),
        open_subsonic_item: Some(OpenSubsonicItemRef::new(
            store_set.profile.backend_id().clone(),
            store_set.profile.account_scope_id().clone(),
            ItemId::new(item_id).unwrap(),
        )),
        artist: "Server artist".to_owned(),
        title: if item_id == "song-1" {
            "Server song".to_owned()
        } else {
            format!("Server song {item_id}")
        },
        album: Some("Server album".to_owned()),
        duration_secs: Some(180),
        origin_url: None,
        started_unix,
    }
}

fn rating_winner(
    store_set: &OpenSubsonicStoreSet,
    item_id: &str,
    operation_id: &str,
) -> OpenSubsonicRatingWinner {
    OpenSubsonicRatingWinner {
        operation_id: operation_id.to_owned(),
        track: PortableTrack {
            key: PortableTrackKey::OpenSubsonic {
                backend_id: store_set.profile.backend_id().as_str().to_owned(),
                account_scope_id: store_set.profile.account_scope_id().as_str().to_owned(),
                item_id: item_id.to_owned(),
            },
            title: format!("Server song {item_id}"),
            artist: "Server artist".to_owned(),
            album: None,
            duration_secs: Some(180),
            isrc: None,
        },
        rating: Rating::Liked,
        origin: OperationOrigin::Local,
    }
}

#[tokio::test]
async fn outbound_scrobble_acknowledges_local_durability_without_network() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let track = scrobble_track(&store_set);

    runtime
        .queue_scrobble(
            &mut store_set,
            "same-second-owner-event-1",
            OpenSubsonicScrobbleKind::Submission,
            track.clone(),
        )
        .unwrap();
    runtime
        .queue_scrobble(
            &mut store_set,
            "same-second-owner-event-2",
            OpenSubsonicScrobbleKind::Submission,
            track,
        )
        .unwrap();

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(durable.bridge_state.outbound_scrobbles().len(), 2);
    assert_ne!(
        durable.bridge_state.outbound_scrobbles()[0].event_id,
        durable.bridge_state.outbound_scrobbles()[1].event_id,
        "distinct owner events in the same second must not collapse"
    );
    assert_eq!(
        durable
            .bridge_state
            .outbound_scrobbles()
            .front()
            .unwrap()
            .item_id
            .as_str(),
        "song-1"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn activation_discards_stale_now_playing_but_preserves_exact_submission() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let track = scrobble_track(&store_set);
    runtime
        .queue_scrobble(
            &mut store_set,
            "stale-now-playing",
            OpenSubsonicScrobbleKind::NowPlaying,
            track.clone(),
        )
        .unwrap();
    runtime
        .queue_scrobble(
            &mut store_set,
            "durable-submission",
            OpenSubsonicScrobbleKind::Submission,
            track,
        )
        .unwrap();

    runtime
        .refresh_snapshot_for_activation(&mut store_set)
        .unwrap();

    assert_eq!(store_set.bridge_state.outbound_scrobbles().len(), 1);
    assert_eq!(
        store_set.bridge_state.outbound_scrobbles()[0].kind,
        OutboundScrobbleKind::Submission
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(durable.bridge_state.outbound_scrobbles().len(), 1);
    assert_eq!(
        durable.bridge_state.outbound_scrobbles()[0].kind,
        OutboundScrobbleKind::Submission
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn outbound_scrobble_rejects_read_only_missing_and_foreign_scope_without_acknowledgement() {
    let (root, _paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let track = scrobble_track(&store_set);

    assert_eq!(
        BridgeRuntime::read_only().queue_scrobble(
            &mut store_set,
            "read-only",
            OpenSubsonicScrobbleKind::Submission,
            track.clone(),
        ),
        Err(ServiceError::InvalidSetup)
    );
    let mut missing_item = track.clone();
    missing_item.open_subsonic_item = None;
    assert_eq!(
        runtime.queue_scrobble(
            &mut store_set,
            "missing-item",
            OpenSubsonicScrobbleKind::Submission,
            missing_item,
        ),
        Err(ServiceError::InvalidSetup)
    );

    for (event_id, backend_id, account_scope_id) in [
        (
            "foreign-profile",
            BackendId::new("other-backend").unwrap(),
            store_set.profile.account_scope_id().clone(),
        ),
        (
            "foreign-account",
            store_set.profile.backend_id().clone(),
            AccountScopeId::new("other-account").unwrap(),
        ),
    ] {
        let mut foreign = track.clone();
        foreign.open_subsonic_item = Some(OpenSubsonicItemRef::new(
            backend_id,
            account_scope_id,
            ItemId::new("song-1").unwrap(),
        ));
        assert_eq!(
            runtime.queue_scrobble(
                &mut store_set,
                event_id,
                OpenSubsonicScrobbleKind::Submission,
                foreign,
            ),
            Err(ServiceError::Server(ServerError::WrongAccountScope))
        );
    }
    assert!(store_set.bridge_state.outbound_scrobbles().is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rating_projection_receipt_is_durable_without_contacting_the_server() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let winner = OpenSubsonicRatingWinner {
        operation_id: "offline-rating-op".to_owned(),
        track: PortableTrack {
            key: PortableTrackKey::OpenSubsonic {
                backend_id: store_set.profile.backend_id().as_str().to_owned(),
                account_scope_id: store_set.profile.account_scope_id().as_str().to_owned(),
                item_id: "song-1".to_owned(),
            },
            title: "Server song".to_owned(),
            artist: "Server artist".to_owned(),
            album: None,
            duration_secs: Some(180),
            isrc: None,
        },
        rating: Rating::Liked,
        origin: OperationOrigin::Local,
    };

    runtime
        .reconcile_ratings(&mut store_set, vec![winner])
        .unwrap();
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(durable.bridge_state.pending_rating_projections().len(), 1);
    assert_eq!(
        durable
            .bridge_state
            .pending_rating_projections()
            .values()
            .next()
            .unwrap()
            .stage,
        PendingRatingProjectionStage::SetRating
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn native_history_health_transition_is_durable_and_redacted() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    runtime
        .set_native_history_health(&mut store_set, NativeHistoryHealth::Detailed)
        .unwrap();
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .native_history_health(),
        NativeHistoryHealth::Detailed
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn revision_conflict_rebases_memory_before_a_retry() {
    let (root, paths, mut stale, _client, runtime) = fixture(9, None).await;
    let mut concurrent = load_store_set(&paths).unwrap().unwrap();
    concurrent
        .bridge_state
        .set_native_history_health(NativeHistoryHealth::Probing);
    let expected = concurrent.revisions();
    commit_store_set(&paths, expected, &mut concurrent).unwrap();

    assert_eq!(
        runtime.set_native_history_health(&mut stale, NativeHistoryHealth::Detailed),
        Err(ServiceError::Store(StoreError::RevisionConflict))
    );
    assert_eq!(
        stale.bridge_state.native_history_health(),
        NativeHistoryHealth::Probing,
        "a rejected mutation must leave the actor on the latest coherent snapshot"
    );
    assert_eq!(stale.revisions(), concurrent.revisions());

    runtime
        .set_native_history_health(&mut stale, NativeHistoryHealth::Detailed)
        .unwrap();
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable.bridge_state.native_history_health(),
        NativeHistoryHealth::Detailed
    );
    assert_eq!(stale.revisions(), durable.revisions());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn response_lost_after_commit_marker_recovers_as_durable_success() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    super::transaction::fail_after_commit_marker_once_for_test();

    runtime
        .set_native_history_health(&mut store_set, NativeHistoryHealth::Detailed)
        .unwrap();
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(store_set.revisions(), durable.revisions());
    assert_eq!(
        durable.bridge_state.native_history_health(),
        NativeHistoryHealth::Detailed
    );

    let committed_revision = store_set.bridge_state.revision();
    runtime
        .set_native_history_health(&mut store_set, NativeHistoryHealth::Detailed)
        .unwrap();
    assert_eq!(
        store_set.bridge_state.revision(),
        committed_revision,
        "replaying the acknowledged mutation must not create a second commit"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn progressive_history_cursor_survives_restart_and_rejects_stale_results() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let initial = HistoryCursor {
        high_water_id: Some("100".to_owned()),
        overlap_started_at_unix: Some(1_000),
        updated_at_unix: 10,
        continuation: None,
        pending_metadata_rows: Vec::new(),
    };
    store_set
        .bridge_state
        .set_history_cursor("navidrome-native".to_owned(), initial.clone())
        .unwrap();
    let expected = store_set.revisions();
    commit_store_set(&paths, expected, &mut store_set).unwrap();

    let progress = HistoryCursor {
        high_water_id: initial.high_water_id.clone(),
        overlap_started_at_unix: Some(800),
        updated_at_unix: 20,
        continuation: Some(HistoryContinuation {
            candidate_high_water_id: Some("300".to_owned()),
            next_start: 400,
            through_unix: Some(800),
            reached_high_water: false,
            overlap_row_ids: vec![250, 249],
            backlog_complete: false,
            head_anchor_high_water_id: None,
            head_next_start: None,
            head_from_unix: None,
            head_through_unix: None,
            head_overlap_row_ids: Vec::new(),
        }),
        pending_metadata_rows: (1..=65)
            .rev()
            .map(|id| PendingNativeMetadataRow {
                row_id: id,
                item_id: ItemId::new(format!("pending-song-{id}")).unwrap(),
                observed_at_unix: i64::try_from(id).unwrap(),
            })
            .collect(),
    };
    let mut result = history_result(
        &store_set,
        Some(initial.clone()),
        Some(progress.clone()),
        true,
    );
    if let Ok(Some(batch)) = &mut result.native {
        batch.metadata_retry_pending = true;
    }
    let outcome = runtime
        .apply_history_refresh(&mut store_set, result)
        .unwrap();
    assert!(outcome.native_truncated);
    assert!(!outcome.native_stale);
    assert_eq!(
        outcome.native_error,
        Some(NativeHistoryError::TemporarilyUnavailable),
        "a metadata retry must not silently report detailed history as healthy"
    );
    assert_eq!(
        store_set
            .bridge_state
            .history_cursors()
            .get("navidrome-native"),
        Some(&progress),
        "a partial refresh must retain the old committed high-water"
    );

    let mut restarted = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        restarted
            .bridge_state
            .history_cursors()
            .get("navidrome-native"),
        Some(&progress),
        "page/time progress must survive an owner restart"
    );

    let stale_cursor = HistoryCursor {
        high_water_id: Some("999".to_owned()),
        overlap_started_at_unix: None,
        updated_at_unix: 30,
        continuation: None,
        pending_metadata_rows: Vec::new(),
    };
    let stale = runtime
        .apply_history_refresh(
            &mut restarted,
            history_result(&store_set, Some(initial), Some(stale_cursor), false),
        )
        .unwrap();
    assert!(stale.native_stale);
    assert_eq!(
        restarted
            .bridge_state
            .history_cursors()
            .get("navidrome-native"),
        Some(&progress),
        "same high-water but an older continuation generation must not overwrite progress"
    );

    let completed = HistoryCursor {
        high_water_id: Some("300".to_owned()),
        overlap_started_at_unix: Some(100),
        updated_at_unix: 40,
        continuation: None,
        pending_metadata_rows: Vec::new(),
    };
    let completed_result =
        history_result(&restarted, Some(progress), Some(completed.clone()), false);
    let outcome = runtime
        .apply_history_refresh(&mut restarted, completed_result)
        .unwrap();
    assert!(!outcome.native_stale);
    assert_eq!(
        restarted
            .bridge_state
            .history_cursors()
            .get("navidrome-native"),
        Some(&completed),
        "only a completed continuation commits the accumulated candidate high-water"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn failed_cursor_commit_does_not_advance_past_unresolved_metadata() {
    let (root, _paths, mut store_set, _client, _runtime) = fixture(9, None).await;
    let unresolved = HistoryCursor {
        high_water_id: Some("2".to_owned()),
        overlap_started_at_unix: Some(1),
        updated_at_unix: 2,
        continuation: None,
        pending_metadata_rows: vec![PendingNativeMetadataRow {
            row_id: 2,
            item_id: ItemId::new("pending-song-2").unwrap(),
            observed_at_unix: 2,
        }],
    };
    let result = history_result(&store_set, None, Some(unresolved), true);

    let error = BridgeRuntime::read_only()
        .apply_history_refresh(&mut store_set, result)
        .unwrap_err();

    assert!(matches!(error, ServiceError::InvalidSetup));
    assert!(
        store_set
            .bridge_state
            .history_cursors()
            .get("navidrome-native")
            .is_none(),
        "a rejected apply must retain the old cursor so the upstream row is scanned again"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn continuation_completion_without_new_rows_preserves_the_bounded_overlap() {
    let cursor = HistoryCursor {
        high_water_id: Some("100".to_owned()),
        overlap_started_at_unix: Some(800),
        updated_at_unix: 20,
        continuation: Some(HistoryContinuation {
            candidate_high_water_id: Some("300".to_owned()),
            next_start: 400,
            through_unix: Some(800),
            reached_high_water: true,
            overlap_row_ids: vec![250, 249],
            backlog_complete: true,
            head_anchor_high_water_id: Some("100".to_owned()),
            head_next_start: Some(200),
            head_from_unix: Some(800),
            head_through_unix: Some(900),
            head_overlap_row_ids: vec![300, 299],
        }),
        pending_metadata_rows: Vec::new(),
    };
    assert_eq!(completed_history_overlap(Some(&cursor), &[]), Some(800));
}

fn history_result(
    store_set: &OpenSubsonicStoreSet,
    base_cursor: Option<HistoryCursor>,
    next_cursor: Option<HistoryCursor>,
    truncated: bool,
) -> HistoryRefreshResult {
    HistoryRefreshResult {
        backend_id: store_set.profile.backend_id().clone(),
        account_scope_id: store_set.profile.account_scope_id().clone(),
        base_cursor,
        native: Ok(Some(NativeHistoryBatch {
            rows: Vec::new(),
            aggregate_baselines: std::collections::BTreeMap::new(),
            next_cursor,
            truncated,
            metadata_retry_pending: false,
        })),
        standard: Ok(Vec::new()),
    }
}

fn native_history_result(
    store_set: &OpenSubsonicStoreSet,
    row_time: i64,
    play_count: u64,
    played_at: &str,
) -> HistoryRefreshResult {
    let item_id = ItemId::new("song-1").unwrap();
    HistoryRefreshResult {
        backend_id: store_set.profile.backend_id().clone(),
        account_scope_id: store_set.profile.account_scope_id().clone(),
        base_cursor: None,
        native: Ok(Some(NativeHistoryBatch {
            rows: vec![NativeHistoryObservation {
                row_id: 77,
                item_id: item_id.clone(),
                track: portable_server_track(&song(store_set, 5, play_count)),
                observed_at_unix: row_time,
            }],
            aggregate_baselines: std::iter::once((
                item_id,
                (play_count, Some(played_at.to_owned())),
            ))
            .collect(),
            next_cursor: None,
            truncated: false,
            metadata_retry_pending: false,
        })),
        standard: Ok(Vec::new()),
    }
}

#[tokio::test]
async fn exact_history_credits_distinguish_covered_backfill_from_aggregate_lag() {
    let (root, _paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let mut baseline = song(&store_set, 5, 10);
    baseline.played_at = Some("1970-01-01T00:01:40Z".to_owned());
    runtime.observe_songs(&mut store_set, &[baseline]).unwrap();

    let covered_backfill = native_history_result(&store_set, 90, 10, "1970-01-01T00:01:40Z");
    runtime
        .apply_history_refresh(&mut store_set, covered_backfill)
        .unwrap();
    assert_eq!(store_set.bridge_state.pending_engagement_imports().len(), 1);
    assert!(
        store_set.bridge_state.history_dedupe_credits().is_empty(),
        "an exact backfill at or before the established played watermark is already in the baseline"
    );

    let mut next_play = song(&store_set, 5, 11);
    next_play.played_at = Some("1970-01-01T00:01:50Z".to_owned());
    runtime.observe_songs(&mut store_set, &[next_play]).unwrap();
    assert_eq!(
        store_set.bridge_state.pending_engagement_imports().len(),
        2,
        "the next distinct aggregate play must not be consumed by an old exact credit"
    );

    let (lag_root, _paths, mut lag, _client, lag_runtime) = fixture(9, None).await;
    let mut lag_baseline = song(&lag, 5, 10);
    lag_baseline.played_at = Some("1970-01-01T00:01:40Z".to_owned());
    lag_runtime
        .observe_songs(&mut lag, &[lag_baseline])
        .unwrap();
    let aggregate_lag = native_history_result(&lag, 110, 10, "1970-01-01T00:01:40Z");
    lag_runtime
        .apply_history_refresh(&mut lag, aggregate_lag)
        .unwrap();
    assert_eq!(lag.bridge_state.history_dedupe_credits().len(), 1);
    let mut caught_up = song(&lag, 5, 11);
    caught_up.played_at = Some("1970-01-01T00:01:50Z".to_owned());
    lag_runtime.observe_songs(&mut lag, &[caught_up]).unwrap();
    assert_eq!(
        lag.bridge_state.pending_engagement_imports().len(),
        1,
        "a count that catches up to the newer exact row must be suppressed as its aggregate echo"
    );
    assert!(lag.bridge_state.history_dedupe_credits().is_empty());

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(lag_root);
}

#[tokio::test]
async fn response_lost_submission_is_not_resent_and_readback_completes_replay_safely() {
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
            r#"{"subsonic-response":{"status":"ok","song":{"id":"song-1","title":"Server song","artist":"Server artist","playCount":10,"played":"2026-03-25T00:00:00Z"}}}"#,
        )
        .await;

        let (mut lost_response, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut lost_response)
                .await
                .contains("/rest/scrobble.view?")
        );
        drop(lost_response);

        let (mut readback, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut readback)
                .await
                .contains("/rest/getSong.view?")
        );
        write_json(
            &mut readback,
            r#"{"subsonic-response":{"status":"ok","song":{"id":"song-1","title":"Server song","artist":"Server artist","playCount":11,"played":"2026-03-26T00:00:00Z"}}}"#,
        )
        .await;
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let track = scrobble_track(&store_set);

    runtime
        .queue_scrobble(
            &mut store_set,
            "stable-response-lost-event",
            OpenSubsonicScrobbleKind::Submission,
            track.clone(),
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
    let uncertain = load_store_set(&paths).unwrap().unwrap();
    let pending = uncertain.bridge_state.outbound_scrobbles().front().unwrap();
    assert_eq!(pending.delivery, OutboundScrobbleDelivery::Uncertain);
    assert!(pending.exact_credit_recorded);

    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    let completed = load_store_set(&paths).unwrap().unwrap();
    assert!(completed.bridge_state.outbound_scrobbles().is_empty());
    assert!(
        completed
            .bridge_state
            .pending_engagement_imports()
            .is_empty(),
        "aggregate readback must consume the local exact credit"
    );

    runtime
        .queue_scrobble(
            &mut store_set,
            "stable-response-lost-event",
            OpenSubsonicScrobbleKind::Submission,
            track,
        )
        .unwrap();
    assert!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .outbound_scrobbles()
            .is_empty(),
        "owner replay after acknowledgement must remain a durable no-op"
    );

    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn definite_rejection_restores_a_retriable_queue_without_exact_credit() {
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
            r#"{"subsonic-response":{"status":"ok","song":{"id":"song-1","title":"Server song","artist":"Server artist","playCount":10}}}"#,
        )
        .await;

        let (mut rejected, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut rejected)
                .await
                .contains("/rest/scrobble.view?")
        );
        rejected
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let track = scrobble_track(&store_set);
    runtime
        .queue_scrobble(
            &mut store_set,
            "definitely-rejected-event",
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
    let durable = load_store_set(&paths).unwrap().unwrap();
    let pending = durable.bridge_state.outbound_scrobbles().front().unwrap();
    assert_eq!(pending.delivery, OutboundScrobbleDelivery::Queued);
    assert!(!pending.exact_credit_recorded);
    assert!(durable.bridge_state.history_dedupe_credits().is_empty());

    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn observations_are_durable_until_owner_ack_and_aggregate_is_delta_only() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let sink: OpenSubsonicBridgeSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
    });
    let (root, paths, mut store_set, _client, runtime) = fixture(9, Some(sink)).await;

    let first_song = song(&store_set, 5, 10);
    runtime
        .observe_songs(&mut store_set, &[first_song])
        .unwrap();
    let first = events.lock().unwrap().clone();
    assert_eq!(first.len(), 1);
    assert!(matches!(
        &first[0],
        OpenSubsonicBridgeImport::Rating {
            rating: Rating::Liked,
            ..
        }
    ));
    let operation_id = first[0].operation_id().to_owned();
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .pending_rating_imports()
            .len(),
        1
    );

    runtime
        .acknowledge_import(&mut store_set, &operation_id)
        .unwrap();
    events.lock().unwrap().clear();
    let changed_song = song(&store_set, 1, 12);
    runtime
        .observe_songs(&mut store_set, &[changed_song])
        .unwrap();
    let second = events.lock().unwrap().clone();
    assert_eq!(
        second
            .iter()
            .filter(|event| matches!(event, OpenSubsonicBridgeImport::Rating { .. }))
            .count(),
        1
    );
    assert_eq!(
        second
            .iter()
            .filter(|event| matches!(event, OpenSubsonicBridgeImport::Engagement { .. }))
            .count(),
        2
    );
    assert!(second.iter().any(|event| matches!(
        event,
        OpenSubsonicBridgeImport::Rating {
            rating: Rating::Disliked,
            ..
        }
    )));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn count_only_aggregate_growth_consumes_exact_credit_before_importing() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let sink: OpenSubsonicBridgeSink = Arc::new(move |event| {
        captured.lock().unwrap().push(event);
    });
    let (root, paths, mut store_set, _client, runtime) = fixture(9, Some(sink)).await;

    let baseline = song(&store_set, 5, 10);
    runtime.observe_songs(&mut store_set, &[baseline]).unwrap();
    events.lock().unwrap().clear();

    let item_id = ItemId::new("song-1").unwrap();
    let counter_epoch = store_set
        .bridge_state
        .aggregate_play_shadows()
        .get(&item_id)
        .unwrap()
        .counter_epoch;
    assert!(
        store_set
            .bridge_state
            .record_exact_history_evidence(item_id.clone(), counter_epoch)
            .unwrap()
    );

    let mut count_only = song(&store_set, 5, 11);
    count_only.played_at = None;
    runtime
        .observe_songs(&mut store_set, &[count_only])
        .unwrap();
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .all(|event| !matches!(event, OpenSubsonicBridgeImport::Engagement { .. }))
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(durable.bridge_state.pending_engagement_imports().is_empty());
    assert!(durable.bridge_state.history_dedupe_credits().is_empty());

    events.lock().unwrap().clear();
    let mut verified = song(&store_set, 5, 12);
    verified.played_at = Some("2026-07-26T00:01:00Z".to_owned());
    runtime.observe_songs(&mut store_set, &[verified]).unwrap();
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, OpenSubsonicBridgeImport::Engagement { .. }))
            .count(),
        1,
        "the count-only delta consumes only the matching exact credit"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn count_only_growth_is_durable_retry_stable_and_accepts_invalid_played() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let baseline = song(&store_set, 5, 10);
    let baseline_played = baseline.played_at.clone();
    runtime.observe_songs(&mut store_set, &[baseline]).unwrap();

    let mut missing = song(&store_set, 5, 12);
    missing.played_at = None;
    runtime
        .observe_songs(&mut store_set, &[missing.clone()])
        .unwrap();
    let ids = store_set
        .bridge_state
        .pending_engagement_imports()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    assert!(
        store_set
            .bridge_state
            .pending_engagement_imports()
            .values()
            .all(|pending| {
                pending.engagement == crate::personal_state::EngagementKind::Play
                    && pending.played_duration_ms.is_none()
            })
    );
    assert_eq!(
        store_set
            .bridge_state
            .aggregate_play_shadows()
            .get(missing.item.item_id())
            .unwrap()
            .played_at,
        baseline_played
    );

    runtime
        .observe_songs(&mut store_set, &[missing.clone()])
        .unwrap();
    assert_eq!(
        store_set
            .bridge_state
            .pending_engagement_imports()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ids,
        "retrying the same counter observation must not mint new events"
    );

    let mut restarted = load_store_set(&paths).unwrap().unwrap();
    runtime.observe_songs(&mut restarted, &[missing]).unwrap();
    assert_eq!(
        restarted
            .bridge_state
            .pending_engagement_imports()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ids,
        "restart replay must preserve the same durable event identities"
    );

    let mut invalid = song(&restarted, 5, 14);
    invalid.played_at = Some("not-a-timestamp".to_owned());
    runtime.observe_songs(&mut restarted, &[invalid]).unwrap();
    assert_eq!(
        restarted.bridge_state.pending_engagement_imports().len(),
        4,
        "invalid played is the same low-confidence count-only fallback as missing played"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rating_partial_failure_resumes_from_durable_stage_and_requires_readback() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for (endpoint, body) in [
            (
                "/rest/setRating.view?",
                r#"{"subsonic-response":{"status":"ok"}}"#,
            ),
            (
                "/rest/star.view?",
                r#"{"subsonic-response":{"status":"failed","error":{"code":50}}}"#,
            ),
            (
                "/rest/star.view?",
                r#"{"subsonic-response":{"status":"ok"}}"#,
            ),
            (
                "/rest/getSong.view?",
                r#"{"subsonic-response":{"status":"ok","song":{"id":"song-1","title":"Server song","artist":"Server artist","userRating":5,"starred":"2026-07-26T00:00:00Z"}}}"#,
            ),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(
                request
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .contains(endpoint),
                "{request}"
            );
            write_json(&mut stream, body).await;
        }
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let track = PortableTrack {
        key: PortableTrackKey::OpenSubsonic {
            backend_id: store_set.profile.backend_id().as_str().to_owned(),
            account_scope_id: store_set.profile.account_scope_id().as_str().to_owned(),
            item_id: "song-1".to_owned(),
        },
        title: "Server song".to_owned(),
        artist: "Server artist".to_owned(),
        album: None,
        duration_secs: Some(180),
        isrc: None,
    };
    let winner = OpenSubsonicRatingWinner {
        operation_id: "local-rating-op".to_owned(),
        track,
        rating: Rating::Liked,
        origin: OperationOrigin::Local,
    };

    runtime
        .reconcile_ratings(&mut store_set, vec![winner])
        .unwrap();
    let queued = load_store_set(&paths)
        .unwrap()
        .unwrap()
        .bridge_state
        .pending_rating_projections()
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(queued.stage, PendingRatingProjectionStage::SetRating);

    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    let pending = load_store_set(&paths)
        .unwrap()
        .unwrap()
        .bridge_state
        .pending_rating_projections()
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(pending.stage, PendingRatingProjectionStage::SetStarred);

    assert!(
        runtime
            .retry_network(&mut store_set, &client)
            .await
            .is_err()
    );
    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .pending_rating_projections()
            .values()
            .next()
            .unwrap()
            .stage,
        PendingRatingProjectionStage::Readback
    );
    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(durable.bridge_state.pending_rating_projections().is_empty());
    assert_eq!(
        durable
            .bridge_state
            .rating_shadow(&ItemId::new("song-1").unwrap())
            .and_then(|shadow| shadow.confirmed_operation_id.as_deref()),
        Some("local-rating-op")
    );
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn failing_rating_does_not_head_of_line_block_later_items() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let failed =
            r#"{"subsonic-response":{"status":"failed","error":{"code":50,"message":"failed"}}}"#;
        let ok = r#"{"subsonic-response":{"status":"ok"}}"#;
        let readback = r#"{"subsonic-response":{"status":"ok","song":{"id":"item-b","title":"Server song item-b","artist":"Server artist","userRating":5,"starred":"2026-07-26T00:00:00Z"}}}"#;
        for (endpoint, item_id, body) in [
            ("/rest/setRating.view?", "item-a", failed),
            ("/rest/setRating.view?", "item-b", ok),
            ("/rest/setRating.view?", "item-a", failed),
            ("/rest/star.view?", "item-b", ok),
            ("/rest/setRating.view?", "item-a", failed),
            ("/rest/getSong.view?", "item-b", readback),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let request_line = request.lines().next().unwrap_or_default();
            assert!(request_line.contains(endpoint), "{request_line}");
            assert!(
                request_line.contains(&format!("id={item_id}")),
                "{request_line}"
            );
            write_json(&mut stream, body).await;
        }
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let rating_a = rating_winner(&store_set, "item-a", "rating-a");
    let rating_b = rating_winner(&store_set, "item-b", "rating-b");
    runtime
        .reconcile_ratings(&mut store_set, vec![rating_a, rating_b])
        .unwrap();

    for attempt in 0..6 {
        let result = runtime.retry_network(&mut store_set, &client).await;
        if attempt % 2 == 0 {
            assert!(result.is_err(), "item-a must remain a permanent failure");
        } else {
            result.unwrap();
        }
    }

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(durable.bridge_state.pending_rating_projections().len(), 1);
    assert_eq!(
        durable
            .bridge_state
            .pending_rating_projections()
            .keys()
            .next()
            .unwrap()
            .as_str(),
        "item-a"
    );
    assert_eq!(
        durable
            .bridge_state
            .pending_rating_projections()
            .values()
            .next()
            .unwrap()
            .stage,
        PendingRatingProjectionStage::SetRating
    );
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn failing_rating_lane_does_not_block_outbound_lane() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for (endpoint, item_id, body) in [
            (
                "/rest/setRating.view?",
                "item-a",
                r#"{"subsonic-response":{"status":"failed","error":{"code":50,"message":"failed"}}}"#,
            ),
            (
                "/rest/getSong.view?",
                "item-b",
                r#"{"subsonic-response":{"status":"ok","song":{"id":"item-b","title":"Server song item-b","artist":"Server artist","playCount":10,"played":"2026-03-25T00:00:00Z"}}}"#,
            ),
            (
                "/rest/setRating.view?",
                "item-a",
                r#"{"subsonic-response":{"status":"failed","error":{"code":50,"message":"failed"}}}"#,
            ),
            (
                "/rest/scrobble.view?",
                "item-b",
                r#"{"subsonic-response":{"status":"ok"}}"#,
            ),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let request_line = request.lines().next().unwrap_or_default();
            assert!(request_line.contains(endpoint), "{request_line}");
            assert!(
                request_line.contains(&format!("id={item_id}")),
                "{request_line}"
            );
            write_json(&mut stream, body).await;
        }
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let rating = rating_winner(&store_set, "item-a", "rating-a");
    runtime
        .reconcile_ratings(&mut store_set, vec![rating])
        .unwrap();
    let track = scrobble_track_for(&store_set, "item-b", 1_774_483_200);
    runtime
        .queue_scrobble(
            &mut store_set,
            "owner-event-b",
            OpenSubsonicScrobbleKind::Submission,
            track,
        )
        .unwrap();

    assert!(
        runtime
            .retry_network(&mut store_set, &client)
            .await
            .is_err()
    );
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
    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(durable.bridge_state.pending_rating_projections().len(), 1);
    assert!(durable.bridge_state.outbound_scrobbles().is_empty());
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn uncertain_scrobble_does_not_head_of_line_block_queued_reports() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let track_a = scrobble_track_for(&store_set, "item-a", 1_774_483_200);
    let track_b = scrobble_track_for(&store_set, "item-b", 1_774_483_201);
    runtime
        .queue_scrobble(
            &mut store_set,
            "owner-event-a",
            OpenSubsonicScrobbleKind::Submission,
            track_a,
        )
        .unwrap();
    runtime
        .queue_scrobble(
            &mut store_set,
            "owner-event-b",
            OpenSubsonicScrobbleKind::Submission,
            track_b,
        )
        .unwrap();
    let mut ordered = store_set
        .bridge_state
        .outbound_scrobbles()
        .iter()
        .map(|pending| (pending.event_id.clone(), pending.item_id.clone()))
        .collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.0.cmp(&right.0));
    let failing_event_id = ordered[0].0.clone();
    let failing_item_id = ordered[0].1.clone();
    let succeeding_event_id = ordered[1].0.clone();
    let succeeding_item_id = ordered[1].1.clone();

    let server_failing_item = failing_item_id.clone();
    let server_succeeding_item = succeeding_item_id.clone();
    let server = tokio::spawn(async move {
        for item_id in [&server_failing_item, &server_succeeding_item] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let request_line = request.lines().next().unwrap_or_default();
            assert!(
                request_line.contains("/rest/getSong.view?"),
                "{request_line}"
            );
            assert!(
                request_line.contains(&format!("id={}", item_id.as_str())),
                "{request_line}"
            );
            let body = format!(
                r#"{{"subsonic-response":{{"status":"ok","song":{{"id":"{}","title":"Server song","artist":"Server artist","playCount":10,"played":"2026-03-25T00:00:00Z"}}}}}}"#,
                item_id.as_str()
            );
            write_json(&mut stream, &body).await;
        }

        let (mut lost_response, _) = listener.accept().await.unwrap();
        let request = read_request(&mut lost_response).await;
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("/rest/scrobble.view?"),
            "{request_line}"
        );
        assert!(
            request_line.contains(&format!("id={}", server_failing_item.as_str())),
            "{request_line}"
        );
        drop(lost_response);

        let (mut succeeding, _) = listener.accept().await.unwrap();
        let request = read_request(&mut succeeding).await;
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("/rest/scrobble.view?"),
            "{request_line}"
        );
        assert!(
            request_line.contains(&format!("id={}", server_succeeding_item.as_str())),
            "{request_line}"
        );
        write_json(&mut succeeding, r#"{"subsonic-response":{"status":"ok"}}"#).await;
    });

    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    assert!(
        runtime
            .retry_network(&mut store_set, &client)
            .await
            .is_err(),
        "the first submission loses its response and becomes uncertain"
    );
    let after_loss = load_store_set(&paths).unwrap().unwrap();
    let uncertain = after_loss
        .bridge_state
        .outbound_scrobbles()
        .iter()
        .find(|pending| pending.event_id == failing_event_id)
        .unwrap();
    assert_eq!(uncertain.delivery, OutboundScrobbleDelivery::Uncertain);
    assert!(uncertain.exact_credit_recorded);
    assert_eq!(
        after_loss
            .bridge_state
            .outbound_scrobbles()
            .iter()
            .find(|pending| pending.event_id == succeeding_event_id)
            .unwrap()
            .delivery,
        OutboundScrobbleDelivery::Queued
    );

    runtime
        .retry_network(&mut store_set, &client)
        .await
        .unwrap();
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(durable.bridge_state.outbound_scrobbles().len(), 1);
    assert_eq!(
        durable.bridge_state.outbound_scrobbles()[0].event_id,
        failing_event_id
    );
    assert_eq!(
        durable.bridge_state.outbound_scrobbles()[0].delivery,
        OutboundScrobbleDelivery::Uncertain
    );
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while request.len() < 32 * 1024 {
        if stream.read(&mut byte).await.unwrap() == 0 {
            break;
        }
        request.push(byte[0]);
        if request.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(request).unwrap()
}

async fn write_json(stream: &mut tokio::net::TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}
