//! No-orphan process lifetime: mpv must die with the app, on every exit path.
//!
//! Layered guarantees (see the plan's "Process lifetime" section):
//!   1. tokio `kill_on_drop` + the [`super::Mpv`] `Drop` guard      — normal quit.
//!   2. Unix signal handlers (SIGINT/SIGTERM/SIGHUP)                — `kill`, terminal/SSH close.
//!   3. Panic hook (covers `panic = "abort"`, where Drop won't run) — crashes.
//!   4. PID registry reaped on next startup                        — uncatchable SIGKILL/power loss.
//!
//! On Windows the Job Object (M9) supersedes (2)/(3) by having the kernel terminate
//! mpv when our process dies for any reason; the registry stays as a universal backstop.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use crate::app::Msg;
#[cfg(unix)]
use tokio::sync::mpsc::UnboundedSender;

/// The live mpv pid, or 0 if none. Read from the panic hook and signal handler, which
/// must be allocation-free and async-signal-safe — an atomic is exactly that.
static MPV_PID: AtomicU32 = AtomicU32::new(0);

/// Record the spawned mpv pid so the panic hook / signal handler can reach it.
pub fn set_mpv_pid(pid: u32) {
    MPV_PID.store(pid, Ordering::SeqCst);
}

/// Atomically take the recorded pid (leaving 0), so we kill it at most once.
fn take_mpv_pid() -> Option<u32> {
    match MPV_PID.swap(0, Ordering::SeqCst) {
        0 => None,
        pid => Some(pid),
    }
}

/// Kill the recorded mpv immediately, if any. Safe to call repeatedly.
pub fn kill_mpv_now() {
    if let Some(pid) = take_mpv_pid() {
        kill_pid(pid);
    }
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    // SIGKILL is async-signal-safe and a single syscall — usable from the panic hook.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    // Belt-and-suspenders next to the Job Object (M9): ask the OS to terminate it.
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    let target = Pid::from_u32(pid);
    sys.refresh_processes(ProcessesToUpdate::Some(&[target]), true);
    if let Some(proc_) = sys.process(target) {
        proc_.kill();
    }
}

/// Wrap the current panic hook so a panic (including `panic = "abort"`, where `Drop`
/// never runs) still kills mpv before the inherited hook restores the terminal.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        kill_mpv_now();
        previous(info);
    }));
}

/// Spawn a task that waits for any termination signal, kills mpv, and asks the main
/// loop to quit. Keyboard Ctrl+C is handled as a key event (raw mode swallows SIGINT),
/// so these cover external `kill`s and terminal/SSH disconnects (SIGHUP).
#[cfg(unix)]
pub fn spawn_signal_handlers(tx: UnboundedSender<Msg>) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let mut hup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to register SIGHUP handler");
                return;
            }
        };
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to register SIGTERM handler");
                return;
            }
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "failed to register SIGINT handler");
                return;
            }
        };

        tokio::select! {
            _ = hup.recv() => tracing::info!("received SIGHUP"),
            _ = term.recv() => tracing::info!("received SIGTERM"),
            _ = int.recv() => tracing::info!("received SIGINT"),
        }

        kill_mpv_now();
        let _ = tx.send(Msg::Quit);
    });
}

#[cfg(windows)]
pub fn spawn_signal_handlers(tx: tokio::sync::mpsc::UnboundedSender<crate::app::Msg>) {
    // Console Ctrl+C; CTRL_CLOSE/logoff/shutdown are owned by the Job Object below.
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            kill_mpv_now();
            let _ = tx.send(crate::app::Msg::Quit);
        }
    });
}

