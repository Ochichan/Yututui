//! Bounded ordered media-event delivery shared by Linux and Windows workers.
//!
//! Metadata, status, option, capability, feedback, and position-discontinuity
//! publications retain a complete snapshot in FIFO order. Pure playback progress
//! replaces one compact scalar clock. Both platforms bound total pending work
//! (ordered events plus the progress slot) at 256 items.
//!
//! If the ordered FIFO is full, a newer ordered publication replaces the tail
//! snapshot and ORs its changed facets into that tail. This bounded coalescing may
//! omit intermediate events, but it always retains a coherent newest external
//! state without waiting for a future progress publication. A receiver removes
//! one item per lock acquisition, so producer contention is independent of queue
//! length.

#![cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use super::{MediaChanges, MediaPlaybackStatus, MediaSnapshot};

/// Total pending-work capacity for both platform workers.
pub(super) const MAX_PENDING_UPDATES: usize = 256;

#[derive(Debug, Clone, Copy)]
pub(super) struct PositionClock {
    pub(super) position_epoch: u64,
    position: f64,
    captured_at: Instant,
    rate: f64,
    volume: f64,
    status: MediaPlaybackStatus,
}

impl PositionClock {
    fn from_snapshot(snapshot: &MediaSnapshot) -> Self {
        Self {
            position_epoch: snapshot.position_epoch,
            position: snapshot.position,
            captured_at: snapshot.captured_at,
            rate: snapshot.rate,
            volume: snapshot.volume,
            status: snapshot.status,
        }
    }

    pub(super) fn apply_to(self, snapshot: &mut MediaSnapshot) {
        snapshot.position = self.position;
        snapshot.captured_at = self.captured_at;
        snapshot.rate = self.rate;
        snapshot.volume = self.volume;
        snapshot.status = self.status;
        snapshot.copy_delivery_clock_from_core(self.position_epoch);
    }
}

/// One logical non-progress publication. Snapshot and changes remain coupled so
/// metadata and position can never come from different core publications.
#[derive(Debug)]
pub(super) struct OrderedMediaEvent {
    pub(super) snapshot: MediaSnapshot,
    pub(super) changes: MediaChanges,
}

/// Exactly one item crosses the receiver lock per call. This enum deliberately
/// keeps the hot ordered event inline: boxing every event would add one allocation
/// merely to silence a size lint, while at most one item is in flight.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(super) enum DeliveryItem {
    Ordered(OrderedMediaEvent),
    Progress(PositionClock),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubmitOutcome {
    /// Pending work was previously empty and fully observed; wake the worker.
    Wake,
    /// The worker is already awake or has not yet observed the queue as empty.
    Coalesced,
    /// A pure-progress update could not claim a new slot at total capacity.
    Dropped,
    /// The receiver has gone away or this sender was closed.
    Closed,
}

pub(super) struct LatestMediaSender {
    inner: Arc<Inner>,
}

pub(super) struct LatestMediaReceiver {
    inner: Arc<Inner>,
}

struct Inner {
    pending: Mutex<Pending>,
    ready: Condvar,
    capacity: usize,
}

struct Pending {
    events: VecDeque<OrderedMediaEvent>,
    progress: Option<PositionClock>,
    /// One platform wake covers all work until the receiver observes the queue
    /// completely empty while holding this mutex.
    wake_pending: bool,
    sender_open: bool,
    receiver_open: bool,
}

/// Both platform workers use the same bounded total-work policy.
pub(super) fn latest_media_channel_bounded() -> (LatestMediaSender, LatestMediaReceiver) {
    latest_media_channel_with_capacity(MAX_PENDING_UPDATES)
}

fn latest_media_channel_with_capacity(capacity: usize) -> (LatestMediaSender, LatestMediaReceiver) {
    assert!(capacity > 0);
    let inner = Arc::new(Inner {
        pending: Mutex::new(Pending {
            // Do not reserve 256 large snapshot slots for the common one-item case.
            events: VecDeque::new(),
            progress: None,
            wake_pending: false,
            sender_open: true,
            receiver_open: true,
        }),
        ready: Condvar::new(),
        capacity,
    });
    (
        LatestMediaSender {
            inner: Arc::clone(&inner),
        },
        LatestMediaReceiver { inner },
    )
}

