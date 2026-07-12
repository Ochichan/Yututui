//! Off-owner execution and re-entry for effects emitted by the daemon engine.

use std::future::Future;
use std::time::Duration;

use tokio::task::{JoinError, JoinSet};

use super::{DaemonEvent, DaemonEventSender, engine, gui_search_pending::GuiSearchPending};
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
    gui_search_pending: &mut GuiSearchPending,
    effects: Vec<engine::EngineEffect>,
) -> Vec<DaemonEvent> {
    dispatch_engine_effects_inner(
        api,
        event_tx,
        shutdown,
        tasks,
        gui_search_pending,
        None,
        effects,
    )
}

pub(super) fn dispatch_session_engine_effects(
    api: &crate::api::ApiHandle,
    event_tx: &DaemonEventSender,
    shutdown: &ShutdownLatch,
    tasks: &mut DaemonEffectTasks,
    gui_search_pending: &mut GuiSearchPending,
    origin: &crate::remote::RemoteSessionScope,
    effects: Vec<engine::EngineEffect>,
) -> Vec<DaemonEvent> {
    dispatch_engine_effects_inner(
        api,
        event_tx,
        shutdown,
        tasks,
        gui_search_pending,
        Some(origin),
        effects,
    )
}

fn dispatch_engine_effects_inner(
    api: &crate::api::ApiHandle,
    event_tx: &DaemonEventSender,
    shutdown: &ShutdownLatch,
    tasks: &mut DaemonEffectTasks,
    gui_search_pending: &mut GuiSearchPending,
    session_origin: Option<&crate::remote::RemoteSessionScope>,
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
                seed,
                seed_video_id,
                exclude_ids,
                limit,
                mode,
                config,
            } => {
                if let Err(error) = api.streaming(
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
                            seed_video_id,
                            error: error.to_string(),
                        }));
                    }
                }
            }
            engine::EngineEffect::StreamingPreflight {
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => {
                if let Err(error) =
                    api.streaming_preflight(seed_video_id.clone(), picks, fallback, mode, config)
                {
                    tracing::warn!(%error, "api command enqueue failed");
                    if !shutdown.is_triggered() {
                        terminal.push(DaemonEvent::Api(crate::api::ApiEvent::StreamingError {
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
            engine::EngineEffect::GuiSearch {
                requester: requester_key,
                ticket,
                query,
                source,
                config,
            } => {
                let Some(origin) = session_origin.filter(|origin| {
                    requester_key.session_id() == origin.session_id()
                        && requester_key.page_id() == origin.page_id()
                }) else {
                    tracing::error!(
                        session_id = requester_key.session_id(),
                        page_id = ?requester_key.page_id(),
                        "GUI search effect did not match its owner-turn session origin"
                    );
                    continue;
                };
                let request_id = gui_search_pending.begin(
                    requester_key,
                    origin.clone(),
                    ticket,
                    query.clone(),
                    source,
                );
                if let Err(error) = api.gui_search(request_id, query, source, config) {
                    tracing::warn!(%error, "api command enqueue failed");
                    if !shutdown.is_triggered() {
                        terminal.push(DaemonEvent::Api(crate::api::ApiEvent::GuiSearchCompleted {
                            request_id,
                            groups: vec![crate::api::GuiSearchGroup {
                                source,
                                songs: Vec::new(),
                                error: Some(error.to_string()),
                            }],
                        }));
                    }
                }
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
    async fn gui_search_enqueue_failure_keeps_requester_route_for_terminal_completion() {
        let (interactive_tx, interactive_rx) = tokio::sync::mpsc::channel(1);
        let (bulk_tx, _bulk_rx) = tokio::sync::mpsc::channel(1);
        drop(interactive_rx);
        let api = crate::api::ApiHandle::from_test_senders(interactive_tx, bulk_tx);
        let (event_tx, _event_rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        let mut tasks = DaemonEffectTasks::new();
        let mut pending = GuiSearchPending::default();
        let requester = crate::remote::RemoteSessionScope::for_test(44, Some("page"));
        let requester_key = engine::RequesterKey::new(44, Some("page".to_owned()));

        let terminal = dispatch_session_engine_effects(
            &api,
            &event_tx,
            &shutdown,
            &mut tasks,
            &mut pending,
            &requester,
            vec![engine::EngineEffect::GuiSearch {
                requester: requester_key,
                ticket: 8,
                query: "needle".to_owned(),
                source: crate::search_source::SearchSource::Youtube,
                config: crate::search_source::SearchConfig::default(),
            }],
        );

        let [DaemonEvent::Api(crate::api::ApiEvent::GuiSearchCompleted { request_id, groups })] =
            terminal.as_slice()
        else {
            panic!("enqueue failure must synthesize one terminal GUI completion");
        };
        assert_eq!(groups.len(), 1);
        assert!(groups[0].error.is_some());
        let route = pending
            .take(*request_id)
            .expect("synthesized completion must retain its requester route");
        assert_eq!(route.requester.session_id(), 44);
        assert_eq!(route.requester.page_id(), Some("page"));
        assert_eq!(route.requester_key.session_id(), 44);
        assert_eq!(route.requester_key.page_id(), Some("page"));
        assert_eq!(route.ticket, 8);
        assert_eq!(route.query, "needle");
        tasks.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gui_search_effect_cannot_bind_to_a_different_session_turn() {
        let (interactive_tx, mut interactive_rx) = tokio::sync::mpsc::channel(1);
        let (bulk_tx, _bulk_rx) = tokio::sync::mpsc::channel(1);
        let api = crate::api::ApiHandle::from_test_senders(interactive_tx, bulk_tx);
        let (event_tx, _event_rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        let mut tasks = DaemonEffectTasks::new();
        let mut pending = GuiSearchPending::default();
        let origin = crate::remote::RemoteSessionScope::for_test(44, Some("page"));

        let terminal = dispatch_session_engine_effects(
            &api,
            &event_tx,
            &shutdown,
            &mut tasks,
            &mut pending,
            &origin,
            vec![engine::EngineEffect::GuiSearch {
                requester: engine::RequesterKey::new(45, Some("page".to_owned())),
                ticket: 8,
                query: "needle".to_owned(),
                source: crate::search_source::SearchSource::Youtube,
                config: crate::search_source::SearchConfig::default(),
            }],
        );

        assert!(terminal.is_empty());
        assert_eq!(pending.len(), 0);
        assert!(interactive_rx.try_recv().is_err());
        tasks.shutdown().await;
    }
}
