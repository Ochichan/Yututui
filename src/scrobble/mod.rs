//! Scrobbling: Last.fm + ListenBrainz.
//!
//! A pure snapshot-diff state machine ([`monitor`]) decides *when* a listen counts, a
//! durable JSONL queue ([`queue`]) makes scrobbles crash-safe, and a network actor
//! ([`actor`]) owns the clients ([`lastfm`], [`listenbrainz`]) behind the [`service`]
//! trait. Both run loops (TUI and daemon) feed the actor the same
//! [`crate::media::MediaSnapshot`] they already publish to the OS media session — the one
//! place the two playback owners converge — via [`ScrobbleHandle::observe`], which
//! derives an [`Observation`] and rate-gates the channel traffic.

pub mod actor;
pub mod auth_cli;
pub mod lastfm;
mod lifetime;
pub mod listenbrainz;
pub mod monitor;
pub mod queue;
pub mod service;

#[cfg(test)]
mod terminal_delivery_tests;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub use actor::{ScrobbleCmd, ScrobbleEvent, spawn};
pub use monitor::{Observation, ObservedTrack};

use tokio::sync::{Notify, mpsc::Sender};

use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};

/// Runtime snapshot the actor works from, resolved by
/// [`crate::config::Config::scrobble_settings`] (embedded credentials + config overrides
/// + enabled gates already applied).
// No `Debug`: carries the app secret, session key, and token.
#[derive(Clone)]
pub struct ScrobbleSettings {
    /// Application credentials (embedded or config override). `None` → Last.fm wholly
    /// unavailable, including the connect flow.
    pub lastfm_app: Option<LastfmApp>,
    /// The connected, enabled Last.fm session. `None` = disconnected or switched off.
    pub lastfm: Option<LastfmSession>,
    pub listenbrainz: Option<ListenBrainzSession>,
    /// Scrobble local files too (when they carry title + artist metadata).
    pub local_files: bool,
}

impl ScrobbleSettings {
    /// Whether any service would receive scrobbles — when false the actor idles.
    pub fn any_active(&self) -> bool {
        self.lastfm.is_some() || self.listenbrainz.is_some()
    }
}

// No `Debug`: `api_secret` is the app secret.
#[derive(Clone)]
pub struct LastfmApp {
    pub api_key: String,
    pub api_secret: String,
}

// No `Debug`: `session_key` is a secret.
#[derive(Clone)]
pub struct LastfmSession {
    pub session_key: String,
    /// Mirror in-app like/unlike to `track.love`/`track.unlove`.
    pub love_sync: bool,
}

// No `Debug`: `token` is a secret.
#[derive(Clone)]
pub struct ListenBrainzSession {
    pub token: String,
    /// Base API URL (self-hosted friendly); default [`listenbrainz::DEFAULT_API_URL`].
    pub api_url: String,
}

impl Observation {
    /// Derive an observation from the snapshot both run loops already build. Injects the
    /// clocks here so the monitor stays deterministic under test.
    pub fn from_media(snapshot: &crate::media::MediaSnapshot) -> Self {
        use crate::media::MediaPlaybackStatus;
        let track = snapshot.track.as_ref().map(|t| {
            let is_local = t.key.starts_with("local:");
            ObservedTrack {
                key: t.key.clone(),
                title: t.title.clone(),
                // `Song::local_file` fills a "Local file" placeholder for untagged files;
                // that is display text, not an artist — scrobbling treats it as absent.
                artist: if is_local && t.artist == "Local file" {
                    String::new()
                } else {
                    t.artist.clone()
                },
                album: t.album.clone(),
                duration: t.duration,
                is_live: t.is_live,
                is_local,
                origin_url: t.url.clone(),
                liked: t.liked,
            }
        });
        Self {
            playing: snapshot.status == MediaPlaybackStatus::Playing,
            stopped: snapshot.status == MediaPlaybackStatus::Stopped,
            position: snapshot.position_now(),
            position_epoch: snapshot.position_epoch,
            rate: snapshot.rate,
            at: Instant::now(),
            wall_unix: crate::signals::unix_now(),
            track,
        }
    }
}

/// Bounded delivery envelope for observations. Compatible heartbeats retain the first and latest
/// snapshots plus cumulative track-time evidence, avoiding both unbounded buffering and lost
/// scrobble-threshold credit while the actor inbox is saturated.
#[derive(Clone)]
pub struct ObservationBatch {
    first: Observation,
    latest: Option<Observation>,
    preserved_credit: f64,
}

impl ObservationBatch {
    pub(crate) fn single(observation: Observation) -> Self {
        Self {
            first: observation,
            latest: None,
            preserved_credit: 0.0,
        }
    }

    fn merge(&mut self, incoming: Self) {
        debug_assert!(self.can_merge(&incoming));
        let previous = self.latest.as_ref().unwrap_or(&self.first);
        let bridge_credit = previous.credit_until(&incoming.first);
        self.preserved_credit = (self.preserved_credit + bridge_credit + incoming.preserved_credit)
            .min(monitor::MAX_PRESERVED_CREDIT);
        self.latest = Some(incoming.latest.unwrap_or(incoming.first));
    }

    fn can_merge(&self, incoming: &Self) -> bool {
        observations_are_merge_compatible(
            self.latest.as_ref().unwrap_or(&self.first),
            &incoming.first,
        )
    }

    fn ends_terminal(&self) -> bool {
        !self.latest.as_ref().unwrap_or(&self.first).playing
    }

    pub(crate) fn into_parts(self) -> (Observation, Option<(Observation, f64)>) {
        let tail = self.latest.map(|latest| (latest, self.preserved_credit));
        (self.first, tail)
    }

    #[cfg(test)]
    fn latest(&self) -> &Observation {
        self.latest.as_ref().unwrap_or(&self.first)
    }
}

fn observations_are_merge_compatible(previous: &Observation, next: &Observation) -> bool {
    previous.playing == next.playing
        && previous.stopped == next.stopped
        && previous.position_epoch == next.position_epoch
        && match (&previous.track, &next.track) {
            (None, None) => true,
            (Some(previous), Some(next)) => previous == next,
            _ => false,
        }
}

