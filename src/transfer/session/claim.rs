use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::bail;

use super::*;

/// Atomically admit or resume a download for the row's current selected video.
pub(crate) fn claim_import_download(
    session_id: &str,
    source_order: u32,
    expected_key: &str,
) -> anyhow::Result<ImportDownloadClaim> {
    claim_import_download_with_save(session_id, source_order, expected_key, save_updated_session)
}

fn claim_import_download_with_save<F>(
    session_id: &str,
    source_order: u32,
    expected_key: &str,
    save: F,
) -> anyhow::Result<ImportDownloadClaim>
where
    F: FnOnce(ImportSession) -> anyhow::Result<()>,
{
    let _guard = ImportRecordGuard::try_acquire(session_id)?;
    let mut session = ImportSession::load(session_id)?;
    if session.session_instance_id.is_empty() {
        session.session_instance_id = new_durable_id();
    }
    let session_instance_id = session.session_instance_id.clone();
    let row = row_by_source_order_mut(&mut session, source_order)?;
    if row_has_artifact_ownership(row) {
        bail!("import row {source_order} is already written");
    }
    if !matches!(row.status, ImportSessionRowStatus::Matched)
        || row.selected_key.as_deref() != Some(expected_key)
        || matches!(
            row.review_decision,
            Some(ReviewDecision::Rejected | ReviewDecision::Skipped)
        )
    {
        bail!("import row {source_order} no longer selects download target `{expected_key}`");
    }
    row.revision = row.revision.max(1);
    if let Some(existing) = row.download_claim.as_ref() {
        if existing.session_id == session_id
            && existing.session_instance_id == session_instance_id
            && existing.row_id == row.row_id
            && existing.source_order == source_order
            && existing.row_revision == row.revision
            && existing.expected_key == expected_key
        {
            // A stopped process may not have emitted a terminal event. Reuse the exact claim so
            // the retry can reconcile its incoming file/transaction. Live duplicates are
            // coalesced by download ownership admission.
            return Ok(existing.clone());
        }
        bail!("import row {source_order} has a conflicting active download claim");
    }
    let claim = ImportDownloadClaim {
        session_id: session_id.to_owned(),
        session_instance_id,
        row_id: row.row_id.clone(),
        source_order,
        row_revision: row.revision,
        claim_id: new_durable_id(),
        expected_key: expected_key.to_owned(),
    };
    // Legacy LocalPlaylist jobs may have set checkpoint/session `written` after adding the
    // remote selection to the Library playlist. That is not a local audio artifact: admission
    // establishes the session row's artifact lifecycle without changing checkpoint idempotency.
    row.written = false;
    row.download_claim = Some(claim.clone());
    match save(session) {
        Ok(()) => Ok(claim),
        Err(save_error) => {
            match ImportSession::load(session_id) {
                Ok(reloaded)
                    if reloaded.session_instance_id == claim.session_instance_id
                        && reloaded.rows.iter().any(|row| {
                            row.source_order == claim.source_order
                                && row.row_id == claim.row_id
                                && row.download_claim.as_ref() == Some(&claim)
                        }) =>
                {
                    Ok(claim)
                }
                Ok(_) => Err(save_error
                    .context("download claim was not published; admission is safe to retry")),
                Err(reload_error) => Err(anyhow::anyhow!(
                    "download claim ownership is uncertain after save failure: {save_error:#}; reload failed: {reload_error:#}"
                )),
            }
        }
    }
}

pub(crate) fn validate_import_download_claim_unlocked(
    claim: &ImportDownloadClaim,
    allow_committed: bool,
) -> anyhow::Result<()> {
    let session = ImportSession::load(&claim.session_id)?;
    if session.session_instance_id != claim.session_instance_id {
        bail!("import session `{}` was replaced", claim.session_id);
    }
    let row = session
        .rows
        .iter()
        .find(|row| row.source_order == claim.source_order && row.row_id == claim.row_id)
        .ok_or_else(|| anyhow::anyhow!("import row identity changed"))?;
    if row.revision != claim.row_revision
        || row.selected_key.as_deref() != Some(&claim.expected_key)
    {
        bail!("import row selection changed after download admission");
    }
    if row.download_claim.as_ref() == Some(claim) {
        return Ok(());
    }
    if allow_committed
        && row.written
        && row
            .artifact_receipt
            .as_ref()
            .and_then(|receipt| receipt.claim.as_ref())
            == Some(claim)
    {
        return Ok(());
    }
    bail!("import download claim is no longer active")
}

