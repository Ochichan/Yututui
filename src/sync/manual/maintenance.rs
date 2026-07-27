//! Authenticated compaction acknowledgement and conservative remote garbage collection.
//!
//! A device may acknowledge only the exact signed checkpoint already recorded in its durable
//! private rollback anchor. The lowest active device performs deletion only after every device
//! active in that checkpoint's membership epoch has published the same authenticated
//! acknowledgement.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::personal_state::{CompactionCheckpoint, DeviceId, engagement_compaction_leader};

use super::protocol::segment_bounds;
use super::{LocalRevisionGuard, ManualSyncBudget, ManualSyncInput};
use crate::sync::{
    EncryptedObject, ObjectCondition, ObjectDeleteResult, ObjectKey, ObjectMetadata,
    ObjectWriteResult, SignedCheckpoint, SignedCompactionAck, VaultDeadline, VaultError,
    VaultTransport, VerifiedMembership, compaction_ack_key, compaction_ack_prefix,
    compaction_quorum,
};

const MAX_ACK_BYTES: usize = 2 * 1024 * 1024;
const RETAINED_CHECKPOINTS: usize = 3;
const GC_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub(super) struct MaintenanceSummary {
    pub(super) acknowledgements_written: usize,
    pub(super) quorum: Option<CompactionQuorumEvidence>,
    pub(super) segments_deleted: usize,
    pub(super) checkpoints_deleted: usize,
    pub(super) obsolete_acknowledgements_retained: usize,
    pub(super) gc_deferred: bool,
    pub(super) remote_writes: usize,
}

#[derive(Debug, Clone)]
pub(super) struct CompactionQuorumEvidence {
    compaction: CompactionCheckpoint,
    membership_epoch: u64,
    membership_head_hash: String,
    installed_checkpoint_sequence: u64,
    installed_checkpoint_hash: String,
    minimum_installed_checkpoint_sequence: u64,
}

impl CompactionQuorumEvidence {
    pub(super) fn authorizes(
        &self,
        compaction: &CompactionCheckpoint,
        membership: &VerifiedMembership,
    ) -> bool {
        self.compaction == *compaction
            && self.membership_epoch == membership.epoch
            && self.membership_head_hash == membership.head_hash
            && self.installed_checkpoint_sequence > 0
            && !self.installed_checkpoint_hash.is_empty()
            && self.minimum_installed_checkpoint_sequence > 0
    }
}

#[derive(Debug, Default)]
struct DeletionOutcome {
    deleted: usize,
    deferred: bool,
    acknowledgements_verified: bool,
}

#[derive(Debug, Default)]
struct ResidualOutcome {
    retained: usize,
    deferred: bool,
}

