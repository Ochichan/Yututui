use std::path::Path;

use anyhow::bail;

use super::{
    ImportRecordGuard, ImportSession, ImportSessionRow, row_by_source_order_mut,
    save_updated_session,
};
use crate::transfer::artifact_identity::{ArtifactReceipt, ImportDownloadClaim};

/// Persist the metadata side of a crash-recoverable artifact move while the caller owns the
/// session record lock. Stable row identity prevents a stale transaction from updating a reused
/// source-order slot.
pub(crate) fn record_artifact_move_done_unlocked(
    session_id: &str,
    row_id: &str,
    source_order: u32,
    paths: (&Path, &Path),
    allow_missing_source_path: bool,
    expected_claim: Option<&ImportDownloadClaim>,
    receipt: ArtifactReceipt,
) -> anyhow::Result<()> {
    let (from, to) = paths;
    let mut session = ImportSession::load(session_id)?;
    if let Some(claim) = expected_claim
        && (claim.session_id != session_id
            || session.session_instance_id != claim.session_instance_id)
    {
        bail!("import session `{session_id}` was replaced before artifact commit");
    }
    let row = row_by_source_order_mut(&mut session, source_order)?;
    if row.row_id != row_id {
        bail!("import session `{session_id}` row identity changed at source order {source_order}");
    }
    match row.local_path.as_deref() {
        Some(current) if current == to || current == from => {}
        None if allow_missing_source_path => {}
        Some(current) => bail!(
            "import session `{session_id}` row {row_id} points to conflicting path {}",
            current.display()
        ),
        None => bail!("import session `{session_id}` row {row_id} lost its organize source path"),
    }
    if let Some(claim) = expected_claim
        && row.download_claim.is_none()
        && row.written
        && row.local_path.as_deref() == Some(to)
        && row.artifact_receipt.as_ref() == Some(&receipt)
        && receipt.claim.as_ref() == Some(claim)
    {
        // The authoritative row committed before the transaction phase acknowledgement. The
        // exact receipt is the durable proof needed to finish removing the stale transaction.
        return Ok(());
    }
    match expected_claim {
        Some(claim)
            if row.revision == claim.row_revision
                && row.selected_key.as_deref() == Some(&claim.expected_key)
                && row.download_claim.as_ref() == Some(claim)
                && receipt.claim.as_ref() == Some(claim) => {}
        Some(_) => bail!("import download claim changed before artifact commit"),
        None if row.download_claim.is_some() => {
            bail!("cannot organize an artifact while its download claim is active")
        }
        None => {}
    }
    row.written = true;
    row.local_path = Some(to.to_path_buf());
    row.artifact_receipt = Some(receipt);
    if expected_claim.is_some() {
        row.download_claim = None;
    }
    row.errors.clear();
    save_updated_session(session)
}

pub(crate) fn clear_missing_artifact_unlocked(
    session_id: &str,
    row_id: &str,
    source_order: u32,
    expected_path: &Path,
) -> anyhow::Result<()> {
    let mut session = ImportSession::load(session_id)?;
    let row = row_by_source_order_mut(&mut session, source_order)?;
    if row.row_id != row_id {
        bail!("import session `{session_id}` row identity changed at source order {source_order}");
    }
    if !row.written || row.local_path.as_deref() != Some(expected_path) {
        bail!("import session `{session_id}` row {row_id} changed before missing-artifact repair");
    }
    row.written = false;
    row.local_path = None;
    row.artifact_receipt = None;
    save_updated_session(session)
}

pub(crate) fn promote_artifact_receipt_unlocked(
    session_id: &str,
    row_id: &str,
    source_order: u32,
    expected_path: &Path,
    receipt: ArtifactReceipt,
) -> anyhow::Result<()> {
    let mut session = ImportSession::load(session_id)?;
    let row = row_by_source_order_mut(&mut session, source_order)?;
    if row.row_id != row_id
        || !row.written
        || row.local_path.as_deref() != Some(expected_path)
        || row.download_claim.is_some()
    {
        bail!("legacy artifact row changed before receipt promotion");
    }
    match row.artifact_receipt.as_ref() {
        Some(existing) if existing == &receipt => return Ok(()),
        Some(_) => bail!("artifact receipt appeared during legacy promotion"),
        None => row.artifact_receipt = Some(receipt),
    }
    save_updated_session(session)
}

