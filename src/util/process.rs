//! Child process construction with explicit environment inheritance and bounded output capture.

use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::{Command as StdCommand, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;

use crate::util::process_guard::ChildTreeGuard;

const STDERR_TAIL_MAX: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessProfile {
    /// mpv/yt-dlp media playback and stream resolution.
    Media,
    /// The background `ytt daemon serve` process.
    Daemon,
    /// Networked yt-dlp commands.
    YtDlp,
    /// OS browser opener (`open`, `xdg-open`, `cmd /C start`).
    DesktopOpen,
    /// Clipboard helpers (`pbcopy`, `wl-copy`, `xclip`, ...).
    Clipboard,
}

pub fn std_command(program: &str, profile: ProcessProfile) -> StdCommand {
    let mut cmd = StdCommand::new(program);
    apply_std_env(&mut cmd, profile);
    configure_std_child(&mut cmd, profile);
    cmd
}

pub fn tokio_command(program: &str, profile: ProcessProfile) -> TokioCommand {
    let mut cmd = TokioCommand::new(program);
    apply_tokio_env(&mut cmd, profile);
    configure_tokio_child(&mut cmd, profile);
    cmd
}

pub struct LimitedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    /// Last bytes of child stderr, bounded; empty if none.
    pub stderr_tail: Vec<u8>,
}

pub fn std_output_limited(
    mut cmd: StdCommand,
    profile: ProcessProfile,
    timeout: Duration,
    stdout_max: usize,
) -> Result<LimitedOutput> {
    // `profile` is part of this API's contract, so enforce tree isolation even when a caller
    // supplied a raw Command instead of constructing it through `std_command`.
    configure_std_child(&mut cmd, profile);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("spawn child process")?;
    let mut child_tree = ChildTreeGuard::for_std(&child, profile);

    // Drain stdout on a side thread WHILE polling for exit. Reading only after the child exits
    // (as before) deadlocks if the child fills the OS pipe buffer and blocks writing — the
    // classic pipe deadlock. The reader is bounded (`take`), so a chatty child can't grow
    // memory either.
    let stdout = child.stdout.take();
    let limit = stdout_max.saturating_add(1) as u64;
    let reader = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        if let Some(mut stdout) = stdout {
            stdout.by_ref().take(limit).read_to_end(&mut out)?;
        }
        Ok(out)
    });

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                child_tree.terminate();
                kill_and_wait_std(&mut child, profile);
                let _ = reader.join();
                return Err(error).context("poll child process");
            }
        }
        if start.elapsed() >= timeout {
            child_tree.terminate();
            kill_and_wait_std(&mut child, profile);
            let _ = reader.join();
            bail!("child process timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // The direct child is already reaped. Terminate its process group / close its Job Object
    // before joining the pipe reader: a detached helper may have inherited stdout and can keep
    // that reader from ever observing EOF even though the requested command succeeded.
    child_tree.terminate();
    let out = match reader.join() {
        Ok(Ok(out)) => out,
        Ok(Err(error)) => {
            child_tree.terminate();
            return Err(error).context("read child stdout");
        }
        Err(_) => {
            child_tree.terminate();
            bail!("child stdout reader panicked");
        }
    };
    if out.len() > stdout_max {
        child_tree.terminate();
        bail!("child stdout too large: more than {stdout_max} bytes");
    }

    Ok(LimitedOutput {
        status,
        stdout: out,
        stderr_tail: Vec::new(),
    })
}

pub async fn tokio_output_limited(
    mut cmd: TokioCommand,
    profile: ProcessProfile,
    timeout: Duration,
    stdout_max: usize,
) -> Result<LimitedOutput> {
    // As above, do not make cancellation safety depend on which command constructor was used.
    configure_tokio_child(&mut cmd, profile);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawn child process")?;
    let mut child_tree = ChildTreeGuard::for_tokio(&child, profile);
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            child_tree.terminate();
            kill_and_wait_tokio(&mut child, profile).await;
            bail!("child stdout was not piped");
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            child_tree.terminate();
            kill_and_wait_tokio(&mut child, profile).await;
            bail!("child stderr was not piped");
        }
    };
    let mut out = Vec::new();
    let limit = stdout_max.saturating_add(1) as u64;
    let mut stderr_drain = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut tail = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = stderr.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > STDERR_TAIL_MAX {
                let excess = tail.len() - STDERR_TAIL_MAX;
                tail.drain(..excess);
            }
        }
        std::io::Result::Ok(tail)
    });

    let read = tokio::time::timeout(timeout, async {
        let mut limited = (&mut stdout).take(limit);
        limited.read_to_end(&mut out).await
    })
    .await;
    match read {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            child_tree.terminate();
            kill_and_wait_tokio(&mut child, profile).await;
            stderr_drain.abort();
            return Err(e).context("read child stdout");
        }
        Err(_) => {
            child_tree.terminate();
            kill_and_wait_tokio(&mut child, profile).await;
            stderr_drain.abort();
            bail!("child process timed out");
        }
    }

    if out.len() > stdout_max {
        child_tree.terminate();
        kill_and_wait_tokio(&mut child, profile).await;
        stderr_drain.abort();
        bail!("child stdout too large: more than {stdout_max} bytes");
    }

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            child_tree.terminate();
            kill_and_wait_tokio(&mut child, profile).await;
            stderr_drain.abort();
            return Err(e).context("wait for child process");
        }
        Err(_) => {
            child_tree.terminate();
            kill_and_wait_tokio(&mut child, profile).await;
            stderr_drain.abort();
            bail!("child process timed out");
        }
    };
    // Close the Job Object / kill the process group before joining stderr: a helper that
    // inherited the pipe would otherwise be able to hold this successful call open.
    child_tree.terminate();
    // Short-bounded, not plain `.await`: a grandchild (JS runtime, ffmpeg) that inherited
    // the stderr write end can hold the pipe open past the child's own exit, and this
    // function must never outlive its timeout contract on the success path either.
    let stderr_tail = match tokio::time::timeout(Duration::from_secs(2), &mut stderr_drain).await {
        Ok(joined) => joined.ok().and_then(|r| r.ok()).unwrap_or_default(),
        Err(_) => {
            stderr_drain.abort();
            Vec::new()
        }
    };
    Ok(LimitedOutput {
        status,
        stdout: out,
        stderr_tail,
    })
}

