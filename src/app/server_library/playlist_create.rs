//! Explicit, deletion-free creation of a linked server copy from one local playlist.

use crossterm::event::{KeyCode, KeyEvent};

use super::{
    App, Cmd, LibrarySource, ServerLibraryCommand, ServerLibraryEvent, ServerLibraryFailure,
    StatusKind,
};
use crate::personal_state::{PersonalPlaylistSnapshot, PlaylistId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerPlaylistCreateStage {
    Confirming,
    Applying,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerPlaylistCreateModal {
    pub generation: u64,
    pub snapshot: PersonalPlaylistSnapshot,
    pub stage: ServerPlaylistCreateStage,
}

impl ServerPlaylistCreateModal {
    pub fn playlist_id(&self) -> &PlaylistId {
        &self.snapshot.playlist_id
    }

    pub fn name(&self) -> &str {
        &self.snapshot.name
    }

    pub fn server_additions(&self) -> usize {
        self.snapshot.entries.len()
    }
}

impl App {
    pub(in crate::app) fn start_server_playlist_create(
        &mut self,
        local_playlist_id: PlaylistId,
    ) -> Vec<Cmd> {
        if !self.server.settings.summary.configured
            || self.server.library.source != LibrarySource::Yututui
            || self.server.library.playlist_preview.is_some()
            || self.server.library.playlist_create.is_some()
            || self.server.library.playlist_recovery.is_some()
        {
            return Vec::new();
        }
        let snapshot = match crate::personal_state::personal_playlist_snapshot(
            &self.personal_state.ledger,
            &local_playlist_id,
        ) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) | Err(_) => {
                self.fail_server_playlist_create_changed();
                return Vec::new();
            }
        };
        let generation = self.server.library.next_create_generation();
        self.server.library.failure = None;
        self.server.library.playlist_create = Some(ServerPlaylistCreateModal {
            generation,
            snapshot,
            stage: ServerPlaylistCreateStage::Confirming,
        });
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn apply_server_playlist_create(&mut self) -> Vec<Cmd> {
        let Some(modal) = self.server.library.playlist_create.as_ref() else {
            return Vec::new();
        };
        if modal.stage != ServerPlaylistCreateStage::Confirming {
            return Vec::new();
        }
        let generation = modal.generation;
        let expected = modal.snapshot.clone();
        let current = crate::personal_state::personal_playlist_snapshot(
            &self.personal_state.ledger,
            &expected.playlist_id,
        );
        if !matches!(current, Ok(Some(ref snapshot)) if snapshot == &expected) {
            self.fail_server_playlist_create_changed();
            return Vec::new();
        }
        let Some(modal) = self.server.library.playlist_create.as_mut() else {
            return Vec::new();
        };
        modal.stage = ServerPlaylistCreateStage::Applying;
        self.dirty = true;
        vec![Cmd::ServerLibrary(
            ServerLibraryCommand::CreateLinkedPlaylist {
                generation,
                snapshot: expected,
            },
        )]
    }

    pub(in crate::app) fn cancel_server_playlist_create(&mut self) -> Vec<Cmd> {
        if self
            .server
            .library
            .playlist_create
            .as_ref()
            .is_some_and(|modal| modal.stage == ServerPlaylistCreateStage::Applying)
        {
            return Vec::new();
        }
        if self.server.library.playlist_create.take().is_some() {
            self.server.library.next_create_generation();
            self.dirty = true;
        }
        Vec::new()
    }

    pub(in crate::app) fn on_key_server_playlist_create(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self
            .server
            .library
            .playlist_create
            .as_ref()
            .is_some_and(|modal| modal.stage == ServerPlaylistCreateStage::Applying)
        {
            return Vec::new();
        }
        match key.code {
            KeyCode::Enter => self.apply_server_playlist_create(),
            KeyCode::Esc => self.cancel_server_playlist_create(),
            _ => Vec::new(),
        }
    }

    pub(in crate::app) fn finish_server_playlist_create_event(
        &mut self,
        event: ServerLibraryEvent,
    ) -> Vec<Cmd> {
        let ServerLibraryEvent::PlaylistCreated {
            generation,
            local_playlist_id,
            result,
        } = event
        else {
            return Vec::new();
        };
        let Some(modal) = self.server.library.playlist_create.as_ref() else {
            return Vec::new();
        };
        if modal.generation != generation
            || modal.playlist_id() != &local_playlist_id
            || modal.stage != ServerPlaylistCreateStage::Applying
        {
            return Vec::new();
        }
        match result {
            Ok(_) => {
                let name = modal.name().to_owned();
                self.server.library.playlist_create = None;
                self.server.library.failure = None;
                self.status.kind = StatusKind::Info;
                self.status.text = match crate::i18n::current() {
                    crate::i18n::Language::Korean => {
                        format!("“{name}”의 서버 복사본을 만들고 연결했어요.")
                    }
                    crate::i18n::Language::Japanese => {
                        format!("「{name}」のサーバーコピーを作成してリンクしました。")
                    }
                    _ => format!("Created and linked the server copy of “{name}”."),
                };
                self.dirty = true;
                self.request_music_server_status()
            }
            Err(failure) => {
                self.fail_server_playlist_create(failure);
                Vec::new()
            }
        }
    }

    fn fail_server_playlist_create(&mut self, failure: ServerLibraryFailure) {
        self.server.library.playlist_create = None;
        self.server.library.failure = Some(failure);
        self.status.kind = StatusKind::Error;
        self.status.text = failure.label().to_owned();
        self.dirty = true;
    }

    fn fail_server_playlist_create_changed(&mut self) {
        self.server.library.playlist_create = None;
        self.server.library.next_create_generation();
        self.status.kind = StatusKind::Error;
        self.status.text = crate::t!(
            "Playlist changed. Open Create linked server playlist again.",
            "플레이리스트가 바뀌었어요. 서버 연결 목록 만들기를 다시 열어 주세요.",
            "プレイリストが変更されました。サーバー連携リストの作成を開き直してください。"
        )
        .to_owned();
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::personal_state::{
        ExternalOperationInput, Operation, OperationOrigin, append_external_operations,
    };

    fn app_with_playlist() -> App {
        let mut app = App::new(50);
        app.server.settings.summary.configured = true;
        let (ledger, _) = append_external_operations(
            &app.personal_state.ledger,
            OperationOrigin::Imported,
            &[ExternalOperationInput {
                acknowledgement_id: "create-local-playlist".to_owned(),
                operation: Operation::UpsertPlaylist {
                    playlist_id: PlaylistId::new("local").unwrap(),
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
    fn confirmation_dispatches_the_exact_snapshot_once() {
        let mut app = app_with_playlist();
        assert!(
            app.start_server_playlist_create(PlaylistId::new("local").unwrap())
                .is_empty()
        );
        assert!(matches!(
            app.server.library.playlist_create.as_ref(),
            Some(ServerPlaylistCreateModal {
                stage: ServerPlaylistCreateStage::Confirming,
                ..
            })
        ));

        let commands =
            app.on_key_server_playlist_create(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            commands.as_slice(),
            [Cmd::ServerLibrary(ServerLibraryCommand::CreateLinkedPlaylist {
                snapshot,
                ..
            })] if snapshot.playlist_id.as_str() == "local" && snapshot.name == "Road Trip"
        ));
        assert!(
            app.on_key_server_playlist_create(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE,))
                .is_empty()
        );
        assert!(
            app.on_key_server_playlist_create(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .is_empty()
        );
        assert!(matches!(
            app.server.library.playlist_create.as_ref(),
            Some(ServerPlaylistCreateModal {
                stage: ServerPlaylistCreateStage::Applying,
                ..
            })
        ));
    }

    #[test]
    fn confirmation_rejects_a_playlist_changed_after_opening() {
        let mut app = app_with_playlist();
        app.start_server_playlist_create(PlaylistId::new("local").unwrap());
        let (ledger, _) = append_external_operations(
            &app.personal_state.ledger,
            OperationOrigin::Imported,
            &[ExternalOperationInput {
                acknowledgement_id: "rename-after-create-preview".to_owned(),
                operation: Operation::UpsertPlaylist {
                    playlist_id: PlaylistId::new("local").unwrap(),
                    name: "Changed".to_owned(),
                },
                recorded_at_unix: 2,
            }],
        )
        .unwrap();
        app.personal_state.replace_ledger(ledger);

        assert!(app.apply_server_playlist_create().is_empty());
        assert!(app.server.library.playlist_create.is_none());
        assert_eq!(app.status.kind, StatusKind::Error);
        assert!(!app.status.text.is_empty());
    }

    #[test]
    fn stale_mismatched_and_duplicate_results_are_ignored() {
        let mut app = app_with_playlist();
        app.start_server_playlist_create(PlaylistId::new("local").unwrap());
        let commands = app.apply_server_playlist_create();
        let generation = match commands.as_slice() {
            [
                Cmd::ServerLibrary(ServerLibraryCommand::CreateLinkedPlaylist {
                    generation, ..
                }),
            ] => *generation,
            _ => panic!("create command"),
        };
        for event in [
            ServerLibraryEvent::PlaylistCreated {
                generation: generation + 1,
                local_playlist_id: PlaylistId::new("local").unwrap(),
                result: Ok(crate::open_subsonic::ServerPlaylistId::new("remote").unwrap()),
            },
            ServerLibraryEvent::PlaylistCreated {
                generation,
                local_playlist_id: PlaylistId::new("other").unwrap(),
                result: Ok(crate::open_subsonic::ServerPlaylistId::new("remote").unwrap()),
            },
        ] {
            assert!(app.finish_server_playlist_create_event(event).is_empty());
            assert!(matches!(
                app.server.library.playlist_create.as_ref(),
                Some(ServerPlaylistCreateModal {
                    stage: ServerPlaylistCreateStage::Applying,
                    ..
                })
            ));
        }

        let completed = ServerLibraryEvent::PlaylistCreated {
            generation,
            local_playlist_id: PlaylistId::new("local").unwrap(),
            result: Ok(crate::open_subsonic::ServerPlaylistId::new("remote").unwrap()),
        };
        assert!(matches!(
            app.finish_server_playlist_create_event(completed)
                .as_slice(),
            [Cmd::MusicServer(
                crate::app::MusicServerCommand::Refresh { .. }
            )]
        ));
        assert!(app.server.library.playlist_create.is_none());
        assert!(
            app.finish_server_playlist_create_event(ServerLibraryEvent::PlaylistCreated {
                generation,
                local_playlist_id: PlaylistId::new("local").unwrap(),
                result: Ok(crate::open_subsonic::ServerPlaylistId::new("remote").unwrap()),
            })
            .is_empty()
        );
    }

    #[test]
    fn create_is_hidden_without_a_configured_server_and_escape_cancels() {
        let mut app = app_with_playlist();
        app.server.settings.summary.configured = false;
        assert!(
            app.start_server_playlist_create(PlaylistId::new("local").unwrap())
                .is_empty()
        );
        assert!(app.server.library.playlist_create.is_none());

        app.server.settings.summary.configured = true;
        app.start_server_playlist_create(PlaylistId::new("local").unwrap());
        assert!(
            app.on_key_server_playlist_create(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .is_empty()
        );
        assert!(app.server.library.playlist_create.is_none());
    }
}
