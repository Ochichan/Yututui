use serde::{Deserialize, Serialize};

use crate::personal_state::{PersonalStatePaths, PersonalStateV2};

use super::super::{
    EnrollmentState, PrivateStore, SyncAuditEntry, SyncAuditStore, SyncFailureKind, SyncHealth,
    SyncHealthState, SyncHealthStore, SyncPaths, WebDavProfileStore,
};
use super::{DeviceSummary, SyncServiceError};

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

/// Build the privacy-safe status projection embedded in owner status snapshots.
///
/// Errors become one of the same five public states; endpoint, path, credential, and upstream
/// error details never enter the local IPC protocol.
pub fn read_current_status(in_progress: bool) -> SyncStatusReport {
    let mut report = match SyncPaths::current()
        .map_err(SyncServiceError::from)
        .and_then(|paths| read_status(&paths))
    {
        Ok(report) => report,
        Err(error) => {
            let failure = error.failure_kind().unwrap_or(SyncFailureKind::Storage);
            let state = if failure == SyncFailureKind::Offline {
                SyncHealthState::OfflineWillRetry
            } else {
                SyncHealthState::NeedsAttention
            };
            SyncStatusReport {
                state,
                label: state.label().to_owned(),
                configured: true,
                failure: Some(failure),
                recovery_action: Some(failure.recovery_action().to_owned()),
                ..SyncStatusReport::default()
            }
        }
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
    // process which began the attempt disappeared before recording success or failure.
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

    struct TempRoot(std::path::PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "yututui-sync-status-{}-{sequence}",
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
        let mut report = report_from_health(true, Some("device-a".to_owned()), reloaded);
        assert_eq!(report.state, SyncHealthState::NeedsAttention);
        assert_eq!(report.failure, Some(SyncFailureKind::LocalStateChanged));
        assert_eq!(report.recovery_action.as_deref(), Some("Retry"));
        assert_eq!(report.last_attempt_unix, Some(42));

        apply_live_progress(&mut report, true);
        assert_eq!(report.state, SyncHealthState::Syncing);
        assert_eq!(report.label, "Syncing");
        assert_eq!(report.failure, None);
        assert_eq!(report.recovery_action, None);
    }
}
