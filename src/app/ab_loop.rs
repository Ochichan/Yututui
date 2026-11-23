//! App owner for track-scoped A–B loop control. The daemon mirrors this in
//! `daemon/engine/ab_loop.rs`; both evaluate transitions via `yututui_core::ab_loop`.

use yututui_core::ab_loop::{AbLoopAction, AbLoopRefusal, AbLoopState};

use super::*;

impl App {
    /// Active A–B loop state for the currently loaded track.
    pub fn ab_loop(&self) -> AbLoopState {
        self.playback.ab_loop
    }

    pub(in crate::app) fn push_ab_loop_clear_if_set(&self, commands: &mut Vec<PlayerCmd>) {
        if self.playback.ab_loop.is_set() {
            commands.extend(crate::player::ab_loop_commands(AbLoopState::Off));
        }
    }

    /// Reapply the stored A–B loop on a same-track transport/cache recovery, or clear owner state
    /// if recovery is restarting the player idle without reloading the track.
    pub(in crate::app) fn with_ab_loop_restore(
        &mut self,
        mut restore: Vec<PlayerCmd>,
    ) -> Vec<PlayerCmd> {
        if restore.is_empty() {
            self.playback.ab_loop = AbLoopState::Off;
            restore
        } else if self.playback.ab_loop.is_set() {
            let mut bounds = crate::player::ab_loop_commands(self.playback.ab_loop);
            bounds.append(&mut restore);
            bounds
        } else {
            restore
        }
    }

    pub(in crate::app) fn try_ab_loop(
        &self,
        action: AbLoopAction,
        announce: bool,
    ) -> Result<Vec<Cmd>, AbLoopRefusal> {
        if matches!(action, AbLoopAction::Clear) {
            if !self.playback.ab_loop.is_set() {
                return Ok(Vec::new());
            }
            return Ok(vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
                PlayerIntent::batch(
                    "ab_loop_clear",
                    crate::player::ab_loop_commands(AbLoopState::Off),
                    PlayerCommit::AbLoop {
                        state: AbLoopState::Off,
                        seek_to: None,
                        announce,
                    },
                ),
            )))]);
        }

        let Some(song) = self.queue.current() else {
            return Err(AbLoopRefusal::NothingPlaying);
        };
        if self.prefetch.loaded_video_id.is_none() {
            return Err(AbLoopRefusal::NothingPlaying);
        }
        if song.is_radio_station() {
            return Err(AbLoopRefusal::NotSeekable);
        }

        let current_pos = self.playback.time_pos.unwrap_or(0.0);
        let transition =
            self.playback
                .ab_loop
                .transition(action, current_pos, self.playback.duration)?;
        let mut commands = crate::player::ab_loop_commands(transition.state);
        if let Some(target) = transition.seek_to {
            commands.push(PlayerCmd::exact_seek(target));
        }
        Ok(vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
            PlayerIntent::batch(
                "ab_loop",
                commands,
                PlayerCommit::AbLoop {
                    state: transition.state,
                    seek_to: transition.seek_to,
                    announce,
                },
            ),
        )))])
    }

    pub(in crate::app) fn handle_ab_loop_action(&mut self, action: AbLoopAction) -> Vec<Cmd> {
        match self.try_ab_loop(action, true) {
            Ok(cmds) => {
                if cmds.is_empty() && matches!(action, AbLoopAction::Clear) {
                    self.set_status_info(t!(
                        "A–B loop off",
                        "A–B 반복 꺼짐",
                        "A–Bループはオフです"
                    ));
                }
                cmds
            }
            Err(refusal) => {
                self.set_status_info(ab_loop_refusal_message(refusal));
                Vec::new()
            }
        }
    }

    pub(in crate::app) fn commit_ab_loop(
        &mut self,
        state: AbLoopState,
        seek_to: Option<f64>,
        announce: bool,
    ) {
        self.playback.ab_loop = state;
        if let Some(target) = seek_to {
            self.supersede_source_recovery();
            self.playback.time_pos = Some(target);
            self.playback.time_pos_at = Some(Instant::now());
        }
        if announce {
            self.set_status_info(self.ab_loop_status_message(state));
        }
        self.dirty = true;
    }

    fn ab_loop_status_message(&self, state: AbLoopState) -> String {
        match state {
            AbLoopState::Off => {
                t!("A–B loop off", "A–B 반복 해제", "A–Bループを解除しました").to_owned()
            }
            AbLoopState::PointA { a_secs } => {
                let a = crate::util::format::time(a_secs);
                let key = self
                    .keymap
                    .chord(KeyContext::Player, Action::AbLoopCycle)
                    .map_or_else(
                        || "b".to_owned(),
                        |chord| crate::keymap::format_chord_for_display(chord, self.retro_mode()),
                    );
                match crate::i18n::current() {
                    crate::i18n::Language::Korean => {
                        format!("A–B 반복: A 지점 {a} 설정 ({key}를 한 번 더 눌러 B 설정)")
                    }
                    crate::i18n::Language::Japanese => {
                        format!("A–Bループ: A点 {a} を設定（もう一度 {key} でB点を設定）")
                    }
                    crate::i18n::Language::English => {
                        format!("A–B loop: A set at {a} (press {key} again for B)")
                    }
                }
            }
            AbLoopState::Active { a_secs, b_secs } => {
                let a = crate::util::format::time(a_secs);
                let b = crate::util::format::time(b_secs);
                match crate::i18n::current() {
                    crate::i18n::Language::Korean => format!("A–B 반복: {a} – {b}"),
                    crate::i18n::Language::Japanese => format!("A–Bループ: {a} – {b}"),
                    crate::i18n::Language::English => format!("A–B loop: {a} – {b}"),
                }
            }
        }
    }
}

fn ab_loop_refusal_message(refusal: AbLoopRefusal) -> &'static str {
    match refusal {
        AbLoopRefusal::NothingPlaying => t!(
            "Nothing is playing for A–B loop",
            "A–B 반복을 설정할 재생 중인 곡이 없어요",
            "A–Bループを設定する再生中の曲がありません"
        ),
        AbLoopRefusal::NotSeekable => t!(
            "A–B loop isn't available on live radio",
            "실시간 라디오에서는 A–B 반복을 사용할 수 없어요",
            "ライブ配信ではA–Bループを使えません"
        ),
        AbLoopRefusal::InvalidPosition => t!(
            "Invalid A–B loop position",
            "유효하지 않은 A–B 반복 위치예요",
            "無効なA–Bループ位置です"
        ),
        AbLoopRefusal::MissingPointA => t!(
            "Set point A before point B",
            "B 지점 전에 A 지점을 먼저 설정하세요",
            "B点の前にA点を設定してください"
        ),
        AbLoopRefusal::SpanTooShort => t!(
            "Move at least 0.5s away from A to set B",
            "A 지점에서 0.5초 이상 떨어진 위치에서 B를 설정하세요",
            "A点から0.5秒以上離れた位置でB点を設定してください"
        ),
    }
}
