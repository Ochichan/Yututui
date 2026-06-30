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
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::app::Msg;
use crate::util::process;

/// Most concurrent resolves (we only look one ahead, but `prev`/retries can overlap).
const MAX_CONCURRENT: usize = 2;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(12);
const RESOLVE_STDOUT_MAX: usize = 16 * 1024;

pub enum ResolveCmd {
    Resolve { video_id: String, watch_url: String },
}

pub struct ResolverHandle {
    tx: UnboundedSender<ResolveCmd>,
}

impl ResolverHandle {
    pub fn resolve(&self, video_id: String, watch_url: String) {
        let _ = self.tx.send(ResolveCmd::Resolve {
            video_id,
            watch_url,
        });
    }
}

/// Spawn the resolver actor; results return as [`Msg::Resolved`].
pub fn spawn(msg_tx: UnboundedSender<Msg>, cookies: Option<PathBuf>) -> ResolverHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<ResolveCmd>();
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let in_flight = Arc::new(Mutex::new(HashSet::<String>::new()));
    tokio::spawn(async move {
        while let Some(ResolveCmd::Resolve {
            video_id,
            watch_url,
        }) = rx.recv().await
        {
            if !in_flight
                .lock()
                .is_ok_and(|mut ids| ids.insert(video_id.clone()))
            {
                tracing::debug!(video_id = %video_id, "prefetch already in flight");
                continue;
            }
            let sem = sem.clone();
            let tx = msg_tx.clone();
            let cookies = cookies.clone();
            let in_flight = in_flight.clone();
            tokio::spawn(async move {
                let Ok(_permit) = sem.acquire_owned().await else {
                    if let Ok(mut ids) = in_flight.lock() {
                        ids.remove(&video_id);
                    }
                    return;
                };
                match resolve_url(&watch_url, cookies.as_deref()).await {
                    Some(stream_url) => {
                        tracing::info!(video_id = %video_id, "prefetched");
                        let _ = tx.send(Msg::Resolved {
                            video_id: video_id.clone(),
                            stream_url,
                        });
                    }
                    None => tracing::warn!(video_id = %video_id, "prefetch failed"),
                }
                if let Ok(mut ids) = in_flight.lock() {
                    ids.remove(&video_id);
                }
            });
        }
    });
    ResolverHandle { tx }
}

/// Resolve a watch URL to a direct audio stream URL via `yt-dlp -g`.
async fn resolve_url(watch_url: &str, cookies: Option<&std::path::Path>) -> Option<String> {
    let mut cmd = process::tokio_command("yt-dlp", process::ProcessProfile::YtDlp);
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
