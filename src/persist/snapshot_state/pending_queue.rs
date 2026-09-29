use std::collections::HashMap;
#[cfg(test)]
use std::ops::Index;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::persist) struct JournalCompletion {
    kind: StoreKind,
    order: JournalOrder,
}

impl JournalCompletion {
    pub(in crate::persist) fn confirmed(kind: StoreKind, order: JournalOrder) -> Self {
        Self { kind, order }
    }
}

pub(in crate::persist) struct PendingQueue {
    admission: SnapshotAdmission,
    operations: HashMap<StoreKind, ShadowCoveredOperation>,
    /// Monotonic acceptance frontier retained even after an operation leaves pending. Reading
    /// this under the same mutex as insertion linearizes targeted confirmation with admission.
    latest_accepted: HashMap<StoreKind, JournalOrder>,
}

impl PendingQueue {
    pub(in crate::persist) fn new() -> Self {
        Self {
            admission: SnapshotAdmission::Open,
            operations: HashMap::new(),
            latest_accepted: HashMap::new(),
        }
    }

    pub(in crate::persist) fn admission(&self) -> SnapshotAdmission {
        self.admission
    }

    pub(in crate::persist) fn seal(&mut self) {
        self.admission = SnapshotAdmission::Sealed;
    }

    pub(in crate::persist) fn insert_owned(
        &mut self,
        operation: ShadowCoveredOperation,
    ) -> Option<ShadowCoveredOperation> {
        self.latest_accepted
            .entry(operation.kind())
            .and_modify(|order| *order = (*order).max(operation.order))
            .or_insert(operation.order);
        self.operations.insert(operation.kind(), operation)
    }

    pub(in crate::persist) fn latest_accepted(&self, kind: &StoreKind) -> Option<JournalOrder> {
        self.latest_accepted.get(kind).copied()
    }

    pub(in crate::persist) fn resolve_journal(&mut self, completion: JournalCompletion) -> bool {
        let Some(operation) = self.operations.get_mut(&completion.kind) else {
            return false;
        };
        if operation.order != completion.order {
            return false;
        }
        operation.0.publication.resolve_journal();
        true
    }

    pub(in crate::persist) fn get(&self, kind: &StoreKind) -> Option<&ShadowCoveredOperation> {
        self.operations.get(kind)
    }

    pub(in crate::persist) fn values(
        &self,
    ) -> std::collections::hash_map::Values<'_, StoreKind, ShadowCoveredOperation> {
        self.operations.values()
    }

    pub(in crate::persist) fn iter(
        &self,
    ) -> std::collections::hash_map::Iter<'_, StoreKind, ShadowCoveredOperation> {
        self.operations.iter()
    }

    pub(in crate::persist) fn keys(
        &self,
    ) -> std::collections::hash_map::Keys<'_, StoreKind, ShadowCoveredOperation> {
        self.operations.keys()
    }

    pub(in crate::persist) fn contains_key(&self, kind: &StoreKind) -> bool {
        self.operations.contains_key(kind)
    }

    pub(in crate::persist) fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    #[cfg(test)]
    pub(in crate::persist) fn len(&self) -> usize {
        self.operations.len()
    }

    pub(in crate::persist) fn remove(
        &mut self,
        kind: &StoreKind,
    ) -> Option<ShadowCoveredOperation> {
        self.operations.remove(kind)
    }

    #[cfg(test)]
    pub(in crate::persist) fn insert(
        &mut self,
        kind: StoreKind,
        operation: PendingOperation,
    ) -> Option<ShadowCoveredOperation> {
        let operation = ShadowCoveredOperation::for_test(operation);
        debug_assert_eq!(operation.kind(), kind);
        self.insert_owned(operation)
    }
}

impl Default for PendingQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Index<&StoreKind> for PendingQueue {
    type Output = ShadowCoveredOperation;

    fn index(&self, kind: &StoreKind) -> &Self::Output {
        &self.operations[kind]
    }
}
