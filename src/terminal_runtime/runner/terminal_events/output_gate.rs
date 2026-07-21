use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::player::lifetime::ShutdownLatch;
pub(super) use crate::terminal_policy::LIVENESS_OUTPUT_GATE_TIMEOUT as LIVENESS_ACQUIRE_TIMEOUT;
use crate::terminal_policy::OWNER_OUTPUT_TIMEOUT;

const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Serialises CPR writes with complete terminal frames and OSC messages.
///
/// Acquisition is deliberately policy-free: a caller receives a typed timeout and decides
/// whether it is one ambiguous liveness observation or a fatal owner I/O error. This prevents a
/// low-level contended mutex from killing media on its first timeout.
#[derive(Clone)]
pub(in crate::terminal_runtime::runner) struct TerminalOutputLock {
    gate: Arc<OutputGate>,
    shutdown: ShutdownLatch,
    owner_output_timeout: Duration,
    liveness_acquire_timeout: Duration,
}

impl TerminalOutputLock {
    pub(super) fn new(shutdown: ShutdownLatch) -> Self {
        Self {
            gate: Arc::new(OutputGate::default()),
            shutdown,
            owner_output_timeout: OWNER_OUTPUT_TIMEOUT,
            liveness_acquire_timeout: LIVENESS_ACQUIRE_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn with_timeouts(
        shutdown: ShutdownLatch,
        owner_output_timeout: Duration,
        liveness_acquire_timeout: Duration,
    ) -> Self {
        Self {
            gate: Arc::new(OutputGate::default()),
            shutdown,
            owner_output_timeout,
            liveness_acquire_timeout,
        }
    }

    pub(in crate::terminal_runtime::runner) fn run<T>(
        &self,
        operation: impl FnOnce(Instant) -> T,
    ) -> io::Result<T> {
        self.run_for(self.owner_output_timeout, operation)
    }

    pub(in crate::terminal_runtime::runner) fn run_for<T>(
        &self,
        budget: Duration,
        operation: impl FnOnce(Instant) -> T,
    ) -> io::Result<T> {
        let deadline = Instant::now().checked_add(budget).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal output ownership deadline overflow",
            )
        })?;
        self.run_until(deadline, operation)
    }

    fn run_until<T>(
        &self,
        deadline: Instant,
        operation: impl FnOnce(Instant) -> T,
    ) -> io::Result<T> {
        let _permit = self
            .acquire(OutputRole::Owner, Some(deadline))
            .map_err(AcquireFailure::into_error)?;
        Ok(operation(deadline))
    }

    pub(in crate::terminal_runtime::runner) fn run_io<T>(
        &self,
        operation: impl FnOnce(Instant) -> io::Result<T>,
    ) -> io::Result<T> {
        self.run(operation)?
    }

    pub(in crate::terminal_runtime::runner) fn run_io_for<T>(
        &self,
        budget: Duration,
        operation: impl FnOnce(Instant) -> io::Result<T>,
    ) -> io::Result<T> {
        self.run_for(budget, operation)?
    }

    pub(super) fn run_liveness_io<T>(
        &self,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> Result<T, LivenessOutputError> {
        let deadline = Instant::now() + self.liveness_acquire_timeout;
        let _permit = self
            .acquire(OutputRole::Liveness, Some(deadline))
            .map_err(LivenessOutputError::Gate)?;
        operation().map_err(LivenessOutputError::Operation)
    }

    pub(super) fn snapshot(&self) -> OutputGateSnapshot {
        let state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.held.map_or(
            OutputGateSnapshot {
                holder_role: None,
                holder_generation: 0,
                held_for: Duration::ZERO,
                liveness_waiters: state.liveness_waiters + usize::from(state.liveness_reserved),
                owner_deadline_expired: false,
            },
            |held| {
                let now = Instant::now();
                let held_for = now.saturating_duration_since(held.started_at);
                OutputGateSnapshot {
                    holder_role: Some(held.role),
                    holder_generation: held.generation,
                    held_for,
                    liveness_waiters: state.liveness_waiters + usize::from(state.liveness_reserved),
                    owner_deadline_expired: held.role == OutputRole::Owner
                        && held.deadline.is_some_and(|deadline| now >= deadline),
                }
            },
        )
    }

    pub(super) fn wake_all(&self) {
        self.gate.changed.notify_all();
    }

    fn acquire(
        &self,
        role: OutputRole,
        deadline: Option<Instant>,
    ) -> Result<OutputPermit, AcquireFailure> {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if role == OutputRole::Liveness {
            state.liveness_waiters = state.liveness_waiters.saturating_add(1);
        }

        loop {
            if self.shutdown.is_triggered() {
                if role == OutputRole::Liveness {
                    state.liveness_reserved = false;
                }
                remove_liveness_waiter(role, &mut state, &self.gate.changed);
                return Err(AcquireFailure::shutdown(state.held));
            }
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                let failure = AcquireFailure::timed_out(state.held);
                if role == OutputRole::Liveness {
                    // Preserve priority across the scheduler gap before the next heartbeat. A
                    // stream of individually legal owner frames must not starve CPR forever.
                    state.liveness_reserved = true;
                }
                remove_liveness_waiter(role, &mut state, &self.gate.changed);
                return Err(failure);
            }
            let liveness_has_priority = role == OutputRole::Owner
                && (state.liveness_waiters > 0 || state.liveness_reserved);
            if state.held.is_none() && !liveness_has_priority {
                state.next_generation = state.next_generation.wrapping_add(1).max(1);
                let generation = state.next_generation;
                state.held = Some(HeldOutput {
                    role,
                    generation,
                    started_at: Instant::now(),
                    deadline,
                });
                if role == OutputRole::Liveness {
                    state.liveness_reserved = false;
                }
                remove_liveness_waiter(role, &mut state, &self.gate.changed);
                return Ok(OutputPermit {
                    gate: Arc::clone(&self.gate),
                    generation,
                });
            }

            let wait_for = deadline
                .map(|deadline| deadline.saturating_duration_since(now))
                .unwrap_or(SHUTDOWN_POLL_INTERVAL)
                .min(SHUTDOWN_POLL_INTERVAL);
            let waited = self.gate.changed.wait_timeout(state, wait_for);
            let (next, _) = waited.unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }
}

