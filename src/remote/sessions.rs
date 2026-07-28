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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use interprocess::local_socket::tokio::Stream;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

use super::proto::{
    ClientFrame, ClientOp, HelloAck, HelloRequest, InstanceMode, PROTOCOL_VERSION,
    REMOTE_MAX_TOPICS, RemoteResponse, ServerFrame, Topic,
};
use super::requests::{CommandDeduper, RequestKey};
use super::server::{ReadLineOutcome, RemoteEvent, RemoteReply, read_bounded_line};
use super::{WireSettlement, WireSettlements};

mod writer;
pub(crate) use writer::SessionLine;
use writer::{OutboundBudget, WriterTask, frame_line, run_session_writer};

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
    /// How long a personal-data export may wait on filesystem work.
    pub personal_export_reply_timeout: Duration,
    /// How long a manual WebDAV merge may wait on its bounded network phase.
    pub manual_sync_reply_timeout: Duration,
    /// Upper bound for one socket frame write/flush and for writer teardown.
    pub write_timeout: Duration,
    /// Deterministic fault injection: hold an accepted response before its actual socket write.
    #[cfg(test)]
    pub wire_write_delay: Duration,
}

impl Default for SessionTuning {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            max_queued_items: 256,
            max_queued_bytes: 8 * 1024 * 1024,
            reply_timeout: Duration::from_secs(2),
            playback_reply_timeout: Duration::from_secs(20),
            personal_export_reply_timeout: Duration::from_secs(5 * 60),
            manual_sync_reply_timeout: Duration::from_secs(10 * 60),
            write_timeout: Duration::from_secs(2),
            #[cfg(test)]
            wire_write_delay: Duration::ZERO,
        }
    }
}

