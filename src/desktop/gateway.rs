//! The persistent protocol-v8 session driver (docs/gui/03 §3.2).
//!
//! One long-lived thread runs a current-thread tokio runtime holding the socket: Hello →
//! subscribe → read frames, with a periodic ping keep-alive and exponential-backoff
//! reconnect. Connection-state transitions are pushed to the event loop via the `emit`
//! callback (the platform code wraps them in its `UserEvent`). Daemon spawn/stop never run
//! here — that would freeze the socket ([01 §9]); the gateway only observes and reports.
//!
//! M1 scope: connect + Hello + live connection state + reconnect, plus the command path —
//! the loop forwards webview `cmd`/`req`/`sub`/`unsub` envelopes into the session
//! ([`GatewayHandle::send`]) and the session fans `event`/`reply` server frames back to the
//! window as [`GatewayEvent::Frame`] (rendered via `bridge::receive_script`).

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::desktop::bridge::{InEnvelope, OutEnvelope, OutKind};
use crate::remote::endpoint;
use crate::remote::proto::{
    ClientFrame, ClientOp, HelloAck, HelloBody, HelloRequest, InstanceFile, InstanceMode,
    PROTOCOL_VERSION, PushEvent, RemoteCommand, RemoteResponse, ServerFrame, Topic,
};

pub use super::gateway_frontend::{MAIN_FRONTEND_TOPICS, refresh_ready_main_frontend};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const PING_INTERVAL: Duration = Duration::from_secs(15);
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Live connection state, mirrored into the frontend's `connection` store as `conn` frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Online {
        protocol_version: u8,
        capabilities: Vec<String>,
        owner_mode: InstanceMode,
    },
    /// `reason` is a machine string (`"no_core"`, `"no_v8"`, `"bad_token"`, `"disconnected"`,
    /// `"shutting_down"`, …); clients key off it, never a display string.
    Offline {
        reason: String,
    },
}

impl ConnState {
    /// The `conn` envelope payload (camelCase, matching the TS `ConnPayload`) that drives the
    /// frontend's connection store (docs/gui/05 §4.1).
    pub fn to_conn_payload(&self) -> serde_json::Value {
        match self {
            ConnState::Connecting => serde_json::json!({ "state": "connecting" }),
            ConnState::Online {
                protocol_version,
                capabilities,
                owner_mode,
            } => serde_json::json!({
                "state": "online",
                "protocolVersion": protocol_version,
                "capabilities": capabilities,
                "ownerMode": owner_mode,
            }),
            ConnState::Offline { reason } => serde_json::json!({
                "state": "offline",
                "reason": reason,
            }),
        }
    }
}

/// What the gateway thread reports to the event loop.
#[derive(Debug, Clone)]
pub enum GatewayEvent {
    Connection(ConnState),
    /// An inbound server frame (topic push or correlated reply) already rendered as the
    /// webview envelope the loop feeds to `bridge::receive_script` (M1 fan-out).
    Frame {
        envelope: InEnvelope,
        /// WebView generation that originated a correlated request. Native shell traffic and
        /// server-originated frames use `None`. Adapters must not deliver a page-scoped reply
        /// to a rebuilt page with a different generation.
        source_generation: Option<u64>,
    },
    /// Typed push retained for the native menu/mini projection. Keeping `seq` here lets the
    /// common DesktopApp reject stale generations before an old frame can overwrite new state.
    Push {
        sequence: u64,
        topic: Topic,
        event: PushEvent,
    },
}

/// Handle to the gateway thread; dropping it (or calling [`GatewayHandle::stop`]) tears the
/// session down.
pub struct GatewayHandle {
    shutdown: Option<oneshot::Sender<()>>,
    commands: mpsc::Sender<GatewayCommand>,
    state: Arc<AtomicU8>,
    next_native_id: AtomicU64,
    done: Option<std::sync::mpsc::Receiver<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug)]
struct GatewayCommand {
    envelope: OutEnvelope,
    source_generation: Option<u64>,
}

const GATEWAY_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

const GATEWAY_OFFLINE: u8 = 0;
const GATEWAY_ONLINE: u8 = 1;
const GATEWAY_CLOSED: u8 = 2;
/// Main-window request IDs count up from one. Native shell requests occupy the upper half so
/// their replies can be consumed by the event loop instead of being forwarded into the page.
pub const NATIVE_REQUEST_ID_BASE: u64 = crate::desktop::bridge::MAX_PAGE_REQUEST_ID + 1;

pub fn is_native_request_id(id: Option<u64>) -> bool {
    id.is_some_and(|id| id >= NATIVE_REQUEST_ID_BASE)
}

#[derive(Debug)]
pub enum GatewaySendError {
    Offline(OutEnvelope),
    Backpressure(OutEnvelope),
    Closed(OutEnvelope),
}

#[derive(Debug)]
pub enum GatewayCommandError {
    Encode,
    Send(GatewaySendError),
}

impl GatewayCommandError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Encode => "encode_failed",
            Self::Send(error) => error.code(),
        }
    }
}

impl GatewaySendError {
    pub fn into_envelope(self) -> OutEnvelope {
        match self {
            Self::Offline(env) | Self::Backpressure(env) | Self::Closed(env) => env,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Offline(_) => "offline",
            Self::Backpressure(_) => "backpressure",
            Self::Closed(_) => "closed",
        }
    }
}

