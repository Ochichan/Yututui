use super::*;
use std::sync::Mutex;

use crate::desktop::bridge::InKind;
use crate::remote::proto::{RemoteCommand, ToggleState};

#[tokio::test]
async fn buffered_reader_keeps_a_fragment_across_select_cancellation() {
    use tokio::io::{AsyncWriteExt, BufReader, duplex};

    let (reader_side, mut writer) = duplex(256);
    let mut reader = BufReader::new(reader_side);
    let mut frame = Vec::new();
    writer.write_all(b"{\"id\":7,\"resp\"").await.unwrap();

    tokio::select! {
        result = read_line_buffered(&mut reader, &mut frame) => {
            panic!("fragment unexpectedly completed: {result:?}");
        }
        _ = tokio::time::sleep(Duration::from_millis(5)) => {}
    }
    assert_eq!(frame, b"{\"id\":7,\"resp\"");

    writer.write_all(b":null}\n").await.unwrap();
    let line = read_line_buffered(&mut reader, &mut frame)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(line, "{\"id\":7,\"resp\":null}\n");
    assert!(frame.is_empty());
}

#[test]
fn event_sequence_must_start_at_one_and_remain_contiguous() {
    let mut last = 0;
    assert!(accept_next_sequence(&mut last, 1));
    assert!(accept_next_sequence(&mut last, 2));
    assert!(!accept_next_sequence(&mut last, 4));
    assert_eq!(last, 2);
    assert!(!accept_next_sequence(&mut last, 2));

    let mut initial = 0;
    assert!(!accept_next_sequence(&mut initial, 2));
}

fn command_env(kind: OutKind, id: Option<u64>) -> OutEnvelope {
    OutEnvelope {
        v: 1,
        id,
        page_id: None,
        request_id: None,
        kind,
        name: "next".to_string(),
        payload: serde_json::Value::Null,
    }
}

fn test_handle(
    capacity: usize,
    online: bool,
) -> (
    GatewayHandle,
    mpsc::Receiver<OutEnvelope>,
    watch::Receiver<SubscriptionState>,
) {
    let (shutdown, _shutdown_rx) = oneshot::channel();
    let (commands, receiver) = mpsc::channel(capacity);
    let (subscriptions, subscription_rx) = watch::channel(SubscriptionState::default());
    (
        GatewayHandle {
            shutdown: Some(shutdown),
            worker: None,
            commands,
            subscriptions,
            online: Arc::new(AtomicBool::new(online)),
        },
        receiver,
        subscription_rx,
    )
}

#[test]
fn send_reports_full_and_closed_admission() {
    let (handle, receiver, _subscription_rx) = test_handle(1, true);

    assert_eq!(
        handle.send(command_env(OutKind::Cmd, None)),
        Ok(DeliveryReceipt::Enqueued)
    );
    assert_eq!(
        handle.send(command_env(OutKind::Cmd, None)),
        Err(DeliveryError::Busy)
    );

    drop(receiver);
    assert_eq!(
        handle.send(command_env(OutKind::Cmd, None)),
        Err(DeliveryError::Closed)
    );
}

#[test]
fn native_revision_checked_command_uses_the_reserved_correlation_range() {
    let (handle, mut receiver, _subscription_rx) = test_handle(2, true);
    let id = handle
        .send_remote(RemoteCommand::QueuePlayIfRevision {
            position: 3,
            expected_rev: 41,
        })
        .unwrap();
    let envelope = receiver.try_recv().unwrap();
    assert!(id >= NATIVE_REQUEST_ID_BASE);
    assert_eq!(envelope.id, Some(id));
    assert_eq!(envelope.name, "queue_play_if_revision");
    assert_eq!(
        envelope.payload,
        serde_json::json!({ "position": 3, "expected_rev": 41 })
    );
}

#[test]
fn correlated_reply_recovers_its_native_page_generation() {
    let key = (Some("generation-test-page".to_string()), 991);
    SOURCE_GENERATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.clone(), 73);
    let event = attach_source_generation(GatewayEvent::Frame(InEnvelope::res_for_page(
        key.1,
        key.0,
        serde_json::json!({}),
    )));
    assert!(matches!(
        event,
        GatewayEvent::PageFrame {
            source_generation: Some(73),
            ..
        }
    ));
}