#[cfg(unix)]
fn should_isolate_process_group(profile: ProcessProfile) -> bool {
    matches!(profile, ProcessProfile::Media | ProcessProfile::YtDlp)
}

#[cfg(unix)]
fn configure_std_child(cmd: &mut StdCommand, profile: ProcessProfile) {
    if should_isolate_process_group(profile) {
        // SAFETY: pre_exec runs in the child after fork and before exec. `setpgid` is an
        // async-signal-safe syscall and does not touch Rust allocation state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
}

#[cfg(not(unix))]
fn configure_std_child(_cmd: &mut StdCommand, _profile: ProcessProfile) {}

fn configure_tokio_child(cmd: &mut TokioCommand, profile: ProcessProfile) {
    if matches!(profile, ProcessProfile::Media | ProcessProfile::YtDlp) {
        // The tree guard owns descendants; this is the direct-child fallback if a future is
        // dropped between spawn and its next poll or Windows cannot assign the child to a job.
        cmd.kill_on_drop(true);
    }

    #[cfg(unix)]
    if should_isolate_process_group(profile) {
        // SAFETY: see `configure_std_child`.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }
}

/// Arrange for an already-open Unix descriptor to survive `exec` in exactly this child.
///
/// Rust creates sockets with close-on-exec enabled. mpv's `fd://N` IPC transport needs the
/// descriptor inherited, but clearing the flag in the guardian itself would leak it into any
/// concurrently spawned helper. Doing it in `pre_exec` keeps the change child-local.
#[cfg(unix)]
pub(crate) fn inherit_fd_in_child(cmd: &mut StdCommand, fd: std::os::fd::RawFd) {
    // SAFETY: `fcntl` is async-signal-safe. The descriptor stays owned by the caller until
    // `Command::spawn` returns, and this closure neither allocates nor touches shared Rust state.
    unsafe {
        cmd.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Make a Linux media child die if its guardian disappears between heartbeat checks.
///
/// The guardian's private IPC socket is the portable primary lease. PDEATHSIG closes the tiny
/// remaining window where the guardian itself is killed before it can terminate mpv's group.
#[cfg(target_os = "linux")]
pub(crate) fn configure_parent_death_signal(cmd: &mut StdCommand) {
    // Capture before fork so the child can detect the documented race where its parent exits
    // before PR_SET_PDEATHSIG is installed.
    let expected_parent = std::process::id() as libc::pid_t;
    // SAFETY: `prctl` and `getppid` are async-signal-safe syscalls. Error construction uses only
    // raw OS codes and the closure performs no allocation.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != expected_parent {
                return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn configure_parent_death_signal(_cmd: &mut StdCommand) {}

#[cfg(unix)]
pub(crate) fn terminate_process_group(pgid: libc::pid_t) {
    if pgid <= 0 {
        return;
    }
    // SAFETY: Media/YtDlp children are placed in a process group whose id is the direct child's
    // pid. A negative pid targets exactly that group. The direct-pid fallback keeps startup
    // lifeline records written by older versions (before Media isolation) reapable.
    unsafe {
        if libc::kill(-pgid, libc::SIGKILL) != 0 {
            libc::kill(pgid, libc::SIGKILL);
        }
    }
}

/// Ask a same-binary guardian to terminate its media tree and reap it before exiting.
///
/// This intentionally targets only the stable direct-child pid and uses SIGTERM: SIGKILLing the
/// guardian would destroy mpv's only reliable reaper in containers with a non-reaping PID 1.
#[cfg(unix)]
pub(crate) fn request_process_termination(pid: libc::pid_t) {
    if pid <= 0 {
        return;
    }
    // SAFETY: `pid` is retained by the caller's live Child ownership handshake. SIGTERM is handled
    // by the guardian using one lock-free atomic store.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

/// A process identity pinned by the kernel before a recovery path inspects its command line.
///
/// Raw numeric PIDs can be recycled between an identity check and a later signal. Linux pidfds
/// and Windows process handles instead keep the signal target bound to the same process object.
pub(crate) struct StableProcessTarget {
    #[cfg(target_os = "linux")]
    pidfd: std::os::fd::OwnedFd,
    #[cfg(windows)]
    process: std::os::windows::io::OwnedHandle,
}

pub(crate) enum StableProcessOpen {
    // Unsupported targets can only construct `Unavailable`; keep the shared result API intact.
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
    Pinned(StableProcessTarget),
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
    Gone,
    Unavailable(std::io::Error),
}

pub(crate) enum StableProcessKill {
    // Unsupported targets can only construct `Retry`; keep the shared result API intact.
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
    Killed,
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
    Gone,
    Retry(std::io::Error),
}

/// Pin `pid` without ever falling back to a racy raw-PID recovery target.
#[cfg(target_os = "linux")]
pub(crate) fn open_stable_process(pid: u32) -> StableProcessOpen {
    use std::os::fd::FromRawFd;

    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return StableProcessOpen::Gone;
    };
    // SAFETY: pidfd_open takes the numeric pid and zero flags and returns either a newly owned
    // close-on-exec descriptor or -1. Ownership is transferred to OwnedFd exactly once.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if raw >= 0 {
        // SAFETY: a nonnegative pidfd_open result is a fresh descriptor owned by this call.
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw as std::os::fd::RawFd) };
        return StableProcessOpen::Pinned(StableProcessTarget { pidfd });
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        StableProcessOpen::Gone
    } else {
        StableProcessOpen::Unavailable(error)
    }
}

#[cfg(windows)]
pub(crate) fn open_stable_process(pid: u32) -> StableProcessOpen {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };

    // SAFETY: OpenProcess returns a newly owned process handle or null. The requested rights are
    // limited to identity/query, waiting, and termination for this exact recovery target.
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            0,
            pid,
        )
    };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error()
            == Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER as i32)
        {
            StableProcessOpen::Gone
        } else {
            StableProcessOpen::Unavailable(error)
        };
    }
    // SAFETY: `raw` is the fresh owned handle returned by the successful OpenProcess call.
    let process = unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(raw as _) };
    StableProcessOpen::Pinned(StableProcessTarget { process })
}

