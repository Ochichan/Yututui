use std::collections::{BTreeMap, BTreeSet};

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use crate::personal_state::{
    DeviceId, Operation, OperationEnvelope, OperationOrigin, PersonalStateV2, VersionVector,
    refresh_device_registry,
};

use super::crypto::{
    DeviceSecretMaterial, EncryptedObject, base64url_encode, decrypt_json_with_identity,
    encrypt_json_to_recipients, sha256_domain_hex, sign_serializable, verify_serializable,
};
use super::error::VaultError;
use super::membership::VerifiedMembership;

const BATCH_KIND: &str = "yututui_vault_operation_batch";
const BATCH_SIGNATURE_DOMAIN: &[u8] = b"yututui-vault-operation-batch-signature-v1";
const BATCH_HASH_DOMAIN: &[u8] = b"yututui-vault-operation-batch-hash-v1";

/// One immutable, contiguous segment authored by exactly one approved device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationBatchPayload {
    pub kind: String,
    pub schema_version: u32,
    pub dataset_id: String,
    pub membership_epoch: u64,
    pub signer_device_id: DeviceId,
    pub first_sequence: u64,
    pub last_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_batch_hash: Option<String>,
    pub operations: Vec<OperationEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedOperationBatch {
    pub payload: OperationBatchPayload,
    pub signature: String,
}

/// Durable high-water state for one device's immutable segment stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchAnchor {
    pub signer_device_id: DeviceId,
    pub last_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_batch_first_sequence: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_batch_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchAcceptance {
    Applied,
    Duplicate,
}

impl BatchAnchor {
    pub fn empty(signer_device_id: DeviceId) -> Self {
        Self {
            signer_device_id,
            last_sequence: 0,
            last_batch_first_sequence: None,
            last_batch_hash: None,
        }
    }

    /// Restores a trusted anchor retained alongside a checkpoint.
    pub fn from_checkpoint(
        signer_device_id: DeviceId,
        last_sequence: u64,
        last_batch_hash: Option<String>,
    ) -> Result<Self, VaultError> {
        if (last_sequence == 0) != last_batch_hash.is_none()
            || last_batch_hash
                .as_deref()
                .is_some_and(|hash| !valid_hash(hash))
        {
            return Err(VaultError::InvalidEncryptedObject);
        }
        Ok(Self {
            signer_device_id,
            last_sequence,
            // A checkpoint does not need to retain the first sequence of the last segment.
            last_batch_first_sequence: None,
            last_batch_hash,
        })
    }
}

impl SignedOperationBatch {
    pub fn create(
        membership: &VerifiedMembership,
        current_state: &PersonalStateV2,
        signer_device_id: DeviceId,
        signing_key: &SigningKey,
        anchor: &BatchAnchor,
        operations: Vec<OperationEnvelope>,
    ) -> Result<Self, VaultError> {
        if current_state.dataset_id != membership.dataset_id {
            return Err(VaultError::InvalidEncryptedObject);
        }
        validate_state_causal_authorization(current_state, membership)?;
        if anchor.signer_device_id != signer_device_id {
            return Err(VaultError::InvalidEncryptedObject);
        }
        if current_state.version_vector.observed(&signer_device_id) != anchor.last_sequence {
            return Err(VaultError::RevisionConflict);
        }
        let signer = active_signer(membership, &signer_device_id)?;
        if signer.ed25519_verifying_key != base64url_encode(signing_key.verifying_key().as_bytes())
        {
            return Err(VaultError::InvalidSigningKey);
        }
        let first_sequence = operations
            .first()
            .map(|operation| operation.stamp.dot.sequence)
            .ok_or(VaultError::InvalidEncryptedObject)?;
        let last_sequence = operations
            .last()
            .map(|operation| operation.stamp.dot.sequence)
            .ok_or(VaultError::InvalidEncryptedObject)?;
        let payload = OperationBatchPayload {
            kind: BATCH_KIND.to_owned(),
            schema_version: super::VAULT_SCHEMA_VERSION,
            dataset_id: membership.dataset_id.clone(),
            membership_epoch: membership.epoch,
            signer_device_id,
            first_sequence,
            last_sequence,
            previous_batch_hash: anchor.last_batch_hash.clone(),
            operations,
        };
        validate_payload(
            &payload,
            membership,
            true,
            state_has_legacy_baseline(current_state),
            Some(&current_state.version_vector),
        )?;
        if payload.first_sequence != anchor.last_sequence.saturating_add(1) {
            return Err(VaultError::SequenceGap);
        }
        let signature = sign_serializable(BATCH_SIGNATURE_DOMAIN, signing_key, &payload)?;
        let batch = Self { payload, signature };
        batch.verify(membership)?;
        Ok(batch)
    }

