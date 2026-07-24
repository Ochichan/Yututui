mod durable;
#[path = "pairing/join_durable.rs"]
mod join_durable;

use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::personal_state::{
    DeviceId, ImportSummary, Operation, PersonalStateCommit, PersonalStateV2, append_operation_as,
    plan_join_import,
};

use super::super::manual::{ManualSyncEngine, ManualSyncInput};
use super::super::{
    DeviceSecretMaterial, EncryptedObject, EnrollmentState, MAX_VAULT_PAYLOAD_BYTES,
    MembershipAction, ObjectCondition, ObjectKey, PairingCode, PairingInvite,
    PairingRequestPayload, PrivateStore, PrivateStoreSnapshot, SealedPairingApproval,
    SealedPairingRequest, SyncAuditAction, SyncAuditEntry, SyncAuditOutcome, SyncAuditStore,
    SyncHealthStore, SyncPaths, VaultCredential, VaultError, VaultTransport, WebDavProfile,
    WebDavProfileStore,
};
use super::manual::{
    PreparedManualSync, load_remote_membership, membership_anchor, membership_prefix_for_registry,
    prepare_manual_sync_with_transport, validate_active_context,
};
use super::transition::{AnchorActivationKind, commit_with_anchor_transition};
use super::{SyncServiceError, apply_manual_sync, open_saved_webdav_transport};
use durable::{HostPairingSnapshot, HostPairingStore};
use join_durable::{JoinPairingSnapshot, JoinPairingStore};

const LOCATOR_KIND: &str = "yututui_pairing_locator";
const PAIRING_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_PAIRING_WAIT_SECONDS: i64 = 10 * 60;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingLocator {
    kind: String,
    schema_version: u32,
    invite_id: String,
    dataset_id: String,
    expires_at_unix: i64,
}

/// Existing-device pairing state. Debug/serde are intentionally absent because it owns the code.
pub struct PairingHostInvite {
    invite: PairingInvite,
    durable: HostPairingSnapshot,
    resumed: bool,
}

impl PairingHostInvite {
    pub fn code(&self) -> &str {
        self.invite.code().expose_secret()
    }

    pub fn expires_at_unix(&self) -> i64 {
        self.invite.expires_at_unix()
    }

    pub fn resumed(&self) -> bool {
        self.resumed
    }
}

pub struct PairingReview {
    pub device_id: String,
    pub device_name: String,
    pub fingerprint: String,
    sealed: SealedPairingRequest,
    payload: PairingRequestPayload,
}

pub struct PairingJoinPreview {
    pub summary: ImportSummary,
    pub device_id: String,
    candidate: PersonalStateV2,
    checkpoint: super::super::SignedCheckpoint,
    expected_local_revision: u64,
    expected_private_revision: u64,
}

