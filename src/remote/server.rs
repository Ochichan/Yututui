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
use tokio::io::AsyncWriteExt;
#[cfg(test)]
use tokio::io::BufReader;
use tokio::sync::oneshot;
use tokio::time::timeout;

use super::endpoint;
use super::proto::{
    HelloRequest, InstanceFile, InstanceMode, PROTOCOL_VERSION, PROTOCOL_VERSION_V7, RemoteCommand,
    RemoteRequest, RemoteResponse, Topic,
};
use super::sessions::{RemoteSessionHub, RemoteSessionRef, SessionTuning, run_session};

/// How long the single-instance probe waits for the existing server to answer a connect.
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
/// How long a connection may take to deliver its one request line before we drop it.
const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// Normal remote requests are tiny JSON objects. Bound one line to avoid unbounded buffering.
const MAX_REQUEST_BYTES: usize = 4 * 1024;
/// Cap on connections still in the pre-handshake window (before a one-shot reply or a session
/// `hello`). Established sessions release their slot at handoff and are then bounded separately
/// by the session cap, so this only limits a transient same-user connect burst — set well above
/// the session cap plus a realistic client fleet.
const PREHANDSHAKE_MAX: usize = 32;
/// The first line is always a small one-shot request or a `hello` handshake; bound it far below
/// the 256 KB session-frame size so a pre-handshake peer can't make the server buffer a large
/// line before it is even parsed. Subsequent session frames keep the larger cap.
const PREHANDSHAKE_MAX_LINE: usize = 32 * 1024;

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
    /// A session sent `Subscribe`. Handled on the owner loop — never the reducer — by
    /// the [`crate::remote::publish::Publisher`]: it records the subscriptions, emits
    /// one initial snapshot per newly subscribed topic, then the `Reply`, all in order
    /// into this session's outbound queue (docs/gui/02 §6/§8).
    SessionSubscribe {
        session: RemoteSessionRef,
        frame_id: u64,
        topics: Vec<Topic>,
    },
}

pub(crate) type EventSink = Arc<dyn Fn(RemoteEvent) -> bool + Send + Sync>;

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
    /// Returns the cleanup guard plus the session hub the host hands to its
    /// [`crate::remote::publish::Publisher`].
    pub fn start<F>(self, emit: F) -> (InstanceGuard, Arc<RemoteSessionHub>)
    where
        F: Fn(RemoteEvent) -> bool + Send + Sync + 'static,
    {
        let hub = Arc::new(RemoteSessionHub::new(
            self.mode,
            self.capabilities.clone(),
            SessionTuning::default(),
        ));
        tokio::spawn(serve(
            self.listener,
            Arc::clone(&self.token),
            Arc::new(emit),
            Arc::clone(&hub),
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
        (
            InstanceGuard {
                socket: self.endpoint,
                owns_instance_file: self.owns_instance_file,
            },
            hub,
        )
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
        // v8 sessions with live push (docs/gui/02 §10) — advertised now that subscribe
        // delivers initial snapshots through the owner-lane Publisher.
        "events-v8".to_string(),
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
pub(crate) async fn serve(
    listener: Listener,
    token: Arc<str>,
    emit: EventSink,
    hub: Arc<RemoteSessionHub>,
) {
    // A healthy listener only errors transiently. If `accept` fails repeatedly the listener is
    // wedged and a bare `continue` becomes a hot spin (one CPU pegged for nothing) — bail after
    // a short run of consecutive failures. Remote control degrades; the player keeps running.
    const MAX_CONSECUTIVE_ERRORS: u32 = 16;
    let mut errors: u32 = 0;
    // Bounds concurrent connections still in the pre-handshake window (see PREHANDSHAKE_MAX).
    let gate = Arc::new(tokio::sync::Semaphore::new(PREHANDSHAKE_MAX));
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
        // Refuse to spawn beyond the pre-handshake cap so a burst of same-user connects can't
        // pile unbounded tasks (each buffering a first line). The permit is released at the
        // one-shot reply or the session handoff, so long-lived sessions don't hold a slot.
        let Ok(permit) = Arc::clone(&gate).try_acquire_owned() else {
            tracing::debug!("remote: pre-handshake connection cap reached; dropping connection");
            drop(conn);
            continue;
        };
        let emit = emit.clone();
        let token = token.clone();
        let hub = Arc::clone(&hub);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(conn, &token, emit, hub, permit).await {
                tracing::debug!(error = %e, "remote: connection ended with error");
            }
        });
    }
}

