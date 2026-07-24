//! End-to-end encrypted, multi-device personal-state synchronization.
//!
//! The network layer only moves authenticated [`EncryptedObject`] values. Credentials, endpoint
//! configuration, merge policy, and live-state publication remain in separate owners so a WebDAV
//! failure cannot bypass the local-first persistence path.

mod batch;
mod checkpoint;
mod crypto;
mod error;
mod local_state;
pub mod manual;
mod membership;
mod pairing;
mod private_store;
mod profile;
mod recovery;
pub mod service;
mod transport;
pub mod webdav;
mod worker;

pub use batch::{
    BatchAcceptance, BatchAnchor, OperationBatchPayload, SignedOperationBatch,
    apply_operation_batch,
};
pub use checkpoint::{
    CheckpointAcceptance, CheckpointAnchor, CheckpointBatchAnchor, CheckpointPayload,
    SignedCheckpoint,
};
pub use crypto::{
    DeviceSecretMaterial, EncryptedObject, decrypt_json_with_identity, encrypt_json_to_recipients,
};
pub use error::VaultError;
pub use local_state::{
    SyncAuditAction, SyncAuditEntry, SyncAuditOutcome, SyncAuditStore, SyncFailureKind, SyncHealth,
    SyncHealthState, SyncHealthStore,
};
pub use membership::{
    MembershipAction, MembershipActor, MembershipAnchor, MembershipChain, MembershipChangePayload,
    MembershipRootPayload, RecoveryCutoff, SignedMembershipChange, SignedMembershipRoot,
    VerifiedMembership,
};
pub use pairing::{
    ApprovedPairing, PairingApprovalPayload, PairingCode, PairingInvite, PairingRequestPayload,
    SealedPairingApproval, SealedPairingRequest,
};
pub use private_store::{
    EnrollmentState, PendingPairing, PrivateStore, PrivateStoreSnapshot, VaultCredential,
    VaultCredentialKind,
};
pub use profile::{MAX_CUSTOM_CA_PEM_BYTES, SyncPaths, WebDavProfile, WebDavProfileStore};
pub use recovery::{RecoveryKit, RecoveryResult};
pub use transport::{
    FileVaultTransport, ListCost, ListLimits, ListOutcome, ObjectCondition, ObjectKey,
    ObjectMetadata, ObjectWriteResult, VaultDeadline, VaultTransport,
};
pub(crate) use worker::spawn_detached_prepare;

pub const VAULT_SCHEMA_VERSION: u32 = 1;
pub const MAX_VAULT_PAYLOAD_BYTES: usize = crate::data_export::EXPORT_MAX_BYTES as usize;
pub const MAX_DEVICES: usize = 256;
pub const MAX_BATCH_OPERATIONS: usize = 512;
pub const MAX_BATCH_PLAINTEXT_BYTES: usize = 1024 * 1024;

#[cfg(test)]
mod legacy_enrollment_tests;
#[cfg(test)]
mod tests;
