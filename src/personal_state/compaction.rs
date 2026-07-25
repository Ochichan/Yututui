//! Deterministic, network-independent engagement compaction.
//!
//! This module only builds and merges the canonical personal-state checkpoint. Publishing that
//! checkpoint, collecting authenticated device acknowledgements, and deleting remote objects are
//! sync-protocol responsibilities.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::legacy::{ENGAGEMENT_EVENTS_MAX, sha256_hex};
use super::reducer::{project_at, stamp_order};
use super::{
    CompactionCheckpoint, DeviceId, Operation, OperationEnvelope, PersonalStateError,
    PersonalStateV2, VersionVector,
};

const COMPACTION_HASH_KIND: &str = "yututui-engagement-compaction-v1";
pub(crate) const RAW_EVENT_RETENTION_SECS: i64 = 365 * 24 * 60 * 60;

/// A deterministic local candidate. It contains no credential, endpoint, or network result.
#[derive(Debug, Clone, PartialEq)]
pub struct EngagementCompactionPlan {
    pub candidate: PersonalStateV2,
    pub pruned_engagement_operations: usize,
}

/// The lowest active device id is the sole compaction leader.
pub fn engagement_compaction_leader(state: &PersonalStateV2) -> Option<DeviceId> {
    state
        .device_registry
        .values()
        .filter(|device| !device.revoked && device.device_id.as_str() != "legacy")
        .map(|device| device.device_id.clone())
        .min()
}

/// Build a projection-preserving engagement compaction candidate.
///
/// `now_unix` is injected because wall time decides only the 365-day retention boundary. Given
/// identical state and `now_unix`, the candidate and checkpoint id are byte-for-byte deterministic.
/// `Ok(None)` means the ledger already contains exactly the bounded raw engagement window.
pub fn plan_engagement_compaction(
    state: &PersonalStateV2,
    local_device_id: &DeviceId,
    now_unix: i64,
    prior_compaction_has_quorum: bool,
) -> Result<Option<EngagementCompactionPlan>, PersonalStateError> {
    state.validate()?;
    let leader = engagement_compaction_leader(state).ok_or(
        PersonalStateError::InvalidOperation("personal state has no active compaction leader"),
    )?;
    if leader != *local_device_id {
        return Err(PersonalStateError::InvalidOperation(
            "only the lowest active device may compact personal state",
        ));
    }
    // Every active device must durably install the signed vault checkpoint containing the current
    // compaction before the leader may replace it. Otherwise an offline device could skip an
    // intermediate generation and no longer prove which raw events the grandchild removed.
    if state.compaction_checkpoint.is_some() && !prior_compaction_has_quorum {
        return Ok(None);
    }

    let retained = retained_engagement_operation_ids(state.operations.as_slice(), now_unix)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let pruned_engagement_operations = state
        .operations
        .iter()
        .filter(|operation| {
            matches!(operation.operation, Operation::RecordEngagement { .. })
                && !retained.contains(operation.operation_id.as_str())
        })
        .count();
    if pruned_engagement_operations == 0 {
        return Ok(None);
    }
    let _ = state.next_revision()?;

    let coverage = state.version_vector.clone();
    let previous_checkpoint_hash = state
        .compaction_checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.checkpoint_id.clone());
    let compaction_generation =
        state
            .compaction_checkpoint
            .as_ref()
            .map_or(Ok(1), |checkpoint| {
                checkpoint.compaction_generation.checked_add(1).ok_or(
                    PersonalStateError::InvalidOperation("compaction generation exhausted"),
                )
            })?;
    let checkpoint_id = checkpoint_id_for(
        &state.dataset_id,
        compaction_generation,
        &coverage,
        previous_checkpoint_hash.as_deref(),
        &retained,
        &state.operations,
    )?;
    let checkpoint = CompactionCheckpoint {
        checkpoint_id,
        compaction_generation,
        coverage,
        previous_checkpoint_hash,
        retained_engagement_operations: retained.clone(),
        leader_authorization: None,
        // Acknowledgements are sync-protocol objects signed by each device. Portable state must
        // never manufacture or union them.
        acknowledged_by: BTreeSet::new(),
    };

    let before = project_at(state, now_unix)?;
    let mut candidate = state.clone();
    candidate.operations.retain(|operation| {
        !matches!(operation.operation, Operation::RecordEngagement { .. })
            || retained.contains(operation.operation_id.as_str())
    });
    candidate.compaction_checkpoint = Some(checkpoint);
    candidate.projection_fingerprint = None;
    candidate.normalize()?;
    let after = project_at(&candidate, now_unix)?;
    if before.fingerprint != after.fingerprint || before.legacy != after.legacy {
        return Err(PersonalStateError::ProjectionMismatch);
    }

    Ok(Some(EngagementCompactionPlan {
        candidate,
        pruned_engagement_operations,
    }))
}

