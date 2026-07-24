use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use crate::personal_state::{
    DeviceId, Operation, OperationEnvelope, OperationOrigin, PersonalStateV2, VersionVector,
};

use super::batch::BatchAnchor;
use super::crypto::{
    DeviceSecretMaterial, EncryptedObject, base64url_encode, decrypt_json_with_identity,
    encrypt_json_to_recipients, sha256_domain_hex, sign_serializable, verify_serializable,
};
use super::error::VaultError;
use super::membership::{MembershipAnchor, MembershipChain, VerifiedMembership};

const CHECKPOINT_KIND: &str = "yututui_vault_checkpoint";
const CHECKPOINT_SIGNATURE_DOMAIN: &[u8] = b"yututui-vault-checkpoint-signature-v1";
const CHECKPOINT_HASH_DOMAIN: &[u8] = b"yututui-vault-checkpoint-hash-v1";
const CHECKPOINT_BATCH_ANCHOR_DOMAIN: &[u8] = b"yututui-vault-checkpoint-batch-anchor-v1";
const CHECKPOINT_RETAINED_OPERATIONS_DOMAIN: &[u8] =
    b"yututui-vault-checkpoint-retained-operations-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointBatchAnchor {
    pub last_sequence: u64,
    pub last_batch_hash: String,
}

/// A signed full-state checkpoint. Its membership chain makes the exact recipient epoch
/// independently verifiable after every device has been lost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointPayload {
    pub kind: String,
    pub schema_version: u32,
    pub dataset_id: String,
    pub checkpoint_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_checkpoint_hash: Option<String>,
    pub membership_epoch: u64,
    pub signer_device_id: DeviceId,
    pub membership: MembershipChain,
    pub batch_anchors: BTreeMap<DeviceId, CheckpointBatchAnchor>,
    pub state: PersonalStateV2,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedCheckpoint {
    pub payload: CheckpointPayload,
    pub signature: String,
}

/// Locally durable rollback anchor for the last accepted checkpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointAnchor {
    pub checkpoint_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointAcceptance {
    Applied,
    Duplicate,
}

impl CheckpointAnchor {
    pub fn from_trusted(
        checkpoint_sequence: u64,
        checkpoint_hash: String,
    ) -> Result<Self, VaultError> {
        if checkpoint_sequence == 0 || !valid_hash(&checkpoint_hash) {
            return Err(VaultError::InvalidEncryptedObject);
        }
        Ok(Self {
            checkpoint_sequence,
            checkpoint_hash: Some(checkpoint_hash),
        })
    }
}

impl SignedCheckpoint {
    pub fn create(
        membership: MembershipChain,
        membership_anchor: &MembershipAnchor,
        signer_device_id: DeviceId,
        signing_key: &SigningKey,
        checkpoint_anchor: &CheckpointAnchor,
        state: PersonalStateV2,
    ) -> Result<Self, VaultError> {
        if (checkpoint_anchor.checkpoint_sequence == 0)
            != checkpoint_anchor.checkpoint_hash.is_none()
            || checkpoint_anchor
                .checkpoint_hash
                .as_deref()
                .is_some_and(|hash| !valid_hash(hash))
        {
            return Err(VaultError::InvalidEncryptedObject);
        }
        let verified = membership.verify(membership_anchor)?;
        let signer = active_signer(&verified, &signer_device_id)?;
        if signer.ed25519_verifying_key != base64url_encode(signing_key.verifying_key().as_bytes())
        {
            return Err(VaultError::InvalidSigningKey);
        }
        let batch_anchors = derive_batch_anchors(&state, &verified)?;
        let payload = CheckpointPayload {
            kind: CHECKPOINT_KIND.to_owned(),
            schema_version: super::VAULT_SCHEMA_VERSION,
            dataset_id: verified.dataset_id.clone(),
            checkpoint_sequence: checkpoint_anchor
                .checkpoint_sequence
                .checked_add(1)
                .ok_or(VaultError::InvalidEncryptedObject)?,
            previous_checkpoint_hash: checkpoint_anchor.checkpoint_hash.clone(),
            membership_epoch: verified.epoch,
            signer_device_id,
            membership,
            batch_anchors,
            state,
        };
        validate_payload(&payload, &verified)?;
        let signature = sign_serializable(CHECKPOINT_SIGNATURE_DOMAIN, signing_key, &payload)?;
        let checkpoint = Self { payload, signature };
        checkpoint.verify(membership_anchor)?;
        Ok(checkpoint)
    }

