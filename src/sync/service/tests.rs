use crate::personal_state::{PersonalStatePaths, PersonalStateV2};

use super::{SyncServiceError, load_personal_state_read_only, read_status, sync_now};
use crate::sync::{
    SyncAuditOutcome, SyncAuditStore, SyncFailureKind, SyncHealthState, SyncHealthStore, SyncPaths,
};

struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "yututui-sync-service-test-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn unconfigured_sync_fails_without_creating_misleading_health_state() {
    let temp = TempRoot::new();
    let personal_paths = PersonalStatePaths::for_data_root(temp.0.clone());
    let sync_paths = SyncPaths::for_data_root(temp.0.clone());
    let state = PersonalStateV2::empty("dataset-unconfigured-sync".to_owned()).unwrap();

    let error = sync_now(&state, 0, &personal_paths, &sync_paths)
        .err()
        .expect("sync must require setup");
    assert_eq!(error, SyncServiceError::NotConfigured);
    let status = read_status(&sync_paths).unwrap();
    assert!(!status.configured);
    assert_eq!(status.state, SyncHealthState::Off);
    assert!(!sync_paths.health().exists());
    assert!(!sync_paths.audit().exists());
}

#[test]
fn public_errors_and_reasons_are_redacted_fixed_vocabulary() {
    for error in [
        SyncServiceError::NotConfigured,
        SyncServiceError::Authentication,
        SyncServiceError::Certificate,
        SyncServiceError::Offline,
        SyncServiceError::InvalidRemoteData,
        SyncServiceError::LocalStateChanged,
        SyncServiceError::Storage,
    ] {
        let rendered = error.to_string();
        let reason = error.reason();
        assert!(!rendered.contains("://"));
        assert!(!rendered.contains('/'));
        assert!(!reason.contains("://"));
        assert!(!reason.contains('/'));
    }
}

#[test]
fn owner_prepare_failure_is_retained_without_replacing_the_primary_error() {
    let temp = TempRoot::new();
    let paths = SyncPaths::for_data_root(temp.0.clone());
    std::fs::create_dir_all(paths.root()).unwrap();

    let result =
        super::track_owner_preparation::<()>(&paths, || Err(SyncServiceError::Authentication));

    assert_eq!(result, Err(SyncServiceError::Authentication));
    let health = SyncHealthStore::new(paths.health())
        .unwrap()
        .load(true)
        .unwrap();
    assert_eq!(health.state, SyncHealthState::NeedsAttention);
    assert_eq!(health.failure, Some(SyncFailureKind::Authentication));
    let audit = SyncAuditStore::new(paths.audit())
        .unwrap()
        .load(crate::signals::unix_now())
        .unwrap();
    assert_eq!(audit.entries().len(), 1);
    assert_eq!(audit.entries()[0].outcome, SyncAuditOutcome::Failed);
    assert_eq!(
        audit.entries()[0].failure,
        Some(SyncFailureKind::Authentication)
    );
}

#[test]
fn owner_prepare_error_wins_when_health_storage_is_unavailable() {
    let temp = TempRoot::new();
    std::fs::write(temp.0.join("sync"), b"not a directory").unwrap();
    let paths = SyncPaths::for_data_root(temp.0.clone());

    let result = super::track_owner_preparation::<()>(&paths, || Err(SyncServiceError::Offline));

    assert_eq!(result, Err(SyncServiceError::Offline));
}

#[test]
fn read_only_personal_state_refuses_and_preserves_pending_transactions() {
    let temp = TempRoot::new();
    let personal_paths = PersonalStatePaths::for_data_root(temp.0.clone());
    let pending = personal_paths.transactions.join("pending-owner-write");
    std::fs::create_dir_all(&pending).unwrap();
    let sentinel = pending.join("staged-ledger.json");
    std::fs::write(&sentinel, b"owner-staged-bytes").unwrap();

    let error = load_personal_state_read_only(&personal_paths)
        .expect_err("a reader must not recover an owner's pending transaction");

    assert_eq!(error, SyncServiceError::Storage);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"owner-staged-bytes");
    assert!(pending.exists());
    assert!(!personal_paths.ledger.exists());
}

#[test]
fn read_only_personal_state_returns_none_without_creating_storage() {
    let temp = TempRoot::new();
    let personal_paths = PersonalStatePaths::for_data_root(temp.0.join("missing-data"));

    assert_eq!(
        load_personal_state_read_only(&personal_paths).unwrap(),
        None
    );
    assert!(!personal_paths.data_root.exists());
}