/// Windows: place mpv in a Job Object flagged `KILL_ON_JOB_CLOSE`, so the kernel
/// terminates it the instant our process exits for *any* reason — clean quit,
/// `panic = "abort"`, the console close button, logoff/shutdown, or a Task-Manager
/// kill. This is the definitive no-orphan fix for the hard kills that signal handlers
/// cannot intercept (see the plan's "Process lifetime" section). The returned handle
/// (as `isize`) must be kept alive for the whole session; the kernel auto-closes it on
/// process death. Returns `None` on failure — we then lean on the sysinfo kill plus the
/// startup registry reaper. mpv's own children (yt-dlp) inherit job membership, so they
/// die too.
#[cfg(windows)]
pub fn assign_to_job(process_handle: std::os::windows::io::RawHandle) -> Option<isize> {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            tracing::warn!("CreateJobObject failed; relying on sysinfo backstop");
            return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast::<c_void>(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set == 0 {
            tracing::warn!("SetInformationJobObject failed");
            close_handle(job);
            return None;
        }
        if AssignProcessToJobObject(job, process_handle as HANDLE) == 0 {
            tracing::warn!("AssignProcessToJobObject failed");
            close_handle(job);
            return None;
        }
        tracing::info!("mpv bound to a kill-on-close Job Object");
        Some(job as isize)
    }
}

#[cfg(windows)]
fn close_handle(handle: windows_sys::Win32::Foundation::HANDLE) {
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }
}

/// Close the job handle. Because of `KILL_ON_JOB_CLOSE`, this also terminates mpv —
/// exactly what a clean quit wants. Called from the [`super::Mpv`] guard's `Drop`.
#[cfg(windows)]
pub fn close_job(handle: isize) {
    close_handle(handle as windows_sys::Win32::Foundation::HANDLE);
}

/// Windows: register a console control handler that kills mpv on the close button,
/// logoff, and shutdown. The Job Object already guarantees the kill, but this makes
/// teardown prompt (the OS gives us a short window before force-terminating us).
#[cfg(windows)]
pub fn install_console_ctrl_handler() {
    use windows_sys::Win32::System::Console::{
        CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, SetConsoleCtrlHandler,
    };

    // `windows-sys` models Win32 `BOOL` as a plain `i32` (1 = TRUE, 0 = FALSE).
    unsafe extern "system" fn handler(ctrl_type: u32) -> i32 {
        kill_mpv_now();
        match ctrl_type {
            // Close / logoff / shutdown: cleanup done; the OS still terminates us.
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => 1,
            // Ctrl+C / Ctrl+Break: let default handling proceed.
            _ => 0,
        }
    }

    unsafe {
        if SetConsoleCtrlHandler(Some(handler), 1) == 0 {
            tracing::warn!("SetConsoleCtrlHandler failed");
        }
    }
}

/// On-disk record tying an mpv pid to the app instance that spawned it.
#[derive(Serialize, Deserialize)]
struct Lifeline {
    app_pid: u32,
    mpv_pid: u32,
}

fn registry_path(dir: &Path) -> std::path::PathBuf {
    dir.join("mpv-lifeline.json")
}

/// Record `{app_pid, mpv_pid}` so a later run can reap a leaked mpv.
pub fn register(dir: &Path, app_pid: u32, mpv_pid: u32) {
    let record = Lifeline { app_pid, mpv_pid };
    if let Ok(json) = serde_json::to_string(&record) {
        let _ = std::fs::write(registry_path(dir), json);
    }
}

/// Reap a previous instance's mpv if that instance died without cleaning up (the only
/// path not covered by signals/Drop/panic/Job Object — e.g. SIGKILL or power loss).
pub fn reap_orphans(dir: &Path) {
    let path = registry_path(dir);
    let Ok(data) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(record) = serde_json::from_str::<Lifeline>(&data) else {
        let _ = std::fs::remove_file(&path);
        return;
    };

    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);

    let app_alive = sys.process(Pid::from_u32(record.app_pid)).is_some();
    if !app_alive
        && let Some(proc_) = sys.process(Pid::from_u32(record.mpv_pid))
        // Guard against pid reuse: only kill if it still looks like mpv.
        && proc_.name().to_string_lossy().to_lowercase().contains("mpv")
    {
        proc_.kill();
        tracing::warn!(
            mpv_pid = record.mpv_pid,
            "reaped orphaned mpv from a prior run"
        );
    }

    let _ = std::fs::remove_file(&path);
}