#[test]
fn correlated_rejection_becomes_an_immediate_error_envelope() {
    let (handle, receiver, _subscription_rx) = test_handle(1, true);
    let _ = handle
        .send(command_env(OutKind::Cmd, None))
        .expect("fill the one-slot queue");

    let busy = send_or_reject(Some(&handle), command_env(OutKind::Req, Some(41)))
        .expect("correlated rejection should be returned to the webview");
    assert_eq!(busy.id, Some(41));
    assert_eq!(busy.kind, InKind::Err);
    assert_eq!(busy.payload, Some(serde_json::json!({ "reason": "busy" })));

    drop(receiver);
    let closed = send_or_reject(None, command_env(OutKind::Cmd, Some(42)))
        .expect("a correlated command should also receive a local error");
    assert_eq!(closed.id, Some(42));
    assert_eq!(closed.kind, InKind::Err);
    assert_eq!(
        closed.payload,
        Some(serde_json::json!({ "reason": "closed" }))
    );
}

#[test]
fn offline_gateway_rejects_correlated_commands_but_accepts_subscriptions() {
    let (handle, mut receiver, subscription_rx) = test_handle(4, false);

    for (kind, id) in [(OutKind::Req, 51), (OutKind::Cmd, 52)] {
        let rejected = send_or_reject(Some(&handle), command_env(kind, Some(id)))
            .expect("offline correlated command receives a local error");
        assert_eq!(rejected.id, Some(id));
        assert_eq!(rejected.kind, InKind::Err);
        assert_eq!(
            rejected.payload,
            Some(serde_json::json!({ "reason": "offline" }))
        );
    }
    assert!(
        receiver.try_recv().is_err(),
        "offline correlated commands never enter the bounded queue"
    );

    for kind in [OutKind::Sub, OutKind::Unsub] {
        assert!(matches!(
            handle.send(sub_env(kind, serde_json::json!(["player"]))),
            Ok(DeliveryReceipt::Coalesced { .. })
        ));
    }
    assert!(receiver.try_recv().is_err());
    assert!(subscription_rx.borrow().topics.is_empty());
}

#[test]
fn latest_subscription_lane_survives_command_saturation_and_replays_after_reconnect() {
    let (handle, _receiver, subscription_rx) = test_handle(1, true);
    let _ = handle
        .send(command_env(OutKind::Cmd, Some(60)))
        .expect("fill the command lane");
    assert_eq!(
        handle.send(command_env(OutKind::Cmd, Some(61))),
        Err(DeliveryError::Busy)
    );

    for env in [
        sub_env(OutKind::Sub, serde_json::json!(["player", "queue"])),
        sub_env(OutKind::Sub, serde_json::json!(["settings"])),
        sub_env(OutKind::Unsub, serde_json::json!(["player"])),
    ] {
        assert!(matches!(
            handle.send(env),
            Ok(DeliveryReceipt::Coalesced { .. })
        ));
    }

    let desired = subscription_rx.borrow().clone();
    assert_eq!(desired.topics, vec![Topic::Queue, Topic::Settings]);
    assert_eq!(
        initial_topics(&desired.topics),
        vec![Topic::System, Topic::Queue, Topic::Settings],
        "a fresh session replays the final coalesced state"
    );
}

#[test]
fn uncorrelated_rejection_has_no_synthetic_reply() {
    assert_eq!(send_or_reject(None, command_env(OutKind::Cmd, None)), None);
}

#[test]
fn cmd_names_and_payloads_map_to_remote_commands() {
    assert_eq!(
        to_remote_command("toggle_pause", &serde_json::Value::Null),
        Some(RemoteCommand::TogglePause)
    );
    assert_eq!(
        to_remote_command("next", &serde_json::json!({})),
        Some(RemoteCommand::Next)
    );
    assert_eq!(
        to_remote_command("seek_to", &serde_json::json!({ "ms": 1000 })),
        Some(RemoteCommand::SeekTo { ms: 1000 })
    );
    assert_eq!(
        to_remote_command("set_volume", &serde_json::json!({ "percent": 42 })),
        Some(RemoteCommand::SetVolume { percent: 42 })
    );
    assert_eq!(
        to_remote_command("queue_play", &serde_json::json!({ "position": 3 })),
        Some(RemoteCommand::QueuePlay { position: 3 })
    );
    assert_eq!(
        to_remote_command(
            "queue_play_if_revision",
            &serde_json::json!({ "position": 3, "expected_rev": 41 })
        ),
        Some(RemoteCommand::QueuePlayIfRevision {
            position: 3,
            expected_rev: 41,
        })
    );
    assert_eq!(
        to_remote_command(
            "queue_remove_if_revision",
            &serde_json::json!({ "position": 2, "expected_rev": 41 })
        ),
        Some(RemoteCommand::QueueRemoveIfRevision {
            position: 2,
            expected_rev: 41,
        })
    );
    assert_eq!(
        to_remote_command("streaming", &serde_json::json!({ "state": "on" })),
        Some(RemoteCommand::Streaming {
            state: ToggleState::On
        })
    );
    // The GUI-session verbs the frontend already sends (search + row playback).
    assert_eq!(
        to_remote_command(
            "run_search",
            &serde_json::json!({ "ticket": 3, "query": "queen", "source": "all" })
        ),
        Some(RemoteCommand::RunSearch {
            ticket: 3,
            query: "queen".to_string(),
            source: crate::search_source::SearchSource::All,
        })
    );
    assert_eq!(
        to_remote_command(
            "play_tracks",
            &serde_json::json!({ "video_ids": ["a", "b"] })
        ),
        Some(RemoteCommand::PlayTracks {
            video_ids: vec!["a".to_string(), "b".to_string()],
        })
    );
    assert_eq!(
        to_remote_command("enqueue_tracks", &serde_json::json!({ "video_ids": ["c"] })),
        Some(RemoteCommand::EnqueueTracks {
            video_ids: vec!["c".to_string()],
        })
    );
}

