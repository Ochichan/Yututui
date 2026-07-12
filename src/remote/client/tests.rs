use super::*;
use crate::remote::proto::{InstanceMode, PROTOCOL_VERSION_V7};
use crate::remote::requests::{CommandDeduper, RequestKey};
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::Listener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncBufReadExt;

fn test_endpoint(name: &str) -> String {
    std::env::temp_dir()
        .join(format!("yututui-client-{name}-{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

fn test_instance(endpoint: String) -> InstanceFile {
    InstanceFile {
        app_pid: std::process::id(),
        endpoint,
        token: "secret".to_string(),
        created_unix: 1,
        mode: InstanceMode::StandaloneTui,
        protocol_version: PROTOCOL_VERSION,
        capabilities: vec![
            "remote-control".to_string(),
            "status".to_string(),
            RETAINED_REQUEST_OUTCOMES_CAPABILITY.to_string(),
        ],
    }
}

fn legacy_v8_instance(endpoint: String) -> InstanceFile {
    let mut instance = test_instance(endpoint);
    instance
        .capabilities
        .retain(|capability| capability != RETAINED_REQUEST_OUTCOMES_CAPABILITY);
    instance
}

fn bind_test_listener(endpoint: &str) -> Listener {
    let _ = std::fs::remove_file(endpoint);
    let name = endpoint.to_fs_name::<GenericFilePath>().unwrap();
    ListenerOptions::new()
        .name(name)
        .reclaim_name(false)
        .create_tokio()
        .unwrap()
}

async fn serve_one_response(listener: Listener, response_line: String, expected_version: u8) {
    let conn = listener.accept().await.unwrap();
    {
        let mut reader = BufReader::new(&conn);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
        assert_eq!(request.version, expected_version);
        assert_eq!(request.token, "secret");
        assert_eq!(
            request.request_id.is_some(),
            expected_version >= PROTOCOL_VERSION
        );
    }
    let mut writer = &conn;
    writer.write_all(response_line.as_bytes()).await.unwrap();
    writer.write_all(b"\n").await.unwrap();
    writer.flush().await.unwrap();
}

async fn accept_requests_without_response(listener: Listener, expected_requests: usize) {
    for _ in 0..expected_requests {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let _: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
    }
}

async fn serve_timeout_then_success(listener: Listener) -> [Option<String>; 2] {
    let responses = [
        RemoteResponseEnvelope {
            response: RemoteResponse::err(CONFIRMATION_LOST_REASON),
            retained_replay: false,
        },
        RemoteResponseEnvelope {
            response: RemoteResponse::ok("late success".to_owned()),
            retained_replay: true,
        },
    ];
    let mut request_ids: [Option<String>; 2] = [None, None];
    for (index, response) in responses.into_iter().enumerate() {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
        request_ids[index] = request.request_id;

        let mut writer = &conn;
        let mut response = serde_json::to_vec(&response).unwrap();
        response.push(b'\n');
        writer.write_all(&response).await.unwrap();
        writer.flush().await.unwrap();
    }
    request_ids
}

async fn serve_eof_then_success(listener: Listener) -> [Option<String>; 2] {
    let mut request_ids: [Option<String>; 2] = [None, None];
    for (index, request_id) in request_ids.iter_mut().enumerate() {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
        *request_id = request.request_id;

        if index == 1 {
            let mut writer = &conn;
            let mut response = serde_json::to_vec(&RemoteResponseEnvelope {
                response: RemoteResponse::ok("recovered after ambiguous EOF".to_owned()),
                retained_replay: true,
            })
            .unwrap();
            response.push(b'\n');
            writer.write_all(&response).await.unwrap();
            writer.flush().await.unwrap();
        }
    }
    request_ids
}

async fn serve_corrupt_then_cached_then_fresh(listener: Listener) -> ([Option<String>; 3], usize) {
    let requests = CommandDeduper::default();
    let executions = Arc::new(AtomicUsize::new(0));
    let mut request_ids: [Option<String>; 3] = [None, None, None];

    for (index, request_id) in request_ids.iter_mut().enumerate() {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
        *request_id = request.request_id.clone();

        let key = RequestKey::stable(request.request_id.unwrap()).unwrap();
        let executions_for_emit = Arc::clone(&executions);
        let response = requests
            .execute_with_replay_proof(
                key,
                &request.command,
                Duration::from_secs(1),
                move |reply| {
                    let execution = executions_for_emit.fetch_add(1, Ordering::Relaxed) + 1;
                    reply
                        .send(RemoteResponse::ok(format!("execution {execution}")))
                        .unwrap();
                    true
                },
            )
            .await;

        let mut writer = &conn;
        if index == 0 {
            // The mutation completed and its response is cached, but the first caller sees
            // an invalid response. Its retry must join that cache entry instead of executing
            // the mutation again.
            writer.write_all(b"{truncated\n").await.unwrap();
        } else {
            let response = RemoteResponseEnvelope {
                response: response.response,
                retained_replay: response.retained_replay,
            };
            let mut response = serde_json::to_vec(&response).unwrap();
            response.push(b'\n');
            writer.write_all(&response).await.unwrap();
        }
        writer.flush().await.unwrap();
    }

    (request_ids, executions.load(Ordering::Relaxed))
}

async fn serve_two_malformed_responses(listener: Listener) -> [Option<String>; 2] {
    let mut request_ids: [Option<String>; 2] = [None, None];
    for request_id in &mut request_ids {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
        *request_id = request.request_id;

        let mut writer = &conn;
        writer.write_all(b"{not json}\n").await.unwrap();
        writer.flush().await.unwrap();
    }
    request_ids
}

async fn serve_eof_then_fresh_response(
    listener: Listener,
    response: RemoteResponse,
) -> [Option<String>; 2] {
    let mut request_ids: [Option<String>; 2] = [None, None];
    for (index, request_id) in request_ids.iter_mut().enumerate() {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
        *request_id = request.request_id;

        if index == 1 {
            let mut writer = &conn;
            let envelope = RemoteResponseEnvelope {
                response: response.clone(),
                retained_replay: false,
            };
            let mut response = serde_json::to_vec(&envelope).unwrap();
            response.push(b'\n');
            writer.write_all(&response).await.unwrap();
            writer.flush().await.unwrap();
        }
    }
    request_ids
}

async fn serve_legacy_eof_and_detect_retry(listener: Listener) -> usize {
    let conn = listener.accept().await.unwrap();
    let mut reader = BufReader::new(&conn);
    let mut request = String::new();
    reader.read_line(&mut request).await.unwrap();
    let _: RemoteRequest = serde_json::from_str(request.trim()).unwrap();
    drop(reader);
    drop(conn);

    if timeout(Duration::from_millis(100), listener.accept())
        .await
        .is_ok()
    {
        2
    } else {
        1
    }
}

#[tokio::test]
async fn send_to_instance_round_trips_status_response() {
    let endpoint = test_endpoint("status");
    let listener = bind_test_listener(&endpoint);
    let snapshot = crate::remote::proto::StatusSnapshot {
        title: Some("Song".to_string()),
        artist: Some("Artist".to_string()),
        paused: false,
        volume: 75,
        position: 1,
        total: 2,
        streaming: true,
        owner_mode: InstanceMode::StandaloneTui,
        settings: Default::default(),
        queue: Vec::new(),
        shuffle: false,
        repeat: Default::default(),
        elapsed_ms: None,
        duration_ms: None,
        is_live: false,
        queue_rev: None,
        track_id: None,
        position_epoch: 0,
        artwork: None,
    };
    let response = serde_json::to_string(&RemoteResponse::status(snapshot.clone())).unwrap();
    let server = tokio::spawn(serve_one_response(listener, response, PROTOCOL_VERSION));

    let resp = send_to(test_instance(endpoint.clone()), RemoteCommand::Status)
        .await
        .unwrap();
    server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert!(resp.ok);
    assert_eq!(resp.status, Some(snapshot));
}

#[tokio::test]
async fn send_to_instance_preserves_semantic_rejection_response() {
    let endpoint = test_endpoint("rejected");
    let listener = bind_test_listener(&endpoint);
    let response = serde_json::to_string(&RemoteResponse::err("queue_empty")).unwrap();
    let server = tokio::spawn(serve_one_response(listener, response, PROTOCOL_VERSION));

    let resp = send_to(test_instance(endpoint.clone()), RemoteCommand::Next)
        .await
        .unwrap();
    server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert!(!resp.ok);
    assert_eq!(resp.reason.as_deref(), Some("queue_empty"));
}

#[tokio::test]
async fn semantic_timeout_retries_once_with_the_same_request_identity() {
    let endpoint = test_endpoint("retry-identity");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_timeout_then_success(listener));

    let resp = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
        .await
        .unwrap();
    let request_ids = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert!(resp.ok);
    assert_eq!(resp.message.as_deref(), Some("late success"));
    assert!(request_ids[0].is_some());
    assert_eq!(request_ids[0], request_ids[1]);
}

#[tokio::test]
async fn no_response_retries_once_with_the_same_request_identity() {
    let endpoint = test_endpoint("retry-no-response");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_eof_then_success(listener));

    let resp = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
        .await
        .unwrap();
    let request_ids = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert!(resp.ok);
    assert_eq!(
        resp.message.as_deref(),
        Some("recovered after ambiguous EOF")
    );
    assert!(request_ids[0].is_some());
    assert_eq!(request_ids[0], request_ids[1]);
}

#[tokio::test]
async fn old_v8_owner_without_retained_outcome_capability_is_not_retried() {
    let endpoint = test_endpoint("legacy-v8-no-retry");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_legacy_eof_and_detect_retry(listener));

    let err = send_to_instance(
        legacy_v8_instance(endpoint.clone()),
        RemoteCommand::TogglePause,
    )
    .await
    .unwrap_err();
    let accepted = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert_eq!(err, ClientError::ConfirmationLost);
    assert_eq!(accepted, 1, "an old v8 owner must not receive a retry");
}

