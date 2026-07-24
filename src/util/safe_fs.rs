//! Small filesystem primitives for private app state.
//!
//! Unix gets the security properties the callers care about: private directories are `0700`,
//! private files are `0600`, and reads/appends use `O_NOFOLLOW` so a symlink is rejected instead
//! of followed. Windows rejects reparse-point final paths for the same private state APIs.

mod owner_only;
mod pinned;
mod recovery_backups;
pub use owner_only::*;
pub(crate) use pinned::{OwnedGeneration, PinnedDir};
use recovery_backups::backup_with_label;
pub use recovery_backups::{
    RecoveryBackup, SECRET_BACKUP_RETENTION, backup_aside, backup_aside_secret, backup_too_large,
    backup_too_large_secret, backup_unreadable_secret, enforce_secret_backup_retention,
    recovery_backups,
};
#[cfg(test)]
use recovery_backups::{backup_by_copy, backup_path};

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

#[cfg(all(windows, test))]
const SOURCE_DURABILITY_MARKER: &str = ".ytt-recorder-source-durable";

static DO_NOT_CLOBBER: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();

#[cfg(not(test))]
#[derive(Clone, Debug, PartialEq, Eq)]
enum ProcessMutationAccess {
    Writable,
    ReadOnly(Arc<str>),
}

#[cfg(not(test))]
static PROCESS_MUTATION_ACCESS: OnceLock<ProcessMutationAccess> = OnceLock::new();
#[cfg(not(test))]
static PROCESS_MUTATION_REVOKED: AtomicBool = AtomicBool::new(false);
#[cfg(not(test))]
static PROCESS_MUTATION_REVOKE_REASON: OnceLock<Arc<str>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static PROCESS_MUTATION_REVOKE_REASON: std::cell::RefCell<Option<std::sync::Arc<str>>> =
        const { std::cell::RefCell::new(None) };
}

/// Fix the process-wide durable mutation capability immediately after the persistence root lease
/// is decided. This guard deliberately sits below individual stores: artwork, recorder, scrobble,
/// managed-tool, update, AI-usage, transfer, and download metadata writers all share these
/// primitives even though they are not members of the eight-store persistence actor.
#[cfg(not(test))]
pub(crate) fn configure_process_mutations(access: Result<(), Arc<str>>) -> io::Result<()> {
    let desired = match access {
        Ok(()) => ProcessMutationAccess::Writable,
        Err(reason) => ProcessMutationAccess::ReadOnly(reason),
    };
    match PROCESS_MUTATION_ACCESS.set(desired.clone()) {
        Ok(()) => Ok(()),
        Err(_) if PROCESS_MUTATION_ACCESS.get() == Some(&desired) => Ok(()),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "process durable mutation capability was already initialized differently",
        )),
    }
}

/// Permanently revoke durable mutation after startup recovery loses a coherent frontier.
/// Coordination-only helpers deliberately bypass this gate so an endpoint or lease can still
/// clean up its own exact identity while the process aborts.
pub(crate) fn revoke_process_mutations(reason: std::sync::Arc<str>) {
    #[cfg(not(test))]
    {
        let _ = PROCESS_MUTATION_REVOKE_REASON.set(reason);
        PROCESS_MUTATION_REVOKED.store(true, Ordering::Release);
    }
    #[cfg(test)]
    PROCESS_MUTATION_REVOKE_REASON.with(|latched| {
        let mut latched = latched.borrow_mut();
        if latched.is_none() {
            *latched = Some(reason);
        }
    });
}

#[cfg(test)]
pub(crate) fn clear_process_mutation_revoke_for_test() {
    PROCESS_MUTATION_REVOKE_REASON.with(|reason| *reason.borrow_mut() = None);
}

fn ensure_process_mutation_allowed() -> io::Result<()> {
    #[cfg(test)]
    {
        PROCESS_MUTATION_REVOKE_REASON.with(|reason| match reason.borrow().as_ref() {
            Some(reason) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("durable mutation was revoked after recovery failure: {reason}"),
            )),
            None => Ok(()),
        })
    }
    #[cfg(not(test))]
    {
        if PROCESS_MUTATION_REVOKED.load(Ordering::Acquire) {
            let reason = PROCESS_MUTATION_REVOKE_REASON
                .get()
                .map_or("startup recovery failed", |reason| reason.as_ref());
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("durable mutation was revoked after recovery failure: {reason}"),
            ));
        }
        match PROCESS_MUTATION_ACCESS.get() {
            Some(ProcessMutationAccess::Writable) => Ok(()),
            Some(ProcessMutationAccess::ReadOnly(reason)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("durable mutation is disabled for this process: {reason}"),
            )),
            None => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "durable mutation is disabled until the persistence writer lease is initialized",
            )),
        }
    }
}

