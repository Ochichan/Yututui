//! Playback subsystem: an mpv child process driven over JSON IPC.
//!
//! [`spawn`] starts mpv, connects the IPC actor, and returns a cheap [`PlayerHandle`]
//! (clone-free command sender) plus an [`Mpv`] lifetime guard. The guard MUST stay in
//! scope for the whole session: dropping it kills mpv. See [`lifetime`] for the full
//! no-orphan story.

pub mod backend;
pub(crate) mod cache_budget;
pub(crate) mod cache_runtime;
pub mod cache_support;
pub(crate) mod diagnostics;
pub mod ipc;
pub mod lifetime;
pub mod long_form_seek;
pub mod mpv;
pub(crate) mod pending;
pub mod proto;
pub mod recovery;
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

/// Accuracy/latency contract for an absolute seek.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekPrecision {
    /// User-facing scrub/jump: land at the nearest keyframe for low restart latency.
    InteractiveFast,
    /// Restoration/recovery: preserve the requested position exactly.
    Exact,
}

/// Owner-known source semantics which mpv cannot reliably infer from duration/seekability.
///
/// This value travels with exactly one admitted load. In particular, a finite seekable radio or
/// DVR stream is still `Live` and must never become eligible for the managed disk cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaSourceContext {
    OnDemand,
    Live,
}

impl MediaSourceContext {
    pub const fn from_live(live: bool) -> Self {
        if live { Self::Live } else { Self::OnDemand }
    }

    pub const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }
}

/// One playback destination and its non-inferable source semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackLoad {
    url: String,
    source_context: MediaSourceContext,
}

impl PlaybackLoad {
    pub fn new(url: impl Into<String>, source_context: MediaSourceContext) -> Self {
        Self {
            url: url.into(),
            source_context,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn as_str(&self) -> &str {
        self.url()
    }

    pub const fn source_context(&self) -> MediaSourceContext {
        self.source_context
    }
}

impl std::ops::Deref for PlaybackLoad {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.url()
    }
}

impl PartialEq<str> for PlaybackLoad {
    fn eq(&self, other: &str) -> bool {
        self.url == other
    }
}

impl PartialEq<&str> for PlaybackLoad {
    fn eq(&self, other: &&str) -> bool {
        self.url == *other
    }
}

/// Commands the reducer sends to the player actor.
#[derive(Clone)]
pub enum PlayerCmd {
    /// Resolve nothing — load this (already-playable) URL and start it.
    Load(PlaybackLoad),
    /// Replace the current source and restore position/pause only after its `file-loaded` event.
    LoadWithResume(recovery::LoadWithResume),
    /// Stop the current mpv file without advancing mpv's own playlist.
    Stop,
    /// Toggle pause/resume.
    CyclePause,
    /// Seek by a relative number of seconds (negative = backward).
    SeekRelative(f64),
    /// Seek to an absolute position in seconds with an explicit latency/accuracy contract.
    SeekAbsolute {
        seconds: f64,
        precision: SeekPrecision,
    },
    /// Update the live managed current-media cache policy.
    SetLongFormSeekOptimization(crate::config::LongFormSeekOptimization),
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
    pub fn load(url: impl Into<String>, source_context: MediaSourceContext) -> Self {
        Self::Load(PlaybackLoad::new(url, source_context))
    }

    pub fn interactive_seek(seconds: f64) -> Self {
        Self::SeekAbsolute {
            seconds,
            precision: SeekPrecision::InteractiveFast,
        }
    }

    pub fn exact_seek(seconds: f64) -> Self {
        Self::SeekAbsolute {
            seconds,
            precision: SeekPrecision::Exact,
        }
    }

    pub(crate) fn is_interactive_seek(&self) -> bool {
        matches!(
            self,
            Self::SeekAbsolute {
                precision: SeekPrecision::InteractiveFast,
                ..
            }
        )
    }

    fn is_coalescing_barrier(&self) -> bool {
        matches!(
            self,
            Self::SeekAbsolute {
                precision: SeekPrecision::Exact,
                ..
            } | Self::Load(_)
                | Self::LoadWithResume(_)
                | Self::Stop
                | Self::CyclePause
                | Self::SetLongFormSeekOptimization(_)
                | Self::SetAudioFilter(_)
                | Self::SetProperty { .. }
                | Self::TrackedProperty(_)
        )
    }

    fn invalidates_file_generation(&self) -> bool {
        matches!(self, Self::Load(_) | Self::LoadWithResume(_) | Self::Stop)
    }

