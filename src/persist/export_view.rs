//! What a reader outside the persist actor must honour before trusting a store snapshot.
//!
//! The persist actor journals an intent before it rewrites a store, so a snapshot on disk can be
//! one write behind an authoritative sidecar. Readers that skip the journal see stale data;
//! readers that reimplement it drift. `data_export` reimplemented it and drifted: it required
//! every record to be `op: "replace"` with a mandatory `sidecar` field named
//! `<store>.intent.latest.json`, while the actor writes generation-ordered records whose
//! steady-state newest entry is `op: "commit"` (no sidecar) and whose replace records name a
//! unique per-order sidecar. Every profile the app had actually used therefore failed to export
//! with `missing field 'sidecar'`.
//!
//! This is the one crate-internal seam for that question, resolved exactly as the app's own
//! recovery replay resolves it.

use std::path::{Path, PathBuf};

use super::{
    INTENT_JOURNAL_MAX_BYTES, JournalOperation, StoreKind, intent_journal_path, read_journal_state,
    sibling_path_from_record,
};

/// The pending persistence intent for one store, resolved against the journal's commit frontier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingStoreIntent {
    /// Nothing is pending; the store's own snapshot file is authoritative.
    Committed,
    /// A sidecar holds bytes newer than the snapshot. `sha256` must match before they are used.
    Replace { sidecar: PathBuf, sha256: String },
    /// The store is pending deletion; readers must treat it as empty.
    Delete,
    /// The journal has content but not one record this store can interpret. The app repairs by
    /// ignoring such a journal; a fail-closed reader such as an export must refuse the store.
    Unreadable,
}

/// Resolve the pending intent for `path`'s store.
///
/// This is a pure read: an export must never write to the profile it is backing up, so no lock
/// file is created and no journal is repaired. Callers that then read the named sidecar must
/// resolve again afterwards and compare — an unchanged answer proves the actor did not write
/// underneath them, and a changed one is reported instead of exporting a torn view.
pub(crate) fn pending_store_intent(
    kind: StoreKind,
    path: &Path,
) -> std::io::Result<PendingStoreIntent> {
    let state = read_journal_state(kind, path)?;
    let Some(candidate) = state.candidate else {
        if state.committed_through.is_none() && journal_has_content(path)? {
            return Ok(PendingStoreIntent::Unreadable);
        }
        return Ok(PendingStoreIntent::Committed);
    };
    match candidate.operation {
        JournalOperation::Delete => Ok(PendingStoreIntent::Delete),
        JournalOperation::Replace { sidecar, sha256 } => {
            let sidecar = sibling_path_from_record(path, &sidecar).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} journal names an invalid sidecar", kind.label()),
                )
            })?;
            Ok(PendingStoreIntent::Replace { sidecar, sha256 })
        }
    }
}

/// Whether the journal file holds at least one non-blank line.
fn journal_has_content(path: &Path) -> std::io::Result<bool> {
    let Some(journal_path) = intent_journal_path(path) else {
        return Ok(false);
    };
    match crate::util::safe_fs::read_no_symlink_limited(&journal_path, INTENT_JOURNAL_MAX_BYTES) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes)
            .lines()
            .any(|line| !line.trim().is_empty())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
