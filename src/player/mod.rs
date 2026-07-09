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
pub mod video;

use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Child;
use tokio::sync::mpsc::{self, Sender};

/// Commands the reducer sends to the player actor.
pub enum PlayerCmd {
    /// Resolve nothing — load this (already-playable) URL and start it.
    Load(String),
    /// Stop the current mpv file without advancing mpv's own playlist.
    Stop,
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
    AfCommand {
        label: String,
        param: String,
        value: String,
    },
    /// Set an arbitrary mpv property (e.g. `speed`).
    SetProperty { name: String, value: Value },
}

enum CmdLossPolicy {
    Critical,
    LatestWins,
}

impl PlayerCmd {
    fn loss_policy(&self) -> CmdLossPolicy {
        match self {
            PlayerCmd::Load(_)
            | PlayerCmd::Stop
            | PlayerCmd::CyclePause
            | PlayerCmd::SetAudioFilter(_)
            | PlayerCmd::SetProperty { .. } => CmdLossPolicy::Critical,
            PlayerCmd::SeekRelative(_)
            | PlayerCmd::SeekAbsolute(_)
            | PlayerCmd::SetVolume(_)
            | PlayerCmd::AfCommand { .. } => CmdLossPolicy::LatestWins,
        }
    }
}

/// Events emitted by the mpv IPC actor.
pub enum PlayerEvent {
    TimePos(f64),
    /// Track duration in seconds. `None` when mpv reports the property as unavailable
    /// (live stream, teardown) *after* it had a value — the reducers must see the loss
    /// so a stale length never outlives its file (same contract as [`Self::CacheTime`]).
    Duration(Option<f64>),
    Paused(bool),
    Volume(f64),
    Metadata(Value),
    /// `demuxer-cache-time`: the timestamp of the newest demuxed data — for a live radio
    /// stream, the live edge. `None` when mpv reports the property as unavailable (the
    /// reducer must see the loss, unlike the always-numeric `time-pos`).
    CacheTime(Option<f64>),
    /// mpv `audio-codec-name` (e.g. `mp3`, `aac`) — the radio recorder maps it to the
    /// passthrough container extension. `None` when the property is unavailable.
    AudioCodec(Option<String>),
    /// mpv `file-format` (container) — a fallback / HLS signal for the recorder's extension
    /// choice. `None` when unavailable.
    FileFormat(Option<String>),
    Eof,
    Error(String),
}

pub(crate) type EventSink = Arc<dyn Fn(PlayerEvent) + Send + Sync>;

/// A handle for sending [`PlayerCmd`]s to the player actor. Cheap to hold; sends are
/// non-blocking and silently no-op if the actor has gone away.
pub struct PlayerHandle {
    tx: Sender<PlayerCmd>,
    urgent_tx: Sender<PlayerCmd>,
    latest: Arc<Mutex<LatestPending>>,
    latest_drain_running: Arc<AtomicBool>,
}

impl PlayerHandle {
    pub fn send(&self, cmd: PlayerCmd) {
        match cmd.loss_policy() {
            CmdLossPolicy::Critical => self.send_critical(cmd),
            CmdLossPolicy::LatestWins => self.send_latest(cmd),
        }
    }

    fn send_critical(&self, cmd: PlayerCmd) {
        match self.urgent_tx.try_send(cmd) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                let tx = self.urgent_tx.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        if tokio::time::timeout(Duration::from_millis(250), tx.send(cmd))
                            .await
                            .is_err()
                        {
                            tracing::warn!(
                                "player urgent command queue stayed full; dropping critical command"
                            );
                        }
                    });
                } else {
                    tracing::warn!(
                        "player urgent command queue full outside runtime; dropping critical command"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("player actor stopped before critical command");
            }
        }
    }

    fn send_latest(&self, cmd: PlayerCmd) {
        match self.tx.try_send(cmd) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                self.latest
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(cmd);
                self.spawn_latest_drain();
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::debug!("player actor stopped before latest-wins command");
            }
        }
    }

    pub fn load(&self, url: impl Into<String>) {
        self.send(PlayerCmd::Load(url.into()));
    }

    fn spawn_latest_drain(&self) {
        if self.latest_drain_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let tx = self.tx.clone();
        let latest = Arc::clone(&self.latest);
        let running = Arc::clone(&self.latest_drain_running);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(drain_latest_queue(tx, latest, running));
        } else {
            self.latest_drain_running.store(false, Ordering::Release);
            tracing::debug!("player latest-wins command queued outside runtime");
        }
    }
}

