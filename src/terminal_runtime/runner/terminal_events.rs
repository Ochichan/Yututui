//! Exclusive terminal input ownership and terminal-client liveness.
//!
//! Crossterm cursor-position queries consume their replies from the same global input source as
//! ordinary events. Keeping both operations on this one thread prevents the heartbeat from
//! stealing keys (or the event loop from stealing CPR replies). The owner receives events over a
//! bounded channel. Saturation is terminal-owner failure: the reader kills media rather than
//! dropping input or blocking the liveness deadline behind an unresponsive reducer.

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::Event;

use crate::player::lifetime::ShutdownLatch;
use crate::terminal_policy::{
    AMBIGUOUS_CONFIRMATIONS, AMBIGUOUS_RETRY_INTERVAL,
    HARD_WATCHDOG_TIMEOUT as RUNTIME_LIVENESS_DEADLINE, HEARTBEAT_INTERVAL, OWNER_PROBE_INTERVAL,
    STARTUP_LIVENESS_REPORT_TIMEOUT as STARTUP_READY_TIMEOUT,
};

mod backend;
mod liveness;
mod output_gate;
#[cfg(unix)]
mod owner_probe;
mod schedule;
#[cfg(test)]
mod shutdown_race_tests;
use backend::{
    CrosstermInput, output_operation_stage, owner_output_operation_expired,
    terminal_output_operation,
};
use liveness::{
    EvidenceDomain, FailureStore, LivenessStage, ProbeOutcome, SuspicionTracker, TerminalFailure,
    TerminalFailureClass, WatchdogProgress,
};
use output_gate::OutputRole;
pub(in crate::terminal_runtime::runner) use output_gate::TerminalOutputLock;
#[cfg(unix)]
#[cfg(test)]
use owner_probe::OWNER_PROBE_TIMEOUT;
use schedule::{LivenessAction, LivenessSchedule};

// Multiplexer CLIs are materially heavier than CPR and need not run for every input heartbeat.
// A detach immediately after a successful query is detected within 2.5 s plus the bounded 500 ms
// query; explicit no-client results remain immediate rather than consuming ambiguity retries.
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(50);
const WATCHDOG_SCHEDULING_GAP: Duration = Duration::from_secs(1);
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_millis(2500);
const WEDGED_JOIN_TIMEOUT: Duration = Duration::from_millis(100);
const STARTUP_JOIN_GRACE: Duration = Duration::from_millis(100);
type KillMedia = Arc<dyn Fn() + Send + Sync + 'static>;
type SharedFailure = Arc<FailureStore>;

#[cfg(not(test))]
fn cancel_terminal_output() {
    crate::tui::cancel_output();
}

// TUI writer tests share a process-global active-writer slot. Liveness unit tests exercise the
// gate wake separately and must not cancel a different concurrently running PTY fixture.
#[cfg(test)]
fn cancel_terminal_output() {}

/// The async side of the exclusive terminal input source.
pub(super) struct TerminalEventReceiver {
    events: tokio::sync::mpsc::Receiver<Event>,
    failure: SharedFailure,
}

impl TerminalEventReceiver {
    pub(super) async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// Return a non-consuming snapshot of the immutable first failure plus any teardown details.
    pub(super) fn take_failure(&self) -> Option<io::Error> {
        self.failure.error_snapshot()
    }
}

/// Join ownership for the blocking terminal reader.
pub(super) struct TerminalEventWorker {
    stop: Arc<AtomicBool>,
    shutdown: ShutdownLatch,
    input_thread: Option<JoinHandle<()>>,
    watchdog_thread: Option<JoinHandle<()>>,
    failure: SharedFailure,
    output_lock: TerminalOutputLock,
}

impl TerminalEventWorker {
    pub(super) async fn shutdown(&mut self) -> Option<io::Error> {
        // Win terminal-failure arbitration before cancelling an in-flight CPR/gate operation.
        self.shutdown.trigger();
        self.stop.store(true, Ordering::Release);
        cancel_terminal_output();
        self.output_lock.wake_all();
        let join_timeout =
            if self.failure.primary_class() == Some(TerminalFailureClass::WorkerStall) {
                WEDGED_JOIN_TIMEOUT
            } else {
                SHUTDOWN_JOIN_TIMEOUT
            };
        let deadline = Instant::now() + join_timeout;
        join_threads_until(
            &mut self.input_thread,
            &mut self.watchdog_thread,
            deadline,
            &self.failure,
        )
        .await;
        self.failure.error_snapshot()
    }
}

impl Drop for TerminalEventWorker {
    fn drop(&mut self) {
        // The normal teardown path joins the thread. The atomic is the non-blocking fallback for
        // panic/cancellation; process teardown remains the ultimate boundary in that path.
        self.shutdown.trigger();
        self.stop.store(true, Ordering::Release);
        self.output_lock.wake_all();
    }
}

