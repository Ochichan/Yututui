use std::path::PathBuf;

use super::{RuntimeEvent, RuntimeHandles};
use crate::app::{App, Msg};
use crate::recorder::job::RecorderJob;

impl RuntimeHandles {
    pub(super) fn dispatch_recorder(&mut self, app: &mut App, job: RecorderJob) {
        match job {
            save @ RecorderJob::Save { .. } => {
                // The tiny journal+fsync is the Save acceptance boundary. It deliberately
                // precedes blocking-pool admission: if shutdown rejects or hard-cuts the worker,
                // next startup still recovers the explicit user action.
                match crate::recorder::job::accept_save(save) {
                    Ok(accepted) => {
                        let id = accepted.id();
                        self.reduce_owner_msg(
                            app,
                            Msg::Recorder(crate::recorder::job::RecorderEvent::SaveAccepted { id }),
                        );
                        let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                        self.background_tasks
                            .spawn_blocking("recorder_save", move || {
                                let event = crate::recorder::job::run_accepted(accepted);
                                emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Recorder(
                                    event,
                                )));
                            });
                    }
                    Err(event) => self.reduce_owner_msg(app, Msg::Recorder(event)),
                }
            }
            other => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("recorder_file_job", move || {
                        if let Some(event) = crate::recorder::job::run(other) {
                            emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Recorder(event)));
                        }
                    });
            }
        }
    }

    /// Recover durably accepted recorder Saves before ordinary temp recordings are wiped.
    ///
    /// Startup awaits the result, but all filesystem work runs in (and remains owned by) the
    /// same tracked blocking set used for live recorder jobs.
    pub async fn recover_recordings(
        &mut self,
        temp_dir: PathBuf,
        final_dir: PathBuf,
    ) -> crate::recorder::job::RecoveryReport {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let admitted = self
            .background_tasks
            .spawn_blocking("recorder_recovery", move || {
                let report = crate::recorder::job::recover_pending(&temp_dir, &final_dir);
                let _ = tx.send(report);
            });
        if !admitted {
            return crate::recorder::job::RecoveryReport {
                warnings: vec!["recording recovery was rejected during shutdown".to_owned()],
                admission_uncertain: true,
                ..Default::default()
            };
        }
        let report = rx
            .await
            .unwrap_or_else(|error| crate::recorder::job::RecoveryReport {
                warnings: vec![format!("recording recovery task stopped: {error}")],
                admission_uncertain: true,
                ..Default::default()
            });
        // The worker sends immediately before returning its tracked label. Yield once so the
        // normal non-blocking reap usually observes that completion here; if not, dispatch or
        // teardown still owns and joins it.
        tokio::task::yield_now().await;
        self.background_tasks.reap_finished();
        report
    }
}
