//! Child process construction with explicit environment inheritance and bounded output capture.

use std::io::Read;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::process::{Command as StdCommand, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;

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
    cmd
}

pub fn tokio_command(program: &str, profile: ProcessProfile) -> TokioCommand {
    let mut cmd = TokioCommand::new(program);
    apply_tokio_env(&mut cmd, profile);
    cmd
}

pub struct LimitedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

pub fn std_output_limited(
    mut cmd: StdCommand,
    timeout: Duration,
    stdout_max: usize,
) -> Result<LimitedOutput> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = cmd.spawn().context("spawn child process")?;

    // Drain stdout on a side thread WHILE polling for exit. Reading only after the child exits
    // (as before) deadlocks if the child fills the OS pipe buffer and blocks writing — the
    // classic pipe deadlock. The reader is bounded (`take`), so a chatty child can't grow
    // memory either.
    let stdout = child.stdout.take();
    let limit = stdout_max.saturating_add(1) as u64;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut stdout) = stdout {
            let _ = stdout.by_ref().take(limit).read_to_end(&mut out);
        }
        out
    });

    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("poll child process")? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!("child process timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let out = reader.join().unwrap_or_default();
    if out.len() > stdout_max {
        bail!("child stdout too large: more than {stdout_max} bytes");
    }

    Ok(LimitedOutput {
        status,
        stdout: out,
    })
}

pub async fn tokio_output_limited(
    mut cmd: TokioCommand,
    timeout: Duration,
    stdout_max: usize,
) -> Result<LimitedOutput> {
    cmd.stdout(Stdio::piped());
    let mut child = cmd.spawn().context("spawn child process")?;
    let mut stdout = child.stdout.take().context("child stdout was not piped")?;
    let mut out = Vec::new();
    let limit = stdout_max.saturating_add(1) as u64;

    let read = tokio::time::timeout(timeout, async {
        let mut limited = (&mut stdout).take(limit);
        limited.read_to_end(&mut out).await
    })
    .await;
    match read {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            let _ = child.kill().await;
            return Err(e).context("read child stdout");
        }
        Err(_) => {
            let _ = child.kill().await;
            bail!("child process timed out");
        }
    }

    if out.len() > stdout_max {
        let _ = child.kill().await;
        bail!("child stdout too large: more than {stdout_max} bytes");
    }

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(e).context("wait for child process"),
        Err(_) => {
            let _ = child.kill().await;
            bail!("child process timed out");
        }
    };
    Ok(LimitedOutput {
        status,
        stdout: out,
    })
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
            "RUST_LOG" | "YTM_MPV_EXTRA" | "YTM_NO_MEDIA_SESSION" | "YTM_YTDLP" | "YTM_MPV"
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
        assert!(!should_inherit("YTM_YTDLP", ProcessProfile::YtDlp));
        assert!(!should_inherit("HTTPS_PROXY", ProcessProfile::Clipboard));
    }

    #[test]
    fn std_output_limited_captures_and_bounds_stdout() {
        let exe = std::env::current_exe().unwrap();
        let mut ok = StdCommand::new(&exe);
        ok.arg("--help");
        let out = std_output_limited(ok, Duration::from_secs(5), 128 * 1024).unwrap();
        assert!(out.status.success());
        assert!(!out.stdout.is_empty());

        let mut too_large = StdCommand::new(exe);
        too_large.arg("--help");
        assert!(std_output_limited(too_large, Duration::from_secs(5), 1).is_err());
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
