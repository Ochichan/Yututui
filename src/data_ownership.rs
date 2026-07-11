//! Cross-process ownership guard for persisted personal data.
//!
//! Every process that can mutate the config/library stores holds a shared lock for its whole
//! lifetime. A one-shot offline export holds the exclusive form across load, projection, and
//! publication, so an owner cannot start or mutate midway through a supposedly coherent snapshot.
//! The per-store lock files are stable and never unlinked: replacing one while another process
//! holds an open handle would split coordination across two inodes.

use std::ffi::OsString;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

/// A live TUI/daemon's shared claim on the persisted stores.
pub struct OwnerGuard {
    _files: Vec<File>,
}

/// A one-shot offline export's exclusive claim on the persisted stores.
pub struct OfflineExportGuard {
    _files: Vec<File>,
}

/// A lock acquisition can fail because another role owns the data or because the private runtime
/// directory/file could not be opened safely.
#[derive(Debug)]
pub enum AcquireError {
    Busy,
    Io(io::Error),
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => write!(
                formatter,
                "personal data is currently owned by another process"
            ),
            Self::Io(error) => write!(formatter, "could not open the personal-data lock: {error}"),
        }
    }
}

impl std::error::Error for AcquireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Busy => None,
            Self::Io(error) => Some(error),
        }
    }
}

/// Claim the stores for a live owner. This must happen before loading any persisted state.
pub fn acquire_owner() -> Result<OwnerGuard, AcquireError> {
    let files = acquire_locks(true)?;
    Ok(OwnerGuard { _files: files })
}

/// Claim a coherent offline export window. A live primary or secondary owner makes this fail
/// immediately; callers can then delegate to the advertised primary or give actionable guidance.
pub fn acquire_offline_export() -> Result<OfflineExportGuard, AcquireError> {
    let files = acquire_locks(false)?;
    Ok(OfflineExportGuard { _files: files })
}

fn acquire_locks(shared: bool) -> Result<Vec<File>, AcquireError> {
    let paths = ownership_lock_paths().map_err(AcquireError::Io)?;
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let file = open_lock_file(&path).map_err(AcquireError::Io)?;
        if shared {
            file.try_lock_shared().map_err(classify_lock_error)?;
        } else {
            file.try_lock().map_err(classify_lock_error)?;
        }
        files.push(file);
    }
    Ok(files)
}

fn ownership_lock_paths() -> io::Result<Vec<PathBuf>> {
    let directory = ownership_lock_directory()?;
    crate::util::safe_fs::ensure_private_dir(&directory)?;
    ownership_lock_paths_for(
        &directory,
        crate::paths::config_dir().as_deref(),
        crate::paths::data_dir().as_deref(),
    )
}

fn ownership_lock_paths_for(
    directory: &Path,
    config_directory: Option<&Path>,
    data_directory: Option<&Path>,
) -> io::Result<Vec<PathBuf>> {
    Ok(vec![
        directory.join(format!(
            "personal-data-config-{}.lock",
            stable_store_identity("config", config_directory)?
        )),
        directory.join(format!(
            "personal-data-data-{}.lock",
            stable_store_identity("data", data_directory)?
        )),
    ])
}

