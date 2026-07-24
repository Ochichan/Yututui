//! Off-loop portable-export orchestration for the headless daemon owner.

use std::path::PathBuf;

use super::DaemonEventSender;
use super::effects::DaemonEffectTasks;
use crate::config::Config;
use crate::library::Library;
use crate::personal_state::PersonalStateV2;
use crate::player::lifetime::ShutdownLatch;
use crate::remote::{RemoteReply, proto::RemoteResponse};
use crate::signals::Signals;
use crate::station::StationStore;

#[derive(Default)]
pub(super) struct PersonalExport {
    next_generation: u64,
    pending: Option<Pending>,
}

struct Pending {
    generation: u64,
    reply: RemoteReply,
}

pub(super) struct Finished {
    pub(super) generation: u64,
    pub(super) result: Result<PathBuf, String>,
}

pub(super) struct Target {
    directory: String,
    schema: u32,
}

impl Target {
    pub(super) fn new(directory: String, schema: u32) -> Self {
        Self { directory, schema }
    }
}

impl PersonalExport {
    pub(super) fn start_engine(
        &mut self,
        target: Target,
        reply: RemoteReply,
        engine: &super::engine::DaemonEngine,
        event_tx: &DaemonEventSender,
        shutdown: &ShutdownLatch,
        tasks: &mut DaemonEffectTasks,
    ) {
        self.start(
            target,
            reply,
            || engine.personal_export_sources(),
            event_tx,
            shutdown,
            tasks,
        );
    }

    pub(super) fn start<F>(
        &mut self,
        target: Target,
        reply: RemoteReply,
        sources: F,
        event_tx: &DaemonEventSender,
        shutdown: &ShutdownLatch,
        tasks: &mut DaemonEffectTasks,
    ) where
        F: FnOnce() -> Result<
            (
                PersonalStateV2,
                Config,
                Library,
                Signals,
                StationStore,
                usize,
            ),
            String,
        >,
    {
        let Target { directory, schema } = target;
        if self.pending.is_some() {
            let _ = reply.send(RemoteResponse::err_with_message(
                "personal_export_busy",
                "another personal-data export is already running".to_owned(),
            ));
            return;
        }
        let (personal_state, config, library, signals, station, live_estimated_bytes) =
            match sources() {
                Ok(sources) => sources,
                Err(message) => {
                    let _ = reply.send(RemoteResponse::err_with_message(
                        "personal_export_too_large",
                        message,
                    ));
                    return;
                }
            };

        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == 0 {
            self.next_generation = 1;
        }
        let generation = self.next_generation;
        self.pending = Some(Pending { generation, reply });

        let scheduled = tasks.schedule_personal_export(event_tx, shutdown, generation, move || {
            let playlists = crate::data_export::load_playlists_read_only(live_estimated_bytes)
                .map_err(|error| crate::util::sanitize::sanitize_error_text(error.to_string()))?;
            let destination = PathBuf::from(directory);
            let result = if schema == 1 {
                let snapshot = crate::data_export::ExportSnapshot::new(
                    &config, &library, &playlists, &signals, &station,
                );
                // Projection no longer borrows these stores; discard them before serialization
                // to keep peak memory bounded by one full representation at a time.
                drop((personal_state, config, library, playlists, signals, station));
                crate::data_export::export_snapshot(destination.as_path(), &snapshot)
            } else {
                crate::data_export::export_v2_from_sources(
                    destination.as_path(),
                    &personal_state,
                    &library,
                    &playlists,
                    &signals,
                    &station,
                )
            };
            result.map_err(|error| crate::util::sanitize::sanitize_error_text(error.to_string()))
        });
        if !scheduled {
            self.settle_generation(generation, RemoteResponse::err("shutting_down"));
        }
    }

    /// Complete the exact retained request for this worker generation. Taking `pending` is the
    /// busy-state transition and deliberately happens before the synchronous reply ordering
    /// barrier.
    pub(super) fn finish(&mut self, finished: Finished) {
        let Some(reply) = self.take_reply(finished.generation) else {
            tracing::debug!(
                generation = finished.generation,
                "retired stale personal-data export completion"
            );
            return;
        };
        let response = match finished.result {
            Ok(path) => RemoteResponse::ok(path.display().to_string()),
            Err(message) => RemoteResponse::err_with_message("personal_export_failed", message),
        };
        let _ = reply.send(response);
    }

    /// Settle an accepted request whose worker could not re-enter before owner admission closed.
    /// This runs before the daemon waits for wire settlements.
    pub(super) fn shutdown(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let _ = pending.reply.send(RemoteResponse::err("shutting_down"));
    }

