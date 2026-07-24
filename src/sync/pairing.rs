use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::personal_state::{DeviceId, DevicePublicIdentity, DeviceRecord};

use super::crypto::{
    DeviceSecretMaterial, EncryptedObject, base32_visual_decode, open_pairing_json,
    pairing_proof_b64, random_id_hex, seal_pairing_json, sign_serializable, verify_pairing_proof,
    verify_serializable,
};
use super::error::VaultError;
use super::membership::{MembershipAction, MembershipAnchor, MembershipChain};

const PAIRING_REQUEST_KIND: &str = "yututui_pairing_request";
const PAIRING_APPROVAL_KIND: &str = "yututui_pairing_approval";
const PAIRING_LIFETIME_SECS: i64 = 10 * 60;
const REQUEST_SIGNATURE_DOMAIN: &[u8] = b"yututui-pairing-request-signature-v1";
const APPROVAL_SIGNATURE_DOMAIN: &[u8] = b"yututui-pairing-approval-signature-v1";
const INVITE_ID_DOMAIN: &[u8] = b"yututui-pairing-invite-id-v1";
const CHECKPOINT_CIPHERTEXT_HASH_DOMAIN: &[u8] = b"yututui-pairing-checkpoint-ciphertext-hash-v1";

pub use super::crypto::PairingCode;

pub struct PairingInvite {
    code: PairingCode,
    invite_id: String,
    dataset_id: String,
    membership_root_hash: String,
    membership_starting_head_hash: String,
    expires_at_unix: i64,
    finalized: AtomicBool,
}

impl fmt::Debug for PairingInvite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairingInvite")
            .field("code", &"[redacted]")
            .field("invite_id", &"[redacted]")
            .field("dataset_id", &"[redacted]")
            .field("membership_root_hash", &"[redacted]")
            .field("membership_starting_head_hash", &"[redacted]")
            .field("expires_at_unix", &self.expires_at_unix)
            .field("finalized", &self.finalized.load(Ordering::Acquire))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingRequestPayload {
    pub kind: String,
    pub schema_version: u32,
    pub invite_id: String,
    pub request_nonce: String,
    pub device: DeviceRecord,
    pub issued_at_unix: i64,
    pub expires_at_unix: i64,
    pub pairing_proof: String,
    pub device_signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PairingRequestProof<'a> {
    kind: &'a str,
    schema_version: u32,
    invite_id: &'a str,
    request_nonce: &'a str,
    device: &'a DeviceRecord,
    issued_at_unix: i64,
    expires_at_unix: i64,
}

pub struct SealedPairingRequest {
    pub invite_id: String,
    pub encrypted: EncryptedObject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingApprovalPayload {
    pub kind: String,
    pub schema_version: u32,
    pub invite_id: String,
    pub request_nonce: String,
    pub dataset_id: String,
    pub membership_root_hash: String,
    pub membership_starting_head_hash: String,
    pub membership_head_hash: String,
    pub approved_device_id: DeviceId,
    pub approver_device_id: DeviceId,
    pub membership: MembershipChain,
    pub checkpoint_sequence: u64,
    pub checkpoint_ciphertext_hash: String,
    pub expires_at_unix: i64,
    pub approver_signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PairingApprovalProof<'a> {
    kind: &'a str,
    schema_version: u32,
    invite_id: &'a str,
    request_nonce: &'a str,
    dataset_id: &'a str,
    membership_root_hash: &'a str,
    membership_starting_head_hash: &'a str,
    membership_head_hash: &'a str,
    approved_device_id: &'a DeviceId,
    approver_device_id: &'a DeviceId,
    membership: &'a MembershipChain,
    checkpoint_sequence: u64,
    checkpoint_ciphertext_hash: &'a str,
    expires_at_unix: i64,
}

pub struct SealedPairingApproval {
    pub invite_id: String,
    pub encrypted: EncryptedObject,
}

pub struct ApprovedPairing {
    invite_id: String,
    request_nonce: String,
    approved_device_id: DeviceId,
    membership: MembershipChain,
    membership_root_hash: String,
    membership_head_hash: String,
    signed_checkpoint: super::checkpoint::SignedCheckpoint,
    encrypted_checkpoint: EncryptedObject,
}

impl PairingInvite {
    /// Derive the opaque remote invite directory without exposing the pairing secret.
    pub fn invite_id_for_code(code: &PairingCode) -> Result<String, VaultError> {
        pairing_invite_id(code)
    }