#[tokio::test]
async fn quiescing_owner_rejection_does_not_overwrite_ambiguous_mutation() {
    let endpoint = test_endpoint("retry-quiescing-owner");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_eof_then_fresh_response(
        listener,
        RemoteResponse::err("shutting_down"),
    ));

    let err = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
        .await
        .unwrap_err();
    let request_ids = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert_eq!(err, ClientError::ConfirmationLost);
    assert_eq!(request_ids[0], request_ids[1]);
}

#[tokio::test]
async fn successor_bad_token_rejection_does_not_overwrite_ambiguous_mutation() {
    let endpoint = test_endpoint("retry-successor-bad-token");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_eof_then_fresh_response(
        listener,
        RemoteResponse::err("bad_token"),
    ));

    let err = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
        .await
        .unwrap_err();
    let request_ids = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert_eq!(err, ClientError::ConfirmationLost);
    assert_eq!(request_ids[0], request_ids[1]);
}

#[tokio::test]
async fn run_search_accepts_a_fresh_retry_ack_without_retained_capability_or_proof() {
    let endpoint = test_endpoint("search-fresh-retry-ack");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_eof_then_fresh_response(
        listener,
        RemoteResponse::ok("search dispatched".to_owned()),
    ));
    let command = RemoteCommand::RunSearch {
        ticket: 7,
        query: "query".to_owned(),
        source: crate::search_source::SearchSource::Youtube,
    };

    let response = send_to_instance(legacy_v8_instance(endpoint.clone()), command)
        .await
        .unwrap();
    let request_ids = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert!(response.ok);
    assert_eq!(response.message.as_deref(), Some("search dispatched"));
    assert_eq!(request_ids[0], request_ids[1]);
}

