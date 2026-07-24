//! Owner-only, signed crash journal for one host-side pairing handoff.
//!
//! The remote pairing objects are immutable. Their exact ciphertext therefore has to survive a
//! process crash: regenerating age/passphrase ciphertext after a response-lost PUT would produce
//! different bytes and make an otherwise safe retry indistinguishable from a fork.

use std::io;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::personal_state::DeviceId;

use super::super::super::crypto::{
    MAX_ENCRYPTED_OBJECT_BYTES, sha256_domain_hex, sign_serializable, verify_serializable,
};
use super::super::super::{
    DeviceSecretMaterial, EncryptedObject, PairingCode, PairingInvite, SyncPaths, VaultError,
};

const HOST_STATE_KIND: &str = "yututui_pairing_host_state";
const HOST_STATE_SCHEMA_VERSION: u32 = 1;
const HOST_STATE_SIGNATURE_DOMAIN: &[u8] = b"yututui-pairing-host-state-signature-v1";
const LOCATOR_HASH_DOMAIN: &[u8] = b"yututui-pairing-host-locator-ciphertext-v1";
const REQUEST_HASH_DOMAIN: &[u8] = b"yututui-pairing-host-request-ciphertext-v1";
const CHECKPOINT_HASH_DOMAIN: &[u8] = b"yututui-pairing-host-checkpoint-ciphertext-v1";
const APPROVAL_HASH_DOMAIN: &[u8] = b"yututui-pairing-host-approval-ciphertext-v1";
const MAX_HOST_STATE_BYTES: u64 = 128 * 1024;

pub(super) struct HostPairingStore<'a> {
    paths: &'a SyncPaths,
}

pub(super) struct HostPairingSnapshot {
    revision: u64,
    dataset_id: String,
    host_device_id: String,
    code: PairingCode,
    invite_id: String,
    membership_root_hash: String,
    membership_starting_head_hash: String,
    expires_at_unix: i64,
    expected_local_revision: u64,
    expected_private_revision: u64,
    locator_ciphertext_hash: String,
    request_ciphertext_hash: Option<String>,
    request_device_id: Option<String>,
    handoff_membership_head_hash: Option<String>,
    handoff_checkpoint_hash: Option<String>,
    handoff_checkpoint_ciphertext_hash: Option<String>,
    handoff_approval_ciphertext_hash: Option<String>,
}

pub(super) struct DurableHandoff {
    pub checkpoint: EncryptedObject,
    pub approval: EncryptedObject,
}

impl<'a> HostPairingStore<'a> {
    pub fn new(paths: &'a SyncPaths) -> Self {
        Self { paths }
    }

    pub fn load(
        &self,
        device: &DeviceSecretMaterial,
    ) -> Result<Option<HostPairingSnapshot>, VaultError> {
        let bytes = match crate::util::safe_fs::read_owner_only_limited(
            self.paths.pairing_host_state(),
            MAX_HOST_STATE_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(VaultError::StorageFailed),
        };
        let mut disk: DiskHostPairingState =
            serde_json::from_slice(&bytes).map_err(|_| VaultError::InvalidPrivateStore)?;
        let snapshot = disk.take_snapshot(device)?;
        let locator = read_encrypted(
            self.paths.pairing_host_locator(),
            MAX_ENCRYPTED_OBJECT_BYTES,
        )?;
        if ciphertext_hash(LOCATOR_HASH_DOMAIN, &locator) != snapshot.locator_ciphertext_hash {
            return Err(VaultError::InvalidPrivateStore);
        }
        if let Some(expected_hash) = snapshot.request_ciphertext_hash.as_deref() {
            let request = read_encrypted(
                self.paths.pairing_host_request(),
                MAX_ENCRYPTED_OBJECT_BYTES,
            )?;
            if ciphertext_hash(REQUEST_HASH_DOMAIN, &request) != expected_hash {
                return Err(VaultError::InvalidPrivateStore);
            }
        }
        Ok(Some(snapshot))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        device: &DeviceSecretMaterial,
        invite: &PairingInvite,
        expected_local_revision: u64,
        expected_private_revision: u64,
        locator: &EncryptedObject,
    ) -> Result<HostPairingSnapshot, VaultError> {
        if self.load(device)?.is_some() {
            return Err(VaultError::RevisionConflict);
        }
        let snapshot = HostPairingSnapshot {
            revision: 1,
            dataset_id: invite.dataset_id().to_owned(),
            host_device_id: device.device_id().to_owned(),
            code: PairingCode::parse(invite.code().expose_secret())?,
            invite_id: invite.invite_id().to_owned(),
            membership_root_hash: invite.membership_root_hash().to_owned(),
            membership_starting_head_hash: invite.membership_starting_head_hash().to_owned(),
            expires_at_unix: invite.expires_at_unix(),
            expected_local_revision,
            expected_private_revision,
            locator_ciphertext_hash: ciphertext_hash(LOCATOR_HASH_DOMAIN, locator),
            request_ciphertext_hash: None,
            request_device_id: None,
            handoff_membership_head_hash: None,
            handoff_checkpoint_hash: None,
            handoff_checkpoint_ciphertext_hash: None,
            handoff_approval_ciphertext_hash: None,
        };
        validate_snapshot(&snapshot)?;
        write_encrypted(self.paths.pairing_host_locator(), locator)?;
        self.write_state(device, &snapshot)?;
        Ok(snapshot)
    }

