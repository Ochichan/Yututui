//! Credential-owner boundary for server-playlist mutations.

use super::playlists::PlaylistActorCommand;
use crate::open_subsonic::ServerError;

/// Keep previews available to read-only consumers while failing every mutation before network or
/// durable bridge work can begin.
pub(super) fn authorize(
    command: PlaylistActorCommand,
    mutations_allowed: bool,
) -> Option<PlaylistActorCommand> {
    if mutations_allowed || matches!(&command, PlaylistActorCommand::Prepare { .. }) {
        return Some(command);
    }
    match command {
        PlaylistActorCommand::Prepare { .. } => unreachable!("preview commands are read-only"),
        PlaylistActorCommand::ApplyPreview { reply, .. } => {
            let _ = reply.send(Err(ServerError::PermissionDenied));
        }
        PlaylistActorCommand::Reconcile { reply, .. } => {
            let _ = reply.send(Err(ServerError::PermissionDenied.into()));
        }
        PlaylistActorCommand::CreateLinked { reply, .. } => {
            let _ = reply.send(Err(ServerError::PermissionDenied));
        }
        PlaylistActorCommand::Unlink { reply, .. }
        | PlaylistActorCommand::DeleteBoth { reply, .. }
        | PlaylistActorCommand::DeleteLocal { reply, .. }
        | PlaylistActorCommand::AbandonCreate { reply, .. } => {
            let _ = reply.send(Err(ServerError::PermissionDenied));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_subsonic::{PlaylistPreviewTarget, ServiceError};
    use crate::personal_state::{PersonalPlaylistSnapshot, PlaylistId};

    fn local_snapshot() -> PersonalPlaylistSnapshot {
        PersonalPlaylistSnapshot {
            playlist_id: PlaylistId::new("local").unwrap(),
            name: "Local".to_owned(),
            entries: Vec::new(),
        }
    }

    #[test]
    fn preview_is_allowed_but_every_mutation_is_denied_without_an_active_owner() {
        let server_id = crate::open_subsonic::ServerPlaylistId::new("server").unwrap();
        let (reply, _response) = tokio::sync::oneshot::channel();
        assert!(
            authorize(
                PlaylistActorCommand::Prepare {
                    server_playlist_id: server_id.clone(),
                    target: PlaylistPreviewTarget::ImportCopy,
                    reply,
                },
                false,
            )
            .is_some()
        );

        let (reply, response) = tokio::sync::oneshot::channel();
        assert!(
            authorize(
                PlaylistActorCommand::ApplyPreview {
                    preview_id: "preview".to_owned(),
                    server_playlist_id: server_id.clone(),
                    current_local: None,
                    reply,
                },
                false,
            )
            .is_none()
        );
        assert_eq!(
            response.blocking_recv().unwrap(),
            Err(ServerError::PermissionDenied)
        );

        let (reply, response) = tokio::sync::oneshot::channel();
        assert!(
            authorize(
                PlaylistActorCommand::Reconcile {
                    snapshots: vec![local_snapshot()],
                    reply,
                },
                false,
            )
            .is_none()
        );
        assert_eq!(
            response.blocking_recv().unwrap(),
            Err(ServiceError::Server(ServerError::PermissionDenied))
        );

        let (reply, response) = tokio::sync::oneshot::channel();
        assert!(
            authorize(
                PlaylistActorCommand::CreateLinked {
                    snapshot: local_snapshot(),
                    replace_missing: false,
                    expected_missing_server_id: None,
                    reply,
                },
                false,
            )
            .is_none()
        );
        assert_eq!(
            response.blocking_recv().unwrap(),
            Err(ServerError::PermissionDenied)
        );

        for (command, response) in [
            {
                let (reply, response) = tokio::sync::oneshot::channel();
                (
                    PlaylistActorCommand::Unlink {
                        server_playlist_id: server_id.clone(),
                        reply,
                    },
                    response,
                )
            },
            {
                let (reply, response) = tokio::sync::oneshot::channel();
                (
                    PlaylistActorCommand::DeleteBoth {
                        server_playlist_id: server_id.clone(),
                        reply,
                    },
                    response,
                )
            },
            {
                let (reply, response) = tokio::sync::oneshot::channel();
                (
                    PlaylistActorCommand::DeleteLocal {
                        server_playlist_id: server_id,
                        reply,
                    },
                    response,
                )
            },
            {
                let (reply, response) = tokio::sync::oneshot::channel();
                (
                    PlaylistActorCommand::AbandonCreate {
                        local_playlist_id: PlaylistId::new("local").unwrap(),
                        reply,
                    },
                    response,
                )
            },
        ] {
            assert!(authorize(command, false).is_none());
            assert_eq!(
                response.blocking_recv().unwrap(),
                Err(ServerError::PermissionDenied)
            );
        }
    }
}