    fn admitted_media_expected(&self) -> Option<bool> {
        match self {
            Self::Load(_) | Self::LoadWithResume(_) => Some(true),
            Self::Stop => Some(false),
            _ => None,
        }
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
    /// Managed packet-cache disable could not be proven. The owner must retire this mpv and
    /// resume the same media once, at the captured position/pause state, forced to RAM-only.
    CacheEmergency {
        /// Origin generation only; unlike `FileScoped`, cache safety terminals are process-wide
        /// and must reach the owner even after a newer Load/Stop admission.
        file_generation: u64,
        position_secs: f64,
        paused: bool,
        reason: long_form_seek::CacheReason,
    },
    /// Process-global cache reset failed while replacing media. This terminal is deliberately
    /// unscoped: the owner must replay its already-admitted destination RAM-only, never attach
    /// the previous file's captured position to the new item.
    CacheReplacementEmergency {
        reason: long_form_seek::CacheReason,
    },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LongFormSeekRuntimeStatus {
    pub status: long_form_seek::CacheStatus,
    pub last_failure: Option<long_form_seek::CacheReason>,
    pub last_cleanup_ms: Option<u64>,
}

pub(crate) struct SharedLongFormSeekStatus {
    runtime: LongFormSeekRuntimeStatus,
    history: Arc<Mutex<LongFormSeekHistory>>,
}

#[derive(Default)]
struct LongFormSeekHistory {
    last_failure: Option<long_form_seek::CacheReason>,
    last_cleanup_ms: Option<u64>,
    cleanup_started: Option<(u64, std::time::Instant)>,
}

impl SharedLongFormSeekStatus {
    #[cfg(test)]
    fn new(status: long_form_seek::CacheStatus) -> Self {
        Self::with_history(status, Arc::new(Mutex::new(LongFormSeekHistory::default())))
    }

    fn for_owner_process(status: long_form_seek::CacheStatus) -> Self {
        static HISTORY: std::sync::OnceLock<Arc<Mutex<LongFormSeekHistory>>> =
            std::sync::OnceLock::new();
        Self::with_history(
            status,
            Arc::clone(
                HISTORY.get_or_init(|| Arc::new(Mutex::new(LongFormSeekHistory::default()))),
            ),
        )
    }

    fn with_history(
        status: long_form_seek::CacheStatus,
        history: Arc<Mutex<LongFormSeekHistory>>,
    ) -> Self {
        let (last_failure, last_cleanup_ms) = {
            let history = history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (history.last_failure, history.last_cleanup_ms)
        };
        Self {
            runtime: LongFormSeekRuntimeStatus {
                status,
                last_failure,
                last_cleanup_ms,
            },
            history,
        }
    }

    fn update(&mut self, status: long_form_seek::CacheStatus) {
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if runtime_cache_failure(status.reason) {
            history.last_failure = Some(status.reason);
        }
        if status.effective == long_form_seek::CacheEffectiveState::DisablePending
            && status.reason == long_form_seek::CacheReason::MediaClosed
            && let Some(generation) = status.file_generation
        {
            history
                .cleanup_started
                .get_or_insert((generation, std::time::Instant::now()));
        }
        if let Some((generation, started)) = history.cleanup_started
            && status.file_generation != Some(generation)
        {
            history.last_cleanup_ms =
                Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
            history.cleanup_started = None;
        }
        self.runtime = LongFormSeekRuntimeStatus {
            status,
            last_failure: history.last_failure,
            last_cleanup_ms: history.last_cleanup_ms,
        };
    }
}

fn runtime_cache_failure(reason: long_form_seek::CacheReason) -> bool {
    matches!(
        reason,
        long_form_seek::CacheReason::CacheRootUnavailable
            | long_form_seek::CacheReason::InsufficientFreeSpace
            | long_form_seek::CacheReason::UnsafeRateBound
            | long_form_seek::CacheReason::InvalidRangeState
            | long_form_seek::CacheReason::ProbeFailed
            | long_form_seek::CacheReason::WriteBudgetExhausted
            | long_form_seek::CacheReason::PropertyRejected
            | long_form_seek::CacheReason::PropertyTimeout
            | long_form_seek::CacheReason::PropertyVerificationFailed
            | long_form_seek::CacheReason::DisableFailed
    )
}

/// A handle for sending [`PlayerCmd`]s to the player actor. Commands enter one ordered
/// bounded lane; when that lane is full, a single drainer owns a bounded semantic backlog.
pub struct PlayerHandle {
    tx: Sender<PlayerCmd>,
    pending: Arc<Mutex<PlayerPending>>,
    intentional_close: Arc<AtomicBool>,
    admitted_file_generation: Arc<AtomicU64>,
    /// Generation of the latest admitted Load, or zero when the latest boundary is Stop/no-media.
    expected_media_generation: Arc<AtomicU64>,
    file_generation_tx: tokio::sync::watch::Sender<u64>,
    long_form_seek_status: Arc<Mutex<SharedLongFormSeekStatus>>,
}

#[derive(Clone, Copy)]
struct FileAdmission {
    previous_generation: u64,
    previous_expected_media_generation: u64,
    generation: u64,
}

impl PlayerHandle {
    pub fn send(&self, cmd: PlayerCmd) -> DeliveryResult {
        let interactive = cmd.is_interactive_seek();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.closed || self.tx.is_closed() {
            pending.mark_closed();
            return Err(DeliveryError::Closed);
        }
        let admission =
            self.begin_file_admission(cmd.admitted_media_expected().map(|value| (1, value)));
        if pending.drainer_running || !pending.cmds.is_empty() {
            return match pending.push(cmd) {
                Ok(coalesced) => {
                    drop(pending);
                    self.publish_file_generation(admission);
                    if interactive {
                        diagnostics::interactive_admitted(1, u64::from(coalesced));
                    }
                    Ok(receipt_for_pending(coalesced))
                }
                Err(error) => {
                    self.rollback_file_generation(admission);
                    Err(error)
                }
            };
        }

        match self.tx.try_send(cmd) {
            Ok(()) => {
                drop(pending);
                self.publish_file_generation(admission);
                if interactive {
                    diagnostics::interactive_admitted(1, 0);
                }
                Ok(DeliveryReceipt::Enqueued)
            }
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                let handle = match tokio::runtime::Handle::try_current() {
                    Ok(handle) => handle,
                    Err(_) => {
                        self.rollback_file_generation(admission);
                        return Err(DeliveryError::Busy);
                    }
                };
                let coalesced = match pending.push(cmd) {
                    Ok(coalesced) => coalesced,
                    Err(error) => {
                        self.rollback_file_generation(admission);
                        return Err(error);
                    }
                };
                pending.drainer_running = true;
                drop(pending);
                handle.spawn(drain_player_queue(
                    self.tx.clone(),
                    Arc::clone(&self.pending),
                ));
                self.publish_file_generation(admission);
                if interactive {
                    diagnostics::interactive_admitted(1, u64::from(coalesced));
                }
                Ok(receipt_for_pending(coalesced))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.rollback_file_generation(admission);
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

        let interactive_count = cmds.iter().filter(|cmd| cmd.is_interactive_seek()).count() as u64;

        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.closed || self.tx.is_closed() {
            pending.mark_closed();
            return Err(DeliveryError::Closed);
        }

        let admission_shape = cmds.iter().fold(None, |shape, cmd| {
            cmd.admitted_media_expected()
                .map_or(shape, |expects_media| {
                    let count = shape.map_or(1, |(count, _)| count + 1);
                    Some((count, expects_media))
                })
        });
        let staged = pending.stage_batch(cmds)?;
        let needs_drainer = !pending.drainer_running && !staged.cmds.is_empty();
        let runtime = if needs_drainer {
            Some(tokio::runtime::Handle::try_current().map_err(|_| DeliveryError::Busy)?)
        } else {
            None
        };

        let admission = self.begin_file_admission(admission_shape);
        self.publish_file_generation(admission);
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
        diagnostics::interactive_admitted(
            interactive_count,
            u64::from(matches!(staged.receipt, DeliveryReceipt::Coalesced { .. })),
        );
        Ok(staged.receipt)
    }

    pub fn load(
        &self,
        url: impl Into<String>,
        source_context: MediaSourceContext,
    ) -> DeliveryResult {
        self.send(PlayerCmd::load(url, source_context))
    }

    pub fn current_file_generation(&self) -> u64 {
        self.admitted_file_generation.load(Ordering::Acquire)
    }

    /// Cache-policy view for the latest admitted media. While the actor still owns an older
    /// physical generation, project a fact-free probing state without mutating the raw runtime
    /// diagnostics or associating the old media's disk activity with the new owner generation.
    pub fn long_form_seek_status(&self) -> long_form_seek::CacheStatus {
        let mut status = self
            .long_form_seek_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime
            .status;
        let admitted = self.current_file_generation();
        let expects_media =
            self.expected_media_generation.load(Ordering::Acquire) == admitted && admitted != 0;
        if expects_media && status.file_generation != Some(admitted) {
            status.effective = long_form_seek::CacheEffectiveState::Probing;
            status.reason = long_form_seek::CacheReason::AwaitingMediaFacts;
            status.file_generation = Some(admitted);
            status.file_cache_bytes = 0;
            status.peak_file_cache_bytes = 0;
        } else if !expects_media && status.file_generation.is_some() {
            status.effective = long_form_seek::CacheEffectiveState::NoMedia;
            status.reason = long_form_seek::CacheReason::NoMedia;
            status.file_generation = None;
            status.file_cache_bytes = 0;
            status.peak_file_cache_bytes = 0;
        }
        status
    }

    /// Raw physical-player cache diagnostics, including cleanup work for an older generation.
    pub(crate) fn long_form_seek_runtime_status(&self) -> LongFormSeekRuntimeStatus {
        self.long_form_seek_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .runtime
    }

    pub fn event_is_current(&self, event: &PlayerEvent) -> bool {
        event
            .file_generation()
            .is_none_or(|generation| generation == self.current_file_generation())
    }

    fn begin_file_admission(&self, shape: Option<(u64, bool)>) -> Option<FileAdmission> {
        let (count, expects_media) = shape?;
        let previous_generation = self.admitted_file_generation.load(Ordering::Acquire);
        let previous_expected_media_generation =
            self.expected_media_generation.load(Ordering::Acquire);
        let generation = previous_generation.wrapping_add(count);
        self.expected_media_generation.store(
            if expects_media { generation } else { 0 },
            Ordering::Release,
        );
        self.admitted_file_generation
            .store(generation, Ordering::Release);
        Some(FileAdmission {
            previous_generation,
            previous_expected_media_generation,
            generation,
        })
    }

    fn rollback_file_generation(&self, admission: Option<FileAdmission>) {
        if let Some(admission) = admission {
            self.admitted_file_generation
                .store(admission.previous_generation, Ordering::Release);
            self.expected_media_generation.store(
                admission.previous_expected_media_generation,
                Ordering::Release,
            );
        }
    }

    fn publish_file_generation(&self, admission: Option<FileAdmission>) {
        if let Some(admission) = admission {
            self.file_generation_tx.send_replace(admission.generation);
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
            expected_media_generation: Arc::new(AtomicU64::new(0)),
            file_generation_tx,
            long_form_seek_status: Arc::new(Mutex::new(SharedLongFormSeekStatus::new(
                long_form_seek::CacheStatus {
                    requested: crate::config::LongFormSeekOptimization::Off,
                    effective: long_form_seek::CacheEffectiveState::NoMedia,
                    reason: long_form_seek::CacheReason::NoMedia,
                    file_generation: None,
                    policy_revision: 0,
                    file_cache_bytes: 0,
                    peak_file_cache_bytes: 0,
                },
            ))),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_set_long_form_seek_runtime(
        &self,
        status: long_form_seek::CacheStatus,
        last_failure: Option<long_form_seek::CacheReason>,
        last_cleanup_ms: Option<u64>,
    ) {
        let mut shared = self
            .long_form_seek_status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.runtime = LongFormSeekRuntimeStatus {
            status,
            last_failure,
            last_cleanup_ms,
        };
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
    // `data_dir` is supplied only to mutation-owning player processes. Derive cache ownership
    // from that same lease so a secondary/read-only instance cannot create packet-cache state.
    let writable_cache_root = data_dir.as_ref().and_then(|_| crate::paths::cache_dir());
    let environment_args = std::env::var("YTM_MPV_EXTRA").ok();
    let cache_support = cache_support::prepare_cache_support(
        &audio.mpv,
        writable_cache_root.as_deref(),
        environment_args.as_deref(),
        mpv::flag_supported,
    );
    let child = mpv::spawn(
        &ipc_path,
        cookies_file.as_deref(),
        gapless,
        &audio.mpv,
        &cache_support.spawn_args,
    )?;
    let cache_runtime = cache_runtime::CacheRuntime::for_owner_process(
        cache_support,
        audio.mpv.long_form_seek_optimization,
    );
    let long_form_seek_status = Arc::new(Mutex::new(SharedLongFormSeekStatus::for_owner_process(
        cache_runtime.status(),
    )));
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
    let expected_media_generation = Arc::new(AtomicU64::new(0));
    let (file_generation_tx, file_generation_rx) = tokio::sync::watch::channel(0);
    tokio::spawn(ipc::run_actor(
        conn,
        rx,
        Arc::new(emit),
        Arc::clone(&intentional_close),
        file_generation_rx,
        cache_runtime,
        Arc::clone(&long_form_seek_status),
    ));

    Ok((
        PlayerHandle {
            tx,
            pending: Arc::new(Mutex::new(PlayerPending::default())),
            intentional_close,
            admitted_file_generation,
            expected_media_generation,
            file_generation_tx,
            long_form_seek_status,
        },
        mpv,
    ))
}

#[cfg(test)]
mod tests;