/// Read the first line and discriminate the connection mode (docs/gui/02 §4.3):
/// a one-shot `RemoteRequest` (`command` key) is answered and closed; a `HelloRequest`
/// (`hello` key) upgrades into a long-lived session. The two shapes are structurally
/// unambiguous, so this is two explicit parse attempts, never `untagged`.
///
/// The first line is read **unbuffered** (byte-wise off the raw stream): a `BufReader`
/// here could slurp bytes past line 1 — frames a fast session client already sent — and
/// dropping it would lose them. The read buffer is sized for session frames from the
/// start; the one-shot 4 KB cap is enforced post-parse.
async fn handle_conn(
    conn: Stream,
    token: &str,
    emit: EventSink,
    hub: Arc<RemoteSessionHub>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> io::Result<()> {
    let mut line = Vec::new();
    {
        let mut raw = &conn;
        match timeout(
            READ_TIMEOUT,
            read_bounded_line(&mut raw, &mut line, PREHANDSHAKE_MAX_LINE),
        )
        .await
        {
            Ok(Ok(ReadLineOutcome::Line)) => {}
            Ok(Ok(ReadLineOutcome::TooLarge)) => {
                write_response(&conn, &RemoteResponse::err("bad_request")).await?;
                return Ok(());
            }
            // EOF, read error, or timeout: drop the connection silently.
            _ => return Ok(()),
        }
    }

    let text = match std::str::from_utf8(&line) {
        Ok(s) => s.trim(),
        Err(_) => {
            write_response(&conn, &RemoteResponse::err("bad_request")).await?;
            return Ok(());
        }
    };

    if let Ok(req) = serde_json::from_str::<RemoteRequest>(text) {
        // One-shot mode keeps its historical 4 KB bound, enforced after the parse
        // (the shared first-line buffer had to be session-sized).
        let resp = if line.len() > MAX_REQUEST_BYTES {
            RemoteResponse::err("bad_request")
        } else {
            build_response(req, token, &emit).await
        };
        write_response(&conn, &resp).await?;
        return Ok(());
    }
    if let Ok(hello) = serde_json::from_str::<HelloRequest>(text) {
        // Handshake done: release the pre-handshake slot before the long-lived session, which
        // is bounded for the rest of its life by the separate session cap.
        drop(permit);
        return run_session(conn, hello, token, hub, emit).await;
    }
    write_response(&conn, &RemoteResponse::err("bad_request")).await?;
    Ok(())
}

pub(crate) enum ReadLineOutcome {
    Line,
    TooLarge,
}

/// Thin wrapper over the shared [`crate::util::io::read_bounded_line`] that keeps the remote
/// protocol's semantics: a clean EOF is an error here (a peer vanishing mid-request is fatal),
/// unlike mpv where EOF is a normal close. One reader implementation, two policies.
pub(crate) async fn read_bounded_line<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max_bytes: usize,
) -> io::Result<ReadLineOutcome> {
    match crate::util::io::read_bounded_line(reader, line, max_bytes).await? {
        crate::util::io::BoundedLine::Line => Ok(ReadLineOutcome::Line),
        crate::util::io::BoundedLine::TooLarge => Ok(ReadLineOutcome::TooLarge),
        crate::util::io::BoundedLine::Eof => {
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "remote EOF"))
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
async fn build_response(req: RemoteRequest, token: &str, emit: &EventSink) -> RemoteResponse {
    // Range, not equality: the v7 one-shot shapes are frozen, so a v8 server keeps
    // serving shipped v7 clients (`ytt -r`, the tray) forever (docs/gui/02 §9).
    if !(PROTOCOL_VERSION_V7..=PROTOCOL_VERSION).contains(&req.version) {
        return RemoteResponse::err("bad_version");
    }
    if req.token != token {
        return RemoteResponse::err("bad_token");
    }
    if let Err(err) = req.command.validate() {
        return RemoteResponse::err(err.reason());
    }

    let reply_timeout = super::reply_timeout_for(&req.command);
    let (reply_tx, reply_rx) = oneshot::channel();
    if !emit(RemoteEvent::Command(req.command, reply_tx)) {
        return RemoteResponse::err("shutting_down");
    }
    match timeout(reply_timeout, reply_rx).await {
        Ok(Ok(resp)) => resp,
        _ => RemoteResponse::err("timeout"),
    }
}

#[cfg(test)]
fn test_hub() -> Arc<RemoteSessionHub> {
    test_hub_with(SessionTuning::default())
}

#[cfg(test)]
fn test_hub_with(tuning: SessionTuning) -> Arc<RemoteSessionHub> {
    Arc::new(RemoteSessionHub::new(
        InstanceMode::StandaloneTui,
        vec!["events-v8".to_string()],
        tuning,
    ))
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
            test_hub(),
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

        // Valid JSON under the one-shot byte cap, but semantically too large: rejected
        // before the reducer stand-in can answer with "pong".
        let bad_query = RemoteRequest {
            version: PROTOCOL_VERSION,
            token: "secret".to_string(),
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
            command: RemoteCommand::Play {
                query: "q".repeat(MAX_REQUEST_BYTES),
            },
        };
        let resp = parse(&send_line(&path, &serde_json::to_string(&req).unwrap()).await);
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("bad_request"));

        let _ = std::fs::remove_file(&path);
    }
}

