use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use super::{VaultError, manual::LocalRevisionGuard};

/// A live revision fence shared by the primary owner and detached sync workers.
///
/// The owner publishes every accepted ledger revision. Workers retain a clone and check it
/// immediately before each remote mutation, so a newer local generation retires stale network
/// work before it can publish or delete more vault objects.
#[derive(Debug)]
pub struct OwnerRevisionGuard {
    shared: Arc<OwnerRevisionState>,
    expected_generation: u64,
}

#[derive(Debug, Default)]
struct OwnerRevisionState {
    revision: AtomicU64,
    generation: AtomicU64,
    poisoned: AtomicBool,
}

impl Clone for OwnerRevisionGuard {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            expected_generation: self.shared.generation.load(Ordering::Acquire),
        }
    }
}

impl Default for OwnerRevisionGuard {
    fn default() -> Self {
        Self::new(0)
    }
}

impl OwnerRevisionGuard {
    pub fn new(revision: u64) -> Self {
        Self {
            shared: Arc::new(OwnerRevisionState {
                revision: AtomicU64::new(revision),
                generation: AtomicU64::new(0),
                poisoned: AtomicBool::new(false),
            }),
            expected_generation: 0,
        }
    }

    /// Publish one accepted owner content replacement.
    ///
    /// Generation changes even when a corrupt or legacy caller tries to reuse the same revision,
    /// so an already detached worker fails closed. Production personal-state mutation paths also
    /// reject revision exhaustion before reaching this defensive fence.
    pub fn publish(&self, revision: u64) {
        if self
            .shared
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
            .is_err()
        {
            self.shared.poisoned.store(true, Ordering::Release);
        }
        self.shared.revision.store(revision, Ordering::Release);
    }

    pub fn current(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }
}

impl LocalRevisionGuard for OwnerRevisionGuard {
    fn ensure_current(&self, expected_revision: u64) -> Result<(), VaultError> {
        if !self.shared.poisoned.load(Ordering::Acquire)
            && self.shared.generation.load(Ordering::Acquire) == self.expected_generation
            && self.current() == expected_revision
        {
            Ok(())
        } else {
            Err(VaultError::RevisionConflict)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_observe_owner_revision_changes() {
        let owner = OwnerRevisionGuard::new(7);
        let worker = owner.clone();

        assert_eq!(worker.ensure_current(7), Ok(()));
        owner.publish(8);
        assert_eq!(worker.ensure_current(7), Err(VaultError::RevisionConflict));
        assert_eq!(worker.ensure_current(8), Err(VaultError::RevisionConflict));
        assert_eq!(owner.clone().ensure_current(8), Ok(()));
    }

    #[test]
    fn same_revision_content_replacement_retires_an_existing_worker() {
        let owner = OwnerRevisionGuard::new(u64::MAX);
        let worker = owner.clone();

        owner.publish(u64::MAX);

        assert_eq!(
            worker.ensure_current(u64::MAX),
            Err(VaultError::RevisionConflict)
        );
        assert_eq!(owner.clone().ensure_current(u64::MAX), Ok(()));
    }
}
