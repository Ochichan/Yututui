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

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout};

use crate::desktop::bridge::{InEnvelope, OutEnvelope, OutKind};
use crate::remote::endpoint;
use crate::remote::proto::{
    ClientFrame, ClientOp, HelloAck, HelloBody, HelloRequest, InstanceFile, InstanceMode,
    PROTOCOL_VERSION, PushEvent, RemoteCommand, ServerFrame, Topic,
};
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};

mod page_lifetime;

use page_lifetime::{
    FrontendCorrelation, SubscriptionState, activate_page_state, apply_subscription_change,
    correlation, drain_offline_commands, event_envelope, initial_topics, parse_topics,
    reconcile_subscriptions, reject_offline_command, reject_pending, reply_envelope,
    request_identity, validate_page_id,
};

pub use super::gateway_frontend::{MAIN_FRONTEND_TOPICS, refresh_ready_main_frontend};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const PING_INTERVAL: Duration = Duration::from_secs(15);
/// A subscription is owner-loop work and can be admitted before its snapshot/reply is produced.
/// Bound that acknowledgement window so a wedged owner cannot leave the tray optimistically
/// "subscribed" forever; reconnect replays the watch lane's latest desired snapshot.
#[cfg(not(test))]
const SUBSCRIPTION_REPLY_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const SUBSCRIPTION_REPLY_TIMEOUT: Duration = Duration::from_millis(50);
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// One serialized subscription transition. The server replies once per wire operation; only
/// after every reply succeeds may `run_session` advance its confirmed applied state.
struct PendingSubscriptionTransition {
    target: SubscriptionState,
    remaining_ids: HashSet<u64>,
    deadline: Instant,
}

impl PendingSubscriptionTransition {
    fn new(target: SubscriptionState, ids: impl IntoIterator<Item = u64>) -> Self {
        let remaining_ids: HashSet<u64> = ids.into_iter().collect();
        debug_assert!(!remaining_ids.is_empty());
        Self {
            target,
            remaining_ids,
            deadline: Instant::now() + SUBSCRIPTION_REPLY_TIMEOUT,
        }
    }
}

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
    Frame(InEnvelope),
    /// A correlated frame annotated with the native WebView generation that originated it.
    /// The PR40 page namespace remains inside the envelope; this host-side generation closes the
    /// final race where a rebuilt native surface reuses an old page-local correlation id.
    PageFrame {
        envelope: InEnvelope,
        source_generation: Option<u64>,
    },
    /// Typed, sequence-checked push for the native tray/mini projection. The ready-to-forward
    /// envelope retains PR40's page namespace for page-scoped search completions.
    Push {
        sequence: u64,
        topic: Topic,
        event: PushEvent,
        envelope: InEnvelope,
    },
}

type CorrelationKey = (Option<String>, u64);
static SOURCE_GENERATIONS: LazyLock<Mutex<HashMap<CorrelationKey, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Handle to the gateway thread; dropping it (or calling [`GatewayHandle::stop`]) tears the
/// session down and joins the worker before returning.
pub struct GatewayHandle {
    shutdown: Option<oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    commands: mpsc::Sender<OutEnvelope>,
    /// Latest desired GUI topic set. This lane is independent from the bounded command queue:
    /// subscription churn replaces one tiny snapshot instead of consuming command capacity.
    subscriptions: watch::Sender<SubscriptionState>,
    online: Arc<AtomicBool>,
}

/// Native shell correlations occupy the upper half, which the bridge rejects from WebViews.
pub const NATIVE_REQUEST_ID_BASE: u64 = crate::desktop::bridge::MAX_PAGE_REQUEST_ID + 1;
static NEXT_NATIVE_ID: AtomicU64 = AtomicU64::new(NATIVE_REQUEST_ID_BASE);

pub fn is_native_request_id(id: Option<u64>) -> bool {
    id.is_some_and(|id| id >= NATIVE_REQUEST_ID_BASE)
}

#[derive(Debug)]
pub enum GatewayCommandError {
    Encode,
    Admission(&'static str),
}

impl GatewayCommandError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Encode => "encode_failed",
            Self::Admission(reason) => reason,
        }
    }
}

