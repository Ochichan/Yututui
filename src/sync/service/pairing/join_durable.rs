//! Signed owner-only crash journal for a joining device.
//!
//! The journal retains the one-time code only until the authenticated first merge commits. This
//! lets a restarted join consume an approval that the host committed before expiry without asking
//! the user to re-enter the code. Its signature and exact request-ciphertext hash also distinguish
//! our own incomplete setup artifacts from unrelated or replaced files.

use std::io;

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::personal_state::{DeviceId, DeviceRecord};

use super::super::super::crypto::{
    MAX_ENCRYPTED_OBJECT_BYTES, sha256_domain_hex, sign_serializable, verify_serializable,
};
use super::super::super::{
    DeviceSecretMaterial, EncryptedObject, PairingCode, PairingInvite, SyncPaths, VaultError,
};

const JOIN_STATE_KIND: &str = "yututui_pairing_join_state";
const JOIN_STATE_SCHEMA_VERSION: u32 = 1;
const JOIN_STATE_SIGNATURE_DOMAIN: &[u8] = b"yututui-pairing-join-state-signature-v1";
const JOIN_REQUEST_HASH_DOMAIN: &[u8] = b"yututui-pairing-join-request-ciphertext-v1";
const MAX_JOIN_STATE_BYTES: u64 = 128 * 1024;

pub(super) struct JoinPairingStore<'a> {
    paths: &'a SyncPaths,
}

pub(super) struct JoinPairingSnapshot {
    revision: u64,
    dataset_id: String,
    device_id: String,
    code: PairingCode,
    invite_id: String,
    request_nonce: String,
    expires_at_unix: i64,
    request_ciphertext_hash: String,
}

impl<'a> JoinPairingStore<'a> {
    pub fn new(paths: &'a SyncPaths) -> Self {
        Self { paths }
    }

    pub fn create(
        &self,
        device: &DeviceSecretMaterial,
        code: &PairingCode,
        dataset_id: &str,
        expires_at_unix: i64,
        request_nonce: &str,
        request: &EncryptedObject,
    ) -> Result<JoinPairingSnapshot, VaultError> {
        if self.load(device)?.is_some() {
            return Err(VaultError::RevisionConflict);
        }
        let payload = PairingInvite::open_journaled_request(code, request)?;
        if !device.matches_record(&payload.device) || payload.request_nonce != request_nonce {
            return Err(VaultError::PairingProofFailed);
        }
        let snapshot = JoinPairingSnapshot {
            revision: 1,
            dataset_id: dataset_id.to_owned(),
            device_id: device.device_id().to_owned(),
            code: PairingCode::parse(code.expose_secret())?,
            invite_id: payload.invite_id,
            request_nonce: request_nonce.to_owned(),
            expires_at_unix,
            request_ciphertext_hash: request_hash(request),
        };
        validate_snapshot(
            &snapshot,
            &payload.device,
            &payload.request_nonce,
            payload.issued_at_unix,
            payload.expires_at_unix,
        )?;
        write_request(self.paths.pending_join_request(), request)?;
        self.write_state(device, &snapshot)?;
        Ok(snapshot)
    }

    pub fn load(
        &self,
        device: &DeviceSecretMaterial,
    ) -> Result<Option<JoinPairingSnapshot>, VaultError> {
        let Some(mut disk) = self.read_disk()? else {
            return Ok(None);
        };
        let snapshot = disk.take_signed_snapshot(
            device.device_id(),
            &device.public_identity().ed25519_verifying_key,
        )?;
        validate_snapshot_basic(&snapshot)?;
        let request = read_request(self.paths.pending_join_request())?;
        if request_hash(&request) != snapshot.request_ciphertext_hash {
            return Err(VaultError::InvalidPrivateStore);
        }
        Ok(Some(snapshot))
    }