/// Pin every deferred-v8 frontend name (with the exact payload its store emits — see
/// gui/src/lib/dev/democore.ts, the reference implementation) to its enum variant, so a
/// rename on either side fails here instead of as a runtime bad_command.
#[test]
fn deferred_v8_names_and_store_payloads_map_to_remote_commands() {
    use crate::remote::proto::RateChange;
    let cases: Vec<(&str, serde_json::Value, RemoteCommand)> = vec![
        (
            "rate",
            serde_json::json!({ "video_id": "v1", "rating": "cycle" }),
            RemoteCommand::Rate {
                video_id: "v1".into(),
                rating: RateChange::Cycle,
            },
        ),
        (
            "queue_move",
            serde_json::json!({ "from": 2, "to": 5, "expected_rev": 9 }),
            RemoteCommand::QueueMove {
                from: 2,
                to: 5,
                expected_rev: Some(9),
            },
        ),
        (
            "queue_remove_many",
            serde_json::json!({ "positions": [1, 3] }),
            RemoteCommand::QueueRemoveMany {
                positions: vec![1, 3],
                expected_rev: None,
            },
        ),
        // The keyboard path sends no revision (gui actions.ts); the guard is optional.
        (
            "queue_clear_upcoming",
            serde_json::json!({}),
            RemoteCommand::QueueClearUpcoming { expected_rev: None },
        ),
        (
            "play_video",
            serde_json::json!({ "video_id": "v1" }),
            RemoteCommand::PlayVideo {
                video_id: "v1".into(),
            },
        ),
        (
            "ask_ai",
            serde_json::json!({ "ticket": 7, "prompt": "hi" }),
            RemoteCommand::AskAi {
                ticket: 7,
                prompt: "hi".into(),
            },
        ),
        (
            "library_play",
            serde_json::json!({ "scope": "favorites", "filter": "" }),
            RemoteCommand::LibraryPlay {
                scope: "favorites".into(),
                filter: String::new(),
            },
        ),
        (
            "library_enqueue",
            serde_json::json!({ "scope": "history", "filter": "cat" }),
            RemoteCommand::LibraryEnqueue {
                scope: "history".into(),
                filter: "cat".into(),
            },
        ),
        (
            "library_remove",
            serde_json::json!({ "scope": "favorites", "video_id": "v1" }),
            RemoteCommand::LibraryRemove {
                scope: "favorites".into(),
                video_id: "v1".into(),
            },
        ),
        (
            "fetch_library_page",
            serde_json::json!({ "scope": "all", "filter": "", "offset": 0, "limit": 100 }),
            RemoteCommand::FetchLibraryPage {
                scope: "all".into(),
                filter: String::new(),
                offset: 0,
                limit: 100,
            },
        ),
        (
            "download",
            serde_json::json!({ "video_id": "v1", "title": "T" }),
            RemoteCommand::Download {
                video_id: "v1".into(),
                title: "T".into(),
            },
        ),
        (
            "delete_download",
            serde_json::json!({ "video_id": "v1", "delete_file": true }),
            RemoteCommand::DeleteDownload {
                video_id: "v1".into(),
                delete_file: true,
            },
        ),
        (
            "keymap_bind",
            serde_json::json!({ "context": "Player", "action": "toggle_pause", "chord": "space" }),
            RemoteCommand::KeymapBind {
                context: "Player".into(),
                action: "toggle_pause".into(),
                chord: "space".into(),
            },
        ),
        (
            "keymap_unbind",
            serde_json::json!({ "context": "Player", "action": "toggle_pause" }),
            RemoteCommand::KeymapUnbind {
                context: "Player".into(),
                action: "toggle_pause".into(),
            },
        ),
        (
            "keymap_reset_all",
            serde_json::json!({}),
            RemoteCommand::KeymapResetAll,
        ),
        (
            "theme_set_override",
            serde_json::json!({ "role": "accent", "hex": "#aabbcc" }),
            RemoteCommand::ThemeSetOverride {
                role: "accent".into(),
                hex: "#aabbcc".into(),
            },
        ),
        (
            "theme_clear_override",
            serde_json::json!({ "role": "accent" }),
            RemoteCommand::ThemeClearOverride {
                role: "accent".into(),
            },
        ),
        (
            "clear_romanization_cache",
            serde_json::json!({}),
            RemoteCommand::ClearRomanizationCache,
        ),
        (
            "playlist_create",
            serde_json::json!({ "name": "Mix" }),
            RemoteCommand::PlaylistCreate { name: "Mix".into() },
        ),
        (
            "playlist_delete",
            serde_json::json!({ "playlist_id": "p1" }),
            RemoteCommand::PlaylistDelete {
                playlist_id: "p1".into(),
            },
        ),
        (
            "playlist_add_tracks",
            serde_json::json!({ "playlist_id": "p1", "video_ids": ["v1"] }),
            RemoteCommand::PlaylistAddTracks {
                playlist_id: "p1".into(),
                video_ids: vec!["v1".into()],
            },
        ),
        (
            "playlist_remove_track",
            serde_json::json!({ "playlist_id": "p1", "video_id": "v1" }),
            RemoteCommand::PlaylistRemoveTrack {
                playlist_id: "p1".into(),
                video_id: "v1".into(),
            },
        ),
        (
            "playlist_play",
            serde_json::json!({ "playlist_id": "p1" }),
            RemoteCommand::PlaylistPlay {
                playlist_id: "p1".into(),
            },
        ),
        (
            "fetch_playlist_detail",
            serde_json::json!({ "playlist_id": "p1" }),
            RemoteCommand::FetchPlaylistDetail {
                playlist_id: "p1".into(),
            },
        ),
        (
            "fetch_why_gem",
            serde_json::json!({ "video_id": "v1" }),
            RemoteCommand::FetchWhyGem {
                video_id: "v1".into(),
            },
        ),
        (
            "transfer_list_spotify",
            serde_json::json!({}),
            RemoteCommand::TransferListSpotify,
        ),
        (
            "transfer_start",
            serde_json::json!({ "spec": { "dry_run": true } }),
            RemoteCommand::TransferStart {
                spec: serde_json::json!({ "dry_run": true }),
            },
        ),
        (
            "transfer_cancel",
            serde_json::json!({}),
            RemoteCommand::TransferCancel,
        ),
        (
            "lastfm_connect",
            serde_json::json!({}),
            RemoteCommand::LastfmConnect,
        ),
        (
            "spotify_connect",
            serde_json::json!({}),
            RemoteCommand::SpotifyConnect,
        ),
        (
            "listen_brainz_configure",
            serde_json::json!({ "submit": true, "token": "tok" }),
            RemoteCommand::ListenBrainzConfigure {
                submit: Some(true),
                token: Some("tok".into()),
                custom_url: None,
            },
        ),
        (
            "account_set",
            serde_json::json!({ "service": "lastfm", "field": "scrobbling", "value": true }),
            RemoteCommand::AccountSet {
                service: "lastfm".into(),
                field: "scrobbling".into(),
                value: serde_json::json!(true),
            },
        ),
    ];
    for (name, payload, expected) in cases {
        assert_eq!(
            to_remote_command(name, &payload),
            Some(expected),
            "frontend name {name} must translate"
        );
    }
}