#[derive(Debug)]
pub struct GatewaySendError(&'static str);

impl GatewaySendError {
    pub fn code(&self) -> &'static str {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatewayAdmissionError {
    Offline,
    InvalidPage,
    InvalidSubscription,
    Delivery(DeliveryError),
}

impl GatewayAdmissionError {
    fn reason(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::InvalidPage => "bad_page_id",
            Self::InvalidSubscription => "bad_subscription",
            Self::Delivery(error) => error.reason(),
        }
    }

    fn delivery_error(self) -> DeliveryError {
        match self {
            // `DeliveryError` is shared across actors and has no gateway-specific offline
            // variant. `send_or_reject` retains the precise machine reason for the UI.
            Self::Offline => DeliveryError::Closed,
            Self::InvalidPage | Self::InvalidSubscription => DeliveryError::StaleOrFull,
            Self::Delivery(error) => error,
        }
    }
}

impl GatewayHandle {
    pub fn stop(mut self) {
        self.shutdown_and_join();
    }

    fn shutdown_and_join(&mut self) {
        self.online.store(false, Ordering::Release);
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::warn!(target: "ytt_desktop", "gateway thread panicked during shutdown");
        }
    }

    /// Forward a webview envelope (`cmd`/`req`/`sub`/`unsub`) to the live session. Commands
    /// are rejected while offline; subscription declarations remain admissible so their desired
    /// state can be replayed after reconnect. Saturation and shutdown remain typed outcomes.
    pub fn send(&self, env: OutEnvelope) -> DeliveryResult {
        self.admit(env)
            .map_err(GatewayAdmissionError::delivery_error)
    }

    /// Admit a page request while retaining the native surface generation that owns it.
    fn send_from_generation(
        &self,
        env: OutEnvelope,
        source_generation: Option<u64>,
    ) -> Result<DeliveryReceipt, GatewayAdmissionError> {
        let key = correlation(&env).and_then(|correlation| {
            source_generation.map(|generation| ((correlation.page_id, correlation.id), generation))
        });
        if let Some((key, generation)) = &key {
            SOURCE_GENERATIONS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key.clone(), *generation);
        }
        let result = self.admit(env);
        if result.is_err()
            && let Some((key, _)) = key
        {
            SOURCE_GENERATIONS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
        }
        result
    }

    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Acquire)
    }

    /// Route a native tray/panel command over the same ordered v8 session as the frontend.
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
        let id = NEXT_NATIVE_ID.fetch_add(1, Ordering::Relaxed);
        if id < NATIVE_REQUEST_ID_BASE {
            return Err(GatewayCommandError::Admission("closed"));
        }
        self.admit(OutEnvelope {
            v: crate::desktop::bridge::BRIDGE_VERSION,
            id: Some(id),
            page_id: None,
            request_id: None,
            kind: OutKind::Req,
            name,
            payload: serde_json::Value::Object(payload),
        })
        .map(|_| ())
        .map_err(|error| GatewayCommandError::Admission(error.reason()))?;
        Ok(id)
    }

    /// Force authoritative snapshots after a ready handshake or stale-revision rejection.
    pub fn refresh_topics(&self, topics: &[Topic]) -> Result<(), GatewaySendError> {
        if topics.is_empty() {
            return Ok(());
        }
        self.admit(OutEnvelope {
            v: crate::desktop::bridge::BRIDGE_VERSION,
            id: None,
            page_id: None,
            request_id: None,
            kind: OutKind::Sub,
            name: "refresh".to_string(),
            payload: serde_json::json!(topics),
        })
        .map(|_| ())
        .map_err(|error| GatewaySendError(error.reason()))
    }

    pub fn refresh_topic(&self, topic: Topic) -> Result<(), GatewaySendError> {
        self.refresh_topics(&[topic])
    }

    fn admit(&self, env: OutEnvelope) -> Result<DeliveryReceipt, GatewayAdmissionError> {
        let refresh = env.kind == OutKind::Sub && env.name == "refresh";
        if matches!(env.kind, OutKind::Sub | OutKind::Unsub) && !refresh {
            return self.update_subscriptions(&env);
        }
        if !refresh {
            self.activate_page(&env)?;
        }
        if (matches!(env.kind, OutKind::Cmd | OutKind::Req) || refresh)
            && !self.online.load(Ordering::Acquire)
        {
            return Err(GatewayAdmissionError::Offline);
        }
        match self.commands.try_send(env) {
            Ok(()) => Ok(DeliveryReceipt::Enqueued),
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err(GatewayAdmissionError::Delivery(DeliveryError::Busy))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(GatewayAdmissionError::Delivery(DeliveryError::Closed))
            }
        }
    }

    fn update_subscriptions(
        &self,
        env: &OutEnvelope,
    ) -> Result<DeliveryReceipt, GatewayAdmissionError> {
        validate_page_id(env.page_id.as_deref())?;
        let Some(topics) = parse_topics(&env.payload) else {
            return Err(GatewayAdmissionError::InvalidSubscription);
        };
        let changed = self.subscriptions.send_if_modified(|desired| {
            let page_changed = activate_page_state(env.page_id.as_deref(), desired);
            let topics_changed = apply_subscription_change(env.kind, &topics, &mut desired.topics);
            page_changed || topics_changed
        });
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: changed,
            evicted_oldest: false,
        })
    }

    /// Observe a page-aware command before it enters the bounded lane. This closes the previous
    /// page's subscription lifetime even if the replacement issues a command before its stores
    /// declare topics. Legacy envelopes omit `page_id` and retain their previous behavior.
    fn activate_page(&self, env: &OutEnvelope) -> Result<(), GatewayAdmissionError> {
        validate_page_id(env.page_id.as_deref())?;
        self.subscriptions
            .send_if_modified(|desired| activate_page_state(env.page_id.as_deref(), desired));
        Ok(())
    }
}

