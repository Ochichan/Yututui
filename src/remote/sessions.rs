//! Runtime state for protocol-v8 sessions: the per-owner registry, per-session outbound
//! queues with a byte budget, and the connection loop that speaks
//! [`crate::remote::proto::ClientFrame`]/[`crate::remote::proto::ServerFrame`].
//!
//! Naming: `RemoteSession*` (not `Session*`) — `crate::session` is the unrelated
//! last-session resume cache.
//!
//! Backpressure (docs/gui/02 §8): each session owns a bounded outbound queue capped at
//! **256 items or 8 MB of buffered bytes, whichever trips first**. Enqueue never blocks;
//! an overflowing session is evicted with `Goodbye { reason: "slow_consumer" }`. A
//! wedged client can never block the owner loop and can never pin unbounded memory.
//! `Ping` is answered directly on the session task — it never enters the owner loop.
//!
//! Subscribe frames are forwarded to the owner loop, where the
//! [`crate::remote::publish::Publisher`] records the subscription set, emits one initial
//! snapshot per newly subscribed topic, and then the `Reply` — all into this session's
//! outbound queue, making the snapshot-before-Reply order structural (docs/gui/02 §6).

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use interprocess::local_socket::tokio::Stream;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::proto::{
    ClientFrame, ClientOp, HelloAck, HelloRequest, InstanceMode, PROTOCOL_VERSION,
    REMOTE_MAX_TOPICS, RemoteResponse, ServerFrame, Topic,
};
use super::server::{ReadLineOutcome, RemoteEvent, RemoteReply, read_bounded_line};

/// Hard cap on concurrent sessions per owner (`sessions_full` on the next Hello).
/// TUI + GUI + tray + tests is ≤ 4 realistic.
pub(crate) const MAX_SESSIONS: usize = 8;
/// One session frame line (inbound). Commands can carry a track list, but bulk plays are
/// server-side by contract, so this stays modest.
pub(crate) const SESSION_MAX_FRAME_BYTES: usize = 256 * 1024;

/// Tunables that tests shrink to make idleness/eviction observable in milliseconds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SessionTuning {
    /// A session silent this long is garbage-collected (clients ping every 15 s).
    pub idle_timeout: Duration,
    /// Outbound queue caps: items and buffered bytes, whichever trips first.
    pub max_queued_items: usize,
    pub max_queued_bytes: usize,
    /// How long a `Command` frame may wait on the owner loop for its reply.
    pub reply_timeout: Duration,
    /// How long playback-loading `Command` frames may wait on the owner loop.
    pub playback_reply_timeout: Duration,
}

impl Default for SessionTuning {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            max_queued_items: 256,
            max_queued_bytes: 8 * 1024 * 1024,
            reply_timeout: Duration::from_secs(2),
            playback_reply_timeout: Duration::from_secs(20),
        }
    }
}

/// Why a session's outbound lane was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseReason {
    SlowConsumer,
    IdleTimeout,
    ClientGone,
    BadFrame,
    ShuttingDown,
}

impl CloseReason {
    fn goodbye(self) -> Option<&'static str> {
        match self {
            CloseReason::SlowConsumer => Some("slow_consumer"),
            CloseReason::IdleTimeout => Some("idle_timeout"),
            CloseReason::BadFrame => Some("bad_request"),
            CloseReason::ShuttingDown => Some("shutting_down"),
            CloseReason::ClientGone => None,
        }
    }
}

/// One outbound line: either a fully-serialized raw frame, or a push event whose
/// payload is shared across sessions (`Arc`) with the tiny per-session envelope —
/// `{"frame":"event","seq":N,"topic":"…","event":<payload>}` — spliced by the writer at
/// write time. Serialize-once fan-out without a per-session payload copy.
pub(crate) enum SessionLine {
    Raw(Vec<u8>),
    Event {
        seq: u64,
        topic: Topic,
        payload: Arc<Vec<u8>>,
    },
}

impl SessionLine {
    fn cost(&self) -> usize {
        match self {
            SessionLine::Raw(bytes) => bytes.len(),
            // Envelope overhead is ~64 bytes; close enough for the byte budget.
            SessionLine::Event { payload, .. } => payload.len() + 64,
        }
    }
}