#[test]
fn unsupported_or_malformed_commands_are_none() {
    assert_eq!(
        to_remote_command("not_a_command", &serde_json::Value::Null),
        None
    );
    // Right name, wrong field type.
    assert_eq!(
        to_remote_command("seek_to", &serde_json::json!({ "ms": "soon" })),
        None
    );
    // Non-object, non-null payloads can't carry command fields.
    assert_eq!(to_remote_command("next", &serde_json::json!(7)), None);
    assert_eq!(
        to_remote_command("set_volume", &serde_json::json!({ "percent": 101 })),
        None
    );
    assert_eq!(
        to_remote_command(
            "set_setting",
            &serde_json::json!({
                "change": { "setting": "speed", "tenths": 21 }
            })
        ),
        None
    );
}

#[test]
fn topics_parse_from_the_wire_array() {
    assert_eq!(
        parse_topics(&serde_json::json!(["player", "queue", "system"])),
        Some(vec![Topic::Player, Topic::Queue, Topic::System])
    );
    assert_eq!(parse_topics(&serde_json::json!([])), None);
    assert_eq!(parse_topics(&serde_json::json!("player")), None);
    assert_eq!(parse_topics(&serde_json::json!(["bogus"])), None);
}

fn sub_env(kind: OutKind, topics: serde_json::Value) -> OutEnvelope {
    OutEnvelope {
        v: 1,
        id: None,
        page_id: None,
        request_id: None,
        kind,
        name: String::new(),
        payload: topics,
    }
}

