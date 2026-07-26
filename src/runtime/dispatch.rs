//! Dispatch reducer commands to runtime-owned actors and background jobs.

use super::*;

mod linked_playlists;

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
            Cmd::MusicServer(command) => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                match command {
                    crate::app::MusicServerCommand::Refresh { generation } => {
                        self.begin_open_subsonic_reload(generation);
                        let read_only = self.persistence_read_only.is_some();
                        let bridge_sink =
                            (!read_only).then(|| super::open_subsonic_bridge_sink(emitter.clone()));
                        self.background_tasks.spawn_cancellable(
                            "music_server_connection_test",
                            async move {
                                let (runtime_result, result) =
                                    refresh_music_server_runtime(read_only, bridge_sink).await;
                                emitter
                                    .emit_terminal(RuntimeEvent::OpenSubsonicReloaded {
                                        generation,
                                        result: runtime_result,
                                    })
                                    .await;
                                emitter
                                    .emit_terminal(RuntimeEvent::App(Msg::Server(
                                        crate::app::ServerEvent::Settings(
                                            crate::app::MusicServerEvent::Refreshed {
                                                generation,
                                                result,
                                            },
                                        ),
                                    )))
                                    .await;
                            },
                        );
                    }
                    crate::app::MusicServerCommand::TestAndPrepare { generation, input } => {
                        self.background_tasks
                            .spawn_cancellable("music_server_test", async move {
                                let result = prepare_music_server_setup(input)
                                    .await
                                    .map(Box::new)
                                    .map_err(music_server_failure);
                                emitter
                                    .emit_terminal(RuntimeEvent::App(Msg::Server(
                                        crate::app::ServerEvent::Settings(
                                            crate::app::MusicServerEvent::Prepared {
                                                generation,
                                                result,
                                            },
                                        ),
                                    )))
                                    .await;
                            });
                    }
                    crate::app::MusicServerCommand::Commit {
                        generation,
                        prepared,
                    } => {
                        self.begin_open_subsonic_reload(generation);
                        let bridge_sink = super::open_subsonic_bridge_sink(emitter.clone());
                        self.background_tasks.spawn_cancellable(
                            "music_server_commit",
                            async move {
                                let committed = tokio::task::spawn_blocking(move || {
                                    crate::open_subsonic::OpenSubsonicPaths::current()
                                        .map_err(crate::open_subsonic::ServiceError::from)
                                        .and_then(|paths| {
                                            crate::open_subsonic::commit_setup(&paths, *prepared)
                                        })
                                })
                                .await
                                .map_err(|_| crate::open_subsonic::ServiceError::ActorUnavailable)
                                .and_then(std::convert::identity);
                                // commit_setup revokes the previous credential owner before its
                                // transaction begins. Reload on both success and failure so an
                                // ambiguous post-marker error cannot leave a stale local route.
                                let runtime_result =
                                    reload_music_server_runtime(Some(bridge_sink)).await;
                                let mut result = committed
                                    .map(music_server_summary)
                                    .map_err(music_server_failure);
                                if runtime_result.is_err()
                                    && let Ok(summary) = result.as_mut()
                                {
                                    // The durable commit already succeeded. Keep the configured
                                    // identity visible and offer retry.
                                    summary.health = crate::app::MusicServerHealth::NeedsAttention;
                                }
                                emitter
                                    .emit_terminal(RuntimeEvent::OpenSubsonicReloaded {
                                        generation,
                                        result: runtime_result,
                                    })
                                    .await;
                                emitter
                                    .emit_terminal(RuntimeEvent::App(Msg::Server(
                                        crate::app::ServerEvent::Settings(
                                            crate::app::MusicServerEvent::Committed {
                                                generation,
                                                result,
                                            },
                                        ),
                                    )))
                                    .await;
                            },
                        );
                    }
                    crate::app::MusicServerCommand::DisableHistory { generation } => {
                        self.begin_open_subsonic_reload(generation);
                        // The old actor may retain a dedicated native-history password. Drop it
                        // before the owner-only store removes that secret, then reload only from
                        // the committed snapshot.
                        self.retire_open_subsonic_runtime();
                        let bridge_sink = super::open_subsonic_bridge_sink(emitter.clone());
                        self.background_tasks.spawn_cancellable(
                            "music_server_history_disable",
                            async move {
                                let disabled = tokio::task::spawn_blocking(move || {
                                    crate::open_subsonic::OpenSubsonicPaths::current()
                                        .map_err(crate::open_subsonic::ServiceError::from)
                                        .and_then(|paths| {
                                            crate::open_subsonic::disable_native_history(&paths)
                                        })
                                })
                                .await
                                .map_err(|_| crate::open_subsonic::ServiceError::ActorUnavailable)
                                .and_then(std::convert::identity);
                                let runtime_result =
                                    reload_music_server_runtime(Some(bridge_sink)).await;
                                let mut result = disabled
                                    .map(music_server_summary)
                                    .map_err(music_server_failure);
                                if runtime_result.is_err()
                                    && let Ok(summary) = result.as_mut()
                                {
                                    summary.health = crate::app::MusicServerHealth::NeedsAttention;
                                }
                                emitter
                                    .emit_terminal(RuntimeEvent::OpenSubsonicReloaded {
                                        generation,
                                        result: runtime_result,
                                    })
                                    .await;
                                emitter
                                    .emit_terminal(RuntimeEvent::App(Msg::Server(
                                        crate::app::ServerEvent::Settings(
                                            crate::app::MusicServerEvent::HistoryDisabled {
                                                generation,
                                                result,
                                            },
                                        ),
                                    )))
                                    .await;
                            },
                        );
                    }
                    crate::app::MusicServerCommand::Remove { generation } => {
                        self.begin_open_subsonic_reload(generation);
                        // Removal is explicit and destructive. Stop credentialed routes before
                        // deleting their owner-only store; refresh/setup probes retain the active
                        // runtime until a replacement is fully ready.
                        self.retire_open_subsonic_runtime();
                        let bridge_sink = super::open_subsonic_bridge_sink(emitter.clone());
                        self.background_tasks.spawn_cancellable(
                            "music_server_remove",
                            async move {
                                let removed = tokio::task::spawn_blocking(move || {
                                    crate::open_subsonic::OpenSubsonicPaths::current()
                                        .map_err(crate::open_subsonic::ServiceError::from)
                                        .and_then(|paths| {
                                            crate::open_subsonic::remove_profile(&paths)
                                        })
                                })
                                .await
                                .map_err(|_| crate::open_subsonic::ServiceError::ActorUnavailable)
                                .and_then(std::convert::identity);
                                // A failed removal must not strand the process without the actor
                                // it retired above. Reloading the coherent current snapshot also
                                // resolves an ambiguous post-commit storage error safely.
                                let runtime_result =
                                    reload_music_server_runtime(Some(bridge_sink)).await;
                                let reload_state = match &runtime_result {
                                    Ok(runtime) => Ok(runtime.is_some()),
                                    Err(error) => Err(*error),
                                };
                                let result =
                                    resolve_music_server_remove(removed.err(), reload_state);
                                emitter
                                    .emit_terminal(RuntimeEvent::OpenSubsonicReloaded {
                                        generation,
                                        result: runtime_result,
                                    })
                                    .await;
                                emitter
                                    .emit_terminal(RuntimeEvent::App(Msg::Server(
                                        crate::app::ServerEvent::Settings(
                                            crate::app::MusicServerEvent::Removed {
                                                generation,
                                                result,
                                            },
                                        ),
                                    )))
                                    .await;
                            },
                        );
                    }
                    crate::app::MusicServerCommand::AbandonPlaylistCreate {
                        generation,
                        local_playlist_id,
                    } => {
                        self.background_tasks.spawn_cancellable(
                            "music_server_playlist_create_abandon",
                            async move {
                                let abandoned = if let Some(handle) =
                                    crate::open_subsonic::current_handle()
                                {
                                    handle
                                        .abandon_playlist_create(local_playlist_id)
                                        .await
                                        .map_err(|error| {
                                            music_server_failure(
                                                crate::open_subsonic::ServiceError::Server(error),
                                            )
                                        })
                                } else {
                                    tokio::task::spawn_blocking(move || {
                                        let paths =
                                            crate::open_subsonic::OpenSubsonicPaths::current()
                                                .map_err(
                                                    crate::open_subsonic::ServiceError::from,
                                                )?;
                                        crate::open_subsonic::abandon_playlist_create_attention(
                                            &paths,
                                            &local_playlist_id,
                                        )
                                    })
                                    .await
                                    .map_err(|_| crate::app::MusicServerFailure::Unavailable)
                                    .and_then(|result| result.map_err(music_server_failure))
                                };
                                let result = match abandoned {
                                    Ok(()) => tokio::task::spawn_blocking(|| {
                                        let paths =
                                            crate::open_subsonic::OpenSubsonicPaths::current()
                                                .map_err(
                                                    crate::open_subsonic::ServiceError::from,
                                                )?;
                                        crate::open_subsonic::read_status(&paths)
                                    })
                                    .await
                                    .map_err(|_| crate::app::MusicServerFailure::Unavailable)
                                    .and_then(|result| {
                                        result
                                            .map(music_server_summary)
                                            .map_err(music_server_failure)
                                    }),
                                    Err(error) => Err(error),
                                };
                                emitter
                                    .emit_terminal(RuntimeEvent::App(Msg::Server(
                                        crate::app::ServerEvent::Settings(
                                            crate::app::MusicServerEvent::PlaylistCreateAbandoned {
                                                generation,
                                                result,
                                            },
                                        ),
                                    )))
                                    .await;
                            },
                        );
                    }
                }
            }
            Cmd::ServerLibrary(command) => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_cancellable("music_server_library", async move {
                        match command {
                            crate::app::ServerLibraryCommand::LoadPage {
                                generation,
                                section,
                                offset,
                                limit,
                            } => {
                                let result = match crate::open_subsonic::current_handle() {
                                    Some(handle) => handle
                                        .library_page(crate::open_subsonic::ServerLibraryRequest {
                                            section,
                                            offset,
                                            limit: limit.min(crate::open_subsonic::MAX_PAGE_SIZE),
                                        })
                                        .await
                                        .map_err(server_library_failure),
                                    None => Err(crate::app::ServerLibraryFailure::Unavailable),
                                };
                                emitter
                                    .emit_terminal(RuntimeEvent::App(Msg::Server(
                                        crate::app::ServerEvent::Library(
                                            crate::app::ServerLibraryEvent::PageLoaded {
                                                generation,
                                                offset,
                                                result,
                                            },
                                        ),
                                    )))
                                    .await;
                            }
                            crate::app::ServerLibraryCommand::LoadDetail { generation, target } => {
                                let request = match target {
                                    crate::app::ServerLibraryDetailTarget::Album(id) => {
                                        crate::open_subsonic::ServerLibraryDetailRequest::Album(id)
                                    }
                                    crate::app::ServerLibraryDetailTarget::Artist(id) => {
                                        crate::open_subsonic::ServerLibraryDetailRequest::Artist(id)
                                    }
                                    crate::app::ServerLibraryDetailTarget::Playlist(id) => {
                                        crate::open_subsonic::ServerLibraryDetailRequest::Playlist(
                                            id,
                                        )
                                    }
                                };
                                let result = match crate::open_subsonic::current_handle() {
                                    Some(handle) => handle
                                        .library_detail(request)
                                        .await
                                        .map_err(server_library_failure),
                                    None => Err(crate::app::ServerLibraryFailure::Unavailable),
                                };
                                emitter
                                    .emit_terminal(RuntimeEvent::App(Msg::Server(
                                        crate::app::ServerEvent::Library(
                                            crate::app::ServerLibraryEvent::DetailLoaded {
                                                generation,
                                                result,
                                            },
                                        ),
                                    )))
                                    .await;
                            }
                            crate::app::ServerLibraryCommand::PreparePlaylist {
                                generation,
                                server_playlist_id,
                                kind,
                            } => {
                                emitter
                                    .emit_terminal(
                                        linked_playlists::prepare_playlist(
                                            generation,
                                            server_playlist_id,
                                            kind,
                                        )
                                        .await,
                                    )
                                    .await;
                            }
                            crate::app::ServerLibraryCommand::ApplyPlaylistPreview {
                                generation,
                                preview_id,
                                server_playlist_id,
                            } => {
                                emitter
                                    .emit_terminal(
                                        linked_playlists::apply_playlist_preview(
                                            generation,
                                            preview_id,
                                            server_playlist_id,
                                        )
                                        .await,
                                    )
                                    .await;
                            }
                            crate::app::ServerLibraryCommand::CreateLinkedPlaylist {
                                generation,
                                snapshot,
                            } => {
                                emitter
                                    .emit_terminal(
                                        linked_playlists::create_linked_playlist(
                                            generation, snapshot,
                                        )
                                        .await,
                                    )
                                    .await;
                            }
                            crate::app::ServerLibraryCommand::RecoverPlaylist {
                                generation,
                                action,
                                server_playlist_id,
                                snapshot,
                            } => {
                                emitter
                                    .emit_terminal(
                                        linked_playlists::recover_playlist(
                                            generation,
                                            action,
                                            server_playlist_id,
                                            snapshot,
                                        )
                                        .await,
                                    )
                                    .await;
                            }
                        }
                    });
            }
            // Persist: hand the persistence actor an owned snapshot (or clear one). Cloning a
            // store is a couple ms of memcpy at worst; the fsync it replaces on this task was
            // 5-50ms. The marker variants clone the live snapshot from `app` here; `Config`
            // carries its own owned snapshot.
            Cmd::Persist(PersistCmd::TransferPlaylistCommit(commit)) => {
                self.dispatch_transfer_playlist_commit(app, commit);
            }
            Cmd::Persist(PersistCmd::PersonalSyncCommit(commit)) => {
                self.dispatch_personal_sync_commit(app, commit);
            }
            Cmd::Persist(PersistCmd::SyncActivationCommit(commit)) => {
                self.dispatch_sync_activation_commit(app, commit);
            }
            Cmd::Persist(p) => {
                let result = persist_delivery::admit(&self.persist, app, p);
                report_actor_delivery(app, "persistence", result);
            }
            Cmd::Data(cmd) => match cmd {
                DataCmd::SyncUi(command) => {
                    let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                    self.background_tasks
                        .spawn_blocking("sync_settings", move || {
                            let event = super::sync_ui_worker::run(command);
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
                    let result = self.scrobble_handle.as_ref().map_or(
                        Err(crate::util::delivery::DeliveryError::Closed),
                        |handle| handle.auth_start(),
                    );
                    report_actor_delivery(app, "scrobble.auth", result);
                }
                ScrobbleCmd::Reconfigure(settings) => {
                    let result = self.scrobble_handle.as_ref().map_or(
                        Err(crate::util::delivery::DeliveryError::Closed),
                        |handle| handle.reconfigure(*settings),
                    );
                    report_actor_delivery(app, "scrobble.reconfigure", result);
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

async fn refresh_music_server_runtime(
    read_only: bool,
    bridge_sink: Option<crate::open_subsonic::OpenSubsonicBridgeSink>,
) -> (
    Result<Option<crate::open_subsonic::OpenSubsonicRuntime>, crate::open_subsonic::ServiceError>,
    Result<crate::app::MusicServerRefreshOutcome, crate::app::MusicServerFailure>,
) {
    let paths = match crate::open_subsonic::OpenSubsonicPaths::current() {
        Ok(paths) => paths,
        Err(error) => {
            let error = crate::open_subsonic::ServiceError::from(error);
            return (Err(error), Err(music_server_failure(error)));
        }
    };
    let local_status = match crate::open_subsonic::read_status(&paths) {
        Ok(status) => status,
        Err(error) => return (Err(error), Err(music_server_failure(error))),
    };
    let mut local_summary = music_server_summary(local_status);
    let runtime_result = if read_only {
        crate::open_subsonic::load_actor_read_only(&paths).await
    } else {
        crate::open_subsonic::load_actor_with_bridge_sink(&paths, bridge_sink).await
    };
    let outcome = match &runtime_result {
        Ok(Some(_)) => {
            let mut summary = crate::open_subsonic::read_status(&paths)
                .map(music_server_summary)
                .unwrap_or(local_summary);
            summary.health = live_music_server_health(
                summary.playback_reports_needing_decision,
                summary.playlist_creates_needing_decision,
                summary.playlist_links_needing_decision,
                summary.playlist_projections_needing_decision,
                summary.playlist_contents_needing_decision,
            );
            summary.configured = true;
            crate::app::MusicServerRefreshOutcome {
                summary,
                failure: None,
            }
        }
        Ok(None) => crate::app::MusicServerRefreshOutcome {
            summary: crate::app::MusicServerSummary::default(),
            failure: None,
        },
        Err(error) => {
            local_summary.health = crate::app::MusicServerHealth::NeedsAttention;
            crate::app::MusicServerRefreshOutcome {
                summary: local_summary,
                failure: Some(music_server_failure(*error)),
            }
        }
    };
    (runtime_result, Ok(outcome))
}

async fn reload_music_server_runtime(
    bridge_sink: Option<crate::open_subsonic::OpenSubsonicBridgeSink>,
) -> Result<Option<crate::open_subsonic::OpenSubsonicRuntime>, crate::open_subsonic::ServiceError> {
    let paths = crate::open_subsonic::OpenSubsonicPaths::current()?;
    crate::open_subsonic::load_actor_with_bridge_sink(&paths, bridge_sink).await
}

pub(super) fn resolve_music_server_remove(
    removal_error: Option<crate::open_subsonic::ServiceError>,
    reload: Result<bool, crate::open_subsonic::ServiceError>,
) -> Result<(), crate::app::MusicServerFailure> {
    match reload {
        // The coherent store is absent. This is also the proof that an error returned after the
        // removal commit marker was an ambiguous success.
        Ok(false) => Ok(()),
        Ok(true) => Err(removal_error
            .map(music_server_failure)
            .unwrap_or(crate::app::MusicServerFailure::Unavailable)),
        Err(error) => Err(music_server_failure(error)),
    }
}

async fn prepare_music_server_setup(
    input: crate::app::MusicServerSetupInput,
) -> Result<crate::open_subsonic::PreparedSetup, crate::open_subsonic::ServiceError> {
    const MAX_CUSTOM_CA_BYTES: u64 = 192 * 1024;

    let crate::app::MusicServerSetupInput {
        mut display_name,
        mut origin,
        mut username,
        mut secret,
        credential_mode,
        mut custom_ca_path,
        allow_lan_http,
        identity_intent,
    } = input;
    let custom_ca_pem = if custom_ca_path.trim().is_empty() {
        None
    } else {
        let path = std::path::PathBuf::from(custom_ca_path.as_str());
        custom_ca_path.clear();
        Some(
            tokio::task::spawn_blocking(move || {
                let bytes =
                    crate::util::safe_fs::read_no_symlink_limited(&path, MAX_CUSTOM_CA_BYTES)
                        .map_err(|_| crate::open_subsonic::ServiceError::InvalidSetup)?;
                if bytes.is_empty() {
                    return Err(crate::open_subsonic::ServiceError::InvalidSetup);
                }
                Ok(bytes)
            })
            .await
            .map_err(|_| crate::open_subsonic::ServiceError::ActorUnavailable)??,
        )
    };
    let secret_value = std::mem::take(&mut *secret);
    let credential = match credential_mode {
        crate::app::MusicServerCredentialMode::Password => {
            let username_value = std::mem::take(&mut *username);
            crate::open_subsonic::ServerCredential::password(
                username_value,
                age::secrecy::SecretString::from(secret_value),
            )
            .map_err(|_| crate::open_subsonic::ServiceError::InvalidSetup)?
        }
        crate::app::MusicServerCredentialMode::ApiKey => {
            crate::open_subsonic::ServerCredential::api_key(age::secrecy::SecretString::from(
                secret_value,
            ))
            .map_err(|_| crate::open_subsonic::ServiceError::InvalidSetup)?
        }
    };
    let paths = crate::open_subsonic::OpenSubsonicPaths::current()?;
    crate::open_subsonic::test_and_prepare_setup(
        &paths,
        crate::open_subsonic::SetupInput::new(
            std::mem::take(&mut *display_name),
            std::mem::take(&mut *origin),
            allow_lan_http,
            custom_ca_pem,
            credential,
            match identity_intent {
                crate::app::MusicServerIdentityIntent::Create => {
                    crate::open_subsonic::SetupIdentityIntent::Create
                }
                crate::app::MusicServerIdentityIntent::UpdateSameServerAndAccount => {
                    crate::open_subsonic::SetupIdentityIntent::UpdateSameServerAndAccount
                }
                crate::app::MusicServerIdentityIntent::ReplaceServerOrAccount => {
                    crate::open_subsonic::SetupIdentityIntent::ReplaceServerOrAccount
                }
            },
        ),
    )
    .await
}

fn music_server_summary(
    status: crate::open_subsonic::OpenSubsonicStatus,
) -> crate::app::MusicServerSummary {
    let configured = status.kind != crate::open_subsonic::OpenSubsonicStatusKind::Off;
    let credential_kind = status.credential_kind.map(|kind| match kind {
        crate::open_subsonic::CredentialKind::Password => {
            crate::app::MusicServerCredentialMode::Password
        }
        crate::open_subsonic::CredentialKind::ApiKey => {
            crate::app::MusicServerCredentialMode::ApiKey
        }
    });
    crate::app::MusicServerSummary {
        health: match status.kind {
            crate::open_subsonic::OpenSubsonicStatusKind::Off => crate::app::MusicServerHealth::Off,
            crate::open_subsonic::OpenSubsonicStatusKind::UpToDate => {
                crate::app::MusicServerHealth::UpToDate
            }
            crate::open_subsonic::OpenSubsonicStatusKind::NeedsAttention => {
                crate::app::MusicServerHealth::NeedsAttention
            }
        },
        configured,
        display_name: status.display_name,
        credential_kind,
        lan_http: status.uses_lan_http,
        custom_ca: status.uses_custom_ca,
        playback_reports_needing_decision: status.outbound_scrobbles_needing_attention,
        playlist_creates_needing_decision: status.playlist_creates_needing_attention,
        playlist_create_attention: status.playlist_create_attention,
        playlist_links_needing_decision: status.playlist_links_needing_decision,
        playlist_projections_needing_decision: status.playlist_projections_needing_attention,
        playlist_contents_needing_decision: status.playlist_contents_needing_attention,
        history: match status.native_history_health {
            crate::open_subsonic::NativeHistoryHealth::Off => {
                crate::app::MusicServerHistoryHealth::Off
            }
            crate::open_subsonic::NativeHistoryHealth::Probing => {
                crate::app::MusicServerHistoryHealth::Probing
            }
            crate::open_subsonic::NativeHistoryHealth::Detailed => {
                crate::app::MusicServerHistoryHealth::Detailed
            }
            crate::open_subsonic::NativeHistoryHealth::PlayCountsOnly => {
                crate::app::MusicServerHistoryHealth::PlayCountsOnly
            }
            crate::open_subsonic::NativeHistoryHealth::UpdatePassword => {
                crate::app::MusicServerHistoryHealth::UpdatePassword
            }
        },
    }
}

pub(super) const fn live_music_server_health(
    playback_reports_needing_decision: usize,
    playlist_creates_needing_decision: usize,
    playlist_links_needing_decision: usize,
    playlist_projections_needing_decision: usize,
    playlist_contents_needing_decision: usize,
) -> crate::app::MusicServerHealth {
    if playback_reports_needing_decision == 0
        && playlist_creates_needing_decision == 0
        && playlist_links_needing_decision == 0
        && playlist_projections_needing_decision == 0
        && playlist_contents_needing_decision == 0
    {
        crate::app::MusicServerHealth::UpToDate
    } else {
        crate::app::MusicServerHealth::NeedsAttention
    }
}

fn music_server_failure(
    error: crate::open_subsonic::ServiceError,
) -> crate::app::MusicServerFailure {
    match error {
        crate::open_subsonic::ServiceError::Store(_) => crate::app::MusicServerFailure::Storage,
        crate::open_subsonic::ServiceError::Server(error) => match error {
            crate::open_subsonic::ServerError::AuthenticationRequired
            | crate::open_subsonic::ServerError::PermissionDenied => {
                crate::app::MusicServerFailure::Authentication
            }
            crate::open_subsonic::ServerError::CertificateFailed => {
                crate::app::MusicServerFailure::Certificate
            }
            crate::open_subsonic::ServerError::OriginRejected
            | crate::open_subsonic::ServerError::WrongAccountScope => {
                crate::app::MusicServerFailure::InvalidInput
            }
            crate::open_subsonic::ServerError::Offline
            | crate::open_subsonic::ServerError::RateLimited(_)
            | crate::open_subsonic::ServerError::TemporarilyUnavailable => {
                crate::app::MusicServerFailure::Connection
            }
            crate::open_subsonic::ServerError::UnsupportedFeature
            | crate::open_subsonic::ServerError::NotFound
            | crate::open_subsonic::ServerError::InvalidResponse
            | crate::open_subsonic::ServerError::ResponseTooLarge => {
                crate::app::MusicServerFailure::InvalidInput
            }
        },
        crate::open_subsonic::ServiceError::InvalidSetup => {
            crate::app::MusicServerFailure::InvalidInput
        }
        crate::open_subsonic::ServiceError::ActorUnavailable
        | crate::open_subsonic::ServiceError::ProxyUnavailable => {
            crate::app::MusicServerFailure::Unavailable
        }
    }
}

fn server_library_failure(
    error: crate::open_subsonic::ServerError,
) -> crate::app::ServerLibraryFailure {
    match error {
        crate::open_subsonic::ServerError::AuthenticationRequired
        | crate::open_subsonic::ServerError::PermissionDenied => {
            crate::app::ServerLibraryFailure::Authentication
        }
        crate::open_subsonic::ServerError::UnsupportedFeature
        | crate::open_subsonic::ServerError::NotFound => {
            crate::app::ServerLibraryFailure::Unsupported
        }
        crate::open_subsonic::ServerError::InvalidResponse
        | crate::open_subsonic::ServerError::ResponseTooLarge
        | crate::open_subsonic::ServerError::WrongAccountScope => {
            crate::app::ServerLibraryFailure::InvalidResponse
        }
        crate::open_subsonic::ServerError::Offline
        | crate::open_subsonic::ServerError::CertificateFailed
        | crate::open_subsonic::ServerError::OriginRejected
        | crate::open_subsonic::ServerError::RateLimited(_)
        | crate::open_subsonic::ServerError::TemporarilyUnavailable => {
            crate::app::ServerLibraryFailure::Offline
        }
    }
}
