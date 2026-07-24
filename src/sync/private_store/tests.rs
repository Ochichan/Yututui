use std::sync::atomic::{AtomicU64, Ordering};

use age::secrecy::{ExposeSecret, SecretString};

use crate::personal_state::{
    CausalStamp, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin, VersionVector,
    legacy_state, plan_join_import,
};
use crate::sync::{
    CheckpointAnchor, MembershipAnchor, MembershipChain, RecoveryKit, SignedCheckpoint,
    SignedMembershipRoot,
};

use super::*;

static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ytt-private-store-{}-{sequence}",
            std::process::id()
        ));
        crate::util::safe_fs::ensure_private_dir(&path).expect("create private test directory");
        Self(path)
    }

    fn store(&self) -> PrivateStore {
        PrivateStore::new(self.0.join("sync-private-v1.json")).expect("private store path")
    }

    fn path(&self) -> PathBuf {
        self.0.join("sync-private-v1.json")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn material(device_id: &str) -> DeviceSecretMaterial {
    DeviceSecretMaterial::generate_for(device_id).expect("device material")
}

struct PendingLedgerFixture {
    snapshot: PrivateStoreSnapshot,
    checkpoint: SignedCheckpoint,
    state: PersonalStateV2,
}

fn pending_ledger_fixture() -> PendingLedgerFixture {
    let kit = RecoveryKit::generate("dataset-test", None).expect("recovery kit");
    let device = material("device-a");
    let device_record = DeviceRecord {
        device_id: DeviceId::new(device.device_id()).expect("device id"),
        name: "Device A".to_owned(),
        revoked: false,
        public_identity: Some(device.public_identity()),
    };
    let recovery_signing_key = kit.signing_key().expect("recovery signing key");
    let root = SignedMembershipRoot::create(
        "dataset-test",
        kit.recovery_recipient(),
        &recovery_signing_key,
        device_record.clone(),
    )
    .expect("membership root");
    let root_hash = root.hash().expect("root hash");
    let membership = MembershipChain::new(root);
    let state = initial_state(&device_record);
    let checkpoint = SignedCheckpoint::create(
        membership,
        &MembershipAnchor::RootHash(root_hash.clone()),
        device_record.device_id,
        device.signing_key(),
        &CheckpointAnchor::default(),
        state.clone(),
    )
    .expect("signed checkpoint");
    let snapshot = PrivateStoreSnapshot::pending_ledger_commit(
        device,
        kit.recovery_recipient(),
        kit.recovery_verifying_key()
            .expect("recovery verifying key"),
        root_hash,
        &checkpoint,
    )
    .expect("pending snapshot");
    PendingLedgerFixture {
        snapshot,
        checkpoint,
        state,
    }
}

fn pending_ledger_snapshot() -> PrivateStoreSnapshot {
    pending_ledger_fixture().snapshot
}

fn initial_state(device: &DeviceRecord) -> PersonalStateV2 {
    let dot = Dot {
        device_id: device.device_id.clone(),
        sequence: 1,
    };
    let mut state = PersonalStateV2::empty("dataset-test".to_owned()).expect("empty state");
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
    crate::personal_state::refresh_device_registry(&mut state).expect("device registry");
    state.normalize().expect("normalized state");
    state
}

#[test]
fn owner_only_round_trip_preserves_keys_anchors_and_credential() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let mut snapshot = pending_ledger_snapshot();
    let expected_public = snapshot.device().public_record();
    snapshot.set_credential(
        VaultCredential::password(
            "webdav-user",
            SecretString::from("correct horse battery staple"),
        )
        .expect("credential"),
    );

    store.create(&mut snapshot).expect("create");
    assert_eq!(snapshot.revision(), 1);
    store
        .create(&mut snapshot)
        .expect("idempotent create readback");
    assert_eq!(snapshot.revision(), 1);
    let mut loaded = store.load().expect("load");
    assert_eq!(loaded.revision(), 1);
    assert_eq!(loaded.dataset_id(), "dataset-test");
    assert_eq!(loaded.device().public_record(), expected_public);
    assert_eq!(loaded.enrollment(), EnrollmentState::PendingLedgerCommit);
    assert_eq!(
        loaded.credential().expect("credential").kind(),
        VaultCredentialKind::Password
    );
    assert_eq!(
        loaded
            .credential()
            .expect("credential")
            .username()
            .expect("username")
            .expose_secret(),
        "webdav-user"
    );
    assert_eq!(
        loaded
            .credential()
            .expect("credential")
            .secret()
            .expose_secret(),
        "correct horse battery staple"
    );
    store.save(&mut loaded).expect("idempotent unchanged save");
    assert_eq!(loaded.revision(), 1);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(directory.path())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn pending_setup_recovery_confirmation_is_signed_and_cleared_on_activation() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let fixture = pending_ledger_fixture();
    let mut snapshot = fixture.snapshot;
    let checksum = "e".repeat(SHA256_HEX_BYTES);
    snapshot
        .confirm_setup_recovery(checksum.clone())
        .expect("confirm recovery export");
    store.create(&mut snapshot).expect("create");

    let mut loaded = store.load().expect("load setup marker");
    assert_eq!(loaded.setup_recovery_checksum(), Some(checksum.as_str()));
    loaded
        .mark_active(&fixture.checkpoint, &fixture.state)
        .expect("activate");
    assert_eq!(loaded.setup_recovery_checksum(), None);
    store.save(&mut loaded).expect("save activation");
    assert_eq!(
        store
            .load()
            .expect("reload active setup")
            .setup_recovery_checksum(),
        None
    );
}

