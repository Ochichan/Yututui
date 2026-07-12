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
