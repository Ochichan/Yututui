//! Off-owner execution and re-entry for effects emitted by the daemon engine.

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use tokio::task::{JoinError, JoinSet};

use super::{DaemonEvent, DaemonEventSender, engine, personal_export};
use crate::player::lifetime::ShutdownLatch;

const TERMINAL_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Background work spawned by the daemon owner.
///
/// These tasks are deliberately owned by `serve`: dropping a detached self-heal or retry while
/// shutdown is flushing durable state would let it re-enter a dead owner lane (or outlive the
/// daemon entirely). Every task also observes the out-of-band latch so long sleeps and update
/// checks are cancelled as soon as shutdown starts.
pub(super) struct DaemonEffectTasks {
    tasks: JoinSet<()>,
}

impl DaemonEffectTasks {
    pub(super) fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    fn spawn_cancellable<F>(&mut self, shutdown: ShutdownLatch, work: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if shutdown.is_triggered() {
            return false;
        }
        self.tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = shutdown.wait() => {}
                _ = work => {}
            }
        });
        true
    }

    fn schedule_ytdlp_heal<F>(
        &mut self,
        event_tx: &DaemonEventSender,
        shutdown: &ShutdownLatch,
        video_id: String,
        update: F,
    ) where
        F: Future<Output = bool> + Send + 'static,
    {
        let tx = event_tx.clone();
        let completion_shutdown = shutdown.clone();
        self.spawn_cancellable(shutdown.clone(), async move {
            let updated = update.await;
            if !completion_shutdown.is_triggered() {
                deliver_terminal(
                    &tx,
                    &completion_shutdown,
                    DaemonEvent::YtdlpHeal { video_id, updated },
                )
                .await;
            }
        });
    }

    fn schedule_transport_retry(
        &mut self,
        event_tx: &DaemonEventSender,
        shutdown: &ShutdownLatch,
        generation: u64,
        retry_after: Duration,
    ) {
        let tx = event_tx.clone();
        let completion_shutdown = shutdown.clone();
        self.spawn_cancellable(shutdown.clone(), async move {
            tokio::time::sleep(retry_after).await;
            if !completion_shutdown.is_triggered() {
                deliver_terminal(
                    &tx,
                    &completion_shutdown,
                    DaemonEvent::TransportRecoveryRetry { generation },
                )
                .await;
            }
        });
    }

    /// Run a personal-data export on Tokio's blocking pool while retaining ownership in the
    /// daemon task set. Unlike cancellable network/retry work, a started filesystem projection
    /// cannot be safely interrupted. `JoinSet::abort_all` prevents queued work from starting and
    /// `join_next` still waits for a blocking closure that has begun, so shutdown never detaches a
    /// writer from the process that owns its persistence lease.
    pub(super) fn schedule_personal_export<F>(
        &mut self,
        event_tx: &DaemonEventSender,
        shutdown: &ShutdownLatch,
        generation: u64,
        export: F,
    ) -> bool
    where
        F: FnOnce() -> Result<PathBuf, String> + Send + 'static,
    {
        if shutdown.is_triggered() {
            return false;
        }
        let tx = event_tx.clone();
        let completion_shutdown = shutdown.clone();
        self.tasks.spawn_blocking(move || {
            // Contain export panics so the Finished event always fires and the requester is never
            // left waiting on a completion that died. This holds in unwind builds (dev/test);
            // release builds use `panic = "abort"`, where an export panic aborts the daemon
            // before this closure can report it.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(export))
                .unwrap_or_else(|_| {
                    Err("personal-data export worker failed: task panicked".to_owned())
                });
            let _ = deliver_terminal_blocking(
                &tx,
                &completion_shutdown,
                DaemonEvent::PersonalExportFinished(personal_export::Finished {
                    generation,
                    result,
                }),
            );
        });
        true
    }

    pub(super) fn reap_finished(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            log_task_result(result);
        }
    }

    /// Cancel and join every owner-spawned task before durable shutdown work begins.
    pub(super) async fn shutdown(&mut self) {
        self.tasks.abort_all();
        while let Some(result) = self.tasks.join_next().await {
            log_task_result(result);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.tasks.len()
    }
}