fn do_not_clobber() -> &'static Mutex<Vec<PathBuf>> {
    DO_NOT_CLOBBER.get_or_init(|| Mutex::new(Vec::new()))
}

fn mark_do_not_clobber(path: &Path) {
    let mut guard = do_not_clobber()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !guard.iter().any(|p| p == path) {
        guard.push(path.to_path_buf());
    }
}

fn is_do_not_clobber(path: &Path) -> bool {
    do_not_clobber()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|p| p == path)
}

fn clear_do_not_clobber(path: &Path) {
    do_not_clobber()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|p| p != path);
}

#[cfg(unix)]
fn private_dir_mode() -> u32 {
    PRIVATE_DIR_MODE
}

#[cfg(unix)]
fn private_file_mode() -> u32 {
    PRIVATE_FILE_MODE
}

#[cfg(unix)]
pub fn is_owned_by_current_user(meta: &fs::Metadata) -> bool {
    // SAFETY: `geteuid` has no preconditions and cannot fail; the returned uid is
    // only compared against metadata ownership.
    let euid = unsafe { libc::geteuid() };
    meta.uid() == euid
}

#[cfg(unix)]
fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing symlink path {}", path.display()),
        )),
        Ok(_) | Err(_) => Ok(()),
    }
}

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[cfg(any(windows, test))]
fn validate_windows_change_time(change_time: i64) -> io::Result<i64> {
    if change_time > 0 {
        Ok(change_time)
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "filesystem did not provide a usable change time",
        ))
    }
}

#[cfg(windows)]
fn windows_change_time(file: &File) -> io::Result<i64> {
    use std::mem::{MaybeUninit, size_of};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx,
    };

    let mut info = MaybeUninit::<FILE_BASIC_INFO>::uninit();
    // SAFETY: `file` owns a valid handle, `info` points to writable storage of exactly the size
    // passed, and the buffer is read only after Win32 reports success.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a nonzero result guarantees Win32 initialized the FILE_BASIC_INFO buffer.
    let change_time = unsafe { info.assume_init() }.ChangeTime;
    validate_windows_change_time(change_time)
}

/// Query the Windows change timestamp for the directory currently bound to `path`. The handle is
/// deliberately transient: retaining a share-delete handle can keep observing an old directory
/// after an ancestor of `path` is atomically replaced.
#[cfg(windows)]
pub(crate) fn windows_directory_change_time(path: &Path) -> io::Result<i64> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    windows_change_time(&file)
}

/// Query one artifact's Windows change timestamp through a transient handle. The handle is
/// dropped before this function returns, so fingerprinting never pins imported files open.
#[cfg(windows)]
pub(crate) fn windows_file_change_time(path: &Path) -> io::Result<i64> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };

    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    windows_change_time(&file)
}

#[cfg(windows)]
fn is_windows_reparse_point(meta: &fs::Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if is_windows_reparse_point(&meta) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing reparse-point path {}", path.display()),
        )),
        Ok(_) | Err(_) => Ok(()),
    }
}

#[cfg(not(any(unix, windows)))]
fn reject_symlink(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Ensure `path` exists as a private app directory.
pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    ensure_private_coordination_dir(path)
}

/// Create a private directory for the writer-lease or short-lived runtime coordination plane.
///
/// This is the only directory-creation bypass before the process mutation capability is fixed.
/// It must not be used for application data, caches, downloads, or recovery artifacts.
pub(crate) fn ensure_private_coordination_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing symlink or non-directory private path {}",
                    path.display()
                ),
            ));
        }
        if !is_owned_by_current_user(&meta) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "private path is not owned by current user: {}",
                    path.display()
                ),
            ));
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode != private_dir_mode() {
            fs::set_permissions(path, fs::Permissions::from_mode(private_dir_mode()))?;
        }
    }
    #[cfg(windows)]
    {
        let meta = fs::symlink_metadata(path)?;
        if is_windows_reparse_point(&meta) || !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing symlink or non-directory private path {}",
                    path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Create a private directory tree one component at a time and durably publish every new
/// component before creating its child. This is the directory equivalent of an atomic file
/// publication boundary: a synced file is not considered crash-durable if one of its newly
/// created ancestors can still disappear after power loss.
pub fn ensure_private_dir_durable(path: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    ensure_dir_durable_with(path, true, sync_parent_dir)
}

/// Create a directory tree without changing user-selected permissions and durably publish every
/// newly created component. Existing symlinked directories remain supported for configured output
/// paths; a component that was missing when this operation began must be a real directory.
pub fn ensure_dir_durable(path: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    ensure_dir_durable_with(path, false, sync_parent_dir)
}

fn ensure_dir_durable_with<S>(path: &Path, private: bool, mut sync_parent: S) -> io::Result<()>
where
    S: FnMut(&Path) -> io::Result<()>,
{
    let mut missing = Vec::new();
    let mut cursor = path;
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(_) => {
                if missing.is_empty() && private {
                    ensure_private_coordination_dir(cursor)?;
                } else if !fs::metadata(cursor)?.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        format!("directory path is not a directory: {}", cursor.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(cursor.to_path_buf());
                cursor = cursor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "private directory has no existing ancestor: {}",
                            path.display()
                        ),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }

    for component in missing.into_iter().rev() {
        match fs::create_dir(&component) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        if private {
            ensure_private_coordination_dir(&component)?;
        } else {
            let metadata = fs::symlink_metadata(&component)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refusing non-directory component {}", component.display()),
                ));
            }
        }
        sync_parent(&component)?;
    }
    Ok(())
}