#[cfg(not(any(target_os = "linux", windows)))]
pub(crate) fn open_stable_process(_pid: u32) -> StableProcessOpen {
    StableProcessOpen::Unavailable(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no collateral-safe process signal handle",
    ))
}

impl StableProcessTarget {
    /// Terminate the pinned media target (and its group where the kernel supports a pinned group
    /// signal). A failure is returned for retry; this method never substitutes a numeric PID.
    #[cfg(target_os = "linux")]
    pub(crate) fn terminate_media(&self) -> StableProcessKill {
        use std::os::fd::AsRawFd;

        let send = |flags: libc::c_uint| {
            // SAFETY: the pidfd remains owned by `self`; siginfo is null as documented, SIGKILL is
            // valid, and flags is either the process-group extension or zero.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    flags,
                )
            }
        };
        let mut result = send(libc::PIDFD_SIGNAL_PROCESS_GROUP);
        if result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
            // Linux 5.3-6.8 supports stable process signaling but not the 6.9 group flag. Killing
            // the exact mpv remains collateral-safe; guardian ownership is the primary tree edge.
            result = send(0);
        }
        if result >= 0 {
            StableProcessKill::Killed
        } else {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                StableProcessKill::Gone
            } else {
                StableProcessKill::Retry(error)
            }
        }
    }

    #[cfg(windows)]
    pub(crate) fn terminate_media(&self) -> StableProcessKill {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

        let handle = self.process.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        // SAFETY: the OwnedHandle pins a valid process object for the duration of both calls.
        let state = unsafe { WaitForSingleObject(handle, 0) };
        match state {
            WAIT_OBJECT_0 => StableProcessKill::Gone,
            WAIT_TIMEOUT => {
                // SAFETY: the same pinned handle carries PROCESS_TERMINATE access.
                if unsafe { TerminateProcess(handle, 1) } != 0 {
                    StableProcessKill::Killed
                } else {
                    StableProcessKill::Retry(std::io::Error::last_os_error())
                }
            }
            WAIT_FAILED => StableProcessKill::Retry(std::io::Error::last_os_error()),
            value => StableProcessKill::Retry(std::io::Error::other(format!(
                "unexpected process wait result: {value}"
            ))),
        }
    }

    #[cfg(not(any(target_os = "linux", windows)))]
    pub(crate) fn terminate_media(&self) -> StableProcessKill {
        StableProcessKill::Retry(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this platform has no collateral-safe process signal handle",
        ))
    }
}

/// Observe a direct child's exit without reaping it.
///
/// `None` means the child is still running; `Some` contains whether it exited successfully. The
/// guardian deliberately retains an exited mpv as a zombie until the guardian itself exits. That
/// keeps the mpv pid unavailable for reuse throughout the interval in which the owner registry can
/// still see a running guardian.
#[cfg(unix)]
pub(crate) fn direct_child_exit_without_reap(pid: libc::pid_t) -> std::io::Result<Option<bool>> {
    if pid <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "direct child pid must be positive",
        ));
    }

    let id = libc::id_t::try_from(pid).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "direct child pid does not fit id_t",
        )
    })?;
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: `info` points to writable storage for siginfo_t. P_PID restricts observation to the
    // supplied direct child, WNOHANG makes the call non-blocking, and WNOWAIT deliberately leaves
    // an exited child unreaped. POSIX defines a zero si_pid when no requested state is available;
    // the buffer is pre-zeroed for kernels predating that clarification.
    unsafe {
        if libc::waitid(
            libc::P_PID,
            id,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let info = info.assume_init();
        if info.si_pid() == 0 {
            Ok(None)
        } else {
            Ok(Some(
                info.si_code == libc::CLD_EXITED && info.si_status() == 0,
            ))
        }
    }
}