    pub fn create(
        dataset_id: impl Into<String>,
        membership_root_hash: impl Into<String>,
        membership_starting_head_hash: impl Into<String>,
        now_unix: i64,
    ) -> Result<Self, VaultError> {
        let code = PairingCode::generate()?;
        let invite_id = pairing_invite_id(&code)?;
        let dataset_id = dataset_id.into();
        let membership_root_hash = membership_root_hash.into();
        let membership_starting_head_hash = membership_starting_head_hash.into();
        if super::crypto::validate_dataset_id(&dataset_id).is_err()
            || !valid_hash(&membership_root_hash)
            || !valid_hash(&membership_starting_head_hash)
        {
            return Err(VaultError::InvalidMembership);
        }
        Ok(Self {
            code,
            invite_id,
            dataset_id,
            membership_root_hash,
            membership_starting_head_hash,
            expires_at_unix: now_unix
                .checked_add(PAIRING_LIFETIME_SECS)
                .ok_or(VaultError::PairingExpired)?,
            finalized: AtomicBool::new(false),
        })
    }

    /// Rehydrate an owner-only durable invite without minting a replacement code.
    pub(crate) fn resume(
        code: PairingCode,
        dataset_id: impl Into<String>,
        membership_root_hash: impl Into<String>,
        membership_starting_head_hash: impl Into<String>,
        expires_at_unix: i64,
    ) -> Result<Self, VaultError> {
        let invite_id = pairing_invite_id(&code)?;
        let dataset_id = dataset_id.into();
        let membership_root_hash = membership_root_hash.into();
        let membership_starting_head_hash = membership_starting_head_hash.into();
        if super::crypto::validate_dataset_id(&dataset_id).is_err()
            || !valid_hash(&membership_root_hash)
            || !valid_hash(&membership_starting_head_hash)
        {
            return Err(VaultError::InvalidMembership);
        }
        Ok(Self {
            code,
            invite_id,
            dataset_id,
            membership_root_hash,
            membership_starting_head_hash,
            expires_at_unix,
            finalized: AtomicBool::new(false),
        })
    }

    pub fn code(&self) -> &PairingCode {
        &self.code
    }

    pub fn invite_id(&self) -> &str {
        &self.invite_id
    }

    pub fn expires_at_unix(&self) -> i64 {
        self.expires_at_unix
    }

    pub(crate) fn dataset_id(&self) -> &str {
        &self.dataset_id
    }

    pub(crate) fn membership_root_hash(&self) -> &str {
        &self.membership_root_hash
    }

    pub(crate) fn membership_starting_head_hash(&self) -> &str {
        &self.membership_starting_head_hash
    }

    pub fn create_request(
        code: &PairingCode,
        device_name: impl Into<String>,
        device_secrets: &DeviceSecretMaterial,
        now_unix: i64,
    ) -> Result<(SealedPairingRequest, String), VaultError> {
        let invite_id = pairing_invite_id(code)?;
        let request_nonce = random_id_hex::<16>()?;
        let device_id =
            DeviceId::new(device_secrets.device_id()).map_err(|_| VaultError::InvalidDeviceId)?;
        let device = DeviceRecord {
            device_id,
            name: device_name.into(),
            revoked: false,
            public_identity: Some(device_secrets.public_identity()),
        };
        validate_device(&device)?;
        let expires_at_unix = now_unix
            .checked_add(PAIRING_LIFETIME_SECS)
            .ok_or(VaultError::PairingExpired)?;
        let proof = PairingRequestProof {
            kind: PAIRING_REQUEST_KIND,
            schema_version: super::VAULT_SCHEMA_VERSION,
            invite_id: &invite_id,
            request_nonce: &request_nonce,
            device: &device,
            issued_at_unix: now_unix,
            expires_at_unix,
        };
        let challenge = serde_json::to_vec(&proof).map_err(|_| VaultError::SerializationFailed)?;
        let pairing_proof = pairing_proof_b64(code, &challenge)?;
        let signature_value = (&proof, &pairing_proof);
        let device_signature = sign_serializable(
            REQUEST_SIGNATURE_DOMAIN,
            device_secrets.signing_key(),
            &signature_value,
        )?;
        let payload = PairingRequestPayload {
            kind: PAIRING_REQUEST_KIND.to_owned(),
            schema_version: super::VAULT_SCHEMA_VERSION,
            invite_id: invite_id.clone(),
            request_nonce: request_nonce.clone(),
            device,
            issued_at_unix: now_unix,
            expires_at_unix,
            pairing_proof,
            device_signature,
        };
        let encrypted = seal_pairing_json(code, &payload)?;
        Ok((
            SealedPairingRequest {
                invite_id,
                encrypted,
            },
            request_nonce,
        ))
    }