    /// Authenticate the journal through the code-bound request when its secret keys are missing.
    ///
    /// This is sufficient for explicit cleanup, but never for creating membership or replacing
    /// the lost device keys.
    pub fn load_authenticated(
        &self,
    ) -> Result<Option<(JoinPairingSnapshot, DeviceRecord)>, VaultError> {
        let Some(mut disk) = self.read_disk()? else {
            return Ok(None);
        };
        let code = PairingCode::parse(&disk.code)?;
        let request = read_request(self.paths.pending_join_request())?;
        let payload = PairingInvite::open_journaled_request(&code, &request)?;
        let record = payload.device.clone();
        let identity = record
            .public_identity
            .as_ref()
            .ok_or(VaultError::InvalidPrivateStore)?;
        let snapshot =
            disk.take_signed_snapshot(record.device_id.as_str(), &identity.ed25519_verifying_key)?;
        validate_snapshot(
            &snapshot,
            &record,
            &payload.request_nonce,
            payload.issued_at_unix,
            payload.expires_at_unix,
        )?;
        if request_hash(&request) != snapshot.request_ciphertext_hash {
            return Err(VaultError::InvalidPrivateStore);
        }
        Ok(Some((snapshot, record)))
    }

    pub fn request(&self, snapshot: &JoinPairingSnapshot) -> Result<EncryptedObject, VaultError> {
        let request = read_request(self.paths.pending_join_request())?;
        if request_hash(&request) != snapshot.request_ciphertext_hash {
            return Err(VaultError::InvalidPrivateStore);
        }
        Ok(request)
    }

    pub fn remove_state(&self) -> Result<(), VaultError> {
        crate::util::safe_fs::remove_owner_only_file_durable(self.paths.pending_join_state())
            .map_err(|_| VaultError::StorageFailed)
    }

    fn read_disk(&self) -> Result<Option<DiskJoinPairingState>, VaultError> {
        let bytes = match crate::util::safe_fs::read_owner_only_limited(
            self.paths.pending_join_state(),
            MAX_JOIN_STATE_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(VaultError::StorageFailed),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| VaultError::InvalidPrivateStore)
    }

    fn write_state(
        &self,
        device: &DeviceSecretMaterial,
        snapshot: &JoinPairingSnapshot,
    ) -> Result<(), VaultError> {
        if snapshot.device_id != device.device_id() {
            return Err(VaultError::InvalidPrivateStore);
        }
        let signature = sign_serializable(
            JOIN_STATE_SIGNATURE_DOMAIN,
            device.signing_key(),
            &snapshot.binding(),
        )?;
        let disk = DiskJoinPairingState::from_snapshot(snapshot, signature);
        let bytes = serde_json::to_vec(&disk).map_err(|_| VaultError::SerializationFailed)?;
        if bytes.len() as u64 > MAX_JOIN_STATE_BYTES {
            return Err(VaultError::PayloadTooLarge);
        }
        crate::util::safe_fs::write_owner_only_atomic(self.paths.pending_join_state(), &bytes)
            .map_err(|_| VaultError::StorageFailed)
    }
}

impl JoinPairingSnapshot {
    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    pub fn dataset_id(&self) -> &str {
        &self.dataset_id
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn invite_id(&self) -> &str {
        &self.invite_id
    }

    pub fn request_nonce(&self) -> &str {
        &self.request_nonce
    }

    pub fn expires_at_unix(&self) -> i64 {
        self.expires_at_unix
    }

    pub fn matches_context(
        &self,
        dataset_id: &str,
        device_id: &str,
        invite_id: &str,
        request_nonce: &str,
    ) -> bool {
        self.dataset_id == dataset_id
            && self.device_id == device_id
            && self.invite_id == invite_id
            && self.request_nonce == request_nonce
    }

    fn binding(&self) -> JoinStateBinding<'_> {
        JoinStateBinding {
            kind: JOIN_STATE_KIND,
            schema_version: JOIN_STATE_SCHEMA_VERSION,
            revision: self.revision,
            dataset_id: &self.dataset_id,
            device_id: &self.device_id,
            code: self.code.expose_secret(),
            invite_id: &self.invite_id,
            request_nonce: &self.request_nonce,
            expires_at_unix: self.expires_at_unix,
            request_ciphertext_hash: &self.request_ciphertext_hash,
        }
    }
}

#[derive(Serialize)]
struct JoinStateBinding<'a> {
    kind: &'a str,
    schema_version: u32,
    revision: u64,
    dataset_id: &'a str,
    device_id: &'a str,
    code: &'a str,
    invite_id: &'a str,
    request_nonce: &'a str,
    expires_at_unix: i64,
    request_ciphertext_hash: &'a str,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskJoinPairingState {
    kind: String,
    schema_version: u32,
    revision: u64,
    dataset_id: String,
    device_id: String,
    code: String,
    invite_id: String,
    request_nonce: String,
    expires_at_unix: i64,
    request_ciphertext_hash: String,
    signature: String,
}

