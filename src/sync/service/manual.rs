use crate::personal_state::{DeviceId, DeviceRegistry, PersonalStateCommit, PersonalStateV2};

use super::super::manual::{
    ManualSyncBudget, ManualSyncCandidate, ManualSyncEngine, ManualSyncInput, ManualSyncSummary,
    manifest_key,
};
use super::super::{
    CheckpointAnchor, EnrollmentState, MAX_VAULT_PAYLOAD_BYTES, MembershipAnchor, MembershipChain,
    PrivateStore, PrivateStoreSnapshot, SyncAuditAction, SyncAuditEntry, SyncAuditOutcome,
    SyncAuditStore, SyncFailureKind, SyncHealthStore, SyncPaths, VaultError, VaultTransport,
    WebDavProfile, WebDavProfileStore,
};
use super::transition::{AnchorActivationKind, commit_with_anchor_transition};
use super::{SyncServiceError, open_saved_webdav_transport};

/// A network-produced candidate which contains no endpoint, credential, or private key.
#[derive(Clone)]
pub struct PreparedManualSync {
    pub state: PersonalStateV2,
    pub membership: MembershipChain,
    pub checkpoint_anchor: CheckpointAnchor,
    pub expected_local_revision: u64,
    pub expected_private_revision: u64,
    pub local_device_id: DeviceId,
    pub summary: ManualSyncSummary,
}

pub struct AppliedManualSync {
    pub state: PersonalStateV2,
    pub summary: ManualSyncSummary,
}

impl PreparedManualSync {
    pub fn has_changes(&self) -> bool {
        self.summary.downloaded_operations > 0
            || self.summary.uploaded_operations > 0
            || self.summary.remote_writes > 0
    }
}

pub fn prepare_manual_sync(
    local_state: &PersonalStateV2,
    paths: &SyncPaths,
) -> Result<PreparedManualSync, SyncServiceError> {
    prepare_manual_sync_using(local_state, paths, open_saved_webdav_transport)
}

pub(super) fn prepare_manual_sync_using<T, F>(
    local_state: &PersonalStateV2,
    paths: &SyncPaths,
    open: F,
) -> Result<PreparedManualSync, SyncServiceError>
where
    T: VaultTransport,
    F: FnOnce(&WebDavProfile, &super::super::VaultCredential) -> Result<T, SyncServiceError>,
{
    let private_store = PrivateStore::new(paths.private_store())?;
    let private = private_store.load()?;
    let profile_store = WebDavProfileStore::new(paths.profile())?;
    let profile = profile_store.load(private.device())?;
    validate_active_context(local_state, &private, &profile)?;
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open(&profile, credential)?;
    // Setup or the fresh pairing join already proved the saved profile's WebDAV behavior.
    // Re-running that state-changing probe here would rewrite its marker on every no-op sync.
    prepare_manual_sync_with_transport(local_state, &private, &transport)
}

pub fn prepare_manual_sync_with_transport<T: VaultTransport + ?Sized>(
    local_state: &PersonalStateV2,
    private: &PrivateStoreSnapshot,
    transport: &T,
) -> Result<PreparedManualSync, SyncServiceError> {
    let mut budget = ManualSyncBudget::default();
    prepare_manual_sync_with_budget(local_state, private, transport, &mut budget)
}

pub(super) fn prepare_manual_sync_with_budget<T: VaultTransport + ?Sized>(
    local_state: &PersonalStateV2,
    private: &PrivateStoreSnapshot,
    transport: &T,
    budget: &mut ManualSyncBudget,
) -> Result<PreparedManualSync, SyncServiceError> {
    validate_active_private(local_state, private)?;
    let membership_anchor = membership_anchor(private)?;
    let remote_membership = load_remote_membership_with_budget(
        local_state,
        private,
        &membership_anchor,
        transport,
        budget,
    )?;
    let local_membership = membership_prefix_for_registry(
        &remote_membership,
        &membership_anchor,
        &local_state.device_registry,
    )?;
    let checkpoint_anchor = checkpoint_anchor(private)?;
    let input = ManualSyncInput {
        local_state,
        membership: &local_membership,
        membership_anchor: &membership_anchor,
        device: private.device(),
        checkpoint_anchor: &checkpoint_anchor,
        bootstrap_checkpoint: None,
        expected_local_revision: local_state.revision,
    };
    let ManualSyncCandidate {
        state,
        membership,
        checkpoint_anchor,
        expected_local_revision,
        summary,
    } = ManualSyncEngine::new(transport).synchronize_with_budget(
        &input,
        &|expected| {
            if expected == local_state.revision {
                Ok(())
            } else {
                Err(VaultError::RevisionConflict)
            }
        },
        budget,
    )?;
    Ok(PreparedManualSync {
        state,
        membership,
        checkpoint_anchor,
        expected_local_revision,
        expected_private_revision: private.revision(),
        local_device_id: DeviceId::new(private.device_id())
            .map_err(|_| SyncServiceError::InvalidRemoteData)?,
        summary,
    })
}