#[test]
fn pending_setup_recovery_confirmation_tampering_is_rejected() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let mut snapshot = pending_ledger_snapshot();
    snapshot
        .confirm_setup_recovery("e".repeat(SHA256_HEX_BYTES))
        .expect("confirm recovery export");
    store.create(&mut snapshot).expect("create");
    let original = Zeroizing::new(
        read_owner_only_limited(&directory.path(), MAX_PRIVATE_STORE_BYTES)
            .expect("read setup marker"),
    );

    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        format!(
            "\"setup_recovery_checksum\":\"{}\"",
            "e".repeat(SHA256_HEX_BYTES)
        )
        .as_bytes(),
        format!(
            "\"setup_recovery_checksum\":\"{}\"",
            "d".repeat(SHA256_HEX_BYTES)
        )
        .as_bytes(),
    );
}

#[test]
fn pending_pairing_round_trips_without_persisting_the_code() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let mut snapshot =
        PrivateStoreSnapshot::pending_approval("dataset-test", material("device-join"))
            .expect("pending approval");
    snapshot
        .set_pending_pairing("invite_context-01", "a".repeat(32))
        .expect("set pending pairing");
    store.create(&mut snapshot).expect("create");

    let mut loaded = store.load().expect("load pending pairing");
    let pending = loaded
        .pending_pairing()
        .expect("pending accessor")
        .expect("pending context");
    assert_eq!(pending.invite_id(), "invite_context-01");
    assert_eq!(pending.request_nonce(), "a".repeat(32));
    let bytes = Zeroizing::new(
        read_owner_only_limited(&directory.path(), MAX_PRIVATE_STORE_BYTES)
            .expect("read pending store"),
    );
    assert!(
        !bytes
            .windows(b"pairing_code".len())
            .any(|window| window == b"pairing_code")
    );

    store.save(&mut loaded).expect("idempotent pending save");
    assert!(
        store
            .load()
            .expect("reload pending context")
            .pending_pairing()
            .expect("pending accessor")
            .is_some()
    );
}

#[test]
fn pending_pairing_rejects_invalid_or_out_of_state_context() {
    assert!(
        PrivateStoreSnapshot::pending_approval("Dataset-Test", material("device-invalid")).is_err()
    );
    let mut pending =
        PrivateStoreSnapshot::pending_approval("dataset-test", material("device-pending"))
            .expect("pending approval");
    assert!(
        pending
            .set_pending_pairing("../invite", "a".repeat(32))
            .is_err()
    );
    assert!(
        pending
            .set_pending_pairing("invite-01", "A".repeat(32))
            .is_err()
    );
    pending
        .set_pending_pairing("invite-01", "a".repeat(32))
        .expect("valid context");
    pending
        .set_pending_pairing("invite-01", "a".repeat(32))
        .expect("idempotent context");
    assert!(
        pending
            .set_pending_pairing("invite-02", "b".repeat(32))
            .is_err()
    );

    let mut pending_ledger = pending_ledger_snapshot();
    assert!(
        pending_ledger
            .set_pending_pairing("invite-01", "a".repeat(32))
            .is_err()
    );
    assert!(pending_ledger.pending_pairing().unwrap().is_none());
}

