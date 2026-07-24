use std::io;
use std::path::PathBuf;

use crate::personal_state::{
    DeviceId, DeviceRecord, Operation, OperationOrigin, PersonalStateCommit, PersonalStateV2,
    append_operation_as,
};

use super::super::manual::{
    ManualSyncEngine, ManualSyncInput, ManualSyncSummary, SignedVaultManifest, manifest_key,
};
use super::super::{
    CheckpointAnchor, DeviceSecretMaterial, EnrollmentState, MAX_VAULT_PAYLOAD_BYTES,
    MembershipAnchor, MembershipChain, PrivateStore, PrivateStoreSnapshot, RecoveryKit,
    SignedCheckpoint, SignedMembershipRoot, SyncAuditAction, SyncAuditEntry, SyncAuditOutcome,
    SyncAuditStore, SyncHealthStore, SyncPaths, VaultCredential, VaultTransport, WebDavProfile,
    WebDavProfileStore, decrypt_json_with_identity,
};
use super::transition::{AnchorActivationKind, commit_with_anchor_transition};
use super::{SyncServiceError, map_webdav_error};

/// Secret-bearing setup input. It intentionally implements neither `Debug` nor `Clone`.
pub struct SetupRequest {
    pub endpoint: String,
    pub custom_ca_pem: Option<Vec<u8>>,
    pub device_name: String,
    pub credential: VaultCredential,
    pub recovery_file: PathBuf,
}

pub struct SetupResult {
    pub state: PersonalStateV2,
    pub device_id: DeviceId,
    pub recovery_checksum: String,
    pub summary: ManualSyncSummary,
    pub resumed: bool,
}

struct PreparedSetup {
    expected_state: PersonalStateV2,
    playlist_revision: u64,
    commit: PersonalStateCommit,
    private: PrivateStoreSnapshot,
    profile: WebDavProfile,
    membership: MembershipChain,
    membership_anchor: MembershipAnchor,
    checkpoint: SignedCheckpoint,
    device_id: DeviceId,
}

struct PendingSetup {
    private: PrivateStoreSnapshot,
    profile: WebDavProfile,
}

struct ResumableRemoteSetup {
    commit: PersonalStateCommit,
    checkpoint: SignedCheckpoint,
    summary: ManualSyncSummary,
}

pub fn setup(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
    request: SetupRequest,
) -> Result<SetupResult, SyncServiceError> {
    if let Some(mut pending) = load_pending_setup(
        sync_paths,
        &request.endpoint,
        request.custom_ca_pem.as_deref(),
    )? {
        let requested_profile = WebDavProfile::with_custom_ca(
            current_state.dataset_id.clone(),
            pending.private.device(),
            &request.endpoint,
            request.custom_ca_pem.as_deref(),
        )?;
        if requested_profile.endpoint() != pending.profile.endpoint()
            || requested_profile.custom_ca_pem() != pending.profile.custom_ca_pem()
            || requested_profile.dataset_id() != pending.profile.dataset_id()
            || requested_profile.device_id() != pending.profile.device_id()
        {
            return Err(SyncServiceError::AlreadyConfigured);
        }
        let transport = checked_webdav_transport(
            pending.profile.endpoint(),
            pending.profile.custom_ca_pem(),
            &request.credential,
        )?;
        if let Some(remote) =
            load_resumable_remote_setup(current_state, playlist_revision, &pending, &transport)?
        {
            pending.private.set_credential(request.credential);
            return finish_resumed_setup(
                current_state,
                playlist_revision,
                personal_paths,
                sync_paths,
                pending,
                remote,
            );
        }

        cleanup_pending_setup(
            &PrivateStore::new(sync_paths.private_store())?,
            &pending.private,
            &WebDavProfileStore::new(sync_paths.profile())?,
        )?;
    }

    let transport = checked_webdav_transport(
        &request.endpoint,
        request.custom_ca_pem.as_deref(),
        &request.credential,
    )?;
    let prepared = prepare(current_state, playlist_revision, request, sync_paths)?;
    setup_with_transport(prepared, personal_paths, sync_paths, &transport)
}