#[tokio::test]
async fn run_search_fresh_retry_rejection_preserves_confirmation_lost() {
    let endpoint = test_endpoint("search-fresh-retry-rejected");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_eof_then_fresh_response(
        listener,
        RemoteResponse::err("bad_token"),
    ));
    let command = RemoteCommand::RunSearch {
        ticket: 8,
        query: "query".to_owned(),
        source: crate::search_source::SearchSource::Youtube,
    };

    let err = send_to_instance(legacy_v8_instance(endpoint.clone()), command)
        .await
        .unwrap_err();
    let request_ids = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert_eq!(err, ClientError::ConfirmationLost);
    assert_eq!(request_ids[0], request_ids[1]);
}

#[tokio::test]
async fn status_accepts_a_fresh_retry_rejection_without_confirmation_semantics() {
    let endpoint = test_endpoint("status-fresh-retry-rejected");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_eof_then_fresh_response(
        listener,
        RemoteResponse::err("shutting_down"),
    ));

    let response = send_to_instance(legacy_v8_instance(endpoint.clone()), RemoteCommand::Status)
        .await
        .unwrap();
    let request_ids = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("shutting_down"));
    assert_eq!(request_ids[0], request_ids[1]);
}

#[tokio::test]
async fn malformed_response_retries_once_and_cached_result_executes_only_once() {
    let endpoint = test_endpoint("malformed-cached");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_corrupt_then_cached_then_fresh(listener));

    let instance = test_instance(endpoint.clone());
    let recovered = send_to_instance(instance.clone(), RemoteCommand::TogglePause)
        .await
        .unwrap();
    let later = send_to_instance(instance, RemoteCommand::TogglePause)
        .await
        .unwrap();
    let (request_ids, executions) = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert!(recovered.ok);
    assert_eq!(recovered.message.as_deref(), Some("execution 1"));
    assert!(later.ok);
    assert_eq!(later.message.as_deref(), Some("execution 2"));
    assert_eq!(executions, 2, "the same-ID retry must not re-execute");
    assert!(request_ids[0].is_some());
    assert_eq!(request_ids[0], request_ids[1]);
    assert_ne!(request_ids[1], request_ids[2]);
}

