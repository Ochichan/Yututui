//! Playback subsystem: an mpv child process driven over JSON IPC.
//!
//! [`spawn`] starts mpv, connects the IPC actor, and returns a cheap [`PlayerHandle`]
//! (clone-free command sender) plus an [`Mpv`] lifetime guard. The guard MUST stay in
//! scope for the whole session: dropping it kills mpv. See [`lifetime`] for the full
//! no-orphan story.

pub mod ipc;
pub mod lifetime;
pub mod mpv;
pub mod proto;

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Child;
use tokio::sync::mpsc::{self, UnboundedSender};

use crate::app::Msg;

/// Commands the reducer sends to the player actor.
pub enum PlayerCmd {
    /// Resolve nothing — load this (already-playable) URL and start it.
    Load(String),
    /// Toggle pause/resume.
    CyclePause,
    /// Seek by a relative number of seconds (negative = backward).
    SeekRelative(f64),
    /// Seek to an absolute position in seconds (click-to-seek).
    SeekAbsolute(f64),
    /// Set absolute volume, 0-100.
    SetVolume(i64),
    /// Replace the whole `af` filter chain (the EQ + normalization graph). An empty
    /// string clears all filters.
    SetAudioFilter(String),
    /// Nudge one labeled filter live (e.g. `@eqN` gain) without rebuilding the chain.
    AfCommand { label: String, param: String, value: String },
    /// Set an arbitrary mpv property (e.g. `speed`).
    SetProperty { name: String, value: Value },
}

/// A handle for sending [`PlayerCmd`]s to the player actor. Cheap to hold; sends are
/// non-blocking and silently no-op if the actor has gone away.
pub struct PlayerHandle {
    tx: UnboundedSender<PlayerCmd>,
}

impl PlayerHandle {
    pub fn send(&self, cmd: PlayerCmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn load(&self, url: impl Into<String>) {
        self.send(PlayerCmd::Load(url.into()));
    }
}

/// RAII guard owning the mpv child. Dropping it kills mpv (tokio `kill_on_drop` plus an
/// explicit SIGKILL) and removes the IPC socket — the normal-quit half of the lifeline.
pub struct Mpv {
    child: Child,
    /// The IPC endpoint path. Read only to unlink the Unix socket on drop; Windows named
    /// pipes self-clean, so the field is unused there.
    #[cfg_attr(windows, allow(dead_code))]
    ipc_path: String,
    /// Handle to the kill-on-close Job Object mpv is bound to (Windows only).
    #[cfg(windows)]
    job: Option<isize>,
}

impl Drop for Mpv {
    fn drop(&mut self) {
        lifetime::kill_mpv_now();
        let _ = self.child.start_kill();
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.ipc_path);
        }
        // Closing the job handle terminates mpv (KILL_ON_JOB_CLOSE) — the clean-quit path.
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            lifetime::close_job(job);
        }
    }
}

/// Spawn mpv, wire up the IPC actor, and register the lifeline. `msg_tx` receives
/// player events; `data_dir` (if available) stores the PID registry for orphan reaping;
/// `cookies_file` (if any) is forwarded to mpv's yt-dlp for authenticated streams.
pub async fn spawn(
    msg_tx: UnboundedSender<Msg>,
    data_dir: Option<PathBuf>,
    cookies_file: Option<PathBuf>,
    gapless: bool,
) -> Result<(PlayerHandle, Mpv)> {
    let ipc_path = mpv::ipc_path();
    let child = mpv::spawn(&ipc_path, cookies_file.as_deref(), gapless)?;
    let mpv_pid = child.id().context("mpv exited before reporting a pid")?;

    lifetime::set_mpv_pid(mpv_pid);
    if let Some(dir) = &data_dir {
        lifetime::register(dir, std::process::id(), mpv_pid);
    }

    // Windows: bind mpv to a kill-on-close Job Object as early as possible so it can
    // never outlive us, even on a hard kill. (Unix relies on signals + Drop + reaper.)
    #[cfg(windows)]
    let job = child.raw_handle().and_then(lifetime::assign_to_job);

    let conn = ipc::connect_retry(&ipc_path)
        .await
        .context("could not connect to the mpv IPC endpoint")?;

    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(ipc::run_actor(conn, rx, msg_tx));

    Ok((
        PlayerHandle { tx },
        Mpv {
            child,
            ipc_path,
            #[cfg(windows)]
            job,
        },
    ))
}