impl GatewayHandle {
    pub fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.join_bounded();
    }

    /// Forward a webview envelope (`cmd`/`req`/`sub`/`unsub`) to the live session. Queued
    /// commands are dropped when the session next (re)connects, so a send while offline is a
    /// harmless no-op rather than a stale replay.
    pub fn send(&self, env: OutEnvelope) -> Result<(), GatewaySendError> {
        self.send_from_generation(env, None)
    }

    /// Forward a page request while retaining the WebView generation that owns its request
    /// identifier. This prevents a late reply from an old page resolving a reused ID after a
    /// grace-period teardown and rebuild.
    pub fn send_from_generation(
        &self,
        env: OutEnvelope,
        source_generation: Option<u64>,
    ) -> Result<(), GatewaySendError> {
        let declaration = matches!(env.kind, OutKind::Sub | OutKind::Unsub);
        match self.state.load(Ordering::Acquire) {
            GATEWAY_ONLINE => {}
            GATEWAY_CLOSED => return Err(GatewaySendError::Closed(env)),
            _ if declaration => {}
            _ => return Err(GatewaySendError::Offline(env)),
        }
        self.commands
            .try_send(GatewayCommand {
                envelope: env,
                source_generation,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(command) => {
                    GatewaySendError::Backpressure(command.envelope)
                }
                mpsc::error::TrySendError::Closed(command) => {
                    GatewaySendError::Closed(command.envelope)
                }
            })
    }

    /// Force authoritative snapshots for a topic set without leaving its
    /// unsubscribe/subscription pair vulnerable to queue backpressure between two WebView
    /// messages. The session writes the pair in order; a disconnect between them reconnects with
    /// `desired` intact.
    pub fn refresh_topics(&self, topics: &[Topic]) -> Result<(), GatewaySendError> {
        if topics.is_empty() {
            return Ok(());
        }
        self.send(OutEnvelope {
            v: 1,
            id: None,
            kind: OutKind::Sub,
            name: "refresh".to_string(),
            payload: serde_json::json!(topics),
        })
    }

    pub fn refresh_topic(&self, topic: Topic) -> Result<(), GatewaySendError> {
        self.refresh_topics(&[topic])
    }

    pub fn is_online(&self) -> bool {
        self.state.load(Ordering::Acquire) == GATEWAY_ONLINE
    }

    /// Route a native tray/panel command over the same ordered v8 session as the main window.
    pub fn send_remote(&self, command: RemoteCommand) -> Result<u64, GatewayCommandError> {
        let value = serde_json::to_value(command).map_err(|_| GatewayCommandError::Encode)?;
        let serde_json::Value::Object(mut payload) = value else {
            return Err(GatewayCommandError::Encode);
        };
        let Some(name) = payload
            .remove("cmd")
            .and_then(|value| value.as_str().map(str::to_owned))
        else {
            return Err(GatewayCommandError::Encode);
        };
        let id = self.next_native_id.fetch_add(1, Ordering::Relaxed);
        self.send_from_generation(
            OutEnvelope {
                v: crate::desktop::bridge::BRIDGE_VERSION,
                id: Some(id),
                kind: OutKind::Req,
                name,
                payload: serde_json::Value::Object(payload),
            },
            None,
        )
        .map_err(GatewayCommandError::Send)?;
        Ok(id)
    }

    fn join_bounded(&mut self) {
        let stopped = self
            .done
            .take()
            .is_none_or(|done| done.recv_timeout(GATEWAY_SHUTDOWN_TIMEOUT).is_ok());
        if stopped {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        } else {
            tracing::warn!(target: "ytt_desktop", "gateway thread did not stop before deadline");
            self.thread.take();
        }
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        self.state.store(GATEWAY_CLOSED, Ordering::Release);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.join_bounded();
    }
}

/// Spawn the gateway thread. `emit` runs on the gateway thread; the platform wrapper must
/// forward to the loop via `EventLoopProxy::send_event` (it is `!Send`-safe to just clone a
/// proxy into the closure).
pub fn spawn<F>(emit: F) -> GatewayHandle
where
    F: Fn(GatewayEvent) + Send + 'static,
{
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let state = Arc::new(AtomicU8::new(GATEWAY_OFFLINE));
    // Bounded with a generous cap; a webview that floods commands while the session is stalled
    // drops new envelopes (`try_send`) rather than growing the queue without bound.
    let (cmd_tx, cmd_rx) = mpsc::channel(512);
    let builder = std::thread::Builder::new().name("yututray-gateway".to_string());
    let thread_state = Arc::clone(&state);
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let thread = match builder.spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => runtime.block_on(run(emit, shutdown_rx, cmd_rx, thread_state)),
            Err(_) => {
                thread_state.store(GATEWAY_CLOSED, Ordering::Release);
                emit(GatewayEvent::Connection(ConnState::Offline {
                    reason: "no_runtime".into(),
                }));
            }
        }
        let _ = done_tx.send(());
    }) {
        Ok(thread) => Some(thread),
        Err(e) => {
            state.store(GATEWAY_CLOSED, Ordering::Release);
            tracing::warn!(target: "ytt_desktop", error = %e, "could not start gateway thread");
            None
        }
    };
    GatewayHandle {
        shutdown: Some(shutdown_tx),
        commands: cmd_tx,
        state,
        next_native_id: AtomicU64::new(NATIVE_REQUEST_ID_BASE),
        done: Some(done_rx),
        thread,
    }
}

