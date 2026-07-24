use std::sync::atomic::{AtomicU64, Ordering};

use age::secrecy::SecretString;

use crate::personal_state::{PersonalStateCommit, PersonalStatePaths, legacy_state, load_ledger};
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
    let prepared = prepare(
        &fixture.state,
        0,
        request(recovery_file.clone()),
        &fixture.sync_paths,
    )
    .expect("prepare setup");
    let checksum = prepared
        .private
        .setup_recovery_checksum()
        .expect("recovery marker")
        .to_owned();

    let error = setup_with_transport_using(
        prepared,
        &fixture.personal_paths,
        &fixture.sync_paths,
        &transport,
        || Err(SyncServiceError::Storage),
    )
    .err()
    .expect("injected local failure");
    assert_eq!(error, SyncServiceError::Storage);
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

    let mut pending =
        load_pending_setup(&fixture.sync_paths, "https://dav.example.test/state", None)
            .expect("load pending setup")
            .expect("pending setup");
    let remote = load_resumable_remote_setup(&fixture.state, 0, &pending, &transport)
        .expect("validate remote setup")
        .expect("published remote setup");
    pending.private.set_credential(
        VaultCredential::bearer_token(SecretString::from("replacement-token"))
            .expect("replacement credential"),
    );
    let result = finish_resumed_setup(
        &fixture.state,
        0,
        &fixture.personal_paths,
        &fixture.sync_paths,
        pending,
        remote,
    )
    .expect("resume setup");

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
    let prepared = prepare(
        &fixture.state,
        0,
        request(recovery_file),
        &fixture.sync_paths,
    )
    .expect("prepare setup");

    assert!(
        setup_with_transport(
            prepared,
            &fixture.personal_paths,
            &fixture.sync_paths,
            &failing,
        )
        .is_err()
    );
    assert!(!fixture.sync_paths.private_store().exists());
    assert!(!fixture.sync_paths.profile().exists());
    assert!(!fixture.sync_paths.health().exists());
    assert!(!fixture.sync_paths.audit().exists());
}

#[test]
fn pending_setup_rejects_a_different_endpoint_without_changing_local_state() {
    let fixture = fixture();
    let prepared = prepare(
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