pub(super) fn acknowledge_and_collect<T: VaultTransport + ?Sized, G: LocalRevisionGuard>(
    transport: &T,
    input: &ManualSyncInput<'_>,
    revision_guard: &G,
    checkpoint: &SignedCheckpoint,
    membership: &VerifiedMembership,
    budget: &mut ManualSyncBudget,
) -> Result<MaintenanceSummary, VaultError> {
    let checkpoint_hash = checkpoint.hash()?;
    if input.checkpoint_anchor.checkpoint_sequence != checkpoint.payload.checkpoint_sequence
        || input.checkpoint_anchor.checkpoint_hash.as_deref() != Some(checkpoint_hash.as_str())
    {
        return Ok(MaintenanceSummary::default());
    }
    let Some(compaction) = checkpoint.payload.state.compaction_checkpoint.as_ref() else {
        return Ok(MaintenanceSummary::default());
    };
    let local_device_id =
        DeviceId::new(input.device.device_id()).map_err(|_| VaultError::InvalidDeviceIdentity)?;
    let acknowledgement = SignedCompactionAck::create(
        &checkpoint.payload.dataset_id,
        compaction,
        checkpoint.payload.checkpoint_sequence,
        &checkpoint_hash,
        membership,
        local_device_id.clone(),
        input.device,
    )?;
    let acknowledgement_key = compaction_ack_key(
        &checkpoint.payload.dataset_id,
        compaction,
        membership,
        &local_device_id,
    )?;
    let encrypted = acknowledgement.encrypt(compaction, membership)?;
    revision_guard.ensure_current(input.expected_local_revision)?;
    let wrote = put_acknowledgement(
        transport,
        input,
        membership,
        compaction,
        &acknowledgement_key,
        &acknowledgement,
        &encrypted,
        budget,
    )?;

    let acknowledgements = load_acknowledgements(transport, input, membership, compaction, budget)?;
    let mut summary = MaintenanceSummary {
        acknowledgements_written: usize::from(wrote),
        remote_writes: usize::from(wrote),
        ..MaintenanceSummary::default()
    };
    let has_quorum = compaction_quorum(&acknowledgements, compaction, membership)?;
    if engagement_compaction_leader(&checkpoint.payload.state).as_ref() != Some(&local_device_id)
        || !has_quorum
    {
        return Ok(summary);
    }

    // Optional physical cleanup has its own short deadline and accounting. Exhausting it must not
    // consume the budget needed for the merge, segment upload, or manifest CAS below.
    let mut gc_budget = ManualSyncBudget::with_deadline(VaultDeadline::from_now(GC_DEADLINE));
    let checkpoint_gc = delete_old_checkpoints(
        transport,
        input,
        revision_guard,
        checkpoint,
        &checkpoint_hash,
        &acknowledgements,
        &mut gc_budget,
    )?;
    let segment_gc = if checkpoint_gc.acknowledgements_verified {
        delete_covered_segments(transport, input, revision_guard, compaction, &mut gc_budget)?
    } else {
        DeletionOutcome {
            deferred: true,
            ..DeletionOutcome::default()
        }
    };
    let acknowledgement_residual =
        count_obsolete_acknowledgements(transport, input, compaction, membership, &mut gc_budget)?;
    summary.segments_deleted = segment_gc.deleted;
    summary.checkpoints_deleted = checkpoint_gc.deleted;
    summary.obsolete_acknowledgements_retained = acknowledgement_residual.retained;
    summary.gc_deferred =
        segment_gc.deferred || checkpoint_gc.deferred || acknowledgement_residual.deferred;
    if checkpoint_gc.acknowledgements_verified {
        summary.quorum = Some(CompactionQuorumEvidence {
            compaction: compaction.clone(),
            membership_epoch: membership.epoch,
            membership_head_hash: membership.head_hash.clone(),
            installed_checkpoint_sequence: checkpoint.payload.checkpoint_sequence,
            installed_checkpoint_hash: checkpoint_hash.clone(),
            minimum_installed_checkpoint_sequence: acknowledgements
                .iter()
                .map(|acknowledgement| acknowledgement.payload.installed_checkpoint_sequence)
                .min()
                .ok_or(VaultError::InvalidEncryptedObject)?,
        });
    }
    summary.remote_writes = summary
        .remote_writes
        .saturating_add(summary.segments_deleted)
        .saturating_add(summary.checkpoints_deleted);
    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
fn put_acknowledgement<T: VaultTransport + ?Sized>(
    transport: &T,
    input: &ManualSyncInput<'_>,
    membership: &VerifiedMembership,
    compaction: &CompactionCheckpoint,
    key: &ObjectKey,
    acknowledgement: &SignedCompactionAck,
    encrypted: &EncryptedObject,
    budget: &mut ManualSyncBudget,
) -> Result<bool, VaultError> {
    match budget.put(transport, key, encrypted, ObjectCondition::CreateOnly) {
        Ok(ObjectWriteResult::Created(_) | ObjectWriteResult::Updated(_)) => Ok(true),
        Ok(ObjectWriteResult::AlreadyPresent(_))
        | Err(VaultError::PreconditionFailed)
        | Err(VaultError::StorageFailed)
        | Err(VaultError::RemoteUnavailable) => {
            let Some((existing, metadata)) = budget.get(transport, key, MAX_ACK_BYTES)? else {
                return Err(VaultError::RemoteUnavailable);
            };
            let existing = SignedCompactionAck::decrypt_for_device(
                &existing,
                input.device,
                compaction,
                membership,
            )?;
            match compare_acknowledgement_high_water(&existing, acknowledgement)? {
                std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => return Ok(false),
                std::cmp::Ordering::Less => {}
            }
            match budget.put(
                transport,
                key,
                encrypted,
                ObjectCondition::Match(metadata.etag),
            ) {
                Ok(ObjectWriteResult::Created(_) | ObjectWriteResult::Updated(_)) => Ok(true),
                Ok(ObjectWriteResult::AlreadyPresent(_))
                | Err(VaultError::PreconditionFailed)
                | Err(VaultError::StorageFailed)
                | Err(VaultError::RemoteUnavailable) => {
                    let Some((readback, _)) = budget.get(transport, key, MAX_ACK_BYTES)? else {
                        return Err(VaultError::RemoteUnavailable);
                    };
                    let readback = SignedCompactionAck::decrypt_for_device(
                        &readback,
                        input.device,
                        compaction,
                        membership,
                    )?;
                    match compare_acknowledgement_high_water(&readback, acknowledgement)? {
                        std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => Ok(true),
                        std::cmp::Ordering::Less => Err(VaultError::PreconditionFailed),
                    }
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn compare_acknowledgement_high_water(
    existing: &SignedCompactionAck,
    candidate: &SignedCompactionAck,
) -> Result<std::cmp::Ordering, VaultError> {
    if existing.payload.signer_device_id != candidate.payload.signer_device_id
        || existing.payload.dataset_id != candidate.payload.dataset_id
        || existing.payload.compaction_id != candidate.payload.compaction_id
        || existing.payload.compaction_generation_hash
            != candidate.payload.compaction_generation_hash
        || existing.payload.membership_epoch != candidate.payload.membership_epoch
        || existing.payload.membership_head_hash != candidate.payload.membership_head_hash
    {
        return Err(VaultError::MembershipFork);
    }
    let ordering = existing
        .payload
        .installed_checkpoint_sequence
        .cmp(&candidate.payload.installed_checkpoint_sequence);
    if ordering.is_eq()
        && existing.payload.installed_checkpoint_hash != candidate.payload.installed_checkpoint_hash
    {
        return Err(VaultError::MembershipFork);
    }
    Ok(ordering)
}

fn load_acknowledgements<T: VaultTransport + ?Sized>(
    transport: &T,
    input: &ManualSyncInput<'_>,
    membership: &VerifiedMembership,
    compaction: &CompactionCheckpoint,
    budget: &mut ManualSyncBudget,
) -> Result<Vec<SignedCompactionAck>, VaultError> {
    let prefix = compaction_ack_prefix(&input.local_state.dataset_id, compaction, membership)?;
    let mut by_device = BTreeMap::new();
    for metadata in budget.list(transport, &prefix)? {
        let device_id = acknowledgement_device_id(&metadata.key)?;
        if metadata.key
            != compaction_ack_key(
                &input.local_state.dataset_id,
                compaction,
                membership,
                &device_id,
            )?
        {
            return Err(VaultError::InvalidObjectKey);
        }
        let (object, _) = budget
            .get(transport, &metadata.key, MAX_ACK_BYTES)?
            .ok_or(VaultError::SequenceGap)?;
        let acknowledgement =
            SignedCompactionAck::decrypt_for_device(&object, input.device, compaction, membership)?;
        if acknowledgement.payload.signer_device_id != device_id
            || by_device.insert(device_id, acknowledgement).is_some()
        {
            return Err(VaultError::MembershipFork);
        }
    }
    Ok(by_device.into_values().collect())
}

fn delete_covered_segments<T: VaultTransport + ?Sized, G: LocalRevisionGuard>(
    transport: &T,
    input: &ManualSyncInput<'_>,
    revision_guard: &G,
    compaction: &CompactionCheckpoint,
    budget: &mut ManualSyncBudget,
) -> Result<DeletionOutcome, VaultError> {
    let mut outcome = DeletionOutcome::default();
    for (device_id, covered_sequence) in &compaction.coverage.0 {
        let prefix = super::segment_prefix(&input.local_state.dataset_id, device_id)?;
        let metadata = match budget.list(transport, &prefix) {
            Ok(metadata) => metadata,
            Err(error @ VaultError::RemoteRateLimited(_)) => return Err(error),
            Err(_) => {
                outcome.deferred = true;
                continue;
            }
        };
        for metadata in metadata {
            let (first, last) = match segment_bounds(&metadata.key) {
                Ok(bounds) => bounds,
                Err(_) => {
                    outcome.deferred = true;
                    continue;
                }
            };
            if metadata.key
                != super::segment_key(&input.local_state.dataset_id, device_id, first, last)?
            {
                outcome.deferred = true;
                continue;
            }
            if last <= *covered_sequence {
                match delete_exact(transport, input, revision_guard, &metadata, budget) {
                    Ok(deleted) => outcome.deleted = outcome.deleted.saturating_add(deleted),
                    Err(VaultError::RevisionConflict) => {
                        return Err(VaultError::RevisionConflict);
                    }
                    Err(error @ VaultError::RemoteRateLimited(_)) => return Err(error),
                    Err(_) => outcome.deferred = true,
                }
            }
        }
    }
    Ok(outcome)
}

fn delete_old_checkpoints<T: VaultTransport + ?Sized, G: LocalRevisionGuard>(
    transport: &T,
    input: &ManualSyncInput<'_>,
    revision_guard: &G,
    latest: &SignedCheckpoint,
    latest_hash: &str,
    acknowledgements: &[SignedCompactionAck],
    budget: &mut ManualSyncBudget,
) -> Result<DeletionOutcome, VaultError> {
    let mut outcome = DeletionOutcome::default();
    let prefix = super::checkpoint_prefix(&latest.payload.dataset_id)?;
    let mut checkpoints = BTreeMap::<String, (SignedCheckpoint, ObjectMetadata)>::new();
    let metadata = match budget.list(transport, &prefix) {
        Ok(metadata) => metadata,
        Err(error @ VaultError::RemoteRateLimited(_)) => return Err(error),
        Err(_) => {
            outcome.deferred = true;
            return Ok(outcome);
        }
    };
    for metadata in metadata {
        let Some(hash) = checkpoint_hash_from_key(&metadata.key) else {
            outcome.deferred = true;
            continue;
        };
        let object = match budget.get(
            transport,
            &metadata.key,
            crate::sync::MAX_VAULT_PAYLOAD_BYTES,
        ) {
            Ok(Some((object, _))) => object,
            Err(error @ VaultError::RemoteRateLimited(_)) => return Err(error),
            Ok(None) | Err(_) => {
                outcome.deferred = true;
                continue;
            }
        };
        let checkpoint = match SignedCheckpoint::decrypt_for_device(
            &object,
            input.device,
            input.membership_anchor,
        ) {
            Ok(checkpoint) => checkpoint,
            Err(_) => {
                outcome.deferred = true;
                continue;
            }
        };
        if checkpoint.hash()? != hash
            || metadata.key
                != super::checkpoint_key(
                    &latest.payload.dataset_id,
                    checkpoint.payload.membership_epoch,
                    &hash,
                )?
            || checkpoints.insert(hash, (checkpoint, metadata)).is_some()
        {
            outcome.deferred = true;
        }
    }
    let Some((stored_latest, _)) = checkpoints.get(latest_hash) else {
        outcome.deferred = true;
        return Ok(outcome);
    };
    if stored_latest != latest {
        outcome.deferred = true;
        return Ok(outcome);
    }

    let minimum_installed_sequence = acknowledgements
        .iter()
        .map(|acknowledgement| acknowledgement.payload.installed_checkpoint_sequence)
        .min()
        .ok_or(VaultError::InvalidEncryptedObject)?;
    let mut current_hash = latest_hash.to_owned();
    let mut visited = BTreeSet::new();
    let mut lineage = Vec::new();
    let mut required_bridge_verified = false;
    while let Some((current, metadata)) = checkpoints.get(&current_hash) {
        if !visited.insert(current_hash.clone()) {
            outcome.deferred = true;
            return Ok(outcome);
        }
        lineage.push((current_hash.clone(), current.clone(), metadata.clone()));
        if current.payload.checkpoint_sequence <= minimum_installed_sequence
            && lineage.len() >= RETAINED_CHECKPOINTS
        {
            required_bridge_verified = true;
        }
        let previous_hash = current.payload.previous_checkpoint_hash.clone();
        let Some(previous_hash) = previous_hash else {
            break;
        };
        let Some((previous, _)) = checkpoints.get(&previous_hash) else {
            if !required_bridge_verified {
                outcome.deferred = true;
                return Ok(outcome);
            }
            break;
        };
        if previous.payload.checkpoint_sequence.saturating_add(1)
            != current.payload.checkpoint_sequence
        {
            outcome.deferred = true;
            return Ok(outcome);
        }
        current_hash = previous_hash;
    }

    let lineage_by_sequence = lineage
        .iter()
        .map(|(hash, checkpoint, _)| (checkpoint.payload.checkpoint_sequence, hash.as_str()))
        .collect::<BTreeMap<_, _>>();
    for acknowledgement in acknowledgements {
        if lineage_by_sequence
            .get(&acknowledgement.payload.installed_checkpoint_sequence)
            .copied()
            != Some(acknowledgement.payload.installed_checkpoint_hash.as_str())
        {
            outcome.deferred = true;
            return Ok(outcome);
        }
    }
    outcome.acknowledgements_verified = true;
    let lineage_hashes = lineage
        .iter()
        .map(|(hash, _, _)| hash.as_str())
        .collect::<BTreeSet<_>>();
    if checkpoints
        .keys()
        .any(|hash| !lineage_hashes.contains(hash.as_str()))
    {
        outcome.deferred = true;
    }
    for (depth, (_, checkpoint, metadata)) in lineage.into_iter().enumerate() {
        if depth < RETAINED_CHECKPOINTS
            || checkpoint.payload.checkpoint_sequence >= minimum_installed_sequence
        {
            continue;
        }
        match delete_exact(transport, input, revision_guard, &metadata, budget) {
            Ok(deleted) => outcome.deleted = outcome.deleted.saturating_add(deleted),
            Err(VaultError::RevisionConflict) => return Err(VaultError::RevisionConflict),
            Err(error @ VaultError::RemoteRateLimited(_)) => return Err(error),
            Err(_) => outcome.deferred = true,
        }
    }
    Ok(outcome)
}

fn count_obsolete_acknowledgements<T: VaultTransport + ?Sized>(
    transport: &T,
    input: &ManualSyncInput<'_>,
    compaction: &CompactionCheckpoint,
    membership: &VerifiedMembership,
    budget: &mut ManualSyncBudget,
) -> Result<ResidualOutcome, VaultError> {
    let mut outcome = ResidualOutcome::default();
    let root = ObjectKey::new(format!(
        "yututui/v2/{}/compaction-acks",
        input.local_state.dataset_id
    ))?;
    let current = compaction_ack_prefix(&input.local_state.dataset_id, compaction, membership)?;
    let current_prefix = format!("{}/", current.as_str());
    let metadata = match budget.list(transport, &root) {
        Ok(metadata) => metadata,
        Err(error @ VaultError::RemoteRateLimited(_)) => return Err(error),
        Err(_) => {
            outcome.deferred = true;
            return Ok(outcome);
        }
    };
    for metadata in metadata {
        if metadata.key.as_str().starts_with(&current_prefix) {
            continue;
        }
        if !valid_acknowledgement_key(&root, &metadata.key) {
            outcome.deferred = true;
            continue;
        }
        // A non-current namespace may belong to a newer manifest observed by another client.
        // Retain it unless its exact introducing checkpoint is authenticated as an ancestor being
        // retired. We currently report the residual instead of risking a stale-client deletion.
        outcome.retained = outcome.retained.saturating_add(1);
    }
    Ok(outcome)
}

fn delete_exact<T: VaultTransport + ?Sized, G: LocalRevisionGuard>(
    transport: &T,
    input: &ManualSyncInput<'_>,
    revision_guard: &G,
    metadata: &ObjectMetadata,
    budget: &mut ManualSyncBudget,
) -> Result<usize, VaultError> {
    revision_guard.ensure_current(input.expected_local_revision)?;
    match budget.delete(transport, &metadata.key, &metadata.etag)? {
        ObjectDeleteResult::Deleted => Ok(1),
        ObjectDeleteResult::AlreadyAbsent => Ok(0),
    }
}

fn acknowledgement_device_id(key: &ObjectKey) -> Result<DeviceId, VaultError> {
    key.as_str()
        .rsplit('/')
        .next()
        .and_then(|file| file.strip_suffix(".age"))
        .ok_or(VaultError::InvalidObjectKey)
        .and_then(|device_id| DeviceId::new(device_id).map_err(|_| VaultError::InvalidObjectKey))
}

fn valid_acknowledgement_key(root: &ObjectKey, key: &ObjectKey) -> bool {
    let Some(relative) = key
        .as_str()
        .strip_prefix(root.as_str())
        .and_then(|relative| relative.strip_prefix('/'))
    else {
        return false;
    };
    let mut components = relative.split('/');
    let (Some(scope), Some(device_file), None) =
        (components.next(), components.next(), components.next())
    else {
        return false;
    };
    is_ack_path_scope(scope)
        && device_file
            .strip_suffix(".age")
            .and_then(|device_id| DeviceId::new(device_id).ok())
            .is_some()
}

/// Matches the single derived component produced by `compaction_ack_prefix`.
fn is_ack_path_scope(scope: &str) -> bool {
    scope.len() == crate::sync::compaction::ACK_PATH_SCOPE_CHARS
        && scope
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn checkpoint_hash_from_key(key: &ObjectKey) -> Option<String> {
    let hash = key.as_str().rsplit('/').next()?.strip_suffix(".age")?;
    is_lower_hash(hash).then(|| hash.to_owned())
}

fn is_lower_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod tests;