    /// Verifies the signature and all immutable segment invariants.
    ///
    /// Batches produced before revocation remain valid only through the sequence cutoff
    /// recorded by the revocation membership operation.
    pub fn verify(&self, membership: &VerifiedMembership) -> Result<(), VaultError> {
        // Exact legacy coverage is structurally valid here; apply-time validation requires the
        // referenced state to contain the deterministic baseline before accepting it.
        validate_payload(&self.payload, membership, false, true, None)?;
        let signer = membership.device(&self.payload.signer_device_id)?;
        let identity = signer
            .public_identity
            .as_ref()
            .ok_or(VaultError::InvalidDeviceIdentity)?;
        verify_serializable(
            BATCH_SIGNATURE_DOMAIN,
            &identity.ed25519_verifying_key,
            &self.payload,
            &self.signature,
        )?;
        validate_serialized_size(self)
    }

    pub fn hash(&self) -> Result<String, VaultError> {
        let bytes = serde_json::to_vec(self).map_err(|_| VaultError::SerializationFailed)?;
        Ok(sha256_domain_hex(BATCH_HASH_DOMAIN, &[&bytes]))
    }

    pub fn encrypt(&self, membership: &VerifiedMembership) -> Result<EncryptedObject, VaultError> {
        self.verify(membership)?;
        encrypt_json_to_recipients(self, &membership.active_recipients()?)
    }

    pub fn decrypt_for_device(
        object: &EncryptedObject,
        device: &DeviceSecretMaterial,
        membership: &VerifiedMembership,
    ) -> Result<Self, VaultError> {
        let batch = Self::decrypt(object, device.age_identity(), membership)?;
        let device_id =
            DeviceId::new(device.device_id()).map_err(|_| VaultError::InvalidDeviceIdentity)?;
        let record = membership
            .devices
            .get(&device_id)
            .filter(|record| !record.revoked)
            .ok_or(VaultError::RevokedOrUnknownDevice)?;
        if !device.matches_personal_record(record) {
            return Err(VaultError::InvalidDeviceIdentity);
        }
        Ok(batch)
    }

    pub(crate) fn decrypt(
        object: &EncryptedObject,
        identity: &age::x25519::Identity,
        membership: &VerifiedMembership,
    ) -> Result<Self, VaultError> {
        let batch: Self = decrypt_json_with_identity(object, identity)?;
        batch.verify(membership)?;
        Ok(batch)
    }
}

