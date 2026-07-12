//! Playback subsystem: an mpv child process driven over JSON IPC.
//!
//! [`spawn`] starts mpv, connects the IPC actor, and returns a cheap [`PlayerHandle`]
//! (clone-free command sender) plus an [`Mpv`] lifetime guard. The guard MUST stay in
//! scope for the whole session: dropping it kills mpv. See [`lifetime`] for the full
//! no-orphan story.

pub mod backend;
pub mod ipc;
pub mod lifetime;
pub mod mpv;
pub(crate) mod pending;
pub mod proto;
pub mod video;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Child;
use tokio::sync::mpsc::{self, Sender};

use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::util::process::ProcessProfile;
use crate::util::process_guard::ChildTreeGuard;

/// Commands the reducer sends to the player actor.
#[derive(Clone)]
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
    /// Set a property whose exact mpv reply is a resource-lifetime boundary. The recorder uses
    /// this only for the final `stream-record` command which closes the previous source.
    TrackedProperty(TrackedProperty),
}

#[derive(Clone)]
pub struct TrackedProperty {
    pub(crate) name: String,
    pub(crate) value: Value,
    pub(crate) acknowledgement: crate::util::command_barrier::CommandBarrierSignal,
}

impl PlayerCmd {
    fn is_coalescing_barrier(&self) -> bool {
        matches!(
            self,
            Self::Load(_)
                | Self::Stop
                | Self::CyclePause
                | Self::SetAudioFilter(_)
                | Self::SetProperty { .. }
                | Self::TrackedProperty(_)
        )
    }

    fn invalidates_file_generation(&self) -> bool {
        matches!(self, Self::Load(_) | Self::Stop)
    }

    pub(crate) fn tracked_property(
        name: String,
        value: Value,
        barrier: &crate::util::command_barrier::CommandBarrier,
    ) -> Self {
        Self::TrackedProperty(TrackedProperty {
            name,
            value,
            acknowledgement: barrier.signal(),
        })
    }