/// Open (or create) a private, persistent lock file.
///
/// The caller owns only the returned file handle; the path is deliberately never unlinked on
/// unlock. Kernel advisory locks attach ownership to this handle, so leaving the inode in place
/// avoids the classic race where one process removes and recreates a lock path while another
/// process still owns the old inode.
pub fn open_private_lock_file(path: &Path) -> io::Result<File> {
    if let Some(dir) = path.parent() {
        ensure_private_coordination_dir(dir)?;
    }
    reject_symlink(path)?;
    #[cfg(unix)]
    {
        let mut opts = OpenOptions::new();
        opts.read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(private_file_mode())
            .custom_flags(libc::O_NOFOLLOW);
        let file = opts.open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(private_file_mode()))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    }
}

/// RAII owner of a non-blocking exclusive advisory lock on a private lock file.
///
/// The persistent path is never removed. Unix and Windows explicitly unlock before `_file`
/// closes: a duplicated or inherited descriptor can outlive this guard, so relying on the final
/// close alone can retain the lock past the guard's lifetime.
#[derive(Debug)]
pub struct AdvisoryFileLock {
    _file: File,
}

/// Try to own `path` without blocking. `Ok(None)` means another live process owns the lock.
pub fn try_lock_private_file(path: &Path) -> io::Result<Option<AdvisoryFileLock>> {
    let file = open_private_lock_file(path)?;
    if try_lock_file(&file)? {
        Ok(Some(AdvisoryFileLock { _file: file }))
    } else {
        Ok(None)
    }
}

#[cfg(unix)]
fn try_lock_file(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: the fd belongs to `file` for this call; flock returns errors via errno.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) || error.raw_os_error() == Some(libc::EAGAIN)
    {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn try_lock_file(file: &File) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    // SAFETY: Win32 defines an all-zero OVERLAPPED as offset zero with no event handle, which is
    // the required synchronous byte-range descriptor for this call.
    let mut overlapped = unsafe { std::mem::zeroed() };
    // SAFETY: the handle and OVERLAPPED live for the call; byte range 0..1 is stable for every
    // holder and FAIL_IMMEDIATELY makes contention non-blocking.
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if locked != 0 {
        Ok(true)
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn try_lock_file(_file: &File) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private advisory locking is unsupported on this platform",
    ))
}

#[cfg(windows)]
impl Drop for AdvisoryFileLock {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
        // SAFETY: this recreates the same all-zero offset-zero byte range used for acquisition.
        let mut overlapped = unsafe { std::mem::zeroed() };
        // SAFETY: this guard owns the locked handle and unlocks exactly the range acquired above.
        let _ = unsafe { UnlockFileEx(self._file.as_raw_handle() as _, 0, 1, 0, &mut overlapped) };
    }
}

#[cfg(unix)]
impl Drop for AdvisoryFileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        // SAFETY: `_file` owns a live descriptor for this call. Explicit LOCK_UN is necessary
        // because duplicated descriptors share the same open file description and may outlive
        // this guard; closing only this descriptor would leave the flock held.
        let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn random_suffix() -> io::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let mut out = String::with_capacity(16);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap_or('0'));
    }
    Ok(out)
}

pub(crate) fn temp_path_for(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");
    Ok(path.with_file_name(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        random_suffix()?
    )))
}