async fn run<F: Fn(GatewayEvent)>(
    emit: F,
    mut shutdown_rx: oneshot::Receiver<()>,
    mut cmd_rx: mpsc::Receiver<GatewayCommand>,
    state: Arc<AtomicU8>,
) {
    let mut backoff = BACKOFF_MIN;
    let mut legacy_fallback = false;
    let mut last_connection = Some(ConnState::Connecting);
    emit(GatewayEvent::Connection(ConnState::Connecting));
    // The union of topics any window asked for, kept ACROSS sessions. Subscriptions are
    // declarations of interest, not one-shot actions: a window that booted while the core
    // was down (or that outlives a core restart) sent its one `sub` into a dead session,
    // so every fresh session must replay the set or that window stays empty forever.
    let mut desired: Vec<Topic> = Vec::new();
    loop {
        state.store(GATEWAY_OFFLINE, Ordering::Release);
        // Keep `no_v8` stable while probing for an upgraded core. Publishing Connecting on
        // every backoff tick tears down and recreates the exclusive v7 fallback poller.
        if !legacy_fallback && last_connection.as_ref() != Some(&ConnState::Connecting) {
            emit(GatewayEvent::Connection(ConnState::Connecting));
            last_connection = Some(ConnState::Connecting);
        }
        match open_session().await {
            Ok((conn, ack)) => {
                legacy_fallback = false;
                backoff = BACKOFF_MIN; // healthy connection resets the backoff
                let online = ConnState::Online {
                    protocol_version: ack.version,
                    capabilities: ack.capabilities,
                    owner_mode: ack.owner_mode,
                };
                let reason = run_session(
                    conn,
                    &mut shutdown_rx,
                    &mut cmd_rx,
                    &mut desired,
                    &state,
                    online,
                    &emit,
                )
                .await;
                // Reject new commands before observers see Offline and throughout backoff.
                state.store(GATEWAY_OFFLINE, Ordering::Release);
                tracing::info!(target: "ytt_desktop", %reason, "gateway session ended");
                let offline = ConnState::Offline {
                    reason: reason.clone(),
                };
                emit(GatewayEvent::Connection(offline.clone()));
                last_connection = Some(offline);
                if reason == "shutdown" {
                    state.store(GATEWAY_CLOSED, Ordering::Release);
                    return;
                }
            }
            Err(reason) => {
                legacy_fallback = reason == "no_v8";
                let offline = ConnState::Offline { reason };
                if last_connection.as_ref() != Some(&offline) {
                    emit(GatewayEvent::Connection(offline.clone()));
                    last_connection = Some(offline);
                }
            }
        }

        // Wait out the backoff, but wake immediately on shutdown.
        tokio::select! {
            _ = &mut shutdown_rx => {
                state.store(GATEWAY_CLOSED, Ordering::Release);
                return;
            },
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Read the instance descriptor and complete the Hello handshake.
async fn open_session() -> Result<(Stream, HelloAck), String> {
    let Some(instance) = endpoint::read_instance() else {
        return Err("no_core".to_string());
    };
    if instance.protocol_version < PROTOCOL_VERSION {
        // The owner predates v8 sessions; the tray v7 poll path (status.rs) covers this.
        return Err("no_v8".to_string());
    }
    connect_and_hello(instance).await
}

/// Connect to `instance` and perform the Hello exchange. Factored out for testing against an
/// in-process stub server.
async fn connect_and_hello(instance: InstanceFile) -> Result<(Stream, HelloAck), String> {
    let Ok(name) = instance.endpoint.as_str().to_fs_name::<GenericFilePath>() else {
        return Err("bad_endpoint".to_string());
    };
    let conn = match timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
        Ok(Ok(c)) => c,
        _ => return Err("connect_failed".to_string()),
    };

    let hello = HelloRequest {
        version: PROTOCOL_VERSION,
        token: instance.token,
        hello: HelloBody {
            client: "yututray".to_string(),
            min_version: PROTOCOL_VERSION,
        },
    };
    if write_line(&conn, &hello).await.is_err() {
        return Err("write_failed".to_string());
    }

    let mut reader = BufReader::new(&conn);
    let ack: HelloAck = match timeout(HELLO_TIMEOUT, read_line(&mut reader)).await {
        Ok(Ok(Some(line))) => match serde_json::from_str(line.trim()) {
            Ok(ack) => ack,
            Err(_) => return Err("bad_ack".to_string()),
        },
        _ => return Err("no_ack".to_string()),
    };
    if !ack.ok {
        return Err(ack.reason.unwrap_or_else(|| "rejected".to_string()));
    }
    if ack.version != PROTOCOL_VERSION {
        return Err("bad_ack_version".to_string());
    }
    drop(reader); // release the borrow so the caller owns `conn` outright
    Ok((conn, ack))
}

fn accept_next_sequence(last_sequence: &mut u64, received: u64) -> bool {
    let Some(expected) = last_sequence.checked_add(1) else {
        return false;
    };
    if received != expected {
        return false;
    }
    *last_sequence = received;
    true
}

/// Drive an established session until it closes; returns the machine reason.
async fn run_session<F: Fn(GatewayEvent)>(
    conn: Stream,
    shutdown_rx: &mut oneshot::Receiver<()>,
    cmd_rx: &mut mpsc::Receiver<GatewayCommand>,
    desired: &mut Vec<Topic>,
    state: &Arc<AtomicU8>,
    online: ConnState,
    emit: &F,
) -> String {
    let mut reader = BufReader::new(&conn);
    let mut next_id = 1u64;
    let mut last_sequence = 0u64;
    // Session id of a forwarded `req` → the frontend's own request id, so its `reply` can be
    // correlated back. Only `req` frames insert; every reply removes; the map dies with the
    // session, so it cannot leak across reconnects.
    let mut pending: HashMap<u64, (u64, Option<u64>)> = HashMap::new();

    // Commands queued while we were offline are stale on this fresh session — discard them,
    // EXCEPT sub/unsub, which fold into `desired` so the (re)subscribe below replays them.
    drain_offline_envelopes(cmd_rx, desired, emit);

    // Subscribe to `system` (so we notice owner shutdown even with no window open, and to
    // keep the session non-idle) plus every topic a window already declared. New sessions
    // send fresh snapshots for all of these, which is exactly the rehydrate the frontend
    // expects after a reconnect.
    let sub = ClientFrame {
        id: next_id,
        op: ClientOp::Subscribe {
            topics: initial_topics(desired),
        },
    };
    next_id += 1;
    if write_line(&conn, &sub).await.is_err() {
        return "disconnected".to_string();
    }

    // Publish ONLINE only after stale offline work has been drained and the baseline
    // subscription is on the wire. Commands accepted after this point stay in this session.
    state.store(GATEWAY_ONLINE, Ordering::Release);
    emit(GatewayEvent::Connection(online));

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // consume the immediate first tick
    let mut awaiting_pong = false;
    // `tokio::select!` cancels losing branch futures. Keep the frame allocation outside the
    // read future so a command/ping that wins after a fragmented prefix was consumed cannot
    // discard those bytes and desynchronize the session stream.
    let mut frame_buf = Vec::new();

    loop {
        tokio::select! {
            _ = &mut *shutdown_rx => return "shutdown".to_string(),
            _ = ping.tick() => {
                if awaiting_pong {
                    return "ping_timeout".to_string();
                }
                let frame = ClientFrame { id: next_id, op: ClientOp::Ping };
                next_id += 1;
                if write_line(&conn, &frame).await.is_err() {
                    return "disconnected".to_string();
                }
                awaiting_pong = true;
            }
            maybe = cmd_rx.recv() => {
                if let Some(env) = maybe {
                    track_subscriptions(&env.envelope, desired);
                    if let Some(reason) =
                        forward_command(&conn, env, &mut next_id, &mut pending, emit).await
                    {
                        return reason;
                    }
                }
                // `None` = the handle was dropped; the shutdown branch will fire too.
            }
            line = read_line_buffered(&mut reader, &mut frame_buf) => match line {
                Ok(Some(l)) => match serde_json::from_str::<ServerFrame>(l.trim()) {
                    Ok(ServerFrame::Pong { .. }) => awaiting_pong = false,
                    Ok(ServerFrame::Goodbye { reason }) => return reason,
                    Ok(ServerFrame::Reply { id, resp }) => {
                        if let Some((fid, source_generation)) = pending.remove(&id) {
                            emit(GatewayEvent::Frame {
                                envelope: reply_envelope(fid, resp),
                                source_generation,
                            });
                        }
                        // Unmapped ids are the gateway's own subscribe/ping replies — ignore.
                    }
                    Ok(ServerFrame::Event { seq, topic, event }) => {
                        if !accept_next_sequence(&mut last_sequence, seq) {
                            // Snapshot + subsequent events is only trustworthy when the
                            // per-session stream is contiguous. Reconnect to obtain a fresh
                            // baseline rather than projecting a silently incomplete state.
                            return "sequence_gap".to_string();
                        }
                        let shutting_down = matches!(event, PushEvent::ShuttingDown);
                        emit(GatewayEvent::Push {
                            sequence: seq,
                            topic,
                            event,
                        });
                        if shutting_down {
                            return "shutting_down".to_string();
                        }
                    }
                    // Framing is no longer trustworthy after a malformed server line. Reconnect
                    // instead of silently dropping it and projecting an incomplete snapshot.
                    Err(_) => return "bad_frame".to_string(),
                },
                Ok(None) | Err(_) => return "disconnected".to_string(),
            }
        }
    }
}

/// Translate a webview envelope into a [`ClientFrame`] and write it to the session. Returns
/// `Some(reason)` only on a socket write failure (which ends the session); a command that
/// can't be translated is rejected (for `req`) or dropped (for `cmd`) without tearing down.
async fn forward_command<F: Fn(GatewayEvent)>(
    conn: &Stream,
    command: GatewayCommand,
    next_id: &mut u64,
    pending: &mut HashMap<u64, (u64, Option<u64>)>,
    emit: &F,
) -> Option<String> {
    let GatewayCommand {
        envelope: env,
        source_generation,
    } = command;
    if env.kind == OutKind::Sub && env.name == "refresh" {
        let topics = parse_topics(&env.payload)?;
        for op in [
            ClientOp::Unsubscribe {
                topics: topics.clone(),
            },
            ClientOp::Subscribe { topics },
        ] {
            let sid = *next_id;
            *next_id += 1;
            if write_line(conn, &ClientFrame { id: sid, op })
                .await
                .is_err()
            {
                return Some("disconnected".to_string());
            }
        }
        return None;
    }
    let op = match env.kind {
        OutKind::Cmd | OutKind::Req => match to_remote_command(&env.name, &env.payload) {
            Some(cmd) => ClientOp::Command(cmd),
            None => {
                // Unsupported command name/shape. A correlated req must not hang for its 10 s
                // timeout, so reject it; a fire-and-forget cmd is just logged and dropped.
                if let (OutKind::Req, Some(fid)) = (env.kind, env.id) {
                    emit(GatewayEvent::Frame {
                        envelope: InEnvelope::err(
                            fid,
                            serde_json::json!({ "reason": "bad_command" }),
                        ),
                        source_generation,
                    });
                } else {
                    tracing::debug!(
                        target: "ytt_desktop",
                        name = %env.name,
                        "dropping unsupported gateway command"
                    );
                }
                return None;
            }
        },
        OutKind::Sub => match parse_topics(&env.payload) {
            Some(topics) => ClientOp::Subscribe { topics },
            None => return None,
        },
        OutKind::Unsub => match parse_topics(&env.payload) {
            Some(topics) => ClientOp::Unsubscribe { topics },
            None => return None,
        },
        // `win` never reaches the gateway (bridge routes it natively); ignore defensively.
        OutKind::Win => return None,
    };

    let sid = *next_id;
    *next_id += 1;
    if let (OutKind::Req, Some(fid)) = (env.kind, env.id) {
        pending.insert(sid, (fid, source_generation));
    }
    let frame = ClientFrame { id: sid, op };
    if write_line(conn, &frame).await.is_err() {
        return Some("disconnected".to_string());
    }
    None
}

/// Build a `{"cmd": name, ...payload}` object and parse it as a [`RemoteCommand`]. The command
/// enum is `#[serde(tag = "cmd")]` snake_case, so the frontend's `name` is the tag and its
/// `payload` supplies the fields. Returns `None` for names/shapes the core doesn't model.
fn to_remote_command(name: &str, payload: &serde_json::Value) -> Option<RemoteCommand> {
    let mut obj = match payload {
        serde_json::Value::Object(map) => map.clone(),
        serde_json::Value::Null => serde_json::Map::new(),
        _ => return None,
    };
    obj.insert(
        "cmd".to_string(),
        serde_json::Value::String(name.to_string()),
    );
    let command: RemoteCommand = serde_json::from_value(serde_json::Value::Object(obj)).ok()?;
    command.validate().ok()?;
    Some(command)
}

/// A `sub`/`unsub` payload is a JSON array of wire topic strings. Empty is treated as nothing
/// to do (the client already guards this, but be defensive).
fn parse_topics(payload: &serde_json::Value) -> Option<Vec<Topic>> {
    let topics: Vec<Topic> = serde_json::from_value(payload.clone()).ok()?;
    (!topics.is_empty()).then_some(topics)
}

/// Fold a `sub`/`unsub` envelope into the reconnect-surviving desired-topic set; every other
/// kind passes through untouched. Order-preserving and deduplicated (the set stays small —
/// at most the 13 wire topics).
fn track_subscriptions(env: &OutEnvelope, desired: &mut Vec<Topic>) {
    match env.kind {
        OutKind::Sub => {
            if let Some(topics) = parse_topics(&env.payload) {
                for topic in topics {
                    if !desired.contains(&topic) {
                        desired.push(topic);
                    }
                }
            }
        }
        OutKind::Unsub => {
            if let Some(topics) = parse_topics(&env.payload) {
                desired.retain(|topic| !topics.contains(topic));
            }
        }
        _ => {}
    }
}

fn drain_offline_envelopes<F: Fn(GatewayEvent)>(
    cmd_rx: &mut mpsc::Receiver<GatewayCommand>,
    desired: &mut Vec<Topic>,
    emit: &F,
) {
    while let Ok(command) = cmd_rx.try_recv() {
        let GatewayCommand {
            envelope: env,
            source_generation,
        } = command;
        track_subscriptions(&env, desired);
        if let (OutKind::Req, Some(id)) = (env.kind, env.id) {
            emit(GatewayEvent::Frame {
                envelope: InEnvelope::err(id, serde_json::json!({ "reason": "offline" })),
                source_generation,
            });
        }
    }
}

/// The session-opening subscribe: the compact desktop projection is always live, even when the
/// Svelte main window has never opened. Window-declared topics follow and are de-duplicated.
fn initial_topics(desired: &[Topic]) -> Vec<Topic> {
    let mut topics = vec![Topic::System, Topic::Player, Topic::Queue, Topic::Settings];
    for topic in desired {
        if !topics.contains(topic) {
            topics.push(*topic);
        }
    }
    topics
}

/// Render a correlated [`RemoteResponse`] as the `res`/`err` envelope the frontend awaits.
fn reply_envelope(fid: u64, resp: RemoteResponse) -> InEnvelope {
    if resp.ok {
        let payload = serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null);
        InEnvelope::res(fid, payload)
    } else {
        let reason = resp.reason.unwrap_or_else(|| "error".to_string());
        InEnvelope::err(fid, serde_json::json!({ "reason": reason }))
    }
}

async fn write_line<T: serde::Serialize>(conn: &Stream, value: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    buf.push(b'\n');
    let mut w = conn;
    w.write_all(&buf).await?;
    w.flush().await
}

/// Session-frame cap for the v8 gateway protocol, matching the remote server's
/// `SESSION_MAX_FRAME_BYTES`. A peer that never sends a newline (or sends a giant frame) is
/// torn down instead of growing the buffer until the desktop process OOMs.
const GATEWAY_MAX_FRAME_BYTES: usize = 256 * 1024;

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    read_line_buffered(reader, &mut buf).await
}

