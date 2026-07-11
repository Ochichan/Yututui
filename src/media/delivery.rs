//! Bounded latest-state delivery shared by the Linux and Windows media workers.
//!
//! Ordinary progress replaces one scalar clock slot, while metadata-bearing state
//! replaces one snapshot slot and ORs its [`MediaChanges`]. Position discontinuities
//! are different: every epoch is retained in a compact bounded queue. If that queue
//! fills, the producer applies backpressure instead of losing a `Seeked`/timeline
//! event or growing memory without bound.

#![cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use super::{MediaChanges, MediaPlaybackStatus, MediaSnapshot};

/// Compact discontinuity capacity. This matches the old Linux snapshot-channel
/// depth while retaining only scalar clocks (rather than up to 256 full metadata
/// snapshots); normal state/progress always occupies one latest-value slot.
pub(super) const MAX_PENDING_DISCONTINUITIES: usize = 256;

#[derive(Debug, Clone, Copy)]
pub(super) struct PositionClock {
    #[cfg(any(windows, test))]
    sequence: u64,
    pub(super) position_epoch: u64,
    position: f64,
    captured_at: Instant,
    rate: f64,
    volume: f64,
    status: MediaPlaybackStatus,
    duration: Option<f64>,
    #[cfg(windows)]
    is_live: bool,
}

impl PositionClock {
    fn from_snapshot(snapshot: &MediaSnapshot, sequence: u64) -> Self {
        #[cfg(not(any(windows, test)))]
        let _ = sequence;
        Self {
            #[cfg(any(windows, test))]
            sequence,
            position_epoch: snapshot.position_epoch,
            position: snapshot.position,
            captured_at: snapshot.captured_at,
            rate: snapshot.rate,
            volume: snapshot.volume,
            status: snapshot.status,
            duration: snapshot.track.as_ref().and_then(|track| track.duration),
            #[cfg(windows)]
            is_live: snapshot.track.as_ref().is_some_and(|track| track.is_live),
        }
    }

    pub(super) fn position_now(self) -> f64 {
        let mut position = self.position;
        if self.status == MediaPlaybackStatus::Playing {
            position += self.captured_at.elapsed().as_secs_f64() * self.rate;
        }
        if let Some(duration) = self.duration {
            position = position.min(duration);
        }
        position.max(0.0)
    }

    #[cfg(windows)]
    pub(super) fn timeline_duration(self) -> Option<f64> {
        self.duration.filter(|_| !self.is_live)
    }

    fn apply_to(self, snapshot: &mut MediaSnapshot) {
        snapshot.position = self.position;
        snapshot.captured_at = self.captured_at;
        snapshot.rate = self.rate;
        snapshot.volume = self.volume;
        snapshot.status = self.status;
        snapshot.copy_delivery_clock_from_core(self.position_epoch);
    }
}

#[derive(Debug)]
pub(super) struct DeliveryBatch {
    /// Present only when a non-position facet changed. Progress-only publications
    /// therefore never clone track strings or paths.
    pub(super) snapshot: Option<MediaSnapshot>,
    pub(super) clock: PositionClock,
    pub(super) changes: MediaChanges,
    /// Every position discontinuity, in publication order.
    pub(super) discontinuities: Vec<PositionClock>,
    /// Sequence of the newest coalesced update that requires an immediate SMTC
    /// timeline push for reasons other than a discontinuity (status/options/track).
    #[cfg(any(windows, test))]
    timeline_sequence: Option<u64>,
}

impl DeliveryBatch {
    /// Install the newest scalar clock into the optional full snapshot before a
    /// consumer publishes it or moves it into its shared current-state slot.
    pub(super) fn prepare_snapshot(&mut self) {
        if let Some(snapshot) = self.snapshot.as_mut() {
            self.clock.apply_to(snapshot);
        }
    }

    pub(super) fn apply_clock_to(&self, snapshot: &mut MediaSnapshot) {
        self.clock.apply_to(snapshot);
    }

