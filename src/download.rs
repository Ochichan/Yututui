//! Track downloads via `yt-dlp` + `ffmpeg`: best audio → m4a, with embedded metadata
//! and cover art, saved directly under `<download dir>/<title> [<id>].m4a`. The YouTube id
//! is embedded in the filename so a rescan (or a fresh launch) can recover the track's online
//! identity — see [`crate::api::Song::local_file`] / [`crate::api::Song::parse_embedded_id`].
//!
//! The actor receives [`DownloadCmd::Start`] and spawns one task per track, gated by a
//! [`Semaphore`] so only a configured number run at once (priority #1: bounded work).
//! Progress is parsed from yt-dlp's `--progress-template` lines and streamed back as
//! [`Msg::DownloadProgress`]; the final saved path comes from `--print after_move:filepath`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::api::Song;
use crate::app::Msg;
use crate::util::{process, sanitize};

const OUTPUT_TEMPLATE: &str = "%(title)s [%(id)s].%(ext)s";
const YTDLP_STDOUT_MAX: usize = 64 * 1024;

pub enum DownloadCmd {
    Start(Song),
    SetDir(PathBuf),
}

pub struct DownloadHandle {
    tx: UnboundedSender<DownloadCmd>,
}

impl DownloadHandle {
    pub fn start(&self, song: Song) {
        let _ = self.tx.send(DownloadCmd::Start(song));
    }

    pub fn set_dir(&self, dir: PathBuf) {
        let _ = self.tx.send(DownloadCmd::SetDir(dir));
    }
}

/// Spawn the download actor. Results return as `Msg::Download*`.
pub fn spawn(
    msg_tx: UnboundedSender<Msg>,
    dir: PathBuf,
    cookies: Option<PathBuf>,
    max_concurrent: usize,
) -> DownloadHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<DownloadCmd>();
    let sem = Arc::new(Semaphore::new(max_concurrent.max(1)));
    tokio::spawn(async move {
        let mut dir = dir;
        while let Some(cmd) = rx.recv().await {
            match cmd {
                DownloadCmd::SetDir(new_dir) => dir = new_dir,
                DownloadCmd::Start(song) => {
                    let sem = sem.clone();
                    let tx = msg_tx.clone();
                    let dir = dir.clone();
                    let cookies = cookies.clone();
                    tokio::spawn(async move {
                        let Ok(_permit) = sem.acquire_owned().await else {
                            return; // semaphore closed; nothing to do
                        };
                        if let Err(e) = run_download(&song, &dir, cookies.as_deref(), &tx).await {
                            let error = sanitize::sanitize_error_text(format!("{e:#}"));
                            tracing::warn!(error = %error, video_id = %song.video_id, "download failed");
                            let _ = tx.send(Msg::DownloadError {
                                video_id: song.video_id.clone(),
                                error,
                            });
                        }
                    });
                }
            }
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

    let mut cmd = process::tokio_command("yt-dlp", process::ProcessProfile::YtDlp);
    cmd.arg(song.playback_target())
        .args(["-f", "bestaudio", "-x", "--audio-format", "m4a"])
        .args([
            "--embed-metadata",
            "--embed-thumbnail",
            "--no-playlist",
            "--newline",
        ])
        .arg("-P")
        .arg(dir)
        .args(["-o", OUTPUT_TEMPLATE])
        .args(["--progress-template", "download:%(progress._percent_str)s"])
        .args(["--no-simulate", "--print", "after_move:filepath"]);
    if let Some(c) = cookies {
        cmd.arg("--cookies").arg(c);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("spawn yt-dlp (is it installed?)")?;
    // Piped just above, so these are effectively infallible — but a returned error beats a panic
    // that would unwind through the spawned task and leave the download silently dead.
    let stdout = child.stdout.take().context("yt-dlp stdout was not piped")?;
    let stderr = child.stderr.take().context("yt-dlp stderr was not piped")?;

    // Stream progress from stderr.
    let vid = song.video_id.clone();
    let tx2 = tx.clone();
    let progress = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut last_percent: Option<u8> = None;
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(pct) = parse_percent(&line) {
                let rounded = pct.round().clamp(0.0, 100.0) as u8;
                if last_percent == Some(rounded) {
                    continue;
                }
                last_percent = Some(rounded);
                let _ = tx2.send(Msg::DownloadProgress {
                    video_id: vid.clone(),
                    percent: f64::from(rounded),
                });
            }
        }
    });

    // The final path is printed to stdout (small); read it to EOF.
    let mut out = String::new();
    BufReader::new(stdout)
        .take((YTDLP_STDOUT_MAX + 1) as u64)
        .read_to_string(&mut out)
        .await?;
    if out.len() > YTDLP_STDOUT_MAX {
        bail!("yt-dlp output too large");
    }
    let status = child.wait().await?;
    let _ = progress.await;

    if !status.success() {
        bail!("yt-dlp exited with {status}");
    }
    let path = out.lines().last().unwrap_or_default().trim().to_owned();
    tracing::info!(path = %path, video_id = %song.video_id, "download done");
    let _ = tx.send(Msg::DownloadDone {
        video_id: song.video_id.clone(),
        path,
    });
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

    #[test]
    fn output_template_embeds_id_and_has_no_subdirs() {
        // The id tag is what `Song::local_file` recovers on a rescan, so it must be present.
        assert_eq!(OUTPUT_TEMPLATE, "%(title)s [%(id)s].%(ext)s");
        assert!(OUTPUT_TEMPLATE.contains("[%(id)s]"));
        assert!(!OUTPUT_TEMPLATE.contains('/'));
        assert!(!OUTPUT_TEMPLATE.contains('\\'));
    }
}