#[tokio::test]
async fn repeated_malformed_response_stops_after_one_retry() {
    let endpoint = test_endpoint("malformed-twice");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_two_malformed_responses(listener));

    let err = send_to(test_instance(endpoint.clone()), RemoteCommand::Status)
        .await
        .unwrap_err();
    let request_ids = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert_eq!(err, ClientError::MalformedResponse);
    assert!(request_ids[0].is_some());
    assert_eq!(request_ids[0], request_ids[1]);
}

#[tokio::test]
async fn repeated_lost_mutation_replies_surface_confirmation_lost() {
    let endpoint = test_endpoint("mutation-confirmation-lost");
    let listener = bind_test_listener(&endpoint);
    let server = tokio::spawn(serve_two_malformed_responses(listener));

    let err = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::TogglePause)
        .await
        .unwrap_err();
    let request_ids = server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert_eq!(err, ClientError::ConfirmationLost);
    assert_eq!(request_ids[0], request_ids[1]);
    assert!(
        err.human_message().contains("may have been applied"),
        "the CLI must tell users to inspect state before retrying"
    );
}

#[tokio::test]
async fn request_write_is_deadline_bounded_for_a_non_reading_peer() {
    let (mut writer, _non_reading_peer) = tokio::io::duplex(1);

    let error = write_request(
        &mut writer,
        &vec![b'x'; 64 * 1024],
        Duration::from_millis(5),
    )
    .await
    .unwrap_err();

    assert_eq!(
        error,
        ClientError::WriteFailed("remote request write timed out".to_string())
    );
}

