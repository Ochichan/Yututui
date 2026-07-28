//! Durable, idempotent accounting for retention loss and retired server scopes.

use std::path::Path;

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::QueueDropSummary;
use crate::util::safe_fs;

const AUDIT_SCHEMA_VERSION: u8 = 1;
const AUDIT_READ_MAX: u64 = 256 * 1024;
const AUDIT_BATCH_CAP: usize = 128;

#[derive(Debug, Default, Serialize, Deserialize)]
struct QueueDropAudit {
    schema_version: u8,
    generic_dropped: u64,
    open_subsonic_dropped: u64,
    #[serde(default)]
    scope_retired: u64,
    recent_batches: Vec<QueueDropBatch>,
}

#[derive(Debug, Serialize, Deserialize)]
struct QueueDropBatch {
    batch_id: String,
    observed_at_unix: i64,
    affected_entries: u64,
    generic_dropped: u64,
    open_subsonic_dropped: u64,
    #[serde(default)]
    scope_retired: u64,
}

pub(super) fn record(
    queue_path: &Path,
    summary: &QueueDropSummary,
    observed_at_unix: i64,
) -> std::io::Result<()> {
    if summary.is_empty() {
        return Ok(());
    }
    let path = audit_path(queue_path);
    let mut audit = load(&path)?;
    let batch_id = batch_id(summary);
    if audit
        .recent_batches
        .iter()
        .any(|batch| batch.batch_id == batch_id)
    {
        return Ok(());
    }

    let generic_dropped = summary.generic_dropped() as u64;
    let open_subsonic_dropped = summary.open_subsonic_dropped() as u64;
    let scope_retired = summary.scope_retired() as u64;
    audit.schema_version = AUDIT_SCHEMA_VERSION;
    audit.generic_dropped = audit.generic_dropped.saturating_add(generic_dropped);
    audit.open_subsonic_dropped = audit
        .open_subsonic_dropped
        .saturating_add(open_subsonic_dropped);
    audit.scope_retired = audit.scope_retired.saturating_add(scope_retired);
    audit.recent_batches.push(QueueDropBatch {
        batch_id,
        observed_at_unix,
        affected_entries: summary.affected_entries() as u64,
        generic_dropped,
        open_subsonic_dropped,
        scope_retired,
    });
    if audit.recent_batches.len() > AUDIT_BATCH_CAP {
        let excess = audit.recent_batches.len() - AUDIT_BATCH_CAP;
        audit.recent_batches.drain(..excess);
    }
    safe_fs::write_private_atomic_json(&path, &audit)
}

fn load(path: &Path) -> std::io::Result<QueueDropAudit> {
    let bytes = match safe_fs::read_no_symlink_limited(path, AUDIT_READ_MAX) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(QueueDropAudit::default());
        }
        Err(error) => return Err(error),
    };
    let audit: QueueDropAudit = serde_json::from_slice(&bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid scrobble queue drop audit: {error}"),
        )
    })?;
    if audit.schema_version != AUDIT_SCHEMA_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported scrobble queue drop audit schema",
        ));
    }
    Ok(audit)
}

fn batch_id(summary: &QueueDropSummary) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yututui-scrobble-queue-drop-v1\0");
    for (entry_id, kinds) in &summary.records {
        digest.update((entry_id.len() as u64).to_be_bytes());
        digest.update(entry_id.as_bytes());
        digest.update([
            u8::from(kinds.generic),
            u8::from(kinds.open_subsonic),
            u8::from(kinds.scope_retired),
        ]);
    }
    HEXLOWER.encode(&digest.finalize())
}

fn audit_path(queue_path: &Path) -> std::path::PathBuf {
    queue_path.with_extension("drops.json")
}

#[cfg(test)]
pub(super) fn counts(queue_path: &Path) -> std::io::Result<(u64, u64, usize)> {
    let audit = load(&audit_path(queue_path))?;
    Ok((
        audit.generic_dropped,
        audit.open_subsonic_dropped,
        audit.recent_batches.len(),
    ))
}

#[cfg(test)]
pub(super) fn scope_retired_count(queue_path: &Path) -> std::io::Result<u64> {
    Ok(load(&audit_path(queue_path))?.scope_retired)
}
