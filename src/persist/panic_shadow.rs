use std::sync::{PoisonError, RwLock};

use super::{JournalOrder, PanicOwnedOperation, StoreKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PanicShadowSealed;

struct PanicShadowState {
    phase: PanicShadowPhase,
    slots: [Option<PanicOwnedOperation>; 8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PanicShadowPhase {
    Publishing,
    Sealed,
}

/// Fixed-size newest-owned persistence frontier read by the panic hook.
///
/// Expensive or fallible preparation happens outside this lock. Slots contain Arc-backed
/// operations, so the critical section is limited to order comparison and pointer-count swaps;
/// inherited panic-hook work can therefore wait out a concurrent publisher without observing a
/// torn pending/in-flight transition. A same-order prepared operation supersedes its pending form.
pub(super) struct PanicShadow {
    state: RwLock<PanicShadowState>,
}

impl PanicShadow {
    pub(super) fn new() -> Self {
        Self {
            state: RwLock::new(PanicShadowState {
                phase: PanicShadowPhase::Publishing,
                slots: std::array::from_fn(|_| None),
            }),
        }
    }

    /// Publish one operation unless the panic hook has already fixed its one-shot frontier.
    #[must_use = "a sealed panic frontier rejects new persistence ownership"]
    pub(super) fn publish(&self, operation: PanicOwnedOperation) -> Result<(), PanicShadowSealed> {
        self.publish_with_lock_hook(operation, || {})
    }

    fn publish_with_lock_hook(
        &self,
        operation: PanicOwnedOperation,
        after_phase_check: impl FnOnce(),
    ) -> Result<(), PanicShadowSealed> {
        let index = slot(operation.kind());
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        if state.phase == PanicShadowPhase::Sealed {
            drop(state);
            drop(operation);
            return Err(PanicShadowSealed);
        }
        after_phase_check();
        let retired = replace_slot_if_newer(&mut state.slots[index], operation);
        drop(state);
        drop(retired);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn publish_with_lock_hook_for_test(
        &self,
        operation: PanicOwnedOperation,
        after_phase_check: impl FnOnce(),
    ) -> Result<(), PanicShadowSealed> {
        self.publish_with_lock_hook(operation, after_phase_check)
    }

    /// Atomically publish an owner's complete final batch or reject the whole batch.
    #[must_use = "a sealed panic frontier rejects new persistence ownership"]
    pub(super) fn publish_batch(
        &self,
        operations: Vec<PanicOwnedOperation>,
    ) -> Result<(), PanicShadowSealed> {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        if state.phase == PanicShadowPhase::Sealed {
            drop(state);
            drop(operations);
            return Err(PanicShadowSealed);
        }
        let mut retired = Vec::with_capacity(operations.len());
        for operation in operations {
            let index = slot(operation.kind());
            if let Some(operation) = replace_slot_if_newer(&mut state.slots[index], operation) {
                retired.push(operation);
            }
        }
        drop(state);
        drop(retired);
        Ok(())
    }

    pub(super) fn clear_through(&self, kind: StoreKind, order: JournalOrder) {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        let retired = state.slots[slot(kind)]
            .as_ref()
            .is_some_and(|operation| operation.order() <= order)
            .then(|| state.slots[slot(kind)].take())
            .flatten();
        drop(state);
        drop(retired);
    }

    /// Clone the current durability frontier without closing panic-time publication.
    ///
    /// Clean shutdown uses this after ordinary admission is sealed. Every returned Arc remains
    /// hook-owned until its write is confirmed and [`Self::clear_through`] retires it, so a timed
    /// out fallback can truthfully return while preserving recovery ownership.
    pub(super) fn snapshot(&self) -> [Option<PanicOwnedOperation>; 8] {
        let state = self.state.read().unwrap_or_else(PoisonError::into_inner);
        std::array::from_fn(|index| state.slots[index].clone())
    }

    /// Atomically close every publisher and take the panic hook's frontier exactly once.
    #[must_use = "only the first panic hook owns the sealed persistence frontier"]
    pub(super) fn seal_and_snapshot(
        &self,
    ) -> Result<[Option<PanicOwnedOperation>; 8], PanicShadowSealed> {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        if state.phase == PanicShadowPhase::Sealed {
            return Err(PanicShadowSealed);
        }
        state.phase = PanicShadowPhase::Sealed;
        Ok(std::array::from_fn(|index| state.slots[index].clone()))
    }

    #[cfg(test)]
    pub(super) fn peek_for_test(&self) -> [Option<PanicOwnedOperation>; 8] {
        self.snapshot()
    }
}

fn replace_slot_if_newer(
    slot: &mut Option<PanicOwnedOperation>,
    incoming: PanicOwnedOperation,
) -> Option<PanicOwnedOperation> {
    let replace =
        slot.as_ref()
            .is_none_or(|current| match incoming.order().cmp(&current.order()) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => matches!(
                    (&incoming, current),
                    (
                        PanicOwnedOperation::Prepared(_),
                        PanicOwnedOperation::Pending(_)
                    )
                ),
            });
    if replace {
        slot.replace(incoming)
    } else {
        Some(incoming)
    }
}

const fn slot(kind: StoreKind) -> usize {
    match kind {
        StoreKind::Library => 0,
        StoreKind::Signals => 1,
        StoreKind::Downloads => 2,
        StoreKind::Config => 3,
        StoreKind::Playlists => 4,
        StoreKind::Station => 5,
        StoreKind::RomanizedTitles => 6,
        StoreKind::Session => 7,
    }
}