/// Session-mode socket tests. Deliberately NOT unix-gated: on the Windows CI leg these
/// run over a real named pipe, which makes them the standing long-lived-duplex smoke the
/// v8 design calls out as its one genuinely new transport risk (docs/gui/02 §19.1).
#[cfg(test)]
mod session_socket_tests {
    use super::*;
    use crate::remote::proto::{ClientFrame, ClientOp, HelloAck, HelloBody, ServerFrame, Topic};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::time::timeout as ttimeout;

    const T: Duration = Duration::from_secs(5);

    fn test_endpoint(tag: &str) -> String {
        #[cfg(windows)]
        {
            format!(
                r"\\.\pipe\ytm-tui-session-test-{}-{tag}",
                std::process::id()
            )
        }
        #[cfg(unix)]
        {
            let ep = std::env::temp_dir()
                .join(format!(
                    "ytm-tui-session-test-{}-{tag}.sock",
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
    fn start_server(tag: &str, hub: Arc<RemoteSessionHub>) -> String {
        let ep = test_endpoint(tag);
        let listener = bind(&ep).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RemoteEvent>();
        let publisher_hub = Arc::clone(&hub);
        tokio::spawn(async move {
            let mut publisher = crate::remote::publish::Publisher::new(publisher_hub);
            let mut queue = crate::queue::Queue::default();
            queue.set(
                vec![crate::api::Song::remote("vid", "Song", "Artist", "3:45")],
                0,
            );
            while let Some(event) = rx.recv().await {
                match event {
                    RemoteEvent::Command(_cmd, reply) => {
                        let _ = reply.send(RemoteResponse::ok("pong".to_string()));
                    }
                    RemoteEvent::SessionSubscribe {
                        session,
                        frame_id,
                        topics,
                    } => {
                        publisher.handle_subscribe(
                            &crate::remote::publish::test_view(&queue),
                            &session,
                            frame_id,
                            &topics,
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

    async fn connect(ep: &str) -> Stream {
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

    async fn write_json<W: tokio::io::AsyncWrite + Unpin, S: serde::Serialize>(
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

    async fn read_json_line<R: tokio::io::AsyncBufRead + Unpin, D: serde::de::DeserializeOwned>(
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

    fn hello(version: u8, min_version: u8, token: &str) -> HelloRequest {
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
}