pub fn create_pairing_invite(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    now_unix: i64,
) -> Result<PairingHostInvite, SyncServiceError> {
    let private_store = PrivateStore::new(paths.private_store())?;
    let private = private_store.load()?;
    let profile = WebDavProfileStore::new(paths.profile())?.load(private.device())?;
    validate_active_context(current_state, &private, &profile)?;
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open_saved_webdav_transport(&profile, credential)?;
    let host_store = HostPairingStore::new(paths);
    if let Some(durable) = host_store.load(private.device())? {
        if durable.dataset_id() != current_state.dataset_id
            || durable.host_device_id() != private.device_id()
        {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        if now_unix > durable.invite()?.expires_at_unix()
            && !durable.has_bound_request()
            && !durable.has_handoff()
        {
            host_store.remove()?;
        } else {
            let invite = durable.invite()?;
            let locator = host_store.locator(&durable)?;
            put_immutable_or_verify(
                &transport,
                &global_pairing_key(invite.invite_id(), "locator.age")?,
                &locator,
            )?;
            return Ok(PairingHostInvite {
                invite,
                durable,
                resumed: true,
            });
        }
    }
    let anchor = membership_anchor(&private)?;
    let remote = load_remote_membership(current_state, &private, &anchor, &transport)?;
    let local = membership_prefix_for_registry(&remote, &anchor, &current_state.device_registry)?;
    let verified = local.verify(&anchor)?;
    let invite = PairingInvite::create(
        current_state.dataset_id.clone(),
        verified.root_hash,
        verified.head_hash,
        now_unix,
    )?;
    let locator = PairingLocator {
        kind: LOCATOR_KIND.to_owned(),
        schema_version: super::super::VAULT_SCHEMA_VERSION,
        invite_id: invite.invite_id().to_owned(),
        dataset_id: current_state.dataset_id.clone(),
        expires_at_unix: invite.expires_at_unix(),
    };
    let encrypted = super::super::crypto::seal_pairing_json(invite.code(), &locator)?;
    let durable = host_store.create(
        private.device(),
        &invite,
        current_state.revision,
        private.revision(),
        &encrypted,
    )?;
    put_immutable_or_verify(
        &transport,
        &global_pairing_key(invite.invite_id(), "locator.age")?,
        &encrypted,
    )?;
    Ok(PairingHostInvite {
        invite,
        durable,
        resumed: false,
    })
}

pub fn poll_pairing_request(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    host: &mut PairingHostInvite,
    now_unix: i64,
) -> Result<Option<PairingReview>, SyncServiceError> {
    let private_store = PrivateStore::new(paths.private_store())?;
    let private = private_store.load()?;
    validate_host_identity(current_state, paths, &private, host)?;
    let profile = WebDavProfileStore::new(paths.profile())?.load(private.device())?;
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open_saved_webdav_transport(&profile, credential)?;
    let host_store = HostPairingStore::new(paths);
    let key = dataset_pairing_key(
        &current_state.dataset_id,
        host.invite.invite_id(),
        "request.age",
    )?;
    let encrypted = if host.durable.has_bound_request() {
        let durable_request = host_store.request(&host.durable)?;
        if let Some((remote_request, _)) = transport.get(&key, MAX_VAULT_PAYLOAD_BYTES)?
            && remote_request.as_bytes() != durable_request.as_bytes()
        {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        durable_request
    } else {
        if now_unix > host.expires_at_unix() {
            host_store.remove()?;
            return Err(SyncServiceError::PairingExpired);
        }
        let Some((remote_request, _)) = transport.get(&key, MAX_VAULT_PAYLOAD_BYTES)? else {
            return Ok(None);
        };
        remote_request
    };
    let sealed = SealedPairingRequest {
        invite_id: host.invite.invite_id().to_owned(),
        encrypted,
    };
    let payload = if host.durable.has_bound_request() {
        let payload = host.invite.review_bound_request(&sealed)?;
        if !host_store.request_matches(&host.durable, &sealed.encrypted, &payload.device.device_id)
        {
            return Err(SyncServiceError::PairingRejected);
        }
        payload
    } else {
        let payload = host.invite.review_request(&sealed, now_unix)?;
        host_store.bind_request(
            private.device(),
            &mut host.durable,
            &sealed.encrypted,
            &payload.device.device_id,
        )?;
        payload
    };
    let identity = payload
        .device
        .public_identity
        .as_ref()
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    let fingerprint = super::super::crypto::sha256_domain_hex(
        b"yututui-pairing-review-fingerprint-v1",
        &[
            identity.age_recipient.as_bytes(),
            identity.ed25519_verifying_key.as_bytes(),
        ],
    );
    Ok(Some(PairingReview {
        device_id: payload.device.device_id.as_str().to_owned(),
        device_name: payload.device.name.clone(),
        fingerprint: fingerprint[..16].to_owned(),
        sealed,
        payload,
    }))
}

#[allow(clippy::too_many_arguments)]
pub fn approve_pairing_request(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    paths: &SyncPaths,
    host: &mut PairingHostInvite,
    review: PairingReview,
    _now_unix: i64,
) -> Result<PersonalStateV2, SyncServiceError> {
    let private_store = PrivateStore::new(paths.private_store())?;
    let private = private_store.load()?;
    validate_host_identity(current_state, paths, &private, host)?;
    let profile = WebDavProfileStore::new(paths.profile())?.load(private.device())?;
    validate_active_context(current_state, &private, &profile)?;
    let authenticated_request = host.invite.review_bound_request(&review.sealed)?;
    if authenticated_request != review.payload {
        return Err(SyncServiceError::PairingRejected);
    }
    let host_store = HostPairingStore::new(paths);
    host_store.bind_request(
        private.device(),
        &mut host.durable,
        &review.sealed.encrypted,
        &review.payload.device.device_id,
    )?;
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open_saved_webdav_transport(&profile, credential)?;
    let anchor = membership_anchor(&private)?;
    let mut prepared = prepare_manual_sync_with_transport(current_state, &private, &transport)?;
    if !pairing_device_is_active(&prepared, &review.payload.device) && !host.durable.has_handoff() {
        if crate::signals::unix_now() > host.expires_at_unix() {
            host_store.remove()?;
            return Err(SyncServiceError::PairingExpired);
        }
        prepared = match commit_pairing_membership(
            current_state,
            &private,
            &anchor,
            &transport,
            prepared,
            &review.payload.device,
            host.expires_at_unix(),
        ) {
            Ok(prepared) => prepared,
            Err(SyncServiceError::PairingExpired) => {
                let detected =
                    prepare_manual_sync_with_transport(current_state, &private, &transport)?;
                if !pairing_device_is_active(&detected, &review.payload.device) {
                    host_store.remove()?;
                    return Err(SyncServiceError::PairingExpired);
                }
                detected
            }
            Err(error) => return Err(error),
        };
    }
    if !pairing_device_is_active(&prepared, &review.payload.device) {
        return Err(SyncServiceError::InvalidRemoteData);
    }

    if !host.durable.has_handoff() {
        let (encrypted_checkpoint, checkpoint_hash, membership_head_hash) =
            load_prepared_checkpoint(current_state, &anchor, &transport, &prepared)?;
        let approval = host.invite.approve_committed(
            &review.sealed,
            prepared.membership.clone(),
            &encrypted_checkpoint,
            private.device(),
        )?;
        host_store.prepare_handoff(
            private.device(),
            &mut host.durable,
            membership_head_hash,
            checkpoint_hash,
            &encrypted_checkpoint,
            &approval.encrypted,
        )?;
    }
    let handoff = host_store.load_handoff(&host.durable)?;
    publish_pairing_handoff(
        current_state,
        host.invite.invite_id(),
        &transport,
        &handoff.checkpoint,
        &handoff.approval,
    )?;

    let summary = prepared.summary.clone();
    // Publish the complete, authenticated join handoff before installing the owner's local
    // projection. If the local commit then fails, both devices can recover from the already
    // published checkpoint; the reverse order could strand an added device with no approval.
    let installed = apply_manual_sync(
        current_state,
        playlist_revision,
        prepared,
        personal_paths,
        paths,
    )?;
    let _ = record_pairing_audit(
        paths,
        crate::signals::unix_now(),
        SyncAuditAction::PairCreate,
        Some(review.device_id),
        summary.uploaded_operations,
        summary.downloaded_operations,
    );
    host_store.remove()?;
    Ok(installed)
}

fn pairing_device_is_active(
    prepared: &PreparedManualSync,
    device: &crate::personal_state::DeviceRecord,
) -> bool {
    prepared.state.device_registry.get(&device.device_id) == Some(device)
}

fn commit_pairing_membership<T: VaultTransport + ?Sized>(
    original_state: &PersonalStateV2,
    private: &PrivateStoreSnapshot,
    anchor: &super::super::MembershipAnchor,
    transport: &T,
    synced: PreparedManualSync,
    joining_device: &crate::personal_state::DeviceRecord,
    expires_at_unix: i64,
) -> Result<PreparedManualSync, SyncServiceError> {
    let PreparedManualSync {
        state: synced_state,
        mut membership,
        checkpoint_anchor,
        summary: first_summary,
        ..
    } = synced;
    let local_device =
        DeviceId::new(private.device_id()).map_err(|_| SyncServiceError::InvalidRemoteData)?;
    membership.append_device_action(
        anchor,
        &local_device,
        private.device().signing_key(),
        MembershipAction::AddDevice {
            device: joining_device.clone(),
        },
    )?;
    let state = append_operation_as(
        &synced_state,
        &local_device,
        Operation::AddDevice {
            device: joining_device.clone(),
        },
        crate::signals::unix_now(),
    )?;
    let input = ManualSyncInput {
        local_state: &state,
        membership: &membership,
        membership_anchor: anchor,
        device: private.device(),
        checkpoint_anchor: &checkpoint_anchor,
        bootstrap_checkpoint: None,
        expected_local_revision: state.revision,
    };
    let candidate = ManualSyncEngine::new(transport).synchronize(&input, &|expected| {
        if expected != state.revision {
            return Err(VaultError::RevisionConflict);
        }
        if crate::signals::unix_now() > expires_at_unix {
            return Err(VaultError::PairingExpired);
        }
        Ok(())
    })?;
    let mut summary = first_summary;
    merge_sync_summary(&mut summary, &candidate.summary);
    Ok(PreparedManualSync {
        state: candidate.state,
        membership: candidate.membership,
        checkpoint_anchor: candidate.checkpoint_anchor,
        expected_local_revision: original_state.revision,
        expected_private_revision: private.revision(),
        local_device_id: local_device,
        summary,
    })
}

fn load_prepared_checkpoint<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    anchor: &super::super::MembershipAnchor,
    transport: &T,
    prepared: &PreparedManualSync,
) -> Result<(EncryptedObject, String, String), SyncServiceError> {
    let checkpoint_hash = prepared
        .checkpoint_anchor
        .checkpoint_hash
        .clone()
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    let verified_membership = prepared.membership.verify(anchor)?;
    let checkpoint_key = super::super::manual::checkpoint_key(
        &current_state.dataset_id,
        verified_membership.epoch,
        &checkpoint_hash,
    )?;
    let (encrypted_checkpoint, _) = transport
        .get(&checkpoint_key, MAX_VAULT_PAYLOAD_BYTES)?
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    Ok((
        encrypted_checkpoint,
        checkpoint_hash,
        verified_membership.head_hash,
    ))
}

fn publish_pairing_handoff<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    invite_id: &str,
    transport: &T,
    checkpoint: &EncryptedObject,
    approval: &EncryptedObject,
) -> Result<(), SyncServiceError> {
    put_immutable_or_verify(
        transport,
        &dataset_pairing_key(&current_state.dataset_id, invite_id, "checkpoint.age")?,
        checkpoint,
    )?;
    put_immutable_or_verify(
        transport,
        &dataset_pairing_key(&current_state.dataset_id, invite_id, "approval.age")?,
        approval,
    )
}