impl DiskJoinPairingState {
    fn from_snapshot(snapshot: &JoinPairingSnapshot, signature: String) -> Self {
        Self {
            kind: JOIN_STATE_KIND.to_owned(),
            schema_version: JOIN_STATE_SCHEMA_VERSION,
            revision: snapshot.revision,
            dataset_id: snapshot.dataset_id.clone(),
            device_id: snapshot.device_id.clone(),
            code: snapshot.code.expose_secret().to_owned(),
            invite_id: snapshot.invite_id.clone(),
            request_nonce: snapshot.request_nonce.clone(),
            expires_at_unix: snapshot.expires_at_unix,
            request_ciphertext_hash: snapshot.request_ciphertext_hash.clone(),
            signature,
        }
    }

    fn take_signed_snapshot(
        &mut self,
        device_id: &str,
        verifying_key: &str,
    ) -> Result<JoinPairingSnapshot, VaultError> {
        let binding = JoinStateBinding {
            kind: &self.kind,
            schema_version: self.schema_version,
            revision: self.revision,
            dataset_id: &self.dataset_id,
            device_id: &self.device_id,
            code: &self.code,
            invite_id: &self.invite_id,
            request_nonce: &self.request_nonce,
            expires_at_unix: self.expires_at_unix,
            request_ciphertext_hash: &self.request_ciphertext_hash,
        };
        verify_serializable(
            JOIN_STATE_SIGNATURE_DOMAIN,
            verifying_key,
            &binding,
            &self.signature,
        )
        .map_err(|_| VaultError::InvalidPrivateStore)?;
        let snapshot = JoinPairingSnapshot {
            revision: self.revision,
            dataset_id: std::mem::take(&mut self.dataset_id),
            device_id: std::mem::take(&mut self.device_id),
            code: PairingCode::parse(&self.code)?,
            invite_id: std::mem::take(&mut self.invite_id),
            request_nonce: std::mem::take(&mut self.request_nonce),
            expires_at_unix: self.expires_at_unix,
            request_ciphertext_hash: std::mem::take(&mut self.request_ciphertext_hash),
        };
        if snapshot.device_id != device_id {
            return Err(VaultError::InvalidPrivateStore);
        }
        Ok(snapshot)
    }
}

impl Drop for DiskJoinPairingState {
    fn drop(&mut self) {
        self.code.zeroize();
        self.signature.zeroize();
    }
}

fn validate_snapshot(
    snapshot: &JoinPairingSnapshot,
    record: &DeviceRecord,
    request_nonce: &str,
    request_issued_at_unix: i64,
    request_expires_at_unix: i64,
) -> Result<(), VaultError> {
    validate_snapshot_basic(snapshot)?;
    if record.device_id.as_str() != snapshot.device_id
        || snapshot.request_nonce != request_nonce
        || snapshot.expires_at_unix < request_issued_at_unix
        || snapshot.expires_at_unix > request_expires_at_unix
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    Ok(())
}

fn validate_snapshot_basic(snapshot: &JoinPairingSnapshot) -> Result<(), VaultError> {
    let derived_invite = PairingInvite::invite_id_for_code(&snapshot.code)?;
    if snapshot.revision == 0
        || super::super::super::crypto::validate_dataset_id(&snapshot.dataset_id).is_err()
        || DeviceId::new(snapshot.device_id.as_str()).is_err()
        || derived_invite != snapshot.invite_id
        || snapshot.request_nonce.is_empty()
        || !valid_hash(&snapshot.request_ciphertext_hash)
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    Ok(())
}

fn request_hash(request: &EncryptedObject) -> String {
    sha256_domain_hex(JOIN_REQUEST_HASH_DOMAIN, &[request.as_bytes()])
}

fn write_request(path: &std::path::Path, request: &EncryptedObject) -> Result<(), VaultError> {
    crate::util::safe_fs::write_owner_only_atomic(path, request.as_bytes())
        .map_err(|_| VaultError::StorageFailed)
}

fn read_request(path: &std::path::Path) -> Result<EncryptedObject, VaultError> {
    let bytes =
        crate::util::safe_fs::read_owner_only_limited(path, MAX_ENCRYPTED_OBJECT_BYTES as u64)
            .map_err(|_| VaultError::StorageFailed)?;
    EncryptedObject::from_bytes(bytes).map_err(|_| VaultError::InvalidEncryptedObject)
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}