/// Install a candidate only while both its ledger and private rollback-anchor revisions match.
pub fn apply_manual_sync(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    candidate: PreparedManualSync,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
) -> Result<PersonalStateV2, SyncServiceError> {
    if current_state.revision != candidate.expected_local_revision {
        return Err(SyncServiceError::LocalStateChanged);
    }
    let private_store = PrivateStore::new(sync_paths.private_store())?;
    let mut private = private_store.load()?;
    if private.revision() != candidate.expected_private_revision
        || private.device_id() != candidate.local_device_id.as_str()
        || private.dataset_id() != candidate.state.dataset_id
        || private.enrollment() != EnrollmentState::Active
    {
        return Err(SyncServiceError::LocalStateChanged);
    }
    let checkpoint_sequence = candidate.checkpoint_anchor.checkpoint_sequence;
    let checkpoint_hash = candidate
        .checkpoint_anchor
        .checkpoint_hash
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    let checkpoint_unchanged = private.checkpoint_sequence() == Some(checkpoint_sequence)
        && private.checkpoint_hash() == Some(checkpoint_hash.as_str());
    let prepared = PersonalStateCommit::prepare_for_runtime(candidate.state, playlist_revision)?;
    if checkpoint_unchanged {
        return prepared.commit(personal_paths).map_err(Into::into);
    }
    private.advance_checkpoint(checkpoint_sequence, checkpoint_hash.clone())?;
    commit_with_anchor_transition(
        current_state,
        &prepared,
        personal_paths,
        sync_paths,
        &mut private,
        AnchorActivationKind::ManualSync,
        checkpoint_sequence,
        &checkpoint_hash,
        playlist_revision,
    )
}

/// Run one bidirectional sync and retain only redacted health/audit details locally.
///
/// The network phase still returns a detached candidate; installation performs the same exact
/// ledger and private-store revision checks as callers which schedule preparation elsewhere.
pub fn sync_now(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
) -> Result<AppliedManualSync, SyncServiceError> {
    if !super::status::read_status(sync_paths)?.configured {
        return Err(SyncServiceError::NotConfigured);
    }
    let now = crate::signals::unix_now();
    record_sync_started(sync_paths, now)?;
    let result = (|| {
        let candidate = prepare_manual_sync(current_state, sync_paths)?;
        let summary = candidate.summary.clone();
        let state = apply_manual_sync(
            current_state,
            playlist_revision,
            candidate,
            personal_paths,
            sync_paths,
        )?;
        Ok(AppliedManualSync { state, summary })
    })();
    match &result {
        Ok(applied) => {
            let _ = record_sync_success(sync_paths, now, &applied.summary);
        }
        Err(error) => {
            // A health/audit write must never replace the authoritative sync error. These stores
            // contain operational hints only; a later status read can recover to Needs attention.
            let _ = record_sync_failure(sync_paths, now, *error);
        }
    }
    result
}

/// Apply a detached network result on the primary owner lane and retain redacted status.
pub fn apply_prepared_sync_now(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    candidate: PreparedManualSync,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
) -> Result<AppliedManualSync, SyncServiceError> {
    let now = crate::signals::unix_now();
    record_sync_started(sync_paths, now)?;
    let summary = candidate.summary.clone();
    let result = apply_manual_sync(
        current_state,
        playlist_revision,
        candidate,
        personal_paths,
        sync_paths,
    )
    .map(|state| AppliedManualSync { state, summary });
    match &result {
        Ok(applied) => {
            let _ = record_sync_success(sync_paths, now, &applied.summary);
        }
        Err(error) => {
            let _ = record_sync_failure(sync_paths, now, *error);
        }
    }
    result
}

pub(super) fn record_sync_started(
    paths: &SyncPaths,
    now_unix: i64,
) -> Result<(), SyncServiceError> {
    let store = SyncHealthStore::new(paths.health())?;
    let current = store.load(true)?;
    let _ = store.save(&current, current.syncing(now_unix))?;
    Ok(())
}

pub(super) fn record_sync_success(
    paths: &SyncPaths,
    now_unix: i64,
    summary: &ManualSyncSummary,
) -> Result<(), SyncServiceError> {
    let health_store = SyncHealthStore::new(paths.health())?;
    let current = health_store.load(true)?;
    let _ = health_store.save(&current, current.succeeded(now_unix))?;
    let outcome = if summary.downloaded_operations == 0 && summary.uploaded_operations == 0 {
        SyncAuditOutcome::NoChanges
    } else {
        SyncAuditOutcome::Succeeded
    };
    let mut entry = SyncAuditEntry::new(now_unix, SyncAuditAction::ManualSync, outcome)?;
    entry.local_operations = summary.uploaded_operations;
    entry.remote_operations = summary.downloaded_operations;
    let _ = SyncAuditStore::new(paths.audit())?.append(now_unix, entry)?;
    Ok(())
}

