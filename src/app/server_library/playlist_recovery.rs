//! Explicit linked-playlist recovery and destructive confirmation state.

use crossterm::event::{KeyCode, KeyEvent};

use super::{
    App, Cmd, LibrarySource, ServerLibraryCommand, ServerLibraryEvent, ServerLibraryFailure,
    ServerPlaylistId, StatusKind,
};
use crate::personal_state::PlaylistId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerPlaylistRecoveryAction {
    Restore,
    UnlinkKeepServer,
    UnlinkKeepLocal,
    DeleteBoth,
    DeleteLocal,
}

impl ServerPlaylistRecoveryAction {
    pub const fn destructive(self) -> bool {
        matches!(self, Self::DeleteBoth | Self::DeleteLocal)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerPlaylistRecoveryStage {
    Confirming,
    Applying,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerPlaylistRecoveryModal {
    pub generation: u64,
    pub action: ServerPlaylistRecoveryAction,
    pub server_playlist_id: ServerPlaylistId,
    pub local_playlist_id: PlaylistId,
    pub name: String,
    pub stage: ServerPlaylistRecoveryStage,
}

impl App {
    pub(in crate::app) fn start_server_playlist_recovery(
        &mut self,
        server_playlist_id: ServerPlaylistId,
        local_playlist_id: PlaylistId,
        name: String,
        action: ServerPlaylistRecoveryAction,
    ) -> Vec<Cmd> {
        if self.server.library.source != LibrarySource::OpenSubsonic
            || self.server.library.busy.is_some()
            || self.server.library.playlist_preview.is_some()
            || self.server.library.playlist_create.is_some()
            || self.server.library.playlist_recovery.is_some()
        {
            return Vec::new();
        }
        let generation = self.server.library.next_recovery_generation();
        let stage = if action.destructive() {
            ServerPlaylistRecoveryStage::Confirming
        } else {
            ServerPlaylistRecoveryStage::Applying
        };
        self.server.library.playlist_recovery = Some(ServerPlaylistRecoveryModal {
            generation,
            action,
            server_playlist_id,
            local_playlist_id,
            name,
            stage,
        });
        self.dirty = true;
        if action.destructive() {
            Vec::new()
        } else {
            self.server_playlist_recovery_command()
        }
    }

    pub(in crate::app) fn apply_server_playlist_recovery(&mut self) -> Vec<Cmd> {
        let Some(modal) = self.server.library.playlist_recovery.as_mut() else {
            return Vec::new();
        };
        if modal.stage != ServerPlaylistRecoveryStage::Confirming {
            return Vec::new();
        }
        modal.stage = ServerPlaylistRecoveryStage::Applying;
        self.dirty = true;
        self.server_playlist_recovery_command()
    }

    fn server_playlist_recovery_command(&mut self) -> Vec<Cmd> {
        let (generation, action, server_playlist_id, local_playlist_id) = {
            let Some(modal) = self.server.library.playlist_recovery.as_ref() else {
                return Vec::new();
            };
            (
                modal.generation,
                modal.action,
                modal.server_playlist_id.clone(),
                modal.local_playlist_id.clone(),
            )
        };
        let snapshot = if action == ServerPlaylistRecoveryAction::Restore {
            match crate::personal_state::personal_playlist_snapshot(
                &self.personal_state.ledger,
                &local_playlist_id,
            ) {
                Ok(Some(snapshot)) => Some(snapshot),
                Ok(None) | Err(_) => {
                    self.fail_server_playlist_recovery(ServerLibraryFailure::InvalidResponse);
                    return Vec::new();
                }
            }
        } else {
            None
        };
        vec![Cmd::ServerLibrary(ServerLibraryCommand::RecoverPlaylist {
            generation,
            action,
            server_playlist_id,
            snapshot,
        })]
    }

    pub(in crate::app) fn cancel_server_playlist_recovery(&mut self) -> Vec<Cmd> {
        if self
            .server
            .library
            .playlist_recovery
            .as_ref()
            .is_some_and(|modal| modal.stage == ServerPlaylistRecoveryStage::Applying)
        {
            return Vec::new();
        }
        if self.server.library.playlist_recovery.take().is_some() {
            self.server.library.next_recovery_generation();
            self.dirty = true;
        }
        Vec::new()
    }

    pub(in crate::app) fn on_key_server_playlist_recovery(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self
            .server
            .library
            .playlist_recovery
            .as_ref()
            .is_some_and(|modal| modal.stage == ServerPlaylistRecoveryStage::Applying)
        {
            return Vec::new();
        }
        if key.code == KeyCode::Enter {
            self.apply_server_playlist_recovery()
        } else {
            self.cancel_server_playlist_recovery()
        }
    }

    pub(in crate::app) fn finish_server_playlist_recovery_event(
        &mut self,
        event: ServerLibraryEvent,
    ) -> Vec<Cmd> {
        let ServerLibraryEvent::PlaylistRecovered {
            generation,
            action,
            result,
        } = event
        else {
            return Vec::new();
        };
        let Some(modal) = self.server.library.playlist_recovery.as_ref() else {
            return Vec::new();
        };
        if modal.generation != generation
            || modal.action != action
            || modal.stage != ServerPlaylistRecoveryStage::Applying
            || self.server.library.source != LibrarySource::OpenSubsonic
        {
            return Vec::new();
        }
        match result {
            Ok(()) => {
                let name = modal.name.clone();
                self.server.library.playlist_recovery = None;
                self.server.library.failure = None;
                self.status.kind = StatusKind::Info;
                self.status.text = recovery_success(action, &name);
                self.dirty = true;
                self.request_server_library_page(self.server.library.offset, false)
            }
            Err(failure) => {
                self.fail_server_playlist_recovery(failure);
                Vec::new()
            }
        }
    }

    fn fail_server_playlist_recovery(&mut self, failure: ServerLibraryFailure) {
        self.server.library.playlist_recovery = None;
        self.server.library.failure = Some(failure);
        self.status.kind = StatusKind::Error;
        self.status.text = failure.label().to_owned();
        self.dirty = true;
    }
}

fn recovery_success(action: ServerPlaylistRecoveryAction, name: &str) -> String {
    let action = match action {
        ServerPlaylistRecoveryAction::Restore => {
            crate::t!("Restored", "복구됨", "復元しました")
        }
        ServerPlaylistRecoveryAction::UnlinkKeepServer
        | ServerPlaylistRecoveryAction::UnlinkKeepLocal => {
            crate::t!("Unlinked", "연결 해제됨", "リンクを解除しました")
        }
        ServerPlaylistRecoveryAction::DeleteBoth | ServerPlaylistRecoveryAction::DeleteLocal => {
            crate::t!("Deleted", "삭제됨", "削除しました")
        }
    };
    format!("{action}: {name}")
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::personal_state::{
        ExternalOperationInput, Operation, OperationOrigin, PersonalPlaylistSnapshot,
        append_external_operations,
    };

    fn app_with_playlist() -> App {
        let mut app = App::new(50);
        app.server.library.source = LibrarySource::OpenSubsonic;
        let id = PlaylistId::new("local").unwrap();
        let (ledger, _) = append_external_operations(
            &app.personal_state.ledger,
            OperationOrigin::Imported,
            &[ExternalOperationInput {
                acknowledgement_id: "local-playlist".to_owned(),
                operation: Operation::UpsertPlaylist {
                    playlist_id: id,
                    name: "Road Trip".to_owned(),
                },
                recorded_at_unix: 1,
            }],
        )
        .unwrap();
        app.personal_state.replace_ledger(ledger);
        app
    }

    #[test]
    fn delete_requires_confirmation_and_dispatches_only_once() {
        for action in [
            ServerPlaylistRecoveryAction::DeleteBoth,
            ServerPlaylistRecoveryAction::DeleteLocal,
        ] {
            let mut app = app_with_playlist();
            assert!(
                app.start_server_playlist_recovery(
                    ServerPlaylistId::new("remote").unwrap(),
                    PlaylistId::new("local").unwrap(),
                    "Road Trip".to_owned(),
                    action,
                )
                .is_empty()
            );
            assert!(matches!(
                app.server.library.playlist_recovery.as_ref(),
                Some(ServerPlaylistRecoveryModal {
                    stage: ServerPlaylistRecoveryStage::Confirming,
                    ..
                })
            ));
            let commands = app
                .on_key_server_playlist_recovery(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(matches!(
                commands.as_slice(),
                [Cmd::ServerLibrary(ServerLibraryCommand::RecoverPlaylist {
                    action: command_action,
                    ..
                })] if *command_action == action
            ));
            assert!(
                app.on_key_server_playlist_recovery(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::NONE,
                ))
                .is_empty()
            );
            assert!(
                app.on_key_server_playlist_recovery(KeyEvent::new(
                    KeyCode::Esc,
                    KeyModifiers::NONE,
                ))
                .is_empty()
            );
            assert!(app.cancel_server_playlist_recovery().is_empty());
            assert!(
                app.on_mouse_target(crate::app::MouseTarget::CancelServerPlaylistRecovery)
                    .is_empty()
            );
            assert!(matches!(
                app.server.library.playlist_recovery.as_ref(),
                Some(ServerPlaylistRecoveryModal {
                    stage: ServerPlaylistRecoveryStage::Applying,
                    ..
                })
            ));
        }
    }

    #[test]
    fn delete_confirmation_rejects_shortcut_keys_and_defaults_to_cancel() {
        for code in [KeyCode::Char('y'), KeyCode::Char('n'), KeyCode::Esc] {
            let mut app = app_with_playlist();
            app.start_server_playlist_recovery(
                ServerPlaylistId::new("remote").unwrap(),
                PlaylistId::new("local").unwrap(),
                "Road Trip".to_owned(),
                ServerPlaylistRecoveryAction::DeleteLocal,
            );

            assert!(
                app.on_key_server_playlist_recovery(KeyEvent::new(code, KeyModifiers::NONE))
                    .is_empty()
            );
            assert!(app.server.library.playlist_recovery.is_none());
        }
    }

    #[test]
    fn stale_wrong_and_duplicate_recovery_results_are_ignored() {
        let mut app = app_with_playlist();
        app.start_server_playlist_recovery(
            ServerPlaylistId::new("remote").unwrap(),
            PlaylistId::new("local").unwrap(),
            "Road Trip".to_owned(),
            ServerPlaylistRecoveryAction::DeleteBoth,
        );
        let commands = app.apply_server_playlist_recovery();
        let generation = match commands.as_slice() {
            [Cmd::ServerLibrary(ServerLibraryCommand::RecoverPlaylist { generation, .. })] => {
                *generation
            }
            _ => panic!("recovery command"),
        };

        for event in [
            ServerLibraryEvent::PlaylistRecovered {
                generation: generation + 1,
                action: ServerPlaylistRecoveryAction::DeleteBoth,
                result: Ok(()),
            },
            ServerLibraryEvent::PlaylistRecovered {
                generation,
                action: ServerPlaylistRecoveryAction::DeleteLocal,
                result: Ok(()),
            },
        ] {
            assert!(app.finish_server_playlist_recovery_event(event).is_empty());
            assert!(matches!(
                app.server.library.playlist_recovery.as_ref(),
                Some(ServerPlaylistRecoveryModal {
                    stage: ServerPlaylistRecoveryStage::Applying,
                    ..
                })
            ));
        }

        let completed = ServerLibraryEvent::PlaylistRecovered {
            generation,
            action: ServerPlaylistRecoveryAction::DeleteBoth,
            result: Ok(()),
        };
        assert!(matches!(
            app.finish_server_playlist_recovery_event(completed),
            commands if matches!(
                commands.as_slice(),
                [Cmd::ServerLibrary(ServerLibraryCommand::LoadPage { .. })]
            )
        ));
        assert!(app.server.library.playlist_recovery.is_none());
        assert!(
            app.finish_server_playlist_recovery_event(ServerLibraryEvent::PlaylistRecovered {
                generation,
                action: ServerPlaylistRecoveryAction::DeleteBoth,
                result: Ok(()),
            })
            .is_empty()
        );
    }

    #[test]
    fn restore_carries_the_exact_canonical_local_snapshot() {
        let mut app = app_with_playlist();
        let commands = app.start_server_playlist_recovery(
            ServerPlaylistId::new("missing").unwrap(),
            PlaylistId::new("local").unwrap(),
            "Road Trip".to_owned(),
            ServerPlaylistRecoveryAction::Restore,
        );
        assert!(matches!(
            commands.as_slice(),
            [Cmd::ServerLibrary(ServerLibraryCommand::RecoverPlaylist {
                snapshot: Some(PersonalPlaylistSnapshot {
                    playlist_id,
                    name,
                    ..
                }),
                ..
            })] if playlist_id.as_str() == "local" && name == "Road Trip"
        ));
    }
}
