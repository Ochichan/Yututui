use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail};

use super::ImportDownloadContext;
use super::album_art::fetch_album_art;
use super::import_inbox::validate_downloaded_path;
use crate::api::Song;
use crate::util::safe_fs::FileObjectId;

#[derive(Debug)]
pub(super) struct FinalizedDownload {
    pub(super) artifact_path: PathBuf,
    pub(super) publish_name: OsString,
    pub(super) retained_raw: Option<RetainedRawDownload>,
}

#[derive(Debug)]
pub(super) struct RetainedRawDownload {
    pub(super) path: PathBuf,
    pub(super) object_id: FileObjectId,
}

pub(super) async fn finalize_download_metadata(
    song: &Song,
    path: &Path,
    directory: &Path,
    import_root: Option<&Path>,
    import_context: Option<&ImportDownloadContext>,
    metadata_required: bool,
) -> Result<FinalizedDownload> {
    let metadata = validate_downloaded_path(path, directory, import_root)?;
    if import_root.is_some()
        && metadata.len() > crate::transfer::artifact_identity::ARTIFACT_AUDIO_MAX_BYTES
    {
        bail!(
            "import audio exceeds the {} byte limit before metadata rewrite: {}",
            crate::transfer::artifact_identity::ARTIFACT_AUDIO_MAX_BYTES,
            path.display()
        );
    }
    let cover_art = match song.album_art_url.as_deref() {
        Some(url) => match fetch_album_art(url).await {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                tracing::warn!(%error, "could not fetch album artwork for download tags");
                None
            }
        },
        None => None,
    };
    if let Some(root) = import_root {
        validate_downloaded_path(path, directory, Some(root))?;
    }
    let publish_name = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| anyhow!("downloaded audio has no basename"))?;
    let finalized = match import_context {
        Some(context) => crate::downloads::write_import_audio_generation(
            song,
            path,
            &context.claim.claim_id,
            cover_art.as_deref(),
        )
        .map(|generation| {
            (
                generation.path,
                Some(RetainedRawDownload {
                    path: path.to_path_buf(),
                    object_id: generation.source_object_id,
                }),
            )
        }),
        None => crate::downloads::write_audio_tags_staged(song, path, cover_art.as_deref())
            .map(|()| (path.to_path_buf(), None)),
    };
    let (artifact_path, retained_raw) = match finalized {
        Ok(finalized) => finalized,
        Err(error) => {
            if metadata_required {
                return Err(error).with_context(|| {
                    format!("write required download audio tags: {}", path.display())
                });
            }
            tracing::warn!(%error, path = %path.display(), "could not write download audio tags");
            (path.to_path_buf(), None)
        }
    };
    if let Some(root) = import_root {
        validate_downloaded_path(&artifact_path, directory, Some(root))?;
    }
    let sidecar_result = if metadata_required {
        crate::downloads::write_sidecar_noreplace(song, &artifact_path)
    } else {
        crate::downloads::write_sidecar(song, &artifact_path)
    };
    if let Err(error) = sidecar_result {
        if metadata_required {
            return Err(error).with_context(|| {
                format!(
                    "write required download sidecar: {}",
                    artifact_path.display()
                )
            });
        }
        tracing::warn!(%error, path = %artifact_path.display(), "could not write download sidecar");
    }
    Ok(FinalizedDownload {
        artifact_path,
        publish_name,
        retained_raw,
    })
}
