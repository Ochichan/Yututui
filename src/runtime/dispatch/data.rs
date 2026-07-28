//! Runtime dispatch for data, sync, and export commands.

use super::super::*;

impl RuntimeHandles {
    pub(super) fn dispatch_data(&mut self, app: &mut App, cmd: DataCmd) {
        match cmd {
            DataCmd::SyncUi(command) => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("sync_settings", move || {
                        let event = crate::runtime::sync_ui_worker::run(command);
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Data(
                            crate::app::DataMsg::SyncUi(event),
                        )));
                    });
            }
            DataCmd::PersonalSync {
                action,
                attempt,
                personal_state,
                revision_guard,
                reply,
            } => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                let rejected_action = action.clone();
                let rejected_reply = reply.clone();
                let completed_action = action.clone();
                let spawned = crate::sync::spawn_detached_prepare(
                    move || {
                        crate::sync::SyncPaths::current()
                            .map_err(crate::sync::service::SyncServiceError::from)
                            .and_then(|paths| {
                                let revoke_target = match &action {
                                    crate::app::PersonalSyncAction::SyncNow
                                    | crate::app::PersonalSyncAction::AutomaticSync => None,
                                    crate::app::PersonalSyncAction::Revoke(device_id) => {
                                        Some(device_id)
                                    }
                                };
                                let kind = if matches!(
                                    action,
                                    crate::app::PersonalSyncAction::AutomaticSync
                                ) {
                                    crate::sync::service::SyncAttemptKind::Automatic
                                } else {
                                    crate::sync::service::SyncAttemptKind::Manual
                                };
                                crate::sync::service::prepare_owner_sync_as(
                                    &personal_state,
                                    revoke_target,
                                    &paths,
                                    kind,
                                    &revision_guard,
                                )
                            })
                    },
                    move |result| {
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Data(
                            crate::app::DataMsg::PersonalSyncPrepared(Box::new(
                                crate::app::PersonalSyncPrepared {
                                    action: completed_action,
                                    attempt,
                                    result,
                                    reply,
                                },
                            )),
                        )));
                    },
                );
                if !spawned {
                    self.reduce_owner_msg(
                        app,
                        Msg::Data(crate::app::DataMsg::PersonalSyncPrepared(Box::new(
                            crate::app::PersonalSyncPrepared {
                                action: rejected_action,
                                attempt,
                                result: Err(crate::sync::service::SyncServiceError::Storage),
                                reply: rejected_reply,
                            },
                        ))),
                    );
                }
            }
            DataCmd::PersonalDataExport(PersonalDataExportCmd::Export {
                directory,
                schema,
                sources,
                reply,
            }) => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("personal_data_export", move || {
                        let result = if schema == 1 {
                            let snapshot = crate::data_export::ExportSnapshot::new(
                                &sources.config,
                                &sources.library,
                                &sources.playlists,
                                &sources.signals,
                                &sources.station,
                            );
                            drop(sources);
                            crate::data_export::export_snapshot(&directory, &snapshot)
                        } else {
                            crate::data_export::export_v2_from_sources(
                                &directory,
                                &sources.personal_state,
                                sources.personal_state_device_id.as_ref(),
                                &sources.library,
                                &sources.playlists,
                                &sources.signals,
                                &sources.station,
                            )
                        }
                        .map_err(|error| {
                            crate::util::sanitize::sanitize_error_text(error.to_string())
                        });
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Data(
                            crate::app::DataMsg::PersonalDataExport(
                                crate::app::PersonalDataExportMsg::Finished { result, reply },
                            ),
                        )));
                    });
            }
            DataCmd::ScanDownloads(dir) => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("scan_downloads_data", move || {
                        let scan = crate::library::scan_downloads(&dir);
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Data(
                            crate::app::DataMsg::DownloadsScanned(scan),
                        )));
                    });
            }
        }
    }
}
