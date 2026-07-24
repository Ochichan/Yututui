use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use age::secrecy::SecretString;

use crate::personal_state::{
    CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateV2, PortableTrack, PortableTrackKey, Rating, VersionVector, append_operation_as,
};
use crate::sync::{
    CheckpointAnchor, DeviceSecretMaterial, EncryptedObject, FileVaultTransport, MembershipAction,
    MembershipAnchor, MembershipChain, ObjectCondition, ObjectKey, ObjectMetadata,
    ObjectWriteResult, PrivateStore, PrivateStoreSnapshot, RecoveryKit, SignedCheckpoint,
    SignedMembershipRoot, SyncPaths, VaultCredential, VaultDeadline, VaultError, VaultTransport,
    WebDavProfile, WebDavProfileStore,
};

use super::super::super::manual::{ManualSyncEngine, ManualSyncInput};
use super::*;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempVault(std::path::PathBuf);

impl TempVault {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "yututui-revoke-sync-{}-{sequence}",
            std::process::id()
        ));
        crate::util::safe_fs::ensure_private_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempVault {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    devices: Vec<DeviceSecretMaterial>,
    membership: MembershipChain,
    anchor: MembershipAnchor,
    state: PersonalStateV2,
    checkpoint: SignedCheckpoint,
    private: PrivateStoreSnapshot,
}

fn fixture() -> Fixture {
    let dataset_id = "dataset-revoke-service";
    let recovery = RecoveryKit::generate(dataset_id, None).unwrap();
    let devices = vec![
        DeviceSecretMaterial::generate_for("device-owner").unwrap(),
        DeviceSecretMaterial::generate_for("device-target").unwrap(),
    ];
    let records = devices
        .iter()
        .enumerate()
        .map(|(index, device)| DeviceRecord {
            device_id: DeviceId::new(device.device_id()).unwrap(),
            name: format!("Device {index}"),
            revoked: false,
            public_identity: Some(device.public_identity()),
        })
        .collect::<Vec<_>>();
    let root = SignedMembershipRoot::create(
        dataset_id,
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
    let mut state = PersonalStateV2::empty(dataset_id.to_owned()).unwrap();
    state.operations.push(OperationEnvelope {
        operation_id: "initial-device".to_owned(),
        stamp: CausalStamp {
            dot: first_dot.clone(),
            observed: VersionVector::default(),
            recorded_at_unix: 1,
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
        2,
    )
    .unwrap();

    let checkpoint = SignedCheckpoint::create(
        membership.clone(),
        &anchor,
        records[0].device_id.clone(),
        devices[0].signing_key(),
        &CheckpointAnchor::default(),
        state.clone(),
    )
    .unwrap();
    let mut private = PrivateStoreSnapshot::pending_ledger_commit(
        DeviceSecretMaterial::from_encoded(
            devices[0].device_id(),
            devices[0].age_identity_secret(),
            &devices[0].signing_key_secret_b64(),
        )
        .unwrap(),
        recovery.recovery_recipient(),
        recovery.recovery_verifying_key().unwrap(),
        match &anchor {
            MembershipAnchor::RootHash(hash) => hash.clone(),
            MembershipAnchor::RecoveryVerifyingKey(_) => unreachable!(),
        },
        &checkpoint,
    )
    .unwrap();
    private.mark_active(&checkpoint, &state).unwrap();
    Fixture {
        devices,
        membership,
        anchor,
        state,
        checkpoint,
        private,
    }
}

fn synchronize(
    transport: &FileVaultTransport,
    state: &PersonalStateV2,
    membership: &MembershipChain,
    anchor: &MembershipAnchor,
    device: &DeviceSecretMaterial,
    checkpoint_anchor: &CheckpointAnchor,
    bootstrap: Option<&SignedCheckpoint>,
) -> crate::sync::manual::ManualSyncCandidate {
    let input = ManualSyncInput {
        local_state: state,
        membership,
        membership_anchor: anchor,
        device,
        checkpoint_anchor,
        bootstrap_checkpoint: bootstrap,
        expected_local_revision: state.revision,
    };
    ManualSyncEngine::new(transport)
        .synchronize(&input, &|expected| {
            if expected == state.revision {
                Ok(())
            } else {
                Err(crate::sync::VaultError::RevisionConflict)
            }
        })
        .unwrap()
}

struct CountingTransport {
    inner: FileVaultTransport,
    gets: Cell<usize>,
    puts: Rc<Cell<usize>>,
}

impl VaultTransport for CountingTransport {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.gets.set(self.gets.get().saturating_add(1));
        self.inner.get(key, max_bytes)
    }

    fn put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError> {
        self.puts.set(self.puts.get().saturating_add(1));
        self.inner.put(key, object, condition)
    }

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.inner.list(prefix, max_resources)
    }
}

struct RacingTransport<'a> {
    inner: FileVaultTransport,
    manifest_gets: Cell<usize>,
    injected: Cell<bool>,
    state: &'a PersonalStateV2,
    membership: &'a MembershipChain,
    anchor: &'a MembershipAnchor,
    device: &'a DeviceSecretMaterial,
    checkpoint_anchor: &'a CheckpointAnchor,
    total_gets: Cell<usize>,
    list_calls: Cell<usize>,
}

