//! Spawning the mpv child process.
//!
//! mpv runs headless (`--no-video --no-terminal --idle=yes`) as a pure audio engine
//! and exposes a JSON IPC endpoint we drive from [`super::ipc`]. `--no-config` keeps
//! startup predictable and the footprint small. stdio is sent to null so mpv can never
//! write into our terminal.

use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use tokio::process::Child;

use crate::util::process;
#[cfg(unix)]
use crate::util::runtime;

/// Whether the installed mpv accepts `--media-controls` (added in mpv 0.39).
///
/// mpv ≥ 0.39 registers its **own** OS media session by default — Windows SMTC
/// and macOS Now Playing — even in headless/audio-only mode, which would sit in
/// the flyout next to the session `crate::media` publishes as a duplicate entry
/// with raw stream metadata. Both our spawns pass `--media-controls=no` when
/// supported; older mpv must never see the flag (unknown options are fatal at
/// startup), hence this one-time capability probe instead of version parsing
/// (which breaks on git builds).
///
/// The flag must come BEFORE `--version`: mpv validates only the options parsed
/// before `--version` short-circuits, so the reversed order reports success for
/// any flag. Probed once per process — the daemon respawns mpv on every
/// stop→play cycle, and the answer can't change mid-run.
pub fn media_controls_flag_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        process::std_command("mpv", process::ProcessProfile::Media)
            .args(["--no-config", "--media-controls=no", "--version"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// A per-process IPC endpoint: a Unix socket path on macOS/Linux, a named pipe on
/// Windows. Unique per pid so concurrent instances never collide.
pub fn ipc_path() -> Result<String> {
    let pid = std::process::id();
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\ytm-tui-mpv-{pid}"))
    }
    #[cfg(unix)]
    {
        Ok(runtime::app_runtime_dir()
            .context("prepare mpv IPC runtime dir")?
            .join(format!("ytm-tui-mpv-{pid}.sock"))
            .to_string_lossy()
            .into_owned())
    }
}

/// IPC endpoint for the *video overlay* mpv ([`crate::player::video`]), unique per
/// (pid, spawn generation) so a `Shift+V` respawn never races the previous window's
/// socket/pipe.
pub fn video_ipc_path(generation: u64) -> Result<String> {
    let pid = std::process::id();
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\ytm-tui-mpv-video-{pid}-{generation}"))
    }
    #[cfg(unix)]
    {
        Ok(runtime::app_runtime_dir()
            .context("prepare mpv IPC runtime dir")?
            .join(format!("ytm-tui-mpv-video-{pid}-{generation}.sock"))
            .to_string_lossy()
            .into_owned())
    }
}

/// Spawn mpv listening on `ipc_path`. `kill_on_drop(true)` is the tokio-level half of
/// the no-orphan guarantee: if the owning [`super::Mpv`] is dropped, the child is
/// SIGKILLed. The OS-enforced backstops (signals, panic hook, Job Object) live in
/// [`super::lifetime`].
///
/// `cookies_file`, when present, is handed to mpv's bundled yt-dlp so authenticated
/// streams resolve (age-gated / library tracks). yt-dlp owns stream resolution — PO
/// tokens, signature deciphering, throttling — so we never touch those ourselves.
pub fn spawn(ipc_path: &str, cookies_file: Option<&Path>, gapless: bool) -> Result<Child> {
    let mut cmd = process::tokio_command("mpv", process::ProcessProfile::Media);
    cmd.arg("--no-video")
        .arg("--no-terminal")
        .arg("--idle=yes")
        .arg("--no-config")
        // Don't decode embedded cover art into a video track — pure audio engine.
        .arg("--audio-display=no")
        .arg(if gapless {
            "--gapless-audio=yes"
        } else {
            "--gapless-audio=no"
        })
        // Bound the streaming cache: mpv's default forward demuxer cache is 150 MiB,
        // which would balloon RAM on long tracks. ~40 MiB total is plenty of audio
        // buffering while keeping us honest on priority #1 (low RAM).
        .arg("--cache=yes")
        .arg("--demuxer-max-bytes=32MiB")
        .arg("--demuxer-max-back-bytes=8MiB")
        .arg(format!("--input-ipc-server={ipc_path}"));

    if let Some(path) = cookies_file {
        // mpv forwards this to yt-dlp as `--cookies <file>`.
        cmd.arg(format!(
            "--ytdl-raw-options-append=cookies={}",
            path.display()
        ));
    }

    // The OS media session is ours (`crate::media`), not mpv's — see the probe
    // docs. Placed before YTM_MPV_EXTRA so mpv's last-option-wins rule keeps
    // `YTM_MPV_EXTRA=--media-controls=yes` working as a user override.
    if media_controls_flag_supported() {
        cmd.arg("--media-controls=no");
    }

    // Escape hatch for tests/debugging, e.g. `YTM_MPV_EXTRA="--ao=null --volume=0"`.
    if let Ok(extra) = std::env::var("YTM_MPV_EXTRA") {
        for a in extra.split_whitespace() {
            cmd.arg(a);
        }
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    cmd.spawn()
        .context("failed to spawn mpv — is it installed and on PATH?")
}
