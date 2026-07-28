use serde::{Deserialize, Serialize};

use crate::personal_state::DeviceId;

use super::super::checkpoint::SignedCheckpoint;
use super::super::crypto::{
    DeviceSecretMaterial, EncryptedObject, decrypt_json_with_identity, encrypt_json_to_recipients,
    sign_serializable, verify_serializable,
};
use super::super::membership::{MembershipAnchor, MembershipChain, VerifiedMembership};
use super::super::{ObjectKey, VAULT_SCHEMA_VERSION, VaultError};

const MANIFEST_KIND: &str = "yututui_vault_manifest";
const DEVICE_HEAD_KIND: &str = "yututui_vault_device_head";
const MANIFEST_SIGNATURE_DOMAIN: &[u8] = b"yututui-vault-manifest-signature-v1";
const DEVICE_HEAD_SIGNATURE_DOMAIN: &[u8] = b"yututui-vault-device-head-signature-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultManifestPayload {
    pub kind: String,
    pub schema_version: u32,
    pub dataset_id: String,
    pub generation: u64,
    pub signer_device_id: DeviceId,
    pub membership: MembershipChain,
    pub membership_head_hash: String,
    pub registry_key: ObjectKey,
    pub checkpoint_key: ObjectKey,
    pub checkpoint_sequence: u64,
    pub checkpoint_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedVaultManifest {
    pub payload: VaultManifestPayload,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceHeadPayload {
    pub kind: String,
    pub schema_version: u32,
    pub dataset_id: String,
    pub membership_epoch: u64,
    pub signer_device_id: DeviceId,
    pub last_sequence: u64,
    pub last_batch_hash: String,
    pub last_segment_key: ObjectKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedDeviceHead {
    pub payload: DeviceHeadPayload,
    pub signature: String,
}

impl SignedVaultManifest {
    pub(crate) fn create(
        dataset_id: &str,
        generation: u64,
        membership: MembershipChain,
        membership_anchor: &MembershipAnchor,
        signer_device_id: DeviceId,
        device: &DeviceSecretMaterial,
        checkpoint: &SignedCheckpoint,
    ) -> Result<Self, VaultError> {
        let verified = membership.verify(membership_anchor)?;
        let checkpoint_membership = checkpoint.verify(membership_anchor)?;
        if generation == 0
            || dataset_id != verified.dataset_id
            || signer_device_id.as_str() != device.device_id()
            || checkpoint.payload.membership != membership
            || checkpoint_membership.head_hash != verified.head_hash
        {
            return Err(VaultError::InvalidEncryptedObject);
        }
        let checkpoint_hash = checkpoint.hash()?;
        let checkpoint_key = super::checkpoint_key(
            dataset_id,
            checkpoint.payload.membership_epoch,
            &checkpoint_hash,
        )?;
        let registry_key = super::registry_key(dataset_id, &verified.head_hash)?;
        let payload = VaultManifestPayload {
            kind: MANIFEST_KIND.to_owned(),
            schema_version: VAULT_SCHEMA_VERSION,
            dataset_id: dataset_id.to_owned(),
            generation,
            signer_device_id,
            membership,
            membership_head_hash: verified.head_hash.clone(),
            registry_key,
            checkpoint_key,
            checkpoint_sequence: checkpoint.payload.checkpoint_sequence,
            checkpoint_hash,
        };
        validate_manifest_payload(&payload, membership_anchor)?;
        let signature =
            sign_serializable(MANIFEST_SIGNATURE_DOMAIN, device.signing_key(), &payload)?;
        let manifest = Self { payload, signature };
        manifest.verify(membership_anchor)?;
        Ok(manifest)
    }

    pub(crate) fn verify(
        &self,
        membership_anchor: &MembershipAnchor,
    ) -> Result<VerifiedMembership, VaultError> {
        let verified = validate_manifest_payload(&self.payload, membership_anchor)?;
        let signer = active_signer(&verified, &self.payload.signer_device_id)?;
        verify_serializable(
            MANIFEST_SIGNATURE_DOMAIN,
            &signer.ed25519_verifying_key,
            &self.payload,
            &self.signature,
        )?;
        Ok(verified)
    }

    pub(crate) fn encrypt(
        &self,
        membership_anchor: &MembershipAnchor,
    ) -> Result<EncryptedObject, VaultError> {
        let verified = self.verify(membership_anchor)?;
        encrypt_json_to_recipients(self, &verified.active_recipients()?)
    }

    pub(crate) fn decrypt_for_device(
        object: &EncryptedObject,
        device: &DeviceSecretMaterial,
        membership_anchor: &MembershipAnchor,
    ) -> Result<Self, VaultError> {
        let manifest: Self = decrypt_json_with_identity(object, device.age_identity())?;
        let membership = manifest.verify(membership_anchor)?;
        let record = membership
            .devices
            .get(&manifest.payload.signer_device_id)
            .filter(|record| !record.revoked)
            .ok_or(VaultError::RevokedOrUnknownDevice)?;
        if !device_is_active_recipient(device, &membership) || record.public_identity.is_none() {
            return Err(VaultError::RevokedOrUnknownDevice);
        }
        Ok(manifest)
    }
}

impl SignedDeviceHead {
    pub(crate) fn create(
        dataset_id: &str,
        membership: &VerifiedMembership,
        signer_device_id: DeviceId,
        device: &DeviceSecretMaterial,
        last_sequence: u64,
        last_batch_hash: String,
        last_segment_key: ObjectKey,
    ) -> Result<Self, VaultError> {
        if signer_device_id.as_str() != device.device_id() {
            return Err(VaultError::InvalidDeviceIdentity);
        }
        let payload = DeviceHeadPayload {
            kind: DEVICE_HEAD_KIND.to_owned(),
            schema_version: VAULT_SCHEMA_VERSION,
            dataset_id: dataset_id.to_owned(),
            membership_epoch: membership.epoch,
            signer_device_id,
            last_sequence,
            last_batch_hash,
            last_segment_key,
        };
        validate_head_payload(&payload, membership)?;
        let signature =
            sign_serializable(DEVICE_HEAD_SIGNATURE_DOMAIN, device.signing_key(), &payload)?;
        let head = Self { payload, signature };
        head.verify(membership)?;
        Ok(head)
    }

    pub(crate) fn verify(&self, membership: &VerifiedMembership) -> Result<(), VaultError> {
        validate_head_payload(&self.payload, membership)?;
        let signer = membership.device(&self.payload.signer_device_id)?;
        let identity = signer
            .public_identity
            .as_ref()
            .ok_or(VaultError::InvalidDeviceIdentity)?;
        verify_serializable(
            DEVICE_HEAD_SIGNATURE_DOMAIN,
            &identity.ed25519_verifying_key,
            &self.payload,
            &self.signature,
        )
    }

    pub(crate) fn encrypt(
        &self,
        membership: &VerifiedMembership,
    ) -> Result<EncryptedObject, VaultError> {
        self.verify(membership)?;
        encrypt_json_to_recipients(self, &membership.active_recipients()?)
    }

    pub(crate) fn decrypt_for_device(
        object: &EncryptedObject,
        device: &DeviceSecretMaterial,
        membership: &VerifiedMembership,
    ) -> Result<Self, VaultError> {
        if !device_is_active_recipient(device, membership) {
            return Err(VaultError::RevokedOrUnknownDevice);
        }
        let head: Self = decrypt_json_with_identity(object, device.age_identity())?;
        head.verify(membership)?;
        Ok(head)
    }
}

fn validate_manifest_payload(
    payload: &VaultManifestPayload,
    membership_anchor: &MembershipAnchor,
) -> Result<VerifiedMembership, VaultError> {
    let membership = payload.membership.verify(membership_anchor)?;
    if payload.kind != MANIFEST_KIND
        || payload.schema_version != VAULT_SCHEMA_VERSION
        || payload.generation == 0
        || payload.dataset_id != membership.dataset_id
        || payload.membership_head_hash != membership.head_hash
        || payload.registry_key
            != super::registry_key(&payload.dataset_id, &payload.membership_head_hash)?
        || payload.checkpoint_sequence == 0
        || !valid_hash(&payload.checkpoint_hash)
        || payload.checkpoint_key
            != super::checkpoint_key(
                &payload.dataset_id,
                membership.epoch,
                &payload.checkpoint_hash,
            )?
    {
        return Err(VaultError::InvalidEncryptedObject);
    }
    active_signer(&membership, &payload.signer_device_id)?;
    Ok(membership)
}

fn validate_head_payload(
    payload: &DeviceHeadPayload,
    membership: &VerifiedMembership,
) -> Result<(), VaultError> {
    if payload.kind != DEVICE_HEAD_KIND
        || payload.schema_version != VAULT_SCHEMA_VERSION
        || payload.dataset_id != membership.dataset_id
        || payload.membership_epoch == 0
        || payload.membership_epoch > membership.epoch
        || payload.last_sequence == 0
        || !valid_hash(&payload.last_batch_hash)
        || payload.last_segment_key
            != super::segment_key(
                &payload.dataset_id,
                &payload.signer_device_id,
                segment_bounds(&payload.last_segment_key)?.0,
                payload.last_sequence,
            )?
        || !membership.was_authorized_at_epoch(&payload.signer_device_id, payload.membership_epoch)
        || !membership.accepts_sequence(&payload.signer_device_id, payload.last_sequence)
    {
        return Err(VaultError::InvalidEncryptedObject);
    }
    Ok(())
}

pub(crate) fn segment_bounds(key: &ObjectKey) -> Result<(u64, u64), VaultError> {
    let file = key
        .as_str()
        .rsplit('/')
        .next()
        .ok_or(VaultError::InvalidObjectKey)?;
    let range = file
        .strip_suffix(".age")
        .ok_or(VaultError::InvalidObjectKey)?;
    let (first, last) = range.split_once('-').ok_or(VaultError::InvalidObjectKey)?;
    let first = canonical_sequence(first)?;
    let last = canonical_sequence(last)?;
    if first == 0 || first > last {
        return Err(VaultError::InvalidObjectKey);
    }
    Ok((first, last))
}

fn canonical_sequence(value: &str) -> Result<u64, VaultError> {
    let sequence = value
        .parse::<u64>()
        .map_err(|_| VaultError::InvalidObjectKey)?;
    if sequence.to_string() != value {
        return Err(VaultError::InvalidObjectKey);
    }
    Ok(sequence)
}

fn active_signer<'a>(
    membership: &'a VerifiedMembership,
    device_id: &DeviceId,
) -> Result<&'a crate::personal_state::DevicePublicIdentity, VaultError> {
    membership
        .devices
        .get(device_id)
        .filter(|device| !device.revoked)
        .and_then(|device| device.public_identity.as_ref())
        .ok_or(VaultError::RevokedOrUnknownDevice)
}

fn device_is_active_recipient(
    device: &DeviceSecretMaterial,
    membership: &VerifiedMembership,
) -> bool {
    membership
        .devices
        .values()
        .find(|record| record.device_id.as_str() == device.device_id())
        .is_some_and(|record| !record.revoked && device.matches_personal_record(record))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}
