use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use crate::personal_state::{
    CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateV2, PortableTrack, PortableTrackKey, Rating, VersionVector, append_operation_as,
    project,
};

use super::super::crypto::{DeviceSecretMaterial, EncryptedObject, sha256_domain_hex};
use super::super::{
    CheckpointAnchor, ListCost, ListLimits, ListOutcome, MAX_VAULT_PAYLOAD_BYTES, MembershipAction,
    MembershipAnchor, MembershipChain, ObjectCondition, ObjectKey, ObjectMetadata,
    ObjectWriteResult, RecoveryKit, SignedMembershipRoot, VaultDeadline, VaultError,
    VaultTransport,
};
use super::*;

struct MemoryTransport {
    objects: RefCell<BTreeMap<ObjectKey, Vec<u8>>>,
    writes: Cell<usize>,
    lose_next_response: Cell<bool>,
    reject_next_match: Cell<bool>,
    reject_manifest_matches: Cell<bool>,
    hide_once: RefCell<Option<ObjectKey>>,
    replace_on_get: RefCell<Option<(ObjectKey, Vec<u8>, usize)>>,
    next_reported_length: Cell<Option<u64>>,
    list_cost: Cell<Option<(usize, usize, usize, usize)>>,
    observed_list_limits: RefCell<Vec<ListLimits>>,
}

impl MemoryTransport {
    fn new() -> Self {
        Self {
            objects: RefCell::new(BTreeMap::new()),
            writes: Cell::new(0),
            lose_next_response: Cell::new(false),
            reject_next_match: Cell::new(false),
            reject_manifest_matches: Cell::new(false),
            hide_once: RefCell::new(None),
            replace_on_get: RefCell::new(None),
            next_reported_length: Cell::new(None),
            list_cost: Cell::new(None),
            observed_list_limits: RefCell::new(Vec::new()),
        }
    }

    fn writes(&self) -> usize {
        self.writes.get()
    }

    fn lose_next_response(&self) {
        self.lose_next_response.set(true);
    }

    fn reject_next_match(&self) {
        self.reject_next_match.set(true);
    }

    fn reject_manifest_matches(&self, reject: bool) {
        self.reject_manifest_matches.set(reject);
    }

    fn hide_once(&self, key: ObjectKey) {
        *self.hide_once.borrow_mut() = Some(key);
    }

    fn replace_on_second_get(&self, key: ObjectKey, bytes: Vec<u8>) {
        *self.replace_on_get.borrow_mut() = Some((key, bytes, 1));
    }

    fn report_next_length(&self, length: u64) {
        self.next_reported_length.set(Some(length));
    }

    fn measure_recursive_lists(
        &self,
        requests: usize,
        response_bytes: usize,
        scanned_collections: usize,
        extra_scanned_resources: usize,
    ) {
        self.list_cost.set(Some((
            requests,
            response_bytes,
            scanned_collections,
            extra_scanned_resources,
        )));
        self.observed_list_limits.borrow_mut().clear();
    }

    fn observed_list_limits(&self) -> Vec<ListLimits> {
        self.observed_list_limits.borrow().clone()
    }
}

