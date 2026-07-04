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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_policies_expose_their_configured_capacity() {
        assert_eq!(DOWNLOAD_QUEUE.capacity(), Some(128));
        assert_eq!(RESOLVER_QUEUE.capacity(), Some(64));
        assert_eq!(
            QueuePolicy::Bounded {
                name: "tiny",
                capacity: 1,
            }
            .capacity(),
            Some(1)
        );
        assert_eq!(
            QueuePolicy::CoalescedByKey {
                name: "dedupe",
                capacity: 7,
            }
            .capacity(),
            Some(7)
        );
    }

    #[tokio::test]
    async fn bounded_channel_enforces_policy_capacity_without_dropping_messages() {
        let (tx, mut rx) = bounded_channel(QueuePolicy::Bounded {
            name: "test",
            capacity: 1,
        });

        tx.try_send("first").expect("first slot is available");
        assert!(tx.try_send("second").is_err(), "capacity must be enforced");
        assert_eq!(rx.recv().await, Some("first"));
        tx.try_send("second").expect("slot frees after receive");
        assert_eq!(rx.recv().await, Some("second"));
    }
}