/// One registered session as the hub sees it.
pub(crate) struct RemoteSessionHandle {
    line_tx: mpsc::Sender<SessionLine>,
    queued_bytes: Arc<AtomicUsize>,
    subscriptions: Mutex<std::collections::HashSet<Topic>>,
    /// Per-session monotonic event sequence (docs/gui/02 §6).
    seq: AtomicU64,
    /// Set on eviction: every further enqueue fails, so the reader tears down on its
    /// next frame (worst case it lingers until the idle GC; pushes stop immediately).
    evicted: std::sync::atomic::AtomicBool,
}

/// A host-visible reference to one live session, carried inside
/// [`RemoteEvent::SessionSubscribe`] so the owner loop (the Publisher) can emit the
/// initial snapshots and the reply into exactly this session's outbound queue.
#[derive(Clone)]
pub struct RemoteSessionRef {
    pub(crate) session_id: u64,
    pub(crate) handle: Arc<RemoteSessionHandle>,
}

impl std::fmt::Debug for RemoteSessionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSessionRef")
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl RemoteSessionRef {
    /// Record subscriptions, returning only the newly added topics (idempotence).
    pub(crate) fn subscribe(&self, topics: &[Topic]) -> Vec<Topic> {
        self.handle.subscribe(topics)
    }
}

/// Exactly-once persistent-session reply shared by the session timeout and the owner loop.
///
/// The owner completes this through [`RemoteReply::send`]. Completion first claims the response,
/// then synchronously enqueues the reply frame, and only then wakes the session reader. A timeout
/// races through the same gate, so a late owner cannot emit a duplicate reply after `timeout`.
#[derive(Clone)]
struct DirectSessionReply {
    inner: Arc<DirectSessionReplyInner>,
}

struct DirectSessionReplyInner {
    hub: Arc<RemoteSessionHub>,
    session: RemoteSessionRef,
    frame_id: u64,
    completed: std::sync::atomic::AtomicBool,
    done: Mutex<Option<oneshot::Sender<()>>>,
}

impl DirectSessionReply {
    fn new(
        hub: Arc<RemoteSessionHub>,
        session: RemoteSessionRef,
        frame_id: u64,
    ) -> (Self, oneshot::Receiver<()>) {
        let (done_tx, done_rx) = oneshot::channel();
        (
            Self {
                inner: Arc::new(DirectSessionReplyInner {
                    hub,
                    session,
                    frame_id,
                    completed: std::sync::atomic::AtomicBool::new(false),
                    done: Mutex::new(Some(done_tx)),
                }),
            },
            done_rx,
        )
    }

    fn complete(&self, response: RemoteResponse) -> bool {
        if self
            .inner
            .completed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }

        let frame = ServerFrame::Reply {
            id: self.inner.frame_id,
            resp: response,
        };
        let sent = self.inner.hub.send_raw_to(&self.inner.session, &frame);
        if let Some(done) = self
            .inner
            .done
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = done.send(());
        }

        sent
    }
}

/// Test-only: a hub with one registered session, exposed as the host-visible ref plus
/// the raw outbound receiver so Publisher tests can assert enqueue order directly.
#[cfg(test)]
pub(crate) fn test_register(
    tuning: SessionTuning,
) -> (
    Arc<RemoteSessionHub>,
    RemoteSessionRef,
    mpsc::Receiver<SessionLine>,
) {
    let hub = Arc::new(RemoteSessionHub::new(
        InstanceMode::StandaloneTui,
        vec!["events-v8".to_string()],
        tuning,
    ));
    let (session_id, handle, rx) = hub.register().expect("fresh hub has room");
    (
        Arc::clone(&hub),
        RemoteSessionRef { session_id, handle },
        rx,
    )
}

/// Test-only persistent reply wired through the same exactly-once/direct-enqueue primitive as a
/// real v8 session. The receiver side of the completion signal is intentionally discarded: owner
/// ordering tests inspect the outbound session lane itself.
#[cfg(test)]
pub(crate) fn test_command_reply(
    hub: Arc<RemoteSessionHub>,
    session: RemoteSessionRef,
    frame_id: u64,
) -> RemoteReply {
    let (direct_reply, _done) = DirectSessionReply::new(hub, session, frame_id);
    RemoteReply::direct(move |response| direct_reply.complete(response))
}

/// The per-owner session registry. The server's accept path registers sessions here;
/// the Publisher fans events out through [`broadcast`](Self::broadcast).
pub struct RemoteSessionHub {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<u64, Arc<RemoteSessionHandle>>>,
    tuning: SessionTuning,
    pub(crate) owner_mode: InstanceMode,
    pub(crate) capabilities: Vec<String>,
}