async fn join_threads_until(
    input: &mut Option<JoinHandle<()>>,
    watchdog: &mut Option<JoinHandle<()>>,
    deadline: Instant,
    failure: &SharedFailure,
) {
    loop {
        join_finished_thread(input, "terminal input worker", failure);
        join_finished_thread(watchdog, "terminal liveness watchdog", failure);
        if input.is_none() && watchdog.is_none() {
            return;
        }
        if Instant::now() >= deadline {
            if input.take().is_some() {
                tracing::warn!(
                    "terminal input worker exceeded its bounded shutdown join; detaching until process exit"
                );
            }
            if watchdog.take().is_some() {
                tracing::warn!(
                    "terminal liveness watchdog exceeded its bounded shutdown join; detaching until process exit"
                );
            }
            record_background_failure(
                failure,
                TerminalFailure::runtime(
                    io::ErrorKind::TimedOut,
                    TerminalFailureClass::WorkerStall,
                    LivenessStage::Stopping,
                    None,
                    Duration::ZERO,
                    "terminal background worker exceeded its bounded shutdown join",
                ),
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn join_finished_thread(thread: &mut Option<JoinHandle<()>>, label: &str, failure: &SharedFailure) {
    if !thread.as_ref().is_some_and(JoinHandle::is_finished) {
        return;
    }
    let joined = thread.take().expect("finished thread exists").join();
    if joined.is_err() {
        record_background_failure(
            failure,
            TerminalFailure::runtime(
                io::ErrorKind::Other,
                TerminalFailureClass::InternalFatal,
                LivenessStage::Stopping,
                None,
                Duration::ZERO,
                format!("{label} panicked while joining"),
            ),
        );
    }
}

fn stop_and_join_startup_threads(
    stop: &AtomicBool,
    output_lock: &TerminalOutputLock,
    input: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
) {
    stop.store(true, Ordering::Release);
    cancel_terminal_output();
    output_lock.wake_all();
    let deadline = Instant::now() + STARTUP_JOIN_GRACE;
    for thread in [input, watchdog].into_iter().flatten() {
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
        // A call stuck below crossterm or the OS cannot be cancelled portably. Dropping an
        // unfinished JoinHandle detaches it; no media has been admitted on this startup path.
    }
}

/// Start the sole crossterm input reader and wait for its first liveness check.
///
/// On Unix readiness includes a successful CPR round trip, whose crossterm implementation has a
/// two-second response timeout. This function therefore returns before any long-lived player is
/// spawned, or fails closed if the terminal cannot prove that a client is present.
pub(super) fn start(
    shutdown: ShutdownLatch,
) -> io::Result<(
    TerminalEventReceiver,
    TerminalEventWorker,
    TerminalOutputLock,
)> {
    let (event_tx, event_rx) =
        crate::util::backpressure::bounded_channel(crate::util::backpressure::OWNER_EVENT_QUEUE);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let failure = Arc::new(FailureStore::default());
    let progress = Arc::new(WatchdogProgress::default());
    let kill_media: KillMedia = Arc::new(crate::player::lifetime::kill_mpv_now);
    let output_lock = TerminalOutputLock::new(shutdown.clone());

    let watchdog_stop = Arc::clone(&stop);
    let watchdog_progress = Arc::clone(&progress);
    let watchdog_shutdown = shutdown.clone();
    let watchdog_failure = Arc::clone(&failure);
    let watchdog_kill_media = Arc::clone(&kill_media);
    let watchdog_output_lock = output_lock.clone();
    let watchdog_thread = std::thread::Builder::new()
        .name("ytt-terminal-watchdog".to_owned())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                run_watchdog(
                    Arc::clone(&watchdog_progress),
                    Arc::clone(&watchdog_stop),
                    watchdog_shutdown.clone(),
                    Arc::clone(&watchdog_failure),
                    Arc::clone(&watchdog_kill_media),
                    watchdog_output_lock.clone(),
                    (RUNTIME_LIVENESS_DEADLINE, None),
                );
            }));
            signal_abnormal_worker_exit(
                "terminal liveness watchdog",
                result,
                &watchdog_stop,
                &watchdog_shutdown,
                &watchdog_failure,
                watchdog_kill_media.as_ref(),
                &watchdog_output_lock,
            );
        })?;

    let thread_stop = Arc::clone(&stop);
    let thread_failure = Arc::clone(&failure);
    let thread_output_lock = output_lock.clone();
    let thread_progress = Arc::clone(&progress);
    let thread_kill_media = Arc::clone(&kill_media);
    let input_shutdown = shutdown.clone();
    let input_thread = match std::thread::Builder::new()
        .name("ytt-terminal-input".to_owned())
        .spawn(move || {
            let worker_output = thread_output_lock.clone();
            let worker_shutdown = input_shutdown.clone();
            // A panic unwinds through `run_input_loop` and drops both reporting senders. Keep one
            // of each alive until the wrapper publishes the exact panic failure and triggers
            // shutdown, so neither startup nor the owner can synthesize a generic disconnect first.
            let event_channel_guard = event_tx.clone();
            let ready_channel_guard = ready_tx.clone();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let control_output = thread_output_lock.clone();
                let backend = CrosstermInput::detect(thread_output_lock);
                run_input_loop(
                    backend,
                    event_tx,
                    ready_tx,
                    InputLoopControl {
                        stop: Arc::clone(&thread_stop),
                        shutdown: input_shutdown.clone(),
                        failure: Arc::clone(&thread_failure),
                        progress: Arc::clone(&thread_progress),
                        kill_media: Arc::clone(&thread_kill_media),
                        output_lock: control_output,
                        heartbeat_interval: HEARTBEAT_INTERVAL,
                        owner_probe_interval: OWNER_PROBE_INTERVAL,
                    },
                );
            }));
            signal_abnormal_worker_exit(
                "terminal input worker",
                result,
                &thread_stop,
                &worker_shutdown,
                &thread_failure,
                thread_kill_media.as_ref(),
                &worker_output,
            );
            drop(ready_channel_guard);
            drop(event_channel_guard);
        }) {
        Ok(thread) => thread,
        Err(error) => {
            stop_and_join_startup_threads(&stop, &output_lock, None, Some(watchdog_thread));
            return Err(error);
        }
    };

    match ready_rx.recv_timeout(STARTUP_READY_TIMEOUT) {
        Ok(Ok(())) => {
            progress.arm();
            Ok((
                TerminalEventReceiver {
                    events: event_rx,
                    failure: Arc::clone(&failure),
                },
                TerminalEventWorker {
                    stop,
                    shutdown: shutdown.clone(),
                    input_thread: Some(input_thread),
                    watchdog_thread: Some(watchdog_thread),
                    failure,
                    output_lock: output_lock.clone(),
                },
                output_lock,
            ))
        }
        Ok(Err(error)) => {
            let reported = error.into_error();
            stop_and_join_startup_threads(
                &stop,
                &output_lock,
                Some(input_thread),
                Some(watchdog_thread),
            );
            Err(failure.error_snapshot().unwrap_or(reported))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let snapshot = progress.snapshot(Instant::now());
            let stage = if snapshot.stage == LivenessStage::Idle {
                LivenessStage::CprWait
            } else {
                snapshot.stage
            };
            let timeout_failure = TerminalFailure::startup(
                io::ErrorKind::TimedOut,
                TerminalFailureClass::WorkerStall,
                stage,
                Some(EvidenceDomain::Transport),
                STARTUP_READY_TIMEOUT,
                "terminal readiness worker exceeded its startup report deadline",
            );
            fail_runtime(
                &failure,
                &shutdown,
                &stop,
                timeout_failure.with_last_alive(snapshot.last_alive_elapsed),
                kill_media.as_ref(),
                Some(&output_lock),
            );
            stop_and_join_startup_threads(
                &stop,
                &output_lock,
                Some(input_thread),
                Some(watchdog_thread),
            );
            Err(failure.error_snapshot().unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "terminal liveness startup check exceeded 8250 ms; use `ytt daemon` for background playback",
                )
            }))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            stop_and_join_startup_threads(
                &stop,
                &output_lock,
                Some(input_thread),
                Some(watchdog_thread),
            );
            Err(failure.error_snapshot().unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "terminal input worker stopped before reporting readiness",
                )
            }))
        }
    }
}