/// Cancellation-safe session line reader. `buf` deliberately belongs to the session loop and is
/// preserved when the read future loses a `select!`; only a completed frame or terminal error
/// clears it.
async fn read_line_buffered<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<Option<String>> {
    use crate::util::io::{BoundedLine, read_bounded_line};
    match read_bounded_line(reader, buf, GATEWAY_MAX_FRAME_BYTES).await? {
        BoundedLine::Eof if buf.is_empty() => Ok(None),
        BoundedLine::Line | BoundedLine::Eof => {
            let line = String::from_utf8_lossy(buf).into_owned();
            buf.clear();
            Ok(Some(line))
        }
        BoundedLine::TooLarge => {
            buf.clear();
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gateway frame exceeded cap",
            ))
        }
    }
}

// Envelope→frame translation is platform-agnostic, so these run on every target (the socket
// tests below need a unix domain path and are gated separately).
#[cfg(test)]
mod translate_tests {
    use super::*;
    use crate::desktop::bridge::InKind;
    use crate::remote::proto::{RemoteCommand, ToggleState};
    use std::sync::Mutex;

    #[tokio::test]
    async fn buffered_reader_keeps_a_fragment_across_select_cancellation() {
        use tokio::io::{AsyncWriteExt, BufReader, duplex};

        let (reader_side, mut writer) = duplex(256);
        let mut reader = BufReader::new(reader_side);
        let mut frame = Vec::new();
        writer.write_all(b"{\"id\":7,\"resp\"").await.unwrap();

        tokio::select! {
            result = read_line_buffered(&mut reader, &mut frame) => {
                panic!("fragment unexpectedly completed: {result:?}");
            }
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        assert_eq!(frame, b"{\"id\":7,\"resp\"");

        writer.write_all(b":null}\n").await.unwrap();
        let line = read_line_buffered(&mut reader, &mut frame)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line, "{\"id\":7,\"resp\":null}\n");
        assert!(frame.is_empty());
    }