pub(super) fn record_sync_failure(
    paths: &SyncPaths,
    now_unix: i64,
    error: SyncServiceError,
) -> Result<(), SyncServiceError> {
    let failure = error
        .failure_kind()
        .unwrap_or(SyncFailureKind::InvalidRemoteData);
    let health_store = SyncHealthStore::new(paths.health())?;
    let current = health_store.load(true)?;
    let _ = health_store.save(&current, current.failed(now_unix, failure))?;
    let mut entry = SyncAuditEntry::new(
        now_unix,
        SyncAuditAction::ManualSync,
        SyncAuditOutcome::Failed,
    )?;
    entry.failure = Some(failure);
    let _ = SyncAuditStore::new(paths.audit())?.append(now_unix, entry)?;
    Ok(())
}

pub(super) fn validate_active_context(
    state: &PersonalStateV2,
    private: &PrivateStoreSnapshot,
    profile: &WebDavProfile,
) -> Result<(), SyncServiceError> {
    validate_active_private(state, private)?;
    if profile.dataset_id() != state.dataset_id
        || profile.device_id() != private.device_id()
        || private.credential().is_none()
    {
        return Err(SyncServiceError::NotConfigured);
    }
    Ok(())
}

pub(super) fn validate_active_private(
    state: &PersonalStateV2,
    private: &PrivateStoreSnapshot,
) -> Result<(), SyncServiceError> {
    match private.enrollment() {
        EnrollmentState::Active => {}
        EnrollmentState::PendingApproval | EnrollmentState::PendingLedgerCommit => {
            return Err(SyncServiceError::PendingApproval);
        }
        EnrollmentState::Revoked => return Err(SyncServiceError::Revoked),
    }
    if private.dataset_id() != state.dataset_id {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    let device_id =
        DeviceId::new(private.device_id()).map_err(|_| SyncServiceError::InvalidRemoteData)?;
    let record = state
        .device_registry
        .get(&device_id)
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    if !private.device().matches_personal_record(record) {
        return Err(SyncServiceError::Revoked);
    }
    Ok(())
}

pub(super) fn membership_anchor(
    private: &PrivateStoreSnapshot,
) -> Result<MembershipAnchor, SyncServiceError> {
    private
        .membership_root_hash()
        .map(|hash| MembershipAnchor::RootHash(hash.to_owned()))
        .ok_or(SyncServiceError::InvalidRemoteData)
}

pub(super) fn checkpoint_anchor(
    private: &PrivateStoreSnapshot,
) -> Result<CheckpointAnchor, SyncServiceError> {
    match (private.checkpoint_sequence(), private.checkpoint_hash()) {
        (Some(sequence), Some(hash)) => {
            CheckpointAnchor::from_trusted(sequence, hash.to_owned()).map_err(Into::into)
        }
        _ => Err(SyncServiceError::InvalidRemoteData),
    }
}

pub(super) fn load_remote_membership<T: VaultTransport + ?Sized>(
    state: &PersonalStateV2,
    private: &PrivateStoreSnapshot,
    anchor: &MembershipAnchor,
    transport: &T,
) -> Result<MembershipChain, SyncServiceError> {
    let mut budget = ManualSyncBudget::default();
    load_remote_membership_with_budget(state, private, anchor, transport, &mut budget)
}

pub(super) fn load_remote_membership_with_budget<T: VaultTransport + ?Sized>(
    state: &PersonalStateV2,
    private: &PrivateStoreSnapshot,
    anchor: &MembershipAnchor,
    transport: &T,
    budget: &mut ManualSyncBudget,
) -> Result<MembershipChain, SyncServiceError> {
    let key = manifest_key(&state.dataset_id)?;
    let (encrypted, _) = budget
        .get(transport, &key, MAX_VAULT_PAYLOAD_BYTES)?
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    let manifest = super::super::manual::SignedVaultManifest::decrypt_for_device(
        &encrypted,
        private.device(),
        anchor,
    )?;
    if manifest.payload.dataset_id != state.dataset_id {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    Ok(manifest.payload.membership)
}

pub(super) fn membership_prefix_for_registry(
    remote: &MembershipChain,
    anchor: &MembershipAnchor,
    registry: &DeviceRegistry,
) -> Result<MembershipChain, SyncServiceError> {
    let mut candidate = MembershipChain::new(remote.root.clone());
    if candidate.verify(anchor)?.devices == *registry {
        return Ok(candidate);
    }
    for change in &remote.changes {
        candidate.changes.push(change.clone());
        if candidate.verify(anchor)?.devices == *registry {
            return Ok(candidate);
        }
    }
    Err(SyncServiceError::InvalidRemoteData)
}
