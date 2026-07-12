use super::*;

pub(super) fn committed_session_path(
    session_id: &str,
    row_id: &str,
    source_order: u32,
) -> anyhow::Result<Option<PathBuf>> {
    let session = ImportSession::load(session_id)?;
    let Some(row) = session
        .rows
        .iter()
        .find(|row| row.source_order == source_order && row.row_id == row_id)
    else {
        bail!("import session row identity changed for {session_id} row #{source_order}");
    };
    let Some(path) = row.local_path.as_ref().filter(|_| row.written) else {
        return Ok(None);
    };
    let path = path.clone();
    let sidecar = crate::downloads::sidecar_path(&path);
    let receipt = match row.artifact_receipt.clone() {
        Some(receipt) => receipt,
        None => return promote_legacy_committed_artifact(session_id, row, &path, &sidecar),
    };
    let mut verification = verify_receipt(&path, &sidecar, Some(&receipt));
    if matches!(verification, ReceiptVerification::Conflict(_))
        && receipt.sidecar_required
        && receipt.sidecar.is_some()
        && optional_file_identity(&sidecar, ARTIFACT_SIDECAR_MAX_BYTES)?.is_none()
    {
        repair_missing_committed_sidecar(session_id, row, &path, &receipt)?;
        verification = verify_receipt(&path, &sidecar, Some(&receipt));
    }
    match verification {
        ReceiptVerification::Verified => Ok(Some(path)),
        ReceiptVerification::NeedsDownload => {
            super::super::session::clear_missing_artifact_unlocked(
                session_id,
                row_id,
                source_order,
                &path,
            )?;
            Ok(None)
        }
        ReceiptVerification::Conflict(reason) => {
            bail!("committed artifact receipt conflict for {session_id}/{row_id}: {reason}")
        }
        ReceiptVerification::LegacyUnverified => unreachable!("an explicit receipt was supplied"),
    }
}

fn repair_missing_committed_sidecar(
    session_id: &str,
    row: &super::super::session::ImportSessionRow,
    audio: &Path,
    receipt: &ArtifactReceipt,
) -> anyhow::Result<()> {
    let expected = receipt
        .sidecar
        .as_ref()
        .context("required committed sidecar has no receipt identity")?;
    let audio_identity = file_identity_limited(audio, ARTIFACT_AUDIO_MAX_BYTES)?;
    if audio_identity != receipt.audio {
        bail!(
            "audio changed before missing sidecar repair: {}",
            audio.display()
        );
    }
    let song = super::super::session::import_song_for_row(session_id, row)?;
    let bytes = crate::downloads::sidecar_bytes(&song)?;
    if bytes.len() as u64 > ARTIFACT_SIDECAR_MAX_BYTES {
        bail!("repaired sidecar exceeds its bounded size limit");
    }
    let mut cursor = std::io::Cursor::new(bytes.as_slice());
    let generated = hash_exact_len(&mut cursor, bytes.len() as u64)?;
    if &generated != expected {
        bail!(
            "current metadata cannot reproduce the recorded sidecar for {session_id}/{}",
            row.row_id
        );
    }
    crate::downloads::write_sidecar_noreplace(&song, audio)?;
    Ok(())
}

fn promote_legacy_committed_artifact(
    session_id: &str,
    row: &super::super::session::ImportSessionRow,
    audio: &Path,
    sidecar: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(audio_identity) = optional_file_identity(audio, ARTIFACT_AUDIO_MAX_BYTES)? else {
        super::super::session::clear_missing_artifact_unlocked(
            session_id,
            &row.row_id,
            row.source_order,
            audio,
        )?;
        return Ok(None);
    };
    let sidecar_required = row.selected_key.is_some();
    if sidecar_required && optional_file_identity(sidecar, ARTIFACT_SIDECAR_MAX_BYTES)?.is_none() {
        let song = super::super::session::import_song_for_row(session_id, row)?;
        crate::downloads::write_sidecar_noreplace(&song, audio)?;
    }
    let sidecar_identity = optional_file_identity(sidecar, ARTIFACT_SIDECAR_MAX_BYTES)?;
    if sidecar_required && sidecar_identity.is_none() {
        bail!("legacy import artifact is missing its required sidecar");
    }
    if let Some(sidecar_data) = crate::downloads::read_sidecar(audio)? {
        if let Some(selected_key) = row.selected_key.as_deref()
            && (sidecar_data.linked_youtube_id() != Some(selected_key)
                || sidecar_data.import_session_id.as_deref() != Some(session_id)
                || sidecar_data.import_source_order != Some(row.source_order))
        {
            bail!(
                "legacy sidecar does not identify {session_id}/{} target {selected_key}",
                row.row_id
            );
        }
    } else if sidecar_required {
        bail!("legacy import sidecar could not be decoded after repair");
    }
    let receipt = ArtifactReceipt {
        audio: audio_identity,
        sidecar_required,
        sidecar: sidecar_identity,
        claim: None,
    };
    super::super::session::promote_artifact_receipt_unlocked(
        session_id,
        &row.row_id,
        row.source_order,
        audio,
        receipt,
    )?;
    Ok(Some(audio.to_path_buf()))
}