impl LatestMediaSender {
    /// Publish one logical media update without waiting for consumer capacity.
    pub(super) fn submit(&self, snapshot: &MediaSnapshot, changes: MediaChanges) -> SubmitOutcome {
        let mut pending = self.inner.lock();
        if !pending.sender_open || !pending.receiver_open {
            return SubmitOutcome::Closed;
        }

        let needs_wake = !pending.wake_pending;
        if changes == MediaChanges::default() {
            if pending.progress.is_none() && pending.len() >= self.inner.capacity {
                // The new progress value is dropped, but after a failed platform
                // wake this submit must still re-arm delivery of existing work.
                if needs_wake {
                    pending.wake_pending = true;
                    drop(pending);
                    self.inner.ready.notify_one();
                    return SubmitOutcome::Wake;
                }
                return SubmitOutcome::Dropped;
            }
            pending.progress = Some(PositionClock::from_snapshot(snapshot));
        } else {
            // The complete ordered snapshot is newer than any scalar progress and
            // can reuse that slot before checking the ordered capacity.
            pending.progress = None;
            if pending.events.len() >= self.inner.capacity {
                let tail = pending
                    .events
                    .back_mut()
                    .expect("positive capacity and a full queue imply a tail");
                tail.snapshot = snapshot.clone();
                merge_changes(&mut tail.changes, changes);
            } else {
                pending.events.push_back(OrderedMediaEvent {
                    snapshot: snapshot.clone(),
                    changes,
                });
            }
        }

        pending.wake_pending = true;
        drop(pending);
        if needs_wake {
            self.inner.ready.notify_one();
            SubmitOutcome::Wake
        } else {
            SubmitOutcome::Coalesced
        }
    }

    /// Re-arm wake delivery after a platform notification failed. Pending work
    /// remains intact; a later publication requests another platform wake.
    #[cfg(any(windows, test))]
    pub(super) fn wake_failed(&self) {
        let mut pending = self.inner.lock();
        if pending.has_work() {
            pending.wake_pending = false;
        }
    }

    pub(super) fn close(&self) {
        let mut pending = self.inner.lock();
        if !pending.sender_open {
            return;
        }
        pending.sender_open = false;
        drop(pending);
        self.inner.ready.notify_all();
    }
}

impl Drop for LatestMediaSender {
    fn drop(&mut self) {
        self.close();
    }
}

