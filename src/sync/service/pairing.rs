mod durable;
#[path = "pairing/join_durable.rs"]
mod join_durable;
#[path = "pairing/lifecycle.rs"]
mod lifecycle;
#[path = "pairing/storage.rs"]
mod storage;

use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::personal_state::{
    DeviceId, DeviceRecord, Operation, PersonalStateCommit, PersonalStateV2, append_operation_as,
    plan_join_import,
};

use super::super::manual::{ManualSyncEngine, ManualSyncInput};
use super::super::{
    DeviceSecretMaterial, EncryptedObject, EnrollmentState, MAX_VAULT_PAYLOAD_BYTES,
    MembershipAction, PairingCode, PairingInvite, PairingRequestPayload, PrivateStore,
    PrivateStoreSnapshot, SealedPairingApproval, SealedPairingRequest, SyncAuditAction,
    SyncAuditEntry, SyncAuditOutcome, SyncAuditStore, SyncHealthStore, SyncPaths, VaultCredential,
    VaultError, VaultTransport, WebDavProfile, WebDavProfileStore,
};
use super::manual::{
    PreparedManualSync, load_remote_membership, membership_anchor, membership_prefix_for_registry,
    prepare_manual_sync_with_transport, validate_active_context, validate_active_private,
};
use super::transition::{AnchorActivationKind, commit_with_anchor_transition};
use super::{SyncServiceError, apply_manual_sync, open_saved_webdav_transport};
use durable::{HostPairingSnapshot, HostPairingStore};
use join_durable::{JoinPairingSnapshot, JoinPairingStore};
pub use lifecycle::{
    PairingJoinPreview, PairingJoinWaiting, PreparedPairingApproval, PreparedPairingJoinActivation,
};
#[cfg(test)]
use storage::fetch_join_checkpoint;
use storage::{
    checkpoint_key, dataset_pairing_key, global_pairing_key, load_or_fetch_join_checkpoint,
    persist_join_checkpoint, put_immutable_or_verify, regular_file_exists, validate_locator,
};

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

pub(super) fn host_pairing_needs_review(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
) -> Result<bool, SyncServiceError> {
    let private = PrivateStore::new(paths.private_store())?.load()?;
    validate_active_private(current_state, &private)?;
    let Some(durable) = HostPairingStore::new(paths).load(private.device())? else {
        return Ok(false);
    };
    if durable.dataset_id() != current_state.dataset_id
        || durable.host_device_id() != private.device_id()
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    Ok(durable.has_bound_request() || durable.has_handoff())
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
    create_pairing_invite_with_transport(current_state, paths, now_unix, &private, &transport)
}

fn create_pairing_invite_with_transport<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    now_unix: i64,
    private: &PrivateStoreSnapshot,
    transport: &T,
) -> Result<PairingHostInvite, SyncServiceError> {
    validate_active_private(current_state, private)?;
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
                transport,
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
    let anchor = membership_anchor(private)?;
    let remote = load_remote_membership(current_state, private, &anchor, transport)?;
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
        transport,
        &global_pairing_key(invite.invite_id(), "locator.age")?,
        &encrypted,
    )?;
    Ok(PairingHostInvite {
        invite,
        durable,
        resumed: false,
    })
}

/// Cancel an uncommitted host invitation, including a request that the user rejected.
///
/// Immutable remote request objects are harmless without a signed handoff and expire naturally.
/// Once a handoff exists, cancellation is refused because the joining device may already be an
/// active remote member; the prepared approval must instead be installed and finalized.
pub fn cancel_pairing_invite(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    host: &PairingHostInvite,
) -> Result<(), SyncServiceError> {
    let private = PrivateStore::new(paths.private_store())?.load()?;
    let profile = WebDavProfileStore::new(paths.profile())?.load(private.device())?;
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open_saved_webdav_transport(&profile, credential)?;
    cancel_pairing_invite_with_transport(current_state, paths, host, &private, &transport)
}

