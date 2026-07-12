//! Bounded, latest-value delivery for live Gemini model changes.
//!
//! Model changes are control-plane state: dropping one because the ordinary AI work
//! inbox is full would leave the persisted/UI choice ahead of the actor. A watch
//! channel provides a single bounded retry slot, while generations keep later AI work
//! from overtaking an update that the actor has not applied yet.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

use super::GeminiModel;
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelUpdate {
    generation: u64,
    model: GeminiModel,
}

struct SenderState {
    tx: watch::Sender<ModelUpdate>,
    requested_generation: u64,
    next_generation: u64,
}

pub(super) struct ModelUpdateSender {
    state: Mutex<SenderState>,
    applied_generation: Arc<AtomicU64>,
}

pub(super) struct ModelUpdateReceiver {
    rx: watch::Receiver<ModelUpdate>,
    applied_generation: Arc<AtomicU64>,
}

pub(super) fn channel(initial_model: GeminiModel) -> (ModelUpdateSender, ModelUpdateReceiver) {
    let initial = ModelUpdate {
        generation: 0,
        model: initial_model,
    };
    let (tx, rx) = watch::channel(initial);
    let applied_generation = Arc::new(AtomicU64::new(0));
    (
        ModelUpdateSender {
            state: Mutex::new(SenderState {
                tx,
                requested_generation: 0,
                next_generation: 0,
            }),
            applied_generation: Arc::clone(&applied_generation),
        },
        ModelUpdateReceiver {
            rx,
            applied_generation,
        },
    )
}

impl ModelUpdateSender {
    fn lock_state(&self) -> MutexGuard<'_, SenderState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                tracing::warn!("recovering poisoned AI model-control mutex");
                poisoned.into_inner()
            }
        }
    }

    /// Admit ordinary work only after the actor has observed the latest model update.
    /// Holding the same lock used by [`Self::send`] also defines ordering for callers
    /// that share an [`crate::ai::AiHandle`] across threads.
    pub(super) fn send_work(&self, submit: impl FnOnce() -> DeliveryResult) -> DeliveryResult {
        let state = self.lock_state();
        let applied = self.applied_generation.load(Ordering::Acquire);
        if state.requested_generation > applied {
            return Err(DeliveryError::Busy);
        }
        submit()
    }

    /// Store the newest requested model in the single retry slot. A full ordinary
    /// work inbox is intentionally irrelevant to this path.
    pub(super) fn send(&self, model: GeminiModel) -> DeliveryResult {
        let mut state = self.lock_state();
        let applied = self.applied_generation.load(Ordering::Acquire);
        let replaced_existing = state.requested_generation > applied;
        let Some(generation) = state.next_generation.checked_add(1) else {
            tracing::error!("AI model-control generation exhausted");
            return Err(DeliveryError::Saturated);
        };
        let update = ModelUpdate { generation, model };
        state.tx.send(update).map_err(|_| DeliveryError::Closed)?;
        state.next_generation = generation;
        state.requested_generation = generation;
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing,
            evicted_oldest: false,
        })
    }

    #[cfg(test)]
    pub(super) fn applied_generation(&self) -> u64 {
        self.applied_generation.load(Ordering::Acquire)
    }
}

impl ModelUpdateReceiver {
    pub(super) async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.rx.changed().await
    }

    /// Apply/acknowledge the newest value. If several settings changes arrived while
    /// the actor was busy, `borrow_and_update` skips directly to the latest one.
    pub(super) fn take_latest(&mut self) -> GeminiModel {
        let update = *self.rx.borrow_and_update();
        self.applied_generation
            .store(update.generation, Ordering::Release);
        update.model
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiCmd, AiHandle};
    use tokio::sync::mpsc;

    fn test_handle(
        capacity: usize,
        initial_model: GeminiModel,
    ) -> (AiHandle, mpsc::Receiver<AiCmd>, ModelUpdateReceiver) {
        let (tx, rx) = mpsc::channel(capacity);
        let (model_updates, model_rx) = channel(initial_model);
        (AiHandle { tx, model_updates }, rx, model_rx)
    }

    #[test]
    fn updates_coalesce_and_gate_work_until_the_latest_is_applied() {
        let (sender, mut receiver) = channel(GeminiModel::FlashLite);

        assert_eq!(
            sender.send(GeminiModel::Flash),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: false,
                evicted_oldest: false,
            })
        );
        assert_eq!(
            sender.send(GeminiModel::Latest),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        );
        assert_eq!(
            sender.send_work(|| Ok(DeliveryReceipt::Enqueued)),
            Err(DeliveryError::Busy),
            "later work must not overtake a pending model update"
        );

        assert_eq!(receiver.take_latest(), GeminiModel::Latest);
        assert_eq!(
            sender.send_work(|| Ok(DeliveryReceipt::Enqueued)),
            Ok(DeliveryReceipt::Enqueued)
        );
    }

    #[test]
    fn closed_update_slot_does_not_latch_a_phantom_pending_generation() {
        let (sender, receiver) = channel(GeminiModel::FlashLite);
        drop(receiver);

        assert_eq!(sender.send(GeminiModel::Latest), Err(DeliveryError::Closed));
        assert_eq!(
            sender.send_work(|| Ok(DeliveryReceipt::Enqueued)),
            Ok(DeliveryReceipt::Enqueued),
            "a rejected update must not leave later work permanently busy"
        );
    }

    #[test]
    fn handle_retries_and_replaces_model_when_the_work_inbox_is_full() {
        let (handle, mut rx, mut model_rx) = test_handle(1, GeminiModel::FlashLite);
        assert!(
            handle
                .rerank("queued".to_owned(), "work".to_owned())
                .is_ok()
        );
        assert_eq!(
            handle.summarize_feedback("full".to_owned()),
            Err(DeliveryError::Busy),
            "ordinary AI work keeps bounded-inbox Busy semantics"
        );
        assert_eq!(
            handle.set_model(GeminiModel::Flash),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: false,
                evicted_oldest: false,
            })
        );
        assert_eq!(
            handle.set_model(GeminiModel::Latest),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        );

        assert!(matches!(rx.try_recv(), Ok(AiCmd::Rerank { .. })));
        assert_eq!(
            handle.rerank("later".to_owned(), "work".to_owned()),
            Err(DeliveryError::Busy),
            "free queue capacity must not let work overtake the pending model"
        );
        assert_eq!(model_rx.take_latest(), GeminiModel::Latest);
        assert!(
            handle
                .rerank("after".to_owned(), "apply".to_owned())
                .is_ok()
        );
    }

    #[test]
    fn handle_reports_closed_work_and_model_queues() {
        let (handle, rx, model_rx) = test_handle(1, GeminiModel::FlashLite);
        drop(rx);
        drop(model_rx);

        assert_eq!(
            handle.rerank("closed".to_owned(), "work".to_owned()),
            Err(DeliveryError::Closed)
        );
        assert_eq!(
            handle.set_model(GeminiModel::Latest),
            Err(DeliveryError::Closed)
        );
        assert_eq!(
            handle.rerank("still".to_owned(), "closed".to_owned()),
            Err(DeliveryError::Closed),
            "a rejected model update must not create a phantom Busy state"
        );
    }
}