    /// Reauthenticate an exact locally staged request after restart.
    pub fn resume_request(
        code: &PairingCode,
        encrypted: EncryptedObject,
        expected_request_nonce: &str,
        device_secrets: &DeviceSecretMaterial,
        now_unix: i64,
    ) -> Result<SealedPairingRequest, VaultError> {
        let invite_id = pairing_invite_id(code)?;
        let payload: PairingRequestPayload = open_pairing_json(code, &encrypted)?;
        validate_request(&payload, code, &invite_id, now_unix)?;
        if payload.request_nonce != expected_request_nonce
            || !device_secrets.matches_record(&payload.device)
        {
            return Err(VaultError::PairingProofFailed);
        }
        Ok(SealedPairingRequest {
            invite_id,
            encrypted: encrypted.authenticated_after_verification(),
        })
    }

    /// Reauthenticate the exact locally journaled request without extending its lifetime.
    ///
    /// This is only used to recover an immutable request or consume a host approval that was
    /// committed while the request was valid. It must never authorize a new membership change.
    pub(crate) fn resume_journaled_request(
        code: &PairingCode,
        encrypted: EncryptedObject,
        expected_request_nonce: &str,
        device_secrets: &DeviceSecretMaterial,
    ) -> Result<SealedPairingRequest, VaultError> {
        let invite_id = pairing_invite_id(code)?;
        let payload = Self::open_journaled_request(code, &encrypted)?;
        if payload.invite_id != invite_id
            || payload.request_nonce != expected_request_nonce
            || !device_secrets.matches_record(&payload.device)
        {
            return Err(VaultError::PairingProofFailed);
        }
        Ok(SealedPairingRequest {
            invite_id,
            encrypted: encrypted.authenticated_after_verification(),
        })
    }

    /// Authenticate a locally staged request from its code proof and device signature.
    pub(crate) fn open_journaled_request(
        code: &PairingCode,
        encrypted: &EncryptedObject,
    ) -> Result<PairingRequestPayload, VaultError> {
        let invite_id = pairing_invite_id(code)?;
        let payload: PairingRequestPayload = open_pairing_json(code, encrypted)?;
        validate_request_proof(&payload, code, &invite_id)?;
        Ok(payload)
    }

    pub fn review_request(
        &self,
        sealed: &SealedPairingRequest,
        now_unix: i64,
    ) -> Result<PairingRequestPayload, VaultError> {
        self.ensure_available(now_unix)?;
        if sealed.invite_id != self.invite_id {
            return Err(VaultError::PairingProofFailed);
        }
        let payload: PairingRequestPayload = open_pairing_json(&self.code, &sealed.encrypted)?;
        validate_request(&payload, &self.code, &self.invite_id, now_unix)?;
        Ok(payload)
    }

    /// Reauthenticate the exact immutable request after the host has durably bound it.
    ///
    /// Expiry prevents a new approval from being committed. It does not make a membership change
    /// that is already visible on the signed checkpoint chain impossible to hand off after crash.
    pub(crate) fn review_bound_request(
        &self,
        sealed: &SealedPairingRequest,
    ) -> Result<PairingRequestPayload, VaultError> {
        if sealed.invite_id != self.invite_id {
            return Err(VaultError::PairingProofFailed);
        }
        let payload: PairingRequestPayload = open_pairing_json(&self.code, &sealed.encrypted)?;
        validate_request_proof(&payload, &self.code, &self.invite_id)?;
        Ok(payload)
    }

