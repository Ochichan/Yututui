//! Single-instance lock for the GUI *process itself* (docs/gui/03 §6).
//!
//! Entirely separate from the core's `InstanceFile`/primary socket — the GUI never binds the
//! primary socket and never writes an `InstanceFile`. Windows uses a named mutex; unix uses
//! `flock` on a lockfile in the runtime dir. A second launch signals the first (which shows +
//! focuses the main window) over a small per-user activate endpoint, then exits.

use std::io;

#[cfg(unix)]
use std::path::PathBuf;

use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{GenericFilePath, ListenerOptions};
use tokio::io::AsyncWriteExt;

use crate::util::runtime;

#[cfg(windows)]
const MUTEX_NAME: &str = r"Local\io.github.ochi.ytm-tui.desktop";

/// The result of trying to become the single GUI instance.
pub enum Acquire {
    /// We are the first instance; hold this guard for the whole process lifetime.
    Primary(InstanceGuard),
    /// Another GUI instance already owns the lock.
    AlreadyRunning,
}

/// RAII guard that releases the lock on drop (unix: closes the flocked fd; Windows: closes
/// the mutex handle).
pub struct InstanceGuard {
    #[cfg(unix)]
    _lock: std::fs::File,
    #[cfg(windows)]
    _handle: WindowsMutex,
}

#[cfg(windows)]
struct WindowsMutex(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for WindowsMutex {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}
// The mutex handle is owned solely by the main thread's guard; the raw HANDLE is not shared.
#[cfg(windows)]
unsafe impl Send for WindowsMutex {}

#[cfg(unix)]
pub fn acquire() -> io::Result<Acquire> {
    use std::os::unix::io::AsRawFd;
    let path = lock_path()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    // Advisory, non-blocking exclusive lock on the open file description.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Acquire::Primary(InstanceGuard { _lock: file }))
    } else {
        let err = io::Error::last_os_error();
        // flock sets EWOULDBLOCK when LOCK_NB would otherwise block (another holder).
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(Acquire::AlreadyRunning)
        } else {
            Err(err)
        }
    }
}

#[cfg(windows)]
pub fn acquire() -> io::Result<Acquire> {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let wide: Vec<u16> = MUTEX_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(handle) };
        Ok(Acquire::AlreadyRunning)
    } else {
        Ok(Acquire::Primary(InstanceGuard {
            _handle: WindowsMutex(handle),
        }))
    }
}

#[cfg(unix)]
fn lock_path() -> io::Result<PathBuf> {
    Ok(runtime::app_runtime_dir()?.join(format!(
        "ytm-tui-desktop-{}.lock",
        runtime::filesystem_user_tag()
    )))
}

/// Per-user endpoint the second instance pings to activate the first.
fn activate_endpoint() -> io::Result<String> {
    let tag = runtime::filesystem_user_tag();
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\ytm-tui-desktop-activate-{tag}"))
    }
    #[cfg(unix)]
    {
        Ok(runtime::app_runtime_dir()?
            .join(format!("ytm-tui-desktop-activate-{tag}.sock"))
            .to_string_lossy()
            .into_owned())
    }
}

/// First instance: accept activate signals on a dedicated thread, calling `on_activate`
/// (which posts a show/focus request to the event loop) for each.
pub fn spawn_activate_listener(on_activate: impl Fn() + Send + 'static) -> io::Result<()> {
    let endpoint = activate_endpoint()?;
    std::thread::Builder::new()
        .name("ytt-desktop-activate".to_string())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                // Clear a stale unix socket file (no-op/harmless on Windows pipes).
                let _ = std::fs::remove_file(&endpoint);
                let Ok(name) = endpoint.as_str().to_fs_name::<GenericFilePath>() else {
                    return;
                };
                let Ok(listener) = ListenerOptions::new().name(name).create_tokio() else {
                    return;
                };
                while let Ok(_conn) = listener.accept().await {
                    on_activate();
                }
            });
        })?;
    Ok(())
}

/// Second instance: tell the primary to surface its window, best-effort, then the caller exits.
pub fn signal_activate() {
    let Ok(endpoint) = activate_endpoint() else {
        return;
    };
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    rt.block_on(async move {
        let Ok(name) = endpoint.as_str().to_fs_name::<GenericFilePath>() else {
            return;
        };
        if let Ok(conn) = Stream::connect(name).await {
            let mut w = &conn;
            let _ = w.write_all(b"activate\n").await;
            let _ = w.flush().await;
        }
    });
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::io::AsRawFd;

    #[test]
    fn exclusive_flock_blocks_a_second_holder() {
        let path = std::env::temp_dir().join(format!("ytt-si-test-{}.lock", std::process::id()));
        let a = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let rc_a = unsafe { libc::flock(a.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc_a, 0, "first holder should acquire");

        // A distinct open file description on the same path must fail non-blocking.
        let b = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let rc_b = unsafe { libc::flock(b.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(
            rc_b, -1,
            "second holder must be blocked while the first holds the lock"
        );

        drop(a); // releasing lets the next holder in
        let rc_b2 = unsafe { libc::flock(b.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc_b2, 0, "after release the lock is available");
        let _ = std::fs::remove_file(&path);
    }
}