/// Create an unpredictable, same-directory private staging file without ever opening an
/// existing name. Random collisions are retried, but any other filesystem error is returned.
pub(crate) fn create_private_temp_for(path: &Path) -> io::Result<(PathBuf, File)> {
    ensure_process_mutation_allowed()?;
    const ATTEMPTS: usize = 8;
    for _ in 0..ATTEMPTS {
        let temp = temp_path_for(path)?;
        match create_private_file(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not allocate a collision-free staging path for {}",
            path.display()
        ),
    ))
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub(crate) fn sync_parent_dir(path: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    match path.parent() {
        Some(parent) => sync_dir(parent),
        None => Ok(()),
    }
}

/// Durably remove a private state file.
///
/// A missing file is still followed by a parent-directory sync. This makes retry after an
/// ambiguous post-unlink sync failure idempotent: the retry can establish the directory
/// durability boundary even though the name is already absent.
pub fn remove_private_file_durable(path: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    reject_symlink(path)?;
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    sync_parent_dir(path)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true)
        .write(true)
        .create_new(true)
        .mode(private_file_mode())
        .custom_flags(libc::O_NOFOLLOW);
    opts.open(path)
}

#[cfg(windows)]
fn create_private_file(path: &Path) -> io::Result<File> {
    reject_symlink(path)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn create_private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

trait AtomicWriteOps {
    fn create(&self, path: &Path) -> io::Result<File>;
    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()>;
    fn sync_file(&self, file: &File) -> io::Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn sync_parent(&self, path: &Path) -> io::Result<()>;
}

struct RealAtomicWriteOps;

#[cfg(windows)]
fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains a NUL character: {}", path.display()),
        ));
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(windows)]
pub(crate) fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from = wide_path(from)?;
    let to = wide_path(to)?;
    // SAFETY: both UTF-16 buffers are NUL-terminated and live through the call. The temp and
    // target are in the same private directory, and REPLACE_EXISTING gives Windows the atomic
    // overwrite semantics that `std::fs::rename` does not provide there.
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
pub(crate) fn atomic_replace(from: &Path, to: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    fs::rename(from, to)
}

/// Publish a complete same-volume staged file without replacing an existing destination.
///
/// Unix hard-links the stage so its private name remains available until the caller durably
/// unlinks it. Windows atomically moves the stage with no replace flag; `WRITE_THROUGH` requests
/// the strongest available metadata boundary before returning.
#[cfg(all(not(windows), test))]
pub(crate) fn install_file_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    fs::hard_link(from, to)
}

#[cfg(all(windows, test))]
pub(crate) fn install_file_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let from = wide_path(from)?;
    let to = wide_path(to)?;
    // SAFETY: both UTF-16 buffers are NUL-terminated and remain alive for the call. Omitting
    // MOVEFILE_REPLACE_EXISTING makes an external destination collision fail atomically.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl AtomicWriteOps for RealAtomicWriteOps {
    fn create(&self, path: &Path) -> io::Result<File> {
        create_private_file(path)
    }

    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }

    fn sync_file(&self, file: &File) -> io::Result<()> {
        file.sync_all()
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        atomic_replace(from, to)
    }

    fn sync_parent(&self, path: &Path) -> io::Result<()> {
        sync_parent_dir(path)
    }
}

#[cfg(unix)]
fn open_private_append(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true)
        .create(true)
        .append(true)
        .mode(private_file_mode())
        .custom_flags(libc::O_NOFOLLOW);
    let file = opts.open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(private_file_mode()))?;
    Ok(file)
}

#[cfg(windows)]
fn open_private_append(path: &Path) -> io::Result<File> {
    reject_symlink(path)?;
    OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_private_append(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .create(true)
        .append(true)
        .open(path)
}

/// Atomically replace `path` with private file contents.
///
/// The rename happens before the parent directory is synced. If that final sync fails, this
/// returns an error even though the new contents may already be visible; their durability is
/// uncertain and callers should retry according to their persistence policy.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_private_atomic_with_ops(path, bytes, &RealAtomicWriteOps)
}