impl RemoteSessionHub {
    pub(crate) fn new(
        owner_mode: InstanceMode,
        capabilities: Vec<String>,
        tuning: SessionTuning,
    ) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
            tuning,
            owner_mode,
            capabilities,
        }
    }

    pub(crate) fn register(
        &self,
    ) -> Option<(u64, Arc<RemoteSessionHandle>, mpsc::Receiver<SessionLine>)> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if sessions.len() >= MAX_SESSIONS {
            return None;
        }
        // +1 as a best-effort goodbye reserve: pushes may consume it under sustained
        // overflow (the goodbye is then dropped — eviction itself never depends on it).
        let (line_tx, line_rx) = mpsc::channel(self.tuning.max_queued_items + 1);
        let handle = Arc::new(RemoteSessionHandle {
            line_tx,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            subscriptions: Mutex::new(std::collections::HashSet::new()),
            seq: AtomicU64::new(0),
            evicted: std::sync::atomic::AtomicBool::new(false),
        });
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        sessions.insert(id, Arc::clone(&handle));
        Some((id, handle, line_rx))
    }

    fn unregister(&self, id: u64) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }

    /// Whether any live session subscribes to `topic` — lets the Publisher skip building
    /// a model nobody would receive.
    pub(crate) fn any_subscribed(&self, topic: Topic) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|handle| handle.subscribed(topic))
    }

    /// Fan one pre-serialized push payload out to every session subscribed to `topic`.
    /// A session whose outbound budget trips is evicted with `slow_consumer` — the
    /// owner loop never blocks on a wedged client.
    pub(crate) fn broadcast(&self, topic: Topic, payload: &Arc<Vec<u8>>) {
        let subscribed: Vec<(u64, Arc<RemoteSessionHandle>)> = {
            let sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            sessions
                .iter()
                .filter(|(_, handle)| handle.subscribed(topic))
                .map(|(id, handle)| (*id, Arc::clone(handle)))
                .collect()
        };
        for (id, handle) in subscribed {
            if !handle.push_event(topic, payload, self.tuning.max_queued_bytes) {
                self.evict(id, &handle);
            }
        }
    }

    /// Push one event into a single session (initial snapshots on subscribe). Returns
    /// `false` after evicting the session on overflow.
    pub(crate) fn send_event_to(
        &self,
        session: &RemoteSessionRef,
        topic: Topic,
        payload: &Arc<Vec<u8>>,
    ) -> bool {
        if session
            .handle
            .push_event(topic, payload, self.tuning.max_queued_bytes)
        {
            true
        } else {
            self.evict(session.session_id, &session.handle);
            false
        }
    }

    /// Send one raw frame to a single session (replies). Returns `false` after evicting.
    pub(crate) fn send_raw_to(&self, session: &RemoteSessionRef, frame: &ServerFrame) -> bool {
        if session
            .handle
            .try_send_line(SessionLine::Raw(frame_line(frame)))
            && !session
                .handle
                .over_byte_budget(self.tuning.max_queued_bytes)
        {
            true
        } else {
            self.evict(session.session_id, &session.handle);
            false
        }
    }

    /// Broadcast a goodbye to every live session and drop them — the owner is exiting.
    /// Best-effort: full queues just close without the goodbye.
    pub(crate) fn shutdown_all(&self) {
        let all: Vec<Arc<RemoteSessionHandle>> = {
            let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            let drained = sessions.values().cloned().collect();
            sessions.clear();
            drained
        };
        for handle in all {
            let _ = handle.try_send_line(SessionLine::Raw(frame_line(&ServerFrame::Goodbye {
                reason: "shutting_down".to_string(),
            })));
            handle.evicted.store(true, Ordering::Relaxed);
        }
    }

    /// Evict one session: best-effort goodbye through the reserve slot, then remove it
    /// from the registry. Its reader notices on the next frame (its replies stop
    /// enqueueing) and tears the connection down.
    fn evict(&self, id: u64, handle: &RemoteSessionHandle) {
        let _ = handle.try_send_line(SessionLine::Raw(frame_line(&ServerFrame::Goodbye {
            reason: "slow_consumer".to_string(),
        })));
        handle.evicted.store(true, Ordering::Relaxed);
        self.unregister(id);
    }

    #[cfg(test)]
    fn active(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

impl RemoteSessionHandle {
    /// Enqueue one outbound line. `false` means the session was evicted, the item
    /// budget tripped, or the writer is gone — the caller must treat it as dead.
    fn try_send_line(&self, line: SessionLine) -> bool {
        if self.evicted.load(Ordering::Relaxed) {
            return false;
        }
        // (Ordering is Relaxed: the counter is advisory backpressure, not a lock.)
        self.queued_bytes.fetch_add(line.cost(), Ordering::Relaxed);
        match self.line_tx.try_send(line) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(line))
            | Err(mpsc::error::TrySendError::Closed(line)) => {
                self.queued_bytes.fetch_sub(line.cost(), Ordering::Relaxed);
                false
            }
        }
    }

    /// Enqueue one push event with the next per-session `seq`; `false` on budget trip.
    fn push_event(&self, topic: Topic, payload: &Arc<Vec<u8>>, max_bytes: usize) -> bool {
        if self.over_byte_budget(max_bytes) {
            return false;
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.try_send_line(SessionLine::Event {
            seq,
            topic,
            payload: Arc::clone(payload),
        })
    }

    pub(crate) fn subscribed(&self, topic: Topic) -> bool {
        self.subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&topic)
    }

    /// Record subscriptions, returning the topics that were newly added (idempotence:
    /// duplicates produce no second snapshot stream).
    pub(crate) fn subscribe(&self, topics: &[Topic]) -> Vec<Topic> {
        let mut subs = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        topics
            .iter()
            .copied()
            .filter(|topic| subs.insert(*topic))
            .collect()
    }

    fn over_byte_budget(&self, max_bytes: usize) -> bool {
        self.queued_bytes.load(Ordering::Relaxed) > max_bytes
    }
}