    pub fn approve(
        &self,
        sealed_request: &SealedPairingRequest,
        membership: MembershipChain,
        encrypted_checkpoint: &EncryptedObject,
        approver: &DeviceSecretMaterial,
        now_unix: i64,
    ) -> Result<SealedPairingApproval, VaultError> {
        let request = self.review_request(sealed_request, now_unix)?;
        self.approve_reviewed(request, membership, encrypted_checkpoint, approver)
    }

    /// Finish the authenticated handoff for a request whose exact device addition is already
    /// committed to the signed membership/checkpoint chain.
    pub(crate) fn approve_committed(
        &self,
        sealed_request: &SealedPairingRequest,
        membership: MembershipChain,
        encrypted_checkpoint: &EncryptedObject,
        approver: &DeviceSecretMaterial,
    ) -> Result<SealedPairingApproval, VaultError> {
        let request = self.review_bound_request(sealed_request)?;
        self.approve_reviewed(request, membership, encrypted_checkpoint, approver)
    }

    fn approve_reviewed(
        &self,
        request: PairingRequestPayload,
        membership: MembershipChain,
        encrypted_checkpoint: &EncryptedObject,
        approver: &DeviceSecretMaterial,
    ) -> Result<SealedPairingApproval, VaultError> {
        let verified = membership.verify(&MembershipAnchor::RootHash(
            self.membership_root_hash.clone(),
        ))?;
        if verified.dataset_id != self.dataset_id
            || membership_hash_position(&membership, &self.membership_starting_head_hash).is_none()
            || verified
                .devices
                .get(&request.device.device_id)
                .filter(|device| !device.revoked)
                != Some(&request.device)
        {
            return Err(VaultError::InvalidMembership);
        }
        let starting_position =
            membership_hash_position(&membership, &self.membership_starting_head_hash)
                .ok_or(VaultError::InvalidMembership)?;
        let add_position =
            device_add_position(&membership, &request.device.device_id, &request.device)
                .ok_or(VaultError::InvalidMembership)?;
        if add_position <= starting_position {
            return Err(VaultError::InvalidMembership);
        }
        let approver_id =
            DeviceId::new(approver.device_id()).map_err(|_| VaultError::InvalidDeviceId)?;
        let approver_record = verified
            .devices
            .get(&approver_id)
            .filter(|device| !device.revoked)
            .ok_or(VaultError::RevokedOrUnknownDevice)?;
        if !approver.matches_personal_record(approver_record) {
            return Err(VaultError::InvalidSigningKey);
        }
        let checkpoint = super::checkpoint::SignedCheckpoint::decrypt_for_device(
            encrypted_checkpoint,
            approver,
            &MembershipAnchor::RootHash(self.membership_root_hash.clone()),
        )?;
        if checkpoint.payload.membership != membership
            || checkpoint.payload.dataset_id != self.dataset_id
        {
            return Err(VaultError::InvalidMembership);
        }
        let checkpoint_sequence = checkpoint.payload.checkpoint_sequence;
        let checkpoint_ciphertext_hash = super::crypto::sha256_domain_hex(
            CHECKPOINT_CIPHERTEXT_HASH_DOMAIN,
            &[encrypted_checkpoint.as_bytes()],
        );
        let proof = PairingApprovalProof {
            kind: PAIRING_APPROVAL_KIND,
            schema_version: super::VAULT_SCHEMA_VERSION,
            invite_id: &self.invite_id,
            request_nonce: &request.request_nonce,
            dataset_id: &self.dataset_id,
            membership_root_hash: &verified.root_hash,
            membership_starting_head_hash: &self.membership_starting_head_hash,
            membership_head_hash: &verified.head_hash,
            approved_device_id: &request.device.device_id,
            approver_device_id: &approver_id,
            membership: &membership,
            checkpoint_sequence,
            checkpoint_ciphertext_hash: &checkpoint_ciphertext_hash,
            expires_at_unix: self.expires_at_unix,
        };
        let approver_signature =
            sign_serializable(APPROVAL_SIGNATURE_DOMAIN, approver.signing_key(), &proof)?;
        let payload = PairingApprovalPayload {
            kind: PAIRING_APPROVAL_KIND.to_owned(),
            schema_version: super::VAULT_SCHEMA_VERSION,
            invite_id: self.invite_id.clone(),
            request_nonce: request.request_nonce,
            dataset_id: self.dataset_id.clone(),
            membership_root_hash: verified.root_hash,
            membership_starting_head_hash: self.membership_starting_head_hash.clone(),
            membership_head_hash: verified.head_hash,
            approved_device_id: request.device.device_id,
            approver_device_id: approver_id,
            membership,
            checkpoint_sequence,
            checkpoint_ciphertext_hash,
            expires_at_unix: self.expires_at_unix,
            approver_signature,
        };
        let encrypted = seal_pairing_json(&self.code, &payload)?;
        Ok(SealedPairingApproval {
            invite_id: self.invite_id.clone(),
            encrypted,
        })
    }

