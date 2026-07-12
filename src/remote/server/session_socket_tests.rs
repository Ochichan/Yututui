use super::*;
use crate::remote::proto::{ClientFrame, ClientOp, HelloAck, HelloBody, ServerFrame, Topic};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::time::timeout as ttimeout;

const T: Duration = Duration::from_secs(5);

pub(super) fn test_endpoint(tag: &str) -> String {
    #[cfg(windows)]
    {
        format!(
            r"\\.\pipe\yututui-session-test-{}-{tag}",
            std::process::id()
        )
    }
    #[cfg(unix)]
    {
        let ep = std::env::temp_dir()
            .join(format!(
                "yututui-session-test-{}-{tag}.sock",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&ep);
        ep
    }
}

/// Bind + serve with a mini owner-loop stand-in: commands get a pong, subscribes go
/// through a real Publisher over a fixed one-track queue (snapshot-before-reply).
pub(super) fn start_server(tag: &str, hub: Arc<RemoteSessionHub>) -> String {
    let ep = test_endpoint(tag);
    let listener = bind(&ep).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RemoteEvent>();
    let publisher_hub = Arc::clone(&hub);
    tokio::spawn(async move {
        let mut publisher = crate::remote::publish::Publisher::new(publisher_hub);
        let mut queue = crate::queue::Queue::default();
        queue.set(
            (0..65)
                .map(|index| {
                    crate::api::Song::remote(
                        format!("vid-{index}"),
                        format!("Song {index}"),
                        "Artist",
                        "3:45",
                    )
                })
                .collect(),
            0,
        );
        while let Some(event) = rx.recv().await {
            match event {
                RemoteEvent::Command(RemoteCommand::QueueRemove { position }, reply)
                | RemoteEvent::SessionCommand {
                    command: RemoteCommand::QueueRemove { position },
                    reply,
                    ..
                } => {
                    let response = if queue.remove_at(position).is_some() {
                        RemoteResponse::ok("removed".to_string())
                    } else {
                        RemoteResponse::err("queue_index")
                    };
                    let _ = reply.send(response);
                    publisher.observe(&crate::remote::publish::test_view(&queue));
                }
                RemoteEvent::Command(_, reply) | RemoteEvent::SessionCommand { reply, .. } => {
                    let _ = reply.send(RemoteResponse::ok("pong".to_string()));
                }
                RemoteEvent::SessionSubscribe {
                    session,
                    frame_id,
                    page_id,
                    topics,
                    settlement,
                } => {
                    publisher.handle_tracked_subscribe(
                        &crate::remote::publish::test_view(&queue),
                        &session,
                        page_id.as_deref(),
                        frame_id,
                        &topics,
                        settlement,
                    );
                }
            }
        }
    });
    tokio::spawn(serve(
        listener,
        Arc::from("secret"),
        Arc::new(move |event| tx.send(event).is_ok()),
        hub,
    ));
    ep
}

/// Serve commands but deliberately retain their reply senders. This injects an
/// owner-loop/network delay without external I/O and lets the session deadline own
/// the outcome deterministically.
pub(super) fn start_stalled_command_server(
    tag: &str,
    hub: Arc<RemoteSessionHub>,
) -> (String, Arc<std::sync::Mutex<Vec<RemoteReply>>>) {
    let ep = test_endpoint(tag);
    let listener = bind(&ep).unwrap();
    let held_replies = Arc::new(std::sync::Mutex::new(Vec::<RemoteReply>::new()));
    let server_replies = Arc::clone(&held_replies);
    tokio::spawn(serve(
        listener,
        Arc::from("secret"),
        Arc::new(move |event| match event {
            RemoteEvent::Command(_, reply) | RemoteEvent::SessionCommand { reply, .. } => {
                server_replies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(reply);
                true
            }
            RemoteEvent::SessionSubscribe { .. } => false,
        }),
        hub,
    ));
    (ep, held_replies)
}

pub(super) async fn connect(ep: &str) -> Stream {
    let name = ep.to_fs_name::<GenericFilePath>().unwrap();
    // The accept loop may not be polling yet on a fresh listener; retry briefly.
    for _ in 0..50 {
        if let Ok(conn) = Stream::connect(name.clone()).await {
            return conn;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("could not connect to {ep}");
}

pub(super) async fn write_json<W: tokio::io::AsyncWrite + Unpin, S: serde::Serialize>(
    writer: &mut W,
    value: &S,
) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    ttimeout(T, writer.write_all(&bytes))
        .await
        .unwrap()
        .unwrap();
    ttimeout(T, writer.flush()).await.unwrap().unwrap();
}

pub(super) async fn read_json_line<
    R: tokio::io::AsyncBufRead + Unpin,
    D: serde::de::DeserializeOwned,
>(
    reader: &mut R,
) -> D {
    let mut line = String::new();
    let n = ttimeout(T, reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(n > 0, "unexpected EOF");
    serde_json::from_str(line.trim()).unwrap_or_else(|e| panic!("bad line {line:?}: {e}"))
}

pub(super) fn hello(version: u8, min_version: u8, token: &str) -> HelloRequest {
    HelloRequest {
        version,
        token: token.to_string(),
        hello: HelloBody {
            client: "test".to_string(),
            min_version,
        },
    }
}

/// Hello → subscribe → ping → command over one long-lived duplex connection.
#[tokio::test]
async fn session_handshake_ping_command_and_subscribe() {
    let ep = start_server("roundtrip", test_hub());
    let conn = connect(&ep).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);

    write_json(
        &mut write_half,
        &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
    )
    .await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(ack.ok, "{ack:?}");
    assert_eq!(ack.version, PROTOCOL_VERSION);
    assert!(ack.session_id > 0);
    assert!(ack.capabilities.iter().any(|c| c == "events-v8"));

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 1,
            request_id: None,
            page_id: None,
            op: ClientOp::Subscribe {
                topics: vec![Topic::Player, Topic::System],
            },
        },
    )
    .await;
    // The initial player snapshot precedes the Reply (docs/gui/02 §6); `system`
    // is event-only and produces no snapshot.
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Event { seq, topic, event } => {
            assert_eq!(seq, 1);
            assert_eq!(topic, Topic::Player);
            assert!(
                matches!(
                    event,
                    crate::remote::proto::PushEvent::PlayerSnapshot { .. }
                ),
                "got {event:?}"
            );
        }
        other => panic!("expected the initial player snapshot, got {other:?}"),
    }
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 1);
            assert!(resp.ok);
        }
        other => panic!("expected subscribe reply, got {other:?}"),
    }

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 2,
            request_id: None,
            page_id: None,
            op: ClientOp::Ping,
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Pong { id } => assert_eq!(id, 2),
        other => panic!("expected pong, got {other:?}"),
    }

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 30,
            request_id: None,
            page_id: None,
            op: ClientOp::Subscribe {
                topics: vec![Topic::Player; crate::remote::proto::REMOTE_MAX_TOPICS + 1],
            },
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 30);
            assert!(!resp.ok);
            assert_eq!(resp.reason.as_deref(), Some("too_many_topics"));
        }
        other => panic!("expected topic-cap reply, got {other:?}"),
    }

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 31,
            request_id: None,
            page_id: None,
            op: ClientOp::Command(RemoteCommand::RunSearch {
                ticket: 1,
                query: "q".repeat(crate::remote::proto::REMOTE_MAX_QUERY_BYTES + 1),
                source: crate::search_source::SearchSource::Youtube,
            }),
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 31);
            assert!(!resp.ok);
            assert_eq!(resp.reason.as_deref(), Some("query_too_long"));
        }
        other => panic!("expected command validation reply, got {other:?}"),
    }

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 3,
            request_id: None,
            page_id: None,
            op: ClientOp::Command(RemoteCommand::TogglePause),
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 3);
            assert!(resp.ok);
            assert_eq!(resp.message.as_deref(), Some("pong"));
        }
        other => panic!("expected command reply, got {other:?}"),
    }
}

