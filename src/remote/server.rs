//! The control-socket server, run inside the TUI process.
//!
//! Two jobs:
//!   1. **Single-instance guard** ([`bind_or_detect`]): a connect-probe decides whether a
//!      live instance already owns the socket. If so the new launch bows out; otherwise it
//!      clears any stale socket and binds. Listener-side name reclamation is disabled so the
//!      single [`InstanceGuard`] cleanup path can release descriptor and socket in one order.
//!   2. **Accept loop** (`serve_loop`): one line in, one line out per connection. Each request
//!      is forwarded to the runtime with a [`RemoteReply`], so the response reflects the real
//!      outcome. Persistent-session replies are enqueued synchronously before same-turn pushes.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "linux")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};

use interprocess::local_socket::tokio::Listener;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions};
#[cfg(test)]
use tokio::io::BufReader;
use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinSet};
use tokio::time::timeout;

use super::WireSettlement;
use super::endpoint;
#[cfg(all(test, unix))]
use super::proto::PROTOCOL_VERSION_V7;
use super::proto::{
    HelloRequest, HostTerminalHint, InstanceFile, InstanceMode, PROTOCOL_VERSION,
    RETAINED_REQUEST_OUTCOMES_CAPABILITY, RemoteCommand, RemoteRequest, RemoteResponse, Topic,
};
use super::sessions::{RemoteSessionHub, RemoteSessionRef, SessionTuning, run_session};

mod endpoint_lease;
mod one_shot;

use endpoint_lease::EndpointLease;
use one_shot::{
    TrackedRemoteResponse, build_tracked_response, write_response, write_tracked_response,
};
#[cfg(all(test, unix))]
use one_shot::{build_response, write_response_to};

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
/// A run of this many listener errors means the accept owner is no longer usable.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 16;

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
    Command(RemoteCommand, RemoteReply),
    /// A long-lived session command with its requester lifetime preserved end to end.
    SessionCommand {
        command: RemoteCommand,
        origin: super::RemoteSessionScope,
        reply: RemoteReply,
    },
    /// A session sent `Subscribe`. Handled on the owner loop — never the reducer — by
    /// the [`crate::remote::publish::Publisher`]: it records the subscriptions, emits
    /// one initial snapshot per newly subscribed topic, then the `Reply`, all in order
    /// into this session's outbound queue (docs/gui/02 §6/§8).
    SessionSubscribe {
        session: RemoteSessionRef,
        frame_id: u64,
        page_id: Option<String>,
        topics: Vec<Topic>,
        settlement: WireSettlement,
    },
}

/// The owner-loop half of a remote command response.
///
/// One-shot v7/v8 requests still use a Tokio oneshot internally. Persistent v8 sessions install
/// a direct callback that synchronously enqueues `ServerFrame::Reply` into that session's outbound
/// lane. Consequently, the existing owner loops' `reply.send(response)` call is the ordering
/// barrier: every same-turn [`crate::remote::publish::Publisher::observe`] happens afterwards.
pub struct RemoteReply(RemoteReplyInner);

enum RemoteReplyInner {
    OneShot(oneshot::Sender<RemoteResponse>),
    Direct(Box<dyn FnOnce(RemoteResponse) -> bool + Send>),
}

impl RemoteReply {
    /// Complete a remote command exactly once.
    pub fn send(self, response: RemoteResponse) -> bool {
        match self.0 {
            RemoteReplyInner::OneShot(reply) => reply.send(response).is_ok(),
            RemoteReplyInner::Direct(reply) => reply(response),
        }
    }

    pub(crate) fn direct(reply: impl FnOnce(RemoteResponse) -> bool + Send + 'static) -> Self {
        Self(RemoteReplyInner::Direct(Box::new(reply)))
    }
}

impl From<oneshot::Sender<RemoteResponse>> for RemoteReply {
    fn from(reply: oneshot::Sender<RemoteResponse>) -> Self {
        Self(RemoteReplyInner::OneShot(reply))
    }
}

pub(crate) type EventSink = Arc<dyn Fn(RemoteEvent) -> bool + Send + Sync>;

