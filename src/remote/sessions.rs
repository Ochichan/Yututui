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
//! Subscribe/Unsubscribe currently record the subscription set and reply `ok`; the
//! initial-snapshot-before-Reply emission arrives with the Publisher (B0 slice 5) —
//! until then the owner does not advertise the `events-v8` capability, so no shipping
//! client attempts a session.

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
    ClientFrame, ClientOp, HelloAck, HelloRequest, InstanceMode, PROTOCOL_VERSION, RemoteResponse,
    ServerFrame, Topic,
};
use super::server::{ReadLineOutcome, RemoteEvent, read_bounded_line};

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
}

impl Default for SessionTuning {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(60),
            max_queued_items: 256,
            max_queued_bytes: 8 * 1024 * 1024,
            reply_timeout: Duration::from_secs(2),
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
}

impl CloseReason {
    fn goodbye(self) -> Option<&'static str> {
        match self {
            CloseReason::SlowConsumer => Some("slow_consumer"),
            CloseReason::IdleTimeout => Some("idle_timeout"),
            CloseReason::BadFrame => Some("bad_request"),
            CloseReason::ClientGone => None,
        }
    }
}

/// One registered session as the hub sees it.
struct RemoteSessionHandle {
    line_tx: mpsc::Sender<Vec<u8>>,
    queued_bytes: Arc<AtomicUsize>,
    subscriptions: Mutex<std::collections::HashSet<Topic>>,
}

/// The per-owner session registry. The server's accept path registers sessions here;
/// the Publisher (B0 slice 5) fans events out through it.
pub(crate) struct RemoteSessionHub {
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

    fn register(&self) -> Option<(u64, Arc<RemoteSessionHandle>, mpsc::Receiver<Vec<u8>>)> {
        let mut sessions = self.sessions.lock().expect("session registry poisoned");
        if sessions.len() >= MAX_SESSIONS {
            return None;
        }
        // +1 so the goodbye line always has a slot even when the data queue is full.
        let (line_tx, line_rx) = mpsc::channel(self.tuning.max_queued_items + 1);
        let handle = Arc::new(RemoteSessionHandle {
            line_tx,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            subscriptions: Mutex::new(std::collections::HashSet::new()),
        });
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        sessions.insert(id, Arc::clone(&handle));
        Some((id, handle, line_rx))
    }

    fn unregister(&self, id: u64) {
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .remove(&id);
    }

    #[cfg(test)]
    fn active(&self) -> usize {
        self.sessions
            .lock()
            .expect("session registry poisoned")
            .len()
    }
}

impl RemoteSessionHandle {
    /// Enqueue one serialized frame line. `false` means the byte/item budget tripped —
    /// the caller must evict the session (`slow_consumer`).
    fn try_send_line(&self, line: Vec<u8>) -> bool {
        // Budget check first so a huge backlog of bytes trips even under the item cap.
        // (Ordering is Relaxed: the counter is advisory backpressure, not a lock.)
        self.queued_bytes.fetch_add(line.len(), Ordering::Relaxed);
        match self.line_tx.try_send(line) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(line))
            | Err(mpsc::error::TrySendError::Closed(line)) => {
                self.queued_bytes.fetch_sub(line.len(), Ordering::Relaxed);
                false
            }
        }
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

    // --- Writer task: drains the outbound queue to the socket. Ends when the channel
    // closes (reader side dropped every sender) or the socket dies.
    let queued_bytes = Arc::clone(&handle.queued_bytes);
    let writer = tokio::spawn(async move {
        while let Some(line) = line_rx.recv().await {
            queued_bytes.fetch_sub(line.len(), Ordering::Relaxed);
            if write_half.write_all(&line).await.is_err() {
                break;
            }
            if write_half.flush().await.is_err() {
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
            ClientOp::Ping => ServerFrame::Pong { id: frame.id },
            ClientOp::Subscribe { topics } => {
                let mut subs = handle.subscriptions.lock().expect("subscriptions poisoned");
                for topic in topics {
                    subs.insert(topic);
                }
                drop(subs);
                // TODO(B0 publisher slice): emit one initial snapshot per newly
                // subscribed topic BEFORE this reply (docs/gui/02 §6). Until then the
                // owner does not advertise `events-v8`.
                ServerFrame::Reply {
                    id: frame.id,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                }
            }
            ClientOp::Unsubscribe { topics } => {
                let mut subs = handle.subscriptions.lock().expect("subscriptions poisoned");
                for topic in topics {
                    subs.remove(&topic);
                }
                drop(subs);
                ServerFrame::Reply {
                    id: frame.id,
                    resp: RemoteResponse::ok("unsubscribed".to_string()),
                }
            }
            ClientOp::Command(command) => {
                let (reply_tx, reply_rx) = oneshot::channel();
                let resp = if emit(RemoteEvent::Command(command, reply_tx)) {
                    match timeout(tuning.reply_timeout, reply_rx).await {
                        Ok(Ok(resp)) => resp,
                        _ => RemoteResponse::err("timeout"),
                    }
                } else {
                    RemoteResponse::err("shutting_down")
                };
                ServerFrame::Reply { id: frame.id, resp }
            }
        };

        if !handle.try_send_line(frame_line(&reply))
            || handle.over_byte_budget(tuning.max_queued_bytes)
        {
            break CloseReason::SlowConsumer;
        }
    };

    // --- Teardown: best-effort goodbye through the reserved queue slot, then close the
    // outbound lane so the writer drains and exits.
    if let Some(reason) = close_reason.goodbye() {
        let _ = handle.try_send_line(frame_line(&ServerFrame::Goodbye {
            reason: reason.to_string(),
        }));
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

        // Item cap: capacity is max_queued_items + 1 (goodbye reserve); the reserve slot
        // still accepts, then the queue is full.
        assert!(handle.try_send_line(vec![b'a'; 8]));
        assert!(handle.try_send_line(vec![b'a'; 8]));
        assert!(handle.try_send_line(vec![b'a'; 8]), "goodbye reserve slot");
        assert!(!handle.try_send_line(vec![b'a'; 8]), "item cap trips");

        // Byte budget: a fresh session with a fat line crosses max_queued_bytes and the
        // caller-visible check reports it even though the item queue accepted it.
        let (_, fat, _rx2) = hub.register().unwrap();
        assert!(fat.try_send_line(vec![b'a'; 65]));
        assert!(fat.over_byte_budget(tuning.max_queued_bytes));
    }
}
