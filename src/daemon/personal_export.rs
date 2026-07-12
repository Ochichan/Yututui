//! Off-loop portable-export orchestration for the headless daemon owner.

use std::path::PathBuf;

#[cfg(test)]
use tokio::sync::oneshot;

use super::{DaemonEvent, DaemonEventSender, emit_daemon_event};
use crate::config::Config;
use crate::library::Library;
use crate::remote::{RemoteReply, proto::RemoteResponse};
use crate::signals::Signals;
use crate::station::StationStore;

#[derive(Default)]
pub(super) struct PersonalExport {
    in_progress: bool,
}

pub(super) struct Finished {
    pub reply: RemoteReply,
    pub result: Result<PathBuf, String>,
}

impl PersonalExport {
    pub(super) fn start_engine(
        &mut self,
        directory: String,
        reply: RemoteReply,
        engine: &super::engine::DaemonEngine,
        event_tx: DaemonEventSender,
    ) {
        self.start(
            directory,
            reply,
            || engine.personal_export_sources(),
            event_tx,
        );
    }

    pub(super) fn start<F>(
        &mut self,
        directory: String,
        reply: RemoteReply,
        sources: F,
        event_tx: DaemonEventSender,
    ) where
        F: FnOnce() -> Result<(Config, Library, Signals, StationStore, usize), String>,
    {
        if self.in_progress {
            let _ = reply.send(RemoteResponse::err_with_message(
                "personal_export_busy",
                "another personal-data export is already running".to_owned(),
            ));
            return;
        }
        let (config, library, signals, station, live_estimated_bytes) = match sources() {
            Ok(sources) => sources,
            Err(message) => {
                let _ = reply.send(RemoteResponse::err_with_message(
                    "personal_export_too_large",
                    message,
                ));
                return;
            }
        };
        self.in_progress = true;
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let playlists = crate::data_export::load_playlists_read_only(live_estimated_bytes)
                    .map_err(|error| {
                        crate::util::sanitize::sanitize_error_text(error.to_string())
                    })?;
                let snapshot = crate::data_export::ExportSnapshot::new(
                    &config, &library, &playlists, &signals, &station,
                );
                // Projection no longer borrows these stores; discard them before serialization to
                // keep a daemon export's peak memory bounded by one full representation at a time.
                drop((config, library, playlists, signals, station));
                crate::data_export::export_snapshot(PathBuf::from(directory).as_path(), &snapshot)
                    .map_err(|error| crate::util::sanitize::sanitize_error_text(error.to_string()))
            })
            .await
            .unwrap_or_else(|error| Err(format!("personal-data export worker failed: {error}")));
            emit_daemon_event(
                &event_tx,
                DaemonEvent::PersonalExportFinished(Finished { reply, result }),
            );
        });
    }

    pub(super) fn finish(&mut self, finished: Finished) {
        self.in_progress = false;
        let response = match finished.result {
            Ok(path) => RemoteResponse::ok(path.display().to_string()),
            Err(message) => RemoteResponse::err_with_message("personal_export_failed", message),
        };
        let _ = finished.reply.send(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn duplicate_start_is_rejected_without_launching_more_work() {
        let (raw, _rx) = crate::util::backpressure::bounded_channel(
            crate::util::backpressure::DAEMON_EVENT_QUEUE,
        );
        let events = DaemonEventSender::new(raw);
        let mut export = PersonalExport { in_progress: true };
        let (reply, response) = oneshot::channel();
        let sources_called = std::cell::Cell::new(false);

        export.start(
            "/unused".to_owned(),
            reply.into(),
            || {
                sources_called.set(true);
                Ok((
                    Config::default(),
                    Library::default(),
                    Signals::default(),
                    StationStore::default(),
                    0,
                ))
            },
            events,
        );

        let response = response.await.expect("busy response");
        assert!(!response.ok);
        assert_eq!(response.reason.as_deref(), Some("personal_export_busy"));
        assert!(export.in_progress);
        assert!(
            !sources_called.get(),
            "busy requests must not clone sources"
        );
    }

    #[tokio::test]
    async fn unsafe_live_source_is_rejected_before_busy_or_worker_start() {
        let (raw, _rx) = crate::util::backpressure::bounded_channel(
            crate::util::backpressure::DAEMON_EVENT_QUEUE,
        );
        let events = DaemonEventSender::new(raw);
        let mut export = PersonalExport::default();
        let (reply, response) = oneshot::channel();

        export.start(
            "/unused".to_owned(),
            reply.into(),
            || Err("live source exceeds safe clone budget".to_owned()),
            events,
        );

        let response = response.await.expect("preflight rejection");
        assert!(!response.ok);
        assert_eq!(
            response.reason.as_deref(),
            Some("personal_export_too_large")
        );
        assert!(!export.in_progress);
    }

    #[tokio::test]
    async fn finish_clears_busy_and_returns_the_published_path() {
        let mut export = PersonalExport { in_progress: true };
        let (reply, response) = oneshot::channel();
        export.finish(Finished {
            reply: reply.into(),
            result: Ok(PathBuf::from("/tmp/export.json")),
        });

        let response = response.await.expect("success response");
        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("/tmp/export.json"));
        assert!(!export.in_progress);
    }
}
