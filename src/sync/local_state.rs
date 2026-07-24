//! Device-local health and redacted audit persistence for manual sync.
//!
//! These records are operational hints, not portable personal state. They never contain an
//! endpoint, credential, filesystem path, pairing code, recovery secret, or upstream error text.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::error::VaultError;

#[cfg(test)]
use std::fs;

const HEALTH_KIND: &str = "yututui_sync_health";
const AUDIT_KIND: &str = "yututui_sync_audit";
const LOCAL_SCHEMA_VERSION: u32 = 1;
const MAX_HEALTH_BYTES: u64 = 64 * 1024;
const MAX_AUDIT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_AUDIT_ENTRIES: usize = 2_000;
const AUDIT_RETENTION_SECONDS: i64 = 90 * 24 * 60 * 60;

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncHealthState {
    Off,
    UpToDate,
    Syncing,
    OfflineWillRetry,
    NeedsAttention,
}

impl SyncHealthState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::UpToDate => "Up to date",
            Self::Syncing => "Syncing",
            Self::OfflineWillRetry => "Offline — will retry",
            Self::NeedsAttention => "Needs attention",
        }
    }
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncFailureKind {
    Authentication,
    Certificate,
    Offline,
    RemoteChanged,
    DeviceApproval,
    InvalidRemoteData,
    LocalStateChanged,
    Storage,
}

impl SyncFailureKind {
    pub fn recovery_action(self) -> &'static str {
        match self {
            Self::Authentication => "Update password",
            Self::Certificate => "Choose CA file",
            Self::Offline => "Retry",
            Self::RemoteChanged => "View merge result",
            Self::DeviceApproval => "Review device",
            Self::InvalidRemoteData => "Review sync audit",
            Self::LocalStateChanged => "Retry",
            Self::Storage => "Review sync audit",
        }
    }

    fn health_state(self) -> SyncHealthState {
        match self {
            Self::Offline => SyncHealthState::OfflineWillRetry,
            _ => SyncHealthState::NeedsAttention,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncHealth {
    pub state: SyncHealthState,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<SyncFailureKind>,
}

impl Default for SyncHealth {
    fn default() -> Self {
        Self {
            state: SyncHealthState::Off,
            revision: 0,
            last_attempt_unix: None,
            last_success_unix: None,
            failure: None,
        }
    }
}

impl SyncHealth {
    pub fn syncing(&self, now_unix: i64) -> Self {
        let mut next = self.clone();
        next.state = SyncHealthState::Syncing;
        next.last_attempt_unix = Some(now_unix);
        next.failure = None;
        next
    }

    pub fn succeeded(&self, now_unix: i64) -> Self {
        let mut next = self.clone();
        next.state = SyncHealthState::UpToDate;
        next.last_attempt_unix = Some(now_unix);
        next.last_success_unix = Some(now_unix);
        next.failure = None;
        next
    }

    pub fn failed(&self, now_unix: i64, failure: SyncFailureKind) -> Self {
        let mut next = self.clone();
        next.state = failure.health_state();
        next.last_attempt_unix = Some(now_unix);
        next.failure = Some(failure);
        next
    }
}

pub struct SyncHealthStore {
    path: PathBuf,
}

impl SyncHealthStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, VaultError> {
        let path = path.into();
        validate_store_path(&path)?;
        Ok(Self { path })
    }

    pub fn load(&self, configured: bool) -> Result<SyncHealth, VaultError> {
        let bytes =
            match crate::util::safe_fs::read_owner_only_limited(&self.path, MAX_HEALTH_BYTES) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(if configured {
                        SyncHealth {
                            state: SyncHealthState::NeedsAttention,
                            ..SyncHealth::default()
                        }
                    } else {
                        SyncHealth::default()
                    });
                }
                Err(_) => return Err(VaultError::StorageFailed),
            };
        let disk: DiskHealth =
            serde_json::from_slice(&bytes).map_err(|_| VaultError::StorageFailed)?;
        disk.into_health(configured)
    }

    pub fn save(
        &self,
        current: &SyncHealth,
        candidate: SyncHealth,
    ) -> Result<SyncHealth, VaultError> {
        if candidate.revision != current.revision {
            return Err(VaultError::RevisionConflict);
        }
        let observed = self.load(current.state != SyncHealthState::Off)?;
        if observed.revision != current.revision {
            return Err(VaultError::RevisionConflict);
        }
        let mut candidate = candidate;
        candidate.revision = candidate
            .revision
            .checked_add(1)
            .ok_or(VaultError::RevisionConflict)?;
        let disk = DiskHealth::from_health(&candidate);
        let bytes = serde_json::to_vec(&disk).map_err(|_| VaultError::SerializationFailed)?;
        if bytes.len() as u64 > MAX_HEALTH_BYTES {
            return Err(VaultError::PayloadTooLarge);
        }
        crate::util::safe_fs::write_owner_only_atomic(&self.path, &bytes)
            .map_err(|_| VaultError::StorageFailed)?;
        Ok(candidate)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncAuditAction {
    Setup,
    ManualSync,
    PairCreate,
    PairJoin,
    RevokeDevice,
    RecoveryExport,
}

impl SyncAuditAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Setup => "Personal sync was set up",
            Self::ManualSync => "Personal state was synced",
            Self::PairCreate => "A device connection was started",
            Self::PairJoin => "This device joined personal sync",
            Self::RevokeDevice => "A device was removed",
            Self::RecoveryExport => "A recovery kit was saved",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncAuditOutcome {
    Succeeded,
    NoChanges,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncAuditEntry {
    pub id: String,
    pub at_unix: i64,
    pub action: SyncAuditAction,
    pub outcome: SyncAuditOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default)]
    pub local_operations: usize,
    #[serde(default)]
    pub remote_operations: usize,
    #[serde(default)]
    pub duplicate_operations: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<SyncFailureKind>,
}

