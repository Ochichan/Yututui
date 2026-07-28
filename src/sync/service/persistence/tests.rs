use super::tests::shutdown_fixture;
use super::*;
use crate::personal_state::{EngagementKind, PortableTrack, PortableTrackKey};

const COMPACTION_NOW: i64 = 2_000_000_000;

fn expired_engagement_state(fixture: &super::tests::ShutdownFixture) -> PersonalStateV2 {
    let state = append_operation_as(
        &fixture.initial,
        &fixture.device_id,
        Operation::RecordEngagement {
            event_id: "expired-persistence-event".to_owned(),
            track: PortableTrack {
                key: PortableTrackKey::Catalog {
                    provider: "youtube".to_owned(),
                    exact_catalog_id: "expired-persistence-track".to_owned(),
                },
                title: "Expired persistence track".to_owned(),
                artist: "Persistence artist".to_owned(),
                album: None,
                duration_secs: Some(180),
                isrc: None,
            },
            engagement: EngagementKind::Play,
            played_duration_ms: Some(90_000),
            total_duration_ms: Some(180_000),
            artist_key: "persistence artist".to_owned(),
        },
        1,
    )
    .unwrap();
    PersonalStateCommit::prepare_for_runtime(state, 7)
        .unwrap()
        .state()
        .clone()
}

fn compacted_state(observed: &PersonalStateV2, device_id: &DeviceId) -> PersonalStateV2 {
    let candidate = crate::personal_state::plan_engagement_compaction(
        observed,
        device_id,
        COMPACTION_NOW,
        false,
    )
    .unwrap()
    .unwrap()
    .candidate;
    PersonalStateCommit::prepare_for_runtime(candidate, 7)
        .unwrap()
        .state()
        .clone()
}

fn compacted_candidate(
    fixture: &super::tests::ShutdownFixture,
    observed: &PersonalStateV2,
    compacted: PersonalStateV2,
) -> PreparedManualSync {
    let mut candidate = fixture.candidate.clone();
    candidate.expected_local_revision = observed.revision;
    candidate.state = compacted;
    candidate
}

#[test]
fn initial_and_reconcile_accept_valid_compaction_but_reject_plain_deletion() {
    let fixture = shutdown_fixture();
    let observed = expired_engagement_state(&fixture);
    let compacted = compacted_state(&observed, &fixture.device_id);
    assert!(verified_state_extension(&observed, &compacted).unwrap());

    let initial = PersonalSyncPersistence::initial(
        observed.clone(),
        7,
        compacted_candidate(&fixture, &observed, compacted.clone()),
        PersonalSyncApplyKind::SyncNow,
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture.root.clone()),
    )
    .unwrap();
    assert_eq!(
        initial.state().compaction_checkpoint,
        compacted.compaction_checkpoint
    );

    let reconciled = PersonalSyncPersistence::reconcile(
        observed.clone(),
        observed.clone(),
        compacted,
        7,
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture.root.clone()),
    )
    .unwrap();
    assert!(reconciled.state().compaction_checkpoint.is_some());

    let mut deleted = observed.clone();
    deleted
        .operations
        .retain(|operation| !matches!(operation.operation, Operation::RecordEngagement { .. }));
    deleted.revision = deleted.revision.saturating_add(1);
    deleted.projection_fingerprint = None;
    let deleted = PersonalStateCommit::prepare_for_runtime(deleted, 7)
        .unwrap()
        .state()
        .clone();
    assert!(!verified_state_extension(&observed, &deleted).unwrap());
    assert!(matches!(
        PersonalSyncPersistence::reconcile(
            observed.clone(),
            observed,
            deleted,
            7,
            fixture.personal_paths.clone(),
            SyncPaths::for_data_root(fixture.root.clone()),
        ),
        Err(SyncServiceError::LocalStateChanged)
    ));
}

#[test]
fn shutdown_rebases_raced_local_mutation_after_compacted_candidate() {
    let fixture = shutdown_fixture();
    let observed = expired_engagement_state(&fixture);
    let compacted = compacted_state(&observed, &fixture.device_id);
    let current = append_operation_as(
        &observed,
        &fixture.device_id,
        Operation::SetAvoidArtist {
            artist_key: "raced-after-compaction".to_owned(),
            avoid: true,
        },
        COMPACTION_NOW,
    )
    .unwrap();
    let current = PersonalStateCommit::prepare_for_runtime(current, 7)
        .unwrap()
        .state()
        .clone();

    let writer = PersonalSyncPersistence::shutdown(
        observed.clone(),
        current,
        7,
        compacted_candidate(&fixture, &observed, compacted.clone()),
        &fixture.device_id,
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture.root.clone()),
    )
    .unwrap();

    assert_eq!(
        writer.state().compaction_checkpoint,
        compacted.compaction_checkpoint
    );
    assert!(
        !writer
            .state()
            .operations
            .iter()
            .any(|operation| matches!(operation.operation, Operation::RecordEngagement { .. }))
    );
    assert!(writer.state().operations.iter().any(|operation| matches!(
        operation.operation,
        Operation::SetAvoidArtist {
            ref artist_key,
            avoid: true
        } if artist_key == "raced-after-compaction"
    )));
    assert!(verified_state_extension(&compacted, writer.state()).unwrap());
}

