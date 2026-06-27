//! Stream prefetch: resolve a track's direct audio URL ahead of time so skipping to it
//! is instant (priority #2).
//!
//! Normally mpv resolves a `watch?v=…` URL itself via its bundled yt-dlp hook on load —
//! ~2-3 s. By pre-resolving the *next* track with `yt-dlp -g` while the current one plays,
//! a skip hands mpv an already-resolved CDN URL it can open immediately (~100 ms). The
//! resolved URL is the app's; staleness (yt-dlp URLs are ~6 h, IP-bound) only matters for
//! sessions longer than that, where mpv falls back to a fresh resolve on error.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::app::Msg;

/// Most concurrent resolves (we only look one ahead, but `prev`/retries can overlap).
const MAX_CONCURRENT: usize = 2;

pub enum ResolveCmd {
    Resolve { video_id: String, watch_url: String },
}

pub struct ResolverHandle {
    tx: UnboundedSender<ResolveCmd>,
}

impl ResolverHandle {
    pub fn resolve(&self, video_id: String, watch_url: String) {
        let _ = self.tx.send(ResolveCmd::Resolve { video_id, watch_url });
    }
}

/// Spawn the resolver actor; results return as [`Msg::Resolved`].
pub fn spawn(msg_tx: UnboundedSender<Msg>, cookies: Option<PathBuf>) -> ResolverHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<ResolveCmd>();
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
    tokio::spawn(async move {
        while let Some(ResolveCmd::Resolve { video_id, watch_url }) = rx.recv().await {
            let sem = sem.clone();
            let tx = msg_tx.clone();
            let cookies = cookies.clone();
            tokio::spawn(async move {
                let Ok(_permit) = sem.acquire_owned().await else {
                    return;
                };
                match resolve_url(&watch_url, cookies.as_deref()).await {
                    Some(stream_url) => {
                        tracing::info!(video_id = %video_id, "prefetched");
                        let _ = tx.send(Msg::Resolved { video_id, stream_url });
                    }
                    None => tracing::warn!(video_id = %video_id, "prefetch failed"),
                }
            });
        }
    });
    ResolverHandle { tx }
}

/// Resolve a watch URL to a direct audio stream URL via `yt-dlp -g`.
async fn resolve_url(watch_url: &str, cookies: Option<&std::path::Path>) -> Option<String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.args(["-f", "bestaudio", "-g", "--no-playlist"]).arg(watch_url);
    if let Some(c) = cookies {
        cmd.arg("--cookies").arg(c);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    let out = cmd.output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout.lines().next().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned)
}