impl VaultTransport for MemoryTransport {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        if self.hide_once.borrow().as_ref() == Some(key) {
            self.hide_once.borrow_mut().take();
            return Ok(None);
        }
        let replacement = {
            let mut staged = self.replace_on_get.borrow_mut();
            match staged.as_mut() {
                Some((staged_key, _, remaining)) if staged_key == key && *remaining > 0 => {
                    *remaining -= 1;
                    None
                }
                Some((staged_key, _, _)) if staged_key == key => staged.take(),
                _ => None,
            }
        };
        if let Some((_, bytes, _)) = replacement {
            self.objects.borrow_mut().insert(key.clone(), bytes);
        }
        let objects = self.objects.borrow();
        let Some(bytes) = objects.get(key) else {
            return Ok(None);
        };
        if bytes.len() > max_bytes.saturating_add(4 * 1024 * 1024) {
            return Err(VaultError::PayloadTooLarge);
        }
        let mut metadata = metadata(key, bytes);
        if let Some(length) = self.next_reported_length.take() {
            metadata.content_length = length;
        }
        Ok(Some((
            EncryptedObject::from_bytes(bytes.clone())?,
            metadata,
        )))
    }

    fn put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError> {
        if !object.is_locally_produced() {
            return Err(VaultError::InvalidEncryptedObject);
        }
        if matches!(condition, ObjectCondition::Match(_))
            && key.as_str().ends_with("/manifest")
            && self.reject_manifest_matches.get()
        {
            return Err(VaultError::PreconditionFailed);
        }
        if matches!(condition, ObjectCondition::Match(_)) && self.reject_next_match.replace(false) {
            return Err(VaultError::PreconditionFailed);
        }
        let mut objects = self.objects.borrow_mut();
        let current = objects.get(key);
        match (&condition, current) {
            (ObjectCondition::CreateOnly, Some(_)) => {
                return Err(VaultError::PreconditionFailed);
            }
            (ObjectCondition::Match(expected), Some(bytes))
                if &metadata(key, bytes).etag != expected =>
            {
                return Err(VaultError::PreconditionFailed);
            }
            (ObjectCondition::Match(_), None) => return Err(VaultError::PreconditionFailed),
            _ => {}
        }
        let existed = current.is_some();
        objects.insert(key.clone(), object.as_bytes().to_vec());
        self.writes.set(self.writes.get().saturating_add(1));
        let result_metadata = metadata(key, object.as_bytes());
        if self.lose_next_response.replace(false) {
            return Err(VaultError::StorageFailed);
        }
        Ok(if existed {
            ObjectWriteResult::Updated(result_metadata)
        } else {
            ObjectWriteResult::Created(result_metadata)
        })
    }

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        let prefix = format!("{}/", prefix.as_str());
        let objects = self.objects.borrow();
        let result = objects
            .iter()
            .filter(|(key, _)| key.as_str().starts_with(&prefix))
            .map(|(key, bytes)| metadata(key, bytes))
            .collect::<Vec<_>>();
        if result.len() > max_resources {
            return Err(VaultError::ResourceLimitExceeded);
        }
        Ok(result)
    }

    fn list_with_limits(
        &self,
        prefix: &ObjectKey,
        limits: ListLimits,
        deadline: VaultDeadline,
    ) -> Result<ListOutcome, VaultError> {
        deadline.check()?;
        self.observed_list_limits.borrow_mut().push(limits);
        let objects = self.list(prefix, limits.returned_objects)?;
        let (requests, response_bytes, scanned_collections, extra_scanned_resources) =
            self.list_cost.get().unwrap_or((1, 0, 1, 0));
        let outcome = ListOutcome {
            cost: ListCost {
                requests,
                response_bytes,
                scanned_collections,
                scanned_resources: objects.len().saturating_add(extra_scanned_resources),
                returned_objects: objects.len(),
            },
            objects,
        };
        outcome.validate(limits)?;
        Ok(outcome)
    }
}

fn synchronize_result(
    transport: &MemoryTransport,
    fixture: &Fixture,
    device_index: usize,
    state: &PersonalStateV2,
    checkpoint_anchor: &CheckpointAnchor,
) -> Result<ManualSyncCandidate, VaultError> {
    let input = ManualSyncInput {
        local_state: state,
        membership: &fixture.membership,
        membership_anchor: &fixture.anchor,
        device: &fixture.devices[device_index],
        checkpoint_anchor,
        bootstrap_checkpoint: None,
        expected_local_revision: state.revision,
    };
    ManualSyncEngine::new(transport).synchronize(&input, &|expected| {
        if expected == state.revision {
            Ok(())
        } else {
            Err(VaultError::RevisionConflict)
        }
    })
}

fn metadata(key: &ObjectKey, bytes: &[u8]) -> ObjectMetadata {
    ObjectMetadata {
        key: key.clone(),
        etag: sha256_domain_hex(b"yututui-vault-etag-v1", &[bytes]),
        content_length: bytes.len() as u64,
    }
}

