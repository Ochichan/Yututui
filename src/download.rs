//! Track downloads via `yt-dlp` + `ffmpeg`: best audio → m4a, with embedded metadata
//! and cover art, saved directly under `<download dir>/<title> [<id>].m4a`. The YouTube id
//! is embedded in the filename so a rescan (or a fresh launch) can recover the track's online
//! identity — see [`crate::api::Song::local_file`] / [`crate::api::Song::parse_embedded_id`].
//!
//! The actor receives [`DownloadCmd::Start`] and spawns one task per track, gated by a
//! [`Semaphore`] so only a configured number run at once (priority #1: bounded work).
//! Progress is parsed from yt-dlp's `--progress-template` lines and streamed back as
//! [`DownloadEvent::Progress`]; the final saved path comes from `--print after_move:filepath`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{Sender, error::TrySendError};

use crate::api::Song;
use crate::util::{backpressure, process, sanitize};

const OUTPUT_TEMPLATE: &str = "%(title)s [%(id)s].%(ext)s";
const YTDLP_STDOUT_MAX: usize = 64 * 1024;

pub enum DownloadCmd {
    Start(Song),
    SetDir(PathBuf),
}

pub enum DownloadEvent {
    Progress { video_id: String, percent: f64 },
    Done { video_id: String, path: String },
    Error { video_id: String, error: String },
}

type EventSink = Arc<dyn Fn(DownloadEvent) + Send + Sync>;

pub struct DownloadHandle {
    tx: Sender<DownloadCmd>,
}

pub struct DownloadStartError {
    pub video_id: String,
}

impl DownloadHandle {
    pub fn start(&self, song: Song) -> std::result::Result<(), DownloadStartError> {
        match self.tx.try_send(DownloadCmd::Start(song)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(DownloadCmd::Start(song)))
            | Err(TrySendError::Closed(DownloadCmd::Start(song))) => Err(DownloadStartError {
                video_id: song.video_id,
            }),
            Err(TrySendError::Full(DownloadCmd::SetDir(_)))
            | Err(TrySendError::Closed(DownloadCmd::SetDir(_))) => unreachable!(),
        }
    }

    pub fn set_dir(&self, dir: PathBuf) -> bool {
        self.tx.try_send(DownloadCmd::SetDir(dir)).is_ok()
    }
}

/// Spawn the download actor. Results return as [`DownloadEvent`]s.
pub fn spawn<F>(
    emit: F,
    dir: PathBuf,
    cookies: Option<PathBuf>,
    max_concurrent: usize,
) -> DownloadHandle
where
    F: Fn(DownloadEvent) + Send + Sync + 'static,
{
    let (tx, mut rx) = backpressure::bounded_channel::<DownloadCmd>(backpressure::DOWNLOAD_QUEUE);
    let sem = Arc::new(Semaphore::new(max_concurrent.max(1)));
    let emit: EventSink = Arc::new(emit);
    tokio::spawn(async move {
        let mut dir = dir;
        while let Some(cmd) = rx.recv().await {
            match cmd {
                DownloadCmd::SetDir(new_dir) => dir = new_dir,
                DownloadCmd::Start(song) => {
                    let Ok(permit) = sem.clone().acquire_owned().await else {
                        return;
                    };
                    let emit = emit.clone();
                    let dir = dir.clone();
                    let cookies = cookies.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        if let Err(e) = run_download(&song, &dir, cookies.as_deref(), &emit).await {
                            let error = sanitize::sanitize_error_text(format!("{e:#}"));
                            tracing::warn!(error = %error, video_id = %song.video_id, "download failed");
                            emit(DownloadEvent::Error {
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
    emit: &EventSink,
) -> Result<()> {
    run_download_with_program("yt-dlp", song, dir, cookies, emit).await
}

async fn run_download_with_program(
    program: &str,
    song: &Song,
    dir: &Path,
    cookies: Option<&Path>,
    emit: &EventSink,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create download dir {dir:?}"))?;

    let mut cmd = process::tokio_command(program, process::ProcessProfile::YtDlp);
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
    let emit_progress = emit.clone();
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
                emit_progress(DownloadEvent::Progress {
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
    emit(DownloadEvent::Done {
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
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::sync::Mutex;
    #[cfg(unix)]
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[cfg(unix)]
    #[tokio::test]
    async fn run_download_uses_fake_ytdlp_progress_and_final_path() {
        let root = temp_dir("download");
        let bin_dir = root.join("bin");
        let download_dir = root.join("music");
        fs::create_dir_all(&bin_dir).unwrap();

        let saved = download_dir.join("Track [abc123def45].m4a");
        let fake = write_executable(
            &bin_dir,
            "yt-dlp",
            &format!(
                "#!/bin/sh\nprintf 'download:  12.4%%\\n' >&2\nprintf 'download:  99.6%%\\n' >&2\nprintf '%s\\n' '{}'\n",
                saved.display()
            ),
        );

        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let emit: EventSink = Arc::new(move |event| {
            captured.lock().unwrap().push(event);
        });

        run_download_with_program(
            fake.to_str().unwrap(),
            &Song::remote("abc123def45", "Track", "Artist", "3:12"),
            &download_dir,
            None,
            &emit,
        )
        .await
        .unwrap();

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 3);
        match &events[0] {
            DownloadEvent::Progress { video_id, percent } => {
                assert_eq!(video_id, "abc123def45");
                assert_eq!(*percent, 12.0);
            }
            _ => panic!("expected first progress event"),
        }
        match &events[1] {
            DownloadEvent::Progress { video_id, percent } => {
                assert_eq!(video_id, "abc123def45");
                assert_eq!(*percent, 100.0);
            }
            _ => panic!("expected second progress event"),
        }
        match &events[2] {
            DownloadEvent::Done { video_id, path } => {
                assert_eq!(video_id, "abc123def45");
                assert_eq!(path.as_str(), saved.to_string_lossy().as_ref());
            }
            _ => panic!("expected done event"),
        }

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    fn write_executable(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ytm-tui-{name}-test-{}-{nanos}",
            std::process::id()
        ))
    }
}