    /// Consume the one-time invite only after its approval handoff is durably published.
    ///
    /// Approval construction is intentionally retryable: a failed PUT must not strand a device
    /// that has already been added to the encrypted checkpoint.
    pub fn finalize_approval(
        &self,
        sealed_request: &SealedPairingRequest,
        sealed_approval: &SealedPairingApproval,
        now_unix: i64,
    ) -> Result<(), VaultError> {
        self.ensure_available(now_unix)?;
        let request = self.review_request(sealed_request, now_unix)?;
        if sealed_approval.invite_id != self.invite_id {
            return Err(VaultError::PairingProofFailed);
        }
        let approval: PairingApprovalPayload =
            open_pairing_json(&self.code, &sealed_approval.encrypted)?;
        if approval.kind != PAIRING_APPROVAL_KIND
            || approval.schema_version != super::VAULT_SCHEMA_VERSION
            || approval.invite_id != self.invite_id
            || approval.request_nonce != request.request_nonce
            || approval.dataset_id != self.dataset_id
            || approval.membership_root_hash != self.membership_root_hash
            || approval.membership_starting_head_hash != self.membership_starting_head_hash
            || approval.approved_device_id != request.device.device_id
            || approval.expires_at_unix != self.expires_at_unix
        {
            return Err(VaultError::PairingProofFailed);
        }
        self.finalized
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| VaultError::PairingConsumed)?;
        Ok(())
    }

    fn ensure_available(&self, now_unix: i64) -> Result<(), VaultError> {
        if now_unix > self.expires_at_unix {
            return Err(VaultError::PairingExpired);
        }
        if self.finalized.load(Ordering::Acquire) {
            return Err(VaultError::PairingConsumed);
        }
        Ok(())
    }
}

impl ApprovedPairing {
    pub fn membership(&self) -> &MembershipChain {
        &self.membership
    }

    pub fn membership_root_hash(&self) -> &str {
        &self.membership_root_hash
    }

    pub fn membership_head_hash(&self) -> &str {
        &self.membership_head_hash
    }

    pub fn signed_checkpoint(&self) -> &super::checkpoint::SignedCheckpoint {
        &self.signed_checkpoint
    }

    pub fn encrypted_checkpoint(&self) -> &EncryptedObject {
        &self.encrypted_checkpoint
    }

    pub(crate) fn invite_id(&self) -> &str {
        &self.invite_id
    }

    pub(crate) fn request_nonce(&self) -> &str {
        &self.request_nonce
    }

    pub(crate) fn approved_device_id(&self) -> &DeviceId {
        &self.approved_device_id
    }