impl VaultTransport for RacingTransport<'_> {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.total_gets.set(self.total_gets.get().saturating_add(1));
        if key.as_str().ends_with("/manifest") {
            let count = self.manifest_gets.get().saturating_add(1);
            self.manifest_gets.set(count);
            // Each preparation reads the manifest once to select the membership prefix and once
            // inside the engine. The third read is the revocation phase after its cutoff was set.
            if count == 3 && !self.injected.replace(true) {
                synchronize(
                    &self.inner,
                    self.state,
                    self.membership,
                    self.anchor,
                    self.device,
                    self.checkpoint_anchor,
                    None,
                );
            }
        }
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

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.inner.list(prefix, max_resources)
    }

    fn list_with_limits(
        &self,
        prefix: &ObjectKey,
        limits: crate::sync::ListLimits,
        deadline: VaultDeadline,
    ) -> Result<crate::sync::ListOutcome, VaultError> {
        self.list_calls.set(self.list_calls.get().saturating_add(1));
        self.inner.list_with_limits(prefix, limits, deadline)
    }
}

struct ManifestCasRejectingTransport<'a> {
    inner: &'a FileVaultTransport,
}

impl VaultTransport for ManifestCasRejectingTransport<'_> {
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
        if key.as_str().ends_with("/manifest") && matches!(&condition, ObjectCondition::Match(_)) {
            return Err(VaultError::PreconditionFailed);
        }
        self.inner.put(key, object, condition)
    }

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.inner.list(prefix, max_resources)
    }

    fn list_with_limits(
        &self,
        prefix: &ObjectKey,
        limits: crate::sync::ListLimits,
        deadline: VaultDeadline,
    ) -> Result<crate::sync::ListOutcome, VaultError> {
        self.inner.list_with_limits(prefix, limits, deadline)
    }
}

struct HeadOnlyRacingTransport<'a> {
    inner: FileVaultTransport,
    target_head_key: ObjectKey,
    target_head_gets: Cell<usize>,
    injected: Cell<bool>,
    state: &'a PersonalStateV2,
    membership: &'a MembershipChain,
    anchor: &'a MembershipAnchor,
    device: &'a DeviceSecretMaterial,
    checkpoint_anchor: &'a CheckpointAnchor,
    total_gets: Cell<usize>,
    list_calls: Cell<usize>,
}

