//! Track downloads via `yt-dlp` + `ffmpeg`: best audio → m4a, with embedded metadata
//! and cover art, saved under `<download dir>/<artist>/<title>.m4a`.
//!
//! The actor receives [`DownloadCmd::Start`] and spawns one task per track, gated by a
//! [`Semaphore`] so at most [`MAX_CONCURRENT`] run at once (priority #1: bounded work).
//! Progress is parsed from yt-dlp's `--progress-template` lines and streamed back as
//! [`Msg::DownloadProgress`]; the final saved path comes from `--print after_move:filepath`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::api::Song;
use crate::app::Msg;

/// Most simultaneous downloads.
const MAX_CONCURRENT: usize = 3;

pub enum DownloadCmd {
    Start(Song),
}

pub struct DownloadHandle {
    tx: UnboundedSender<DownloadCmd>,
}

impl DownloadHandle {
    pub fn start(&self, song: Song) {
        let _ = self.tx.send(DownloadCmd::Start(song));
    }
}

/// Spawn the download actor. Results return as `Msg::Download*`.
pub fn spawn(
    msg_tx: UnboundedSender<Msg>,
    dir: PathBuf,
    cookies: Option<PathBuf>,
) -> DownloadHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<DownloadCmd>();
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
    tokio::spawn(async move {
        while let Some(DownloadCmd::Start(song)) = rx.recv().await {
            let sem = sem.clone();
            let tx = msg_tx.clone();
            let dir = dir.clone();
            let cookies = cookies.clone();
            tokio::spawn(async move {
                let Ok(_permit) = sem.acquire_owned().await else {
                    return; // semaphore closed; nothing to do
                };
                if let Err(e) = run_download(&song, &dir, cookies.as_deref(), &tx).await {
                    tracing::warn!(error = %format!("{e:#}"), video_id = %song.video_id, "download failed");
                    let _ = tx.send(Msg::DownloadError {
                        video_id: song.video_id.clone(),
                        error: format!("{e:#}"),
                    });
                }
            });
        }
    });
    DownloadHandle { tx }
}

async fn run_download(
    song: &Song,
    dir: &Path,
    cookies: Option<&Path>,
    tx: &UnboundedSender<Msg>,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create download dir {dir:?}"))?;

    let mut cmd = Command::new("yt-dlp");
    cmd.arg(song.watch_url())
        .args(["-f", "bestaudio", "-x", "--audio-format", "m4a"])
        .args(["--embed-metadata", "--embed-thumbnail", "--no-playlist", "--newline"])
        .arg("-P")
        .arg(dir)
        .args(["-o", "%(artist,uploader,channel)s/%(title)s.%(ext)s"])
        .args(["--progress-template", "download:%(progress._percent_str)s"])
        .args(["--no-simulate", "--print", "after_move:filepath"]);
    if let Some(c) = cookies {
        cmd.arg("--cookies").arg(c);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawn yt-dlp (is it installed?)")?;
    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    // Stream progress from stderr.
    let vid = song.video_id.clone();
    let tx2 = tx.clone();
    let progress = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(pct) = parse_percent(&line) {
                let _ = tx2.send(Msg::DownloadProgress { video_id: vid.clone(), percent: pct });
            }
        }
    });

    // The final path is printed to stdout (small); read it to EOF.
    let mut out = String::new();
    BufReader::new(stdout).read_to_string(&mut out).await?;
    let status = child.wait().await?;
    let _ = progress.await;

    if !status.success() {
        bail!("yt-dlp exited with {status}");
    }
    let path = out.lines().last().unwrap_or_default().trim().to_owned();
    tracing::info!(path = %path, video_id = %song.video_id, "download done");
    let _ = tx.send(Msg::DownloadDone { video_id: song.video_id.clone(), path });
    Ok(())
}

/// Parse a percentage from a `download:<pct>%` progress-template line.
fn parse_percent(line: &str) -> Option<f64> {
    let rest = line.strip_prefix("download:")?;
    rest.trim().trim_end_matches('%').parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress_lines() {
        assert_eq!(parse_percent("download:  45.3%"), Some(45.3));
        assert_eq!(parse_percent("download:100.0%"), Some(100.0));
        assert_eq!(parse_percent("download:   0.0%"), Some(0.0));
    }

    #[test]
    fn ignores_non_progress_lines() {
        assert_eq!(parse_percent("[download] Destination: foo.m4a"), None);
        assert_eq!(parse_percent("download:n/a"), None);
        assert_eq!(parse_percent(""), None);
    }
}