struct Fixture {
    devices: Vec<DeviceSecretMaterial>,
    membership: MembershipChain,
    anchor: MembershipAnchor,
    state: PersonalStateV2,
}

fn fixture(device_count: usize) -> Fixture {
    assert!((2..=3).contains(&device_count));
    let kit = RecoveryKit::generate("dataset-manual-sync", None).unwrap();
    let devices = (1..=device_count)
        .map(|index| DeviceSecretMaterial::generate_for(format!("device-{index}")).unwrap())
        .collect::<Vec<_>>();
    let records = devices
        .iter()
        .enumerate()
        .map(|(index, device)| DeviceRecord {
            device_id: DeviceId::new(device.device_id()).unwrap(),
            name: format!("Device {}", index + 1),
            revoked: false,
            public_identity: Some(device.public_identity()),
        })
        .collect::<Vec<_>>();
    let root = SignedMembershipRoot::create(
        "dataset-manual-sync",
        kit.recovery_recipient(),
        &kit.signing_key().unwrap(),
        records[0].clone(),
    )
    .unwrap();
    let anchor = MembershipAnchor::RootHash(root.hash().unwrap());
    let mut membership = MembershipChain::new(root);
    for record in records.iter().skip(1) {
        membership
            .append_device_action(
                &anchor,
                &records[0].device_id,
                devices[0].signing_key(),
                MembershipAction::AddDevice {
                    device: record.clone(),
                },
            )
            .unwrap();
    }

    let first_dot = Dot {
        device_id: records[0].device_id.clone(),
        sequence: 1,
    };
    let mut state = PersonalStateV2::empty("dataset-manual-sync".to_owned()).unwrap();
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
    for record in records.iter().skip(1) {
        state = append_operation_as(
            &state,
            &records[0].device_id,
            Operation::AddDevice {
                device: record.clone(),
            },
            0,
        )
        .unwrap();
    }
    assert_eq!(
        state.device_registry,
        membership.verify(&anchor).unwrap().devices
    );
    Fixture {
        devices,
        membership,
        anchor,
        state,
    }
}

fn track(id: &str) -> PortableTrack {
    PortableTrack {
        key: PortableTrackKey::Catalog {
            provider: "youtube".to_owned(),
            exact_catalog_id: id.to_owned(),
        },
        title: format!("Track {id}"),
        artist: "Artist".to_owned(),
        album: None,
        duration_secs: Some(180),
        isrc: None,
    }
}

fn rate(
    state: &PersonalStateV2,
    device: &DeviceSecretMaterial,
    id: &str,
    rating: Rating,
) -> PersonalStateV2 {
    append_operation_as(
        state,
        &DeviceId::new(device.device_id()).unwrap(),
        Operation::SetRating {
            track: track(id),
            rating,
        },
        100,
    )
    .unwrap()
}

fn synchronize(
    transport: &MemoryTransport,
    fixture: &Fixture,
    device_index: usize,
    state: &PersonalStateV2,
    checkpoint_anchor: &CheckpointAnchor,
) -> ManualSyncCandidate {
    synchronize_result(transport, fixture, device_index, state, checkpoint_anchor).unwrap()
}

fn leave_unpublished_two_operation_head(
    transport: &MemoryTransport,
    fixture: &Fixture,
    bootstrap: &ManualSyncCandidate,
) -> PersonalStateV2 {
    let first = rate(
        &bootstrap.state,
        &fixture.devices[0],
        "orphan-a",
        Rating::Liked,
    );
    let second = rate(&first, &fixture.devices[0], "orphan-b", Rating::Disliked);
    transport.reject_manifest_matches(true);
    assert_eq!(
        synchronize_result(transport, fixture, 0, &second, &bootstrap.checkpoint_anchor,).err(),
        Some(VaultError::PreconditionFailed)
    );
    transport.reject_manifest_matches(false);
    second
}