pub(crate) fn import_song_for_row(
    session_id: &str,
    row: &ImportSessionRow,
) -> anyhow::Result<crate::api::Song> {
    use anyhow::Context as _;

    let selected_key = row
        .selected_key
        .as_deref()
        .filter(|key| !key.is_empty())
        .context("import row has no selected download key")?;
    let title = if row.title.trim().is_empty() {
        row.selected_display.as_deref().unwrap_or(selected_key)
    } else {
        &row.title
    };
    let artist = if row.artists.is_empty() {
        String::new()
    } else {
        row.artists.join(", ")
    };
    let duration = row
        .duration_secs
        .map(|seconds| crate::util::format::time(f64::from(seconds)))
        .unwrap_or_default();
    let mut song =
        crate::api::Song::from_search(selected_key, title, artist, duration, row.album.clone());
    song.duration_secs = row.duration_secs;
    let album_artist = (!row.album_artists.is_empty()).then(|| row.album_artists.join(", "));
    Ok(song
        .with_catalog_metadata(
            album_artist,
            row.disc_number,
            row.track_number,
            row.isrc.clone(),
            Some(row.source_key.clone()),
            row.source_url.clone(),
        )
        .with_import_metadata(crate::api::SongImportMetadata {
            artists: row.artists.clone(),
            album_artists: row.album_artists.clone(),
            album_release_date: row.album_release_date.clone(),
            album_release_date_precision: row.album_release_date_precision.clone(),
            album_total_tracks: row.album_total_tracks,
            album_type: row.album_type.clone(),
            album_art_url: row.album_art_url.clone(),
            explicit: row.explicit,
        })
        .with_import_session(Some(session_id.to_owned()), Some(row.source_order)))
}

/// Record a download failure for the exact admitted row. A delayed failure never clears a row
/// that another attempt has already committed.
pub(crate) fn record_import_download_error(
    claim: &ImportDownloadClaim,
    error: &str,
) -> anyhow::Result<bool> {
    let _guard = ImportRecordGuard::try_acquire(&claim.session_id)?;
    let durable_move_pending = super::super::artifact_move::has_pending_claim(claim)?;
    let mut session = ImportSession::load(&claim.session_id)?;
    if session.session_instance_id != claim.session_instance_id {
        return Ok(false);
    }
    let row = row_by_source_order_mut(&mut session, claim.source_order)?;
    if row.row_id != claim.row_id
        || row.revision != claim.row_revision
        || row.selected_key.as_deref() != Some(&claim.expected_key)
        || row.download_claim.as_ref() != Some(claim)
        || row.written
        || row.artifact_receipt.is_some()
    {
        return Ok(false);
    }
    if !durable_move_pending {
        row.download_claim = None;
    }
    row.local_path = None;
    row.errors.clear();
    let message = error.trim();
    if !message.is_empty() {
        row.errors.push(message.chars().take(500).collect());
    }
    save_updated_session(session)?;
    Ok(true)
}

/// Persist a synthetic task/actor interruption while deliberately retaining the exact claim.
/// A restart can then reuse that claim and reconcile any incoming bytes or durable transaction.
pub(crate) fn record_import_download_interruption(
    claim: &ImportDownloadClaim,
    error: &str,
) -> anyhow::Result<bool> {
    let _guard = ImportRecordGuard::try_acquire(&claim.session_id)?;
    let mut session = ImportSession::load(&claim.session_id)?;
    if session.session_instance_id != claim.session_instance_id {
        return Ok(false);
    }
    let row = row_by_source_order_mut(&mut session, claim.source_order)?;
    if row.row_id != claim.row_id
        || row.revision != claim.row_revision
        || row.selected_key.as_deref() != Some(&claim.expected_key)
        || row.download_claim.as_ref() != Some(claim)
        || row.written
    {
        return Ok(false);
    }
    row.errors.clear();
    let message = error.trim();
    if !message.is_empty() {
        row.errors.push(message.chars().take(500).collect());
    }
    save_updated_session(session)?;
    Ok(true)
}
