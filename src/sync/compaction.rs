//! Authenticated acknowledgements for a personal-state compaction checkpoint.
//!
//! The portable ledger's `acknowledged_by` field is deliberately not trusted by the vault.
//! Acknowledgement is instead a per-device signed high-water object updated with a strong ETag
//! compare-and-swap after that device durably installs a checkpoint carrying the generation.
//! Physical cleanup may proceed only when every device active in the same membership epoch has
//! signed and every installed checkpoint anchor is on the authenticated current lineage.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::personal_state::{
    CompactionCheckpoint, CompactionLeaderAuthorization, DeviceId, VersionVector,
};

use super::crypto::{
    DeviceSecretMaterial, EncryptedObject, base64url_encode, decrypt_json_with_identity,
    encrypt_json_to_recipients, sha256_domain_hex, sign_serializable, verify_serializable,
};
use super::{
    MembershipAnchor, MembershipChain, ObjectKey, VAULT_SCHEMA_VERSION, VaultError,
    VerifiedMembership,
};

const ACK_KIND: &str = "yututui_vault_compaction_ack";
const ACK_SIGNATURE_DOMAIN: &[u8] = b"yututui-vault-compaction-ack-signature-v1";
const ACK_HASH_DOMAIN: &[u8] = b"yututui-vault-compaction-ack-hash-v1";
const GENERATION_HASH_DOMAIN: &[u8] = b"yututui-vault-compaction-generation-hash-v1";
const AUTHORIZATION_KIND: &str = "yututui_vault_compaction_leader_authorization";
const AUTHORIZATION_SIGNATURE_DOMAIN: &[u8] =
    b"yututui-vault-compaction-leader-authorization-signature-v1";

#[derive(Serialize)]
struct CompactionAuthorizationMaterial<'a> {
    kind: &'static str,
    schema_version: u32,
    dataset_id: &'a str,
    checkpoint_id: &'a str,
    compaction_generation: u64,
    coverage: &'a VersionVector,
    previous_checkpoint_hash: Option<&'a str>,
    retained_engagement_operations: &'a BTreeSet<String>,
    membership_epoch: u64,
    membership_head_hash: &'a str,
    leader_device_id: &'a DeviceId,
    introducing_checkpoint_sequence: u64,
    introducing_checkpoint_parent_hash: &'a str,
}

#[derive(Serialize)]
struct CompactionGenerationMaterial<'a> {
    dataset_id: &'a str,
    checkpoint_id: &'a str,
    compaction_generation: u64,
    coverage: &'a VersionVector,
    previous_checkpoint_hash: Option<&'a str>,
    retained_engagement_operations: &'a BTreeSet<String>,
    leader_authorization: &'a Option<CompactionLeaderAuthorization>,
}

/// Sign one compaction transition with the lowest device id active in the exact membership epoch.
pub fn authorize_compaction(
    dataset_id: &str,
    compaction: &CompactionCheckpoint,
    membership: &VerifiedMembership,
    local_device_id: DeviceId,
    device: &DeviceSecretMaterial,
    introducing_checkpoint_sequence: u64,
    introducing_checkpoint_parent_hash: &str,
) -> Result<CompactionLeaderAuthorization, VaultError> {
    if compaction.leader_authorization.is_some()
        || compaction.compaction_generation == 0
        || dataset_id != membership.dataset_id
        || local_device_id.as_str() != device.device_id()
        || compaction.checkpoint_id.is_empty()
        || introducing_checkpoint_sequence < 2
        || !valid_hash(introducing_checkpoint_parent_hash)
    {
        return Err(VaultError::InvalidEncryptedObject);
    }
    let leader = compaction_leader(membership)?;
    if leader != local_device_id {
        return Err(VaultError::RevokedOrUnknownDevice);
    }
    let signer = active_signer(membership, &leader)?;
    if signer.ed25519_verifying_key
        != base64url_encode(device.signing_key().verifying_key().as_bytes())
    {
        return Err(VaultError::InvalidSigningKey);
    }
    let mut authorization = CompactionLeaderAuthorization {
        membership_epoch: membership.epoch,
        membership_head_hash: membership.head_hash.clone(),
        leader_device_id: leader,
        introducing_checkpoint_sequence,
        introducing_checkpoint_parent_hash: introducing_checkpoint_parent_hash.to_owned(),
        signature: String::new(),
    };
    authorization.signature = sign_serializable(
        AUTHORIZATION_SIGNATURE_DOMAIN,
        device.signing_key(),
        &authorization_material(dataset_id, compaction, &authorization),
    )?;
    Ok(authorization)
}

