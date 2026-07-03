//! The control-socket server, run inside the TUI process.
//!
//! Two jobs:
//!   1. **Single-instance guard** ([`bind_or_detect`]): a connect-probe decides whether a
//!      live instance already owns the socket. If so the new launch bows out; otherwise it
//!      clears any stale socket and binds. `interprocess`'s `reclaim_name` (default on)
//!      unlinks the socket when the listener drops, so a clean exit needs no manual unlink;
//!      we still best-effort it (plus the instance descriptor) via [`InstanceGuard`].
//!   2. **Accept loop** ([`serve`]): one line in, one line out per connection. Each request
//!      is forwarded to the runtime as [`RemoteEvent::Command`] with a oneshot reply channel, so the
//!      response reflects the real outcome (capability guards, `status`, clean `quit` ack).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use interprocess::local_socket::tokio::Listener;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::endpoint;
use super::proto::{
    InstanceFile, InstanceMode, PROTOCOL_VERSION, PROTOCOL_VERSION_V7, RemoteCommand,
    RemoteRequest, RemoteResponse,
};

/// How long the single-instance probe waits for the existing server to answer a connect.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// How long a connection may take to deliver its one request line before we drop it.
const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// How long to wait for the reducer to compute a reply (it runs on the main loop).
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Normal remote requests are tiny JSON objects. Bound one line to avoid unbounded buffering.
const MAX_REQUEST_BYTES: usize = 4 * 1024;

/// Outcome of trying to become the controllable instance.
pub enum BindOutcome {
    /// A live instance already owns the socket — the new launch should bow out.
    AlreadyRunning,
    /// We own the socket; spawn [`RemoteServer::start`] and keep its guard alive.
    Bound(Box<RemoteServer>),
    /// Could not bind (e.g. unwritable runtime dir). Run the TUI without remote control.
    Unavailable,
}

/// Events emitted by the remote-control server.
pub enum RemoteEvent {
    Command(RemoteCommand, oneshot::Sender<RemoteResponse>),
}

type EventSink = Arc<dyn Fn(RemoteEvent) -> bool + Send + Sync>;

/// A bound server ready to start serving. Hands back an [`InstanceGuard`] on [`start`].
pub struct RemoteServer {
    listener: Listener,
    token: Arc<str>,
    /// The bound socket path/name — kept so the descriptor and cleanup guard can be built at
    /// [`start`] time rather than at bind time.
    endpoint: String,
    /// Whether this server publishes the *shared* instance descriptor. A `--new-instance`
    /// secondary binds a private socket and never advertises itself to `ytt -r`.
    owns_instance_file: bool,
    mode: InstanceMode,
    capabilities: Vec<String>,
}

impl RemoteServer {
    pub fn with_instance_metadata(mut self, mode: InstanceMode, capabilities: Vec<String>) -> Self {
        self.mode = mode;
        self.capabilities = capabilities;
        self
    }

    /// Spawn the accept loop, **then** publish the instance descriptor, and return the cleanup
    /// guard. Publishing only after the accept loop exists — and only when the caller is about
    /// to enter the reducer loop — closes the startup race where the descriptor advertised an
    /// endpoint that nothing was yet accepting on, so a `ytt -r` fired during cold start saw a
    /// live descriptor but got no reply. Keep the returned guard alive for the whole session;
    /// dropping it removes the descriptor (and best-effort the socket).
    pub fn start<F>(self, emit: F) -> InstanceGuard
    where
        F: Fn(RemoteEvent) -> bool + Send + Sync + 'static,
    {
        tokio::spawn(serve(
            self.listener,
            Arc::clone(&self.token),
            Arc::new(emit),
        ));
        if self.owns_instance_file
            && let Err(e) = endpoint::write_instance(&InstanceFile {
                app_pid: std::process::id(),
                endpoint: self.endpoint.clone(),
                token: self.token.to_string(),
                created_unix: now_unix(),
                mode: self.mode,
                protocol_version: PROTOCOL_VERSION,
                capabilities: self.capabilities.clone(),
            })
        {
            // The socket is up but we couldn't advertise where/how to reach it. Clients won't
            // find us, so remote control is effectively off this run — log and degrade.
            tracing::warn!(error = %e, "remote: could not write instance descriptor; no remote control");
        }
        InstanceGuard {
            socket: self.endpoint,
            owns_instance_file: self.owns_instance_file,
        }
    }
}

/// Removes the instance descriptor (and best-effort the socket) on drop. The listener also
/// unlinks the socket on drop (`reclaim_name`); this covers the descriptor plus the case
/// where the listener task outlives a clean reducer shutdown.
pub struct InstanceGuard {
    /// The socket path/name — unlinked best-effort on Unix; named pipes self-clean.
    #[cfg_attr(windows, allow(dead_code))]
    socket: String,
    /// A secondary (`--new-instance`) server never published the shared descriptor, so it
    /// must not delete it on the way out.
    owns_instance_file: bool,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if self.owns_instance_file {
            endpoint::remove_instance();
        }
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.socket);
        }
    }
}