    pub fn locator(&self, snapshot: &HostPairingSnapshot) -> Result<EncryptedObject, VaultError> {
        let locator = read_encrypted(
            self.paths.pairing_host_locator(),
            MAX_ENCRYPTED_OBJECT_BYTES,
        )?;
        if ciphertext_hash(LOCATOR_HASH_DOMAIN, &locator) != snapshot.locator_ciphertext_hash {
            return Err(VaultError::InvalidPrivateStore);
        }
        Ok(locator)
    }

    pub fn bind_request(
        &self,
        device: &DeviceSecretMaterial,
        snapshot: &mut HostPairingSnapshot,
        request: &EncryptedObject,
        request_device_id: &DeviceId,
    ) -> Result<(), VaultError> {
        let request_hash = ciphertext_hash(REQUEST_HASH_DOMAIN, request);
        match (
            snapshot.request_ciphertext_hash.as_deref(),
            snapshot.request_device_id.as_deref(),
        ) {
            (Some(current_hash), Some(current_device))
                if current_hash == request_hash && current_device == request_device_id.as_str() =>
            {
                let durable_request = self.request(snapshot)?;
                if durable_request.as_bytes() != request.as_bytes() {
                    return Err(VaultError::PairingConsumed);
                }
                return Ok(());
            }
            (Some(_), Some(_)) | (Some(_), None) | (None, Some(_)) => {
                return Err(VaultError::PairingConsumed);
            }
            (None, None) => {}
        }
        write_encrypted(self.paths.pairing_host_request(), request)?;
        snapshot.request_ciphertext_hash = Some(request_hash);
        snapshot.request_device_id = Some(request_device_id.as_str().to_owned());
        self.save(device, snapshot)
    }

    pub fn request(&self, snapshot: &HostPairingSnapshot) -> Result<EncryptedObject, VaultError> {
        let expected_hash = snapshot
            .request_ciphertext_hash
            .as_deref()
            .ok_or(VaultError::InvalidPrivateStore)?;
        let request = read_encrypted(
            self.paths.pairing_host_request(),
            MAX_ENCRYPTED_OBJECT_BYTES,
        )?;
        if ciphertext_hash(REQUEST_HASH_DOMAIN, &request) != expected_hash {
            return Err(VaultError::InvalidPrivateStore);
        }
        Ok(request)
    }