#[test]
fn device_and_recovery_keys_must_be_distinct() {
    let fixture = pending_ledger_fixture();
    let PendingLedgerFixture {
        snapshot,
        checkpoint,
        ..
    } = fixture;
    let device_identity = snapshot.device.public_identity();
    let recovery_verifying_key = snapshot
        .recovery_verifying_key()
        .expect("recovery verifying key")
        .to_owned();
    let membership_root_hash = snapshot
        .membership_root_hash()
        .expect("membership root")
        .to_owned();
    assert!(
        PrivateStoreSnapshot::pending_ledger_commit(
            snapshot.device,
            device_identity.age_recipient,
            recovery_verifying_key,
            membership_root_hash,
            &checkpoint,
        )
        .is_err()
    );

    let fixture = pending_ledger_fixture();
    let PendingLedgerFixture {
        snapshot,
        checkpoint,
        ..
    } = fixture;
    let device_identity = snapshot.device.public_identity();
    let recovery_recipient = snapshot
        .recovery_recipient()
        .expect("recovery recipient")
        .to_owned();
    let membership_root_hash = snapshot
        .membership_root_hash()
        .expect("membership root")
        .to_owned();
    assert!(
        PrivateStoreSnapshot::pending_ledger_commit(
            snapshot.device,
            recovery_recipient,
            device_identity.ed25519_verifying_key,
            membership_root_hash,
            &checkpoint,
        )
        .is_err()
    );
}

#[test]
fn load_rejects_self_signed_device_recovery_aliases() {
    for alias_age_recipient in [true, false] {
        let directory = TestDirectory::new();
        let store = directory.store();
        let mut snapshot = pending_ledger_snapshot();
        snapshot.revision = 1;
        let identity = snapshot.device.public_identity();
        let anchors = snapshot.trust_anchors.as_mut().expect("trust anchors");
        if alias_age_recipient {
            anchors.recovery_recipient = identity.age_recipient;
        } else {
            anchors.recovery_verifying_key = identity.ed25519_verifying_key;
        }
        let disk = DiskPrivateStore::from_snapshot(&snapshot).expect("signed aliased fixture");
        let bytes = Zeroizing::new(serde_json::to_vec(&disk).expect("serialize aliased fixture"));
        write_owner_only_atomic(&directory.path(), &bytes).expect("write aliased fixture");
        assert_eq!(store.load().err(), Some(VaultError::InvalidPrivateStore));
    }
}

#[test]
fn signed_pending_ledger_anchor_rejects_tampering() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let mut snapshot = pending_ledger_snapshot();
    store.create(&mut snapshot).expect("create pending ledger");
    let original = Zeroizing::new(
        read_owner_only_limited(&directory.path(), MAX_PRIVATE_STORE_BYTES)
            .expect("read signed pending ledger"),
    );
    let sequence = b"\"sequence\":1";
    let changed_sequence = b"\"sequence\":2";
    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        sequence,
        changed_sequence,
    );
    let head = snapshot
        .pending_ledger_anchor
        .as_ref()
        .expect("pending anchor")
        .membership_head_hash
        .clone();
    let head_field = format!("\"membership_head_hash\":\"{head}\"");
    let changed_head = format!("\"membership_head_hash\":\"{}\"", "f".repeat(64));
    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        head_field.as_bytes(),
        changed_head.as_bytes(),
    );
}

#[test]
fn pending_pairing_context_is_covered_by_the_device_binding() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let mut snapshot =
        PrivateStoreSnapshot::pending_approval("dataset-test", material("device-join"))
            .expect("pending approval");
    snapshot
        .set_pending_pairing("invite-01", "a".repeat(32))
        .expect("pending context");
    store.create(&mut snapshot).expect("create");
    let original = Zeroizing::new(
        read_owner_only_limited(&directory.path(), MAX_PRIVATE_STORE_BYTES)
            .expect("read signed store"),
    );

    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        b"\"invite_id\":\"invite-01\"",
        b"\"invite_id\":\"invite-02\"",
    );
    let nonce = format!("\"request_nonce\":\"{}\"", "a".repeat(32));
    let changed_nonce = format!("\"request_nonce\":\"{}\"", "b".repeat(32));
    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        nonce.as_bytes(),
        changed_nonce.as_bytes(),
    );
}

