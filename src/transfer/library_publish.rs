//! Copy one downloaded track into a music folder YuTuTui does not own.
//!
//! The destination is a directory the user pointed at — typically a music server's library, local
//! or mounted. That makes every guarantee here structural rather than procedural:
//!
//! - everything lands under one fixed `YuTuTui/` subtree, so no bug can reach the rest of the
//!   user's library;
//! - containment is a handle walk, not a pathname comparison, so a symlink planted between
//!   validation and the write is rejected rather than followed;
//! - publication is `promote_noreplace`, so a file this process did not just create is never
//!   overwritten;
//! - nothing is ever unlinked. An interrupted publish leaves a dot-prefixed stage that the next
//!   attempt reuses, because a module that can delete inside someone else's music library is a
//!   worse thing to own than a stray temporary file.
//!
//! The copy itself is the same shape the import organiser already uses: stage, bounded copy,
//! durable sync, digest the stage, promote. The digest deliberately comes from the staged handle
//! after the sync rather than from the source, so a re-download racing the copy cannot make the
//! ledger describe bytes that never landed.

use std::ffi::{OsStr, OsString};
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::recorder::sanitize_track_filename;
use crate::transfer::artifact_identity::{ARTIFACT_AUDIO_MAX_BYTES, file_identity_from_open};
use crate::transfer::library_modes::{
    PublishAudience, ensure_scoped_directory, publish_modes, reject_symlink_or_non_directory,
};
use crate::util::safe_fs;

/// The one subtree publication is allowed to touch.
pub(crate) const LIBRARY_SUBTREE: &str = "YuTuTui";

const UNKNOWN_ALBUM: &str = "Unknown Album";
const UNKNOWN_ARTIST: &str = "Unknown Artist";

/// What the caller knows about the track being published.
pub(crate) struct PublishTrack<'a> {
    pub(crate) video_id: &'a str,
    pub(crate) title: &'a str,
    pub(crate) artist: Option<&'a str>,
    pub(crate) album_artist: Option<&'a str>,
    pub(crate) album: Option<&'a str>,
    pub(crate) track_number: Option<u32>,
}

/// Where one track goes, relative to the configured music folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublishPlan {
    pub(crate) relative_dir: PathBuf,
    pub(crate) basename: OsString,
    pub(crate) stage_basename: OsString,
}

