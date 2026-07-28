//! Minimal TUI controls for the experimental detailed-history credential.

use super::{App, Cmd, MusicServerBusy, MusicServerCommand, MusicServerHistoryHealth, StatusKind};

impl App {
    pub(in crate::app) fn activate_music_server_history(&mut self) -> Vec<Cmd> {
        match self.server.settings.summary.history {
            MusicServerHistoryHealth::Off => {
                self.status.kind = StatusKind::Info;
                self.status.text = crate::t!(
                    "Enable detailed history with: ytt server history enable --experimental",
                    "다음 명령으로 상세 이력을 켜세요: ytt server history enable --experimental",
                    "次のコマンドで詳細履歴を有効にします: ytt server history enable --experimental"
                )
                .to_owned();
                self.dirty = true;
                Vec::new()
            }
            MusicServerHistoryHealth::Probing
            | MusicServerHistoryHealth::Detailed
            | MusicServerHistoryHealth::PlayCountsOnly
            | MusicServerHistoryHealth::UpdatePassword => {
                let generation = self.server.settings.next_generation();
                self.server.settings.busy = Some(MusicServerBusy::History);
                self.server.settings.failure = None;
                self.dirty = true;
                vec![Cmd::MusicServer(MusicServerCommand::DisableHistory {
                    generation,
                })]
            }
        }
    }
}
