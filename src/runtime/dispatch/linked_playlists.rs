//! Runtime dispatch for explicit linked-server-playlist workflows.

use crate::app::{
    Msg, ServerEvent, ServerLibraryEvent, ServerLibraryFailure, ServerPlaylistPreviewKind,
    ServerPlaylistRecoveryAction,
};
use crate::open_subsonic::{PlaylistPreviewTarget, ServerPlaylistId};
use crate::personal_state::PersonalPlaylistSnapshot;

use super::super::RuntimeEvent;

pub(super) async fn prepare_playlist(
    generation: u64,
    server_playlist_id: ServerPlaylistId,
    kind: ServerPlaylistPreviewKind,
) -> RuntimeEvent {
    let target = match kind {
        ServerPlaylistPreviewKind::ImportCopy => PlaylistPreviewTarget::ImportCopy,
        ServerPlaylistPreviewKind::LinkAndSync => PlaylistPreviewTarget::LinkNew,
    };
    let result = match crate::open_subsonic::current_handle() {
        Some(handle) => handle
            .prepare_playlist(server_playlist_id, target)
            .await
            .map_err(super::server_library_failure),
        None => Err(ServerLibraryFailure::Unavailable),
    };
    server_library_event(ServerLibraryEvent::PlaylistPrepared { generation, result })
}

pub(super) async fn apply_playlist_preview(
    generation: u64,
    preview_id: String,
    server_playlist_id: ServerPlaylistId,
) -> RuntimeEvent {
    let result = match crate::open_subsonic::current_handle() {
        Some(handle) => handle
            .apply_playlist_preview(preview_id, server_playlist_id, None)
            .await
            .map_err(super::server_library_failure),
        None => Err(ServerLibraryFailure::Unavailable),
    };
    server_library_event(ServerLibraryEvent::PlaylistApplied { generation, result })
}

pub(super) async fn create_linked_playlist(
    generation: u64,
    snapshot: PersonalPlaylistSnapshot,
) -> RuntimeEvent {
    let local_playlist_id = snapshot.playlist_id.clone();
    let result = match crate::open_subsonic::current_handle() {
        Some(handle) => handle
            .create_linked_playlist(snapshot)
            .await
            .map_err(super::server_library_failure),
        None => Err(ServerLibraryFailure::Unavailable),
    };
    server_library_event(ServerLibraryEvent::PlaylistCreated {
        generation,
        local_playlist_id,
        result,
    })
}

pub(super) async fn recover_playlist(
    generation: u64,
    action: ServerPlaylistRecoveryAction,
    server_playlist_id: ServerPlaylistId,
    snapshot: Option<PersonalPlaylistSnapshot>,
) -> RuntimeEvent {
    let result = match crate::open_subsonic::current_handle() {
        Some(handle) => {
            let result = match action {
                ServerPlaylistRecoveryAction::Restore => match snapshot {
                    Some(snapshot) => handle
                        .restore_linked_playlist(server_playlist_id, snapshot)
                        .await
                        .map(drop),
                    None => Err(crate::open_subsonic::ServerError::InvalidResponse),
                },
                ServerPlaylistRecoveryAction::UnlinkKeepServer
                | ServerPlaylistRecoveryAction::UnlinkKeepLocal => {
                    handle.unlink_playlist(server_playlist_id).await
                }
                ServerPlaylistRecoveryAction::DeleteBoth => {
                    handle.delete_linked_playlist(server_playlist_id).await
                }
                ServerPlaylistRecoveryAction::DeleteLocal => {
                    handle
                        .delete_missing_local_playlist(server_playlist_id)
                        .await
                }
            };
            result.map_err(super::server_library_failure)
        }
        None => Err(ServerLibraryFailure::Unavailable),
    };
    server_library_event(ServerLibraryEvent::PlaylistRecovered {
        generation,
        action,
        result,
    })
}

fn server_library_event(event: ServerLibraryEvent) -> RuntimeEvent {
    RuntimeEvent::App(Msg::Server(ServerEvent::Library(event)))
}
