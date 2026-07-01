//! Explicit queue policies for actor inboxes.

use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePolicy {
    /// A bounded FIFO queue. Producers must decide what to do when it is full.
    Bounded { name: &'static str, capacity: usize },
    /// Only the newest request matters; older queued requests should be drained/dropped.
    LatestOnly { name: &'static str },
    /// Logically coalesced by a domain key before work starts.
    CoalescedByKey { name: &'static str, capacity: usize },
    /// Kept unbounded intentionally because it is already coalesced, terminal-lifetime scoped,
    /// or latency-critical with tiny messages. New uses should not default here.
    UnboundedAllowlisted {
        name: &'static str,
        reason: &'static str,
    },
}

impl QueuePolicy {
    pub const fn capacity(self) -> Option<usize> {
        match self {
            QueuePolicy::Bounded { capacity, .. }
            | QueuePolicy::CoalescedByKey { capacity, .. } => Some(capacity),
            QueuePolicy::LatestOnly { .. } | QueuePolicy::UnboundedAllowlisted { .. } => None,
        }
    }
}

pub const DOWNLOAD_QUEUE: QueuePolicy = QueuePolicy::Bounded {
    name: "download",
    capacity: 128,
};

pub const RESOLVER_QUEUE: QueuePolicy = QueuePolicy::CoalescedByKey {
    name: "resolver",
    capacity: 64,
};

pub const ART_RESIZE_QUEUE: QueuePolicy = QueuePolicy::LatestOnly { name: "art_resize" };

pub const RUNTIME_EVENT_QUEUE: QueuePolicy = QueuePolicy::UnboundedAllowlisted {
    name: "runtime_event",
    reason: "single-session app event bus; high-frequency player progress is coalesced before send",
};

pub fn bounded_channel<T>(policy: QueuePolicy) -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    let capacity = policy
        .capacity()
        .expect("bounded_channel requires a bounded policy");
    mpsc::channel(capacity)
}