/// Review surfaces call this while holding the record guard, before changing the checkpoint.
pub(crate) fn ensure_review_row_mutable_unlocked(
    session_id: &str,
    source_order: u32,
) -> anyhow::Result<()> {
    if !session_path(session_id).is_some_and(|path| path.exists()) {
        return Ok(());
    }
    let session = ImportSession::load(session_id)?;
    let row = session
        .rows
        .iter()
        .find(|row| row.source_order == source_order)
        .ok_or_else(|| anyhow::anyhow!("import row {source_order} is missing"))?;
    if row.written {
        bail!("row {source_order} is already written; review cannot change it");
    }
    if row.download_claim.is_some() {
        bail!("row {source_order} has an active download; review cannot change it");
    }
    Ok(())
}

/// Bulk Ready changes are checkpoint-owned and independent of playlist-write/download state.
/// They still must not retarget a row while an admitted download owns that selection.
pub(crate) fn ensure_ready_row_mutable_unlocked(
    session_id: &str,
    source_order: u32,
) -> anyhow::Result<()> {
    if !session_path(session_id).is_some_and(|path| path.exists()) {
        return Ok(());
    }
    let session = ImportSession::load(session_id)?;
    let row = session
        .rows
        .iter()
        .find(|row| row.source_order == source_order)
        .ok_or_else(|| anyhow::anyhow!("import row {source_order} is missing"))?;
    if row.download_claim.is_some() {
        bail!("row {source_order} has an active download; Ready selection cannot change it");
    }
    if row_has_artifact_ownership(row) {
        bail!("row {source_order} already owns a local artifact; Ready selection cannot change it");
    }
    Ok(())
}

pub(crate) fn row_has_artifact_ownership(row: &ImportSessionRow) -> bool {
    row.local_path.is_some() || row.artifact_receipt.is_some()
}

