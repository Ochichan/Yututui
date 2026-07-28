//! Runtime dispatch for music-server setup and lifecycle commands.

use super::super::*;
use super::{
    music_server_failure, music_server_summary, prepare_music_server_setup,
    refresh_music_server_runtime, reload_music_server_runtime, resolve_music_server_remove,
};

impl RuntimeHandles {
    pub(super) fn dispatch_music_server(&mut self, command: crate::app::MusicServerCommand) {
        let emitter = self.background_tasks.emitter(self.worker_tx.clone());
        match command {
            crate::app::MusicServerCommand::Refresh { generation } => {
                self.begin_open_subsonic_reload(generation);
                let read_only = self.persistence_read_only.is_some();
                let bridge_sink = (!read_only).then(|| open_subsonic_bridge_sink(emitter.clone()));
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
                                    crate::app::MusicServerEvent::Refreshed { generation, result },
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
                                    crate::app::MusicServerEvent::Prepared { generation, result },
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
                let bridge_sink = open_subsonic_bridge_sink(emitter.clone());
                self.background_tasks
                    .spawn_cancellable("music_server_commit", async move {
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
                        let runtime_result = reload_music_server_runtime(Some(bridge_sink)).await;
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
                                    crate::app::MusicServerEvent::Committed { generation, result },
                                ),
                            )))
                            .await;
                    });
            }
            crate::app::MusicServerCommand::DisableHistory { generation } => {
                self.begin_open_subsonic_reload(generation);
                // The old actor may retain a dedicated native-history password. Drop it
                // before the owner-only store removes that secret, then reload only from
                // the committed snapshot.
                self.retire_open_subsonic_runtime();
                let bridge_sink = open_subsonic_bridge_sink(emitter.clone());
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
                        let runtime_result = reload_music_server_runtime(Some(bridge_sink)).await;
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
                let bridge_sink = open_subsonic_bridge_sink(emitter.clone());
                self.background_tasks
                    .spawn_cancellable("music_server_remove", async move {
                        let removed = tokio::task::spawn_blocking(move || {
                            crate::open_subsonic::OpenSubsonicPaths::current()
                                .map_err(crate::open_subsonic::ServiceError::from)
                                .and_then(|paths| crate::open_subsonic::remove_profile(&paths))
                        })
                        .await
                        .map_err(|_| crate::open_subsonic::ServiceError::ActorUnavailable)
                        .and_then(std::convert::identity);
                        // A failed removal must not strand the process without the actor
                        // it retired above. Reloading the coherent current snapshot also
                        // resolves an ambiguous post-commit storage error safely.
                        let runtime_result = reload_music_server_runtime(Some(bridge_sink)).await;
                        let reload_state = match &runtime_result {
                            Ok(runtime) => Ok(runtime.is_some()),
                            Err(error) => Err(*error),
                        };
                        let result = resolve_music_server_remove(removed.err(), reload_state);
                        emitter
                            .emit_terminal(RuntimeEvent::OpenSubsonicReloaded {
                                generation,
                                result: runtime_result,
                            })
                            .await;
                        emitter
                            .emit_terminal(RuntimeEvent::App(Msg::Server(
                                crate::app::ServerEvent::Settings(
                                    crate::app::MusicServerEvent::Removed { generation, result },
                                ),
                            )))
                            .await;
                    });
            }
            crate::app::MusicServerCommand::PublishTrack {
                generation,
                video_id,
            } => {
                self.background_tasks.spawn_cancellable(
                    "music_server_publish_track",
                    publish_track_task::run(emitter.clone(), generation, video_id),
                );
            }
            crate::app::MusicServerCommand::AbandonPlaylistCreate {
                generation,
                local_playlist_id,
            } => {
                self.background_tasks.spawn_cancellable(
                    "music_server_playlist_create_abandon",
                    async move {
                        let abandoned = if let Some(handle) = crate::open_subsonic::current_handle()
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
                                let paths = crate::open_subsonic::OpenSubsonicPaths::current()
                                    .map_err(crate::open_subsonic::ServiceError::from)?;
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
                                let paths = crate::open_subsonic::OpenSubsonicPaths::current()
                                    .map_err(crate::open_subsonic::ServiceError::from)?;
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
}