impl VaultTransport for HeadOnlyRacingTransport<'_> {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.total_gets.set(self.total_gets.get().saturating_add(1));
        if key == &self.target_head_key {
            let count = self.target_head_gets.get().saturating_add(1);
            self.target_head_gets.set(count);
            // The first read is the pre-revoke merge, the second freezes the cutoff, and the
            // third is the final head revalidation immediately before the revocation manifest CAS.
            if count == 3 && !self.injected.replace(true) {
                let transport = ManifestCasRejectingTransport { inner: &self.inner };
                let input = ManualSyncInput {
                    local_state: self.state,
                    membership: self.membership,
                    membership_anchor: self.anchor,
                    device: self.device,
                    checkpoint_anchor: self.checkpoint_anchor,
                    bootstrap_checkpoint: None,
                    expected_local_revision: self.state.revision,
                };
                let result = ManualSyncEngine::new(&transport).synchronize(&input, &|expected| {
                    if expected == self.state.revision {
                        Ok(())
                    } else {
                        Err(VaultError::RevisionConflict)
                    }
                });
                assert_eq!(result.err(), Some(VaultError::PreconditionFailed));
            }
        }
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

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.inner.list(prefix, max_resources)
    }

    fn list_with_limits(
        &self,
        prefix: &ObjectKey,
        limits: crate::sync::ListLimits,
        deadline: VaultDeadline,
    ) -> Result<crate::sync::ListOutcome, VaultError> {
        self.list_calls.set(self.list_calls.get().saturating_add(1));
        self.inner.list_with_limits(prefix, limits, deadline)
    }
}

#[test]
fn saved_profile_no_op_path_never_probes_or_writes() {
    let mut fixture = fixture();
    let directory = TempVault::new();
    let inner = FileVaultTransport::create(directory.0.join("remote")).unwrap();
    let _ = synchronize(
        &inner,
        &fixture.state,
        &fixture.membership,
        &fixture.anchor,
        &fixture.devices[0],
        &CheckpointAnchor::default(),
        Some(&fixture.checkpoint),
    );
    fixture.private.set_credential(
        VaultCredential::bearer_token(SecretString::from("saved-profile-token")).unwrap(),
    );
    let paths = SyncPaths::for_data_root(directory.0.clone());
    PrivateStore::new(paths.private_store())
        .unwrap()
        .create(&mut fixture.private)
        .unwrap();
    let mut profile = WebDavProfile::new(
        fixture.state.dataset_id.clone(),
        fixture.private.device(),
        "https://dav.example.test/state",
    )
    .unwrap();
    WebDavProfileStore::new(paths.profile())
        .unwrap()
        .create(&mut profile, fixture.private.device())
        .unwrap();
    let transport = CountingTransport {
        inner,
        gets: Cell::new(0),
        puts: Rc::new(Cell::new(0)),
    };
    let puts = Rc::clone(&transport.puts);
    let opened = Cell::new(false);

    let prepared = super::super::manual::prepare_manual_sync_using(
        &fixture.state,
        &paths,
        |saved_profile, _credential| {
            opened.set(true);
            assert_eq!(saved_profile.endpoint(), "https://dav.example.test/state/");
            Ok(transport)
        },
    )
    .unwrap();

    assert!(opened.get());
    assert!(!prepared.has_changes());
    assert_eq!(prepared.summary.remote_writes, 0);
    // The transport factory exposes only VaultTransport operations; the saved-profile path has
    // no capability-probe surface to call. A true no-op therefore performs no remote PUT.
    assert_eq!(prepared.summary.uploaded_operations, 0);
    assert_eq!(puts.get(), 0);
}

#[test]
fn expired_manual_budget_stops_before_the_membership_get() {
    let fixture = fixture();
    let directory = TempVault::new();
    let transport = CountingTransport {
        inner: FileVaultTransport::create(directory.0.clone()).unwrap(),
        gets: Cell::new(0),
        puts: Rc::new(Cell::new(0)),
    };
    let mut budget =
        super::super::super::manual::ManualSyncBudget::with_deadline(VaultDeadline::expired());

    assert_eq!(
        prepare_manual_sync_with_budget(&fixture.state, &fixture.private, &transport, &mut budget,)
            .err(),
        Some(SyncServiceError::Offline)
    );
    assert_eq!(transport.gets.get(), 0);
}