pub(super) fn new_durable_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        use sha2::{Digest as _, Sha256};

        static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut digest = Sha256::new();
        digest.update(std::process::id().to_le_bytes());
        digest.update(now.to_le_bytes());
        digest.update(counter.to_le_bytes());
        let len = bytes.len();
        bytes.copy_from_slice(&digest.finalize()[..len]);
    }
    let mut id = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut id, "{byte:02x}");
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save_claimable(session_id: &str, instance: &str, key: &str) {
        ImportSession {
            session_id: session_id.to_owned(),
            session_instance_id: instance.to_owned(),
            rows: vec![ImportSessionRow {
                row_id: "row-00001".to_owned(),
                source_order: 1,
                status: ImportSessionRowStatus::Matched,
                title: "Claimed".to_owned(),
                artists: vec!["Artist".to_owned()],
                source_key: "spotify:track:claimed".to_owned(),
                selected_key: Some(key.to_owned()),
                ..ImportSessionRow::default()
            }],
            ..ImportSession::default()
        }
        .save()
        .expect("save claim fixture");
    }

    #[test]
    fn stopped_admission_reuses_the_exact_durable_claim() {
        let session_id = "sp2yt-claim-reuse";
        save_claimable(session_id, "claim-reuse-instance", "video-a");

        let first = claim_import_download(session_id, 1, "video-a").unwrap();
        let resumed = claim_import_download(session_id, 1, "video-a").unwrap();

        assert_eq!(resumed, first);
        assert_eq!(
            ImportSession::load(session_id).unwrap().rows[0]
                .download_claim
                .as_ref(),
            Some(&first)
        );
        record_import_download_error(&first, "cancelled before worker start").unwrap();
        ImportSession::delete_record(session_id).unwrap();
    }

    #[test]
    fn post_publish_save_error_reloads_visible_claim_as_accepted() {
        let session_id = "sp2yt-claim-post-publish-error";
        save_claimable(session_id, "claim-post-publish-instance", "video-a");

        let claim = claim_import_download_with_save(session_id, 1, "video-a", |session| {
            save_updated_session(session)?;
            Err(anyhow::anyhow!("injected durability acknowledgement loss"))
        })
        .expect("visible exact claim remains an accepted admission");

        let saved = ImportSession::load(session_id).unwrap();
        assert_eq!(saved.rows[0].download_claim.as_ref(), Some(&claim));
        record_import_download_error(&claim, "not started").unwrap();
        ImportSession::delete_record(session_id).unwrap();
    }

    #[test]
    fn stale_terminal_cannot_touch_reselected_or_rejected_row() {
        let session_id = "sp2yt-claim-stale-terminal";
        save_claimable(session_id, "claim-stale-instance", "video-a");
        let claim_a = claim_import_download(session_id, 1, "video-a").unwrap();
        assert!(record_import_download_error(&claim_a, "attempt A failed").unwrap());

        let mut session = ImportSession::load(session_id).unwrap();
        session.rows[0].selected_key = Some("video-b".to_owned());
        session.rows[0].revision += 1;
        session.rows[0].errors.clear();
        session.save().unwrap();
        let claim_b = claim_import_download(session_id, 1, "video-b").unwrap();

        assert!(!record_import_download_error(&claim_a, "late A failure").unwrap());
        let selected_b = ImportSession::load(session_id).unwrap();
        assert_eq!(selected_b.rows[0].download_claim.as_ref(), Some(&claim_b));
        assert!(selected_b.rows[0].errors.is_empty());

        assert!(record_import_download_error(&claim_b, "B stopped").unwrap());
        let mut rejected = ImportSession::load(session_id).unwrap();
        rejected.rows[0].review_decision = Some(ReviewDecision::Rejected);
        rejected.rows[0].revision += 1;
        rejected.rows[0].errors.clear();
        rejected.save().unwrap();
        assert!(!record_import_download_error(&claim_a, "late A after reject").unwrap());
        let rejected = ImportSession::load(session_id).unwrap();
        assert!(matches!(
            rejected.rows[0].review_decision,
            Some(ReviewDecision::Rejected)
        ));
        assert!(rejected.rows[0].errors.is_empty());
        ImportSession::delete_record(session_id).unwrap();
    }

    #[test]
    fn active_claim_blocks_delete_and_old_incarnation_is_fenced_after_recreate() {
        let session_id = "sp2yt-claim-aba";
        save_claimable(session_id, "claim-aba-old", "video-a");
        let old = claim_import_download(session_id, 1, "video-a").unwrap();
        assert_eq!(
            ImportSession::delete_record(session_id).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );

        // Simulate an out-of-band old-shape replacement using the same human job id and row id.
        save_claimable(session_id, "claim-aba-new", "video-a");
        assert!(!record_import_download_error(&old, "old incarnation terminal").unwrap());
        assert!(validate_import_download_claim_unlocked(&old, false).is_err());
        let recreated = ImportSession::load(session_id).unwrap();
        assert_eq!(recreated.session_instance_id, "claim-aba-new");
        assert!(recreated.rows[0].errors.is_empty());
        ImportSession::delete_record(session_id).unwrap();
    }

    #[test]
    fn dropping_record_guard_unlocks_a_duplicated_file_description() {
        let session_id = "sp2yt-claim-duplicated-lock-description";
        let guard = ImportRecordGuard::try_acquire(session_id).expect("hold record lock");
        let duplicated = guard
            ._file
            .try_clone()
            .expect("duplicate the locked file description");

        drop(guard);
        let reacquired = ImportRecordGuard::try_acquire(session_id)
            .expect("guard drop must explicitly release the lock before closing its handle");

        drop(reacquired);
        drop(duplicated);
    }
}
