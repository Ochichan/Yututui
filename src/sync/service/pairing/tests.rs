use std::sync::atomic::{AtomicU64, Ordering};

use age::secrecy::SecretString;

use crate::personal_state::{
    CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateCommit, PersonalStatePaths, PersonalStateV2, VersionVector, append_operation_as,
    load_ledger,
};
use crate::sync::{
    CheckpointAnchor, DeviceSecretMaterial, FileVaultTransport, MembershipAction, MembershipAnchor,
    MembershipChain, ObjectCondition, PrivateStore, PrivateStoreSnapshot, RecoveryKit,
    SignedCheckpoint, SignedMembershipRoot, VaultCredential, VaultTransport, WebDavProfile,
    WebDavProfileStore,
};

use super::*;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "yututui-pairing-resume-{}-{sequence}",
            std::process::id()
        ));
        crate::util::safe_fs::ensure_private_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct JoinFixture {
    paths: SyncPaths,
    local_state: PersonalStateV2,
    private: PrivateStoreSnapshot,
    checkpoint: SignedCheckpoint,
    encrypted_checkpoint: EncryptedObject,
    encrypted_approval: EncryptedObject,
    request: EncryptedObject,
    request_nonce: String,
    code: PairingCode,
    expires_at_unix: i64,
    invite_id: String,
    _root: TempRoot,
}

struct HostFixture {
    paths: SyncPaths,
    personal_paths: PersonalStatePaths,
    state: PersonalStateV2,
    private: PrivateStoreSnapshot,
    remote: FileVaultTransport,
    _root: TempRoot,
}

fn host_fixture() -> HostFixture {
    let root = TempRoot::new();
    let paths = SyncPaths::for_data_root(root.0.clone());
    let personal_paths = PersonalStatePaths::for_data_root(root.0.clone());
    let recovery = RecoveryKit::generate("dataset-host", None).unwrap();
    let host = DeviceSecretMaterial::generate_for("device-host").unwrap();
    let host_record = device_record(&host, "Host");
    let state = initial_state("dataset-host", &host_record);
    let membership_root = SignedMembershipRoot::create(
        state.dataset_id.clone(),
        recovery.recovery_recipient(),
        &recovery.signing_key().unwrap(),
        host_record.clone(),
    )
    .unwrap();
    let root_hash = membership_root.hash().unwrap();
    let anchor = MembershipAnchor::RootHash(root_hash.clone());
    let membership = MembershipChain::new(membership_root);
    let checkpoint = SignedCheckpoint::create(
        membership.clone(),
        &anchor,
        host_record.device_id,
        host.signing_key(),
        &CheckpointAnchor::default(),
        state.clone(),
    )
    .unwrap();
    let mut private = PrivateStoreSnapshot::pending_ledger_commit(
        host,
        recovery.recovery_recipient(),
        recovery.recovery_verifying_key().unwrap(),
        root_hash,
        &checkpoint,
    )
    .unwrap();
    private.set_credential(test_credential());
    private.mark_active(&checkpoint, &state).unwrap();
    PrivateStore::new(paths.private_store())
        .unwrap()
        .create(&mut private)
        .unwrap();
    let mut profile =
        WebDavProfile::new(&state.dataset_id, private.device(), "https://example.test").unwrap();
    WebDavProfileStore::new(paths.profile())
        .unwrap()
        .create(&mut profile, private.device())
        .unwrap();
    let installed = PersonalStateCommit::prepare_for_runtime(state, 0)
        .unwrap()
        .commit(&personal_paths)
        .unwrap();
    let remote = FileVaultTransport::create(root.0.join("remote")).unwrap();
    let empty_anchor = CheckpointAnchor::default();
    let input = ManualSyncInput {
        local_state: &installed,
        membership: &membership,
        membership_anchor: &anchor,
        device: private.device(),
        checkpoint_anchor: &empty_anchor,
        bootstrap_checkpoint: Some(&checkpoint),
        expected_local_revision: installed.revision,
    };
    let bootstrapped = ManualSyncEngine::new(&remote)
        .synchronize(&input, &|expected| {
            if expected == installed.revision {
                Ok(())
            } else {
                Err(VaultError::RevisionConflict)
            }
        })
        .unwrap();
    assert_eq!(bootstrapped.state, installed);
    HostFixture {
        paths,
        personal_paths,
        state: installed,
        private,
        remote,
        _root: root,
    }
}

fn test_credential() -> VaultCredential {
    VaultCredential::bearer_token(SecretString::from("test-token".to_owned())).unwrap()
}