/// Serialize one frame to a newline-terminated wire line.
fn frame_line(frame: &ServerFrame) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(frame)
        .unwrap_or_else(|_| br#"{"frame":"goodbye","reason":"shutting_down"}"#.to_vec());
    bytes.push(b'\n');
    bytes
}

/// Run one accepted session connection to completion. `conn` has already consumed its
/// Hello line; `hello` is that parsed first line.
///
/// The connection splits into this reader loop plus a writer task draining the outbound
/// queue; the two halves share the hub registration, and either side ending tears the
/// session down (registry entry removed, writer drained, socket closed).
pub(crate) async fn run_session(
    conn: Stream,
    hello: HelloRequest,
    token: &str,
    hub: Arc<RemoteSessionHub>,
    emit: Arc<dyn Fn(RemoteEvent) -> bool + Send + Sync>,
) -> io::Result<()> {
    let (read_half, mut write_half) = tokio::io::split(conn);

    // --- Handshake: validate, then ack (the ack is written directly — the writer task
    // only exists for accepted sessions).
    let reject = |reason: &str| HelloAck {
        ok: false,
        version: PROTOCOL_VERSION,
        session_id: 0,
        capabilities: Vec::new(),
        owner_mode: hub.owner_mode,
        reason: Some(reason.to_string()),
    };
    // Hello is accepted for clients that can speak our version: they offer
    // [min_version, version] and we speak PROTOCOL_VERSION (docs/gui/02 §9).
    let version_ok =
        hello.hello.min_version <= PROTOCOL_VERSION && hello.version >= PROTOCOL_VERSION;
    let ack = if hello.token != token {
        Some(reject("bad_token"))
    } else if !version_ok {
        Some(reject("bad_version"))
    } else {
        None
    };
    if let Some(ack) = ack {
        write_ack(&mut write_half, &ack).await?;
        return Ok(());
    }
    let Some((session_id, handle, mut line_rx)) = hub.register() else {
        write_ack(&mut write_half, &reject("sessions_full")).await?;
        return Ok(());
    };
    let ack = HelloAck {
        ok: true,
        version: PROTOCOL_VERSION,
        session_id,
        capabilities: hub.capabilities.clone(),
        owner_mode: hub.owner_mode,
        reason: None,
    };
    if write_ack(&mut write_half, &ack).await.is_err() {
        hub.unregister(session_id);
        return Ok(());
    }

    // --- Writer task: drains the outbound queue to the socket, splicing the tiny event
    // envelope around shared payloads at write time (serialize-once fan-out). Ends when
    // the channel closes (reader side dropped every sender) or the socket dies.
    let queued_bytes = Arc::clone(&handle.queued_bytes);
    let writer = tokio::spawn(async move {
        while let Some(line) = line_rx.recv().await {
            queued_bytes.fetch_sub(line.cost(), Ordering::Relaxed);
            let ok = match line {
                SessionLine::Raw(bytes) => write_half.write_all(&bytes).await.is_ok(),
                SessionLine::Event {
                    seq,
                    topic,
                    payload,
                } => {
                    let prefix = format!(
                        "{{\"frame\":\"event\",\"seq\":{seq},\"topic\":\"{}\",\"event\":",
                        topic.wire_str()
                    );
                    write_half.write_all(prefix.as_bytes()).await.is_ok()
                        && write_half.write_all(&payload).await.is_ok()
                        && write_half.write_all(b"}\n").await.is_ok()
                }
            };
            if !ok || write_half.flush().await.is_err() {
                break;
            }
        }
        let _ = write_half.shutdown().await;
    });

    // --- Reader loop: one ClientFrame line per iteration, idle-GC'd.
    let mut reader = BufReader::new(read_half);
    let tuning = hub.tuning;
    let close_reason = loop {
        let mut line = Vec::new();
        let read = timeout(
            tuning.idle_timeout,
            read_bounded_line(&mut reader, &mut line, SESSION_MAX_FRAME_BYTES),
        )
        .await;
        let frame: ClientFrame = match read {
            Err(_) => break CloseReason::IdleTimeout,
            Ok(Err(_)) => break CloseReason::ClientGone,
            Ok(Ok(ReadLineOutcome::TooLarge)) => break CloseReason::BadFrame,
            Ok(Ok(ReadLineOutcome::Line)) => {
                let Ok(text) = std::str::from_utf8(&line) else {
                    break CloseReason::BadFrame;
                };
                match serde_json::from_str(text.trim()) {
                    Ok(frame) => frame,
                    Err(_) => break CloseReason::BadFrame,
                }
            }
        };

        let reply = match frame.op {
            // Pings never touch the owner loop: answered right here.
            ClientOp::Ping => Some(ServerFrame::Pong { id: frame.id }),
            // Subscribe runs on the owner loop (docs/gui/02 §8): the Publisher records
            // the subscriptions, emits one initial snapshot per newly subscribed topic,
            // and only then enqueues the Reply — all into this session's queue, so the
            // snapshot-before-Reply order is structural, not raced.
            ClientOp::Subscribe { topics } => {
                if topics.len() > REMOTE_MAX_TOPICS {
                    Some(ServerFrame::Reply {
                        id: frame.id,
                        resp: RemoteResponse::err("too_many_topics"),
                    })
                } else {
                    let session = RemoteSessionRef {
                        session_id,
                        handle: Arc::clone(&handle),
                    };
                    if !emit(RemoteEvent::SessionSubscribe {
                        session,
                        frame_id: frame.id,
                        topics,
                    }) {
                        break CloseReason::ShuttingDown;
                    }
                    None
                }
            }
            ClientOp::Unsubscribe { topics } => {
                if topics.len() > REMOTE_MAX_TOPICS {
                    Some(ServerFrame::Reply {
                        id: frame.id,
                        resp: RemoteResponse::err("too_many_topics"),
                    })
                } else {
                    let mut subs = handle
                        .subscriptions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    for topic in topics {
                        subs.remove(&topic);
                    }
                    drop(subs);
                    Some(ServerFrame::Reply {
                        id: frame.id,
                        resp: RemoteResponse::ok("unsubscribed".to_string()),
                    })
                }
            }
            ClientOp::Command(command) => {
                if let Err(err) = command.validate() {
                    Some(ServerFrame::Reply {
                        id: frame.id,
                        resp: RemoteResponse::err(err.reason()),
                    })
                } else {
                    let reply_timeout = if super::reply_timeout_for(&command) > tuning.reply_timeout
                    {
                        tuning.playback_reply_timeout
                    } else {
                        tuning.reply_timeout
                    };
                    let session = RemoteSessionRef {
                        session_id,
                        handle: Arc::clone(&handle),
                    };
                    let (direct_reply, reply_done) =
                        DirectSessionReply::new(Arc::clone(&hub), session, frame.id);
                    let owner_reply = {
                        let direct_reply = direct_reply.clone();
                        RemoteReply::direct(move |response| direct_reply.complete(response))
                    };
                    if emit(RemoteEvent::Command(command, owner_reply)) {
                        if !matches!(timeout(reply_timeout, reply_done).await, Ok(Ok(()))) {
                            let _ = direct_reply.complete(RemoteResponse::err("timeout"));
                        }
                    } else {
                        let _ = direct_reply.complete(RemoteResponse::err("server_busy"));
                    }
                    None
                }
            }
        };

        if let Some(reply) = reply
            && (!handle.try_send_line(SessionLine::Raw(frame_line(&reply)))
                || handle.over_byte_budget(tuning.max_queued_bytes))
        {
            break CloseReason::SlowConsumer;
        }
    };

    // --- Teardown: best-effort goodbye through the reserved queue slot, then close the
    // outbound lane so the writer drains and exits.
    if let Some(reason) = close_reason.goodbye() {
        let _ = handle.try_send_line(SessionLine::Raw(frame_line(&ServerFrame::Goodbye {
            reason: reason.to_string(),
        })));
    }
    hub.unregister(session_id);
    drop(handle);
    let _ = writer.await;
    Ok(())
}