/// Probe whether a live server is listening on `endpoint_name`.
async fn probe_alive(endpoint_name: &str) -> bool {
    let Ok(name) = endpoint_name.to_fs_name::<GenericFilePath>() else {
        return false;
    };
    matches!(
        timeout(PROBE_TIMEOUT, Stream::connect(name)).await,
        Ok(Ok(_))
    )
}

/// Bind a tokio listener on `endpoint_name`.
///
/// `reclaim_name(false)` disables `interprocess`'s default unlink-on-listener-drop: otherwise
/// the listener (owned by the `serve` task, dropped only when the runtime is) and
/// [`InstanceGuard`] (dropped at end of `run`) are two independent unlinkers of the same path.
/// If a fast restart rebinds the path between those two drops, the late listener-drop would
/// unlink the *new* instance's socket. [`InstanceGuard`] is the single cleanup path instead.
fn bind(endpoint_name: &str) -> io::Result<Listener> {
    let name = endpoint_name.to_fs_name::<GenericFilePath>()?;
    ListenerOptions::new()
        .name(name)
        .reclaim_name(false)
        .create_tokio()
}

/// Decide whether to run as the controllable instance, and bind the socket if so.
///
/// `new_instance` (from `ytt --new-instance`) skips detection entirely: it binds a
/// pid-qualified endpoint and does **not** publish the shared descriptor, so `ytt -r`
/// keeps targeting the primary.
pub async fn bind_or_detect(new_instance: bool) -> BindOutcome {
    if new_instance {
        let ep = match endpoint::alt_socket_endpoint(std::process::id()) {
            Ok(ep) => ep,
            Err(e) => {
                tracing::warn!(error = %e, "remote: could not resolve runtime dir; no remote control");
                return BindOutcome::Unavailable;
            }
        };
        #[cfg(unix)]
        let _ = std::fs::remove_file(&ep);
        return match bind(&ep) {
            Ok(listener) => BindOutcome::Bound(Box::new(RemoteServer {
                listener,
                token: Arc::from(""),
                endpoint: ep,
                owns_instance_file: false,
                mode: InstanceMode::StandaloneTui,
                capabilities: default_capabilities(),
            })),
            Err(e) => {
                tracing::warn!(error = %e, "remote: secondary instance could not bind; no remote control");
                BindOutcome::Unavailable
            }
        };
    }

    let ep = match endpoint::socket_endpoint() {
        Ok(ep) => ep,
        Err(e) => {
            tracing::warn!(error = %e, "remote: could not resolve runtime dir; no remote control");
            return BindOutcome::Unavailable;
        }
    };
    if probe_alive(&ep).await {
        return BindOutcome::AlreadyRunning;
    }
    let legacy_ep = endpoint::legacy_primary_endpoint_for_probe();
    if legacy_ep != ep && probe_alive(&legacy_ep).await {
        return BindOutcome::AlreadyRunning;
    }
    // Stale or absent: best-effort clear the leftover socket file, then bind.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&ep);

    let listener = match bind(&ep) {
        Ok(l) => l,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // Lost a bind race. Re-probe: a live answerer means someone beat us here;
            // otherwise the file is stale — unlink and retry once (not `try_overwrite`,
            // which would also displace a *live* server).
            if probe_alive(&ep).await {
                return BindOutcome::AlreadyRunning;
            }
            #[cfg(unix)]
            let _ = std::fs::remove_file(&ep);
            match bind(&ep) {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(error = %e, "remote: could not bind control socket; no remote control");
                    return BindOutcome::Unavailable;
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "remote: could not bind control socket; no remote control");
            return BindOutcome::Unavailable;
        }
    };

    // Note: the instance descriptor is published in `RemoteServer::start`, after the accept
    // loop is spawned — not here — so `ytt -r` never finds a descriptor before anything is
    // accepting on it. We only bind the socket now (which is what the single-instance probe
    // checks); the socket alone, with no descriptor, is invisible to clients.
    let token = match endpoint::gen_token() {
        Ok(token) => token,
        Err(e) => {
            tracing::warn!(error = %e, "remote: could not generate control token; no remote control");
            return BindOutcome::Unavailable;
        }
    };
    BindOutcome::Bound(Box::new(RemoteServer {
        listener,
        token: Arc::from(token.as_str()),
        endpoint: ep,
        owns_instance_file: true,
        mode: InstanceMode::StandaloneTui,
        capabilities: default_capabilities(),
    }))
}

