//! Explicit queue policies for actor inboxes.

use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePolicy {
    /// A bounded FIFO queue. Producers must decide what to do when it is full.
    Bounded { name: &'static str, capacity: usize },
    /// Logically coalesced by a domain key before work starts.
    CoalescedByKey { name: &'static str, capacity: usize },
}

impl QueuePolicy {
    pub const fn capacity(self) -> Option<usize> {
        match self {
            QueuePolicy::Bounded { capacity, .. }
            | QueuePolicy::CoalescedByKey { capacity, .. } => Some(capacity),
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

pub fn bounded_channel<T>(policy: QueuePolicy) -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    let capacity = policy
        .capacity()
        .expect("bounded_channel requires a bounded policy");
    mpsc::channel(capacity)
}