trait InputBackend {
    fn check_owner_attached(&mut self) -> Vec<ProbeOutcome>;
    fn heartbeat(&mut self) -> ProbeOutcome;
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

struct InputLoopControl {
    stop: Arc<AtomicBool>,
    shutdown: ShutdownLatch,
    failure: SharedFailure,
    progress: Arc<WatchdogProgress>,
    kill_media: KillMedia,
    output_lock: TerminalOutputLock,
    heartbeat_interval: Duration,
    owner_probe_interval: Duration,
}

fn run_input_loop<B: InputBackend>(
    mut backend: B,
    events: tokio::sync::mpsc::Sender<Event>,
    ready: std::sync::mpsc::SyncSender<Result<(), TerminalFailure>>,
    control: InputLoopControl,
) {
    let InputLoopControl {
        stop,
        shutdown,
        failure,
        progress,
        kill_media,
        output_lock,
        heartbeat_interval,
        owner_probe_interval,
    } = control;
    if let Err(failure_value) =
        initial_liveness_check(&mut backend, &stop, &shutdown, &progress, Instant::now())
    {
        fail_runtime(
            &failure,
            &shutdown,
            &stop,
            failure_value.clone(),
            kill_media.as_ref(),
            Some(&output_lock),
        );
        let _ = ready.send(Err(failure_value));
        return;
    }
    progress.alive();
    if ready.send(Ok(())).is_err() {
        stop.store(true, Ordering::Release);
        return;
    }

    let mut liveness =
        LivenessSchedule::new(Instant::now(), heartbeat_interval, owner_probe_interval);
    let mut suspicions = SuspicionTracker::default();
    let mut input_error_generation = 0u64;
    while !stop.load(Ordering::Acquire) && !shutdown.is_triggered() {
        let now = Instant::now();
        if progress.take_probe_request() {
            liveness.force_heartbeat(now);
        }
        if let Some(action) = liveness.due(now) {
            let outcomes = match action {
                LivenessAction::Heartbeat => {
                    progress.enter(LivenessStage::CprWait);
                    vec![backend.heartbeat()]
                }
                LivenessAction::OwnerProbe => {
                    progress.enter(LivenessStage::OwnerProbe);
                    backend.check_owner_attached()
                }
            };
            if runtime_is_stopping(&stop, &shutdown) {
                break;
            }
            match evaluate_probe_outcomes(
                outcomes,
                &mut suspicions,
                &progress,
                false,
                Instant::now(),
            ) {
                Ok(ProbeDisposition::Complete) => {
                    liveness.completed(action, Instant::now());
                }
                Ok(ProbeDisposition::Retry) => {
                    liveness.retry(action, Instant::now());
                }
                Err(terminal_failure) => {
                    fail_runtime(
                        &failure,
                        &shutdown,
                        &stop,
                        terminal_failure,
                        kill_media.as_ref(),
                        Some(&output_lock),
                    );
                    return;
                }
            }
            continue;
        }

        let timeout = liveness.until_next(now).min(STOP_POLL_INTERVAL);
        progress.enter(LivenessStage::InputPoll);
        let polled = backend.poll(timeout);
        if runtime_is_stopping(&stop, &shutdown) {
            break;
        }
        match polled {
            Ok(false) => progress.idle(),
            Ok(true) => {
                progress.enter(LivenessStage::InputRead);
                let read = backend.read();
                if runtime_is_stopping(&stop, &shutdown) {
                    break;
                }
                match read {
                    Ok(event) => match events.try_send(event) {
                        Ok(()) => {
                            suspicions.clear(EvidenceDomain::Transport);
                            progress.alive();
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            let terminal_failure = TerminalFailure::runtime(
                                io::ErrorKind::Other,
                                TerminalFailureClass::InternalFatal,
                                LivenessStage::EventDelivery,
                                None,
                                Duration::ZERO,
                                "terminal input queue saturated; owner is unresponsive",
                            )
                            .with_last_alive(progress.snapshot(Instant::now()).last_alive_elapsed);
                            fail_runtime(
                                &failure,
                                &shutdown,
                                &stop,
                                terminal_failure,
                                kill_media.as_ref(),
                                Some(&output_lock),
                            );
                            return;
                        }
                    },
                    Err(error) if is_transient_input_error(&error) => progress.idle(),
                    Err(error) => {
                        input_error_generation = input_error_generation.wrapping_add(1).max(1);
                        match evaluate_probe_outcomes(
                            [ProbeOutcome::transport(
                                error,
                                input_error_generation,
                                LivenessStage::InputRead,
                            )],
                            &mut suspicions,
                            &progress,
                            false,
                            Instant::now(),
                        ) {
                            Ok(ProbeDisposition::Complete) => {}
                            Ok(ProbeDisposition::Retry) => {
                                wait_retry_interruptibly(&stop, &shutdown);
                            }
                            Err(terminal_failure) => {
                                fail_runtime(
                                    &failure,
                                    &shutdown,
                                    &stop,
                                    terminal_failure,
                                    kill_media.as_ref(),
                                    Some(&output_lock),
                                );
                                return;
                            }
                        }
                    }
                }
            }
            Err(error) if is_transient_input_error(&error) => progress.idle(),
            Err(error) => {
                input_error_generation = input_error_generation.wrapping_add(1).max(1);
                match evaluate_probe_outcomes(
                    [ProbeOutcome::transport(
                        error,
                        input_error_generation,
                        LivenessStage::InputPoll,
                    )],
                    &mut suspicions,
                    &progress,
                    false,
                    Instant::now(),
                ) {
                    Ok(ProbeDisposition::Complete) => {}
                    Ok(ProbeDisposition::Retry) => {
                        wait_retry_interruptibly(&stop, &shutdown);
                    }
                    Err(terminal_failure) => {
                        fail_runtime(
                            &failure,
                            &shutdown,
                            &stop,
                            terminal_failure,
                            kill_media.as_ref(),
                            Some(&output_lock),
                        );
                        return;
                    }
                }
            }
        }
    }
    cancel_terminal_output();
    output_lock.wake_all();
    progress.enter(LivenessStage::Stopping);
}

fn run_watchdog(
    progress: Arc<WatchdogProgress>,
    stop: Arc<AtomicBool>,
    shutdown: ShutdownLatch,
    failure: SharedFailure,
    kill_media: KillMedia,
    output_lock: TerminalOutputLock,
    policy: (Duration, Option<KillMedia>),
) {
    let (deadline, before_failure_arbitration) = policy;
    let mut last_tick = Instant::now();
    while !stop.load(Ordering::Acquire) && !shutdown.is_triggered() {
        let now = Instant::now();
        let scheduling_gap = now.saturating_duration_since(last_tick);
        if scheduling_gap >= WATCHDOG_SCHEDULING_GAP {
            // A delayed watchdog alone is not evidence that the process was suspended. Preserve
            // absolute hard deadlines and merely request a fresh liveness observation.
            progress.request_probe();
        }
        last_tick = now;

        if !progress.is_armed() {
            std::thread::sleep(WATCHDOG_POLL_INTERVAL);
            continue;
        }

        let snapshot = progress.snapshot(now);
        let operation = terminal_output_operation();
        let gate = output_lock.snapshot();
        let stage_expired =
            snapshot.stage != LivenessStage::Idle && snapshot.stage_elapsed >= deadline;
        let suspect_expired = snapshot
            .suspect_elapsed
            .is_some_and(|elapsed| elapsed >= deadline);
        // Owner output has its own shorter contract (currently 7 s). A CPR request's 500 ms write
        // budget must instead return through the ambiguity policy; only an indefinitely wedged
        // CPR reaches the independent 8 s worker deadline.
        let operation_expired = owner_output_operation_expired(operation, gate.holder_role);
        let owner_deadline_expired = gate.owner_deadline_expired;
        if stage_expired || suspect_expired || operation_expired || owner_deadline_expired {
            let stage = if let Some(operation) = operation.filter(|_| operation_expired) {
                output_operation_stage(operation)
            } else if owner_deadline_expired {
                operation.map_or(LivenessStage::OwnerRender, output_operation_stage)
            } else if operation.is_none()
                && gate.holder_role == Some(OutputRole::Owner)
                && (gate.liveness_waiters > 0
                    || snapshot.suspect_stage == LivenessStage::OutputGate)
            {
                LivenessStage::OwnerRender
            } else if stage_expired {
                snapshot.stage
            } else {
                snapshot.suspect_stage
            };
            let elapsed = if let Some(operation) = operation.filter(|_| operation_expired) {
                operation.elapsed
            } else if owner_deadline_expired {
                gate.held_for
            } else if stage_expired {
                snapshot.stage_elapsed
            } else {
                snapshot.suspect_elapsed.unwrap_or_default()
            };
            let operation_detail = operation.map_or_else(
                || "none".to_owned(),
                |value| {
                    format!(
                        "label={}, generation={}, elapsed_ms={}, phase={:?}, expired={}",
                        value.label,
                        value.generation,
                        value.elapsed.as_millis(),
                        value.phase,
                        value.expired,
                    )
                },
            );
            let detail = format!(
                "terminal worker made no progress before its hard deadline (last_alive_ms={}, holder_role={:?}, holder_generation={}, holder_held_ms={}, liveness_waiters={}, output_operation={})",
                snapshot.last_alive_elapsed.as_millis(),
                gate.holder_role,
                gate.holder_generation,
                gate.held_for.as_millis(),
                gate.liveness_waiters,
                operation_detail,
            );
            if let Some(hook) = &before_failure_arbitration {
                hook();
            }
            fail_runtime(
                &failure,
                &shutdown,
                &stop,
                TerminalFailure::runtime(
                    io::ErrorKind::TimedOut,
                    TerminalFailureClass::WorkerStall,
                    stage,
                    Some(EvidenceDomain::Transport),
                    elapsed,
                    detail,
                )
                .with_last_alive(snapshot.last_alive_elapsed),
                kill_media.as_ref(),
                Some(&output_lock),
            );
            return;
        }
        std::thread::sleep(WATCHDOG_POLL_INTERVAL);
    }
    cancel_terminal_output();
    output_lock.wake_all();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeDisposition {
    Complete,
    Retry,
}

fn evaluate_probe_outcomes(
    outcomes: impl IntoIterator<Item = ProbeOutcome>,
    suspicions: &mut SuspicionTracker,
    progress: &WatchdogProgress,
    startup: bool,
    now: Instant,
) -> Result<ProbeDisposition, TerminalFailure> {
    let mut retry = false;
    for outcome in outcomes {
        match outcome {
            ProbeOutcome::Alive(domain) => {
                suspicions.clear(domain);
                if domain == EvidenceDomain::Transport {
                    progress.alive();
                } else {
                    progress.idle();
                }
            }
            ProbeOutcome::PendingInput if startup => {
                progress.idle();
                retry = true;
            }
            ProbeOutcome::PendingInput | ProbeOutcome::RecentInput => {
                suspicions.clear(EvidenceDomain::Transport);
                progress.alive();
            }
            ProbeOutcome::OwnerOutputBusy => {
                // Gate contention is not terminal evidence. It neither confirms an ambiguous CPR
                // nor clears it; only an actual reply or real input can do that.
                progress.idle();
            }
            ProbeOutcome::NoEvidence => progress.idle(),
            ProbeOutcome::DefinitiveLoss {
                domain,
                stage,
                error,
            } => {
                return Err(make_probe_failure(
                    startup,
                    error.kind(),
                    TerminalFailureClass::DefinitiveLoss,
                    stage,
                    Some(domain),
                    Duration::ZERO,
                    error,
                )
                .with_last_alive(progress.snapshot(now).last_alive_elapsed));
            }
            ProbeOutcome::Ambiguous {
                domain,
                stage,
                evidence_id,
                error,
            } => {
                let observation = suspicions.observe(domain, evidence_id, now);
                if domain == EvidenceDomain::Transport {
                    progress.transport_suspect(stage);
                } else {
                    progress.idle();
                }
                if observation.confirmations >= AMBIGUOUS_CONFIRMATIONS {
                    return Err(make_probe_failure(
                        startup,
                        error.kind(),
                        TerminalFailureClass::AmbiguousExhausted,
                        stage,
                        Some(domain),
                        observation.elapsed,
                        format!(
                            "terminal liveness remained ambiguous after {} independent checks: {error}",
                            observation.confirmations
                        ),
                    )
                    .with_last_alive(progress.snapshot(now).last_alive_elapsed));
                }
                retry = true;
            }
            ProbeOutcome::InternalFatal {
                domain,
                stage,
                error,
            } => {
                return Err(make_probe_failure(
                    startup,
                    error.kind(),
                    TerminalFailureClass::InternalFatal,
                    stage,
                    domain,
                    Duration::ZERO,
                    error,
                )
                .with_last_alive(progress.snapshot(now).last_alive_elapsed));
            }
        }
    }
    Ok(if retry {
        ProbeDisposition::Retry
    } else {
        ProbeDisposition::Complete
    })
}

fn make_probe_failure(
    startup: bool,
    kind: io::ErrorKind,
    class: TerminalFailureClass,
    stage: LivenessStage,
    domain: Option<EvidenceDomain>,
    elapsed: Duration,
    detail: impl std::fmt::Display,
) -> TerminalFailure {
    if startup {
        TerminalFailure::startup(kind, class, stage, domain, elapsed, detail)
    } else {
        TerminalFailure::runtime(kind, class, stage, domain, elapsed, detail)
    }
}

fn initial_liveness_check(
    backend: &mut impl InputBackend,
    stop: &AtomicBool,
    shutdown: &ShutdownLatch,
    progress: &WatchdogProgress,
    started_at: Instant,
) -> Result<(), TerminalFailure> {
    let mut suspicions = SuspicionTracker::default();
    for action in [LivenessAction::OwnerProbe, LivenessAction::Heartbeat] {
        loop {
            if stop.load(Ordering::Acquire) || shutdown.is_triggered() {
                return Err(TerminalFailure::startup(
                    io::ErrorKind::BrokenPipe,
                    TerminalFailureClass::InternalFatal,
                    LivenessStage::Stopping,
                    None,
                    started_at.elapsed(),
                    "shutdown interrupted the terminal readiness check",
                ));
            }
            let outcomes = match action {
                LivenessAction::OwnerProbe => {
                    progress.enter(LivenessStage::OwnerProbe);
                    backend.check_owner_attached()
                }
                LivenessAction::Heartbeat => {
                    progress.enter(LivenessStage::CprWait);
                    vec![backend.heartbeat()]
                }
            };
            match evaluate_probe_outcomes(
                outcomes,
                &mut suspicions,
                progress,
                true,
                Instant::now(),
            )? {
                ProbeDisposition::Complete => break,
                ProbeDisposition::Retry => {
                    wait_retry_interruptibly(stop, shutdown);
                }
            }
        }
    }
    Ok(())
}

fn wait_retry_interruptibly(stop: &AtomicBool, shutdown: &ShutdownLatch) {
    let retry_deadline = Instant::now() + AMBIGUOUS_RETRY_INTERVAL;
    while Instant::now() < retry_deadline {
        if stop.load(Ordering::Acquire) || shutdown.is_triggered() {
            break;
        }
        std::thread::sleep(
            Duration::from_millis(10).min(retry_deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn is_transient_input_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

fn runtime_is_stopping(stop: &AtomicBool, shutdown: &ShutdownLatch) -> bool {
    stop.load(Ordering::Acquire) || shutdown.is_triggered()
}

fn fail_runtime(
    failure: &SharedFailure,
    shutdown: &ShutdownLatch,
    stop: &AtomicBool,
    terminal_failure: TerminalFailure,
    kill_media: &dyn Fn(),
    output_lock: Option<&TerminalOutputLock>,
) {
    // Ordinary teardown sets `stop` before cancelling output or waking the gate. Arbitration here
    // is intentionally adjacent to publication so those derived BrokenPipe/ConnectionAborted
    // results cannot become a false terminal primary.
    let competing_failure = terminal_failure.clone();
    if runtime_is_stopping(stop, shutdown) {
        if failure.has_primary() {
            failure.record_secondary(competing_failure);
        }
        return;
    }
    let message = terminal_failure.message.clone();
    let class = terminal_failure.class;
    let stage = terminal_failure.stage;
    let domain = terminal_failure.domain;
    let elapsed_ms = terminal_failure.elapsed_ms;
    let last_alive_ms = terminal_failure.last_alive_ms;
    let mut diagnostic_recorded = false;
    let won_arbitration = shutdown.try_trigger_with_before_publish(
        || {
            // The latch claim is not externally visible until this immutable cause is published.
            diagnostic_recorded = failure.record_primary(terminal_failure.clone());
            if !diagnostic_recorded {
                failure.record_secondary(terminal_failure);
            }
        },
        || {
            cancel_terminal_output();
            kill_media();
        },
    );
    if !won_arbitration {
        if failure.has_primary() {
            failure.record_secondary(competing_failure);
        }
        return;
    }
    if let Some(output_lock) = output_lock {
        output_lock.wake_all();
    }
    tracing::warn!(
        error = %message,
        failure_class = %class,
        phase = %stage,
        evidence_domain = domain.map(|value| value.to_string()),
        elapsed_ms,
        last_alive_ms,
        diagnostic_recorded,
        "terminal owner requested shutdown"
    );
}

fn record_background_failure(failure: &FailureStore, value: TerminalFailure) {
    if !failure.record_primary(value.clone()) {
        failure.record_secondary(value);
    }
}

fn signal_abnormal_worker_exit(
    label: &str,
    unwind: std::thread::Result<()>,
    stop: &AtomicBool,
    shutdown: &ShutdownLatch,
    failure: &SharedFailure,
    kill_media: &dyn Fn(),
    output_lock: &TerminalOutputLock,
) {
    if unwind.is_ok() && runtime_is_stopping(stop, shutdown) {
        // A worker that published the primary failure returns through its ordinary epilogue after
        // setting the shared stop/latch. That is expected completion, not secondary corruption.
        return;
    }
    let detail = if unwind.is_err() {
        format!("{label} panicked")
    } else {
        format!("{label} returned unexpectedly")
    };
    let worker_failure = TerminalFailure::runtime(
        io::ErrorKind::Other,
        TerminalFailureClass::InternalFatal,
        LivenessStage::Stopping,
        None,
        Duration::ZERO,
        detail,
    );
    if runtime_is_stopping(stop, shutdown) {
        if failure.has_primary() {
            failure.record_secondary(worker_failure);
        }
        return;
    }
    fail_runtime(
        failure,
        shutdown,
        stop,
        worker_failure,
        kill_media,
        Some(output_lock),
    );
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};

    use super::*;

    fn noop_kill_media() -> KillMedia {
        Arc::new(|| {})
    }

    struct FakeInput {
        operations: VecDeque<io::Result<Option<Event>>>,
        heartbeats: usize,
    }

    impl InputBackend for FakeInput {
        fn check_owner_attached(&mut self) -> Vec<ProbeOutcome> {
            vec![ProbeOutcome::NoEvidence]
        }

        fn heartbeat(&mut self) -> ProbeOutcome {
            self.heartbeats += 1;
            ProbeOutcome::Alive(EvidenceDomain::Transport)
        }

        fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
            match self.operations.front() {
                Some(Ok(event)) => Ok(event.is_some()),
                Some(Err(_)) => match self.operations.pop_front().expect("front exists") {
                    Err(error) => Err(error),
                    Ok(_) => unreachable!(),
                },
                None => Err(io::Error::new(io::ErrorKind::BrokenPipe, "fixture done")),
            }
        }

        fn read(&mut self) -> io::Result<Event> {
            self.operations
                .pop_front()
                .expect("poll promised an event")?
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "fixture EOF"))
        }
    }

    #[test]
    fn exclusive_reader_preserves_event_order_before_fatal_eof() {
        let first = Event::Key(KeyEvent::from(KeyCode::Char('a')));
        let second = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 7,
            row: 9,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        let backend = FakeInput {
            operations: VecDeque::from([
                Ok(Some(first.clone())),
                Ok(Some(second.clone())),
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal closed",
                )),
            ]),
            heartbeats: 0,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = ShutdownLatch::new();
        let failure = Arc::new(FailureStore::default());
        let progress = Arc::new(WatchdogProgress::default());

        run_input_loop(
            backend,
            tx,
            ready_tx,
            InputLoopControl {
                stop,
                shutdown: shutdown.clone(),
                failure: Arc::clone(&failure),
                progress,
                kill_media: noop_kill_media(),
                output_lock: TerminalOutputLock::new(shutdown.clone()),
                heartbeat_interval: HEARTBEAT_INTERVAL,
                owner_probe_interval: OWNER_PROBE_INTERVAL,
            },
        );

        assert_eq!(ready_rx.recv().unwrap(), Ok(()));
        assert_eq!(rx.try_recv().unwrap(), first);
        assert_eq!(rx.try_recv().unwrap(), second);
        assert!(rx.try_recv().is_err());
        assert!(shutdown.is_triggered());
        assert_eq!(
            failure.primary_class(),
            Some(TerminalFailureClass::DefinitiveLoss)
        );
    }

    #[test]
    fn saturated_input_queue_fails_closed_without_dropping_the_admitted_event() {
        let first = Event::Key(KeyEvent::from(KeyCode::Char('a')));
        let second = Event::Key(KeyEvent::from(KeyCode::Char('b')));
        let backend = FakeInput {
            operations: VecDeque::from([Ok(Some(first.clone())), Ok(Some(second))]),
            heartbeats: 0,
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let shutdown = ShutdownLatch::new();
        let failure = Arc::new(FailureStore::default());
        let progress = Arc::new(WatchdogProgress::default());

        run_input_loop(
            backend,
            tx,
            ready_tx,
            InputLoopControl {
                stop: Arc::new(AtomicBool::new(false)),
                shutdown: shutdown.clone(),
                failure: Arc::clone(&failure),
                progress,
                kill_media: noop_kill_media(),
                output_lock: TerminalOutputLock::new(shutdown.clone()),
                heartbeat_interval: HEARTBEAT_INTERVAL,
                owner_probe_interval: OWNER_PROBE_INTERVAL,
            },
        );

        assert_eq!(ready_rx.recv().unwrap(), Ok(()));
        assert_eq!(rx.try_recv().unwrap(), first);
        assert!(shutdown.is_triggered());
        assert!(
            failure
                .error_snapshot()
                .unwrap()
                .to_string()
                .contains("queue saturated")
        );
    }

    #[test]
    fn production_liveness_constants_match_the_approved_policy() {
        assert_eq!(HEARTBEAT_INTERVAL, Duration::from_millis(500));
        assert_eq!(OWNER_PROBE_INTERVAL, Duration::from_millis(2500));
        assert_eq!(OWNER_PROBE_TIMEOUT, Duration::from_millis(500));
        assert_eq!(AMBIGUOUS_RETRY_INTERVAL, Duration::from_millis(250));
        assert_eq!(RUNTIME_LIVENESS_DEADLINE, Duration::from_secs(8));
        assert_eq!(WATCHDOG_POLL_INTERVAL, Duration::from_millis(50));
        assert_eq!(STARTUP_READY_TIMEOUT, Duration::from_millis(8250));
        assert_eq!(SHUTDOWN_JOIN_TIMEOUT, Duration::from_millis(2500));
        assert_eq!(WEDGED_JOIN_TIMEOUT, Duration::from_millis(100));
        #[cfg(unix)]
        assert!(
            HEARTBEAT_INTERVAL
                + OWNER_PROBE_TIMEOUT
                + Duration::from_secs(2)
                + WATCHDOG_POLL_INTERVAL
                < RUNTIME_LIVENESS_DEADLINE,
            "a documented worst-case heartbeat must finish with watchdog scheduling margin"
        );
        assert_eq!(
            output_gate::LIVENESS_ACQUIRE_TIMEOUT,
            Duration::from_secs(1)
        );
    }

    #[test]
    fn cpr_and_owner_probe_have_independent_cadences() {
        let start = Instant::now();
        let heartbeat_interval = Duration::from_millis(500);
        let owner_probe_interval = Duration::from_millis(2500);
        let mut schedule = LivenessSchedule::new(start, heartbeat_interval, owner_probe_interval);

        assert_eq!(schedule.due(start + Duration::from_millis(499)), None);
        for elapsed_ms in [500, 1000, 1500, 2000] {
            let now = start + Duration::from_millis(elapsed_ms);
            assert_eq!(schedule.due(now), Some(LivenessAction::Heartbeat));
            schedule.completed(LivenessAction::Heartbeat, now);
        }

        let shared_deadline = start + Duration::from_millis(2500);
        assert_eq!(
            schedule.due(shared_deadline),
            Some(LivenessAction::OwnerProbe),
            "the heavier owner query runs only at its independent 2.5-second deadline"
        );
        schedule.completed(LivenessAction::OwnerProbe, shared_deadline);
        assert_eq!(
            schedule.due(shared_deadline),
            Some(LivenessAction::Heartbeat),
            "an owner query must not postpone a CPR which is already due"
        );
    }

    #[test]
    fn runtime_ambiguity_survives_once_and_requires_distinct_confirmation() {
        let progress = WatchdogProgress::default();
        let mut suspicions = SuspicionTracker::default();
        let start = Instant::now();
        let outcome = |evidence_id| {
            ProbeOutcome::transport(
                io::Error::new(io::ErrorKind::TimedOut, "CPR timed out"),
                evidence_id,
                LivenessStage::CprWait,
            )
        };

        assert_eq!(
            evaluate_probe_outcomes([outcome(1)], &mut suspicions, &progress, false, start,)
                .unwrap(),
            ProbeDisposition::Retry
        );
        assert!(
            evaluate_probe_outcomes(
                [outcome(1)],
                &mut suspicions,
                &progress,
                false,
                start + Duration::from_secs(1),
            )
            .is_ok()
        );
        let failure = evaluate_probe_outcomes(
            [outcome(2)],
            &mut suspicions,
            &progress,
            false,
            start + Duration::from_secs(2),
        )
        .unwrap_err();
        assert_eq!(failure.class, TerminalFailureClass::AmbiguousExhausted);
    }

    #[test]
    fn pending_and_recent_input_have_distinct_startup_semantics() {
        let progress = WatchdogProgress::default();
        let mut startup = SuspicionTracker::default();
        assert_eq!(
            evaluate_probe_outcomes(
                [ProbeOutcome::PendingInput],
                &mut startup,
                &progress,
                true,
                Instant::now(),
            )
            .unwrap(),
            ProbeDisposition::Retry
        );
        assert_eq!(
            evaluate_probe_outcomes(
                [ProbeOutcome::RecentInput],
                &mut startup,
                &progress,
                true,
                Instant::now(),
            )
            .unwrap(),
            ProbeDisposition::Complete,
            "an event queued before the first CPR is conclusive startup liveness evidence"
        );
        let mut runtime = SuspicionTracker::default();
        assert_eq!(
            evaluate_probe_outcomes(
                [ProbeOutcome::PendingInput],
                &mut runtime,
                &progress,
                false,
                Instant::now(),
            )
            .unwrap(),
            ProbeDisposition::Complete
        );

        let timeout = |evidence_id| {
            ProbeOutcome::transport(
                io::Error::new(io::ErrorKind::TimedOut, "CPR timed out"),
                evidence_id,
                LivenessStage::CprWait,
            )
        };
        assert_eq!(
            evaluate_probe_outcomes([timeout(1)], &mut runtime, &progress, false, Instant::now(),)
                .unwrap(),
            ProbeDisposition::Retry
        );
        assert_eq!(
            evaluate_probe_outcomes(
                [ProbeOutcome::RecentInput],
                &mut runtime,
                &progress,
                false,
                Instant::now(),
            )
            .unwrap(),
            ProbeDisposition::Complete
        );
        assert_eq!(
            evaluate_probe_outcomes([timeout(2)], &mut runtime, &progress, false, Instant::now(),)
                .expect("recent input must clear the earlier transport suspicion"),
            ProbeDisposition::Retry
        );
    }

    #[test]
    fn failure_is_stored_before_emergency_latch_and_media_kill() {
        let shutdown = ShutdownLatch::new();
        let stop = AtomicBool::new(false);
        let failure = Arc::new(FailureStore::default());
        let kill_observed_ready_cause = Arc::new(AtomicBool::new(false));
        let kill_observation = Arc::clone(&kill_observed_ready_cause);
        let kill_shutdown = shutdown.clone();
        let kill_failure = Arc::clone(&failure);
        fail_runtime(
            &failure,
            &shutdown,
            &stop,
            TerminalFailure::runtime(
                io::ErrorKind::BrokenPipe,
                TerminalFailureClass::DefinitiveLoss,
                LivenessStage::InputRead,
                Some(EvidenceDomain::Transport),
                Duration::ZERO,
                "fixture terminal detached",
            ),
            &move || {
                kill_observation.store(
                    kill_shutdown.is_triggered() && kill_failure.error_snapshot().is_some(),
                    Ordering::Release,
                );
            },
            None,
        );
        assert!(shutdown.is_triggered());
        assert!(kill_observed_ready_cause.load(Ordering::Acquire));
        let first = failure.error_snapshot().unwrap().to_string();
        assert!(first.contains("fixture terminal detached"));
        assert_eq!(
            failure.primary_class(),
            Some(TerminalFailureClass::DefinitiveLoss)
        );
    }

    #[test]
    fn panic_is_recorded_before_reporting_channels_can_disconnect() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(1);
        let event_channel_guard = tx.clone();
        drop(tx);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let ready_channel_guard = ready_tx.clone();
        drop(ready_tx);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            ready_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        let stop = AtomicBool::new(false);
        let shutdown = ShutdownLatch::new();
        let failure = Arc::new(FailureStore::default());
        let output = TerminalOutputLock::new(shutdown.clone());
        let panic: std::thread::Result<()> = Err(Box::new("fixture panic"));
        signal_abnormal_worker_exit(
            "terminal input worker",
            panic,
            &stop,
            &shutdown,
            &failure,
            &|| {},
            &output,
        );

        assert!(shutdown.is_triggered());
        assert!(
            failure
                .error_snapshot()
                .unwrap()
                .to_string()
                .contains("terminal input worker panicked")
        );
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            ready_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        drop(ready_channel_guard);
        drop(event_channel_guard);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(matches!(
            ready_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn concurrent_worker_failure_enriches_an_existing_terminal_primary() {
        let stop = AtomicBool::new(false);
        let shutdown = ShutdownLatch::new();
        let failure = Arc::new(FailureStore::default());
        assert!(failure.record_primary(TerminalFailure::runtime(
            io::ErrorKind::BrokenPipe,
            TerminalFailureClass::DefinitiveLoss,
            LivenessStage::CprWait,
            Some(EvidenceDomain::Transport),
            Duration::ZERO,
            "authoritative terminal failure",
        )));
        shutdown.trigger();
        signal_abnormal_worker_exit(
            "terminal watchdog",
            Err(Box::new("concurrent panic")),
            &stop,
            &shutdown,
            &failure,
            &|| panic!("an existing shutdown must not rerun emergency teardown"),
            &TerminalOutputLock::new(shutdown.clone()),
        );

        let snapshot = failure.error_snapshot().unwrap().to_string();
        assert!(snapshot.contains("authoritative terminal failure"));
        assert!(snapshot.contains("terminal watchdog panicked"));
    }

    #[test]
    fn concurrent_probe_failure_enriches_primary_without_repeating_emergency() {
        let stop = AtomicBool::new(false);
        let shutdown = ShutdownLatch::new();
        let failure = Arc::new(FailureStore::default());
        assert!(failure.record_primary(TerminalFailure::runtime(
            io::ErrorKind::BrokenPipe,
            TerminalFailureClass::DefinitiveLoss,
            LivenessStage::CprWait,
            Some(EvidenceDomain::Transport),
            Duration::ZERO,
            "authoritative terminal failure",
        )));
        shutdown.trigger();

        fail_runtime(
            &failure,
            &shutdown,
            &stop,
            TerminalFailure::runtime(
                io::ErrorKind::ConnectionAborted,
                TerminalFailureClass::AmbiguousExhausted,
                LivenessStage::OwnerProbe,
                Some(EvidenceDomain::OwnerEnvironment),
                Duration::ZERO,
                "concurrent owner probe failure",
            ),
            &|| panic!("an existing shutdown must not rerun emergency teardown"),
            None,
        );

        let snapshot = failure.error_snapshot().unwrap().to_string();
        assert!(snapshot.contains("authoritative terminal failure"));
        assert!(snapshot.contains("concurrent owner probe failure"));
    }
}