/// Select a checkpoint without trusting or combining its unsigned acknowledgement cache.
pub(crate) fn select_checkpoint(
    left: Option<&CompactionCheckpoint>,
    right: Option<&CompactionCheckpoint>,
) -> Result<Option<CompactionCheckpoint>, PersonalStateError> {
    let selected = match (left, right) {
        (None, None) => return Ok(None),
        (Some(checkpoint), None) | (None, Some(checkpoint)) => checkpoint.clone(),
        (Some(left), Some(right)) if checkpoint_content_eq(left, right) => left.clone(),
        (Some(left), Some(right)) if left.compaction_generation > right.compaction_generation => {
            require_newer_checkpoint(left, right)?;
            left.clone()
        }
        (Some(left), Some(right)) if right.compaction_generation > left.compaction_generation => {
            require_newer_checkpoint(right, left)?;
            right.clone()
        }
        (Some(_), Some(_)) => return Err(checkpoint_fork()),
    };
    Ok(Some(without_acknowledgements(selected)))
}

pub(crate) fn operation_survives_checkpoint(
    checkpoint: Option<&CompactionCheckpoint>,
    operation: &OperationEnvelope,
) -> bool {
    let Some(checkpoint) = checkpoint else {
        return true;
    };
    !matches!(operation.operation, Operation::RecordEngagement { .. })
        || !checkpoint.coverage.covers(&operation.stamp.dot)
        || checkpoint
            .retained_engagement_operations
            .contains(&operation.operation_id)
}

pub(crate) fn retained_engagement_operation_ids(
    operations: &[OperationEnvelope],
    now_unix: i64,
) -> Vec<String> {
    let mut winners = BTreeMap::<&str, &OperationEnvelope>::new();
    for operation in operations {
        let Operation::RecordEngagement { event_id, .. } = &operation.operation else {
            continue;
        };
        let replace = winners.get(event_id.as_str()).is_none_or(|current| {
            stamp_order(
                &operation.stamp,
                &operation.operation_id,
                &current.stamp,
                &current.operation_id,
            )
            .is_gt()
        });
        if replace {
            winners.insert(event_id, operation);
        }
    }

    let cutoff = now_unix.saturating_sub(RAW_EVENT_RETENTION_SECS);
    let mut retained = winners
        .into_iter()
        .filter(|(_, operation)| {
            // Legacy imports use zero when the original store had no trustworthy event time.
            // Preserve those events until the deterministic 20,000-event cap evicts them.
            operation.stamp.recorded_at_unix == 0 || operation.stamp.recorded_at_unix >= cutoff
        })
        .collect::<Vec<_>>();
    retained.sort_by(|(left_event_id, left), (right_event_id, right)| {
        right
            .stamp
            .recorded_at_unix
            .cmp(&left.stamp.recorded_at_unix)
            .then(right.stamp.dot.cmp(&left.stamp.dot))
            .then(left_event_id.cmp(right_event_id))
    });
    retained.truncate(ENGAGEMENT_EVENTS_MAX);
    retained.reverse();
    retained
        .into_iter()
        .map(|(_, operation)| operation.operation_id.clone())
        .collect()
}

