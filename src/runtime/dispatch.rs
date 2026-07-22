//! Dispatch reducer commands to runtime-owned actors and background jobs.

use super::*;

impl RuntimeHandles {
    pub fn dispatch(&mut self, app: &mut App, cmd: Cmd) {
        self.background_tasks.reap_finished();
        if let Some(component) = read_only::durable_mutation_component(&cmd) {
            let reason = durable_mutation_rejection_reason(self.persistence_read_only.as_ref());
            if let Some(reason) = reason {
                for follow_up in read_only::reject_mutation(app, &cmd, component, &reason) {
                    self.dispatch(app, follow_up);
                }
                return;
            }
        }
        match cmd {
            Cmd::PlayerControl(PlayerControl::Restart { restore }) => {
                self.handle_player_transport_closed(app, restore);
            }
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => {
                self.dispatch_player_intent(app, intent);
            }
            // dispatch runs synchronously right after each update, so the connect for a
            // spawn generation is always installed before any VideoLoad that follows it.
            Cmd::VideoConnect {
                ipc_path,
                generation,
                bindings,
            } => {
                let tx = self.worker_tx.clone();
                self.video_handle = Some(crate::player::video::connect(
                    ipc_path,
                    generation,
                    bindings,
                    move |generation, event| {
                        emit_callback_observed(&tx, RuntimeEvent::Video { generation, event });
                    },
                ));
            }
            Cmd::VideoLoad(url) => {
                let result =
                    self.send_video_cmd(crate::player::video::VideoCmd::Load(url), "video_load");
                if result.is_err() {
                    // Drop the rejected generation before closing its process so no stale
                    // pending load can later reach an overlay which no longer represents state.
                    self.video_handle = None;
                }
                for follow_up in settle_video_load_delivery(app, result) {
                    self.dispatch(app, follow_up);
                }
            }
            Cmd::VideoTogglePause => {
                let result =
                    self.send_video_cmd(crate::player::video::VideoCmd::CyclePause, "video_pause");
                report_player_delivery(app, "video_pause", result);
            }
            Cmd::VideoToggleFullscreen => {
                let result = self.send_video_cmd(
                    crate::player::video::VideoCmd::CycleFullscreen,
                    "video_fullscreen",
                );
                report_player_delivery(app, "video_fullscreen", result);
            }
            Cmd::VideoToggleMute => {
                let result =
                    self.send_video_cmd(crate::player::video::VideoCmd::CycleMute, "video_mute");
                report_player_delivery(app, "video_mute", result);
            }
            Cmd::UpdateSeen { tag } => crate::update::mark_notified(&tag),
            Cmd::Search(search_cmd) => match search_cmd {
                SearchCmd::Query {
                    request_id,
                    query,
                    source,
                    config,
                } => {
                    if let Err(error) = self.api_handle.search(request_id, query, source, config) {
                        tracing::warn!(%error, "api command enqueue failed");
                        self.reduce_owner_msg(
                            app,
                            Msg::Search(SearchMsg::Error {
                                request_id,
                                source,
                                error: error.to_string(),
                            }),
                        );
                    }
                }
                SearchCmd::Playlists { request_id, query } => {
                    if let Err(error) = self.api_handle.search_playlists(request_id, query) {
                        tracing::warn!(%error, "api command enqueue failed");
                        self.reduce_owner_msg(
                            app,
                            Msg::Search(SearchMsg::Error {
                                request_id,
                                source: crate::search_source::SearchSource::Youtube,
                                error: error.to_string(),
                            }),
                        );
                    }
                }
                SearchCmd::Artists { request_id, query } => {
                    if let Err(error) = self.api_handle.search_artists(request_id, query) {
                        tracing::warn!(%error, "api command enqueue failed");
                        self.reduce_owner_msg(
                            app,
                            Msg::Search(SearchMsg::Error {
                                request_id,
                                source: crate::search_source::SearchSource::Youtube,
                                error: error.to_string(),
                            }),
                        );
                    }
                }
                SearchCmd::PlaylistTracks {
                    playlist_id,
                    title,
                    intent,
                } => {
                    if let Err(error) =
                        self.api_handle
                            .playlist_tracks(playlist_id, title.clone(), intent)
                    {
                        tracing::warn!(%error, "api command enqueue failed");
                        self.reduce_owner_msg(
                            app,
                            Msg::Search(SearchMsg::PlaylistTracksError {
                                title,
                                error: error.to_string(),
                            }),
                        );
                    }
                }
                SearchCmd::ArtistPage {
                    channel_id,
                    title,
                    intent,
                } => {
                    if let Err(error) =
                        self.api_handle
                            .artist_page(channel_id, title.clone(), intent)
                    {
                        tracing::warn!(%error, "api command enqueue failed");
                        self.reduce_owner_msg(
                            app,
                            Msg::Search(SearchMsg::ArtistPageError {
                                title,
                                error: error.to_string(),
                            }),
                        );
                    }
                }
            },
            // Persist: hand the persistence actor an owned snapshot (or clear one). Cloning a
            // store is a couple ms of memcpy at worst; the fsync it replaces on this task was
            // 5-50ms. The marker variants clone the live snapshot from `app` here; `Config`
            // carries its own owned snapshot.
            Cmd::Persist(PersistCmd::TransferPlaylistCommit(commit)) => {
                self.dispatch_transfer_playlist_commit(commit);
            }
            Cmd::Persist(p) => {
                let result = persist_delivery::admit(&self.persist, app, p);
                report_actor_delivery(app, "persistence", result);
            }
            Cmd::Data(cmd) => match cmd {
                DataCmd::PersonalDataExport(PersonalDataExportCmd::Export {
                    directory,
                    sources,
                    reply,
                }) => {
                    let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                    self.background_tasks
                        .spawn_blocking("personal_data_export", move || {
                            let snapshot = crate::data_export::ExportSnapshot::new(
                                &sources.config,
                                &sources.library,
                                &sources.playlists,
                                &sources.signals,
                                &sources.station,
                            );
                            drop(sources);
                            let result = crate::data_export::export_snapshot(&directory, &snapshot)
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
            },
            Cmd::Download(DownloadCmd::Scan(dir)) => {
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
            Cmd::Download(DownloadCmd::Delete { paths, root }) => {
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
                        emitter.emit_terminal_blocking(RuntimeEvent::App(
                            Msg::DownloadsDeleted {
                                root,
                                deleted,
                                failed: failures.len(),
                            },
                        ));
                    });
            }
            Cmd::Local(cmd) => match cmd {
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
                            let mut result = crate::local::scan_roots_with_progress(
                                &roots,
                                &previous,
                                |progress| {
                                    progress_emitter.emit(RuntimeEvent::App(Msg::Local(
                                        crate::app::LocalMsg::ScanProgress(progress),
                                    )));
                                },
                            );
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
                                    crate::transfer::review_action::skip_row(
                                        &session_id,
                                        source_order,
                                    )
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
                } => {
                    self.dispatch_local_find_build(generation, tracks, playlists, revision, options)
                }
                crate::app::LocalCmd::EvaluateFind {
                    request_id,
                    generation,
                    corpus,
                    query,
                    scope,
                    sort,
                } => self
                    .dispatch_local_find_query(request_id, generation, corpus, query, scope, sort),
                crate::app::LocalCmd::CancelFindEvaluations => {
                    self.cancel_local_find_queries();
                }
            },
            Cmd::Recorder(job) => {
                self.dispatch_recorder(app, job);
            }
            Cmd::FetchLyrics(request) => {
                if !report_actor_delivery(app, "lyrics", self.lyrics_handle.fetch(request)) {
                    recover_actor_rejection(app, ActorRejectionRecovery::Lyrics);
                }
            }
            Cmd::FetchArtwork { video_id, source } => {
                if !report_actor_delivery(
                    app,
                    "artwork",
                    self.artwork_handle.fetch(video_id, source),
                ) {
                    recover_actor_rejection(app, ActorRejectionRecovery::Artwork);
                }
            }
            Cmd::Download(DownloadCmd::Start(song)) => {
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
            Cmd::Download(DownloadCmd::SetDir(dir)) => {
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
            Cmd::Resolve {
                video_id,
                watch_url,
            } => {
                let result = self.resolver_handle.resolve(video_id.clone(), watch_url);
                for follow_up in settle_resolver_admission(app, video_id, result) {
                    self.dispatch(app, follow_up);
                }
            }
            Cmd::ResolveForSelfHeal {
                video_id,
                watch_url,
            } => {
                let result = self
                    .resolver_handle
                    .resolve_for_self_heal(video_id.clone(), watch_url);
                for follow_up in settle_resolver_admission(app, video_id, result) {
                    self.dispatch(app, follow_up);
                }
            }
            Cmd::YtdlpSelfHeal { video_id, tools } => {
                // Off-loop: an update check downloads up to ~40 MiB. Progress rides the
                // same Tools status-line events as the maintainer; the verdict returns
                // as Msg::YtdlpHealResult for the reducer's retry-or-skip decision.
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_cancellable("ytdlp_self_heal", async move {
                        let progress_emitter = emitter.clone();
                        crate::tools::ytdlp::clear_probe_cache();
                        let outcome = crate::tools::ytdlp::rollback_or_check_and_update(
                            &tools,
                            &move |event| {
                                progress_emitter.emit(RuntimeEvent::Tools(event));
                            },
                            "playback self-heal",
                        )
                        .await;
                        let updated = matches!(
                            outcome,
                            crate::tools::ytdlp::UpdateOutcome::Installed { .. }
                        );
                        emitter
                            .emit_terminal(RuntimeEvent::App(Msg::YtdlpHealResult {
                                video_id,
                                updated,
                            }))
                            .await;
                    });
            }
            Cmd::AskAi { prompt, context } => {
                let result = self.ai_handle.as_ref().map_or_else(
                    || Err(crate::util::delivery::DeliveryError::Closed),
                    |handle| handle.ask(prompt, context),
                );
                if !report_actor_delivery(app, "ai.ask", result) {
                    recover_actor_rejection(app, ActorRejectionRecovery::AiTurn);
                }
            }
            Cmd::ResolveTrack { seq, query, config } => {
                if let Err(error) = self.api_handle.resolve_track(seq, query, config) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::TrackResolved {
                            seq,
                            result: Err(error.to_string()),
                        },
                    );
                }
            }
            Cmd::AiRerank {
                request_id,
                seed_video_id,
                prompt,
            } => {
                let recovery_seed = seed_video_id.clone();
                let result = self.ai_handle.as_ref().map_or_else(
                    || Err(crate::util::delivery::DeliveryError::Closed),
                    |handle| handle.rerank(request_id, seed_video_id, prompt),
                );
                if !report_actor_delivery(app, "ai.rerank", result)
                    && let Some(msg) = recover_actor_rejection(
                        app,
                        ActorRejectionRecovery::AiRerank {
                            request_id,
                            seed_video_id: recovery_seed,
                        },
                    )
                {
                    self.reduce_owner_msg(app, msg);
                }
            }
            Cmd::SummarizeFeedback { digest } => {
                let result = self.ai_handle.as_ref().map_or_else(
                    || Err(crate::util::delivery::DeliveryError::Closed),
                    |handle| handle.summarize_feedback(digest),
                );
                if !report_actor_delivery(app, "ai.feedback", result) {
                    recover_actor_rejection(app, ActorRejectionRecovery::AiFeedback);
                }
            }
            Cmd::RomanizeTitles { request_id, items } => {
                let keys: Vec<String> = items.iter().map(|item| item.key.clone()).collect();
                if let Some(h) = &self.ai_handle {
                    if !report_actor_delivery(app, "ai.romanize", h.romanize(request_id, items)) {
                        self.reduce_owner_msg(
                            app,
                            Msg::Ai(AiMsg::RomanizedTitles {
                                request_id,
                                keys,
                                entries: Vec::new(),
                            }),
                        );
                    }
                } else {
                    self.reduce_owner_msg(
                        app,
                        Msg::Ai(AiMsg::RomanizedTitles {
                            request_id,
                            keys,
                            entries: Vec::new(),
                        }),
                    );
                }
            }
            Cmd::StreamingFallback {
                request_id,
                seed,
                seed_video_id,
                exclude_ids,
                mode,
                config,
            } => {
                if let Err(error) = self.api_handle.streaming(
                    request_id,
                    seed,
                    seed_video_id.clone(),
                    exclude_ids,
                    crate::playback_policy::STREAMING_POOL_COUNT,
                    mode,
                    config,
                ) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::Streaming(StreamingMsg::Error {
                            request_id,
                            seed_video_id,
                            error: error.to_string(),
                        }),
                    );
                }
            }
            Cmd::StreamingPreflight {
                request_id,
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => {
                if let Err(error) = self.api_handle.streaming_preflight(
                    request_id,
                    seed_video_id.clone(),
                    picks,
                    fallback,
                    mode,
                    config,
                ) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::Streaming(StreamingMsg::PreflightError {
                            request_id,
                            seed_video_id,
                            error: error.to_string(),
                        }),
                    );
                }
            }
            Cmd::SetAiModel(model) => {
                if let Some(h) = &self.ai_handle {
                    report_actor_delivery(app, "ai.model", h.set_model(model));
                }
            }
            Cmd::ReloadAi {
                key,
                model,
                assistant_enabled,
            } => {
                self.ai_handle = key.and_then(|k| {
                    crate::ai::spawn(&k, model, sink(self.worker_tx.clone(), RuntimeEvent::Ai))
                });
                app.ai.available = assistant_enabled && self.ai_handle.is_some();
            }
            Cmd::Scrobble(scrobble) => match scrobble {
                ScrobbleCmd::AuthStart => {
                    report_actor_delivery(app, "scrobble.auth", self.scrobble_handle.auth_start());
                }
                ScrobbleCmd::Reconfigure(settings) => {
                    report_actor_delivery(
                        app,
                        "scrobble.reconfigure",
                        self.scrobble_handle.reconfigure(*settings),
                    );
                }
            },
            Cmd::Transfer(cmd) => {
                let recovery = match &cmd {
                    crate::transfer::actor::TransferCmd::StartJob(_)
                    | crate::transfer::actor::TransferCmd::WriteReviewedLocal { .. } => {
                        Some(ActorRejectionRecovery::TransferStart)
                    }
                    crate::transfer::actor::TransferCmd::CancelJob => {
                        Some(ActorRejectionRecovery::TransferCancel)
                    }
                    crate::transfer::actor::TransferCmd::AuthStart { .. }
                    | crate::transfer::actor::TransferCmd::Disconnect
                    | crate::transfer::actor::TransferCmd::ListSpotifyPlaylists => None,
                };
                let transfer_tx = self.worker_tx.clone();
                let handle = self.transfer_handle.get_or_insert_with(|| {
                    crate::transfer::actor::spawn(move |event| {
                        emit(&transfer_tx, RuntimeEvent::Transfer(event))
                    })
                });
                if !report_actor_delivery(app, "transfer", handle.send(cmd))
                    && let Some(recovery) = recovery
                {
                    recover_actor_rejection(app, recovery);
                }
            }
            // Handled in the main loop (the OSC path writes to the terminal this scope doesn't
            // own); never reaches here. Listed for exhaustiveness.
            Cmd::DesktopNotify { .. } => {}
        }
    }
}