fn merge_sync_summary(
    total: &mut super::super::manual::ManualSyncSummary,
    next: &super::super::manual::ManualSyncSummary,
) {
    total.attempts = total.attempts.saturating_add(next.attempts);
    total.downloaded_operations = total
        .downloaded_operations
        .saturating_add(next.downloaded_operations);
    total.uploaded_operations = total
        .uploaded_operations
        .saturating_add(next.uploaded_operations);
    total.downloaded_segments = total
        .downloaded_segments
        .saturating_add(next.downloaded_segments);
    total.uploaded_segments = total
        .uploaded_segments
        .saturating_add(next.uploaded_segments);
    total.checkpoint_written |= next.checkpoint_written;
    total.manifest_written |= next.manifest_written;
    total.remote_writes = total.remote_writes.saturating_add(next.remote_writes);
}

#[allow(clippy::too_many_arguments)]
pub fn begin_pairing_join(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    endpoint: String,
    custom_ca_pem: Option<Vec<u8>>,
    credential: VaultCredential,
    code: &str,
    device_name: String,
    now_unix: i64,
) -> Result<PairingJoinPreview, SyncServiceError> {
    let code = PairingCode::parse(code)?;
    let invite_id = PairingInvite::invite_id_for_code(&code)?;
    let probe =
        super::setup::checked_webdav_transport(&endpoint, custom_ca_pem.as_deref(), &credential)?;
    let locator_key = global_pairing_key(&invite_id, "locator.age")?;
    let (encrypted_locator, _) = probe
        .get(&locator_key, MAX_VAULT_PAYLOAD_BYTES)?
        .ok_or(SyncServiceError::PairingRejected)?;
    let locator: PairingLocator =
        super::super::crypto::open_pairing_json(&code, &encrypted_locator)?;
    validate_locator(&locator, &invite_id, now_unix)?;

    let private_store = PrivateStore::new(paths.private_store())?;
    let profile_store = WebDavProfileStore::new(paths.profile())?;
    let join_store = JoinPairingStore::new(paths);
    let existing_private = regular_file_exists(paths.private_store())?;
    let existing_profile = regular_file_exists(paths.profile())?;
    if !existing_private {
        if let Some((journal, record)) = join_store.load_authenticated()? {
            if journal.dataset_id() != locator.dataset_id
                || journal.invite_id() != invite_id
                || journal.expires_at_unix() != locator.expires_at_unix
                || journal.device_id() != record.device_id.as_str()
            {
                return Err(SyncServiceError::InvalidRemoteData);
            }
            if existing_profile {
                let profile = profile_store.load_for_pairing_record(&record)?;
                if profile.dataset_id() != journal.dataset_id()
                    || profile.device_id() != journal.device_id()
                {
                    return Err(SyncServiceError::InvalidRemoteData);
                }
            }
            let request = join_store.request(&journal)?;
            let request_key =
                dataset_pairing_key(journal.dataset_id(), journal.invite_id(), "request.age")?;
            if let Some((remote, _)) = probe.get(&request_key, MAX_VAULT_PAYLOAD_BYTES)?
                && remote.as_bytes() != request.as_bytes()
            {
                return Err(SyncServiceError::InvalidRemoteData);
            }
            return Err(SyncServiceError::PairingNeedsCleanup);
        }
        if existing_profile {
            return Err(SyncServiceError::PairingNeedsCleanup);
        }
        // A request without its signed journal, private keys, or profile cannot recover or
        // authorize anything. Discarding this local ciphertext does not change remote state.
        crate::util::safe_fs::remove_owner_only_file_durable(paths.pending_join_request())
            .map_err(|_| SyncServiceError::Storage)?;
        crate::util::safe_fs::remove_owner_only_file_durable(paths.pending_join_checkpoint())
            .map_err(|_| SyncServiceError::Storage)?;
    }
    drop(probe);

    let (mut private, profile, sealed_request, request_nonce, device_id) = if existing_private {
        let private = private_store.load()?;
        let expected_profile = WebDavProfile::with_custom_ca(
            locator.dataset_id.clone(),
            private.device(),
            &endpoint,
            custom_ca_pem.as_deref(),
        )?;
        let pending = private
            .pending_pairing()?
            .ok_or(SyncServiceError::AlreadyConfigured)?;
        if !matches!(
            private.enrollment(),
            EnrollmentState::PendingApproval | EnrollmentState::PendingLedgerCommit
        ) || private.dataset_id() != locator.dataset_id
            || pending.invite_id() != invite_id
        {
            return Err(SyncServiceError::AlreadyConfigured);
        }
        let request = if let Some(journal) = join_store.load(private.device())? {
            if !journal.matches_context(
                private.dataset_id(),
                private.device_id(),
                pending.invite_id(),
                pending.request_nonce(),
            ) || journal.invite_id() != invite_id
                || journal.expires_at_unix() != locator.expires_at_unix
            {
                return Err(SyncServiceError::InvalidRemoteData);
            }
            join_store.request(&journal)?
        } else {
            let request_bytes = crate::util::safe_fs::read_owner_only_limited(
                paths.pending_join_request(),
                MAX_VAULT_PAYLOAD_BYTES as u64,
            )
            .map_err(|_| SyncServiceError::PairingNeedsCleanup)?;
            let request = EncryptedObject::from_bytes(request_bytes)?;
            join_store.create(
                private.device(),
                &code,
                private.dataset_id(),
                locator.expires_at_unix,
                pending.request_nonce(),
                &request,
            )?;
            request
        };
        let sealed_request = PairingInvite::resume_request(
            &code,
            request,
            pending.request_nonce(),
            private.device(),
            now_unix,
        )?;
        let profile = load_or_create_join_profile(
            &profile_store,
            existing_profile,
            &private,
            expected_profile,
        )?;
        let request_nonce = pending.request_nonce().to_owned();
        let device_id =
            DeviceId::new(private.device_id()).map_err(|_| SyncServiceError::PairingRejected)?;
        (private, profile, sealed_request, request_nonce, device_id)
    } else {
        let device_id = DeviceId::new(format!(
            "dev-{}",
            super::super::crypto::random_id_hex::<16>()?
        ))
        .map_err(|_| SyncServiceError::PairingRejected)?;
        let device = DeviceSecretMaterial::generate_for(device_id.as_str())?;
        let (sealed_request, request_nonce) =
            PairingInvite::create_request(&code, device_name, &device, now_unix)?;
        let mut private =
            PrivateStoreSnapshot::pending_approval(locator.dataset_id.clone(), device)?;
        private.set_pending_pairing(invite_id.clone(), request_nonce.clone())?;
        private.set_credential(credential);
        let mut profile = WebDavProfile::with_custom_ca(
            locator.dataset_id.clone(),
            private.device(),
            &endpoint,
            custom_ca_pem.as_deref(),
        )?;
        join_store.create(
            private.device(),
            &code,
            &locator.dataset_id,
            locator.expires_at_unix,
            &request_nonce,
            &sealed_request.encrypted,
        )?;
        if let Err(error) = private_store.create(&mut private) {
            let _ = join_store.remove_state();
            let _ =
                crate::util::safe_fs::remove_owner_only_file_durable(paths.pending_join_request());
            return Err(error.into());
        }
        if let Err(error) = profile_store.create(&mut profile, private.device()) {
            // Leave the signed pending journal and private keys intact. A retry with the same code
            // can reconstruct the missing profile without minting a second device identity.
            return Err(error.into());
        }
        (private, profile, sealed_request, request_nonce, device_id)
    };
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open_saved_webdav_transport(&profile, credential)?;
    (|| {
        let request_key = dataset_pairing_key(
            &locator.dataset_id,
            &sealed_request.invite_id,
            "request.age",
        )?;
        put_immutable_or_verify(&transport, &request_key, &sealed_request.encrypted)?;

        let wait_until = locator
            .expires_at_unix
            .min(now_unix.saturating_add(MAX_PAIRING_WAIT_SECONDS));
        let approval_key = dataset_pairing_key(
            &locator.dataset_id,
            &sealed_request.invite_id,
            "approval.age",
        )?;
        let encrypted_approval = poll_until_present(&transport, &approval_key, wait_until)?;
        let checkpoint_key = dataset_pairing_key(
            &locator.dataset_id,
            &sealed_request.invite_id,
            "checkpoint.age",
        )?;
        let encrypted_checkpoint = poll_until_present(&transport, &checkpoint_key, wait_until)?;
        let sealed_approval = SealedPairingApproval {
            invite_id: sealed_request.invite_id,
            encrypted: encrypted_approval,
        };
        let approved = super::super::ApprovedPairing::open(
            &code,
            &sealed_approval,
            &request_nonce,
            private.device(),
            &encrypted_checkpoint,
            crate::signals::unix_now(),
        )?;
        match private.enrollment() {
            EnrollmentState::PendingApproval => {
                private.approve(&approved)?;
                private_store.save(&mut private)?;
            }
            EnrollmentState::PendingLedgerCommit => {
                private.validate_approved_pairing(&approved)?;
            }
            EnrollmentState::Active | EnrollmentState::Revoked => {
                return Err(SyncServiceError::AlreadyConfigured);
            }
        }
        // Trust anchors must become durable before the code-independent checkpoint artifact.
        // If the following write is interrupted, `resume_pairing_join` authenticates and restores
        // the exact immutable checkpoint from WebDAV using those anchors and the retained invite.
        persist_join_checkpoint(paths, approved.encrypted_checkpoint())?;
        let plan = plan_join_import(
            &approved.signed_checkpoint().payload.state,
            current_state,
            &device_id,
        )?;
        Ok(PairingJoinPreview {
            summary: plan.summary,
            device_id: device_id.as_str().to_owned(),
            candidate: plan.candidate,
            checkpoint: approved.signed_checkpoint().clone(),
            expected_local_revision: current_state.revision,
            expected_private_revision: private.revision(),
        })
    })()
}