#[test]
fn enrollment_and_checkpoint_transitions_are_explicit_and_monotonic() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let fixture = pending_ledger_fixture();
    let mut snapshot = fixture.snapshot;
    store.create(&mut snapshot).expect("create");

    let wrong_state = PersonalStateV2::empty("dataset-test".to_owned()).expect("wrong state");
    assert!(
        snapshot
            .mark_active(&fixture.checkpoint, &wrong_state)
            .is_err()
    );
    let mut wrong_checkpoint = fixture.checkpoint.clone();
    wrong_checkpoint.payload.checkpoint_sequence += 1;
    assert!(
        snapshot
            .mark_active(&wrong_checkpoint, &fixture.state)
            .is_err()
    );
    assert_eq!(snapshot.enrollment(), EnrollmentState::PendingLedgerCommit);
    snapshot
        .mark_active(&fixture.checkpoint, &fixture.state)
        .expect("activate");
    store.save(&mut snapshot).expect("save exact activation");
    snapshot
        .advance_checkpoint(7, "c".repeat(SHA256_HEX_BYTES))
        .expect("checkpoint");
    store.save(&mut snapshot).expect("save active");

    assert_eq!(snapshot.revision(), 3);
    assert_eq!(snapshot.checkpoint_sequence(), Some(7));
    assert_eq!(
        snapshot.advance_checkpoint(6, "d".repeat(SHA256_HEX_BYTES)),
        Err(VaultError::RevisionConflict)
    );
    snapshot.mark_revoked().expect("revoke");
    store.save(&mut snapshot).expect("save revoked");
    assert_eq!(
        store.load().expect("load revoked").enrollment(),
        EnrollmentState::Revoked
    );
}