async fn deliver_terminal(
    tx: &DaemonEventSender,
    shutdown: &ShutdownLatch,
    mut event: DaemonEvent,
) -> bool {
    loop {
        if shutdown.is_triggered() {
            return false;
        }
        match tx.emit_terminal_owned(event) {
            Ok(_) => return true,
            Err((crate::util::delivery::DeliveryError::Saturated, returned)) => {
                event = *returned;
                tokio::select! {
                    biased;
                    _ = shutdown.wait() => return false,
                    _ = tokio::time::sleep(TERMINAL_RETRY_DELAY) => {}
                }
            }
            Err((error, _returned)) => {
                tracing::debug!(%error, "daemon terminal event sink closed or rejected event");
                return false;
            }
        }
    }
}

pub(super) fn deliver_terminal_blocking(
    tx: &DaemonEventSender,
    shutdown: &ShutdownLatch,
    mut event: DaemonEvent,
) -> bool {
    loop {
        if shutdown.is_triggered() {
            return false;
        }
        match tx.emit_terminal_owned(event) {
            Ok(_) => return true,
            Err((crate::util::delivery::DeliveryError::Saturated, returned)) => {
                event = *returned;
                std::thread::sleep(TERMINAL_RETRY_DELAY);
            }
            Err((error, _returned)) => {
                tracing::debug!(%error, "daemon terminal event sink closed or rejected event");
                return false;
            }
        }
    }
}

fn log_task_result(result: Result<(), JoinError>) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        tracing::warn!(%error, "daemon background effect task failed");
    }
}