/// Try to admit one webview envelope and render an immediate local error for a correlated
/// `req`/`cmd` when the bounded queue cannot accept it. Subscription declarations bypass that
/// queue and atomically replace the separate latest-desired-state lane.
#[cfg(test)]
pub(crate) fn send_or_reject(
    handle: Option<&GatewayHandle>,
    env: OutEnvelope,
) -> Option<InEnvelope> {
    send_or_reject_from_generation(handle, env, None)
}

pub(crate) fn send_or_reject_from_generation(
    handle: Option<&GatewayHandle>,
    env: OutEnvelope,
    source_generation: Option<u64>,
) -> Option<InEnvelope> {
    let correlation = correlation(&env);
    let kind = env.kind;
    let name = env.name.clone();
    let result = match handle {
        Some(handle) => handle.send_from_generation(env, source_generation),
        None => Err(GatewayAdmissionError::Delivery(DeliveryError::Closed)),
    };
    match result {
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(
                target: "ytt_desktop",
                envelope_kind = ?kind,
                envelope_name = %name,
                delivery_outcome = error.reason(),
                correlated = correlation.is_some(),
                "gateway command was not accepted"
            );
            correlation.map(|correlation| {
                InEnvelope::err_for_page(
                    correlation.id,
                    correlation.page_id,
                    serde_json::json!({ "reason": error.reason() }),
                )
            })
        }
    }
}

impl Drop for GatewayHandle {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

/// Spawn the gateway thread. `emit` runs on the gateway thread and must not block; the platform
/// wrapper forwards to the loop via `EventLoopProxy::send_event` (it is `!Send`-safe to just clone
/// a proxy into the closure).
pub fn spawn<F>(emit: F) -> GatewayHandle
where
    F: Fn(GatewayEvent) + Send + 'static,
{
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    // Bounded with a generous cap; a webview that floods commands while the session is stalled
    // gets an explicit admission failure rather than growing the queue without bound.
    let (cmd_tx, cmd_rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::DESKTOP_GATEWAY_QUEUE,
    );
    // Native tray/mini projection stays live even when the full WebView is unavailable.
    let (subscription_tx, subscription_rx) = watch::channel(SubscriptionState {
        page_id: None,
        topics: vec![Topic::Player, Topic::Queue, Topic::Settings],
    });
    let online = Arc::new(AtomicBool::new(false));
    let thread_online = Arc::clone(&online);
    let request_namespace: Arc<str> = Arc::from(crate::remote::requests::fresh_request_id());
    let thread_request_namespace = Arc::clone(&request_namespace);
    let builder = std::thread::Builder::new().name("yututray-gateway".to_string());
    let worker = match builder.spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            emit(GatewayEvent::Connection(ConnState::Offline {
                reason: "no_runtime".into(),
            }));
            return;
        };
        let routed_emit = move |event| {
            emit(attach_source_generation(event));
        };
        rt.block_on(run(
            routed_emit,
            shutdown_rx,
            cmd_rx,
            subscription_rx,
            thread_online,
            thread_request_namespace,
        ));
    }) {
        Ok(worker) => Some(worker),
        Err(e) => {
            tracing::warn!(target: "ytt_desktop", error = %e, "could not start gateway thread");
            None
        }
    };
    GatewayHandle {
        shutdown: Some(shutdown_tx),
        worker,
        commands: cmd_tx,
        subscriptions: subscription_tx,
        online,
    }
}