#[test]
fn subscriptions_fold_into_the_desired_set_across_kinds() {
    let mut desired = Vec::new();
    assert!(apply_subscription_change(
        OutKind::Sub,
        &[Topic::Player, Topic::Queue],
        &mut desired
    ));
    // Overlapping re-sub dedups instead of duplicating.
    assert!(apply_subscription_change(
        OutKind::Sub,
        &[Topic::Queue, Topic::Settings],
        &mut desired
    ));
    assert!(apply_subscription_change(
        OutKind::Unsub,
        &[Topic::Queue],
        &mut desired
    ));
    assert_eq!(desired, vec![Topic::Player, Topic::Settings]);
    assert!(!apply_subscription_change(
        OutKind::Sub,
        &[Topic::Player],
        &mut desired
    ));
    assert_eq!(desired, vec![Topic::Player, Topic::Settings]);
}

#[test]
fn offline_reconnect_drain_rejects_every_correlated_command() {
    let (tx, mut rx) = mpsc::channel(8);
    tx.try_send(command_env(OutKind::Cmd, Some(61))).unwrap();
    tx.try_send(command_env(OutKind::Req, Some(62))).unwrap();
    tx.try_send(command_env(OutKind::Cmd, None)).unwrap();

    let errors = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&errors);
    drain_offline_commands(
        &mut rx,
        &move |event| {
            if let GatewayEvent::Frame(frame) = event {
                captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(frame);
            }
        },
        "disconnected",
    );

    let errors = errors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].id, Some(61));
    assert_eq!(errors[1].id, Some(62));
    assert!(errors.iter().all(|error| {
        error.kind == InKind::Err
            && error.payload == Some(serde_json::json!({ "reason": "disconnected" }))
    }));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn session_exit_rejects_every_pending_req_and_cmd_id() {
    assert_eq!(
        correlation(&command_env(OutKind::Req, Some(71))),
        Some(FrontendCorrelation {
            page_id: None,
            id: 71,
            mutation: false,
            req: true,
        })
    );
    assert_eq!(
        correlation(&command_env(OutKind::Cmd, Some(72))),
        Some(FrontendCorrelation {
            page_id: None,
            id: 72,
            mutation: true,
            req: false,
        })
    );

    let mut pending = HashMap::from([
        (
            100,
            FrontendCorrelation {
                page_id: None,
                id: 72,
                mutation: true,
                req: false,
            },
        ),
        (
            101,
            FrontendCorrelation {
                page_id: None,
                id: 71,
                mutation: false,
                req: false,
            },
        ),
    ]);
    let errors = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&errors);
    reject_pending(&mut pending, "disconnected", &move |event| {
        if let GatewayEvent::Frame(frame) = event {
            captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(frame);
        }
    });

    assert!(pending.is_empty());
    let errors = errors
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0].id, Some(71));
    assert_eq!(errors[1].id, Some(72));
    assert_eq!(
        errors[0].payload,
        Some(serde_json::json!({ "reason": "disconnected" })),
        "a read-only request can be failed with the transport reason"
    );
    assert_eq!(
        errors[1].payload,
        Some(serde_json::json!({ "reason": "confirmation_lost" })),
        "a written mutation must not be reported as definitively rejected"
    );
}

#[test]
fn initial_subscribe_replays_desired_topics_after_system() {
    assert_eq!(initial_topics(&[]), vec![Topic::System]);
    assert_eq!(
        initial_topics(&[Topic::Player, Topic::Queue]),
        vec![Topic::System, Topic::Player, Topic::Queue]
    );
    // A window that explicitly subscribed to system doesn't double it.
    assert_eq!(
        initial_topics(&[Topic::System, Topic::Player]),
        vec![Topic::System, Topic::Player]
    );
}
