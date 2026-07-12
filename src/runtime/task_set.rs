use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::{JoinError, JoinSet};
use tokio::time::{Instant, sleep_until};

use super::{RuntimeEvent, RuntimeSender};

type TaskLabel = &'static str;
const TERMINAL_RETRY_DELAY: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum BackgroundShutdown {
    Drained,
    TimedOut {
        blocking_remaining: usize,
        cancellable_remaining: usize,
    },
}

impl BackgroundShutdown {
    pub fn is_drained(&self) -> bool {
        matches!(self, Self::Drained)
    }
}

#[derive(Default)]
struct AdmissionState {
    closed: bool,
}

#[derive(Clone, Default)]
struct RuntimeTaskAdmission {
    state: Arc<Mutex<AdmissionState>>,
}

impl RuntimeTaskAdmission {
    fn close(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let changed = !state.closed;
        state.closed = true;
        changed
    }

    fn while_open<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        // Hold the gate through the synchronous action. Once `close` returns, no spawn or
        // ingress send that won admission earlier can still be in progress.
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed { None } else { Some(action()) }
    }
}

#[derive(Clone)]
pub(super) struct RuntimeTaskEmitter {
    admission: RuntimeTaskAdmission,
    tx: RuntimeSender,
    shutdown_outbox: Arc<Mutex<VecDeque<RuntimeEvent>>>,
}

impl RuntimeTaskEmitter {
    pub(super) fn emit(&self, event: RuntimeEvent) -> bool {
        self.admission
            .while_open(|| match super::emit(&self.tx, event) {
                Ok(_) => true,
                Err(error) => {
                    tracing::debug!(%error, "runtime background event sink rejected event");
                    false
                }
            })
            .unwrap_or(false)
    }

    /// Retain an exact one-shot completion on this blocking worker until the owner admits it or
    /// shutdown closes admission. Only `MustDeliver` events belong on this path.
    pub(super) fn emit_terminal_blocking(&self, mut event: RuntimeEvent) -> bool {
        loop {
            match self.try_emit_terminal(event) {
                TerminalAttempt::Admitted => return true,
                TerminalAttempt::Retry(returned) => {
                    event = *returned;
                    std::thread::sleep(TERMINAL_RETRY_DELAY);
                }
                TerminalAttempt::Closed(returned) => {
                    self.retain_for_shutdown(*returned);
                    return true;
                }
            }
        }
    }

    /// Async counterpart to [`Self::emit_terminal_blocking`], used by cancellable network/tool
    /// work so a saturated owner never blocks the Tokio runtime thread.
    pub(super) async fn emit_terminal(&self, mut event: RuntimeEvent) -> bool {
        loop {
            match self.try_emit_terminal(event) {
                TerminalAttempt::Admitted => return true,
                TerminalAttempt::Retry(returned) => {
                    event = *returned;
                    tokio::time::sleep(TERMINAL_RETRY_DELAY).await;
                }
                TerminalAttempt::Closed(returned) => {
                    self.retain_for_shutdown(*returned);
                    return true;
                }
            }
        }
    }

    fn try_emit_terminal(&self, event: RuntimeEvent) -> TerminalAttempt {
        let mut event = Some(event);
        let Some(result) = self.admission.while_open(|| {
            super::ingress::emit_terminal_owned(
                &self.tx,
                event
                    .take()
                    .expect("terminal event remains owned before admission"),
            )
        }) else {
            return TerminalAttempt::Closed(Box::new(
                event.expect("closed admission did not consume terminal event"),
            ));
        };
        match result {
            Ok(_) => TerminalAttempt::Admitted,
            Err((crate::util::delivery::DeliveryError::Saturated, returned)) => {
                TerminalAttempt::Retry(returned)
            }
            Err((error, returned)) => {
                tracing::debug!(%error, "runtime terminal event sink closed or rejected event");
                TerminalAttempt::Closed(returned)
            }
        }
    }

    fn retain_for_shutdown(&self, event: RuntimeEvent) {
        self.shutdown_outbox
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push_back(event);
    }
}

enum TerminalAttempt {
    Admitted,
    Retry(Box<RuntimeEvent>),
    Closed(Box<RuntimeEvent>),
}

pub(super) struct RuntimeTaskSet {
    admission: RuntimeTaskAdmission,
    blocking: JoinSet<TaskLabel>,
    cancellable: JoinSet<TaskLabel>,
    shutdown_outbox: Arc<Mutex<VecDeque<RuntimeEvent>>>,
}