fn stable_store_identity(label: &str, directory: Option<&Path>) -> io::Result<String> {
    let mut digest = Sha256::new();
    digest.update(label.as_bytes());
    digest.update([0]);
    match directory {
        Some(directory) => update_path_digest(&mut digest, &canonicalize_allow_missing(directory)?),
        None => digest.update(b"<unresolved>"),
    }
    let bytes = digest.finalize();
    let mut out = String::with_capacity(32);
    for byte in &bytes[..16] {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    Ok(out)
}

fn canonicalize_allow_missing(path: &Path) -> io::Result<PathBuf> {
    let mut probe = std::path::absolute(path)?;
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(&probe) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(component) = probe.file_name().map(ToOwned::to_owned) else {
                    return Err(error);
                };
                if !probe.pop() {
                    return Err(error);
                }
                missing.push(component);
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
fn update_path_digest(digest: &mut Sha256, path: &Path) {
    use std::os::unix::ffi::OsStrExt as _;
    digest.update(path.as_os_str().as_bytes());
}

#[cfg(windows)]
fn update_path_digest(digest: &mut Sha256, path: &Path) {
    use std::os::windows::ffi::OsStrExt as _;
    for unit in path.as_os_str().encode_wide() {
        digest.update(unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn update_path_digest(digest: &mut Sha256, path: &Path) {
    digest.update(path.to_string_lossy().as_bytes());
}

fn ownership_lock_directory() -> io::Result<PathBuf> {
    // Unit/subprocess tests can inject one isolated rendezvous without changing any production
    // path rule or touching a user's live ownership lock.
    #[cfg(test)]
    if let Some(directory) = std::env::var_os("YUTUTUI_TEST_OWNERSHIP_DIR") {
        return Ok(PathBuf::from(directory));
    }

    #[cfg(unix)]
    {
        // Unlike sockets, ownership must not follow XDG_RUNTIME_DIR or TMPDIR: two processes may
        // have different launch environments while reading and writing the same persisted stores.
        // `/tmp` is the OS-wide rendezvous base; the effective uid and private 0700 child make the
        // namespace per-account and fail closed if another account pre-creates it.
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        Ok(PathBuf::from("/tmp").join(format!("yututui-{uid}")))
    }

    #[cfg(windows)]
    {
        // `directories` resolves LocalAppData through the Windows known-folder API, independent
        // of USER/USERNAME/TMP environment overrides. The lock opener applies a private DACL.
        directories::ProjectDirs::from("", "", "yututui")
            .map(|directories| directories.data_local_dir().join("ownership"))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "the per-user ownership directory cannot be resolved",
                )
            })
    }

    #[cfg(not(any(unix, windows)))]
    {
        crate::util::runtime::app_runtime_dir()
    }
}

fn classify_lock_error(error: std::fs::TryLockError) -> AcquireError {
    match error {
        std::fs::TryLockError::WouldBlock => AcquireError::Busy,
        std::fs::TryLockError::Error(error) => AcquireError::Io(error),
    }
}

#[cfg(unix)]
fn open_lock_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "personal-data lock is not a regular file",
        ));
    }
    // SAFETY: `geteuid` has no preconditions; ownership is compared with metadata for the
    // already-open, no-follow file before it is trusted as the cross-process rendezvous.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "personal-data lock is not owned by the current user",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(windows)]
fn open_lock_file(path: &Path) -> io::Result<File> {
    crate::data_export::windows_private::open_private_lock_file(path)
}