pub(super) fn checked_webdav_transport(
    endpoint: &str,
    custom_ca_pem: Option<&[u8]>,
    credential: &VaultCredential,
) -> Result<super::super::webdav::BlockingWebDavTransport, SyncServiceError> {
    let transport = super::super::webdav::BlockingWebDavTransport::with_custom_ca(
        endpoint,
        custom_ca_pem,
        credential,
    )
    .map_err(map_webdav_error)?;
    if !transport
        .probe_capabilities()
        .map_err(map_webdav_error)?
        .supports_encrypted_sync()
    {
        return Err(SyncServiceError::UnsupportedServer);
    }
    Ok(transport)
}

fn prepare(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    request: SetupRequest,
    sync_paths: &SyncPaths,
) -> Result<PreparedSetup, SyncServiceError> {
    ensure_setup_absent(sync_paths)?;
    let local = unkeyed_local_device(current_state)?;
    let device = DeviceSecretMaterial::generate_for(local.device_id.as_str())?;
    let enrolled = DeviceRecord {
        device_id: local.device_id.clone(),
        name: request.device_name,
        revoked: false,
        public_identity: Some(device.public_identity()),
    };
    let state = append_operation_as(
        current_state,
        &local.device_id,
        Operation::AddDevice {
            device: enrolled.clone(),
        },
        crate::signals::unix_now(),
    )?;
    let commit = PersonalStateCommit::prepare_for_runtime(state, playlist_revision)?;

    let recovery = RecoveryKit::generate(commit.state().dataset_id.clone(), None)?;
    let recovery_checksum = recovery
        .export_confirmed(&request.recovery_file)
        .map_err(|_| SyncServiceError::RecoveryKitNotConfirmed)?;
    let root = SignedMembershipRoot::create(
        commit.state().dataset_id.clone(),
        recovery.recovery_recipient(),
        &recovery.signing_key()?,
        enrolled,
    )?;
    let membership_anchor = MembershipAnchor::RootHash(root.hash()?);
    let membership = MembershipChain::new(root);
    let checkpoint = SignedCheckpoint::create(
        membership.clone(),
        &membership_anchor,
        local.device_id.clone(),
        device.signing_key(),
        &CheckpointAnchor::default(),
        commit.state().clone(),
    )?;
    let mut private = PrivateStoreSnapshot::pending_ledger_commit(
        device,
        recovery.recovery_recipient(),
        recovery.recovery_verifying_key()?,
        match &membership_anchor {
            MembershipAnchor::RootHash(hash) => hash.clone(),
            MembershipAnchor::RecoveryVerifyingKey(_) => {
                return Err(SyncServiceError::InvalidRemoteData);
            }
        },
        &checkpoint,
    )?;
    private.confirm_setup_recovery(recovery_checksum.clone())?;
    private.set_credential(request.credential);
    let profile = WebDavProfile::with_custom_ca(
        commit.state().dataset_id.clone(),
        private.device(),
        &request.endpoint,
        request.custom_ca_pem.as_deref(),
    )?;
    Ok(PreparedSetup {
        expected_state: current_state.clone(),
        playlist_revision,
        commit,
        private,
        profile,
        membership,
        membership_anchor,
        checkpoint,
        device_id: local.device_id.clone(),
    })
}

fn setup_with_transport<T: VaultTransport + ?Sized>(
    prepared: PreparedSetup,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
    transport: &T,
) -> Result<SetupResult, SyncServiceError> {
    setup_with_transport_using(prepared, personal_paths, sync_paths, transport, || Ok(()))
}