    pub fn request_matches(
        &self,
        snapshot: &HostPairingSnapshot,
        request: &EncryptedObject,
        request_device_id: &DeviceId,
    ) -> bool {
        let request_hash = ciphertext_hash(REQUEST_HASH_DOMAIN, request);
        snapshot.request_ciphertext_hash.as_deref() == Some(request_hash.as_str())
            && snapshot.request_device_id.as_deref() == Some(request_device_id.as_str())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare_handoff(
        &self,
        device: &DeviceSecretMaterial,
        snapshot: &mut HostPairingSnapshot,
        membership_head_hash: String,
        checkpoint_hash: String,
        checkpoint: &EncryptedObject,
        approval: &EncryptedObject,
    ) -> Result<(), VaultError> {
        let checkpoint_ciphertext_hash = ciphertext_hash(CHECKPOINT_HASH_DOMAIN, checkpoint);
        let approval_ciphertext_hash = ciphertext_hash(APPROVAL_HASH_DOMAIN, approval);
        if snapshot.has_handoff() {
            if snapshot.handoff_membership_head_hash.as_deref()
                == Some(membership_head_hash.as_str())
                && snapshot.handoff_checkpoint_hash.as_deref() == Some(checkpoint_hash.as_str())
                && snapshot.handoff_checkpoint_ciphertext_hash.as_deref()
                    == Some(checkpoint_ciphertext_hash.as_str())
                && snapshot.handoff_approval_ciphertext_hash.as_deref()
                    == Some(approval_ciphertext_hash.as_str())
            {
                let _ = self.load_handoff(snapshot)?;
                return Ok(());
            }
            return Err(VaultError::MembershipFork);
        }
        if snapshot.request_ciphertext_hash.is_none() || snapshot.request_device_id.is_none() {
            return Err(VaultError::InvalidPrivateStore);
        }
        write_encrypted(self.paths.pairing_host_checkpoint(), checkpoint)?;
        write_encrypted(self.paths.pairing_host_approval(), approval)?;
        snapshot.handoff_membership_head_hash = Some(membership_head_hash);
        snapshot.handoff_checkpoint_hash = Some(checkpoint_hash);
        snapshot.handoff_checkpoint_ciphertext_hash = Some(checkpoint_ciphertext_hash);
        snapshot.handoff_approval_ciphertext_hash = Some(approval_ciphertext_hash);
        self.save(device, snapshot)
    }

    pub fn load_handoff(
        &self,
        snapshot: &HostPairingSnapshot,
    ) -> Result<DurableHandoff, VaultError> {
        if !snapshot.has_handoff() {
            return Err(VaultError::InvalidPrivateStore);
        }
        let checkpoint = read_encrypted(
            self.paths.pairing_host_checkpoint(),
            MAX_ENCRYPTED_OBJECT_BYTES,
        )?;
        let approval = read_encrypted(
            self.paths.pairing_host_approval(),
            MAX_ENCRYPTED_OBJECT_BYTES,
        )?;
        if snapshot.handoff_checkpoint_ciphertext_hash.as_deref()
            != Some(ciphertext_hash(CHECKPOINT_HASH_DOMAIN, &checkpoint).as_str())
            || snapshot.handoff_approval_ciphertext_hash.as_deref()
                != Some(ciphertext_hash(APPROVAL_HASH_DOMAIN, &approval).as_str())
        {
            return Err(VaultError::InvalidPrivateStore);
        }
        Ok(DurableHandoff {
            checkpoint,
            approval,
        })
    }

    pub fn remove(&self) -> Result<(), VaultError> {
        crate::util::safe_fs::remove_owner_only_file_durable(self.paths.pairing_host_state())
            .map_err(|_| VaultError::StorageFailed)?;
        for path in [
            self.paths.pairing_host_locator(),
            self.paths.pairing_host_request(),
            self.paths.pairing_host_checkpoint(),
            self.paths.pairing_host_approval(),
        ] {
            crate::util::safe_fs::remove_owner_only_file_durable(path)
                .map_err(|_| VaultError::StorageFailed)?;
        }
        Ok(())
    }

    fn save(
        &self,
        device: &DeviceSecretMaterial,
        snapshot: &mut HostPairingSnapshot,
    ) -> Result<(), VaultError> {
        let current = self.load(device)?.ok_or(VaultError::InvalidPrivateStore)?;
        if current.revision != snapshot.revision
            || current.invite_id != snapshot.invite_id
            || current.dataset_id != snapshot.dataset_id
            || current.host_device_id != snapshot.host_device_id
        {
            return Err(VaultError::RevisionConflict);
        }
        snapshot.revision = snapshot
            .revision
            .checked_add(1)
            .ok_or(VaultError::RevisionConflict)?;
        if let Err(error) = self.write_state(device, snapshot) {
            snapshot.revision = snapshot.revision.saturating_sub(1);
            return Err(error);
        }
        Ok(())
    }

    fn write_state(
        &self,
        device: &DeviceSecretMaterial,
        snapshot: &HostPairingSnapshot,
    ) -> Result<(), VaultError> {
        validate_snapshot(snapshot)?;
        if snapshot.host_device_id != device.device_id() {
            return Err(VaultError::InvalidPrivateStore);
        }
        let binding = snapshot.binding();
        let signature =
            sign_serializable(HOST_STATE_SIGNATURE_DOMAIN, device.signing_key(), &binding)?;
        let disk = DiskHostPairingState::from_snapshot(snapshot, signature);
        let bytes = serde_json::to_vec(&disk).map_err(|_| VaultError::SerializationFailed)?;
        if bytes.len() as u64 > MAX_HOST_STATE_BYTES {
            return Err(VaultError::PayloadTooLarge);
        }
        crate::util::safe_fs::write_owner_only_atomic(self.paths.pairing_host_state(), &bytes)
            .map_err(|_| VaultError::StorageFailed)
    }
}

impl HostPairingSnapshot {
    pub fn invite(&self) -> Result<PairingInvite, VaultError> {
        PairingInvite::resume(
            PairingCode::parse(self.code.expose_secret())?,
            self.dataset_id.clone(),
            self.membership_root_hash.clone(),
            self.membership_starting_head_hash.clone(),
            self.expires_at_unix,
        )
    }