/// The run loops' handle: derives + rate-gates observations, forwards commands.
///
/// The TUI publishes a snapshot after *every* reducer message (scrolling included), so
/// `observe` only forwards when the scrobble-relevant fingerprint changed or a ~1s
/// heartbeat is due while playing — the actor sees ~1 Hz, the reducer path stays free of
/// channel traffic.
pub struct ScrobbleHandle {
    tx: Sender<ScrobbleCmd>,
    shutdown_tx: Sender<ShutdownRequest>,
    pending: Arc<PendingCommands>,
    /// The actor owns durable append/fsync work on an isolated OS thread. Retaining its join
    /// handle is the process-lifetime half of the admission contract: a caller may stop waiting
    /// for a receipt, but normal teardown cannot detach accepted work and then exit underneath it.
    actor_thread: Option<std::thread::JoinHandle<()>>,
    last_fingerprint: Option<Fingerprint>,
    last_sent: Option<Instant>,
    last_rejection_log: Option<Instant>,
    retry_needed: bool,
}

pub(crate) struct ShutdownRequest {
    done: tokio::sync::oneshot::Sender<Result<(), DeliveryError>>,
}

const PENDING_COMMAND_CAPACITY: usize = 16;
const PENDING_BARRIER_CAPACITY: usize = 64;
const PENDING_TERMINAL_CAPACITY: usize = PENDING_COMMAND_CAPACITY;

#[derive(Default)]
struct PendingCommands {
    state: Mutex<PendingState>,
    drained: Notify,
}

#[derive(Default)]
struct PendingState {
    commands: VecDeque<PendingCommand>,
    /// One observation-only tail slot. Configuration is ordered after it, so this reserve never
    /// crosses a causal barrier or evicts accepted work.
    observation_reserve: Option<Box<ObservationBatch>>,
    /// Configuration has its own capacity-one latest-value slot. It is ordered after every
    /// observation that was already admitted when the first snapshot occupied this slot, while
    /// newer snapshots replace the pending value. Persisted settings therefore cannot
    /// permanently diverge from the actor merely because its ordinary inbox is saturated.
    configuration: Option<ScrobbleSettings>,
    /// Sealed configuration/observation segments that must stay ahead of the current latest
    /// configuration. Keeping them separate from the pre-configuration queue prevents an
    /// observation reserve from being overtaken while a newer barrier is staged.
    barrier_commands: VecDeque<PendingCommand>,
    /// A barrier command removed from `barrier_commands` while the delivery thread is blocked in
    /// `blocking_send`. Count it against the barrier budget until admission completes so opening
    /// one actor-inbox slot cannot transiently expand the accepted causal chain.
    barrier_command_in_flight: bool,
    /// Ordered, bounded observations after the pending configuration barrier. Only compatible
    /// heartbeats merge; track, epoch, play/pause, stop, and liked edges retain FIFO order so an
    /// acknowledged threshold crossing cannot be overwritten by a later state snapshot.
    post_configuration_observations: VecDeque<Box<ObservationBatch>>,
    /// A bounded paused/stopped FIFO after the ordinary transition budget. Only compatible
    /// observations coalesce: stop, track, liked, and epoch changes stay ordered. Overflow is
    /// `Busy` rather than an acknowledged lossy first/latest summary.
    terminal_observations: VecDeque<Box<ObservationBatch>>,
    /// Latest terminal observation rejected with `Busy`. This is deliberately outside the
    /// admitted FIFO: ordinary retries still receive an honest rejection and may admit a newer
    /// snapshot, while shutdown seals this single bounded tail and applies it only after every
    /// accepted command. Keeping it in actor-shared state makes that final recovery independent
    /// of the caller's paused/stopped retry timer and future lifetime.
    terminal_retry: Option<Box<ObservationBatch>>,
    drainer_running: bool,
    closed: bool,
    shutting_down: bool,
}

enum PendingCommand {
    Observe(Box<ObservationBatch>),
    Reconfigure(ScrobbleSettings),
}

impl PendingCommand {
    fn into_actor_command(self) -> ScrobbleCmd {
        match self {
            Self::Observe(observation) => ScrobbleCmd::Observe(observation),
            Self::Reconfigure(settings) => ScrobbleCmd::Reconfigure(Box::new(settings)),
        }
    }
}

#[derive(PartialEq)]
struct Fingerprint {
    track: Option<ObservedTrack>,
    playing: bool,
    stopped: bool,
    epoch: u64,
    rate_bits: u64,
}

impl ScrobbleHandle {
    #[cfg(test)]
    pub(crate) fn new(tx: Sender<ScrobbleCmd>, shutdown_tx: Sender<ShutdownRequest>) -> Self {
        Self::with_pending(tx, shutdown_tx, Arc::new(PendingCommands::default()), None)
    }

    fn with_pending(
        tx: Sender<ScrobbleCmd>,
        shutdown_tx: Sender<ShutdownRequest>,
        pending: Arc<PendingCommands>,
        actor_thread: Option<std::thread::JoinHandle<()>>,
    ) -> Self {
        Self {
            tx,
            shutdown_tx,
            pending,
            actor_thread,
            last_fingerprint: None,
            last_sent: None,
            last_rejection_log: None,
            retry_needed: false,
        }
    }

