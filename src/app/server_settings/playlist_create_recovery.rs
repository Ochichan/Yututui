//! Explicit UI recovery for replay-unsafe server playlist creation.

use super::*;

impl App {
    pub(super) fn open_playlist_create_abandon_confirmation(&mut self) -> Vec<Cmd> {
        let Some(attention) = self
            .server
            .settings
            .summary
            .playlist_create_attention
            .first()
            .cloned()
        else {
            return Vec::new();
        };
        self.server.settings.wizard =
            Some(MusicServerWizard::AbandonPlaylistCreateConfirm(attention));
        self.dirty = true;
        Vec::new()
    }

    pub(super) fn start_abandon_playlist_create(&mut self) -> Vec<Cmd> {
        if self.server.settings.busy.is_some() {
            return Vec::new();
        }
        let Some(MusicServerWizard::AbandonPlaylistCreateConfirm(attention)) =
            self.server.settings.wizard.take()
        else {
            return Vec::new();
        };
        let generation = self.server.settings.next_generation();
        self.server.settings.busy = Some(MusicServerBusy::PlaylistRecovery);
        self.server.settings.wizard = Some(MusicServerWizard::Waiting);
        self.dirty = true;
        vec![Cmd::MusicServer(
            MusicServerCommand::AbandonPlaylistCreate {
                generation,
                local_playlist_id: attention.local_playlist_id,
            },
        )]
    }

    pub(super) fn finish_playlist_create_abandoned(
        &mut self,
        result: Result<MusicServerSummary, MusicServerFailure>,
    ) -> Vec<Cmd> {
        self.server.settings.busy = None;
        self.server.settings.wizard = None;
        match result {
            Ok(summary) => {
                self.server.settings.summary = summary;
                self.server.settings.failure = None;
                self.status.kind = StatusKind::Info;
                self.status.text = crate::t!(
                    "Pending playlist creation forgotten; any server copy was left untouched",
                    "보류 중인 플레이리스트 생성을 잊었어요. 서버 복사본은 건드리지 않았어요",
                    "保留中のプレイリスト作成を破棄しました。サーバー上のコピーは変更していません"
                )
                .to_owned();
            }
            Err(failure) => {
                self.server.settings.failure = Some(failure);
                self.server.settings.summary.health = MusicServerHealth::NeedsAttention;
                self.status.kind = StatusKind::Error;
                self.status.text = failure.label().to_owned();
            }
        }
        self.dirty = true;
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn pending_playlist_create_adds_a_confirmed_abandon_action() {
        let local_playlist_id = crate::personal_state::PlaylistId::new("local-pending").unwrap();
        let mut app = App::new(50);
        app.server.settings.summary.configured = true;
        app.server.settings.summary.health = MusicServerHealth::NeedsAttention;
        app.server
            .settings
            .summary
            .playlist_creates_needing_decision = 1;
        app.server.settings.summary.playlist_create_attention = vec![PlaylistCreateAttention {
            local_playlist_id: local_playlist_id.clone(),
            state: crate::open_subsonic::PlaylistCreateRecoveryState::ServerIdentityUnknown,
        }];

        assert_eq!(app.server.settings.row_count(), 5);
        assert!(app.activate_music_server_row(3).is_empty());
        assert!(matches!(
            app.server.settings.wizard,
            Some(MusicServerWizard::AbandonPlaylistCreateConfirm(ref attention))
                if attention.local_playlist_id == local_playlist_id
        ));

        let commands = app.on_key_music_server_settings(key(KeyCode::Enter));
        assert!(matches!(
            commands.as_slice(),
            [Cmd::MusicServer(MusicServerCommand::AbandonPlaylistCreate {
                local_playlist_id: pending,
                ..
            })] if pending == &local_playlist_id
        ));
        assert_eq!(
            app.server.settings.busy,
            Some(MusicServerBusy::PlaylistRecovery)
        );
        assert!(matches!(
            app.server.settings.wizard,
            Some(MusicServerWizard::Waiting)
        ));
        assert!(
            app.on_key_music_server_settings(key(KeyCode::Enter))
                .is_empty()
        );
    }
}
