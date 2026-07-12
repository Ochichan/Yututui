use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::remote::WireSettlement;
use crate::remote::proto::ServerFrame;
#[cfg(test)]
use crate::remote::proto::Topic;

use super::{CloseReason, SessionClose};

/// One outbound line: either a fully-serialized raw frame, or a push event whose
/// payload is shared across sessions (`Arc`) with the tiny per-session envelope —
/// `{"frame":"event","seq":N,"topic":"…","event":<payload>}` — spliced by the writer at
/// write time. Serialize-once fan-out without a per-session payload copy.
pub(crate) enum SessionLine {
    Raw(Vec<u8>),
    TrackedRaw {
        bytes: Vec<u8>,
        _settlement: WireSettlement,
    },
    Event {
        #[cfg(test)]
        seq: u64,
        #[cfg(test)]
        topic: Topic,
        /// Exact serialized envelope prefix, built at admission so byte accounting and the
        /// bytes written to the socket cannot drift apart as sequence/topic widths change.
        prefix: Vec<u8>,
        payload: Arc<Vec<u8>>,
    },
}

impl SessionLine {
    pub(super) fn cost(&self) -> Option<usize> {
        match self {
            SessionLine::Raw(bytes) | SessionLine::TrackedRaw { bytes, .. } => Some(bytes.len()),
            SessionLine::Event {
                prefix, payload, ..
            } => prefix.len().checked_add(payload.len())?.checked_add(2),
        }
    }
}

#[derive(Debug)]
struct OutboundBudgetState {
    /// Whether the writer can still accept a newly admitted frame. This is cleared under the
    /// same lock used by reservation, making close-vs-send a single ordered decision.
    writer_alive: bool,
    /// Includes the frame currently owned by the writer as well as frames still in the channel.
    items: usize,
    /// Exact newline-delimited wire bytes for every counted item, including the in-flight frame.
    bytes: usize,
}

pub(super) struct OutboundBudget {
    state: Mutex<OutboundBudgetState>,
    max_items: usize,
    max_bytes: usize,
}

impl OutboundBudget {
    pub(super) fn new(max_items: usize, max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(OutboundBudgetState {
                writer_alive: true,
                items: 0,
                bytes: 0,
            }),
            max_items,
            max_bytes,
        }
    }

    /// Reserve and enqueue while holding one mutex. The writer may receive the channel item in
    /// parallel, but it cannot release the matching reservation until this decision completes.
    pub(super) fn try_send(
        &self,
        line_tx: &mpsc::Sender<SessionLine>,
        close: &SessionClose,
        line: SessionLine,
    ) -> bool {
        let Some(cost) = line.cost() else {
            return false;
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.writer_alive || close.is_closed() {
            return false;
        }
        let Some(next_items) = state.items.checked_add(1) else {
            return false;
        };
        let Some(next_bytes) = state.bytes.checked_add(cost) else {
            return false;
        };
        if next_items > self.max_items || next_bytes > self.max_bytes {
            return false;
        }
        state.items = next_items;
        state.bytes = next_bytes;

        match line_tx.try_send(line) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(line))
            | Err(mpsc::error::TrySendError::Closed(line)) => {
                let rollback = line
                    .cost()
                    .expect("an admitted session line must retain a representable cost");
                Self::release_locked(&mut state, rollback);
                false
            }
        }
    }

    fn release(&self, cost: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Self::release_locked(&mut state, cost);
    }

    fn release_locked(state: &mut OutboundBudgetState, cost: usize) {
        state.items = state
            .items
            .checked_sub(1)
            .expect("session item accounting underflow");
        state.bytes = state
            .bytes
            .checked_sub(cost)
            .expect("session byte accounting underflow");
    }

    /// Serialize cancellation against admission. A producer either completes its send first, or
    /// observes `writer_alive == false`; it can never reserve against a reset counter.
    pub(super) fn request_close(&self, close: &SessionClose, reason: CloseReason) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.writer_alive = false;
        close.request(reason);
    }

    /// Stop admission, close the channel, and release every queued reservation exactly. The
    /// caller has already released any in-flight frame before entering this method.
    pub(super) fn finish_writer(&self, line_rx: &mut mpsc::Receiver<SessionLine>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.writer_alive = false;
        line_rx.close();
        while let Ok(line) = line_rx.try_recv() {
            let cost = line
                .cost()
                .expect("a queued session line must retain a representable cost");
            Self::release_locked(&mut state, cost);
        }
        assert_eq!(state.items, 0, "writer closed with an in-flight item");
        assert_eq!(state.bytes, 0, "writer closed with in-flight bytes");
    }

    /// Abort-safe fallback. Dropping the receiver releases the actual frames immediately after
    /// this guard runs; clearing the state under the admission lock prevents stale counters or a
    /// close/reset rollback race even when the writer task itself is aborted.
    fn force_closed(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.writer_alive = false;
        state.items = 0;
        state.bytes = 0;
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> (bool, usize, usize) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.writer_alive, state.items, state.bytes)
    }
}

