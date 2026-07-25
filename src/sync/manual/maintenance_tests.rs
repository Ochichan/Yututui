use crate::personal_state::{
    CausalStamp, DeviceId, DeviceRecord, Dot, EngagementKind, Operation, OperationEnvelope,
    OperationOrigin, PersonalStateV2, PortableTrack, PortableTrackKey, Rating, VersionVector,
    append_operation_as,
};
use crate::sync::{
    CheckpointAnchor, DeviceSecretMaterial, EncryptedObject, FileVaultTransport, MembershipAction,
    MembershipAnchor, MembershipChain, ObjectCondition, ObjectDeleteResult, ObjectKey,
    ObjectMetadata, ObjectWriteResult, RecoveryKit, SignedCheckpoint, SignedCompactionAck,
    SignedMembershipRoot, VaultError, VaultTransport, authorize_compaction, compaction_ack_key,
};

use super::super::{
    ManualSyncCandidate, ManualSyncEngine, ManualSyncInput, checkpoint_key, checkpoint_prefix,
    segment_prefix,
};

struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "yututui-compaction-maintenance-{}-{sequence}",
            std::process::id()
        ));
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    devices: Vec<DeviceSecretMaterial>,
    membership: MembershipChain,
    anchor: MembershipAnchor,
    state: PersonalStateV2,
}

fn fixture() -> Fixture {
    let recovery = RecoveryKit::generate("maintenance-dataset", None).unwrap();
    let devices = ["device-1", "device-2"]
        .into_iter()
        .map(|id| DeviceSecretMaterial::generate_for(id).unwrap())
        .collect::<Vec<_>>();
    let records = devices
        .iter()
        .map(|device| DeviceRecord {
            device_id: DeviceId::new(device.device_id()).unwrap(),
            name: device.device_id().to_owned(),
            revoked: false,
            public_identity: Some(device.public_identity()),
        })
        .collect::<Vec<_>>();
    let root = SignedMembershipRoot::create(
        "maintenance-dataset",
        recovery.recovery_recipient(),
        &recovery.signing_key().unwrap(),
        records[0].clone(),
    )
    .unwrap();
    let anchor = MembershipAnchor::RootHash(root.hash().unwrap());
    let mut membership = MembershipChain::new(root);
    membership
        .append_device_action(
            &anchor,
            &records[0].device_id,
            devices[0].signing_key(),
            MembershipAction::AddDevice {
                device: records[1].clone(),
            },
        )
        .unwrap();

    let first_dot = Dot {
        device_id: records[0].device_id.clone(),
        sequence: 1,
    };
    let mut state = PersonalStateV2::empty("maintenance-dataset".to_owned()).unwrap();
    state.operations.push(OperationEnvelope {
        operation_id: "initial-device".to_owned(),
        stamp: CausalStamp {
            dot: first_dot.clone(),
            observed: VersionVector::default(),
            recorded_at_unix: 0,
        },
        origin: OperationOrigin::Local,
        operation: Operation::AddDevice {
            device: records[0].clone(),
        },
    });
    state.version_vector.observe(&first_dot);
    crate::personal_state::refresh_device_registry(&mut state).unwrap();
    state.normalize().unwrap();
    state = append_operation_as(
        &state,
        &records[0].device_id,
        Operation::AddDevice {
            device: records[1].clone(),
        },
        0,
    )
    .unwrap();
    Fixture {
        devices,
        membership,
        anchor,
        state,
    }
}

fn synchronize<T: VaultTransport + ?Sized>(
    transport: &T,
    fixture: &Fixture,
    device_index: usize,
    state: &PersonalStateV2,
    checkpoint_anchor: &CheckpointAnchor,
) -> Result<ManualSyncCandidate, VaultError> {
    ManualSyncEngine::new(transport).synchronize(
        &ManualSyncInput {
            local_state: state,
            membership: &fixture.membership,
            membership_anchor: &fixture.anchor,
            device: &fixture.devices[device_index],
            checkpoint_anchor,
            bootstrap_checkpoint: None,
            expected_local_revision: state.revision,
        },
        &|expected| {
            if expected == state.revision {
                Ok(())
            } else {
                Err(VaultError::RevisionConflict)
            }
        },
    )
}