#[test]
fn join_activation_accepts_one_local_baseline_and_retains_the_approved_anchor() {
    let fixture = pending_ledger_fixture();
    let mut library = crate::library::Library::default();
    library.toggle_favorite(&crate::api::Song::remote(
        "local-favorite".to_owned(),
        "Local favorite".to_owned(),
        "Local artist".to_owned(),
        "3:00".to_owned(),
    ));
    let local = legacy_state(
        &library,
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .expect("local state");
    let local_device_id = DeviceId::new(fixture.snapshot.device_id()).expect("local device id");
    let plan = plan_join_import(&fixture.state, &local, &local_device_id).expect("join preview");
    assert!(plan.summary.changed);
    assert_eq!(
        plan.candidate.operations.len(),
        fixture.state.operations.len() + 1
    );

    let expected_hash = fixture.checkpoint.hash().expect("approved hash");
    let expected_sequence = fixture.checkpoint.payload.checkpoint_sequence;
    let mut snapshot = fixture.snapshot;
    snapshot
        .mark_active_after_join(&fixture.checkpoint, &plan.candidate)
        .expect("activate joined device");
    assert_eq!(snapshot.enrollment(), EnrollmentState::Active);
    assert_eq!(snapshot.checkpoint_hash(), Some(expected_hash.as_str()));
    assert_eq!(snapshot.checkpoint_sequence(), Some(expected_sequence));
}

#[test]
fn join_activation_rejects_missing_remote_state_and_unexplained_causal_coverage() {
    let mut fixture = pending_ledger_fixture();
    let mut library = crate::library::Library::default();
    library.toggle_favorite(&crate::api::Song::remote(
        "local-favorite".to_owned(),
        "Local favorite".to_owned(),
        "Local artist".to_owned(),
        "3:00".to_owned(),
    ));
    let local = legacy_state(
        &library,
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .expect("local state");
    let local_device_id = DeviceId::new(fixture.snapshot.device_id()).expect("local device id");
    let plan = plan_join_import(&fixture.state, &local, &local_device_id).expect("join preview");

    let mut replaced_remote_operation = plan.candidate.clone();
    replaced_remote_operation.operations[0].operation_id = "forged-initial-device".to_owned();
    replaced_remote_operation
        .normalize()
        .expect("structurally valid replacement");
    assert_eq!(
        fixture
            .snapshot
            .mark_active_after_join(&fixture.checkpoint, &replaced_remote_operation,),
        Err(VaultError::InvalidPrivateStore)
    );

    let mut fixture = pending_ledger_fixture();
    let local_device_id = DeviceId::new(fixture.snapshot.device_id()).expect("local device id");
    let mut unexplained = plan_join_import(&fixture.state, &local, &local_device_id)
        .expect("join preview")
        .candidate;
    unexplained.version_vector.0.insert(
        DeviceId::new("foreign-causal-source").expect("foreign id"),
        1,
    );
    unexplained
        .validate()
        .expect("high-water coverage is structurally valid");
    assert_eq!(
        fixture
            .snapshot
            .mark_active_after_join(&fixture.checkpoint, &unexplained),
        Err(VaultError::InvalidPrivateStore)
    );
}

#[test]
fn join_activation_authenticates_the_exact_pending_checkpoint() {
    let mut fixture = pending_ledger_fixture();
    let mut tampered_checkpoint = fixture.checkpoint.clone();
    let replacement = if tampered_checkpoint.signature.starts_with('A') {
        "B"
    } else {
        "A"
    };
    tampered_checkpoint
        .signature
        .replace_range(0..1, replacement);
    assert_eq!(
        fixture
            .snapshot
            .mark_active_after_join(&tampered_checkpoint, &fixture.state),
        Err(VaultError::SignatureVerificationFailed)
    );

    let fixture = pending_ledger_fixture();
    let expected_hash = fixture.checkpoint.hash().expect("approved hash");
    let mut snapshot = fixture.snapshot;
    snapshot
        .mark_active_after_join(&fixture.checkpoint, &fixture.state)
        .expect("no-op join activation");
    assert_eq!(snapshot.checkpoint_hash(), Some(expected_hash.as_str()));
}

#[test]
fn stale_revision_never_overwrites_newer_secrets() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let mut initial = pending_ledger_snapshot();
    store.create(&mut initial).expect("create");
    let mut first = store.load().expect("first reader");
    let mut stale = store.load().expect("stale reader");

    first.set_credential(
        VaultCredential::bearer_token(SecretString::from("new-token")).expect("token"),
    );
    store.save(&mut first).expect("first save");
    stale.set_credential(
        VaultCredential::bearer_token(SecretString::from("stale-token")).expect("token"),
    );
    assert_eq!(store.save(&mut stale), Err(VaultError::RevisionConflict));
    assert_eq!(
        store
            .load()
            .expect("load winner")
            .credential()
            .expect("credential")
            .secret()
            .expose_secret(),
        "new-token"
    );
}

#[test]
fn missing_corrupt_and_identity_mismatched_files_fail_closed() {
    let directory = TestDirectory::new();
    let store = directory.store();
    assert_eq!(store.load().err(), Some(VaultError::InvalidPrivateStore));

    write_owner_only_atomic(&directory.path(), b"not json").expect("write corruption");
    assert_eq!(store.load().err(), Some(VaultError::InvalidPrivateStore));
    remove_owner_only_file_durable(&directory.path()).expect("remove corruption");

    let mut snapshot = pending_ledger_snapshot();
    store.create(&mut snapshot).expect("create");
    let original = Zeroizing::new(
        read_owner_only_limited(&directory.path(), MAX_PRIVATE_STORE_BYTES)
            .expect("read disk json"),
    );
    let mut tampered = Zeroizing::new(original.to_vec());
    replace_once(&mut tampered, b"device-a", b"device-b");
    write_owner_only_atomic(&directory.path(), &tampered).expect("write device mismatch");
    assert_eq!(store.load().err(), Some(VaultError::InvalidPrivateStore));

    let mut tampered = Zeroizing::new(original.to_vec());
    replace_once(&mut tampered, b"dataset-test", b"dataset-evil");
    write_owner_only_atomic(&directory.path(), &tampered).expect("write dataset mismatch");
    assert_eq!(store.load().err(), Some(VaultError::InvalidPrivateStore));
}

#[test]
fn signed_control_fields_reject_revision_state_checkpoint_and_credential_tampering() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let fixture = pending_ledger_fixture();
    let mut snapshot = fixture.snapshot;
    snapshot
        .mark_active(&fixture.checkpoint, &fixture.state)
        .expect("activate fixture");
    snapshot
        .advance_checkpoint(7, "c".repeat(SHA256_HEX_BYTES))
        .expect("checkpoint fixture");
    snapshot.set_credential(
        VaultCredential::password("webdav-user", SecretString::from("original-secret"))
            .expect("credential fixture"),
    );
    store.create(&mut snapshot).expect("create");
    let original = Zeroizing::new(
        read_owner_only_limited(&directory.path(), MAX_PRIVATE_STORE_BYTES)
            .expect("read signed store"),
    );

    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        b"\"revision\":1",
        b"\"revision\":2",
    );
    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        b"\"enrollment\":\"active\"",
        b"\"enrollment\":\"revoked\"",
    );
    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        b"\"checkpoint_anchor\":{\"sequence\":7",
        b"\"checkpoint_anchor\":{\"sequence\":6",
    );
    let checkpoint_hash = format!("\"hash\":\"{}\"", "c".repeat(SHA256_HEX_BYTES));
    let changed_checkpoint_hash = format!("\"hash\":\"{}\"", "d".repeat(SHA256_HEX_BYTES));
    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        checkpoint_hash.as_bytes(),
        changed_checkpoint_hash.as_bytes(),
    );
    assert_tamper_rejected(
            &store,
            &directory.path(),
            &original,
            b"\"credential\":{\"kind\":\"password\",\"username\":\"webdav-user\",\"secret\":\"original-secret\"}",
            b"\"credential\":{\"kind\":\"bearer_token\",\"username\":null,\"secret\":\"original-secret\"}",
        );
    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        b"\"username\":\"webdav-user\"",
        b"\"username\":\"webdav-evil\"",
    );
    assert_tamper_rejected(
        &store,
        &directory.path(),
        &original,
        b"\"secret\":\"original-secret\"",
        b"\"secret\":\"tampered-secret\"",
    );
}