struct WriterBudgetGuard {
    budget: Arc<OutboundBudget>,
}

impl Drop for WriterBudgetGuard {
    fn drop(&mut self) {
        self.budget.force_closed();
    }
}

/// A connection task owns its writer. If the server aborts the connection during shutdown, do
/// not let dropping the writer's `JoinHandle` detach a socket task from that ownership tree.
pub(super) struct WriterTask {
    pub(super) handle: tokio::task::JoinHandle<()>,
}

impl Drop for WriterTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Serialize one frame to a newline-terminated wire line.
pub(super) fn frame_line(frame: &ServerFrame) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(frame)
        .unwrap_or_else(|_| br#"{"frame":"goodbye","reason":"shutting_down"}"#.to_vec());
    bytes.push(b'\n');
    bytes
}

async fn write_session_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    line: &SessionLine,
) -> io::Result<()> {
    match line {
        SessionLine::Raw(bytes) | SessionLine::TrackedRaw { bytes, .. } => {
            writer.write_all(bytes).await?
        }
        SessionLine::Event {
            prefix, payload, ..
        } => {
            writer.write_all(prefix).await?;
            writer.write_all(payload).await?;
            writer.write_all(b"}\n").await?;
        }
    }
    writer.flush().await
}

pub(super) async fn run_session_writer<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut line_rx: mpsc::Receiver<SessionLine>,
    budget: Arc<OutboundBudget>,
    close: Arc<SessionClose>,
    write_timeout: Duration,
    #[cfg(test)] wire_write_delay: Duration,
) {
    let _budget_guard = WriterBudgetGuard {
        budget: Arc::clone(&budget),
    };
    let mut final_reason = None;
    let mut goodbye_is_frame_safe = true;
    loop {
        let next = tokio::select! {
            biased;
            reason = close.cancelled() => Err(reason),
            line = line_rx.recv() => Ok(line),
        };
        let Some(line) = (match next {
            Ok(line) => line,
            Err(reason) => {
                final_reason = Some(reason);
                break;
            }
        }) else {
            break;
        };

        let cost = line
            .cost()
            .expect("an admitted session line must retain a representable cost");
        #[cfg(test)]
        if !wire_write_delay.is_zero() {
            tokio::time::sleep(wire_write_delay).await;
        }
        let outcome = tokio::select! {
            biased;
            reason = close.cancelled() => Err(reason),
            result = timeout(write_timeout, write_session_line(&mut writer, &line)) => Ok(result),
        };
        budget.release(cost);
        match outcome {
            Err(reason) => {
                // Cancelling `write_all` may leave an arbitrary prefix on the stream. Appending a
                // Goodbye would concatenate two JSON objects into one corrupt line, so close the
                // socket without another frame whenever cancellation raced an in-flight write.
                goodbye_is_frame_safe = false;
                final_reason = Some(reason);
                break;
            }
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(_)) | Err(_)) => {
                budget.request_close(&close, CloseReason::ClientGone);
                break;
            }
        }
    }

    budget.finish_writer(&mut line_rx);
    if goodbye_is_frame_safe && let Some(reason) = final_reason.and_then(CloseReason::goodbye) {
        let goodbye = SessionLine::Raw(frame_line(&ServerFrame::Goodbye {
            reason: reason.to_string(),
        }));
        let _ = timeout(write_timeout, write_session_line(&mut writer, &goodbye)).await;
    }
    let _ = timeout(write_timeout, writer.shutdown()).await;
}