fn setup_with_transport_using<T: VaultTransport + ?Sized>(
    mut prepared: PreparedSetup,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
    transport: &T,
    before_local_commit: impl FnOnce() -> Result<(), SyncServiceError>,
) -> Result<SetupResult, SyncServiceError> {
    let private_store = PrivateStore::new(sync_paths.private_store())?;
    let profile_store = WebDavProfileStore::new(sync_paths.profile())?;
    private_store.create(&mut prepared.private)?;
    if let Err(error) = profile_store.create(&mut prepared.profile, prepared.private.device()) {
        private_store
            .remove(prepared.private.revision())
            .map_err(|_| SyncServiceError::Storage)?;
        return Err(error.into());
    }

    let checkpoint_anchor = CheckpointAnchor::default();
    let input = ManualSyncInput {
        local_state: prepared.commit.state(),
        membership: &prepared.membership,
        membership_anchor: &prepared.membership_anchor,
        device: prepared.private.device(),
        checkpoint_anchor: &checkpoint_anchor,
        bootstrap_checkpoint: Some(&prepared.checkpoint),
        expected_local_revision: prepared.commit.state().revision,
    };
    let candidate = match ManualSyncEngine::new(transport).synchronize(&input, &|expected| {
        if expected == prepared.commit.state().revision {
            Ok(())
        } else {
            Err(super::super::VaultError::RevisionConflict)
        }
    }) {
        Ok(candidate) => candidate,
        Err(error) => {
            // Once the manifest may be visible, local keys and the confirmed recovery marker are
            // the only safe way to finish the exact bootstrap. A conclusive missing read means no
            // usable vault was published, so credentials/profile can be removed immediately.
            if transport
                .get(
                    &manifest_key(&prepared.commit.state().dataset_id)?,
                    MAX_VAULT_PAYLOAD_BYTES,
                )
                .is_ok_and(|remote| remote.is_none())
            {
                cleanup_pending_setup(&private_store, &prepared.private, &profile_store)?;
            }
            return Err(error.into());
        }
    };
    if candidate.state != *prepared.commit.state()
        || candidate.membership != prepared.membership
        || candidate.checkpoint_anchor.checkpoint_sequence
            != prepared.checkpoint.payload.checkpoint_sequence
        || candidate.checkpoint_anchor.checkpoint_hash.as_deref()
            != Some(prepared.checkpoint.hash()?.as_str())
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }

    before_local_commit()?;
    let checkpoint_hash = prepared.checkpoint.hash()?;
    let recovery_checksum = prepared
        .private
        .setup_recovery_checksum()
        .ok_or(SyncServiceError::RecoveryKitNotConfirmed)?
        .to_owned();
    prepared
        .private
        .mark_active(&prepared.checkpoint, prepared.commit.state())?;
    let installed = commit_with_anchor_transition(
        &prepared.expected_state,
        &prepared.commit,
        personal_paths,
        sync_paths,
        &mut prepared.private,
        AnchorActivationKind::Setup,
        prepared.checkpoint.payload.checkpoint_sequence,
        &checkpoint_hash,
        prepared.playlist_revision,
    )?;
    let _ = record_setup_success(sync_paths, candidate.summary.remote_writes);
    Ok(SetupResult {
        state: installed,
        device_id: prepared.device_id,
        recovery_checksum,
        summary: candidate.summary,
        resumed: false,
    })
}

fn finish_resumed_setup(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
    mut pending: PendingSetup,
    remote: ResumableRemoteSetup,
) -> Result<SetupResult, SyncServiceError> {
    let recovery_checksum = pending
        .private
        .setup_recovery_checksum()
        .ok_or(SyncServiceError::RecoveryKitNotConfirmed)?
        .to_owned();
    let device_id = DeviceId::new(pending.private.device_id())
        .map_err(|_| SyncServiceError::InvalidRemoteData)?;
    let checkpoint_hash = remote.checkpoint.hash()?;
    pending
        .private
        .mark_active(&remote.checkpoint, remote.commit.state())?;
    let installed = commit_with_anchor_transition(
        current_state,
        &remote.commit,
        personal_paths,
        sync_paths,
        &mut pending.private,
        AnchorActivationKind::Setup,
        remote.checkpoint.payload.checkpoint_sequence,
        &checkpoint_hash,
        playlist_revision,
    )?;
    let _ = record_setup_success(sync_paths, 0);
    Ok(SetupResult {
        state: installed,
        device_id,
        recovery_checksum,
        summary: remote.summary,
        resumed: true,
    })
}