impl SessionTuning {
    fn command_reply_timeout(self, command: &super::proto::RemoteCommand) -> Duration {
        use super::proto::RemoteCommand;

        match command {
            RemoteCommand::ExportPersonalData { .. } => self.personal_export_reply_timeout,
            RemoteCommand::SyncNow | RemoteCommand::SyncRevokeDevice { .. } => {
                self.manual_sync_reply_timeout
            }
            RemoteCommand::Next
            | RemoteCommand::Prev
            | RemoteCommand::TogglePause
            | RemoteCommand::Play { .. }
            | RemoteCommand::Enqueue { .. }
            | RemoteCommand::QueuePlay { .. }
            | RemoteCommand::QueueRemove { .. }
            | RemoteCommand::QueuePlayIfRevision { .. }
            | RemoteCommand::QueueRemoveIfRevision { .. }
            | RemoteCommand::ResumeSession
            | RemoteCommand::PlayTracks { .. }
            | RemoteCommand::EnqueueTracks { .. } => self.playback_reply_timeout,
            _ => self.reply_timeout,
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

/// First-writer-wins cancellation shared by the connection reader and socket writer. A hub-side
/// eviction must wake both halves even when the peer never reads or sends another byte.
#[derive(Default)]
struct SessionClose {
    closed: std::sync::atomic::AtomicBool,
    reason: Mutex<Option<CloseReason>>,
    notify: Notify,
}

impl SessionClose {
    fn request(&self, reason: CloseReason) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
        self.notify.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    async fn cancelled(&self) -> CloseReason {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(reason) = *self.reason.lock().unwrap_or_else(|e| e.into_inner()) {
                return reason;
            }
            notified.await;
        }
    }
}

/// One registered session as the hub sees it.
pub(crate) struct RemoteSessionHandle {
    line_tx: mpsc::Sender<SessionLine>,
    budget: Arc<OutboundBudget>,
    subscriptions: Mutex<std::collections::HashSet<Topic>>,
    current_page: Mutex<Option<String>>,
    subscribe_admission: Mutex<SubscribeAdmission>,
    /// Per-session monotonic event sequence (docs/gui/02 §6).
    seq: AtomicU64,
    /// Wakes both socket halves immediately on eviction/shutdown.
    close: Arc<SessionClose>,
}

#[derive(Clone, Default)]
struct SubscribeAdmission {
    revision: u64,
    page_id: Option<String>,
}

struct SessionRegistry {
    sessions: HashMap<u64, Arc<RemoteSessionHandle>>,
    owner_admission_open: bool,
    shutting_down: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisterError {
    SessionsFull,
    ShuttingDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscribeIngress {
    Accepted,
    Busy,
    ShuttingDown,
    StalePage,
}

/// A host-visible reference to one live session, carried inside
/// [`RemoteEvent::SessionSubscribe`] so the owner loop (the Publisher) can emit the
/// initial snapshots and the reply into exactly this session's outbound queue.
#[derive(Clone)]
pub struct RemoteSessionRef {
    pub(crate) session_id: u64,
    pub(crate) handle: Arc<RemoteSessionHandle>,
}

/// Identity and live routing handle for one session-originated command. `page_id` is optional
/// only for compatibility with shipped protocol-v8 clients that predate page generations.
#[derive(Clone, Debug)]
pub struct RemoteSessionScope {
    session: Option<RemoteSessionRef>,
    session_id: u64,
    page_id: Option<String>,
}

impl RemoteSessionScope {
    pub(crate) fn new(session: RemoteSessionRef, page_id: Option<String>) -> Self {
        Self {
            session_id: session.session_id,
            session: Some(session),
            page_id,
        }
    }

    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    pub fn page_id(&self) -> Option<&str> {
        self.page_id.as_deref()
    }

    pub(crate) fn session(&self) -> Option<&RemoteSessionRef> {
        self.session.as_ref()
    }

    pub(crate) fn is_live(&self) -> bool {
        self.session
            .as_ref()
            .is_none_or(|session| !session.handle.close.is_closed())
    }

    #[cfg(test)]
    pub(crate) fn for_test(session_id: u64, page_id: Option<&str>) -> Self {
        Self {
            session: None,
            session_id,
            page_id: page_id.map(str::to_owned),
        }
    }
}

impl std::fmt::Debug for RemoteSessionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSessionRef")
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl RemoteSessionRef {
    /// Commit a page transition only for an event accepted by the owner ingress. A rejected
    /// event never exposes the desired page, so `server_busy` cannot advance the command generation.
    pub(crate) fn admit_subscribe<F>(&self, page_id: Option<&str>, emit: F) -> SubscribeIngress
    where
        F: FnOnce() -> Option<bool>,
    {
        self.handle.admit_subscribe(page_id, emit)
    }

    pub(crate) fn apply_subscribe<R, F>(
        &self,
        page_id: Option<&str>,
        topics: &[Topic],
        apply: F,
    ) -> Option<R>
    where
        F: FnOnce(&[Topic]) -> R,
    {
        self.handle.apply_subscribe(page_id, topics, apply)
    }

    pub(crate) fn unsubscribe_if_current(&self, page_id: Option<&str>, topics: &[Topic]) -> bool {
        self.handle.unsubscribe_if_current(page_id, topics)
    }

    #[cfg(test)]
    pub(crate) fn close_for_test(&self) {
        self.handle.request_close(CloseReason::ClientGone);
    }
}

/// Exactly-once persistent-session reply shared by request retention and the owner loop.
///
/// The owner completes this through [`RemoteReply::send`]. Completion first claims the response,
/// then synchronously enqueues the reply frame, and only then completes the retained outcome. A
/// timeout or replay races through the same gate, so a late owner cannot emit a duplicate reply.
#[derive(Clone)]
struct DirectSessionReply {
    inner: Arc<DirectSessionReplyInner>,
}

struct DirectSessionReplyInner {
    hub: Arc<RemoteSessionHub>,
    session: RemoteSessionRef,
    frame_id: u64,
    completed: std::sync::atomic::AtomicBool,
    settlement: Mutex<Option<WireSettlement>>,
}

impl DirectSessionReply {
    fn new(
        hub: Arc<RemoteSessionHub>,
        session: RemoteSessionRef,
        frame_id: u64,
        settlement: Option<WireSettlement>,
    ) -> Self {
        Self {
            inner: Arc::new(DirectSessionReplyInner {
                hub,
                session,
                frame_id,
                completed: std::sync::atomic::AtomicBool::new(false),
                settlement: Mutex::new(settlement),
            }),
        }
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
        let settlement = self
            .inner
            .settlement
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match settlement {
            Some(settlement) => {
                self.inner
                    .hub
                    .send_tracked_raw_to(&self.inner.session, &frame, settlement)
            }
            None => self.inner.hub.send_raw_to(&self.inner.session, &frame),
        }
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
/// real v8 session. Owner ordering tests inspect the outbound session lane itself.
#[cfg(test)]
pub(crate) fn test_command_reply(
    hub: Arc<RemoteSessionHub>,
    session: RemoteSessionRef,
    frame_id: u64,
) -> RemoteReply {
    let direct_reply = DirectSessionReply::new(hub, session, frame_id, None);
    RemoteReply::direct(move |response| direct_reply.complete(response))
}

#[cfg(test)]
pub(crate) fn test_register_next(
    hub: &Arc<RemoteSessionHub>,
) -> (RemoteSessionRef, mpsc::Receiver<SessionLine>) {
    let (session_id, handle, rx) = hub.register().expect("test hub has room");
    (RemoteSessionRef { session_id, handle }, rx)
}

/// The per-owner session registry. The server's accept path registers sessions here;
/// the Publisher fans events out through [`broadcast`](Self::broadcast).
pub struct RemoteSessionHub {
    next_id: AtomicU64,
    registry: Mutex<SessionRegistry>,
    /// Wakes the accept owner without relying on the bounded remote event lane. The registry
    /// boolean remains the monotonic source of truth; this notification only makes waiting
    /// prompt.
    shutdown_notify: Notify,
    /// Wakes the accept owner when producer admission closes without cancelling established
    /// sessions; those writers remain alive through the wire-settlement barrier.
    quiesce_notify: Notify,
    settlements: WireSettlements,
    pub(crate) requests: CommandDeduper,
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
            registry: Mutex::new(SessionRegistry {
                sessions: HashMap::new(),
                owner_admission_open: true,
                shutting_down: false,
            }),
            shutdown_notify: Notify::new(),
            quiesce_notify: Notify::new(),
            settlements: WireSettlements::default(),
            requests: CommandDeduper::default(),
            tuning,
            owner_mode,
            capabilities,
        }
    }

    fn register(
        &self,
    ) -> Result<(u64, Arc<RemoteSessionHandle>, mpsc::Receiver<SessionLine>), RegisterError> {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if registry.shutting_down || !registry.owner_admission_open {
            return Err(RegisterError::ShuttingDown);
        }
        if registry.sessions.len() >= MAX_SESSIONS {
            return Err(RegisterError::SessionsFull);
        }
        let (line_tx, line_rx) = mpsc::channel(self.tuning.max_queued_items.max(1));
        let handle = Arc::new(RemoteSessionHandle {
            line_tx,
            budget: Arc::new(OutboundBudget::new(
                self.tuning.max_queued_items,
                self.tuning.max_queued_bytes,
            )),
            subscriptions: Mutex::new(std::collections::HashSet::new()),
            current_page: Mutex::new(None),
            subscribe_admission: Mutex::new(SubscribeAdmission::default()),
            seq: AtomicU64::new(0),
            close: Arc::new(SessionClose::default()),
        });
        // Session identity is retained by bounded value-only indexes after a socket closes.
        // Never wrap the allocator: reusing `0` after `u64::MAX` could make a stale key alias a
        // later session. Exhaustion maps onto the existing admission-safe `sessions_full` path.
        let id = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| RegisterError::SessionsFull)?;
        registry.sessions.insert(id, Arc::clone(&handle));
        Ok((id, handle, line_rx))
    }

    fn unregister(&self, id: u64) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .remove(&id);
    }

    /// Whether any live session subscribes to `topic` — lets the Publisher skip building
    /// a model nobody would receive.
    pub(crate) fn any_subscribed(&self, topic: Topic) -> bool {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .values()
            .any(|handle| handle.subscribed(topic))
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutting_down
    }

    pub(crate) fn owner_admission_is_open(&self) -> bool {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        registry.owner_admission_open && !registry.shutting_down
    }

    /// Monotonically stop new connections and owner events while preserving established socket
    /// tasks. Token creation uses this same registry lock, so no settlement can begin after this
    /// method returns.
    pub(crate) fn quiesce_owner_admission(&self) -> bool {
        let changed = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let changed = registry.owner_admission_open;
            registry.owner_admission_open = false;
            changed
        };
        if changed {
            self.quiesce_notify.notify_waiters();
        }
        changed
    }

    pub(crate) async fn wait_for_owner_quiesce(&self) {
        loop {
            let notified = self.quiesce_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.owner_admission_is_open() {
                return;
            }
            notified.await;
        }
    }

    /// Wait for the monotonic shutdown latch without losing a notification between the state
    /// check and waiter registration.
    pub(crate) async fn wait_for_shutdown(&self) {
        loop {
            let notified = self.shutdown_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_shutting_down() {
                return;
            }
            notified.await;
        }
    }

    /// Linearize a non-blocking action against the shutdown latch. Holding the registry lock
    /// means the action either happens before shutdown begins or is rejected after the latch.
    #[cfg(test)]
    pub(crate) fn run_if_running<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        (!registry.shutting_down).then(action)
    }