/// A bound server ready to start serving. Hands back an [`InstanceGuard`] on
/// [`RemoteServer::start`].
pub struct RemoteServer {
    listener: Option<Listener>,
    token: Arc<str>,
    /// The bound socket path/name — kept so the descriptor and cleanup guard can be built at
    /// [`start`] time rather than at bind time.
    endpoint: String,
    /// Whether this server publishes the *shared* instance descriptor. A `--new-instance`
    /// secondary binds a private socket and never advertises itself to `ytt -r`.
    owns_instance_file: bool,
    mode: InstanceMode,
    capabilities: Vec<String>,
    /// Terminal identity advertised in the descriptor so a second launch can try to focus
    /// the hosting window. Only the interactive TUI attaches one; daemons stay `None`.
    host_terminal: Option<HostTerminalHint>,
}

impl RemoteServer {
    pub fn with_instance_metadata(mut self, mode: InstanceMode, capabilities: Vec<String>) -> Self {
        self.mode = mode;
        self.capabilities = capabilities;
        self
    }

    pub fn with_host_terminal(mut self, hint: HostTerminalHint) -> Self {
        self.host_terminal = Some(hint);
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
    pub fn start<F>(mut self, emit: F) -> (InstanceGuard, Arc<RemoteSessionHub>)
    where
        F: Fn(RemoteEvent) -> bool + Send + Sync + 'static,
    {
        #[cfg(unix)]
        let socket_identity = match socket_file_identity(&self.endpoint) {
            Ok(identity) => Some(identity),
            Err(error) => {
                tracing::warn!(%error, "remote: could not capture bound socket identity");
                None
            }
        };
        let listener = self
            .listener
            .take()
            .expect("a remote server can only be started once");
        let endpoint = std::mem::take(&mut self.endpoint);
        let hub = Arc::new(RemoteSessionHub::new(
            self.mode,
            self.capabilities.clone(),
            SessionTuning::default(),
        ));
        let endpoint_lease = Arc::new(EndpointLease::new(
            endpoint,
            #[cfg(unix)]
            socket_identity,
        ));
        let serve_task = spawn_accept_owner(
            listener,
            Arc::clone(&self.token),
            Arc::new(emit),
            Arc::clone(&hub),
            Arc::clone(&endpoint_lease),
        );
        let advertisement = self.owns_instance_file.then(|| InstanceFile {
            app_pid: std::process::id(),
            endpoint: endpoint_lease.socket().to_string(),
            token: self.token.to_string(),
            created_unix: now_unix(),
            mode: self.mode,
            protocol_version: PROTOCOL_VERSION,
            capabilities: self.capabilities.clone(),
            host_terminal: self.host_terminal.clone(),
        });
        endpoint_lease.publish_instance(advertisement);
        (
            InstanceGuard {
                endpoint_lease,
                hub: Arc::clone(&hub),
                serve_task: Some(serve_task),
            },
            hub,
        )
    }
}

impl Drop for RemoteServer {
    fn drop(&mut self) {
        let Some(listener) = self.listener.take() else {
            return;
        };
        #[cfg(unix)]
        let identity = socket_file_identity(&self.endpoint).ok();
        // Close the listener before unlinking its exact socket. This is the early-startup cleanup
        // path used when writer-lease or recovery preflight fails after single-instance binding.
        drop(listener);
        #[cfg(unix)]
        if let Err(error) = remove_socket_file_if_matches(&self.endpoint, identity.as_ref()) {
            tracing::warn!(%error, "remote: failed to release unstarted socket endpoint");
        }
    }
}

struct PublishedInstance {
    path: PathBuf,
    identity: InstanceFile,
}

#[cfg(unix)]
#[derive(Debug)]
struct SocketFileIdentity {
    device: u64,
    inode: u64,
    change_seconds: i64,
    change_nanoseconds: i64,
    // Keep the old inode allocated after its listener closes so Linux cannot immediately reuse
    // the same identity for a rebound successor before this lease finishes cleanup.
    #[cfg(target_os = "linux")]
    _generation_guard: std::fs::File,
}

#[cfg(unix)]
impl PartialEq for SocketFileIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.device == other.device
            && self.inode == other.inode
            && self.change_seconds == other.change_seconds
            && self.change_nanoseconds == other.change_nanoseconds
    }
}