    pub fn dataset_id(&self) -> &str {
        &self.dataset_id
    }

    pub fn host_device_id(&self) -> &str {
        &self.host_device_id
    }

    pub fn same_durable_record(&self, other: &Self) -> bool {
        self.revision == other.revision
            && self.dataset_id == other.dataset_id
            && self.host_device_id == other.host_device_id
            && self.code.expose_secret() == other.code.expose_secret()
            && self.invite_id == other.invite_id
            && self.membership_root_hash == other.membership_root_hash
            && self.membership_starting_head_hash == other.membership_starting_head_hash
            && self.expires_at_unix == other.expires_at_unix
            && self.expected_local_revision == other.expected_local_revision
            && self.expected_private_revision == other.expected_private_revision
            && self.locator_ciphertext_hash == other.locator_ciphertext_hash
            && self.request_ciphertext_hash == other.request_ciphertext_hash
            && self.request_device_id == other.request_device_id
            && self.handoff_membership_head_hash == other.handoff_membership_head_hash
            && self.handoff_checkpoint_hash == other.handoff_checkpoint_hash
            && self.handoff_checkpoint_ciphertext_hash == other.handoff_checkpoint_ciphertext_hash
            && self.handoff_approval_ciphertext_hash == other.handoff_approval_ciphertext_hash
    }

    pub fn has_bound_request(&self) -> bool {
        self.request_ciphertext_hash.is_some()
    }

    pub fn has_handoff(&self) -> bool {
        self.handoff_membership_head_hash.is_some()
    }

