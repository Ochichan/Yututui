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
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(12);
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
                .is_ok_and(|mut ids| ids.insert(video_id.as_str().to_owned()))
            {
                tracing::debug!(video_id = %video_id, "prefetch already in flight");
                continue;
            }
            let Ok(permit) = sem.clone().acquire_owned().await else {
                if let Ok(mut ids) = in_flight.lock() {
                    ids.remove(video_id.as_str());
                }
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
                if let Ok(mut ids) = in_flight.lock() {
                    ids.remove(video_id.as_str());
                }
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
    let mut cmd = process::tokio_command(program, process::ProcessProfile::YtDlp);
    cmd.args(["-f", "bestaudio", "-g", "--no-playlist"])
        .arg(watch_url);
    if let Some(c) = cookies {
        cmd.arg("--cookies").arg(c);
    }
    cmd.stdin(Stdio::null()).stderr(Stdio::null());
    let out = process::tokio_output_limited(cmd, RESOLVE_TIMEOUT, RESOLVE_STDOUT_MAX)
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
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

    fn write_executable(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ytm-tui-resolver-test-{}-{nanos}",
            std::process::id()
        ))
    }
}