impl LatestMediaReceiver {
    pub(super) fn try_take(&self) -> Option<DeliveryItem> {
        take_next(&mut self.inner.lock())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn is_closed(&self) -> bool {
        let pending = self.inner.lock();
        !pending.sender_open && !pending.has_work()
    }

    #[cfg(test)]
    fn recv_blocking(&self) -> Option<DeliveryItem> {
        let mut pending = self.inner.lock();
        loop {
            if let Some(item) = take_next(&mut pending) {
                return Some(item);
            }
            if !pending.sender_open {
                return None;
            }
            pending = self
                .inner
                .ready
                .wait(pending)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    #[cfg(test)]
    fn pending_shape(&self) -> PendingShape {
        let pending = self.inner.lock();
        PendingShape {
            events: pending.events.len(),
            progress: pending.progress.is_some(),
            wake_pending: pending.wake_pending,
        }
    }
}

impl Drop for LatestMediaReceiver {
    fn drop(&mut self) {
        self.inner.lock().receiver_open = false;
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Pending {
    fn has_work(&self) -> bool {
        !self.events.is_empty() || self.progress.is_some()
    }

    fn len(&self) -> usize {
        self.events.len() + usize::from(self.progress.is_some())
    }
}

/// Pop exactly one item while holding the mutex. `wake_pending` deliberately
/// remains set after the last item is removed: a submit racing before the next
/// empty observation coalesces into the worker that is already draining.
fn take_next(pending: &mut Pending) -> Option<DeliveryItem> {
    if let Some(event) = pending.events.pop_front() {
        return Some(DeliveryItem::Ordered(event));
    }
    if let Some(progress) = pending.progress.take() {
        return Some(DeliveryItem::Progress(progress));
    }
    pending.wake_pending = false;
    None
}

fn merge_changes(into: &mut MediaChanges, update: MediaChanges) {
    into.track |= update.track;
    into.artwork |= update.artwork;
    into.status |= update.status;
    into.position |= update.position;
    into.options |= update.options;
    into.caps |= update.caps;
    into.feedback |= update.feedback;
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingShape {
    events: usize,
    progress: bool,
    wake_pending: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{MediaCaps, MediaTrack};
    use crate::queue::Repeat;
    use std::time::Duration;

    fn snapshot(title: &str, epoch: u64) -> MediaSnapshot {
        MediaSnapshot {
            track: Some(MediaTrack {
                key: format!("track-{title}"),
                title: title.to_owned(),
                artist: format!("artist-{title}"),
                album: Some(format!("album-{title}")),
                duration: Some(180.0 + epoch as f64),
                is_live: false,
                url: None,
                art_remote_url: None,
                art_file: None,
                art_query: None,
                liked: false,
                disliked: false,
            }),
            status: MediaPlaybackStatus::Playing,
            position: epoch as f64,
            captured_at: Instant::now(),
            rate: 1.0,
            shuffle: false,
            repeat: Repeat::Off,
            volume: 0.5,
            caps: MediaCaps::default(),
            position_epoch: epoch,
        }
    }

    fn track_seek() -> MediaChanges {
        MediaChanges {
            track: true,
            position: true,
            ..MediaChanges::default()
        }
    }

    fn ordered(item: DeliveryItem) -> OrderedMediaEvent {
        match item {
            DeliveryItem::Ordered(event) => event,
            DeliveryItem::Progress(_) => panic!("expected ordered event"),
        }
    }

    fn progress(item: DeliveryItem) -> PositionClock {
        match item {
            DeliveryItem::Progress(clock) => clock,
            DeliveryItem::Ordered(_) => panic!("expected scalar progress"),
        }
    }

    #[test]
    fn slow_consumer_keeps_uncoalesced_track_and_seek_events_in_exact_order() {
        let (sender, receiver) = latest_media_channel_with_capacity(4);
        sender.submit(&snapshot("A", 11), track_seek());
        sender.submit(&snapshot("A-progress", 12), MediaChanges::default());
        sender.submit(&snapshot("B", 21), track_seek());

        let a = ordered(receiver.try_take().unwrap());
        let b = ordered(receiver.try_take().unwrap());
        assert_eq!(a.snapshot.track.as_ref().unwrap().title, "A");
        assert_eq!(a.snapshot.position_epoch, 11);
        assert_eq!(a.changes, track_seek());
        assert_eq!(b.snapshot.track.as_ref().unwrap().title, "B");
        assert_eq!(b.snapshot.position_epoch, 21);
        assert_eq!(b.changes, track_seek());
        assert!(receiver.try_take().is_none());
    }

    #[test]
    fn scalar_progress_occupies_one_slot_for_fifty_thousand_updates() {
        let (sender, receiver) = latest_media_channel_with_capacity(1);
        for epoch in 0..50_000 {
            assert_eq!(
                sender.submit(&snapshot("unchanged", epoch), MediaChanges::default()),
                if epoch == 0 {
                    SubmitOutcome::Wake
                } else {
                    SubmitOutcome::Coalesced
                }
            );
        }
        assert_eq!(
            receiver.pending_shape(),
            PendingShape {
                events: 0,
                progress: true,
                wake_pending: true,
            }
        );
        assert_eq!(
            progress(receiver.try_take().unwrap()).position_epoch,
            49_999
        );
    }

    #[test]
    fn total_capacity_counts_events_and_progress_but_allows_progress_replacement() {
        let (sender, receiver) = latest_media_channel_with_capacity(2);
        sender.submit(&snapshot("A", 1), track_seek());
        sender.submit(&snapshot("A", 2), MediaChanges::default());
        assert_eq!(
            receiver.pending_shape(),
            PendingShape {
                events: 1,
                progress: true,
                wake_pending: true,
            }
        );
        assert_eq!(
            sender.submit(&snapshot("A", 3), MediaChanges::default()),
            SubmitOutcome::Coalesced
        );
        assert_eq!(receiver.pending_shape().events, 1);
        assert!(receiver.pending_shape().progress);
    }

    #[test]
    fn progress_at_full_capacity_drops_without_waiting() {
        let (sender, _receiver) = latest_media_channel_with_capacity(1);
        sender.submit(&snapshot("A", 1), track_seek());
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            done_tx
                .send(sender.submit(&snapshot("progress", 2), MediaChanges::default()))
                .unwrap();
        });
        assert_eq!(
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SubmitOutcome::Dropped
        );
        producer.join().unwrap();
    }

    #[test]
    fn capacity_one_coalesces_a_into_coherent_newest_b() {
        let (sender, receiver) = latest_media_channel_with_capacity(1);
        let a_changes = MediaChanges {
            track: true,
            position: true,
            ..MediaChanges::default()
        };
        let b_changes = MediaChanges {
            status: true,
            options: true,
            caps: true,
            ..MediaChanges::default()
        };
        sender.submit(&snapshot("A", 1), a_changes);
        sender.submit(&snapshot("B", 2), b_changes);

        let event = ordered(receiver.try_take().unwrap());
        assert_eq!(event.snapshot.track.as_ref().unwrap().title, "B");
        assert_eq!(event.snapshot.position_epoch, 2);
        assert_eq!(
            event.changes,
            MediaChanges {
                track: true,
                status: true,
                position: true,
                options: true,
                caps: true,
                ..MediaChanges::default()
            }
        );
        assert!(receiver.try_take().is_none());
    }

    #[test]
    fn full_queue_retains_final_ordered_snapshot_without_a_future_submit() {
        let (sender, receiver) = latest_media_channel_with_capacity(3);
        for epoch in 0..3 {
            sender.submit(&snapshot(&format!("old-{epoch}"), epoch), track_seek());
        }
        let final_changes = MediaChanges {
            status: true,
            options: true,
            ..MediaChanges::default()
        };
        sender.submit(&snapshot("final-B", 99), final_changes);

        assert_eq!(
            ordered(receiver.try_take().unwrap())
                .snapshot
                .position_epoch,
            0
        );
        assert_eq!(
            ordered(receiver.try_take().unwrap())
                .snapshot
                .position_epoch,
            1
        );
        let final_event = ordered(receiver.try_take().unwrap());
        assert_eq!(
            final_event.snapshot.track.as_ref().unwrap().title,
            "final-B"
        );
        assert_eq!(final_event.snapshot.position_epoch, 99);
        assert!(final_event.changes.track);
        assert!(final_event.changes.position);
        assert!(final_event.changes.status);
        assert!(final_event.changes.options);
        assert!(receiver.try_take().is_none());
    }

    #[test]
    fn accepted_ordered_event_clears_older_progress_and_uses_its_slot() {
        let (sender, receiver) = latest_media_channel_with_capacity(2);
        sender.submit(&snapshot("A", 1), track_seek());
        sender.submit(&snapshot("A", 2), MediaChanges::default());
        sender.submit(
            &snapshot("C", 3),
            MediaChanges {
                status: true,
                ..MediaChanges::default()
            },
        );
        assert_eq!(
            receiver.pending_shape(),
            PendingShape {
                events: 2,
                progress: false,
                wake_pending: true,
            }
        );
        assert_eq!(
            ordered(receiver.try_take().unwrap())
                .snapshot
                .position_epoch,
            1
        );
        assert_eq!(
            ordered(receiver.try_take().unwrap())
                .snapshot
                .position_epoch,
            3
        );
    }

    #[test]
    fn shared_default_capacity_never_exceeds_256_total_items() {
        let (sender, receiver) = latest_media_channel_bounded();
        for epoch in 0..(MAX_PENDING_UPDATES - 1) as u64 {
            sender.submit(&snapshot("accepted", epoch), track_seek());
        }
        sender.submit(&snapshot("progress", 999), MediaChanges::default());
        assert_eq!(receiver.pending_shape().events, MAX_PENDING_UPDATES - 1);
        assert!(receiver.pending_shape().progress);

        // Ordered work reuses the progress slot, reaching exactly 256 events.
        sender.submit(&snapshot("ordered", 1_000), track_seek());
        assert_eq!(receiver.pending_shape().events, MAX_PENDING_UPDATES);
        assert!(!receiver.pending_shape().progress);
        // A further ordered event replaces the tail instead of growing the queue.
        sender.submit(&snapshot("tail", 1_001), track_seek());
        assert_eq!(receiver.pending_shape().events, MAX_PENDING_UPDATES);
    }

    #[test]
    fn submit_after_last_take_coalesces_until_empty_is_observed() {
        let (sender, receiver) = latest_media_channel_with_capacity(1);
        assert_eq!(
            sender.submit(&snapshot("first", 1), MediaChanges::default()),
            SubmitOutcome::Wake
        );
        assert_eq!(progress(receiver.try_take().unwrap()).position_epoch, 1);
        assert_eq!(
            sender.submit(&snapshot("racing", 2), MediaChanges::default()),
            SubmitOutcome::Coalesced
        );
        assert_eq!(progress(receiver.try_take().unwrap()).position_epoch, 2);
    }

    #[test]
    fn submit_after_empty_observation_requests_a_new_wake() {
        let (sender, receiver) = latest_media_channel_with_capacity(1);
        sender.submit(&snapshot("first", 1), MediaChanges::default());
        let _ = receiver.try_take().unwrap();
        assert!(receiver.try_take().is_none());
        assert_eq!(
            sender.submit(&snapshot("after-empty", 2), MediaChanges::default()),
            SubmitOutcome::Wake
        );
    }

    #[test]
    fn failed_platform_wake_is_retried_without_losing_pending_work() {
        let (sender, receiver) = latest_media_channel_with_capacity(2);
        sender.submit(&snapshot("first", 1), track_seek());
        sender.wake_failed();
        assert_eq!(
            sender.submit(&snapshot("latest", 2), MediaChanges::default()),
            SubmitOutcome::Wake
        );
        assert_eq!(
            ordered(receiver.try_take().unwrap())
                .snapshot
                .position_epoch,
            1
        );
        assert_eq!(progress(receiver.try_take().unwrap()).position_epoch, 2);
        assert!(receiver.try_take().is_none());
    }

    #[test]
    fn full_queue_failed_wake_is_rearmed_even_when_progress_drops() {
        let (sender, receiver) = latest_media_channel_with_capacity(1);
        sender.submit(&snapshot("pending", 1), track_seek());
        sender.wake_failed();
        assert_eq!(
            sender.submit(&snapshot("dropped-progress", 2), MediaChanges::default()),
            SubmitOutcome::Wake
        );
        let event = ordered(receiver.try_take().unwrap());
        assert_eq!(event.snapshot.track.unwrap().title, "pending");
        assert!(receiver.try_take().is_none());
    }

    #[test]
    fn close_drains_every_item_before_reporting_closed() {
        let (sender, receiver) = latest_media_channel_with_capacity(2);
        sender.submit(&snapshot("pending", 1), track_seek());
        sender.submit(&snapshot("pending", 2), MediaChanges::default());
        sender.close();

        assert!(matches!(
            receiver.recv_blocking(),
            Some(DeliveryItem::Ordered(_))
        ));
        assert!(matches!(
            receiver.recv_blocking(),
            Some(DeliveryItem::Progress(_))
        ));
        assert!(receiver.recv_blocking().is_none());
    }

    #[test]
    fn close_wakes_blocking_receiver_without_fabricating_work() {
        let (sender, receiver) = latest_media_channel_with_capacity(1);
        let waiter = std::thread::spawn(move || receiver.recv_blocking().is_none());
        sender.close();
        assert!(waiter.join().unwrap());
    }

    #[test]
    fn receiver_close_makes_later_submit_nonblocking_and_closed() {
        let (sender, receiver) = latest_media_channel_with_capacity(1);
        drop(receiver);
        assert_eq!(
            sender.submit(&snapshot("closed", 1), track_seek()),
            SubmitOutcome::Closed
        );
    }
}
