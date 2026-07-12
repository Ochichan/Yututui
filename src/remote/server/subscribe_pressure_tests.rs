use tokio::io::BufReader;

use super::session_socket_tests::{
    connect, hello, read_json_line, start_server, start_stalled_command_server, test_endpoint,
    write_json,
};
use super::*;
use crate::remote::proto::{ClientFrame, ClientOp, HelloAck, ServerFrame};

#[tokio::test]
async fn session_reports_server_busy_when_retained_request_cache_is_saturated() {
    let hub = test_hub();
    hub.requests.fill_with_completed_for_test();
    let ep = start_server("retained-cache-full", hub);
    let conn = connect(&ep).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    write_json(
        &mut write_half,
        &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
    )
    .await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(ack.ok);

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 49,
            request_id: Some("new-while-retained".to_owned()),
            page_id: None,
            op: ClientOp::Command(RemoteCommand::ToggleShuffle),
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 49);
            assert!(!resp.ok);
            assert_eq!(resp.reason.as_deref(), Some("server_busy"));
        }
        other => panic!("expected cache saturation reply, got {other:?}"),
    }
}

#[tokio::test]
async fn subscribe_owner_pressure_returns_server_busy_without_closing_the_session() {
    let (ep, _held_replies) = start_stalled_command_server("subscribe-busy", test_hub());
    let conn = connect(&ep).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    write_json(
        &mut write_half,
        &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
    )
    .await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(ack.ok);

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 37,
            request_id: None,
            page_id: None,
            op: ClientOp::Subscribe {
                topics: vec![Topic::Player],
            },
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 37);
            assert!(!resp.ok);
            assert_eq!(resp.reason.as_deref(), Some("server_busy"));
        }
        other => panic!("expected subscribe saturation reply, got {other:?}"),
    }

    // Temporary owner ingress pressure is not owner shutdown. Keep this session usable so
    // clients can choose their own bounded retry/reconnect policy.
    write_json(
        &mut write_half,
        &ClientFrame {
            id: 38,
            request_id: None,
            page_id: None,
            op: ClientOp::Ping,
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Pong { id } => assert_eq!(id, 38),
        other => panic!("busy subscribe closed or contaminated the session: {other:?}"),
    }
}