fn remove_liveness_waiter(role: OutputRole, state: &mut OutputGateState, changed: &Condvar) {
    if role == OutputRole::Liveness {
        state.liveness_waiters = state.liveness_waiters.saturating_sub(1);
        changed.notify_all();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputRole {
    Owner,
    Liveness,
}

#[derive(Clone, Copy)]
struct HeldOutput {
    role: OutputRole,
    generation: u64,
    started_at: Instant,
    deadline: Option<Instant>,
}

#[derive(Default)]
struct OutputGateState {
    held: Option<HeldOutput>,
    next_generation: u64,
    liveness_waiters: usize,
    liveness_reserved: bool,
}

#[derive(Default)]
struct OutputGate {
    state: Mutex<OutputGateState>,
    changed: Condvar,
}

struct OutputPermit {
    gate: Arc<OutputGate>,
    #[allow(dead_code)]
    generation: u64,
}

impl Drop for OutputPermit {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.held = None;
        self.gate.changed.notify_all();
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct OutputGateSnapshot {
    pub(super) holder_role: Option<OutputRole>,
    pub(super) holder_generation: u64,
    pub(super) held_for: Duration,
    pub(super) liveness_waiters: usize,
    /// The owner's deadline covers the whole closure, including work before it starts writing.
    pub(super) owner_deadline_expired: bool,
}

#[derive(Debug)]
pub(super) struct AcquireFailure {
    kind: io::ErrorKind,
    pub(super) holder_role: Option<OutputRole>,
    pub(super) holder_generation: u64,
    pub(super) held_for: Duration,
}

impl AcquireFailure {
    fn timed_out(holder: Option<HeldOutput>) -> Self {
        Self::new(io::ErrorKind::TimedOut, holder)
    }

    fn shutdown(holder: Option<HeldOutput>) -> Self {
        Self::new(io::ErrorKind::BrokenPipe, holder)
    }

    fn new(kind: io::ErrorKind, holder: Option<HeldOutput>) -> Self {
        holder.map_or(
            Self {
                kind,
                holder_role: None,
                holder_generation: 0,
                held_for: Duration::ZERO,
            },
            |held| Self {
                kind,
                holder_role: Some(held.role),
                holder_generation: held.generation,
                held_for: held.started_at.elapsed(),
            },
        )
    }

    pub(super) fn into_error(self) -> io::Error {
        let message = match self.holder_role {
            Some(role) => format!(
                "terminal output ownership could not be acquired before its deadline (holder_role={role:?}, holder_generation={}, held_ms={})",
                self.holder_generation,
                self.held_for.as_millis()
            ),
            None if self.kind == io::ErrorKind::BrokenPipe => {
                "terminal output ownership ended during shutdown".to_owned()
            }
            None => {
                "terminal output ownership could not be acquired before its deadline".to_owned()
            }
        };
        io::Error::new(self.kind, message)
    }
}

#[derive(Debug)]
pub(super) enum LivenessOutputError {
    Gate(AcquireFailure),
    Operation(io::Error),
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn output_timeout_reports_stable_holder_generation_without_triggering_shutdown() {
        let shutdown = ShutdownLatch::new();
        let output = TerminalOutputLock::with_timeouts(
            shutdown.clone(),
            Duration::from_millis(20),
            Duration::from_millis(20),
        );
        let held = output
            .acquire(
                OutputRole::Owner,
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .unwrap();

        let first = match output.run_liveness_io(|| Ok(())) {
            Err(LivenessOutputError::Gate(error)) => error,
            other => panic!("expected a gate timeout, got {other:?}"),
        };
        let second = match output.run_liveness_io(|| Ok(())) {
            Err(LivenessOutputError::Gate(error)) => error,
            other => panic!("expected a gate timeout, got {other:?}"),
        };
        assert_ne!(first.holder_generation, 0);
        assert_eq!(first.holder_generation, second.holder_generation);
        assert!(!shutdown.is_triggered());
        drop(held);
    }

    #[test]
    fn per_operation_owner_deadline_covers_work_before_the_writer_starts() {
        let shutdown = ShutdownLatch::new();
        let output = TerminalOutputLock::with_timeouts(
            shutdown,
            Duration::from_secs(7),
            Duration::from_secs(1),
        );
        let held = output
            .acquire(
                OutputRole::Owner,
                Some(Instant::now() + Duration::from_millis(20)),
            )
            .unwrap();

        std::thread::sleep(Duration::from_millis(30));
        let snapshot = output.snapshot();
        assert_eq!(snapshot.holder_role, Some(OutputRole::Owner));
        assert!(snapshot.owner_deadline_expired);
        assert!(snapshot.held_for >= Duration::from_millis(20));
        drop(held);
    }

    #[test]
    fn queued_liveness_has_priority_over_a_new_owner() {
        let shutdown = ShutdownLatch::new();
        let output = TerminalOutputLock::with_timeouts(
            shutdown,
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let first_owner = output
            .acquire(
                OutputRole::Owner,
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .unwrap();
        let order = Arc::new(AtomicUsize::new(0));

        let live_output = output.clone();
        let live_order = Arc::clone(&order);
        let liveness = std::thread::spawn(move || {
            live_output
                .run_liveness_io(|| {
                    assert_eq!(live_order.fetch_add(1, Ordering::SeqCst), 0);
                    Ok(())
                })
                .unwrap();
        });
        while output.snapshot().liveness_waiters == 0 {
            std::thread::yield_now();
        }

        let owner_output = output.clone();
        let owner_order = Arc::clone(&order);
        let owner = std::thread::spawn(move || {
            owner_output
                .run(|_| assert_eq!(owner_order.fetch_add(1, Ordering::SeqCst), 1))
                .unwrap();
        });
        drop(first_owner);
        liveness.join().unwrap();
        owner.join().unwrap();
    }

    #[test]
    fn timed_out_liveness_reservation_prevents_successive_owner_starvation() {
        let shutdown = ShutdownLatch::new();
        let output = TerminalOutputLock::with_timeouts(
            shutdown,
            Duration::from_secs(1),
            Duration::from_millis(20),
        );
        let first_owner = output
            .acquire(
                OutputRole::Owner,
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .unwrap();
        assert!(matches!(
            output.run_liveness_io(|| Ok(())),
            Err(LivenessOutputError::Gate(_))
        ));
        assert_eq!(output.snapshot().liveness_waiters, 1);

        let next_owner = output.clone();
        let (owner_tx, owner_rx) = std::sync::mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || {
            next_owner.run(|_| owner_tx.send(()).unwrap()).unwrap();
        });
        drop(first_owner);
        assert!(
            owner_rx.recv_timeout(Duration::from_millis(30)).is_err(),
            "a new owner bypassed the preserved liveness reservation"
        );

        output.run_liveness_io(|| Ok(())).unwrap();
        owner_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        owner.join().unwrap();
    }

    #[test]
    fn liveness_deadline_race_without_a_holder_still_reserves_the_next_turn() {
        let shutdown = ShutdownLatch::new();
        let output = TerminalOutputLock::with_timeouts(
            shutdown,
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert!(matches!(
            output.acquire(OutputRole::Liveness, Some(Instant::now())),
            Err(AcquireFailure {
                kind: io::ErrorKind::TimedOut,
                ..
            })
        ));
        assert_eq!(output.snapshot().liveness_waiters, 1);

        let next_owner = output.clone();
        let (owner_tx, owner_rx) = std::sync::mpsc::sync_channel(1);
        let owner = std::thread::spawn(move || {
            next_owner.run(|_| owner_tx.send(()).unwrap()).unwrap();
        });
        assert!(owner_rx.recv_timeout(Duration::from_millis(20)).is_err());
        output.run_liveness_io(|| Ok(())).unwrap();
        owner_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        owner.join().unwrap();
    }

    #[test]
    fn shutdown_wakes_a_waiter_before_its_timeout() {
        let shutdown = ShutdownLatch::new();
        let output = TerminalOutputLock::with_timeouts(
            shutdown.clone(),
            Duration::from_secs(2),
            Duration::from_secs(2),
        );
        let held = output
            .acquire(
                OutputRole::Owner,
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .unwrap();
        let waiter_output = output.clone();
        let waiter = std::thread::spawn(move || waiter_output.run_liveness_io(|| Ok(())));
        while output.snapshot().liveness_waiters == 0 {
            std::thread::yield_now();
        }
        shutdown.trigger();
        output.wake_all();
        let error = waiter.join().unwrap().unwrap_err();
        assert!(matches!(error, LivenessOutputError::Gate(_)));
        drop(held);
    }

    #[test]
    fn owner_gate_wait_and_operation_receive_one_absolute_deadline() {
        let shutdown = ShutdownLatch::new();
        let output = TerminalOutputLock::with_timeouts(
            shutdown,
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let held = output
            .acquire(
                OutputRole::Owner,
                Some(Instant::now() + Duration::from_secs(1)),
            )
            .unwrap();
        let shared_deadline = Instant::now() + Duration::from_millis(500);
        let waiting_output = output.clone();
        let waiter = std::thread::spawn(move || {
            waiting_output.run_until(shared_deadline, |operation_deadline| operation_deadline)
        });

        std::thread::sleep(Duration::from_millis(20));
        drop(held);
        assert_eq!(waiter.join().unwrap().unwrap(), shared_deadline);
    }
}