fn attach_source_generation(event: GatewayEvent) -> GatewayEvent {
    match event {
        GatewayEvent::Frame(envelope) => {
            let source_generation = envelope.id.and_then(|id| {
                SOURCE_GENERATIONS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&(envelope.page_id.clone(), id))
            });
            GatewayEvent::PageFrame {
                envelope,
                source_generation,
            }
        }
        event => event,
    }
}

async fn run<F: Fn(GatewayEvent)>(
    emit: F,
    mut shutdown_rx: oneshot::Receiver<()>,
    mut cmd_rx: mpsc::Receiver<OutEnvelope>,
    mut subscription_rx: watch::Receiver<SubscriptionState>,
    online: Arc<AtomicBool>,
    request_namespace: Arc<str>,
) {
    online.store(false, Ordering::Release);
    let mut backoff = BACKOFF_MIN;
    loop {
        emit(GatewayEvent::Connection(ConnState::Connecting));
        let opened = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                online.store(false, Ordering::Release);
                drain_offline_commands(&mut cmd_rx, &emit, "shutdown");
                return;
            }
            opened = open_session() => opened,
        };
        let reason = match opened {
            Ok((conn, ack)) => {
                // Reject commands that raced the previous session's offline transition. Topic
                // intent lives in `subscription_rx`, so it cannot be drained or lost here.
                drain_offline_commands(&mut cmd_rx, &emit, "offline");
                backoff = BACKOFF_MIN; // healthy connection resets the backoff
                online.store(true, Ordering::Release);
                emit(GatewayEvent::Connection(ConnState::Online {
                    protocol_version: ack.version,
                    capabilities: ack.capabilities,
                    owner_mode: ack.owner_mode,
                }));
                let reason = run_session(
                    conn,
                    &mut shutdown_rx,
                    &mut cmd_rx,
                    &mut subscription_rx,
                    &emit,
                    online.as_ref(),
                    request_namespace.as_ref(),
                )
                .await;
                online.store(false, Ordering::Release);
                tracing::info!(target: "ytt_desktop", %reason, "gateway session ended");
                reason
            }
            Err(reason) => {
                online.store(false, Ordering::Release);
                reason
            }
        };
        // Commands left in the local lane never reached the session. Reject those definitively
        // before the offline notification causes the frontend to conservatively classify any
        // still-pending mutation as confirmation-lost.
        drain_offline_commands(&mut cmd_rx, &emit, &reason);
        emit(GatewayEvent::Connection(ConnState::Offline {
            reason: reason.clone(),
        }));
        if reason == "shutdown" {
            return;
        }

        // Subscription updates need no consumer during backoff: the watch lane retains only
        // the latest bounded snapshot and the next session replays it in full.
        let delay = tokio::time::sleep(backoff);
        tokio::pin!(delay);
        loop {
            tokio::select! {
                biased;
                _ = &mut shutdown_rx => {
                    online.store(false, Ordering::Release);
                    drain_offline_commands(&mut cmd_rx, &emit, "shutdown");
                    return;
                },
                _ = &mut delay => break,
                maybe = cmd_rx.recv() => match maybe {
                    Some(env) => reject_offline_command(env, &emit, &reason),
                    None => {
                        online.store(false, Ordering::Release);
                        return;
                    }
                }
            }
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
    drop(reader); // release the borrow so the caller owns `conn` outright
    Ok((conn, ack))
}