    #[test]
    fn cmd_names_and_payloads_map_to_remote_commands() {
        assert_eq!(
            to_remote_command("toggle_pause", &serde_json::Value::Null),
            Some(RemoteCommand::TogglePause)
        );
        assert_eq!(
            to_remote_command("next", &serde_json::json!({})),
            Some(RemoteCommand::Next)
        );
        assert_eq!(
            to_remote_command("seek_to", &serde_json::json!({ "ms": 1000 })),
            Some(RemoteCommand::SeekTo { ms: 1000 })
        );
        assert_eq!(
            to_remote_command("set_volume", &serde_json::json!({ "percent": 42 })),
            Some(RemoteCommand::SetVolume { percent: 42 })
        );
        assert_eq!(
            to_remote_command("queue_play", &serde_json::json!({ "position": 3 })),
            Some(RemoteCommand::QueuePlay { position: 3 })
        );
        assert_eq!(
            to_remote_command(
                "queue_play_if_revision",
                &serde_json::json!({ "position": 3, "expected_rev": 41 })
            ),
            Some(RemoteCommand::QueuePlayIfRevision {
                position: 3,
                expected_rev: 41,
            })
        );
        assert_eq!(
            to_remote_command(
                "queue_remove_if_revision",
                &serde_json::json!({ "position": 2, "expected_rev": 41 })
            ),
            Some(RemoteCommand::QueueRemoveIfRevision {
                position: 2,
                expected_rev: 41,
            })
        );
        assert_eq!(
            to_remote_command("streaming", &serde_json::json!({ "state": "on" })),
            Some(RemoteCommand::Streaming {
                state: ToggleState::On
            })
        );
        // The GUI-session verbs the frontend already sends (search + row playback).
        assert_eq!(
            to_remote_command(
                "run_search",
                &serde_json::json!({ "ticket": 3, "query": "queen", "source": "all" })
            ),
            Some(RemoteCommand::RunSearch {
                ticket: 3,
                query: "queen".to_string(),
                source: crate::search_source::SearchSource::All,
            })
        );
        assert_eq!(
            to_remote_command(
                "play_tracks",
                &serde_json::json!({ "video_ids": ["a", "b"] })
            ),
            Some(RemoteCommand::PlayTracks {
                video_ids: vec!["a".to_string(), "b".to_string()],
            })
        );
        assert_eq!(
            to_remote_command("enqueue_tracks", &serde_json::json!({ "video_ids": ["c"] })),
            Some(RemoteCommand::EnqueueTracks {
                video_ids: vec!["c".to_string()],
            })
        );
    }

