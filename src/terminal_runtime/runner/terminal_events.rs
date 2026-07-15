//! Exclusive terminal input ownership and terminal-client liveness.
//!
//! Crossterm cursor-position queries consume their replies from the same global input source as
//! ordinary events. Keeping both operations on this one thread prevents the heartbeat from
//! stealing keys (or the event loop from stealing CPR replies). The owner receives events over a
//! bounded channel. Saturation is terminal-owner failure: the reader kills media rather than
//! dropping input or blocking the liveness deadline behind an unresponsive reducer.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::Event;

use crate::player::lifetime::ShutdownLatch;

#[cfg(unix)]
mod owner_probe;
#[cfg(unix)]
#[cfg(test)]
use owner_probe::OWNER_PROBE_TIMEOUT;
#[cfg(unix)]
use owner_probe::TerminalOwnerProbe;

const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
// Multiplexer CLIs are materially heavier than CPR and need not run for every input heartbeat.
// A detach immediately after a successful query is still detected within 2.5 s plus the bounded
// 500 ms query, leaving teardown margin below the four-second acceptance bound.
const OWNER_PROBE_INTERVAL: Duration = Duration::from_millis(2500);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STARTUP_READY_TIMEOUT: Duration = Duration::from_secs(4);
/// A CPR may spend two seconds waiting after its half-second idle interval. When its deadline
/// coincides with a multiplexer probe, that probe may add 500 ms first, but its completion also
/// advances watchdog progress. Three-and-a-half seconds retains scheduler/watchdog margin below
/// the four-second detach acceptance bound. This watchdog is independent of the input worker, so
/// it also covers a syscall/library call which never returns.
const RUNTIME_LIVENESS_DEADLINE: Duration = Duration::from_millis(3500);
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(50);
// An owner frame may begin waiting just after liveness starts a normal two-second CPR. Keep this
// larger than the CPR bound; the independent 3.5-second watchdog remains the hard upper bound when
// either side actually wedges while owning the gate.
const OUTPUT_LOCK_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(4);
const STARTUP_JOIN_GRACE: Duration = Duration::from_millis(100);
type KillMedia = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalFailure {
    kind: io::ErrorKind,
    message: String,
}

impl TerminalFailure {
    fn new(kind: io::ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn into_error(self) -> io::Error {
        io::Error::new(self.kind, self.message)
    }
}

impl From<io::Error> for TerminalFailure {
    fn from(error: io::Error) -> Self {
        Self::new(error.kind(), error.to_string())
    }
}

type SharedFailure = Arc<Mutex<Option<TerminalFailure>>>;

/// The async side of the exclusive terminal input source.
pub(super) struct TerminalEventReceiver {
    events: tokio::sync::mpsc::Receiver<Event>,
    failure: SharedFailure,
}

/// Serialises CPR writes with complete terminal frames/OSC messages.
#[derive(Clone)]
pub(super) struct TerminalOutputLock {
    gate: Arc<OutputGate>,
    shutdown: ShutdownLatch,
    failure: SharedFailure,
    kill_media: KillMedia,
    acquire_timeout: Duration,
}

impl TerminalOutputLock {
    pub(super) fn run<T>(&self, operation: impl FnOnce() -> T) -> io::Result<T> {
        self.run_with_role(OutputRole::Owner, operation)
    }

    pub(super) fn run_io<T>(&self, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        self.run(operation)?
    }

    fn run_liveness_io<T>(&self, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        self.run_with_role(OutputRole::Liveness, operation)?
    }

    fn run_with_role<T>(&self, role: OutputRole, operation: impl FnOnce() -> T) -> io::Result<T> {
        let _permit = match self.acquire(role) {
            Ok(permit) => permit,
            Err(error) => {
                let returned = io::Error::new(error.kind(), error.to_string());
                if !self.shutdown.is_triggered() {
                    fail_runtime(
                        &self.failure,
                        &self.shutdown,
                        error,
                        self.kill_media.as_ref(),
                    );
                }
                return Err(returned);
            }
        };
        Ok(operation())
    }