    fn binding(&self) -> HostStateBinding<'_> {
        HostStateBinding {
            kind: HOST_STATE_KIND,
            schema_version: HOST_STATE_SCHEMA_VERSION,
            revision: self.revision,
            dataset_id: &self.dataset_id,
            host_device_id: &self.host_device_id,
            code: self.code.expose_secret(),
            invite_id: &self.invite_id,
            membership_root_hash: &self.membership_root_hash,
            membership_starting_head_hash: &self.membership_starting_head_hash,
            expires_at_unix: self.expires_at_unix,
            expected_local_revision: self.expected_local_revision,
            expected_private_revision: self.expected_private_revision,
            locator_ciphertext_hash: &self.locator_ciphertext_hash,
            request_ciphertext_hash: self.request_ciphertext_hash.as_deref(),
            request_device_id: self.request_device_id.as_deref(),
            handoff_membership_head_hash: self.handoff_membership_head_hash.as_deref(),
            handoff_checkpoint_hash: self.handoff_checkpoint_hash.as_deref(),
            handoff_checkpoint_ciphertext_hash: self.handoff_checkpoint_ciphertext_hash.as_deref(),
            handoff_approval_ciphertext_hash: self.handoff_approval_ciphertext_hash.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct HostStateBinding<'a> {
    kind: &'a str,
    schema_version: u32,
    revision: u64,
    dataset_id: &'a str,
    host_device_id: &'a str,
    code: &'a str,
    invite_id: &'a str,
    membership_root_hash: &'a str,
    membership_starting_head_hash: &'a str,
    expires_at_unix: i64,
    expected_local_revision: u64,
    expected_private_revision: u64,
    locator_ciphertext_hash: &'a str,
    request_ciphertext_hash: Option<&'a str>,
    request_device_id: Option<&'a str>,
    handoff_membership_head_hash: Option<&'a str>,
    handoff_checkpoint_hash: Option<&'a str>,
    handoff_checkpoint_ciphertext_hash: Option<&'a str>,
    handoff_approval_ciphertext_hash: Option<&'a str>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskHostPairingState {
    kind: String,
    schema_version: u32,
    revision: u64,
    dataset_id: String,
    host_device_id: String,
    code: String,
    invite_id: String,
    membership_root_hash: String,
    membership_starting_head_hash: String,
    expires_at_unix: i64,
    expected_local_revision: u64,
    expected_private_revision: u64,
    locator_ciphertext_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_ciphertext_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff_membership_head_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff_checkpoint_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff_checkpoint_ciphertext_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff_approval_ciphertext_hash: Option<String>,
    signature: String,
}

impl DiskHostPairingState {
    fn from_snapshot(snapshot: &HostPairingSnapshot, signature: String) -> Self {
        Self {
            kind: HOST_STATE_KIND.to_owned(),
            schema_version: HOST_STATE_SCHEMA_VERSION,
            revision: snapshot.revision,
            dataset_id: snapshot.dataset_id.clone(),
            host_device_id: snapshot.host_device_id.clone(),
            code: snapshot.code.expose_secret().to_owned(),
            invite_id: snapshot.invite_id.clone(),
            membership_root_hash: snapshot.membership_root_hash.clone(),
            membership_starting_head_hash: snapshot.membership_starting_head_hash.clone(),
            expires_at_unix: snapshot.expires_at_unix,
            expected_local_revision: snapshot.expected_local_revision,
            expected_private_revision: snapshot.expected_private_revision,
            locator_ciphertext_hash: snapshot.locator_ciphertext_hash.clone(),
            request_ciphertext_hash: snapshot.request_ciphertext_hash.clone(),
            request_device_id: snapshot.request_device_id.clone(),
            handoff_membership_head_hash: snapshot.handoff_membership_head_hash.clone(),
            handoff_checkpoint_hash: snapshot.handoff_checkpoint_hash.clone(),
            handoff_checkpoint_ciphertext_hash: snapshot.handoff_checkpoint_ciphertext_hash.clone(),
            handoff_approval_ciphertext_hash: snapshot.handoff_approval_ciphertext_hash.clone(),
            signature,
        }
    }