#[test]
fn remove_is_revision_guarded_and_missing_is_not_recreated() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let mut snapshot = pending_ledger_snapshot();
    store.create(&mut snapshot).expect("create");

    assert_eq!(
        store.remove(snapshot.revision() + 1),
        Err(VaultError::RevisionConflict)
    );
    store
        .remove(snapshot.revision())
        .expect("remove observed revision");
    assert_eq!(store.load().err(), Some(VaultError::InvalidPrivateStore));
}

#[test]
fn lock_contention_and_oversized_json_fail_closed() {
    let directory = TestDirectory::new();
    let store = directory.store();
    let mut snapshot = pending_ledger_snapshot();
    store.create(&mut snapshot).expect("create");

    let held_lock = try_lock_private_file(&store.lock_path)
        .expect("try lock")
        .expect("hold lock");
    assert_eq!(store.load().err(), Some(VaultError::StorageBusy));
    drop(held_lock);

    let oversized = vec![b' '; MAX_PRIVATE_STORE_BYTES as usize + 1];
    write_owner_only_atomic(&directory.path(), &oversized).expect("write oversized file");
    assert_eq!(store.load().err(), Some(VaultError::InvalidPrivateStore));
}

#[cfg(unix)]
#[test]
fn weak_permissions_are_rejected_without_repair() {
    use std::os::unix::fs::PermissionsExt;

    let directory = TestDirectory::new();
    let store = directory.store();
    let mut snapshot = pending_ledger_snapshot();
    store.create(&mut snapshot).expect("create");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o644))
        .expect("weaken permissions");

    assert_eq!(store.load().err(), Some(VaultError::InvalidPrivateStore));
    assert_eq!(
        fs::metadata(directory.path())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o7777,
        0o644
    );
}

fn replace_once(bytes: &mut [u8], needle: &[u8], replacement: &[u8]) {
    assert_eq!(needle.len(), replacement.len());
    let offset = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("needle in private-store JSON");
    bytes[offset..offset + needle.len()].copy_from_slice(replacement);
}

fn assert_tamper_rejected(
    store: &PrivateStore,
    path: &std::path::Path,
    original: &[u8],
    needle: &[u8],
    replacement: &[u8],
) {
    let offset = original
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("control field in private-store JSON");
    let mut tampered = Zeroizing::new(Vec::with_capacity(
        original.len() - needle.len() + replacement.len(),
    ));
    tampered.extend_from_slice(&original[..offset]);
    tampered.extend_from_slice(replacement);
    tampered.extend_from_slice(&original[offset + needle.len()..]);
    write_owner_only_atomic(path, &tampered).expect("write signed-field tamper");
    assert_eq!(store.load().err(), Some(VaultError::InvalidPrivateStore));
}
