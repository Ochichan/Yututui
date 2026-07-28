//! Publication permissions for artifacts that land in a directory the user chose.
//!
//! Two callers share this. The import organiser moves downloads into a local library, and the
//! music-server publisher copies them into a Navidrome music folder. Both face the same question:
//! a file this process creates is private by default, but a library is read by something else —
//! another local user, or a server daemon running under its own account. Neither caller can widen
//! permissions blindly, so the mode is derived from the destination root the user already chose.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::bail;

use crate::util::safe_fs;

/// Who has to be able to read what gets published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublishAudience {
    /// Owner-only artifacts staying inside an app-owned private root.
    Private,
    /// A library another local account or a server daemon must be able to read.
    SharedLibrary,
    /// No explicit mode is requested; the filesystem default stands.
    Inherited,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct PublishModes {
    /// The published artifact itself.
    pub(crate) file: Option<u32>,
    /// A companion record that stays owner-only even in a shared library.
    pub(crate) sidecar: Option<u32>,
    /// Directories created on the way to the artifact.
    pub(crate) directory: Option<u32>,
}

/// Derive the publication modes for one audience under `destination_root`.
///
/// `SharedLibrary` never invents access. It mirrors the group and other bits the destination root
/// already grants, so publishing into a private folder stays private and publishing into a folder
/// the user opened up to a server daemon stays readable by it. Windows carries no mode; a share
/// there is governed by its own ACLs.
pub(crate) fn publish_modes(
    audience: PublishAudience,
    destination_root: &Path,
) -> std::io::Result<PublishModes> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        match audience {
            PublishAudience::SharedLibrary => {
                let root_mode = fs::metadata(destination_root)?.mode() & 0o777;
                let group_visible = root_mode & 0o050 == 0o050;
                let other_visible = root_mode & 0o005 == 0o005;
                let directory = 0o700
                    | if group_visible { 0o050 } else { 0 }
                    | if other_visible { 0o005 } else { 0 };
                let file = 0o600
                    | if group_visible { 0o040 } else { 0 }
                    | if other_visible { 0o004 } else { 0 };
                Ok(PublishModes {
                    file: Some(file),
                    sidecar: Some(0o600),
                    directory: Some(directory),
                })
            }
            PublishAudience::Private => Ok(PublishModes {
                file: Some(0o600),
                sidecar: Some(0o600),
                directory: None,
            }),
            PublishAudience::Inherited => Ok(PublishModes::default()),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (audience, destination_root);
        Ok(PublishModes::default())
    }
}

pub(crate) fn validate_publish_mode(mode: Option<u32>) -> anyhow::Result<()> {
    if mode.is_some_and(|mode| !matches!(mode, 0o600 | 0o640 | 0o644)) {
        bail!("artifact move contains an invalid publication mode");
    }
    Ok(())
}

/// Refuse anything that is not a real directory, so a planted symlink cannot redirect a scope.
pub(crate) fn reject_symlink_or_non_directory(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing non-directory artifact scope {}", path.display()),
        ));
    }
    Ok(())
}

/// Create only real directory components beneath an already-canonical root. In particular, do
/// not let `create_dir_all` follow an existing in-root symlink and create directories outside the
/// selected destination before the later canonical-scope check gets a chance to reject it.
pub(crate) fn ensure_scoped_directory(
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

#[cfg(test)]
mod tests;
