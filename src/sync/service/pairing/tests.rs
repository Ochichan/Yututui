use std::sync::atomic::{AtomicU64, Ordering};

use age::secrecy::SecretString;

use crate::personal_state::{
    CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateV2, VersionVector, append_operation_as,
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