#[cfg(not(any(unix, windows)))]
fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_lock_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "yututui-personal-data-lock-{}-{}",
            std::process::id(),
            crate::remote::endpoint::gen_token().expect("random test suffix")
        ))
    }

    #[test]
    fn shared_owners_exclude_offline_export_until_all_drop() {
        let path = unique_lock_path();
        let first = open_lock_file(&path).expect("first lock file");
        first.try_lock_shared().expect("first owner lock");
        let second = open_lock_file(&path).expect("second lock file");
        second.try_lock_shared().expect("second owner lock");

        let exclusive = open_lock_file(&path).expect("exclusive contender");
        assert!(matches!(
            exclusive.try_lock().expect_err("owners exclude export"),
            std::fs::TryLockError::WouldBlock
        ));

        drop(first);
        assert!(matches!(
            exclusive
                .try_lock()
                .expect_err("one remaining owner excludes export"),
            std::fs::TryLockError::WouldBlock
        ));
        drop(second);
        exclusive.try_lock().expect("export lock after owners exit");
        drop(exclusive);
        std::fs::remove_file(path).expect("cleanup lock fixture");
    }

    #[test]
    fn offline_export_excludes_new_owner() {
        let path = unique_lock_path();
        let exclusive = open_lock_file(&path).expect("exclusive lock file");
        exclusive.try_lock().expect("offline export lock");
        let owner = open_lock_file(&path).expect("owner contender");
        assert!(matches!(
            owner.try_lock_shared().expect_err("export excludes owner"),
            std::fs::TryLockError::WouldBlock
        ));
        drop(exclusive);
        owner.try_lock_shared().expect("owner lock after export");
        drop(owner);
        std::fs::remove_file(path).expect("cleanup lock fixture");
    }

    #[test]
    fn ownership_sets_overlap_for_each_shared_store_and_separate_distinct_profiles() {
        let root = std::env::temp_dir().join(format!(
            "yututui-ownership-identities-{}-{}",
            std::process::id(),
            crate::remote::endpoint::gen_token().expect("random test suffix")
        ));
        std::fs::create_dir(&root).expect("identity root");
        let first = ownership_lock_paths_for(
            &root,
            Some(&root.join("config-a")),
            Some(&root.join("data-a")),
        )
        .expect("first lock set");
        let shared_config = ownership_lock_paths_for(
            &root,
            Some(&root.join("config-a")),
            Some(&root.join("data-b")),
        )
        .expect("shared-config lock set");
        let distinct = ownership_lock_paths_for(
            &root,
            Some(&root.join("config-b")),
            Some(&root.join("data-b")),
        )
        .expect("distinct lock set");

        assert_eq!(first[0], shared_config[0]);
        assert_ne!(first[1], shared_config[1]);
        assert_ne!(first[0], distinct[0]);
        assert_ne!(first[1], distinct[1]);
        std::fs::remove_dir(root).expect("cleanup identity root");
    }

    #[cfg(unix)]
    #[test]
    fn environment_mismatch_still_contends_on_one_ownership_lock() {
        use std::process::Command;

        let root = std::env::temp_dir().join(format!(
            "yututui-ownership-env-{}-{}",
            std::process::id(),
            crate::remote::endpoint::gen_token().expect("random test suffix")
        ));
        crate::util::safe_fs::ensure_private_dir(&root).expect("private test directory");
        let config_directory = root.join("profile-config");
        let data_directory = root.join("profile-data");
        let lock_paths =
            ownership_lock_paths_for(&root, Some(&config_directory), Some(&data_directory))
                .expect("ownership lock paths");
        let owners: Vec<File> = lock_paths
            .iter()
            .map(|path| {
                let owner = open_lock_file(path).expect("owner lock file");
                owner.try_lock_shared().expect("owner lock");
                owner
            })
            .collect();
        let secure_xdg = root.join("different-xdg-runtime");
        crate::util::safe_fs::ensure_private_dir(&secure_xdg).expect("secure XDG fixture");

        for xdg in [None, Some(secure_xdg.as_path())] {
            let mut command = Command::new(std::env::current_exe().expect("test executable"));
            command
                .args([
                    "--exact",
                    "data_ownership::tests::subprocess_offline_lock_probe",
                    "--nocapture",
                ])
                .env("YUTUTUI_TEST_OWNERSHIP_DIR", &root)
                .env("YUTUTUI_OWNERSHIP_SUBPROCESS", "1")
                .env("YTM_CONFIG_DIR", &config_directory)
                .env("YTM_DATA_DIR", &data_directory)
                .env("TMPDIR", root.join("different-temp"))
                .env("USER", "different-user-environment");
            if let Some(xdg) = xdg {
                command.env("XDG_RUNTIME_DIR", xdg);
            } else {
                command.env_remove("XDG_RUNTIME_DIR");
            }
            let output = command.output().expect("run ownership contender");
            assert!(
                output.status.success(),
                "subprocess did not observe the shared owner locks:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        drop(owners);
        for path in lock_paths {
            std::fs::remove_file(path).expect("cleanup lock fixture");
        }
        std::fs::remove_dir(secure_xdg).expect("cleanup XDG fixture");
        std::fs::remove_dir(root).expect("cleanup test directory");
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_offline_lock_probe() {
        if std::env::var_os("YUTUTUI_OWNERSHIP_SUBPROCESS").is_none() {
            return;
        }
        assert!(matches!(acquire_offline_export(), Err(AcquireError::Busy)));
    }
}