#[test]
fn two_clients_converge_and_a_no_op_sync_writes_nothing() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    );
    let initial_anchor = bootstrap.checkpoint_anchor.clone();
    let first = rate(
        &bootstrap.state,
        &fixture.devices[0],
        "video-a",
        Rating::Liked,
    );
    let second = rate(
        &bootstrap.state,
        &fixture.devices[1],
        "video-b",
        Rating::Disliked,
    );

    let first_synced = synchronize(&transport, &fixture, 0, &first, &initial_anchor);
    let second_synced = synchronize(&transport, &fixture, 1, &second, &initial_anchor);
    let first_converged = synchronize(
        &transport,
        &fixture,
        0,
        &first_synced.state,
        &first_synced.checkpoint_anchor,
    );
    assert_eq!(
        project(&first_converged.state).unwrap().legacy,
        project(&second_synced.state).unwrap().legacy
    );
    assert_eq!(
        first_converged.state.operations,
        second_synced.state.operations
    );

    let writes_before = transport.writes();
    let no_op = synchronize(
        &transport,
        &fixture,
        0,
        &first_converged.state,
        &first_converged.checkpoint_anchor,
    );
    assert_eq!(transport.writes(), writes_before);
    assert_eq!(no_op.summary.remote_writes, 0);
    assert!(!no_op.summary.manifest_written);
}

#[test]
fn three_clients_converge_across_a_checkpoint_gap() {
    let fixture = fixture(3);
    let transport = MemoryTransport::new();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    );
    let initial_anchor = bootstrap.checkpoint_anchor.clone();
    let states = [
        rate(
            &bootstrap.state,
            &fixture.devices[0],
            "video-a",
            Rating::Liked,
        ),
        rate(
            &bootstrap.state,
            &fixture.devices[1],
            "video-b",
            Rating::Disliked,
        ),
        rate(
            &bootstrap.state,
            &fixture.devices[2],
            "video-c",
            Rating::Liked,
        ),
    ];
    let first = synchronize(&transport, &fixture, 0, &states[0], &initial_anchor);
    let second = synchronize(&transport, &fixture, 1, &states[1], &initial_anchor);
    let third = synchronize(&transport, &fixture, 2, &states[2], &initial_anchor);
    let first = synchronize(
        &transport,
        &fixture,
        0,
        &first.state,
        &first.checkpoint_anchor,
    );
    let second = synchronize(
        &transport,
        &fixture,
        1,
        &second.state,
        &second.checkpoint_anchor,
    );
    assert_eq!(first.state.operations, second.state.operations);
    assert_eq!(first.state.operations, third.state.operations);
    assert_eq!(
        project(&first.state).unwrap().legacy,
        project(&third.state).unwrap().legacy
    );
}

#[test]
fn conditional_race_refetches_and_response_loss_uses_readback() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    transport.lose_next_response();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    );
    leave_unpublished_two_operation_head(&transport, &fixture, &bootstrap);
    let changed = rate(
        &bootstrap.state,
        &fixture.devices[1],
        "video-race",
        Rating::Liked,
    );
    transport.measure_recursive_lists(3, 128, 2, 2);
    transport.reject_next_match();
    let synced = synchronize(
        &transport,
        &fixture,
        1,
        &changed,
        &bootstrap.checkpoint_anchor,
    );
    assert_eq!(synced.summary.attempts, 2);
    let limits = transport.observed_list_limits();
    assert!(
        limits.len() >= 2,
        "both 412 attempts must charge their recursive listings"
    );
    for pair in limits.windows(2) {
        assert!(pair[1].requests < pair[0].requests);
        assert!(pair[1].response_bytes < pair[0].response_bytes);
        assert!(pair[1].scanned_collections < pair[0].scanned_collections);
        assert!(pair[1].scanned_resources < pair[0].scanned_resources);
    }
    assert!(
        project(&synced.state)
            .unwrap()
            .legacy
            .favorites
            .iter()
            .any(|favorite| favorite.key == track("video-race").key)
    );
}