    /// Verifies the membership chain, complete state, registry, epoch, and active signer.
    pub fn verify(
        &self,
        membership_anchor: &MembershipAnchor,
    ) -> Result<VerifiedMembership, VaultError> {
        let verified = self.payload.membership.verify(membership_anchor)?;
        validate_payload(&self.payload, &verified)?;
        let signer = active_signer(&verified, &self.payload.signer_device_id)?;
        verify_serializable(
            CHECKPOINT_SIGNATURE_DOMAIN,
            &signer.ed25519_verifying_key,
            &self.payload,
            &self.signature,
        )?;
        Ok(verified)
    }

    pub fn hash(&self) -> Result<String, VaultError> {
        let bytes = serde_json::to_vec(self).map_err(|_| VaultError::SerializationFailed)?;
        Ok(sha256_domain_hex(CHECKPOINT_HASH_DOMAIN, &[&bytes]))
    }

    /// Starts the device's next immutable segment from this signed checkpoint coverage.
    pub fn next_batch_anchor(
        &self,
        membership_anchor: &MembershipAnchor,
        device_id: &DeviceId,
    ) -> Result<BatchAnchor, VaultError> {
        let membership = self.verify(membership_anchor)?;
        membership
            .devices
            .get(device_id)
            .filter(|device| !device.revoked)
            .ok_or(VaultError::RevokedOrUnknownDevice)?;
        let last_sequence = self.payload.state.version_vector.observed(device_id);
        if last_sequence == 0 {
            return Ok(BatchAnchor::empty(device_id.clone()));
        }
        let anchor = self
            .payload
            .batch_anchors
            .get(device_id)
            .ok_or(VaultError::InvalidEncryptedObject)?;
        BatchAnchor::from_checkpoint(
            device_id.clone(),
            anchor.last_sequence,
            Some(anchor.last_batch_hash.clone()),
        )
    }

    /// Encrypts only to devices active in the embedded epoch and the offline recovery key.
    pub fn encrypt(
        &self,
        membership_anchor: &MembershipAnchor,
    ) -> Result<EncryptedObject, VaultError> {
        let verified = self.verify(membership_anchor)?;
        encrypt_json_to_recipients(self, &verified.active_recipients()?)
    }

    /// Authenticates the complete age stream before parsing and verifying its signed payload.
    pub fn decrypt_for_device(
        object: &EncryptedObject,
        device: &DeviceSecretMaterial,
        membership_anchor: &MembershipAnchor,
    ) -> Result<Self, VaultError> {
        let checkpoint = Self::decrypt(object, device.age_identity(), membership_anchor)?;
        let verified = checkpoint.verify(membership_anchor)?;
        let device_id =
            DeviceId::new(device.device_id()).map_err(|_| VaultError::InvalidDeviceIdentity)?;
        let record = verified
            .devices
            .get(&device_id)
            .filter(|record| !record.revoked)
            .ok_or(VaultError::RevokedOrUnknownDevice)?;
        if !device.matches_personal_record(record) {
            return Err(VaultError::InvalidDeviceIdentity);
        }
        Ok(checkpoint)
    }

    pub(crate) fn decrypt(
        object: &EncryptedObject,
        identity: &age::x25519::Identity,
        membership_anchor: &MembershipAnchor,
    ) -> Result<Self, VaultError> {
        let checkpoint: Self = decrypt_json_with_identity(object, identity)?;
        checkpoint.verify(membership_anchor)?;
        Ok(checkpoint)
    }