#[test]
fn only_possibly_delivered_or_post_flush_failures_are_ambiguous() {
    assert!(attempt_is_ambiguous(&Err(ClientError::WriteFailed(
        "partial".to_string()
    ))));
    assert!(attempt_is_ambiguous(&Err(ClientError::FlushFailed(
        "lost ack".to_string()
    ))));
    assert!(attempt_is_ambiguous(&Err(ClientError::NoResponse)));
    assert!(attempt_is_ambiguous(&Err(ClientError::MalformedResponse)));
    assert!(attempt_is_ambiguous(&Ok(RemoteResponseEnvelope {
        response: RemoteResponse::err(CONFIRMATION_LOST_REASON),
        retained_replay: false,
    })));
    assert!(!attempt_is_ambiguous(&Err(ClientError::ConnectFailed)));
    assert!(!attempt_is_ambiguous(&Err(ClientError::MalformedEndpoint)));
    assert!(!attempt_is_ambiguous(&Err(ClientError::EncodeFailed(
        "before write".to_string()
    ))));
}

#[test]
fn only_explicit_retained_replay_proves_a_retry_outcome() {
    let plain_success = Ok(RemoteResponseEnvelope {
        response: RemoteResponse::ok("fresh owner response".to_owned()),
        retained_replay: false,
    });
    assert!(!attempt_is_retained_replay(&plain_success));

    let replayed_success = Ok(RemoteResponseEnvelope {
        response: RemoteResponse::ok("retained owner response".to_owned()),
        retained_replay: true,
    });
    assert!(attempt_is_retained_replay(&replayed_success));

    let replay_wait_timed_out = Ok(RemoteResponseEnvelope {
        response: RemoteResponse::err(CONFIRMATION_LOST_REASON),
        retained_replay: true,
    });
    assert!(!attempt_is_retained_replay(&replay_wait_timed_out));
}

#[tokio::test]
async fn send_to_instance_preserves_no_response() {
    let endpoint = test_endpoint("no-response");
    let listener = bind_test_listener(&endpoint);
    // Status retries once after an ambiguous EOF. Keep the listener alive for both
    // exchanges so the second attempt observes the same semantic no-response outcome
    // instead of replacing it with a connect failure after the fixture exits.
    let server = tokio::spawn(accept_requests_without_response(listener, 2));

    let err = send_to(test_instance(endpoint.clone()), RemoteCommand::Status)
        .await
        .unwrap_err();
    server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert_eq!(err, ClientError::NoResponse);
}

#[tokio::test]
async fn send_to_instance_rejects_a_malformed_endpoint_before_connecting() {
    let err = send_to(
        test_instance("invalid\0endpoint".to_string()),
        RemoteCommand::Status,
    )
    .await
    .unwrap_err();

    assert_eq!(err, ClientError::MalformedEndpoint);
}

#[tokio::test]
async fn primary_probe_rejects_a_live_socket_without_a_descriptor() {
    let endpoint = test_endpoint("owner-without-descriptor");
    let listener = bind_test_listener(&endpoint);

    let error = probe_primary_endpoints(std::slice::from_ref(&endpoint))
        .await
        .unwrap_err();

    assert_eq!(error, CapabilityLookupError::UnpublishedOwner);
    drop(listener);
    let _ = std::fs::remove_file(endpoint);
}

#[tokio::test]
async fn primary_probe_accepts_only_a_definitely_absent_socket() {
    let endpoint = test_endpoint("absent-owner");
    let _ = std::fs::remove_file(&endpoint);

    probe_primary_endpoints(std::slice::from_ref(&endpoint))
        .await
        .unwrap();
}

#[tokio::test]
async fn primary_probe_accepts_an_unbindable_overlong_filesystem_path() {
    let endpoint = std::env::temp_dir()
        .join("x".repeat(512))
        .to_string_lossy()
        .into_owned();

    probe_primary_endpoints(std::slice::from_ref(&endpoint))
        .await
        .unwrap();
}

#[tokio::test]
async fn primary_probe_fails_closed_for_an_invalid_endpoint() {
    let endpoint = "invalid\0endpoint".to_string();

    let error = probe_primary_endpoints(std::slice::from_ref(&endpoint))
        .await
        .unwrap_err();

    assert_eq!(error, CapabilityLookupError::OwnerProbeFailed);
}