impl SyncAuditEntry {
    pub fn new(
        at_unix: i64,
        action: SyncAuditAction,
        outcome: SyncAuditOutcome,
    ) -> Result<Self, VaultError> {
        Ok(Self {
            id: super::crypto::random_id_hex::<16>()?,
            at_unix,
            action,
            outcome,
            device_id: None,
            local_operations: 0,
            remote_operations: 0,
            duplicate_operations: 0,
            failure: None,
        })
    }

    pub fn summary(&self) -> String {
        match (self.outcome, self.failure) {
            (SyncAuditOutcome::Succeeded, _) => format!(
                "{}: {} local and {} remote change(s) merged.",
                self.action.label(),
                self.local_operations,
                self.remote_operations
            ),
            (SyncAuditOutcome::NoChanges, _) => {
                format!("{}: no changes were needed.", self.action.label())
            }
            (SyncAuditOutcome::Failed, Some(failure)) => format!(
                "{}: needs attention — {}.",
                self.action.label(),
                failure.recovery_action()
            ),
            (SyncAuditOutcome::Failed, None) => {
                format!("{}: needs attention.", self.action.label())
            }
        }
    }
}

#[derive(Default)]
pub struct SyncAuditLog {
    entries: Vec<SyncAuditEntry>,
}

impl SyncAuditLog {
    pub fn entries(&self) -> &[SyncAuditEntry] {
        &self.entries
    }
}

pub struct SyncAuditStore {
    path: PathBuf,
}