    pub fn open(
        code: &PairingCode,
        sealed: &SealedPairingApproval,
        expected_request_nonce: &str,
        device_secrets: &DeviceSecretMaterial,
        encrypted_checkpoint: &EncryptedObject,
        _now_unix: i64,
    ) -> Result<Self, VaultError> {
        if sealed.invite_id != pairing_invite_id(code)? {
            return Err(VaultError::PairingProofFailed);
        }
        let payload: PairingApprovalPayload = open_pairing_json(code, &sealed.encrypted)?;
        if payload.kind != PAIRING_APPROVAL_KIND
            || payload.schema_version != super::VAULT_SCHEMA_VERSION
            || payload.invite_id != sealed.invite_id
            || payload.request_nonce != expected_request_nonce
            || payload.checkpoint_sequence == 0
            || !valid_hash(&payload.checkpoint_ciphertext_hash)
            || payload.approved_device_id.as_str() != device_secrets.device_id()
            || !valid_hash(&payload.membership_root_hash)
            || !valid_hash(&payload.membership_starting_head_hash)
            || !valid_hash(&payload.membership_head_hash)
        {
            return Err(VaultError::PairingProofFailed);
        }
        // A host-signed approval is bound to the exact request nonce, device keys, membership
        // addition, and checkpoint ciphertext. Once that membership is committed, delaying the
        // immutable handoff past the ten-minute invite window cannot admit a different device.
        let verified = payload.membership.verify(&MembershipAnchor::RootHash(
            payload.membership_root_hash.clone(),
        ))?;
        if verified.dataset_id != payload.dataset_id
            || verified.head_hash != payload.membership_head_hash
            || membership_hash_position(&payload.membership, &payload.membership_starting_head_hash)
                .is_none()
            || !verified
                .devices
                .get(&payload.approved_device_id)
                .is_some_and(|record| device_secrets.matches_record(record) && !record.revoked)
        {
            return Err(VaultError::InvalidMembership);
        }
        let starting_position =
            membership_hash_position(&payload.membership, &payload.membership_starting_head_hash)
                .ok_or(VaultError::InvalidMembership)?;
        let approved_record = verified
            .devices
            .get(&payload.approved_device_id)
            .ok_or(VaultError::InvalidMembership)?;
        if device_add_position(
            &payload.membership,
            &payload.approved_device_id,
            approved_record,
        )
        .is_none_or(|position| position <= starting_position)
        {
            return Err(VaultError::InvalidMembership);
        }
        let approver_key = verified
            .devices
            .get(&payload.approver_device_id)
            .filter(|device| !device.revoked)
            .and_then(|device| device.public_identity.as_ref())
            .map(|identity| identity.ed25519_verifying_key.as_str())
            .ok_or(VaultError::RevokedOrUnknownDevice)?;
        let proof = PairingApprovalProof {
            kind: &payload.kind,
            schema_version: payload.schema_version,
            invite_id: &payload.invite_id,
            request_nonce: &payload.request_nonce,
            dataset_id: &payload.dataset_id,
            membership_root_hash: &payload.membership_root_hash,
            membership_starting_head_hash: &payload.membership_starting_head_hash,
            membership_head_hash: &payload.membership_head_hash,
            approved_device_id: &payload.approved_device_id,
            approver_device_id: &payload.approver_device_id,
            membership: &payload.membership,
            checkpoint_sequence: payload.checkpoint_sequence,
            checkpoint_ciphertext_hash: &payload.checkpoint_ciphertext_hash,
            expires_at_unix: payload.expires_at_unix,
        };
        verify_serializable(
            APPROVAL_SIGNATURE_DOMAIN,
            approver_key,
            &proof,
            &payload.approver_signature,
        )?;
        let checkpoint_hash = super::crypto::sha256_domain_hex(
            CHECKPOINT_CIPHERTEXT_HASH_DOMAIN,
            &[encrypted_checkpoint.as_bytes()],
        );
        if checkpoint_hash != payload.checkpoint_ciphertext_hash {
            return Err(VaultError::PairingProofFailed);
        }
        let checkpoint = super::checkpoint::SignedCheckpoint::decrypt_for_device(
            encrypted_checkpoint,
            device_secrets,
            &MembershipAnchor::RootHash(payload.membership_root_hash.clone()),
        )?;
        if checkpoint.payload.membership != payload.membership
            || checkpoint.payload.dataset_id != payload.dataset_id
            || checkpoint.payload.checkpoint_sequence != payload.checkpoint_sequence
        {
            return Err(VaultError::InvalidMembership);
        }
        Ok(Self {
            invite_id: payload.invite_id,
            request_nonce: payload.request_nonce,
            approved_device_id: payload.approved_device_id,
            membership: payload.membership,
            membership_root_hash: payload.membership_root_hash,
            membership_head_hash: payload.membership_head_hash,
            signed_checkpoint: checkpoint,
            encrypted_checkpoint: encrypted_checkpoint.clone(),
        })
    }
}