fn default_capabilities() -> Vec<String> {
    vec![
        "remote-control".to_string(),
        "status".to_string(),
        "queue-control".to_string(),
    ]
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Accept connections forever, handling each on its own task.
pub async fn serve(listener: Listener, token: Arc<str>, emit: EventSink) {
    // A healthy listener only errors transiently. If `accept` fails repeatedly the listener is
    // wedged and a bare `continue` becomes a hot spin (one CPU pegged for nothing) — bail after
    // a short run of consecutive failures. Remote control degrades; the player keeps running.
    const MAX_CONSECUTIVE_ERRORS: u32 = 16;
    let mut errors: u32 = 0;
    loop {
        let conn = match listener.accept().await {
            Ok(c) => {
                errors = 0;
                c
            }
            Err(e) => {
                errors += 1;
                tracing::warn!(error = %e, errors, "remote: accept failed");
                if errors >= MAX_CONSECUTIVE_ERRORS {
                    tracing::warn!("remote: too many accept failures; stopping the control server");
                    return;
                }
                continue;
            }
        };
        let emit = emit.clone();
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(conn, &token, emit).await {
                tracing::debug!(error = %e, "remote: connection ended with error");
            }
        });
    }
}

/// Read one request line (bounded), compute a response via the reducer, write it back.
async fn handle_conn(conn: Stream, token: &str, emit: EventSink) -> io::Result<()> {
    let mut reader = BufReader::new(&conn);
    let mut line = Vec::new();
    match timeout(READ_TIMEOUT, read_bounded_line(&mut reader, &mut line)).await {
        Ok(Ok(ReadLineOutcome::Line)) => {}
        Ok(Ok(ReadLineOutcome::TooLarge)) => {
            write_response(&conn, &RemoteResponse::err("bad_request")).await?;
            return Ok(());
        }
        // EOF, read error, or timeout: drop the connection silently.
        _ => return Ok(()),
    }

    let line = match std::str::from_utf8(&line) {
        Ok(s) => s.trim(),
        Err(_) => {
            write_response(&conn, &RemoteResponse::err("bad_request")).await?;
            return Ok(());
        }
    };
    let resp = build_response(line, token, &emit).await;
    write_response(&conn, &resp).await?;
    Ok(())
}

enum ReadLineOutcome {
    Line,
    TooLarge,
}

async fn read_bounded_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> io::Result<ReadLineOutcome> {
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "remote EOF"));
        }
        line.push(byte[0]);
        if line.len() > MAX_REQUEST_BYTES {
            return Ok(ReadLineOutcome::TooLarge);
        }
        if byte[0] == b'\n' {
            return Ok(ReadLineOutcome::Line);
        }
    }
}

async fn write_response(conn: &Stream, resp: &RemoteResponse) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&resp).unwrap_or_else(|_| br#"{"ok":false}"#.to_vec());
    bytes.push(b'\n');

    let mut writer = conn;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Validate the request and round-trip it through the reducer for the real outcome.
async fn build_response(line: &str, token: &str, emit: &EventSink) -> RemoteResponse {
    let req: RemoteRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => return RemoteResponse::err("bad_request"),
    };
    // Range, not equality: the v7 one-shot shapes are frozen, so a v8 server keeps
    // serving shipped v7 clients (`ytt -r`, the tray) forever (docs/gui/02 §9).
    if !(PROTOCOL_VERSION_V7..=PROTOCOL_VERSION).contains(&req.version) {
        return RemoteResponse::err("bad_version");
    }
    if req.token != token {
        return RemoteResponse::err("bad_token");
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    if !emit(RemoteEvent::Command(req.command, reply_tx)) {
        return RemoteResponse::err("shutting_down");
    }
    match timeout(REPLY_TIMEOUT, reply_rx).await {
        Ok(Ok(resp)) => resp,
        _ => RemoteResponse::err("timeout"),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::remote::proto::RemoteCommand;
    use tokio::io::AsyncBufReadExt;

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
    async fn server_round_trips_request_through_the_reducer() {
        let path = std::env::temp_dir()
            .join(format!("ytm-tui-remote-test-{}.sock", std::process::id()))
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
        ));

        // Correct token → the reducer's response is relayed back verbatim.
        let req = RemoteRequest {
            version: PROTOCOL_VERSION,
            token: "secret".to_string(),
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
            command: RemoteCommand::TogglePause,
        };
        let resp = parse(&send_line(&path, &serde_json::to_string(&legacy).unwrap()).await);
        assert!(resp.ok, "v7 one-shot must keep working: {resp:?}");
        assert_eq!(resp.message.as_deref(), Some("pong"));

        // Anything below the frozen floor fails loudly.
        let ancient = RemoteRequest {
            version: 6,
            token: "secret".to_string(),
            command: RemoteCommand::TogglePause,
        };
        let resp = parse(&send_line(&path, &serde_json::to_string(&ancient).unwrap()).await);
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("bad_version"));

        // Wrong token → rejected before reaching the reducer.
        let bad = RemoteRequest {
            version: PROTOCOL_VERSION,
            token: "nope".to_string(),
            command: RemoteCommand::TogglePause,
        };
        let resp = parse(&send_line(&path, &serde_json::to_string(&bad).unwrap()).await);
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("bad_token"));

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
                "ytm-tui-remote-oversized-test-{}.sock",
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
        ));

        let too_large = format!("{} \n", "x".repeat(MAX_REQUEST_BYTES + 1));
        let resp = parse(&send_line(&path, &too_large).await);
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("bad_request"));

        let _ = std::fs::remove_file(&path);
    }
}