    #[test]
    fn unsupported_or_malformed_commands_are_none() {
        // A v8 command the core doesn't model yet (queue/rating extensions).
        assert_eq!(
            to_remote_command(
                "rate",
                &serde_json::json!({ "video_id": "x", "rating": "cycle" })
            ),
            None
        );
        assert_eq!(
            to_remote_command("not_a_command", &serde_json::Value::Null),
            None
        );
        // Right name, wrong field type.
        assert_eq!(
            to_remote_command("seek_to", &serde_json::json!({ "ms": "soon" })),
            None
        );
        // Non-object, non-null payloads can't carry command fields.
        assert_eq!(to_remote_command("next", &serde_json::json!(7)), None);
        // Syntactically correct but out-of-contract desktop control ranges fail at the host,
        // before they can reach an older core that might clamp them silently.
        assert_eq!(
            to_remote_command("set_volume", &serde_json::json!({ "percent": 101 })),
            None
        );
        assert_eq!(
            to_remote_command(
                "set_setting",
                &serde_json::json!({
                    "change": { "setting": "speed", "tenths": 21 }
                })
            ),
            None
        );
    }

    #[test]
    fn topics_parse_from_the_wire_array() {
        assert_eq!(
            parse_topics(&serde_json::json!(["player", "queue", "system"])),
            Some(vec![Topic::Player, Topic::Queue, Topic::System])
        );
        assert_eq!(parse_topics(&serde_json::json!([])), None);
        assert_eq!(parse_topics(&serde_json::json!("player")), None);
        assert_eq!(parse_topics(&serde_json::json!(["bogus"])), None);
    }