struct DeleteUnsupportedTransport<'a>(&'a FileVaultTransport);

impl VaultTransport for DeleteUnsupportedTransport<'_> {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.0.get(key, max_bytes)
    }

    fn put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError> {
        self.0.put(key, object, condition)
    }

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.0.list(prefix, max_resources)
    }
}

struct RateLimitedDeleteTransport<'a> {
    inner: &'a FileVaultTransport,
    delete_attempts: std::sync::atomic::AtomicUsize,
}

impl VaultTransport for RateLimitedDeleteTransport<'_> {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.inner.get(key, max_bytes)
    }

    fn put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError> {
        self.inner.put(key, object, condition)
    }

    fn delete(
        &self,
        _key: &ObjectKey,
        _expected_etag: &str,
    ) -> Result<ObjectDeleteResult, VaultError> {
        self.delete_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(VaultError::RemoteRateLimited(Some(
            std::time::Duration::from_secs(75),
        )))
    }

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.inner.list(prefix, max_resources)
    }
}

fn expired_engagement(fixture: &Fixture, state: &PersonalStateV2) -> PersonalStateV2 {
    append_operation_as(
        state,
        &DeviceId::new(fixture.devices[0].device_id()).unwrap(),
        Operation::RecordEngagement {
            event_id: "expired-play".to_owned(),
            track: PortableTrack {
                key: PortableTrackKey::Catalog {
                    provider: "youtube".to_owned(),
                    exact_catalog_id: "expired-track".to_owned(),
                },
                title: "Expired track".to_owned(),
                artist: "Artist".to_owned(),
                album: None,
                duration_secs: Some(180),
                isrc: None,
            },
            engagement: EngagementKind::Play,
            played_duration_ms: Some(90_000),
            total_duration_ms: Some(180_000),
            artist_key: "artist".to_owned(),
        },
        crate::signals::unix_now().saturating_sub(366 * 24 * 60 * 60),
    )
    .unwrap()
}

fn like_track(fixture: &Fixture, state: &PersonalStateV2, id: &str) -> PersonalStateV2 {
    append_operation_as(
        state,
        &DeviceId::new(fixture.devices[0].device_id()).unwrap(),
        Operation::SetRating {
            track: PortableTrack {
                key: PortableTrackKey::Catalog {
                    provider: "youtube".to_owned(),
                    exact_catalog_id: id.to_owned(),
                },
                title: id.to_owned(),
                artist: "Artist".to_owned(),
                album: None,
                duration_secs: Some(180),
                isrc: None,
            },
            rating: Rating::Liked,
        },
        crate::signals::unix_now(),
    )
    .unwrap()
}