    /// Advances a retained checkpoint anchor after all cryptographic checks succeed.
    pub fn accept(
        &self,
        membership_anchor: &MembershipAnchor,
        anchor: &mut CheckpointAnchor,
    ) -> Result<CheckpointAcceptance, VaultError> {
        self.verify(membership_anchor)?;
        let hash = self.hash()?;
        let result = classify_anchor(anchor, &self.payload, &hash)?;
        if result == CheckpointAcceptance::Applied {
            anchor.checkpoint_sequence = self.payload.checkpoint_sequence;
            anchor.checkpoint_hash = Some(hash);
        }
        Ok(result)
    }
}

fn classify_anchor(
    anchor: &CheckpointAnchor,
    payload: &CheckpointPayload,
    hash: &str,
) -> Result<CheckpointAcceptance, VaultError> {
    if anchor.checkpoint_sequence == 0 {
        if anchor.checkpoint_hash.is_some() {
            return Err(VaultError::InvalidEncryptedObject);
        }
        // A new or fully recovered device may first see a later checkpoint. Its membership
        // root/recovery anchor authenticates that first high-water mark.
        return Ok(CheckpointAcceptance::Applied);
    }

    if payload.checkpoint_sequence < anchor.checkpoint_sequence {
        return Err(VaultError::RollbackDetected);
    }
    if payload.checkpoint_sequence == anchor.checkpoint_sequence {
        return if anchor.checkpoint_hash.as_deref() == Some(hash) {
            Ok(CheckpointAcceptance::Duplicate)
        } else {
            Err(VaultError::RollbackDetected)
        };
    }
    if payload.checkpoint_sequence != anchor.checkpoint_sequence.saturating_add(1) {
        return Err(VaultError::SequenceGap);
    }
    if payload.previous_checkpoint_hash != anchor.checkpoint_hash {
        return Err(VaultError::RollbackDetected);
    }
    Ok(CheckpointAcceptance::Applied)
}

fn validate_payload(
    payload: &CheckpointPayload,
    membership: &VerifiedMembership,
) -> Result<(), VaultError> {
    if payload.kind != CHECKPOINT_KIND
        || payload.schema_version != super::VAULT_SCHEMA_VERSION
        || payload.dataset_id != membership.dataset_id
        || payload.membership_epoch != membership.epoch
        || payload.checkpoint_sequence == 0
        || (payload.checkpoint_sequence == 1) != payload.previous_checkpoint_hash.is_none()
        || payload
            .previous_checkpoint_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
        || payload.state.dataset_id != payload.dataset_id
    {
        return Err(VaultError::InvalidEncryptedObject);
    }
    payload
        .state
        .validate()
        .map_err(|_| VaultError::InvalidEncryptedObject)?;
    if payload
        .state
        .compaction_checkpoint
        .as_ref()
        .is_some_and(|checkpoint| !checkpoint.acknowledged_by.is_empty())
    {
        return Err(VaultError::InvalidEncryptedObject);
    }
    if payload.state.device_registry != membership.devices {
        return Err(VaultError::RegistryMismatch);
    }
    let has_legacy_baseline = payload
        .state
        .operations
        .iter()
        .any(is_deterministic_legacy_baseline);
    if payload.state.operations.iter().any(|operation| {
        (!membership.accepts_sequence(&operation.stamp.dot.device_id, operation.stamp.dot.sequence)
            && !is_deterministic_legacy_baseline(operation))
            || !vector_is_authorized(&operation.stamp.observed, membership, has_legacy_baseline)
    }) || !vector_is_authorized(
        &payload.state.version_vector,
        membership,
        has_legacy_baseline,
    ) || payload
        .state
        .compaction_checkpoint
        .as_ref()
        .is_some_and(|checkpoint| {
            !vector_is_authorized(&checkpoint.coverage, membership, has_legacy_baseline)
        })
    {
        return Err(VaultError::RevokedOrUnknownDevice);
    }
    // The cutoff is the exact durable high-water the revoker attested. Equality also covers
    // operations represented by a compaction vector but no longer retained as raw entries.
    if membership
        .revocation_cutoffs
        .iter()
        .any(|(device_id, cutoff)| payload.state.version_vector.observed(device_id) != *cutoff)
    {
        return Err(VaultError::InvalidMembership);
    }
    if payload.batch_anchors != derive_batch_anchors(&payload.state, membership)? {
        return Err(VaultError::InvalidEncryptedObject);
    }
    active_signer(membership, &payload.signer_device_id)?;
    Ok(())
}

