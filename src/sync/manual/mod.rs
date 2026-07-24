//! Transport-agnostic, user-triggered encrypted vault synchronization.
//!
//! This layer owns the immutable object protocol and merge/retry rules. Network policy and
//! WebDAV request construction live in the adapter; automatic scheduling is intentionally a
//! later layer.

mod engine;
mod protocol;

use crate::personal_state::DeviceId;

use super::{ObjectKey, VaultError};

pub use engine::{
    LocalRevisionGuard, ManualSyncBudget, ManualSyncCandidate, ManualSyncEngine, ManualSyncInput,
    ManualSyncSummary,
};
pub use protocol::{
    DeviceHeadPayload, SignedDeviceHead, SignedVaultManifest, VaultManifestPayload,
};

const VAULT_ROOT: &str = "yututui/v2";

pub fn manifest_key(dataset_id: &str) -> Result<ObjectKey, VaultError> {
    ObjectKey::new(format!("{VAULT_ROOT}/{dataset_id}/manifest"))
}

pub fn registry_key(dataset_id: &str, membership_hash: &str) -> Result<ObjectKey, VaultError> {
    ObjectKey::new(format!(
        "{VAULT_ROOT}/{dataset_id}/registry/{membership_hash}.age"
    ))
}

pub fn segment_prefix(dataset_id: &str, device_id: &DeviceId) -> Result<ObjectKey, VaultError> {
    ObjectKey::new(format!(
        "{VAULT_ROOT}/{dataset_id}/devices/{}/segments",
        device_id.as_str()
    ))
}

pub fn segment_key(
    dataset_id: &str,
    device_id: &DeviceId,
    first_sequence: u64,
    last_sequence: u64,
) -> Result<ObjectKey, VaultError> {
    ObjectKey::new(format!(
        "{}/{first_sequence}-{last_sequence}.age",
        segment_prefix(dataset_id, device_id)?.as_str()
    ))
}

pub fn device_head_key(dataset_id: &str, device_id: &DeviceId) -> Result<ObjectKey, VaultError> {
    ObjectKey::new(format!(
        "{VAULT_ROOT}/{dataset_id}/devices/{}/head.age",
        device_id.as_str()
    ))
}

pub fn checkpoint_prefix(dataset_id: &str) -> Result<ObjectKey, VaultError> {
    ObjectKey::new(format!("{VAULT_ROOT}/{dataset_id}/checkpoints"))
}

pub fn checkpoint_key(
    dataset_id: &str,
    epoch: u64,
    checkpoint_hash: &str,
) -> Result<ObjectKey, VaultError> {
    ObjectKey::new(format!(
        "{VAULT_ROOT}/{dataset_id}/checkpoints/{epoch}/{checkpoint_hash}.age"
    ))
}

#[cfg(test)]
mod tests;
