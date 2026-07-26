//! Cross-owner durability acknowledgements for exact OpenSubsonic submissions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::util::delivery::DeliveryError;

/// Secret-free confirmation lane for a durable OpenSubsonic submission.
///
/// The source journal first records that handoff began, then that the bridge queued the event.
/// It removes the marker only after the bridge durably acknowledges the source transition.
#[derive(Clone)]
pub struct OpenSubsonicSubmissionAck {
    event_id: String,
    tx: tokio::sync::mpsc::Sender<OpenSubsonicMarkerAckRequest>,
    bridge_marker_durability: Arc<AtomicU8>,
    removal_durability: Arc<AtomicU8>,
    scope_retirement_durability: Arc<AtomicU8>,
    deferred_submission: Arc<AtomicBool>,
}

pub(super) const MARKER_ACK_IDLE: u8 = 0;
pub(super) const MARKER_ACK_QUEUED: u8 = 1;
pub(super) const MARKER_ACK_DURABLE: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum OpenSubsonicMarkerAckStage {
    BridgeQueued,
    SourceAcknowledged,
    ScopeRetired,
}

pub(crate) struct OpenSubsonicMarkerAckRequest {
    pub(super) event_id: String,
    pub(super) stage: OpenSubsonicMarkerAckStage,
    pub(super) durability: Arc<AtomicU8>,
}

impl OpenSubsonicMarkerAckRequest {
    pub(super) fn retry(self) {
        self.durability.store(MARKER_ACK_IDLE, Ordering::Release);
    }

    fn mark_durable(self) {
        self.durability.store(MARKER_ACK_DURABLE, Ordering::Release);
    }
}

impl OpenSubsonicSubmissionAck {
    #[cfg(test)]
    pub(crate) fn new(
        event_id: String,
        tx: tokio::sync::mpsc::Sender<OpenSubsonicMarkerAckRequest>,
    ) -> Self {
        Self::with_bridge_marker_state(event_id, tx, false, Arc::new(AtomicBool::new(false)))
    }

    #[cfg(test)]
    pub(crate) fn new_with_bridge_marker(
        event_id: String,
        tx: tokio::sync::mpsc::Sender<OpenSubsonicMarkerAckRequest>,
    ) -> Self {
        Self::with_bridge_marker_state(event_id, tx, true, Arc::new(AtomicBool::new(false)))
    }

    pub(super) fn with_bridge_marker_state(
        event_id: String,
        tx: tokio::sync::mpsc::Sender<OpenSubsonicMarkerAckRequest>,
        bridge_marker_durable: bool,
        deferred_submission: Arc<AtomicBool>,
    ) -> Self {
        Self {
            event_id,
            tx,
            bridge_marker_durability: Arc::new(AtomicU8::new(if bridge_marker_durable {
                MARKER_ACK_DURABLE
            } else {
                MARKER_ACK_IDLE
            })),
            removal_durability: Arc::new(AtomicU8::new(MARKER_ACK_IDLE)),
            scope_retirement_durability: Arc::new(AtomicU8::new(MARKER_ACK_IDLE)),
            deferred_submission,
        }
    }

    /// Ask the source actor to re-announce this durable event in the current process.
    pub(crate) fn defer_submission(&self) {
        self.deferred_submission.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn submission_is_deferred(&self) -> bool {
        self.deferred_submission.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn take_deferred_submission(&self) -> bool {
        self.deferred_submission.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn confirm_bridge_durable(&self) -> Result<(), DeliveryError> {
        self.request(
            OpenSubsonicMarkerAckStage::BridgeQueued,
            &self.bridge_marker_durability,
        )
    }

    pub(crate) fn bridge_marker_is_durable(&self) -> bool {
        self.bridge_marker_durability.load(Ordering::Acquire) == MARKER_ACK_DURABLE
    }

    pub(crate) fn confirm_source_acknowledged(&self) -> Result<(), DeliveryError> {
        self.request(
            OpenSubsonicMarkerAckStage::SourceAcknowledged,
            &self.removal_durability,
        )
    }

    pub(crate) fn source_marker_is_removed(&self) -> bool {
        self.removal_durability.load(Ordering::Acquire) == MARKER_ACK_DURABLE
    }

    /// Retire a marker whose exact server/account identity no longer belongs to the active
    /// profile. This is a distinct, audited source-journal outcome, never a fake server ack.
    pub(crate) fn retire_account_scope(&self) -> Result<(), DeliveryError> {
        self.request(
            OpenSubsonicMarkerAckStage::ScopeRetired,
            &self.scope_retirement_durability,
        )
    }

    pub(crate) fn account_scope_is_retired(&self) -> bool {
        self.scope_retirement_durability.load(Ordering::Acquire) == MARKER_ACK_DURABLE
    }

    pub(crate) fn account_scope_retirement_is_pending(&self) -> bool {
        self.scope_retirement_durability.load(Ordering::Acquire) == MARKER_ACK_QUEUED
    }

    #[cfg(test)]
    pub(crate) fn mark_account_scope_retired(&self) {
        self.scope_retirement_durability
            .store(MARKER_ACK_DURABLE, Ordering::Release);
    }

    fn request(
        &self,
        stage: OpenSubsonicMarkerAckStage,
        durability: &Arc<AtomicU8>,
    ) -> Result<(), DeliveryError> {
        if durability.load(Ordering::Acquire) == MARKER_ACK_DURABLE {
            return Ok(());
        }
        if durability
            .compare_exchange(
                MARKER_ACK_IDLE,
                MARKER_ACK_QUEUED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(());
        }
        let request = OpenSubsonicMarkerAckRequest {
            event_id: self.event_id.clone(),
            stage,
            durability: Arc::clone(durability),
        };
        match self.tx.try_send(request) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(request)) => {
                request.retry();
                Err(DeliveryError::Busy)
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(request)) => {
                request.retry();
                Err(DeliveryError::Closed)
            }
        }
    }
}

#[derive(Default)]
pub(super) struct PendingOpenSubsonicMarkerAcks {
    pub(super) bridge_queued: Vec<OpenSubsonicMarkerAckRequest>,
    pub(super) source_acknowledged: Vec<OpenSubsonicMarkerAckRequest>,
    pub(super) scope_retired: Vec<OpenSubsonicMarkerAckRequest>,
}

impl PendingOpenSubsonicMarkerAcks {
    pub(super) fn push(&mut self, request: OpenSubsonicMarkerAckRequest) {
        match request.stage {
            OpenSubsonicMarkerAckStage::BridgeQueued => self.bridge_queued.push(request),
            OpenSubsonicMarkerAckStage::SourceAcknowledged => {
                self.source_acknowledged.push(request);
            }
            OpenSubsonicMarkerAckStage::ScopeRetired => self.scope_retired.push(request),
        }
    }

    pub(super) fn mark_durable(self) {
        for request in self
            .bridge_queued
            .into_iter()
            .chain(self.source_acknowledged)
            .chain(self.scope_retired)
        {
            request.mark_durable();
        }
    }

    pub(super) fn retry(self) {
        for request in self
            .bridge_queued
            .into_iter()
            .chain(self.source_acknowledged)
            .chain(self.scope_retired)
        {
            request.retry();
        }
    }
}
