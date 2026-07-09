//! Stream prefetch: resolve a track's direct audio URL ahead of time so skipping to it
//! is instant (priority #2).
//!
//! Normally mpv resolves a `watch?v=…` URL itself via its bundled yt-dlp hook on load —
//! ~2-3 s. By pre-resolving the *next* track with `yt-dlp -g` while the current one plays,
//! a skip hands mpv an already-resolved CDN URL it can open immediately (~100 ms). The
//! resolved URL is the app's; staleness (yt-dlp URLs are ~6 h, IP-bound) only matters for
//! sessions longer than that, where mpv falls back to a fresh resolve on error.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::sync::mpsc::Sender;

use crate::ids::{StreamUrl, VideoId, WatchUrl};
use crate::util::{backpressure, process};

/// Most concurrent resolves (we only look one ahead, but `prev`/retries can overlap).
const MAX_CONCURRENT: usize = 2;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(20);
const RESOLVE_STDOUT_MAX: usize = 16 * 1024;

pub enum ResolveCmd {
    Resolve {
        video_id: VideoId,
        watch_url: WatchUrl,
    },
}

pub enum ResolverEvent {
    Resolved {
        video_id: VideoId,
        stream_url: StreamUrl,
    },
    /// Resolution failed (yt-dlp error/timeout). Consumed by the playback self-heal's
    /// retry (which must not hang on a silent failure); plain prefetch listeners
    /// ignore it — a missed prefetch only means a slower skip later.
    Failed { video_id: VideoId },
}

type EventSink = Arc<dyn Fn(ResolverEvent) + Send + Sync>;

pub struct ResolverHandle {
    tx: Sender<ResolveCmd>,
}

impl ResolverHandle {
    pub fn resolve(&self, video_id: String, watch_url: String) -> bool {
        self.tx
            .try_send(ResolveCmd::Resolve {
                video_id: VideoId::from(video_id),
                watch_url: WatchUrl::from(watch_url),
            })
            .is_ok()
    }

    pub fn resolve_or_log(&self, video_id: String, watch_url: String) {
        if !self.resolve(video_id.clone(), watch_url) {
            tracing::debug!(video_id = %video_id, "prefetch queue full; dropping request");
        }
    }
}

/// Spawn the resolver actor; results return as [`ResolverEvent`]s.
pub fn spawn<F>(emit: F, cookies: Option<PathBuf>) -> ResolverHandle
where
    F: Fn(ResolverEvent) + Send + Sync + 'static,
{
    let (tx, mut rx) = backpressure::bounded_channel::<ResolveCmd>(backpressure::RESOLVER_QUEUE);
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let in_flight = Arc::new(Mutex::new(HashSet::<String>::new()));
    let emit: EventSink = Arc::new(emit);
    tokio::spawn(async move {
        while let Some(ResolveCmd::Resolve {
            video_id,
            watch_url,
        }) = rx.recv().await
        {
            if !in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(video_id.as_str().to_owned())
            {
                tracing::debug!(video_id = %video_id, "prefetch already in flight");
                continue;
            }
            let Ok(permit) = sem.clone().acquire_owned().await else {
                in_flight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(video_id.as_str());
                return;
            };
            let emit = emit.clone();
            let cookies = cookies.clone();
            let in_flight = in_flight.clone();
            tokio::spawn(async move {
                let _permit = permit;
                match resolve_url(watch_url.as_str(), cookies.as_deref()).await {
                    Some(stream_url) => {
                        tracing::info!(video_id = %video_id, "prefetched");
                        emit(ResolverEvent::Resolved {
                            video_id: video_id.clone(),
                            stream_url: StreamUrl::from(stream_url),
                        });
                    }
                    None => {
                        tracing::warn!(video_id = %video_id, "prefetch failed");
                        emit(ResolverEvent::Failed {
                            video_id: video_id.clone(),
                        });
                    }
                }
                in_flight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(video_id.as_str());
            });
        }
    });
    ResolverHandle { tx }
}

/// Resolve a watch URL to a direct audio stream URL via `yt-dlp -g`.
async fn resolve_url(watch_url: &str, cookies: Option<&std::path::Path>) -> Option<String> {
    // Looked up per invocation (not captured at actor spawn) so a managed yt-dlp
    // installed mid-session — including by the playback self-heal — applies to the
    // very next resolve.
    resolve_url_with_program(&crate::tools::ytdlp_program(), watch_url, cookies).await
}

async fn resolve_url_with_program(
    program: &str,
    watch_url: &str,
    cookies: Option<&std::path::Path>,
) -> Option<String> {
    let watch_url = crate::api::validate_playable_url_destination(
        crate::search_source::SearchSource::Youtube,
        watch_url,
    )
    .await
    .map_err(|error| {
        tracing::warn!(%error, "refusing to resolve unsafe watch URL");
    })
    .ok()?;
    if let Some(stream_url) =
        resolve_url_with_format(program, &watch_url, cookies, "bestaudio", "audio-only").await
    {
        return Some(stream_url);
    }
    resolve_url_with_format(
        program,
        &watch_url,
        cookies,
        "bestaudio/best[acodec!=none]/best",
        "audio-containing fallback",
    )
    .await
}