    fn acquire(&self, role: OutputRole) -> io::Result<OutputPermit> {
        let deadline = Instant::now() + self.acquire_timeout;
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(role, OutputRole::Liveness) {
            state.liveness_waiters = state.liveness_waiters.saturating_add(1);
        }

        loop {
            if self.shutdown.is_triggered() {
                if matches!(role, OutputRole::Liveness) {
                    state.liveness_waiters = state.liveness_waiters.saturating_sub(1);
                    self.gate.changed.notify_all();
                }
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "terminal output ownership ended during shutdown",
                ));
            }
            let liveness_has_priority =
                matches!(role, OutputRole::Owner) && state.liveness_waiters > 0;
            if !state.held && !liveness_has_priority {
                state.held = true;
                if matches!(role, OutputRole::Liveness) {
                    state.liveness_waiters = state.liveness_waiters.saturating_sub(1);
                }
                return Ok(OutputPermit {
                    gate: Arc::clone(&self.gate),
                });
            }

            let now = Instant::now();
            if now >= deadline {
                if matches!(role, OutputRole::Liveness) {
                    state.liveness_waiters = state.liveness_waiters.saturating_sub(1);
                    self.gate.changed.notify_all();
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "terminal output ownership could not be acquired before its deadline",
                ));
            }
            let waited = self.gate.changed.wait_timeout(state, deadline - now);
            let (next, _) = waited.unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }
}

#[derive(Clone, Copy)]
enum OutputRole {
    Owner,
    Liveness,
}

#[derive(Default)]
struct OutputGateState {
    held: bool,
    liveness_waiters: usize,
}

#[derive(Default)]
struct OutputGate {
    state: Mutex<OutputGateState>,
    changed: Condvar,
}

struct OutputPermit {
    gate: Arc<OutputGate>,
}

impl Drop for OutputPermit {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.held = false;
        self.gate.changed.notify_all();
    }
}

#[derive(Default)]
struct WatchdogProgress {
    completed_checks: AtomicU64,
    armed: AtomicBool,
}

impl WatchdogProgress {
    fn completed(&self) {
        self.completed_checks.fetch_add(1, Ordering::Release);
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }
}

impl TerminalEventReceiver {
    pub(super) async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// Return the first terminal failure without letting a poisoned diagnostic mutex suppress
    /// owner teardown.
    pub(super) fn take_failure(&self) -> Option<io::Error> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .map(TerminalFailure::into_error)
    }
}

/// Join ownership for the blocking terminal reader.
pub(super) struct TerminalEventWorker {
    stop: Arc<AtomicBool>,
    input_thread: Option<JoinHandle<()>>,
    watchdog_thread: Option<JoinHandle<()>>,
    failure: SharedFailure,
}

impl TerminalEventWorker {
    pub(super) async fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        let deadline = Instant::now() + SHUTDOWN_JOIN_TIMEOUT;
        join_threads_until(
            &mut self.input_thread,
            &mut self.watchdog_thread,
            deadline,
            &self.failure,
        )
        .await;
    }
}

impl Drop for TerminalEventWorker {
    fn drop(&mut self) {
        // The normal teardown path joins the thread. The atomic is the non-blocking fallback for
        // panic/cancellation; process teardown remains the ultimate boundary in that path.
        self.stop.store(true, Ordering::Release);
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
            record_failure(
                failure,
                TerminalFailure::new(
                    io::ErrorKind::TimedOut,
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
        record_failure(
            failure,
            TerminalFailure::new(io::ErrorKind::Other, format!("{label} panicked")),
        );
    }
}

fn stop_and_join_startup_threads(
    stop: &AtomicBool,
    input: Option<JoinHandle<()>>,
    watchdog: Option<JoinHandle<()>>,
) {
    stop.store(true, Ordering::Release);
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
    let failure = Arc::new(Mutex::new(None));
    let progress = Arc::new(WatchdogProgress::default());
    let kill_media: KillMedia = Arc::new(crate::player::lifetime::kill_mpv_now);
    let output_lock = TerminalOutputLock {
        gate: Arc::new(OutputGate::default()),
        shutdown: shutdown.clone(),
        failure: Arc::clone(&failure),
        kill_media: Arc::clone(&kill_media),
        acquire_timeout: OUTPUT_LOCK_TIMEOUT,
    };

    let watchdog_stop = Arc::clone(&stop);
    let watchdog_progress = Arc::clone(&progress);
    let watchdog_shutdown = shutdown.clone();
    let watchdog_failure = Arc::clone(&failure);
    let watchdog_kill_media = Arc::clone(&kill_media);
    let watchdog_thread = std::thread::Builder::new()
        .name("ytt-terminal-watchdog".to_owned())
        .spawn(move || {
            run_watchdog(
                watchdog_progress,
                watchdog_stop,
                watchdog_shutdown,
                watchdog_failure,
                watchdog_kill_media,
                RUNTIME_LIVENESS_DEADLINE,
            );
        })?;

    let thread_stop = Arc::clone(&stop);
    let thread_failure = Arc::clone(&failure);
    let thread_output_lock = output_lock.clone();
    let thread_progress = Arc::clone(&progress);
    let thread_kill_media = Arc::clone(&kill_media);
    let input_thread = match std::thread::Builder::new()
        .name("ytt-terminal-input".to_owned())
        .spawn(move || {
            let backend = CrosstermInput::detect(thread_output_lock);
            run_input_loop(
                backend,
                event_tx,
                ready_tx,
                InputLoopControl {
                    stop: thread_stop,
                    shutdown,
                    failure: thread_failure,
                    progress: thread_progress,
                    kill_media: thread_kill_media,
                    heartbeat_interval: HEARTBEAT_INTERVAL,
                    owner_probe_interval: OWNER_PROBE_INTERVAL,
                },
            );
        }) {
        Ok(thread) => thread,
        Err(error) => {
            stop_and_join_startup_threads(&stop, None, Some(watchdog_thread));
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
                    input_thread: Some(input_thread),
                    watchdog_thread: Some(watchdog_thread),
                    failure,
                },
                output_lock,
            ))
        }
        Ok(Err(error)) => {
            stop_and_join_startup_threads(&stop, Some(input_thread), Some(watchdog_thread));
            Err(error.into_error())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            stop_and_join_startup_threads(&stop, Some(input_thread), Some(watchdog_thread));
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal liveness startup check exceeded four seconds; use `ytt daemon` for background playback",
            ))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            stop_and_join_startup_threads(&stop, Some(input_thread), Some(watchdog_thread));
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal input worker stopped before reporting readiness",
            ))
        }
    }
}