fn write_private_atomic_with_ops<O: AtomicWriteOps>(
    path: &Path,
    bytes: &[u8],
    ops: &O,
) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    reject_symlink(path)?;
    if is_do_not_clobber(path) {
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to overwrite {} after recovery backup failed",
                    path.display()
                ),
            ));
        }
        clear_do_not_clobber(path);
    }
    let tmp = temp_path_for(path)?;
    let write_result = (|| {
        let mut file = ops.create(&tmp)?;
        ops.write_all(&mut file, bytes)?;
        ops.sync_file(&file)?;
        drop(file);
        #[cfg(unix)]
        fs::set_permissions(&tmp, fs::Permissions::from_mode(private_file_mode()))?;
        ops.rename(&tmp, path)?;
        ops.sync_parent(path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

/// Serialize `value` as pretty JSON and atomically replace `path` with a private file.
pub fn write_private_atomic_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_private_atomic(path, &json)
}

#[cfg(unix)]
fn open_no_symlink(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).custom_flags(libc::O_NOFOLLOW);
    opts.open(path)
}

#[cfg(windows)]
fn open_no_symlink(path: &Path) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if is_windows_reparse_point(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing reparse-point path {}", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_no_symlink(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

/// Open a regular file for reading, rejecting symlinks/reparse points.
pub(crate) fn open_regular_no_symlink(path: &Path) -> io::Result<File> {
    let file = open_no_symlink(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    Ok(file)
}

/// Inspect a regular file without following a final symlink/reparse point.
pub(crate) fn metadata_no_symlink(path: &Path) -> io::Result<fs::Metadata> {
    open_regular_no_symlink(path)?.metadata()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeSpace {
    pub available_bytes: u64,
    pub total_bytes: u64,
}

/// User-available and total bytes on the volume containing `path`.
#[cfg(unix)]
pub fn volume_space(path: &Path) -> io::Result<VolumeSpace> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut stat = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` points to writable storage of the required
    // size. A successful return initializes the complete `statvfs` value.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the zero return above guarantees initialization.
    let stat = unsafe { stat.assume_init() };
    let block = if stat.f_frsize == 0 {
        stat.f_bsize
    } else {
        stat.f_frsize
    } as u128;
    Ok(VolumeSpace {
        available_bytes: u64::try_from(u128::from(stat.f_bavail) * block).unwrap_or(u64::MAX),
        total_bytes: u64::try_from(u128::from(stat.f_blocks) * block).unwrap_or(u64::MAX),
    })
}

#[cfg(windows)]
pub fn volume_space(path: &Path) -> io::Result<VolumeSpace> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let path = wide_path(path)?;
    let (mut available, mut total) = (0u64, 0u64);
    // SAFETY: `path` is NUL-terminated and the two output pointers are live `u64` storage;
    // the unused third result is explicitly null.
    if unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut available,
            &mut total,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(VolumeSpace {
        available_bytes: available,
        total_bytes: total,
    })
}

#[cfg(all(windows, test))]
pub(crate) fn open_regular_for_sync_no_symlink(path: &Path) -> io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = OpenOptions::new();
    options
        .read(true)
        // `File::sync_all` maps to FlushFileBuffers, whose Win32 contract requires a handle
        // opened with GENERIC_WRITE. Keep this privilege isolated from ordinary read helpers.
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        // Inspect the final path itself rather than following a junction/symlink between a
        // path-level preflight and CreateFileW.
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if is_windows_reparse_point(&metadata) || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing non-regular or reparse-point path {}",
                path.display()
            ),
        ));
    }
    Ok(file)
}

#[cfg(all(windows, test))]
fn sync_source_parent_publication(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("recording source has no parent: {}", path.display()),
        )
    })?;
    // Windows does not document FlushFileBuffers for directory handles. Publish one deterministic
    // tiny marker in the *source's own parent* instead: `write_private_atomic` flushes its file,
    // then MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) establishes the same-
    // directory metadata barrier without ever renaming the source. One marker per owner namespace
    // is bounded and disappears with that namespace's normal lifecycle cleanup.
    write_private_atomic(&parent.join(SOURCE_DURABILITY_MARKER), b"v1\n")
}

/// Flush an existing regular recorder source without following a final symlink/reparse point.
/// Unix additionally fsyncs the parent directory. Windows uses a GENERIC_WRITE, open-reparse
/// handle for FlushFileBuffers, then a bounded same-parent marker published through
/// MoveFileExW(MOVEFILE_WRITE_THROUGH) because Windows has no documented directory-fsync API.
#[cfg(all(windows, test))]
pub(crate) fn sync_regular_file_durable(path: &Path) -> io::Result<()> {
    sync_regular_file_contents(path)?;
    sync_source_parent_publication(path)
}

/// Flush only the contents of an existing regular file without following a final
/// symlink/reparse point. Callers that publish the file with their own durable directory
/// primitive can use this without creating the Windows source-parent publication marker.
#[cfg(all(windows, test))]
pub(crate) fn sync_regular_file_contents(path: &Path) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    let file = open_regular_for_sync_no_symlink(path)?;
    file.sync_all()?;
    Ok(())
}

/// Stable kernel identity for one open regular file. Content equality is deliberately not a
/// substitute: callers use this to prove that two names reference the same inode/file record
/// before removing either name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileObjectId {
    volume: u64,
    file: u64,
}

#[cfg(unix)]
pub(crate) fn file_object_id(file: &File) -> io::Result<FileObjectId> {
    let metadata = file.metadata()?;
    Ok(FileObjectId {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}

#[cfg(windows)]
pub(crate) fn file_object_id(file: &File) -> io::Result<FileObjectId> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    // SAFETY: the structure is plain Win32 output storage and is initialized by the API before
    // any field is inspected.
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `file` owns a live handle for this call and `information` is writable for the full
    // BY_HANDLE_FILE_INFORMATION result.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileObjectId {
        volume: u64::from(information.dwVolumeSerialNumber),
        file: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn file_object_id(_file: &File) -> io::Result<FileObjectId> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "stable file identity is unsupported on this platform",
    ))
}

/// Read a regular file, rejecting symlinks on Unix.
pub fn read_no_symlink(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = open_regular_no_symlink(path)?;
    let mut out = Vec::new();
    file.read_to_end(&mut out)?;
    Ok(out)
}

pub fn read_to_string_no_symlink(path: &Path) -> io::Result<String> {
    String::from_utf8(read_no_symlink(path)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Read at most `max_bytes`, rejecting symlinks on Unix and oversized regular files.
pub fn read_no_symlink_limited(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = open_regular_no_symlink(path)?;
    let meta = file.metadata()?;
    if meta.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file too large: {}", path.display()),
        ));
    }
    let mut out = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut out)?;
    if out.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file too large: {}", path.display()),
        ));
    }
    Ok(out)
}

/// Read a bounded private file without repairing or creating any part of its path.
///
/// On Unix the already-existing parent must be a current-user `0700` directory and the opened
/// final inode must be a current-user `0600` regular file. The final open uses `O_NOFOLLOW`, and
/// ownership/mode/size checks are made against that opened inode. Windows applies the equivalent
/// no-reparse-point and regular-file checks available to this module. This is intentionally a
/// validator only: unlike [`ensure_private_dir`], it never changes permissions.
pub(crate) fn read_private_file_limited(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("private file has no parent: {}", path.display()),
        )
    })?;
    let parent_meta = fs::symlink_metadata(parent)?;

    #[cfg(unix)]
    if !parent_meta.is_dir()
        || parent_meta.file_type().is_symlink()
        || !is_owned_by_current_user(&parent_meta)
        || parent_meta.permissions().mode() & 0o7777 != private_dir_mode()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "private parent is not current-user 0700: {}",
                parent.display()
            ),
        ));
    }

    #[cfg(windows)]
    if !parent_meta.is_dir() || is_windows_reparse_point(&parent_meta) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing non-private parent: {}", parent.display()),
        ));
    }

    #[cfg(not(any(unix, windows)))]
    if !parent_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing non-directory parent: {}", parent.display()),
        ));
    }

    let file = open_no_symlink(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("not a private regular file: {}", path.display()),
        ));
    }

    #[cfg(windows)]
    if is_windows_reparse_point(&meta) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing private file reparse point: {}", path.display()),
        ));
    }

    #[cfg(unix)]
    if !is_owned_by_current_user(&meta) || meta.permissions().mode() & 0o7777 != private_file_mode()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("private file is not current-user 0600: {}", path.display()),
        ));
    }

    if meta.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file too large: {}", path.display()),
        ));
    }
    let mut out = Vec::with_capacity(meta.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut out)?;
    if out.len() as u64 > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file too large: {}", path.display()),
        ));
    }
    Ok(out)
}

/// Append one JSONL line to a private file.
///
/// A non-empty unterminated tail is separated first so a record interrupted by a crash cannot
/// consume the next record. Callers remain responsible for serializing higher-level operations
/// (such as append versus compaction) for each path.
pub fn append_private_jsonl(path: &Path, line: &str) -> io::Result<()> {
    append_private_jsonl_inner(path, line, false)
}

/// Append one JSONL line and force the file and parent directory to storage before returning.
pub fn append_private_jsonl_durable(path: &Path, line: &str) -> io::Result<()> {
    append_private_jsonl_inner(path, line, true)
}

fn append_private_jsonl_inner(path: &Path, line: &str, durable: bool) -> io::Result<()> {
    ensure_process_mutation_allowed()?;
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    let mut file = open_private_append(path)?;
    let needs_separator = if file.seek(SeekFrom::End(0))? != 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut tail = [0];
        file.read_exact(&mut tail)?;
        tail[0] != b'\n'
    } else {
        false
    };
    let payload_len = line
        .len()
        .checked_add(1 + usize::from(needs_separator))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "JSONL record is too large"))?;
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(payload_len)
        .map_err(io::Error::other)?;
    if needs_separator {
        payload.push(b'\n');
    }
    payload.extend_from_slice(line.as_bytes());
    payload.push(b'\n');
    file.write_all(&payload)?;
    if durable {
        file.sync_all()?;
        sync_parent_dir(path)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Schema-drift-tolerant JSON loading
//
// Persisted state (config, play history, caches) is read by whatever build is
// installed today, but was written by whatever build was installed last time. A
// strict `from_str::<T>` fails the instant ONE field's stored value no longer
// deserializes — a renamed enum variant, a retyped field, an un-aliased rename —
// after which the caller falls back to `T::default()`, wiping the WHOLE file. On a
// `.app` reinstall (new binary reading the previous install's files) that reads as
// "all my settings and history reset again."
//
// `recover_lenient` bounds the blast radius: it keeps every field that still fits
// and defaults only the specific incompatible leaves (recursing into nested objects
// and pruning individual bad elements out of arrays), so a single schema change can
// never again reset an entire file.
// ---------------------------------------------------------------------------

/// Element-wise array recovery costs one `T` deserialization per element; skip it for
/// pathologically large arrays and keep the (empty) default instead of stalling startup.
const MAX_ELEMENTWISE_ARRAY: usize = 20_000;

/// Total budget of `T` deserialization attempts across a whole [`recover_lenient`] pass. The
/// per-array [`MAX_ELEMENTWISE_ARRAY`] cap bounds a single array; this bounds the cross-field
/// `O(fields × elements)` accumulation so a crafted-but-under-size-cap file (many fields, each
/// a large drifted array) can't burn startup CPU. Genuine schema-drift recovery only touches
/// the drifted leaves and stays far under this.
const MAX_RECOVERY_DESERIALIZES: usize = 50_000;

/// Load `T` from `path` with schema-drift recovery.
///
/// - missing / unreadable / not a regular file → `T::default()`
/// - valid JSON (even if schema-drifted) → recovered value, preserving all still-valid fields
/// - present but not valid JSON at all → the file is moved aside to `*.corrupt.bak` (so the
///   next `save` cannot silently destroy it) and `T::default()` is returned
pub fn load_json_or_default<T>(path: &Path) -> T
where
    T: DeserializeOwned + Serialize + Default,
{
    let text = match read_to_string_no_symlink(path) {
        Ok(text) => text,
        // A genuinely missing file has nothing to protect. A present-but-unreadable one
        // (permission error, invalid UTF-8, not a regular file, …) must not be silently
        // replaced by the next default `save`, so set it aside first.
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                let _ = backup_with_label(path, "unreadable");
            }
            return T::default();
        }
    };
    // Fast path: byte-for-byte the previous behaviour when nothing drifted.
    if let Ok(value) = serde_json::from_str::<T>(&text) {
        return value;
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => recover_lenient::<T>(value),
        Err(_) => {
            let _ = backup_aside(path);
            T::default()
        }
    }
}

/// Like [`load_json_or_default`] but bounds the read at `max_bytes`. A file larger than the
/// cap is moved aside to `*.too-large.bak` (so the next `save` can't silently destroy it)
/// and `T::default()` is returned — an oversized or corrupted-huge state file can neither
/// stall startup nor spike memory before the schema-drift recovery even runs. The trust
/// boundary is low (same-user data dir), but sync tools, other same-user processes, and
/// user mistakes can all leave a giant file behind.
///
/// - missing / unreadable / symlink / not a regular file → `T::default()`
/// - larger than `max_bytes` → moved to `*.too-large.bak`, `T::default()`
/// - otherwise identical to [`load_json_or_default`] (fast path, then schema-drift recovery)
pub fn load_json_or_default_limited<T>(path: &Path, max_bytes: u64) -> T
where
    T: DeserializeOwned + Serialize + Default,
{
    let bytes = match read_no_symlink_limited(path, max_bytes) {
        Ok(bytes) => bytes,
        // `read_no_symlink_limited` reports oversize as `InvalidData`; set it aside so a fresh
        // default cannot clobber it, mirroring the unparseable-JSON path.
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            let _ = backup_with_label(path, "too-large");
            return T::default();
        }
        // Missing → nothing to protect. Present-but-unreadable (permission, not a regular
        // file, …) → set it aside so the next default save can't clobber intact data.
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                let _ = backup_with_label(path, "unreadable");
            }
            return T::default();
        }
    };
    let Ok(text) = String::from_utf8(bytes) else {
        let _ = backup_with_label(path, "unreadable");
        return T::default();
    };
    if let Ok(value) = serde_json::from_str::<T>(&text) {
        return value;
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(value) => recover_lenient::<T>(value),
        Err(_) => {
            let _ = backup_aside(path);
            T::default()
        }
    }
}

/// Deserialize `T` from an in-memory JSON `value`, preserving every field that still fits
/// and defaulting only the leaves that no longer deserialize. See the module note above.
pub fn recover_lenient<T>(value: Value) -> T
where
    T: DeserializeOwned + Serialize + Default,
{
    // Fast path — nothing drifted.
    if let Ok(value) = T::deserialize(&value) {
        return value;
    }
    // Graft the on-disk value onto defaults (always valid) key by key, keeping only what
    // still lets the whole thing deserialize as `T`.
    let Ok(mut root) = serde_json::to_value(T::default()) else {
        return T::default();
    };
    if let (Value::Object(_), Value::Object(incoming)) = (&root, &value) {
        let incoming = incoming.clone();
        let mut path: Vec<String> = Vec::new();
        let mut budget: usize = MAX_RECOVERY_DESERIALIZES;
        for (key, sub) in &incoming {
            path.push(key.clone());
            graft::<T>(&mut root, &mut path, sub, &mut budget);
            path.pop();
        }
    }
    T::deserialize(&root).unwrap_or_default()
}

/// Fold on-disk `incoming` into `root` at `path`. Invariant: on entry `root` deserializes as
/// `T`; on exit it still does, now holding the richest value at `path` that keeps that true.
/// Sibling keys are all still at their (valid) defaults or already-grafted-valid state, so
/// only the key at `path` can introduce invalidity — which is what makes the single-key /
/// single-element validity checks below accurate.
fn graft<T>(root: &mut Value, path: &mut Vec<String>, incoming: &Value, budget: &mut usize)
where
    T: DeserializeOwned,
{
    if *budget == 0 {
        return; // out of recovery budget: leave the (valid) default at this path
    }
    let current = path_get(root, path).cloned();

    // Try the whole subtree wholesale first.
    path_set(root, path, incoming.clone());
    *budget -= 1;
    if T::deserialize(&*root).is_ok() {
        return;
    }

    match (current.as_ref(), incoming) {
        // Both objects: restore the default subtree, then fold child keys individually so a
        // single bad leaf drops only itself instead of the whole section.
        (Some(Value::Object(_)), Value::Object(sub)) => {
            path_set(root, path, current.clone().unwrap());
            for (key, child) in sub {
                if *budget == 0 {
                    break;
                }
                path.push(key.clone());
                graft::<T>(root, path, child, budget);
                path.pop();
            }
        }
        // Both arrays: keep only the elements that individually deserialize, so a drifted
        // record drops out of a history log without taking the rest of the log with it.
        (Some(Value::Array(_)), Value::Array(items)) => {
            let mut kept: Vec<Value> = Vec::new();
            if items.len() <= MAX_ELEMENTWISE_ARRAY {
                let mut attempted = 0usize;
                for item in items {
                    if *budget == 0 {
                        break;
                    }
                    path_set(root, path, Value::Array(vec![item.clone()]));
                    *budget -= 1;
                    attempted += 1;
                    if T::deserialize(&*root).is_ok() {
                        kept.push(item.clone());
                    } else if attempted >= 256 && kept.len() * 10 < attempted {
                        // Overwhelmingly incompatible array (the element *type* drifted, not a
                        // few records): stop proving each element fails and keep the default.
                        path_set(root, path, current.clone().unwrap());
                        return;
                    }
                }
            }
            path_set(root, path, Value::Array(kept));
        }
        // Incompatible leaf or type mismatch (renamed enum, retyped scalar, …): keep default.
        _ => match current {
            Some(value) => path_set(root, path, value),
            None => path_remove(root, path),
        },
    }
}

fn path_get<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut cur = root;
    for key in path {
        cur = cur.as_object()?.get(key)?;
    }
    Some(cur)
}

/// Set `root` at `path` to `value`, creating intermediate objects as needed.
fn path_set(root: &mut Value, path: &[String], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        *root = value;
        return;
    };
    let mut cur = root;
    for key in parents {
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
        cur = cur
            .as_object_mut()
            .unwrap()
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    if !cur.is_object() {
        *cur = Value::Object(Map::new());
    }
    cur.as_object_mut().unwrap().insert(last.clone(), value);
}

fn path_remove(root: &mut Value, path: &[String]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = root;
    for key in parents {
        match cur.as_object_mut().and_then(|obj| obj.get_mut(key)) {
            Some(next) => cur = next,
            None => return,
        }
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.remove(last);
    }
}

#[cfg(test)]
mod tests;