impl PublishPlan {
    pub(crate) fn relative_path(&self) -> String {
        self.relative_dir
            .join(&self.basename)
            .to_string_lossy()
            .into_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PublishedArtifact {
    pub(crate) relative_path: String,
    pub(crate) len: u64,
    pub(crate) sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublishOutcome {
    /// The bytes were copied and promoted by this call.
    Published(PublishedArtifact),
    /// The exact same bytes were already at the final name; nothing was written.
    AlreadyPublished(PublishedArtifact),
    /// Something else occupies the final name. Never replaced, always reported.
    Conflict {
        relative_path: String,
        published_sha256: String,
        local_sha256: String,
    },
}

/// Build the destination layout for one track.
///
/// Every component goes through the recorder's filename sanitiser, which bounds length in bytes
/// rather than characters, neutralises separators and Windows device names, and is already
/// traversal-tested. The `[video_id]` suffix mirrors the download naming scheme; it keeps
/// republication idempotent and leaves the identity breadcrumb a future server/local binding will
/// need.
pub(crate) fn plan_publish_path(track: &PublishTrack<'_>, extension: Option<&str>) -> PublishPlan {
    let artist = track
        .album_artist
        .or(track.artist)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(UNKNOWN_ARTIST);
    let album = track
        .album
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(UNKNOWN_ALBUM);

    let relative_dir = Path::new(LIBRARY_SUBTREE)
        .join(sanitize_track_filename(artist))
        .join(sanitize_track_filename(album));

    let stem = match track.track_number {
        Some(number) => format!(
            "{number:02} - {} [{}]",
            track.title.trim(),
            track.video_id.trim()
        ),
        None => format!("{} [{}]", track.title.trim(), track.video_id.trim()),
    };
    let mut basename = sanitize_track_filename(&stem);
    if let Some(extension) = normalized_extension(extension) {
        basename.push('.');
        basename.push_str(&extension);
    }

    // Deterministic per track, so an interrupted attempt is recognisable and reusable rather than
    // accumulating. The leading dot keeps server scanners from indexing a partial file.
    let stage_basename = format!(
        ".ytt-publish-{}.part",
        sanitize_track_filename(track.video_id.trim())
    );

    PublishPlan {
        relative_dir,
        basename: OsString::from(basename),
        stage_basename: OsString::from(stage_basename),
    }
}

fn normalized_extension(extension: Option<&str>) -> Option<String> {
    let extension = extension?.trim().trim_start_matches('.');
    if extension.is_empty() || extension.len() > 16 {
        return None;
    }
    if !extension.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(extension.to_ascii_lowercase())
}

/// Copy `source` into the configured music folder according to `plan`.
///
/// `destination_root` must already be a real directory the user chose. It is canonicalised here,
/// and every directory beneath it is created and then re-opened by handle.
pub(crate) fn publish_into_library(
    source: &Path,
    destination_root: &Path,
    plan: &PublishPlan,
) -> Result<PublishOutcome> {
    reject_symlink_or_non_directory(destination_root)?;
    let destination_root = std::fs::canonicalize(destination_root).with_context(|| {
        format!(
            "canonicalize the music folder {}",
            destination_root.display()
        )
    })?;

    let modes = publish_modes(PublishAudience::SharedLibrary, &destination_root)?;
    ensure_scoped_directory(&destination_root, &plan.relative_dir, modes.directory)?;

    let source_len = source_length(source)?;
    ensure_room_for(&destination_root, source_len)?;

    // The handle walk is the containment boundary: every component is opened O_NOFOLLOW and the
    // directory is re-verified by kernel identity inside the promotion below.
    let directory = safe_fs::PinnedDir::open_existing(&destination_root, &plan.relative_dir)?;
    let relative_path = plan.relative_path();

    if let Some(outcome) = settle_existing_publication(&directory, plan, source, &relative_path)? {
        return Ok(outcome);
    }

    let mut stage = open_or_reuse_stage(&directory, &plan.stage_basename)?;
    if let Some(mode) = modes.file {
        apply_publish_mode(&mut stage, mode)?;
    }
    copy_source_into_stage(source, &mut stage, source_len)?;
    stage.sync_durable()?;

    let identity = file_identity_from_open(
        stage.file_mut()?,
        Path::new(&plan.stage_basename),
        ARTIFACT_AUDIO_MAX_BYTES,
    )?;

    stage
        .promote_noreplace(&directory, &plan.basename)
        .with_context(|| format!("publish {relative_path}"))?;
    directory.sync_directory()?;

    Ok(PublishOutcome::Published(PublishedArtifact {
        relative_path,
        len: identity.len,
        sha256: identity.sha256,
    }))
}

/// Decide what an occupied final name means before anything is written.
fn settle_existing_publication(
    directory: &safe_fs::PinnedDir,
    plan: &PublishPlan,
    source: &Path,
    relative_path: &str,
) -> Result<Option<PublishOutcome>> {
    let mut published = match directory.open_child_readonly(&plan.basename) {
        Ok(published) => published,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {relative_path}")),
    };

    let published_identity = file_identity_from_open(
        published.file_mut()?,
        Path::new(&plan.basename),
        ARTIFACT_AUDIO_MAX_BYTES,
    )?;
    let local_identity = local_identity(source)?;

    if published_identity.sha256 == local_identity.sha256 {
        return Ok(Some(PublishOutcome::AlreadyPublished(PublishedArtifact {
            relative_path: relative_path.to_owned(),
            len: published_identity.len,
            sha256: published_identity.sha256,
        })));
    }

    Ok(Some(PublishOutcome::Conflict {
        relative_path: relative_path.to_owned(),
        published_sha256: published_identity.sha256,
        local_sha256: local_identity.sha256,
    }))
}

/// Claim the stage, reusing an interrupted one rather than deleting it.
fn open_or_reuse_stage(
    directory: &safe_fs::PinnedDir,
    stage_basename: &OsStr,
) -> Result<safe_fs::OwnedGeneration> {
    match directory.create_new(stage_basename) {
        Ok(stage) => Ok(stage),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Our own deterministic stage name from an interrupted attempt. `open_child` refuses
            // anything that is not a regular file, so a directory or symlink planted under this
            // name fails here instead of being written through.
            let mut stage = directory.open_child(stage_basename).with_context(|| {
                format!(
                    "reuse the interrupted publish stage {}",
                    Path::new(stage_basename).display()
                )
            })?;
            let file = stage.file_mut()?;
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            Ok(stage)
        }
        Err(error) => Err(error).context("create the publish stage"),
    }
}

fn copy_source_into_stage(
    source: &Path,
    stage: &mut safe_fs::OwnedGeneration,
    expected_len: u64,
) -> Result<()> {
    use std::io::Read as _;

    let mut reader = std::fs::File::open(source)
        .with_context(|| format!("open the downloaded track {}", source.display()))?;
    let limit = expected_len
        .checked_add(1)
        .context("published artifact size overflow")?;
    let copied = std::io::copy(&mut reader.by_ref().take(limit), stage.file_mut()?)
        .with_context(|| format!("copy {} into the music folder", source.display()))?;
    if copied != expected_len {
        bail!(
            "{} changed while it was being published; expected {expected_len} bytes and copied {copied}",
            source.display()
        );
    }
    Ok(())
}

fn local_identity(
    source: &Path,
) -> Result<crate::transfer::artifact_identity::ArtifactFileIdentity> {
    let mut file = std::fs::File::open(source)
        .with_context(|| format!("open the downloaded track {}", source.display()))?;
    let identity = file_identity_from_open(&mut file, source, ARTIFACT_AUDIO_MAX_BYTES)?;
    Ok(identity)
}

fn source_length(source: &Path) -> Result<u64> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("inspect the downloaded track {}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "refusing to publish {}, which is not a regular file",
            source.display()
        );
    }
    if metadata.len() > ARTIFACT_AUDIO_MAX_BYTES {
        bail!(
            "{} exceeds the {ARTIFACT_AUDIO_MAX_BYTES} byte publication limit",
            source.display()
        );
    }
    Ok(metadata.len())
}

/// Refuse before writing anything when the destination volume cannot hold the copy.
fn ensure_room_for(destination_root: &Path, needed: u64) -> Result<()> {
    let space = safe_fs::volume_space(destination_root)?;
    if space.available_bytes < needed {
        bail!(
            "the music folder has {} bytes free and this track needs {needed}",
            space.available_bytes
        );
    }
    Ok(())
}

fn apply_publish_mode(stage: &mut safe_fs::OwnedGeneration, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        stage.set_mode(mode)
    }
    #[cfg(not(unix))]
    {
        let _ = (stage, mode);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