    fn settle_generation(&mut self, generation: u64, response: RemoteResponse) {
        if let Some(reply) = self.take_reply(generation) {
            let _ = reply.send(response);
        }
    }

    fn take_reply(&mut self, generation: u64) -> Option<RemoteReply> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.generation == generation)
        {
            self.pending.take().map(|pending| pending.reply)
        } else {
            None
        }
    }

    #[cfg(test)]
    fn is_in_progress(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    fn event_channel() -> (
        DaemonEventSender,
        tokio::sync::mpsc::Receiver<super::super::DaemonEvent>,
    ) {
        let (raw, rx) = crate::util::backpressure::bounded_channel(
            crate::util::backpressure::DAEMON_EVENT_QUEUE,
        );
        (DaemonEventSender::new(raw), rx)
    }

    fn sources() -> (
        PersonalStateV2,
        Config,
        Library,
        Signals,
        StationStore,
        usize,
    ) {
        (
            PersonalStateV2::default(),
            Config::default(),
            Library::default(),
            Signals::default(),
            StationStore::default(),
            0,
        )
    }

    #[tokio::test]
    async fn duplicate_start_is_rejected_without_launching_more_work() {
        let (events, _event_rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        let mut tasks = DaemonEffectTasks::new();
        let (first_reply, first_response) = oneshot::channel();
        let mut export = PersonalExport {
            next_generation: 1,
            pending: Some(Pending {
                generation: 1,
                reply: first_reply.into(),
            }),
        };
        let (reply, response) = oneshot::channel();
        let sources_called = std::cell::Cell::new(false);

        export.start(
            Target::new("/unused".to_owned(), 2),
            reply.into(),
            || {
                sources_called.set(true);
                Ok(sources())
            },
            &events,
            &shutdown,
            &mut tasks,
        );

        let response = response.await.expect("busy response");
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some("personal_export_busy"));
        assert!(export.is_in_progress());
        assert!(
            !sources_called.get(),
            "busy requests must not clone sources"
        );
        export.shutdown();
        assert_eq!(
            first_response
                .await
                .expect("retained request shutdown response")
                .reason
                .as_deref(),
            Some("shutting_down")
        );
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn unsafe_live_source_is_rejected_before_busy_or_worker_start() {
        let (events, _event_rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        let mut tasks = DaemonEffectTasks::new();
        let mut export = PersonalExport::default();
        let (reply, response) = oneshot::channel();

        export.start(
            Target::new("/unused".to_owned(), 2),
            reply.into(),
            || Err("live source exceeds safe clone budget".to_owned()),
            &events,
            &shutdown,
            &mut tasks,
        );

        let response = response.await.expect("preflight rejection");
        assert!(!response.ok);
        assert_eq!(
            response.reason.as_deref(),
            Some("personal_export_too_large")
        );
        assert!(!export.is_in_progress());
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_race_clears_busy_and_settles_without_spawning() {
        let (events, _event_rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        shutdown.trigger();
        let mut tasks = DaemonEffectTasks::new();
        let mut export = PersonalExport::default();
        let (reply, response) = oneshot::channel();

        export.start(
            Target::new("/unused".to_owned(), 2),
            reply.into(),
            || Ok(sources()),
            &events,
            &shutdown,
            &mut tasks,
        );

        assert!(!export.is_in_progress());
        assert_eq!(
            response.await.expect("shutdown response").reason.as_deref(),
            Some("shutting_down")
        );
        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn finish_clears_busy_and_returns_the_published_path() {
        let (reply, response) = oneshot::channel();
        let mut export = PersonalExport {
            next_generation: 7,
            pending: Some(Pending {
                generation: 7,
                reply: reply.into(),
            }),
        };
        export.finish(Finished {
            generation: 7,
            result: Ok(PathBuf::from("/tmp/export.json")),
        });

        assert!(!export.is_in_progress());
        let response = response.await.expect("success response");
        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("/tmp/export.json"));
    }

    #[tokio::test]
    async fn stale_completion_cannot_clear_or_reply_to_the_current_generation() {
        let (reply, mut response) = oneshot::channel();
        let mut export = PersonalExport {
            next_generation: 8,
            pending: Some(Pending {
                generation: 8,
                reply: reply.into(),
            }),
        };

        export.finish(Finished {
            generation: 7,
            result: Ok(PathBuf::from("/tmp/stale.json")),
        });

        assert!(export.is_in_progress());
        assert!(response.try_recv().is_err());
        export.shutdown();
        assert_eq!(
            response.await.expect("shutdown response").reason.as_deref(),
            Some("shutting_down")
        );
    }
}