/// Observe a `std::process::Child` completion without releasing its pid/process object.
///
/// Guarded owners use this to remove every pid-based teardown hook before the final `wait()`.
#[cfg(unix)]
pub(crate) fn child_exit_without_reap(
    child: &std::process::Child,
) -> std::io::Result<Option<bool>> {
    let pid = libc::pid_t::try_from(child.id()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "child pid does not fit pid_t",
        )
    })?;
    direct_child_exit_without_reap(pid)
}

#[cfg(windows)]
pub(crate) fn child_exit_without_reap(
    child: &std::process::Child,
) -> std::io::Result<Option<bool>> {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

    let process = child.as_raw_handle() as HANDLE;
    // SAFETY: the Child retains ownership of a valid process handle for this whole call. Waiting
    // with a zero timeout and reading its exit code neither closes the handle nor releases the OS
    // process object, so the pid cannot be recycled through this observation.
    unsafe {
        match WaitForSingleObject(process, 0) {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if GetExitCodeProcess(process, &mut code) == 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(Some(code == 0))
                }
            }
            WAIT_FAILED => Err(std::io::Error::last_os_error()),
            result => Err(std::io::Error::other(format!(
                "unexpected process wait result: {result}"
            ))),
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn child_exit_without_reap(
    _child: &std::process::Child,
) -> std::io::Result<Option<bool>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "non-reaping child observation is unsupported on this platform",
    ))
}

#[cfg(all(test, unix))]
pub(crate) fn process_exists_for_test(pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 probes process existence without delivering a signal.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
pub(super) fn create_child_job(process: isize) -> Option<isize> {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    // SAFETY: `process` is borrowed from the live child. Each owned job handle returned by
    // CreateJobObjectW is either closed on an error path or returned to ChildTreeGuard, which
    // closes it exactly once through `close_child_job`.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            let error = std::io::Error::last_os_error();
            tracing::warn!(%error, "failed to create child Job Object; using direct-child fallback");
            return None;
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast::<c_void>(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let error = std::io::Error::last_os_error();
            tracing::warn!(%error, "failed to configure child Job Object; using direct-child fallback");
            let _ = CloseHandle(job);
            return None;
        }

        if AssignProcessToJobObject(job, process as HANDLE) == 0 {
            let error = std::io::Error::last_os_error();
            tracing::warn!(%error, "failed to assign child to Job Object; using direct-child fallback");
            let _ = CloseHandle(job);
            return None;
        }

        tracing::debug!("child bound to a kill-on-close Job Object");
        Some(job as isize)
    }
}

