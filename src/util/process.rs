//! Child process construction with explicit environment inheritance and bounded output capture.

use std::process::{Command as StdCommand, ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncReadExt;
use tokio::process::Command as TokioCommand;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessProfile {
    /// mpv/yt-dlp media playback and stream resolution.
    Media,
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
    std::env::vars()
        .filter(|(k, _)| should_inherit(k, profile))
        .collect()
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

    if matches!(profile, ProcessProfile::Media | ProcessProfile::YtDlp)
        && matches!(
            upper.as_str(),
            "HTTP_PROXY" | "HTTPS_PROXY" | "ALL_PROXY" | "NO_PROXY"
        )
    {
        return true;
    }

    if matches!(
        profile,
        ProcessProfile::Media | ProcessProfile::DesktopOpen | ProcessProfile::Clipboard
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
        assert!(should_inherit(
            "XDG_RUNTIME_DIR",
            ProcessProfile::DesktopOpen
        ));
        assert!(should_inherit("HTTPS_PROXY", ProcessProfile::YtDlp));
        assert!(!should_inherit("HTTPS_PROXY", ProcessProfile::Clipboard));
    }
}