    pub fn observe(&mut self, snapshot: &crate::media::MediaSnapshot) -> DeliveryResult {
        let obs = Observation::from_media(snapshot);
        let terminal = !obs.playing;
        let fingerprint = Fingerprint {
            track: obs.track.clone(),
            playing: obs.playing,
            stopped: obs.stopped,
            epoch: obs.position_epoch,
            rate_bits: crate::util::finite_or(obs.rate, 1.0).to_bits(),
        };
        let heartbeat_due = self
            .last_sent
            .is_none_or(|t| t.elapsed().as_secs_f64() >= 1.0);
        if !self.retry_needed
            && self.last_fingerprint.as_ref() == Some(&fingerprint)
            && !(obs.playing && heartbeat_due)
        {
            return Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            });
        }

        let receipt = match self.admit_pending(PendingCommand::Observe(Box::new(
            ObservationBatch::single(obs),
        ))) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.retry_needed = error == DeliveryError::Busy;
                if self
                    .last_rejection_log
                    .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(5))
                {
                    if terminal {
                        tracing::warn!(
                            delivery_outcome = error.reason(),
                            "scrobble observation queue rejected terminal snapshot"
                        );
                    } else {
                        tracing::debug!(
                            delivery_outcome = error.reason(),
                            "scrobble observation queue rejected snapshot"
                        );
                    }
                    self.last_rejection_log = Some(Instant::now());
                }
                return Err(error);
            }
        };
        self.retry_needed = false;
        self.last_fingerprint = Some(fingerprint);
        self.last_sent = Some(Instant::now());
        Ok(receipt)
    }

    pub fn heartbeat_due(&self) -> bool {
        self.last_sent
            .is_none_or(|t| t.elapsed().as_secs_f64() >= 1.0)
    }

    /// A prior `Busy` result must be retried even while playback is paused/stopped and therefore
    /// has no ordinary heartbeat. Owner loops use this to park a low-rate retry timer.
    pub fn retry_needed(&self) -> bool {
        self.retry_needed
    }

    pub fn reconfigure(&self, settings: ScrobbleSettings) -> DeliveryResult {
        self.admit_pending(PendingCommand::Reconfigure(settings))
    }

    /// Kick the Last.fm browser authorization flow (events come back via the sink).
    pub fn auth_start(&self) -> DeliveryResult {
        let mut state = lock_pending(&self.pending.state);
        if state.closed || state.shutting_down || self.tx.is_closed() {
            state.closed |= self.tx.is_closed();
            return Err(DeliveryError::Closed);
        }
        if state.drainer_running
            || !state.commands.is_empty()
            || state.observation_reserve.is_some()
            || !state.barrier_commands.is_empty()
            || state.configuration.is_some()
            || !state.post_configuration_observations.is_empty()
            || !state.terminal_observations.is_empty()
            || state.terminal_retry.is_some()
        {
            return Err(DeliveryError::Busy);
        }
        send(&self.tx, ScrobbleCmd::AuthStart)
    }

    fn admit_pending(&self, command: PendingCommand) -> DeliveryResult {
        let mut state = lock_pending(&self.pending.state);
        if state.closed || state.shutting_down || self.tx.is_closed() {
            state.closed |= self.tx.is_closed();
            return Err(DeliveryError::Closed);
        }

        let is_observation = matches!(&command, PendingCommand::Observe(_));
        let terminal_retry = match &command {
            PendingCommand::Observe(observation) if observation.ends_terminal() => {
                Some(observation.clone())
            }
            _ => None,
        };

        if !state.drainer_running
            && state.commands.is_empty()
            && state.observation_reserve.is_none()
            && state.barrier_commands.is_empty()
            && state.configuration.is_none()
            && state.post_configuration_observations.is_empty()
            && state.terminal_observations.is_empty()
        {
            match self.tx.try_send(command.into_actor_command()) {
                Ok(()) => {
                    if is_observation {
                        state.terminal_retry = None;
                    }
                    return Ok(DeliveryReceipt::Enqueued);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    state.closed = true;
                    return Err(DeliveryError::Closed);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(command)) => {
                    let command = pending_from_actor_command(command);
                    let result = stage_pending_locked(&self.tx, &self.pending, &mut state, command);
                    update_terminal_retry(&mut state, is_observation, terminal_retry, &result);
                    return result;
                }
            }
        }

        let result = stage_pending_locked(&self.tx, &self.pending, &mut state, command);
        update_terminal_retry(&mut state, is_observation, terminal_retry, &result);
        result
    }
}

fn update_terminal_retry(
    state: &mut PendingState,
    is_observation: bool,
    terminal_retry: Option<Box<ObservationBatch>>,
    result: &DeliveryResult,
) {
    if !is_observation {
        return;
    }
    match result {
        Ok(_) => state.terminal_retry = None,
        Err(DeliveryError::Busy) => {
            // A newer rejected terminal snapshot supersedes the previous rejected candidate.
            // A rejected playing heartbeat does not erase the last terminal edge; its eventual
            // successful retry will clear the slot atomically.
            if terminal_retry.is_some() {
                state.terminal_retry = terminal_retry;
            }
        }
        Err(_) => {}
    }
}

fn take_terminal_retry(pending: &PendingCommands) -> Option<Box<ObservationBatch>> {
    lock_pending(&pending.state).terminal_retry.take()
}

fn lock_pending(pending: &Mutex<PendingState>) -> std::sync::MutexGuard<'_, PendingState> {
    pending.lock().unwrap_or_else(|poisoned| {
        tracing::warn!("scrobble command backlog mutex poisoned; recovering");
        poisoned.into_inner()
    })
}

fn pending_from_actor_command(command: ScrobbleCmd) -> PendingCommand {
    match command {
        ScrobbleCmd::Observe(observation) => PendingCommand::Observe(observation),
        ScrobbleCmd::Reconfigure(settings) => PendingCommand::Reconfigure(*settings),
        ScrobbleCmd::AuthStart => unreachable!("only deferrable commands use the pending queue"),
    }
}

