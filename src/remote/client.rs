//! The `ytt -r <command>` client: a short-lived process that connects to the running
//! instance, sends one command, prints the result, and exits.
//!
//! Critically, this path NEVER touches terminal raw mode or the alternate screen (no
//! `tui::init`, no graphics probe) — it must leave the caller's terminal pristine so it's
//! safe to wire to a window-manager keybinding or a status-bar click.
//!
//! Exit codes follow the i3-msg / swaymsg convention:
//!   0 = applied, 1 = transport / no running instance, 2 = usage or semantic rejection.

use std::time::Duration;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use super::args::{self, ParseError, Parsed};
use super::endpoint;
use super::proto::{
    CONFIRMATION_LOST_REASON, InstanceFile, MAX_ONESHOT_REPLY_BYTES, PROTOCOL_VERSION,
    RETAINED_REQUEST_OUTCOMES_CAPABILITY, RemoteCommand, RemoteRequest, RemoteResponse,
    RemoteResponseEnvelope, RequestRetryClass,
};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

const EXIT_OK: i32 = 0;
const EXIT_TRANSPORT: i32 = 1;
const EXIT_USAGE: i32 = 2;

/// Transport-level failures from the local control socket. Semantic command rejection is
/// represented by an `ok: false` [`RemoteResponse`], not by this error type.
#[derive(Debug, PartialEq, Eq)]
pub enum ClientError {
    NoRunningInstance,
    MalformedEndpoint,
    ConnectFailed,
    EncodeFailed(String),
    WriteFailed(String),
    FlushFailed(String),
    NoResponse,
    MalformedResponse,
    /// A mutating request may have reached the owner, but neither the original exchange nor the
    /// same-ID retry produced an authoritative result.
    ConfirmationLost,
}

impl ClientError {
    pub fn human_message(&self) -> String {
        match self {
            ClientError::NoRunningInstance => {
                "no running ytt instance found — start one with `ytt`.".to_string()
            }
            ClientError::MalformedEndpoint => {
                "malformed endpoint in the instance descriptor.".to_string()
            }
            ClientError::ConnectFailed => {
                "could not reach ytt (it may have exited) — start one with `ytt`.".to_string()
            }
            ClientError::EncodeFailed(e) => format!("could not encode request: {e}"),
            ClientError::WriteFailed(e) => format!("write failed: {e}"),
            ClientError::FlushFailed(e) => format!("flush failed: {e}"),
            ClientError::NoResponse => "no response from ytt.".to_string(),
            ClientError::MalformedResponse => "malformed response from ytt.".to_string(),
            ClientError::ConfirmationLost => {
                "confirmation lost — the action may have been applied; check current state before retrying."
                    .to_string()
            }
        }
    }
}

/// Entry point from `main` for `ytt -r …`. Parses args, runs the exchange on a tiny
/// current-thread runtime, and returns the process exit code. Never returns to the normal
/// TUI startup path.
pub fn run(args_in: &[String]) -> i32 {
    let parsed = match args::parse(args_in) {
        Ok(p) => p,
        Err(ParseError::Usage(text)) => {
            print!("{text}");
            return EXIT_OK;
        }
        Err(ParseError::Invalid(msg)) => {
            eprintln!("ytt -r: {msg}");
            return EXIT_USAGE;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ytt -r: could not start runtime: {e}");
            return EXIT_TRANSPORT;
        }
    };
    rt.block_on(exchange_for_cli(parsed))
}

/// Send one semantic command to the running instance and return its raw response.
///
/// This is the reusable control path for both `ytt -r` and desktop companions. It never touches
/// terminal state and never prints; callers decide how to render success, rejection, or transport
/// failures.
pub async fn send(command: RemoteCommand) -> Result<RemoteResponse, ClientError> {
    let Some(instance) = endpoint::read_instance() else {
        return Err(ClientError::NoRunningInstance);
    };
    send_to_instance(instance, command).await
}

async fn send_to_instance(
    instance: InstanceFile,
    command: RemoteCommand,
) -> Result<RemoteResponse, ClientError> {
    let request_id = super::requests::fresh_request_id();
    send_to_instance_with_request_id(instance, command, &request_id).await
}

async fn send_to_instance_with_request_id(
    instance: InstanceFile,
    command: RemoteCommand,
    request_id: &str,
) -> Result<RemoteResponse, ClientError> {
    let first = send_attempt(&instance, &command, request_id).await;
    if attempt_is_ambiguous(&first) {
        let requires_confirmation = command.requires_confirmation();
        let retry_class = command.request_retry_class();
        if retry_class == RequestRetryClass::RetainedOutcome
            && !instance
                .capabilities
                .iter()
                .any(|capability| capability == RETAINED_REQUEST_OUTCOMES_CAPABILITY)
        {
            // A v8 version number alone is not evidence of deduplication: older v8 servers ignore
            // the additive request ID. Retrying a mutation against one could apply it twice.
            return Err(ClientError::ConfirmationLost);
        }
        // Retained mutations can observe the first owner's late completion without a second
        // admission; explicitly reexecuted queries instead issue safe fresh work. Client and
        // server use the same nominal deadline, so a timeout can race the response write and
        // surface as `NoResponse`; read failures and malformed/truncated replies are equally
        // post-flush and therefore ambiguous. A write/flush failure can likewise happen after
        // the complete request reached the server. Every retry reuses this exact identity.
        let retry = send_attempt(&instance, &command, request_id).await;
        let retry_is_authoritative = match retry_class {
            RequestRetryClass::RetainedOutcome => attempt_is_retained_replay(&retry),
            // RunSearch is deliberately executed afresh: an `ok` acknowledgement proves at
            // least one dispatch, while a pre-admission rejection cannot resolve whether the
            // first attempt dispatched. Status does not require confirmation and accepts any
            // well-formed fresh retry response below.
            RequestRetryClass::ReexecuteReadOnly => {
                !requires_confirmation || attempt_confirms_fresh_dispatch(&retry)
            }
        };
        if !retry_is_authoritative {
            return Err(ClientError::ConfirmationLost);
        }
        return retry.map(|envelope| envelope.response);
    }
    first.map(|envelope| envelope.response)
}