fn load_resumable_remote_setup<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    pending: &PendingSetup,
    transport: &T,
) -> Result<Option<ResumableRemoteSetup>, SyncServiceError> {
    let anchor = MembershipAnchor::RootHash(
        pending
            .private
            .membership_root_hash()
            .ok_or(SyncServiceError::InvalidRemoteData)?
            .to_owned(),
    );
    let key = manifest_key(&current_state.dataset_id)?;
    let Some((encrypted_manifest, _)) = transport.get(&key, MAX_VAULT_PAYLOAD_BYTES)? else {
        return Ok(None);
    };
    let manifest = SignedVaultManifest::decrypt_for_device(
        &encrypted_manifest,
        pending.private.device(),
        &anchor,
    )?;
    let verified = manifest.verify(&anchor)?;
    if manifest.payload.dataset_id != current_state.dataset_id
        || manifest.payload.generation != 1
        || manifest.payload.membership_head_hash
            != pending
                .private
                .pending_membership_head_hash()
                .ok_or(SyncServiceError::InvalidRemoteData)?
        || manifest.payload.checkpoint_sequence
            != pending
                .private
                .pending_checkpoint_sequence()
                .ok_or(SyncServiceError::InvalidRemoteData)?
        || manifest.payload.checkpoint_hash
            != pending
                .private
                .pending_checkpoint_hash()
                .ok_or(SyncServiceError::InvalidRemoteData)?
        || verified.devices.len() != 1
        || verified
            .devices
            .get(
                &DeviceId::new(pending.private.device_id())
                    .map_err(|_| SyncServiceError::InvalidRemoteData)?,
            )
            .is_none_or(|record| !pending.private.device().matches_personal_record(record))
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }

    let (encrypted_registry, _) = transport
        .get(&manifest.payload.registry_key, MAX_VAULT_PAYLOAD_BYTES)?
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    let registry: MembershipChain =
        decrypt_json_with_identity(&encrypted_registry, pending.private.device().age_identity())?;
    if registry != manifest.payload.membership || registry.verify(&anchor)? != verified {
        return Err(SyncServiceError::InvalidRemoteData);
    }

    let (encrypted_checkpoint, _) = transport
        .get(&manifest.payload.checkpoint_key, MAX_VAULT_PAYLOAD_BYTES)?
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    let checkpoint = SignedCheckpoint::decrypt_for_device(
        &encrypted_checkpoint,
        pending.private.device(),
        &anchor,
    )?;
    if checkpoint.payload.membership != manifest.payload.membership
        || checkpoint.payload.checkpoint_sequence != manifest.payload.checkpoint_sequence
        || checkpoint.hash()? != manifest.payload.checkpoint_hash
        || checkpoint.payload.previous_checkpoint_hash.is_some()
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    let commit = validate_setup_extension(
        current_state,
        playlist_revision,
        pending.private.device(),
        &checkpoint.payload.state,
    )?;
    Ok(Some(ResumableRemoteSetup {
        commit,
        checkpoint,
        summary: ManualSyncSummary {
            attempts: 1,
            ..ManualSyncSummary::default()
        },
    }))
}