/// Verifies, de-duplicates, and merges one batch before advancing its durable anchor.
///
/// The state and anchor are only replaced after the entire candidate validates, so callers
/// never observe a partially applied segment.
pub fn apply_operation_batch(
    state: &mut PersonalStateV2,
    anchor: &mut BatchAnchor,
    membership: &VerifiedMembership,
    batch: &SignedOperationBatch,
) -> Result<BatchAcceptance, VaultError> {
    batch.verify(membership)?;
    if state.dataset_id != membership.dataset_id
        || batch.payload.dataset_id != state.dataset_id
        || anchor.signer_device_id != batch.payload.signer_device_id
    {
        return Err(VaultError::InvalidEncryptedObject);
    }
    if state
        .version_vector
        .observed(&batch.payload.signer_device_id)
        != anchor.last_sequence
    {
        return Err(VaultError::RevisionConflict);
    }
    validate_state_causal_authorization(state, membership)?;
    validate_payload(
        &batch.payload,
        membership,
        false,
        state_has_legacy_baseline(state),
        Some(&state.version_vector),
    )?;

    let batch_hash = batch.hash()?;
    match classify_anchor(anchor, batch, &batch_hash)? {
        BatchAcceptance::Duplicate => return Ok(BatchAcceptance::Duplicate),
        BatchAcceptance::Applied => {}
    }

    let mut candidate = state.clone();
    merge_operations(&mut candidate, &batch.payload.operations)?;
    candidate.projection_fingerprint = None;
    candidate
        .normalize()
        .map_err(|_| VaultError::InvalidEncryptedObject)?;
    validate_state_causal_authorization(&candidate, membership)?;
    if candidate.device_registry != membership.devices {
        return Err(VaultError::RegistryMismatch);
    }

    anchor.last_sequence = batch.payload.last_sequence;
    anchor.last_batch_first_sequence = Some(batch.payload.first_sequence);
    anchor.last_batch_hash = Some(batch_hash);
    *state = candidate;
    Ok(BatchAcceptance::Applied)
}

fn classify_anchor(
    anchor: &BatchAnchor,
    batch: &SignedOperationBatch,
    batch_hash: &str,
) -> Result<BatchAcceptance, VaultError> {
    let payload = &batch.payload;
    if payload.first_sequence <= anchor.last_sequence {
        if payload.last_sequence == anchor.last_sequence
            && anchor.last_batch_first_sequence == Some(payload.first_sequence)
            && anchor.last_batch_hash.as_deref() == Some(batch_hash)
        {
            return Ok(BatchAcceptance::Duplicate);
        }
        return Err(VaultError::ReplayDetected);
    }
    if payload.first_sequence != anchor.last_sequence.saturating_add(1) {
        return Err(VaultError::SequenceGap);
    }
    if payload.previous_batch_hash != anchor.last_batch_hash {
        return Err(VaultError::RollbackDetected);
    }
    Ok(BatchAcceptance::Applied)
}

fn merge_operations(
    state: &mut PersonalStateV2,
    operations: &[OperationEnvelope],
) -> Result<(), VaultError> {
    let mut by_id = state
        .operations
        .iter()
        .map(|operation| (&operation.operation_id, operation))
        .collect::<BTreeMap<_, _>>();
    let mut by_dot = state
        .operations
        .iter()
        .map(|operation| (&operation.stamp.dot, operation))
        .collect::<BTreeMap<_, _>>();

    let mut additions = Vec::new();
    for operation in operations {
        match by_id.get(&operation.operation_id) {
            Some(existing) if *existing == operation => continue,
            Some(_) => return Err(VaultError::InvalidEncryptedObject),
            None => {}
        }
        if by_dot.contains_key(&operation.stamp.dot) {
            return Err(VaultError::InvalidEncryptedObject);
        }
        by_id.insert(&operation.operation_id, operation);
        by_dot.insert(&operation.stamp.dot, operation);
        additions.push(operation.clone());
    }
    drop(by_id);
    drop(by_dot);

    for operation in &additions {
        state.version_vector.merge(&operation.stamp.observed);
        state.version_vector.observe(&operation.stamp.dot);
    }
    state.operations.extend(additions);
    refresh_device_registry(state).map_err(|_| VaultError::InvalidEncryptedObject)
}

