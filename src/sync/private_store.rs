//! Owner-only storage for local vault keys and WebDAV credentials.
//!
//! This file is deliberately separate from the portable personal-state ledger. Nothing in this
//! store is exported or synchronized, and the offline recovery identity is never accepted by its
//! API or represented by its on-disk schema.

use std::fs;
use std::path::PathBuf;

use age::secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::personal_state::{DeviceId, DevicePublicIdentity, PersonalStateV2};
use crate::util::safe_fs::{
    AdvisoryFileLock, read_owner_only_limited, remove_owner_only_file_durable, sync_parent_dir,
    try_lock_private_file, write_owner_only_atomic,
};

use super::checkpoint::SignedCheckpoint;
use super::crypto::{
    DeviceSecretMaterial, sha256_domain_hex, sign_serializable, validate_age_recipient,
    validate_dataset_id, verify_serializable, verifying_key_from_base64,
};
use super::error::VaultError;
use super::membership::MembershipAnchor;
use super::pairing::ApprovedPairing;

const PRIVATE_STORE_KIND: &str = "yututui_vault_private_store";
const PRIVATE_STORE_SCHEMA_VERSION: u32 = 1;
const MAX_PRIVATE_STORE_BYTES: u64 = 256 * 1024;
const MAX_USERNAME_BYTES: usize = 1024;
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
const MAX_PAIRING_INVITE_ID_BYTES: usize = 128;
const PAIRING_REQUEST_NONCE_HEX_BYTES: usize = 32;
const SHA256_HEX_BYTES: usize = 64;
const DEVICE_BINDING_SIGNATURE_DOMAIN: &[u8] = b"yututui-vault-private-device-binding-signature-v1";
const CREDENTIAL_SECRET_COMMITMENT_DOMAIN: &[u8] =
    b"yututui-vault-private-credential-secret-commitment-v1";

/// The durable point reached while enrolling this device.
///
/// Keys are written before the corresponding membership operation. A restart can therefore
/// resume a pending enrollment without ever generating replacement keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnrollmentState {
    /// The device has generated keys but has not received membership approval.
    PendingApproval,
    /// The membership approval is anchored, but its ledger projection is not yet committed.
    PendingLedgerCommit,
    /// The private keys and the local membership projection agree.
    Active,
    /// This local device was explicitly revoked. Its keys remain only for inspection/recovery.
    Revoked,
}

/// Authentication form used by the state-vault WebDAV endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultCredentialKind {
    Password,
    BearerToken,
}

/// Restart-safe context for one joining device's encrypted approval.
///
/// It contains neither the one-time pairing code nor any WebDAV credential, and intentionally
/// implements neither `Debug`, `Clone`, nor serde traits.
#[derive(PartialEq, Eq)]
pub struct PendingPairing {
    invite_id: String,
    request_nonce: String,
}

impl PendingPairing {
    pub fn invite_id(&self) -> &str {
        &self.invite_id
    }

    pub fn request_nonce(&self) -> &str {
        &self.request_nonce
    }
}

impl Drop for PendingPairing {
    fn drop(&mut self) {
        self.invite_id.zeroize();
        self.request_nonce.zeroize();
    }
}

/// A WebDAV credential whose secret values have no `Debug`, `Clone`, or serde implementation.
pub struct VaultCredential {
    kind: VaultCredentialKind,
    username: Option<SecretString>,
    secret: SecretString,
}

impl VaultCredential {
    pub fn password(
        username: impl Into<String>,
        password: SecretString,
    ) -> Result<Self, VaultError> {
        let username = username.into();
        validate_credential_part(&username, MAX_USERNAME_BYTES, false)?;
        validate_credential_part(password.expose_secret(), MAX_CREDENTIAL_BYTES, true)?;
        Ok(Self {
            kind: VaultCredentialKind::Password,
            username: Some(SecretString::from(username)),
            secret: password,
        })
    }

    pub fn bearer_token(token: SecretString) -> Result<Self, VaultError> {
        validate_credential_part(token.expose_secret(), MAX_CREDENTIAL_BYTES, true)?;
        Ok(Self {
            kind: VaultCredentialKind::BearerToken,
            username: None,
            secret: token,
        })
    }

    pub fn kind(&self) -> VaultCredentialKind {
        self.kind
    }

    pub fn username(&self) -> Option<&SecretString> {
        self.username.as_ref()
    }