/// Duplicate an owned Job Object handle into the already-created guardian process.
///
/// The guardian is still blocked on its request pipe when this runs. It receives the numeric
/// target-process handle only after duplication succeeds, so mpv can never start outside the
/// kill-on-close job. Keeping a copy in the guardian also lets a heartbeat timeout terminate the
/// job even while the owner process is alive but wedged.
#[cfg(windows)]
pub(super) fn duplicate_child_job_into(job: isize, process: isize) -> std::io::Result<u64> {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut remote: HANDLE = std::ptr::null_mut();
    // SAFETY: `job` and `process` are borrowed from a live ChildTreeGuard. `remote` is a valid
    // output pointer. On success Windows creates a distinct handle owned by the target process;
    // it is deliberately not closed in this process because it is not valid here.
    unsafe {
        if DuplicateHandle(
            GetCurrentProcess(),
            job as HANDLE,
            process as HANDLE,
            &mut remote,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(remote as usize as u64)
}

/// Create a nested, kill-on-close Job for a blocked guardian and transfer the only surviving
/// handle into that guardian.
///
/// The guardian is already a member of the parent's outer Job. Windows 8+ nested-job semantics
/// make its future children members of both jobs. The parent keeps the outer handle (so owner
/// hard death kills everything), while the guardian alone owns this inner handle (so direct
/// guardian death also kills mpv). The guardian cannot spawn mpv until the returned token is sent.
#[cfg(windows)]
pub(super) fn create_guardian_inner_job(process: isize) -> std::io::Result<u64> {
    let inner = create_child_job(process)
        .ok_or_else(|| std::io::Error::other("guardian inner Job Object assignment failed"))?;
    let remote = match duplicate_child_job_into(inner, process) {
        Ok(remote) => remote,
        Err(error) => {
            close_child_job(inner);
            return Err(error);
        }
    };
    // The duplicated target-process handle is now the only inner-job handle. Closing our copy
    // cannot terminate the job because the guardian's copy is already live.
    close_child_job(inner);
    Ok(remote)
}

/// Terminate the guardian's inherited Job Object, including the guardian itself and every mpv
/// descendant. If termination fails, the caller still falls back to its direct process-tree kill.
#[cfg(windows)]
pub(crate) fn terminate_inherited_job(job: u64) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;

    // SAFETY: the value was produced by `duplicate_child_job_into` for this process and remains
    // open for the guardian's lifetime. TerminateJobObject does not consume the handle.
    unsafe {
        if TerminateJobObject(job as usize as HANDLE, 1) == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn close_child_job(job: isize) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

    // SAFETY: `job` is an owned handle removed from ChildTreeGuard before this call, so no other
    // path can close it again. KILL_ON_JOB_CLOSE terminates all assigned descendants.
    unsafe {
        if CloseHandle(job as HANDLE) == 0 {
            let error = std::io::Error::last_os_error();
            tracing::warn!(%error, "failed to close child Job Object");
        }
    }
}

#[cfg(windows)]
pub(super) fn terminate_child_process(process: isize) {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::TerminateProcess;

    // SAFETY: `process` is borrowed from the still-live child. ChildTreeGuard is declared after
    // that child, so this synchronous fallback runs while the handle remains valid.
    unsafe {
        if TerminateProcess(process as HANDLE, 1) == 0 {
            let error = std::io::Error::last_os_error();
            tracing::warn!(%error, "failed to terminate unassigned child directly");
        }
    }
}

/// Allocation-free best-effort process termination for the Windows console control handler.
#[cfg(windows)]
pub(crate) fn terminate_process_id(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    // SAFETY: OpenProcess returns a newly owned handle or null. The handle is used only for
    // TerminateProcess and closed exactly once before returning.
    unsafe {
        let process: HANDLE = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if process.is_null() {
            return;
        }
        let _ = TerminateProcess(process, 1);
        let _ = CloseHandle(process);
    }
}

fn kill_and_wait_std(child: &mut std::process::Child, profile: ProcessProfile) {
    #[cfg(not(unix))]
    let _ = profile;

    #[cfg(unix)]
    if should_isolate_process_group(profile)
        && let Ok(pid) = libc::pid_t::try_from(child.id())
    {
        terminate_process_group(pid);
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub async fn kill_and_wait_tokio(child: &mut tokio::process::Child, profile: ProcessProfile) {
    #[cfg(not(unix))]
    let _ = profile;

    #[cfg(unix)]
    if should_isolate_process_group(profile)
        && let Some(id) = child.id()
        && let Ok(pid) = libc::pid_t::try_from(id)
    {
        terminate_process_group(pid);
    }
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
}

pub fn apply_std_env(cmd: &mut StdCommand, profile: ProcessProfile) {
    cmd.env_clear();
    for (k, v) in allowed_env(profile) {
        cmd.env(k, v);
    }
}

pub fn apply_tokio_env(cmd: &mut TokioCommand, profile: ProcessProfile) {
    cmd.env_clear();
    for (k, v) in allowed_env(profile) {
        cmd.env(k, v);
    }
}

fn allowed_env(profile: ProcessProfile) -> Vec<(String, String)> {
    let mut vars = std::env::vars()
        .filter(|(k, _)| should_inherit(k, profile))
        .collect();
    augment_env(&mut vars, profile);
    vars
}

fn augment_env(vars: &mut Vec<(String, String)>, profile: ProcessProfile) {
    #[cfg(target_os = "macos")]
    augment_macos_path(vars, profile);
    #[cfg(not(target_os = "macos"))]
    let _ = (vars, profile);
}

#[cfg(target_os = "macos")]
fn augment_macos_path(vars: &mut Vec<(String, String)>, profile: ProcessProfile) {
    if !matches!(
        profile,
        ProcessProfile::Media
            | ProcessProfile::Daemon
            | ProcessProfile::YtDlp
            | ProcessProfile::DesktopOpen
            | ProcessProfile::Clipboard
    ) {
        return;
    }

    let current = vars
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::env::split_paths(&current).collect();
    for hint in macos_path_hints() {
        if !paths.iter().any(|path| path == &hint) {
            paths.push(hint);
        }
    }
    let Ok(joined) = std::env::join_paths(&paths) else {
        return;
    };
    let joined = joined.to_string_lossy().into_owned();
    if let Some((_, value)) = vars.iter_mut().find(|(key, _)| key == "PATH") {
        *value = joined;
    } else {
        vars.push(("PATH".to_string(), joined));
    }
}

#[cfg(target_os = "macos")]
fn macos_path_hints() -> Vec<PathBuf> {
    let mut hints = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        hints.push(home.join(".cargo/bin"));
        hints.push(home.join(".local/bin"));
    }
    hints
}

fn should_inherit(key: &str, profile: ProcessProfile) -> bool {
    if is_sensitive_env_key(key) {
        return false;
    }
    let upper = key.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "PATH"
            | "PATHEXT"
            | "HOME"
            | "USER"
            | "USERNAME"
            | "LOGNAME"
            | "USERPROFILE"
            | "LOCALAPPDATA"
            | "APPDATA"
            | "TMPDIR"
            | "TEMP"
            | "TMP"
            | "SYSTEMROOT"
            | "WINDIR"
            | "COMSPEC"
            | "LANG"
            | "SSL_CERT_FILE"
            | "SSL_CERT_DIR"
            | "NIX_SSL_CERT_FILE"
    ) || upper.starts_with("LC_")
    {
        return true;
    }

    if matches!(
        profile,
        ProcessProfile::Media | ProcessProfile::Daemon | ProcessProfile::YtDlp
    ) && matches!(
        upper.as_str(),
        "HTTP_PROXY" | "HTTPS_PROXY" | "ALL_PROXY" | "NO_PROXY"
    ) {
        return true;
    }

    if matches!(profile, ProcessProfile::Daemon)
        && matches!(
            upper.as_str(),
            "RUST_LOG"
                | "YTM_MPV_EXTRA"
                | "YTM_NO_MEDIA_SESSION"
                | "YTM_YTDLP"
                | "YTM_MPV"
                | "YTM_YTDLP_USER_CONFIG"
        )
    {
        return true;
    }

    if matches!(
        profile,
        ProcessProfile::Media
            | ProcessProfile::Daemon
            | ProcessProfile::DesktopOpen
            | ProcessProfile::Clipboard
    ) && matches!(
        upper.as_str(),
        "DISPLAY"
            | "WAYLAND_DISPLAY"
            | "XAUTHORITY"
            | "XDG_RUNTIME_DIR"
            | "XDG_CONFIG_HOME"
            | "XDG_CACHE_HOME"
            | "XDG_DATA_HOME"
            | "DBUS_SESSION_BUS_ADDRESS"
            | "__CF_USER_TEXT_ENCODING"
    ) {
        return true;
    }

    // xdg-open needs these to resolve the default browser handler: Flatpak/Snap
    // register their .desktop files via XDG_DATA_DIRS, and DE detection selects
    // the right opener. Scoped to DesktopOpen only — mpv/yt-dlp/clipboard don't
    // need them. Without XDG_DATA_DIRS, xdg-open can't find a Flatpak Firefox
    // handler and fails silently.
    if matches!(profile, ProcessProfile::DesktopOpen)
        && matches!(
            upper.as_str(),
            "XDG_DATA_DIRS" | "XDG_CURRENT_DESKTOP" | "DESKTOP_SESSION" | "BROWSER"
        )
    {
        return true;
    }

    false
}

fn is_sensitive_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper == "GEMINI_API_KEY"
        || upper == "GOOGLE_API_KEY"
        || upper == "OPENAI_API_KEY"
        || upper == "ANTHROPIC_API_KEY"
        || upper.starts_with("AWS_")
        || upper.starts_with("NPM_")
        || upper.starts_with("GITHUB_")
        || upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.ends_with("_KEY")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn child_tree_test_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "ytt-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[cfg(unix)]
    fn read_fixture_pid(pid_file: &std::path::Path) -> libc::pid_t {
        // Shell redirection creates the file before `echo` writes the pid. Under a busy parallel
        // test run, observing pathname existence therefore does not yet prove the record is
        // complete. Wait for one parseable record rather than turning that tiny publication
        // window into a flaky process-lifetime failure.
        for _ in 0..100 {
            if let Ok(contents) = std::fs::read_to_string(pid_file)
                && let Ok(pid) = contents.trim().parse()
            {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!(
            "pid file should contain a pid: {:?}",
            std::fs::read_to_string(pid_file)
        );
    }

    #[cfg(unix)]
    fn process_is_alive(pid: libc::pid_t) -> bool {
        process_exists_for_test(pid)
    }

    #[cfg(unix)]
    async fn assert_process_exits(pid: libc::pid_t, root: &std::path::Path) {
        for _ in 0..40 {
            if !process_is_alive(pid) {
                let _ = std::fs::remove_dir_all(root);
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = std::fs::remove_dir_all(root);
        panic!("grandchild process {pid} survived child-tree cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn unreaped_child_pid_remains_owned_while_its_guardian_finishes() {
        let mut child = StdCommand::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn direct child fixture");
        let pid = libc::pid_t::try_from(child.id()).expect("child pid fits pid_t");
        assert_eq!(direct_child_exit_without_reap(pid).unwrap(), None);

        child.kill().expect("terminate direct child fixture");
        let mut observed_exit = false;
        for _ in 0..100 {
            if direct_child_exit_without_reap(pid).unwrap().is_some() {
                observed_exit = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(observed_exit, "waitid should observe the exited child");
        assert!(
            process_exists_for_test(pid),
            "WNOWAIT must retain the exited child's pid while its guardian is live"
        );
        assert!(direct_child_exit_without_reap(pid).unwrap().is_some());
        // WNOWAIT must leave ownership with the Child handle rather than consuming its status.
        child.wait().expect("reap direct child fixture");
        assert!(!process_exists_for_test(pid));
    }

    #[test]
    fn sensitive_keys_are_not_inherited() {
        for key in [
            "GEMINI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "NPM_TOKEN",
            "GITHUB_TOKEN",
            "SERVICE_PASSWORD",
        ] {
            assert!(!should_inherit(key, ProcessProfile::YtDlp));
        }
    }

    #[test]
    fn required_runtime_keys_are_inherited() {
        assert!(should_inherit("PATH", ProcessProfile::YtDlp));
        assert!(should_inherit("HOME", ProcessProfile::Media));
        assert!(should_inherit("USER", ProcessProfile::DesktopOpen));
        assert!(should_inherit("USERNAME", ProcessProfile::DesktopOpen));
        assert!(should_inherit("XDG_RUNTIME_DIR", ProcessProfile::Daemon));
        assert!(should_inherit("HTTPS_PROXY", ProcessProfile::YtDlp));
        assert!(should_inherit("YTM_MPV_EXTRA", ProcessProfile::Daemon));
        assert!(should_inherit("RUST_LOG", ProcessProfile::Daemon));
        // Reaches the spawned daemon child (env is otherwise cleared) so a headless
        // launch can disable the OS media session; daemon-scoped like YTM_MPV_EXTRA.
        assert!(should_inherit(
            "YTM_NO_MEDIA_SESSION",
            ProcessProfile::Daemon
        ));
        assert!(!should_inherit(
            "YTM_NO_MEDIA_SESSION",
            ProcessProfile::YtDlp
        ));
        // Tool-path overrides must survive into the daemon child (env is cleared),
        // or `YTM_YTDLP`/`YTM_MPV` would silently stop applying in daemon mode.
        assert!(should_inherit("YTM_YTDLP", ProcessProfile::Daemon));
        assert!(should_inherit("YTM_MPV", ProcessProfile::Daemon));
        assert!(should_inherit(
            "YTM_YTDLP_USER_CONFIG",
            ProcessProfile::Daemon
        ));
        assert!(!should_inherit("YTM_YTDLP", ProcessProfile::YtDlp));
        assert!(!should_inherit(
            "YTM_YTDLP_USER_CONFIG",
            ProcessProfile::YtDlp
        ));
        assert!(!should_inherit("HTTPS_PROXY", ProcessProfile::Clipboard));
    }

    #[test]
    fn std_output_limited_captures_and_bounds_stdout() {
        let exe = std::env::current_exe().unwrap();
        let mut ok = StdCommand::new(&exe);
        ok.arg("--help");
        let out = std_output_limited(
            ok,
            ProcessProfile::Media,
            Duration::from_secs(5),
            128 * 1024,
        )
        .unwrap();
        assert!(out.status.success());
        assert!(!out.stdout.is_empty());
        assert!(out.stderr_tail.is_empty());

        let mut too_large = StdCommand::new(exe);
        too_large.arg("--help");
        assert!(
            std_output_limited(too_large, ProcessProfile::Media, Duration::from_secs(5), 1)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tokio_output_limited_captures_stderr_tail() {
        let mut cmd = TokioCommand::new("sh");
        cmd.args(["-c", "echo out; echo err1 >&2; echo err2 >&2"]);
        let out = tokio_output_limited(
            cmd,
            ProcessProfile::YtDlp,
            Duration::from_secs(5),
            128 * 1024,
        )
        .await
        .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("out"));
        assert!(String::from_utf8_lossy(&out.stderr_tail).contains("err2"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tokio_output_limited_bounds_stderr_tail() {
        let mut cmd = TokioCommand::new("sh");
        cmd.args(["-c", "yes x | head -c 9000 >&2; printf final >&2"]);
        let out = tokio_output_limited(
            cmd,
            ProcessProfile::YtDlp,
            Duration::from_secs(5),
            128 * 1024,
        )
        .await
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stderr_tail.len(), STDERR_TAIL_MAX);
        assert!(out.stderr_tail.ends_with(b"final"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tokio_output_limited_kills_ytdlp_process_group_on_timeout() {
        let root = child_tree_test_root("tokio-timeout-tree");
        let pid_file = root.join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > '{}'; sleep 30", pid_file.display());
        let mut cmd = tokio_command("sh", ProcessProfile::YtDlp);
        cmd.args(["-c", &script]);

        let result =
            tokio_output_limited(cmd, ProcessProfile::YtDlp, Duration::from_millis(250), 128).await;
        assert!(result.is_err(), "the fake yt-dlp process should time out");

        assert_process_exits(read_fixture_pid(&pid_file), &root).await;
    }

    #[cfg(unix)]
    #[test]
    fn std_output_limited_kills_ytdlp_process_group_on_timeout() {
        let root = child_tree_test_root("std-timeout-tree");
        let pid_file = root.join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > '{}'; sleep 30", pid_file.display());
        let mut cmd = std_command("sh", ProcessProfile::YtDlp);
        cmd.args(["-c", &script]);

        let result =
            std_output_limited(cmd, ProcessProfile::YtDlp, Duration::from_millis(250), 128);
        assert!(result.is_err(), "the fake yt-dlp process should time out");

        let pid = read_fixture_pid(&pid_file);
        for _ in 0..40 {
            if !process_is_alive(pid) {
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(&root);
        panic!("grandchild process {pid} survived child-tree cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn successful_std_output_limited_closes_inherited_stdout_before_reader_join() {
        let root = child_tree_test_root("std-success-inherited-stdout");
        let pid_file = root.join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > '{}'; printf ok", pid_file.display());
        let mut cmd = std_command("sh", ProcessProfile::YtDlp);
        cmd.args(["-c", &script]);
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result =
                std_output_limited(cmd, ProcessProfile::YtDlp, Duration::from_secs(5), 128);
            let _ = completed_tx.send(result);
        });

        let result = match completed_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(result) => result,
            Err(error) => {
                if pid_file.exists() {
                    let pid = read_fixture_pid(&pid_file);
                    // SAFETY: the pid came from this test's just-spawned fixture process.
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
                let _ = worker.join();
                let _ = std::fs::remove_dir_all(&root);
                panic!("successful output capture waited on inherited stdout: {error}");
            }
        }
        .expect("direct child should finish successfully");
        worker.join().expect("output worker joins");
        assert!(result.status.success());
        assert_eq!(result.stdout, b"ok");

        let pid = read_fixture_pid(&pid_file);
        for _ in 0..40 {
            if !process_is_alive(pid) {
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(&root);
        panic!("grandchild process {pid} survived successful child-tree cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tokio_output_limited_cleans_tree_on_oversized_stdout() {
        let root = child_tree_test_root("tokio-oversize-tree");
        let pid_file = root.join("grandchild.pid");
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>/dev/null & echo $! > '{}'; \
             head -c 1024 /dev/zero",
            pid_file.display()
        );
        let mut cmd = tokio_command("sh", ProcessProfile::YtDlp);
        cmd.args(["-c", &script]);

        let result =
            tokio_output_limited(cmd, ProcessProfile::YtDlp, Duration::from_secs(5), 64).await;
        let Err(error) = result else {
            panic!("oversized stdout must fail");
        };
        assert!(error.to_string().contains("stdout too large"));

        assert_process_exits(read_fixture_pid(&pid_file), &root).await;
    }

    #[cfg(unix)]
    #[test]
    fn std_output_limited_cleans_tree_on_oversized_stdout() {
        let root = child_tree_test_root("std-oversize-tree");
        let pid_file = root.join("grandchild.pid");
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>/dev/null & echo $! > '{}'; \
             head -c 1024 /dev/zero",
            pid_file.display()
        );
        let mut cmd = std_command("sh", ProcessProfile::YtDlp);
        cmd.args(["-c", &script]);

        let result = std_output_limited(cmd, ProcessProfile::YtDlp, Duration::from_secs(5), 64);
        let Err(error) = result else {
            panic!("oversized stdout must fail");
        };
        assert!(error.to_string().contains("stdout too large"));

        let pid = read_fixture_pid(&pid_file);
        for _ in 0..40 {
            if !process_is_alive(pid) {
                let _ = std::fs::remove_dir_all(&root);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_dir_all(&root);
        panic!("grandchild process {pid} survived child-tree cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_tokio_output_limited_kills_ytdlp_process_group() {
        let root = child_tree_test_root("tokio-cancel-tree");
        let pid_file = root.join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > '{}'; sleep 30", pid_file.display());
        let mut cmd = tokio_command("sh", ProcessProfile::YtDlp);
        cmd.args(["-c", &script]);
        let task = tokio::spawn(tokio_output_limited(
            cmd,
            ProcessProfile::YtDlp,
            Duration::from_secs(30),
            128,
        ));

        tokio::time::timeout(Duration::from_secs(5), async {
            while !pid_file.exists() {
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("grandchild did not start before cancellation");
        let pid = read_fixture_pid(&pid_file);
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));

        assert_process_exits(pid, &root).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_tokio_output_limited_cleans_detached_ytdlp_helper() {
        let root = child_tree_test_root("tokio-success-tree");
        let pid_file = root.join("grandchild.pid");
        let script = format!(
            "sleep 30 </dev/null >/dev/null 2>/dev/null & echo $! > '{}'; printf ok",
            pid_file.display()
        );
        let mut cmd = tokio_command("sh", ProcessProfile::YtDlp);
        cmd.args(["-c", &script]);

        let result = tokio_output_limited(cmd, ProcessProfile::YtDlp, Duration::from_secs(5), 128)
            .await
            .expect("direct child should finish successfully");
        assert!(result.status.success());
        assert_eq!(result.stdout, b"ok");

        assert_process_exits(read_fixture_pid(&pid_file), &root).await;
    }

    #[test]
    fn desktop_open_inherits_xdg_browser_discovery_keys() {
        // xdg-open needs these to find a Flatpak/Snap/native default browser
        // handler (env is otherwise cleared). Regression guard for the Flatpak
        // Firefox "browser never opens" bug.
        for key in [
            "XDG_DATA_DIRS",
            "XDG_CURRENT_DESKTOP",
            "DESKTOP_SESSION",
            "BROWSER",
        ] {
            assert!(
                should_inherit(key, ProcessProfile::DesktopOpen),
                "{key} must reach xdg-open"
            );
            // Scoped to DesktopOpen only — mpv/yt-dlp must not inherit them.
            assert!(!should_inherit(key, ProcessProfile::YtDlp));
            assert!(!should_inherit(key, ProcessProfile::Media));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allowed_env_adds_common_gui_launch_paths() {
        let vars = allowed_env(ProcessProfile::Daemon);
        let path = vars
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.as_str())
            .unwrap_or_default();
        let paths: Vec<PathBuf> = std::env::split_paths(path).collect();
        assert!(paths.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(paths.contains(&PathBuf::from("/usr/local/bin")));
    }
}
