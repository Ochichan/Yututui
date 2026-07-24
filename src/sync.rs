//! End-to-end encrypted, multi-device personal-state primitives.
//!
//! This module deliberately contains no network client. PR 2 establishes the authenticated
//! vault format, device lifecycle, private key storage, and a file-backed conformance transport;
//! the WebDAV adapter in the next layer can only move [`EncryptedObject`] values.

mod batch;
mod checkpoint;
mod crypto;
mod error;
mod membership;
mod pairing;
mod private_store;
mod recovery;
mod transport;

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
pub use recovery::{RecoveryKit, RecoveryResult};
pub use transport::{
    FileVaultTransport, ObjectCondition, ObjectKey, ObjectMetadata, ObjectWriteResult,
    VaultTransport,
};

pub const VAULT_SCHEMA_VERSION: u32 = 1;
pub const MAX_VAULT_PAYLOAD_BYTES: usize = crate::data_export::EXPORT_MAX_BYTES as usize;
pub const MAX_DEVICES: usize = 256;
pub const MAX_BATCH_OPERATIONS: usize = 512;
pub const MAX_BATCH_PLAINTEXT_BYTES: usize = 1024 * 1024;

#[cfg(test)]
mod legacy_enrollment_tests;
#[cfg(test)]
mod tests;