fn join_fixture() -> JoinFixture {
    let root = TempRoot::new();
    let paths = SyncPaths::for_data_root(root.0.clone());
    let recovery = RecoveryKit::generate("dataset-remote", None).unwrap();
    let host = DeviceSecretMaterial::generate_for("device-host").unwrap();
    let joining = DeviceSecretMaterial::generate_for("device-joining").unwrap();
    let host_record = device_record(&host, "Host");
    let joining_record = device_record(&joining, "Joining");
    let membership_root = SignedMembershipRoot::create(
        "dataset-remote",
        recovery.recovery_recipient(),
        &recovery.signing_key().unwrap(),
        host_record.clone(),
    )
    .unwrap();
    let root_hash = membership_root.hash().unwrap();
    let anchor = MembershipAnchor::RootHash(root_hash.clone());
    let mut membership = MembershipChain::new(membership_root);
    let starting_head = membership.verify(&anchor).unwrap().head_hash;
    let invite = PairingInvite::create("dataset-remote", root_hash, starting_head, 1_000).unwrap();
    let (request, nonce) =
        PairingInvite::create_request(invite.code(), "Joining", &joining, 1_001).unwrap();
    let reviewed = invite.review_request(&request, 1_002).unwrap();
    assert_eq!(reviewed.device, joining_record);
    membership
        .append_device_action(
            &anchor,
            &host_record.device_id,
            host.signing_key(),
            MembershipAction::AddDevice {
                device: joining_record.clone(),
            },
        )
        .unwrap();
    let state = append_operation_as(
        &initial_state("dataset-remote", &host_record),
        &host_record.device_id,
        Operation::AddDevice {
            device: joining_record,
        },
        1_002,
    )
    .unwrap();
    let checkpoint = SignedCheckpoint::create(
        membership.clone(),
        &anchor,
        host_record.device_id,
        host.signing_key(),
        &CheckpointAnchor::default(),
        state,
    )
    .unwrap();
    let encrypted_checkpoint = checkpoint.encrypt(&anchor).unwrap();
    let approval = invite
        .approve(&request, membership, &encrypted_checkpoint, &host, 1_003)
        .unwrap();
    let approved = crate::sync::ApprovedPairing::open(
        invite.code(),
        &approval,
        &nonce,
        &joining,
        &encrypted_checkpoint,
        1_004,
    )
    .unwrap();

    let mut private = PrivateStoreSnapshot::pending_approval("dataset-remote", joining).unwrap();
    private
        .set_pending_pairing(invite.invite_id(), nonce.clone())
        .unwrap();
    private.approve(&approved).unwrap();
    private.set_credential(
        VaultCredential::bearer_token(SecretString::from("test-token".to_owned())).unwrap(),
    );
    let private_store = PrivateStore::new(paths.private_store()).unwrap();
    private_store.create(&mut private).unwrap();
    let mut profile =
        WebDavProfile::new("dataset-remote", private.device(), "https://example.test").unwrap();
    WebDavProfileStore::new(paths.profile())
        .unwrap()
        .create(&mut profile, private.device())
        .unwrap();
    crate::util::safe_fs::write_owner_only_atomic(
        paths.pending_join_request(),
        request.encrypted.as_bytes(),
    )
    .unwrap();

    JoinFixture {
        paths,
        local_state: PersonalStateV2::empty("dataset-local".to_owned()).unwrap(),
        private,
        checkpoint,
        encrypted_checkpoint,
        encrypted_approval: approval.encrypted,
        request: request.encrypted,
        request_nonce: nonce,
        code: PairingCode::parse(invite.code().expose_secret()).unwrap(),
        expires_at_unix: invite.expires_at_unix(),
        invite_id: invite.invite_id().to_owned(),
        _root: root,
    }
}

fn device_record(device: &DeviceSecretMaterial, name: &str) -> DeviceRecord {
    DeviceRecord {
        device_id: DeviceId::new(device.device_id()).unwrap(),
        name: name.to_owned(),
        revoked: false,
        public_identity: Some(device.public_identity()),
    }
}

fn initial_state(dataset_id: &str, device: &DeviceRecord) -> PersonalStateV2 {
    let dot = Dot {
        device_id: device.device_id.clone(),
        sequence: 1,
    };
    let mut state = PersonalStateV2::empty(dataset_id.to_owned()).unwrap();
    state.operations.push(OperationEnvelope {
        operation_id: "initial-device".to_owned(),
        stamp: CausalStamp {
            dot: dot.clone(),
            observed: VersionVector::default(),
            recorded_at_unix: 0,
        },
        origin: OperationOrigin::Local,
        operation: Operation::AddDevice {
            device: device.clone(),
        },
    });
    state.version_vector.observe(&dot);
    crate::personal_state::refresh_device_registry(&mut state).unwrap();
    state.normalize().unwrap();
    state
}

