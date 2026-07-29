use std::collections::{BTreeMap, BTreeSet};

use crate::sync::DeviceSecretMaterial;

use super::*;
use crate::personal_state::{
    CausalStamp, DevicePublicIdentity, DeviceRecord, Dot, EngagementKind, OperationOrigin,
    PortableTrack, PortableTrackKey,
};

const DAY: i64 = 24 * 60 * 60;
const NOW: i64 = 500 * DAY;

fn track(id: &str) -> PortableTrack {
    PortableTrack {
        key: PortableTrackKey::Catalog {
            provider: "youtube".to_owned(),
            exact_catalog_id: id.to_owned(),
        },
        title: id.to_owned(),
        artist: "Artist".to_owned(),
        album: None,
        duration_secs: Some(180),
        isrc: None,
    }
}

fn device_record(secret: &DeviceSecretMaterial) -> DeviceRecord {
    DeviceRecord {
        device_id: DeviceId::new(secret.device_id()).unwrap(),
        name: secret.device_id().to_owned(),
        revoked: false,
        public_identity: Some(DevicePublicIdentity {
            age_recipient: secret.public_identity().age_recipient,
            ed25519_verifying_key: secret.public_identity().ed25519_verifying_key,
        }),
    }
}

fn state_with_devices(ids: &[&str]) -> (PersonalStateV2, Vec<DeviceSecretMaterial>) {
    assert!(!ids.is_empty());
    let secrets = ids
        .iter()
        .map(|id| DeviceSecretMaterial::generate_for(*id).unwrap())
        .collect::<Vec<_>>();
    let author = DeviceId::new(secrets[0].device_id()).unwrap();
    let mut state = PersonalStateV2::empty(format!("compaction-{}", ids.join("-"))).unwrap();
    for (index, secret) in secrets.iter().enumerate() {
        let sequence = index as u64 + 1;
        let dot = Dot {
            device_id: author.clone(),
            sequence,
        };
        state.operations.push(OperationEnvelope {
            operation_id: format!("membership-{sequence}"),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: state.version_vector.clone(),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::AddDevice {
                device: device_record(secret),
            },
        });
        state.version_vector.observe(&dot);
    }
    super::super::refresh_device_registry(&mut state).unwrap();
    state.normalize().unwrap();
    (state, secrets)
}

fn push_engagement(
    state: &mut PersonalStateV2,
    author: &DeviceId,
    event_id: impl Into<String>,
    recorded_at_unix: i64,
) -> String {
    let sequence = state.version_vector.observed(author) + 1;
    let event_id = event_id.into();
    let operation_id = format!("{}:{sequence}", author.as_str());
    let dot = Dot {
        device_id: author.clone(),
        sequence,
    };
    state.operations.push(OperationEnvelope {
        operation_id: operation_id.clone(),
        stamp: CausalStamp {
            dot: dot.clone(),
            observed: state.version_vector.clone(),
            recorded_at_unix,
        },
        origin: OperationOrigin::Local,
        operation: Operation::RecordEngagement {
            event_id,
            track: track(&format!("track-{sequence}")),
            engagement: EngagementKind::Play,
            played_duration_ms: Some(90_000),
            total_duration_ms: Some(180_000),
            artist_key: "artist".to_owned(),
        },
    });
    state.version_vector.observe(&dot);
    operation_id
}

#[test]
fn only_the_lowest_active_device_can_plan_compaction() {
    let (mut state, secrets) = state_with_devices(&["device-z", "device-a"]);
    let device_z = DeviceId::new(secrets[0].device_id()).unwrap();
    let device_a = DeviceId::new(secrets[1].device_id()).unwrap();
    push_engagement(
        &mut state,
        &device_z,
        "expired",
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    );
    state.normalize().unwrap();

    assert_eq!(engagement_compaction_leader(&state), Some(device_a.clone()));
    assert!(matches!(
        plan_engagement_compaction(&state, &device_z, NOW, false),
        Err(PersonalStateError::InvalidOperation(
            "only the lowest active device may compact personal state"
        ))
    ));
    let compacted = plan_engagement_compaction(&state, &device_a, NOW, false)
        .unwrap()
        .unwrap();
    let repeated = plan_engagement_compaction(&state, &device_a, NOW, false)
        .unwrap()
        .unwrap();
    assert_eq!(compacted.candidate, repeated.candidate);
    assert_eq!(compacted.pruned_engagement_operations, 1);
    assert_eq!(
        compacted
            .candidate
            .operations
            .iter()
            .filter(|operation| matches!(operation.operation, Operation::AddDevice { .. }))
            .count(),
        2
    );

    let revoked = crate::personal_state::append_operation_as(
        &state,
        &device_z,
        Operation::RevokeDevice {
            device_id: device_a,
        },
        NOW,
    )
    .unwrap();
    assert_eq!(
        engagement_compaction_leader(&revoked),
        Some(device_z.clone())
    );
    assert!(
        plan_engagement_compaction(&revoked, &device_z, NOW, false)
            .unwrap()
            .is_some()
    );
}

