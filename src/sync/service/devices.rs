use serde::{Deserialize, Serialize};

use crate::personal_state::{
    DeviceId, DeviceRecord, Operation, PersonalStateV2, append_operation_as,
};

use super::super::manual::ManualSyncBudget;
use super::super::{
    EnrollmentState, MembershipAction, PrivateStore, SyncAuditAction, SyncAuditEntry,
    SyncAuditOutcome, SyncAuditStore, SyncHealthStore, SyncPaths, VaultTransport,
    WebDavProfileStore,
};
use super::manual::{
    AppliedManualSync, PreparedManualSync, membership_anchor, prepare_manual_sync_with_budget,
    record_sync_failure, record_sync_started, validate_active_context,
};
use super::{SyncServiceError, apply_manual_sync, open_saved_webdav_transport};

const MAX_REVOKE_RACE_ATTEMPTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub device_id: String,
    pub name: String,
    pub active: bool,
    pub keyed: bool,
}

impl From<&DeviceRecord> for DeviceSummary {
    fn from(device: &DeviceRecord) -> Self {
        Self {
            device_id: device.device_id.as_str().to_owned(),
            name: device.name.clone(),
            active: !device.revoked,
            keyed: device.public_identity.is_some(),
        }
    }
}

pub fn revoke_device(
    current_state: &PersonalStateV2,
    target: &DeviceId,
    paths: &SyncPaths,
) -> Result<PreparedManualSync, SyncServiceError> {
    let private_store = PrivateStore::new(paths.private_store())?;
    let private = private_store.load()?;
    let profile = WebDavProfileStore::new(paths.profile())?.load(private.device())?;
    validate_active_context(current_state, &private, &profile)?;
    if private.enrollment() != EnrollmentState::Active {
        return Err(SyncServiceError::PendingApproval);
    }
    let local_device =
        DeviceId::new(private.device_id()).map_err(|_| SyncServiceError::InvalidRemoteData)?;
    if target == &local_device {
        return Err(SyncServiceError::Revoked);
    }
    let target_record = current_state
        .device_registry
        .get(target)
        .filter(|device| !device.revoked)
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    if target_record.public_identity.is_none()
        || current_state
            .device_registry
            .values()
            .filter(|device| !device.revoked && device.public_identity.is_some())
            .count()
            <= 1
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open_saved_webdav_transport(&profile, credential)?;
    // The persisted profile was capability-tested during setup or the fresh pairing join.
    // Revocation shares the normal sync transport and must not rewrite the probe marker.
    revoke_with_transport(current_state, target, &private, &transport)
}

#[allow(clippy::too_many_arguments)]
pub fn revoke_device_now(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    target: &DeviceId,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
) -> Result<AppliedManualSync, SyncServiceError> {
    if !super::status::read_status(sync_paths)?.configured {
        return Err(SyncServiceError::NotConfigured);
    }
    let now = crate::signals::unix_now();
    record_sync_started(sync_paths, now)?;
    let result = revoke_device(current_state, target, sync_paths).and_then(|candidate| {
        apply_prepared_revoke_now_inner(
            current_state,
            playlist_revision,
            target,
            candidate,
            personal_paths,
            sync_paths,
            now,
        )
    });
    if let Err(error) = &result {
        let _ = record_sync_failure(sync_paths, now, *error);
    }
    result
}

