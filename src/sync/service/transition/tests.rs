use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::personal_state::{
    CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateCommit, PersonalStatePaths, PersonalStateV2, PortableTrack, PortableTrackKey,
    Rating, VersionVector, append_operation_as, load_ledger,
};
use crate::sync::{
    CheckpointAnchor, DeviceSecretMaterial, MembershipAnchor, MembershipChain, PrivateStore,
    PrivateStoreSnapshot, RecoveryKit, SignedCheckpoint, SignedMembershipRoot, SyncPaths,
};

use super::{
    AnchorActivationKind, AnchorCommitPoint, commit_with_anchor_transition,
    commit_with_anchor_transition_using, recover_pending_anchor_transition,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: std::path::PathBuf,
    personal_paths: PersonalStatePaths,
    sync_paths: SyncPaths,
    initial: PersonalStateV2,
    candidate: PersonalStateCommit,
    target_private: PrivateStoreSnapshot,
    initial_checkpoint_sequence: u64,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture(label: &str) -> Fixture {
    let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "yututui-anchor-transition-{label}-{}-{sequence}",
        std::process::id()
    ));
    crate::util::safe_fs::ensure_private_dir(&root).unwrap();
    let personal_paths = PersonalStatePaths::for_data_root(root.clone());
    let sync_paths = SyncPaths::for_data_root(root.clone());
    crate::util::safe_fs::ensure_private_dir(sync_paths.root()).unwrap();

    let dataset_id = format!("anchor-transition-{label}");
    let recovery = RecoveryKit::generate(dataset_id.clone(), None).unwrap();
    let device = DeviceSecretMaterial::generate_for("anchor-device").unwrap();
    let device_id = DeviceId::new(device.device_id()).unwrap();
    let device_record = DeviceRecord {
        device_id: device_id.clone(),
        name: "Anchor device".to_owned(),
        revoked: false,
        public_identity: Some(device.public_identity()),
    };
    let root_membership = SignedMembershipRoot::create(
        dataset_id.clone(),
        recovery.recovery_recipient(),
        &recovery.signing_key().unwrap(),
        device_record.clone(),
    )
    .unwrap();
    let membership_anchor = MembershipAnchor::RootHash(root_membership.hash().unwrap());
    let membership = MembershipChain::new(root_membership);
    let dot = Dot {
        device_id: device_id.clone(),
        sequence: 1,
    };
    let mut state = PersonalStateV2::empty(dataset_id).unwrap();
    state.operations.push(OperationEnvelope {
        operation_id: "anchor-device:1".to_owned(),
        stamp: CausalStamp {
            dot: dot.clone(),
            observed: VersionVector::default(),
            recorded_at_unix: 1,
        },
        origin: OperationOrigin::Local,
        operation: Operation::AddDevice {
            device: device_record,
        },
    });
    state.version_vector.observe(&dot);
    crate::personal_state::refresh_device_registry(&mut state).unwrap();
    state.normalize().unwrap();
    let initial_commit = PersonalStateCommit::prepare_for_runtime(state, 7).unwrap();
    let initial = initial_commit.state().clone();
    let checkpoint = SignedCheckpoint::create(
        membership,
        &membership_anchor,
        device_id.clone(),
        device.signing_key(),
        &CheckpointAnchor::default(),
        initial.clone(),
    )
    .unwrap();
    let initial_checkpoint_sequence = checkpoint.payload.checkpoint_sequence;
    let mut private = PrivateStoreSnapshot::pending_ledger_commit(
        device,
        recovery.recovery_recipient(),
        recovery.recovery_verifying_key().unwrap(),
        match membership_anchor {
            MembershipAnchor::RootHash(hash) => hash,
            MembershipAnchor::RecoveryVerifyingKey(_) => unreachable!(),
        },
        &checkpoint,
    )
    .unwrap();
    private.mark_active(&checkpoint, &initial).unwrap();
    PrivateStore::new(sync_paths.private_store())
        .unwrap()
        .create(&mut private)
        .unwrap();
    initial_commit.commit(&personal_paths).unwrap();

    let candidate_state = append_operation_as(
        &initial,
        &device_id,
        Operation::SetRating {
            track: PortableTrack {
                key: PortableTrackKey::Catalog {
                    provider: "youtube".to_owned(),
                    exact_catalog_id: "anchor-track".to_owned(),
                },
                title: "Anchor track".to_owned(),
                artist: "Anchor artist".to_owned(),
                album: None,
                duration_secs: None,
                isrc: None,
            },
            rating: Rating::Liked,
        },
        2,
    )
    .unwrap();
    let candidate = PersonalStateCommit::prepare_for_runtime(candidate_state, 7).unwrap();
    let mut target_private = PrivateStore::new(sync_paths.private_store())
        .unwrap()
        .load()
        .unwrap();
    target_private
        .advance_checkpoint(initial_checkpoint_sequence + 1, "b".repeat(64))
        .unwrap();
    Fixture {
        root,
        personal_paths,
        sync_paths,
        initial,
        candidate,
        target_private,
        initial_checkpoint_sequence,
    }
}

#[test]
fn uncommitted_outer_decision_is_discarded_without_changing_either_store() {
    let mut fixture = fixture("uncommitted");
    let result = commit_with_anchor_transition_using(
        &fixture.initial,
        &fixture.candidate,
        &fixture.personal_paths,
        &fixture.sync_paths,
        &mut fixture.target_private,
        AnchorActivationKind::ManualSync,
        fixture.initial_checkpoint_sequence + 1,
        &"b".repeat(64),
        7,
        |point| {
            if point == AnchorCommitPoint::Staged {
                Err(io::Error::other("simulated crash"))
            } else {
                Ok(())
            }
        },
    );
    assert!(result.is_err());

    assert!(
        recover_pending_anchor_transition(&fixture.personal_paths, &fixture.sync_paths)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        load_ledger(&fixture.personal_paths).unwrap(),
        Some(fixture.initial.clone())
    );
    let private = PrivateStore::new(fixture.sync_paths.private_store())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(
        private.checkpoint_sequence(),
        Some(fixture.initial_checkpoint_sequence)
    );
    assert_eq!(private.revision(), 1);
    assert_transition_artifacts_removed(&fixture.sync_paths);
}

