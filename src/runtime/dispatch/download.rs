//! Runtime dispatch for download actor and filesystem commands.

use super::super::*;

impl RuntimeHandles {
    pub(super) fn dispatch_download(&mut self, app: &mut App, cmd: DownloadCmd) {
        match cmd {
            DownloadCmd::Scan(dir) => {
                // Directory scan does per-file IO — keep it off the loop task too.
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("scan_downloads", move || {
                        let scan = crate::library::scan_downloads(&dir);
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Data(
                            crate::app::DataMsg::DownloadsScanned(scan),
                        )));
                    });
            }
            DownloadCmd::Delete { paths, root } => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("delete_downloads", move || {
                        let (deleted, failures) =
                            crate::download::delete_download_files(paths, &root);
                        for (path, error) in &failures {
                            tracing::warn!(
                                path = %crate::util::sanitize::sanitize_error_text(path.display().to_string()),
                                error = %crate::util::sanitize::sanitize_error_text(error.to_string()),
                                "refused or failed to delete downloaded file"
                            );
                        }
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::DownloadsDeleted {
                            root,
                            deleted,
                            failed: failures.len(),
                        }));
                    });
            }
            DownloadCmd::Start(song) => {
                let import_metadata_present =
                    song.import_session_id.is_some() || song.import_source_order.is_some();
                let result = match crate::download::import_request_for_song(&song) {
                    Ok(Some(request)) => Some(self.download_handle.start_for_import(request)),
                    Ok(None) if import_metadata_present => {
                        let follow_ups =
                            app.update(Msg::Download(crate::app::DownloadMsg::Rejected {
                                tracking_key: crate::download::download_tracking_key(&song),
                                error: "Import session row is unavailable; refresh and retry."
                                    .to_owned(),
                            }));
                        for follow_up in follow_ups {
                            self.dispatch(app, follow_up);
                        }
                        None
                    }
                    Err(error) if import_metadata_present => {
                        tracing::warn!(%error, "import download admission failed");
                        let follow_ups =
                            app.update(Msg::Download(crate::app::DownloadMsg::Rejected {
                                tracking_key: crate::download::download_tracking_key(&song),
                                error: format!("Import download was not admitted: {error:#}"),
                            }));
                        for follow_up in follow_ups {
                            self.dispatch(app, follow_up);
                        }
                        None
                    }
                    Ok(None) => Some(self.download_handle.start(*song)),
                    Err(error) => {
                        tracing::warn!(%error, "ordinary download metadata admission failed");
                        Some(self.download_handle.start(*song))
                    }
                };
                if let Some(Err(error)) = result {
                    tracing::warn!(video_id = %error.video_id, "download request rejected; surfacing retry status");
                    for follow_up in recover_download_admission(app, error) {
                        self.dispatch(app, follow_up);
                    }
                }
            }
            DownloadCmd::SetDir(dir) => {
                if let Err(error) = self.download_handle.set_dir(dir) {
                    tracing::warn!(dir = %error.dir().display(), %error, "could not update download directory");
                    let follow_ups = app.update(Msg::Download(crate::app::DownloadMsg::DirError {
                        error: error.to_string(),
                    }));
                    for follow_up in follow_ups {
                        self.dispatch(app, follow_up);
                    }
                }
            }
        }
    }
}
