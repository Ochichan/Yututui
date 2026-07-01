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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use super::args::{self, ParseError, Parsed};
use super::endpoint;
use super::proto::{InstanceFile, PROTOCOL_VERSION, RemoteCommand, RemoteRequest, RemoteResponse};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const SEARCH_REPLY_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REPLY_BYTES: usize = 4096;

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
    let Ok(name) = instance.endpoint.as_str().to_fs_name::<GenericFilePath>() else {
        return Err(ClientError::MalformedEndpoint);
    };

    let conn = match timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
        Ok(Ok(c)) => c,
        // Connect refused / timed out: the descriptor is stale or the instance just exited.
        _ => return Err(ClientError::ConnectFailed),
    };

    let reply_timeout = reply_timeout_for(&command);
    let req = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: instance.token,
        command,
    };
    let mut payload = match serde_json::to_vec(&req) {
        Ok(v) => v,
        Err(e) => return Err(ClientError::EncodeFailed(e.to_string())),
    };
    payload.push(b'\n');

    {
        let mut writer = &conn;
        if let Err(e) = writer.write_all(&payload).await {
            return Err(ClientError::WriteFailed(e.to_string()));
        }
        if let Err(e) = writer.flush().await {
            return Err(ClientError::FlushFailed(e.to_string()));
        }
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

fn reply_timeout_for(command: &RemoteCommand) -> Duration {
    match command {
        RemoteCommand::Play { .. } | RemoteCommand::Enqueue { .. } => SEARCH_REPLY_TIMEOUT,
        _ => REPLY_TIMEOUT,
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
    use interprocess::local_socket::ListenerOptions;
    use interprocess::local_socket::tokio::Listener;
    use tokio::io::AsyncBufReadExt;

    fn test_endpoint(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("ytm-tui-client-{name}-{}.sock", std::process::id()))
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
            capabilities: vec!["remote-control".to_string(), "status".to_string()],
        }
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
    async fn send_to_instance_rejects_malformed_response() {
        let endpoint = test_endpoint("malformed");
        let listener = bind_test_listener(&endpoint);
        let server = tokio::spawn(serve_one_response(listener, "{not json}".to_string()));

        let err = send_to_instance(test_instance(endpoint.clone()), RemoteCommand::Status)
            .await
            .unwrap_err();
        server.await.unwrap();
        let _ = std::fs::remove_file(endpoint);

        assert_eq!(err, ClientError::MalformedResponse);
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
        if buf.len() >= MAX_REPLY_BYTES {
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
