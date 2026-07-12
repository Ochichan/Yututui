//! Correlated acknowledgement barrier for commands which close an owned resource.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) const COMMAND_ACK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct CommandBarrier {
    inner: Arc<BarrierInner>,
}

#[derive(Debug)]
struct BarrierInner {
    state: Mutex<BarrierState>,
    changed: Condvar,
    signals: AtomicUsize,
}

#[derive(Clone, Debug)]
enum BarrierState {
    Pending,
    Succeeded,
    Failed(String),
}

/// The actor-side half. Every clone is counted by `Arc`; dropping the last signal while the
/// barrier is pending deterministically fails the waiter, covering queue clears and cancellation.
#[derive(Debug)]
pub(crate) struct CommandBarrierSignal {
    inner: Arc<BarrierInner>,
}

impl CommandBarrier {
    pub(crate) fn pending() -> Self {
        Self {
            inner: Arc::new(BarrierInner {
                state: Mutex::new(BarrierState::Pending),
                changed: Condvar::new(),
                signals: AtomicUsize::new(0),
            }),
        }
    }

    pub(crate) fn signal(&self) -> CommandBarrierSignal {
        self.inner.signals.fetch_add(1, Ordering::Relaxed);
        CommandBarrierSignal {
            inner: Arc::clone(&self.inner),
        }
    }

    pub(crate) fn wait(&self) -> Result<(), String> {
        self.wait_until(Instant::now() + COMMAND_ACK_TIMEOUT)
    }

    #[cfg(test)]
    fn wait_for(&self, timeout: Duration) -> Result<(), String> {
        self.wait_until(Instant::now() + timeout)
    }

    pub(crate) fn wait_until(&self, deadline: Instant) -> Result<(), String> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match &*state {
                BarrierState::Succeeded => return Ok(()),
                BarrierState::Failed(error) => return Err(error.clone()),
                BarrierState::Pending => {}
            }
            let now = Instant::now();
            if now >= deadline {
                return Err("timed out waiting for the command acknowledgement".to_owned());
            }
            let (next, _) = self
                .inner
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
        }
    }

    #[cfg(test)]
    pub(crate) fn wait_for_test(&self, timeout: Duration) -> Result<(), String> {
        self.wait_for(timeout)
    }
}

impl Clone for CommandBarrierSignal {
    fn clone(&self) -> Self {
        self.inner.signals.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for CommandBarrierSignal {
    fn drop(&mut self) {
        if self.inner.signals.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.fail("command was dropped before acknowledgement");
        }
    }
}

impl CommandBarrierSignal {
    pub(crate) fn succeed(&self) {
        self.resolve(BarrierState::Succeeded);
    }

    pub(crate) fn fail(&self, error: impl Into<String>) {
        self.resolve(BarrierState::Failed(error.into()));
    }

    fn resolve(&self, next: BarrierState) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, BarrierState::Pending) {
            *state = next;
            self.inner.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_and_failure_are_correlated() {
        let success = CommandBarrier::pending();
        success.signal().succeed();
        assert!(success.wait_for_test(Duration::ZERO).is_ok());

        let failure = CommandBarrier::pending();
        failure.signal().fail("command rejected");
        assert_eq!(
            failure.wait_for_test(Duration::ZERO).unwrap_err(),
            "command rejected"
        );
    }

    #[test]
    fn pending_barrier_times_out_without_becoming_retry_safe() {
        let barrier = CommandBarrier::pending();
        let _signal = barrier.signal();
        assert!(
            barrier
                .wait_for_test(Duration::from_millis(1))
                .unwrap_err()
                .contains("timed out")
        );
    }

    #[test]
    fn harmless_signal_clone_drop_does_not_fail_but_last_drop_does() {
        let barrier = CommandBarrier::pending();
        let first = barrier.signal();
        let second = first.clone();
        drop(first);
        assert!(
            barrier
                .wait_for_test(Duration::from_millis(1))
                .unwrap_err()
                .contains("timed out")
        );
        drop(second);
        assert!(
            barrier
                .wait_for_test(Duration::ZERO)
                .unwrap_err()
                .contains("dropped before acknowledgement")
        );
    }
}