#[test]
fn retention_boundary_is_inclusive_and_projection_is_unchanged() {
    let (mut state, secrets) = state_with_devices(&["boundary-device"]);
    let device = DeviceId::new(secrets[0].device_id()).unwrap();
    let expired = push_engagement(
        &mut state,
        &device,
        "expired",
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    );
    let boundary = push_engagement(
        &mut state,
        &device,
        "boundary",
        NOW - RAW_EVENT_RETENTION_SECS,
    );
    state.normalize().unwrap();
    let before = project_at(&state, NOW).unwrap();

    let plan = plan_engagement_compaction(&state, &device, NOW, false)
        .unwrap()
        .unwrap();
    let after = project_at(&plan.candidate, NOW).unwrap();
    assert_eq!(plan.pruned_engagement_operations, 1);
    assert_eq!(before.fingerprint, after.fingerprint);
    assert_eq!(before.legacy, after.legacy);
    assert!(
        !plan
            .candidate
            .operations
            .iter()
            .any(|operation| operation.operation_id == expired)
    );
    assert!(
        plan.candidate
            .operations
            .iter()
            .any(|operation| operation.operation_id == boundary)
    );
    assert_eq!(
        plan.candidate
            .compaction_checkpoint
            .as_ref()
            .unwrap()
            .coverage,
        state.version_vector
    );
    assert!(
        plan.candidate
            .compaction_checkpoint
            .as_ref()
            .unwrap()
            .acknowledged_by
            .is_empty()
    );
    assert!(
        plan_engagement_compaction(&plan.candidate, &device, NOW, false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn newest_twenty_thousand_events_are_retained_with_a_stable_dot_tie_break() {
    let (mut state, secrets) = state_with_devices(&["cap-device"]);
    let device = DeviceId::new(secrets[0].device_id()).unwrap();
    let mut operation_ids = Vec::new();
    for index in 0..20_001 {
        operation_ids.push(push_engagement(
            &mut state,
            &device,
            format!("same-time-{index}"),
            NOW,
        ));
    }
    state.normalize().unwrap();
    let before = project_at(&state, NOW).unwrap();

    let plan = plan_engagement_compaction(&state, &device, NOW, false)
        .unwrap()
        .unwrap();
    let checkpoint = plan.candidate.compaction_checkpoint.as_ref().unwrap();
    assert_eq!(plan.pruned_engagement_operations, 1);
    assert_eq!(checkpoint.retained_engagement_operations.len(), 20_000);
    assert!(
        !checkpoint
            .retained_engagement_operations
            .contains(&operation_ids[0])
    );
    assert!(operation_ids[1..].iter().all(|operation_id| {
        checkpoint
            .retained_engagement_operations
            .contains(operation_id)
    }));
    assert_eq!(plan.candidate.operations.len(), 20_001);
    assert_eq!(
        before.fingerprint,
        project_at(&plan.candidate, NOW).unwrap().fingerprint
    );
}

#[test]
fn merge_does_not_resurrect_covered_pruned_events() {
    let (mut original, secrets) = state_with_devices(&["merge-device"]);
    let device = DeviceId::new(secrets[0].device_id()).unwrap();
    let expired = push_engagement(
        &mut original,
        &device,
        "expired",
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    );
    push_engagement(&mut original, &device, "kept", NOW);
    original.normalize().unwrap();
    let compacted = plan_engagement_compaction(&original, &device, NOW, false)
        .unwrap()
        .unwrap()
        .candidate;

    let (left, left_summary) = crate::personal_state::merge(&compacted, &original).unwrap();
    let (right, _) = crate::personal_state::merge(&original, &compacted).unwrap();
    assert_eq!(left.operations, compacted.operations);
    assert_eq!(right.operations, compacted.operations);
    assert_eq!(left.compaction_checkpoint, compacted.compaction_checkpoint);
    assert_eq!(right.compaction_checkpoint, compacted.compaction_checkpoint);
    assert_eq!(left_summary.added_operations, 0);
    assert!(
        !left
            .operations
            .iter()
            .any(|operation| operation.operation_id == expired)
    );
}

#[test]
fn merge_rejects_a_non_engagement_operation_substituted_at_a_pruned_dot() {
    let (mut original, secrets) = state_with_devices(&["substitution-device"]);
    let device = DeviceId::new(secrets[0].device_id()).unwrap();
    let expired = push_engagement(
        &mut original,
        &device,
        "expired",
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    );
    original.normalize().unwrap();
    let compacted = plan_engagement_compaction(&original, &device, NOW, false)
        .unwrap()
        .unwrap()
        .candidate;

    let mut substituted = original;
    let operation = substituted
        .operations
        .iter_mut()
        .find(|operation| operation.operation_id == expired)
        .unwrap();
    operation.operation = Operation::SetRating {
        track: track("forged-rating"),
        rating: crate::personal_state::Rating::Liked,
    };
    substituted.normalize().unwrap();

    assert!(matches!(
        crate::personal_state::merge(&compacted, &substituted),
        Err(PersonalStateError::InvalidOperation(
            "engagement compaction checkpoint hash does not match"
        ))
    ));
}

#[test]
fn later_compaction_links_to_and_dominates_the_previous_checkpoint() {
    let (mut state, secrets) = state_with_devices(&["chain-device"]);
    let device = DeviceId::new(secrets[0].device_id()).unwrap();
    push_engagement(
        &mut state,
        &device,
        "first-expired",
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    );
    state.normalize().unwrap();
    let first = plan_engagement_compaction(&state, &device, NOW, false)
        .unwrap()
        .unwrap()
        .candidate;
    let first_checkpoint = first.compaction_checkpoint.as_ref().unwrap().clone();
    assert_eq!(first_checkpoint.compaction_generation, 1);

    let mut advanced = crate::personal_state::append_operation_as(
        &first,
        &device,
        Operation::RecordEngagement {
            event_id: "second-expired".to_owned(),
            track: track("second-expired"),
            engagement: EngagementKind::Play,
            played_duration_ms: Some(1),
            total_duration_ms: Some(100),
            artist_key: "artist".to_owned(),
        },
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    )
    .unwrap();
    advanced.projection_fingerprint = None;
    assert!(
        plan_engagement_compaction(&advanced, &device, NOW, false)
            .unwrap()
            .is_none(),
        "a descendant compaction must wait for explicit authenticated quorum"
    );
    let second = plan_engagement_compaction(&advanced, &device, NOW, true)
        .unwrap()
        .unwrap()
        .candidate;
    let second_checkpoint = second.compaction_checkpoint.as_ref().unwrap();
    assert_eq!(second_checkpoint.compaction_generation, 2);
    assert_eq!(
        second_checkpoint.previous_checkpoint_hash.as_deref(),
        Some(first_checkpoint.checkpoint_id.as_str())
    );
    assert!(vector_dominates(
        &second_checkpoint.coverage,
        &first_checkpoint.coverage
    ));

    let (merged, _) = crate::personal_state::merge(&first, &second).unwrap();
    assert_eq!(
        merged.compaction_checkpoint.as_ref().unwrap().checkpoint_id,
        second_checkpoint.checkpoint_id
    );
}

#[test]
fn non_adjacent_compaction_generations_merge_and_import_in_both_directions() {
    let (mut original, secrets) = state_with_devices(&["generation-device"]);
    let device = DeviceId::new(secrets[0].device_id()).unwrap();
    push_engagement(
        &mut original,
        &device,
        "generation-one-expired",
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    );
    original.normalize().unwrap();
    let first = plan_engagement_compaction(&original, &device, NOW, false)
        .unwrap()
        .unwrap()
        .candidate;

    let mut after_first = crate::personal_state::append_operation_as(
        &first,
        &device,
        Operation::RecordEngagement {
            event_id: "generation-two-expired".to_owned(),
            track: track("generation-two-expired"),
            engagement: EngagementKind::Play,
            played_duration_ms: Some(1),
            total_duration_ms: Some(100),
            artist_key: "artist".to_owned(),
        },
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    )
    .unwrap();
    after_first.projection_fingerprint = None;
    let second = plan_engagement_compaction(&after_first, &device, NOW, true)
        .unwrap()
        .unwrap()
        .candidate;

    let mut after_second = crate::personal_state::append_operation_as(
        &second,
        &device,
        Operation::RecordEngagement {
            event_id: "generation-three-expired".to_owned(),
            track: track("generation-three-expired"),
            engagement: EngagementKind::Play,
            played_duration_ms: Some(1),
            total_duration_ms: Some(100),
            artist_key: "artist".to_owned(),
        },
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    )
    .unwrap();
    after_second.projection_fingerprint = None;
    let third = plan_engagement_compaction(&after_second, &device, NOW, true)
        .unwrap()
        .unwrap()
        .candidate;

    assert_eq!(
        [
            first
                .compaction_checkpoint
                .as_ref()
                .unwrap()
                .compaction_generation,
            second
                .compaction_checkpoint
                .as_ref()
                .unwrap()
                .compaction_generation,
            third
                .compaction_checkpoint
                .as_ref()
                .unwrap()
                .compaction_generation,
        ],
        [1, 2, 3]
    );

    let (first_then_third, _) = crate::personal_state::merge(&first, &third).unwrap();
    let (third_then_first, _) = crate::personal_state::merge(&third, &first).unwrap();
    assert_eq!(first_then_third, third);
    assert_eq!(third_then_first, third);

    let import_old = crate::personal_state::plan_import(&third, &first, None).unwrap();
    let import_new = crate::personal_state::plan_import(&first, &third, None).unwrap();
    assert_eq!(import_old.candidate, third);
    assert_eq!(import_new.candidate.operations, third.operations);
    assert_eq!(import_new.candidate.version_vector, third.version_vector);
    assert_eq!(
        import_new.candidate.compaction_checkpoint,
        third.compaction_checkpoint
    );

    let (first_second, _) = crate::personal_state::merge(&first, &second).unwrap();
    let (left_associated, _) = crate::personal_state::merge(&first_second, &third).unwrap();
    let (second_third, _) = crate::personal_state::merge(&second, &third).unwrap();
    let (right_associated, _) = crate::personal_state::merge(&first, &second_third).unwrap();
    assert_eq!(left_associated, right_associated);
    assert_eq!(left_associated, third);
}

#[test]
fn compaction_generation_overflow_fails_closed() {
    let (mut original, secrets) = state_with_devices(&["generation-overflow-device"]);
    let device = DeviceId::new(secrets[0].device_id()).unwrap();
    push_engagement(
        &mut original,
        &device,
        "initial-expired",
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    );
    original.normalize().unwrap();
    let mut exhausted = plan_engagement_compaction(&original, &device, NOW, false)
        .unwrap()
        .unwrap()
        .candidate;
    let (coverage, previous, retained) = {
        let checkpoint = exhausted.compaction_checkpoint.as_ref().unwrap();
        (
            checkpoint.coverage.clone(),
            checkpoint.previous_checkpoint_hash.clone(),
            checkpoint.retained_engagement_operations.clone(),
        )
    };
    let exhausted_id = checkpoint_id_for(
        &exhausted.dataset_id,
        u64::MAX,
        &coverage,
        previous.as_deref(),
        &retained,
        &exhausted.operations,
    )
    .unwrap();
    let checkpoint = exhausted.compaction_checkpoint.as_mut().unwrap();
    checkpoint.compaction_generation = u64::MAX;
    checkpoint.checkpoint_id = exhausted_id;
    exhausted.normalize().unwrap();

    let mut advanced = crate::personal_state::append_operation_as(
        &exhausted,
        &device,
        Operation::RecordEngagement {
            event_id: "overflow-expired".to_owned(),
            track: track("overflow-expired"),
            engagement: EngagementKind::Play,
            played_duration_ms: Some(1),
            total_duration_ms: Some(100),
            artist_key: "artist".to_owned(),
        },
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    )
    .unwrap();
    advanced.projection_fingerprint = None;

    assert!(matches!(
        plan_engagement_compaction(&advanced, &device, NOW, true),
        Err(PersonalStateError::InvalidOperation(
            "compaction generation exhausted"
        ))
    ));
}

#[test]
fn terminal_personal_state_revision_blocks_compaction_before_candidate_creation() {
    let (mut state, secrets) = state_with_devices(&["revision-exhausted-device"]);
    let device = DeviceId::new(secrets[0].device_id()).unwrap();
    push_engagement(
        &mut state,
        &device,
        "revision-exhausted-event",
        NOW - RAW_EVENT_RETENTION_SECS - 1,
    );
    state.revision = u64::MAX;
    state.normalize().unwrap();

    assert!(matches!(
        plan_engagement_compaction(&state, &device, NOW, false),
        Err(PersonalStateError::InvalidOperation(
            "personal-state revision exhausted"
        ))
    ));
    assert!(state.compaction_checkpoint.is_none());
    assert!(state.operations.iter().any(|operation| {
        matches!(
            &operation.operation,
            Operation::RecordEngagement { event_id, .. }
                if event_id == "revision-exhausted-event"
        )
    }));
}

#[test]
fn incomparable_or_backward_checkpoint_history_is_rejected() {
    let device_a = DeviceId::new("device-a").unwrap();
    let device_b = DeviceId::new("device-b").unwrap();
    let ancestor_coverage = VersionVector(BTreeMap::from([
        (device_a.clone(), 5),
        (device_b.clone(), 2),
    ]));
    let ancestor = checkpoint("fork-dataset", 1, ancestor_coverage, None);
    let backward = checkpoint(
        "fork-dataset",
        2,
        VersionVector(BTreeMap::from([
            (device_a.clone(), 4),
            (device_b.clone(), 3),
        ])),
        Some(&ancestor.checkpoint_id),
    );
    assert!(select_checkpoint(Some(&ancestor), Some(&backward)).is_err());
    let wrong_parent = checkpoint(
        "fork-dataset",
        2,
        VersionVector(BTreeMap::from([
            (device_a.clone(), 6),
            (device_b.clone(), 2),
        ])),
        Some(&"f".repeat(64)),
    );
    assert!(select_checkpoint(Some(&ancestor), Some(&wrong_parent)).is_err());

    let left = checkpoint(
        "fork-dataset",
        2,
        VersionVector(BTreeMap::from([
            (device_a.clone(), 7),
            (device_b.clone(), 2),
        ])),
        Some(&ancestor.checkpoint_id),
    );
    let right = checkpoint(
        "fork-dataset",
        2,
        VersionVector(BTreeMap::from([(device_a, 5), (device_b, 4)])),
        Some(&ancestor.checkpoint_id),
    );
    assert!(select_checkpoint(Some(&left), Some(&right)).is_err());
}

#[test]
fn unsigned_acknowledgement_sets_are_never_unioned() {
    let device = DeviceId::new("device-a").unwrap();
    let checkpoint = checkpoint(
        "ack-dataset",
        1,
        VersionVector(BTreeMap::from([(device.clone(), 1)])),
        None,
    );
    let mut with_ack = checkpoint.clone();
    with_ack.acknowledged_by.insert(device);

    let selected = select_checkpoint(Some(&checkpoint), Some(&with_ack))
        .unwrap()
        .unwrap();
    assert!(selected.acknowledged_by.is_empty());
    assert_eq!(selected.checkpoint_id, checkpoint.checkpoint_id);
}

fn checkpoint(
    dataset_id: &str,
    compaction_generation: u64,
    coverage: VersionVector,
    previous_checkpoint_hash: Option<&str>,
) -> CompactionCheckpoint {
    let retained_engagement_operations = BTreeSet::new();
    CompactionCheckpoint {
        checkpoint_id: checkpoint_id_for(
            dataset_id,
            compaction_generation,
            &coverage,
            previous_checkpoint_hash,
            &retained_engagement_operations,
            &[],
        )
        .unwrap(),
        compaction_generation,
        coverage,
        previous_checkpoint_hash: previous_checkpoint_hash.map(str::to_owned),
        retained_engagement_operations,
        leader_authorization: None,
        acknowledged_by: BTreeSet::new(),
    }
}