    #[cfg(test)]
    pub(crate) fn property(&self) -> Option<(&str, &Value)> {
        match self {
            Self::SetProperty { name, value } => Some((name, value)),
            Self::TrackedProperty(tracked) => Some((&tracked.name, &tracked.value)),
            _ => None,
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
    /// The mpv IPC transport ended unexpectedly. The detail is sanitized at the actor
    /// boundary and this terminal event is emitted at most once per actor lifetime.
    TransportClosed(String),
    /// Per-file mpv state tagged at the ordered `start-file` boundary. Owners compare this
    /// generation with the latest admitted Load/Stop before reducing the enclosed event.
    FileScoped {
        file_generation: u64,
        event: Box<PlayerEvent>,
    },
}

impl PlayerEvent {
    pub(crate) fn file_scoped(file_generation: u64, event: Self) -> Self {
        debug_assert!(!matches!(event, Self::FileScoped { .. }));
        Self::FileScoped {
            file_generation,
            event: Box::new(event),
        }
    }

    /// The admitted audio-file generation for events which belong to one loaded file.
    /// Runtime owners reject delayed telemetry and terminal state after a newer Load/Stop.
    pub fn file_generation(&self) -> Option<u64> {
        match self {
            Self::FileScoped {
                file_generation, ..
            } => Some(*file_generation),
            _ => None,
        }
    }

    pub(crate) fn unscoped(&self) -> &Self {
        match self {
            Self::FileScoped { event, .. } => event,
            _ => self,
        }
    }

    pub(crate) fn into_unscoped(self) -> Self {
        match self {
            Self::FileScoped { event, .. } => *event,
            event => event,
        }
    }
}

pub(crate) type EventSink = Arc<dyn Fn(PlayerEvent) + Send + Sync>;

/// A handle for sending [`PlayerCmd`]s to the player actor. Commands enter one ordered
/// bounded lane; when that lane is full, a single drainer owns a bounded semantic backlog.
pub struct PlayerHandle {
    tx: Sender<PlayerCmd>,
    pending: Arc<Mutex<PlayerPending>>,
    intentional_close: Arc<AtomicBool>,
    admitted_file_generation: Arc<AtomicU64>,
    file_generation_tx: tokio::sync::watch::Sender<u64>,
}

impl PlayerHandle {
    pub fn send(&self, cmd: PlayerCmd) -> DeliveryResult {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.closed || self.tx.is_closed() {
            pending.mark_closed();
            return Err(DeliveryError::Closed);
        }
        let invalidates_file = cmd.invalidates_file_generation();
        if invalidates_file {
            // Publish the new owner expectation before the command becomes visible to the actor.
            // The owner loop cannot reduce an event until this synchronous admission returns.
            self.admitted_file_generation.fetch_add(1, Ordering::AcqRel);
        }
        if pending.drainer_running || !pending.cmds.is_empty() {
            return match pending.push(cmd) {
                Ok(coalesced) => {
                    drop(pending);
                    self.publish_file_generation(invalidates_file);
                    Ok(receipt_for_pending(coalesced))
                }
                Err(error) => {
                    self.rollback_file_generation(invalidates_file);
                    Err(error)
                }
            };
        }

        match self.tx.try_send(cmd) {
            Ok(()) => {
                drop(pending);
                self.publish_file_generation(invalidates_file);
                Ok(DeliveryReceipt::Enqueued)
            }
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                let handle = match tokio::runtime::Handle::try_current() {
                    Ok(handle) => handle,
                    Err(_) => {
                        self.rollback_file_generation(invalidates_file);
                        return Err(DeliveryError::Busy);
                    }
                };
                let coalesced = match pending.push(cmd) {
                    Ok(coalesced) => coalesced,
                    Err(error) => {
                        self.rollback_file_generation(invalidates_file);
                        return Err(error);
                    }
                };
                pending.drainer_running = true;
                drop(pending);
                handle.spawn(drain_player_queue(
                    self.tx.clone(),
                    Arc::clone(&self.pending),
                ));
                self.publish_file_generation(invalidates_file);
                Ok(receipt_for_pending(coalesced))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.rollback_file_generation(invalidates_file);
                pending.mark_closed();
                Err(DeliveryError::Closed)
            }
        }
    }

    /// Admit a command batch as one ordered unit.
    ///
    /// Batches containing more than one command always enter the semantic backlog while
    /// holding its lock. This prevents a direct-channel prefix from becoming visible before
    /// admission of the remainder is known to succeed. The staged backlog is committed only
    /// when its final, semantically coalesced form fits within [`PLAYER_PENDING_MAX`].
    pub fn send_batch(&self, mut cmds: Vec<PlayerCmd>) -> DeliveryResult {
        match cmds.len() {
            // An empty intent cannot produce player work and must never authorize a reducer
            // commit or remote success response.
            0 => return Err(DeliveryError::Busy),
            1 => return self.send(cmds.pop().expect("batch length was checked")),
            _ => {}
        }

        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.closed || self.tx.is_closed() {
            pending.mark_closed();
            return Err(DeliveryError::Closed);
        }

        let file_invalidations = cmds
            .iter()
            .filter(|cmd| cmd.invalidates_file_generation())
            .count() as u64;
        let staged = pending.stage_batch(cmds)?;
        let needs_drainer = !pending.drainer_running && !staged.cmds.is_empty();
        let runtime = if needs_drainer {
            Some(tokio::runtime::Handle::try_current().map_err(|_| DeliveryError::Busy)?)
        } else {
            None
        };

        if file_invalidations > 0 {
            let generation = self
                .admitted_file_generation
                .fetch_add(file_invalidations, Ordering::AcqRel)
                .wrapping_add(file_invalidations);
            self.file_generation_tx.send_replace(generation);
        }
        pending.cmds = staged.cmds;
        if needs_drainer {
            pending.drainer_running = true;
        }
        drop(pending);

        if let Some(runtime) = runtime {
            runtime.spawn(drain_player_queue(
                self.tx.clone(),
                Arc::clone(&self.pending),
            ));
        }
        Ok(staged.receipt)
    }

    pub fn load(&self, url: impl Into<String>) -> DeliveryResult {
        self.send(PlayerCmd::Load(url.into()))
    }

    pub fn current_file_generation(&self) -> u64 {
        self.admitted_file_generation.load(Ordering::Acquire)
    }

    pub fn event_is_current(&self, event: &PlayerEvent) -> bool {
        event
            .file_generation()
            .is_none_or(|generation| generation == self.current_file_generation())
    }

    fn rollback_file_generation(&self, invalidates_file: bool) {
        if invalidates_file {
            self.admitted_file_generation.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn publish_file_generation(&self, invalidates_file: bool) {
        if invalidates_file {
            self.file_generation_tx
                .send_replace(self.current_file_generation());
        }
    }

    #[cfg(test)]
    pub(crate) fn test_handle(tx: Sender<PlayerCmd>) -> Self {
        let (file_generation_tx, _) = tokio::sync::watch::channel(0);
        Self {
            tx,
            pending: Arc::new(Mutex::new(PlayerPending::default())),
            intentional_close: Arc::new(AtomicBool::new(false)),
            admitted_file_generation: Arc::new(AtomicU64::new(0)),
            file_generation_tx,
        }
    }
}

impl Drop for PlayerHandle {
    fn drop(&mut self) {
        // A backlog drainer may still own an internal sender clone. Publish intent before
        // either that clone or the mpv guard disappears so EOF cannot masquerade as a crash.
        self.intentional_close.store(true, Ordering::Release);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .mark_closed();
    }
}

fn receipt_for_pending(coalesced: bool) -> DeliveryReceipt {
    if coalesced {
        DeliveryReceipt::Coalesced {
            replaced_existing: true,
            evicted_oldest: false,
        }
    } else {
        DeliveryReceipt::Deferred
    }
}

const PLAYER_PENDING_MAX: usize = pending::PLAYER_PENDING_MAX;

#[derive(Default)]
struct PlayerPending {
    cmds: VecDeque<PlayerCmd>,
    drainer_running: bool,
    closed: bool,
}

impl PlayerPending {
    fn mark_closed(&mut self) {
        self.cmds.clear();
        self.drainer_running = false;
        self.closed = true;
    }

    fn push(&mut self, cmd: PlayerCmd) -> std::result::Result<bool, DeliveryError> {
        pending::push_pending_command(&mut self.cmds, cmd, PLAYER_PENDING_MAX, DeliveryError::Busy)
    }

    fn stage_batch(
        &self,
        cmds: Vec<PlayerCmd>,
    ) -> std::result::Result<pending::StagedPlayerCommands, DeliveryError> {
        pending::stage_pending_batch(&self.cmds, cmds, PLAYER_PENDING_MAX, DeliveryError::Busy)
    }
}

async fn drain_player_queue(tx: Sender<PlayerCmd>, pending: Arc<Mutex<PlayerPending>>) {
    loop {
        let permit = match tx.reserve().await {
            Ok(permit) => permit,
            Err(_) => {
                let mut pending = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.mark_closed();
                return;
            }
        };
        let cmd = {
            let mut pending = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match pending.cmds.pop_front() {
                Some(cmd) => Some(cmd),
                None => {
                    pending.drainer_running = false;
                    None
                }
            }
        };
        match cmd {
            Some(cmd) => permit.send(cmd),
            None => {
                drop(permit);
                return;
            }
        }
    }
}

/// RAII guard owning the mpv child. Dropping it kills and boundedly reaps mpv (with tokio
/// `kill_on_drop` as the final backstop) before removing the IPC socket.
pub struct Mpv {
    /// Owns mpv's full process group / Job Object. It is explicitly terminated before `child`
    /// is reaped so the borrowed Windows process handle remains valid.
    child_tree: ChildTreeGuard,
    child: Child,
    /// The IPC endpoint path. Read only to unlink the Unix socket on drop; Windows named
    /// pipes self-clean, so the field is unused there.
    #[cfg_attr(windows, allow(dead_code))]
    ipc_path: String,
}

impl Drop for Mpv {
    fn drop(&mut self) {
        self.child_tree.terminate();
        lifetime::kill_mpv_now();
        terminate_and_reap(&mut self.child);
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(&self.ipc_path);
        }
    }
}

const MPV_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const MPV_REAP_POLL: std::time::Duration = std::time::Duration::from_millis(5);

fn terminate_and_reap(child: &mut Child) {
    let pid = child.id();
    if let Err(error) = child.start_kill() {
        tracing::debug!(?pid, %error, "mpv was already unavailable during teardown");
    }
    let deadline = std::time::Instant::now() + MPV_REAP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(MPV_REAP_POLL);
            }
            Ok(None) => {
                tracing::warn!(?pid, "mpv did not exit before the bounded reap deadline");
                return;
            }
            Err(error) => {
                tracing::warn!(?pid, %error, "failed to reap mpv during teardown");
                return;
            }
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
    audio: crate::config::AudioRuntimeConfig,
) -> Result<(PlayerHandle, Mpv)>
where
    F: Fn(PlayerEvent) + Send + Sync + 'static,
{
    let ipc_path = mpv::ipc_path()?;
    let child = mpv::spawn(&ipc_path, cookies_file.as_deref(), gapless, &audio.mpv)?;
    // Arm tree ownership immediately after spawn, before registry work or any cancellable await.
    let child_tree = ChildTreeGuard::for_tokio(&child, ProcessProfile::Media);
    let mpv_pid = child.id().context("mpv exited before reporting a pid")?;

    lifetime::set_mpv_pid(mpv_pid);
    if let Some(dir) = &data_dir {
        lifetime::register(dir, std::process::id(), mpv_pid, &ipc_path);
    }

    // Establish complete ownership before the first cancellable await. IPC connection can fail
    // or the startup task can be aborted while it is retrying; in both cases dropping this guard
    // terminates the process tree, kills/reaps mpv, clears the signal-handler PID, and removes the
    // Unix socket. Keeping `child` and its tree guard as independent locals across the await could
    // otherwise leave a stale global PID when startup is cancelled.
    let mpv = Mpv {
        child_tree,
        child,
        ipc_path: ipc_path.clone(),
    };

    let conn = ipc::connect_retry(&ipc_path)
        .await
        .context("could not connect to the mpv IPC endpoint")?;

    let (tx, rx) =
        crate::util::backpressure::bounded_channel(crate::util::backpressure::PLAYER_CMD_QUEUE);
    let intentional_close = Arc::new(AtomicBool::new(false));
    let admitted_file_generation = Arc::new(AtomicU64::new(0));
    let (file_generation_tx, file_generation_rx) = tokio::sync::watch::channel(0);
    tokio::spawn(ipc::run_actor(
        conn,
        rx,
        Arc::new(emit),
        Arc::clone(&intentional_close),
        file_generation_rx,
    ));

    Ok((
        PlayerHandle {
            tx,
            pending: Arc::new(Mutex::new(PlayerPending::default())),
            intentional_close,
            admitted_file_generation,
            file_generation_tx,
        },
        mpv,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_generation_advances_only_for_admitted_load_and_stop_barriers() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let handle = PlayerHandle::test_handle(tx);
        assert_eq!(handle.current_file_generation(), 0);

        assert!(
            handle
                .send(PlayerCmd::Load("https://example.invalid/a".to_owned()))
                .is_ok()
        );
        assert_eq!(handle.current_file_generation(), 1);
        assert!(handle.event_is_current(&PlayerEvent::file_scoped(
            1,
            PlayerEvent::Duration(Some(10.0)),
        )));
        assert!(!handle.event_is_current(&PlayerEvent::file_scoped(0, PlayerEvent::Eof)));

        assert!(handle.send(PlayerCmd::SetVolume(20)).is_ok());
        assert_eq!(handle.current_file_generation(), 1);
        assert!(handle.send(PlayerCmd::Stop).is_ok());
        assert_eq!(handle.current_file_generation(), 2);

        assert!(matches!(rx.try_recv(), Ok(PlayerCmd::Load(_))));
        assert!(matches!(rx.try_recv(), Ok(PlayerCmd::SetVolume(20))));
        assert!(matches!(rx.try_recv(), Ok(PlayerCmd::Stop)));
    }

    #[test]
    fn rejected_load_rolls_back_the_expected_file_generation() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        assert!(tx.try_send(PlayerCmd::SetVolume(1)).is_ok());
        let handle = PlayerHandle::test_handle(tx);

        assert_eq!(
            handle.send(PlayerCmd::Load("https://example.invalid/a".to_owned())),
            Err(DeliveryError::Busy)
        );
        assert_eq!(handle.current_file_generation(), 0);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn terminate_and_reap_waits_for_the_child_exit() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .kill_on_drop(true)
            .spawn()
            .expect("spawn inert child");

        terminate_and_reap(&mut child);

        assert!(matches!(child.try_wait(), Ok(Some(_))));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn mpv_drop_terminates_media_process_group_descendants() {
        use std::process::Stdio;

        let _pid_guard = lifetime::lock_mpv_pid_for_test().await;
        let root = std::env::temp_dir().join(format!(
            "ytt-mpv-tree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create mpv tree fixture");
        let pid_file = root.join("helper.pid");
        let script = format!("sleep 10 & echo $! > '{}'; wait", pid_file.display());
        let mut command = crate::util::process::tokio_command("sh", ProcessProfile::Media);
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("spawn fake long-lived mpv tree");
        let child_tree = ChildTreeGuard::for_tokio(&child, ProcessProfile::Media);
        let mpv = Mpv {
            child_tree,
            child,
            ipc_path: root.join("mpv.sock").to_string_lossy().into_owned(),
        };

        let helper_pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = contents.trim().parse::<libc::pid_t>()
                {
                    break pid;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("helper pid should be published");
        assert!(crate::util::process::process_exists_for_test(helper_pid));

        drop(mpv);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while crate::util::process::process_exists_for_test(helper_pid) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("mpv helper survived Mpv::drop");
        std::fs::remove_dir_all(root).expect("remove mpv tree fixture");
    }

    #[test]
    fn pending_seek_coalescing_preserves_arrival_semantics() {
        let mut pending = PlayerPending::default();
        assert!(!pending.push(PlayerCmd::SeekRelative(2.0)).unwrap());
        assert!(pending.push(PlayerCmd::SeekRelative(3.5)).unwrap());
        assert!(pending.push(PlayerCmd::SeekAbsolute(12.0)).unwrap());
        assert!(!pending.push(PlayerCmd::SeekRelative(4.0)).unwrap());

        assert!(matches!(
            pending.cmds.pop_front(),
            Some(PlayerCmd::SeekAbsolute(secs)) if (secs - 12.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            pending.cmds.pop_front(),
            Some(PlayerCmd::SeekRelative(secs)) if (secs - 4.0).abs() < f64::EPSILON
        ));
        assert!(pending.cmds.is_empty());
    }

    #[test]
    fn pending_keeps_latest_volume_and_filter_value() {
        let mut pending = PlayerPending::default();
        assert!(!pending.push(PlayerCmd::SetVolume(20)).unwrap());
        assert!(pending.push(PlayerCmd::SetVolume(30)).unwrap());
        assert!(
            !pending
                .push(PlayerCmd::AfCommand {
                    label: "eq".to_owned(),
                    param: "gain".to_owned(),
                    value: "1".to_owned(),
                })
                .unwrap()
        );
        assert!(
            pending
                .push(PlayerCmd::AfCommand {
                    label: "eq".to_owned(),
                    param: "gain".to_owned(),
                    value: "2".to_owned(),
                })
                .unwrap()
        );

        match pending.cmds.pop_front() {
            Some(PlayerCmd::SetVolume(vol)) => assert_eq!(vol, 30),
            _ => panic!("expected latest volume"),
        }
        match pending.cmds.pop_front() {
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
        assert!(pending.cmds.is_empty());
    }

    #[test]
    fn critical_barriers_bound_coalescing_and_toggle_pairs_cancel() {
        let mut pending = PlayerPending::default();
        assert!(!pending.push(PlayerCmd::SetVolume(20)).unwrap());
        assert!(!pending.push(PlayerCmd::Load("old".to_owned())).unwrap());
        assert!(!pending.push(PlayerCmd::Load("new".to_owned())).unwrap());
        assert!(!pending.push(PlayerCmd::SetVolume(30)).unwrap());
        assert!(!pending.push(PlayerCmd::CyclePause).unwrap());
        assert!(pending.push(PlayerCmd::CyclePause).unwrap());

        assert!(matches!(
            pending.cmds.pop_front(),
            Some(PlayerCmd::SetVolume(20))
        ));
        assert!(matches!(
            pending.cmds.pop_front(),
            Some(PlayerCmd::Load(url)) if url == "old"
        ));
        assert!(matches!(
            pending.cmds.pop_front(),
            Some(PlayerCmd::Load(url)) if url == "new"
        ));
        assert!(matches!(
            pending.cmds.pop_front(),
            Some(PlayerCmd::SetVolume(30))
        ));
        assert!(pending.cmds.is_empty());
    }

    #[tokio::test]
    async fn full_player_lane_defers_control_with_one_ordered_drainer() {
        let (tx, mut rx) = mpsc::channel(1);
        assert!(tx.try_send(PlayerCmd::SetVolume(1)).is_ok());
        let handle = PlayerHandle::test_handle(tx);

        assert_eq!(handle.send(PlayerCmd::Stop), Ok(DeliveryReceipt::Deferred));

        assert!(matches!(rx.recv().await, Some(PlayerCmd::SetVolume(1))));
        assert!(matches!(rx.recv().await, Some(PlayerCmd::Stop)));
    }

    #[tokio::test]
    async fn full_player_lane_coalesces_latest_value_without_reordering_control() {
        let (tx, mut rx) = mpsc::channel(1);
        assert!(tx.try_send(PlayerCmd::Stop).is_ok());
        let handle = PlayerHandle::test_handle(tx);

        assert_eq!(
            handle.send(PlayerCmd::SetVolume(40)),
            Ok(DeliveryReceipt::Deferred)
        );
        assert!(matches!(
            handle.send(PlayerCmd::SetVolume(80)),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                ..
            })
        ));

        assert!(matches!(rx.recv().await, Some(PlayerCmd::Stop)));
        assert!(matches!(rx.recv().await, Some(PlayerCmd::SetVolume(80))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn command_batch_enters_backlog_atomically_and_preserves_order() {
        let (tx, mut rx) = mpsc::channel(4);
        let handle = PlayerHandle::test_handle(tx);

        assert_eq!(
            handle.send_batch(vec![
                PlayerCmd::Stop,
                PlayerCmd::SetVolume(42),
                PlayerCmd::SeekAbsolute(19.0),
            ]),
            Ok(DeliveryReceipt::Deferred)
        );

        // A multi-command batch never exposes a direct-channel prefix. The spawned drainer
        // cannot run on this current-thread runtime until this task yields.
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        {
            let pending = handle
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(pending.cmds.len(), 3);
            assert!(pending.drainer_running);
        }

        assert!(matches!(rx.recv().await, Some(PlayerCmd::Stop)));
        assert!(matches!(rx.recv().await, Some(PlayerCmd::SetVolume(42))));
        assert!(matches!(
            rx.recv().await,
            Some(PlayerCmd::SeekAbsolute(secs)) if (secs - 19.0).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn saturated_batch_does_not_commit_a_coalesced_prefix() {
        let (tx, _rx) = mpsc::channel(1);
        let handle = PlayerHandle::test_handle(tx);
        {
            let mut pending = handle
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for _ in 0..PLAYER_PENDING_MAX - 1 {
                assert!(!pending.push(PlayerCmd::Stop).unwrap());
            }
            assert!(!pending.push(PlayerCmd::Load("old".to_owned())).unwrap());
            // Keep admission on the backlog-only path without needing a Tokio runtime.
            pending.drainer_running = true;
        }

        // Neither command fits. Atomic staging must preserve the old Load instead of
        // publishing a partial recovery transaction.
        assert_eq!(
            handle.send_batch(vec![PlayerCmd::Load("new".to_owned()), PlayerCmd::Stop]),
            Err(DeliveryError::Busy)
        );

        let pending = handle
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(pending.cmds.len(), PLAYER_PENDING_MAX);
        assert!(matches!(
            pending.cmds.back(),
            Some(PlayerCmd::Load(url)) if url == "old"
        ));
    }

    #[test]
    fn batch_coalescing_respects_barriers_and_never_revokes_accepted_loads() {
        let mut pending = PlayerPending::default();
        let staged = pending
            .stage_batch(vec![
                PlayerCmd::SetVolume(10),
                PlayerCmd::Load("track".to_owned()),
                PlayerCmd::SetVolume(20),
                PlayerCmd::SetVolume(30),
            ])
            .unwrap();
        pending.cmds = staged.cmds;

        assert!(matches!(
            pending.cmds.pop_front(),
            Some(PlayerCmd::SetVolume(10))
        ));
        assert!(matches!(
            pending.cmds.pop_front(),
            Some(PlayerCmd::Load(url)) if url == "track"
        ));
        assert!(matches!(
            pending.cmds.pop_front(),
            Some(PlayerCmd::SetVolume(30))
        ));
        assert!(pending.cmds.is_empty());

        for _ in 0..PLAYER_PENDING_MAX - 1 {
            assert!(!pending.push(PlayerCmd::Stop).unwrap());
        }
        assert!(!pending.push(PlayerCmd::Load("old".to_owned())).unwrap());
        assert!(matches!(
            pending.stage_batch(vec![
                PlayerCmd::Load("new".to_owned()),
                PlayerCmd::Load("newest".to_owned()),
            ]),
            Err(DeliveryError::Busy)
        ));
        assert_eq!(pending.cmds.len(), PLAYER_PENDING_MAX);
        assert!(matches!(
            pending.cmds.back(),
            Some(PlayerCmd::Load(url)) if url == "old"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn separate_track_loads_stay_distinct_behind_a_full_player_lane() {
        let (tx, mut rx) = mpsc::channel(1);
        assert!(tx.try_send(PlayerCmd::Stop).is_ok());
        let handle = PlayerHandle::test_handle(tx);

        assert_eq!(
            handle.send(PlayerCmd::Load("track-b".to_owned())),
            Ok(DeliveryReceipt::Deferred)
        );
        assert_eq!(
            handle.send(PlayerCmd::Load("track-c".to_owned())),
            Ok(DeliveryReceipt::Deferred)
        );
        {
            let pending = handle
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(pending.cmds.len(), 2);
            assert!(matches!(
                pending.cmds.front(),
                Some(PlayerCmd::Load(url)) if url == "track-b"
            ));
            assert!(matches!(
                pending.cmds.back(),
                Some(PlayerCmd::Load(url)) if url == "track-c"
            ));
        }

        assert!(matches!(rx.recv().await, Some(PlayerCmd::Stop)));
        assert!(matches!(
            rx.recv().await,
            Some(PlayerCmd::Load(url)) if url == "track-b"
        ));
        assert!(matches!(
            rx.recv().await,
            Some(PlayerCmd::Load(url)) if url == "track-c"
        ));
    }

    #[test]
    fn closed_player_lane_rejects_the_whole_batch() {
        let (tx, rx) = mpsc::channel(2);
        drop(rx);
        let handle = PlayerHandle::test_handle(tx);

        assert_eq!(
            handle.send_batch(vec![PlayerCmd::Stop, PlayerCmd::SetVolume(20)]),
            Err(DeliveryError::Closed)
        );
        let pending = handle
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(pending.closed);
        assert!(pending.cmds.is_empty());
    }

    #[test]
    fn empty_player_batch_is_never_admitted() {
        let (open_tx, _open_rx) = mpsc::channel(1);
        let open_handle = PlayerHandle::test_handle(open_tx);
        assert_eq!(open_handle.send_batch(Vec::new()), Err(DeliveryError::Busy));

        let (closed_tx, closed_rx) = mpsc::channel(1);
        drop(closed_rx);
        let closed_handle = PlayerHandle::test_handle(closed_tx);
        assert_eq!(
            closed_handle.send_batch(Vec::new()),
            Err(DeliveryError::Busy)
        );
    }

    #[test]
    fn closed_player_lane_reports_closed() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let handle = PlayerHandle::test_handle(tx);

        assert_eq!(handle.send(PlayerCmd::Stop), Err(DeliveryError::Closed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn active_player_drainer_does_not_admit_after_lane_closes() {
        let (tx, rx) = mpsc::channel(1);
        assert!(tx.try_send(PlayerCmd::Stop).is_ok());
        let handle = PlayerHandle::test_handle(tx);

        assert_eq!(
            handle.send(PlayerCmd::SetVolume(40)),
            Ok(DeliveryReceipt::Deferred)
        );
        // Keep the drainer unpolled until after the receiver closes, then make
        // admission synchronously observe the sender's closed state.
        drop(rx);
        assert_eq!(
            handle.send(PlayerCmd::SetVolume(80)),
            Err(DeliveryError::Closed)
        );
        assert_eq!(handle.send(PlayerCmd::Stop), Err(DeliveryError::Closed));

        let pending = handle
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(pending.closed);
        assert!(!pending.drainer_running);
        assert!(pending.cmds.is_empty());
    }

    #[test]
    fn full_player_lane_outside_runtime_reports_busy() {
        let (tx, _rx) = mpsc::channel(1);
        assert!(tx.try_send(PlayerCmd::Stop).is_ok());
        let handle = PlayerHandle::test_handle(tx);

        assert_eq!(handle.send(PlayerCmd::CyclePause), Err(DeliveryError::Busy));
    }

    #[tokio::test]
    async fn player_pending_backlog_is_bounded() {
        let (tx, _rx) = mpsc::channel(1);
        assert!(tx.try_send(PlayerCmd::Stop).is_ok());
        let handle = PlayerHandle::test_handle(tx);

        for _ in 0..PLAYER_PENDING_MAX {
            assert!(handle.send(PlayerCmd::Stop).is_ok());
        }
        assert_eq!(handle.send(PlayerCmd::Stop), Err(DeliveryError::Busy));
    }
}