/// Verify a retained leader authorization against its exact historical membership epoch.
pub fn verify_compaction_authorization(
    dataset_id: &str,
    compaction: &CompactionCheckpoint,
    membership: &MembershipChain,
    membership_anchor: &MembershipAnchor,
) -> Result<(), VaultError> {
    let authorization = compaction
        .leader_authorization
        .as_ref()
        .ok_or(VaultError::InvalidEncryptedObject)?;
    if compaction.compaction_generation == 0
        || authorization.introducing_checkpoint_sequence < 2
        || !valid_hash(&authorization.introducing_checkpoint_parent_hash)
    {
        return Err(VaultError::InvalidEncryptedObject);
    }
    let historical = membership.verify_epoch_head(
        membership_anchor,
        authorization.membership_epoch,
        &authorization.membership_head_hash,
    )?;
    if historical.dataset_id != dataset_id
        || compaction_leader(&historical)? != authorization.leader_device_id
    {
        return Err(VaultError::RevokedOrUnknownDevice);
    }
    let signer = active_signer(&historical, &authorization.leader_device_id)?;
    verify_serializable(
        AUTHORIZATION_SIGNATURE_DOMAIN,
        &signer.ed25519_verifying_key,
        &authorization_material(dataset_id, compaction, authorization),
        &authorization.signature,
    )
}

fn authorization_material<'a>(
    dataset_id: &'a str,
    compaction: &'a CompactionCheckpoint,
    authorization: &'a CompactionLeaderAuthorization,
) -> CompactionAuthorizationMaterial<'a> {
    CompactionAuthorizationMaterial {
        kind: AUTHORIZATION_KIND,
        schema_version: VAULT_SCHEMA_VERSION,
        dataset_id,
        checkpoint_id: &compaction.checkpoint_id,
        compaction_generation: compaction.compaction_generation,
        coverage: &compaction.coverage,
        previous_checkpoint_hash: compaction.previous_checkpoint_hash.as_deref(),
        retained_engagement_operations: &compaction.retained_engagement_operations,
        membership_epoch: authorization.membership_epoch,
        membership_head_hash: &authorization.membership_head_hash,
        leader_device_id: &authorization.leader_device_id,
        introducing_checkpoint_sequence: authorization.introducing_checkpoint_sequence,
        introducing_checkpoint_parent_hash: &authorization.introducing_checkpoint_parent_hash,
    }
}