#[test]
fn revoke_first_merges_unseen_target_operations_and_uses_the_exact_cutoff() {
    let fixture = fixture();
    let directory = TempVault::new();
    let transport = FileVaultTransport::create(directory.0.clone()).unwrap();
    let bootstrapped = synchronize(
        &transport,
        &fixture.state,
        &fixture.membership,
        &fixture.anchor,
        &fixture.devices[0],
        &CheckpointAnchor::default(),
        Some(&fixture.checkpoint),
    );
    let target_id = DeviceId::new(fixture.devices[1].device_id()).unwrap();
    let remote_state = append_operation_as(
        &fixture.state,
        &target_id,
        Operation::SetRating {
            track: PortableTrack {
                key: PortableTrackKey::Catalog {
                    provider: "youtube".to_owned(),
                    exact_catalog_id: "unseen-before-revoke".to_owned(),
                },
                title: "Unseen before revoke".to_owned(),
                artist: "Remote artist".to_owned(),
                album: None,
                duration_secs: Some(180),
                isrc: None,
            },
            rating: Rating::Liked,
        },
        3,
    )
    .unwrap();
    let remote = synchronize(
        &transport,
        &remote_state,
        &fixture.membership,
        &fixture.anchor,
        &fixture.devices[1],
        &bootstrapped.checkpoint_anchor,
        None,
    );
    assert_eq!(remote.state.version_vector.observed(&target_id), 1);

    let revoked =
        revoke_with_transport(&fixture.state, &target_id, &fixture.private, &transport).unwrap();

    assert_eq!(revoked.state.version_vector.observed(&target_id), 1);
    assert!(
        revoked
            .state
            .operations
            .iter()
            .any(|operation| operation.operation_id == "device-target:1")
    );
    assert!(revoked.state.device_registry[&target_id].revoked);
    let verified = revoked.membership.verify(&fixture.anchor).unwrap();
    assert_eq!(verified.revocation_cutoffs.get(&target_id), Some(&1));
    assert_eq!(verified.devices, revoked.state.device_registry);

    let no_op = synchronize(
        &transport,
        &revoked.state,
        &revoked.membership,
        &fixture.anchor,
        &fixture.devices[0],
        &revoked.checkpoint_anchor,
        None,
    );
    assert_eq!(no_op.state, revoked.state);
    assert_eq!(no_op.summary.remote_writes, 0);
}

#[test]
fn revoke_rebases_when_target_writes_between_high_water_and_manifest_update() {
    let fixture = fixture();
    let directory = TempVault::new();
    let inner = FileVaultTransport::create(directory.0.clone()).unwrap();
    let bootstrapped = synchronize(
        &inner,
        &fixture.state,
        &fixture.membership,
        &fixture.anchor,
        &fixture.devices[0],
        &CheckpointAnchor::default(),
        Some(&fixture.checkpoint),
    );
    let target_id = DeviceId::new(fixture.devices[1].device_id()).unwrap();
    let racing_state = append_operation_as(
        &fixture.state,
        &target_id,
        Operation::SetRating {
            track: PortableTrack {
                key: PortableTrackKey::Catalog {
                    provider: "youtube".to_owned(),
                    exact_catalog_id: "during-revoke".to_owned(),
                },
                title: "During revoke".to_owned(),
                artist: "Remote artist".to_owned(),
                album: None,
                duration_secs: Some(200),
                isrc: None,
            },
            rating: Rating::Disliked,
        },
        4,
    )
    .unwrap();
    let transport = RacingTransport {
        inner,
        manifest_gets: Cell::new(0),
        injected: Cell::new(false),
        state: &racing_state,
        membership: &fixture.membership,
        anchor: &fixture.anchor,
        device: &fixture.devices[1],
        checkpoint_anchor: &bootstrapped.checkpoint_anchor,
        total_gets: Cell::new(0),
        list_calls: Cell::new(0),
    };

    let mut budget = super::super::super::manual::ManualSyncBudget::default();
    let revoked = revoke_with_transport_and_budget(
        &fixture.state,
        &target_id,
        &fixture.private,
        &transport,
        &mut budget,
    )
    .unwrap();

    assert!(transport.injected.get());
    assert!(transport.manifest_gets.get() >= 6);
    assert_eq!(revoked.state.version_vector.observed(&target_id), 1);
    assert!(
        revoked
            .state
            .operations
            .iter()
            .any(|operation| operation.operation_id == "device-target:1")
    );
    assert_eq!(
        revoked
            .membership
            .verify(&fixture.anchor)
            .unwrap()
            .revocation_cutoffs
            .get(&target_id),
        Some(&1)
    );
    assert_eq!(
        budget.consumed_requests(),
        transport
            .total_gets
            .get()
            .saturating_add(transport.list_calls.get()),
        "preparation, revocation, and the outer retry must charge one shared request budget"
    );
}

