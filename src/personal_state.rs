//! Mergeable, portable personal and recommendation state.
//!
//! The v2 ledger is the synchronization source of truth. Existing runtime stores remain
//! projections so the playback and recommendation paths do not depend on a network backend.

mod coordinator;
mod import;
pub(crate) mod legacy;
mod model;
mod reducer;
mod transaction;

pub(crate) use coordinator::reconcile_runtime;
pub use coordinator::{append_operation_as, reconcile_runtime_as};
pub use import::{ImportPlan, ImportSummary, plan_import};
pub use legacy::{LegacyProjection, legacy_state};
#[cfg(test)]
pub(crate) use model::derive_device_registry;
pub(crate) use model::refresh_device_registry;
pub use model::{
    CausalStamp, CompactionCheckpoint, DeviceId, DevicePublicIdentity, DeviceRecord,
    DeviceRegistry, Dot, EngagementKind, Operation, OperationEnvelope, OperationOrigin,
    PERSONAL_STATE_KIND, PERSONAL_STATE_SCHEMA_VERSION, PersonalStateError, PersonalStateMetadata,
    PersonalStateV2, PlaylistEntryId, PlaylistId, PortableTrack, PortableTrackKey, Rating,
    VersionVector,
};
pub(crate) use reducer::runtime_fingerprint;
pub use reducer::{MergeSummary, PersonalProjection, merge, project};
pub(crate) use transaction::load_ledger;
pub use transaction::{PersonalStateCommit, PersonalStatePaths, recover_pending_transactions};

#[cfg(test)]
mod tests;
