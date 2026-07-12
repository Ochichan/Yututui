//! Atomic, no-replace publication for completed personal-data export files.
//!
//! Hard links are the preferred path because they leave the private temp name available until the
//! final directory entry is synced. Filesystems without hard-link support fall back only to an OS
//! primitive whose contract is an atomic same-directory move that fails if the destination exists.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The final name and temp name refer to the same file until cleanup removes the temp link.
    Linked,
    /// The temp name was atomically moved and no longer exists.
    Moved,
}

pub(super) fn no_replace(temp: &Path, final_path: &Path) -> io::Result<Outcome> {
    publish_with(
        temp,
        final_path,
        |from, to| fs::hard_link(from, to),
        platform_move_fallback,
    )
}

fn publish_with<Link, Fallback>(
    temp: &Path,
    final_path: &Path,
    link: Link,
    fallback: Fallback,
) -> io::Result<Outcome>
where
    Link: FnOnce(&Path, &Path) -> io::Result<()>,
    Fallback: FnOnce(&Path, &Path, io::Error) -> io::Result<Outcome>,
{
    match link(temp, final_path) {
        Ok(()) => Ok(Outcome::Linked),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
        Err(error) if link_failure_allows_move(&error) => fallback(temp, final_path, error),
        Err(error) => Err(error),
    }
}

fn link_failure_allows_move(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied
    ) {
        return true;
    }

    #[cfg(unix)]
    if error.raw_os_error().is_some_and(|code| {
        code == libc::EOPNOTSUPP || code == libc::ENOTSUP || code == libc::EMLINK
    }) {
        return true;
    }

    #[cfg(windows)]
    if error.raw_os_error().is_some_and(|code| {
        use windows_sys::Win32::Foundation::{
            ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, ERROR_PRIVILEGE_NOT_HELD,
            ERROR_TOO_MANY_LINKS,
        };
        [
            ERROR_INVALID_FUNCTION,
            ERROR_NOT_SUPPORTED,
            ERROR_PRIVILEGE_NOT_HELD,
            ERROR_TOO_MANY_LINKS,
        ]
        .into_iter()
        .any(|expected| code == expected as i32)
    }) {
        return true;
    }

    false
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn platform_move_fallback(
    temp: &Path,
    final_path: &Path,
    _hard_link_error: io::Error,
) -> io::Result<Outcome> {
    move_no_replace(temp, final_path)?;
    Ok(Outcome::Moved)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_move_fallback(
    _temp: &Path,
    _final_path: &Path,
    hard_link_error: io::Error,
) -> io::Result<Outcome> {
    // Other platforms retain the fail-closed hard-link-only behavior until they have a reviewed
    // atomic exclusive-rename primitive.
    Err(hard_link_error)
}

#[cfg(target_os = "linux")]
fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let to = CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both C strings are live and NUL-terminated for the syscall. AT_FDCWD makes their
    // absolute paths self-contained, and RENAME_NOREPLACE gives the required atomic exclusion.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let to = CString::new(to.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    // SAFETY: both C strings are live and NUL-terminated for this call. RENAME_EXCL makes the
    // same-directory rename atomic and guarantees an existing destination is never replaced.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn move_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    super::windows_private::move_no_replace(from, to)
}

/// Finish durability and temp-link cleanup after publication has already succeeded.
///
/// Errors here are observable warnings, not returned failures: the complete final path already
/// exists, so returning `Err` would tell callers no export exists and invite duplicate retries.
pub(super) fn finish(
    directory: &Path,
    temp: &Path,
    final_path: &Path,
    outcome: Outcome,
) -> PathBuf {
    finish_with(
        directory,
        temp,
        final_path,
        outcome,
        sync_directory,
        |path| fs::remove_file(path),
    )
}

fn finish_with<Sync, Remove>(
    directory: &Path,
    temp: &Path,
    final_path: &Path,
    outcome: Outcome,
    mut sync: Sync,
    remove: Remove,
) -> PathBuf
where
    Sync: FnMut(&Path) -> io::Result<()>,
    Remove: FnOnce(&Path) -> io::Result<()>,
{
    let published_synced = match sync(directory) {
        Ok(()) => true,
        Err(error) => {
            if outcome == Outcome::Linked {
                tracing::warn!(
                    path = %directory.display(),
                    published = %final_path.display(),
                    retained_temp = %temp.display(),
                    %error,
                    "personal-data export was published but its directory could not be synced; retaining the private temp link for recovery"
                );
            } else {
                tracing::warn!(
                    path = %directory.display(),
                    published = %final_path.display(),
                    pre_publish_temp = %temp.display(),
                    %error,
                    "personal-data export was moved but its directory could not be synced; the pre-publish temp name was synced for crash recovery"
                );
            }
            false
        }
    };

    if outcome == Outcome::Linked && published_synced {
        match remove(temp) {
            Ok(()) => {
                if let Err(error) = sync(directory) {
                    tracing::warn!(
                        path = %directory.display(),
                        published = %final_path.display(),
                        %error,
                        "personal-data export was published but temp-link cleanup could not be synced"
                    );
                }
            }
            Err(error) => tracing::warn!(
                path = %temp.display(),
                published = %final_path.display(),
                %error,
                "personal-data export was published but its temp link could not be removed"
            ),
        }
    }

    final_path.to_path_buf()
}