fn attempt_is_ambiguous(result: &Result<RemoteResponseEnvelope, ClientError>) -> bool {
    matches!(
        result,
        Ok(envelope) if matches!(
            envelope.response.reason.as_deref(),
            Some("timeout" | CONFIRMATION_LOST_REASON)
        )
    ) || matches!(
        result,
        Err(ClientError::NoResponse
            | ClientError::MalformedResponse
            | ClientError::WriteFailed(_)
            | ClientError::FlushFailed(_))
    )
}

fn attempt_is_retained_replay(result: &Result<RemoteResponseEnvelope, ClientError>) -> bool {
    matches!(result, Ok(envelope) if envelope.retained_replay && !matches!(
        envelope.response.reason.as_deref(),
        Some("timeout" | CONFIRMATION_LOST_REASON)
    ))
}

fn attempt_confirms_fresh_dispatch(result: &Result<RemoteResponseEnvelope, ClientError>) -> bool {
    matches!(result, Ok(envelope) if envelope.response.ok)
}

async fn send_attempt(
    instance: &InstanceFile,
    command: &RemoteCommand,
    request_id: &str,
) -> Result<RemoteResponseEnvelope, ClientError> {
    let Ok(name) = instance.endpoint.as_str().to_fs_name::<GenericFilePath>() else {
        return Err(ClientError::MalformedEndpoint);
    };

    let conn = match timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
        Ok(Ok(c)) => c,
        // Connect refused / timed out: the descriptor is stale or the instance just exited.
        _ => return Err(ClientError::ConnectFailed),
    };

    let reply_timeout = super::reply_timeout_for(command);
    let req = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: instance.token.clone(),
        request_id: Some(request_id.to_owned()),
        command: command.clone(),
    };
    let mut payload = match serde_json::to_vec(&req) {
        Ok(v) => v,
        Err(e) => return Err(ClientError::EncodeFailed(e.to_string())),
    };
    payload.push(b'\n');

    {
        let mut writer = &conn;
        write_request(&mut writer, &payload, WRITE_TIMEOUT).await?;
    }

    let mut reader = BufReader::new(&conn);
    let line = match timeout(reply_timeout, read_bounded_line(&mut reader)).await {
        Ok(Ok(Some(line))) => line,
        Ok(Ok(None)) => return Err(ClientError::NoResponse),
        Ok(Err(_)) => return Err(ClientError::MalformedResponse),
        Err(_) => return Err(ClientError::NoResponse),
    };
    serde_json::from_str(line.trim()).map_err(|_| ClientError::MalformedResponse)
}

async fn write_request<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
    write_timeout: Duration,
) -> Result<(), ClientError> {
    let write = async {
        writer
            .write_all(payload)
            .await
            .map_err(|e| ClientError::WriteFailed(e.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|e| ClientError::FlushFailed(e.to_string()))
    };
    match timeout(write_timeout, write).await {
        Ok(result) => result,
        Err(_) => Err(ClientError::WriteFailed(
            "remote request write timed out".to_string(),
        )),
    }
}

async fn exchange_for_cli(parsed: Parsed) -> i32 {
    let resp = match send(parsed.command).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ytt -r: {}", e.human_message());
            return EXIT_TRANSPORT;
        }
    };

    if resp.ok {
        if parsed.json {
            match serde_json::to_string(&resp) {
                Ok(line) => println!("{line}"),
                Err(e) => {
                    eprintln!("ytt -r: could not encode response: {e}");
                    return EXIT_TRANSPORT;
                }
            }
        } else if !parsed.quiet
            && let Some(msg) = &resp.message
        {
            println!("{msg}");
        }
        EXIT_OK
    } else {
        // Errors always print, even under `-q`. The machine reason is the actionable bit.
        let reason = resp.reason.as_deref().unwrap_or("rejected");
        eprintln!("ytt -r: {reason}");
        EXIT_USAGE
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::remote::proto::InstanceMode;
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

    async fn serve_one_response(listener: Listener, response_line: String) {
        let conn = listener.accept().await.unwrap();
        {
            let mut reader = BufReader::new(&conn);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            assert!(request.contains("\"version\""));
            assert!(request.contains("\"secret\""));
        }
        let mut writer = &conn;
        writer.write_all(response_line.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();
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

    async fn serve_corrupt_then_cached_then_fresh(
        listener: Listener,
    ) -> ([Option<String>; 3], usize) {
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
            artwork: None,
        };
        let response = serde_json::to_string(&RemoteResponse::status(snapshot.clone())).unwrap();
        let server = tokio::spawn(serve_one_response(listener, response));

        let resp = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::Status)
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
        let server = tokio::spawn(serve_one_response(listener, response));

        let resp = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::Next)
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

        let response =
            send_to_instance(legacy_v8_instance(endpoint.clone()), RemoteCommand::Status)
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

        let err = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::Status)
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
}

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
            };
        }
        if buf.len() >= MAX_ONESHOT_REPLY_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "remote response too large",
            ));
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
}
