use std::sync::atomic::{AtomicU64, Ordering};

use age::secrecy::SecretString;

use crate::personal_state::{
    CausalStamp, Dot, Operation, OperationEnvelope, OperationOrigin, PersonalStateCommit,
    PersonalStatePaths, legacy_state, load_ledger,
};
use crate::sync::{
    EncryptedObject, EnrollmentState, FileVaultTransport, ObjectCondition, ObjectKey,
    ObjectMetadata, ObjectWriteResult, PrivateStore, SyncPaths, VaultCredential, VaultError,
    VaultTransport, WebDavProfileStore,
};

use super::*;

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "yututui-setup-resume-{}-{sequence}",
            std::process::id()
        ));
        crate::util::safe_fs::ensure_private_dir(&path).expect("create test root");
        Self(path)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    root: TestRoot,
    personal_paths: PersonalStatePaths,
    sync_paths: SyncPaths,
    state: PersonalStateV2,
}

fn fixture() -> Fixture {
    let root = TestRoot::new();
    let personal_paths = PersonalStatePaths::for_data_root(root.0.clone());
    let sync_paths = SyncPaths::for_data_root(root.0.clone());
    let state = legacy_state(
        &crate::library::Library::default(),
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .expect("legacy state");
    let state = PersonalStateCommit::prepare_for_runtime(state, 0)
        .expect("prepare initial state")
        .commit(&personal_paths)
        .expect("install initial state");
    Fixture {
        root,
        personal_paths,
        sync_paths,
        state,
    }
}

fn request(recovery_file: PathBuf) -> SetupRequest {
    SetupRequest {
        endpoint: "https://dav.example.test/state".to_owned(),
        custom_ca_pem: None,
        device_name: "First device".to_owned(),
        credential: VaultCredential::bearer_token(SecretString::from("test-token"))
            .expect("credential"),
        recovery_file,
    }
}

#[test]
fn setup_resumes_after_remote_bootstrap_and_pre_marker_local_failure() {
    let fixture = fixture();
    let transport =
        FileVaultTransport::create(fixture.root.0.join("remote")).expect("file vault transport");
    let recovery_file = fixture.root.0.join("recovery.json");
    let initial = prepare_initial(
        &fixture.state,
        0,
        request(recovery_file.clone()),
        &fixture.sync_paths,
    )
    .expect("prepare setup");
    let checksum = initial
        .private
        .setup_recovery_checksum()
        .expect("recovery marker")
        .to_owned();

    let prepared = prepare_setup_with_transport(initial, &fixture.sync_paths, &transport)
        .expect("remote setup");
    assert_eq!(prepared.recovery_checksum(), checksum);
    assert!(recovery_file.is_file());
    assert_eq!(
        load_ledger(&fixture.personal_paths)
            .expect("load ledger")
            .expect("installed ledger"),
        fixture.state
    );
    let pending = PrivateStore::new(fixture.sync_paths.private_store())
        .expect("private store")
        .load()
        .expect("pending private");
    assert_eq!(pending.enrollment(), EnrollmentState::PendingLedgerCommit);
    assert_eq!(pending.setup_recovery_checksum(), Some(checksum.as_str()));
    assert!(!fixture.sync_paths.health().exists());
    assert!(!fixture.sync_paths.audit().exists());

    let pending = load_saved_pending_setup(&fixture.sync_paths).expect("load saved pending setup");
    let prepared = prepared_saved_setup_with_transport(&fixture.state, 0, pending, &transport)
        .expect("prepare resumed setup");
    let result = apply_prepared_setup(
        &fixture.state,
        0,
        &fixture.personal_paths,
        &fixture.sync_paths,
        prepared,
    )
    .expect("apply resumed setup");

    assert!(result.resumed);
    assert_eq!(result.recovery_checksum, checksum);
    assert_ne!(result.state, fixture.state);
    assert_eq!(
        load_ledger(&fixture.personal_paths)
            .expect("load resumed ledger")
            .expect("resumed ledger"),
        result.state
    );
    let active = PrivateStore::new(fixture.sync_paths.private_store())
        .expect("private store")
        .load()
        .expect("active private");
    assert_eq!(active.enrollment(), EnrollmentState::Active);
    assert_eq!(active.setup_recovery_checksum(), None);
}

#[test]
fn confirmed_missing_manifest_cleans_credentials_and_profile_after_bootstrap_failure() {
    let fixture = fixture();
    let transport =
        FileVaultTransport::create(fixture.root.0.join("remote")).expect("file vault transport");
    let failing = RejectWrites(&transport);
    let recovery_file = fixture.root.0.join("recovery.json");
    let initial = prepare_initial(
        &fixture.state,
        0,
        request(recovery_file.clone()),
        &fixture.sync_paths,
    )
    .expect("prepare setup");

    assert!(prepare_setup_with_transport(initial, &fixture.sync_paths, &failing).is_err());
    assert!(!fixture.sync_paths.private_store().exists());
    assert!(!fixture.sync_paths.profile().exists());
    assert!(!recovery_file.exists());
    assert!(!fixture.sync_paths.health().exists());
    assert!(!fixture.sync_paths.audit().exists());
}

#[test]
fn pending_setup_rejects_a_different_endpoint_without_changing_local_state() {
    let fixture = fixture();
    let prepared = prepare_initial(
        &fixture.state,
        0,
        request(fixture.root.0.join("recovery.json")),
        &fixture.sync_paths,
    )
    .expect("prepare setup");
    let private_store =
        PrivateStore::new(fixture.sync_paths.private_store()).expect("private store");
    let profile_store =
        WebDavProfileStore::new(fixture.sync_paths.profile()).expect("profile store");
    let mut private = prepared.private;
    let mut profile = prepared.profile;
    private_store.create(&mut private).expect("create private");
    profile_store
        .create(&mut profile, private.device())
        .expect("create profile");

    let mut different = request(fixture.root.0.join("unused-recovery.json"));
    different.endpoint = "https://other.example.test/state".to_owned();
    assert_eq!(
        setup(
            &fixture.state,
            0,
            &fixture.personal_paths,
            &fixture.sync_paths,
            different,
        )
        .err(),
        Some(SyncServiceError::AlreadyConfigured)
    );
    assert_eq!(
        load_ledger(&fixture.personal_paths)
            .expect("load ledger")
            .expect("ledger"),
        fixture.state
    );
}

#[test]
fn detached_setup_prepares_without_installing_and_applies_the_exact_target() {
    let fixture = fixture();
    let transport =
        FileVaultTransport::create(fixture.root.0.join("remote")).expect("file vault transport");
    let initial = prepare_initial(
        &fixture.state,
        0,
        request(fixture.root.0.join("recovery.json")),
        &fixture.sync_paths,
    )
    .expect("initial setup");
    let prepared = prepare_setup_with_transport(initial, &fixture.sync_paths, &transport)
        .expect("remote bootstrap");
    let target = prepared.target_state(&fixture.state).expect("exact target");
    assert_eq!(target, prepared.checkpoint.payload.state);
    assert_eq!(
        load_ledger(&fixture.personal_paths)
            .expect("load ledger")
            .expect("ledger"),
        fixture.state
    );
    assert_eq!(
        PrivateStore::new(fixture.sync_paths.private_store())
            .expect("private store")
            .load()
            .expect("pending private")
            .enrollment(),
        EnrollmentState::PendingLedgerCommit
    );

    let result = apply_prepared_setup(
        &fixture.state,
        0,
        &fixture.personal_paths,
        &fixture.sync_paths,
        prepared,
    )
    .expect("apply setup");
    assert_eq!(result.state, target);
    assert_eq!(
        PrivateStore::new(fixture.sync_paths.private_store())
            .expect("private store")
            .load()
            .expect("active private")
            .enrollment(),
        EnrollmentState::Active
    );
}

#[test]
fn detached_setup_rebases_a_contiguous_local_suffix_after_the_bootstrap() {
    let fixture = fixture();
    let transport =
        FileVaultTransport::create(fixture.root.0.join("remote")).expect("file vault transport");
    let initial = prepare_initial(
        &fixture.state,
        0,
        request(fixture.root.0.join("recovery.json")),
        &fixture.sync_paths,
    )
    .expect("initial setup");
    let prepared = prepare_setup_with_transport(initial, &fixture.sync_paths, &transport)
        .expect("remote bootstrap");
    let local = append_unkeyed_local_operation(
        &fixture.state,
        Operation::SetAvoidArtist {
            artist_key: "in-flight-artist".to_owned(),
            avoid: true,
        },
    );
    let local = PersonalStateCommit::prepare_for_runtime(local, 0)
        .expect("prepare local suffix")
        .commit(&fixture.personal_paths)
        .expect("commit local suffix");

    let target = prepared.target_state(&local).expect("rebased target");
    let bootstrap_sequence = prepared
        .checkpoint
        .payload
        .state
        .version_vector
        .observed(prepared.device_id());
    let rebased = target
        .operations
        .iter()
        .find(|operation| {
            matches!(
                operation.operation,
                Operation::SetAvoidArtist {
                    ref artist_key,
                    avoid: true
                } if artist_key == "in-flight-artist"
            )
        })
        .expect("rebased local operation");
    assert_eq!(rebased.origin, OperationOrigin::Local);
    assert_eq!(rebased.stamp.dot.device_id, *prepared.device_id());
    assert_eq!(rebased.stamp.dot.sequence, bootstrap_sequence + 1);
    assert_eq!(
        rebased.stamp.observed,
        prepared.checkpoint.payload.state.version_vector
    );

    let result = apply_prepared_setup(
        &local,
        0,
        &fixture.personal_paths,
        &fixture.sync_paths,
        prepared,
    )
    .expect("apply rebased setup");
    assert_eq!(result.state, target);
}

#[test]
fn setup_activation_persistence_retries_the_exact_target_idempotently() {
    let fixture = fixture();
    let transport =
        FileVaultTransport::create(fixture.root.0.join("remote")).expect("file vault transport");
    let initial = prepare_initial(
        &fixture.state,
        0,
        request(fixture.root.0.join("recovery.json")),
        &fixture.sync_paths,
    )
    .expect("initial setup");
    let prepared = prepare_setup_with_transport(initial, &fixture.sync_paths, &transport)
        .expect("remote bootstrap");
    let local = append_unkeyed_local_operation(
        &fixture.state,
        Operation::SetAvoidArtist {
            artist_key: "activation-retry-artist".to_owned(),
            avoid: true,
        },
    );
    let local = PersonalStateCommit::prepare_for_runtime(local, 0)
        .expect("prepare local suffix")
        .commit(&fixture.personal_paths)
        .expect("commit local suffix");

    let writer = crate::sync::service::PersonalSyncPersistence::setup_activation(
        local.clone(),
        0,
        prepared.clone(),
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture.root.0.clone()),
    )
    .expect("prepare owner activation");
    let target = writer.state().clone();
    writer.write().expect("first activation write");
    assert!(writer.committed());
    assert_eq!(
        load_ledger(&fixture.personal_paths).expect("load activated ledger"),
        Some(target.clone())
    );

    let retry = crate::sync::service::PersonalSyncPersistence::setup_activation(
        local,
        0,
        prepared,
        fixture.personal_paths.clone(),
        SyncPaths::for_data_root(fixture.root.0.clone()),
    )
    .expect("recreate unacknowledged activation");
    assert_eq!(retry.state(), &target);
    retry.write().expect("idempotent activation retry");
    assert!(retry.committed());
    assert_eq!(
        PrivateStore::new(fixture.sync_paths.private_store())
            .expect("private store")
            .load()
            .expect("active private")
            .enrollment(),
        EnrollmentState::Active
    );
}

