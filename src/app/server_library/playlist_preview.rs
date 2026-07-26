//! App-owned, generation-stamped server-playlist preview state.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{
    App, Cmd, LibrarySource, ServerLibraryCommand, ServerLibraryEvent, ServerLibraryFailure,
    ServerPlaylistId, StatusKind,
};
use crate::keymap::Chord;
use crate::open_subsonic::{PlaylistMergePreview, PlaylistPreviewMode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerPlaylistPreviewKind {
    ImportCopy,
    LinkAndSync,
}

impl ServerPlaylistPreviewKind {
    const fn expected_mode(self) -> PlaylistPreviewMode {
        match self {
            Self::ImportCopy => PlaylistPreviewMode::ImportCopy,
            Self::LinkAndSync => PlaylistPreviewMode::LinkNew,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerPlaylistPreviewStage {
    Preparing {
        server_playlist_id: ServerPlaylistId,
        kind: ServerPlaylistPreviewKind,
    },
    Ready(PlaylistMergePreview),
    Applying(PlaylistMergePreview),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerPlaylistPreviewModal {
    pub generation: u64,
    pub kind: ServerPlaylistPreviewKind,
    pub stage: ServerPlaylistPreviewStage,
}

impl ServerPlaylistPreviewModal {
    pub fn preview(&self) -> Option<&PlaylistMergePreview> {
        match &self.stage {
            ServerPlaylistPreviewStage::Ready(preview)
            | ServerPlaylistPreviewStage::Applying(preview) => Some(preview),
            ServerPlaylistPreviewStage::Preparing { .. } => None,
        }
    }

    pub const fn busy(&self) -> bool {
        matches!(
            self.stage,
            ServerPlaylistPreviewStage::Preparing { .. } | ServerPlaylistPreviewStage::Applying(_)
        )
    }
}

impl App {
    pub(in crate::app) fn start_server_playlist_preview(
        &mut self,
        server_playlist_id: ServerPlaylistId,
        kind: ServerPlaylistPreviewKind,
    ) -> Vec<Cmd> {
        if self.server.library.source != LibrarySource::OpenSubsonic
            || self.server.library.busy.is_some()
            || self.server.library.playlist_preview.is_some()
            || self.server.library.playlist_create.is_some()
            || self.server.library.playlist_recovery.is_some()
        {
            return Vec::new();
        }
        let generation = self.server.library.next_preview_generation();
        self.server.library.failure = None;
        self.server.library.playlist_preview = Some(ServerPlaylistPreviewModal {
            generation,
            kind,
            stage: ServerPlaylistPreviewStage::Preparing {
                server_playlist_id: server_playlist_id.clone(),
                kind,
            },
        });
        self.dirty = true;
        vec![Cmd::ServerLibrary(ServerLibraryCommand::PreparePlaylist {
            generation,
            server_playlist_id,
            kind,
        })]
    }

    pub(in crate::app) fn cancel_server_playlist_preview(&mut self) -> Vec<Cmd> {
        if self
            .server
            .library
            .playlist_preview
            .as_ref()
            .is_some_and(|modal| matches!(modal.stage, ServerPlaylistPreviewStage::Applying(_)))
        {
            // Apply already crossed the actor boundary and cannot honestly be cancelled.
            return Vec::new();
        }
        if self.server.library.playlist_preview.take().is_some() {
            self.server.library.next_preview_generation();
            self.dirty = true;
        }
        Vec::new()
    }

    pub(in crate::app) fn apply_server_playlist_preview(&mut self) -> Vec<Cmd> {
        let Some(modal) = self.server.library.playlist_preview.as_mut() else {
            return Vec::new();
        };
        let ServerPlaylistPreviewStage::Ready(preview) = &modal.stage else {
            return Vec::new();
        };
        let generation = modal.generation;
        let preview_id = preview.preview_id.clone();
        let server_playlist_id = preview.server_playlist_id.clone();
        modal.stage = ServerPlaylistPreviewStage::Applying(preview.clone());
        self.dirty = true;
        vec![Cmd::ServerLibrary(
            ServerLibraryCommand::ApplyPlaylistPreview {
                generation,
                preview_id,
                server_playlist_id,
            },
        )]
    }

    pub(in crate::app) fn on_key_server_playlist_preview(&mut self, key: KeyEvent) -> Vec<Cmd> {
        if self
            .server
            .library
            .playlist_preview
            .as_ref()
            .is_some_and(|modal| matches!(modal.stage, ServerPlaylistPreviewStage::Applying(_)))
        {
            return Vec::new();
        }
        let chord = Chord::from(key);
        let confirmed = key.code == KeyCode::Enter
            || chord == Chord::new(KeyCode::Char('y'), KeyModifiers::empty());
        if confirmed {
            self.apply_server_playlist_preview()
        } else {
            self.cancel_server_playlist_preview()
        }
    }

    pub(in crate::app) fn finish_server_playlist_preview_event(
        &mut self,
        event: ServerLibraryEvent,
    ) -> Vec<Cmd> {
        let generation = match &event {
            ServerLibraryEvent::PlaylistPrepared { generation, .. }
            | ServerLibraryEvent::PlaylistApplied { generation, .. } => *generation,
            ServerLibraryEvent::PageLoaded { .. }
            | ServerLibraryEvent::DetailLoaded { .. }
            | ServerLibraryEvent::PlaylistCreated { .. }
            | ServerLibraryEvent::PlaylistRecovered { .. } => {
                return Vec::new();
            }
        };
        let Some(modal) = self.server.library.playlist_preview.as_ref() else {
            return Vec::new();
        };
        if modal.generation != generation
            || self.server.library.source != LibrarySource::OpenSubsonic
        {
            return Vec::new();
        }

        match event {
            ServerLibraryEvent::PlaylistPrepared { result, .. } => {
                let (expected_id, kind) = match &modal.stage {
                    ServerPlaylistPreviewStage::Preparing {
                        server_playlist_id,
                        kind,
                    } => (server_playlist_id.clone(), *kind),
                    ServerPlaylistPreviewStage::Ready(_)
                    | ServerPlaylistPreviewStage::Applying(_) => return Vec::new(),
                };
                match result {
                    Ok(preview)
                        if preview.server_playlist_id == expected_id
                            && preview.mode == kind.expected_mode() =>
                    {
                        self.server.library.playlist_preview = Some(ServerPlaylistPreviewModal {
                            generation,
                            kind,
                            stage: ServerPlaylistPreviewStage::Ready(preview),
                        });
                    }
                    Ok(_) => {
                        self.fail_server_playlist_preview(ServerLibraryFailure::InvalidResponse)
                    }
                    Err(failure) => self.fail_server_playlist_preview(failure),
                }
            }
            ServerLibraryEvent::PlaylistApplied { result, .. } => {
                let (kind, name) = match &modal.stage {
                    ServerPlaylistPreviewStage::Applying(preview) => {
                        (modal.kind, preview.name.clone())
                    }
                    ServerPlaylistPreviewStage::Preparing { .. }
                    | ServerPlaylistPreviewStage::Ready(_) => return Vec::new(),
                };
                match result {
                    Ok(_) => {
                        self.server.library.playlist_preview = None;
                        self.server.library.failure = None;
                        self.status.kind = StatusKind::Info;
                        self.status.text = success_message(kind, &name);
                        self.dirty = true;
                        if kind == ServerPlaylistPreviewKind::LinkAndSync
                            && self.server.library.detail.is_none()
                        {
                            return self
                                .request_server_library_page(self.server.library.offset, false);
                        }
                    }
                    Err(failure) => self.fail_server_playlist_preview(failure),
                }
            }
            ServerLibraryEvent::PageLoaded { .. }
            | ServerLibraryEvent::DetailLoaded { .. }
            | ServerLibraryEvent::PlaylistCreated { .. }
            | ServerLibraryEvent::PlaylistRecovered { .. } => {
                return Vec::new();
            }
        }
        self.dirty = true;
        Vec::new()
    }

    fn fail_server_playlist_preview(&mut self, failure: ServerLibraryFailure) {
        self.server.library.playlist_preview = None;
        self.server.library.failure = Some(failure);
        self.status.kind = StatusKind::Error;
        self.status.text = failure.label().to_owned();
    }
}

fn success_message(kind: ServerPlaylistPreviewKind, name: &str) -> String {
    match (crate::i18n::current(), kind) {
        (crate::i18n::Language::Korean, ServerPlaylistPreviewKind::ImportCopy) => {
            format!("“{name}” 복사본을 가져왔어요.")
        }
        (crate::i18n::Language::Japanese, ServerPlaylistPreviewKind::ImportCopy) => {
            format!("「{name}」をコピーとしてインポートしました。")
        }
        (_, ServerPlaylistPreviewKind::ImportCopy) => {
            format!("Imported a copy of “{name}”.")
        }
        (crate::i18n::Language::Korean, ServerPlaylistPreviewKind::LinkAndSync) => {
            format!("“{name}”을(를) 연결했어요.")
        }
        (crate::i18n::Language::Japanese, ServerPlaylistPreviewKind::LinkAndSync) => {
            format!("「{name}」をリンクしました。")
        }
        (_, ServerPlaylistPreviewKind::LinkAndSync) => {
            format!("Linked “{name}”.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_subsonic::ServerPlaylistId;
    use crate::personal_state::PlaylistId;

    fn preview(kind: ServerPlaylistPreviewKind) -> PlaylistMergePreview {
        PlaylistMergePreview {
            preview_id: "preview".to_owned(),
            mode: kind.expected_mode(),
            server_playlist_id: ServerPlaylistId::new("remote").unwrap(),
            local_playlist_id: PlaylistId::new("local").unwrap(),
            name: "Road Trip".to_owned(),
            remote_tracks: 4,
            add_to_local: 3,
            add_to_server: 0,
        }
    }

    fn app() -> App {
        let mut app = App::new(50);
        app.server.library.source = LibrarySource::OpenSubsonic;
        app
    }

    #[test]
    fn ready_preview_only_dispatches_apply_once() {
        let mut app = app();
        let prepare = app.start_server_playlist_preview(
            ServerPlaylistId::new("remote").unwrap(),
            ServerPlaylistPreviewKind::ImportCopy,
        );
        let generation = match prepare.as_slice() {
            [Cmd::ServerLibrary(ServerLibraryCommand::PreparePlaylist { generation, .. })] => {
                *generation
            }
            _ => panic!("expected prepare command"),
        };
        app.finish_server_playlist_preview_event(ServerLibraryEvent::PlaylistPrepared {
            generation,
            result: Ok(preview(ServerPlaylistPreviewKind::ImportCopy)),
        });

        assert_eq!(app.apply_server_playlist_preview().len(), 1);
        assert!(app.apply_server_playlist_preview().is_empty());
        assert!(matches!(
            app.server
                .library
                .playlist_preview
                .as_ref()
                .map(|modal| &modal.stage),
            Some(ServerPlaylistPreviewStage::Applying(_))
        ));
    }

    #[test]
    fn stale_or_mismatched_prepare_result_never_opens_confirmation() {
        let mut app = app();
        let commands = app.start_server_playlist_preview(
            ServerPlaylistId::new("remote").unwrap(),
            ServerPlaylistPreviewKind::LinkAndSync,
        );
        let generation = match commands.as_slice() {
            [Cmd::ServerLibrary(ServerLibraryCommand::PreparePlaylist { generation, .. })] => {
                *generation
            }
            _ => panic!("expected prepare command"),
        };

        app.finish_server_playlist_preview_event(ServerLibraryEvent::PlaylistPrepared {
            generation: generation + 1,
            result: Ok(preview(ServerPlaylistPreviewKind::LinkAndSync)),
        });
        assert!(matches!(
            app.server
                .library
                .playlist_preview
                .as_ref()
                .map(|modal| &modal.stage),
            Some(ServerPlaylistPreviewStage::Preparing { .. })
        ));

        app.finish_server_playlist_preview_event(ServerLibraryEvent::PlaylistPrepared {
            generation,
            result: Ok(preview(ServerPlaylistPreviewKind::ImportCopy)),
        });
        assert!(app.server.library.playlist_preview.is_none());
        assert_eq!(
            app.server.library.failure,
            Some(ServerLibraryFailure::InvalidResponse)
        );
    }

    #[test]
    fn any_non_confirmation_key_cancels_and_late_result_is_ignored() {
        let mut app = app();
        let commands = app.start_server_playlist_preview(
            ServerPlaylistId::new("remote").unwrap(),
            ServerPlaylistPreviewKind::ImportCopy,
        );
        let generation = match commands.as_slice() {
            [Cmd::ServerLibrary(ServerLibraryCommand::PreparePlaylist { generation, .. })] => {
                *generation
            }
            _ => panic!("expected prepare command"),
        };
        assert!(
            app.on_key_server_playlist_preview(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE,))
                .is_empty()
        );
        assert!(app.server.library.playlist_preview.is_none());

        app.finish_server_playlist_preview_event(ServerLibraryEvent::PlaylistPrepared {
            generation,
            result: Ok(preview(ServerPlaylistPreviewKind::ImportCopy)),
        });
        assert!(app.server.library.playlist_preview.is_none());
    }
}