fn validate_request(
    payload: &PairingRequestPayload,
    code: &PairingCode,
    expected_invite_id: &str,
    now_unix: i64,
) -> Result<(), VaultError> {
    validate_request_proof(payload, code, expected_invite_id)?;
    if now_unix > payload.expires_at_unix {
        Err(VaultError::PairingExpired)
    } else {
        Ok(())
    }
}

fn validate_request_proof(
    payload: &PairingRequestPayload,
    code: &PairingCode,
    expected_invite_id: &str,
) -> Result<(), VaultError> {
    if payload.kind != PAIRING_REQUEST_KIND
        || payload.schema_version != super::VAULT_SCHEMA_VERSION
        || payload.invite_id != expected_invite_id
        || payload.expires_at_unix
            != payload
                .issued_at_unix
                .checked_add(PAIRING_LIFETIME_SECS)
                .unwrap_or(i64::MAX)
        || !valid_nonce(&payload.request_nonce)
    {
        return Err(VaultError::PairingProofFailed);
    }
    validate_device(&payload.device)?;
    let proof = PairingRequestProof {
        kind: &payload.kind,
        schema_version: payload.schema_version,
        invite_id: &payload.invite_id,
        request_nonce: &payload.request_nonce,
        device: &payload.device,
        issued_at_unix: payload.issued_at_unix,
        expires_at_unix: payload.expires_at_unix,
    };
    let challenge = serde_json::to_vec(&proof).map_err(|_| VaultError::SerializationFailed)?;
    verify_pairing_proof(code, &challenge, &payload.pairing_proof)?;
    let verifying_key = payload
        .device
        .public_identity
        .as_ref()
        .ok_or(VaultError::InvalidDeviceIdentity)?
        .ed25519_verifying_key
        .as_str();
    verify_serializable(
        REQUEST_SIGNATURE_DOMAIN,
        verifying_key,
        &(&proof, &payload.pairing_proof),
        &payload.device_signature,
    )?;
    Ok(())
}

fn validate_device(device: &DeviceRecord) -> Result<(), VaultError> {
    if device.revoked
        || device.name.chars().count() > 1_024
        || device.name.chars().any(char::is_control)
        || super::crypto::validate_device_id(device.device_id.as_str()).is_err()
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

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn pairing_invite_id(code: &PairingCode) -> Result<String, VaultError> {
    let bytes =
        base32_visual_decode(code.expose_secret()).map_err(|_| VaultError::InvalidPairingCode)?;
    Ok(super::crypto::sha256_domain_hex(INVITE_ID_DOMAIN, &[&bytes])[..32].to_owned())
}

fn membership_hash_position(membership: &MembershipChain, expected_hash: &str) -> Option<usize> {
    if membership.root.hash().ok().as_deref() == Some(expected_hash) {
        return Some(0);
    }
    membership
        .changes
        .iter()
        .enumerate()
        .find_map(|(index, change)| {
            (change.hash().ok().as_deref() == Some(expected_hash)).then_some(index + 1)
        })
}

fn device_add_position(
    membership: &MembershipChain,
    device_id: &DeviceId,
    expected_record: &DeviceRecord,
) -> Option<usize> {
    if &membership.root.payload.initial_device == expected_record
        && &membership.root.payload.initial_device.device_id == device_id
    {
        return Some(0);
    }
    membership
        .changes
        .iter()
        .enumerate()
        .find_map(|(index, change)| match &change.payload.action {
            MembershipAction::AddDevice { device }
                if device == expected_record && &device.device_id == device_id =>
            {
                Some(index + 1)
            }
            _ => None,
        })
}