fn cancel_pairing_invite_with_transport<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    host: &PairingHostInvite,
    private: &PrivateStoreSnapshot,
    transport: &T,
) -> Result<(), SyncServiceError> {
    validate_host_identity(current_state, paths, private, host)?;
    if host.durable.has_handoff() {
        return Err(SyncServiceError::AlreadyConfigured);
    }
    if let Some(request_device_id) = host.durable.request_device_id() {
        // A lost response may leave the remote AddDevice/checkpoint committed before the local
        // handoff journal is written. Re-read authenticated membership before deleting the only
        // recovery context; an active target must resume/finalize instead of becoming an orphan.
        let anchor = membership_anchor(private)?;
        let membership = load_remote_membership(current_state, private, &anchor, transport)?;
        let verified = membership.verify(&anchor)?;
        let request_device_id =
            DeviceId::new(request_device_id).map_err(|_| SyncServiceError::InvalidRemoteData)?;
        if verified
            .devices
            .get(&request_device_id)
            .is_some_and(|device| !device.revoked)
        {
            return Err(SyncServiceError::AlreadyConfigured);
        }
    }
    HostPairingStore::new(paths).remove()?;
    Ok(())
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
    poll_pairing_request_with_transport(current_state, paths, host, now_unix, &private, &transport)
}

fn poll_pairing_request_with_transport<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    host: &mut PairingHostInvite,
    now_unix: i64,
    private: &PrivateStoreSnapshot,
    transport: &T,
) -> Result<Option<PairingReview>, SyncServiceError> {
    validate_host_identity(current_state, paths, private, host)?;
    validate_active_private(current_state, private)?;
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
    let fingerprint = pairing_fingerprint(&payload.device)?;
    Ok(Some(PairingReview {
        device_id: payload.device.device_id.as_str().to_owned(),
        device_name: payload.device.name.clone(),
        fingerprint,
        sealed,
        payload,
    }))
}

fn pairing_fingerprint(device: &DeviceRecord) -> Result<String, SyncServiceError> {
    let identity = device
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
    Ok(fingerprint[..16].to_owned())
}

#[allow(clippy::too_many_arguments)]
pub fn approve_pairing_request(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    paths: &SyncPaths,
    host: &mut PairingHostInvite,
    review: PairingReview,
    now_unix: i64,
) -> Result<PersonalStateV2, SyncServiceError> {
    let prepared = prepare_pairing_approval(current_state, paths, host, review, now_unix)?;
    apply_prepared_pairing_approval(
        current_state,
        playlist_revision,
        personal_paths,
        paths,
        prepared,
    )
}

/// Publish an authenticated approval handoff without installing its local ledger candidate.
///
/// This is the network-worker half of host approval. The caller must route the returned candidate
/// through the primary persistence owner, then call [`finalize_prepared_pairing_approval`].
pub fn prepare_pairing_approval(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    host: &mut PairingHostInvite,
    review: PairingReview,
    now_unix: i64,
) -> Result<PreparedPairingApproval, SyncServiceError> {
    let private_store = PrivateStore::new(paths.private_store())?;
    let private = private_store.load()?;
    validate_host_identity(current_state, paths, &private, host)?;
    let profile = WebDavProfileStore::new(paths.profile())?.load(private.device())?;
    validate_active_context(current_state, &private, &profile)?;
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open_saved_webdav_transport(&profile, credential)?;
    prepare_pairing_approval_with_transport(
        current_state,
        paths,
        host,
        review,
        now_unix,
        &private,
        &transport,
    )
}