fn load_or_create_join_profile(
    profile_store: &WebDavProfileStore,
    exists: bool,
    private: &PrivateStoreSnapshot,
    mut expected: WebDavProfile,
) -> Result<WebDavProfile, SyncServiceError> {
    if !exists {
        profile_store.create(&mut expected, private.device())?;
        return Ok(expected);
    }
    let profile = profile_store
        .load(private.device())
        .map_err(|_| SyncServiceError::PairingNeedsCleanup)?;
    if profile.dataset_id() != private.dataset_id()
        || profile.device_id() != private.device_id()
        || profile.endpoint() != expected.endpoint()
        || profile.custom_ca_pem() != expected.custom_ca_pem()
    {
        return Err(SyncServiceError::PairingNeedsCleanup);
    }
    Ok(profile)
}

/// Rebuild the deletion-free merge preview from the exact authenticated checkpoint saved before
/// the private store entered `PendingLedgerCommit`.
pub fn resume_pairing_join(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
) -> Result<PairingJoinPreview, SyncServiceError> {
    if !regular_file_exists(paths.private_store())? {
        let join_store = JoinPairingStore::new(paths);
        if join_store.load_authenticated()?.is_some()
            || regular_file_exists(paths.profile())?
            || regular_file_exists(paths.pending_join_request())?
        {
            return Err(SyncServiceError::PairingNeedsCleanup);
        }
        return Err(SyncServiceError::NotConfigured);
    }
    let private_store = PrivateStore::new(paths.private_store())?;
    let mut private = private_store.load()?;
    if matches!(
        private.enrollment(),
        EnrollmentState::Active | EnrollmentState::Revoked
    ) {
        return Err(SyncServiceError::AlreadyConfigured);
    }
    let profile = WebDavProfileStore::new(paths.profile())?
        .load(private.device())
        .map_err(|_| SyncServiceError::PairingNeedsCleanup)?;
    if profile.dataset_id() != private.dataset_id() || profile.device_id() != private.device_id() {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    if private.enrollment() == EnrollmentState::PendingApproval {
        let pending = private
            .pending_pairing()?
            .ok_or(SyncServiceError::PairingNeedsCleanup)?;
        let journal = JoinPairingStore::new(paths)
            .load(private.device())?
            .ok_or(SyncServiceError::PairingNeedsCleanup)?;
        if !journal.matches_context(
            private.dataset_id(),
            private.device_id(),
            pending.invite_id(),
            pending.request_nonce(),
        ) {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        let credential = private
            .credential()
            .ok_or(SyncServiceError::MissingCredential)?;
        let transport = open_saved_webdav_transport(&profile, credential)?;
        return resume_pending_approval_with_transport(
            current_state,
            paths,
            &private_store,
            &mut private,
            &journal,
            &transport,
            crate::signals::unix_now(),
        );
    }
    if private.enrollment() != EnrollmentState::PendingLedgerCommit {
        return Err(SyncServiceError::AlreadyConfigured);
    }
    let pending = private
        .pending_pairing()?
        .ok_or(SyncServiceError::PendingApproval)?;
    let (encrypted_checkpoint, fetched) =
        load_or_fetch_join_checkpoint(paths, &private, &profile, pending.invite_id())?;
    plan_resumed_join(
        current_state,
        paths,
        &private,
        encrypted_checkpoint,
        fetched,
    )
}

fn resume_pending_approval_with_transport<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    private_store: &PrivateStore,
    private: &mut PrivateStoreSnapshot,
    journal: &JoinPairingSnapshot,
    transport: &T,
    now_unix: i64,
) -> Result<PairingJoinPreview, SyncServiceError> {
    let join_store = JoinPairingStore::new(paths);
    let request = PairingInvite::resume_journaled_request(
        journal.code(),
        join_store.request(journal)?,
        journal.request_nonce(),
        private.device(),
    )?;
    let request_key =
        dataset_pairing_key(journal.dataset_id(), journal.invite_id(), "request.age")?;
    match transport.get(&request_key, MAX_VAULT_PAYLOAD_BYTES)? {
        Some((remote, _)) if remote.as_bytes() != request.encrypted.as_bytes() => {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        Some(_) => {}
        None if now_unix <= journal.expires_at_unix() => {
            put_immutable_or_verify(transport, &request_key, &request.encrypted)?;
        }
        None => {}
    }
    let approval_key =
        dataset_pairing_key(journal.dataset_id(), journal.invite_id(), "approval.age")?;
    let checkpoint_key =
        dataset_pairing_key(journal.dataset_id(), journal.invite_id(), "checkpoint.age")?;
    let approval = transport.get(&approval_key, MAX_VAULT_PAYLOAD_BYTES)?;
    let checkpoint = transport.get(&checkpoint_key, MAX_VAULT_PAYLOAD_BYTES)?;
    let (Some((encrypted_approval, _)), Some((encrypted_checkpoint, _))) = (approval, checkpoint)
    else {
        return Err(SyncServiceError::PendingApproval);
    };
    let approved = super::super::ApprovedPairing::open(
        journal.code(),
        &SealedPairingApproval {
            invite_id: journal.invite_id().to_owned(),
            encrypted: encrypted_approval,
        },
        journal.request_nonce(),
        private.device(),
        &encrypted_checkpoint,
        now_unix,
    )?;
    private.approve(&approved)?;
    private_store.save(private)?;
    persist_join_checkpoint(paths, approved.encrypted_checkpoint())?;
    let device_id =
        DeviceId::new(private.device_id()).map_err(|_| SyncServiceError::InvalidRemoteData)?;
    let plan = plan_join_import(
        &approved.signed_checkpoint().payload.state,
        current_state,
        &device_id,
    )?;
    Ok(PairingJoinPreview {
        summary: plan.summary,
        device_id: device_id.as_str().to_owned(),
        candidate: plan.candidate,
        checkpoint: approved.signed_checkpoint().clone(),
        expected_local_revision: current_state.revision,
        expected_private_revision: private.revision(),
    })
}

fn plan_resumed_join(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    private: &PrivateStoreSnapshot,
    encrypted_checkpoint: EncryptedObject,
    persist_after_validation: bool,
) -> Result<PairingJoinPreview, SyncServiceError> {
    let membership_root_hash = private
        .membership_root_hash()
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    let anchor = super::super::MembershipAnchor::RootHash(membership_root_hash.to_owned());
    let checkpoint = super::super::SignedCheckpoint::decrypt_for_device(
        &encrypted_checkpoint,
        private.device(),
        &anchor,
    )?;
    let verified = checkpoint.verify(&anchor)?;
    let checkpoint_hash = checkpoint.hash()?;
    if private.pending_checkpoint_sequence() != Some(checkpoint.payload.checkpoint_sequence)
        || private.pending_checkpoint_hash() != Some(checkpoint_hash.as_str())
        || private.pending_membership_head_hash() != Some(verified.head_hash.as_str())
        || checkpoint.payload.dataset_id != private.dataset_id()
        || !verified
            .devices
            .get(
                &DeviceId::new(private.device_id())
                    .map_err(|_| SyncServiceError::InvalidRemoteData)?,
            )
            .is_some_and(|record| {
                private.device().matches_personal_record(record) && !record.revoked
            })
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    if persist_after_validation {
        persist_join_checkpoint(paths, &encrypted_checkpoint)?;
    }
    let device_id =
        DeviceId::new(private.device_id()).map_err(|_| SyncServiceError::InvalidRemoteData)?;
    let plan = plan_join_import(&checkpoint.payload.state, current_state, &device_id)?;
    Ok(PairingJoinPreview {
        summary: plan.summary,
        device_id: device_id.as_str().to_owned(),
        candidate: plan.candidate,
        checkpoint,
        expected_local_revision: current_state.revision,
        expected_private_revision: private.revision(),
    })
}

pub fn apply_pairing_join(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    paths: &SyncPaths,
    preview: PairingJoinPreview,
) -> Result<PersonalStateV2, SyncServiceError> {
    if current_state.revision != preview.expected_local_revision {
        return Err(SyncServiceError::LocalStateChanged);
    }
    let private_store = PrivateStore::new(paths.private_store())?;
    let mut private = private_store.load()?;
    if private.revision() != preview.expected_private_revision
        || private.enrollment() != EnrollmentState::PendingLedgerCommit
        || private.device_id() != preview.device_id
    {
        return Err(SyncServiceError::LocalStateChanged);
    }
    let commit = PersonalStateCommit::prepare_for_runtime(preview.candidate, playlist_revision)?;
    private.mark_active_after_join(&preview.checkpoint, commit.state())?;
    let checkpoint_sequence = preview.checkpoint.payload.checkpoint_sequence;
    let checkpoint_hash = preview.checkpoint.hash()?;
    let installed = commit_with_anchor_transition(
        current_state,
        &commit,
        personal_paths,
        paths,
        &mut private,
        AnchorActivationKind::PairJoin,
        checkpoint_sequence,
        &checkpoint_hash,
        playlist_revision,
    )?;
    let _ = crate::util::safe_fs::remove_owner_only_file_durable(paths.pending_join_checkpoint());
    let _ = JoinPairingStore::new(paths).remove_state();
    let _ = crate::util::safe_fs::remove_owner_only_file_durable(paths.pending_join_request());
    let _ = record_pairing_audit(
        paths,
        crate::signals::unix_now(),
        SyncAuditAction::PairJoin,
        None,
        preview.summary.operations_added,
        0,
    );
    Ok(installed)
}

/// Leave an authenticated join staged for a later code-independent `--resume`.
pub fn defer_pairing_join(
    paths: &SyncPaths,
    preview: &PairingJoinPreview,
) -> Result<(), SyncServiceError> {
    let private_store = PrivateStore::new(paths.private_store())?;
    let private = private_store.load()?;
    if private.revision() != preview.expected_private_revision
        || private.enrollment() != EnrollmentState::PendingLedgerCommit
        || private.device_id() != preview.device_id
    {
        return Err(SyncServiceError::LocalStateChanged);
    }
    Ok(())
}

/// Explicitly discard only an unapproved local join attempt.
///
/// A checkpoint-anchored `PendingLedgerCommit` is deliberately retained because its device may
/// already be an active remote member. Cleanup removes the profile before the private key store,
/// then removes the signed journal before its request. A crash therefore leaves either an
/// authenticatable journal/request pair or a request-only ciphertext with no surviving local
/// key/profile, which is safe to discard without changing remote state.
pub fn cancel_pairing_join(paths: &SyncPaths) -> Result<(), SyncServiceError> {
    let private_exists = regular_file_exists(paths.private_store())?;
    let profile_exists = regular_file_exists(paths.profile())?;
    let private_store = PrivateStore::new(paths.private_store())?;
    let profile_store = WebDavProfileStore::new(paths.profile())?;
    let join_store = JoinPairingStore::new(paths);

    let private = if private_exists {
        let private = private_store.load()?;
        if private.enrollment() != EnrollmentState::PendingApproval
            || private.pending_pairing()?.is_none()
        {
            return Err(SyncServiceError::AlreadyConfigured);
        }
        let journal_authenticated = if let Some(journal) = join_store.load(private.device())? {
            let pending = private
                .pending_pairing()?
                .ok_or(SyncServiceError::InvalidRemoteData)?;
            if !journal.matches_context(
                private.dataset_id(),
                private.device_id(),
                pending.invite_id(),
                pending.request_nonce(),
            ) {
                return Err(SyncServiceError::InvalidRemoteData);
            }
            true
        } else {
            false
        };
        if profile_exists {
            match profile_store.load(private.device()) {
                Ok(profile)
                    if profile.dataset_id() == private.dataset_id()
                        && profile.device_id() == private.device_id() => {}
                Err(_) if journal_authenticated => {}
                Ok(_) | Err(_) => return Err(SyncServiceError::InvalidRemoteData),
            }
        }
        Some(private)
    } else if let Some((journal, record)) = join_store.load_authenticated()? {
        if profile_exists {
            let profile = profile_store.load_for_pairing_record(&record)?;
            if profile.dataset_id() != journal.dataset_id()
                || profile.device_id() != journal.device_id()
            {
                return Err(SyncServiceError::InvalidRemoteData);
            }
        }
        None
    } else {
        if profile_exists {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        let request_exists = regular_file_exists(paths.pending_join_request())?;
        let checkpoint_exists = regular_file_exists(paths.pending_join_checkpoint())?;
        if !request_exists && !checkpoint_exists {
            return Err(SyncServiceError::NotConfigured);
        }
        None
    };

    if profile_exists {
        profile_store.remove()?;
    }
    crate::util::safe_fs::remove_owner_only_file_durable(paths.pending_join_checkpoint())
        .map_err(|_| SyncServiceError::Storage)?;
    if let Some(private) = private {
        private_store.remove(private.revision())?;
    }
    join_store.remove_state()?;
    crate::util::safe_fs::remove_owner_only_file_durable(paths.pending_join_request())
        .map_err(|_| SyncServiceError::Storage)?;
    Ok(())
}

fn validate_host_identity(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    private: &PrivateStoreSnapshot,
    host: &PairingHostInvite,
) -> Result<(), SyncServiceError> {
    if current_state.dataset_id != host.durable.dataset_id()
        || private.device_id() != host.durable.host_device_id()
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    let observed = HostPairingStore::new(paths)
        .load(private.device())?
        .ok_or(SyncServiceError::LocalStateChanged)?;
    if !observed.same_durable_record(&host.durable) {
        return Err(SyncServiceError::LocalStateChanged);
    }
    Ok(())
}

fn validate_locator(
    locator: &PairingLocator,
    invite_id: &str,
    now_unix: i64,
) -> Result<(), SyncServiceError> {
    if locator.kind != LOCATOR_KIND
        || locator.schema_version != super::super::VAULT_SCHEMA_VERSION
        || locator.invite_id != invite_id
        || locator.expires_at_unix > now_unix.saturating_add(MAX_PAIRING_WAIT_SECONDS)
        || crate::personal_state::PersonalStateV2::empty(locator.dataset_id.clone()).is_err()
    {
        return Err(SyncServiceError::PairingRejected);
    }
    if locator.expires_at_unix < now_unix {
        return Err(SyncServiceError::PairingExpired);
    }
    Ok(())
}

fn poll_until_present<T: VaultTransport + ?Sized>(
    transport: &T,
    key: &ObjectKey,
    wait_until_unix: i64,
) -> Result<EncryptedObject, SyncServiceError> {
    loop {
        if let Some((object, _)) = transport.get(key, MAX_VAULT_PAYLOAD_BYTES)? {
            return Ok(object);
        }
        if crate::signals::unix_now() > wait_until_unix {
            return Err(SyncServiceError::PairingExpired);
        }
        thread::sleep(PAIRING_POLL_INTERVAL);
    }
}

fn put_immutable_or_verify<T: VaultTransport + ?Sized>(
    transport: &T,
    key: &ObjectKey,
    object: &EncryptedObject,
) -> Result<(), SyncServiceError> {
    match transport.put(key, object, ObjectCondition::CreateOnly) {
        Ok(_) => Ok(()),
        Err(VaultError::PreconditionFailed) => {
            let (existing, _) = transport
                .get(key, MAX_VAULT_PAYLOAD_BYTES)?
                .ok_or(SyncServiceError::InvalidRemoteData)?;
            if existing.as_bytes() == object.as_bytes() {
                Ok(())
            } else {
                Err(SyncServiceError::InvalidRemoteData)
            }
        }
        Err(error) => match transport.get(key, MAX_VAULT_PAYLOAD_BYTES) {
            Ok(Some((existing, _))) if existing.as_bytes() == object.as_bytes() => Ok(()),
            Ok(Some(_)) => Err(SyncServiceError::InvalidRemoteData),
            Ok(None) | Err(_) => Err(error.into()),
        },
    }
}

fn global_pairing_key(invite_id: &str, file: &str) -> Result<ObjectKey, SyncServiceError> {
    pairing_key(None, invite_id, file)
}

fn dataset_pairing_key(
    dataset_id: &str,
    invite_id: &str,
    file: &str,
) -> Result<ObjectKey, SyncServiceError> {
    pairing_key(Some(dataset_id), invite_id, file)
}

fn pairing_key(
    dataset_id: Option<&str>,
    invite_id: &str,
    file: &str,
) -> Result<ObjectKey, SyncServiceError> {
    let key = match dataset_id {
        Some(dataset_id) => {
            format!("yututui/v2/{dataset_id}/pairing/{invite_id}/{file}")
        }
        None => format!("yututui/v2/pairing/{invite_id}/{file}"),
    };
    ObjectKey::new(key).map_err(Into::into)
}

fn regular_file_exists(path: &std::path::Path) -> Result<bool, SyncServiceError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(SyncServiceError::Storage),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(SyncServiceError::Storage),
    }
}

fn persist_join_checkpoint(
    paths: &SyncPaths,
    checkpoint: &EncryptedObject,
) -> Result<(), SyncServiceError> {
    if regular_file_exists(paths.pending_join_checkpoint())? {
        let current = read_join_checkpoint(paths)?;
        if current.as_bytes() == checkpoint.as_bytes() {
            return Ok(());
        }
        return Err(SyncServiceError::InvalidRemoteData);
    }
    crate::util::safe_fs::write_owner_only_atomic(
        paths.pending_join_checkpoint(),
        checkpoint.as_bytes(),
    )
    .map_err(|_| SyncServiceError::Storage)
}

fn read_join_checkpoint(paths: &SyncPaths) -> Result<EncryptedObject, SyncServiceError> {
    let bytes = crate::util::safe_fs::read_owner_only_limited(
        paths.pending_join_checkpoint(),
        super::super::crypto::MAX_ENCRYPTED_OBJECT_BYTES as u64,
    )
    .map_err(|_| SyncServiceError::Storage)?;
    EncryptedObject::from_bytes(bytes).map_err(Into::into)
}

fn load_or_fetch_join_checkpoint(
    paths: &SyncPaths,
    private: &PrivateStoreSnapshot,
    profile: &WebDavProfile,
    invite_id: &str,
) -> Result<(EncryptedObject, bool), SyncServiceError> {
    if regular_file_exists(paths.pending_join_checkpoint())? {
        return read_join_checkpoint(paths).map(|checkpoint| (checkpoint, false));
    }
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open_saved_webdav_transport(profile, credential)?;
    fetch_join_checkpoint(&transport, private.dataset_id(), invite_id)
        .map(|checkpoint| (checkpoint, true))
}

fn fetch_join_checkpoint<T: VaultTransport + ?Sized>(
    transport: &T,
    dataset_id: &str,
    invite_id: &str,
) -> Result<EncryptedObject, SyncServiceError> {
    let key = dataset_pairing_key(dataset_id, invite_id, "checkpoint.age")?;
    transport
        .get(&key, MAX_VAULT_PAYLOAD_BYTES)?
        .map(|(checkpoint, _)| checkpoint)
        .ok_or(SyncServiceError::PendingApproval)
}

fn record_pairing_audit(
    paths: &SyncPaths,
    now_unix: i64,
    action: SyncAuditAction,
    device_id: Option<String>,
    local_operations: usize,
    remote_operations: usize,
) -> Result<(), SyncServiceError> {
    let health_store = SyncHealthStore::new(paths.health())?;
    let current = health_store.load(true)?;
    let _ = health_store.save(&current, current.succeeded(now_unix))?;
    let mut entry = SyncAuditEntry::new(now_unix, action, SyncAuditOutcome::Succeeded)?;
    entry.device_id = device_id;
    entry.local_operations = local_operations;
    entry.remote_operations = remote_operations;
    let _ = SyncAuditStore::new(paths.audit())?.append(now_unix, entry)?;
    Ok(())
}

#[cfg(test)]
#[path = "pairing/tests.rs"]
mod tests;