#[derive(Default)]
struct LatestPending {
    seek_relative: f64,
    seek_absolute: Option<f64>,
    volume: Option<i64>,
    af_commands: Vec<PlayerCmd>,
}

impl LatestPending {
    fn push(&mut self, cmd: PlayerCmd) {
        match cmd {
            PlayerCmd::SeekRelative(secs) => self.seek_relative += secs,
            PlayerCmd::SeekAbsolute(secs) => self.seek_absolute = Some(secs),
            PlayerCmd::SetVolume(vol) => self.volume = Some(vol),
            PlayerCmd::AfCommand {
                label,
                param,
                value,
            } => {
                self.af_commands.retain(|cmd| {
                    !matches!(
                        cmd,
                        PlayerCmd::AfCommand {
                            label: existing_label,
                            param: existing_param,
                            ..
                        } if existing_label == &label && existing_param == &param
                    )
                });
                self.af_commands.push(PlayerCmd::AfCommand {
                    label,
                    param,
                    value,
                });
            }
            cmd => self.af_commands.push(cmd),
        }
    }

    fn take_one(&mut self) -> Option<PlayerCmd> {
        if self.seek_relative != 0.0 {
            let secs = self.seek_relative;
            self.seek_relative = 0.0;
            return Some(PlayerCmd::SeekRelative(secs));
        }
        if let Some(secs) = self.seek_absolute.take() {
            return Some(PlayerCmd::SeekAbsolute(secs));
        }
        if let Some(vol) = self.volume.take() {
            return Some(PlayerCmd::SetVolume(vol));
        }
        if self.af_commands.is_empty() {
            None
        } else {
            Some(self.af_commands.remove(0))
        }
    }

    fn has_pending(&self) -> bool {
        self.seek_relative != 0.0
            || self.seek_absolute.is_some()
            || self.volume.is_some()
            || !self.af_commands.is_empty()
    }
}