fn prepare_pairing_approval_with_transport<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    host: &mut PairingHostInvite,
    review: PairingReview,
    now_unix: i64,
    private: &PrivateStoreSnapshot,
    transport: &T,
) -> Result<PreparedPairingApproval, SyncServiceError> {
    validate_host_identity(current_state, paths, private, host)?;
    validate_active_private(current_state, private)?;
    let authenticated_request = host.invite.review_bound_request(&review.sealed)?;
    if authenticated_request != review.payload {
        return Err(SyncServiceError::PairingRejected);
    }
    let target_device = authenticated_request.device;
    let target_device_name = target_device.name.clone();
    let target_fingerprint = pairing_fingerprint(&target_device)?;
    let host_store = HostPairingStore::new(paths);
    host_store.bind_request(
        private.device(),
        &mut host.durable,
        &review.sealed.encrypted,
        &target_device.device_id,
    )?;
    let anchor = membership_anchor(private)?;
    let mut prepared = prepare_manual_sync_with_transport(current_state, private, transport)?;
    if !pairing_device_is_active(&prepared, &target_device) && !host.durable.has_handoff() {
        if now_unix > host.expires_at_unix() {
            host_store.remove()?;
            return Err(SyncServiceError::PairingExpired);
        }
        prepared = match commit_pairing_membership(
            current_state,
            private,
            &anchor,
            transport,
            prepared,
            &target_device,
            host.expires_at_unix(),
        ) {
            Ok(prepared) => prepared,
            Err(SyncServiceError::PairingExpired) => {
                let detected =
                    prepare_manual_sync_with_transport(current_state, private, transport)?;
                if !pairing_device_is_active(&detected, &target_device) {
                    host_store.remove()?;
                    return Err(SyncServiceError::PairingExpired);
                }
                detected
            }
            Err(error) => return Err(error),
        };
    }
    if !pairing_device_is_active(&prepared, &target_device) {
        return Err(SyncServiceError::InvalidRemoteData);
    }

    if !host.durable.has_handoff() {
        let (encrypted_checkpoint, checkpoint_hash, membership_head_hash) =
            load_prepared_checkpoint(
                current_state,
                &anchor,
                transport,
                private.device(),
                &prepared,
            )?;
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
        transport,
        &handoff.checkpoint,
        &handoff.approval,
    )?;
    Ok(PreparedPairingApproval {
        candidate: prepared,
        target_device,
        target_device_name,
        target_fingerprint,
        invite_id: host.invite.invite_id().to_owned(),
    })
}

/// Install a prepared host approval through the same exact manual-sync transaction as the CLI.
pub fn apply_prepared_pairing_approval(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    paths: &SyncPaths,
    prepared: PreparedPairingApproval,
) -> Result<PersonalStateV2, SyncServiceError> {
    let installed = apply_manual_sync(
        current_state,
        playlist_revision,
        prepared.candidate.clone(),
        personal_paths,
        paths,
    )?;
    finalize_prepared_pairing_approval(&installed, paths, &prepared)?;
    Ok(installed)
}