    fn sub_env(kind: OutKind, topics: serde_json::Value) -> OutEnvelope {
        OutEnvelope {
            v: 1,
            id: None,
            kind,
            name: String::new(),
            payload: topics,
        }
    }

    fn queued(envelope: OutEnvelope) -> GatewayCommand {
        GatewayCommand {
            envelope,
            source_generation: None,
        }
    }

    #[test]
    fn subscriptions_fold_into_the_desired_set_across_kinds() {
        let mut desired = Vec::new();
        track_subscriptions(
            &sub_env(OutKind::Sub, serde_json::json!(["player", "queue"])),
            &mut desired,
        );
        // Overlapping re-sub dedups instead of duplicating.
        track_subscriptions(
            &sub_env(OutKind::Sub, serde_json::json!(["queue", "settings"])),
            &mut desired,
        );
        track_subscriptions(
            &sub_env(OutKind::Unsub, serde_json::json!(["queue"])),
            &mut desired,
        );
        assert_eq!(desired, vec![Topic::Player, Topic::Settings]);

        // Non-subscription envelopes never touch the set.
        track_subscriptions(
            &OutEnvelope {
                v: 1,
                id: Some(1),
                kind: OutKind::Cmd,
                name: "next".to_string(),
                payload: serde_json::Value::Null,
            },
            &mut desired,
        );
        assert_eq!(desired, vec![Topic::Player, Topic::Settings]);
    }