#[test]
fn every_active_device_must_ack_before_the_leader_deletes_covered_segments() {
    let fixture = fixture();
    let temp = TempRoot::new();
    let transport = FileVaultTransport::create(temp.0.clone()).unwrap();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let with_expired_event = expired_engagement(&fixture, &bootstrap.state);
    let compacted = synchronize(
        &transport,
        &fixture,
        0,
        &with_expired_event,
        &bootstrap.checkpoint_anchor,
    )
    .unwrap();
    assert_eq!(compacted.summary.compacted_engagement_operations, 1);
    assert!(compacted.state.compaction_checkpoint.is_some());

    let leader = DeviceId::new(fixture.devices[0].device_id()).unwrap();
    let segment_prefix = segment_prefix(&compacted.state.dataset_id, &leader).unwrap();
    assert!(!transport.list(&segment_prefix, 10).unwrap().is_empty());

    let follower_download = synchronize(
        &transport,
        &fixture,
        1,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    assert_eq!(
        follower_download
            .summary
            .compaction_acknowledgements_written,
        0,
        "downloading a checkpoint is not yet a durable acknowledgement"
    );
    assert!(!transport.list(&segment_prefix, 10).unwrap().is_empty());

    let follower_ack = synchronize(
        &transport,
        &fixture,
        1,
        &follower_download.state,
        &follower_download.checkpoint_anchor,
    )
    .unwrap();
    assert_eq!(follower_ack.summary.compaction_acknowledgements_written, 1);
    assert!(!transport.list(&segment_prefix, 10).unwrap().is_empty());

    let leader_ack = synchronize(
        &transport,
        &fixture,
        0,
        &compacted.state,
        &compacted.checkpoint_anchor,
    )
    .unwrap();
    assert_eq!(leader_ack.summary.compaction_acknowledgements_written, 1);
    assert!(leader_ack.summary.compacted_segments_deleted >= 1);
    assert!(transport.list(&segment_prefix, 10).unwrap().is_empty());

    let repeated = synchronize(
        &transport,
        &fixture,
        0,
        &leader_ack.state,
        &leader_ack.checkpoint_anchor,
    )
    .unwrap();
    assert_eq!(repeated.summary.remote_writes, 0);
    assert_eq!(repeated.summary.compacted_segments_deleted, 0);
}

#[test]
fn offline_device_blocks_a_grandchild_compaction_until_it_acknowledges() {
    let fixture = fixture();
    let temp = TempRoot::new();
    let transport = FileVaultTransport::create(temp.0.clone()).unwrap();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let first = synchronize(
        &transport,
        &fixture,
        0,
        &expired_engagement(&fixture, &bootstrap.state),
        &bootstrap.checkpoint_anchor,
    )
    .unwrap();
    let first_compaction = first.state.compaction_checkpoint.as_ref().unwrap().clone();

    let second_expired = expired_engagement(&fixture, &first.state);
    let leader_id = DeviceId::new(fixture.devices[0].device_id()).unwrap();
    let mut supplied_descendant = crate::personal_state::plan_engagement_compaction(
        &second_expired,
        &leader_id,
        crate::signals::unix_now(),
        true,
    )
    .unwrap()
    .unwrap()
    .candidate;
    let membership = fixture.membership.verify(&fixture.anchor).unwrap();
    let descendant_dataset_id = supplied_descendant.dataset_id.clone();
    let descendant_compaction = supplied_descendant.compaction_checkpoint.as_mut().unwrap();
    descendant_compaction.leader_authorization = Some(
        authorize_compaction(
            &descendant_dataset_id,
            descendant_compaction,
            &membership,
            leader_id,
            &fixture.devices[0],
            first
                .checkpoint_anchor
                .checkpoint_sequence
                .checked_add(1)
                .unwrap(),
            first.checkpoint_anchor.checkpoint_hash.as_deref().unwrap(),
        )
        .unwrap(),
    );
    let supplied_result = synchronize(
        &transport,
        &fixture,
        0,
        &supplied_descendant,
        &first.checkpoint_anchor,
    );
    assert!(
        supplied_result.is_err(),
        "unexpected descendant result: {:?}",
        supplied_result.err()
    );

    let blocked = synchronize(
        &transport,
        &fixture,
        0,
        &second_expired,
        &first.checkpoint_anchor,
    )
    .unwrap();
    assert_eq!(blocked.summary.compacted_engagement_operations, 0);
    assert!(!blocked.summary.compaction_quorum_reached);
    assert_eq!(
        blocked
            .state
            .compaction_checkpoint
            .as_ref()
            .unwrap()
            .checkpoint_id,
        first_compaction.checkpoint_id
    );

    let follower_download = synchronize(
        &transport,
        &fixture,
        1,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    assert_eq!(
        follower_download
            .state
            .compaction_checkpoint
            .as_ref()
            .unwrap()
            .checkpoint_id,
        first_compaction.checkpoint_id
    );
    let follower_ack = synchronize(
        &transport,
        &fixture,
        1,
        &follower_download.state,
        &follower_download.checkpoint_anchor,
    )
    .unwrap();
    let grandchild = synchronize(
        &transport,
        &fixture,
        0,
        &blocked.state,
        &blocked.checkpoint_anchor,
    )
    .unwrap();
    let grandchild_compaction = grandchild.state.compaction_checkpoint.as_ref().unwrap();
    assert!(grandchild.summary.compaction_quorum_reached);
    assert_eq!(grandchild.summary.compacted_engagement_operations, 1);
    assert_eq!(
        grandchild_compaction.previous_checkpoint_hash.as_deref(),
        Some(first_compaction.checkpoint_id.as_str())
    );
    assert_ne!(
        grandchild_compaction.checkpoint_id,
        first_compaction.checkpoint_id
    );
    assert_eq!(
        grandchild.checkpoint_anchor.checkpoint_sequence,
        follower_ack
            .checkpoint_anchor
            .checkpoint_sequence
            .saturating_add(1)
    );
    let grandchild_hash = grandchild
        .checkpoint_anchor
        .checkpoint_hash
        .as_deref()
        .unwrap();
    let grandchild_key = checkpoint_key(
        &grandchild.state.dataset_id,
        fixture.membership.verify(&fixture.anchor).unwrap().epoch,
        grandchild_hash,
    )
    .unwrap();
    let grandchild_object = transport
        .get(&grandchild_key, 2 * 1024 * 1024)
        .unwrap()
        .unwrap()
        .0;
    let signed_grandchild = SignedCheckpoint::decrypt_for_device(
        &grandchild_object,
        &fixture.devices[1],
        &fixture.anchor,
    )
    .unwrap();
    assert_eq!(
        signed_grandchild.payload.previous_checkpoint_hash,
        follower_ack.checkpoint_anchor.checkpoint_hash
    );

    let follower_grandchild_download = synchronize(
        &transport,
        &fixture,
        1,
        &follower_ack.state,
        &follower_ack.checkpoint_anchor,
    )
    .unwrap();
    let _follower_grandchild_ack = synchronize(
        &transport,
        &fixture,
        1,
        &follower_grandchild_download.state,
        &follower_grandchild_download.checkpoint_anchor,
    )
    .unwrap();
    let leader_grandchild_ack = synchronize(
        &transport,
        &fixture,
        0,
        &grandchild.state,
        &grandchild.checkpoint_anchor,
    )
    .unwrap();
    assert!(
        leader_grandchild_ack
            .summary
            .obsolete_compaction_acknowledgements_retained
            >= 2,
        "retired acknowledgement namespaces are reported, not deleted speculatively"
    );
}

#[test]
fn unsupported_delete_defers_gc_without_failing_the_merge() {
    let fixture = fixture();
    let temp = TempRoot::new();
    let transport = FileVaultTransport::create(temp.0.clone()).unwrap();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let compacted = synchronize(
        &transport,
        &fixture,
        0,
        &expired_engagement(&fixture, &bootstrap.state),
        &bootstrap.checkpoint_anchor,
    )
    .unwrap();
    let leader = DeviceId::new(fixture.devices[0].device_id()).unwrap();
    let segment_prefix = segment_prefix(&compacted.state.dataset_id, &leader).unwrap();

    let follower_download = synchronize(
        &transport,
        &fixture,
        1,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let _follower_ack = synchronize(
        &transport,
        &fixture,
        1,
        &follower_download.state,
        &follower_download.checkpoint_anchor,
    )
    .unwrap();
    let unsupported = DeleteUnsupportedTransport(&transport);
    let leader_ack = synchronize(
        &unsupported,
        &fixture,
        0,
        &compacted.state,
        &compacted.checkpoint_anchor,
    )
    .unwrap();

    assert!(leader_ack.summary.compaction_quorum_reached);
    assert!(leader_ack.summary.compaction_gc_deferred);
    assert_eq!(leader_ack.summary.compacted_segments_deleted, 0);
    assert!(!transport.list(&segment_prefix, 10).unwrap().is_empty());
}

#[test]
fn retry_after_during_gc_stops_the_pass_and_reaches_the_scheduler_boundary() {
    let fixture = fixture();
    let temp = TempRoot::new();
    let transport = FileVaultTransport::create(temp.0.clone()).unwrap();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let compacted = synchronize(
        &transport,
        &fixture,
        0,
        &expired_engagement(&fixture, &bootstrap.state),
        &bootstrap.checkpoint_anchor,
    )
    .unwrap();
    let follower_download = synchronize(
        &transport,
        &fixture,
        1,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let _follower_ack = synchronize(
        &transport,
        &fixture,
        1,
        &follower_download.state,
        &follower_download.checkpoint_anchor,
    )
    .unwrap();
    let rate_limited = RateLimitedDeleteTransport {
        inner: &transport,
        delete_attempts: std::sync::atomic::AtomicUsize::new(0),
    };

    let error = match synchronize(
        &rate_limited,
        &fixture,
        0,
        &compacted.state,
        &compacted.checkpoint_anchor,
    ) {
        Ok(_) => panic!("the maintenance pass unexpectedly ignored Retry-After"),
        Err(error) => error,
    };

    assert_eq!(
        error,
        VaultError::RemoteRateLimited(Some(std::time::Duration::from_secs(75)))
    );
    assert_eq!(
        rate_limited
            .delete_attempts
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "a server retry hint must stop the current maintenance pass"
    );
    assert_eq!(
        crate::sync::service::SyncServiceError::from(error),
        crate::sync::service::SyncServiceError::RateLimited(Some(std::time::Duration::from_secs(
            75
        ),))
    );
}

#[test]
fn a_signed_fork_ack_does_not_authorize_a_descendant_compaction() {
    let fixture = fixture();
    let temp = TempRoot::new();
    let transport = FileVaultTransport::create(temp.0.clone()).unwrap();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let compacted = synchronize(
        &transport,
        &fixture,
        0,
        &expired_engagement(&fixture, &bootstrap.state),
        &bootstrap.checkpoint_anchor,
    )
    .unwrap();
    let leader_ack = synchronize(
        &transport,
        &fixture,
        0,
        &compacted.state,
        &compacted.checkpoint_anchor,
    )
    .unwrap();
    let membership = fixture.membership.verify(&fixture.anchor).unwrap();
    let compaction = leader_ack.state.compaction_checkpoint.as_ref().unwrap();
    let follower_id = DeviceId::new(fixture.devices[1].device_id()).unwrap();
    let forged = SignedCompactionAck::create(
        &leader_ack.state.dataset_id,
        compaction,
        leader_ack.checkpoint_anchor.checkpoint_sequence,
        &"f".repeat(64),
        &membership,
        follower_id.clone(),
        &fixture.devices[1],
    )
    .unwrap();
    let forged_key = compaction_ack_key(
        &leader_ack.state.dataset_id,
        compaction,
        &membership,
        &follower_id,
    )
    .unwrap();
    transport
        .put(
            &forged_key,
            &forged.encrypt(compaction, &membership).unwrap(),
            ObjectCondition::CreateOnly,
        )
        .unwrap();

    let with_next_expired = expired_engagement(&fixture, &leader_ack.state);
    let blocked = synchronize(
        &transport,
        &fixture,
        0,
        &with_next_expired,
        &leader_ack.checkpoint_anchor,
    )
    .unwrap();
    assert!(!blocked.summary.compaction_quorum_reached);
    assert_eq!(blocked.summary.compacted_engagement_operations, 0);
    assert_eq!(
        blocked
            .state
            .compaction_checkpoint
            .as_ref()
            .unwrap()
            .checkpoint_id,
        compaction.checkpoint_id
    );
}

#[test]
fn checkpoint_gc_keeps_the_bridge_after_an_old_generation_ack() {
    let fixture = fixture();
    let temp = TempRoot::new();
    let transport = FileVaultTransport::create(temp.0.clone()).unwrap();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let compacted = synchronize(
        &transport,
        &fixture,
        0,
        &expired_engagement(&fixture, &bootstrap.state),
        &bootstrap.checkpoint_anchor,
    )
    .unwrap();
    let follower_download = synchronize(
        &transport,
        &fixture,
        1,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let follower_ack = synchronize(
        &transport,
        &fixture,
        1,
        &follower_download.state,
        &follower_download.checkpoint_anchor,
    )
    .unwrap();
    let mut latest = synchronize(
        &transport,
        &fixture,
        0,
        &compacted.state,
        &compacted.checkpoint_anchor,
    )
    .unwrap();
    let acknowledged_sequence = follower_ack.checkpoint_anchor.checkpoint_sequence;

    for index in 0..10 {
        let changed = like_track(&fixture, &latest.state, &format!("bridge-{index}"));
        latest = synchronize(&transport, &fixture, 0, &changed, &latest.checkpoint_anchor).unwrap();
    }
    assert!(
        latest
            .checkpoint_anchor
            .checkpoint_sequence
            .saturating_sub(acknowledged_sequence)
            >= 10
    );

    let checkpoint_prefix = checkpoint_prefix(&latest.state.dataset_id).unwrap();
    let retained = transport.list(&checkpoint_prefix, 30).unwrap();
    assert!(
        retained.len() >= 10,
        "every checkpoint after the oldest installed acknowledgement must remain"
    );
    let caught_up = synchronize(
        &transport,
        &fixture,
        1,
        &follower_ack.state,
        &follower_ack.checkpoint_anchor,
    )
    .unwrap();
    assert_eq!(
        caught_up.checkpoint_anchor, latest.checkpoint_anchor,
        "the offline device must retain a continuous authenticated checkpoint path"
    );
    let _follower_high_water = synchronize(
        &transport,
        &fixture,
        1,
        &caught_up.state,
        &caught_up.checkpoint_anchor,
    )
    .unwrap();
    let leader_gc = synchronize(
        &transport,
        &fixture,
        0,
        &latest.state,
        &latest.checkpoint_anchor,
    )
    .unwrap();
    assert_eq!(
        transport.list(&checkpoint_prefix, 30).unwrap().len(),
        3,
        "advancing every acknowledgement high-water should resume bounded GC"
    );
    let repeated = synchronize(
        &transport,
        &fixture,
        0,
        &leader_gc.state,
        &leader_gc.checkpoint_anchor,
    )
    .unwrap();
    assert_eq!(repeated.summary.remote_writes, 0);
}

#[test]
fn minimum_ack_checkpoint_remains_a_quorum_anchor_while_a_device_is_offline() {
    let fixture = fixture();
    let temp = TempRoot::new();
    let transport = FileVaultTransport::create(temp.0.clone()).unwrap();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let compacted = synchronize(
        &transport,
        &fixture,
        0,
        &expired_engagement(&fixture, &bootstrap.state),
        &bootstrap.checkpoint_anchor,
    )
    .unwrap();
    let first_compaction = compacted
        .state
        .compaction_checkpoint
        .as_ref()
        .unwrap()
        .clone();
    let follower_download = synchronize(
        &transport,
        &fixture,
        1,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let follower_ack = synchronize(
        &transport,
        &fixture,
        1,
        &follower_download.state,
        &follower_download.checkpoint_anchor,
    )
    .unwrap();
    let mut latest = synchronize(
        &transport,
        &fixture,
        0,
        &compacted.state,
        &compacted.checkpoint_anchor,
    )
    .unwrap();

    for index in 0..6 {
        let changed = like_track(&fixture, &latest.state, &format!("offline-{index}"));
        latest = synchronize(&transport, &fixture, 0, &changed, &latest.checkpoint_anchor).unwrap();
    }
    let minimum_ack_hash = follower_ack
        .checkpoint_anchor
        .checkpoint_hash
        .as_deref()
        .unwrap();
    let minimum_ack_key = checkpoint_key(
        &latest.state.dataset_id,
        fixture.membership.verify(&fixture.anchor).unwrap().epoch,
        minimum_ack_hash,
    )
    .unwrap();
    assert!(
        transport
            .get(&minimum_ack_key, 2 * 1024 * 1024)
            .unwrap()
            .is_some(),
        "the minimum installed acknowledgement must remain available for lineage verification"
    );

    let maintenance = synchronize(
        &transport,
        &fixture,
        0,
        &latest.state,
        &latest.checkpoint_anchor,
    )
    .unwrap();
    assert!(
        maintenance.summary.compaction_quorum_reached,
        "an offline device's authenticated acknowledgement remains valid across ordinary checkpoints"
    );
    let next = synchronize(
        &transport,
        &fixture,
        0,
        &expired_engagement(&fixture, &maintenance.state),
        &maintenance.checkpoint_anchor,
    )
    .unwrap();
    assert!(next.summary.compaction_quorum_reached);
    assert_eq!(next.summary.compacted_engagement_operations, 1);
    assert_eq!(
        next.state
            .compaction_checkpoint
            .as_ref()
            .unwrap()
            .previous_checkpoint_hash
            .as_deref(),
        Some(first_compaction.checkpoint_id.as_str())
    );
}

#[test]
fn quorum_gc_keeps_the_three_newest_checkpoint_generations() {
    let fixture = fixture();
    let temp = TempRoot::new();
    let transport = FileVaultTransport::create(temp.0.clone()).unwrap();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let orphan_state = like_track(&fixture, &bootstrap.state, "unrelated-checkpoint");
    let orphan = SignedCheckpoint::create(
        fixture.membership.clone(),
        &fixture.anchor,
        DeviceId::new(fixture.devices[0].device_id()).unwrap(),
        fixture.devices[0].signing_key(),
        &bootstrap.checkpoint_anchor,
        orphan_state,
    )
    .unwrap();
    let orphan_hash = orphan.hash().unwrap();
    let orphan_key = checkpoint_key(
        &bootstrap.state.dataset_id,
        orphan.payload.membership_epoch,
        &orphan_hash,
    )
    .unwrap();
    transport
        .put(
            &orphan_key,
            &orphan.encrypt(&fixture.anchor).unwrap(),
            ObjectCondition::CreateOnly,
        )
        .unwrap();
    let with_expired_event = expired_engagement(&fixture, &bootstrap.state);
    let mut latest = synchronize(
        &transport,
        &fixture,
        0,
        &with_expired_event,
        &bootstrap.checkpoint_anchor,
    )
    .unwrap();
    for index in 0..3 {
        let changed = like_track(&fixture, &latest.state, &format!("later-{index}"));
        latest = synchronize(&transport, &fixture, 0, &changed, &latest.checkpoint_anchor).unwrap();
    }
    let checkpoint_prefix = checkpoint_prefix(&latest.state.dataset_id).unwrap();
    assert!(transport.list(&checkpoint_prefix, 20).unwrap().len() >= 5);

    let follower_download = synchronize(
        &transport,
        &fixture,
        1,
        &fixture.state,
        &CheckpointAnchor::default(),
    )
    .unwrap();
    let _follower_ack = synchronize(
        &transport,
        &fixture,
        1,
        &follower_download.state,
        &follower_download.checkpoint_anchor,
    )
    .unwrap();
    let leader_gc = synchronize(
        &transport,
        &fixture,
        0,
        &latest.state,
        &latest.checkpoint_anchor,
    )
    .unwrap();

    assert!(leader_gc.summary.old_checkpoints_deleted >= 2);
    assert_eq!(transport.list(&checkpoint_prefix, 20).unwrap().len(), 4);
    assert!(
        transport
            .get(&orphan_key, 2 * 1024 * 1024)
            .unwrap()
            .is_some(),
        "valid checkpoints outside the authenticated latest lineage are never GC targets"
    );
    let acknowledgement_root = ObjectKey::new(format!(
        "yututui/v2/{}/compaction-acks",
        latest.state.dataset_id
    ))
    .unwrap();
    assert_eq!(transport.list(&acknowledgement_root, 20).unwrap().len(), 2);
}
