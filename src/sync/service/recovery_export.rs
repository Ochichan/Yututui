use std::path::{Path, PathBuf};

use zeroize::Zeroizing;

use crate::personal_state::PersonalStateV2;

use super::super::{
    PrivateStore, RecoveryKit, SyncAuditAction, SyncAuditEntry, SyncAuditOutcome, SyncAuditStore,
    SyncPaths,
};
use super::SyncServiceError;

const RECOVERY_KIT_MAX_BYTES: u64 = 64 * 1024;
const RECOVERY_NAME: &str = "yututui-recovery-kit.json";
const RECOVERY_COPY_LIMIT: usize = 10_000;

#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryExportResult {
    pub checksum: String,
}

/// Verify an existing recovery kit and make a no-overwrite owner-only copy.
///
/// Paths never enter the return value, audit entry, or error. The caller may retain its own
/// transient field while the wizard is open, but status/toast layers only need the checksum.
pub fn export_recovery_kit(
    state: &PersonalStateV2,
    paths: &SyncPaths,
    source: &Path,
    destination_directory: &Path,
) -> Result<RecoveryExportResult, SyncServiceError> {
    let source_bytes = Zeroizing::new(
        crate::util::safe_fs::read_no_symlink_limited(source, RECOVERY_KIT_MAX_BYTES)
            .map_err(|_| SyncServiceError::Storage)?,
    );
    if source_bytes.is_empty() {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    let kit =
        RecoveryKit::from_json(&source_bytes).map_err(|_| SyncServiceError::InvalidRemoteData)?;
    if kit.dataset_id() != state.dataset_id {
        return Err(SyncServiceError::InvalidRemoteData);
    }

    let private = PrivateStore::new(paths.private_store())?.load()?;
    let recovery_verifying_key = kit
        .recovery_verifying_key()
        .map_err(|_| SyncServiceError::InvalidRemoteData)?;
    if private.dataset_id() != state.dataset_id
        || private.recovery_recipient() != Some(kit.recovery_recipient().as_str())
        || private.recovery_verifying_key() != Some(recovery_verifying_key.as_str())
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }

    let destination = unused_recovery_path(destination_directory)?;
    let checksum = kit
        .export_confirmed(&destination)
        .map_err(|_| SyncServiceError::Storage)?;
    let now = crate::signals::unix_now();
    if let Ok(entry) = SyncAuditEntry::new(
        now,
        SyncAuditAction::RecoveryExport,
        SyncAuditOutcome::Succeeded,
    ) {
        let _ = SyncAuditStore::new(paths.audit()).and_then(|store| store.append(now, entry));
    }
    Ok(RecoveryExportResult { checksum })
}

fn unused_recovery_path(directory: &Path) -> Result<PathBuf, SyncServiceError> {
    let metadata = std::fs::symlink_metadata(directory).map_err(|_| SyncServiceError::Storage)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(SyncServiceError::Storage);
    }
    let directory = std::fs::canonicalize(directory).map_err(|_| SyncServiceError::Storage)?;
    for index in 0..RECOVERY_COPY_LIMIT {
        let name = if index == 0 {
            RECOVERY_NAME.to_owned()
        } else {
            format!("yututui-recovery-kit-{index}.json")
        };
        let candidate = directory.join(name);
        match std::fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => {}
            Err(_) => return Err(SyncServiceError::Storage),
        }
    }
    Err(SyncServiceError::Storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_names_are_bounded_and_never_overwrite() {
        let root = std::env::temp_dir().join(format!(
            "yututui-recovery-export-name-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let first = unused_recovery_path(&root).unwrap();
        assert_eq!(first.file_name().unwrap(), RECOVERY_NAME);
        std::fs::write(&first, b"occupied").unwrap();
        let second = unused_recovery_path(&root).unwrap();
        assert_eq!(second.file_name().unwrap(), "yututui-recovery-kit-1.json");
        std::fs::remove_dir_all(root).unwrap();
    }
}
