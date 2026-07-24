//! Fail-closed filesystem primitives for credentials and device private keys.
//!
//! Unlike the older private-state helpers, these functions never repair an existing file while
//! reading it. A secret is accepted only from an already-open regular, non-link file with the
//! platform's exact current-user protection.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const TEMP_ATTEMPTS: usize = 8;

/// Atomically replace `path` with bytes stored in a current-user-only file.
///
/// The complete temporary file is protected and synced before publication. If the final parent
/// sync initially fails, an exact protected readback is followed by one parent-sync retry. This
/// makes a response-lost publication idempotent without accepting a different visible file.
pub fn write_owner_only_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    super::ensure_process_mutation_allowed()?;
    if let Some(parent) = path.parent() {
        super::ensure_private_dir(parent)?;
    }
    platform::validate_existing_target(path)?;

    let (temp_path, file) = create_temp_for(path)?;
    let result = write_and_publish(&temp_path, path, file, bytes);
    if result.is_err() {
        cleanup_temp(&temp_path);
    }
    result
}

fn create_temp_for(path: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..TEMP_ATTEMPTS {
        let temp_path = super::temp_path_for(path)?;
        match platform::create(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a private staging file",
    ))
}

fn write_and_publish(
    temp_path: &Path,
    path: &Path,
    mut file: File,
    bytes: &[u8],
) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()?;
    platform::validate(&file)?;
    let expected = platform::identity(&file)?;
    drop(file);

    super::atomic_replace(temp_path, path)?;

    let final_file = platform::open(path)?;
    platform::validate(&final_file)?;
    if platform::identity(&final_file)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private file identity changed during publication",
        ));
    }
    drop(final_file);
    match super::sync_parent_dir(path) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            let published = read_owner_only_limited(path, bytes.len() as u64)?;
            if published != bytes {
                return Err(first_error);
            }
            super::sync_parent_dir(path)
        }
    }
}

fn cleanup_temp(path: &Path) {
    if fs::remove_file(path).is_ok() {
        let _ = super::sync_parent_dir(path);
    }
}

/// Read a bounded current-user-only file without following a link or repairing permissions.
pub fn read_owner_only_limited(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = platform::open(path)?;
    platform::validate(&file)?;
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes {
        return Err(too_large());
    }

    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(too_large());
    }
    Ok(bytes)
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "private file exceeds its size limit",
    )
}