#[tokio::test]
async fn timed_out_session_retry_reuses_late_completion_without_duplicate_owner_mutation() {
    let tuning = SessionTuning {
        idle_timeout: Duration::from_secs(2),
        reply_timeout: Duration::from_millis(30),
        playback_reply_timeout: Duration::from_millis(180),
        ..SessionTuning::default()
    };
    let (ep, held_replies) = start_stalled_command_server("command-timeout", test_hub_with(tuning));
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
            id: 41,
            request_id: Some("session-timeout-mutation".to_owned()),
            page_id: None,
            op: ClientOp::Command(RemoteCommand::ToggleShuffle),
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 41);
            assert!(!resp.ok);
            assert_eq!(resp.reason.as_deref(), Some("confirmation_lost"));
        }
        other => panic!("expected timeout reply, got {other:?}"),
    }
    let late_quick = held_replies.lock().unwrap().remove(0);
    assert!(
        late_quick.send(RemoteResponse::ok("late quick".to_owned())),
        "the dedupe registry must retain the late owner reply"
    );

    // A new correlation id may retry the same stable request id. It receives the first
    // execution's late response and does not enqueue a second owner mutation.
    write_json(
        &mut write_half,
        &ClientFrame {
            id: 42,
            request_id: Some("session-timeout-mutation".to_owned()),
            page_id: None,
            op: ClientOp::Command(RemoteCommand::ToggleShuffle),
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 42);
            assert!(resp.ok);
            assert_eq!(resp.message.as_deref(), Some("late quick"));
        }
        other => panic!("expected cached late reply, got {other:?}"),
    }
    assert!(
        held_replies.lock().unwrap().is_empty(),
        "retry must not reach the owner a second time"
    );

    // Reusing an identity for different semantics is rejected, never silently deduped.
    write_json(
        &mut write_half,
        &ClientFrame {
            id: 43,
            request_id: Some("session-timeout-mutation".to_owned()),
            page_id: None,
            op: ClientOp::Command(RemoteCommand::TogglePause),
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 43);
            assert_eq!(resp.reason.as_deref(), Some("request_id_conflict"));
        }
        other => panic!("expected request-id conflict, got {other:?}"),
    }

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 44,
            request_id: Some("session-timeout-playback".to_owned()),
            page_id: None,
            op: ClientOp::Command(RemoteCommand::TogglePause),
        },
    )
    .await;
    let playback_started = std::time::Instant::now();
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Reply { id, resp } => {
            assert_eq!(id, 44);
            assert!(!resp.ok);
            assert_eq!(resp.reason.as_deref(), Some("confirmation_lost"));
        }
        other => panic!("expected playback timeout reply, got {other:?}"),
    }
    assert!(
        playback_started.elapsed() >= Duration::from_millis(100),
        "playback commands must use their longer timeout class"
    );
    let late_playback = held_replies.lock().unwrap().remove(0);
    assert!(
        late_playback.send(RemoteResponse::ok("late playback".to_owned())),
        "playback timeout must also retain the late reply"
    );

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 45,
            request_id: None,
            page_id: None,
            op: ClientOp::Ping,
        },
    )
    .await;
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Pong { id } => assert_eq!(id, 45),
        other => panic!("late reply contaminated the next request: {other:?}"),
    }
}