trait InputBackend {
    fn check_owner_attached(&mut self) -> io::Result<()>;
    fn heartbeat(&mut self) -> io::Result<()>;
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

struct InputLoopControl {
    stop: Arc<AtomicBool>,
    shutdown: ShutdownLatch,
    failure: SharedFailure,
    progress: Arc<WatchdogProgress>,
    kill_media: KillMedia,
    heartbeat_interval: Duration,
    owner_probe_interval: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LivenessAction {
    Heartbeat,
    OwnerProbe,
}

struct LivenessSchedule {
    next_heartbeat: Instant,
    next_owner_probe: Instant,
    heartbeat_interval: Duration,
    owner_probe_interval: Duration,
}

impl LivenessSchedule {
    fn new(now: Instant, heartbeat_interval: Duration, owner_probe_interval: Duration) -> Self {
        Self {
            next_heartbeat: now + heartbeat_interval,
            next_owner_probe: now + owner_probe_interval,
            heartbeat_interval,
            owner_probe_interval,
        }
    }

    fn due(&self, now: Instant) -> Option<LivenessAction> {
        let heartbeat_due = now >= self.next_heartbeat;
        let owner_probe_due = now >= self.next_owner_probe;
        match (heartbeat_due, owner_probe_due) {
            (false, false) => None,
            (true, false) => Some(LivenessAction::Heartbeat),
            (false, true) => Some(LivenessAction::OwnerProbe),
            (true, true) if self.next_owner_probe <= self.next_heartbeat => {
                Some(LivenessAction::OwnerProbe)
            }
            (true, true) => Some(LivenessAction::Heartbeat),
        }
    }

    fn completed(&mut self, action: LivenessAction, now: Instant) {
        match action {
            LivenessAction::Heartbeat => self.next_heartbeat = now + self.heartbeat_interval,
            LivenessAction::OwnerProbe => {
                self.next_owner_probe = now + self.owner_probe_interval;
            }
        }
    }

    fn until_next(&self, now: Instant) -> Duration {
        self.next_heartbeat
            .min(self.next_owner_probe)
            .saturating_duration_since(now)
    }
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
        heartbeat_interval,
        owner_probe_interval,
    } = control;
    if let Err(error) = initial_liveness_check(&mut backend) {
        let failure_value = TerminalFailure::from(error);
        record_failure(&failure, failure_value.clone());
        let _ = ready.send(Err(failure_value));
        shutdown.trigger();
        return;
    }
    progress.completed();
    if ready.send(Ok(())).is_err() {
        stop.store(true, Ordering::Release);
        return;
    }

