use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
use super::private_file_mode;
use super::{
    create_private_file, ensure_process_mutation_allowed, mark_do_not_clobber, open_no_symlink,
    reject_symlink, sync_parent_dir,
};

/// Recovery backups for secret-bearing files are useful, but old credentials should not
/// accumulate indefinitely.
pub const SECRET_BACKUP_RETENTION: usize = 3;
const RECOVERY_BACKUP_LABELS: &[&str] = &["corrupt", "too-large", "unreadable"];

/// Metadata for a recovery backup. Contents are intentionally never read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryBackup {
    pub path: PathBuf,
    pub label: String,
    pub len: u64,
    pub modified_unix: Option<u64>,
}

/// Move an unparseable file aside so a fresh default won't silently destroy it. Never
/// overwrites an existing backup (numbered `*.corrupt.N.bak`).
pub fn backup_aside(path: &Path) -> io::Result<PathBuf> {
    backup_with_label(path, "corrupt")
}

/// Secret-bearing counterpart to [`backup_aside`]. Preserves the newest recovery copy and
/// prunes older secret backups beyond [`SECRET_BACKUP_RETENTION`].
pub fn backup_aside_secret(path: &Path) -> io::Result<PathBuf> {
    backup_with_label_retained(path, "corrupt", SECRET_BACKUP_RETENTION)
}

/// Move an oversized file aside (`*.too-large.bak`). Public counterpart to [`backup_aside`]
/// for callers (e.g. `Config::load`) that read with their own size cap outside
/// [`crate::util::safe_fs::load_json_or_default_limited`].
pub fn backup_too_large(path: &Path) -> io::Result<PathBuf> {
    backup_with_label(path, "too-large")
}

/// Secret-bearing counterpart to [`backup_too_large`].
pub fn backup_too_large_secret(path: &Path) -> io::Result<PathBuf> {
    backup_with_label_retained(path, "too-large", SECRET_BACKUP_RETENTION)
}

/// Secret-bearing backup for present-but-unreadable files.
pub fn backup_unreadable_secret(path: &Path) -> io::Result<PathBuf> {
    backup_with_label_retained(path, "unreadable", SECRET_BACKUP_RETENTION)
}

/// List app-created recovery backups for `path` without reading their contents.
pub fn recovery_backups(path: &Path) -> io::Result<Vec<RecoveryBackup>> {
    recovery_backups_for_labels(path, RECOVERY_BACKUP_LABELS)
}

/// Enforce the default secret-backup retention for `path`.
pub fn enforce_secret_backup_retention(path: &Path) -> io::Result<usize> {
    ensure_process_mutation_allowed()?;
    rotate_recovery_backups(path, SECRET_BACKUP_RETENTION, None)
}

/// Move a file aside under a labeled, numbered backup (`*.<label>.bak`, then
/// `*.<label>.N.bak`) without ever overwriting an existing one. Shared by
/// [`backup_aside`] (unparseable JSON) and the oversized-file path in
/// [`crate::util::safe_fs::load_json_or_default_limited`].
pub(super) fn backup_with_label(path: &Path, label: &str) -> io::Result<PathBuf> {
    backup_with_label_inner(path, label)
}

fn backup_with_label_retained(path: &Path, label: &str, keep: usize) -> io::Result<PathBuf> {
    let bak = backup_with_label_inner(path, label)?;
    rotate_recovery_backups(path, keep, Some(&bak))?;
    Ok(bak)
}

fn backup_with_label_inner(path: &Path, label: &str) -> io::Result<PathBuf> {
    ensure_process_mutation_allowed()?;
    reject_symlink(path)?;
    for n in 0..1000 {
        let bak = backup_path(path, label, n);
        match fs::hard_link(path, &bak) {
            Ok(()) => {
                fs::remove_file(path)?;
                sync_parent_dir(path)?;
                return Ok(bak);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => match backup_by_copy(path, &bak) {
                Ok(()) => return Ok(bak),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    mark_do_not_clobber(path);
                    return Err(e);
                }
            },
        }
    }
    let err = io::Error::other("too many backups");
    mark_do_not_clobber(path);
    Err(err)
}

pub(super) fn backup_by_copy(path: &Path, bak: &Path) -> io::Result<()> {
    let mut src = open_no_symlink(path)?;
    let meta = src.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    let mut dst = create_private_file(bak)?;
    io::copy(&mut src, &mut dst)?;
    dst.sync_all()?;
    drop(dst);
    #[cfg(unix)]
    fs::set_permissions(bak, fs::Permissions::from_mode(private_file_mode()))?;
    fs::remove_file(path)?;
    sync_parent_dir(path)?;
    Ok(())
}

fn rotate_recovery_backups(
    path: &Path,
    keep: usize,
    protected: Option<&Path>,
) -> io::Result<usize> {
    let mut backups = recovery_backups(path)?;
    if backups.len() <= keep {
        return Ok(0);
    }
    backups.sort_by(|a, b| {
        a.modified_unix
            .cmp(&b.modified_unix)
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut removed = 0usize;
    while backups.len() > keep {
        let Some(idx) = backups
            .iter()
            .position(|backup| protected != Some(backup.path.as_path()))
        else {
            break;
        };
        let backup = backups.remove(idx);
        match fs::remove_file(&backup.path) {
            Ok(()) => {
                removed += 1;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                removed += 1;
            }
            Err(e) => return Err(e),
        }
    }
    if removed > 0 {
        sync_parent_dir(path)?;
    }
    Ok(removed)
}

fn recovery_backups_for_labels(path: &Path, labels: &[&str]) -> io::Result<Vec<RecoveryBackup>> {
    let mut out = Vec::new();
    for label in labels {
        for n in 0..1000 {
            let backup = backup_path(path, label, n);
            let meta = match fs::symlink_metadata(&backup) {
                Ok(meta) if meta.is_file() => meta,
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let modified_unix = meta
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|age| age.as_secs());
            out.push(RecoveryBackup {
                path: backup,
                label: (*label).to_owned(),
                len: meta.len(),
                modified_unix,
            });
        }
    }
    Ok(out)
}

pub(super) fn backup_path(path: &Path, label: &str, n: usize) -> PathBuf {
    if n == 0 {
        path.with_extension(format!("{label}.bak"))
    } else {
        path.with_extension(format!("{label}.{n}.bak"))
    }
}
