use std::future::pending;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use super::*;
use crate::remote::proto::{InstanceMode, RemoteResponse};

struct StallingWriter {
    started: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Write for StallingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
            self.release.recv().map_err(io::Error::other)?;
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BrokenWriter;

impl Write for BrokenWriter {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RecordingWriter {
    bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Write for RecordingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn config(topics: Vec<Topic>) -> SessionConfig {
    SessionConfig {
        token: "0123456789abcdef0123456789abcdef".to_string(),
        topics,
        json: true,
        quiet: false,
        io_timeout: Duration::from_secs(1),
        ping_interval: Duration::from_secs(30),
    }
}

fn ack() -> HelloAck {
    HelloAck {
        ok: true,
        version: PROTOCOL_VERSION,
        session_id: 7,
        capabilities: vec!["events-v8".to_string()],
        owner_mode: InstanceMode::Daemon,
        reason: None,
    }
}

async fn server_handshake<S>(
    stream: S,
    expected_topics: &[Topic],
) -> (BufReader<tokio::io::ReadHalf<S>>, tokio::io::WriteHalf<S>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let hello: HelloRequest = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(hello.version, PROTOCOL_VERSION);
    assert_eq!(hello.token, "0123456789abcdef0123456789abcdef");
    assert_eq!(hello.hello.client, "ytt-cli-watch");
    assert_eq!(hello.hello.min_version, PROTOCOL_VERSION);
    write_test_line(&mut write_half, &ack()).await;

    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let subscribe: ClientFrame = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(subscribe.id, SUBSCRIBE_ID);
    assert_eq!(
        subscribe.op,
        ClientOp::Subscribe {
            topics: expected_topics.to_vec()
        }
    );
    (reader, write_half)
}

async fn write_test_line<W: AsyncWrite + Unpin, T: serde::Serialize>(writer: &mut W, value: &T) {
    let mut bytes = serde_json::to_vec(value).unwrap();
    bytes.push(b'\n');
    writer.write_all(&bytes).await.unwrap();
    writer.flush().await.unwrap();
}

fn system_event(seq: u64, event: PushEvent) -> ServerFrame {
    ServerFrame::Event {
        seq,
        topic: Topic::System,
        event,
    }
}

#[tokio::test]
async fn immediate_cancel_wins_before_operation_is_polled() {
    let operation_polled = std::cell::Cell::new(false);
    let operation = std::future::poll_fn(|_| {
        operation_polled.set(true);
        std::task::Poll::Ready(EXIT_TRANSPORT)
    });
    let cancel = std::future::ready(Ok::<(), io::Error>(()));

    assert_eq!(race_operation_with_cancel(operation, cancel).await, EXIT_OK);
    assert!(!operation_polled.get());
}

#[tokio::test]
async fn cancel_during_pre_ack_handshake_exits_successfully() {
    let (client, server) = tokio::io::duplex(4096);
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let operation = async move {
        let mut emit = |_line: &str| Ok(());
        match run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        )
        .await
        {
            Ok(()) => EXIT_OK,
            Err(error) => error.exit_code(),
        }
    };
    let cancel = async move {
        cancel_rx.await.map_err(io::Error::other)?;
        Ok(())
    };
    let server_fut = async move {
        let (read_half, _write_half) = tokio::io::split(server);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str::<HelloRequest>(line.trim()).unwrap();
        cancel_tx.send(()).unwrap();

        line.clear();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    };