    pub(crate) fn run_if_owner_admission_open<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        (registry.owner_admission_open && !registry.shutting_down).then(action)
    }

    /// Linearize one owner-event attempt and its wire token against quiesce. The action either
    /// returns a token created under this lock or observes the closed admission frontier.
    pub(crate) fn admit_tracked<T>(&self, action: impl FnOnce(WireSettlement) -> T) -> Option<T> {
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if registry.shutting_down || !registry.owner_admission_open {
            return None;
        }
        Some(action(self.settlements.begin()))
    }

    /// Admit an untracked owner event only while the pre-shutdown frontier is open. Kept for
    /// tests and validation-only paths; requests with wire replies use `admit_tracked`.
    #[cfg(test)]
    pub(crate) fn emit_if_running(&self, emit: impl FnOnce() -> bool) -> bool {
        self.run_if_owner_admission_open(emit).unwrap_or(false)
    }

    pub(crate) async fn wait_for_wire_settlements(&self) -> bool {
        let budget = self
            .tuning
            .write_timeout
            .max(super::RESPONSE_WRITE_TIMEOUT)
            .saturating_add(Duration::from_millis(250));
        let settled = self.settlements.wait_for_idle(budget).await;
        if !settled {
            let active = self.settlements.active();
            tracing::warn!(
                active,
                ?budget,
                "remote shutdown timed out waiting for accepted replies to flush"
            );
        }
        settled
    }

    #[cfg(test)]
    pub(crate) fn active_wire_settlements(&self) -> usize {
        self.settlements.active()
    }

    #[cfg(test)]
    pub(crate) fn wire_write_delay(&self) -> Duration {
        self.tuning.wire_write_delay
    }

    /// Fan one pre-serialized push payload out to every session subscribed to `topic`.
    /// A session whose outbound budget trips is evicted with `slow_consumer` — the
    /// owner loop never blocks on a wedged client.
    pub(crate) fn broadcast(&self, topic: Topic, payload: &Arc<Vec<u8>>) {
        let subscribed: Vec<(u64, Arc<RemoteSessionHandle>)> = {
            let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            registry
                .sessions
                .iter()
                .filter(|(_, handle)| handle.subscribed(topic))
                .map(|(id, handle)| (*id, Arc::clone(handle)))
                .collect()
        };
        for (id, handle) in subscribed {
            if !handle.push_event(topic, payload) {
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
        if session.handle.push_event(topic, payload) {
            true
        } else {
            self.evict(session.session_id, &session.handle);
            false
        }
    }

    /// Target a live session only while it still owns the topic subscription. A stale
    /// `RemoteSessionRef` retained by asynchronous work cannot publish into a replacement.
    pub(crate) fn send_event_to_subscriber(
        &self,
        session: &RemoteSessionRef,
        page_id: Option<&str>,
        topic: Topic,
        payload: &Arc<Vec<u8>>,
    ) -> bool {
        let live = self
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .get(&session.session_id)
            .is_some_and(|handle| Arc::ptr_eq(handle, &session.handle));
        if !live {
            return false;
        }
        match session
            .handle
            .push_event_to_subscriber(page_id, topic, payload)
        {
            None => false,
            Some(true) => true,
            Some(false) => {
                self.evict(session.session_id, &session.handle);
                false
            }
        }
    }

    /// Send one raw frame to a single session (replies). Returns `false` after evicting.
    pub(crate) fn send_raw_to(&self, session: &RemoteSessionRef, frame: &ServerFrame) -> bool {
        if session
            .handle
            .try_send_line(SessionLine::Raw(frame_line(frame)))
        {
            true
        } else {
            self.evict(session.session_id, &session.handle);
            false
        }
    }

    /// Enqueue the final frame for one owner-accepted request. The settlement token stays inside
    /// the FIFO item until the writer flushes it (or proves the peer/write is gone).
    pub(crate) fn send_tracked_raw_to(
        &self,
        session: &RemoteSessionRef,
        frame: &ServerFrame,
        settlement: WireSettlement,
    ) -> bool {
        if session.handle.try_send_line(SessionLine::TrackedRaw {
            bytes: frame_line(frame),
            _settlement: settlement,
        }) {
            true
        } else {
            self.evict(session.session_id, &session.handle);
            false
        }
    }

    /// Broadcast a goodbye to every live session and drop them — the owner is exiting.
    /// Best-effort: full queues just close without the goodbye.
    pub(crate) fn shutdown_all(&self) {
        let (all, first_shutdown): (Vec<Arc<RemoteSessionHandle>>, bool) = {
            let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let first_shutdown = !registry.shutting_down;
            registry.owner_admission_open = false;
            registry.shutting_down = true;
            let drained = registry.sessions.values().cloned().collect();
            registry.sessions.clear();
            (drained, first_shutdown)
        };
        for handle in all {
            handle.request_close(CloseReason::ShuttingDown);
        }
        if first_shutdown {
            self.quiesce_notify.notify_waiters();
            self.shutdown_notify.notify_waiters();
        }
    }

    /// Evict one session and remove it from the registry. Cancellation wakes a blocked reader
    /// and interrupts a blocked writer; the writer sends the goodbye directly within its own
    /// deadline, outside the user-frame queue.
    fn evict(&self, id: u64, handle: &RemoteSessionHandle) {
        handle.request_close(CloseReason::SlowConsumer);
        self.unregister(id);
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> usize {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .len()
    }
}

impl RemoteSessionHandle {
    /// Enqueue one outbound line. `false` means the session was evicted, the item
    /// budget tripped, or the writer is gone — the caller must treat it as dead.
    fn try_send_line(&self, line: SessionLine) -> bool {
        self.budget.try_send(&self.line_tx, &self.close, line)
    }

    /// Enqueue one push event with the next per-session `seq`; `false` on budget trip.
    fn push_event(&self, topic: Topic, payload: &Arc<Vec<u8>>) -> bool {
        let Ok(previous) =
            self.seq
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |sequence| {
                    sequence.checked_add(1)
                })
        else {
            return false;
        };
        let seq = previous + 1;
        let prefix = format!(
            "{{\"frame\":\"event\",\"seq\":{seq},\"topic\":\"{}\",\"event\":",
            topic.wire_str()
        )
        .into_bytes();
        self.try_send_line(SessionLine::Event {
            #[cfg(test)]
            seq,
            #[cfg(test)]
            topic,
            prefix,
            payload: Arc::clone(payload),
        })
    }

    /// Check the topic/page generations and enqueue while holding both generation locks. This
    /// prevents a reader-thread page replacement or unsubscribe from racing between validation
    /// and the targeted push. `None` is a generation mismatch; `Some(false)` is overflow/close.
    fn push_event_to_subscriber(
        &self,
        page_id: Option<&str>,
        topic: Topic,
        payload: &Arc<Vec<u8>>,
    ) -> Option<bool> {
        let subscriptions = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        if !subscriptions.contains(&topic) {
            return None;
        }
        let current_page = self.current_page.lock().unwrap_or_else(|e| e.into_inner());
        if current_page.as_deref() != page_id {
            return None;
        }
        let admission = self
            .subscribe_admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if admission.page_id.as_deref() != page_id {
            return None;
        }
        Some(self.push_event(topic, payload))
    }

    fn request_close(&self, reason: CloseReason) {
        self.budget.request_close(&self.close, reason);
    }

    pub(crate) fn subscribed(&self, topic: Topic) -> bool {
        self.subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&topic)
    }

    #[cfg(test)]
    pub(crate) fn subscribe(&self, topics: &[Topic]) -> Vec<Topic> {
        let mut subscriptions = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        topics
            .iter()
            .copied()
            .filter(|topic| subscriptions.insert(*topic))
            .collect()
    }

    fn admit_subscribe<F>(&self, page_id: Option<&str>, emit: F) -> SubscribeIngress
    where
        F: FnOnce() -> Option<bool>,
    {
        // The event sink is a synchronous, non-blocking ingress attempt. Keep the admission lock
        // through that linearization point: an owner concurrently applying an older accepted
        // event must not observe a provisional page that a saturated newer send will roll back.
        let mut admission = self
            .subscribe_admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if page_id.is_none() && admission.page_id.is_some() {
            return SubscribeIngress::StalePage;
        }
        let Some(revision) = admission.revision.checked_add(1) else {
            return SubscribeIngress::Busy;
        };
        let result = emit();
        if result == Some(true) {
            admission.revision = revision;
            if let Some(page_id) = page_id {
                admission.page_id = Some(page_id.to_owned());
            }
        }
        match result {
            Some(true) => SubscribeIngress::Accepted,
            Some(false) => SubscribeIngress::Busy,
            None => SubscribeIngress::ShuttingDown,
        }
    }

    /// Apply a subscribe event as one page-generation transaction. The lock order matches
    /// targeted pushes: subscriptions, then applied page, then admitted page. Holding all three
    /// through the callback makes page validation, topic insertion, initial snapshots, and the
    /// final reply indivisible with respect to a newer page admission or unsubscribe.
    fn apply_subscribe<R, F>(&self, page_id: Option<&str>, topics: &[Topic], apply: F) -> Option<R>
    where
        F: FnOnce(&[Topic]) -> R,
    {
        let mut subscriptions = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        let mut current_page = self.current_page.lock().unwrap_or_else(|e| e.into_inner());
        let admission = self
            .subscribe_admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if admission.page_id.as_deref() != page_id {
            return None;
        }
        match page_id {
            Some(page_id) if current_page.as_deref() != Some(page_id) => {
                subscriptions.clear();
                *current_page = Some(page_id.to_owned());
            }
            None if current_page.is_some() => return None,
            _ => {}
        }
        let newly_added = topics
            .iter()
            .copied()
            .filter(|topic| subscriptions.insert(*topic))
            .collect::<Vec<_>>();
        Some(apply(&newly_added))
    }

    fn unsubscribe_if_current(&self, page_id: Option<&str>, topics: &[Topic]) -> bool {
        let mut subscriptions = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        let current_page = self.current_page.lock().unwrap_or_else(|e| e.into_inner());
        let admission = self
            .subscribe_admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if current_page.as_deref() != page_id || admission.page_id.as_deref() != page_id {
            return false;
        }
        for topic in topics {
            subscriptions.remove(topic);
        }
        true
    }

    fn page_is_current(&self, page_id: Option<&str>) -> bool {
        self.subscribe_admission
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .page_id
            .as_deref()
            == page_id
    }
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
    let tuning = hub.tuning;

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
        write_ack(&mut write_half, &ack, tuning.write_timeout).await?;
        return Ok(());
    }
    let (session_id, handle, line_rx) = match hub.register() {
        Ok(registered) => registered,
        Err(RegisterError::SessionsFull) => {
            write_ack(
                &mut write_half,
                &reject("sessions_full"),
                tuning.write_timeout,
            )
            .await?;
            return Ok(());
        }
        Err(RegisterError::ShuttingDown) => {
            write_ack(
                &mut write_half,
                &reject("shutting_down"),
                tuning.write_timeout,
            )
            .await?;
            return Ok(());
        }
    };
    let ack = HelloAck {
        ok: true,
        version: PROTOCOL_VERSION,
        session_id,
        capabilities: hub.capabilities.clone(),
        owner_mode: hub.owner_mode,
        reason: None,
    };
    if write_ack(&mut write_half, &ack, tuning.write_timeout)
        .await
        .is_err()
    {
        handle.request_close(CloseReason::ClientGone);
        hub.unregister(session_id);
        return Ok(());
    }

    // --- Writer task: drains the outbound queue to the socket, splicing the tiny event
    // envelope around shared payloads at write time (serialize-once fan-out). Every write and
    // flush is deadline-bounded, and hub cancellation interrupts an in-flight write.
    let budget = Arc::clone(&handle.budget);
    let writer_close = Arc::clone(&handle.close);
    let mut writer = WriterTask {
        handle: tokio::spawn(run_session_writer(
            write_half,
            line_rx,
            budget,
            writer_close,
            tuning.write_timeout,
            #[cfg(test)]
            tuning.wire_write_delay,
        )),
    };

    // --- Reader loop: one ClientFrame line per iteration, idle-GC'd.
    let mut reader = BufReader::new(read_half);
    let close_reason = loop {
        // Quiesce stops new owner work but must not close this writer: it may still own a tracked
        // reply accepted on the preceding turn. Full hub shutdown wakes this wait after the wire
        // barrier has observed that reply's flush.
        if !hub.owner_admission_is_open() {
            hub.wait_for_shutdown().await;
            break CloseReason::ShuttingDown;
        }
        let mut line = Vec::new();
        let read = tokio::select! {
            biased;
            reason = handle.close.cancelled() => break reason,
            read = timeout(
                tuning.idle_timeout,
                read_bounded_line(&mut reader, &mut line, SESSION_MAX_FRAME_BYTES),
            ) => read,
        };
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
        if hub.is_shutting_down() {
            break CloseReason::ShuttingDown;
        }

        let ClientFrame {
            id: frame_id,
            request_id,
            page_id,
            op,
        } = frame;
        let reply = match op {
            // Pings never touch the owner loop: answered right here.
            ClientOp::Ping => Some(ServerFrame::Pong { id: frame_id }),
            // Subscribe runs on the owner loop (docs/gui/02 §8): the Publisher records
            // the subscriptions, emits one initial snapshot per newly subscribed topic,
            // and only then enqueues the Reply — all into this session's queue, so the
            // snapshot-before-Reply order is structural, not raced.
            ClientOp::Subscribe { topics } => {
                if page_id
                    .as_deref()
                    .is_some_and(|page_id| !super::requests::valid_page_id(page_id))
                {
                    Some(ServerFrame::Reply {
                        id: frame_id,
                        resp: RemoteResponse::err("bad_page_id"),
                    })
                } else if topics.len() > REMOTE_MAX_TOPICS {
                    Some(ServerFrame::Reply {
                        id: frame_id,
                        resp: RemoteResponse::err("too_many_topics"),
                    })
                } else {
                    let session = RemoteSessionRef {
                        session_id,
                        handle: Arc::clone(&handle),
                    };
                    let event_page_id = page_id.clone();
                    match session.admit_subscribe(page_id.as_deref(), || {
                        hub.admit_tracked(|settlement| {
                            emit(RemoteEvent::SessionSubscribe {
                                session: session.clone(),
                                frame_id,
                                page_id: event_page_id,
                                topics,
                                settlement,
                            })
                        })
                    }) {
                        SubscribeIngress::Accepted => None,
                        SubscribeIngress::Busy => Some(ServerFrame::Reply {
                            id: frame_id,
                            resp: RemoteResponse::err("server_busy"),
                        }),
                        SubscribeIngress::StalePage => Some(ServerFrame::Reply {
                            id: frame_id,
                            resp: RemoteResponse::err("stale_page"),
                        }),
                        SubscribeIngress::ShuttingDown => Some(ServerFrame::Reply {
                            id: frame_id,
                            resp: RemoteResponse::err("shutting_down"),
                        }),
                    }
                }
            }
            ClientOp::Unsubscribe { topics } => {
                if page_id
                    .as_deref()
                    .is_some_and(|page_id| !super::requests::valid_page_id(page_id))
                {
                    Some(ServerFrame::Reply {
                        id: frame_id,
                        resp: RemoteResponse::err("bad_page_id"),
                    })
                } else if topics.len() > REMOTE_MAX_TOPICS {
                    Some(ServerFrame::Reply {
                        id: frame_id,
                        resp: RemoteResponse::err("too_many_topics"),
                    })
                } else {
                    let session = RemoteSessionRef {
                        session_id,
                        handle: Arc::clone(&handle),
                    };
                    if session.unsubscribe_if_current(page_id.as_deref(), &topics) {
                        Some(ServerFrame::Reply {
                            id: frame_id,
                            resp: RemoteResponse::ok("unsubscribed".to_string()),
                        })
                    } else {
                        Some(ServerFrame::Reply {
                            id: frame_id,
                            resp: RemoteResponse::err("stale_page"),
                        })
                    }
                }
            }
            ClientOp::Command(command) => {
                if page_id
                    .as_deref()
                    .is_some_and(|page_id| !super::requests::valid_page_id(page_id))
                {
                    Some(ServerFrame::Reply {
                        id: frame_id,
                        resp: RemoteResponse::err("bad_page_id"),
                    })
                } else if !handle.page_is_current(page_id.as_deref()) {
                    Some(ServerFrame::Reply {
                        id: frame_id,
                        resp: RemoteResponse::err("stale_page"),
                    })
                } else if let Err(err) = command.validate() {
                    Some(ServerFrame::Reply {
                        id: frame_id,
                        resp: RemoteResponse::err(err.reason()),
                    })
                } else {
                    let origin = RemoteSessionScope::new(
                        RemoteSessionRef {
                            session_id,
                            handle: Arc::clone(&handle),
                        },
                        page_id,
                    );
                    let reply_timeout = tuning.command_reply_timeout(&command);
                    let session = RemoteSessionRef {
                        session_id,
                        handle: Arc::clone(&handle),
                    };
                    let request_key = if let Some(request_id) = request_id {
                        RequestKey::stable(request_id)
                    } else {
                        Some(RequestKey::Session {
                            session_id,
                            frame_id,
                        })
                    };
                    if let Some(request_key) = request_key {
                        if let Some(settlement) = hub.admit_tracked(std::convert::identity) {
                            let direct_reply = DirectSessionReply::new(
                                Arc::clone(&hub),
                                session,
                                frame_id,
                                Some(settlement),
                            );
                            let owner_command = command.clone();
                            let owner_origin = origin.clone();
                            let hub_for_admission = Arc::clone(&hub);
                            let emit_for_admission = Arc::clone(&emit);
                            let direct_for_owner = direct_reply.clone();
                            let execution = hub.requests.execute_with_replay_proof(
                                request_key,
                                &command,
                                reply_timeout,
                                move |retained_reply| {
                                    hub_for_admission
                                        .run_if_owner_admission_open(|| {
                                            let direct = direct_for_owner.clone();
                                            let reply = RemoteReply::direct(move |response| {
                                                let sent = direct.complete(response.clone());
                                                let retained =
                                                    retained_reply.send(response).is_ok();
                                                sent || retained
                                            });
                                            emit_for_admission(RemoteEvent::SessionCommand {
                                                command: owner_command,
                                                origin: owner_origin,
                                                reply,
                                            })
                                        })
                                        .unwrap_or(false)
                                },
                            );
                            tokio::pin!(execution);
                            let outcome = tokio::select! {
                                biased;
                                reason = handle.close.cancelled() => break reason,
                                outcome = &mut execution => outcome,
                            };
                            let _ = direct_reply.complete(outcome.response);
                        } else {
                            let _ =
                                DirectSessionReply::new(Arc::clone(&hub), session, frame_id, None)
                                    .complete(RemoteResponse::err("shutting_down"));
                        }
                    } else {
                        let _ = DirectSessionReply::new(Arc::clone(&hub), session, frame_id, None)
                            .complete(RemoteResponse::err("bad_request_id"));
                    }
                    None
                }
            }
        };

        if let Some(reply) = reply {
            let line = SessionLine::Raw(frame_line(&reply));
            if !handle.try_send_line(line) {
                break CloseReason::SlowConsumer;
            }
        }
    };

    // --- Teardown: signal both socket halves. The writer emits a best-effort goodbye directly,
    // then exits within the write deadline even if this peer never reads.
    handle.request_close(close_reason);
    hub.unregister(session_id);
    drop(handle);
    if timeout(tuning.write_timeout.saturating_mul(3), &mut writer.handle)
        .await
        .is_err()
    {
        writer.handle.abort();
        let _ = (&mut writer.handle).await;
    }
    Ok(())
}

async fn write_ack<W: tokio::io::AsyncWrite + Unpin>(
    write: &mut W,
    ack: &HelloAck,
    write_timeout: Duration,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(ack).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    bytes.push(b'\n');
    match timeout(write_timeout, async {
        write.write_all(&bytes).await?;
        write.flush().await
    })
    .await
    {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "remote Hello Ack write timed out",
        )),
    }
}

#[cfg(test)]
#[path = "sessions/tests.rs"]
mod tests;