pub(super) fn dispatch_engine_effects(
    api: &crate::api::ApiHandle,
    event_tx: &DaemonEventSender,
    shutdown: &ShutdownLatch,
    tasks: &mut DaemonEffectTasks,
    effects: Vec<engine::EngineEffect>,
) -> Vec<DaemonEvent> {
    tasks.reap_finished();
    let mut terminal = Vec::new();
    for effect in effects {
        if shutdown.is_triggered() {
            break;
        }
        match effect {
            engine::EngineEffect::StreamingFallback {
                request_id,
                seed,
                seed_video_id,
                exclude_ids,
                limit,
                mode,
                config,
            } => {
                if let Err(error) = api.streaming(
                    request_id,
                    seed,
                    seed_video_id.clone(),
                    exclude_ids,
                    limit,
                    mode,
                    config,
                ) {
                    tracing::warn!(%error, "api command enqueue failed");
                    if !shutdown.is_triggered() {
                        terminal.push(DaemonEvent::Api(crate::api::ApiEvent::StreamingError {
                            request_id,
                            seed_video_id,
                            error: error.to_string(),
                        }));
                    }
                }
            }
            engine::EngineEffect::StreamingPreflight {
                request_id,
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => {
                if let Err(error) = api.streaming_preflight(
                    request_id,
                    seed_video_id.clone(),
                    picks,
                    fallback,
                    mode,
                    config,
                ) {
                    tracing::warn!(%error, "api command enqueue failed");
                    if !shutdown.is_triggered() {
                        terminal.push(DaemonEvent::Api(crate::api::ApiEvent::StreamingError {
                            request_id,
                            seed_video_id,
                            error: error.to_string(),
                        }));
                    }
                }
            }
            // Off-loop: the update check may download ~40 MiB. The verdict re-enters
            // the serve loop as a DaemonEvent so the engine can retry or skip.
            engine::EngineEffect::YtdlpSelfHeal { video_id, tools } => {
                tasks.schedule_ytdlp_heal(event_tx, shutdown, video_id, async move {
                    crate::tools::ytdlp::clear_probe_cache();
                    let outcome = crate::tools::ytdlp::rollback_or_check_and_update(
                        &tools,
                        &|_| {},
                        "daemon playback self-heal",
                    )
                    .await;
                    matches!(
                        outcome,
                        crate::tools::ytdlp::UpdateOutcome::Installed { .. }
                    )
                });
            }
            engine::EngineEffect::TransportRecoveryRetry {
                generation,
                retry_after,
            } => tasks.schedule_transport_retry(event_tx, shutdown, generation, retry_after),
        }
    }
    terminal
}

#[cfg(test)]
mod tests {
    use std::future;

    use super::*;

    fn event_channel() -> (DaemonEventSender, tokio::sync::mpsc::Receiver<DaemonEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        (DaemonEventSender::new(tx), rx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transport_retry_is_not_spawned_after_shutdown_latches() {
        let (event_tx, mut event_rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        shutdown.trigger();
        let mut tasks = DaemonEffectTasks::new();

        tasks.schedule_transport_retry(&event_tx, &shutdown, 9, Duration::ZERO);

        assert_eq!(tasks.len(), 0);
        assert!(event_rx.try_recv().is_err());
        tasks.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn heal_and_retry_tasks_are_cancelled_and_joined_before_exit() {
        let (event_tx, mut event_rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        let mut tasks = DaemonEffectTasks::new();

        tasks.schedule_ytdlp_heal(
            &event_tx,
            &shutdown,
            "video".to_owned(),
            future::pending::<bool>(),
        );
        tasks.schedule_transport_retry(&event_tx, &shutdown, 10, Duration::from_secs(3600));
        assert_eq!(tasks.len(), 2);

        shutdown.trigger();
        tasks.shutdown().await;

        assert_eq!(tasks.len(), 0);
        assert!(event_rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_completion_retries_saturation_without_blocking_owner_drain() {
        let (raw_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        assert!(
            raw_tx
                .try_send(DaemonEvent::TransportRecoveryRetry { generation: 1 })
                .is_ok()
        );
        let event_tx = DaemonEventSender::with_deferred_capacity(raw_tx, 0);
        let shutdown = ShutdownLatch::new();
        let delivery_tx = event_tx.clone();
        let delivery_shutdown = shutdown.clone();
        let delivery = tokio::spawn(async move {
            deliver_terminal(
                &delivery_tx,
                &delivery_shutdown,
                DaemonEvent::TransportRecoveryRetry { generation: 2 },
            )
            .await
        });
        tokio::task::yield_now().await;

        assert!(matches!(
            event_rx.recv().await,
            Some(DaemonEvent::TransportRecoveryRetry { generation: 1 })
        ));
        assert!(delivery.await.unwrap());
        assert!(matches!(
            event_rx.recv().await,
            Some(DaemonEvent::TransportRecoveryRetry { generation: 2 })
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn personal_export_completion_is_owned_joined_and_retries_saturation() {
        let (raw_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        assert!(
            raw_tx
                .try_send(DaemonEvent::TransportRecoveryRetry { generation: 1 })
                .is_ok()
        );
        let event_tx = DaemonEventSender::with_deferred_capacity(raw_tx, 0);
        let shutdown = ShutdownLatch::new();
        let mut tasks = DaemonEffectTasks::new();

        assert!(tasks.schedule_personal_export(&event_tx, &shutdown, 7, || {
            Ok(PathBuf::from("/tmp/export.json"))
        }));
        assert_eq!(tasks.len(), 1);
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert!(matches!(
            event_rx.recv().await,
            Some(DaemonEvent::TransportRecoveryRetry { generation: 1 })
        ));
        let completion = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("personal export completion should retry bounded saturation");
        let Some(DaemonEvent::PersonalExportFinished(personal_export::Finished {
            generation,
            result,
        })) = completion
        else {
            panic!("expected personal export completion");
        };
        assert_eq!(generation, 7);
        assert_eq!(result.unwrap(), PathBuf::from("/tmp/export.json"));

        tasks.shutdown().await;
        assert_eq!(tasks.len(), 0);
    }
}