#[tokio::test]
async fn persistent_mutation_reply_precedes_same_turn_event_repeatedly() {
    let ep = start_server("reply-before-event", test_hub());
    let conn = connect(&ep).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);

    write_json(
        &mut write_half,
        &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
    )
    .await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(ack.ok, "{ack:?}");

    write_json(
        &mut write_half,
        &ClientFrame {
            id: 1,
            request_id: None,
            page_id: None,
            op: ClientOp::Subscribe {
                topics: vec![Topic::Queue],
            },
        },
    )
    .await;
    assert!(matches!(
        read_json_line::<_, ServerFrame>(&mut reader).await,
        ServerFrame::Event {
            topic: Topic::Queue,
            ..
        }
    ));
    assert!(matches!(
        read_json_line::<_, ServerFrame>(&mut reader).await,
        ServerFrame::Reply { id: 1, .. }
    ));

    for turn in 0..16_u64 {
        let frame_id = 100 + turn;
        write_json(
            &mut write_half,
            &ClientFrame {
                id: frame_id,
                request_id: Some(format!("reply-before-event-{turn}")),
                page_id: None,
                op: ClientOp::Command(RemoteCommand::QueueRemove { position: 0 }),
            },
        )
        .await;

        match read_json_line::<_, ServerFrame>(&mut reader).await {
            ServerFrame::Reply { id, resp } => {
                assert_eq!(id, frame_id, "wrong reply id on turn {turn}");
                assert!(resp.ok, "mutation failed on turn {turn}: {resp:?}");
            }
            other => panic!("Event overtook Reply on turn {turn}: {other:?}"),
        }
        assert!(matches!(
            read_json_line::<_, ServerFrame>(&mut reader).await,
            ServerFrame::Event {
                topic: Topic::Queue,
                ..
            }
        ));
    }
}