fn validate_payload(
    payload: &OperationBatchPayload,
    membership: &VerifiedMembership,
    require_active_signer: bool,
    allow_legacy_coverage: bool,
    causal_ceiling: Option<&VersionVector>,
) -> Result<(), VaultError> {
    if payload.kind != BATCH_KIND
        || payload.schema_version != super::VAULT_SCHEMA_VERSION
        || payload.dataset_id != membership.dataset_id
        || payload.membership_epoch == 0
        || payload.membership_epoch > membership.epoch
        || payload.operations.is_empty()
        || payload.operations.len() > super::MAX_BATCH_OPERATIONS
        || payload.first_sequence == 0
    {
        return Err(VaultError::InvalidEncryptedObject);
    }
    let signer = membership.device(&payload.signer_device_id)?;
    if (require_active_signer && signer.revoked)
        || !membership.was_authorized_at_epoch(&payload.signer_device_id, payload.membership_epoch)
    {
        return Err(VaultError::RevokedOrUnknownDevice);
    }

    let expected_last = payload
        .first_sequence
        .checked_add(payload.operations.len() as u64 - 1)
        .ok_or(VaultError::InvalidEncryptedObject)?;
    if payload.last_sequence != expected_last
        || (payload.first_sequence == 1) != payload.previous_batch_hash.is_none()
        || payload
            .previous_batch_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
    {
        return Err(VaultError::InvalidEncryptedObject);
    }

    let mut operation_ids = BTreeSet::new();
    let mut observed_ceiling = causal_ceiling.cloned();
    for (offset, operation) in payload.operations.iter().enumerate() {
        let expected_sequence = payload
            .first_sequence
            .checked_add(offset as u64)
            .ok_or(VaultError::InvalidEncryptedObject)?;
        if operation.stamp.dot.device_id != payload.signer_device_id
            || operation.stamp.dot.sequence != expected_sequence
            || !operation_ids.insert(operation.operation_id.as_str())
            || !membership.accepts_sequence(&payload.signer_device_id, expected_sequence)
            || !vector_is_authorized(
                &operation.stamp.observed,
                membership,
                allow_legacy_coverage,
                Some(payload.membership_epoch),
            )
            || observed_ceiling.as_ref().is_some_and(|ceiling| {
                operation
                    .stamp
                    .observed
                    .0
                    .iter()
                    .any(|(device_id, sequence)| ceiling.observed(device_id) < *sequence)
            })
        {
            return Err(VaultError::InvalidEncryptedObject);
        }
        if let Some(ceiling) = &mut observed_ceiling {
            ceiling.observe(&operation.stamp.dot);
        }
    }
    validate_serialized_size(payload)
}

fn validate_state_causal_authorization(
    state: &PersonalStateV2,
    membership: &VerifiedMembership,
) -> Result<(), VaultError> {
    let has_legacy_baseline = state_has_legacy_baseline(state);
    if state.operations.iter().any(|operation| {
        (!membership.accepts_sequence(&operation.stamp.dot.device_id, operation.stamp.dot.sequence)
            && !is_deterministic_legacy_baseline(operation))
            || !vector_is_authorized(
                &operation.stamp.observed,
                membership,
                has_legacy_baseline,
                None,
            )
    }) || !vector_is_authorized(&state.version_vector, membership, has_legacy_baseline, None)
        || state
            .compaction_checkpoint
            .as_ref()
            .is_some_and(|checkpoint| {
                !vector_is_authorized(&checkpoint.coverage, membership, has_legacy_baseline, None)
            })
    {
        return Err(VaultError::RevokedOrUnknownDevice);
    }
    Ok(())
}