#[test]
fn revoke_rebases_when_target_publishes_a_head_without_winning_the_manifest_cas() {
    let fixture = fixture();
    let directory = TempVault::new();
    let inner = FileVaultTransport::create(directory.0.clone()).unwrap();
    let bootstrapped = synchronize(
        &inner,
        &fixture.state,
        &fixture.membership,
        &fixture.anchor,
        &fixture.devices[0],
        &CheckpointAnchor::default(),
        Some(&fixture.checkpoint),
    );
    let target_id = DeviceId::new(fixture.devices[1].device_id()).unwrap();
    let racing_state = append_operation_as(
        &fixture.state,
        &target_id,
        Operation::SetRating {
            track: PortableTrack {
                key: PortableTrackKey::Catalog {
                    provider: "youtube".to_owned(),
                    exact_catalog_id: "head-only-during-revoke".to_owned(),
                },
                title: "Head only during revoke".to_owned(),
                artist: "Remote artist".to_owned(),
                album: None,
                duration_secs: Some(201),
                isrc: None,
            },
            rating: Rating::Liked,
        },
        5,
    )
    .unwrap();
    let transport = HeadOnlyRacingTransport {
        target_head_key: crate::sync::manual::device_head_key(
            &fixture.state.dataset_id,
            &target_id,
        )
        .unwrap(),
        inner,
        target_head_gets: Cell::new(0),
        injected: Cell::new(false),
        state: &racing_state,
        membership: &fixture.membership,
        anchor: &fixture.anchor,
        device: &fixture.devices[1],
        checkpoint_anchor: &bootstrapped.checkpoint_anchor,
        total_gets: Cell::new(0),
        list_calls: Cell::new(0),
    };

    let mut budget = super::super::super::manual::ManualSyncBudget::default();
    let revoked = revoke_with_transport_and_budget(
        &fixture.state,
        &target_id,
        &fixture.private,
        &transport,
        &mut budget,
    )
    .unwrap();

    assert!(transport.injected.get());
    assert!(transport.target_head_gets.get() >= 6);
    assert_eq!(revoked.state.version_vector.observed(&target_id), 1);
    assert!(
        revoked
            .state
            .operations
            .iter()
            .any(|operation| operation.operation_id == "device-target:1")
    );
    assert_eq!(
        revoked
            .membership
            .verify(&fixture.anchor)
            .unwrap()
            .revocation_cutoffs
            .get(&target_id),
        Some(&1)
    );
    assert_eq!(
        budget.consumed_requests(),
        transport
            .total_gets
            .get()
            .saturating_add(transport.list_calls.get()),
        "the rejected cutoff and outer rebase share one bounded request budget"
    );
}