/// Durably remove a validated current-user-only file.
///
/// A missing file remains an idempotent success followed by the same parent durability boundary.
pub fn remove_owner_only_file_durable(path: &Path) -> io::Result<()> {
    super::ensure_process_mutation_allowed()?;
    let file = match platform::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return super::sync_parent_dir(path);
        }
        Err(error) => return Err(error),
    };
    platform::validate(&file)?;
    let expected = platform::identity(&file)?;
    platform::require_path_identity(path, &expected)?;
    drop(file);

    fs::remove_file(path)?;
    super::sync_parent_dir(path)
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Identity {
        device: u64,
        inode: u64,
    }

    pub(super) fn create(path: &Path) -> io::Result<File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let hardened = (|| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            validate(&file)
        })();
        match hardened {
            Ok(()) => Ok(file),
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(path);
                Err(error)
            }
        }
    }

    pub(super) fn open(path: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }

    pub(super) fn validate(file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || !super::super::is_owned_by_current_user(&metadata)
            || metadata.permissions().mode() & 0o7777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private file is not a current-user 0600 regular file",
            ));
        }
        Ok(())
    }

    pub(super) fn identity(file: &File) -> io::Result<Identity> {
        let metadata = file.metadata()?;
        Ok(Identity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    pub(super) fn require_path_identity(path: &Path, expected: &Identity) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.dev() != expected.device
            || metadata.ino() != expected.inode
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private file identity changed before removal",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_existing_target(path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to replace a link or non-file private path",
                ))
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    pub(super) type Identity = crate::data_export::windows_private::FileIdentity;

    pub(super) fn create(path: &Path) -> io::Result<File> {
        let file = crate::data_export::windows_private::create_private_file(path)?;
        if let Err(error) =
            crate::data_export::windows_private::verify_current_user_private_file(&file)
        {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(error);
        }
        Ok(file)
    }

    pub(super) fn open(path: &Path) -> io::Result<File> {
        fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    pub(super) fn validate(file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private path is not a regular non-reparse file",
            ));
        }
        crate::data_export::windows_private::verify_current_user_private_file(file)
    }

    pub(super) fn identity(file: &File) -> io::Result<Identity> {
        crate::data_export::windows_private::file_identity(file)
    }

    pub(super) fn require_path_identity(path: &Path, expected: &Identity) -> io::Result<()> {
        crate::data_export::windows_private::revalidate_path_identity(path, *expected)
    }

    pub(super) fn validate_existing_target(path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || !metadata.is_file() =>
            {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing to replace a reparse point or non-file private path",
                ))
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Identity;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "owner-only files are unsupported on this platform",
        )
    }

    pub(super) fn create(_path: &Path) -> io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn open(_path: &Path) -> io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn validate(_file: &File) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn identity(_file: &File) -> io::Result<Identity> {
        Err(unsupported())
    }

    pub(super) fn require_path_identity(_path: &Path, _expected: &Identity) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn validate_existing_target(_path: &Path) -> io::Result<()> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).expect("random test suffix");
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        std::env::temp_dir().join(format!(
            "yututui-owner-only-{name}-{}-{suffix}",
            std::process::id()
        ))
    }

    #[test]
    fn bounded_read_and_atomic_overwrite_use_only_complete_contents() {
        let directory = temp_root("overwrite");
        let path = directory.join("vault.key");

        write_owner_only_atomic(&path, b"old private value").expect("write old value");
        assert_eq!(
            read_owner_only_limited(&path, 17).expect("read exact bound"),
            b"old private value"
        );
        assert_eq!(
            read_owner_only_limited(&path, 16)
                .expect_err("reject oversized value")
                .kind(),
            io::ErrorKind::InvalidData
        );

        write_owner_only_atomic(&path, b"new").expect("replace with new value");
        assert_eq!(
            read_owner_only_limited(&path, 3).expect("read replacement"),
            b"new"
        );
        let temp_prefix = format!(
            ".{}.tmp.",
            path.file_name().expect("file name").to_string_lossy()
        );
        assert!(
            fs::read_dir(&directory)
                .expect("read private directory")
                .all(|entry| !entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&temp_prefix))
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn durable_remove_is_idempotent() {
        let directory = temp_root("remove");
        let path = directory.join("credential");
        write_owner_only_atomic(&path, b"secret").expect("write private file");

        remove_owner_only_file_durable(&path).expect("remove private file");
        assert!(!path.exists());
        remove_owner_only_file_durable(&path).expect("repeat missing removal");
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn unix_files_have_exact_owner_only_mode_and_weak_mode_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let directory = temp_root("unix-mode");
        let path = directory.join("device.key");
        write_owner_only_atomic(&path, b"key material").expect("write private file");

        let metadata = fs::symlink_metadata(&path).expect("private metadata");
        assert!(super::super::is_owned_by_current_user(&metadata));
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);

        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("weaken private mode");
        assert_eq!(
            read_owner_only_limited(&path, 1024)
                .expect_err("weak mode must fail closed")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            remove_owner_only_file_durable(&path)
                .expect_err("weak mode removal must fail closed")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("restore mode for cleanup");
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn unix_symlink_is_rejected_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let directory = temp_root("unix-link");
        super::super::ensure_private_dir(&directory).expect("private test directory");
        let target = directory.join("target");
        let link = directory.join("device.key");
        write_owner_only_atomic(&target, b"original").expect("write target");
        symlink(&target, &link).expect("create link");

        assert!(read_owner_only_limited(&link, 1024).is_err());
        assert!(write_owner_only_atomic(&link, b"replacement").is_err());
        assert!(remove_owner_only_file_durable(&link).is_err());
        assert_eq!(
            read_owner_only_limited(&target, 1024).expect("read unchanged target"),
            b"original"
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(windows)]
    #[test]
    fn windows_reparse_point_is_rejected_when_symlinks_are_available() {
        use std::os::windows::fs::symlink_file;

        let directory = temp_root("windows-link");
        super::super::ensure_private_dir(&directory).expect("private test directory");
        let target = directory.join("target");
        let link = directory.join("device.key");
        write_owner_only_atomic(&target, b"original").expect("write target");
        if symlink_file(&target, &link).is_ok() {
            assert!(read_owner_only_limited(&link, 1024).is_err());
            assert!(write_owner_only_atomic(&link, b"replacement").is_err());
            assert!(remove_owner_only_file_durable(&link).is_err());
            assert_eq!(
                read_owner_only_limited(&target, 1024).expect("read unchanged target"),
                b"original"
            );
        }
        let _ = fs::remove_dir_all(directory);
    }
}
