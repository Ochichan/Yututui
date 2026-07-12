use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::{Listener, Stream};
use interprocess::local_socket::{GenericFilePath, ListenerOptions};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};

use crate::remote::proto::RemoteResponse;

use super::*;

fn bind(endpoint: &str) -> Listener {
    let _ = std::fs::remove_file(endpoint);
    let name = endpoint.to_fs_name::<GenericFilePath>().unwrap();
    ListenerOptions::new().name(name).create_tokio().unwrap()
}

async fn connect(endpoint: &str) -> Stream {
    let name = endpoint.to_fs_name::<GenericFilePath>().unwrap();
    Stream::connect(name).await.unwrap()
}

#[tokio::test]
async fn live_session_reconciles_the_latest_topic_set_without_command_queue_capacity() {
    let endpoint = std::env::temp_dir()
        .join(format!("ytt-gw-sub-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let listener = bind(&endpoint);
    let conn = connect(&endpoint).await;
    let (initial_seen_tx, initial_seen_rx) = oneshot::channel();
    let (desired_ready_tx, desired_ready_rx) = oneshot::channel();
    let (delta_seen_tx, delta_seen_rx) = oneshot::channel();
    let (session_done_tx, session_done_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let peer = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&peer);
        let mut lines = Vec::new();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let initial: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        let _ = initial_seen_tx.send(());
        // Change the desired watch value while the initial Subscribe is still
        // unacknowledged. No delta may be emitted until this reply succeeds.
        let _ = desired_ready_rx.await;
        write_line(
            &peer,
            &ServerFrame::Reply {
                id: initial.id,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await
        .unwrap();
        lines.push(initial);

        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let frame = serde_json::from_str::<ClientFrame>(line.trim()).unwrap();
            write_line(
                &peer,
                &ServerFrame::Reply {
                    id: frame.id,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await
            .unwrap();
            lines.push(frame);
        }
        let _ = delta_seen_tx.send(());
        let _ = session_done_rx.await;
        lines
    });

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (_command_tx, mut command_rx) = mpsc::channel(1);
    let (subscription_tx, mut subscription_rx) = watch::channel(SubscriptionState {
        page_id: Some("page-a".to_string()),
        topics: vec![Topic::Player],
    });
    let controller = async move {
        initial_seen_rx.await.unwrap();
        subscription_tx.send_replace(SubscriptionState {
            page_id: Some("page-a".to_string()),
            topics: vec![Topic::Queue],
        });
        let _ = desired_ready_tx.send(());
        delta_seen_rx.await.unwrap();
        let _ = shutdown_tx.send(());
    };
    let online = AtomicBool::new(true);
    let session = run_session(
        conn,
        &mut shutdown_rx,
        &mut command_rx,
        &mut subscription_rx,
        &|_| {},
        &online,
        "test-gateway",
    );
    let ((), reason) = tokio::join!(controller, session);
    assert_eq!(reason, "shutdown");
    let _ = session_done_tx.send(());

    let frames = server.await.unwrap();
    assert_eq!(
        frames[0].op,
        ClientOp::Subscribe {
            topics: vec![Topic::System, Topic::Player]
        }
    );
    assert_eq!(
        frames[1].op,
        ClientOp::Unsubscribe {
            topics: vec![Topic::Player]
        }
    );
    assert_eq!(
        frames[2].op,
        ClientOp::Subscribe {
            topics: vec![Topic::Queue]
        }
    );
    let _ = std::fs::remove_file(&endpoint);
}

#[tokio::test]
async fn busy_subscription_reply_ends_the_session_for_reconnect_without_losing_desired_state() {
    let endpoint = std::env::temp_dir()
        .join(format!("ytt-gw-sub-busy-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let listener = bind(&endpoint);
    let conn = connect(&endpoint).await;
    let server = tokio::spawn(async move {
        let peer = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&peer);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let frame: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        write_line(
            &peer,
            &ServerFrame::Reply {
                id: frame.id,
                resp: RemoteResponse::err("server_busy"),
            },
        )
        .await
        .unwrap();
        frame
    });

    let (_shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (_command_tx, mut command_rx) = mpsc::channel(1);
    let desired = SubscriptionState {
        page_id: Some("page-busy".to_string()),
        topics: vec![Topic::Player, Topic::Queue],
    };
    let (_subscription_tx, mut subscription_rx) = watch::channel(desired.clone());
    let online = AtomicBool::new(true);
    let reason = run_session(
        conn,
        &mut shutdown_rx,
        &mut command_rx,
        &mut subscription_rx,
        &|_| {},
        &online,
        "test-gateway",
    )
    .await;

    assert_eq!(reason, "subscription_busy");
    assert!(!online.load(Ordering::Acquire));
    assert_eq!(*subscription_rx.borrow(), desired);
    let frame = server.await.unwrap();
    assert_eq!(
        frame.op,
        ClientOp::Subscribe {
            topics: vec![Topic::System, Topic::Player, Topic::Queue]
        }
    );
    let _ = std::fs::remove_file(&endpoint);
}

#[tokio::test]
async fn missing_subscription_reply_times_out_without_losing_desired_state() {
    let endpoint = std::env::temp_dir()
        .join(format!("ytt-gw-sub-timeout-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let listener = bind(&endpoint);
    let conn = connect(&endpoint).await;
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let peer = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&peer);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let frame: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        let _ = release_rx.await;
        frame
    });

    let (_shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (_command_tx, mut command_rx) = mpsc::channel(1);
    let desired = SubscriptionState {
        page_id: Some("page-delayed".to_owned()),
        topics: vec![Topic::Search],
    };
    let (_subscription_tx, mut subscription_rx) = watch::channel(desired.clone());
    let online = AtomicBool::new(true);
    let reason = run_session(
        conn,
        &mut shutdown_rx,
        &mut command_rx,
        &mut subscription_rx,
        &|_| {},
        &online,
        "test-gateway",
    )
    .await;

    assert_eq!(reason, "subscription_timeout");
    assert!(!online.load(Ordering::Acquire));
    assert_eq!(*subscription_rx.borrow(), desired);
    let _ = release_tx.send(());
    let frame = server.await.unwrap();
    assert_eq!(frame.page_id.as_deref(), Some("page-delayed"));
    assert_eq!(
        frame.op,
        ClientOp::Subscribe {
            topics: vec![Topic::System, Topic::Search]
        }
    );
    let _ = std::fs::remove_file(&endpoint);
}

#[tokio::test]
async fn replacement_search_waits_for_its_page_subscription_ack() {
    let endpoint = std::env::temp_dir()
        .join(format!("ytt-gw-page-search-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let listener = bind(&endpoint);
    let conn = connect(&endpoint).await;
    let (initial_seen_tx, initial_seen_rx) = oneshot::channel();
    let (replacement_queued_tx, replacement_queued_rx) = oneshot::channel();
    let (search_seen_tx, search_seen_rx) = oneshot::channel();
    let (shutdown_sent_tx, shutdown_sent_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let peer = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&peer);

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let initial: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(initial.page_id.as_deref(), Some("page-a"));
        let _ = initial_seen_tx.send(());
        replacement_queued_rx.await.unwrap();

        // The page-B command is already queued locally, but cannot overtake page A's pending
        // subscription acknowledgement.
        let mut early = String::new();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), reader.read_line(&mut early))
                .await
                .is_err()
        );
        write_line(
            &peer,
            &ServerFrame::Reply {
                id: initial.id,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await
        .unwrap();

        let mut transitions = Vec::new();
        for _ in 0..2 {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let frame: ClientFrame = serde_json::from_str(line.trim()).unwrap();
            assert!(
                !matches!(frame.op, ClientOp::Command(_)),
                "command overtook the page transition"
            );
            write_line(
                &peer,
                &ServerFrame::Reply {
                    id: frame.id,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await
            .unwrap();
            transitions.push(frame);
        }

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let search: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(search.page_id.as_deref(), Some("page-b"));
        assert!(matches!(
            search.op,
            ClientOp::Command(RemoteCommand::RunSearch { ticket: 1, .. })
        ));
        write_line(
            &peer,
            &ServerFrame::Reply {
                id: search.id,
                resp: RemoteResponse::ok("searching".to_string()),
            },
        )
        .await
        .unwrap();
        let _ = search_seen_tx.send(());
        // Do not drop `peer` until the controller has published shutdown. Otherwise socket EOF
        // can win before the shutdown signal exists and make the session reason nondeterministic.
        let _ = shutdown_sent_rx.await;
        transitions
    });

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let (subscription_tx, mut subscription_rx) = watch::channel(SubscriptionState {
        page_id: Some("page-a".to_string()),
        topics: vec![Topic::Search],
    });
    let controller = async move {
        initial_seen_rx.await.unwrap();
        subscription_tx.send_replace(SubscriptionState {
            page_id: Some("page-b".to_string()),
            topics: vec![Topic::Search],
        });
        command_tx
            .send(OutEnvelope {
                v: 1,
                id: Some(9),
                page_id: Some("page-a".to_string()),
                request_id: Some("gui:page-a:9".to_string()),
                kind: OutKind::Cmd,
                name: "run_search".to_string(),
                payload: serde_json::json!({
                    "ticket": 9,
                    "query": "stale",
                    "source": "all",
                }),
            })
            .await
            .unwrap();
        command_tx
            .send(OutEnvelope {
                v: 1,
                id: Some(1),
                page_id: Some("page-b".to_string()),
                request_id: Some("gui:page-b:1".to_string()),
                kind: OutKind::Cmd,
                name: "run_search".to_string(),
                payload: serde_json::json!({
                    "ticket": 1,
                    "query": "beta",
                    "source": "all",
                }),
            })
            .await
            .unwrap();
        let _ = replacement_queued_tx.send(());
        search_seen_rx.await.unwrap();
        let _ = shutdown_tx.send(());
        let _ = shutdown_sent_tx.send(());
    };
    let emitted = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&emitted);
    let capture = move |event| captured.lock().unwrap().push(event);
    let online = AtomicBool::new(true);
    let session = run_session(
        conn,
        &mut shutdown_rx,
        &mut command_rx,
        &mut subscription_rx,
        &capture,
        &online,
        "test-gateway",
    );
    let ((), reason) = tokio::join!(controller, session);
    assert_eq!(reason, "shutdown");

    let transitions = server.await.unwrap();
    assert_eq!(transitions[0].page_id.as_deref(), Some("page-a"));
    assert!(matches!(transitions[0].op, ClientOp::Unsubscribe { .. }));
    assert_eq!(transitions[1].page_id.as_deref(), Some("page-b"));
    assert!(matches!(transitions[1].op, ClientOp::Subscribe { .. }));
    assert!(emitted.lock().unwrap().iter().any(|event| matches!(
        event,
        GatewayEvent::Frame(InEnvelope {
            id: Some(9),
            page_id: Some(page_id),
            kind: crate::desktop::bridge::InKind::Err,
            payload: Some(payload),
            ..
        }) if page_id == "page-a" && payload["reason"] == "stale_page"
    )));
    let _ = std::fs::remove_file(&endpoint);
}
