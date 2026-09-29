use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use tokio::sync::mpsc::Sender;

use super::super::{EventSink, PlayerEvent};
use super::proof::ExtraProof;

pub struct EventGate {
    pub(super) extra_is_lead: AtomicBool,
    pub(super) fading: AtomicBool,
    pub(super) admitted: Arc<AtomicU64>,
    pub(super) pending_incoming_extra: AtomicBool,
    pub(super) pending_generation: AtomicU64,
    pub(super) pending_epoch: AtomicU64,
    next_overlap_epoch: AtomicU64,
    proof: Option<Sender<ExtraProof>>,
}

impl EventGate {
    #[cfg(test)]
    pub(super) fn new(admitted: Arc<AtomicU64>) -> Arc<Self> {
        Self::with_proof(admitted, None)
    }

    pub fn with_proof(admitted: Arc<AtomicU64>, proof: Option<Sender<ExtraProof>>) -> Arc<Self> {
        Arc::new(Self {
            extra_is_lead: AtomicBool::new(false),
            fading: AtomicBool::new(false),
            admitted,
            pending_incoming_extra: AtomicBool::new(false),
            pending_generation: AtomicU64::new(0),
            pending_epoch: AtomicU64::new(0),
            next_overlap_epoch: AtomicU64::new(0),
            proof,
        })
    }

    pub fn sink(self: &Arc<Self>, from_extra: bool, emit: EventSink) -> EventSink {
        let gate = Arc::clone(self);
        Arc::new(move |event| gate.emit(from_extra, event, &emit))
    }

    pub(super) fn arm_pending(&self, incoming_extra: bool, generation: u64) -> u64 {
        let epoch = self
            .next_overlap_epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.pending_epoch.store(0, Ordering::Release);
        self.pending_incoming_extra
            .store(incoming_extra, Ordering::Release);
        self.pending_generation.store(generation, Ordering::Release);
        self.pending_epoch.store(epoch, Ordering::Release);
        epoch
    }

    pub(super) fn clear_pending(&self) {
        self.pending_epoch.store(0, Ordering::Release);
        self.pending_generation.store(0, Ordering::Release);
    }

    fn pending_identity(&self) -> Option<(u64, u64, bool)> {
        loop {
            let epoch = self.pending_epoch.load(Ordering::Acquire);
            if epoch == 0 {
                return None;
            }
            let generation = self.pending_generation.load(Ordering::Acquire);
            let incoming = self.pending_incoming_extra.load(Ordering::Acquire);
            if self.pending_epoch.load(Ordering::Acquire) != epoch {
                continue;
            }
            if generation == 0 {
                return None;
            }
            return Some((generation, epoch, incoming));
        }
    }

    fn observe_pending(&self, from_extra: bool, event: &PlayerEvent) -> Option<ExtraProof> {
        let (generation, epoch, incoming) = self.pending_identity()?;
        if from_extra != incoming {
            return None;
        }
        match event.unscoped() {
            PlayerEvent::Error(_)
                if event
                    .file_generation()
                    .is_none_or(|event_generation| event_generation == generation) =>
            {
                Some(ExtraProof::Failed { epoch })
            }
            PlayerEvent::TransportClosed(_) => {
                Some(ExtraProof::TransportClosed { epoch, from_extra })
            }
            PlayerEvent::TimePos(_) | PlayerEvent::Duration(Some(_))
                if event.file_generation() == Some(generation) =>
            {
                Some(ExtraProof::Ready { epoch })
            }
            _ => None,
        }
    }

    pub(super) fn emit(&self, from_extra: bool, event: PlayerEvent, sink: &EventSink) {
        if let Some(proof) = self
            .observe_pending(from_extra, &event)
            .or_else(|| deck_closed_proof(from_extra, &event))
            && let Some(tx) = &self.proof
        {
            let _ = tx.try_send(proof);
        }
        let unscoped = event.unscoped();
        if matches!(
            unscoped,
            PlayerEvent::CacheEmergency { .. } | PlayerEvent::CacheReplacementEmergency { .. }
        ) {
            sink(event);
            return;
        }
        if matches!(unscoped, PlayerEvent::Volume(_)) && self.fading.load(Ordering::Acquire) {
            return;
        }
        if self.pending_generation.load(Ordering::Acquire) != 0 {
            if self.admit_pending_incoming(from_extra, &event) {
                sink(event);
            }
            return;
        }
        let extra_leads = self.extra_is_lead.load(Ordering::Acquire);
        if from_extra != extra_leads {
            return;
        }
        sink(event);
    }

    pub(super) fn admit_pending_incoming(&self, from_extra: bool, event: &PlayerEvent) -> bool {
        let Some((generation, _, incoming)) = self.pending_identity() else {
            return false;
        };
        if from_extra != incoming || event.file_generation() != Some(generation) {
            return false;
        }
        matches!(
            event.unscoped(),
            PlayerEvent::TimePos(_)
                | PlayerEvent::Duration(_)
                | PlayerEvent::Paused(_)
                | PlayerEvent::Metadata(_)
                | PlayerEvent::Chapters(_)
                | PlayerEvent::CacheTime(_)
                | PlayerEvent::AudioCodec(_)
                | PlayerEvent::FileFormat(_)
        )
    }
}

fn deck_closed_proof(from_extra: bool, event: &PlayerEvent) -> Option<ExtraProof> {
    matches!(event.unscoped(), PlayerEvent::TransportClosed(_))
        .then_some(ExtraProof::DeckClosed { from_extra })
}