#[test]
fn ambiguous_bootstrap_failure_retains_pending_keys_and_new_recovery_file() {
    let fixture = fixture();
    let recovery_file = fixture.root.0.join("recovery.json");
    let initial = prepare_initial(
        &fixture.state,
        0,
        request(recovery_file.clone()),
        &fixture.sync_paths,
    )
    .expect("initial setup");

    assert!(
        prepare_setup_with_transport(initial, &fixture.sync_paths, &UnavailableTransport).is_err()
    );
    assert!(fixture.sync_paths.private_store().exists());
    assert!(fixture.sync_paths.profile().exists());
    assert!(recovery_file.exists());
}

fn append_unkeyed_local_operation(
    state: &PersonalStateV2,
    operation: Operation,
) -> PersonalStateV2 {
    let device_id = state
        .device_registry
        .values()
        .find(|device| !device.revoked && device.device_id.as_str() != "legacy")
        .expect("local device")
        .device_id
        .clone();
    let sequence = state.version_vector.observed(&device_id) + 1;
    let dot = Dot {
        device_id: device_id.clone(),
        sequence,
    };
    let mut candidate = state.clone();
    candidate.operations.push(OperationEnvelope {
        operation_id: format!("{}:{sequence}", device_id.as_str()),
        stamp: CausalStamp {
            dot: dot.clone(),
            observed: state.version_vector.clone(),
            recorded_at_unix: 123,
        },
        origin: OperationOrigin::Local,
        operation,
    });
    candidate.version_vector.observe(&dot);
    candidate.projection_fingerprint = None;
    candidate.normalize().expect("normalize local suffix");
    candidate
}

struct RejectWrites<'a>(&'a FileVaultTransport);

impl VaultTransport for RejectWrites<'_> {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        self.0.get(key, max_bytes)
    }

    fn put(
        &self,
        _key: &ObjectKey,
        _object: &EncryptedObject,
        _condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError> {
        Err(VaultError::StorageFailed)
    }

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        self.0.list(prefix, max_resources)
    }
}

struct UnavailableTransport;

impl VaultTransport for UnavailableTransport {
    fn get(
        &self,
        _key: &ObjectKey,
        _max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        Err(VaultError::StorageFailed)
    }

    fn put(
        &self,
        _key: &ObjectKey,
        _object: &EncryptedObject,
        _condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError> {
        Err(VaultError::StorageFailed)
    }

    fn list(
        &self,
        _prefix: &ObjectKey,
        _max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        Err(VaultError::StorageFailed)
    }
}
