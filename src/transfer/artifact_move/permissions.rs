use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::bail;

use super::{ArtifactMoveKind, import_private_root, reject_symlink_or_non_directory};
use crate::util::safe_fs;

/// Create only real directory components beneath an already-canonical root. In particular, do
/// not let `create_dir_all` follow an existing in-root symlink and create directories outside the
/// selected destination before the later canonical-scope check gets a chance to reject it.
pub(super) fn ensure_scoped_directory(
    root: &Path,
    relative: &Path,
    directory_mode: Option<u32>,
) -> std::io::Result<PathBuf> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            if matches!(component, Component::CurDir) {
                continue;
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid relative artifact directory {}", relative.display()),
            ));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(_) => {
                reject_symlink_or_non_directory(&current)?;
                validate_scoped_directory_mode(&current, directory_mode)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match create_scoped_directory(&current, directory_mode) {
                    Ok(()) => safe_fs::sync_parent_dir(&current)?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                reject_symlink_or_non_directory(&current)?;
                validate_scoped_directory_mode(&current, directory_mode)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(current)
}

#[derive(Clone, Copy, Default)]
pub(super) struct ArtifactPublishModes {
    pub(super) audio: Option<u32>,
    pub(super) sidecar: Option<u32>,
    pub(super) directory: Option<u32>,
}

pub(super) fn artifact_publish_modes(
    kind: ArtifactMoveKind,
    destination_root: &Path,
) -> std::io::Result<ArtifactPublishModes> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        if matches!(kind, ArtifactMoveKind::Organize)
            && import_private_root(destination_root).is_none()
        {
            let root_mode = fs::metadata(destination_root)?.mode() & 0o777;
            let group_visible = root_mode & 0o050 == 0o050;
            let other_visible = root_mode & 0o005 == 0o005;
            let directory = 0o700
                | if group_visible { 0o050 } else { 0 }
                | if other_visible { 0o005 } else { 0 };
            let audio = 0o600
                | if group_visible { 0o040 } else { 0 }
                | if other_visible { 0o004 } else { 0 };
            return Ok(ArtifactPublishModes {
                audio: Some(audio),
                sidecar: Some(0o600),
                directory: Some(directory),
            });
        }
        if matches!(kind, ArtifactMoveKind::ImportDownload)
            && import_private_root(destination_root).is_some()
        {
            return Ok(ArtifactPublishModes {
                audio: Some(0o600),
                sidecar: Some(0o600),
                directory: None,
            });
        }
        Ok(ArtifactPublishModes::default())
    }
    #[cfg(not(unix))]
    {
        let _ = (kind, destination_root);
        Ok(ArtifactPublishModes::default())
    }
}

pub(super) fn validate_publish_mode(mode: Option<u32>) -> anyhow::Result<()> {
    if mode.is_some_and(|mode| !matches!(mode, 0o600 | 0o640 | 0o644)) {
        bail!("artifact move contains an invalid publication mode");
    }
    Ok(())
}

#[cfg(unix)]
fn create_scoped_directory(path: &Path, mode: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};

    let Some(mode) = mode else {
        return fs::create_dir(path);
    };
    let mut builder = fs::DirBuilder::new();
    builder.mode(mode).create(path)?;
    let observed = fs::symlink_metadata(path)?;
    if observed.file_type().is_symlink() || !observed.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("new artifact directory was replaced: {}", path.display()),
        ));
    }
    let observed_mode = observed.mode() & 0o777;
    if observed_mode & mode != mode {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "new artifact directory {} has mode {observed_mode:04o}; expected {mode:04o} (check the process umask)",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_scoped_directory_mode(path: &Path, mode: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let Some(mode) = mode else {
        return Ok(());
    };
    let observed = fs::symlink_metadata(path)?.mode() & 0o777;
    if observed & mode == mode {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "artifact directory {} has mode {observed:04o}; library publication requires at least {mode:04o}",
                path.display()
            ),
        ))
    }
}

#[cfg(not(unix))]
fn create_scoped_directory(path: &Path, _mode: Option<u32>) -> std::io::Result<()> {
    fs::create_dir(path)
}

#[cfg(not(unix))]
fn validate_scoped_directory_mode(_path: &Path, _mode: Option<u32>) -> std::io::Result<()> {
    Ok(())
}