fn stage_pending_locked(
    tx: &Sender<ScrobbleCmd>,
    pending: &Arc<PendingCommands>,
    state: &mut PendingState,
    command: PendingCommand,
) -> DeliveryResult {
    let started_drainer = !state.drainer_running;
    if started_drainer {
        state.drainer_running = true;
        // Spawn while the mutex is held. The new thread cannot inspect an empty backlog before
        // the command below is staged, and failure is explicit without publishing a receipt for
        // work that has nowhere to go.
        if !spawn_pending_drainer(tx.clone(), Arc::clone(pending)) {
            state.drainer_running = false;
            return Err(DeliveryError::Busy);
        }
    }

    let replaced_existing = match command {
        PendingCommand::Observe(latest) => {
            if state.configuration.is_some()
                || !state.post_configuration_observations.is_empty()
                || !state.terminal_observations.is_empty()
            {
                stage_post_configuration_observation(state, latest)?
            } else if let Some(reserved) = state.observation_reserve.as_mut() {
                if reserved.can_merge(&latest) {
                    reserved.merge(*latest);
                    true
                } else if latest.ends_terminal() {
                    // The ordinary reserve may contain an incompatible playing/track edge. Keep
                    // it ordered and retain the final pause/stop in the dedicated tail slot.
                    stage_terminal_observation(state, latest)?
                } else {
                    return Err(DeliveryError::Busy);
                }
            } else if state.commands.back().is_some_and(|command| {
                matches!(command, PendingCommand::Observe(current) if current.can_merge(&latest))
            }) {
                let Some(PendingCommand::Observe(current)) = state.commands.back_mut() else {
                    unreachable!("compatible pending observation changed under the mutex")
                };
                current.merge(*latest);
                true
            } else if state.commands.len() < PENDING_COMMAND_CAPACITY {
                state.commands.push_back(PendingCommand::Observe(latest));
                false
            } else {
                state.observation_reserve = Some(latest);
                false
            }
        }
        PendingCommand::Reconfigure(latest) => {
            if !state.post_configuration_observations.is_empty()
                || !state.terminal_observations.is_empty()
            {
                let sealed_len = usize::from(state.configuration.is_some())
                    + state.post_configuration_observations.len()
                    + state.terminal_observations.len();
                let occupied_barrier_slots =
                    state.barrier_commands.len() + usize::from(state.barrier_command_in_flight);
                if sealed_len > PENDING_BARRIER_CAPACITY.saturating_sub(occupied_barrier_slots) {
                    return Err(DeliveryError::Busy);
                }
                // An observation between A and B makes both settings snapshots semantically
                // distinct. Seal A→observations (or just the observations when A is already
                // in flight) before staging B; no accepted edge is overwritten or reordered.
                if let Some(current) = state.configuration.take() {
                    state
                        .barrier_commands
                        .push_back(PendingCommand::Reconfigure(current));
                }
                state.barrier_commands.extend(
                    state
                        .post_configuration_observations
                        .drain(..)
                        .map(PendingCommand::Observe),
                );
                state.barrier_commands.extend(
                    state
                        .terminal_observations
                        .drain(..)
                        .map(PendingCommand::Observe),
                );
                state.configuration = Some(latest);
                false
            } else if let Some(current) = state.configuration.as_mut() {
                *current = latest;
                true
            } else {
                state.configuration = Some(latest);
                false
            }
        }
    };

    if !started_drainer {
        return if replaced_existing {
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing,
                evicted_oldest: false,
            })
        } else {
            Ok(DeliveryReceipt::Deferred)
        };
    }
    Ok(DeliveryReceipt::Deferred)
}

fn stage_post_configuration_observation(
    state: &mut PendingState,
    latest: Box<ObservationBatch>,
) -> Result<bool, DeliveryError> {
    if !state.terminal_observations.is_empty() {
        stage_terminal_observation(state, latest)
    } else if let Some(current) = state.post_configuration_observations.back_mut()
        && current.can_merge(&latest)
    {
        current.merge(*latest);
        Ok(true)
    } else if state.post_configuration_observations.len() < PENDING_COMMAND_CAPACITY {
        state.post_configuration_observations.push_back(latest);
        Ok(false)
    } else if latest.ends_terminal() {
        stage_terminal_observation(state, latest)
    } else {
        Err(DeliveryError::Busy)
    }
}

fn stage_terminal_observation(
    state: &mut PendingState,
    latest: Box<ObservationBatch>,
) -> Result<bool, DeliveryError> {
    if let Some(current) = state.terminal_observations.back_mut()
        && current.can_merge(&latest)
    {
        current.merge(*latest);
        Ok(true)
    } else if !latest.ends_terminal() {
        Err(DeliveryError::Busy)
    } else if state.terminal_observations.len() < PENDING_TERMINAL_CAPACITY {
        state.terminal_observations.push_back(latest);
        Ok(false)
    } else {
        Err(DeliveryError::Busy)
    }
}

fn spawn_pending_drainer(tx: Sender<ScrobbleCmd>, pending: Arc<PendingCommands>) -> bool {
    std::thread::Builder::new()
        .name("ytt-scrobble-delivery".to_owned())
        .spawn(move || {
            loop {
                {
                    let mut state = lock_pending(&pending.state);
                    if state.commands.is_empty()
                        && state.observation_reserve.is_none()
                        && state.barrier_commands.is_empty()
                        && state.configuration.is_none()
                        && state.post_configuration_observations.is_empty()
                        && state.terminal_observations.is_empty()
                    {
                        state.drainer_running = false;
                        pending.drained.notify_waiters();
                        return;
                    }
                }
                let (command, from_barrier) = {
                    let mut state = lock_pending(&pending.state);
                    let next = state
                        .commands
                        .pop_front()
                        .map(|command| (command, false))
                        .or_else(|| {
                            state
                                .observation_reserve
                                .take()
                                .map(PendingCommand::Observe)
                                .map(|command| (command, false))
                        })
                        .or_else(|| {
                            state
                                .barrier_commands
                                .pop_front()
                                .map(|command| (command, true))
                        })
                        .or_else(|| {
                            state
                                .configuration
                                .take()
                                .map(PendingCommand::Reconfigure)
                                .map(|command| (command, false))
                        })
                        .or_else(|| {
                            state
                                .post_configuration_observations
                                .pop_front()
                                .map(PendingCommand::Observe)
                                .map(|command| (command, false))
                        })
                        .or_else(|| {
                            state
                                .terminal_observations
                                .pop_front()
                                .map(PendingCommand::Observe)
                                .map(|command| (command, false))
                        });
                    match next {
                        Some((command, from_barrier)) => {
                            state.barrier_command_in_flight = from_barrier;
                            (command, from_barrier)
                        }
                        None => {
                            state.drainer_running = false;
                            pending.drained.notify_waiters();
                            return;
                        }
                    }
                };
                let send_failed = tx.blocking_send(command.into_actor_command()).is_err();
                if from_barrier || send_failed {
                    let mut state = lock_pending(&pending.state);
                    state.barrier_command_in_flight = false;
                    if !send_failed {
                        continue;
                    }
                    state.commands.clear();
                    state.observation_reserve = None;
                    state.barrier_commands.clear();
                    state.configuration = None;
                    state.post_configuration_observations.clear();
                    state.terminal_observations.clear();
                    state.terminal_retry = None;
                    state.drainer_running = false;
                    state.closed = true;
                    pending.drained.notify_waiters();
                    return;
                }
            }
        })
        .is_ok()
}