#[tokio::test]
async fn session_hello_rejections() {
    let ep = start_server("reject", test_hub());

    // Wrong token.
    let conn = connect(&ep).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    write_json(
        &mut write_half,
        &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "nope"),
    )
    .await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(!ack.ok);
    assert_eq!(ack.reason.as_deref(), Some("bad_token"));

    // A v7-only client must not get a session (it never tries in practice — the
    // descriptor gate — but the wire answer is still bad_version).
    let conn = connect(&ep).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    write_json(&mut write_half, &hello(7, 7, "secret")).await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(!ack.ok);
    assert_eq!(ack.reason.as_deref(), Some("bad_version"));
}

#[tokio::test]
async fn ninth_session_gets_sessions_full() {
    let ep = start_server("cap", test_hub());
    let mut held = Vec::new();
    for i in 0..super::super::sessions::MAX_SESSIONS {
        let conn = connect(&ep).await;
        let (read_half, mut write_half) = tokio::io::split(conn);
        let mut reader = BufReader::new(read_half);
        write_json(
            &mut write_half,
            &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
        )
        .await;
        let ack: HelloAck = read_json_line(&mut reader).await;
        assert!(ack.ok, "session {i}: {ack:?}");
        held.push((reader, write_half));
    }
    let conn = connect(&ep).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);
    write_json(
        &mut write_half,
        &hello(PROTOCOL_VERSION, PROTOCOL_VERSION, "secret"),
    )
    .await;
    let ack: HelloAck = read_json_line(&mut reader).await;
    assert!(!ack.ok);
    assert_eq!(ack.reason.as_deref(), Some("sessions_full"));
}

#[tokio::test]
async fn shutdown_latch_closes_a_prehandshake_connection_promptly() {
    let hub = test_hub();
    let ep = start_server("shutdown-latch", Arc::clone(&hub));
    let conn = connect(&ep).await;
    let (read_half, mut write_half) = tokio::io::split(conn);
    let mut reader = BufReader::new(read_half);

    // Keep the accepted child blocked inside its first-line read. Hub shutdown must wake the
    // accept owner, which aborts and joins this pre-handshake task instead of leaving it until
    // READ_TIMEOUT.
    ttimeout(T, write_half.write_all(b"{"))
        .await
        .unwrap()
        .unwrap();
    ttimeout(T, write_half.flush()).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    hub.shutdown_all();

    let mut line = String::new();
    let read = ttimeout(Duration::from_millis(200), reader.read_line(&mut line))
        .await
        .expect("shutdown must close a partial handshake before its read deadline");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "a shutdown connection must not produce a protocol frame: {line:?}"
    );
    assert_eq!(hub.active(), 0);
}