/// Remove the durable host invite only after its exact target is present in the installed ledger.
///
/// This step is idempotent so a persistence acknowledgement lost after the ledger transaction can
/// be retried without regenerating or republishing any secret-bearing pairing object.
pub fn finalize_prepared_pairing_approval(
    installed_state: &PersonalStateV2,
    paths: &SyncPaths,
    prepared: &PreparedPairingApproval,
) -> Result<(), SyncServiceError> {
    let private = PrivateStore::new(paths.private_store())?.load()?;
    validate_active_private(installed_state, &private)?;
    if installed_state.dataset_id != prepared.candidate.state.dataset_id
        || prepared.candidate.local_device_id.as_str() != private.device_id()
        || installed_state
            .device_registry
            .get(&prepared.target_device.device_id)
            != Some(&prepared.target_device)
    {
        return Err(SyncServiceError::LocalStateChanged);
    }
    let host_store = HostPairingStore::new(paths);
    let Some(durable) = host_store.load(private.device())? else {
        return Ok(());
    };
    if durable.dataset_id() != installed_state.dataset_id
        || durable.host_device_id() != private.device_id()
        || durable.invite_id() != prepared.invite_id
        || durable.request_device_id() != Some(prepared.target_device.device_id.as_str())
        || !durable.has_handoff()
    {
        return Err(SyncServiceError::LocalStateChanged);
    }
    host_store.remove()?;
    let _ = record_pairing_audit(
        paths,
        crate::signals::unix_now(),
        SyncAuditAction::PairCreate,
        Some(prepared.target_device.device_id.as_str().to_owned()),
        prepared.candidate.summary.uploaded_operations,
        prepared.candidate.summary.downloaded_operations,
    );
    Ok(())
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
    device: &DeviceSecretMaterial,
    prepared: &PreparedManualSync,
) -> Result<(EncryptedObject, String, String), SyncServiceError> {
    let checkpoint_hash = prepared
        .checkpoint_anchor
        .checkpoint_hash
        .clone()
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    let verified_membership = prepared.membership.verify(anchor)?;
    let checkpoint_key = checkpoint_key(
        &current_state.dataset_id,
        verified_membership.epoch,
        &checkpoint_hash,
    )?;
    let (encrypted_checkpoint, _) = transport
        .get(&checkpoint_key, MAX_VAULT_PAYLOAD_BYTES)?
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    let checkpoint =
        super::super::SignedCheckpoint::decrypt_for_device(&encrypted_checkpoint, device, anchor)?;
    if checkpoint.hash()? != checkpoint_hash
        || checkpoint.payload.checkpoint_sequence != prepared.checkpoint_anchor.checkpoint_sequence
        || checkpoint.payload.membership != prepared.membership
        || checkpoint.payload.state != prepared.state
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    Ok((
        // A complete age decrypt, membership/signature validation, and exact signed payload/hash
        // comparison above make this readback safe to stage and publish to the joining device.
        encrypted_checkpoint.authenticated_after_verification(),
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
pub fn start_pairing_join(
    paths: &SyncPaths,
    endpoint: String,
    custom_ca_pem: Option<Vec<u8>>,
    credential: VaultCredential,
    code: &str,
    device_name: String,
    now_unix: i64,
) -> Result<PairingJoinWaiting, SyncServiceError> {
    let transport =
        super::setup::checked_webdav_transport(&endpoint, custom_ca_pem.as_deref(), &credential)?;
    start_pairing_join_with_transport(
        paths,
        endpoint,
        custom_ca_pem,
        credential,
        code,
        device_name,
        now_unix,
        &transport,
    )
}

#[allow(clippy::too_many_arguments)]
fn start_pairing_join_with_transport<T: VaultTransport + ?Sized>(
    paths: &SyncPaths,
    endpoint: String,
    custom_ca_pem: Option<Vec<u8>>,
    credential: VaultCredential,
    code: &str,
    device_name: String,
    now_unix: i64,
    transport: &T,
) -> Result<PairingJoinWaiting, SyncServiceError> {
    let code = PairingCode::parse(code)?;
    let invite_id = PairingInvite::invite_id_for_code(&code)?;
    let locator_key = global_pairing_key(&invite_id, "locator.age")?;
    let (encrypted_locator, _) = transport
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
            if let Some((remote, _)) = transport.get(&request_key, MAX_VAULT_PAYLOAD_BYTES)?
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
        for path in [
            paths.pending_join_request(),
            paths.pending_join_checkpoint(),
        ] {
            if regular_file_exists(path)? {
                crate::util::safe_fs::remove_owner_only_file_durable(path)
                    .map_err(|_| SyncServiceError::Storage)?;
            }
        }
    }
    let (sealed_request, device_id) = if existing_private {
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
        load_or_create_join_profile(&profile_store, existing_profile, &private, expected_profile)?;
        let device_id =
            DeviceId::new(private.device_id()).map_err(|_| SyncServiceError::PairingRejected)?;
        (sealed_request, device_id)
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
        (sealed_request, device_id)
    };
    let request_key = dataset_pairing_key(
        &locator.dataset_id,
        &sealed_request.invite_id,
        "request.age",
    )?;
    put_immutable_or_verify(transport, &request_key, &sealed_request.encrypted)?;
    Ok(PairingJoinWaiting {
        device_id: device_id.as_str().to_owned(),
        expires_at_unix: locator.expires_at_unix,
        resumed: existing_private,
    })
}

/// Preserve the CLI's blocking behavior on top of the one-shot lifecycle API.
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
    let _waiting = start_pairing_join(
        paths,
        endpoint,
        custom_ca_pem,
        credential,
        code,
        device_name,
        now_unix,
    )?;
    loop {
        if let Some(preview) = poll_pairing_join(current_state, paths, crate::signals::unix_now())?
        {
            return Ok(preview);
        }
        thread::sleep(PAIRING_POLL_INTERVAL);
    }
}

/// Perform one bounded read of the approval lifecycle.
///
/// `Ok(None)` means the host has not completed approval yet. No sleep or retry occurs here.
pub fn poll_pairing_join(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    now_unix: i64,
) -> Result<Option<PairingJoinPreview>, SyncServiceError> {
    match resume_pairing_join(current_state, paths) {
        Ok(preview) => Ok(Some(preview)),
        Err(SyncServiceError::PendingApproval) => {
            let expires_at_unix = JoinPairingStore::new(paths)
                .load_authenticated()?
                .map(|(journal, _)| journal.expires_at_unix())
                .ok_or(SyncServiceError::PairingNeedsCleanup)?;
            if now_unix > expires_at_unix {
                Err(SyncServiceError::PairingExpired)
            } else {
                Ok(None)
            }
        }
        Err(error) => Err(error),
    }
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
    let prepared = prepare_pairing_join_activation(current_state, paths, preview)?;
    apply_prepared_pairing_join(
        current_state,
        playlist_revision,
        personal_paths,
        paths,
        prepared,
    )
}

/// Validate and detach a join activation without writing the ledger or private store.
pub fn prepare_pairing_join_activation(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    preview: PairingJoinPreview,
) -> Result<PreparedPairingJoinActivation, SyncServiceError> {
    validate_pairing_join_preview(current_state, paths, &preview)?;
    Ok(preview.into_activation())
}

/// Apply a clone-safe join activation on the persistence-owner lane.
pub fn apply_prepared_pairing_join(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    paths: &SyncPaths,
    prepared: PreparedPairingJoinActivation,
) -> Result<PersonalStateV2, SyncServiceError> {
    let preview = prepared.preview;
    validate_pairing_join_preview(current_state, paths, &preview)?;
    apply_pairing_join_exact(
        current_state,
        playlist_revision,
        personal_paths,
        paths,
        preview,
    )
}

fn validate_pairing_join_preview(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    preview: &PairingJoinPreview,
) -> Result<(), SyncServiceError> {
    if current_state.revision != preview.expected_local_revision {
        return Err(SyncServiceError::LocalStateChanged);
    }
    let private = PrivateStore::new(paths.private_store())?.load()?;
    if private.revision() != preview.expected_private_revision
        || private.enrollment() != EnrollmentState::PendingLedgerCommit
        || private.device_id() != preview.device_id
    {
        return Err(SyncServiceError::LocalStateChanged);
    }
    if preview.candidate.dataset_id != private.dataset_id()
        || preview.checkpoint.payload.dataset_id != private.dataset_id()
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    Ok(())
}

fn apply_pairing_join_exact(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    paths: &SyncPaths,
    preview: PairingJoinPreview,
) -> Result<PersonalStateV2, SyncServiceError> {
    let private_store = PrivateStore::new(paths.private_store())?;
    let mut private = private_store.load()?;
    let commit = prepare_pairing_join_commit(current_state, playlist_revision, preview.candidate)?;
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

pub(super) fn prepare_pairing_join_commit(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    mut candidate: PersonalStateV2,
) -> Result<PersonalStateCommit, SyncServiceError> {
    // Joining replaces the fresh device's local dataset with the authenticated remote dataset.
    // PersonalState transactions are still ordered by one local revision counter, so the first
    // remote candidate must be strictly newer than whichever local ledger is currently visible.
    candidate.revision = candidate.revision.max(current_state.next_revision()?);
    let commit = PersonalStateCommit::prepare_for_runtime(candidate, playlist_revision)?;
    if commit.state().revision <= current_state.revision {
        return Err(SyncServiceError::LocalStateChanged);
    }
    Ok(commit)
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