fn restage_pending_approval(fixture: &JoinFixture) -> PrivateStoreSnapshot {
    let private_store = PrivateStore::new(fixture.paths.private_store()).unwrap();
    let profile_store = WebDavProfileStore::new(fixture.paths.profile()).unwrap();
    profile_store.remove().unwrap();
    private_store.remove(fixture.private.revision()).unwrap();
    crate::util::safe_fs::remove_owner_only_file_durable(fixture.paths.pending_join_checkpoint())
        .unwrap();

    let signing_key = fixture.private.device().signing_key_secret_b64();
    let device = DeviceSecretMaterial::from_encoded(
        fixture.private.device_id(),
        fixture.private.device().age_identity_secret(),
        &signing_key,
    )
    .unwrap();
    let mut private =
        PrivateStoreSnapshot::pending_approval(fixture.private.dataset_id(), device).unwrap();
    private
        .set_pending_pairing(&fixture.invite_id, &fixture.request_nonce)
        .unwrap();
    private.set_credential(
        VaultCredential::bearer_token(SecretString::from("test-token".to_owned())).unwrap(),
    );
    JoinPairingStore::new(&fixture.paths)
        .create(
            private.device(),
            &fixture.code,
            private.dataset_id(),
            fixture.expires_at_unix,
            &fixture.request_nonce,
            &fixture.request,
        )
        .unwrap();
    private_store.create(&mut private).unwrap();
    let mut profile = WebDavProfile::new(
        private.dataset_id(),
        private.device(),
        "https://example.test",
    )
    .unwrap();
    profile_store
        .create(&mut profile, private.device())
        .unwrap();
    private
}

fn put_pairing_handoff(remote: &FileVaultTransport, fixture: &JoinFixture) {
    for (name, object) in [
        ("request.age", &fixture.request),
        ("checkpoint.age", &fixture.encrypted_checkpoint),
        ("approval.age", &fixture.encrypted_approval),
    ] {
        let key =
            dataset_pairing_key(fixture.private.dataset_id(), &fixture.invite_id, name).unwrap();
        remote
            .put(&key, object, ObjectCondition::CreateOnly)
            .unwrap();
    }
}

fn start_join_for_host(
    fixture: &HostFixture,
    host: &PairingHostInvite,
    name: &str,
    now_unix: i64,
) -> (SyncPaths, PairingJoinWaiting) {
    let join_root = fixture._root.0.join(name);
    crate::util::safe_fs::ensure_private_dir(&join_root).unwrap();
    let join_paths = SyncPaths::for_data_root(join_root);
    let waiting = start_pairing_join_with_transport(
        &join_paths,
        "https://example.test".to_owned(),
        None,
        test_credential(),
        host.code(),
        "Joining device".to_owned(),
        now_unix,
        &fixture.remote,
    )
    .unwrap();
    (join_paths, waiting)
}