    let mut liveness =
        LivenessSchedule::new(Instant::now(), heartbeat_interval, owner_probe_interval);
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if let Some(action) = liveness.due(now) {
            let result = match action {
                LivenessAction::Heartbeat => backend.heartbeat(),
                LivenessAction::OwnerProbe => backend.check_owner_attached(),
            };
            match result {
                Ok(()) => {
                    progress.completed();
                    liveness.completed(action, Instant::now());
                }
                Err(error) => {
                    fail_runtime(&failure, &shutdown, error, kill_media.as_ref());
                    return;
                }
            }
            continue;
        }

        let timeout = liveness.until_next(now).min(STOP_POLL_INTERVAL);
        match backend.poll(timeout) {
            Ok(false) => {}
            Ok(true) => match backend.read() {
                Ok(event) => match events.try_send(event) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        fail_runtime(
                            &failure,
                            &shutdown,
                            io::Error::other(
                                "terminal input queue saturated; owner is unresponsive",
                            ),
                            kill_media.as_ref(),
                        );
                        return;
                    }
                },
                Err(error) if is_transient_input_error(&error) => {}
                Err(error) => {
                    fail_runtime(&failure, &shutdown, error, kill_media.as_ref());
                    return;
                }
            },
            Err(error) if is_transient_input_error(&error) => {}
            Err(error) => {
                fail_runtime(&failure, &shutdown, error, kill_media.as_ref());
                return;
            }
        }
    }
}

fn run_watchdog(
    progress: Arc<WatchdogProgress>,
    stop: Arc<AtomicBool>,
    shutdown: ShutdownLatch,
    failure: SharedFailure,
    kill_media: KillMedia,
    deadline: Duration,
) {
    let mut observed = progress.completed_checks.load(Ordering::Acquire);
    let mut expires_at = Instant::now() + deadline;
    while !stop.load(Ordering::Acquire) && !shutdown.is_triggered() {
        if !progress.armed.load(Ordering::Acquire) {
            std::thread::sleep(WATCHDOG_POLL_INTERVAL);
            observed = progress.completed_checks.load(Ordering::Acquire);
            expires_at = Instant::now() + deadline;
            continue;
        }

        let completed = progress.completed_checks.load(Ordering::Acquire);
        if completed != observed {
            observed = completed;
            expires_at = Instant::now() + deadline;
        } else if Instant::now() >= expires_at {
            fail_runtime(
                &failure,
                &shutdown,
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "terminal liveness worker stopped completing checks before its watchdog deadline",
                ),
                kill_media.as_ref(),
            );
            return;
        }
        std::thread::sleep(WATCHDOG_POLL_INTERVAL);
    }
}

fn initial_liveness_check(backend: &mut impl InputBackend) -> io::Result<()> {
    liveness_check(backend).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("terminal client liveness could not be established: {error}"),
        )
    })
}

fn liveness_check(backend: &mut impl InputBackend) -> io::Result<()> {
    backend.check_owner_attached()?;
    backend.heartbeat()
}

fn is_transient_input_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

fn fail_runtime(
    failure: &SharedFailure,
    shutdown: &ShutdownLatch,
    error: io::Error,
    kill_media: &dyn Fn(),
) {
    let mut failure_value = None;
    let mut diagnostic_recorded = false;
    // Set the monotonic latch before the global lifetime backstop, then kill media before touching
    // diagnostic locks, formatting, or tracing. The wake is emitted after this closure: in the
    // uncontended case the owner therefore still sees the exact cause, while a thread suspended
    // with the diagnostic mutex can never hold media teardown past the watchdog deadline.
    shutdown.trigger_with_emergency(|| {
        kill_media();
        let value = TerminalFailure::new(
            error.kind(),
            format!("terminal client liveness was lost: {error}"),
        );
        diagnostic_recorded = try_record_failure(failure, value.clone());
        failure_value = Some(value);
    });
    if let Some(failure_value) = failure_value {
        tracing::warn!(
            error = %failure_value.message,
            diagnostic_recorded,
            "terminal owner requested shutdown"
        );
    }
}

fn try_record_failure(failure: &SharedFailure, value: TerminalFailure) -> bool {
    let mut slot = match failure.try_lock() {
        Ok(slot) => slot,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return false,
    };
    if slot.is_none() {
        *slot = Some(value);
    }
    true
}

fn record_failure(failure: &SharedFailure, value: TerminalFailure) {
    let mut slot = failure
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(value);
    }
}

struct CrosstermInput {
    output_lock: TerminalOutputLock,
    #[cfg(unix)]
    owner: TerminalOwnerProbe,
}

