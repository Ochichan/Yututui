//! Secret-bearing private-store staging used by the outer sync anchor transaction.

use std::path::Path;

use zeroize::Zeroizing;

use crate::util::safe_fs::{read_owner_only_limited, sync_parent_dir, write_owner_only_atomic};

use super::{
    DiskPrivateStore, MAX_PRIVATE_STORE_BYTES, PrivateStore, PrivateStoreSnapshot, VaultError,
    encoded_snapshot, validate_same_store_identity, validate_snapshot,
};
use crate::sync::crypto::sha256_domain_hex;

const PRIVATE_TRANSITION_PAYLOAD_DOMAIN: &[u8] = b"yututui-vault-private-transition-payload-v1";

/// Non-secret binding for a staged next revision of the owner-only private store.
///
/// The staged bytes themselves can contain credentials and private keys, so this value exposes
/// only revisions and a domain-separated digest and deliberately has no path field.
pub(crate) struct StagedPrivateStoreUpdate {
    expected_revision: u64,
    target_revision: u64,
    payload_hash: String,
}

impl StagedPrivateStoreUpdate {
    pub(crate) fn expected_revision(&self) -> u64 {
        self.expected_revision
    }

    pub(crate) fn target_revision(&self) -> u64 {
        self.target_revision
    }

    pub(crate) fn payload_hash(&self) -> &str {
        &self.payload_hash
    }
}

impl PrivateStore {
    /// Stage, but do not publish, the exact next private-store revision for a cross-store commit.
    pub(crate) fn stage_transition_update(
        &self,
        snapshot: &mut PrivateStoreSnapshot,
        staged_path: &Path,
    ) -> Result<StagedPrivateStoreUpdate, VaultError> {
        if !staged_path.is_absolute() || staged_path.file_name().is_none() {
            return Err(VaultError::InvalidPrivateStore);
        }
        validate_snapshot(snapshot)?;
        let _lock = self.acquire_lock()?;
        let current = self.load_locked()?;
        validate_same_store_identity(&current, snapshot)?;
        if current.revision != snapshot.revision {
            return Err(VaultError::RevisionConflict);
        }
        let expected_revision = snapshot.revision;
        let target_revision = expected_revision
            .checked_add(1)
            .ok_or(VaultError::RevisionConflict)?;
        snapshot.revision = target_revision;
        let bytes = match encoded_snapshot(snapshot) {
            Ok(bytes) => bytes,
            Err(error) => {
                snapshot.revision = expected_revision;
                return Err(error);
            }
        };
        let payload_hash =
            sha256_domain_hex(PRIVATE_TRANSITION_PAYLOAD_DOMAIN, &[bytes.as_slice()]);
        if write_owner_only_atomic(staged_path, &bytes).is_err() {
            snapshot.revision = expected_revision;
            return Err(VaultError::StorageFailed);
        }
        // The staged target revision belongs to the journal, not to the still-unpublished
        // in-memory snapshot. Keeping the caller at the observed revision makes a pre-decision
        // retry an exact compare-and-swap instead of a false stale-snapshot conflict.
        snapshot.revision = expected_revision;
        Ok(StagedPrivateStoreUpdate {
            expected_revision,
            target_revision,
            payload_hash,
        })
    }

    /// Publish a previously staged private-store revision by exact compare-and-swap.
    pub(crate) fn install_transition_update(
        &self,
        staged_path: &Path,
        expected_revision: u64,
        target_revision: u64,
        payload_hash: &str,
    ) -> Result<(), VaultError> {
        if target_revision != expected_revision.saturating_add(1) {
            return Err(VaultError::InvalidPrivateStore);
        }
        let bytes = Zeroizing::new(
            read_owner_only_limited(staged_path, MAX_PRIVATE_STORE_BYTES)
                .map_err(|_| VaultError::InvalidPrivateStore)?,
        );
        if sha256_domain_hex(PRIVATE_TRANSITION_PAYLOAD_DOMAIN, &[bytes.as_slice()]) != payload_hash
        {
            return Err(VaultError::InvalidPrivateStore);
        }
        let target = decoded_snapshot(&bytes)?;
        if target.revision != target_revision {
            return Err(VaultError::InvalidPrivateStore);
        }

        let _lock = self.acquire_lock()?;
        let current = self.load_locked()?;
        if current.revision == target_revision {
            if encoded_snapshot(&current)?.as_slice() != bytes.as_slice() {
                return Err(VaultError::RevisionConflict);
            }
            return sync_parent_dir(&self.path).map_err(|_| VaultError::StorageFailed);
        }
        if current.revision != expected_revision {
            return Err(VaultError::RevisionConflict);
        }
        validate_same_store_identity(&current, &target)?;
        self.write_locked(&target)
    }
}

fn decoded_snapshot(bytes: &[u8]) -> Result<PrivateStoreSnapshot, VaultError> {
    let mut disk: DiskPrivateStore =
        serde_json::from_slice(bytes).map_err(|_| VaultError::InvalidPrivateStore)?;
    disk.validate()?;
    disk.take_snapshot()
}