#[tokio::test]
async fn equal_request_ids_and_tickets_from_two_sessions_keep_their_origins() {
    let endpoint = test_endpoint("two-search-origins");
    let listener = bind(&endpoint).unwrap();
    let hub = test_hub();
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let serve_hub = Arc::clone(&hub);
    tokio::spawn(serve(
        listener,
        Arc::from("secret"),
        Arc::new(move |event| match event {
            RemoteEvent::SessionCommand {
                command: RemoteCommand::RunSearch { ticket, .. },
                origin,
                reply,
            } => {
                let observed = (
                    origin.session_id(),
                    origin.page_id().map(str::to_owned),
                    ticket,
                );
                let accepted = seen_tx.send(observed).is_ok();
                let _ = reply.send(RemoteResponse::ok("searching".to_owned()));
                accepted
            }
            RemoteEvent::SessionSubscribe { .. } => true,
            _ => false,
        }),
        serve_hub,
    ));

    let mut clients = Vec::new();
    for page_id in ["page-a", "page-b"] {
        let conn = connect(&endpoint).await;
        let (read_half, mut write_half) = tokio::io::split(conn);
        let mut reader = BufReader::new(read_half);
        write_json(
            &mut write_half,
            &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
        )
        .await;
        let ack: HelloAck = read_json_line(&mut reader).await;
        assert!(ack.ok);
        write_json(
            &mut write_half,
            &ClientFrame {
                id: 0,
                request_id: None,
                page_id: Some(page_id.to_owned()),
                op: ClientOp::Subscribe {
                    topics: vec![Topic::Search],
                },
            },
        )
        .await;
        write_json(
            &mut write_half,
            &ClientFrame {
                id: 1,
                request_id: Some("same-reconnect-id".to_owned()),
                page_id: Some(page_id.to_owned()),
                op: ClientOp::Command(RemoteCommand::RunSearch {
                    ticket: 1,
                    query: page_id.to_owned(),
                    source: crate::search_source::SearchSource::All,
                }),
            },
        )
        .await;
        clients.push((ack.session_id, page_id.to_owned(), reader, write_half));
    }

    let mut observed = Vec::new();
    for _ in 0..2 {
        observed.push(
            tokio::time::timeout(Duration::from_secs(2), seen_rx.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    observed.sort_by_key(|item| item.0);
    let mut expected: Vec<_> = clients
        .iter()
        .map(|(session_id, page_id, _, _)| (*session_id, Some(page_id.clone()), 1))
        .collect();
    expected.sort_by_key(|item| item.0);
    assert_eq!(observed, expected);

    for (_, _, reader, _) in &mut clients {
        match read_json_line::<_, ServerFrame>(reader).await {
            ServerFrame::Reply { id: 1, resp } => assert!(resp.ok),
            other => panic!("expected search admission reply, got {other:?}"),
        }
    }
    hub.shutdown_all();
    #[cfg(unix)]
    let _ = std::fs::remove_file(endpoint);
}

#[tokio::test]
async fn raw_session_command_from_replaced_page_is_rejected_before_owner_dispatch() {
    let endpoint = test_endpoint("stale-command-page");
    let listener = bind(&endpoint).unwrap();
    let hub = test_hub();
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let serve_hub = Arc::clone(&hub);
    tokio::spawn(serve(
        listener,
        Arc::from("secret"),
        Arc::new(move |event| match event {
            RemoteEvent::SessionSubscribe { .. } => true,
            RemoteEvent::SessionCommand { origin, reply, .. } => {
                let accepted = seen_tx.send(origin.page_id().map(str::to_owned)).is_ok();
                let _ = reply.send(RemoteResponse::ok("accepted".to_owned()));
                accepted
            }
            _ => false,
        }),
        serve_hub,
    ));

    let conn = connect(&endpoint).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    write_json(
        &mut write_half,
        &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
    )
    .await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(ack.ok);

    for (id, page_id) in [(10, "page-a"), (11, "page-b")] {
        write_json(
            &mut write_half,
            &ClientFrame {
                id,
                request_id: None,
                page_id: Some(page_id.to_owned()),
                op: ClientOp::Subscribe {
                    topics: vec![Topic::Search],
                },
            },
        )
        .await;
    }

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 12,
            request_id: None,
            page_id: Some("page-a".to_owned()),
            op: ClientOp::Unsubscribe {
                topics: vec![Topic::Search],
            },
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id: 12, resp } => {
            assert_eq!(resp.reason.as_deref(), Some("stale_page"));
        }
        other => panic!("expected stale-page unsubscribe reply, got {other:?}"),
    }

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 13,
            request_id: None,
            page_id: Some("page-a".to_owned()),
            op: ClientOp::Command(RemoteCommand::RunSearch {
                ticket: 1,
                query: "stale".to_owned(),
                source: crate::search_source::SearchSource::All,
            }),
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id: 13, resp } => {
            assert_eq!(resp.reason.as_deref(), Some("stale_page"));
        }
        other => panic!("expected stale-page reply, got {other:?}"),
    }
    assert!(seen_rx.try_recv().is_err(), "stale command reached owner");

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 14,
            request_id: None,
            page_id: Some("page-b".to_owned()),
            op: ClientOp::Command(RemoteCommand::RunSearch {
                ticket: 2,
                query: "current".to_owned(),
                source: crate::search_source::SearchSource::All,
            }),
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id: 14, resp } => assert!(resp.ok),
        other => panic!("expected current-page reply, got {other:?}"),
    }
    assert_eq!(seen_rx.recv().await, Some(Some("page-b".to_owned())));

    hub.shutdown_all();
    #[cfg(unix)]
    let _ = std::fs::remove_file(endpoint);
}