/// Start the next delta only when the previous subscription transition has been acknowledged.
/// A transition with no wire delta (for example a page generation change with no page-owned
/// topics) is confirmed locally and needs no pending entry.
async fn begin_subscription_transition(
    conn: &Stream,
    next_id: &mut u64,
    applied: &mut SubscriptionState,
    desired: &SubscriptionState,
) -> io::Result<Option<PendingSubscriptionTransition>> {
    let ids = reconcile_subscriptions(conn, next_id, applied, desired).await?;
    if ids.is_empty() {
        applied.clone_from(desired);
        Ok(None)
    } else {
        Ok(Some(PendingSubscriptionTransition::new(
            desired.clone(),
            ids,
        )))
    }
}

fn subscription_rejection_reason(resp: &crate::remote::proto::RemoteResponse) -> &'static str {
    if resp.reason.as_deref() == Some("server_busy") {
        "subscription_busy"
    } else {
        "subscription_rejected"
    }
}

/// Drive an established session until it closes; returns the machine reason.
async fn run_session<F: Fn(GatewayEvent)>(
    conn: Stream,
    shutdown_rx: &mut oneshot::Receiver<()>,
    cmd_rx: &mut mpsc::Receiver<OutEnvelope>,
    subscription_rx: &mut watch::Receiver<SubscriptionState>,
    emit: &F,
    online: &AtomicBool,
    request_namespace: &str,
) -> String {
    if !matches!(
        shutdown_rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ) {
        online.store(false, Ordering::Release);
        return "shutdown".to_string();
    }
    let mut reader = BufReader::new(&conn);
    let mut next_id = 1u64;
    let mut last_sequence = 0u64;
    // Retain fragmented bytes when another select branch wins; cancelling a fresh local buffer
    // would silently desynchronize the stream.
    let mut frame_buf = Vec::new();
    // Session id of a correlated `req` or `cmd` → frontend identity plus operation kind. Every
    // reply removes one entry; session exit marks written mutations as confirmation-lost while
    // ordinary requests retain the transport reason.
    let mut pending: HashMap<u64, FrontendCorrelation> = HashMap::new();

    // Subscribe to `system` (so we notice owner shutdown even with no window open, and to
    // keep the session non-idle) plus every topic a window already declared. New sessions
    // send fresh snapshots for all of these, which is exactly the rehydrate the frontend
    // expects after a reconnect.
    let mut desired_subscriptions = subscription_rx.borrow_and_update().clone();
    let mut applied_subscriptions = SubscriptionState::default();
    let sub = ClientFrame {
        id: next_id,
        request_id: None,
        page_id: desired_subscriptions.page_id.clone(),
        op: ClientOp::Subscribe {
            topics: initial_topics(&desired_subscriptions.topics),
        },
    };
    next_id += 1;
    if write_line(&conn, &sub).await.is_err() {
        online.store(false, Ordering::Release);
        return "disconnected".to_string();
    }
    let mut pending_subscription = Some(PendingSubscriptionTransition::new(
        desired_subscriptions.clone(),
        [sub.id],
    ));

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // consume the immediate first tick
    let mut awaiting_pong = false;
    let mut subscriptions_open = true;

    let reason = 'session: loop {
        tokio::select! {
            biased;
            _ = &mut *shutdown_rx => break 'session "shutdown".to_string(),
            _ = ping.tick() => {
                if awaiting_pong {
                    break 'session "ping_timeout".to_string();
                }
                let frame = ClientFrame {
                    id: next_id,
                    request_id: None,
                    page_id: None,
                    op: ClientOp::Ping,
                };
                next_id += 1;
                if write_line(&conn, &frame).await.is_err() {
                    break 'session "disconnected".to_string();
                }
                awaiting_pong = true;
            }
            _ = tokio::time::sleep_until(
                pending_subscription
                    .as_ref()
                    .map_or_else(Instant::now, |pending| pending.deadline)
            ), if pending_subscription.is_some() => {
                break 'session "subscription_timeout".to_string();
            }
            changed = subscription_rx.changed(), if subscriptions_open => {
                if changed.is_err() {
                    subscriptions_open = false;
                    continue;
                }
                desired_subscriptions = subscription_rx.borrow_and_update().clone();
                if pending_subscription.is_none() {
                    pending_subscription = match begin_subscription_transition(
                        &conn,
                        &mut next_id,
                        &mut applied_subscriptions,
                        &desired_subscriptions,
                    )
                    .await
                    {
                        Ok(pending) => pending,
                        Err(_) => break 'session "disconnected".to_string(),
                    };
                }
            }
            // A page-scoped command cannot overtake the subscription transition that makes
            // that page current in the core. This also keeps an initial RunSearch behind its
            // Search subscription acknowledgement, so an accepted completion has a live sink.
            maybe = cmd_rx.recv(), if pending_subscription.is_none()
                && applied_subscriptions == desired_subscriptions => {
                match maybe {
                    Some(env) => {
                        if env.page_id.is_some()
                            && env.page_id != applied_subscriptions.page_id
                        {
                            reject_offline_command(env, emit, "stale_page");
                            continue;
                        }
                        if let Some(reason) =
                            forward_command(
                                &conn,
                                env,
                                &mut next_id,
                                &mut pending,
                                emit,
                                request_namespace,
                            )
                            .await
                        {
                            break 'session reason;
                        }
                    }
                    // The handle was dropped. Do not spin on an always-ready closed receiver.
                    None => break 'session "shutdown".to_string(),
                }
            }
            line = read_line_buffered(&mut reader, &mut frame_buf) => match line {
                Ok(Some(l)) => match serde_json::from_str::<ServerFrame>(l.trim()) {
                    Ok(ServerFrame::Pong { .. }) => awaiting_pong = false,
                    Ok(ServerFrame::Goodbye { reason }) => break 'session reason,
                    Ok(ServerFrame::Reply { id, resp }) => {
                        if pending_subscription
                            .as_ref()
                            .is_some_and(|pending| pending.remaining_ids.contains(&id))
                        {
                            if !resp.ok {
                                break 'session subscription_rejection_reason(&resp).to_string();
                            }
                            let transition_complete = {
                                let pending = pending_subscription
                                    .as_mut()
                                    .expect("the subscription reply id was just matched");
                                pending.remaining_ids.remove(&id);
                                pending.remaining_ids.is_empty()
                            };
                            if transition_complete {
                                let completed = pending_subscription
                                    .take()
                                    .expect("a completed transition must still be pending");
                                applied_subscriptions = completed.target;
                                if applied_subscriptions != desired_subscriptions {
                                    pending_subscription = match begin_subscription_transition(
                                        &conn,
                                        &mut next_id,
                                        &mut applied_subscriptions,
                                        &desired_subscriptions,
                                    )
                                    .await
                                    {
                                        Ok(pending) => pending,
                                        Err(_) => break 'session "disconnected".to_string(),
                                    };
                                }
                            }
                            continue;
                        }
                        if let Some(correlation) = pending.remove(&id) {
                            emit(GatewayEvent::Frame(reply_envelope(correlation, resp)));
                        }
                        // Other unmapped ids are stale/invalid peer replies. Pings use `Pong`.
                    }
                    Ok(ServerFrame::Event { seq, topic, event }) => {
                        if !accept_next_sequence(&mut last_sequence, seq) {
                            break 'session "sequence_gap".to_string();
                        }
                        let shutting_down = matches!(event, PushEvent::ShuttingDown);
                        let envelope = event_envelope(topic, &event);
                        emit(GatewayEvent::Push {
                            sequence: seq,
                            topic,
                            event,
                            envelope,
                        });
                        if shutting_down {
                            break 'session "shutting_down".to_string();
                        }
                    }
                    Err(_) => break 'session "bad_frame".to_string(),
                },
                Ok(None) | Err(_) => break 'session "disconnected".to_string(),
            }
        }
    };
    // Close admission before emitting terminal errors so callbacks cannot enqueue new
    // correlated work into a session that has already ended.
    online.store(false, Ordering::Release);
    reject_pending(&mut pending, &reason, emit);
    reason
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

