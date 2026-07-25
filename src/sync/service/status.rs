use serde::{Deserialize, Serialize};

use crate::personal_state::{PersonalStatePaths, PersonalStateV2};

use super::super::{
    EnrollmentState, PrivateStore, SyncAuditEntry, SyncAuditStore, SyncFailureKind, SyncHealth,
    SyncHealthState, SyncHealthStore, SyncPaths, WebDavProfileStore,
};
use super::{DeviceSummary, SyncServiceError};

/// Sanitized setup/device-connection phase used by interactive clients.
///
/// This projection deliberately excludes endpoints, paths, pairing codes, credentials, and key
/// material. It is safe to retain in a TUI model or expose through an additive status snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncLifecycleState {
    Absent,
    SetupPending,
    Active,
    JoinWaiting,
    JoinReadyToMerge,
    Revoked,
    NeedsCleanup,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOverview {
    pub status: SyncStatusReport,
    pub lifecycle: SyncLifecycleState,
    pub devices: Vec<DeviceSummary>,
    pub audit: Vec<SyncAuditEntry>,
}

pub struct LocalSyncSnapshot {
    pub state: PersonalStateV2,
    pub playlist_revision: u64,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatusReport {
    pub state: SyncHealthState,
    pub label: String,
    pub configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub last_attempt_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub last_success_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<SyncFailureKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_action: Option<String>,
}

impl Default for SyncStatusReport {
    fn default() -> Self {
        Self {
            state: SyncHealthState::Off,
            label: SyncHealthState::Off.label().to_owned(),
            configured: false,
            device_id: None,
            last_attempt_unix: None,
            last_success_unix: None,
            failure: None,
            recovery_action: None,
        }
    }
}

pub fn read_status(paths: &SyncPaths) -> Result<SyncStatusReport, SyncServiceError> {
    let private_store = PrivateStore::new(paths.private_store())?;
    let private = match private_store.load() {
        Ok(private) => Some(private),
        Err(super::super::VaultError::InvalidPrivateStore)
            if !regular_file_exists(paths.private_store())? =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    let configured = if let Some(private) = private.as_ref() {
        let profile = WebDavProfileStore::new(paths.profile())?.load(private.device())?;
        if profile.dataset_id() != private.dataset_id()
            || profile.device_id() != private.device_id()
        {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        matches!(private.enrollment(), EnrollmentState::Active)
    } else {
        if regular_file_exists(paths.profile())? {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        false
    };
    let health = SyncHealthStore::new(paths.health())?.load(configured)?;
    Ok(report_from_health(
        configured,
        private.map(|private| private.device_id().to_owned()),
        health,
    ))
}

pub fn read_lifecycle(paths: &SyncPaths) -> Result<SyncLifecycleState, SyncServiceError> {
    let private_exists = regular_file_exists(paths.private_store())?;
    let profile_exists = regular_file_exists(paths.profile())?;
    if !private_exists {
        return Ok(if profile_exists {
            SyncLifecycleState::NeedsCleanup
        } else {
            SyncLifecycleState::Absent
        });
    }

    let private = PrivateStore::new(paths.private_store())?.load()?;
    if !profile_exists {
        return Ok(SyncLifecycleState::NeedsCleanup);
    }
    let profile = WebDavProfileStore::new(paths.profile())
        .and_then(|store| store.load(private.device()))
        .map_err(|_| SyncServiceError::InvalidRemoteData)?;
    if profile.dataset_id() != private.dataset_id() || profile.device_id() != private.device_id() {
        return Ok(SyncLifecycleState::NeedsCleanup);
    }

    Ok(match private.enrollment() {
        EnrollmentState::Active => SyncLifecycleState::Active,
        EnrollmentState::Revoked => SyncLifecycleState::Revoked,
        EnrollmentState::PendingApproval => SyncLifecycleState::JoinWaiting,
        EnrollmentState::PendingLedgerCommit if private.setup_recovery_checksum().is_some() => {
            SyncLifecycleState::SetupPending
        }
        EnrollmentState::PendingLedgerCommit
            if regular_file_exists(paths.pending_join_checkpoint())? =>
        {
            SyncLifecycleState::JoinReadyToMerge
        }
        EnrollmentState::PendingLedgerCommit => SyncLifecycleState::NeedsCleanup,
    })
}

pub fn read_overview(
    paths: &SyncPaths,
    state: &PersonalStateV2,
    in_progress: bool,
    now_unix: i64,
) -> Result<SyncOverview, SyncServiceError> {
    // Read the durable lifecycle first. Incomplete local artifacts intentionally fail
    // `read_status`, but the interactive surface still needs a bounded, actionable projection
    // from which the user can recover. A revoked device likewise keeps the health written while
    // it was active; loading that non-Off health as "not configured" would reject the otherwise
    // valid revoked lifecycle.
    let lifecycle = read_lifecycle(paths)?;
    let status = match lifecycle {
        // Cleanup is an authoritative local-storage problem, not a live network operation. Do
        // not let an obsolete in-progress flag hide its one human-readable recovery action.
        SyncLifecycleState::NeedsCleanup => needs_cleanup_status(),
        SyncLifecycleState::Revoked => SyncStatusReport::default(),
        _ => {
            let mut status = read_status(paths)?;
            if lifecycle == SyncLifecycleState::Active
                && super::pairing::host_pairing_needs_review(state, paths)?
            {
                let failure = SyncFailureKind::DeviceApproval;
                status.state = SyncHealthState::NeedsAttention;
                status.label = SyncHealthState::NeedsAttention.label().to_owned();
                status.failure = Some(failure);
                status.recovery_action = Some(failure.recovery_action().to_owned());
            }
            apply_live_progress(&mut status, in_progress);
            status
        }
    };
    Ok(SyncOverview {
        status,
        lifecycle,
        devices: read_devices(state),
        audit: read_audit(paths, now_unix)?,
    })
}

fn needs_cleanup_status() -> SyncStatusReport {
    let state = SyncHealthState::NeedsAttention;
    let failure = SyncFailureKind::Storage;
    SyncStatusReport {
        state,
        label: state.label().to_owned(),
        configured: true,
        failure: Some(failure),
        recovery_action: Some(failure.recovery_action().to_owned()),
        ..SyncStatusReport::default()
    }
}

/// Build the privacy-safe status projection embedded in owner status snapshots.
///
/// Errors become one of the same five public states; endpoint, path, credential, and upstream
/// error details never enter the local IPC protocol.
pub fn read_current_status(in_progress: bool) -> SyncStatusReport {
    match SyncPaths::current() {
        Ok(paths) => read_current_status_at(&paths, in_progress),
        Err(error) => status_from_error(error.into(), in_progress),
    }
}

/// Build the owner status projection from an explicitly bound data root.
///
/// Daemon owners use this entry point so a status read observes the same store set as their
/// personal-state coordinator instead of whichever process-global data directory is visible at
/// the instant the snapshot is built.
pub fn read_current_status_at(paths: &SyncPaths, in_progress: bool) -> SyncStatusReport {
    let mut report = match read_status(paths) {
        Ok(report) => report,
        Err(error) => return status_from_error(error, in_progress),
    };
    apply_live_progress(&mut report, in_progress);
    report
}

fn status_from_error(error: SyncServiceError, in_progress: bool) -> SyncStatusReport {
    let failure = error.failure_kind().unwrap_or(SyncFailureKind::Storage);
    let state = if failure == SyncFailureKind::Offline {
        SyncHealthState::OfflineWillRetry
    } else {
        SyncHealthState::NeedsAttention
    };
    let mut report = SyncStatusReport {
        state,
        label: state.label().to_owned(),
        configured: true,
        failure: Some(failure),
        recovery_action: Some(failure.recovery_action().to_owned()),
        ..SyncStatusReport::default()
    };
    apply_live_progress(&mut report, in_progress);
    report
}

fn report_from_health(
    configured: bool,
    device_id: Option<String>,
    health: SyncHealth,
) -> SyncStatusReport {
    // `Syncing` is a live-owner fact, not a durable terminal state. Seeing it on disk means the
    // process which began the attempt may have disappeared before recording success or failure.
    // Only the owner's in-memory `in_progress` flag may overlay it below; being a read-only
    // secondary is not evidence that the writer is still running.
    let (state, failure) = if health.state == SyncHealthState::Syncing {
        (
            SyncHealthState::NeedsAttention,
            Some(SyncFailureKind::LocalStateChanged),
        )
    } else {
        (health.state, health.failure)
    };
    SyncStatusReport {
        state,
        label: state.label().to_owned(),
        configured,
        device_id,
        last_attempt_unix: health.last_attempt_unix,
        last_success_unix: health.last_success_unix,
        failure,
        recovery_action: failure.map(|failure| failure.recovery_action().to_owned()),
    }
}

fn apply_live_progress(report: &mut SyncStatusReport, in_progress: bool) {
    if in_progress {
        report.state = SyncHealthState::Syncing;
        report.label = SyncHealthState::Syncing.label().to_owned();
        report.failure = None;
        report.recovery_action = None;
    }
}

pub fn read_audit(
    paths: &SyncPaths,
    now_unix: i64,
) -> Result<Vec<SyncAuditEntry>, SyncServiceError> {
    Ok(SyncAuditStore::new(paths.audit())?
        .load(now_unix)?
        .entries()
        .to_vec())
}

pub fn read_devices(state: &PersonalStateV2) -> Vec<DeviceSummary> {
    state
        .device_registry
        .values()
        .filter(|device| device.device_id.as_str() != "legacy")
        .map(DeviceSummary::from)
        .collect()
}

pub fn load_local_snapshot() -> Result<LocalSyncSnapshot, SyncServiceError> {
    let stores = crate::persist::load_startup_store_set().map_err(|_| SyncServiceError::Storage)?;
    Ok(LocalSyncSnapshot {
        state: stores.personal_state,
        playlist_revision: stores.playlists.revision(),
    })
}

/// Read only the currently installed personal-state ledger without repairing transaction
/// artifacts or reconciling runtime projections.
///
/// Observational CLI commands can run beside the primary writer. An in-flight transaction is
/// therefore reported as temporarily unavailable instead of being completed or discarded here.
pub fn load_personal_state_read_only(
    paths: &PersonalStatePaths,
) -> Result<Option<PersonalStateV2>, SyncServiceError> {
    crate::personal_state::load_ledger_read_only(paths).map_err(Into::into)
}

fn regular_file_exists(path: &std::path::Path) -> Result<bool, SyncServiceError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(SyncServiceError::Storage),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(SyncServiceError::Storage),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::personal_state::{
        CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
        VersionVector,
    };
    use crate::sync::{
        CheckpointAnchor, DeviceSecretMaterial, MembershipAnchor, MembershipChain,
        PrivateStoreSnapshot, RecoveryKit, SignedCheckpoint, SignedMembershipRoot, WebDavProfile,
    };

    struct TempRoot(std::path::PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "yututui-sync-status-{}-{sequence}",
                std::process::id()
            ));
            crate::util::safe_fs::ensure_private_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn state_with_device(dataset_id: &str, device: &DeviceRecord) -> PersonalStateV2 {
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
                recorded_at_unix: 1,
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

    fn install_revoked_profile(paths: &SyncPaths) -> PersonalStateV2 {
        const DATASET_ID: &str = "dataset-status-revoked";
        let recovery = RecoveryKit::generate(DATASET_ID, None).unwrap();
        let device = DeviceSecretMaterial::generate_for("status-revoked-device").unwrap();
        let device_record = DeviceRecord {
            device_id: DeviceId::new(device.device_id()).unwrap(),
            name: "Revoked device".to_owned(),
            revoked: false,
            public_identity: Some(device.public_identity()),
        };
        let root = SignedMembershipRoot::create(
            DATASET_ID,
            recovery.recovery_recipient(),
            &recovery.signing_key().unwrap(),
            device_record.clone(),
        )
        .unwrap();
        let root_hash = root.hash().unwrap();
        let membership = MembershipChain::new(root);
        let state = state_with_device(DATASET_ID, &device_record);
        let checkpoint = SignedCheckpoint::create(
            membership,
            &MembershipAnchor::RootHash(root_hash.clone()),
            device_record.device_id,
            device.signing_key(),
            &CheckpointAnchor::default(),
            state.clone(),
        )
        .unwrap();
        let mut private = PrivateStoreSnapshot::pending_ledger_commit(
            device,
            recovery.recovery_recipient(),
            recovery.recovery_verifying_key().unwrap(),
            root_hash,
            &checkpoint,
        )
        .unwrap();
        private.mark_active(&checkpoint, &state).unwrap();
        private.mark_revoked().unwrap();

        std::fs::create_dir_all(paths.root()).unwrap();
        PrivateStore::new(paths.private_store())
            .unwrap()
            .create(&mut private)
            .unwrap();
        let mut profile = WebDavProfile::new(
            DATASET_ID,
            private.device(),
            "https://dav.example.test/state",
        )
        .unwrap();
        WebDavProfileStore::new(paths.profile())
            .unwrap()
            .create(&mut profile, private.device())
            .unwrap();

        let health_store = SyncHealthStore::new(paths.health()).unwrap();
        let health = health_store.load(true).unwrap();
        health_store.save(&health, health.succeeded(7)).unwrap();
        state
    }

    #[test]
    fn cleanup_overview_survives_the_status_error_with_actionable_storage_health() {
        let root = TempRoot::new();
        let paths = SyncPaths::for_data_root(root.0.clone());
        std::fs::create_dir_all(paths.root()).unwrap();
        std::fs::write(paths.profile(), b"orphaned-profile").unwrap();
        let state = PersonalStateV2::empty("dataset-cleanup-overview".to_owned()).unwrap();

        assert_eq!(
            read_lifecycle(&paths).unwrap(),
            SyncLifecycleState::NeedsCleanup
        );
        assert_eq!(
            read_status(&paths),
            Err(SyncServiceError::InvalidRemoteData)
        );

        let overview = read_overview(&paths, &state, true, 10).unwrap();
        assert_eq!(overview.lifecycle, SyncLifecycleState::NeedsCleanup);
        assert_eq!(overview.status.state, SyncHealthState::NeedsAttention);
        assert_eq!(overview.status.label, "Needs attention");
        assert!(overview.status.configured);
        assert_eq!(overview.status.failure, Some(SyncFailureKind::Storage));
        assert_eq!(
            overview.status.recovery_action.as_deref(),
            Some("Review sync audit")
        );
    }

    #[test]
    fn revoked_overview_ignores_health_from_the_former_active_lifecycle() {
        let root = TempRoot::new();
        let paths = SyncPaths::for_data_root(root.0.clone());
        let state = install_revoked_profile(&paths);

        assert_eq!(read_lifecycle(&paths).unwrap(), SyncLifecycleState::Revoked);
        assert_eq!(read_status(&paths), Err(SyncServiceError::Storage));

        let overview = read_overview(&paths, &state, false, 10).unwrap();
        assert_eq!(overview.lifecycle, SyncLifecycleState::Revoked);
        assert_eq!(overview.status, SyncStatusReport::default());
        assert_eq!(overview.devices.len(), 1);
    }

    #[test]
    fn persisted_syncing_is_interrupted_but_a_live_owner_can_overlay_it() {
        let root = TempRoot::new();
        let paths = SyncPaths::for_data_root(root.0.clone());
        std::fs::create_dir_all(paths.root()).unwrap();
        let store = SyncHealthStore::new(paths.health()).unwrap();
        let initial = store.load(true).unwrap();
        store.save(&initial, initial.syncing(42)).unwrap();

        // Reload from disk to model a fresh process after the writer disappeared.
        let reloaded = SyncHealthStore::new(paths.health())
            .unwrap()
            .load(true)
            .unwrap();
        let mut report = report_from_health(true, Some("device-a".to_owned()), reloaded.clone());
        assert_eq!(report.state, SyncHealthState::NeedsAttention);
        assert_eq!(report.failure, Some(SyncFailureKind::LocalStateChanged));
        assert_eq!(report.recovery_action.as_deref(), Some("Retry"));
        assert_eq!(report.last_attempt_unix, Some(42));

        apply_live_progress(&mut report, true);
        assert_eq!(report.state, SyncHealthState::Syncing);
        assert_eq!(report.label, "Syncing");
        assert_eq!(report.failure, None);
        assert_eq!(report.recovery_action, None);

        let secondary = report_from_health(true, Some("device-a".to_owned()), reloaded);
        assert_eq!(secondary.state, SyncHealthState::NeedsAttention);
        assert_eq!(secondary.failure, Some(SyncFailureKind::LocalStateChanged));
    }
}