#[test]
fn stale_revision_is_rejected_before_any_remote_write() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    let input = ManualSyncInput {
        local_state: &fixture.state,
        membership: &fixture.membership,
        membership_anchor: &fixture.anchor,
        device: &fixture.devices[0],
        checkpoint_anchor: &CheckpointAnchor::default(),
        bootstrap_checkpoint: None,
        expected_local_revision: fixture.state.revision,
    };
    assert_eq!(
        ManualSyncEngine::new(&transport)
            .synchronize(&input, &|_| Err(VaultError::RevisionConflict))
            .err(),
        Some(VaultError::RevisionConflict)
    );
    assert_eq!(transport.writes(), 0);
}

#[test]
fn expired_whole_sync_deadline_fails_before_transport_io() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    let input = ManualSyncInput {
        local_state: &fixture.state,
        membership: &fixture.membership,
        membership_anchor: &fixture.anchor,
        device: &fixture.devices[0],
        checkpoint_anchor: &CheckpointAnchor::default(),
        bootstrap_checkpoint: None,
        expected_local_revision: fixture.state.revision,
    };
    assert_eq!(
        ManualSyncEngine::new(&transport)
            .synchronize_with_deadline(&input, &|_| Ok(()), VaultDeadline::expired())
            .err(),
        Some(VaultError::RemoteUnavailable)
    );
    assert_eq!(transport.writes(), 0);
    assert!(transport.observed_list_limits().is_empty());
}

#[test]
fn interpreted_payload_budget_is_enforced_across_remote_reads() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    );
    transport.report_next_length(MAX_VAULT_PAYLOAD_BYTES as u64 + 1);
    let input = ManualSyncInput {
        local_state: &bootstrap.state,
        membership: &fixture.membership,
        membership_anchor: &fixture.anchor,
        device: &fixture.devices[0],
        checkpoint_anchor: &bootstrap.checkpoint_anchor,
        bootstrap_checkpoint: None,
        expected_local_revision: bootstrap.state.revision,
    };
    assert_eq!(
        ManualSyncEngine::new(&transport)
            .synchronize(&input, &|_| Ok(()))
            .err(),
        Some(VaultError::PayloadTooLarge)
    );
}

#[test]
fn more_than_512_operations_are_split_into_bounded_segments() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    );
    let mut changed = bootstrap.state.clone();
    for index in 0..513 {
        changed = rate(
            &changed,
            &fixture.devices[0],
            &format!("video-{index}"),
            Rating::Liked,
        );
    }
    let synced = synchronize(
        &transport,
        &fixture,
        0,
        &changed,
        &bootstrap.checkpoint_anchor,
    );
    assert_eq!(synced.summary.uploaded_operations, 513);
    assert_eq!(synced.summary.uploaded_segments, 2);
}

#[test]
fn same_device_concurrent_head_cannot_be_overwritten_backwards() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    );
    leave_unpublished_two_operation_head(&transport, &fixture, &bootstrap);

    let device_id = DeviceId::new(fixture.devices[0].device_id()).unwrap();
    let head_key = device_head_key(&bootstrap.state.dataset_id, &device_id).unwrap();
    let head_before = transport.objects.borrow().get(&head_key).unwrap().clone();
    transport.hide_once(head_key.clone());
    let competing = rate(
        &bootstrap.state,
        &fixture.devices[0],
        "competing",
        Rating::Liked,
    );

    assert_eq!(
        synchronize_result(
            &transport,
            &fixture,
            0,
            &competing,
            &bootstrap.checkpoint_anchor,
        )
        .err(),
        Some(VaultError::InvalidEncryptedObject)
    );
    assert_eq!(
        transport.objects.borrow().get(&head_key),
        Some(&head_before)
    );
    assert!(
        !transport
            .objects
            .borrow()
            .contains_key(&segment_key(&bootstrap.state.dataset_id, &device_id, 3, 3).unwrap())
    );
}