async fn wait_until_pending_drained(pending: &PendingCommands) -> Result<(), DeliveryError> {
    loop {
        let notified = pending.drained.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let state = lock_pending(&pending.state);
            if state.closed {
                return Err(DeliveryError::Closed);
            }
            if !state.drainer_running
                && state.commands.is_empty()
                && state.observation_reserve.is_none()
                && state.barrier_commands.is_empty()
                && state.configuration.is_none()
                && state.post_configuration_observations.is_empty()
                && state.terminal_observations.is_empty()
            {
                return Ok(());
            }
        }
        notified.await;
    }
}

fn send(tx: &Sender<ScrobbleCmd>, command: ScrobbleCmd) -> DeliveryResult {
    match tx.try_send(command) {
        Ok(()) => Ok(DeliveryReceipt::Enqueued),
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Err(DeliveryError::Busy),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Err(DeliveryError::Closed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_handle(
        capacity: usize,
    ) -> (
        ScrobbleHandle,
        tokio::sync::mpsc::Receiver<ScrobbleCmd>,
        tokio::sync::mpsc::Receiver<ShutdownRequest>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
        (ScrobbleHandle::new(tx, shutdown_tx), rx, shutdown_rx)
    }

    fn settings() -> ScrobbleSettings {
        ScrobbleSettings {
            lastfm_app: None,
            lastfm: None,
            listenbrainz: None,
            local_files: false,
        }
    }

    fn apply_observation_batch(
        monitor: &mut monitor::ScrobbleMonitor,
        batch: ObservationBatch,
    ) -> Vec<monitor::ScrobbleAction> {
        let (first, tail) = batch.into_parts();
        let mut actions = monitor.observe(&first, false);
        if let Some((latest, preserved_credit)) = tail {
            actions.extend(monitor.observe_with_preserved_credit(&latest, false, preserved_credit));
        }
        actions
    }

    #[tokio::test(flavor = "current_thread")]
    async fn full_inbox_retains_latest_observation_without_another_heartbeat() {
        let (mut handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.tx.try_send(ScrobbleCmd::AuthStart).is_ok());

        let first = crate::media::MediaSnapshot::idle();
        let mut stopped = crate::media::MediaSnapshot::idle();
        stopped.position_epoch = 7;
        assert!(handle.observe(&first).is_ok());
        assert!(handle.observe(&stopped).is_ok());
        assert_eq!(handle.last_fingerprint.as_ref().map(|f| f.epoch), Some(7));
        assert!(handle.last_sent.is_some());

        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Observe(observation)) if observation.latest().position_epoch == 0
        ));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Observe(observation)) if observation.latest().position_epoch == 7
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_config_and_heartbeat_bursts_keep_latest_values() {
        let (handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok());

        let mut prior = Observation::from_media(&crate::media::MediaSnapshot::idle());
        prior.position_epoch = 1;
        assert_eq!(
            handle.admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                prior
            ),))),
            Ok(DeliveryReceipt::Deferred)
        );
        let tail_started = Instant::now();
        for generation in 1..=PENDING_COMMAND_CAPACITY * 2 {
            let mut configured = settings();
            configured.local_files = generation == PENDING_COMMAND_CAPACITY * 2;
            assert!(handle.reconfigure(configured).is_ok());
        }
        for generation in 1..=PENDING_COMMAND_CAPACITY * 2 {
            let mut later = Observation::from_media(&crate::media::MediaSnapshot::idle());
            later.position_epoch = 2;
            later.position = generation as f64;
            later.at = tail_started + std::time::Duration::from_secs(generation as u64);
            assert!(
                handle
                    .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                        later
                    ),)))
                    .is_ok(),
                "the latest observation after configuration must have a bounded retry slot"
            );
        }

        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Observe(observation)) if observation.latest().position_epoch == 1
        ));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Reconfigure(settings)) if settings.local_files
        ));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Observe(observation))
                if observation.latest().position_epoch == 2
                    && observation.latest().position == (PENDING_COMMAND_CAPACITY * 2) as f64
        ));
        assert!(rx.try_recv().is_err(), "superseded snapshots must not leak");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observation_between_configs_preserves_a_then_observation_then_b() {
        let (handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok());

        let mut config_a = settings();
        config_a.local_files = false;
        let mut config_b = settings();
        config_b.local_files = true;
        assert!(handle.reconfigure(config_a).is_ok());

        let mut observation = Observation::from_media(&crate::media::MediaSnapshot::idle());
        observation.position_epoch = 7;
        assert!(
            handle
                .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                    observation,
                ))))
                .is_ok()
        );
        assert_eq!(
            handle.reconfigure(config_b),
            Ok(DeliveryReceipt::Deferred),
            "a config separated by an observation is a new barrier, not a replacement"
        );

        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Reconfigure(settings)) if !settings.local_files
        ));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Observe(observation))
                if observation.latest().position_epoch == 7
        ));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Reconfigure(settings)) if settings.local_files
        ));
    }

    #[test]
    fn full_barrier_chain_rejects_new_config_without_revoking_accepted_work() {
        let (handle, _rx, _shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok());
        {
            let mut state = lock_pending(&handle.pending.state);
            // Keep admission deterministic without a live drainer: model a delivery thread that
            // is blocked in the already-full actor inbox.
            state.drainer_running = true;
        }

        for segment in 0..=PENDING_BARRIER_CAPACITY / 2 {
            let mut config = settings();
            config.local_files = segment % 2 == 1;
            assert!(handle.reconfigure(config).is_ok());

            let mut observation = Observation::from_media(&crate::media::MediaSnapshot::idle());
            observation.position_epoch = segment as u64 + 1;
            assert!(
                handle
                    .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                        observation,
                    ))))
                    .is_ok()
            );
        }
        assert_eq!(handle.reconfigure(settings()), Err(DeliveryError::Busy));

        let state = lock_pending(&handle.pending.state);
        assert_eq!(
            state.barrier_commands.len() + usize::from(state.barrier_command_in_flight),
            PENDING_BARRIER_CAPACITY,
            "the next two-command barrier cannot fit once the sealed budget is full"
        );
        assert!(state.configuration.is_some());
        assert_eq!(state.post_configuration_observations.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn post_config_transition_tail_is_bounded_fifo_and_never_evicts_accepted_edges() {
        let (handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok());

        let mut prior = Observation::from_media(&crate::media::MediaSnapshot::idle());
        prior.position_epoch = 99;
        assert!(
            handle
                .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                    prior,
                ))))
                .is_ok()
        );
        assert!(handle.reconfigure(settings()).is_ok());

        for epoch in 1..=PENDING_COMMAND_CAPACITY as u64 {
            let mut edge = Observation::from_media(&crate::media::MediaSnapshot::idle());
            edge.position_epoch = epoch;
            assert!(
                handle
                    .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                        edge
                    ),)))
                    .is_ok()
            );
        }
        let mut overflow = Observation::from_media(&crate::media::MediaSnapshot::idle());
        overflow.playing = true;
        overflow.stopped = false;
        overflow.position_epoch = PENDING_COMMAND_CAPACITY as u64 + 1;
        assert_eq!(
            handle.admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                overflow,
            )))),
            Err(DeliveryError::Busy),
            "a non-terminal edge beyond the full transition tail reports Busy instead of revoking accepted work"
        );

        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Observe(observation)) if observation.latest().position_epoch == 99
        ));
        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::Reconfigure(_))));
        for epoch in 1..=PENDING_COMMAND_CAPACITY as u64 {
            assert!(matches!(
                rx.recv().await,
                Some(ScrobbleCmd::Observe(observation))
                    if observation.latest().position_epoch == epoch
            ));
        }
    }

    #[test]
    fn saturated_post_config_tail_retains_one_shot_final_pause() {
        let (mut handle, _rx, _shutdown_rx) = test_handle(1);
        {
            let mut state = lock_pending(&handle.pending.state);
            // Keep admission deterministic without a live drainer: model a delivery thread that
            // is blocked in the already-full actor inbox.
            state.drainer_running = true;
            state.configuration = Some(settings());
            for epoch in 1..=PENDING_COMMAND_CAPACITY as u64 {
                let mut heartbeat = Observation::from_media(&crate::media::MediaSnapshot::idle());
                heartbeat.playing = true;
                heartbeat.stopped = false;
                heartbeat.position_epoch = epoch;
                state
                    .post_configuration_observations
                    .push_back(Box::new(ObservationBatch::single(heartbeat)));
            }
        }

        let mut paused = crate::media::MediaSnapshot::idle();
        paused.status = crate::media::MediaPlaybackStatus::Paused;
        paused.position_epoch = 777;
        assert!(handle.observe(&paused).is_ok());

        assert_eq!(handle.last_fingerprint.as_ref().map(|f| f.epoch), Some(777));
        let mut stopped = crate::media::MediaSnapshot::idle();
        stopped.position_epoch = 778;
        assert!(handle.observe(&stopped).is_ok());
        assert_eq!(handle.last_fingerprint.as_ref().map(|f| f.epoch), Some(778));
        assert_eq!(
            handle.last_fingerprint.as_ref().map(|f| f.stopped),
            Some(true)
        );
        let state = lock_pending(&handle.pending.state);
        assert_eq!(
            state.post_configuration_observations.len(),
            PENDING_COMMAND_CAPACITY,
            "terminal reserve does not evict an accepted transition"
        );
        assert_eq!(state.terminal_observations.len(), 2);
        let paused = state.terminal_observations.front().unwrap().latest();
        assert_eq!(paused.position_epoch, 777);
        assert!(!paused.playing);
        assert!(!paused.stopped);
        let stopped = state.terminal_observations.back().unwrap().latest();
        assert_eq!(stopped.position_epoch, 778);
        assert!(!stopped.playing);
        assert!(stopped.stopped);
    }

    #[test]
    fn saturated_pre_config_backlog_retains_final_pause_stop_after_incompatible_reserve() {
        let (mut handle, _rx, _shutdown_rx) = test_handle(1);
        {
            let mut state = lock_pending(&handle.pending.state);
            // Model a drainer blocked on a full actor inbox: sixteen distinct transitions occupy
            // the ordinary queue and an incompatible playing edge already owns its reserve.
            state.drainer_running = true;
            for epoch in 1..=PENDING_COMMAND_CAPACITY as u64 {
                let mut edge = Observation::from_media(&crate::media::MediaSnapshot::idle());
                edge.playing = true;
                edge.stopped = false;
                edge.position_epoch = epoch;
                state.commands.push_back(PendingCommand::Observe(Box::new(
                    ObservationBatch::single(edge),
                )));
            }
            let mut reserved = Observation::from_media(&crate::media::MediaSnapshot::idle());
            reserved.playing = true;
            reserved.stopped = false;
            reserved.position_epoch = 500;
            state.observation_reserve = Some(Box::new(ObservationBatch::single(reserved)));
        }

        let mut paused = crate::media::MediaSnapshot::idle();
        paused.status = crate::media::MediaPlaybackStatus::Paused;
        paused.position_epoch = 777;
        assert!(handle.observe(&paused).is_ok());
        let mut stopped = crate::media::MediaSnapshot::idle();
        stopped.position_epoch = 778;
        assert!(handle.observe(&stopped).is_ok());

        assert_eq!(handle.last_fingerprint.as_ref().map(|f| f.epoch), Some(778));
        let state = lock_pending(&handle.pending.state);
        assert_eq!(state.commands.len(), PENDING_COMMAND_CAPACITY);
        assert_eq!(
            state
                .observation_reserve
                .as_ref()
                .expect("playing reserve remains ordered first")
                .latest()
                .position_epoch,
            500
        );
        assert!(state.configuration.is_none());
        assert!(state.post_configuration_observations.is_empty());
        assert_eq!(state.terminal_observations.len(), 2);
        let paused = state.terminal_observations.front().unwrap().latest();
        assert_eq!(paused.position_epoch, 777);
        assert!(!paused.playing);
        assert!(!paused.stopped);
        let stopped = state.terminal_observations.back().unwrap().latest();
        assert_eq!(stopped.position_epoch, 778);
        assert!(!stopped.playing);
        assert!(stopped.stopped);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saturated_incompatible_lane_delivers_terminal_edges_in_order_without_retry() {
        let (mut handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok(), "actor inbox starts full");
        {
            let mut state = lock_pending(&handle.pending.state);
            // Model the delivery thread blocked on that full inbox. Its ordinary reserve already
            // contains an incompatible playing edge, forcing terminal states into the dedicated
            // bounded lane without relying on scheduler timing.
            state.drainer_running = true;
            let mut reserved = Observation::from_media(&crate::media::MediaSnapshot::idle());
            reserved.playing = true;
            reserved.stopped = false;
            reserved.position_epoch = 500;
            state.observation_reserve = Some(Box::new(ObservationBatch::single(reserved)));
        }

        for (epoch, status) in [
            (777, crate::media::MediaPlaybackStatus::Paused),
            (778, crate::media::MediaPlaybackStatus::Stopped),
            (779, crate::media::MediaPlaybackStatus::Paused),
            (780, crate::media::MediaPlaybackStatus::Stopped),
        ] {
            let mut terminal = crate::media::MediaSnapshot::idle();
            terminal.status = status;
            terminal.position_epoch = epoch;
            assert!(handle.observe(&terminal).is_ok());
        }
        {
            let state = lock_pending(&handle.pending.state);
            assert_eq!(state.terminal_observations.len(), 4);
            assert_eq!(
                state
                    .terminal_observations
                    .iter()
                    .map(|batch| batch.latest().position_epoch)
                    .collect::<Vec<_>>(),
                vec![777, 778, 779, 780]
            );
        }

        assert!(spawn_pending_drainer(
            handle.tx.clone(),
            Arc::clone(&handle.pending)
        ));
        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Observe(observation))
                if observation.latest().position_epoch == 500
        ));
        for epoch in 777..=780 {
            assert!(matches!(
                rx.recv().await,
                Some(ScrobbleCmd::Observe(terminal))
                    if terminal.latest().position_epoch == epoch
            ));
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_until_pending_drained(&handle.pending),
        )
        .await
        .expect("delivery thread drains deterministically")
        .expect("actor lane remains open");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn threshold_crossing_before_incompatible_track_edge_is_not_overwritten() {
        let (handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok());
        let mut prior = Observation::from_media(&crate::media::MediaSnapshot::idle());
        prior.position_epoch = 99;
        assert!(
            handle
                .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                    prior,
                ))))
                .is_ok()
        );
        assert!(handle.reconfigure(settings()).is_ok());

        let track = |key: &str| ObservedTrack {
            key: key.to_owned(),
            title: key.to_owned(),
            artist: "Artist".to_owned(),
            album: None,
            duration: Some(40.0),
            is_live: false,
            is_local: false,
            origin_url: None,
            liked: false,
        };
        let started_at = Instant::now();
        let started_unix = crate::signals::unix_now();
        for step in 0..=4_u64 {
            let observation = Observation {
                track: Some(track("threshold-a")),
                playing: true,
                stopped: false,
                position: (step * 5) as f64,
                position_epoch: 1,
                rate: 1.0,
                at: started_at + std::time::Duration::from_secs(step * 5),
                wall_unix: started_unix + (step * 5) as i64,
            };
            assert!(
                handle
                    .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                        observation
                    ),)))
                    .is_ok()
            );
        }
        let next_track = Observation {
            track: Some(track("incompatible-b")),
            playing: true,
            stopped: false,
            position: 0.0,
            position_epoch: 2,
            rate: 1.0,
            at: started_at + std::time::Duration::from_secs(21),
            wall_unix: started_unix + 21,
        };
        assert!(
            handle
                .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                    next_track,
                ))))
                .is_ok()
        );

        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::Observe(_))));
        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::Reconfigure(_))));
        let mut monitor = monitor::ScrobbleMonitor::new();
        let mut actions = Vec::new();
        for _ in 0..2 {
            let Some(ScrobbleCmd::Observe(batch)) = rx.recv().await else {
                panic!("both incompatible post-configuration edges must remain ordered")
            };
            actions.extend(apply_observation_batch(&mut monitor, *batch));
        }
        assert!(actions.iter().any(|action| matches!(
            action,
            monitor::ScrobbleAction::Scrobble(track) if track.key == "threshold-a"
        )));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn thirty_heartbeats_during_saturation_preserve_threshold_credit() {
        let (handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok());
        let track = ObservedTrack {
            key: "saturated-track".to_owned(),
            title: "Saturated Track".to_owned(),
            artist: "Artist".to_owned(),
            album: None,
            duration: Some(40.0),
            is_live: false,
            is_local: false,
            origin_url: None,
            liked: false,
        };
        let started_at = Instant::now();
        let started_unix = crate::signals::unix_now();
        for step in 0..=30_u64 {
            let observation = Observation {
                track: Some(track.clone()),
                playing: true,
                stopped: false,
                position: step as f64,
                position_epoch: 1,
                rate: 1.0,
                at: started_at + std::time::Duration::from_secs(step),
                wall_unix: started_unix + step as i64,
            };
            assert!(
                handle
                    .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                        observation,
                    ))))
                    .is_ok()
            );
        }

        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
        let mut monitor = monitor::ScrobbleMonitor::new();
        let mut actions = Vec::new();
        let mut previous: Option<Observation> = None;
        let mut delivered_credit = 0.0;
        loop {
            let Some(ScrobbleCmd::Observe(batch)) = rx.recv().await else {
                panic!("coalesced heartbeat batch must reach the actor")
            };
            let (first, tail) = batch.into_parts();
            if let Some(previous) = previous.as_ref() {
                delivered_credit += previous.credit_until(&first);
            }
            actions.extend(monitor.observe(&first, false));
            let latest = if let Some((latest, preserved_credit)) = tail {
                delivered_credit += preserved_credit;
                actions.extend(monitor.observe_with_preserved_credit(
                    &latest,
                    false,
                    preserved_credit,
                ));
                latest
            } else {
                first
            };
            let complete = latest.position == 30.0;
            previous = Some(latest);
            if complete {
                break;
            }
        }

        assert!(actions.iter().any(|action| matches!(
            action,
            monitor::ScrobbleAction::Scrobble(track) if track.key == "saturated-track"
        )));
        assert_eq!(delivered_credit, 30.0);
    }

    #[test]
    fn observe_preserves_admission_state_after_closed_queue() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        drop(shutdown_rx);
        let mut handle = ScrobbleHandle::new(tx, shutdown_tx);

        assert_eq!(
            handle.observe(&crate::media::MediaSnapshot::idle()),
            Err(DeliveryError::Closed)
        );
        assert!(handle.last_fingerprint.is_none());
        assert!(handle.last_sent.is_none());
    }

    #[test]
    fn deferred_reconfigure_survives_without_a_tokio_runtime() {
        let (handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.tx.try_send(ScrobbleCmd::AuthStart).is_ok());

        let mut configured = settings();
        configured.local_files = true;
        assert_eq!(
            handle.reconfigure(configured),
            Ok(DeliveryReceipt::Deferred)
        );
        assert!(matches!(rx.blocking_recv(), Some(ScrobbleCmd::AuthStart)));
        assert!(matches!(
            rx.blocking_recv(),
            Some(ScrobbleCmd::Reconfigure(settings)) if settings.local_files
        ));
    }

    #[tokio::test]
    async fn control_commands_report_full_queue() {
        let (handle, _rx, _shutdown_rx) = test_handle(1);
        assert!(handle.tx.try_send(ScrobbleCmd::AuthStart).is_ok());

        assert_eq!(handle.auth_start(), Err(DeliveryError::Busy));
        let (done, _done_rx) = tokio::sync::oneshot::channel();
        assert!(
            handle
                .shutdown_tx
                .try_send(ShutdownRequest { done })
                .is_ok()
        );
        assert_eq!(handle.shutdown_flush().await, Err(DeliveryError::Busy));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reconfigure_retries_and_merges_the_latest_snapshot() {
        let (handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok());
        {
            let mut state = lock_pending(&handle.pending.state);
            // Model a delivery thread blocked on the full actor inbox so both reconfigures are
            // admitted before a real drainer can remove the first snapshot.
            state.drainer_running = true;
        }

        let mut first = settings();
        first.local_files = false;
        let mut latest = settings();
        latest.local_files = true;
        assert_eq!(handle.reconfigure(first), Ok(DeliveryReceipt::Deferred));
        assert_eq!(
            handle.reconfigure(latest),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        );
        assert_eq!(handle.auth_start(), Err(DeliveryError::Busy));

        assert!(spawn_pending_drainer(
            handle.tx.clone(),
            Arc::clone(&handle.pending)
        ));
        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Reconfigure(settings)) if settings.local_files
        ));
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "the replaced snapshot must not leak"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_pause_after_deferred_reconfigure_is_delivered_without_another_media_event() {
        let (mut handle, mut rx, _shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok());

        let mut configured = settings();
        configured.local_files = true;
        assert_eq!(
            handle.reconfigure(configured),
            Ok(DeliveryReceipt::Deferred)
        );
        let mut paused = crate::media::MediaSnapshot::idle();
        paused.status = crate::media::MediaPlaybackStatus::Paused;
        paused.position_epoch = 19;
        assert!(handle.observe(&paused).is_ok());
        assert_eq!(handle.last_fingerprint.as_ref().map(|f| f.epoch), Some(19));
        assert_eq!(
            handle.last_fingerprint.as_ref().map(|f| f.playing),
            Some(false)
        );
        assert_eq!(
            handle.last_fingerprint.as_ref().map(|f| f.stopped),
            Some(false)
        );

        assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Reconfigure(settings)) if settings.local_files
        ));
        assert!(matches!(
            rx.recv().await,
            Some(ScrobbleCmd::Observe(observation))
                if observation.latest().position_epoch == 19
                    && !observation.latest().playing
                    && !observation.latest().stopped
        ));
    }

    #[tokio::test]
    async fn control_commands_report_closed_queue() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        drop(shutdown_rx);
        let handle = ScrobbleHandle::new(tx, shutdown_tx);

        assert_eq!(handle.reconfigure(settings()), Err(DeliveryError::Closed));
        assert_eq!(handle.auth_start(), Err(DeliveryError::Closed));
        assert_eq!(handle.shutdown_flush().await, Err(DeliveryError::Closed));
    }

    #[tokio::test]
    async fn shutdown_flush_admits_prior_observation_latest_configuration_and_final_pause() {
        let (mut handle, mut rx, mut shutdown_rx) = test_handle(1);
        assert!(handle.auth_start().is_ok());
        let mut observed = crate::media::MediaSnapshot::idle();
        observed.position_epoch = 23;
        assert!(handle.observe(&observed).is_ok());
        let mut latest = settings();
        latest.local_files = true;
        assert!(handle.reconfigure(settings()).is_ok());
        assert!(handle.reconfigure(latest).is_ok());
        let mut paused = crate::media::MediaSnapshot::idle();
        paused.status = crate::media::MediaPlaybackStatus::Paused;
        paused.position_epoch = 24;
        assert!(handle.observe(&paused).is_ok());

        let flush = handle.shutdown_flush();
        let consume_and_acknowledge = async move {
            assert!(matches!(rx.recv().await, Some(ScrobbleCmd::AuthStart)));
            assert!(matches!(
                rx.recv().await,
                Some(ScrobbleCmd::Observe(observation)) if observation.latest().position_epoch == 23
            ));
            assert!(matches!(
                rx.recv().await,
                Some(ScrobbleCmd::Reconfigure(settings)) if settings.local_files
            ));
            assert!(matches!(
                rx.recv().await,
                Some(ScrobbleCmd::Observe(observation))
                    if observation.latest().position_epoch == 24
                        && !observation.latest().playing
                        && !observation.latest().stopped
            ));
            let request = shutdown_rx.recv().await.expect("shutdown command queued");
            request.done.send(Ok(())).unwrap();
        };
        let (result, ()) = tokio::join!(flush, consume_and_acknowledge);
        assert_eq!(result, Ok(()));
    }
}