#[test]
fn capability_gate_distinguishes_old_and_capable_instances() {
    let mut old = test_instance("unused".to_string());
    let error =
        require_capability(old.clone(), super::super::PERSONAL_EXPORT_CAPABILITY).unwrap_err();
    assert_eq!(
        error,
        MissingCapability {
            capability: super::super::PERSONAL_EXPORT_CAPABILITY.to_string()
        }
    );

    old.capabilities
        .push(super::super::PERSONAL_EXPORT_CAPABILITY.to_string());
    assert!(require_capability(old, super::super::PERSONAL_EXPORT_CAPABILITY).is_ok());
}

#[tokio::test]
async fn send_to_instance_uses_v7_for_v7_owner() {
    let endpoint = test_endpoint("v7");
    let listener = bind_test_listener(&endpoint);
    let response = serde_json::to_string(&RemoteResponse::ok("ok".to_string())).unwrap();
    let server = tokio::spawn(serve_one_response(listener, response, PROTOCOL_VERSION_V7));
    let mut instance = test_instance(endpoint.clone());
    instance.protocol_version = PROTOCOL_VERSION_V7;

    let response = send_to(instance, RemoteCommand::Next).await.unwrap();
    server.await.unwrap();
    let _ = std::fs::remove_file(endpoint);

    assert!(response.ok);
}

#[test]
fn one_shot_version_negotiates_v7_v8_and_future() {
    assert_eq!(
        negotiated_oneshot_version(PROTOCOL_VERSION_V7),
        Ok(PROTOCOL_VERSION_V7)
    );
    assert_eq!(
        negotiated_oneshot_version(PROTOCOL_VERSION),
        Ok(PROTOCOL_VERSION)
    );
    assert_eq!(negotiated_oneshot_version(u8::MAX), Ok(PROTOCOL_VERSION));
    assert_eq!(
        negotiated_oneshot_version(PROTOCOL_VERSION_V7 - 1),
        Err(ClientError::UnsupportedProtocol)
    );
}

fn snapshot(queue: Vec<crate::remote::proto::QueueItemSnapshot>) -> StatusSnapshot {
    StatusSnapshot {
        title: None,
        artist: None,
        paused: true,
        volume: 50,
        position: usize::from(!queue.is_empty()),
        total: queue.len(),
        streaming: false,
        owner_mode: InstanceMode::Daemon,
        settings: crate::remote::proto::SettingsSnapshot::default(),
        queue,
        shuffle: false,
        repeat: Default::default(),
        elapsed_ms: None,
        duration_ms: None,
        is_live: false,
        queue_rev: None,
        track_id: None,
        position_epoch: 0,
        artwork: None,
    }
}

#[test]
fn queue_formatter_is_one_based_marks_current_and_sanitizes_controls() {
    let status = snapshot(vec![
        crate::remote::proto::QueueItemSnapshot {
            title: "첫 곡\n\u{1b}[31m".to_string(),
            artist: "가수\r이름".to_string(),
            duration: "3:14\t".to_string(),
            current: true,
        },
        crate::remote::proto::QueueItemSnapshot {
            title: "Second".to_string(),
            artist: String::new(),
            duration: String::new(),
            current: false,
        },
    ]);

    let line = queue_human(&status);
    assert_eq!(line, "> 1. 첫 곡 [31m — 가수 이름  [3:14]\n  2. Second");
    assert!(!line.contains('\u{1b}'));
    assert_eq!(line.lines().count(), 2);
}

#[test]
fn queue_formatter_has_stable_empty_message() {
    assert_eq!(queue_human(&snapshot(Vec::new())), "queue empty");
}