    #[test]
    fn offline_reconnect_drain_drops_commands_but_keeps_subscriptions() {
        let (tx, mut rx) = mpsc::channel(8);
        tx.try_send(queued(OutEnvelope {
            v: 1,
            id: None,
            kind: OutKind::Cmd,
            name: "next".to_string(),
            payload: serde_json::Value::Null,
        }))
        .unwrap();
        tx.try_send(queued(sub_env(
            OutKind::Sub,
            serde_json::json!(["player", "queue"]),
        )))
        .unwrap();
        tx.try_send(queued(sub_env(
            OutKind::Unsub,
            serde_json::json!(["queue"]),
        )))
        .unwrap();

        let mut desired = Vec::new();
        drain_offline_envelopes(&mut rx, &mut desired, &|_| {});

        assert_eq!(desired, vec![Topic::Player]);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn handle_rejects_requests_while_offline_but_keeps_subscriptions() {
        let (commands, mut receiver) = mpsc::channel(4);
        let state = Arc::new(AtomicU8::new(GATEWAY_OFFLINE));
        let handle = GatewayHandle {
            shutdown: None,
            commands,
            state: Arc::clone(&state),
            next_native_id: AtomicU64::new(NATIVE_REQUEST_ID_BASE),
            done: None,
            thread: None,
        };
        let request = OutEnvelope {
            v: 1,
            id: Some(7),
            kind: OutKind::Req,
            name: "status".to_string(),
            payload: serde_json::Value::Null,
        };
        assert!(matches!(
            handle.send(request),
            Err(GatewaySendError::Offline(_))
        ));
        handle
            .send(sub_env(OutKind::Sub, serde_json::json!(["player"])))
            .unwrap();
        assert!(receiver.try_recv().is_ok());

        state.store(GATEWAY_ONLINE, Ordering::Release);
        handle
            .send(OutEnvelope {
                v: 1,
                id: Some(8),
                kind: OutKind::Req,
                name: "status".to_string(),
                payload: serde_json::Value::Null,
            })
            .unwrap();
        assert!(receiver.try_recv().is_ok());

        let native_id = handle
            .send_remote(RemoteCommand::SetVolume { percent: 68 })
            .unwrap();
        let envelope = receiver.try_recv().unwrap().envelope;
        assert_eq!(native_id, NATIVE_REQUEST_ID_BASE);
        assert_eq!(envelope.id, Some(native_id));
        assert_eq!(envelope.kind, OutKind::Req);
        assert_eq!(envelope.name, "set_volume");
        assert_eq!(envelope.payload, serde_json::json!({ "percent": 68 }));
    }

    #[test]
    fn offline_drain_replies_to_correlated_requests() {
        let (tx, mut rx) = mpsc::channel(4);
        tx.try_send(GatewayCommand {
            envelope: OutEnvelope {
                v: 1,
                id: Some(41),
                kind: OutKind::Req,
                name: "status".to_string(),
                payload: serde_json::Value::Null,
            },
            source_generation: Some(73),
        })
        .unwrap();
        let events = Mutex::new(Vec::new());
        drain_offline_envelopes(&mut rx, &mut Vec::new(), &|event| {
            events.lock().unwrap().push(event);
        });
        let events = events.into_inner().unwrap();
        assert!(matches!(
            events.as_slice(),
            [GatewayEvent::Frame {
                envelope: InEnvelope {
                    id: Some(41),
                    kind: InKind::Err,
                    ..
                },
                source_generation: Some(73),
            }]
        ));
    }

    #[test]
    fn event_sequence_must_start_at_one_and_remain_contiguous() {
        let mut last = 0;
        assert!(accept_next_sequence(&mut last, 1));
        assert!(accept_next_sequence(&mut last, 2));
        assert!(!accept_next_sequence(&mut last, 4));
        assert_eq!(last, 2, "a gap must not advance the accepted sequence");
        assert!(!accept_next_sequence(&mut last, 2));

        let mut initial = 0;
        assert!(!accept_next_sequence(&mut initial, 2));
    }

    #[test]
    fn initial_subscribe_replays_desired_topics_after_system() {
        let desktop = vec![Topic::System, Topic::Player, Topic::Queue, Topic::Settings];
        assert_eq!(initial_topics(&[]), desktop);
        assert_eq!(
            initial_topics(&[Topic::Player, Topic::Queue]),
            vec![Topic::System, Topic::Player, Topic::Queue, Topic::Settings]
        );
        // A window that explicitly subscribed to system doesn't double it.
        assert_eq!(
            initial_topics(&[Topic::System, Topic::Artwork]),
            vec![
                Topic::System,
                Topic::Player,
                Topic::Queue,
                Topic::Settings,
                Topic::Artwork,
            ]
        );
    }

    #[test]
    fn reply_envelope_maps_ok_and_error() {
        let ok = reply_envelope(5, RemoteResponse::ok("done".to_string()));
        assert_eq!(ok.id, Some(5));
        assert_eq!(ok.kind, InKind::Res);

        let bad = reply_envelope(
            6,
            RemoteResponse {
                ok: false,
                reason: Some("bad_request".to_string()),
                message: None,
                status: None,
            },
        );
        assert_eq!(bad.id, Some(6));
        assert_eq!(bad.kind, InKind::Err);
        assert_eq!(
            bad.payload,
            Some(serde_json::json!({ "reason": "bad_request" }))
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
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
    async fn hello_success_with_a_non_v8_ack_is_rejected() {
        let endpoint = std::env::temp_dir()
            .join(format!("ytt-gw-version-{}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let listener = bind(&endpoint);
        let ack = HelloAck {
            ok: true,
            version: PROTOCOL_VERSION - 1,
            session_id: 1,
            capabilities: vec!["events-v8".to_string()],
            owner_mode: InstanceMode::Daemon,
            reason: None,
        };
        let server = tokio::spawn(serve_hello(listener, ack));
        let err = connect_and_hello(test_instance(endpoint.clone(), "tok"))
            .await
            .unwrap_err();
        server.abort();
        let _ = std::fs::remove_file(&endpoint);
        assert_eq!(err, "bad_ack_version");
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

    async fn accept_two_lines(listener: Listener) -> [String; 2] {
        let conn = listener.accept().await.unwrap();
        let mut reader = BufReader::new(&conn);
        let mut first = String::new();
        let mut second = String::new();
        reader.read_line(&mut first).await.unwrap();
        reader.read_line(&mut second).await.unwrap();
        [first, second]
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

        // The frontend's own id (99) must never reach the wire — the session allocates its own.
        let env = OutEnvelope {
            v: 1,
            id: Some(99),
            kind: OutKind::Cmd,
            name: "seek_to".to_string(),
            payload: serde_json::json!({ "ms": 1234 }),
        };
        let mut next_id = 5u64;
        let mut pending = HashMap::new();
        let reason = forward_command(
            &conn,
            GatewayCommand {
                envelope: env,
                source_generation: Some(3),
            },
            &mut next_id,
            &mut pending,
            &|_| {},
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
        assert_eq!(next_id, 6, "id counter advanced");
        assert!(
            pending.is_empty(),
            "a fire-and-forget cmd is not correlated"
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
            kind: OutKind::Req,
            name: "status".to_string(),
            payload: serde_json::Value::Null,
        };
        let mut next_id = 1u64;
        let mut pending = HashMap::new();
        forward_command(
            &conn,
            GatewayCommand {
                envelope: env,
                source_generation: Some(9),
            },
            &mut next_id,
            &mut pending,
            &|_| {},
        )
        .await;

        let line = server.await.unwrap();
        let _ = std::fs::remove_file(&endpoint);
        let frame: ClientFrame = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(frame.id, 1);
        assert_eq!(frame.op, ClientOp::Command(RemoteCommand::Status));
        // Session id 1 maps back to the frontend's request id 77 so its reply can be routed.
        assert_eq!(pending.get(&1), Some(&(77, Some(9))));
    }

    #[tokio::test]
    async fn refresh_topics_serialize_one_full_main_frontend_baseline() {
        let endpoint = std::env::temp_dir()
            .join(format!("ytt-gw-refresh-{}.sock", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let listener = bind(&endpoint);
        let server = tokio::spawn(accept_two_lines(listener));
        let conn = connect(&endpoint).await;
        let mut next_id = 11;
        let mut pending = HashMap::new();

        let reason = forward_command(
            &conn,
            GatewayCommand {
                envelope: OutEnvelope {
                    v: 1,
                    id: None,
                    kind: OutKind::Sub,
                    name: "refresh".to_string(),
                    payload: serde_json::json!(MAIN_FRONTEND_TOPICS),
                },
                source_generation: None,
            },
            &mut next_id,
            &mut pending,
            &|_| {},
        )
        .await;
        assert!(reason.is_none());

        let lines = server.await.unwrap();
        let _ = std::fs::remove_file(&endpoint);
        let frames = lines
            .iter()
            .map(|line| serde_json::from_str::<ClientFrame>(line.trim()).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            frames[0].op,
            ClientOp::Unsubscribe {
                topics: MAIN_FRONTEND_TOPICS.to_vec()
            }
        );
        assert_eq!(
            frames[1].op,
            ClientOp::Subscribe {
                topics: MAIN_FRONTEND_TOPICS.to_vec()
            }
        );
        assert_eq!(next_id, 13);
        assert!(pending.is_empty());
    }
}