#[cfg(unix)]
pub(super) fn sync_directory(directory: &Path) -> io::Result<()> {
    fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
pub(super) fn sync_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn test_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "yututui-export-publish-{label}-{}-{}",
            std::process::id(),
            super::super::random_suffix().expect("random suffix")
        ));
        fs::create_dir(&path).expect("create test directory");
        path
    }

    #[test]
    fn no_replace_preserves_an_existing_destination() {
        let directory = test_directory("no-replace");
        let temp = directory.join("temp");
        let final_path = directory.join("final");
        fs::write(&temp, b"new").expect("write temp fixture");
        fs::write(&final_path, b"old").expect("write final fixture");

        let error = no_replace(&temp, &final_path).expect_err("must not replace");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&final_path).expect("read final"), b"old");
        assert_eq!(fs::read(&temp).expect("read temp"), b"new");
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn supported_link_failure_uses_move_and_reports_that_temp_was_consumed() {
        let directory = test_directory("fallback");
        let temp = directory.join("temp");
        let final_path = directory.join("final");
        fs::write(&temp, b"complete").expect("write temp fixture");

        let outcome = publish_with(
            &temp,
            &final_path,
            |_from, _to| Err(io::Error::new(io::ErrorKind::Unsupported, "no links")),
            |from, to, _link_error| {
                fs::rename(from, to)?;
                Ok(Outcome::Moved)
            },
        )
        .expect("exclusive-move fallback");

        assert_eq!(outcome, Outcome::Moved);
        assert!(!temp.exists());
        assert_eq!(fs::read(&final_path).expect("read final"), b"complete");
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn post_publish_sync_failure_reports_final_and_retains_recovery_link() {
        let directory = test_directory("post-publish-warning");
        let temp = directory.join("temp");
        let final_path = directory.join("final");
        fs::write(&temp, b"complete").expect("write temp fixture");
        fs::hard_link(&temp, &final_path).expect("publish fixture");
        let sync_calls = Cell::new(0usize);

        let reported = finish_with(
            &directory,
            &temp,
            &final_path,
            Outcome::Linked,
            |_| {
                sync_calls.set(sync_calls.get() + 1);
                Err(io::Error::other("injected directory sync failure"))
            },
            |path| fs::remove_file(path),
        );

        assert_eq!(reported, final_path);
        assert_eq!(sync_calls.get(), 1);
        assert_eq!(
            fs::read(&temp).expect("retained recovery link"),
            b"complete"
        );
        assert_eq!(fs::read(&reported).expect("published file"), b"complete");
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn moved_publish_outcome_does_not_try_to_remove_the_consumed_temp_name() {
        let directory = test_directory("moved-finish");
        let temp = directory.join("consumed-temp");
        let final_path = directory.join("final");
        fs::write(&final_path, b"complete").expect("write published fixture");
        let remove_called = Cell::new(false);
        let sync_calls = Cell::new(0usize);

        let reported = finish_with(
            &directory,
            &temp,
            &final_path,
            Outcome::Moved,
            |_| {
                sync_calls.set(sync_calls.get() + 1);
                Ok(())
            },
            |_| {
                remove_called.set(true);
                Ok(())
            },
        );

        assert_eq!(reported, final_path);
        assert_eq!(sync_calls.get(), 1);
        assert!(!remove_called.get());
        assert_eq!(fs::read(&reported).expect("published file"), b"complete");
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn platform_exclusive_move_consumes_the_temp_name() {
        let directory = test_directory("exclusive-move");
        let temp = directory.join("temp");
        let final_path = directory.join("final");
        fs::write(&temp, b"complete").expect("write temp fixture");

        move_no_replace(&temp, &final_path).expect("exclusive move");

        assert!(!temp.exists());
        assert_eq!(fs::read(&final_path).expect("read final"), b"complete");
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn platform_exclusive_move_never_replaces_an_existing_destination() {
        let directory = test_directory("exclusive-move-collision");
        let temp = directory.join("temp");
        let final_path = directory.join("final");
        fs::write(&temp, b"new").expect("write temp fixture");
        fs::write(&final_path, b"old").expect("write final fixture");

        let error = move_no_replace(&temp, &final_path).expect_err("must not replace");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&temp).expect("read temp"), b"new");
        assert_eq!(fs::read(&final_path).expect("read final"), b"old");
        fs::remove_dir_all(directory).expect("cleanup");
    }
}