pub(crate) fn validate_checkpoint(
    state: &PersonalStateV2,
    checkpoint: &CompactionCheckpoint,
) -> Result<(), PersonalStateError> {
    if checkpoint.retained_engagement_operations.len() > ENGAGEMENT_EVENTS_MAX
        || checkpoint
            .coverage
            .0
            .iter()
            .any(|(device, sequence)| state.version_vector.observed(device) < *sequence)
        || !valid_hash(&checkpoint.checkpoint_id)
        || checkpoint
            .previous_checkpoint_hash
            .as_deref()
            .is_some_and(|hash| !valid_hash(hash))
    {
        return Err(PersonalStateError::InvalidOperation(
            "invalid engagement compaction checkpoint",
        ));
    }
    let expected = checkpoint_id_for(
        &state.dataset_id,
        checkpoint.compaction_generation,
        &checkpoint.coverage,
        checkpoint.previous_checkpoint_hash.as_deref(),
        &checkpoint.retained_engagement_operations,
        &state.operations,
    )?;
    if checkpoint.checkpoint_id != expected {
        return Err(PersonalStateError::InvalidOperation(
            "engagement compaction checkpoint hash does not match",
        ));
    }

    let operations = state
        .operations
        .iter()
        .map(|operation| (operation.operation_id.as_str(), operation))
        .collect::<BTreeMap<_, _>>();
    for operation_id in &checkpoint.retained_engagement_operations {
        let Some(operation) = operations.get(operation_id.as_str()) else {
            return Err(PersonalStateError::InvalidOperation(
                "compaction retained engagement operation is missing",
            ));
        };
        if !matches!(operation.operation, Operation::RecordEngagement { .. })
            || !checkpoint.coverage.covers(&operation.stamp.dot)
        {
            return Err(PersonalStateError::InvalidOperation(
                "compaction retained engagement operation is invalid",
            ));
        }
    }
    if state.operations.iter().any(|operation| {
        matches!(operation.operation, Operation::RecordEngagement { .. })
            && checkpoint.coverage.covers(&operation.stamp.dot)
            && !checkpoint
                .retained_engagement_operations
                .contains(&operation.operation_id)
    }) {
        return Err(PersonalStateError::InvalidOperation(
            "compacted engagement operation was resurrected",
        ));
    }
    Ok(())
}

fn require_newer_checkpoint(
    newer: &CompactionCheckpoint,
    older: &CompactionCheckpoint,
) -> Result<(), PersonalStateError> {
    if !vector_dominates(&newer.coverage, &older.coverage) {
        return Err(PersonalStateError::InvalidOperation(
            "compaction checkpoint coverage moved backwards",
        ));
    }
    if older.compaction_generation.checked_add(1) == Some(newer.compaction_generation)
        && newer.previous_checkpoint_hash.as_deref() != Some(older.checkpoint_id.as_str())
    {
        return Err(checkpoint_fork());
    }
    Ok(())
}

fn checkpoint_fork() -> PersonalStateError {
    PersonalStateError::InvalidOperation("compaction checkpoint history forked")
}

fn vector_dominates(candidate: &VersionVector, ancestor: &VersionVector) -> bool {
    ancestor
        .0
        .iter()
        .all(|(device, sequence)| candidate.observed(device) >= *sequence)
}

fn checkpoint_content_eq(left: &CompactionCheckpoint, right: &CompactionCheckpoint) -> bool {
    left.checkpoint_id == right.checkpoint_id
        && left.compaction_generation == right.compaction_generation
        && left.coverage == right.coverage
        && left.previous_checkpoint_hash == right.previous_checkpoint_hash
        && left.retained_engagement_operations == right.retained_engagement_operations
        && left.leader_authorization == right.leader_authorization
}

fn without_acknowledgements(mut checkpoint: CompactionCheckpoint) -> CompactionCheckpoint {
    checkpoint.acknowledged_by.clear();
    checkpoint
}

#[derive(Serialize)]
struct CheckpointHashMaterial<'a> {
    kind: &'static str,
    dataset_id: &'a str,
    compaction_generation: u64,
    coverage: &'a VersionVector,
    previous_checkpoint_hash: Option<&'a str>,
    retained_engagement_operations: &'a BTreeSet<String>,
    // Commit to every surviving covered operation, not only engagement ids. Otherwise an older
    // state could replace a pruned engagement dot with a rating or membership operation.
    retained_covered_operations: Vec<&'a OperationEnvelope>,
}

pub(crate) fn checkpoint_id_for(
    dataset_id: &str,
    compaction_generation: u64,
    coverage: &VersionVector,
    previous_checkpoint_hash: Option<&str>,
    retained_engagement_operations: &BTreeSet<String>,
    operations: &[OperationEnvelope],
) -> Result<String, PersonalStateError> {
    let mut retained_covered_operations = operations
        .iter()
        .filter(|operation| {
            coverage.covers(&operation.stamp.dot)
                && (!matches!(operation.operation, Operation::RecordEngagement { .. })
                    || retained_engagement_operations.contains(&operation.operation_id))
        })
        .collect::<Vec<_>>();
    retained_covered_operations.sort_by(|left, right| {
        left.stamp
            .dot
            .cmp(&right.stamp.dot)
            .then(left.operation_id.cmp(&right.operation_id))
    });
    retained_covered_operations.dedup_by(|left, right| left.operation_id == right.operation_id);
    let material = CheckpointHashMaterial {
        kind: COMPACTION_HASH_KIND,
        dataset_id,
        compaction_generation,
        coverage,
        previous_checkpoint_hash,
        retained_engagement_operations,
        retained_covered_operations,
    };
    Ok(sha256_hex(&serde_json::to_vec(&material)?))
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