    /// SMTC pushes every discontinuity. A separate final push is needed only when
    /// a later coalesced status/options update occurred after the last one.
    #[cfg(any(windows, test))]
    pub(super) fn needs_final_timeline(&self) -> bool {
        self.timeline_sequence.is_some_and(|sequence| {
            self.discontinuities
                .last()
                .is_none_or(|position| sequence > position.sequence)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubmitOutcome {
    /// The pending slot was empty; the platform worker must be woken.
    Wake,
    /// A wake is already pending; this update was coalesced into it.
    Coalesced,
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
    space: Condvar,
    discontinuity_capacity: usize,
}

struct Pending {
    snapshot: Option<MediaSnapshot>,
    clock: Option<PositionClock>,
    /// A platform wake has been requested for the current pending slot. This is separate from
    /// `clock`: a consumer clears it when taking a batch, and Windows can clear it after a failed
    /// `PostThreadMessageW` so the next update retries instead of coalescing forever.
    wake_pending: bool,
    changes: MediaChanges,
    discontinuities: VecDeque<PositionClock>,
    #[cfg(any(windows, test))]
    timeline_sequence: Option<u64>,
    next_sequence: u64,
    sender_open: bool,
    receiver_open: bool,
}

pub(super) fn latest_media_channel() -> (LatestMediaSender, LatestMediaReceiver) {
    latest_media_channel_with_capacity(MAX_PENDING_DISCONTINUITIES)
}

fn latest_media_channel_with_capacity(
    discontinuity_capacity: usize,
) -> (LatestMediaSender, LatestMediaReceiver) {
    assert!(discontinuity_capacity > 0);
    let inner = Arc::new(Inner {
        pending: Mutex::new(Pending {
            snapshot: None,
            clock: None,
            wake_pending: false,
            changes: MediaChanges::default(),
            discontinuities: VecDeque::with_capacity(discontinuity_capacity),
            #[cfg(any(windows, test))]
            timeline_sequence: None,
            next_sequence: 1,
            sender_open: true,
            receiver_open: true,
        }),
        ready: Condvar::new(),
        space: Condvar::new(),
        discontinuity_capacity,
    });
    (
        LatestMediaSender {
            inner: Arc::clone(&inner),
        },
        LatestMediaReceiver { inner },
    )
}

impl LatestMediaSender {
    /// Publish one logical media update. Only a full-queue discontinuity can block:
    /// scalar progress and metadata always replace their single latest-value slots.
    pub(super) fn submit(&self, snapshot: &MediaSnapshot, changes: MediaChanges) -> SubmitOutcome {
        let mut pending = self.inner.lock();
        while changes.position
            && pending.discontinuities.len() == self.inner.discontinuity_capacity
            && pending.sender_open
            && pending.receiver_open
        {
            pending = self
                .inner
                .space
                .wait(pending)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        if !pending.sender_open || !pending.receiver_open {
            return SubmitOutcome::Closed;
        }

        let needs_wake = !pending.wake_pending;
        pending.wake_pending = true;
        let sequence = pending.next_sequence;
        pending.next_sequence = pending.next_sequence.wrapping_add(1);
        let clock = PositionClock::from_snapshot(snapshot, sequence);

        if requires_snapshot(changes) {
            pending.snapshot = Some(snapshot.clone());
        }
        pending.clock = Some(clock);
        merge_changes(&mut pending.changes, changes);
        if changes.position {
            pending.discontinuities.push_back(clock);
        }
        #[cfg(any(windows, test))]
        if changes.track || changes.status || changes.options {
            pending.timeline_sequence = Some(sequence);
        }
        drop(pending);
        self.inner.ready.notify_one();

        if needs_wake {
            SubmitOutcome::Wake
        } else {
            SubmitOutcome::Coalesced
        }
    }

    /// Re-arm wake delivery after a platform notification failed. Pending state remains intact;
    /// a later publication will return [`SubmitOutcome::Wake`] and retry the platform signal.
    #[cfg(any(windows, test))]
    pub(super) fn wake_failed(&self) {
        let mut pending = self.inner.lock();
        if pending.clock.is_some() {
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
        self.inner.space.notify_all();
    }
}

impl Drop for LatestMediaSender {
    fn drop(&mut self) {
        self.close();
    }
}

impl LatestMediaReceiver {
    pub(super) fn try_take(&self) -> Option<DeliveryBatch> {
        let mut pending = self.inner.lock();
        take_batch(&mut pending).inspect(|_| self.inner.space.notify_all())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn is_closed(&self) -> bool {
        let pending = self.inner.lock();
        !pending.sender_open && pending.clock.is_none()
    }

    #[cfg(test)]
    fn recv_blocking(&self) -> Option<DeliveryBatch> {
        let mut pending = self.inner.lock();
        while pending.clock.is_none() && pending.sender_open {
            pending = self
                .inner
                .ready
                .wait(pending)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        take_batch(&mut pending).inspect(|_| self.inner.space.notify_all())
    }

    #[cfg(test)]
    fn pending_shape(&self) -> (bool, bool, usize) {
        let pending = self.inner.lock();
        (
            pending.snapshot.is_some(),
            pending.clock.is_some(),
            pending.discontinuities.len(),
        )
    }
}

impl Drop for LatestMediaReceiver {
    fn drop(&mut self) {
        let mut pending = self.inner.lock();
        pending.receiver_open = false;
        drop(pending);
        self.inner.space.notify_all();
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn take_batch(pending: &mut Pending) -> Option<DeliveryBatch> {
    let clock = pending.clock.take()?;
    pending.wake_pending = false;
    Some(DeliveryBatch {
        snapshot: pending.snapshot.take(),
        clock,
        changes: std::mem::take(&mut pending.changes),
        discontinuities: pending.discontinuities.drain(..).collect(),
        #[cfg(any(windows, test))]
        timeline_sequence: pending.timeline_sequence.take(),
    })
}

fn requires_snapshot(changes: MediaChanges) -> bool {
    changes.track
        || changes.artwork
        || changes.status
        || changes.options
        || changes.caps
        || changes.feedback
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
mod tests {
    use super::*;
    use crate::media::{MediaCaps, MediaTrack};
    use crate::queue::Repeat;

    fn snapshot(title: &str, epoch: u64) -> MediaSnapshot {
        MediaSnapshot {
            track: Some(MediaTrack {
                key: "track".to_owned(),
                title: title.to_owned(),
                artist: "artist".to_owned(),
                album: None,
                duration: Some(180.0),
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

    #[test]
    fn slow_consumer_gets_newest_metadata_and_every_aggregated_facet() {
        let (sender, receiver) = latest_media_channel_with_capacity(4);
        let facets = [
            MediaChanges {
                track: true,
                position: true,
                ..MediaChanges::default()
            },
            MediaChanges {
                artwork: true,
                status: true,
                ..MediaChanges::default()
            },
            MediaChanges {
                options: true,
                caps: true,
                feedback: true,
                ..MediaChanges::default()
            },
        ];

        assert_eq!(
            sender.submit(&snapshot("old", 1), facets[0]),
            SubmitOutcome::Wake
        );
        assert_eq!(
            sender.submit(&snapshot("middle", 1), facets[1]),
            SubmitOutcome::Coalesced
        );
        assert_eq!(
            sender.submit(&snapshot("newest", 1), facets[2]),
            SubmitOutcome::Coalesced
        );

        let mut batch = receiver.try_take().expect("one coalesced batch");
        batch.prepare_snapshot();
        let state = batch.snapshot.expect("metadata-bearing latest snapshot");
        assert_eq!(state.track.as_ref().unwrap().title, "newest");
        assert_eq!(batch.changes, MediaChanges::all());
        assert_eq!(batch.discontinuities.len(), 1);
    }

    #[test]
    fn scalar_progress_occupies_one_slot_and_never_clones_full_state() {
        let (sender, receiver) = latest_media_channel_with_capacity(2);
        for epoch in 0..50_000 {
            let outcome = sender.submit(&snapshot("unchanged", epoch), MediaChanges::default());
            assert_eq!(
                outcome,
                if epoch == 0 {
                    SubmitOutcome::Wake
                } else {
                    SubmitOutcome::Coalesced
                }
            );
        }
        assert_eq!(receiver.pending_shape(), (false, true, 0));

        let batch = receiver.try_take().expect("latest scalar clock");
        assert!(batch.snapshot.is_none());
        assert_eq!(batch.clock.position_epoch, 49_999);
        assert_eq!(batch.changes, MediaChanges::default());
    }

    #[test]
    fn failed_platform_wake_is_retried_without_losing_pending_state() {
        let (sender, receiver) = latest_media_channel_with_capacity(2);
        assert_eq!(
            sender.submit(&snapshot("first", 1), MediaChanges::default()),
            SubmitOutcome::Wake
        );

        // Model a failed Windows PostThreadMessageW. The first snapshot stays pending, but the
        // next publication must request another wake instead of being coalesced into a wake that
        // never reached the worker.
        sender.wake_failed();
        assert_eq!(
            sender.submit(&snapshot("latest", 2), MediaChanges::default()),
            SubmitOutcome::Wake
        );

        let batch = receiver
            .try_take()
            .expect("latest state remains deliverable");
        assert_eq!(batch.clock.position_epoch, 2);
        assert!(receiver.try_take().is_none());

        assert_eq!(
            sender.submit(&snapshot("after-drain", 3), MediaChanges::default()),
            SubmitOutcome::Wake,
            "taking a batch also re-arms the next platform wake"
        );
    }

    #[test]
    fn bounded_slow_consumer_preserves_every_discontinuity_in_order() {
        let (sender, receiver) = latest_media_channel_with_capacity(2);
        let (filled_tx, filled_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            for epoch in 1..=20 {
                assert_ne!(
                    sender.submit(
                        &snapshot("same", epoch),
                        MediaChanges {
                            position: true,
                            ..MediaChanges::default()
                        }
                    ),
                    SubmitOutcome::Closed
                );
                if epoch == 2 {
                    filled_tx.send(()).unwrap();
                    continue_rx.recv().unwrap();
                }
            }
        });

        filled_rx.recv().unwrap();
        assert_eq!(receiver.pending_shape(), (false, true, 2));
        continue_tx.send(()).unwrap();

        let mut epochs = Vec::new();
        while let Some(batch) = receiver.recv_blocking() {
            assert!(batch.discontinuities.len() <= 2);
            epochs.extend(
                batch
                    .discontinuities
                    .into_iter()
                    .map(|clock| clock.position_epoch),
            );
        }
        producer.join().unwrap();
        assert_eq!(epochs, (1..=20).collect::<Vec<_>>());
    }

    #[test]
    fn dedicated_consumer_releases_backpressure_from_a_current_thread_runtime() {
        let (sender, receiver) = latest_media_channel_with_capacity(1);
        let (first_pending_tx, first_pending_rx) = std::sync::mpsc::channel();
        let (drained_tx, drained_rx) = std::sync::mpsc::channel();

        // Mirrors MPRIS's dedicated worker: it can drain independently even while the daemon's
        // single-thread runtime is synchronously submitting the second lossless discontinuity.
        let consumer = std::thread::spawn(move || {
            first_pending_rx.recv().unwrap();
            let batch = receiver.recv_blocking().expect("first discontinuity");
            assert_eq!(batch.discontinuities[0].position_epoch, 1);
            drained_tx.send(()).unwrap();
            receiver
        });

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let position_change = MediaChanges {
                    position: true,
                    ..MediaChanges::default()
                };
                assert_eq!(
                    sender.submit(&snapshot("same", 1), position_change),
                    SubmitOutcome::Wake
                );
                first_pending_tx.send(()).unwrap();
                assert_ne!(
                    sender.submit(&snapshot("same", 2), position_change),
                    SubmitOutcome::Closed
                );
                done_tx.send(()).unwrap();
            });
        });

        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("dedicated consumer must prevent single-thread runtime deadlock");
        drained_rx.recv().unwrap();
        producer.join().unwrap();
        let receiver = consumer.join().unwrap();
        assert_eq!(
            receiver
                .try_take()
                .expect("second discontinuity remains pending")
                .discontinuities[0]
                .position_epoch,
            2
        );
    }

    #[test]
    fn final_timeline_is_needed_only_after_the_last_discontinuity() {
        let (sender, receiver) = latest_media_channel_with_capacity(4);
        sender.submit(
            &snapshot("same", 1),
            MediaChanges {
                status: true,
                ..MediaChanges::default()
            },
        );
        sender.submit(
            &snapshot("same", 2),
            MediaChanges {
                position: true,
                ..MediaChanges::default()
            },
        );
        assert!(!receiver.try_take().unwrap().needs_final_timeline());

        sender.submit(
            &snapshot("same", 3),
            MediaChanges {
                position: true,
                ..MediaChanges::default()
            },
        );
        sender.submit(
            &snapshot("same", 3),
            MediaChanges {
                options: true,
                ..MediaChanges::default()
            },
        );
        assert!(receiver.try_take().unwrap().needs_final_timeline());
    }

    #[test]
    fn close_wakes_a_blocking_receiver_without_fabricating_state() {
        let (sender, receiver) = latest_media_channel_with_capacity(1);
        let waiter = std::thread::spawn(move || receiver.recv_blocking().is_none());
        sender.close();
        assert!(waiter.join().unwrap());
    }
}
