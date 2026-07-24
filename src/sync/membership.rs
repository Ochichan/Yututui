use std::collections::{BTreeMap, BTreeSet};

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use crate::personal_state::{DeviceId, DevicePublicIdentity, DeviceRecord, DeviceRegistry};

use super::crypto::{
    sha256_domain_hex, sign_serializable, verify_serializable, verifying_key_to_base64,
};
use super::error::VaultError;

const ROOT_KIND: &str = "yututui_vault_membership_root";
const CHANGE_KIND: &str = "yututui_vault_membership_change";
const ROOT_SIGNATURE_DOMAIN: &[u8] = b"yututui-vault-membership-root-signature-v1";
const CHANGE_SIGNATURE_DOMAIN: &[u8] = b"yututui-vault-membership-change-signature-v1";
const ROOT_HASH_DOMAIN: &[u8] = b"yututui-vault-membership-root-hash-v1";
const CHANGE_HASH_DOMAIN: &[u8] = b"yututui-vault-membership-change-hash-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipRootPayload {
    pub kind: String,
    pub schema_version: u32,
    pub dataset_id: String,
    pub membership_epoch: u64,
    pub recovery_recipient: String,
    pub recovery_verifying_key: String,
    pub initial_device: DeviceRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedMembershipRoot {
    pub payload: MembershipRootPayload,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum MembershipActor {
    Device { device_id: DeviceId },
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCutoff {
    pub device_id: DeviceId,
    pub last_accepted_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum MembershipAction {
    AddDevice {
        device: DeviceRecord,
    },
    RevokeDevice {
        device_id: DeviceId,
        last_accepted_sequence: u64,
    },
    /// Replace every active device after all device keys were lost.
    Recover {
        new_device: DeviceRecord,
        revoked_devices: Vec<RecoveryCutoff>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipChangePayload {
    pub kind: String,
    pub schema_version: u32,
    pub dataset_id: String,
    pub membership_epoch: u64,
    pub previous_membership_hash: String,
    pub actor: MembershipActor,
    pub action: MembershipAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedMembershipChange {
    pub payload: MembershipChangePayload,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipChain {
    pub root: SignedMembershipRoot,
    #[serde(default)]
    pub changes: Vec<SignedMembershipChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipAnchor {
    RootHash(String),
    RecoveryVerifyingKey(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedMembership {
    pub dataset_id: String,
    pub epoch: u64,
    pub root_hash: String,
    pub head_hash: String,
    pub recovery_recipient: String,
    pub recovery_verifying_key: String,
    pub devices: DeviceRegistry,
    pub revocation_cutoffs: BTreeMap<DeviceId, u64>,
    admission_epochs: BTreeMap<DeviceId, u64>,
    revocation_epochs: BTreeMap<DeviceId, u64>,
}

impl SignedMembershipRoot {
    pub fn create(
        dataset_id: impl Into<String>,
        recovery_recipient: impl Into<String>,
        recovery_signing_key: &SigningKey,
        initial_device: DeviceRecord,
    ) -> Result<Self, VaultError> {
        let payload = MembershipRootPayload {
            kind: ROOT_KIND.to_owned(),
            schema_version: super::VAULT_SCHEMA_VERSION,
            dataset_id: dataset_id.into(),
            membership_epoch: 1,
            recovery_recipient: recovery_recipient.into(),
            recovery_verifying_key: verifying_key_to_base64(&recovery_signing_key.verifying_key()),
            initial_device,
        };
        validate_root_payload(&payload)?;
        let signature = sign_serializable(ROOT_SIGNATURE_DOMAIN, recovery_signing_key, &payload)?;
        let root = Self { payload, signature };
        root.verify_signature()?;
        Ok(root)
    }

    pub fn hash(&self) -> Result<String, VaultError> {
        let bytes = serde_json::to_vec(self).map_err(|_| VaultError::SerializationFailed)?;
        Ok(sha256_domain_hex(ROOT_HASH_DOMAIN, &[&bytes]))
    }

    fn verify_signature(&self) -> Result<(), VaultError> {
        validate_root_payload(&self.payload)?;
        verify_serializable(
            ROOT_SIGNATURE_DOMAIN,
            &self.payload.recovery_verifying_key,
            &self.payload,
            &self.signature,
        )
    }
}

impl SignedMembershipChange {
    pub fn hash(&self) -> Result<String, VaultError> {
        let bytes = serde_json::to_vec(self).map_err(|_| VaultError::SerializationFailed)?;
        Ok(sha256_domain_hex(CHANGE_HASH_DOMAIN, &[&bytes]))
    }
}

impl MembershipChain {
    pub fn new(root: SignedMembershipRoot) -> Self {
        Self {
            root,
            changes: Vec::new(),
        }
    }

    pub fn verify(&self, anchor: &MembershipAnchor) -> Result<VerifiedMembership, VaultError> {
        self.root.verify_signature()?;
        let root_hash = self.root.hash()?;
        match anchor {
            MembershipAnchor::RootHash(expected) if expected != &root_hash => {
                return Err(VaultError::MembershipAnchorMismatch);
            }
            MembershipAnchor::RecoveryVerifyingKey(expected)
                if expected != &self.root.payload.recovery_verifying_key =>
            {
                return Err(VaultError::MembershipAnchorMismatch);
            }
            _ => {}
        }

        let mut verified = VerifiedMembership {
            dataset_id: self.root.payload.dataset_id.clone(),
            epoch: self.root.payload.membership_epoch,
            root_hash: root_hash.clone(),
            head_hash: root_hash,
            recovery_recipient: self.root.payload.recovery_recipient.clone(),
            recovery_verifying_key: self.root.payload.recovery_verifying_key.clone(),
            devices: BTreeMap::from([(
                self.root.payload.initial_device.device_id.clone(),
                self.root.payload.initial_device.clone(),
            )]),
            revocation_cutoffs: BTreeMap::new(),
            admission_epochs: BTreeMap::from([(
                self.root.payload.initial_device.device_id.clone(),
                self.root.payload.membership_epoch,
            )]),
            revocation_epochs: BTreeMap::new(),
        };

        for change in &self.changes {
            validate_change_header(change, &verified)?;
            verify_change_signature(change, &verified)?;
            apply_action(
                &mut verified,
                &change.payload.actor,
                &change.payload.action,
                change.payload.membership_epoch,
            )?;
            verified.epoch = change.payload.membership_epoch;
            verified.head_hash = change.hash()?;
        }
        Ok(verified)
    }

    pub fn append_device_action(
        &mut self,
        anchor: &MembershipAnchor,
        actor: &DeviceId,
        signing_key: &SigningKey,
        action: MembershipAction,
    ) -> Result<VerifiedMembership, VaultError> {
        let verified = self.verify(anchor)?;
        let actor_record = verified
            .devices
            .get(actor)
            .filter(|device| !device.revoked)
            .ok_or(VaultError::RevokedOrUnknownDevice)?;
        let actor_identity = actor_record
            .public_identity
            .as_ref()
            .ok_or(VaultError::InvalidDeviceIdentity)?;
        if actor_identity.ed25519_verifying_key
            != verifying_key_to_base64(&signing_key.verifying_key())
        {
            return Err(VaultError::InvalidSigningKey);
        }
        if matches!(action, MembershipAction::Recover { .. }) {
            return Err(VaultError::InvalidMembership);
        }
        let payload = next_payload(
            &verified,
            MembershipActor::Device {
                device_id: actor.clone(),
            },
            action,
        )?;
        let signature = sign_serializable(CHANGE_SIGNATURE_DOMAIN, signing_key, &payload)?;
        self.append_verified(anchor, SignedMembershipChange { payload, signature })
    }

    pub fn append_recovery(
        &mut self,
        anchor: &MembershipAnchor,
        recovery_signing_key: &SigningKey,
        new_device: DeviceRecord,
        mut revoked_devices: Vec<RecoveryCutoff>,
    ) -> Result<VerifiedMembership, VaultError> {
        let verified = self.verify(anchor)?;
        if verified.recovery_verifying_key
            != verifying_key_to_base64(&recovery_signing_key.verifying_key())
        {
            return Err(VaultError::InvalidSigningKey);
        }
        revoked_devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));
        let payload = next_payload(
            &verified,
            MembershipActor::Recovery,
            MembershipAction::Recover {
                new_device,
                revoked_devices,
            },
        )?;
        let signature = sign_serializable(CHANGE_SIGNATURE_DOMAIN, recovery_signing_key, &payload)?;
        self.append_verified(anchor, SignedMembershipChange { payload, signature })
    }

    fn append_verified(
        &mut self,
        anchor: &MembershipAnchor,
        change: SignedMembershipChange,
    ) -> Result<VerifiedMembership, VaultError> {
        self.changes.push(change);
        match self.verify(anchor) {
            Ok(verified) => Ok(verified),
            Err(error) => {
                self.changes.pop();
                Err(error)
            }
        }
    }
}

impl VerifiedMembership {
    pub fn active_devices(&self) -> impl Iterator<Item = &DeviceRecord> {
        self.devices.values().filter(|device| !device.revoked)
    }

    pub fn device(&self, device_id: &DeviceId) -> Result<&DeviceRecord, VaultError> {
        self.devices
            .get(device_id)
            .ok_or(VaultError::RevokedOrUnknownDevice)
    }

    pub fn accepts_sequence(&self, device_id: &DeviceId, sequence: u64) -> bool {
        match self.devices.get(device_id) {
            Some(device) if !device.revoked => true,
            Some(_) => self
                .revocation_cutoffs
                .get(device_id)
                .is_some_and(|cutoff| sequence <= *cutoff),
            None => false,
        }
    }

    pub fn was_authorized_at_epoch(&self, device_id: &DeviceId, epoch: u64) -> bool {
        self.was_admitted_by_epoch(device_id, epoch)
            && self
                .revocation_epochs
                .get(device_id)
                .is_none_or(|revoked| epoch < *revoked)
    }

    pub fn was_admitted_by_epoch(&self, device_id: &DeviceId, epoch: u64) -> bool {
        self.admission_epochs
            .get(device_id)
            .is_some_and(|admitted| *admitted <= epoch)
    }

    pub fn active_recipients(&self) -> Result<Vec<String>, VaultError> {
        let mut recipients = self
            .active_devices()
            .map(|device| {
                device
                    .public_identity
                    .as_ref()
                    .map(|identity| identity.age_recipient.clone())
                    .ok_or(VaultError::InvalidDeviceIdentity)
            })
            .collect::<Result<Vec<_>, _>>()?;
        recipients.push(self.recovery_recipient.clone());
        recipients.sort();
        recipients.dedup();
        if recipients.len() > super::MAX_DEVICES + 1 {
            return Err(VaultError::TooManyRecipients);
        }
        Ok(recipients)
    }
}

fn next_payload(
    verified: &VerifiedMembership,
    actor: MembershipActor,
    action: MembershipAction,
) -> Result<MembershipChangePayload, VaultError> {
    let membership_epoch = verified
        .epoch
        .checked_add(1)
        .ok_or(VaultError::InvalidMembership)?;
    let payload = MembershipChangePayload {
        kind: CHANGE_KIND.to_owned(),
        schema_version: super::VAULT_SCHEMA_VERSION,
        dataset_id: verified.dataset_id.clone(),
        membership_epoch,
        previous_membership_hash: verified.head_hash.clone(),
        actor,
        action,
    };
    let mut candidate = verified.clone();
    apply_action(
        &mut candidate,
        &payload.actor,
        &payload.action,
        membership_epoch,
    )?;
    Ok(payload)
}

fn validate_root_payload(payload: &MembershipRootPayload) -> Result<(), VaultError> {
    if payload.kind != ROOT_KIND
        || payload.schema_version != super::VAULT_SCHEMA_VERSION
        || payload.membership_epoch != 1
    {
        return Err(VaultError::InvalidMembership);
    }
    validate_dataset_id(&payload.dataset_id)?;
    validate_device(&payload.initial_device)?;
    super::crypto::validate_age_recipient(&payload.recovery_recipient)?;
    super::crypto::verifying_key_from_base64(&payload.recovery_verifying_key)?;
    let initial_identity = payload
        .initial_device
        .public_identity
        .as_ref()
        .ok_or(VaultError::InvalidDeviceIdentity)?;
    if initial_identity.age_recipient == payload.recovery_recipient
        || initial_identity.ed25519_verifying_key == payload.recovery_verifying_key
    {
        return Err(VaultError::InvalidDeviceIdentity);
    }
    Ok(())
}

fn validate_change_header(
    change: &SignedMembershipChange,
    verified: &VerifiedMembership,
) -> Result<(), VaultError> {
    let payload = &change.payload;
    let expected_epoch = verified
        .epoch
        .checked_add(1)
        .ok_or(VaultError::MembershipFork)?;
    if payload.kind != CHANGE_KIND
        || payload.schema_version != super::VAULT_SCHEMA_VERSION
        || payload.dataset_id != verified.dataset_id
        || payload.membership_epoch != expected_epoch
        || payload.previous_membership_hash != verified.head_hash
    {
        return Err(VaultError::MembershipFork);
    }
    Ok(())
}

fn verify_change_signature(
    change: &SignedMembershipChange,
    verified: &VerifiedMembership,
) -> Result<(), VaultError> {
    let key = match &change.payload.actor {
        MembershipActor::Recovery => &verified.recovery_verifying_key,
        MembershipActor::Device { device_id } => verified
            .devices
            .get(device_id)
            .filter(|device| !device.revoked)
            .and_then(|device| device.public_identity.as_ref())
            .map(|identity| &identity.ed25519_verifying_key)
            .ok_or(VaultError::RevokedOrUnknownDevice)?,
    };
    verify_serializable(
        CHANGE_SIGNATURE_DOMAIN,
        key,
        &change.payload,
        &change.signature,
    )
}

fn apply_action(
    verified: &mut VerifiedMembership,
    actor: &MembershipActor,
    action: &MembershipAction,
    action_epoch: u64,
) -> Result<(), VaultError> {
    match (actor, action) {
        (MembershipActor::Recovery, MembershipAction::Recover { .. }) => {}
        (MembershipActor::Recovery, _) | (_, MembershipAction::Recover { .. }) => {
            return Err(VaultError::InvalidMembership);
        }
        (MembershipActor::Device { device_id }, _) => {
            if verified
                .devices
                .get(device_id)
                .is_none_or(|device| device.revoked)
            {
                return Err(VaultError::RevokedOrUnknownDevice);
            }
        }
    }

    match action {
        MembershipAction::AddDevice { device } => {
            validate_device(device)?;
            if verified.devices.contains_key(&device.device_id)
                || verified.devices.len() >= super::MAX_DEVICES
            {
                return Err(VaultError::DeviceAlreadyExists);
            }
            ensure_unique_device_identity(verified, device)?;
            verified
                .devices
                .insert(device.device_id.clone(), device.clone());
            verified
                .admission_epochs
                .insert(device.device_id.clone(), action_epoch);
        }
        MembershipAction::RevokeDevice {
            device_id,
            last_accepted_sequence,
        } => {
            let device = verified
                .devices
                .get_mut(device_id)
                .filter(|device| !device.revoked)
                .ok_or(VaultError::RevokedOrUnknownDevice)?;
            device.revoked = true;
            verified
                .revocation_cutoffs
                .insert(device_id.clone(), *last_accepted_sequence);
            verified
                .revocation_epochs
                .insert(device_id.clone(), action_epoch);
            if !verified.devices.values().any(|device| !device.revoked) {
                return Err(VaultError::LastActiveDevice);
            }
        }
        MembershipAction::Recover {
            new_device,
            revoked_devices,
        } => {
            validate_device(new_device)?;
            if verified.devices.contains_key(&new_device.device_id)
                || verified.devices.len() >= super::MAX_DEVICES
            {
                return Err(VaultError::DeviceAlreadyExists);
            }
            ensure_unique_device_identity(verified, new_device)?;
            let active = verified
                .devices
                .values()
                .filter(|device| !device.revoked)
                .map(|device| device.device_id.clone())
                .collect::<BTreeSet<_>>();
            let provided = revoked_devices
                .iter()
                .map(|cutoff| cutoff.device_id.clone())
                .collect::<BTreeSet<_>>();
            if active != provided || provided.len() != revoked_devices.len() {
                return Err(VaultError::InvalidMembership);
            }
            for cutoff in revoked_devices {
                let device = verified
                    .devices
                    .get_mut(&cutoff.device_id)
                    .ok_or(VaultError::InvalidMembership)?;
                device.revoked = true;
                verified
                    .revocation_cutoffs
                    .insert(cutoff.device_id.clone(), cutoff.last_accepted_sequence);
                verified
                    .revocation_epochs
                    .insert(cutoff.device_id.clone(), action_epoch);
            }
            verified
                .devices
                .insert(new_device.device_id.clone(), new_device.clone());
            verified
                .admission_epochs
                .insert(new_device.device_id.clone(), action_epoch);
        }
    }
    Ok(())
}

fn validate_device(device: &DeviceRecord) -> Result<(), VaultError> {
    if device.revoked
        || super::crypto::validate_device_id(device.device_id.as_str()).is_err()
        || device.name.chars().count() > 1_024
        || device.name.chars().any(char::is_control)
    {
        return Err(VaultError::InvalidDeviceIdentity);
    }
    let DevicePublicIdentity {
        age_recipient,
        ed25519_verifying_key,
    } = device
        .public_identity
        .as_ref()
        .ok_or(VaultError::InvalidDeviceIdentity)?;
    super::crypto::validate_age_recipient(age_recipient)?;
    super::crypto::verifying_key_from_base64(ed25519_verifying_key)?;
    Ok(())
}

fn ensure_unique_device_identity(
    verified: &VerifiedMembership,
    candidate: &DeviceRecord,
) -> Result<(), VaultError> {
    let candidate = candidate
        .public_identity
        .as_ref()
        .ok_or(VaultError::InvalidDeviceIdentity)?;
    if candidate.age_recipient == verified.recovery_recipient
        || candidate.ed25519_verifying_key == verified.recovery_verifying_key
        || verified.devices.values().any(|device| {
            device.public_identity.as_ref().is_some_and(|existing| {
                existing.age_recipient == candidate.age_recipient
                    || existing.ed25519_verifying_key == candidate.ed25519_verifying_key
            })
        })
    {
        Err(VaultError::InvalidDeviceIdentity)
    } else {
        Ok(())
    }
}

fn validate_dataset_id(dataset_id: &str) -> Result<(), VaultError> {
    super::crypto::validate_dataset_id(dataset_id)
}