impl RuntimeTaskSet {
    pub(super) fn new() -> Self {
        Self {
            admission: RuntimeTaskAdmission::default(),
            blocking: JoinSet::new(),
            cancellable: JoinSet::new(),
            shutdown_outbox: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub(super) fn emitter(&self, tx: RuntimeSender) -> RuntimeTaskEmitter {
        RuntimeTaskEmitter {
            admission: self.admission.clone(),
            tx,
            shutdown_outbox: Arc::clone(&self.shutdown_outbox),
        }
    }

    pub(super) fn spawn_blocking<F>(&mut self, label: TaskLabel, work: F) -> bool
    where
        F: FnOnce() + Send + 'static,
    {
        let admitted = self
            .admission
            .while_open(|| {
                self.blocking.spawn_blocking(move || {
                    work();
                    label
                });
            })
            .is_some();
        if !admitted {
            tracing::debug!(
                task = label,
                "runtime background task rejected after shutdown"
            );
        }
        admitted
    }

    pub(super) fn spawn_cancellable<F>(&mut self, label: TaskLabel, work: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let admitted = self
            .admission
            .while_open(|| {
                self.cancellable.spawn(async move {
                    work.await;
                    label
                });
            })
            .is_some();
        if !admitted {
            tracing::debug!(
                task = label,
                "runtime background task rejected after shutdown"
            );
        }
        admitted
    }

    /// Permanently reject new background work and serialize with any in-flight event emission.
    pub(super) fn close_admission(&self) -> bool {
        self.admission.close()
    }

    /// Reap completed joins without awaiting so long-running owner loops retain no stale handles.
    pub(super) fn reap_finished(&mut self) {
        while let Some(result) = self.cancellable.try_join_next() {
            // This set only cancels async tasks during its own shutdown protocol.
            report_join("cancellable", result, true);
        }
        while let Some(result) = self.blocking.try_join_next() {
            report_join("blocking", result, false);
        }
    }

    /// Abort cancellable work, then join both task classes within one shared deadline.
    ///
    /// Tokio cannot abort a `spawn_blocking` closure once it has started. Timed-out blocking
    /// handles therefore remain in this set and can be reaped or waited on by a later call.
    pub(super) async fn shutdown(&mut self, budget: Duration) -> BackgroundShutdown {
        self.close_admission();
        self.reap_finished();
        self.cancellable.abort_all();

        let deadline = Instant::now() + budget;
        loop {
            self.reap_after_abort();
            if self.blocking.is_empty() && self.cancellable.is_empty() {
                return BackgroundShutdown::Drained;
            }
            if Instant::now() >= deadline {
                break;
            }

            let blocking_pending = !self.blocking.is_empty();
            let cancellable_pending = !self.cancellable.is_empty();
            tokio::select! {
                biased;
                result = self.cancellable.join_next(), if cancellable_pending => {
                    if let Some(result) = result {
                        report_join("cancellable", result, true);
                    }
                }
                result = self.blocking.join_next(), if blocking_pending => {
                    if let Some(result) = result {
                        report_join("blocking", result, false);
                    }
                }
                _ = sleep_until(deadline) => break,
            }
        }

        self.reap_after_abort();
        if self.blocking.is_empty() && self.cancellable.is_empty() {
            BackgroundShutdown::Drained
        } else {
            BackgroundShutdown::TimedOut {
                blocking_remaining: self.blocking.len(),
                cancellable_remaining: self.cancellable.len(),
            }
        }
    }

    /// Recover ownership of every non-abortable worker without a final timeout, then return the
    /// exact terminal completions which finished after owner ingress admission closed.
    pub(super) async fn finalize(&mut self) -> Vec<RuntimeEvent> {
        self.close_admission();
        self.cancellable.abort_all();
        while !self.blocking.is_empty() || !self.cancellable.is_empty() {
            let blocking_pending = !self.blocking.is_empty();
            let cancellable_pending = !self.cancellable.is_empty();
            tokio::select! {
                biased;
                result = self.cancellable.join_next(), if cancellable_pending => {
                    if let Some(result) = result {
                        report_join("cancellable", result, true);
                    }
                }
                result = self.blocking.join_next(), if blocking_pending => {
                    if let Some(result) = result {
                        report_join("blocking", result, false);
                    }
                }
            }
        }
        self.shutdown_outbox
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain(..)
            .collect()
    }

    fn reap_after_abort(&mut self) {
        while let Some(result) = self.cancellable.try_join_next() {
            report_join("cancellable", result, true);
        }
        while let Some(result) = self.blocking.try_join_next() {
            report_join("blocking", result, false);
        }
    }

    #[cfg(test)]
    pub(super) fn pending_counts(&self) -> (usize, usize) {
        (self.blocking.len(), self.cancellable.len())
    }
}

fn report_join(
    kind: &'static str,
    result: Result<TaskLabel, JoinError>,
    cancellation_expected: bool,
) {
    match result {
        Ok(label) => tracing::trace!(
            task_kind = kind,
            task = label,
            "runtime background task joined"
        ),
        Err(error) if cancellation_expected && error.is_cancelled() => {
            tracing::trace!(
                task_kind = kind,
                "runtime background task cancelled and joined"
            )
        }
        Err(error) => {
            tracing::warn!(task_kind = kind, %error, "runtime background task join failed")
        }
    }
}