    fn take_snapshot(
        &mut self,
        device: &DeviceSecretMaterial,
    ) -> Result<HostPairingSnapshot, VaultError> {
        let binding = HostStateBinding {
            kind: &self.kind,
            schema_version: self.schema_version,
            revision: self.revision,
            dataset_id: &self.dataset_id,
            host_device_id: &self.host_device_id,
            code: &self.code,
            invite_id: &self.invite_id,
            membership_root_hash: &self.membership_root_hash,
            membership_starting_head_hash: &self.membership_starting_head_hash,
            expires_at_unix: self.expires_at_unix,
            expected_local_revision: self.expected_local_revision,
            expected_private_revision: self.expected_private_revision,
            locator_ciphertext_hash: &self.locator_ciphertext_hash,
            request_ciphertext_hash: self.request_ciphertext_hash.as_deref(),
            request_device_id: self.request_device_id.as_deref(),
            handoff_membership_head_hash: self.handoff_membership_head_hash.as_deref(),
            handoff_checkpoint_hash: self.handoff_checkpoint_hash.as_deref(),
            handoff_checkpoint_ciphertext_hash: self.handoff_checkpoint_ciphertext_hash.as_deref(),
            handoff_approval_ciphertext_hash: self.handoff_approval_ciphertext_hash.as_deref(),
        };
        if self.kind != HOST_STATE_KIND
            || self.schema_version != HOST_STATE_SCHEMA_VERSION
            || self.host_device_id != device.device_id()
        {
            return Err(VaultError::InvalidPrivateStore);
        }
        verify_serializable(
            HOST_STATE_SIGNATURE_DOMAIN,
            &device.public_identity().ed25519_verifying_key,
            &binding,
            &self.signature,
        )
        .map_err(|_| VaultError::InvalidPrivateStore)?;
        let snapshot = HostPairingSnapshot {
            revision: self.revision,
            dataset_id: std::mem::take(&mut self.dataset_id),
            host_device_id: std::mem::take(&mut self.host_device_id),
            code: PairingCode::parse(&self.code)?,
            invite_id: std::mem::take(&mut self.invite_id),
            membership_root_hash: std::mem::take(&mut self.membership_root_hash),
            membership_starting_head_hash: std::mem::take(&mut self.membership_starting_head_hash),
            expires_at_unix: self.expires_at_unix,
            expected_local_revision: self.expected_local_revision,
            expected_private_revision: self.expected_private_revision,
            locator_ciphertext_hash: std::mem::take(&mut self.locator_ciphertext_hash),
            request_ciphertext_hash: self.request_ciphertext_hash.take(),
            request_device_id: self.request_device_id.take(),
            handoff_membership_head_hash: self.handoff_membership_head_hash.take(),
            handoff_checkpoint_hash: self.handoff_checkpoint_hash.take(),
            handoff_checkpoint_ciphertext_hash: self.handoff_checkpoint_ciphertext_hash.take(),
            handoff_approval_ciphertext_hash: self.handoff_approval_ciphertext_hash.take(),
        };
        validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }
}

impl Drop for DiskHostPairingState {
    fn drop(&mut self) {
        self.code.zeroize();
        self.signature.zeroize();
    }
}

fn validate_snapshot(snapshot: &HostPairingSnapshot) -> Result<(), VaultError> {
    let parsed_code = PairingCode::parse(snapshot.code.expose_secret())?;
    let derived_invite = PairingInvite::invite_id_for_code(&parsed_code)?;
    let handoff_presence = [
        snapshot.handoff_membership_head_hash.is_some(),
        snapshot.handoff_checkpoint_hash.is_some(),
        snapshot.handoff_checkpoint_ciphertext_hash.is_some(),
        snapshot.handoff_approval_ciphertext_hash.is_some(),
    ];
    if snapshot.revision == 0
        || super::super::super::crypto::validate_dataset_id(&snapshot.dataset_id).is_err()
        || DeviceId::new(snapshot.host_device_id.as_str()).is_err()
        || derived_invite != snapshot.invite_id
        || !valid_hex(&snapshot.invite_id, 32)
        || !valid_hash(&snapshot.membership_root_hash)
        || !valid_hash(&snapshot.membership_starting_head_hash)
        || !valid_hash(&snapshot.locator_ciphertext_hash)
        || snapshot
            .request_ciphertext_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
        || snapshot
            .request_device_id
            .as_deref()
            .is_some_and(|device_id| DeviceId::new(device_id).is_err())
        || snapshot.request_ciphertext_hash.is_some() != snapshot.request_device_id.is_some()
        || handoff_presence
            .iter()
            .any(|present| *present != handoff_presence[0])
        || snapshot.has_handoff() && snapshot.request_ciphertext_hash.is_none()
        || snapshot
            .handoff_membership_head_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
        || snapshot
            .handoff_checkpoint_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
        || snapshot
            .handoff_checkpoint_ciphertext_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
        || snapshot
            .handoff_approval_ciphertext_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    Ok(())
}

fn write_encrypted(path: &std::path::Path, object: &EncryptedObject) -> Result<(), VaultError> {
    crate::util::safe_fs::write_owner_only_atomic(path, object.as_bytes())
        .map_err(|_| VaultError::StorageFailed)
}

fn read_encrypted(path: &std::path::Path, max_bytes: usize) -> Result<EncryptedObject, VaultError> {
    let bytes = crate::util::safe_fs::read_owner_only_limited(path, max_bytes as u64)
        .map_err(|_| VaultError::StorageFailed)?;
    EncryptedObject::from_bytes(bytes).map_err(|_| VaultError::InvalidEncryptedObject)
}

fn ciphertext_hash(domain: &[u8], object: &EncryptedObject) -> String {
    sha256_domain_hex(domain, &[object.as_bytes()])
}

fn valid_hash(value: &str) -> bool {
    valid_hex(value, 64)
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
#[path = "durable/tests.rs"]
mod tests;
