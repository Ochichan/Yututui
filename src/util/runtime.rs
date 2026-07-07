//! Private per-user runtime directory for sockets and short-lived descriptors.

use std::io;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

use crate::util::safe_fs;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn user_tag() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let tag: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect();
    if tag.is_empty() {
        "default".to_owned()
    } else {
        tag
    }
}

#[cfg(unix)]
fn uid_tag() -> String {
    unsafe { libc::geteuid() }.to_string()
}

#[cfg(not(unix))]
fn uid_tag() -> String {
    user_tag()
}

#[cfg(unix)]
fn secure_existing_runtime_base(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    meta.is_dir()
        && !meta.file_type().is_symlink()
        && meta.uid() == unsafe { libc::geteuid() }
        && (meta.permissions().mode() & 0o077) == 0
}

#[cfg(unix)]
fn runtime_base() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(xdg);
        if path.is_absolute() && secure_existing_runtime_base(&path) {
            return path;
        }
    }
    std::env::temp_dir()
}

#[cfg(not(unix))]
fn runtime_base() -> PathBuf {
    std::env::temp_dir()
}

/// Private app runtime directory. Unix callers get a `0700` directory.
pub fn app_runtime_dir() -> io::Result<PathBuf> {
    let dir = runtime_base().join(format!("yututui-{}", uid_tag()));
    safe_fs::ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Legacy base used before the private runtime dir migration.
pub fn legacy_runtime_base() -> PathBuf {
    #[cfg(unix)]
    {
        if let Some(x) = std::env::var_os("XDG_RUNTIME_DIR") {
            let p = PathBuf::from(x);
            if p.is_absolute() {
                return p;
            }
        }
    }
    std::env::temp_dir()
}

/// Filesystem-safe per-user tag for socket and descriptor filenames.
pub fn filesystem_user_tag() -> String {
    user_tag()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_tag_is_safe() {
        let tag = filesystem_user_tag();
        assert!(!tag.is_empty());
        assert!(tag.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn runtime_dir_is_under_a_private_app_component() {
        let dir = app_runtime_dir().unwrap();
        assert!(
            dir.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("yututui-"))
        );
    }
}
