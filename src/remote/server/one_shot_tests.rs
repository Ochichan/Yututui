use super::*;
use crate::remote::proto::{RemoteCommand, RemoteResponseEnvelope};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

struct PrivateEndpointFixture {
    root: PathBuf,
    socket: String,
    descriptor: PathBuf,
}

impl PrivateEndpointFixture {
    fn new(label: &str) -> Self {
        let nonce = crate::remote::endpoint::gen_token().expect("test endpoint nonce");
        let kind = label
            .bytes()
            .find(u8::is_ascii_alphanumeric)
            .map(char::from)
            .unwrap_or('x');
        // Unix socket names have a small fixed sun_path budget, and the process temp directory
        // can itself be deeply nested (notably on macOS). Use a short, unpredictable child of
        // /tmp; the socket and descriptor still live below a dedicated 0700 boundary rather
        // than directly in the shared sticky directory.
        let root = Path::new("/tmp").join(format!("yt{kind}-{:x}-{nonce}", std::process::id()));
        crate::util::safe_fs::ensure_private_dir(&root).expect("private endpoint directory");
        let socket = root.join("endpoint.sock").to_string_lossy().into_owned();
        let descriptor = root.join("instance.json");
        Self {
            root,
            socket,
            descriptor,
        }
    }
}

impl Drop for PrivateEndpointFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Connect to `path`, send one line, return the one-line response.
async fn send_line(path: &str, line: &str) -> String {
    let name = path.to_fs_name::<GenericFilePath>().unwrap();
    let conn = Stream::connect(name).await.unwrap();
    {
        let mut writer = &conn;
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();
    }
    let mut reader = BufReader::new(&conn);
    let mut resp = String::new();
    reader.read_line(&mut resp).await.unwrap();
    resp
}

fn parse(resp: &str) -> RemoteResponse {
    serde_json::from_str(resp.trim()).unwrap()
}

#[tokio::test]
async fn one_shot_reports_server_busy_when_owner_rejects() {
    let req = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "secret".to_string(),
        request_id: None,
        command: RemoteCommand::TogglePause,
    };
    let emit: EventSink = Arc::new(|_| false);
    let hub = test_hub();

    let resp = build_response(req, "secret", &emit, &hub).await;

    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("server_busy"));
}

