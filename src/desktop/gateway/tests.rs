use super::*;
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::Listener;
use tokio::io::AsyncBufReadExt;

fn test_instance(endpoint: String, token: &str) -> InstanceFile {
    InstanceFile {
        app_pid: std::process::id(),
        endpoint,
        token: token.to_string(),
        created_unix: 1,
        mode: InstanceMode::Daemon,
        protocol_version: PROTOCOL_VERSION,
        capabilities: vec!["events-v8".to_string()],
    }
}

fn bind(endpoint: &str) -> Listener {
    let _ = std::fs::remove_file(endpoint);
    let name = endpoint.to_fs_name::<GenericFilePath>().unwrap();
    ListenerOptions::new().name(name).create_tokio().unwrap()
}

async fn serve_hello(listener: Listener, ack: HelloAck) {
    let conn = listener.accept().await.unwrap();
    let mut reader = BufReader::new(&conn);
    let mut hello_line = String::new();
    reader.read_line(&mut hello_line).await.unwrap();
    // The client greeted us with a v8 Hello carrying the shared token.
    assert!(hello_line.contains("\"hello\""));
    let mut w = &conn;
    let mut buf = serde_json::to_vec(&ack).unwrap();
    buf.push(b'\n');
    w.write_all(&buf).await.unwrap();
    w.flush().await.unwrap();
    // Hold the connection briefly so the client sees an established session.
    let _ = tokio::time::timeout(
        Duration::from_millis(50),
        reader.read_line(&mut String::new()),
    )
    .await;
}

#[tokio::test]
async fn hello_handshake_succeeds() {
    let endpoint = std::env::temp_dir()
        .join(format!("ytt-gw-ok-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let listener = bind(&endpoint);
    let ack = HelloAck {
        ok: true,
        version: 8,
        session_id: 1,
        capabilities: vec!["events-v8".to_string()],
        owner_mode: InstanceMode::Daemon,
        reason: None,
    };
    let server = tokio::spawn(serve_hello(listener, ack));
    let result = connect_and_hello(test_instance(endpoint.clone(), "tok")).await;
    server.abort();
    let _ = std::fs::remove_file(&endpoint);
    let (_conn, ack) = result.expect("handshake should succeed");
    assert_eq!(ack.version, 8);
    assert!(ack.capabilities.contains(&"events-v8".to_string()));
}

#[tokio::test]
async fn hello_rejection_surfaces_the_reason() {
    let endpoint = std::env::temp_dir()
        .join(format!("ytt-gw-bad-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let listener = bind(&endpoint);
    let ack = HelloAck {
        ok: false,
        version: 8,
        session_id: 0,
        capabilities: vec![],
        owner_mode: InstanceMode::Daemon,
        reason: Some("bad_token".to_string()),
    };
    let server = tokio::spawn(serve_hello(listener, ack));
    let err = connect_and_hello(test_instance(endpoint.clone(), "wrong"))
        .await
        .unwrap_err();
    server.abort();
    let _ = std::fs::remove_file(&endpoint);
    assert_eq!(err, "bad_token");
}

#[tokio::test]
async fn missing_core_is_reported() {
    let err = connect_and_hello(test_instance(
        std::env::temp_dir()
            .join("ytt-gw-nope.sock")
            .to_string_lossy()
            .into_owned(),
        "tok",
    ))
    .await
    .unwrap_err();
    assert_eq!(err, "connect_failed");
}

/// Accept one connection and return the first line the client writes.
async fn accept_one_line(listener: Listener) -> String {
    let conn = listener.accept().await.unwrap();
    let mut reader = BufReader::new(&conn);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    line
}

async fn connect(endpoint: &str) -> Stream {
    let name = endpoint.to_fs_name::<GenericFilePath>().unwrap();
    Stream::connect(name).await.unwrap()
}

#[tokio::test]
async fn forward_cmd_translates_and_rewrites_the_id() {
    let endpoint = std::env::temp_dir()
        .join(format!("ytt-gw-fwd-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let listener = bind(&endpoint);
    let server = tokio::spawn(accept_one_line(listener));
    let conn = connect(&endpoint).await;

    // The frontend's id is not the wire correlation id; its stable retry identity is bound to
    // the page lifetime while the session allocates its own monotonically increasing id.
    let env = OutEnvelope {
        v: 1,
        id: Some(99),
        page_id: Some("test-page".to_string()),
        request_id: Some("gui:test-page:99".to_string()),
        kind: OutKind::Cmd,
        name: "seek_to".to_string(),
        payload: serde_json::json!({ "ms": 1234 }),
    };
    let mut next_id = 5u64;
    let mut pending = HashMap::new();
    let reason = forward_command(
        &conn,
        env,
        &mut next_id,
        &mut pending,
        &|_| {},
        "test-gateway",
    )
    .await;
    assert!(reason.is_none(), "a good write must not end the session");

    let line = server.await.unwrap();
    let _ = std::fs::remove_file(&endpoint);
    let frame: ClientFrame = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(
        frame.id, 5,
        "wire id is session-allocated, not the frontend's 99"
    );
    assert_eq!(
        frame.op,
        ClientOp::Command(RemoteCommand::SeekTo { ms: 1234 })
    );
    assert_eq!(
        frame.request_id.as_deref(),
        Some("page:test-page:gui:test-page:99")
    );
    assert_eq!(frame.page_id.as_deref(), Some("test-page"));
    assert_eq!(next_id, 6, "id counter advanced");
    assert_eq!(
        pending.get(&5),
        Some(&FrontendCorrelation {
            page_id: Some("test-page".to_string()),
            id: 99,
            mutation: true,
        })
    );
}

#[tokio::test]
async fn forward_req_records_the_reply_correlation() {
    let endpoint = std::env::temp_dir()
        .join(format!("ytt-gw-req-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let listener = bind(&endpoint);
    let server = tokio::spawn(accept_one_line(listener));
    let conn = connect(&endpoint).await;

    let env = OutEnvelope {
        v: 1,
        id: Some(77),
        page_id: None,
        request_id: None,
        kind: OutKind::Req,
        name: "status".to_string(),
        payload: serde_json::Value::Null,
    };
    let mut next_id = 1u64;
    let mut pending = HashMap::new();
    forward_command(
        &conn,
        env,
        &mut next_id,
        &mut pending,
        &|_| {},
        "test-gateway",
    )
    .await;

    let line = server.await.unwrap();
    let _ = std::fs::remove_file(&endpoint);
    let frame: ClientFrame = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(frame.id, 1);
    assert_eq!(frame.request_id.as_deref(), Some("test-gateway:client:77"));
    assert_eq!(frame.op, ClientOp::Command(RemoteCommand::Status));
    // Session id 1 maps back to the frontend's request id 77 so its reply can be routed.
    assert_eq!(
        pending.get(&1),
        Some(&FrontendCorrelation {
            page_id: None,
            id: 77,
            mutation: false,
        })
    );
}