fn validate_setup_extension(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    device: &DeviceSecretMaterial,
    remote_state: &PersonalStateV2,
) -> Result<PersonalStateCommit, SyncServiceError> {
    let local = unkeyed_local_device(current_state)?;
    if local.device_id.as_str() != device.device_id()
        || current_state.dataset_id != remote_state.dataset_id
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    let mut extra = remote_state.operations.iter().filter(|remote| {
        !current_state
            .operations
            .iter()
            .any(|local| local.operation_id == remote.operation_id)
    });
    let enrollment = extra.next().ok_or(SyncServiceError::InvalidRemoteData)?;
    if extra.next().is_some()
        || current_state.operations.iter().any(|local| {
            remote_state
                .operations
                .iter()
                .find(|remote| remote.operation_id == local.operation_id)
                != Some(local)
        })
        || enrollment.origin != OperationOrigin::Local
        || !matches!(
            &enrollment.operation,
            Operation::AddDevice { device: record }
                if record.device_id == local.device_id
                    && !record.revoked
                    && device.matches_personal_record(record)
        )
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    let rebuilt = append_operation_as(
        current_state,
        &local.device_id,
        enrollment.operation.clone(),
        enrollment.stamp.recorded_at_unix,
    )?;
    let commit = PersonalStateCommit::prepare_for_runtime(rebuilt, playlist_revision)?;
    if commit.state() != remote_state {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    Ok(commit)
}

fn unkeyed_local_device(state: &PersonalStateV2) -> Result<&DeviceRecord, SyncServiceError> {
    let mut devices = state
        .device_registry
        .values()
        .filter(|device| !device.revoked && device.device_id.as_str() != "legacy");
    let device = devices.next().ok_or(SyncServiceError::InvalidRemoteData)?;
    if devices.next().is_some() || device.public_identity.is_some() {
        return Err(SyncServiceError::AlreadyConfigured);
    }
    Ok(device)
}

fn ensure_setup_absent(paths: &SyncPaths) -> Result<(), SyncServiceError> {
    for path in [paths.private_store(), paths.profile()] {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Err(SyncServiceError::AlreadyConfigured),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(SyncServiceError::Storage),
        }
    }
    Ok(())
}

fn load_pending_setup(
    paths: &SyncPaths,
    requested_endpoint: &str,
    requested_custom_ca_pem: Option<&[u8]>,
) -> Result<Option<PendingSetup>, SyncServiceError> {
    let private_exists = regular_file_exists(paths.private_store())?;
    let profile_exists = regular_file_exists(paths.profile())?;
    if !private_exists && !profile_exists {
        return Ok(None);
    }
    if !private_exists {
        return Err(SyncServiceError::AlreadyConfigured);
    }
    let private_store = PrivateStore::new(paths.private_store())?;
    let private = private_store
        .load()
        .map_err(|_| SyncServiceError::InvalidRemoteData)?;
    if private.enrollment() != EnrollmentState::PendingLedgerCommit
        || private.setup_recovery_checksum().is_none()
        || private.pending_pairing()?.is_some()
    {
        return Err(SyncServiceError::AlreadyConfigured);
    }
    let profile_store = WebDavProfileStore::new(paths.profile())?;
    let profile = if profile_exists {
        profile_store
            .load(private.device())
            .map_err(|_| SyncServiceError::InvalidRemoteData)?
    } else {
        let mut profile = WebDavProfile::with_custom_ca(
            private.dataset_id(),
            private.device(),
            requested_endpoint,
            requested_custom_ca_pem,
        )?;
        profile_store.create(&mut profile, private.device())?;
        profile
    };
    if profile.dataset_id() != private.dataset_id() || profile.device_id() != private.device_id() {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    Ok(Some(PendingSetup { private, profile }))
}

fn cleanup_pending_setup(
    private_store: &PrivateStore,
    private: &PrivateStoreSnapshot,
    profile_store: &WebDavProfileStore,
) -> Result<(), SyncServiceError> {
    profile_store
        .remove()
        .map_err(|_| SyncServiceError::Storage)?;
    private_store
        .remove(private.revision())
        .map_err(|_| SyncServiceError::Storage)
}

fn regular_file_exists(path: &std::path::Path) -> Result<bool, SyncServiceError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(SyncServiceError::Storage),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(SyncServiceError::Storage),
    }
}

fn record_setup_success(paths: &SyncPaths, writes: usize) -> Result<(), SyncServiceError> {
    let now = crate::signals::unix_now();
    let health_store = SyncHealthStore::new(paths.health())?;
    let current = health_store.load(true)?;
    let _ = health_store.save(&current, current.succeeded(now))?;
    let mut audit = SyncAuditEntry::new(now, SyncAuditAction::Setup, SyncAuditOutcome::Succeeded)?;
    audit.local_operations = 1;
    audit.remote_operations = writes;
    let _ = SyncAuditStore::new(paths.audit())?.append(now, audit)?;
    Ok(())
}

#[cfg(test)]
#[path = "setup/tests.rs"]
mod tests;