    let (exit_code, ()) = tokio::join!(race_operation_with_cancel(operation, cancel), server_fut);
    assert_eq!(exit_code, EXIT_OK);
}

#[tokio::test]
async fn outer_cancel_interrupts_stalled_output_drain() {
    let (client, server) = tokio::io::duplex(4096);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = StallingWriter {
        started: Some(started_tx),
        release: release_rx,
    };
    let operation = async move {
        let _ = run_session_with_output(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            writer,
            4,
            Duration::from_secs(5),
        )
        .await;
        EXIT_TRANSPORT
    };
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let cancel = async move {
        cancel_rx.await.map_err(io::Error::other)?;
        Ok(())
    };
    let server_fut = async move {
        let (_reader, mut socket_writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut socket_writer,
            &system_event(
                1,
                PushEvent::OwnerChanged {
                    mode: InstanceMode::Daemon,
                },
            ),
        )
        .await;
        write_test_line(
            &mut socket_writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;
        write_test_line(
            &mut socket_writer,
            &ServerFrame::Goodbye {
                reason: "shutting_down".to_string(),
            },
        )
        .await;

        started_rx.await.unwrap();
        cancel_tx.send(()).unwrap();
        tokio::task::yield_now().await;
        release_tx.send(()).unwrap();
    };

    let (exit_code, ()) = tokio::join!(race_operation_with_cancel(operation, cancel), server_fut);
    assert_eq!(exit_code, EXIT_OK);
}

#[tokio::test]
async fn stalled_output_flush_times_out_as_transport_error() {
    let (client, server) = tokio::io::duplex(4096);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = StallingWriter {
        started: Some(started_tx),
        release: release_rx,
    };
    let client_fut = run_session_with_output(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        writer,
        4,
        Duration::from_millis(50),
    );
    let server_fut = async move {
        let (_reader, mut socket_writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut socket_writer,
            &system_event(
                1,
                PushEvent::OwnerChanged {
                    mode: InstanceMode::Daemon,
                },
            ),
        )
        .await;
        write_test_line(
            &mut socket_writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;
        write_test_line(
            &mut socket_writer,
            &ServerFrame::Goodbye {
                reason: "shutting_down".to_string(),
            },
        )
        .await;
        started_rx.await.unwrap();
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    release_tx.send(()).unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("timed out flushing"));
}

#[tokio::test]
async fn full_output_lane_is_nonblocking_and_reports_slow_consumer() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = StallingWriter {
        started: Some(started_tx),
        release: release_rx,
    };
    let (line_tx, mut done_rx) = spawn_output_writer(writer, 1).unwrap();

    enqueue_output_line(&line_tx, "first").unwrap();
    started_rx.await.unwrap();
    enqueue_output_line(&line_tx, "second").unwrap();
    let error = enqueue_output_line(&line_tx, "third").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

    drop(line_tx);
    release_tx.send(()).unwrap();
    let completion = timeout(Duration::from_secs(1), &mut done_rx).await.unwrap();
    assert_eq!(output_completion(completion), Ok(()));
}

#[tokio::test]
async fn broken_output_writer_is_transport_error() {
    let (client, server) = tokio::io::duplex(4096);
    let client_fut = run_session_with_output(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        BrokenWriter,
        4,
        Duration::from_secs(1),
    );
    let server_fut = async move {
        let (_reader, mut socket_writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut socket_writer,
            &system_event(
                1,
                PushEvent::OwnerChanged {
                    mode: InstanceMode::Daemon,
                },
            ),
        )
        .await;
        write_test_line(
            &mut socket_writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("output"));
}

#[tokio::test]
async fn abnormal_goodbye_drains_accepted_output_in_order() {
    let (client, server) = tokio::io::duplex(4096);
    let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = RecordingWriter {
        bytes: std::sync::Arc::clone(&bytes),
    };
    let client_fut = run_session_with_output(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        writer,
        4,
        Duration::from_secs(1),
    );
    let event = system_event(
        1,
        PushEvent::OwnerChanged {
            mode: InstanceMode::Daemon,
        },
    );
    let goodbye = ServerFrame::Goodbye {
        reason: "slow_consumer".to_string(),
    };
    let expected = format!(
        "{}\n{}\n",
        serde_json::to_string(&event).unwrap(),
        serde_json::to_string(&goodbye).unwrap()
    );
    let server_fut = async move {
        let (_reader, mut socket_writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(&mut socket_writer, &event).await;
        write_test_line(
            &mut socket_writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;
        write_test_line(&mut socket_writer, &goodbye).await;
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert!(error.message.contains("slow_consumer"));
    let recorded = bytes.lock().unwrap().clone();
    assert_eq!(std::str::from_utf8(&recorded).unwrap(), expected);
}

#[tokio::test]
async fn accepts_snapshot_before_subscribe_reply_and_json_filters_frames() {
    let (client, server) = tokio::io::duplex(16 * 1024);
    let mut lines = Vec::new();
    let mut emit = |line: &str| -> io::Result<()> {
        lines.push(line.to_string());
        Ok(())
    };
    let client_fut = run_session(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut writer,
            &system_event(
                1,
                PushEvent::OwnerChanged {
                    mode: InstanceMode::Daemon,
                },
            ),
        )
        .await;
        write_test_line(
            &mut writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;
        write_test_line(&mut writer, &system_event(2, PushEvent::ShuttingDown)).await;
        write_test_line(
            &mut writer,
            &ServerFrame::Goodbye {
                reason: "shutting_down".to_string(),
            },
        )
        .await;
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    assert_eq!(result, Ok(()));
    assert_eq!(lines.len(), 3);
    assert!(lines[0].contains(r#""seq":1"#));
    assert!(lines[1].contains(r#""kind":"shutting_down""#));
    assert_eq!(lines[2], r#"{"frame":"goodbye","reason":"shutting_down"}"#);
    assert!(
        lines
            .iter()
            .all(|line| !line.contains(r#""frame":"reply""#))
    );
}

#[tokio::test]
async fn sends_ping_and_suppresses_pong() {
    let (client, server) = tokio::io::duplex(16 * 1024);
    let mut test_config = config(vec![Topic::System]);
    test_config.ping_interval = Duration::from_millis(10);
    let mut lines = Vec::new();
    let mut emit = |line: &str| -> io::Result<()> {
        lines.push(line.to_string());
        Ok(())
    };
    let client_fut = run_session(
        client,
        test_config,
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (mut reader, mut writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;

        let mut line = String::new();
        timeout(Duration::from_secs(1), reader.read_line(&mut line))
            .await
            .expect("client should ping")
            .unwrap();
        let ping: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(ping.id, SUBSCRIBE_ID + 1);
        assert_eq!(ping.op, ClientOp::Ping);
        write_test_line(&mut writer, &ServerFrame::Pong { id: ping.id }).await;
        write_test_line(
            &mut writer,
            &ServerFrame::Goodbye {
                reason: "shutting_down".to_string(),
            },
        )
        .await;
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    assert_eq!(result, Ok(()));
    assert_eq!(
        lines,
        vec![r#"{"frame":"goodbye","reason":"shutting_down"}"#]
    );
}

#[tokio::test]
async fn rejects_event_sequence_gap() {
    let (client, server) = tokio::io::duplex(16 * 1024);
    let mut emit = |_line: &str| Ok(());
    let client_fut = run_session(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut writer,
            &system_event(
                1,
                PushEvent::OwnerChanged {
                    mode: InstanceMode::Daemon,
                },
            ),
        )
        .await;
        write_test_line(
            &mut writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;
        write_test_line(&mut writer, &system_event(3, PushEvent::ShuttingDown)).await;
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("sequence gap"));
}

#[tokio::test]
async fn rejects_duplicate_event_sequence() {
    let (client, server) = tokio::io::duplex(16 * 1024);
    let mut emit = |_line: &str| Ok(());
    let client_fut = run_session(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut writer,
            &system_event(
                1,
                PushEvent::OwnerChanged {
                    mode: InstanceMode::Daemon,
                },
            ),
        )
        .await;
        write_test_line(
            &mut writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;
        write_test_line(&mut writer, &system_event(1, PushEvent::ShuttingDown)).await;
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("sequence gap"));
}

#[tokio::test]
async fn malformed_regular_frame_is_transport_error() {
    let (client, server) = tokio::io::duplex(4096);
    let mut emit = |_line: &str| Ok(());
    let client_fut = run_session(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;
        writer.write_all(b"{not-json}\n").await.unwrap();
        writer.flush().await.unwrap();
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("malformed frame"));
}

#[tokio::test]
async fn partial_eof_after_shutting_down_is_transport_error() {
    let (client, server) = tokio::io::duplex(4096);
    let mut emit = |_line: &str| Ok(());
    let client_fut = run_session(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut writer,
            &system_event(
                1,
                PushEvent::OwnerChanged {
                    mode: InstanceMode::Daemon,
                },
            ),
        )
        .await;
        write_test_line(
            &mut writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;
        write_test_line(&mut writer, &system_event(2, PushEvent::ShuttingDown)).await;
        writer.write_all(b"{\"frame\":\"goodbye\"").await.unwrap();
        writer.flush().await.unwrap();
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("malformed partial frame"));
}

#[tokio::test]
async fn rejects_oversized_frame_without_waiting_for_newline() {
    let (client, server) = tokio::io::duplex(16 * 1024);
    let mut emit = |_line: &str| Ok(());
    let client_fut = run_session(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        let _ = writer.write_all(&oversized).await;
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("oversized"));
}

#[tokio::test]
async fn capacity_handshake_rejection_is_transport_error() {
    let (client, server) = tokio::io::duplex(4096);
    let mut emit = |_line: &str| Ok(());
    let client_fut = run_session(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (read_half, mut write_half) = tokio::io::split(server);
        let mut reader = BufReader::new(read_half);
        let mut hello = String::new();
        reader.read_line(&mut hello).await.unwrap();
        let rejected = HelloAck {
            ok: false,
            version: PROTOCOL_VERSION,
            session_id: 0,
            capabilities: Vec::new(),
            owner_mode: InstanceMode::Daemon,
            reason: Some("sessions_full".to_string()),
        };
        write_test_line(&mut write_half, &rejected).await;
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("sessions_full"));
}

#[test]
fn handshake_rejection_reason_is_sanitized_and_bounded() {
    let rejected = HelloAck {
        ok: false,
        version: PROTOCOL_VERSION,
        session_id: 0,
        capabilities: Vec::new(),
        owner_mode: InstanceMode::Daemon,
        reason: Some(format!(
            "sessions_full\n\u{1b}]0;watch-owned-{}\u{7}",
            "x".repeat(100)
        )),
    };

    let error = validate_ack(&rejected).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(!error.message.chars().any(char::is_control));
    assert!(error.message.contains('…'));
}

#[tokio::test]
async fn subscription_rejection_reason_is_sanitized_and_bounded() {
    let (client, server) = tokio::io::duplex(4096);
    let mut emit = |_line: &str| Ok(());
    let client_fut = run_session(
        client,
        config(vec![Topic::System]),
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
        let reason = format!("rejected\r\n\u{1b}]0;watch-owned-{}\u{7}", "y".repeat(100));
        write_test_line(
            &mut writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::err(&reason),
            },
        )
        .await;
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(!error.message.chars().any(char::is_control));
    assert!(error.message.contains('…'));
}

#[test]
fn protocol_handshake_rejection_is_usage_error() {
    let rejected = HelloAck {
        ok: false,
        version: PROTOCOL_VERSION,
        session_id: 0,
        capabilities: Vec::new(),
        owner_mode: InstanceMode::Daemon,
        reason: Some("bad_version".to_string()),
    };
    let error = validate_ack(&rejected).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(error.message.contains("bad_version"));
}

#[tokio::test]
async fn unanswered_ping_fails_on_io_deadline() {
    let (client, server) = tokio::io::duplex(4096);
    let mut test_config = config(vec![Topic::System]);
    test_config.io_timeout = Duration::from_millis(100);
    test_config.ping_interval = Duration::from_millis(10);
    let mut emit = |_line: &str| Ok(());
    let client_fut = run_session(
        client,
        test_config,
        pending::<Result<(), WatchError>>(),
        &mut emit,
    );
    let server_fut = async move {
        let (mut reader, mut writer) = server_handshake(server, &[Topic::System]).await;
        write_test_line(
            &mut writer,
            &ServerFrame::Reply {
                id: SUBSCRIBE_ID,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        )
        .await;

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let ping: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(ping.op, ClientOp::Ping);
        line.clear();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    };

    let (result, ()) = tokio::join!(client_fut, server_fut);
    let error = result.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("answering pings"));
}

#[tokio::test]
async fn slow_consumer_and_idle_goodbyes_are_transport_errors() {
    for reason in ["slow_consumer", "idle_timeout"] {
        let (client, server) = tokio::io::duplex(4096);
        let mut lines = Vec::new();
        let mut emit = |line: &str| -> io::Result<()> {
            lines.push(line.to_string());
            Ok(())
        };
        let client_fut = run_session(
            client,
            config(vec![Topic::System]),
            pending::<Result<(), WatchError>>(),
            &mut emit,
        );
        let server_fut = async move {
            let (_reader, mut writer) = server_handshake(server, &[Topic::System]).await;
            write_test_line(
                &mut writer,
                &ServerFrame::Reply {
                    id: SUBSCRIBE_ID,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                },
            )
            .await;
            write_test_line(
                &mut writer,
                &ServerFrame::Goodbye {
                    reason: reason.to_string(),
                },
            )
            .await;
        };

        let (result, ()) = tokio::join!(client_fut, server_fut);
        let error = result.unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transport);
        assert!(error.message.contains(reason));
        assert_eq!(lines.len(), 1, "JSON should retain the Goodbye frame");
        assert!(lines[0].contains(reason));
    }
}

#[test]
fn topic_validation_deduplicates_and_rejects_non_watch_topics() {
    assert_eq!(
        normalize_topics(vec![Topic::Player, Topic::Player, Topic::System]).unwrap(),
        vec![Topic::Player, Topic::System]
    );
    let error = normalize_topics(vec![Topic::Lyrics]).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
}

#[test]
fn first_event_sequence_must_start_at_one() {
    let mut last_seq = None;
    let error = validate_event(
        2,
        Topic::System,
        &PushEvent::OwnerChanged {
            mode: InstanceMode::Daemon,
        },
        &[Topic::System],
        &mut last_seq,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Transport);
    assert!(error.message.contains("sequence gap"));
    assert_eq!(last_seq, None);
}

#[test]
fn sanitizer_removes_terminal_controls_and_bounds_fields() {
    assert_eq!(sanitize("song\n\u{1b}[31m  title", 80), "song [31m title");
    assert_eq!(sanitize("left\u{202e}right", 80), "left right");
    assert_eq!(sanitize("abcdef", 3), "abc…");
}

#[test]
fn quiet_suppresses_human_output_but_json_wins() {
    let frame = system_event(
        1,
        PushEvent::OwnerChanged {
            mode: InstanceMode::Daemon,
        },
    );
    let mut lines = Vec::new();
    {
        let mut emit = |line: &str| -> io::Result<()> {
            lines.push(line.to_string());
            Ok(())
        };
        emit_frame(&frame, false, true, &mut emit).unwrap();
    }
    assert!(lines.is_empty());
    {
        let mut emit = |line: &str| -> io::Result<()> {
            lines.push(line.to_string());
            Ok(())
        };
        emit_frame(&frame, true, true, &mut emit).unwrap();
    }
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains(r#""frame":"event""#));
}