#[cfg(unix)]
impl Eq for SocketFileIdentity {}

#[cfg(unix)]
fn socket_file_identity(path: &str) -> io::Result<SocketFileIdentity> {
    #[cfg(target_os = "linux")]
    let (metadata, generation_guard) = {
        let generation_guard = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = generation_guard.metadata()?;
        (metadata, generation_guard)
    };
    #[cfg(not(target_os = "linux"))]
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote endpoint is not a socket",
        ));
    }
    Ok(SocketFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        // Linux can immediately reuse a just-unlinked socket inode. Pair the stable inode key
        // with its change generation so a late owner cannot mistake the rebound socket for its
        // own (an ABA collision on device + inode alone).
        change_seconds: metadata.ctime(),
        change_nanoseconds: metadata.ctime_nsec(),
        #[cfg(target_os = "linux")]
        _generation_guard: generation_guard,
    })
}

#[cfg(unix)]
fn remove_socket_file_if_matches(
    path: &str,
    expected: Option<&SocketFileIdentity>,
) -> io::Result<bool> {
    let Some(expected) = expected else {
        return Ok(false);
    };
    let current = match socket_file_identity(path) {
        Ok(current) => current,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if &current != expected {
        return Ok(false);
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Owns the advertised endpoint and the complete accept/connection task tree.
pub struct InstanceGuard {
    /// Shared with the accept task so an unrecoverable listener can revoke its own exact endpoint
    /// without waiting for the application owner to begin shutdown.
    endpoint_lease: Arc<EndpointLease>,
    /// The guard is the accept loop's final owner. Normal cleanup first wakes it through `hub`;
    /// abort remains the synchronous fallback when the caller drops the guard abruptly.
    hub: Arc<RemoteSessionHub>,
    serve_task: Option<tokio::task::JoinHandle<()>>,
}

impl InstanceGuard {
    /// Stop new connection/owner-event admission without cancelling established writers.
    pub fn quiesce_owner_admission(&self) {
        self.hub.quiesce_owner_admission();
    }

    /// Stop advertising this owner while its listener still owns the bound endpoint.
    ///
    /// Unix permits unlinking a bound socket. Doing that exactly once, before waking/dropping the
    /// listener, lets a successor bind immediately and makes every later cleanup path inert toward
    /// the successor's path. Existing accepted connections remain alive for the shutdown notice.
    pub fn release_endpoint(&mut self) {
        self.endpoint_lease.release();
    }

    /// Whether the accept owner exhausted its listener-error budget and self-revoked.
    pub fn accept_owner_failed(&self) -> bool {
        self.endpoint_lease.accept_owner_failed()
    }

    /// Latch session shutdown and join the accept owner, which aborts and joins all connection
    /// children before returning. Repeated calls are harmless.
    pub async fn shutdown(&mut self) {
        self.release_endpoint();
        self.hub.shutdown_all();
        let result = match self.serve_task.as_mut() {
            Some(serve_task) => serve_task.await,
            None => return,
        };
        // Keep the handle in the guard while it is awaited. If this future is cancelled, Drop
        // can still abort the accept owner rather than silently detaching it.
        self.serve_task.take();
        match result {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) => tracing::warn!(%error, "remote: accept owner failed during shutdown"),
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // Synchronous fallback for setup errors/panics. Release paths while the listener task is
        // still owned, then abort it; graceful callers use `shutdown` and leave this path inert.
        self.release_endpoint();
        self.hub.shutdown_all();
        if let Some(serve_task) = self.serve_task.take() {
            serve_task.abort();
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
                listener: Some(listener),
                token: Arc::from(""),
                endpoint: ep,
                owns_instance_file: false,
                mode: InstanceMode::StandaloneTui,
                capabilities: default_capabilities(),
                host_terminal: None,
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
        listener: Some(listener),
        token: Arc::from(token.as_str()),
        endpoint: ep,
        owns_instance_file: true,
        mode: InstanceMode::StandaloneTui,
        capabilities: default_capabilities(),
        host_terminal: None,
    }))
}

/// Wait (bounded) for the primary endpoint to stop answering, polling every 250ms.
///
/// Used by the second-launch restart path after sending `Quit`: `true` means the old
/// owner's socket went quiet within `deadline`. This keeps [`probe_alive`] private.
pub async fn await_primary_release(deadline: Duration) -> bool {
    let Ok(ep) = endpoint::socket_endpoint() else {
        return false;
    };
    let legacy_ep = endpoint::legacy_primary_endpoint_for_probe();
    let end = tokio::time::Instant::now() + deadline;
    loop {
        let live = probe_alive(&ep).await || (legacy_ep != ep && probe_alive(&legacy_ep).await);
        if !live {
            return true;
        }
        if tokio::time::Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn default_capabilities() -> Vec<String> {
    vec![
        "remote-control".to_string(),
        "status".to_string(),
        "queue-control".to_string(),
        super::PERSONAL_EXPORT_CAPABILITY.to_string(),
        RETAINED_REQUEST_OUTCOMES_CAPABILITY.to_string(),
        // v8 sessions with live push (docs/gui/02 §10) — advertised now that subscribe
        // delivers initial snapshots through the owner-lane Publisher.
        "events-v8".to_string(),
    ]
}

#[cfg(test)]
#[test]
fn standalone_capabilities_advertise_retained_request_outcomes() {
    assert!(default_capabilities().contains(&RETAINED_REQUEST_OUTCOMES_CAPABILITY.to_string()));
}

#[cfg(test)]
#[test]
fn standalone_capabilities_advertise_personal_export() {
    assert!(default_capabilities().contains(&super::PERSONAL_EXPORT_CAPABILITY.to_string()));
}

#[cfg(test)]
#[test]
fn standalone_capabilities_do_not_advertise_long_form_seek_gui_mutation() {
    assert!(
        !default_capabilities()
            .contains(&super::LONG_FORM_SEEK_OPTIMIZATION_CAPABILITY.to_string())
    );
}

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AcceptLoopExit {
    Quiesced,
    Shutdown,
    RepeatedErrors,
}

#[derive(Clone, Copy)]
enum AcceptFaults {
    None,
    #[cfg(all(test, unix))]
    FailNext(u32),
}

impl AcceptFaults {
    fn next_error(&mut self) -> Option<io::Error> {
        #[cfg(all(test, unix))]
        if let Self::FailNext(remaining) = self
            && *remaining > 0
        {
            *remaining -= 1;
            return Some(io::Error::other("injected accept failure"));
        }
        None
    }
}

fn spawn_accept_owner(
    listener: Listener,
    token: Arc<str>,
    emit: EventSink,
    hub: Arc<RemoteSessionHub>,
    endpoint_lease: Arc<EndpointLease>,
) -> tokio::task::JoinHandle<()> {
    spawn_accept_owner_with_faults(
        listener,
        token,
        emit,
        hub,
        endpoint_lease,
        AcceptFaults::None,
    )
}

fn spawn_accept_owner_with_faults(
    listener: Listener,
    token: Arc<str>,
    emit: EventSink,
    hub: Arc<RemoteSessionHub>,
    endpoint_lease: Arc<EndpointLease>,
    faults: AcceptFaults,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if serve_loop(&listener, token, emit, hub, faults).await == AcceptLoopExit::RepeatedErrors {
            endpoint_lease.mark_accept_owner_failed();
        }
    })
}

/// Accept connections until the session hub latches shutdown, owning every connection task.
#[cfg(test)]
pub(crate) async fn serve(
    listener: Listener,
    token: Arc<str>,
    emit: EventSink,
    hub: Arc<RemoteSessionHub>,
) {
    let _ = serve_loop(&listener, token, emit, hub, AcceptFaults::None).await;
}

async fn serve_loop(
    listener: &Listener,
    token: Arc<str>,
    emit: EventSink,
    hub: Arc<RemoteSessionHub>,
    mut faults: AcceptFaults,
) -> AcceptLoopExit {
    // A healthy listener only errors transiently. If `accept` fails repeatedly the listener is
    // wedged and a bare `continue` becomes a hot spin (one CPU pegged for nothing) — bail after
    // a short run of consecutive failures. Remote control degrades; the player keeps running.
    let mut errors: u32 = 0;
    // Bounds concurrent connections still in the pre-handshake window (see PREHANDSHAKE_MAX).
    let gate = Arc::new(tokio::sync::Semaphore::new(PREHANDSHAKE_MAX));
    let mut connections = JoinSet::new();
    let exit = loop {
        reap_connection_tasks(&mut connections);
        let accepted = if let Some(error) = faults.next_error() {
            Err(error)
        } else {
            tokio::select! {
                biased;
                _ = hub.wait_for_shutdown() => break AcceptLoopExit::Shutdown,
                _ = hub.wait_for_owner_quiesce() => break AcceptLoopExit::Quiesced,
                accepted = listener.accept() => accepted,
            }
        };
        let conn = match accepted {
            Ok(c) => {
                errors = 0;
                c
            }
            Err(e) => {
                errors += 1;
                tracing::warn!(error = %e, errors, "remote: accept failed");
                if errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                    tracing::warn!("remote: too many accept failures; stopping the control server");
                    break AcceptLoopExit::RepeatedErrors;
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
        let connection_hub = Arc::clone(&hub);
        // Linearize task creation against the hub latch. A connection is either owned by this
        // JoinSet before shutdown begins, or it is dropped without ever polling `handle_conn`.
        if hub
            .run_if_owner_admission_open(|| {
                connections.spawn(async move {
                    if let Err(e) = handle_conn(conn, &token, emit, connection_hub, permit).await {
                        tracing::debug!(error = %e, "remote: connection ended with error");
                    }
                });
            })
            .is_none()
        {
            break AcceptLoopExit::Quiesced;
        }
    };
    // Quiesce stops new connections but deliberately preserves the accepted connection tree until
    // the owner confirms every pre-frontier reply reached its writer. Listener failure still
    // promotes directly to full shutdown because no healthy accept owner remains.
    match exit {
        AcceptLoopExit::Quiesced => hub.wait_for_shutdown().await,
        AcceptLoopExit::RepeatedErrors => hub.shutdown_all(),
        AcceptLoopExit::Shutdown => {}
    }
    abort_and_join_connection_tasks(&mut connections).await;
    exit
}

fn reap_connection_tasks(connections: &mut JoinSet<()>) {
    while let Some(result) = connections.try_join_next() {
        log_connection_task_result(result);
    }
}

async fn abort_and_join_connection_tasks(connections: &mut JoinSet<()>) {
    connections.abort_all();
    while let Some(result) = connections.join_next().await {
        log_connection_task_result(result);
    }
}

fn log_connection_task_result(result: Result<(), JoinError>) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        tracing::warn!(%error, "remote: connection task failed");
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
        let tracked = if line.len() > MAX_REQUEST_BYTES {
            TrackedRemoteResponse::untracked(RemoteResponse::err("bad_request"))
        } else {
            build_tracked_response(req, token, &emit, &hub).await
        };
        write_tracked_response(&conn, tracked, &hub).await?;
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
mod one_shot_tests;

/// Session-mode socket tests. Deliberately NOT unix-gated: on the Windows CI leg these
/// run over a real named pipe, which makes them the standing long-lived-duplex smoke the
/// v8 design calls out as its one genuinely new transport risk (docs/gui/02 §19.1).
#[cfg(test)]
mod session_socket_tests;

#[cfg(test)]
#[path = "server/lifecycle_tests.rs"]
mod lifecycle_tests;
#[cfg(test)]
mod shutdown_settlement_tests;
#[cfg(all(test, unix))]
#[path = "server/startup_cleanup_tests.rs"]
mod startup_cleanup_tests;
#[cfg(test)]
mod subscribe_pressure_tests;