/// Translate a webview envelope into a [`ClientFrame`] and write it to the session. Returns
/// `Some(reason)` only on a socket write failure (which ends the session); a command that
/// can't be translated is rejected when correlated or logged when uncorrelated, without tearing
/// down the session.
async fn forward_command<F: Fn(GatewayEvent)>(
    conn: &Stream,
    env: OutEnvelope,
    next_id: &mut u64,
    pending: &mut HashMap<u64, FrontendCorrelation>,
    emit: &F,
    request_namespace: &str,
) -> Option<String> {
    if env.kind == OutKind::Sub && env.name == "refresh" {
        let topics = parse_topics(&env.payload)?;
        for op in [
            ClientOp::Unsubscribe {
                topics: topics.clone(),
            },
            ClientOp::Subscribe { topics },
        ] {
            let frame = ClientFrame {
                id: *next_id,
                request_id: None,
                page_id: env.page_id.clone(),
                op,
            };
            *next_id += 1;
            if write_line(conn, &frame).await.is_err() {
                return Some("disconnected".to_string());
            }
        }
        return None;
    }
    let mut correlation = correlation(&env);
    if validate_page_id(env.page_id.as_deref()).is_err() {
        if let Some(correlation) = correlation {
            emit(GatewayEvent::Frame(InEnvelope::err_for_page(
                correlation.id,
                correlation.page_id,
                serde_json::json!({ "reason": "bad_page_id" }),
            )));
        }
        return None;
    }
    // The About card's IPC self-test. `ping` is not a core command: the wire under test
    // is exactly WebView → bridge → gateway, so answer natively (the demo core replies
    // with the same bare string, and reaching this point proves the session is live).
    if matches!(env.kind, OutKind::Cmd | OutKind::Req) && env.name == "ping" {
        if let Some(correlation) = correlation {
            emit(GatewayEvent::Frame(InEnvelope::res_for_page(
                correlation.id,
                correlation.page_id,
                serde_json::json!("pong"),
            )));
        }
        return None;
    }
    let op = match env.kind {
        OutKind::Cmd | OutKind::Req => match to_remote_command(&env.name, &env.payload) {
            Some(cmd) => {
                if let Some(correlation) = &mut correlation {
                    correlation.mutation = cmd.requires_confirmation();
                }
                ClientOp::Command(cmd)
            }
            None => {
                // Unsupported command name/shape. Any correlated command must not hang for its
                // frontend timeout; an uncorrelated command is only logged.
                if let Some(correlation) = correlation.clone() {
                    emit(GatewayEvent::Frame(InEnvelope::err_for_page(
                        correlation.id,
                        correlation.page_id,
                        serde_json::json!({ "reason": "bad_command" }),
                    )));
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
        // Subscription declarations are consumed synchronously by `GatewayHandle` and can
        // never enter this bounded command lane.
        OutKind::Sub | OutKind::Unsub => return None,
        // `win` never reaches the gateway (bridge routes it natively); ignore defensively.
        OutKind::Win => return None,
    };

    let sid = *next_id;
    *next_id += 1;
    let request_id = if matches!(&op, ClientOp::Command(_)) {
        match request_identity(&env, correlation.as_ref(), request_namespace, sid) {
            Ok(request_id) => Some(request_id),
            Err(()) => {
                if let Some(correlation) = correlation.clone() {
                    emit(GatewayEvent::Frame(InEnvelope::err_for_page(
                        correlation.id,
                        correlation.page_id,
                        serde_json::json!({ "reason": "bad_request_id" }),
                    )));
                }
                return None;
            }
        }
    } else {
        None
    };
    if let Some(correlation) = correlation {
        pending.insert(sid, correlation);
    }
    let frame = ClientFrame {
        id: sid,
        request_id,
        page_id: env.page_id.clone(),
        op,
    };
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

async fn write_line<T: serde::Serialize>(conn: &Stream, value: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(value).map_err(io::Error::other)?;
    buf.push(b'\n');
    write_bytes_with_timeout(conn, &buf, WRITE_TIMEOUT).await
}

async fn write_bytes_with_timeout(
    conn: &Stream,
    buf: &[u8],
    write_timeout: Duration,
) -> io::Result<()> {
    let mut w = conn;
    match timeout(write_timeout, async {
        w.write_all(buf).await?;
        w.flush().await
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                target: "ytt_desktop",
                timeout_ms = write_timeout.as_millis(),
                "gateway socket write timed out"
            );
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "gateway write timed out",
            ))
        }
    }
}

/// Session-frame cap for the v8 gateway protocol, matching the remote server's
/// `SESSION_MAX_FRAME_BYTES`. A peer that never sends a newline (or sends a giant frame) is
/// torn down instead of growing the buffer until the desktop process OOMs.
const GATEWAY_MAX_FRAME_BYTES: usize = 256 * 1024;

async fn read_line<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    read_line_buffered(reader, &mut buf).await
}

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
mod shutdown_tests;

#[cfg(test)]
mod translate_tests;

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod subscription_tests;