#[test]
fn settings_formatter_includes_only_summary_fields() {
    let mut status = snapshot(Vec::new());
    status.settings = crate::remote::proto::SettingsSnapshot {
        autoplay_streaming: true,
        streaming_mode: StreamingMode::Discovery,
        streaming_source: SearchSource::InternetArchive,
        speed_tenths: 15,
        seek_seconds: 12,
        normalize: true,
        gapless: false,
        ai_enabled: true,
        radio_mode: false,
    };

    assert_eq!(
        settings_human(&status),
        "autoplay=on  •  source=internet_archive  •  mode=discovery  •  speed=1.5x  •  seek=12s  •  normalize=on  •  gapless=off  •  ai=on  •  radio-mode=off"
    );
}

#[test]
fn queue_and_settings_projection_json_retains_full_remote_response() {
    let mut status = snapshot(vec![crate::remote::proto::QueueItemSnapshot {
        title: "곡".to_string(),
        artist: "가수".to_string(),
        duration: "2:34".to_string(),
        current: true,
    }]);
    status.settings.autoplay_streaming = true;
    let response = RemoteResponse::status(status.clone());

    // Both projection branches pass this whole response to `print_json`; they do not
    // serialize only the selected human projection.
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["reason"], "ok");
    assert!(value.get("message").is_some());
    assert_eq!(value["status"]["queue"][0]["title"], "곡");
    assert_eq!(value["status"]["settings"]["autoplay_streaming"], true);
    assert_eq!(response.status, Some(status));
}

#[test]
fn info_output_is_sorted_and_structurally_omits_credentials() {
    let mut instance = test_instance("/private/secret-endpoint".to_string());
    instance.token = "super-secret-token".to_string();
    instance.created_unix = 99;
    instance.mode = InstanceMode::Daemon;
    instance.capabilities = vec![
        "status".to_string(),
        "events-v8".to_string(),
        "remote-control".to_string(),
        "socket=/PRIVATE/SECRET-ENDPOINT".to_string(),
        "token=SUPER-SECRET-TOKEN".to_string(),
    ];

    let info = SanitizedInfo::from(instance);
    let value = serde_json::to_value(&info).unwrap();
    let object = value.as_object().unwrap();
    assert_eq!(object.len(), 5);
    assert_eq!(
        object["capabilities"],
        serde_json::json!(["events-v8", "remote-control", "status"])
    );
    let line = serde_json::to_string(&info).unwrap();
    let human = info.human_line();
    assert!(!line.contains("secret-endpoint"));
    assert!(!line.contains("super-secret-token"));
    assert!(!human.to_ascii_lowercase().contains("secret-endpoint"));
    assert!(!human.to_ascii_lowercase().contains("super-secret-token"));
    assert!(human.contains("events-v8,remote-control,status"));
    assert!(!object.contains_key("endpoint"));
    assert!(!object.contains_key("token"));
}

#[test]
fn info_rejection_never_reflects_owner_text() {
    let response = RemoteResponse {
        ok: false,
        reason: Some(
            "0123456789abcdef0123456789abcdef\n\u{1b}[31m/private/secret-endpoint".to_string(),
        ),
        message: Some("another owner-controlled field".to_string()),
        status: None,
        data: None,
    };

    let message = info_rejection_message(&response);
    assert_eq!(message, "info_status_rejected");
    assert!(!message.contains("0123456789abcdef0123456789abcdef"));
    assert!(!message.contains("secret-endpoint"));
    assert!(!message.chars().any(char::is_control));
}

#[test]
fn info_human_line_sanitizes_capability_controls() {
    let info = SanitizedInfo {
        app_pid: 7,
        created_unix: 1,
        mode: InstanceMode::StandaloneTui,
        protocol_version: PROTOCOL_VERSION,
        capabilities: vec!["events\n\u{1b}[31m-v8".to_string()],
    };
    let line = info.human_line();
    assert!(!line.contains('\n'));
    assert!(!line.contains('\u{1b}'));
    assert!(!line.contains("created_unix"));
    assert!(line.contains("pid 7"));
    assert!(line.contains("mode standalone_tui"));
    assert!(line.contains("protocol 8"));
    assert!(line.contains("events [31m-v8"));
}