fn prepare_host_approval(
    fixture: &HostFixture,
    host: &mut PairingHostInvite,
    join_name: &str,
    now_unix: i64,
) -> PreparedPairingApproval {
    let _ = start_join_for_host(fixture, host, join_name, now_unix);
    let review = poll_pairing_request_with_transport(
        &fixture.state,
        &fixture.paths,
        host,
        now_unix + 1,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap()
    .unwrap();
    prepare_pairing_approval_with_transport(
        &fixture.state,
        &fixture.paths,
        host,
        review,
        now_unix + 2,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap()
}

fn poll_pending_pairing_join_once_with_transport<T: VaultTransport + ?Sized>(
    current_state: &PersonalStateV2,
    paths: &SyncPaths,
    private_store: &PrivateStore,
    private: &mut PrivateStoreSnapshot,
    journal: &JoinPairingSnapshot,
    transport: &T,
    now_unix: i64,
) -> Result<Option<PairingJoinPreview>, SyncServiceError> {
    match resume_pending_approval_with_transport(
        current_state,
        paths,
        private_store,
        private,
        journal,
        transport,
        now_unix,
    ) {
        Ok(preview) => Ok(Some(preview)),
        Err(SyncServiceError::PendingApproval) if now_unix > journal.expires_at_unix() => {
            Err(SyncServiceError::PairingExpired)
        }
        Err(SyncServiceError::PendingApproval) => Ok(None),
        Err(error) => Err(error),
    }
}

#[test]
fn start_and_one_shot_poll_wait_then_return_approved_preview() {
    let fixture = host_fixture();
    let now = crate::signals::unix_now();
    let mut host = create_pairing_invite_with_transport(
        &fixture.state,
        &fixture.paths,
        now,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    let (join_paths, waiting) = start_join_for_host(&fixture, &host, "join-one-shot", now + 1);
    assert!(!waiting.resumed);
    assert!(waiting.expires_at_unix > now);

    let review = poll_pairing_request_with_transport(
        &fixture.state,
        &fixture.paths,
        &mut host,
        now + 2,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap()
    .unwrap();
    assert_eq!(review.device_id, waiting.device_id);

    let private_store = PrivateStore::new(join_paths.private_store()).unwrap();
    let mut joining_private = private_store.load().unwrap();
    let journal = JoinPairingStore::new(&join_paths)
        .load(joining_private.device())
        .unwrap()
        .unwrap();
    let local = PersonalStateV2::empty("dataset-local".to_owned()).unwrap();
    assert!(
        poll_pending_pairing_join_once_with_transport(
            &local,
            &join_paths,
            &private_store,
            &mut joining_private,
            &journal,
            &fixture.remote,
            now + 3,
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(
        private_store.load().unwrap().enrollment(),
        EnrollmentState::PendingApproval
    );

    let prepared = prepare_pairing_approval_with_transport(
        &fixture.state,
        &fixture.paths,
        &mut host,
        review,
        now + 4,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    let preview = poll_pending_pairing_join_once_with_transport(
        &local,
        &join_paths,
        &private_store,
        &mut joining_private,
        &journal,
        &fixture.remote,
        now + 5,
    )
    .unwrap()
    .unwrap();
    assert_eq!(preview.device_id, waiting.device_id);
    assert_eq!(
        preview.target_state().device_registry,
        prepared.candidate().state.device_registry
    );
    assert_eq!(
        private_store.load().unwrap().enrollment(),
        EnrollmentState::PendingLedgerCommit
    );
}

#[test]
fn prepared_host_approval_does_not_install_ledger_until_apply() {
    fn assert_clone_send_sync<T: Clone + Send + Sync>(_: &T) {}

    let fixture = host_fixture();
    let now = crate::signals::unix_now();
    let mut host = create_pairing_invite_with_transport(
        &fixture.state,
        &fixture.paths,
        now,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    let before = load_ledger(&fixture.personal_paths).unwrap().unwrap();
    let prepared = prepare_host_approval(&fixture, &mut host, "join-prepare", now + 1);
    assert!(host_pairing_needs_review(&fixture.state, &fixture.paths).unwrap());
    assert_clone_send_sync(&prepared);
    assert_eq!(
        load_ledger(&fixture.personal_paths).unwrap().unwrap(),
        before
    );
    assert!(
        !before
            .device_registry
            .contains_key(prepared.target_device_id())
    );
    assert!(
        prepared
            .candidate()
            .state
            .device_registry
            .contains_key(prepared.target_device_id())
    );
    assert_eq!(
        prepared.clone().into_candidate().state,
        prepared.candidate().state
    );

    let installed = apply_prepared_pairing_approval(
        &before,
        0,
        &fixture.personal_paths,
        &fixture.paths,
        prepared.clone(),
    )
    .unwrap();
    assert!(
        installed
            .device_registry
            .contains_key(prepared.target_device_id())
    );
    assert_eq!(
        load_ledger(&fixture.personal_paths).unwrap().unwrap(),
        installed
    );
    assert!(!fixture.paths.pairing_host_state().exists());
    assert!(!host_pairing_needs_review(&installed, &fixture.paths).unwrap());
    finalize_prepared_pairing_approval(&installed, &fixture.paths, &prepared).unwrap();
}

#[test]
fn host_rejects_a_valid_but_substituted_checkpoint_readback() {
    let fixture = host_fixture();
    let now = crate::signals::unix_now();
    let mut host = create_pairing_invite_with_transport(
        &fixture.state,
        &fixture.paths,
        now,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    let _ = start_join_for_host(&fixture, &host, "join-substituted-checkpoint", now + 1);
    let review = poll_pairing_request_with_transport(
        &fixture.state,
        &fixture.paths,
        &mut host,
        now + 2,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap()
    .unwrap();
    let target_device = review.payload.device.clone();
    let anchor = membership_anchor(&fixture.private).unwrap();
    let base =
        prepare_manual_sync_with_transport(&fixture.state, &fixture.private, &fixture.remote)
            .unwrap();
    let prepared = commit_pairing_membership(
        &fixture.state,
        &fixture.private,
        &anchor,
        &fixture.remote,
        base,
        &target_device,
        host.expires_at_unix(),
    )
    .unwrap();

    let host_id = DeviceId::new(fixture.private.device_id()).unwrap();
    let alternate_state = append_operation_as(
        &prepared.state,
        &host_id,
        Operation::SetAvoidArtist {
            artist_key: "substituted-checkpoint".to_owned(),
            avoid: true,
        },
        now + 3,
    )
    .unwrap();
    let previous_anchor = super::super::manual::checkpoint_anchor(&fixture.private).unwrap();
    let alternate = SignedCheckpoint::create(
        prepared.membership.clone(),
        &anchor,
        host_id,
        fixture.private.device().signing_key(),
        &previous_anchor,
        alternate_state,
    )
    .unwrap();
    let expected_hash = prepared
        .checkpoint_anchor
        .checkpoint_hash
        .as_deref()
        .unwrap();
    assert_ne!(alternate.hash().unwrap(), expected_hash);
    let alternate = alternate.encrypt(&anchor).unwrap();
    let key = checkpoint_key(
        &fixture.state.dataset_id,
        prepared.membership.verify(&anchor).unwrap().epoch,
        expected_hash,
    )
    .unwrap();
    let (_, metadata) = fixture
        .remote
        .get(&key, MAX_VAULT_PAYLOAD_BYTES)
        .unwrap()
        .unwrap();
    fixture
        .remote
        .put(
            &key,
            &alternate,
            crate::sync::ObjectCondition::Match(metadata.etag),
        )
        .unwrap();

    assert_eq!(
        load_prepared_checkpoint(
            &fixture.state,
            &anchor,
            &fixture.remote,
            fixture.private.device(),
            &prepared,
        ),
        Err(SyncServiceError::InvalidRemoteData)
    );
}

#[test]
fn pairing_approval_persistence_rebases_and_finalizes_idempotently() {
    let fixture = host_fixture();
    let now = crate::signals::unix_now();
    let mut host = create_pairing_invite_with_transport(
        &fixture.state,
        &fixture.paths,
        now,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    let prepared = prepare_host_approval(&fixture, &mut host, "join-rebase", now + 1);
    let host_device = DeviceId::new(fixture.private.device_id()).unwrap();
    let local = append_operation_as(
        &fixture.state,
        &host_device,
        Operation::SetAvoidArtist {
            artist_key: "approval-in-flight-artist".to_owned(),
            avoid: true,
        },
        now + 3,
    )
    .unwrap();
    let local = PersonalStateCommit::prepare_for_runtime(local, 0)
        .unwrap()
        .commit(&fixture.personal_paths)
        .unwrap();
    let retargeted = prepared.retarget(&fixture.state, &local).unwrap();
    assert_eq!(
        retargeted.candidate().expected_local_revision,
        local.revision
    );
    assert!(
        retargeted
            .candidate()
            .state
            .operations
            .iter()
            .any(|operation| matches!(
                operation.operation,
                Operation::SetAvoidArtist {
                    ref artist_key,
                    avoid: true
                } if artist_key == "approval-in-flight-artist"
            ))
    );

    let writer = crate::sync::service::PersonalSyncPersistence::pairing_approval_activation(
        fixture.state.clone(),
        local.clone(),
        0,
        prepared.clone(),
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture._root.0.clone()),
    )
    .unwrap();
    let target = writer.state().clone();
    writer.write().unwrap();
    assert!(writer.committed());
    assert_eq!(
        load_ledger(&fixture.personal_paths).unwrap(),
        Some(target.clone())
    );
    assert!(!fixture.paths.pairing_host_state().exists());

    let retry = crate::sync::service::PersonalSyncPersistence::pairing_approval_activation(
        fixture.state.clone(),
        local,
        0,
        prepared,
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture._root.0.clone()),
    )
    .unwrap();
    assert_eq!(retry.state(), &target);
    retry.write().unwrap();
    assert!(retry.committed());
    assert!(!fixture.paths.pairing_host_state().exists());
}

#[test]
fn pairing_join_activation_retargets_and_retries_the_exact_target() {
    let fixture = join_fixture();
    let personal_paths = PersonalStatePaths::for_data_root(fixture._root.0.clone());
    let local_device = DeviceSecretMaterial::generate_for("pre-join-local-device").unwrap();
    let local_record = device_record(&local_device, "Local before join");
    let observed =
        PersonalStateCommit::prepare_for_runtime(initial_state("dataset-local", &local_record), 0)
            .unwrap()
            .state()
            .clone();
    let joining_device = DeviceId::new(fixture.private.device_id()).unwrap();
    let initial_plan = crate::personal_state::plan_join_import(
        &fixture.checkpoint.payload.state,
        &observed,
        &joining_device,
    )
    .unwrap();
    let activation = PairingJoinPreview {
        summary: initial_plan.summary,
        device_id: joining_device.as_str().to_owned(),
        candidate: initial_plan.candidate,
        checkpoint: fixture.checkpoint.clone(),
        expected_local_revision: observed.revision,
        expected_private_revision: fixture.private.revision(),
    }
    .into_activation();

    let latest = append_operation_as(
        &observed,
        &local_record.device_id,
        Operation::SetAvoidArtist {
            artist_key: "join-in-flight-artist".to_owned(),
            avoid: true,
        },
        2_000,
    )
    .unwrap();
    let latest = PersonalStateCommit::prepare_for_runtime(latest, 0)
        .unwrap()
        .commit(&personal_paths)
        .unwrap();
    let retargeted = activation.retarget(&latest).unwrap();
    assert_eq!(retargeted.expected_local_revision(), latest.revision);
    assert_eq!(
        activation.target_state_for(&latest).unwrap(),
        retargeted.target_state().clone()
    );

    let writer = crate::sync::service::PersonalSyncPersistence::pairing_join_activation(
        latest.clone(),
        0,
        activation.clone(),
        personal_paths.clone(),
        SyncPaths::for_data_root(fixture._root.0.clone()),
    )
    .unwrap();
    let target = writer.state().clone();
    assert!(
        target
            .operations
            .iter()
            .any(|operation| matches!(operation.operation, Operation::LegacyBaseline { .. }))
    );
    writer.write().unwrap();
    assert!(writer.committed());
    assert_eq!(load_ledger(&personal_paths).unwrap(), Some(target.clone()));
    assert_eq!(
        PrivateStore::new(fixture.paths.private_store())
            .unwrap()
            .load()
            .unwrap()
            .enrollment(),
        EnrollmentState::Active
    );

    let retry = crate::sync::service::PersonalSyncPersistence::pairing_join_activation(
        latest,
        0,
        activation,
        personal_paths.clone(),
        SyncPaths::for_data_root(fixture._root.0.clone()),
    )
    .unwrap();
    assert_eq!(retry.state(), &target);
    retry.write().unwrap();
    assert!(retry.committed());
}

#[test]
fn cancelling_host_invite_invalidates_bound_request_and_rotates_code() {
    let fixture = host_fixture();
    let now = crate::signals::unix_now();
    let mut host = create_pairing_invite_with_transport(
        &fixture.state,
        &fixture.paths,
        now,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    let old_code = host.code().to_owned();
    let old_invite_id = host.invite.invite_id().to_owned();
    let _ = start_join_for_host(&fixture, &host, "join-cancel", now + 1);
    assert!(
        poll_pairing_request_with_transport(
            &fixture.state,
            &fixture.paths,
            &mut host,
            now + 2,
            &fixture.private,
            &fixture.remote,
        )
        .unwrap()
        .is_some()
    );

    cancel_pairing_invite_with_transport(
        &fixture.state,
        &fixture.paths,
        &host,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    for path in [
        fixture.paths.pairing_host_state(),
        fixture.paths.pairing_host_locator(),
        fixture.paths.pairing_host_request(),
    ] {
        assert!(!path.exists());
    }
    assert!(matches!(
        poll_pairing_request_with_transport(
            &fixture.state,
            &fixture.paths,
            &mut host,
            now + 3,
            &fixture.private,
            &fixture.remote,
        ),
        Err(SyncServiceError::LocalStateChanged)
    ));
    let old_request_key =
        dataset_pairing_key(&fixture.state.dataset_id, &old_invite_id, "request.age").unwrap();
    assert!(
        fixture
            .remote
            .get(&old_request_key, MAX_VAULT_PAYLOAD_BYTES)
            .unwrap()
            .is_some()
    );

    let replacement = create_pairing_invite_with_transport(
        &fixture.state,
        &fixture.paths,
        now + 4,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    assert_ne!(replacement.code(), old_code);
    assert_ne!(replacement.invite.invite_id(), old_invite_id);
}

#[test]
fn cancelling_host_invite_is_rejected_after_handoff_exists() {
    let fixture = host_fixture();
    let now = crate::signals::unix_now();
    let mut host = create_pairing_invite_with_transport(
        &fixture.state,
        &fixture.paths,
        now,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    let prepared = prepare_host_approval(&fixture, &mut host, "join-handoff", now + 1);
    assert!(
        prepared
            .candidate()
            .state
            .device_registry
            .contains_key(prepared.target_device_id())
    );
    assert_eq!(
        cancel_pairing_invite_with_transport(
            &fixture.state,
            &fixture.paths,
            &host,
            &fixture.private,
            &fixture.remote,
        ),
        Err(SyncServiceError::AlreadyConfigured)
    );
    assert!(fixture.paths.pairing_host_state().is_file());
    assert!(fixture.paths.pairing_host_approval().is_file());
}

#[test]
fn cancelling_host_invite_is_rejected_after_remote_commit_before_handoff() {
    let fixture = host_fixture();
    let now = crate::signals::unix_now();
    let mut host = create_pairing_invite_with_transport(
        &fixture.state,
        &fixture.paths,
        now,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap();
    let _ = start_join_for_host(&fixture, &host, "join-commit-before-handoff", now + 1);
    let review = poll_pairing_request_with_transport(
        &fixture.state,
        &fixture.paths,
        &mut host,
        now + 2,
        &fixture.private,
        &fixture.remote,
    )
    .unwrap()
    .unwrap();
    HostPairingStore::new(&fixture.paths)
        .bind_request(
            fixture.private.device(),
            &mut host.durable,
            &review.sealed.encrypted,
            &review.payload.device.device_id,
        )
        .unwrap();
    let anchor = membership_anchor(&fixture.private).unwrap();
    let base =
        prepare_manual_sync_with_transport(&fixture.state, &fixture.private, &fixture.remote)
            .unwrap();
    commit_pairing_membership(
        &fixture.state,
        &fixture.private,
        &anchor,
        &fixture.remote,
        base,
        &review.payload.device,
        host.expires_at_unix(),
    )
    .unwrap();
    assert!(host.durable.has_bound_request());
    assert!(!host.durable.has_handoff());

    assert_eq!(
        cancel_pairing_invite_with_transport(
            &fixture.state,
            &fixture.paths,
            &host,
            &fixture.private,
            &fixture.remote,
        ),
        Err(SyncServiceError::AlreadyConfigured)
    );
    assert!(fixture.paths.pairing_host_state().is_file());
}

#[test]
fn approved_join_resumes_without_code_and_not_now_keeps_every_artifact() {
    let fixture = join_fixture();
    persist_join_checkpoint(&fixture.paths, &fixture.encrypted_checkpoint).unwrap();
    let before_private = std::fs::read(fixture.paths.private_store()).unwrap();
    let before_profile = std::fs::read(fixture.paths.profile()).unwrap();
    let before_request = std::fs::read(fixture.paths.pending_join_request()).unwrap();
    let before_checkpoint = std::fs::read(fixture.paths.pending_join_checkpoint()).unwrap();

    let preview = resume_pairing_join(&fixture.local_state, &fixture.paths).unwrap();
    assert_eq!(preview.candidate.dataset_id, "dataset-remote");
    assert_eq!(
        preview.checkpoint.hash().unwrap(),
        fixture.checkpoint.hash().unwrap()
    );
    defer_pairing_join(&fixture.paths, &preview).unwrap();

    assert_eq!(
        std::fs::read(fixture.paths.private_store()).unwrap(),
        before_private
    );
    assert_eq!(
        std::fs::read(fixture.paths.profile()).unwrap(),
        before_profile
    );
    assert_eq!(
        std::fs::read(fixture.paths.pending_join_request()).unwrap(),
        before_request
    );
    assert_eq!(
        std::fs::read(fixture.paths.pending_join_checkpoint()).unwrap(),
        before_checkpoint
    );
}

#[test]
fn missing_local_checkpoint_can_be_restored_from_immutable_remote_object() {
    let fixture = join_fixture();
    let remote = FileVaultTransport::create(fixture._root.0.join("remote")).unwrap();
    let key = dataset_pairing_key(
        fixture.private.dataset_id(),
        &fixture.invite_id,
        "checkpoint.age",
    )
    .unwrap();
    remote
        .put(
            &key,
            &fixture.encrypted_checkpoint,
            ObjectCondition::CreateOnly,
        )
        .unwrap();

    let fetched =
        fetch_join_checkpoint(&remote, fixture.private.dataset_id(), &fixture.invite_id).unwrap();
    let preview = plan_resumed_join(
        &fixture.local_state,
        &fixture.paths,
        &fixture.private,
        fetched,
        true,
    )
    .unwrap();
    assert_eq!(
        preview.checkpoint.hash().unwrap(),
        fixture.checkpoint.hash().unwrap()
    );
    assert!(fixture.paths.pending_join_checkpoint().is_file());
}

#[test]
fn expired_pending_approval_recovers_from_exact_file_vault_handoff_without_code_input() {
    let fixture = join_fixture();
    let mut private = restage_pending_approval(&fixture);
    let remote = FileVaultTransport::create(fixture._root.0.join("expired-remote")).unwrap();
    put_pairing_handoff(&remote, &fixture);
    let journal = JoinPairingStore::new(&fixture.paths)
        .load(private.device())
        .unwrap()
        .unwrap();
    let private_store = PrivateStore::new(fixture.paths.private_store()).unwrap();

    let preview = resume_pending_approval_with_transport(
        &fixture.local_state,
        &fixture.paths,
        &private_store,
        &mut private,
        &journal,
        &remote,
        fixture.expires_at_unix + 60,
    )
    .unwrap();

    assert_eq!(preview.candidate.dataset_id, fixture.private.dataset_id());
    assert_eq!(
        preview.checkpoint.hash().unwrap(),
        fixture.checkpoint.hash().unwrap()
    );
    assert_eq!(
        private_store.load().unwrap().enrollment(),
        EnrollmentState::PendingLedgerCommit
    );
    assert!(fixture.paths.pending_join_state().is_file());
    assert!(fixture.paths.pending_join_checkpoint().is_file());
}

#[test]
fn expired_unapproved_join_is_retained_until_explicit_cancel() {
    let fixture = join_fixture();
    let mut private = restage_pending_approval(&fixture);
    let remote = FileVaultTransport::create(fixture._root.0.join("unapproved-remote")).unwrap();
    let request_key = dataset_pairing_key(
        fixture.private.dataset_id(),
        &fixture.invite_id,
        "request.age",
    )
    .unwrap();
    remote
        .put(&request_key, &fixture.request, ObjectCondition::CreateOnly)
        .unwrap();
    let journal = JoinPairingStore::new(&fixture.paths)
        .load(private.device())
        .unwrap()
        .unwrap();
    let private_store = PrivateStore::new(fixture.paths.private_store()).unwrap();

    assert!(matches!(
        resume_pending_approval_with_transport(
            &fixture.local_state,
            &fixture.paths,
            &private_store,
            &mut private,
            &journal,
            &remote,
            fixture.expires_at_unix + 60,
        ),
        Err(SyncServiceError::PendingApproval)
    ));
    assert!(fixture.paths.private_store().is_file());
    assert!(fixture.paths.profile().is_file());
    assert!(fixture.paths.pending_join_request().is_file());
    assert!(fixture.paths.pending_join_state().is_file());

    cancel_pairing_join(&fixture.paths).unwrap();
    for path in [
        fixture.paths.private_store(),
        fixture.paths.profile(),
        fixture.paths.pending_join_request(),
        fixture.paths.pending_join_checkpoint(),
        fixture.paths.pending_join_state(),
    ] {
        assert!(!path.exists(), "pending artifact survived explicit cancel");
    }
    for name in ["checkpoint.age", "approval.age"] {
        let key =
            dataset_pairing_key(fixture.private.dataset_id(), &fixture.invite_id, name).unwrap();
        assert!(remote.get(&key, MAX_VAULT_PAYLOAD_BYTES).unwrap().is_none());
    }
}

#[test]
fn private_only_crash_recreates_profile_and_profile_only_crash_cancels_safely() {
    let fixture = join_fixture();
    let private = restage_pending_approval(&fixture);
    let remote = FileVaultTransport::create(fixture._root.0.join("crash-boundary-remote")).unwrap();
    let request_key = dataset_pairing_key(
        fixture.private.dataset_id(),
        &fixture.invite_id,
        "request.age",
    )
    .unwrap();
    remote
        .put(&request_key, &fixture.request, ObjectCondition::CreateOnly)
        .unwrap();

    let profile_store = WebDavProfileStore::new(fixture.paths.profile()).unwrap();
    profile_store.remove().unwrap();
    let expected = WebDavProfile::new(
        private.dataset_id(),
        private.device(),
        "https://example.test",
    )
    .unwrap();
    let repaired = load_or_create_join_profile(&profile_store, false, &private, expected).unwrap();
    assert_eq!(repaired.device_id(), private.device_id());

    // Simulate interruption after a cleanup lost the private file but before its exact signed
    // profile/journal/request were removed. Explicit cancel authenticates those orphan artifacts
    // through the code-bound request and never writes a membership object.
    PrivateStore::new(fixture.paths.private_store())
        .unwrap()
        .remove(private.revision())
        .unwrap();
    cancel_pairing_join(&fixture.paths).unwrap();
    assert!(!fixture.paths.profile().exists());
    assert!(!fixture.paths.pending_join_state().exists());
    assert!(!fixture.paths.pending_join_request().exists());
    for name in ["checkpoint.age", "approval.age"] {
        let key =
            dataset_pairing_key(fixture.private.dataset_id(), &fixture.invite_id, name).unwrap();
        assert!(remote.get(&key, MAX_VAULT_PAYLOAD_BYTES).unwrap().is_none());
    }
}

#[test]
fn explicit_cancel_resumes_at_every_local_removal_boundary() {
    for completed_removals in 0..=4 {
        let fixture = join_fixture();
        let private = restage_pending_approval(&fixture);
        persist_join_checkpoint(&fixture.paths, &fixture.encrypted_checkpoint).unwrap();
        let remote =
            FileVaultTransport::create(fixture._root.0.join("cancel-boundary-remote")).unwrap();
        let request_key = dataset_pairing_key(
            fixture.private.dataset_id(),
            &fixture.invite_id,
            "request.age",
        )
        .unwrap();
        remote
            .put(&request_key, &fixture.request, ObjectCondition::CreateOnly)
            .unwrap();

        if completed_removals >= 1 {
            WebDavProfileStore::new(fixture.paths.profile())
                .unwrap()
                .remove()
                .unwrap();
        }
        if completed_removals >= 2 {
            crate::util::safe_fs::remove_owner_only_file_durable(
                fixture.paths.pending_join_checkpoint(),
            )
            .unwrap();
        }
        if completed_removals >= 3 {
            PrivateStore::new(fixture.paths.private_store())
                .unwrap()
                .remove(private.revision())
                .unwrap();
        }
        if completed_removals >= 4 {
            JoinPairingStore::new(&fixture.paths)
                .remove_state()
                .unwrap();
        }

        cancel_pairing_join(&fixture.paths).unwrap();
        for path in [
            fixture.paths.private_store(),
            fixture.paths.profile(),
            fixture.paths.pending_join_request(),
            fixture.paths.pending_join_checkpoint(),
            fixture.paths.pending_join_state(),
        ] {
            assert!(
                !path.exists(),
                "artifact survived boundary {completed_removals}"
            );
        }
        for name in ["checkpoint.age", "approval.age"] {
            let key = dataset_pairing_key(fixture.private.dataset_id(), &fixture.invite_id, name)
                .unwrap();
            assert!(remote.get(&key, MAX_VAULT_PAYLOAD_BYTES).unwrap().is_none());
        }
    }
}