async fn drain_latest_queue(
    tx: Sender<PlayerCmd>,
    latest: Arc<Mutex<LatestPending>>,
    running: Arc<AtomicBool>,
) {
    loop {
        loop {
            let permit = match tx.reserve().await {
                Ok(permit) => permit,
                Err(_) => {
                    running.store(false, Ordering::Release);
                    return;
                }
            };
            let Some(cmd) = latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take_one()
            else {
                drop(permit);
                break;
            };
            permit.send(cmd);
        }

        running.store(false, Ordering::Release);
        let still_pending = latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .has_pending();
        if !still_pending || running.swap(true, Ordering::AcqRel) {
            return;
        }
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

/// Spawn mpv, wire up the IPC actor, and register the lifeline. `emit` receives
/// player events; `data_dir` (if available) stores the PID registry for orphan reaping;
/// `cookies_file` (if any) is forwarded to mpv's yt-dlp for authenticated streams.
pub async fn spawn<F>(
    emit: F,
    data_dir: Option<PathBuf>,
    cookies_file: Option<PathBuf>,
    gapless: bool,
) -> Result<(PlayerHandle, Mpv)>
where
    F: Fn(PlayerEvent) + Send + Sync + 'static,
{
    let ipc_path = mpv::ipc_path()?;
    let child = mpv::spawn(&ipc_path, cookies_file.as_deref(), gapless)?;
    let mpv_pid = child.id().context("mpv exited before reporting a pid")?;

    lifetime::set_mpv_pid(mpv_pid);
    if let Some(dir) = &data_dir {
        lifetime::register(dir, std::process::id(), mpv_pid, &ipc_path);
    }

    // Windows: bind mpv to a kill-on-close Job Object as early as possible so it can
    // never outlive us, even on a hard kill. (Unix relies on signals + Drop + reaper.)
    #[cfg(windows)]
    let job = child.raw_handle().and_then(lifetime::assign_to_job);

    let conn = ipc::connect_retry(&ipc_path)
        .await
        .context("could not connect to the mpv IPC endpoint")?;

    let (tx, rx) =
        crate::util::backpressure::bounded_channel(crate::util::backpressure::PLAYER_CMD_QUEUE);
    let (urgent_tx, urgent_rx) =
        crate::util::backpressure::bounded_channel(crate::util::backpressure::PLAYER_CMD_QUEUE);
    tokio::spawn(ipc::run_actor(conn, urgent_rx, rx, Arc::new(emit)));

    Ok((
        PlayerHandle {
            tx,
            urgent_tx,
            latest: Arc::new(Mutex::new(LatestPending::default())),
            latest_drain_running: Arc::new(AtomicBool::new(false)),
        },
        Mpv {
            child,
            ipc_path,
            #[cfg(windows)]
            job,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_handle(tx: Sender<PlayerCmd>, urgent_tx: Sender<PlayerCmd>) -> PlayerHandle {
        PlayerHandle {
            tx,
            urgent_tx,
            latest: Arc::new(Mutex::new(LatestPending::default())),
            latest_drain_running: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn latest_pending_coalesces_lossy_commands() {
        let mut pending = LatestPending::default();
        pending.push(PlayerCmd::SeekRelative(2.0));
        pending.push(PlayerCmd::SeekRelative(3.5));
        pending.push(PlayerCmd::SeekAbsolute(10.0));
        pending.push(PlayerCmd::SeekAbsolute(12.0));
        pending.push(PlayerCmd::SetVolume(20));
        pending.push(PlayerCmd::SetVolume(30));
        pending.push(PlayerCmd::AfCommand {
            label: "eq".to_owned(),
            param: "gain".to_owned(),
            value: "1".to_owned(),
        });
        pending.push(PlayerCmd::AfCommand {
            label: "eq".to_owned(),
            param: "gain".to_owned(),
            value: "2".to_owned(),
        });

        match pending.take_one() {
            Some(PlayerCmd::SeekRelative(secs)) => assert_eq!(secs, 5.5),
            _ => panic!("expected coalesced relative seek"),
        }
        match pending.take_one() {
            Some(PlayerCmd::SeekAbsolute(secs)) => assert_eq!(secs, 12.0),
            _ => panic!("expected latest absolute seek"),
        }
        match pending.take_one() {
            Some(PlayerCmd::SetVolume(vol)) => assert_eq!(vol, 30),
            _ => panic!("expected latest volume"),
        }
        match pending.take_one() {
            Some(PlayerCmd::AfCommand {
                label,
                param,
                value,
            }) => {
                assert_eq!(label, "eq");
                assert_eq!(param, "gain");
                assert_eq!(value, "2");
            }
            _ => panic!("expected latest af command"),
        }
        assert!(pending.take_one().is_none());
    }

    #[tokio::test]
    async fn critical_commands_use_urgent_lane_when_normal_queue_is_full() {
        let (tx, mut rx) = mpsc::channel(1);
        let (urgent_tx, mut urgent_rx) = mpsc::channel(1);
        assert!(tx.try_send(PlayerCmd::SetVolume(1)).is_ok());
        let handle = test_handle(tx, urgent_tx);

        handle.send(PlayerCmd::Stop);

        assert!(matches!(urgent_rx.try_recv(), Ok(PlayerCmd::Stop)));
        assert!(matches!(rx.try_recv(), Ok(PlayerCmd::SetVolume(1))));
    }

    #[tokio::test]
    async fn latest_wins_commands_buffer_when_normal_queue_is_full() {
        let (tx, _rx) = mpsc::channel(1);
        let (urgent_tx, _urgent_rx) = mpsc::channel(1);
        assert!(tx.try_send(PlayerCmd::SetVolume(1)).is_ok());
        let handle = test_handle(tx, urgent_tx);

        handle.send(PlayerCmd::SetVolume(80));

        let mut latest = handle
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(matches!(latest.take_one(), Some(PlayerCmd::SetVolume(80))));
    }
}
