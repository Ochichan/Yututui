//! Runtime dispatch for music-server library commands.

use super::super::*;
use super::{linked_playlists, server_library_failure};

impl RuntimeHandles {
    pub(super) fn dispatch_server_library(&mut self, command: crate::app::ServerLibraryCommand) {
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
                                crate::open_subsonic::ServerLibraryDetailRequest::Playlist(id)
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
                                linked_playlists::create_linked_playlist(generation, snapshot)
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
}