#[allow(clippy::too_many_arguments)]
pub fn apply_prepared_revoke_now(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    target: &DeviceId,
    candidate: PreparedManualSync,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
) -> Result<AppliedManualSync, SyncServiceError> {
    let now = crate::signals::unix_now();
    record_sync_started(sync_paths, now)?;
    let result = apply_prepared_revoke_now_inner(
        current_state,
        playlist_revision,
        target,
        candidate,
        personal_paths,
        sync_paths,
        now,
    );
    if let Err(error) = &result {
        let _ = record_sync_failure(sync_paths, now, *error);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn apply_prepared_revoke_now_inner(
    current_state: &PersonalStateV2,
    playlist_revision: u64,
    target: &DeviceId,
    candidate: PreparedManualSync,
    personal_paths: &crate::personal_state::PersonalStatePaths,
    sync_paths: &SyncPaths,
    now: i64,
) -> Result<AppliedManualSync, SyncServiceError> {
    let summary = candidate.summary.clone();
    let state = apply_manual_sync(
        current_state,
        playlist_revision,
        candidate,
        personal_paths,
        sync_paths,
    )?;
    record_revoke_success(sync_paths, now, target, &summary)?;
    Ok(AppliedManualSync { state, summary })
}

pub(super) fn record_revoke_success(
    sync_paths: &SyncPaths,
    now: i64,
    target: &DeviceId,
    summary: &super::super::manual::ManualSyncSummary,
) -> Result<(), SyncServiceError> {
    let health_store = SyncHealthStore::new(sync_paths.health())?;
    let current = health_store.load(true)?;
    let _ = health_store.save(&current, current.succeeded(now))?;
    let mut entry = SyncAuditEntry::new(
        now,
        SyncAuditAction::RevokeDevice,
        SyncAuditOutcome::Succeeded,
    )?;
    entry.device_id = Some(target.as_str().to_owned());
    entry.local_operations = summary.uploaded_operations;
    entry.remote_operations = summary.downloaded_operations;
    let _ = SyncAuditStore::new(sync_paths.audit())?.append(now, entry)?;
    Ok(())
}

fn revoke_with_transport<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    target: &DeviceId,
    private: &super::super::PrivateStoreSnapshot,
    transport: &T,
) -> Result<PreparedManualSync, SyncServiceError> {
    let mut budget = ManualSyncBudget::default();
    revoke_with_transport_and_budget(current_state, target, private, transport, &mut budget)
}

fn revoke_with_transport_and_budget<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    target: &DeviceId,
    private: &super::super::PrivateStoreSnapshot,
    transport: &T,
    budget: &mut ManualSyncBudget,
) -> Result<PreparedManualSync, SyncServiceError> {
    let mut completed_summary = super::super::manual::ManualSyncSummary::default();
    for attempt in 1..=MAX_REVOKE_RACE_ATTEMPTS {
        // Establish the target's latest authenticated high-water before freezing the revocation
        // cutoff. Revoking directly from the caller's snapshot can strand a valid unseen segment:
        // the resulting checkpoint must attest the exact accepted sequence.
        let synced = prepare_manual_sync_with_budget(current_state, private, transport, budget)?;
        merge_summary(&mut completed_summary, &synced.summary);
        if synced
            .state
            .device_registry
            .get(target)
            .is_some_and(|device| device.revoked)
        {
            return Ok(PreparedManualSync {
                summary: completed_summary,
                ..synced
            });
        }
        match revoke_synced_state(current_state, target, private, transport, synced, budget) {
            Ok(mut candidate) => {
                merge_summary(&mut completed_summary, &candidate.summary);
                candidate.summary = completed_summary;
                return Ok(candidate);
            }
            // A still-active target may publish after the first merge and before the conditional
            // revocation manifest. Nothing has made the stale cutoff visible in that case; fetch
            // the new high-water and rebuild the signed membership change.
            Err(SyncServiceError::InvalidRemoteData | SyncServiceError::Revoked)
                if attempt < MAX_REVOKE_RACE_ATTEMPTS => {}
            Err(error) => return Err(error),
        }
    }
    Err(SyncServiceError::InvalidRemoteData)
}

fn revoke_synced_state<T: VaultTransport + ?Sized>(
    original_state: &PersonalStateV2,
    target: &DeviceId,
    private: &super::super::PrivateStoreSnapshot,
    transport: &T,
    synced: PreparedManualSync,
    budget: &mut ManualSyncBudget,
) -> Result<PreparedManualSync, SyncServiceError> {
    let anchor = membership_anchor(private)?;
    let PreparedManualSync {
        state: synced_state,
        mut membership,
        checkpoint_anchor,
        ..
    } = synced;
    let local_device =
        DeviceId::new(private.device_id()).map_err(|_| SyncServiceError::InvalidRemoteData)?;
    let cutoff = synced_state.version_vector.observed(target);
    membership.append_device_action(
        &anchor,
        &local_device,
        private.device().signing_key(),
        MembershipAction::RevokeDevice {
            device_id: target.clone(),
            last_accepted_sequence: cutoff,
        },
    )?;
    let state = append_operation_as(
        &synced_state,
        &local_device,
        Operation::RevokeDevice {
            device_id: target.clone(),
        },
        crate::signals::unix_now(),
    )?;
    let input = super::super::manual::ManualSyncInput {
        local_state: &state,
        membership: &membership,
        membership_anchor: &anchor,
        device: private.device(),
        checkpoint_anchor: &checkpoint_anchor,
        bootstrap_checkpoint: None,
        expected_local_revision: state.revision,
    };
    let candidate = super::super::manual::ManualSyncEngine::new(transport)
        .synchronize_with_budget(
            &input,
            &|expected| {
                if expected == state.revision {
                    Ok(())
                } else {
                    Err(super::super::VaultError::RevisionConflict)
                }
            },
            budget,
        )?;
    Ok(PreparedManualSync {
        state: candidate.state,
        membership: candidate.membership,
        checkpoint_anchor: candidate.checkpoint_anchor,
        expected_local_revision: original_state.revision,
        expected_private_revision: private.revision(),
        local_device_id: local_device,
        summary: candidate.summary,
    })
}

fn merge_summary(
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

#[cfg(test)]
#[path = "devices/tests.rs"]
mod tests;