#[tokio::test]
async fn one_shot_run_search_validates_before_requiring_a_session() {
    let emits = Arc::new(AtomicUsize::new(0));
    let sink_emits = Arc::clone(&emits);
    let emit: EventSink = Arc::new(move |_| {
        sink_emits.fetch_add(1, Ordering::Relaxed);
        true
    });
    let request = |query: String| RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "secret".to_owned(),
        request_id: Some("one-shot-search".to_owned()),
        command: RemoteCommand::RunSearch {
            ticket: 1,
            query,
            source: crate::search_source::SearchSource::Youtube,
        },
    };

    let oversized = build_response(
        request("q".repeat(crate::remote::proto::REMOTE_MAX_QUERY_BYTES + 1)),
        "secret",
        &emit,
        &test_hub(),
    )
    .await;
    assert_eq!(oversized.reason.as_deref(), Some("query_too_long"));

    let valid = build_response(request("valid".to_owned()), "secret", &emit, &test_hub()).await;
    assert_eq!(valid.reason.as_deref(), Some("session_required"));
    assert_eq!(emits.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn v7_one_shot_rejects_v8_only_export_before_owner_admission() {
    let emits = Arc::new(AtomicUsize::new(0));
    let sink_emits = Arc::clone(&emits);
    let emit: EventSink = Arc::new(move |_| {
        sink_emits.fetch_add(1, Ordering::Relaxed);
        true
    });
    let request = RemoteRequest {
        version: PROTOCOL_VERSION_V7,
        token: "secret".to_owned(),
        request_id: None,
        command: RemoteCommand::ExportPersonalData {
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
        },
    };

    let response = build_response(request, "secret", &emit, &test_hub()).await;

    assert_eq!(response.reason.as_deref(), Some("bad_version"));
    assert_eq!(emits.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn one_shot_reports_server_busy_when_retained_request_cache_is_saturated() {
    let hub = test_hub();
    hub.requests.fill_with_completed_for_test();

    let emits = Arc::new(AtomicUsize::new(0));
    let emits_for_sink = Arc::clone(&emits);
    let emit: EventSink = Arc::new(move |_| {
        emits_for_sink.fetch_add(1, Ordering::Relaxed);
        true
    });
    let req = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "secret".to_owned(),
        request_id: Some("new-while-retained".to_owned()),
        command: RemoteCommand::ToggleShuffle,
    };

    let response = build_response(req, "secret", &emit, &hub).await;

    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("server_busy"));
    assert_eq!(emits.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn one_shot_marks_only_a_retained_same_id_outcome_as_replayed() {
    let path = std::env::temp_dir()
        .join(format!(
            "yututui-remote-replay-proof-test-{}.sock",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let listener = bind(&path).unwrap();

    let executions = Arc::new(AtomicUsize::new(0));
    let executions_for_sink = Arc::clone(&executions);
    tokio::spawn(serve(
        listener,
        Arc::from("secret"),
        Arc::new(move |event| {
            let RemoteEvent::Command(_, reply) = event else {
                return false;
            };
            executions_for_sink.fetch_add(1, Ordering::Relaxed);
            reply.send(RemoteResponse::ok("applied".to_owned()))
        }),
        test_hub(),
    ));

    let request = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "secret".to_owned(),
        request_id: Some("same-mutation".to_owned()),
        command: RemoteCommand::TogglePause,
    };
    let line = serde_json::to_string(&request).unwrap();
    let first: RemoteResponseEnvelope =
        serde_json::from_str(send_line(&path, &line).await.trim()).unwrap();
    let replay: RemoteResponseEnvelope =
        serde_json::from_str(send_line(&path, &line).await.trim()).unwrap();

    assert!(first.response.ok);
    assert!(!first.retained_replay);
    assert!(replay.response.ok);
    assert!(replay.retained_replay);
    assert_eq!(executions.load(Ordering::Relaxed), 1);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn one_shot_rejects_immediately_after_shutdown_latch() {
    let req = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "secret".to_string(),
        request_id: Some("after-shutdown".to_string()),
        command: RemoteCommand::TogglePause,
    };
    let emits = Arc::new(AtomicUsize::new(0));
    let emits_for_sink = Arc::clone(&emits);
    let emit: EventSink = Arc::new(move |_| {
        emits_for_sink.fetch_add(1, Ordering::Relaxed);
        true
    });
    let hub = test_hub();
    hub.shutdown_all();

    let resp = build_response(req, "secret", &emit, &hub).await;

    assert_eq!(resp.reason.as_deref(), Some("shutting_down"));
    assert_eq!(emits.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn one_shot_response_write_is_deadline_bounded() {
    let (mut writer, _non_reading_peer) = tokio::io::duplex(1);
    let response = RemoteResponse::ok("x".repeat(64 * 1024));

    let error = write_response_to(&mut writer, &response, Duration::from_millis(5))
        .await
        .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn server_round_trips_request_through_the_reducer() {
    let path = std::env::temp_dir()
        .join(format!("yututui-remote-test-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let listener = bind(&path).unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RemoteEvent>();
    // Reducer stand-in: ack any remote command with a fixed message.
    tokio::spawn(async move {
        while let Some(RemoteEvent::Command(_cmd, reply)) = rx.recv().await {
            let _ = reply.send(RemoteResponse::ok("pong".to_string()));
        }
    });
    tokio::spawn(serve(
        listener,
        Arc::from("secret"),
        Arc::new(move |event| tx.send(event).is_ok()),
        test_hub(),
    ));

    // Correct token → the reducer's response is relayed back verbatim.
    let req = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "secret".to_string(),
        request_id: None,
        command: RemoteCommand::TogglePause,
    };
    let resp = parse(&send_line(&path, &serde_json::to_string(&req).unwrap()).await);
    assert!(resp.ok);
    assert_eq!(resp.message.as_deref(), Some("pong"));

    // A legacy v7 client (shipped `ytt -r` / tray) is accepted forever: the version
    // check is a range, not equality.
    let legacy = RemoteRequest {
        version: PROTOCOL_VERSION_V7,
        token: "secret".to_string(),
        request_id: None,
        command: RemoteCommand::TogglePause,
    };
    let resp = parse(&send_line(&path, &serde_json::to_string(&legacy).unwrap()).await);
    assert!(resp.ok, "v7 one-shot must keep working: {resp:?}");
    assert_eq!(resp.message.as_deref(), Some("pong"));

    // Anything below the frozen floor fails loudly.
    let ancient = RemoteRequest {
        version: 6,
        token: "secret".to_string(),
        request_id: None,
        command: RemoteCommand::TogglePause,
    };
    let resp = parse(&send_line(&path, &serde_json::to_string(&ancient).unwrap()).await);
    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("bad_version"));

    // Wrong token → rejected before reaching the reducer.
    let bad = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "nope".to_string(),
        request_id: None,
        command: RemoteCommand::TogglePause,
    };
    let resp = parse(&send_line(&path, &serde_json::to_string(&bad).unwrap()).await);
    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("bad_token"));

    // Valid JSON under the one-shot byte cap, but semantically too large: rejected
    // before the reducer stand-in can answer with "pong".
    let bad_query = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "secret".to_string(),
        request_id: None,
        command: RemoteCommand::RunSearch {
            ticket: 1,
            query: "q".repeat(crate::remote::proto::REMOTE_MAX_QUERY_BYTES + 1),
            source: crate::search_source::SearchSource::Youtube,
        },
    };
    let resp = parse(&send_line(&path, &serde_json::to_string(&bad_query).unwrap()).await);
    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("query_too_long"));

    // Unparseable line → bad_request (no panic, still one response line).
    let resp = parse(&send_line(&path, "{not json}").await);
    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("bad_request"));

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn oversized_request_is_rejected_before_reducer() {
    let path = std::env::temp_dir()
        .join(format!(
            "yututui-remote-oversized-test-{}.sock",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned();
    let _ = std::fs::remove_file(&path);
    let listener = bind(&path).unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RemoteEvent>();
    tokio::spawn(async move {
        if rx.recv().await.is_some() {
            panic!("oversized remote request reached reducer");
        }
    });
    tokio::spawn(serve(
        listener,
        Arc::from("secret"),
        Arc::new(move |event| tx.send(event).is_ok()),
        test_hub(),
    ));

    // Garbage that large fails both parses → bad_request, never the reducer.
    let too_large = format!("{} \n", "x".repeat(MAX_REQUEST_BYTES + 1));
    let resp = parse(&send_line(&path, &too_large).await);
    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("bad_request"));

    // A syntactically VALID one-shot over 4 KB is also rejected: the one-shot cap
    // is enforced post-parse now that the shared first-line buffer is session-sized.
    let req = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: "secret".to_string(),
        request_id: None,
        command: RemoteCommand::Play {
            query: "q".repeat(MAX_REQUEST_BYTES),
        },
    };
    let resp = parse(&send_line(&path, &serde_json::to_string(&req).unwrap()).await);
    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("bad_request"));

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn retiring_guard_cannot_unlink_a_fast_successor() {
    let fixture = PrivateEndpointFixture::new("successor-test");
    let socket = fixture.socket.clone();
    let descriptor = fixture.descriptor.clone();

    let old_hub = test_hub();
    let old_listener = bind(&socket).unwrap();
    let old_socket_identity = socket_file_identity(&socket).unwrap();
    let old_task = tokio::spawn(serve(
        old_listener,
        Arc::from("old-token"),
        Arc::new(|_| true),
        Arc::clone(&old_hub),
    ));
    let old_identity = InstanceFile {
        app_pid: 1,
        endpoint: socket.clone(),
        token: "old-token".to_string(),
        created_unix: 1,
        mode: InstanceMode::StandaloneTui,
        protocol_version: PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    crate::util::safe_fs::write_private_atomic_json(&descriptor, &old_identity).unwrap();
    let endpoint_lease = Arc::new(EndpointLease::new(
        socket.clone(),
        Some(old_socket_identity),
    ));
    endpoint_lease.finish_publication(Some(PublishedInstance {
        path: descriptor.clone(),
        identity: old_identity,
    }));
    let mut guard = InstanceGuard {
        endpoint_lease,
        hub: old_hub,
        serve_task: Some(old_task),
    };

    guard.release_endpoint();
    assert!(!Path::new(&socket).exists());
    assert!(!descriptor.exists());

    let successor_listener = bind(&socket).unwrap();
    let successor_identity = InstanceFile {
        app_pid: 2,
        endpoint: socket.clone(),
        token: "successor-token".to_string(),
        created_unix: 2,
        mode: InstanceMode::StandaloneTui,
        protocol_version: PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    crate::util::safe_fs::write_private_atomic_json(&descriptor, &successor_identity).unwrap();

    tokio::time::timeout(Duration::from_millis(200), guard.shutdown())
        .await
        .expect("old accept owner must stop promptly");
    drop(guard);

    assert!(Path::new(&socket).exists());
    let published: InstanceFile =
        serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
    assert_eq!(published.app_pid, successor_identity.app_pid);
    assert_eq!(published.token, successor_identity.token);
    assert_eq!(published.endpoint, successor_identity.endpoint);
    assert!(probe_alive(&socket).await);

    drop(successor_listener);
}

#[tokio::test]
async fn late_guard_cleanup_preserves_a_rebound_successor_inode() {
    let fixture = PrivateEndpointFixture::new("rebound-test");
    let socket = fixture.socket.clone();
    let descriptor = fixture.descriptor.clone();

    let old_hub = test_hub();
    let old_listener = bind(&socket).unwrap();
    let old_socket_identity = socket_file_identity(&socket).unwrap();
    let old_task = tokio::spawn(serve(
        old_listener,
        Arc::from("old-token"),
        Arc::new(|_| true),
        Arc::clone(&old_hub),
    ));
    let old_identity = InstanceFile {
        app_pid: 1,
        endpoint: socket.clone(),
        token: "old-token".to_string(),
        created_unix: 1,
        mode: InstanceMode::StandaloneTui,
        protocol_version: PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    crate::util::safe_fs::write_private_atomic_json(&descriptor, &old_identity).unwrap();
    let endpoint_lease = Arc::new(EndpointLease::new(
        socket.clone(),
        Some(old_socket_identity),
    ));
    endpoint_lease.finish_publication(Some(PublishedInstance {
        path: descriptor.clone(),
        identity: old_identity,
    }));
    let mut guard = InstanceGuard {
        endpoint_lease,
        hub: Arc::clone(&old_hub),
        serve_task: Some(old_task),
    };

    // Model an accept owner which ended before its host began teardown. Since name reclamation is
    // disabled, the dead listener leaves its old socket file behind until the successor probes it.
    old_hub.shutdown_all();
    tokio::time::timeout(Duration::from_millis(200), async {
        while !guard
            .serve_task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("old accept owner must stop promptly");
    std::fs::remove_file(&socket).unwrap();

    let successor_listener = bind(&socket).unwrap();
    let successor_identity = InstanceFile {
        app_pid: 2,
        endpoint: socket.clone(),
        token: "successor-token".to_string(),
        created_unix: 2,
        mode: InstanceMode::StandaloneTui,
        protocol_version: PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    crate::util::safe_fs::write_private_atomic_json(&descriptor, &successor_identity).unwrap();

    // The retiring descriptor and socket now both point at the successor. The old identity must
    // make both removals no-ops, including the synchronous Drop fallback.
    guard.release_endpoint();
    drop(guard);

    assert!(Path::new(&socket).exists());
    let published: InstanceFile =
        serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
    assert_eq!(published.app_pid, successor_identity.app_pid);
    assert_eq!(published.token, successor_identity.token);
    assert!(probe_alive(&socket).await);

    drop(successor_listener);
}

#[tokio::test]
async fn repeated_accept_failures_self_revoke_the_published_endpoint() {
    let fixture = PrivateEndpointFixture::new("accept-failure-test");
    let socket = fixture.socket.clone();
    let descriptor = fixture.descriptor.clone();

    let listener = bind(&socket).unwrap();
    let socket_identity = socket_file_identity(&socket).unwrap();
    let endpoint_lease = Arc::new(EndpointLease::new(socket.clone(), Some(socket_identity)));
    let hub = test_hub();
    // Match production ordering: create the accept owner before publishing the descriptor.
    let serve_task = spawn_accept_owner_with_faults(
        listener,
        Arc::from("old-token"),
        Arc::new(|_| true),
        Arc::clone(&hub),
        Arc::clone(&endpoint_lease),
        AcceptFaults::FailNext(MAX_CONSECUTIVE_ACCEPT_ERRORS),
    );
    let old_identity = InstanceFile {
        app_pid: 10,
        endpoint: socket.clone(),
        token: "old-token".to_string(),
        created_unix: 10,
        mode: InstanceMode::StandaloneTui,
        protocol_version: PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    endpoint_lease.publish_instance_at(descriptor.clone(), old_identity);
    let mut guard = InstanceGuard {
        endpoint_lease,
        hub: Arc::clone(&hub),
        serve_task: Some(serve_task),
    };

    tokio::time::timeout(Duration::from_millis(500), async {
        while !guard.accept_owner_failed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("repeated accept failures must become owner-visible");

    assert!(!Path::new(&socket).exists());
    assert!(!descriptor.exists());
    assert!(hub.is_shutting_down());
    guard.shutdown().await;
}

#[tokio::test]
async fn failed_accept_owner_cannot_overwrite_or_unlink_a_fast_successor() {
    let fixture = PrivateEndpointFixture::new("accept-successor-test");
    let socket = fixture.socket.clone();
    let descriptor = fixture.descriptor.clone();

    let old_listener = bind(&socket).unwrap();
    let old_socket_identity = socket_file_identity(&socket).unwrap();
    let endpoint_lease = Arc::new(EndpointLease::new(
        socket.clone(),
        Some(old_socket_identity),
    ));
    let old_hub = test_hub();
    let serve_task = spawn_accept_owner_with_faults(
        old_listener,
        Arc::from("old-token"),
        Arc::new(|_| true),
        Arc::clone(&old_hub),
        Arc::clone(&endpoint_lease),
        AcceptFaults::FailNext(MAX_CONSECUTIVE_ACCEPT_ERRORS),
    );

    // Force the worst ordering: the accept owner fails before its caller publishes. The lease
    // must remember the failure, and the listener remains owned until that state is recorded.
    tokio::time::timeout(Duration::from_millis(500), async {
        while !endpoint_lease.accept_owner_failed() || !serve_task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("injected accept owner must stop");

    std::fs::remove_file(&socket).unwrap();
    let successor_listener = bind(&socket).unwrap();
    let successor_identity = InstanceFile {
        app_pid: 20,
        endpoint: socket.clone(),
        token: "successor-token".to_string(),
        created_unix: 20,
        mode: InstanceMode::StandaloneTui,
        protocol_version: PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    crate::util::safe_fs::write_private_atomic_json(&descriptor, &successor_identity).unwrap();

    let old_identity = InstanceFile {
        app_pid: 10,
        endpoint: socket.clone(),
        token: "old-token".to_string(),
        created_unix: 10,
        mode: InstanceMode::StandaloneTui,
        protocol_version: PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    // Production publication is serialized by the lease. Since failure already won, this call
    // must not overwrite the successor descriptor, and inode matching must preserve its socket.
    endpoint_lease.publish_instance_at(descriptor.clone(), old_identity);
    let mut guard = InstanceGuard {
        endpoint_lease,
        hub: old_hub,
        serve_task: Some(serve_task),
    };

    assert!(guard.accept_owner_failed());
    assert!(Path::new(&socket).exists());
    let published: InstanceFile =
        serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
    assert_eq!(published.app_pid, successor_identity.app_pid);
    assert_eq!(published.token, successor_identity.token);
    assert!(probe_alive(&socket).await);

    guard.shutdown().await;
    drop(successor_listener);
}