impl CrosstermInput {
    fn detect(output_lock: TerminalOutputLock) -> Self {
        Self {
            output_lock,
            #[cfg(unix)]
            owner: TerminalOwnerProbe::detect_with(|key| std::env::var_os(key)),
        }
    }
}

impl InputBackend for CrosstermInput {
    fn check_owner_attached(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        self.owner.check_attached()?;
        Ok(())
    }

    fn heartbeat(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            // `cursor::position` writes DSR/CPR and keeps non-CPR input in crossterm's internal
            // queue. Its normal Unix no-response path is bounded at two seconds. Crossterm 0.29's
            // Unix implementation, however, deliberately retries `poll_internal` errors inside an
            // unbounded loop (`cursor/sys/unix.rs`, `Err(_) => {}`). PTY EOF/EIO can therefore pin
            // this call forever. The separate watchdog must remain independent of this worker; the
            // output gate's own deadline additionally prevents the owner from waiting behind it.
            self.output_lock
                .run_liveness_io(crossterm::cursor::position)?;
        }
        #[cfg(not(unix))]
        {
            // Windows console-close delivery is owned by the synchronous control handlers. Still
            // cross the output gate once per check: if an owner render/write is permanently stuck,
            // this bounded acquisition fails closed instead of letting guardian heartbeats run.
            self.output_lock.run_liveness_io(|| Ok(()))?;
        }
        Ok(())
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        crossterm::event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        crossterm::event::read()
    }
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
        fn check_owner_attached(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn heartbeat(&mut self) -> io::Result<()> {
            self.heartbeats += 1;
            Ok(())
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
        let failure = Arc::new(Mutex::new(None));
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
            failure.lock().unwrap().as_ref().unwrap().kind,
            io::ErrorKind::UnexpectedEof
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
        let failure = Arc::new(Mutex::new(None));
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
                heartbeat_interval: HEARTBEAT_INTERVAL,
                owner_probe_interval: OWNER_PROBE_INTERVAL,
            },
        );

        assert_eq!(ready_rx.recv().unwrap(), Ok(()));
        assert_eq!(rx.try_recv().unwrap(), first);
        assert!(shutdown.is_triggered());
        assert!(
            failure
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .message
                .contains("queue saturated")
        );
    }

    struct BlockingHeartbeat {
        calls: usize,
        entered: Option<std::sync::mpsc::SyncSender<()>>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl InputBackend for BlockingHeartbeat {
        fn check_owner_attached(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn heartbeat(&mut self) -> io::Result<()> {
            self.calls += 1;
            if self.calls == 1 {
                return Ok(());
            }
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            let (lock, changed) = &*self.release;
            let mut released = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = changed
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            Ok(())
        }

        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            std::thread::sleep(timeout);
            Ok(false)
        }

        fn read(&mut self) -> io::Result<Event> {
            unreachable!("blocking fixture never reports an event")
        }
    }

    #[test]
    fn independent_watchdog_kills_media_when_backend_call_never_returns() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = ShutdownLatch::new();
        let failure = Arc::new(Mutex::new(None));
        let progress = Arc::new(WatchdogProgress::default());
        let killed_after_latch = Arc::new(AtomicBool::new(false));
        let kill_observation = Arc::clone(&killed_after_latch);
        let kill_shutdown = shutdown.clone();
        let kill_media: KillMedia = Arc::new(move || {
            kill_observation.store(kill_shutdown.is_triggered(), Ordering::Release);
        });

        let input_stop = Arc::clone(&stop);
        let input_shutdown = shutdown.clone();
        let input_failure = Arc::clone(&failure);
        let input_progress = Arc::clone(&progress);
        let input_kill = Arc::clone(&kill_media);
        let input_release = Arc::clone(&release);
        let input = std::thread::spawn(move || {
            run_input_loop(
                BlockingHeartbeat {
                    calls: 0,
                    entered: Some(entered_tx),
                    release: input_release,
                },
                tx,
                ready_tx,
                InputLoopControl {
                    stop: input_stop,
                    shutdown: input_shutdown,
                    failure: input_failure,
                    progress: input_progress,
                    kill_media: input_kill,
                    heartbeat_interval: Duration::from_millis(5),
                    owner_probe_interval: Duration::from_secs(1),
                },
            );
        });
        assert_eq!(ready_rx.recv().unwrap(), Ok(()));
        let armed_at = Instant::now();
        progress.arm();

        let watchdog_progress = Arc::clone(&progress);
        let watchdog_stop = Arc::clone(&stop);
        let watchdog_shutdown = shutdown.clone();
        let watchdog_failure = Arc::clone(&failure);
        let watchdog_kill = Arc::clone(&kill_media);
        let watchdog = std::thread::spawn(move || {
            run_watchdog(
                watchdog_progress,
                watchdog_stop,
                watchdog_shutdown,
                watchdog_failure,
                watchdog_kill,
                Duration::from_millis(100),
            );
        });

        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("fixture must enter its permanently blocked heartbeat");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !shutdown.is_triggered() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(shutdown.is_triggered());
        assert!(
            armed_at.elapsed() < Duration::from_millis(500),
            "scaled watchdog exceeded its bounded detection window"
        );
        assert!(killed_after_latch.load(Ordering::Acquire));
        assert!(
            failure
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .message
                .contains("watchdog deadline")
        );

        stop.store(true, Ordering::Release);
        let (released, changed) = &*release;
        *released.lock().unwrap() = true;
        changed.notify_all();
        input.join().unwrap();
        watchdog.join().unwrap();
    }

    #[test]
    fn production_watchdog_leaves_kill_margin_below_four_seconds() {
        assert!(
            RUNTIME_LIVENESS_DEADLINE + WATCHDOG_POLL_INTERVAL < Duration::from_secs(4),
            "watchdog configuration must detect a blocked terminal before acceptance deadline"
        );
        #[cfg(unix)]
        assert!(
            HEARTBEAT_INTERVAL
                + OWNER_PROBE_TIMEOUT
                + Duration::from_secs(2)
                + WATCHDOG_POLL_INTERVAL
                < RUNTIME_LIVENESS_DEADLINE,
            "a documented worst-case heartbeat must finish with watchdog scheduling margin"
        );
        #[cfg(unix)]
        assert!(
            OWNER_PROBE_INTERVAL + OWNER_PROBE_TIMEOUT + WATCHDOG_POLL_INTERVAL
                < Duration::from_secs(4),
            "a multiplexer detach just after a successful query must fail closed within four seconds"
        );
        assert!(OUTPUT_LOCK_TIMEOUT > Duration::from_secs(2));
        assert!(OUTPUT_LOCK_TIMEOUT < RUNTIME_LIVENESS_DEADLINE);
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
    fn output_lock_timeout_is_fatal_and_never_waits_forever() {
        let shutdown = ShutdownLatch::new();
        let failure = Arc::new(Mutex::new(None));
        let killed_after_latch = Arc::new(AtomicBool::new(false));
        let kill_observation = Arc::clone(&killed_after_latch);
        let kill_shutdown = shutdown.clone();
        let output = TerminalOutputLock {
            gate: Arc::new(OutputGate::default()),
            shutdown: shutdown.clone(),
            failure: Arc::clone(&failure),
            kill_media: Arc::new(move || {
                kill_observation.store(kill_shutdown.is_triggered(), Ordering::Release);
            }),
            acquire_timeout: Duration::from_millis(30),
        };
        let _held_forever = output.acquire(OutputRole::Owner).unwrap();

        let started = Instant::now();
        let error = output
            .run(|| ())
            .expect_err("second owner must hit the bounded acquisition deadline");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(shutdown.is_triggered());
        assert!(killed_after_latch.load(Ordering::Acquire));
        assert!(
            failure
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .message
                .contains("output ownership")
        );
    }

    #[test]
    fn emergency_kill_and_latch_do_not_wait_for_the_diagnostic_mutex() {
        let shutdown = ShutdownLatch::new();
        let failure = Arc::new(Mutex::new(None));
        let held = failure.lock().unwrap();
        let killed_after_latch = Arc::new(AtomicBool::new(false));
        let kill_observation = Arc::clone(&killed_after_latch);
        let thread_shutdown = shutdown.clone();
        let kill_shutdown = shutdown.clone();
        let thread_failure = Arc::clone(&failure);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            fail_runtime(
                &thread_failure,
                &thread_shutdown,
                io::Error::new(io::ErrorKind::BrokenPipe, "fixture terminal detached"),
                &move || {
                    kill_observation.store(kill_shutdown.is_triggered(), Ordering::Release);
                },
            );
            let _ = done_tx.send(());
        });

        let completed = done_rx.recv_timeout(Duration::from_millis(500));
        assert!(shutdown.is_triggered());
        assert!(killed_after_latch.load(Ordering::Acquire));
        drop(held);
        worker.join().unwrap();
        completed.expect("emergency teardown must not block on diagnostic ownership");
        assert!(failure.lock().unwrap().is_none());
    }
}
