//! Process-lifetime ownership for the isolated scrobble actor.

use super::*;

impl ScrobbleHandle {
    /// Admit every locally deferred command, then ask for a final queue flush. Lock contention,
    /// an unconfirmed append/compaction, or a durability I/O failure is returned explicitly.
    /// Production owners use [`Self::shutdown_and_join`] so their diagnostic deadline does not
    /// cancel this correlated receipt or detach the actor thread.
    pub async fn shutdown_flush(&self) -> Result<(), DeliveryError> {
        {
            let mut state = lock_pending(&self.pending.state);
            if state.closed || self.tx.is_closed() {
                state.closed = true;
                return Err(DeliveryError::Closed);
            }
            state.shutting_down = true;
        }
        let (done, rx) = tokio::sync::oneshot::channel();
        let receipt = match self.shutdown_tx.try_send(ShutdownRequest { done }) {
            Ok(()) => DeliveryReceipt::Enqueued,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                return Err(DeliveryError::Busy);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                return Err(DeliveryError::Closed);
            }
        };
        tracing::trace!(?receipt, "scrobble shutdown flush accepted");
        rx.await.map_err(|_| DeliveryError::Closed)?
    }

    /// Seal admission, confirm the actor's final durability result, and join its OS thread.
    ///
    /// `diagnostic_budget` controls only when a slow-shutdown warning is emitted. The same
    /// `shutdown_flush` future remains owned and is awaited after that deadline; cancelling and
    /// recreating it would discard the correlated receipt while the actor is still committing
    /// accepted work. Once the receipt settles, joining is synchronous and non-cancellable.
    pub async fn shutdown_and_join(
        &mut self,
        diagnostic_budget: std::time::Duration,
    ) -> Result<(), DeliveryError> {
        let outcome = {
            let flush = self.shutdown_flush();
            tokio::pin!(flush);
            match tokio::time::timeout(diagnostic_budget, flush.as_mut()).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(
                        elapsed_ms = diagnostic_budget.as_millis(),
                        "scrobble shutdown exceeded its diagnostic budget; waiting for accepted durability"
                    );
                    flush.await
                }
            }
        };
        let join = self.join_actor_thread();
        match (outcome, join) {
            (Err(error), _) => Err(error),
            (Ok(()), result) => result,
        }
    }

    fn join_actor_thread(&mut self) -> Result<(), DeliveryError> {
        let Some(actor_thread) = self.actor_thread.take() else {
            return Ok(());
        };
        if actor_thread.join().is_err() {
            tracing::error!("scrobble actor thread panicked during shutdown");
            Err(DeliveryError::Closed)
        } else {
            Ok(())
        }
    }
}

impl Drop for ScrobbleHandle {
    fn drop(&mut self) {
        if self.actor_thread.is_none() {
            return;
        }

        // Explicit shutdown normally sent the request already. If its future was cancelled, the
        // request remains queued and the join below is still the owner. A handle dropped without
        // an explicit finalizer seals admission and supplies that request here instead.
        let should_request_shutdown = {
            let mut state = lock_pending(&self.pending.state);
            let should_request = !state.shutting_down && !state.closed && !self.tx.is_closed();
            state.shutting_down = true;
            should_request
        };
        if should_request_shutdown {
            let (done, _receipt) = tokio::sync::oneshot::channel();
            match self.shutdown_tx.try_send(ShutdownRequest { done }) {
                Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    tracing::debug!("scrobble actor shutdown channel already closed during drop");
                }
            }
        }
        if let Err(error) = self.join_actor_thread() {
            tracing::error!(%error, "could not join scrobble actor during drop");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_and_join_keeps_waiting_after_the_diagnostic_budget() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<ShutdownRequest>(1);
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let worker = std::thread::spawn(move || {
            let _keep_command_inbox_open = rx;
            let request = shutdown_rx
                .blocking_recv()
                .expect("owner publishes a shutdown request");
            std::thread::sleep(Duration::from_millis(40));
            let _ = request.done.send(Ok(()));
            worker_finished.store(true, Ordering::Release);
        });
        let mut handle = ScrobbleHandle::with_pending(
            tx,
            shutdown_tx,
            Arc::new(PendingCommands::default()),
            Some(worker),
        );

        let started = Instant::now();
        assert_eq!(
            handle.shutdown_and_join(Duration::from_millis(5)).await,
            Ok(())
        );
        assert!(
            started.elapsed() >= Duration::from_millis(20),
            "the diagnostic deadline must not cancel the correlated shutdown receipt"
        );
        assert!(
            finished.load(Ordering::Acquire),
            "successful finalization means the actor OS thread was joined"
        );
    }
}
