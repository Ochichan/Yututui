//! Track downloads via `yt-dlp` + `ffmpeg`: best audio → m4a, with embedded metadata
//! and cover art, saved directly under `<download dir>/<title> [<id>].m4a`. The YouTube id
//! is embedded in the filename so a rescan (or a fresh launch) can recover the track's online
//! identity — see [`crate::api::Song::local_file`] / [`crate::api::Song::parse_embedded_id`].
//!
//! The actor receives [`DownloadCmd::Start`] and spawns one task per track, gated by a
//! [`Semaphore`] so only a configured number run at once (priority #1: bounded work).
//! Progress is parsed from yt-dlp's `--progress-template` lines and streamed back as
//! [`DownloadEvent::Progress`]; the final saved path comes from `--print after_move:filepath`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{Sender, error::TrySendError};

use crate::api::Song;
use crate::util::{backpressure, sanitize};

const OUTPUT_TEMPLATE: &str = "%(title)s [%(id)s].%(ext)s";
const YTDLP_STDOUT_MAX: usize = 64 * 1024;

pub enum DownloadCmd {
    Start(Box<Song>),
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
        match self.tx.try_send(DownloadCmd::Start(Box::new(song))) {
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
                DownloadCmd::SetDir(new_dir) => {
                    // Once per change (not per download): make the resolved/expanded destination
                    // visible so an env/config override lands somewhere the user can confirm.
                    tracing::info!(dir = %new_dir.display(), "download directory set");
                    dir = new_dir;
                }
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
    run_download_with_program(&crate::tools::ytdlp_program(), song, dir, cookies, emit).await
}

async fn run_download_with_program(
    program: &str,
    song: &Song,
    dir: &Path,
    cookies: Option<&Path>,
    emit: &EventSink,
) -> Result<()> {
    // Re-check at the actor boundary (not just in the UI reducer): a non-video YouTube ref
    // (channel/playlist) would otherwise have `playback_target()` fall back to a channel URL,
    // handing yt-dlp something that isn't a downloadable track.
    if let Some(reason) = song.unplayable_youtube_ref_reason() {
        bail!("not a downloadable track: {reason}");
    }
    let playback_target = song
        .playback_target_checked()
        .with_context(|| format!("invalid download target for {}", song.video_id))?;
    std::fs::create_dir_all(dir).with_context(|| format!("create download dir {dir:?}"))?;

    let mut cmd = crate::tools::ytdlp_command_for(program);
    cmd.arg(playback_target)
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
        use crate::util::io::{BoundedLine, read_bounded_line};
        // A progress line is tiny; cap each so a newline-less stderr blob can't grow a String
        // without bound. On an over-cap line, keep draining in bounded chunks (never break —
        // that would leave yt-dlp blocked writing to a full stderr pipe and stall the
        // download); progress is best-effort, so a dropped fragment just isn't parsed.
        const STDERR_LINE_MAX: usize = 64 * 1024;
        const STDERR_DIAGNOSTIC_LINES_MAX: usize = 8;
        let mut reader = BufReader::new(stderr);
        let mut line: Vec<u8> = Vec::new();
        let mut last_percent: Option<u8> = None;
        let mut diagnostics: VecDeque<String> = VecDeque::new();
        loop {
            line.clear();
            match read_bounded_line(&mut reader, &mut line, STDERR_LINE_MAX).await {
                Ok(BoundedLine::Line) => {}
                Ok(BoundedLine::TooLarge) => continue, // drain the oversized line, then resume
                Ok(BoundedLine::Eof) | Err(_) => break,
            }
            if let Some(pct) = parse_percent(&String::from_utf8_lossy(&line)) {
                let rounded = pct.round().clamp(0.0, 100.0) as u8;
                if last_percent == Some(rounded) {
                    continue;
                }
                last_percent = Some(rounded);
                emit_progress(DownloadEvent::Progress {
                    video_id: vid.clone(),
                    percent: f64::from(rounded),
                });
            } else {
                let trimmed = String::from_utf8_lossy(&line).trim().to_owned();
                if !trimmed.is_empty() {
                    diagnostics.push_back(trimmed.chars().take(200).collect::<String>());
                    if diagnostics.len() > STDERR_DIAGNOSTIC_LINES_MAX {
                        diagnostics.pop_front();
                    }
                }
            }
        }
        diagnostics
    });

    // The final path is printed to stdout (small); read it to EOF, then reap the child — all
    // under an overall backstop timeout so a wedged yt-dlp can't hold its concurrency
    // permit (the `Semaphore`) forever. A real audio download completes well within this; the
    // cap only catches a genuinely stuck process, which is killed and reported as an error.
    const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
    let finish = async {
        let mut out = String::new();
        BufReader::new(stdout)
            .take((YTDLP_STDOUT_MAX + 1) as u64)
            .read_to_string(&mut out)
            .await?;
        if out.len() > YTDLP_STDOUT_MAX {
            bail!("yt-dlp output too large");
        }
        let status = child.wait().await?;
        anyhow::Ok((out, status))
    };
    let (out, status) = match tokio::time::timeout(DOWNLOAD_TIMEOUT, finish).await {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = progress.await;
            bail!("yt-dlp download timed out");
        }
    };
    let stderr_lines = progress.await.unwrap_or_default();

    if !status.success() {
        // Newline-joined so `ytdlp_failure_detail` picks the LAST line (yt-dlp prints the
        // decisive `ERROR:` after its warnings) while classification scans all of them.
        let stderr = stderr_lines.into_iter().collect::<Vec<_>>().join("\n");
        bail!(
            "yt-dlp exited with {status}{}",
            crate::tools::ytdlp_failure_detail(stderr.as_bytes())
        );
    }
    // A zero exit + a printed line is NOT proof the file is really there: a yt-dlp
    // version/option change, a stray stdout line, or an external delete can all leave a bogus
    // path. Validate before reporting `Done` so a bad result surfaces as a normal download
    // error (existing toast) instead of a phantom "saved" track pointing at nothing.
    let path_str = out.lines().last().unwrap_or_default().trim();
    if path_str.is_empty() {
        bail!("yt-dlp reported no output filepath");
    }
    let path = PathBuf::from(path_str);
    let meta = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("downloaded file not found: {}", path.display()))?;
    if !meta.is_file() {
        bail!("downloaded path is not a regular file: {}", path.display());
    }
    if meta.len() == 0 {
        bail!("downloaded file is empty: {}", path.display());
    }
    // Must land inside the requested download directory — a surprising output path or a symlink
    // must not smuggle in a file from elsewhere.
    let root = tokio::fs::canonicalize(dir)
        .await
        .unwrap_or_else(|_| dir.to_path_buf());
    if let Ok(resolved) = tokio::fs::canonicalize(&path).await
        && !resolved.starts_with(&root)
    {
        bail!(
            "downloaded path escaped the download directory: {}",
            path.display()
        );
    }
    // Best-effort id check: the filename embeds `[id]`. A mismatch is logged, not fatal —
    // yt-dlp's resolved id can legitimately differ for some refs.
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        && let Some((_, embedded_id)) = crate::api::Song::parse_embedded_id(stem)
        && embedded_id != song.video_id
    {
        tracing::warn!(embedded_id, requested = %song.video_id, "downloaded file id differs from requested");
    }
    let path = path.to_string_lossy().into_owned();
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
            // Create a real (non-empty) file at the reported path, as yt-dlp would — the actor
            // now validates the file exists/non-empty/in-dir before reporting `Done`.
            &format!(
                "#!/bin/sh\nprintf 'download:  12.4%%\\n' >&2\nprintf 'download:  99.6%%\\n' >&2\nprintf 'audio' > '{saved}'\nprintf '%s\\n' '{saved}'\n",
                saved = saved.display()
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
    #[tokio::test]
    async fn run_download_rejects_a_phantom_path_that_was_never_written() {
        let root = temp_dir("download-phantom");
        let bin_dir = root.join("bin");
        let download_dir = root.join("music");
        fs::create_dir_all(&bin_dir).unwrap();
        // yt-dlp exits 0 and prints a plausible path but never creates the file.
        let phantom = download_dir.join("Track [abc123def45].m4a");
        let fake = write_executable(
            &bin_dir,
            "yt-dlp",
            &format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", phantom.display()),
        );
        let emit: EventSink = Arc::new(|_| {});
        let result = run_download_with_program(
            fake.to_str().unwrap(),
            &Song::remote("abc123def45", "Track", "Artist", "3:12"),
            &download_dir,
            None,
            &emit,
        )
        .await;
        assert!(
            result.is_err(),
            "a phantom (never-written) path must be rejected, not reported as saved"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_download_reports_stderr_line_on_ytdlp_failure() {
        let root = temp_dir("download-stderr");
        let bin_dir = root.join("bin");
        let download_dir = root.join("music");
        fs::create_dir_all(&bin_dir).unwrap();
        // A long warning precedes the decisive ERROR line, as a stale yt-dlp does; the
        // reported detail must keep the LAST line even when earlier lines exceed the
        // 200-char excerpt cap on their own.
        let long_warning = format!("WARNING: {}", "w".repeat(250));
        let fake = write_executable(
            &bin_dir,
            "yt-dlp",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' '{long_warning}' >&2\nprintf '%s\\n' 'ERROR: private video needs login' >&2\nexit 1\n"
            ),
        );
        let emit: EventSink = Arc::new(|_| {});
        let result = run_download_with_program(
            fake.to_str().unwrap(),
            &Song::remote("abc123def45", "Track", "Artist", "3:12"),
            &download_dir,
            None,
            &emit,
        )
        .await
        .expect_err("yt-dlp failure should be reported");
        let error = format!("{result:#}");
        assert!(error.contains("ERROR: private video needs login"));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_download_passes_cookie_file_to_ytdlp() {
        let root = temp_dir("download-cookies");
        let bin_dir = root.join("bin");
        let download_dir = root.join("music");
        fs::create_dir_all(&bin_dir).unwrap();
        let args_log = root.join("args.txt");
        let cookies = root.join("cookies.txt");
        fs::write(&cookies, "# Netscape HTTP Cookie File\n").unwrap();
        let saved = download_dir.join("Track [abc123def45].m4a");
        let fake = write_executable(
            &bin_dir,
            "yt-dlp",
            &format!(
                "#!/bin/sh\nfor arg do printf '%s\\n' \"$arg\"; done > '{args_log}'\nprintf 'audio' > '{saved}'\nprintf '%s\\n' '{saved}'\n",
                args_log = args_log.display(),
                saved = saved.display()
            ),
        );

        let emit: EventSink = Arc::new(|_| {});

        run_download_with_program(
            fake.to_str().unwrap(),
            &Song::remote("abc123def45", "Track", "Artist", "3:12"),
            &download_dir,
            Some(cookies.as_path()),
            &emit,
        )
        .await
        .unwrap();

        let args: Vec<String> = fs::read_to_string(&args_log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let cookie_arg = cookies.to_string_lossy().into_owned();
        assert!(args.iter().any(|arg| arg == "--cookies"));
        assert!(args.iter().any(|arg| arg == &cookie_arg));

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