fn vector_is_authorized(
    vector: &VersionVector,
    membership: &VerifiedMembership,
    allow_legacy_coverage: bool,
    at_epoch: Option<u64>,
) -> bool {
    vector.0.iter().all(|(device_id, sequence)| {
        (membership.accepts_sequence(device_id, *sequence)
            && at_epoch.is_none_or(|epoch| membership.was_admitted_by_epoch(device_id, epoch)))
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

fn validate_serialized_size<T: Serialize + ?Sized>(value: &T) -> Result<(), VaultError> {
    let bytes = serde_json::to_vec(value).map_err(|_| VaultError::SerializationFailed)?;
    if bytes.len() > super::MAX_BATCH_PLAINTEXT_BYTES {
        Err(VaultError::PayloadTooLarge)
    } else {
        Ok(())
    }
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::personal_state::{
        CausalStamp, DevicePublicIdentity, DeviceRecord, Dot, Operation, OperationOrigin,
        VersionVector,
    };

    use super::super::membership::{
        MembershipAction, MembershipAnchor, MembershipChain, SignedMembershipRoot,
    };
    use super::*;

    fn fixture() -> (
        MembershipChain,
        MembershipAnchor,
        VerifiedMembership,
        SigningKey,
        DeviceRecord,
        OperationEnvelope,
    ) {
        let device_id = DeviceId::new("device-a").unwrap();
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let device = DeviceRecord {
            device_id: device_id.clone(),
            name: "Device A".to_owned(),
            revoked: false,
            public_identity: Some(DevicePublicIdentity {
                age_recipient: age::x25519::Identity::generate().to_public().to_string(),
                ed25519_verifying_key: base64url_encode(signing_key.verifying_key().as_bytes()),
            }),
        };
        let recovery_signing_key = SigningKey::from_bytes(&[9_u8; 32]);
        let root = SignedMembershipRoot::create(
            "dataset-a",
            age::x25519::Identity::generate().to_public().to_string(),
            &recovery_signing_key,
            device.clone(),
        )
        .unwrap();
        let anchor = MembershipAnchor::RootHash(root.hash().unwrap());
        let chain = MembershipChain::new(root);
        let membership = chain.verify(&anchor).unwrap();
        let operation = OperationEnvelope {
            operation_id: "add-device-a".to_owned(),
            stamp: CausalStamp {
                dot: Dot {
                    device_id,
                    sequence: 1,
                },
                observed: VersionVector(BTreeMap::new()),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::AddDevice {
                device: device.clone(),
            },
        };
        (chain, anchor, membership, signing_key, device, operation)
    }

    #[test]
    fn apply_is_atomic_and_exact_replay_is_a_noop() {
        let (_, _, membership, signing_key, device, operation) = fixture();
        let mut state = PersonalStateV2::empty("dataset-a".to_owned()).unwrap();
        let batch = SignedOperationBatch::create(
            &membership,
            &state,
            device.device_id.clone(),
            &signing_key,
            &BatchAnchor::empty(device.device_id.clone()),
            vec![operation],
        )
        .unwrap();
        let mut anchor = BatchAnchor::empty(device.device_id.clone());

        assert_eq!(
            apply_operation_batch(&mut state, &mut anchor, &membership, &batch).unwrap(),
            BatchAcceptance::Applied
        );
        let applied = state.clone();
        assert_eq!(
            apply_operation_batch(&mut state, &mut anchor, &membership, &batch).unwrap(),
            BatchAcceptance::Duplicate
        );
        assert_eq!(state, applied);
        assert_eq!(state.device_registry, membership.devices);
    }

    #[test]
    fn tamper_gap_and_post_revoke_sequence_are_rejected() {
        let (_, _, mut membership, signing_key, device, operation) = fixture();
        let batch = SignedOperationBatch::create(
            &membership,
            &PersonalStateV2::empty("dataset-a".to_owned()).unwrap(),
            device.device_id.clone(),
            &signing_key,
            &BatchAnchor::empty(device.device_id.clone()),
            vec![operation.clone()],
        )
        .unwrap();
        let mut tampered = batch.clone();
        tampered.payload.operations[0].operation_id = "tampered".to_owned();
        assert_eq!(
            tampered.verify(&membership),
            Err(VaultError::SignatureVerificationFailed)
        );

        let mut sequence_three = operation.clone();
        sequence_three.stamp.dot.sequence = 3;
        sequence_three.operation_id = "sequence-three".to_owned();
        let gap_payload = OperationBatchPayload {
            kind: BATCH_KIND.to_owned(),
            schema_version: super::super::VAULT_SCHEMA_VERSION,
            dataset_id: membership.dataset_id.clone(),
            membership_epoch: membership.epoch,
            signer_device_id: device.device_id.clone(),
            first_sequence: 3,
            last_sequence: 3,
            previous_batch_hash: Some("0".repeat(64)),
            operations: vec![sequence_three],
        };
        let gap = SignedOperationBatch {
            signature: sign_serializable(BATCH_SIGNATURE_DOMAIN, &signing_key, &gap_payload)
                .unwrap(),
            payload: gap_payload,
        };
        let mut state = PersonalStateV2::empty("dataset-a".to_owned()).unwrap();
        let mut anchor = BatchAnchor::empty(device.device_id.clone());
        assert_eq!(
            apply_operation_batch(&mut state, &mut anchor, &membership, &gap),
            Err(VaultError::SequenceGap)
        );
        assert!(state.operations.is_empty());

        membership
            .devices
            .get_mut(&device.device_id)
            .unwrap()
            .revoked = true;
        membership
            .revocation_cutoffs
            .insert(device.device_id.clone(), 1);
        assert!(batch.verify(&membership).is_ok());

        let mut post_revoke = operation;
        post_revoke.stamp.dot.sequence = 2;
        post_revoke.operation_id = "post-revoke".to_owned();
        let payload = OperationBatchPayload {
            kind: BATCH_KIND.to_owned(),
            schema_version: super::super::VAULT_SCHEMA_VERSION,
            dataset_id: membership.dataset_id.clone(),
            membership_epoch: membership.epoch,
            signer_device_id: device.device_id,
            first_sequence: 2,
            last_sequence: 2,
            previous_batch_hash: Some(batch.hash().unwrap()),
            operations: vec![post_revoke],
        };
        let signature = sign_serializable(BATCH_SIGNATURE_DOMAIN, &signing_key, &payload).unwrap();
        let post_revoke = SignedOperationBatch { payload, signature };
        assert_eq!(
            post_revoke.verify(&membership),
            Err(VaultError::InvalidEncryptedObject)
        );
    }

    #[test]
    fn active_device_can_observe_a_revoked_device_through_its_cutoff() {
        let (mut chain, membership_anchor, _, signing_key, device_a, add_device_a) = fixture();
        let device_b_secrets = DeviceSecretMaterial::generate_for("device-b").unwrap();
        let device_b = DeviceRecord {
            device_id: DeviceId::new("device-b").unwrap(),
            name: "Device B".to_owned(),
            revoked: false,
            public_identity: Some(device_b_secrets.public_identity()),
        };
        let membership = chain
            .append_device_action(
                &membership_anchor,
                &device_a.device_id,
                &signing_key,
                MembershipAction::AddDevice {
                    device: device_b.clone(),
                },
            )
            .unwrap();
        let add_device_b = OperationEnvelope {
            operation_id: "add-device-b".to_owned(),
            stamp: CausalStamp {
                dot: Dot {
                    device_id: device_a.device_id.clone(),
                    sequence: 2,
                },
                observed: VersionVector(BTreeMap::from([(device_a.device_id.clone(), 1)])),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::AddDevice {
                device: device_b.clone(),
            },
        };
        let mut state = PersonalStateV2::empty("dataset-a".to_owned()).unwrap();
        let mut device_a_anchor = BatchAnchor::empty(device_a.device_id.clone());
        let device_a_batch = SignedOperationBatch::create(
            &membership,
            &state,
            device_a.device_id.clone(),
            &signing_key,
            &device_a_anchor,
            vec![add_device_a, add_device_b],
        )
        .unwrap();
        apply_operation_batch(
            &mut state,
            &mut device_a_anchor,
            &membership,
            &device_a_batch,
        )
        .unwrap();

        let device_b_operation = OperationEnvelope {
            operation_id: "device-b-sequence-one".to_owned(),
            stamp: CausalStamp {
                dot: Dot {
                    device_id: device_b.device_id.clone(),
                    sequence: 1,
                },
                observed: state.version_vector.clone(),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::SetAvoidArtist {
                artist_key: "artist-b".to_owned(),
                avoid: true,
            },
        };
        let mut device_b_anchor = BatchAnchor::empty(device_b.device_id.clone());
        let device_b_batch = SignedOperationBatch::create(
            &membership,
            &state,
            device_b.device_id.clone(),
            device_b_secrets.signing_key(),
            &device_b_anchor,
            vec![device_b_operation],
        )
        .unwrap();
        apply_operation_batch(
            &mut state,
            &mut device_b_anchor,
            &membership,
            &device_b_batch,
        )
        .unwrap();

        let revoked_membership = chain
            .append_device_action(
                &membership_anchor,
                &device_a.device_id,
                &signing_key,
                MembershipAction::RevokeDevice {
                    device_id: device_b.device_id.clone(),
                    last_accepted_sequence: 1,
                },
            )
            .unwrap();
        let revoke_operation = OperationEnvelope {
            operation_id: "revoke-device-b".to_owned(),
            stamp: CausalStamp {
                dot: Dot {
                    device_id: device_a.device_id.clone(),
                    sequence: 3,
                },
                observed: state.version_vector.clone(),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::RevokeDevice {
                device_id: device_b.device_id.clone(),
            },
        };
        let post_revoke_batch = SignedOperationBatch::create(
            &revoked_membership,
            &state,
            device_a.device_id.clone(),
            &signing_key,
            &device_a_anchor,
            vec![revoke_operation],
        )
        .unwrap();
        assert_eq!(
            apply_operation_batch(
                &mut state,
                &mut device_a_anchor,
                &revoked_membership,
                &post_revoke_batch,
            )
            .unwrap(),
            BatchAcceptance::Applied
        );

        let mut beyond_cutoff = state.version_vector.clone();
        beyond_cutoff.0.insert(device_b.device_id, 2);
        let invalid_operation = OperationEnvelope {
            operation_id: "observe-beyond-revoke-cutoff".to_owned(),
            stamp: CausalStamp {
                dot: Dot {
                    device_id: device_a.device_id.clone(),
                    sequence: 4,
                },
                observed: beyond_cutoff,
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::SetAvoidArtist {
                artist_key: "artist-after-revoke".to_owned(),
                avoid: true,
            },
        };
        assert_eq!(
            SignedOperationBatch::create(
                &revoked_membership,
                &state,
                device_a.device_id,
                &signing_key,
                &device_a_anchor,
                vec![invalid_operation],
            )
            .err(),
            Some(VaultError::InvalidEncryptedObject)
        );
    }

    #[test]
    fn unapproved_membership_operation_does_not_advance_state_or_anchor() {
        let (_, _, membership, signing_key, device, operation) = fixture();
        let mut state = PersonalStateV2::empty("dataset-a".to_owned()).unwrap();
        let first = SignedOperationBatch::create(
            &membership,
            &state,
            device.device_id.clone(),
            &signing_key,
            &BatchAnchor::empty(device.device_id.clone()),
            vec![operation],
        )
        .unwrap();
        let mut anchor = BatchAnchor::empty(device.device_id.clone());
        apply_operation_batch(&mut state, &mut anchor, &membership, &first).unwrap();

        let poisoned_operation = OperationEnvelope {
            operation_id: "poisoned-legacy-vector".to_owned(),
            stamp: CausalStamp {
                dot: Dot {
                    device_id: device.device_id.clone(),
                    sequence: 2,
                },
                observed: VersionVector(BTreeMap::from([
                    (device.device_id.clone(), 1),
                    (DeviceId::new("legacy").unwrap(), 1),
                ])),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::SetAvoidArtist {
                artist_key: "artist".to_owned(),
                avoid: true,
            },
        };
        let poisoned_payload = OperationBatchPayload {
            kind: BATCH_KIND.to_owned(),
            schema_version: super::super::VAULT_SCHEMA_VERSION,
            dataset_id: membership.dataset_id.clone(),
            membership_epoch: membership.epoch,
            signer_device_id: device.device_id.clone(),
            first_sequence: 2,
            last_sequence: 2,
            previous_batch_hash: anchor.last_batch_hash.clone(),
            operations: vec![poisoned_operation],
        };
        let poisoned = SignedOperationBatch {
            signature: sign_serializable(BATCH_SIGNATURE_DOMAIN, &signing_key, &poisoned_payload)
                .unwrap(),
            payload: poisoned_payload,
        };
        let previous_state = state.clone();
        let previous_anchor = anchor.clone();
        assert_eq!(
            apply_operation_batch(&mut state, &mut anchor, &membership, &poisoned),
            Err(VaultError::InvalidEncryptedObject)
        );
        assert_eq!(state, previous_state);
        assert_eq!(anchor, previous_anchor);

        let mut unknown_payload = poisoned.payload;
        unknown_payload.operations[0].operation_id = "poisoned-unknown-vector".to_owned();
        unknown_payload.operations[0]
            .stamp
            .observed
            .0
            .remove(&DeviceId::new("legacy").unwrap());
        unknown_payload.operations[0]
            .stamp
            .observed
            .0
            .insert(DeviceId::new("unknown-device").unwrap(), 7);
        let unknown = SignedOperationBatch {
            signature: sign_serializable(BATCH_SIGNATURE_DOMAIN, &signing_key, &unknown_payload)
                .unwrap(),
            payload: unknown_payload,
        };
        assert_eq!(
            apply_operation_batch(&mut state, &mut anchor, &membership, &unknown),
            Err(VaultError::InvalidEncryptedObject)
        );
        assert_eq!(state, previous_state);
        assert_eq!(anchor, previous_anchor);

        let rogue_device = DeviceRecord {
            device_id: DeviceId::new("device-rogue").unwrap(),
            name: "Rogue".to_owned(),
            revoked: false,
            public_identity: device.public_identity.clone(),
        };
        let rogue_operation = OperationEnvelope {
            operation_id: "rogue-device-add".to_owned(),
            stamp: CausalStamp {
                dot: Dot {
                    device_id: device.device_id.clone(),
                    sequence: 2,
                },
                observed: VersionVector(BTreeMap::from([(device.device_id.clone(), 1)])),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::AddDevice {
                device: rogue_device,
            },
        };
        let rogue = SignedOperationBatch::create(
            &membership,
            &state,
            device.device_id,
            &signing_key,
            &anchor,
            vec![rogue_operation],
        )
        .unwrap();
        assert_eq!(
            apply_operation_batch(&mut state, &mut anchor, &membership, &rogue),
            Err(VaultError::RegistryMismatch)
        );
        assert_eq!(state, previous_state);
        assert_eq!(anchor, previous_anchor);
    }

    #[test]
    fn signer_cannot_claim_an_epoch_before_admission() {
        let (mut chain, membership_anchor, _, first_key, first_device, _) = fixture();
        let second_key = SigningKey::from_bytes(&[23_u8; 32]);
        let second_device = DeviceRecord {
            device_id: DeviceId::new("device-b").unwrap(),
            name: "Device B".to_owned(),
            revoked: false,
            public_identity: Some(DevicePublicIdentity {
                age_recipient: age::x25519::Identity::generate().to_public().to_string(),
                ed25519_verifying_key: base64url_encode(second_key.verifying_key().as_bytes()),
            }),
        };
        chain
            .append_device_action(
                &membership_anchor,
                &first_device.device_id,
                &first_key,
                MembershipAction::AddDevice {
                    device: second_device.clone(),
                },
            )
            .unwrap();
        let membership = chain.verify(&membership_anchor).unwrap();
        assert_eq!(membership.epoch, 2);

        let operation = OperationEnvelope {
            operation_id: "pre-admission-claim".to_owned(),
            stamp: CausalStamp {
                dot: Dot {
                    device_id: second_device.device_id.clone(),
                    sequence: 1,
                },
                observed: VersionVector::default(),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::SetAvoidArtist {
                artist_key: "artist".to_owned(),
                avoid: true,
            },
        };
        let payload = OperationBatchPayload {
            kind: BATCH_KIND.to_owned(),
            schema_version: super::super::VAULT_SCHEMA_VERSION,
            dataset_id: membership.dataset_id.clone(),
            membership_epoch: 1,
            signer_device_id: second_device.device_id,
            first_sequence: 1,
            last_sequence: 1,
            previous_batch_hash: None,
            operations: vec![operation],
        };
        let signature = sign_serializable(BATCH_SIGNATURE_DOMAIN, &second_key, &payload).unwrap();
        assert_eq!(
            SignedOperationBatch { payload, signature }.verify(&membership),
            Err(VaultError::RevokedOrUnknownDevice)
        );
    }
}
