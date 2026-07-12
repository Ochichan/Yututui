//! Abort-on-drop ownership for optional Tokio background work.

use std::future::Future;

use tokio::task::JoinHandle;

/// An optional background task that cannot become detached when its owner is dropped.
///
/// Disabled producers return [`Self::disabled`]. Enabled producers return
/// [`Self::spawn`], and the caller retains the value for exactly as long as the work is
/// allowed to emit events or mutate disk state. Normal shutdown aborts and joins through
/// [`Self::shutdown`]; early-return and panic paths get the abort fallback in [`Drop`].
#[must_use = "background work must remain owned for its complete lifetime"]
pub struct BackgroundTask {
    label: &'static str,
    task: Option<JoinHandle<()>>,
}

impl BackgroundTask {
    pub fn disabled(label: &'static str) -> Self {
        Self { label, task: None }
    }

    pub fn spawn<F>(label: &'static str, future: F) -> Self
    where
        F: Future<Output = ()> + Send + 'static,
    {
        Self {
            label,
            task: Some(tokio::spawn(future)),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.task.is_some()
    }

    /// Stop admission immediately and wait until Tokio confirms cancellation.
    pub async fn shutdown(&mut self) {
        let Some(task) = self.task.as_mut() else {
            return;
        };
        task.abort();
        let result = task.await;
        // Keep the handle in `self` across the await. If this shutdown future is itself
        // cancelled, `Drop` still owns the task and can enforce the abort fallback.
        self.task.take();
        match result {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) => tracing::warn!(
                task = self.label,
                error = %error,
                "background task failed during shutdown"
            ),
        }
    }
}

impl Drop for BackgroundTask {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drop_aborts_instead_of_detaching() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let task = BackgroundTask::spawn("test", async move {
            struct MarkDrop(Option<tokio::sync::oneshot::Sender<()>>);
            impl Drop for MarkDrop {
                fn drop(&mut self) {
                    if let Some(tx) = self.0.take() {
                        let _ = tx.send(());
                    }
                }
            }
            let _mark = MarkDrop(Some(dropped_tx));
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();

        drop(task);

        dropped_rx.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_joins_cancellation_and_is_idempotent() {
        let mut task = BackgroundTask::spawn("test", std::future::pending());
        assert!(task.is_enabled());

        task.shutdown().await;
        task.shutdown().await;

        assert!(!task.is_enabled());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_shutdown_future_keeps_abort_handle_owned() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let mut task = BackgroundTask::spawn("test", async move {
            struct MarkDrop(Option<tokio::sync::oneshot::Sender<()>>);
            impl Drop for MarkDrop {
                fn drop(&mut self) {
                    if let Some(tx) = self.0.take() {
                        let _ = tx.send(());
                    }
                }
            }
            let _mark = MarkDrop(Some(dropped_tx));
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();

        let mut shutdown = Box::pin(task.shutdown());
        tokio::select! {
            biased;
            _ = &mut shutdown => panic!("aborted task joined before cancellation point"),
            _ = std::future::ready(()) => {}
        }
        drop(shutdown);

        assert!(
            task.is_enabled(),
            "cancelled shutdown must retain task ownership"
        );
        drop(task);
        dropped_rx.await.unwrap();
    }
}