fn derive_batch_anchors(
    state: &PersonalStateV2,
    membership: &VerifiedMembership,
) -> Result<BTreeMap<DeviceId, CheckpointBatchAnchor>, VaultError> {
    let has_legacy_baseline = state_has_legacy_baseline(state);
    let mut anchors = BTreeMap::new();
    for (device_id, last_sequence) in &state.version_vector.0 {
        if has_legacy_baseline && device_id.as_str() == "legacy" && *last_sequence == 1 {
            continue;
        }
        if !membership.accepts_sequence(device_id, *last_sequence) {
            return Err(VaultError::RevokedOrUnknownDevice);
        }
        let operations_hash = retained_operations_hash(state, device_id)?;
        let epoch = membership.epoch.to_be_bytes();
        let sequence = last_sequence.to_be_bytes();
        let last_batch_hash = sha256_domain_hex(
            CHECKPOINT_BATCH_ANCHOR_DOMAIN,
            &[
                state.dataset_id.as_bytes(),
                membership.head_hash.as_bytes(),
                &epoch,
                device_id.as_str().as_bytes(),
                &sequence,
                operations_hash.as_bytes(),
            ],
        );
        anchors.insert(
            device_id.clone(),
            CheckpointBatchAnchor {
                last_sequence: *last_sequence,
                last_batch_hash,
            },
        );
    }
    Ok(anchors)
}

fn retained_operations_hash(
    state: &PersonalStateV2,
    device_id: &DeviceId,
) -> Result<String, VaultError> {
    let mut operations = state
        .operations
        .iter()
        .filter(|operation| &operation.stamp.dot.device_id == device_id)
        .collect::<Vec<_>>();
    operations.sort_by(|left, right| {
        left.stamp
            .dot
            .cmp(&right.stamp.dot)
            .then(left.operation_id.cmp(&right.operation_id))
    });
    let bytes = serde_json::to_vec(&operations).map_err(|_| VaultError::SerializationFailed)?;
    Ok(sha256_domain_hex(
        CHECKPOINT_RETAINED_OPERATIONS_DOMAIN,
        &[&bytes],
    ))
}

fn vector_is_authorized(
    vector: &VersionVector,
    membership: &VerifiedMembership,
    allow_legacy_coverage: bool,
) -> bool {
    vector.0.iter().all(|(device_id, sequence)| {
        membership.accepts_sequence(device_id, *sequence)
            || (allow_legacy_coverage && device_id.as_str() == "legacy" && *sequence == 1)
    })
}

fn state_has_legacy_baseline(state: &PersonalStateV2) -> bool {
    state
        .operations
        .iter()
        .any(is_deterministic_legacy_baseline)
}