#[tokio::test]
async fn shutdown_cancels_pending_command_without_waiting_for_reply_timeout() {
    let tuning = SessionTuning {
        idle_timeout: Duration::from_secs(5),
        reply_timeout: Duration::from_secs(5),
        playback_reply_timeout: Duration::from_secs(5),
        write_timeout: Duration::from_millis(100),
        ..SessionTuning::default()
    };
    let hub = test_hub_with(tuning);
    let (ep, held_replies) = start_stalled_command_server("cancel-command", Arc::clone(&hub));
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
            id: 77,
            request_id: Some("cancel-on-shutdown".to_string()),
            page_id: None,
            op: ClientOp::Command(RemoteCommand::TogglePause),
        },
    )
    .await;
    ttimeout(T, async {
        loop {
            if !held_replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("command must reach the dedupe owner");

    let started = std::time::Instant::now();
    hub.shutdown_all();
    match ttimeout(
        Duration::from_millis(500),
        read_json_line::<_, ServerFrame>(&mut reader),
    )
    .await
    .expect("shutdown must beat the five-second command deadline")
    {
        ServerFrame::Goodbye { reason } => assert_eq!(reason, "shutting_down"),
        other => panic!("expected shutdown goodbye, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(1));

    let late_reply = held_replies.lock().unwrap().remove(0);
    assert!(
        late_reply.send(RemoteResponse::ok("late".to_string())),
        "session cancellation must not cancel the dedupe completion receiver"
    );
}

#[tokio::test]
async fn oversized_session_frame_is_rejected_at_the_wire_cap() {
    let ep = start_server("oversized-frame", test_hub());
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

    let oversized = vec![b'x'; super::super::sessions::SESSION_MAX_FRAME_BYTES + 1];
    ttimeout(T, write_half.write_all(&oversized))
        .await
        .unwrap()
        .unwrap();
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Goodbye { reason } => assert_eq!(reason, "bad_request"),
        other => panic!("expected oversized-frame goodbye, got {other:?}"),
    }
}

#[tokio::test]
async fn idle_session_is_garbage_collected_with_goodbye() {
    let tuning = SessionTuning {
        idle_timeout: Duration::from_millis(200),
        ..SessionTuning::default()
    };
    let ep = start_server("idle", test_hub_with(tuning));
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

    // Send nothing: the server GCs us with a best-effort goodbye, then EOF.
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Goodbye { reason } => assert_eq!(reason, "idle_timeout"),
        other => panic!("expected goodbye, got {other:?}"),
    }
    let mut rest = String::new();
    let n = ttimeout(T, reader.read_line(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "connection must close after goodbye, got {rest:?}");
}

#[tokio::test]
async fn malformed_session_frame_gets_goodbye_bad_request() {
    let ep = start_server("badframe", test_hub());
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

    ttimeout(T, write_half.write_all(b"{not a frame}\n"))
        .await
        .unwrap()
        .unwrap();
    ttimeout(T, write_half.flush()).await.unwrap().unwrap();
    match read_json_line::<_, ServerFrame>(&mut reader).await {
        ServerFrame::Goodbye { reason } => assert_eq!(reason, "bad_request"),
        other => panic!("expected goodbye, got {other:?}"),
    }
}

#[tokio::test]
async fn publisher_shutdown_wakes_the_accept_owner_to_completion() {
    let ep = test_endpoint("accept-owner-shutdown");
    let listener = bind(&ep).unwrap();
    let hub = test_hub();
    let serve_task = tokio::spawn(serve(
        listener,
        Arc::from("secret"),
        Arc::new(|_| true),
        Arc::clone(&hub),
    ));
    tokio::task::yield_now().await;

    crate::remote::publish::Publisher::new(Arc::clone(&hub)).shutting_down();

    ttimeout(Duration::from_millis(200), serve_task)
        .await
        .expect("publisher shutdown must wake a pending accept")
        .expect("accept owner must not panic");
    assert!(hub.is_shutting_down());
    #[cfg(unix)]
    let _ = std::fs::remove_file(ep);
}