    pub fn secret(&self) -> &SecretString {
        &self.secret
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TrustAnchors {
    recovery_recipient: String,
    recovery_verifying_key: String,
    membership_root_hash: String,
}

#[derive(Clone, PartialEq, Eq)]
struct StoredCheckpointAnchor {
    sequence: u64,
    hash: String,
}

#[derive(Clone, PartialEq, Eq)]
struct PendingLedgerAnchor {
    membership_head_hash: String,
    checkpoint: StoredCheckpointAnchor,
}

/// A fully validated in-memory snapshot of the private store.
///
/// This type intentionally implements neither `Debug`, `Clone`, nor serde traits.
pub struct PrivateStoreSnapshot {
    revision: u64,
    dataset_id: String,
    device: DeviceSecretMaterial,
    enrollment: EnrollmentState,
    pending_pairing: Option<PendingPairing>,
    trust_anchors: Option<TrustAnchors>,
    pending_ledger_anchor: Option<PendingLedgerAnchor>,
    checkpoint_anchor: Option<StoredCheckpointAnchor>,
    credential: Option<VaultCredential>,
}

impl PrivateStoreSnapshot {
    /// Create the pre-approval state for a joining device.
    pub fn pending_approval(
        dataset_id: impl Into<String>,
        device: DeviceSecretMaterial,
    ) -> Result<Self, VaultError> {
        let dataset_id = dataset_id.into();
        validate_dataset_id(&dataset_id)?;
        Ok(Self {
            revision: 0,
            dataset_id,
            device,
            enrollment: EnrollmentState::PendingApproval,
            pending_pairing: None,
            trust_anchors: None,
            pending_ledger_anchor: None,
            checkpoint_anchor: None,
            credential: None,
        })
    }

    /// Create a state whose signed membership root is ready to be projected into the ledger.
    pub fn pending_ledger_commit(
        device: DeviceSecretMaterial,
        recovery_recipient: impl Into<String>,
        recovery_verifying_key: impl Into<String>,
        membership_root_hash: impl Into<String>,
        checkpoint: &SignedCheckpoint,
    ) -> Result<Self, VaultError> {
        let trust_anchors = TrustAnchors {
            recovery_recipient: recovery_recipient.into(),
            recovery_verifying_key: recovery_verifying_key.into(),
            membership_root_hash: membership_root_hash.into(),
        };
        let (dataset_id, pending_ledger_anchor) =
            verified_pending_ledger(&device, &trust_anchors, checkpoint)?;
        Ok(Self {
            revision: 0,
            dataset_id,
            device,
            enrollment: EnrollmentState::PendingLedgerCommit,
            pending_pairing: None,
            trust_anchors: Some(trust_anchors),
            pending_ledger_anchor: Some(pending_ledger_anchor),
            checkpoint_anchor: None,
            credential: None,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn dataset_id(&self) -> &str {
        &self.dataset_id
    }

    pub fn device_id(&self) -> &str {
        self.device.device_id()
    }

    pub fn device(&self) -> &DeviceSecretMaterial {
        &self.device
    }

    pub fn enrollment(&self) -> EnrollmentState {
        self.enrollment
    }

    pub fn recovery_recipient(&self) -> Option<&str> {
        self.trust_anchors
            .as_ref()
            .map(|anchors| anchors.recovery_recipient.as_str())
    }

    pub fn recovery_verifying_key(&self) -> Option<&str> {
        self.trust_anchors
            .as_ref()
            .map(|anchors| anchors.recovery_verifying_key.as_str())
    }

    pub fn membership_root_hash(&self) -> Option<&str> {
        self.trust_anchors
            .as_ref()
            .map(|anchors| anchors.membership_root_hash.as_str())
    }

    pub fn checkpoint_sequence(&self) -> Option<u64> {
        self.checkpoint_anchor
            .as_ref()
            .map(|anchor| anchor.sequence)
    }

    pub fn checkpoint_hash(&self) -> Option<&str> {
        self.checkpoint_anchor
            .as_ref()
            .map(|anchor| anchor.hash.as_str())
    }

    pub fn credential(&self) -> Option<&VaultCredential> {
        self.credential.as_ref()
    }

    /// Durably associate a sent pairing request with the approval it expects.
    ///
    /// Repeating the exact context is idempotent. Replacing it would orphan a potentially
    /// approved request, so a different context requires removing and restarting enrollment.
    pub fn set_pending_pairing(
        &mut self,
        invite_id: impl Into<String>,
        request_nonce: impl Into<String>,
    ) -> Result<(), VaultError> {
        if self.enrollment != EnrollmentState::PendingApproval || self.trust_anchors.is_some() {
            return Err(VaultError::InvalidPrivateStore);
        }
        let pending = PendingPairing {
            invite_id: invite_id.into(),
            request_nonce: request_nonce.into(),
        };
        validate_pending_pairing(&pending)?;
        match &self.pending_pairing {
            Some(current) if current == &pending => Ok(()),
            Some(_) => Err(VaultError::InvalidPrivateStore),
            None => {
                self.pending_pairing = Some(pending);
                Ok(())
            }
        }
    }

    /// Read the expected approval context only while enrollment is still pending.
    pub fn pending_pairing(&self) -> Result<Option<&PendingPairing>, VaultError> {
        if self.enrollment != EnrollmentState::PendingApproval {
            return Err(VaultError::InvalidPrivateStore);
        }
        Ok(self.pending_pairing.as_ref())
    }

    /// Record approval and its authenticated public anchors before changing the ledger.
    pub fn approve(&mut self, approved: &ApprovedPairing) -> Result<(), VaultError> {
        if self.enrollment != EnrollmentState::PendingApproval || self.trust_anchors.is_some() {
            return Err(VaultError::InvalidPrivateStore);
        }
        let pending = self
            .pending_pairing
            .as_ref()
            .ok_or(VaultError::InvalidPrivateStore)?;
        if pending.invite_id != approved.invite_id()
            || pending.request_nonce != approved.request_nonce()
            || self.device.device_id() != approved.approved_device_id().as_str()
        {
            return Err(VaultError::InvalidPrivateStore);
        }
        let membership = approved.membership().verify(&MembershipAnchor::RootHash(
            approved.membership_root_hash().to_owned(),
        ))?;
        let trust_anchors = TrustAnchors {
            recovery_recipient: membership.recovery_recipient,
            recovery_verifying_key: membership.recovery_verifying_key,
            membership_root_hash: membership.root_hash,
        };
        if membership.head_hash != approved.membership_head_hash() {
            return Err(VaultError::InvalidPrivateStore);
        }
        let (dataset_id, pending_ledger_anchor) =
            verified_pending_ledger(&self.device, &trust_anchors, approved.signed_checkpoint())?;
        if dataset_id != self.dataset_id {
            return Err(VaultError::InvalidPrivateStore);
        }
        self.trust_anchors = Some(trust_anchors);
        self.pending_ledger_anchor = Some(pending_ledger_anchor);
        self.enrollment = EnrollmentState::PendingLedgerCommit;
        self.pending_pairing = None;
        Ok(())
    }

    /// Mark the private material active only after the matching ledger projection commits.
    pub fn mark_active(
        &mut self,
        committed_checkpoint: &SignedCheckpoint,
        committed_state: &PersonalStateV2,
    ) -> Result<(), VaultError> {
        if self.enrollment != EnrollmentState::PendingLedgerCommit || self.trust_anchors.is_none() {
            return Err(VaultError::InvalidPrivateStore);
        }
        let trust_anchors = self
            .trust_anchors
            .as_ref()
            .ok_or(VaultError::InvalidPrivateStore)?;
        let (dataset_id, committed_anchor) =
            verified_pending_ledger(&self.device, trust_anchors, committed_checkpoint)?;
        if dataset_id != self.dataset_id
            || committed_checkpoint.payload.state != *committed_state
            || self.pending_ledger_anchor.as_ref() != Some(&committed_anchor)
        {
            return Err(VaultError::InvalidPrivateStore);
        }
        self.checkpoint_anchor = Some(committed_anchor.checkpoint);
        self.pending_ledger_anchor = None;
        self.enrollment = EnrollmentState::Active;
        Ok(())
    }

    pub fn mark_revoked(&mut self) -> Result<(), VaultError> {
        if self.enrollment != EnrollmentState::Active {
            return Err(VaultError::InvalidPrivateStore);
        }
        self.enrollment = EnrollmentState::Revoked;
        Ok(())
    }

    /// Advance the authenticated checkpoint anchor. Rollback and same-sequence forks fail closed.
    pub fn advance_checkpoint(
        &mut self,
        sequence: u64,
        hash: impl Into<String>,
    ) -> Result<(), VaultError> {
        if self.enrollment != EnrollmentState::Active {
            return Err(VaultError::InvalidPrivateStore);
        }
        let hash = hash.into();
        validate_sha256_hex(&hash)?;
        if sequence == 0 {
            return Err(VaultError::InvalidPrivateStore);
        }
        if let Some(current) = &self.checkpoint_anchor {
            if sequence == current.sequence && hash == current.hash {
                return Ok(());
            }
            if sequence <= current.sequence {
                return Err(VaultError::RevisionConflict);
            }
        }
        self.checkpoint_anchor = Some(StoredCheckpointAnchor { sequence, hash });
        Ok(())
    }

    pub fn set_credential(&mut self, credential: VaultCredential) {
        self.credential = Some(credential);
    }

    pub fn clear_credential(&mut self) {
        self.credential = None;
    }
}

/// A durable, compare-and-swap owner of one private sync store file.
///
/// The lock file is persistent and never removed, preventing lock-inode replacement races.
pub struct PrivateStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl PrivateStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, VaultError> {
        let path = path.into();
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(VaultError::InvalidPrivateStore);
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(VaultError::InvalidPrivateStore)?;
        let lock_path = path.with_file_name(format!("{file_name}.lock"));
        Ok(Self { path, lock_path })
    }

    /// Create a missing store at revision one. Existing or invalid state is never overwritten.
    pub fn create(&self, snapshot: &mut PrivateStoreSnapshot) -> Result<(), VaultError> {
        if snapshot.revision > 1 {
            return Err(VaultError::RevisionConflict);
        }
        let original_revision = snapshot.revision;
        validate_snapshot(snapshot)?;
        let _lock = self.acquire_lock()?;
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {
                let _ = self.load_locked()?;
                snapshot.revision = 1;
                if self.exact_snapshot_is_visible_locked(snapshot) {
                    return sync_parent_dir(&self.path).map_err(|_| VaultError::StorageFailed);
                }
                snapshot.revision = original_revision;
                return Err(VaultError::RevisionConflict);
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && original_revision == 0 => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(VaultError::RevisionConflict);
            }
            Err(_) => return Err(VaultError::StorageFailed),
        }
        snapshot.revision = 1;
        if let Err(error) = self.write_locked(snapshot) {
            if !self.exact_snapshot_is_visible_locked(snapshot) {
                snapshot.revision = original_revision;
            }
            return Err(error);
        }
        Ok(())
    }

    /// Load an existing store. Missing, malformed, weakly protected, or mismatched state errors.
    pub fn load(&self) -> Result<PrivateStoreSnapshot, VaultError> {
        let _lock = self.acquire_lock()?;
        self.load_locked()
    }

    /// Persist local changes only if this exact snapshot revision is still current.
    pub fn save(&self, snapshot: &mut PrivateStoreSnapshot) -> Result<(), VaultError> {
        validate_snapshot(snapshot)?;
        let _lock = self.acquire_lock()?;
        let current = self.load_locked()?;
        validate_same_store_identity(&current, snapshot)?;
        if current.revision == snapshot.revision && self.exact_snapshot_is_visible_locked(snapshot)
        {
            return sync_parent_dir(&self.path).map_err(|_| VaultError::StorageFailed);
        }
        if current.revision != snapshot.revision {
            return Err(VaultError::RevisionConflict);
        }
        let next_revision = snapshot
            .revision
            .checked_add(1)
            .ok_or(VaultError::RevisionConflict)?;
        let previous_revision = snapshot.revision;
        snapshot.revision = next_revision;
        if let Err(error) = self.write_locked(snapshot) {
            if !self.exact_snapshot_is_visible_locked(snapshot) {
                snapshot.revision = previous_revision;
            }
            return Err(error);
        }
        Ok(())
    }

    /// Remove the exact observed revision. Missing or changed state is an error, not a reset.
    pub fn remove(&self, expected_revision: u64) -> Result<(), VaultError> {
        let _lock = self.acquire_lock()?;
        let current = self.load_locked()?;
        if current.revision != expected_revision {
            return Err(VaultError::RevisionConflict);
        }
        remove_owner_only_file_durable(&self.path).map_err(|_| VaultError::StorageFailed)
    }

    fn acquire_lock(&self) -> Result<AdvisoryFileLock, VaultError> {
        match try_lock_private_file(&self.lock_path) {
            Ok(Some(lock)) => Ok(lock),
            Ok(None) => Err(VaultError::StorageBusy),
            Err(_) => Err(VaultError::StorageFailed),
        }
    }

    fn load_locked(&self) -> Result<PrivateStoreSnapshot, VaultError> {
        let bytes = read_owner_only_limited(&self.path, MAX_PRIVATE_STORE_BYTES)
            .map_err(|_| VaultError::InvalidPrivateStore)?;
        let bytes = Zeroizing::new(bytes);
        let mut disk: DiskPrivateStore =
            serde_json::from_slice(&bytes).map_err(|_| VaultError::InvalidPrivateStore)?;
        disk.validate()?;
        disk.take_snapshot()
    }

    fn write_locked(&self, snapshot: &PrivateStoreSnapshot) -> Result<(), VaultError> {
        validate_snapshot(snapshot)?;
        let bytes = encoded_snapshot(snapshot)?;
        match write_owner_only_atomic(&self.path, &bytes) {
            Ok(()) => Ok(()),
            Err(_) if self.exact_snapshot_is_visible_locked(snapshot) => {
                sync_parent_dir(&self.path).map_err(|_| VaultError::StorageFailed)
            }
            Err(_) => Err(VaultError::StorageFailed),
        }
    }

    fn exact_snapshot_is_visible_locked(&self, snapshot: &PrivateStoreSnapshot) -> bool {
        let Ok(expected) = encoded_snapshot(snapshot) else {
            return false;
        };
        read_owner_only_limited(&self.path, MAX_PRIVATE_STORE_BYTES)
            .is_ok_and(|actual| actual == *expected)
    }
}

fn encoded_snapshot(snapshot: &PrivateStoreSnapshot) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    let disk = DiskPrivateStore::from_snapshot(snapshot)?;
    let bytes =
        Zeroizing::new(serde_json::to_vec(&disk).map_err(|_| VaultError::SerializationFailed)?);
    if bytes.len() as u64 > MAX_PRIVATE_STORE_BYTES {
        return Err(VaultError::PayloadTooLarge);
    }
    Ok(bytes)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskPrivateStore {
    kind: String,
    schema_version: u32,
    revision: u64,
    dataset_id: String,
    device_id: String,
    enrollment: DiskEnrollmentState,
    pending_pairing: Option<DiskPendingPairing>,
    device_age_identity: String,
    device_signing_key: String,
    device_binding_signature: String,
    recovery_recipient: Option<String>,
    recovery_verifying_key: Option<String>,
    membership_root_hash: Option<String>,
    pending_ledger_anchor: Option<DiskPendingLedgerAnchor>,
    checkpoint_anchor: Option<DiskCheckpointAnchor>,
    credential: Option<DiskVaultCredential>,
}

impl Drop for DiskPrivateStore {
    fn drop(&mut self) {
        self.device_age_identity.zeroize();
        self.device_signing_key.zeroize();
        if let Some(credential) = &mut self.credential {
            credential.zeroize();
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DiskEnrollmentState {
    PendingApproval,
    PendingLedgerCommit,
    Active,
    Revoked,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskPendingPairing {
    invite_id: String,
    request_nonce: String,
}

impl DiskPendingPairing {
    fn validate(&self) -> Result<(), VaultError> {
        validate_pairing_values(&self.invite_id, &self.request_nonce)
    }

    fn into_pending(mut self) -> Result<PendingPairing, VaultError> {
        self.validate()?;
        Ok(PendingPairing {
            invite_id: std::mem::take(&mut self.invite_id),
            request_nonce: std::mem::take(&mut self.request_nonce),
        })
    }
}

impl Drop for DiskPendingPairing {
    fn drop(&mut self) {
        self.invite_id.zeroize();
        self.request_nonce.zeroize();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskCheckpointAnchor {
    sequence: u64,
    hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskPendingLedgerAnchor {
    membership_head_hash: String,
    checkpoint: DiskCheckpointAnchor,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskVaultCredential {
    kind: DiskVaultCredentialKind,
    username: Option<String>,
    secret: String,
}

impl DiskVaultCredential {
    fn zeroize(&mut self) {
        if let Some(username) = &mut self.username {
            username.zeroize();
        }
        self.secret.zeroize();
    }
}

impl Drop for DiskVaultCredential {
    fn drop(&mut self) {
        self.zeroize();
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DiskVaultCredentialKind {
    Password,
    BearerToken,
}

impl DiskPrivateStore {
    fn from_snapshot(snapshot: &PrivateStoreSnapshot) -> Result<Self, VaultError> {
        let (recovery_recipient, recovery_verifying_key, membership_root_hash) =
            match &snapshot.trust_anchors {
                Some(anchors) => (
                    Some(anchors.recovery_recipient.clone()),
                    Some(anchors.recovery_verifying_key.clone()),
                    Some(anchors.membership_root_hash.clone()),
                ),
                None => (None, None, None),
            };
        let credential = snapshot.credential.as_ref().map(|credential| {
            let kind = match credential.kind {
                VaultCredentialKind::Password => DiskVaultCredentialKind::Password,
                VaultCredentialKind::BearerToken => DiskVaultCredentialKind::BearerToken,
            };
            DiskVaultCredential {
                kind,
                username: credential
                    .username
                    .as_ref()
                    .map(|username| username.expose_secret().to_owned()),
                secret: credential.secret.expose_secret().to_owned(),
            }
        });
        let pending_pairing = snapshot
            .pending_pairing
            .as_ref()
            .map(|pending| DiskPendingPairing {
                invite_id: pending.invite_id.clone(),
                request_nonce: pending.request_nonce.clone(),
            });
        let enrollment = snapshot.enrollment.into();
        let pending_ledger_anchor =
            snapshot
                .pending_ledger_anchor
                .as_ref()
                .map(|anchor| DiskPendingLedgerAnchor {
                    membership_head_hash: anchor.membership_head_hash.clone(),
                    checkpoint: DiskCheckpointAnchor {
                        sequence: anchor.checkpoint.sequence,
                        hash: anchor.checkpoint.hash.clone(),
                    },
                });
        let checkpoint_anchor =
            snapshot
                .checkpoint_anchor
                .as_ref()
                .map(|anchor| DiskCheckpointAnchor {
                    sequence: anchor.sequence,
                    hash: anchor.hash.clone(),
                });
        let credential_commitment = credential_commitment(credential.as_ref());
        let public_identity = snapshot.device.public_identity();
        let binding = DiskDeviceBinding {
            kind: PRIVATE_STORE_KIND,
            schema_version: PRIVATE_STORE_SCHEMA_VERSION,
            revision: snapshot.revision,
            dataset_id: &snapshot.dataset_id,
            device_id: snapshot.device.device_id(),
            public_identity: &public_identity,
            enrollment: &enrollment,
            pending_pairing: pending_pairing.as_ref(),
            recovery_recipient: recovery_recipient.as_deref(),
            recovery_verifying_key: recovery_verifying_key.as_deref(),
            membership_root_hash: membership_root_hash.as_deref(),
            pending_ledger_anchor: pending_ledger_anchor.as_ref(),
            checkpoint_anchor: checkpoint_anchor.as_ref(),
            credential: credential_commitment,
        };
        let device_binding_signature = sign_serializable(
            DEVICE_BINDING_SIGNATURE_DOMAIN,
            snapshot.device.signing_key(),
            &binding,
        )?;
        Ok(Self {
            kind: PRIVATE_STORE_KIND.to_owned(),
            schema_version: PRIVATE_STORE_SCHEMA_VERSION,
            revision: snapshot.revision,
            dataset_id: snapshot.dataset_id.clone(),
            device_id: snapshot.device.device_id().to_owned(),
            enrollment,
            pending_pairing,
            device_age_identity: snapshot
                .device
                .age_identity_secret()
                .expose_secret()
                .to_owned(),
            device_signing_key: snapshot
                .device
                .signing_key_secret_b64()
                .expose_secret()
                .to_owned(),
            device_binding_signature,
            recovery_recipient,
            recovery_verifying_key,
            membership_root_hash,
            pending_ledger_anchor,
            checkpoint_anchor,
            credential,
        })
    }

    fn validate(&self) -> Result<(), VaultError> {
        if self.kind != PRIVATE_STORE_KIND
            || self.schema_version != PRIVATE_STORE_SCHEMA_VERSION
            || self.revision == 0
        {
            return Err(VaultError::InvalidPrivateStore);
        }
        validate_dataset_id(&self.dataset_id)?;
        let anchors = match (
            &self.recovery_recipient,
            &self.recovery_verifying_key,
            &self.membership_root_hash,
        ) {
            (Some(recipient), Some(verifying_key), Some(root_hash)) => Some(TrustAnchors {
                recovery_recipient: recipient.clone(),
                recovery_verifying_key: verifying_key.clone(),
                membership_root_hash: root_hash.clone(),
            }),
            (None, None, None) => None,
            _ => return Err(VaultError::InvalidPrivateStore),
        };
        if let Some(anchors) = &anchors {
            validate_trust_anchors(anchors)?;
        }
        match self.enrollment {
            DiskEnrollmentState::PendingApproval if anchors.is_none() => {}
            DiskEnrollmentState::PendingLedgerCommit
            | DiskEnrollmentState::Active
            | DiskEnrollmentState::Revoked
                if anchors.is_some() => {}
            _ => return Err(VaultError::InvalidPrivateStore),
        }
        if let Some(pending) = &self.pending_pairing {
            if !matches!(self.enrollment, DiskEnrollmentState::PendingApproval) {
                return Err(VaultError::InvalidPrivateStore);
            }
            pending.validate()?;
        }
        match (&self.enrollment, &self.pending_ledger_anchor) {
            (DiskEnrollmentState::PendingLedgerCommit, Some(anchor)) => {
                validate_pending_ledger_anchor(anchor)?;
            }
            (
                DiskEnrollmentState::PendingApproval
                | DiskEnrollmentState::Active
                | DiskEnrollmentState::Revoked,
                None,
            ) => {}
            _ => return Err(VaultError::InvalidPrivateStore),
        }
        match (&self.enrollment, &self.checkpoint_anchor) {
            (DiskEnrollmentState::Active | DiskEnrollmentState::Revoked, Some(checkpoint)) => {
                validate_checkpoint_anchor(checkpoint)?;
            }
            (
                DiskEnrollmentState::PendingApproval | DiskEnrollmentState::PendingLedgerCommit,
                None,
            ) => {}
            _ => {
                return Err(VaultError::InvalidPrivateStore);
            }
        }
        if let Some(credential) = &self.credential {
            credential.validate()?;
        }
        Ok(())
    }

    fn take_snapshot(&mut self) -> Result<PrivateStoreSnapshot, VaultError> {
        let age_identity = SecretString::from(std::mem::take(&mut self.device_age_identity));
        let signing_key = SecretString::from(std::mem::take(&mut self.device_signing_key));
        let device =
            DeviceSecretMaterial::from_encoded(self.device_id.clone(), age_identity, &signing_key)
                .map_err(|_| VaultError::InvalidPrivateStore)?;
        self.verify_device_binding(&device)?;
        let pending_pairing = self
            .pending_pairing
            .take()
            .map(DiskPendingPairing::into_pending)
            .transpose()?;
        let trust_anchors = match (
            self.recovery_recipient.take(),
            self.recovery_verifying_key.take(),
            self.membership_root_hash.take(),
        ) {
            (
                Some(recovery_recipient),
                Some(recovery_verifying_key),
                Some(membership_root_hash),
            ) => Some(TrustAnchors {
                recovery_recipient,
                recovery_verifying_key,
                membership_root_hash,
            }),
            (None, None, None) => None,
            _ => return Err(VaultError::InvalidPrivateStore),
        };
        let checkpoint_anchor =
            self.checkpoint_anchor
                .take()
                .map(|anchor| StoredCheckpointAnchor {
                    sequence: anchor.sequence,
                    hash: anchor.hash,
                });
        let pending_ledger_anchor =
            self.pending_ledger_anchor
                .take()
                .map(|anchor| PendingLedgerAnchor {
                    membership_head_hash: anchor.membership_head_hash,
                    checkpoint: StoredCheckpointAnchor {
                        sequence: anchor.checkpoint.sequence,
                        hash: anchor.checkpoint.hash,
                    },
                });
        let credential = self
            .credential
            .take()
            .map(DiskVaultCredential::into_credential)
            .transpose()?;
        let snapshot = PrivateStoreSnapshot {
            revision: self.revision,
            dataset_id: self.dataset_id.clone(),
            device,
            enrollment: (&self.enrollment).into(),
            pending_pairing,
            trust_anchors,
            pending_ledger_anchor,
            checkpoint_anchor,
            credential,
        };
        validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    fn verify_device_binding(&self, device: &DeviceSecretMaterial) -> Result<(), VaultError> {
        let public_identity = device.public_identity();
        let credential_commitment = credential_commitment(self.credential.as_ref());
        let binding = DiskDeviceBinding {
            kind: PRIVATE_STORE_KIND,
            schema_version: PRIVATE_STORE_SCHEMA_VERSION,
            revision: self.revision,
            dataset_id: &self.dataset_id,
            device_id: &self.device_id,
            public_identity: &public_identity,
            enrollment: &self.enrollment,
            pending_pairing: self.pending_pairing.as_ref(),
            recovery_recipient: self.recovery_recipient.as_deref(),
            recovery_verifying_key: self.recovery_verifying_key.as_deref(),
            membership_root_hash: self.membership_root_hash.as_deref(),
            pending_ledger_anchor: self.pending_ledger_anchor.as_ref(),
            checkpoint_anchor: self.checkpoint_anchor.as_ref(),
            credential: credential_commitment,
        };
        verify_serializable(
            DEVICE_BINDING_SIGNATURE_DOMAIN,
            &public_identity.ed25519_verifying_key,
            &binding,
            &self.device_binding_signature,
        )
        .map_err(|_| VaultError::InvalidPrivateStore)
    }
}

#[derive(Serialize)]
struct DiskDeviceBinding<'a> {
    kind: &'a str,
    schema_version: u32,
    revision: u64,
    dataset_id: &'a str,
    device_id: &'a str,
    public_identity: &'a DevicePublicIdentity,
    enrollment: &'a DiskEnrollmentState,
    pending_pairing: Option<&'a DiskPendingPairing>,
    recovery_recipient: Option<&'a str>,
    recovery_verifying_key: Option<&'a str>,
    membership_root_hash: Option<&'a str>,
    pending_ledger_anchor: Option<&'a DiskPendingLedgerAnchor>,
    checkpoint_anchor: Option<&'a DiskCheckpointAnchor>,
    credential: Option<DiskCredentialCommitment<'a>>,
}

#[derive(Serialize)]
struct DiskCredentialCommitment<'a> {
    kind: DiskVaultCredentialKind,
    username: Option<&'a str>,
    secret_hash: String,
}

fn credential_commitment(
    credential: Option<&DiskVaultCredential>,
) -> Option<DiskCredentialCommitment<'_>> {
    credential.map(|credential| DiskCredentialCommitment {
        kind: credential.kind,
        username: credential.username.as_deref(),
        secret_hash: sha256_domain_hex(
            CREDENTIAL_SECRET_COMMITMENT_DOMAIN,
            &[credential.secret.as_bytes()],
        ),
    })
}

impl DiskVaultCredential {
    fn validate(&self) -> Result<(), VaultError> {
        validate_credential_part(&self.secret, MAX_CREDENTIAL_BYTES, true)?;
        match (&self.kind, &self.username) {
            (DiskVaultCredentialKind::Password, Some(username)) => {
                validate_credential_part(username, MAX_USERNAME_BYTES, false)
            }
            (DiskVaultCredentialKind::BearerToken, None) => Ok(()),
            _ => Err(VaultError::InvalidPrivateStore),
        }
    }

    fn into_credential(mut self) -> Result<VaultCredential, VaultError> {
        self.validate()?;
        let secret = SecretString::from(std::mem::take(&mut self.secret));
        match &self.kind {
            DiskVaultCredentialKind::Password => {
                let username = self
                    .username
                    .take()
                    .ok_or(VaultError::InvalidPrivateStore)?;
                VaultCredential::password(username, secret)
            }
            DiskVaultCredentialKind::BearerToken => VaultCredential::bearer_token(secret),
        }
    }
}

impl From<EnrollmentState> for DiskEnrollmentState {
    fn from(value: EnrollmentState) -> Self {
        match value {
            EnrollmentState::PendingApproval => Self::PendingApproval,
            EnrollmentState::PendingLedgerCommit => Self::PendingLedgerCommit,
            EnrollmentState::Active => Self::Active,
            EnrollmentState::Revoked => Self::Revoked,
        }
    }
}

impl From<&DiskEnrollmentState> for EnrollmentState {
    fn from(value: &DiskEnrollmentState) -> Self {
        match value {
            DiskEnrollmentState::PendingApproval => Self::PendingApproval,
            DiskEnrollmentState::PendingLedgerCommit => Self::PendingLedgerCommit,
            DiskEnrollmentState::Active => Self::Active,
            DiskEnrollmentState::Revoked => Self::Revoked,
        }
    }
}

fn validate_snapshot(snapshot: &PrivateStoreSnapshot) -> Result<(), VaultError> {
    validate_dataset_id(&snapshot.dataset_id)?;
    if snapshot.device.device_id().is_empty() {
        return Err(VaultError::InvalidPrivateStore);
    }
    match (snapshot.enrollment, &snapshot.trust_anchors) {
        (EnrollmentState::PendingApproval, None) => {}
        (
            EnrollmentState::PendingLedgerCommit
            | EnrollmentState::Active
            | EnrollmentState::Revoked,
            Some(anchors),
        ) => validate_trust_anchors(anchors)?,
        _ => return Err(VaultError::InvalidPrivateStore),
    }
    if let Some(anchors) = &snapshot.trust_anchors {
        validate_device_recovery_separation(&snapshot.device, anchors)?;
    }
    if let Some(pending) = &snapshot.pending_pairing {
        if snapshot.enrollment != EnrollmentState::PendingApproval {
            return Err(VaultError::InvalidPrivateStore);
        }
        validate_pending_pairing(pending)?;
    }
    match (snapshot.enrollment, &snapshot.pending_ledger_anchor) {
        (EnrollmentState::PendingLedgerCommit, Some(anchor)) => {
            validate_pending_ledger_anchor_value(anchor)?;
        }
        (
            EnrollmentState::PendingApproval | EnrollmentState::Active | EnrollmentState::Revoked,
            None,
        ) => {}
        _ => return Err(VaultError::InvalidPrivateStore),
    }
    match (snapshot.enrollment, &snapshot.checkpoint_anchor) {
        (EnrollmentState::Active | EnrollmentState::Revoked, Some(anchor)) => {
            validate_checkpoint_anchor_value(anchor)?;
        }
        (EnrollmentState::PendingApproval | EnrollmentState::PendingLedgerCommit, None) => {}
        _ => return Err(VaultError::InvalidPrivateStore),
    }
    Ok(())
}

fn validate_same_store_identity(
    current: &PrivateStoreSnapshot,
    candidate: &PrivateStoreSnapshot,
) -> Result<(), VaultError> {
    let anchors_match = current.trust_anchors == candidate.trust_anchors
        || (current.enrollment == EnrollmentState::PendingApproval
            && current.trust_anchors.is_none()
            && candidate.enrollment == EnrollmentState::PendingLedgerCommit
            && candidate.trust_anchors.is_some());
    let pending_ledger_transition_valid = if current.enrollment == EnrollmentState::PendingApproval
        && candidate.enrollment == EnrollmentState::PendingLedgerCommit
    {
        current.pending_ledger_anchor.is_none() && candidate.pending_ledger_anchor.is_some()
    } else if current.enrollment == EnrollmentState::PendingLedgerCommit
        && candidate.enrollment == EnrollmentState::Active
    {
        current.pending_ledger_anchor.is_some()
            && candidate.pending_ledger_anchor.is_none()
            && candidate.checkpoint_anchor.as_ref()
                == current
                    .pending_ledger_anchor
                    .as_ref()
                    .map(|anchor| &anchor.checkpoint)
    } else {
        current.pending_ledger_anchor == candidate.pending_ledger_anchor
    };
    let pending_pairing_transition_valid = if current.enrollment == EnrollmentState::PendingApproval
        && candidate.enrollment == EnrollmentState::PendingApproval
    {
        current.pending_pairing.is_none() || current.pending_pairing == candidate.pending_pairing
    } else if current.enrollment == EnrollmentState::PendingApproval
        && candidate.enrollment == EnrollmentState::PendingLedgerCommit
    {
        candidate.pending_pairing.is_none()
    } else {
        current.pending_pairing == candidate.pending_pairing
    };
    if current.dataset_id != candidate.dataset_id
        || current.device.public_record() != candidate.device.public_record()
        || !anchors_match
        || !pending_pairing_transition_valid
        || !pending_ledger_transition_valid
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    Ok(())
}

fn validate_pending_pairing(pending: &PendingPairing) -> Result<(), VaultError> {
    validate_pairing_values(&pending.invite_id, &pending.request_nonce)
}

fn verified_pending_ledger(
    device: &DeviceSecretMaterial,
    anchors: &TrustAnchors,
    checkpoint: &SignedCheckpoint,
) -> Result<(String, PendingLedgerAnchor), VaultError> {
    validate_trust_anchors(anchors)?;
    validate_device_recovery_separation(device, anchors)?;
    let verified = checkpoint.verify(&MembershipAnchor::RootHash(
        anchors.membership_root_hash.clone(),
    ))?;
    if verified.recovery_recipient != anchors.recovery_recipient
        || verified.recovery_verifying_key != anchors.recovery_verifying_key
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    let device_id =
        DeviceId::new(device.device_id()).map_err(|_| VaultError::InvalidPrivateStore)?;
    let record = verified
        .devices
        .get(&device_id)
        .filter(|record| !record.revoked)
        .ok_or(VaultError::InvalidPrivateStore)?;
    if !device.matches_personal_record(record) {
        return Err(VaultError::InvalidPrivateStore);
    }
    let checkpoint_anchor = StoredCheckpointAnchor {
        sequence: checkpoint.payload.checkpoint_sequence,
        hash: checkpoint.hash()?,
    };
    validate_checkpoint_anchor_value(&checkpoint_anchor)?;
    Ok((
        verified.dataset_id,
        PendingLedgerAnchor {
            membership_head_hash: verified.head_hash,
            checkpoint: checkpoint_anchor,
        },
    ))
}

fn validate_device_recovery_separation(
    device: &DeviceSecretMaterial,
    anchors: &TrustAnchors,
) -> Result<(), VaultError> {
    let identity = device.public_identity();
    if identity.age_recipient == anchors.recovery_recipient
        || identity.ed25519_verifying_key == anchors.recovery_verifying_key
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    Ok(())
}

fn validate_pending_ledger_anchor(anchor: &DiskPendingLedgerAnchor) -> Result<(), VaultError> {
    validate_sha256_hex(&anchor.membership_head_hash)?;
    validate_checkpoint_anchor(&anchor.checkpoint)
}

fn validate_pending_ledger_anchor_value(anchor: &PendingLedgerAnchor) -> Result<(), VaultError> {
    validate_sha256_hex(&anchor.membership_head_hash)?;
    validate_checkpoint_anchor_value(&anchor.checkpoint)
}

fn validate_checkpoint_anchor(anchor: &DiskCheckpointAnchor) -> Result<(), VaultError> {
    if anchor.sequence == 0 {
        return Err(VaultError::InvalidPrivateStore);
    }
    validate_sha256_hex(&anchor.hash)
}

fn validate_checkpoint_anchor_value(anchor: &StoredCheckpointAnchor) -> Result<(), VaultError> {
    if anchor.sequence == 0 {
        return Err(VaultError::InvalidPrivateStore);
    }
    validate_sha256_hex(&anchor.hash)
}

fn validate_pairing_values(invite_id: &str, request_nonce: &str) -> Result<(), VaultError> {
    let invite_is_valid = !invite_id.is_empty()
        && invite_id.len() <= MAX_PAIRING_INVITE_ID_BYTES
        && invite_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && invite_id
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && invite_id
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric);
    let nonce_is_valid = request_nonce.len() == PAIRING_REQUEST_NONCE_HEX_BYTES
        && request_nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if !invite_is_valid || !nonce_is_valid {
        return Err(VaultError::InvalidPrivateStore);
    }
    Ok(())
}

fn validate_trust_anchors(anchors: &TrustAnchors) -> Result<(), VaultError> {
    validate_age_recipient(&anchors.recovery_recipient)
        .map_err(|_| VaultError::InvalidPrivateStore)?;
    verifying_key_from_base64(&anchors.recovery_verifying_key)
        .map_err(|_| VaultError::InvalidPrivateStore)?;
    validate_sha256_hex(&anchors.membership_root_hash)
}

fn validate_sha256_hex(value: &str) -> Result<(), VaultError> {
    if value.len() != SHA256_HEX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    Ok(())
}

fn validate_credential_part(
    value: &str,
    max_bytes: usize,
    allow_control: bool,
) -> Result<(), VaultError> {
    if value.len() > max_bytes
        || value.is_empty()
        || (!allow_control && value.chars().any(char::is_control))
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    Ok(())
}

#[cfg(test)]
#[path = "private_store/tests.rs"]
mod tests;