#[test]
fn same_sequence_head_fork_cannot_supply_the_upload_etag() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    );
    let advanced = leave_unpublished_two_operation_head(&transport, &fixture, &bootstrap);
    let membership = fixture.membership.verify(&fixture.anchor).unwrap();
    let device_id = DeviceId::new(fixture.devices[0].device_id()).unwrap();
    let head_key = device_head_key(&advanced.dataset_id, &device_id).unwrap();
    let head_bytes = transport.objects.borrow().get(&head_key).unwrap().clone();
    let head = SignedDeviceHead::decrypt_for_device(
        &EncryptedObject::from_bytes(head_bytes).unwrap(),
        &fixture.devices[0],
        &membership,
    )
    .unwrap();
    let fork = SignedDeviceHead::create(
        &advanced.dataset_id,
        &membership,
        device_id,
        &fixture.devices[0],
        head.payload.last_sequence,
        "0".repeat(64),
        head.payload.last_segment_key,
    )
    .unwrap()
    .encrypt(&membership)
    .unwrap();
    let fork_bytes = fork.as_bytes().to_vec();
    transport.replace_on_second_get(head_key.clone(), fork_bytes.clone());
    let pending = rate(&advanced, &fixture.devices[0], "after-fork", Rating::Liked);

    assert_eq!(
        synchronize_result(
            &transport,
            &fixture,
            0,
            &pending,
            &bootstrap.checkpoint_anchor,
        )
        .err(),
        Some(VaultError::RollbackDetected)
    );
    assert_eq!(transport.objects.borrow().get(&head_key), Some(&fork_bytes));
}

#[test]
fn signed_head_must_name_the_terminal_segment() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    );
    let advanced = leave_unpublished_two_operation_head(&transport, &fixture, &bootstrap);
    let membership = fixture.membership.verify(&fixture.anchor).unwrap();
    let device_id = DeviceId::new(fixture.devices[0].device_id()).unwrap();
    let head_key = device_head_key(&advanced.dataset_id, &device_id).unwrap();
    let head_bytes = transport.objects.borrow().get(&head_key).unwrap().clone();
    let head = SignedDeviceHead::decrypt_for_device(
        &EncryptedObject::from_bytes(head_bytes).unwrap(),
        &fixture.devices[0],
        &membership,
    )
    .unwrap();
    let wrong_terminal = segment_key(
        &advanced.dataset_id,
        &device_id,
        head.payload.last_sequence,
        head.payload.last_sequence,
    )
    .unwrap();
    let forged = SignedDeviceHead::create(
        &advanced.dataset_id,
        &membership,
        device_id,
        &fixture.devices[0],
        head.payload.last_sequence,
        head.payload.last_batch_hash,
        wrong_terminal,
    )
    .unwrap()
    .encrypt(&membership)
    .unwrap();
    transport
        .objects
        .borrow_mut()
        .insert(head_key, forged.as_bytes().to_vec());

    assert_eq!(
        synchronize_result(
            &transport,
            &fixture,
            0,
            &bootstrap.state,
            &bootstrap.checkpoint_anchor,
        )
        .err(),
        Some(VaultError::RollbackDetected)
    );
}

#[test]
fn unreadable_head_with_uncheckpointed_segments_is_rejected() {
    let fixture = fixture(2);
    let transport = MemoryTransport::new();
    let bootstrap = synchronize(
        &transport,
        &fixture,
        0,
        &fixture.state,
        &CheckpointAnchor::default(),
    );
    leave_unpublished_two_operation_head(&transport, &fixture, &bootstrap);
    let device_id = DeviceId::new(fixture.devices[0].device_id()).unwrap();
    let head_key = device_head_key(&bootstrap.state.dataset_id, &device_id).unwrap();
    let mut corrupted = transport.objects.borrow().get(&head_key).unwrap().clone();
    *corrupted.last_mut().unwrap() ^= 1;
    transport.objects.borrow_mut().insert(head_key, corrupted);

    assert_eq!(
        synchronize_result(
            &transport,
            &fixture,
            0,
            &bootstrap.state,
            &bootstrap.checkpoint_anchor,
        )
        .err(),
        Some(VaultError::DecryptionFailed)
    );
}