fn is_deterministic_legacy_baseline(envelope: &OperationEnvelope) -> bool {
    let Some(hash) = envelope.operation_id.strip_prefix("legacy-") else {
        return false;
    };
    envelope.stamp.dot.device_id.as_str() == "legacy"
        && envelope.stamp.dot.sequence == 1
        && envelope.stamp.observed.0.is_empty()
        && envelope.stamp.recorded_at_unix == 0
        && envelope.origin == OperationOrigin::Imported
        && matches!(&envelope.operation, Operation::LegacyBaseline { .. })
        && valid_hash(hash)
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

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::personal_state::{
        CausalStamp, CompactionCheckpoint, DeviceRecord, Dot, LegacyProjection, Operation,
        OperationEnvelope, OperationOrigin, VersionVector, refresh_device_registry,
    };

    use super::super::batch::SignedOperationBatch;
    use super::super::membership::{MembershipAction, SignedMembershipRoot};
    use super::*;

    struct Fixture {
        chain: MembershipChain,
        anchor: MembershipAnchor,
        device_one_secrets: DeviceSecretMaterial,
        device_one: DeviceRecord,
        recovery_age: age::x25519::Identity,
        state: PersonalStateV2,
    }

    fn device(id: &str) -> (DeviceRecord, DeviceSecretMaterial) {
        let secrets = DeviceSecretMaterial::generate_for(id).unwrap();
        let record = DeviceRecord {
            device_id: DeviceId::new(id).unwrap(),
            name: id.to_owned(),
            revoked: false,
            public_identity: Some(secrets.public_identity()),
        };
        (record, secrets)
    }

    fn envelope(
        id: &str,
        author: &DeviceId,
        sequence: u64,
        observed: VersionVector,
        operation: Operation,
    ) -> OperationEnvelope {
        OperationEnvelope {
            operation_id: id.to_owned(),
            stamp: CausalStamp {
                dot: Dot {
                    device_id: author.clone(),
                    sequence,
                },
                observed,
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation,
        }
    }

    fn state_with(operations: Vec<OperationEnvelope>) -> PersonalStateV2 {
        let mut state = PersonalStateV2::empty("dataset-checkpoint".to_owned()).unwrap();
        for operation in &operations {
            state.version_vector.merge(&operation.stamp.observed);
            state.version_vector.observe(&operation.stamp.dot);
        }
        state.operations = operations;
        refresh_device_registry(&mut state).unwrap();
        state.normalize().unwrap();
        state
    }

    fn fixture() -> Fixture {
        let (device_one, device_one_secrets) = device("device-one");
        let recovery_age = age::x25519::Identity::generate();
        let recovery_key = SigningKey::from_bytes(&[13_u8; 32]);
        let root = SignedMembershipRoot::create(
            "dataset-checkpoint",
            recovery_age.to_public().to_string(),
            &recovery_key,
            device_one.clone(),
        )
        .unwrap();
        let anchor = MembershipAnchor::RootHash(root.hash().unwrap());
        let chain = MembershipChain::new(root);
        let state = state_with(vec![envelope(
            "add-device-one",
            &device_one.device_id,
            1,
            VersionVector::default(),
            Operation::AddDevice {
                device: device_one.clone(),
            },
        )]);
        Fixture {
            chain,
            anchor,
            device_one_secrets,
            device_one,
            recovery_age,
            state,
        }
    }

    #[test]
    fn encrypted_checkpoint_round_trips_and_detects_rollback() {
        let fixture = fixture();
        let first = SignedCheckpoint::create(
            fixture.chain.clone(),
            &fixture.anchor,
            fixture.device_one.device_id.clone(),
            fixture.device_one_secrets.signing_key(),
            &CheckpointAnchor::default(),
            fixture.state.clone(),
        )
        .unwrap();
        let next_batch_anchor = first
            .next_batch_anchor(&fixture.anchor, &fixture.device_one.device_id)
            .unwrap();
        assert_eq!(next_batch_anchor.last_sequence, 1);
        let membership = fixture.chain.verify(&fixture.anchor).unwrap();
        let next_operation = envelope(
            "after-checkpoint",
            &fixture.device_one.device_id,
            2,
            VersionVector(BTreeMap::from([(fixture.device_one.device_id.clone(), 1)])),
            Operation::SetAvoidArtist {
                artist_key: "artist".to_owned(),
                avoid: true,
            },
        );
        let next_batch = SignedOperationBatch::create(
            &membership,
            &fixture.state,
            fixture.device_one.device_id.clone(),
            fixture.device_one_secrets.signing_key(),
            &next_batch_anchor,
            vec![next_operation],
        )
        .unwrap();
        assert_eq!(next_batch.payload.first_sequence, 2);
        assert_eq!(
            next_batch.payload.previous_batch_hash,
            next_batch_anchor.last_batch_hash
        );

        let mut tampered_anchor = first.clone();
        tampered_anchor
            .payload
            .batch_anchors
            .get_mut(&fixture.device_one.device_id)
            .unwrap()
            .last_batch_hash = "f".repeat(64);
        assert!(tampered_anchor.verify(&fixture.anchor).is_err());

        let encrypted = first.encrypt(&fixture.anchor).unwrap();
        let opened = SignedCheckpoint::decrypt_for_device(
            &encrypted,
            &fixture.device_one_secrets,
            &fixture.anchor,
        )
        .unwrap();
        assert_eq!(opened, first);
        assert!(
            SignedCheckpoint::decrypt(&encrypted, &fixture.recovery_age, &fixture.anchor).is_ok()
        );

        let mut high_water = CheckpointAnchor::default();
        assert_eq!(
            first.accept(&fixture.anchor, &mut high_water).unwrap(),
            CheckpointAcceptance::Applied
        );
        assert_eq!(
            first.accept(&fixture.anchor, &mut high_water).unwrap(),
            CheckpointAcceptance::Duplicate
        );
        let second = SignedCheckpoint::create(
            fixture.chain,
            &fixture.anchor,
            fixture.device_one.device_id,
            fixture.device_one_secrets.signing_key(),
            &high_water,
            fixture.state,
        )
        .unwrap();
        assert_eq!(
            second.accept(&fixture.anchor, &mut high_water).unwrap(),
            CheckpointAcceptance::Applied
        );
        assert_eq!(
            first.accept(&fixture.anchor, &mut high_water),
            Err(VaultError::RollbackDetected)
        );
    }

    #[test]
    fn revoked_device_is_excluded_from_rotated_checkpoint() {
        let mut fixture = fixture();
        let (device_two, device_two_secrets) = device("device-two");
        fixture
            .chain
            .append_device_action(
                &fixture.anchor,
                &fixture.device_one.device_id,
                fixture.device_one_secrets.signing_key(),
                MembershipAction::AddDevice {
                    device: device_two.clone(),
                },
            )
            .unwrap();
        let mut too_low_cutoff = fixture.chain.clone();
        too_low_cutoff
            .append_device_action(
                &fixture.anchor,
                &device_two.device_id,
                device_two_secrets.signing_key(),
                MembershipAction::RevokeDevice {
                    device_id: fixture.device_one.device_id.clone(),
                    last_accepted_sequence: 1,
                },
            )
            .unwrap();
        let mut too_high_cutoff = fixture.chain.clone();
        too_high_cutoff
            .append_device_action(
                &fixture.anchor,
                &device_two.device_id,
                device_two_secrets.signing_key(),
                MembershipAction::RevokeDevice {
                    device_id: fixture.device_one.device_id.clone(),
                    last_accepted_sequence: 3,
                },
            )
            .unwrap();
        fixture
            .chain
            .append_device_action(
                &fixture.anchor,
                &device_two.device_id,
                device_two_secrets.signing_key(),
                MembershipAction::RevokeDevice {
                    device_id: fixture.device_one.device_id.clone(),
                    last_accepted_sequence: 2,
                },
            )
            .unwrap();

        let observed_one =
            VersionVector(BTreeMap::from([(fixture.device_one.device_id.clone(), 1)]));
        let observed_two =
            VersionVector(BTreeMap::from([(fixture.device_one.device_id.clone(), 2)]));
        let state = state_with(vec![
            envelope(
                "add-device-one",
                &fixture.device_one.device_id,
                1,
                VersionVector::default(),
                Operation::AddDevice {
                    device: fixture.device_one.clone(),
                },
            ),
            envelope(
                "add-device-two",
                &fixture.device_one.device_id,
                2,
                observed_one,
                Operation::AddDevice {
                    device: device_two.clone(),
                },
            ),
            envelope(
                "revoke-device-one",
                &device_two.device_id,
                1,
                observed_two,
                Operation::RevokeDevice {
                    device_id: fixture.device_one.device_id.clone(),
                },
            ),
        ]);
        assert_eq!(
            SignedCheckpoint::create(
                too_low_cutoff,
                &fixture.anchor,
                device_two.device_id.clone(),
                device_two_secrets.signing_key(),
                &CheckpointAnchor::default(),
                state.clone(),
            ),
            Err(VaultError::RevokedOrUnknownDevice)
        );
        assert_eq!(
            SignedCheckpoint::create(
                too_high_cutoff,
                &fixture.anchor,
                device_two.device_id.clone(),
                device_two_secrets.signing_key(),
                &CheckpointAnchor::default(),
                state.clone(),
            ),
            Err(VaultError::InvalidMembership)
        );
        let checkpoint = SignedCheckpoint::create(
            fixture.chain,
            &fixture.anchor,
            device_two.device_id,
            device_two_secrets.signing_key(),
            &CheckpointAnchor::default(),
            state,
        )
        .unwrap();
        let encrypted = checkpoint.encrypt(&fixture.anchor).unwrap();
        assert_eq!(
            SignedCheckpoint::decrypt_for_device(
                &encrypted,
                &fixture.device_one_secrets,
                &fixture.anchor
            ),
            Err(VaultError::DecryptionFailed)
        );
        assert!(
            SignedCheckpoint::decrypt_for_device(&encrypted, &device_two_secrets, &fixture.anchor)
                .is_ok()
        );
    }

    #[test]
    fn registry_injection_is_rejected_before_signing() {
        let mut fixture = fixture();
        fixture
            .state
            .device_registry
            .get_mut(&fixture.device_one.device_id)
            .unwrap()
            .name = "injected name".to_owned();
        assert_eq!(
            SignedCheckpoint::create(
                fixture.chain,
                &fixture.anchor,
                fixture.device_one.device_id,
                fixture.device_one_secrets.signing_key(),
                &CheckpointAnchor::default(),
                fixture.state,
            ),
            Err(VaultError::InvalidEncryptedObject)
        );
    }

    #[test]
    fn unknown_version_vector_device_is_rejected() {
        let mut fixture = fixture();
        fixture
            .state
            .version_vector
            .0
            .insert(DeviceId::new("unknown-device").unwrap(), 1);
        assert_eq!(
            SignedCheckpoint::create(
                fixture.chain,
                &fixture.anchor,
                fixture.device_one.device_id,
                fixture.device_one_secrets.signing_key(),
                &CheckpointAnchor::default(),
                fixture.state,
            ),
            Err(VaultError::RevokedOrUnknownDevice)
        );
    }

    #[test]
    fn unsigned_compaction_acknowledgements_are_rejected() {
        let mut fixture = fixture();
        fixture.state.compaction_checkpoint = Some(CompactionCheckpoint {
            checkpoint_id: "compaction-one".to_owned(),
            coverage: VersionVector::default(),
            previous_checkpoint_hash: None,
            acknowledged_by: BTreeSet::from([fixture.device_one.device_id.clone()]),
        });
        assert_eq!(
            SignedCheckpoint::create(
                fixture.chain,
                &fixture.anchor,
                fixture.device_one.device_id,
                fixture.device_one_secrets.signing_key(),
                &CheckpointAnchor::default(),
                fixture.state,
            ),
            Err(VaultError::InvalidEncryptedObject)
        );
    }

    #[test]
    fn only_the_deterministic_legacy_baseline_bypasses_membership() {
        let mut baseline = OperationEnvelope {
            operation_id: format!("legacy-{}", "0".repeat(64)),
            stamp: CausalStamp {
                dot: Dot {
                    device_id: DeviceId::new("legacy").unwrap(),
                    sequence: 1,
                },
                observed: VersionVector::default(),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Imported,
            operation: Operation::LegacyBaseline {
                baseline: Box::<LegacyProjection>::default(),
            },
        };
        assert!(is_deterministic_legacy_baseline(&baseline));

        baseline.origin = OperationOrigin::Local;
        assert!(!is_deterministic_legacy_baseline(&baseline));
        baseline.origin = OperationOrigin::Imported;
        baseline.stamp.recorded_at_unix = 1;
        assert!(!is_deterministic_legacy_baseline(&baseline));
        baseline.stamp.recorded_at_unix = 0;
        baseline.operation_id = "legacy-not-a-hash".to_owned();
        assert!(!is_deterministic_legacy_baseline(&baseline));
    }
}