async fn resolve_url_with_format(
    program: &str,
    watch_url: &str,
    cookies: Option<&std::path::Path>,
    format_selector: &str,
    stage: &str,
) -> Option<String> {
    let mut cmd = crate::tools::ytdlp_command_for(program);
    cmd.args(["-f", format_selector, "-g", "--no-playlist"])
        .arg(watch_url);
    crate::tools::append_ytdlp_cookie_args(&mut cmd, cookies);
    // Match mpv's cookie-auth stream client so prefetched CDN URLs are not the
    // TVHTML5 URLs that ffmpeg then rejects with HTTP 403.
    if cookies.is_some() {
        crate::tools::append_ytdlp_youtube_stream_extractor_args(&mut cmd);
    }
    cmd.stdin(Stdio::null());
    let out = process::tokio_output_limited(
        cmd,
        process::ProcessProfile::YtDlp,
        RESOLVE_TIMEOUT,
        RESOLVE_STDOUT_MAX,
    )
    .await
    .ok()?;
    if !out.status.success() {
        tracing::warn!(
            status = %out.status,
            stage,
            detail = %crate::tools::ytdlp_failure_detail(&out.stderr_tail).trim_start(),
            "yt-dlp resolve failed"
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|l| is_acceptable_stream_url(l))
        .map(str::to_owned)
}

/// Guard the URL yt-dlp hands back before it is cached and passed to mpv `loadfile`.
/// The API validator rejects non-HTTP(S), credentials, control characters, and local/private IPs.
/// This keeps `file:`/`data:`/local-path/garbage — e.g. from an override binary or a broken
/// yt-dlp — out of the player. Same-user trust is already low; this matches the resolver's
/// existing defense-in-depth (bounded stdout, timeout).
fn is_acceptable_stream_url(s: &str) -> bool {
    crate::api::validate_playable_url(crate::search_source::SearchSource::Youtube, s).is_ok()
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[tokio::test]
    async fn resolve_url_uses_fake_ytdlp_stdout() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let fake = write_executable(
            &dir,
            "yt-dlp",
            "#!/bin/sh\nprintf '%s\\n' 'https://cdn.example/audio.m4a'\n",
        );

        let resolved = resolve_url_with_program(
            fake.to_str().unwrap(),
            "https://music.youtube.com/watch?v=abc123",
            None,
        )
        .await;

        assert_eq!(resolved.as_deref(), Some("https://cdn.example/audio.m4a"));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_url_passes_cookie_file_to_ytdlp() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let args_log = dir.join("args.txt");
        let cookies = dir.join("cookies.txt");
        fs::write(&cookies, "# Netscape HTTP Cookie File\n").unwrap();
        let fake = write_executable(
            &dir,
            "yt-dlp",
            &format!(
                "#!/bin/sh\nfor arg do printf '%s\\n' \"$arg\"; done > '{}'\nprintf '%s\\n' 'https://cdn.example/audio.m4a'\n",
                args_log.display()
            ),
        );

        let resolved = resolve_url_with_program(
            fake.to_str().unwrap(),
            "https://music.youtube.com/watch?v=abc123",
            Some(cookies.as_path()),
        )
        .await;

        assert_eq!(resolved.as_deref(), Some("https://cdn.example/audio.m4a"));
        let args: Vec<String> = fs::read_to_string(&args_log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let cookie_arg = cookies.to_string_lossy().into_owned();
        assert!(args.iter().any(|arg| arg == "--cookies"));
        assert!(args.iter().any(|arg| arg == &cookie_arg));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_url_falls_back_to_audio_containing_format() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let args_log = dir.join("args.txt");
        let fake = write_executable(
            &dir,
            "yt-dlp",
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n  *'bestaudio/best[acodec!=none]/best'*) printf '%s\\n' 'https://cdn.example/fallback.mp4' ;;\n  *) exit 1 ;;\nesac\n",
                args_log.display()
            ),
        );

        let resolved = resolve_url_with_program(
            fake.to_str().unwrap(),
            "https://music.youtube.com/watch?v=abc123",
            None,
        )
        .await;

        assert_eq!(
            resolved.as_deref(),
            Some("https://cdn.example/fallback.mp4")
        );
        let args = fs::read_to_string(&args_log).unwrap();
        assert!(args.contains("-f bestaudio -g --no-playlist"));
        assert!(args.contains("-f bestaudio/best[acodec!=none]/best -g --no-playlist"));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_url_rejects_non_http_scheme() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        // A broken/hostile yt-dlp emitting a local path or file: URL must not reach mpv.
        let fake = write_executable(
            &dir,
            "yt-dlp",
            "#!/bin/sh\nprintf '%s\\n' 'file:///etc/passwd'\n",
        );
        let resolved =
            resolve_url_with_program(fake.to_str().unwrap(), "https://x/watch?v=abc123", None)
                .await;
        assert_eq!(resolved, None, "non-http scheme is rejected");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stream_url_guard_allows_http_and_rejects_junk() {
        assert!(is_acceptable_stream_url("https://cdn.example/a.m4a"));
        assert!(is_acceptable_stream_url("HTTP://cdn.example/a.m4a"));
        assert!(!is_acceptable_stream_url("file:///etc/passwd"));
        assert!(!is_acceptable_stream_url("data:audio/mp3;base64,AAAA"));
        assert!(!is_acceptable_stream_url("/local/path.m4a"));
        assert!(!is_acceptable_stream_url("https://a\x1b.example/x"));
        assert!(!is_acceptable_stream_url(""));
    }

    fn write_executable(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn temp_dir() -> PathBuf {
        // A per-call sequence, not just a timestamp: these tests start simultaneously and
        // macOS's clock granularity made same-nanos collisions real — two tests sharing a
        // dir overwrite each other's fake `yt-dlp`.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "yututui-resolver-test-{}-{nanos}-{seq}",
            std::process::id()
        ))
    }
}