#[test]
fn pairing_join_retargets_three_times_from_the_durable_import_baseline() {
    let fixture = shutdown_fixture();
    let baseline_count = |state: &PersonalStateV2| {
        state
            .operations
            .iter()
            .filter(|operation| matches!(operation.operation, Operation::LegacyBaseline { .. }))
            .count()
    };

    let first = crate::personal_state::plan_join_import(
        &fixture.candidate.state,
        &fixture.local,
        &fixture.device_id,
    )
    .unwrap()
    .candidate;
    assert_eq!(
        baseline_count(&first),
        baseline_count(&fixture.candidate.state) + 1
    );

    let second_local = append_operation_as(
        &fixture.local,
        &fixture.device_id,
        Operation::SetAvoidArtist {
            artist_key: "join-second-retarget".to_owned(),
            avoid: true,
        },
        6,
    )
    .unwrap();
    let second = crate::personal_state::plan_join_import(&first, &second_local, &fixture.device_id)
        .unwrap()
        .candidate;
    verify_activation_extension(&first, &second).unwrap();
    assert_eq!(baseline_count(&second), baseline_count(&first) + 1);

    let third_local = append_operation_as(
        &second_local,
        &fixture.device_id,
        Operation::SetAvoidArtist {
            artist_key: "join-third-retarget".to_owned(),
            avoid: true,
        },
        7,
    )
    .unwrap();
    let third = crate::personal_state::plan_join_import(&second, &third_local, &fixture.device_id)
        .unwrap()
        .candidate;
    verify_activation_extension(&second, &third).unwrap();
    assert_eq!(baseline_count(&third), baseline_count(&second) + 1);
}

#[test]
fn pairing_approval_shutdown_extends_the_second_reconcile_boundary() {
    let fixture = shutdown_fixture();
    let sync_paths = SyncPaths::for_data_root(fixture.root.clone());
    let target_device = fixture
        .initial
        .device_registry
        .get(&fixture.device_id)
        .unwrap()
        .clone();
    let prepared =
        PreparedPairingApproval::for_persistence_test(fixture.candidate.clone(), target_device);

    let initial_writer = PersonalSyncPersistence::initial(
        fixture.initial.clone(),
        7,
        prepared.candidate().clone(),
        PersonalSyncApplyKind::PairApprove(Box::new(prepared.clone())),
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture.root.clone()),
    )
    .unwrap();
    initial_writer.write().unwrap();
    let durable = initial_writer.state().clone();

    let first_local = PersonalStateCommit::prepare_for_runtime(fixture.local.clone(), 7)
        .unwrap()
        .state()
        .clone();
    let first_retarget = prepared.retarget(&fixture.initial, &first_local).unwrap();
    let first_writer = PersonalSyncPersistence::reconcile(
        durable.clone(),
        first_local.clone(),
        first_retarget.candidate().state.clone(),
        7,
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture.root.clone()),
    )
    .unwrap();

    let second_local = append_operation_as(
        &first_local,
        &fixture.device_id,
        Operation::SetAvoidArtist {
            artist_key: "approval-second-retarget".to_owned(),
            avoid: true,
        },
        8,
    )
    .unwrap();
    let second_local = PersonalStateCommit::prepare_for_runtime(second_local, 7)
        .unwrap()
        .state()
        .clone();
    let second_retarget = prepared.retarget(&fixture.initial, &second_local).unwrap();
    let second_writer = PersonalSyncPersistence::reconcile(
        durable.clone(),
        second_local.clone(),
        second_retarget.candidate().state.clone(),
        7,
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture.root.clone()),
    )
    .unwrap();
    assert!(
        verified_state_extension(first_writer.state(), second_writer.state()).unwrap(),
        "the second retry must preserve the first retry target"
    );
    second_writer.write().unwrap();
    assert!(second_writer.committed());

    let third_local = append_operation_as(
        &second_local,
        &fixture.device_id,
        Operation::SetAvoidArtist {
            artist_key: "approval-shutdown-retarget".to_owned(),
            avoid: true,
        },
        9,
    )
    .unwrap();
    let third_local = PersonalStateCommit::prepare_for_runtime(third_local, 7)
        .unwrap()
        .state()
        .clone();
    let shutdown_writer = PersonalSyncPersistence::pairing_approval_activation_shutdown(
        fixture.initial.clone(),
        third_local,
        7,
        durable,
        Some(second_writer.state().clone()),
        prepared,
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture.root.clone()),
    )
    .unwrap();
    assert!(
        verified_state_extension(second_writer.state(), shutdown_writer.state()).unwrap(),
        "shutdown must extend a reconcile target committed just before ingress closes"
    );

    shutdown_writer.write().unwrap();

    assert!(shutdown_writer.committed());
    let installed = load_ledger(&fixture.personal_paths).unwrap().unwrap();
    assert_eq!(&installed, shutdown_writer.state());
    for artist_key in [
        "local-artist",
        "approval-second-retarget",
        "approval-shutdown-retarget",
    ] {
        assert!(installed.operations.iter().any(|operation| matches!(
            operation.operation,
            Operation::SetAvoidArtist {
                artist_key: ref candidate,
                avoid: true
            } if candidate == artist_key
        )));
    }
    let private = PrivateStore::new(sync_paths.private_store())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(
        private.checkpoint_sequence(),
        Some(fixture.checkpoint_sequence + 1)
    );
    assert_eq!(private.checkpoint_hash(), Some("b".repeat(64).as_str()));
}
