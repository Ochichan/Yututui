//! Spawning the mpv child process.
//!
//! mpv runs headless (`--no-video --no-terminal --idle=yes`) as a pure audio engine
//! and exposes a JSON IPC endpoint we drive from [`super::ipc`]. `--no-config` keeps
//! startup predictable and the footprint small. stdio is sent to null so mpv can never
//! write into our terminal.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Child;

use crate::util::process;
#[cfg(unix)]
use crate::util::runtime;

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
