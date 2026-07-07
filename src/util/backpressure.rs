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

pub const OWNER_EVENT_QUEUE: QueuePolicy = QueuePolicy::Bounded {
    name: "owner-events",
    capacity: 4096,
};

pub const DAEMON_EVENT_QUEUE: QueuePolicy = QueuePolicy::Bounded {
    name: "daemon-events",
    capacity: 4096,
};

pub const PLAYER_CMD_QUEUE: QueuePolicy = QueuePolicy::Bounded {
    name: "player-cmd",
    capacity: 256,
};

pub const VIDEO_CMD_QUEUE: QueuePolicy = QueuePolicy::Bounded {
    name: "video-cmd",
    capacity: 32,
};

pub const ART_RESIZE_QUEUE: QueuePolicy = QueuePolicy::CoalescedByKey {
    name: "art-resize",
    capacity: 8,
};

pub const MEDIA_ARTWORK_QUEUE: QueuePolicy = QueuePolicy::CoalescedByKey {
    name: "media-artwork",
    capacity: 128,
};

pub const PERSIST_CONTROL_QUEUE: QueuePolicy = QueuePolicy::Bounded {
    name: "persist-control",
    capacity: 32,
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
        assert_eq!(OWNER_EVENT_QUEUE.capacity(), Some(4096));
        assert_eq!(DAEMON_EVENT_QUEUE.capacity(), Some(4096));
        assert_eq!(PLAYER_CMD_QUEUE.capacity(), Some(256));
        assert_eq!(VIDEO_CMD_QUEUE.capacity(), Some(32));
        assert_eq!(ART_RESIZE_QUEUE.capacity(), Some(8));
        assert_eq!(MEDIA_ARTWORK_QUEUE.capacity(), Some(128));
        assert_eq!(PERSIST_CONTROL_QUEUE.capacity(), Some(32));
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
