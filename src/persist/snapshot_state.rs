use std::collections::HashMap;
use std::ops::Deref;
#[cfg(test)]
use std::ops::Index;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::{
    AcceptedJournalOrder, JournalIntent, JournalOrder, OwnedSnapshot, PanicOwnedOperation,
    PanicShadow, PanicShadowSealed, Snapshot, StoreKind, debounce,
};

pub(super) enum PendingAction {
    Save(Arc<OwnedSnapshot>),
    DeleteRomanizedTitles,
    #[cfg(test)]
    TestDeleteRomanizedTitles {
        deleter: Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SnapshotPublication {
    OrderingUnavailable(Arc<str>),
    NeedsJournal,
    JournalResolved,
}

impl SnapshotPublication {
    fn from_accepted(accepted: &AcceptedJournalOrder) -> Self {
        match &accepted.error {
            Some(error) => Self::OrderingUnavailable(Arc::clone(error)),
            None => Self::NeedsJournal,
        }
    }

    pub(super) fn ensure_ordering(&self) -> std::io::Result<()> {
        match self {
            Self::OrderingUnavailable(error) => Err(std::io::Error::other(error.to_string())),
            Self::NeedsJournal | Self::JournalResolved => Ok(()),
        }
    }

    pub(super) fn needs_journal(&self) -> bool {
        matches!(self, Self::NeedsJournal)
    }

    fn resolve_journal(&mut self) {
        if matches!(self, Self::NeedsJournal) {
            *self = Self::JournalResolved;
        }
    }
}

#[derive(Clone)]
pub(super) struct PendingOperation {
    pub(super) order: JournalOrder,
    kind: StoreKind,
    publication: SnapshotPublication,
    action: Arc<PendingAction>,
}

impl PendingOperation {
    pub(super) fn save(snapshot: Snapshot, accepted: AcceptedJournalOrder) -> Self {
        let publication = SnapshotPublication::from_accepted(&accepted);
        let snapshot = OwnedSnapshot::from(snapshot);
        let kind = snapshot.kind();
        Self {
            order: accepted.order,
            kind,
            publication,
            action: Arc::new(PendingAction::Save(Arc::new(snapshot))),
        }
    }

    pub(super) fn new(action: PendingAction, accepted: AcceptedJournalOrder) -> Self {
        let publication = SnapshotPublication::from_accepted(&accepted);
        Self {
            order: accepted.order,
            kind: StoreKind::RomanizedTitles,
            publication,
            action: Arc::new(action),
        }
    }

    pub(super) fn kind(&self) -> StoreKind {
        self.kind
    }

    pub(super) fn action(&self) -> &PendingAction {
        self.action.as_ref()
    }

    pub(super) fn label(&self) -> &'static str {
        match self.action.as_ref() {
            PendingAction::Save(snapshot) => snapshot.label(),
            PendingAction::DeleteRomanizedTitles => StoreKind::RomanizedTitles.label(),
            #[cfg(test)]
            PendingAction::TestDeleteRomanizedTitles { .. } => "romanized title cache delete",
        }
    }

    pub(super) fn storage_path(&self) -> Option<PathBuf> {
        match self.action.as_ref() {
            PendingAction::Save(snapshot) => snapshot.storage_path(),
            PendingAction::DeleteRomanizedTitles => crate::romanize::cache_path(),
            #[cfg(test)]
            PendingAction::TestDeleteRomanizedTitles { .. } => None,
        }
    }

    pub(super) fn write(&self) -> std::io::Result<()> {
        match self.action.as_ref() {
            PendingAction::Save(snapshot) => snapshot.write(),
            PendingAction::DeleteRomanizedTitles => crate::romanize::RomanizeCache::delete_saved(),
            #[cfg(test)]
            PendingAction::TestDeleteRomanizedTitles { deleter } => deleter(),
        }
    }

    pub(super) fn publication(&self) -> &SnapshotPublication {
        &self.publication
    }

    #[cfg(test)]
    pub(super) fn resolve_journal_for_test(&mut self) {
        self.publication.resolve_journal();
    }

    pub(super) fn debounce(&self) -> Duration {
        match self.action.as_ref() {
            PendingAction::Save(_) => debounce(self.kind),
            PendingAction::DeleteRomanizedTitles => Duration::ZERO,
            #[cfg(test)]
            PendingAction::TestDeleteRomanizedTitles { .. } => Duration::ZERO,
        }
    }

    pub(super) fn journal_intent(&self) -> Option<JournalIntent> {
        if !self.publication.needs_journal() {
            return None;
        }
        match self.action.as_ref() {
            PendingAction::Save(snapshot) => {
                let path = snapshot.storage_path()?;
                let bytes = match snapshot.to_json_bytes() {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(
                            store = snapshot.kind().label(),
                            error = %error,
                            "failed to encode persistence intent"
                        );
                        return None;
                    }
                };
                Some(JournalIntent::Replace {
                    order: self.order,
                    kind: snapshot.kind(),
                    path,
                    bytes,
                })
            }
            PendingAction::DeleteRomanizedTitles => Some(JournalIntent::Delete {
                order: self.order,
                kind: StoreKind::RomanizedTitles,
                path: crate::romanize::cache_path()?,
            }),
            #[cfg(test)]
            PendingAction::TestDeleteRomanizedTitles { .. } => None,
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Option<&OwnedSnapshot> {
        match self.action.as_ref() {
            PendingAction::Save(snapshot) => Some(snapshot),
            PendingAction::DeleteRomanizedTitles
            | PendingAction::TestDeleteRomanizedTitles { .. } => None,
        }
    }
}

/// A pending-map value covered by an equal or newer panic-time owner.
///
/// The constructor is intentionally private to the publication helpers below. Actor code can
/// move or requeue this value, but cannot create pending work that the panic hook does not own.
#[derive(Clone)]
pub(super) struct ShadowCoveredOperation(PendingOperation);

impl ShadowCoveredOperation {
    #[cfg(test)]
    pub(super) fn for_test(operation: PendingOperation) -> Self {
        Self(operation)
    }

    #[cfg(test)]
    pub(super) fn pending_clone(&self) -> PendingOperation {
        self.0.clone()
    }
}

impl Deref for ShadowCoveredOperation {
    type Target = PendingOperation;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub(super) fn publish_pending_operation(
    shadow: &PanicShadow,
    operation: PendingOperation,
) -> Result<ShadowCoveredOperation, PanicShadowSealed> {
    shadow.publish(PanicOwnedOperation::Pending(Arc::new(operation.clone())))?;
    Ok(ShadowCoveredOperation(operation))
}

pub(super) fn publish_pending_batch(
    shadow: &PanicShadow,
    operations: Vec<PendingOperation>,
) -> Result<Vec<ShadowCoveredOperation>, PanicShadowSealed> {
    let shadow_operations = operations
        .iter()
        .cloned()
        .map(|operation| PanicOwnedOperation::Pending(Arc::new(operation)))
        .collect();
    shadow.publish_batch(shadow_operations)?;
    Ok(operations.into_iter().map(ShadowCoveredOperation).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct JournalCompletion {
    kind: StoreKind,
    order: JournalOrder,
}

impl JournalCompletion {
    pub(super) fn confirmed(kind: StoreKind, order: JournalOrder) -> Self {
        Self { kind, order }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SnapshotAdmission {
    Open,
    Sealed,
}

pub(super) struct PendingQueue {
    admission: SnapshotAdmission,
    operations: HashMap<StoreKind, ShadowCoveredOperation>,
    /// Monotonic acceptance frontier retained even after an operation leaves pending. Reading
    /// this under the same mutex as insertion linearizes targeted confirmation with admission.
    latest_accepted: HashMap<StoreKind, JournalOrder>,
}

impl PendingQueue {
    pub(super) fn new() -> Self {
        Self {
            admission: SnapshotAdmission::Open,
            operations: HashMap::new(),
            latest_accepted: HashMap::new(),
        }
    }

    pub(super) fn admission(&self) -> SnapshotAdmission {
        self.admission
    }

    pub(super) fn seal(&mut self) {
        self.admission = SnapshotAdmission::Sealed;
    }

    pub(super) fn insert_owned(
        &mut self,
        operation: ShadowCoveredOperation,
    ) -> Option<ShadowCoveredOperation> {
        self.latest_accepted
            .entry(operation.kind())
            .and_modify(|order| *order = (*order).max(operation.order))
            .or_insert(operation.order);
        self.operations.insert(operation.kind(), operation)
    }

    pub(super) fn latest_accepted(&self, kind: &StoreKind) -> Option<JournalOrder> {
        self.latest_accepted.get(kind).copied()
    }

    pub(super) fn resolve_journal(&mut self, completion: JournalCompletion) -> bool {
        let Some(operation) = self.operations.get_mut(&completion.kind) else {
            return false;
        };
        if operation.order != completion.order {
            return false;
        }
        operation.0.publication.resolve_journal();
        true
    }

    pub(super) fn get(&self, kind: &StoreKind) -> Option<&ShadowCoveredOperation> {
        self.operations.get(kind)
    }

    pub(super) fn values(
        &self,
    ) -> std::collections::hash_map::Values<'_, StoreKind, ShadowCoveredOperation> {
        self.operations.values()
    }

    pub(super) fn iter(
        &self,
    ) -> std::collections::hash_map::Iter<'_, StoreKind, ShadowCoveredOperation> {
        self.operations.iter()
    }

    pub(super) fn keys(
        &self,
    ) -> std::collections::hash_map::Keys<'_, StoreKind, ShadowCoveredOperation> {
        self.operations.keys()
    }

    pub(super) fn contains_key(&self, kind: &StoreKind) -> bool {
        self.operations.contains_key(kind)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.operations.len()
    }

    pub(super) fn remove(&mut self, kind: &StoreKind) -> Option<ShadowCoveredOperation> {
        self.operations.remove(kind)
    }

    #[cfg(test)]
    pub(super) fn insert(
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