impl SyncAuditStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, VaultError> {
        let path = path.into();
        validate_store_path(&path)?;
        Ok(Self { path })
    }

    pub fn load(&self, now_unix: i64) -> Result<SyncAuditLog, VaultError> {
        let bytes = match crate::util::safe_fs::read_owner_only_limited(&self.path, MAX_AUDIT_BYTES)
        {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SyncAuditLog::default());
            }
            Err(_) => return Err(VaultError::StorageFailed),
        };
        let disk: DiskAudit =
            serde_json::from_slice(&bytes).map_err(|_| VaultError::StorageFailed)?;
        disk.into_log(now_unix)
    }

    pub fn append(&self, now_unix: i64, entry: SyncAuditEntry) -> Result<SyncAuditLog, VaultError> {
        validate_entry(&entry)?;
        let mut log = self.load(now_unix)?;
        if log.entries.iter().any(|current| current.id == entry.id) {
            return Ok(log);
        }
        log.entries.push(entry);
        prune_entries(&mut log.entries, now_unix);
        let disk = DiskAudit {
            kind: AUDIT_KIND.to_owned(),
            schema_version: LOCAL_SCHEMA_VERSION,
            entries: log.entries.clone(),
        };
        let bytes = serde_json::to_vec(&disk).map_err(|_| VaultError::SerializationFailed)?;
        if bytes.len() as u64 > MAX_AUDIT_BYTES {
            return Err(VaultError::PayloadTooLarge);
        }
        crate::util::safe_fs::write_owner_only_atomic(&self.path, &bytes)
            .map_err(|_| VaultError::StorageFailed)?;
        Ok(log)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskHealth {
    kind: String,
    schema_version: u32,
    revision: u64,
    state: SyncHealthState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_attempt_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_success_unix: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<SyncFailureKind>,
}

impl DiskHealth {
    fn from_health(health: &SyncHealth) -> Self {
        Self {
            kind: HEALTH_KIND.to_owned(),
            schema_version: LOCAL_SCHEMA_VERSION,
            revision: health.revision,
            state: health.state,
            last_attempt_unix: health.last_attempt_unix,
            last_success_unix: health.last_success_unix,
            failure: health.failure,
        }
    }

    fn into_health(self, configured: bool) -> Result<SyncHealth, VaultError> {
        if self.kind != HEALTH_KIND
            || self.schema_version != LOCAL_SCHEMA_VERSION
            || (!configured && self.state != SyncHealthState::Off)
        {
            return Err(VaultError::StorageFailed);
        }
        Ok(SyncHealth {
            state: if configured && self.state == SyncHealthState::Off {
                SyncHealthState::NeedsAttention
            } else {
                self.state
            },
            revision: self.revision,
            last_attempt_unix: self.last_attempt_unix,
            last_success_unix: self.last_success_unix,
            failure: self.failure,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskAudit {
    kind: String,
    schema_version: u32,
    #[serde(default)]
    entries: Vec<SyncAuditEntry>,
}

impl DiskAudit {
    fn into_log(self, now_unix: i64) -> Result<SyncAuditLog, VaultError> {
        if self.kind != AUDIT_KIND
            || self.schema_version != LOCAL_SCHEMA_VERSION
            || self.entries.len() > MAX_AUDIT_ENTRIES
        {
            return Err(VaultError::StorageFailed);
        }
        for entry in &self.entries {
            validate_entry(entry)?;
        }
        let mut entries = self.entries;
        prune_entries(&mut entries, now_unix);
        Ok(SyncAuditLog { entries })
    }
}

fn prune_entries(entries: &mut Vec<SyncAuditEntry>, now_unix: i64) {
    let cutoff = now_unix.saturating_sub(AUDIT_RETENTION_SECONDS);
    entries
        .retain(|entry| entry.at_unix >= cutoff && entry.at_unix <= now_unix.saturating_add(300));
    entries.sort_by(|left, right| {
        left.at_unix
            .cmp(&right.at_unix)
            .then_with(|| left.id.cmp(&right.id))
    });
    if entries.len() > MAX_AUDIT_ENTRIES {
        entries.drain(..entries.len() - MAX_AUDIT_ENTRIES);
    }
}

fn validate_entry(entry: &SyncAuditEntry) -> Result<(), VaultError> {
    if entry.id.len() != 32
        || !entry.id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || entry.at_unix < 0
        || entry
            .device_id
            .as_deref()
            .is_some_and(|id| crate::personal_state::DeviceId::new(id).is_err())
        || (entry.outcome == SyncAuditOutcome::Failed) != entry.failure.is_some()
    {
        return Err(VaultError::StorageFailed);
    }
    Ok(())
}

fn validate_store_path(path: &Path) -> Result<(), VaultError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(VaultError::StorageFailed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let suffix = super::super::crypto::random_id_hex::<8>().unwrap();
        let root = std::env::temp_dir().join(format!(
            "yututui-sync-local-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn health_exposes_only_five_public_states_and_one_recovery_action() {
        let health = SyncHealth::default();
        assert_eq!(health.state.label(), "Off");
        let syncing = health.syncing(10);
        assert_eq!(syncing.state.label(), "Syncing");
        let offline = syncing.failed(11, SyncFailureKind::Offline);
        assert_eq!(offline.state.label(), "Offline — will retry");
        assert_eq!(offline.failure.unwrap().recovery_action(), "Retry");
        let attention = offline.failed(12, SyncFailureKind::Authentication);
        assert_eq!(attention.state.label(), "Needs attention");
        assert_eq!(
            attention.failure.unwrap().recovery_action(),
            "Update password"
        );
        assert_eq!(attention.succeeded(13).state.label(), "Up to date");
    }

    #[test]
    fn health_round_trip_is_revisioned_and_contains_no_remote_detail() {
        let root = temp_dir("health");
        let path = root.join("health.json");
        let store = SyncHealthStore::new(path.clone()).unwrap();
        let initial = store.load(true).unwrap();
        let saved = store.save(&initial, initial.succeeded(100)).unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(store.load(true).unwrap(), saved);
        let raw = String::from_utf8(fs::read(path).unwrap()).unwrap();
        assert!(!raw.contains("http"));
        assert!(!raw.contains("password"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn audit_is_bounded_deduplicated_and_redacted_by_construction() {
        let root = temp_dir("audit");
        let path = root.join("audit.json");
        let store = SyncAuditStore::new(path.clone()).unwrap();
        let mut entry = SyncAuditEntry::new(
            1_000_000,
            SyncAuditAction::ManualSync,
            SyncAuditOutcome::Succeeded,
        )
        .unwrap();
        entry.local_operations = 2;
        entry.remote_operations = 3;
        let id = entry.id.clone();
        store.append(1_000_000, entry.clone()).unwrap();
        let log = store.append(1_000_000, entry).unwrap();
        assert_eq!(log.entries().len(), 1);
        assert_eq!(log.entries()[0].id, id);
        assert!(log.entries()[0].summary().contains("2 local and 3 remote"));
        let raw = String::from_utf8(fs::read(path).unwrap()).unwrap();
        for forbidden in ["http://", "https://", "/home/", "password", "token"] {
            assert!(!raw.contains(forbidden), "{forbidden}");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn old_audit_entries_are_pruned() {
        let root = temp_dir("prune");
        let store = SyncAuditStore::new(root.join("audit.json")).unwrap();
        let old =
            SyncAuditEntry::new(1, SyncAuditAction::Setup, SyncAuditOutcome::NoChanges).unwrap();
        store.append(AUDIT_RETENTION_SECONDS + 100, old).unwrap();
        assert!(
            store
                .load(AUDIT_RETENTION_SECONDS + 100)
                .unwrap()
                .entries()
                .is_empty()
        );
        let _ = fs::remove_dir_all(root);
    }
}