fn compaction_leader(membership: &VerifiedMembership) -> Result<DeviceId, VaultError> {
    membership
        .active_devices()
        .filter(|device| device.device_id.as_str() != "legacy")
        .map(|device| device.device_id.clone())
        .min()
        .ok_or(VaultError::LastActiveDevice)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionAckPayload {
    pub kind: String,
    pub schema_version: u32,
    pub dataset_id: String,
    pub compaction_id: String,
    pub compaction_generation_hash: String,
    pub coverage: VersionVector,
    pub installed_checkpoint_sequence: u64,
    pub installed_checkpoint_hash: String,
    pub membership_epoch: u64,
    pub membership_head_hash: String,
    pub signer_device_id: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedCompactionAck {
    pub payload: CompactionAckPayload,
    pub signature: String,
}

impl SignedCompactionAck {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        dataset_id: &str,
        compaction: &CompactionCheckpoint,
        checkpoint_sequence: u64,
        checkpoint_hash: &str,
        membership: &VerifiedMembership,
        signer_device_id: DeviceId,
        device: &DeviceSecretMaterial,
    ) -> Result<Self, VaultError> {
        if signer_device_id.as_str() != device.device_id() {
            return Err(VaultError::InvalidDeviceIdentity);
        }
        let signer = active_signer(membership, &signer_device_id)?;
        if signer.ed25519_verifying_key
            != base64url_encode(device.signing_key().verifying_key().as_bytes())
        {
            return Err(VaultError::InvalidSigningKey);
        }
        let payload = CompactionAckPayload {
            kind: ACK_KIND.to_owned(),
            schema_version: VAULT_SCHEMA_VERSION,
            dataset_id: dataset_id.to_owned(),
            compaction_id: compaction.checkpoint_id.clone(),
            compaction_generation_hash: compaction_generation_hash(dataset_id, compaction)?,
            coverage: compaction.coverage.clone(),
            installed_checkpoint_sequence: checkpoint_sequence,
            installed_checkpoint_hash: checkpoint_hash.to_owned(),
            membership_epoch: membership.epoch,
            membership_head_hash: membership.head_hash.clone(),
            signer_device_id,
        };
        validate_payload(&payload, compaction, membership)?;
        let signature = sign_serializable(ACK_SIGNATURE_DOMAIN, device.signing_key(), &payload)?;
        let acknowledgement = Self { payload, signature };
        acknowledgement.verify(compaction, membership)?;
        Ok(acknowledgement)
    }

    pub fn verify(
        &self,
        compaction: &CompactionCheckpoint,
        membership: &VerifiedMembership,
    ) -> Result<(), VaultError> {
        validate_payload(&self.payload, compaction, membership)?;
        let signer = active_signer(membership, &self.payload.signer_device_id)?;
        verify_serializable(
            ACK_SIGNATURE_DOMAIN,
            &signer.ed25519_verifying_key,
            &self.payload,
            &self.signature,
        )
    }

    pub fn hash(&self) -> Result<String, VaultError> {
        let bytes = serde_json::to_vec(self).map_err(|_| VaultError::SerializationFailed)?;
        Ok(sha256_domain_hex(ACK_HASH_DOMAIN, &[&bytes]))
    }

    pub fn encrypt(
        &self,
        compaction: &CompactionCheckpoint,
        membership: &VerifiedMembership,
    ) -> Result<EncryptedObject, VaultError> {
        self.verify(compaction, membership)?;
        encrypt_json_to_recipients(self, &membership.active_recipients()?)
    }

    pub fn decrypt_for_device(
        object: &EncryptedObject,
        device: &DeviceSecretMaterial,
        compaction: &CompactionCheckpoint,
        membership: &VerifiedMembership,
    ) -> Result<Self, VaultError> {
        let acknowledgement: Self = decrypt_json_with_identity(object, device.age_identity())?;
        acknowledgement.verify(compaction, membership)?;
        let recipient_id =
            DeviceId::new(device.device_id()).map_err(|_| VaultError::InvalidDeviceIdentity)?;
        let recipient = membership
            .devices
            .get(&recipient_id)
            .filter(|record| !record.revoked)
            .ok_or(VaultError::RevokedOrUnknownDevice)?;
        if !device.matches_personal_record(recipient) {
            return Err(VaultError::InvalidDeviceIdentity);
        }
        Ok(acknowledgement)
    }
}

/// Return true only when every device active in the exact membership epoch signed the checkpoint.
///
/// Duplicate acknowledgements are harmless. An acknowledgement from a revoked/unknown device or
/// from another epoch/checkpoint is rejected instead of being silently ignored.
pub fn compaction_quorum(
    acknowledgements: &[SignedCompactionAck],
    compaction: &CompactionCheckpoint,
    membership: &VerifiedMembership,
) -> Result<bool, VaultError> {
    let mut acknowledged = BTreeSet::new();
    for acknowledgement in acknowledgements {
        acknowledgement.verify(compaction, membership)?;
        acknowledged.insert(acknowledgement.payload.signer_device_id.clone());
    }
    let active = membership
        .active_devices()
        .map(|device| device.device_id.clone())
        .collect::<BTreeSet<_>>();
    Ok(!active.is_empty() && acknowledged == active)
}

pub fn compaction_ack_prefix(
    dataset_id: &str,
    compaction: &CompactionCheckpoint,
    membership: &VerifiedMembership,
) -> Result<ObjectKey, VaultError> {
    validate_component(dataset_id)?;
    validate_component(&compaction.checkpoint_id)?;
    let generation_hash = compaction_generation_hash(dataset_id, compaction)?;
    if membership.dataset_id != dataset_id || !valid_hash(&membership.head_hash) {
        return Err(VaultError::InvalidObjectKey);
    }
    ObjectKey::new(format!(
        "yututui/v2/{dataset_id}/compaction-acks/{}/{generation_hash}/{}",
        compaction.checkpoint_id, membership.head_hash
    ))
}

pub fn compaction_ack_key(
    dataset_id: &str,
    compaction: &CompactionCheckpoint,
    membership: &VerifiedMembership,
    device_id: &DeviceId,
) -> Result<ObjectKey, VaultError> {
    ObjectKey::new(format!(
        "{}/{}.age",
        compaction_ack_prefix(dataset_id, compaction, membership)?.as_str(),
        device_id.as_str()
    ))
}

fn validate_payload(
    payload: &CompactionAckPayload,
    compaction: &CompactionCheckpoint,
    membership: &VerifiedMembership,
) -> Result<(), VaultError> {
    if payload.kind != ACK_KIND
        || payload.schema_version != VAULT_SCHEMA_VERSION
        || payload.dataset_id != membership.dataset_id
        || payload.compaction_id != compaction.checkpoint_id
        || compaction.compaction_generation == 0
        || payload.compaction_generation_hash
            != compaction_generation_hash(&payload.dataset_id, compaction)?
        || payload.coverage != compaction.coverage
        || payload.membership_epoch != membership.epoch
        || payload.membership_head_hash != membership.head_hash
        || payload.installed_checkpoint_sequence == 0
        || !valid_hash(&payload.installed_checkpoint_hash)
        || !compaction.acknowledged_by.is_empty()
    {
        return Err(VaultError::InvalidEncryptedObject);
    }
    validate_component(&payload.dataset_id)?;
    validate_component(&payload.compaction_id)?;
    if payload.coverage.0.iter().any(|(device_id, sequence)| {
        *sequence == 0
            || !(membership.accepts_sequence(device_id, *sequence)
                || device_id.as_str() == "legacy" && *sequence == 1)
    }) {
        return Err(VaultError::RevokedOrUnknownDevice);
    }
    active_signer(membership, &payload.signer_device_id)?;
    Ok(())
}

fn compaction_generation_hash(
    dataset_id: &str,
    compaction: &CompactionCheckpoint,
) -> Result<String, VaultError> {
    let material = CompactionGenerationMaterial {
        dataset_id,
        checkpoint_id: &compaction.checkpoint_id,
        compaction_generation: compaction.compaction_generation,
        coverage: &compaction.coverage,
        previous_checkpoint_hash: compaction.previous_checkpoint_hash.as_deref(),
        retained_engagement_operations: &compaction.retained_engagement_operations,
        leader_authorization: &compaction.leader_authorization,
    };
    let bytes = serde_json::to_vec(&material).map_err(|_| VaultError::SerializationFailed)?;
    Ok(sha256_domain_hex(GENERATION_HASH_DOMAIN, &[&bytes]))
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

fn validate_component(value: &str) -> Result<(), VaultError> {
    if value.is_empty()
        || value.len() > 512
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(VaultError::InvalidObjectKey);
    }
    Ok(())
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::personal_state::{DeviceRecord, VersionVector};
    use crate::sync::{
        MembershipAction, MembershipAnchor, MembershipChain, RecoveryKit, SignedMembershipRoot,
    };

    struct Fixture {
        first: DeviceSecretMaterial,
        second: DeviceSecretMaterial,
        membership: MembershipChain,
        anchor: MembershipAnchor,
        compaction: CompactionCheckpoint,
    }

    impl Fixture {
        fn verified(&self) -> VerifiedMembership {
            self.membership.verify(&self.anchor).unwrap()
        }
    }

    fn device_record(device: &DeviceSecretMaterial) -> DeviceRecord {
        DeviceRecord {
            device_id: DeviceId::new(device.device_id()).unwrap(),
            name: device.device_id().to_owned(),
            revoked: false,
            public_identity: Some(device.public_identity()),
        }
    }

    fn fixture() -> Fixture {
        let dataset_id = "compaction-ack-dataset";
        let recovery = RecoveryKit::generate(dataset_id, None).unwrap();
        let first = DeviceSecretMaterial::generate_for("device-a").unwrap();
        let second = DeviceSecretMaterial::generate_for("device-b").unwrap();
        let root = SignedMembershipRoot::create(
            dataset_id,
            recovery.recovery_recipient(),
            &recovery.signing_key().unwrap(),
            device_record(&first),
        )
        .unwrap();
        let anchor = MembershipAnchor::RootHash(root.hash().unwrap());
        let mut membership = MembershipChain::new(root);
        membership
            .append_device_action(
                &anchor,
                &DeviceId::new(first.device_id()).unwrap(),
                first.signing_key(),
                MembershipAction::AddDevice {
                    device: device_record(&second),
                },
            )
            .unwrap();
        let compaction = CompactionCheckpoint {
            checkpoint_id: "compaction-001".to_owned(),
            compaction_generation: 1,
            coverage: VersionVector(BTreeMap::from([
                (DeviceId::new(first.device_id()).unwrap(), 7),
                (DeviceId::new(second.device_id()).unwrap(), 5),
            ])),
            previous_checkpoint_hash: None,
            retained_engagement_operations: BTreeSet::new(),
            leader_authorization: None,
            acknowledged_by: BTreeSet::new(),
        };
        Fixture {
            first,
            second,
            membership,
            anchor,
            compaction,
        }
    }

    fn acknowledgement(fixture: &Fixture, device: &DeviceSecretMaterial) -> SignedCompactionAck {
        let membership = fixture.verified();
        SignedCompactionAck::create(
            &membership.dataset_id,
            &fixture.compaction,
            4,
            &"a".repeat(64),
            &membership,
            DeviceId::new(device.device_id()).unwrap(),
            device,
        )
        .unwrap()
    }

    #[test]
    fn only_historical_leader_can_authorize_a_compaction() {
        let mut fixture = fixture();
        let historical = fixture.verified();
        let leader_id = DeviceId::new(fixture.first.device_id()).unwrap();
        let second_id = DeviceId::new(fixture.second.device_id()).unwrap();
        let mut zero_generation = fixture.compaction.clone();
        zero_generation.compaction_generation = 0;
        assert_eq!(
            authorize_compaction(
                &historical.dataset_id,
                &zero_generation,
                &historical,
                leader_id.clone(),
                &fixture.first,
                4,
                &"a".repeat(64),
            ),
            Err(VaultError::InvalidEncryptedObject)
        );

        assert_eq!(
            authorize_compaction(
                &historical.dataset_id,
                &fixture.compaction,
                &historical,
                second_id.clone(),
                &fixture.second,
                4,
                &"a".repeat(64),
            ),
            Err(VaultError::RevokedOrUnknownDevice)
        );

        let authorization = authorize_compaction(
            &historical.dataset_id,
            &fixture.compaction,
            &historical,
            leader_id.clone(),
            &fixture.first,
            4,
            &"a".repeat(64),
        )
        .unwrap();
        let mut authorized = fixture.compaction.clone();
        authorized.leader_authorization = Some(authorization);
        verify_compaction_authorization(
            &historical.dataset_id,
            &authorized,
            &fixture.membership,
            &fixture.anchor,
        )
        .unwrap();

        let mut forged = CompactionLeaderAuthorization {
            membership_epoch: historical.epoch,
            membership_head_hash: historical.head_hash.clone(),
            leader_device_id: second_id.clone(),
            introducing_checkpoint_sequence: 4,
            introducing_checkpoint_parent_hash: "a".repeat(64),
            signature: String::new(),
        };
        forged.signature = sign_serializable(
            AUTHORIZATION_SIGNATURE_DOMAIN,
            fixture.second.signing_key(),
            &authorization_material(&historical.dataset_id, &fixture.compaction, &forged),
        )
        .unwrap();
        let mut forged_compaction = fixture.compaction.clone();
        forged_compaction.leader_authorization = Some(forged);
        assert_eq!(
            verify_compaction_authorization(
                &historical.dataset_id,
                &forged_compaction,
                &fixture.membership,
                &fixture.anchor,
            ),
            Err(VaultError::RevokedOrUnknownDevice)
        );

        fixture
            .membership
            .append_device_action(
                &fixture.anchor,
                &second_id,
                fixture.second.signing_key(),
                MembershipAction::RevokeDevice {
                    device_id: leader_id,
                    last_accepted_sequence: 7,
                },
            )
            .unwrap();
        verify_compaction_authorization(
            &historical.dataset_id,
            &authorized,
            &fixture.membership,
            &fixture.anchor,
        )
        .unwrap();
    }

    #[test]
    fn quorum_requires_every_active_device_and_accepts_duplicate_objects() {
        let fixture = fixture();
        let membership = fixture.verified();
        let first = acknowledgement(&fixture, &fixture.first);
        let second = SignedCompactionAck::create(
            &membership.dataset_id,
            &fixture.compaction,
            5,
            &"b".repeat(64),
            &membership,
            DeviceId::new(fixture.second.device_id()).unwrap(),
            &fixture.second,
        )
        .unwrap();

        assert!(
            !compaction_quorum(
                std::slice::from_ref(&first),
                &fixture.compaction,
                &membership,
            )
            .unwrap()
        );
        assert!(
            compaction_quorum(
                &[first.clone(), first, second],
                &fixture.compaction,
                &membership,
            )
            .unwrap()
        );
    }

    #[test]
    fn acknowledgement_is_encrypted_and_bound_to_generation_and_membership() {
        let fixture = fixture();
        let membership = fixture.verified();
        let acknowledgement = acknowledgement(&fixture, &fixture.first);
        let encrypted = acknowledgement
            .encrypt(&fixture.compaction, &membership)
            .unwrap();
        let decoded = SignedCompactionAck::decrypt_for_device(
            &encrypted,
            &fixture.second,
            &fixture.compaction,
            &membership,
        )
        .unwrap();
        assert_eq!(decoded, acknowledgement);
        let mut other_generation = fixture.compaction.clone();
        other_generation.checkpoint_id = "compaction-002".to_owned();
        assert!(
            SignedCompactionAck::decrypt_for_device(
                &encrypted,
                &fixture.second,
                &other_generation,
                &membership,
            )
            .is_err()
        );
    }

    #[test]
    fn tamper_and_revocation_invalidate_an_acknowledgement() {
        let mut fixture = fixture();
        let old_membership = fixture.verified();
        let old_epoch_ack = acknowledgement(&fixture, &fixture.first);
        let mut acknowledgement = acknowledgement(&fixture, &fixture.second);
        acknowledgement.payload.installed_checkpoint_hash = "b".repeat(64);
        assert!(
            acknowledgement
                .verify(&fixture.compaction, &old_membership)
                .is_err()
        );

        fixture
            .membership
            .append_device_action(
                &fixture.anchor,
                &DeviceId::new(fixture.first.device_id()).unwrap(),
                fixture.first.signing_key(),
                MembershipAction::RevokeDevice {
                    device_id: DeviceId::new(fixture.second.device_id()).unwrap(),
                    last_accepted_sequence: 5,
                },
            )
            .unwrap();
        let revoked_membership = fixture.verified();
        assert!(
            old_epoch_ack
                .verify(&fixture.compaction, &revoked_membership)
                .is_err(),
            "an acknowledgement is bound to the exact membership epoch"
        );
    }

    #[test]
    fn acknowledgement_keys_are_dataset_and_device_scoped() {
        let fixture = fixture();
        let membership = fixture.verified();
        let device = DeviceId::new("device-a").unwrap();
        let generation_hash =
            compaction_generation_hash("compaction-ack-dataset", &fixture.compaction).unwrap();
        assert_eq!(
            compaction_ack_key(
                "compaction-ack-dataset",
                &fixture.compaction,
                &membership,
                &device,
            )
            .unwrap()
            .as_str(),
            format!(
                "yututui/v2/compaction-ack-dataset/compaction-acks/compaction-001/{generation_hash}/{}/device-a.age",
                membership.head_hash
            )
        );
        assert!(compaction_ack_prefix("../dataset", &fixture.compaction, &membership).is_err());
        let mut wrong_membership = membership;
        wrong_membership.head_hash = "not-a-hash".to_owned();
        assert!(
            compaction_ack_prefix(
                "compaction-ack-dataset",
                &fixture.compaction,
                &wrong_membership,
            )
            .is_err()
        );
    }
}