async fn write_ack<W: tokio::io::AsyncWrite + Unpin>(
    write: &mut W,
    ack: &HelloAck,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(ack).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    bytes.push(b'\n');
    write.write_all(&bytes).await?;
    write.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(tuning: SessionTuning) -> RemoteSessionHub {
        RemoteSessionHub::new(
            InstanceMode::StandaloneTui,
            vec!["events-v8".to_string()],
            tuning,
        )
    }

    #[test]
    fn registry_caps_sessions_and_frees_slots() {
        let hub = hub(SessionTuning::default());
        let mut held = Vec::new();
        for _ in 0..MAX_SESSIONS {
            held.push(hub.register().expect("under the cap"));
        }
        assert!(hub.register().is_none(), "9th session must be rejected");
        let (id, _, _) = held.pop().unwrap();
        hub.unregister(id);
        assert!(hub.register().is_some(), "slot frees on unregister");
        assert_eq!(hub.active(), MAX_SESSIONS);
    }

    #[test]
    fn outbound_queue_trips_on_items_and_bytes() {
        let tuning = SessionTuning {
            max_queued_items: 2,
            max_queued_bytes: 64,
            ..SessionTuning::default()
        };
        let hub = hub(tuning);
        let (_, handle, _rx) = hub.register().unwrap();

        let raw = |n: usize| SessionLine::Raw(vec![b'a'; n]);

        // Item cap: capacity is max_queued_items + 1 (goodbye reserve); the reserve slot
        // still accepts, then the queue is full.
        assert!(handle.try_send_line(raw(8)));
        assert!(handle.try_send_line(raw(8)));
        assert!(handle.try_send_line(raw(8)), "goodbye reserve slot");
        assert!(!handle.try_send_line(raw(8)), "item cap trips");

        // Byte budget: a fresh session with a fat line crosses max_queued_bytes and the
        // caller-visible check reports it even though the item queue accepted it.
        let (_, fat, _rx2) = hub.register().unwrap();
        assert!(fat.try_send_line(raw(65)));
        assert!(fat.over_byte_budget(tuning.max_queued_bytes));
    }

    #[test]
    fn broadcast_reaches_only_subscribers_with_per_session_seq_and_evicts_overflow() {
        let tuning = SessionTuning {
            max_queued_items: 2,
            max_queued_bytes: 1024,
            ..SessionTuning::default()
        };
        let hub = hub(tuning);
        let (_, sub, mut sub_rx) = hub.register().unwrap();
        let (_, other, mut other_rx) = hub.register().unwrap();
        assert_eq!(sub.subscribe(&[Topic::Player]), vec![Topic::Player]);
        assert!(sub.subscribe(&[Topic::Player]).is_empty(), "idempotent");
        other.subscribe(&[Topic::Queue]);

        let payload = Arc::new(br#"{"kind":"shutting_down"}"#.to_vec());
        // Fill without draining: capacity is max_items+1 (best-effort goodbye reserve),
        // so pushes 1–3 land, push 4 trips and evicts the subscriber.
        for _ in 0..4 {
            hub.broadcast(Topic::Player, &payload);
        }
        assert_eq!(hub.active(), 1, "overflowing subscriber evicted");
        assert!(other_rx.try_recv().is_err(), "non-subscriber got nothing");
        assert!(
            !sub.try_send_line(SessionLine::Raw(vec![b'x'])),
            "evicted sessions accept nothing"
        );
        for want_seq in 1..=3u64 {
            match sub_rx.try_recv().expect("queued events survive eviction") {
                SessionLine::Event { seq, topic, .. } => {
                    assert_eq!(seq, want_seq, "per-session monotonic");
                    assert_eq!(topic, Topic::Player);
                }
                SessionLine::Raw(_) => panic!("expected event"),
            }
        }
    }
}
