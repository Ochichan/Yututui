use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::mpsc;

use crate::util::event_policy::{EventLane as Lane, EventPolicy};

use super::DaemonEvent;

const DAEMON_MUST_DELIVER_OVERFLOW_MAX: usize = 1024;

struct DeferredDaemonEvent {
    event: DaemonEvent,
    event_kind: &'static str,
    policy: EventPolicy,
}

pub(super) struct DaemonMustDeliverOverflow {
    queue: Mutex<VecDeque<DeferredDaemonEvent>>,
    drainer_running: AtomicBool,
}

impl DaemonMustDeliverOverflow {
    pub(super) fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            drainer_running: AtomicBool::new(false),
        }
    }

    pub(super) fn push(
        self: &Arc<Self>,
        tx: mpsc::Sender<DaemonEvent>,
        event: DaemonEvent,
        event_kind: &'static str,
        policy: EventPolicy,
    ) {
        let item = DeferredDaemonEvent {
            event,
            event_kind,
            policy,
        };
        let start_drainer = {
            let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
            if queue.len() >= DAEMON_MUST_DELIVER_OVERFLOW_MAX {
                drop(queue);
                tracing::warn!(
                    event_policy = policy.name(),
                    event_lane = policy.lane().map(Lane::name).unwrap_or("none"),
                    event_kind,
                    drop_reason = "must_deliver_overflow_fallback",
                    "daemon must-deliver overflow full; falling back to direct deferred send"
                );
                defer_must_deliver_direct(tx, item.event, item.event_kind, item.policy);
                return;
            }
            queue.push_back(item);
            !self.drainer_running.swap(true, Ordering::AcqRel)
        };
        if start_drainer {
            spawn_drainer(Arc::clone(self), tx);
        }
    }

    fn pop_or_stop(&self) -> Option<DeferredDaemonEvent> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        match queue.pop_front() {
            Some(item) => Some(item),
            None => {
                self.drainer_running.store(false, Ordering::Release);
                None
            }
        }
    }
}

fn spawn_drainer(overflow: Arc<DaemonMustDeliverOverflow>, tx: mpsc::Sender<DaemonEvent>) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            while let Some(item) = overflow.pop_or_stop() {
                if tx.send(item.event).await.is_err() {
                    log_closed(item.event_kind, item.policy);
                }
            }
        });
    } else {
        std::thread::spawn(move || {
            while let Some(item) = overflow.pop_or_stop() {
                if tx.blocking_send(item.event).is_err() {
                    log_closed(item.event_kind, item.policy);
                }
            }
        });
    }
}

fn defer_must_deliver_direct(
    tx: mpsc::Sender<DaemonEvent>,
    event: DaemonEvent,
    event_kind: &'static str,
    policy: EventPolicy,
) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if tx.send(event).await.is_err() {
                log_closed(event_kind, policy);
            }
        });
    } else {
        std::thread::spawn(move || {
            if tx.blocking_send(event).is_err() {
                log_closed(event_kind, policy);
            }
        });
    }
}

fn log_closed(event_kind: &'static str, policy: EventPolicy) {
    tracing::error!(
        event_policy = policy.name(),
        event_lane = policy.lane().map(Lane::name).unwrap_or("none"),
        event_kind,
        drop_reason = "must_deliver_failed",
        "daemon owner event queue closed before must-deliver event was accepted"
    );
}
