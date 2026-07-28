//! Runtime dispatch for local-library and import commands.

use super::super::*;

impl RuntimeHandles {
    pub(super) fn dispatch_local(&mut self, cmd: crate::app::LocalCmd) {
        match cmd {
            crate::app::LocalCmd::LoadIndex { index_path } => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("local_load_index", move || {
                        let load = index_path
                            .as_deref()
                            .map(crate::local::LocalIndex::load_with_diagnostics)
                            .unwrap_or_default();
                        let warnings = load
                            .warnings
                            .into_iter()
                            .map(|warning| crate::local::ScanError {
                                path: warning.path,
                                message: warning.message,
                            })
                            .collect();
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                            crate::app::LocalMsg::IndexLoaded {
                                index_path,
                                index: load.index,
                                warnings,
                            },
                        )));
                    });
            }
            crate::app::LocalCmd::ScanRoots {
                roots,
                index_path,
                previous,
            } => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("local_scan_roots", move || {
                        let progress_emitter = emitter.clone();
                        let mut result =
                            crate::local::scan_roots_with_progress(&roots, &previous, |progress| {
                                progress_emitter.emit(RuntimeEvent::App(Msg::Local(
                                    crate::app::LocalMsg::ScanProgress(progress),
                                )));
                            });
                        if let Some(path) = index_path.as_deref()
                            && let Err(error) = result.index.save(path)
                        {
                            result.errors.push(crate::local::ScanError {
                                path: path.to_path_buf(),
                                message: format!("could not save local index: {error}"),
                            });
                            result.summary.errors = result.errors.len();
                        }
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                            crate::app::LocalMsg::ScanFinished { index_path, result },
                        )));
                    });
            }
            crate::app::LocalCmd::ReviewImport {
                op_id,
                session_id,
                source_order,
                action,
            } => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("review_import", move || {
                        let t0 = std::time::Instant::now();
                        let result = match action {
                            crate::app::ImportReviewAction::AcceptFirst => {
                                crate::transfer::review_action::accept_first_candidate(
                                    &session_id,
                                    source_order,
                                )
                            }
                            crate::app::ImportReviewAction::ChooseNext => {
                                crate::transfer::review_action::choose_next_candidate(
                                    &session_id,
                                    source_order,
                                )
                            }
                            crate::app::ImportReviewAction::Reject => {
                                crate::transfer::review_action::reject_row(
                                    &session_id,
                                    source_order,
                                )
                            }
                            crate::app::ImportReviewAction::Skip => {
                                crate::transfer::review_action::skip_row(&session_id, source_order)
                            }
                        }
                        .map_err(|error| format!("{error:#}"));
                        let elapsed_ms = t0.elapsed().as_millis();
                        tracing::debug!(
                            session_id = %session_id,
                            source_order,
                            ?action,
                            elapsed_ms,
                            "finished import review action"
                        );
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                            crate::app::LocalMsg::ImportReviewFinished {
                                op_id,
                                session_id,
                                source_order,
                                action,
                                result,
                                elapsed_ms,
                            },
                        )));
                    });
            }
            crate::app::LocalCmd::ReviewImportAcceptAll { op_id, session_id } => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("review_import_accept_all", move || {
                        let t0 = std::time::Instant::now();
                        let result =
                            crate::transfer::review_action::accept_all_candidates(&session_id)
                                .map_err(|error| format!("{error:#}"));
                        let elapsed_ms = t0.elapsed().as_millis();
                        tracing::debug!(
                            session_id = %session_id,
                            elapsed_ms,
                            "finished import review accept all"
                        );
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                            crate::app::LocalMsg::ImportReviewAcceptAllFinished {
                                op_id,
                                session_id,
                                result,
                                elapsed_ms,
                            },
                        )));
                    });
            }
            crate::app::LocalCmd::BuildFindCorpus {
                generation,
                tracks,
                playlists,
                revision,
                options,
            } => self.dispatch_local_find_build(generation, tracks, playlists, revision, options),
            crate::app::LocalCmd::EvaluateFind {
                request_id,
                generation,
                corpus,
                query,
                scope,
                sort,
            } => self.dispatch_local_find_query(request_id, generation, corpus, query, scope, sort),
            crate::app::LocalCmd::CancelFindEvaluations => {
                self.cancel_local_find_queries();
            }
        }
    }
}