#[test]
fn every_committed_crash_point_rolls_forward_both_stores_exactly_once() {
    for fail_at in [
        AnchorCommitPoint::Committed,
        AnchorCommitPoint::LedgerInstalled,
        AnchorCommitPoint::PrivateInstalled,
    ] {
        let mut fixture = fixture(match fail_at {
            AnchorCommitPoint::Committed => "committed",
            AnchorCommitPoint::LedgerInstalled => "ledger",
            AnchorCommitPoint::PrivateInstalled => "private",
            AnchorCommitPoint::Staged => unreachable!(),
        });
        let expected = fixture.candidate.state().clone();
        let result = commit_with_anchor_transition_using(
            &fixture.initial,
            &fixture.candidate,
            &fixture.personal_paths,
            &fixture.sync_paths,
            &mut fixture.target_private,
            AnchorActivationKind::ManualSync,
            fixture.initial_checkpoint_sequence + 1,
            &"b".repeat(64),
            7,
            |point| {
                if point == fail_at {
                    Err(io::Error::other("simulated crash"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());

        let recovered =
            recover_pending_anchor_transition(&fixture.personal_paths, &fixture.sync_paths)
                .unwrap()
                .expect("committed transition rolls forward");
        assert_eq!(recovered, expected);
        assert_eq!(
            load_ledger(&fixture.personal_paths).unwrap(),
            Some(expected)
        );
        let private = PrivateStore::new(fixture.sync_paths.private_store())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            private.checkpoint_sequence(),
            Some(fixture.initial_checkpoint_sequence + 1)
        );
        assert_eq!(private.checkpoint_hash(), Some("b".repeat(64).as_str()));
        assert_eq!(private.revision(), 2);
        assert_transition_artifacts_removed(&fixture.sync_paths);

        assert!(
            recover_pending_anchor_transition(&fixture.personal_paths, &fixture.sync_paths)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn retry_after_an_uncommitted_stage_reuses_the_observed_private_revision() {
    let mut fixture = fixture("stage-retry");
    let expected = fixture.candidate.state().clone();
    let result = commit_with_anchor_transition_using(
        &fixture.initial,
        &fixture.candidate,
        &fixture.personal_paths,
        &fixture.sync_paths,
        &mut fixture.target_private,
        AnchorActivationKind::ManualSync,
        fixture.initial_checkpoint_sequence + 1,
        &"b".repeat(64),
        7,
        |point| {
            if point == AnchorCommitPoint::Staged {
                Err(io::Error::other("simulated crash"))
            } else {
                Ok(())
            }
        },
    );
    assert!(result.is_err());
    assert_eq!(
        fixture.target_private.revision(),
        1,
        "staging must not publish a target revision into live memory"
    );

    let installed = commit_with_anchor_transition(
        &fixture.initial,
        &fixture.candidate,
        &fixture.personal_paths,
        &fixture.sync_paths,
        &mut fixture.target_private,
        AnchorActivationKind::ManualSync,
        fixture.initial_checkpoint_sequence + 1,
        &"b".repeat(64),
        7,
    )
    .unwrap();
    assert_eq!(installed, expected);
    let private = PrivateStore::new(fixture.sync_paths.private_store())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(private.revision(), 2);
    assert_transition_artifacts_removed(&fixture.sync_paths);
}

#[test]
fn actor_retry_after_ledger_install_finishes_recovery_without_a_second_transition() {
    let mut fixture = fixture("actor-retry");
    let expected = fixture.candidate.state().clone();
    let result = commit_with_anchor_transition_using(
        &fixture.initial,
        &fixture.candidate,
        &fixture.personal_paths,
        &fixture.sync_paths,
        &mut fixture.target_private,
        AnchorActivationKind::ManualSync,
        fixture.initial_checkpoint_sequence + 1,
        &"b".repeat(64),
        7,
        |point| {
            if point == AnchorCommitPoint::LedgerInstalled {
                Err(io::Error::other("simulated crash"))
            } else {
                Ok(())
            }
        },
    );
    assert!(result.is_err());
    assert_eq!(
        load_ledger(&fixture.personal_paths).unwrap(),
        Some(expected.clone())
    );

    let installed = commit_with_anchor_transition(
        &expected,
        &fixture.candidate,
        &fixture.personal_paths,
        &fixture.sync_paths,
        &mut fixture.target_private,
        AnchorActivationKind::ManualSync,
        fixture.initial_checkpoint_sequence + 1,
        &"b".repeat(64),
        7,
    )
    .unwrap();
    assert_eq!(installed, expected);
    let private = PrivateStore::new(fixture.sync_paths.private_store())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(
        private.revision(),
        2,
        "recovery must not stage revision three"
    );
    assert_transition_artifacts_removed(&fixture.sync_paths);
}

fn assert_transition_artifacts_removed(paths: &SyncPaths) {
    for path in [
        paths.anchor_transition_manifest(),
        paths.anchor_transition_ledger(),
        paths.anchor_transition_private(),
        paths.anchor_transition_commit(),
    ] {
        assert!(!path.exists(), "transition artifact was retained");
    }
}
