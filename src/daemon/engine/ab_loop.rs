//! Daemon owner for track-scoped A–B loop control. The App mirrors this in `app/ab_loop.rs`;
//! both evaluate transitions via `yututui_core::ab_loop`.

use yututui_core::ab_loop::{AbLoopAction, AbLoopRefusal, AbLoopState};

use super::*;

impl DaemonEngine {
    pub(super) fn with_ab_loop_clear(&self, mut commands: Vec<PlayerCmd>) -> Vec<PlayerCmd> {
        if self.playback.ab_loop.is_set() {
            let mut clear = crate::player::ab_loop_commands(AbLoopState::Off);
            clear.append(&mut commands);
            clear
        } else {
            commands
        }
    }

    pub(super) fn on_ordinary_track_loaded(&mut self, song: &Song) {
        self.playback.ab_loop = AbLoopState::Off;
        self.sponsorblock_on_load(song);
    }

    pub(super) fn clear_ab_loop_on_stop(&self, handle: &PlayerHandle) {
        if self.playback.ab_loop.is_set() {
            record_player_delivery(
                "ab_loop_clear_stop",
                handle.send_batch(crate::player::ab_loop_commands(AbLoopState::Off)),
            );
        }
    }

    pub(super) fn apply_ab_loop(&mut self, action: AbLoopAction) -> RemoteResponse {
        if matches!(action, AbLoopAction::Clear) {
            if !self.playback.ab_loop.is_set() {
                return RemoteResponse::status(self.status());
            }
            if let Err(error) = self.send_active_player_batch(
                "ab_loop_clear",
                crate::player::ab_loop_commands(AbLoopState::Off),
            ) {
                return self.reject_player_command(error);
            }
            self.playback.ab_loop = AbLoopState::Off;
            return RemoteResponse::status(self.status());
        }

        let Some(song) = self.queue.current() else {
            return RemoteResponse::err(AbLoopRefusal::NothingPlaying.reason());
        };
        if self.loaded_video_id.is_none() {
            return RemoteResponse::err(AbLoopRefusal::NothingPlaying.reason());
        }
        if song.is_radio_station() {
            return RemoteResponse::err(AbLoopRefusal::NotSeekable.reason());
        }

        let current_pos = self.playback.time_pos.unwrap_or(0.0);
        let transition =
            match self
                .playback
                .ab_loop
                .transition(action, current_pos, self.playback.duration)
            {
                Ok(transition) => transition,
                Err(refusal) => return RemoteResponse::err(refusal.reason()),
            };

        let mut commands = crate::player::ab_loop_commands(transition.state);
        if let Some(target) = transition.seek_to {
            commands.push(PlayerCmd::exact_seek(target));
        }
        if let Err(error) = self.send_active_player_batch("ab_loop", commands) {
            return self.reject_player_command(error);
        }
        self.playback.ab_loop = transition.state;
        if let Some(target) = transition.seek_to {
            self.note_seek(target);
        }
        RemoteResponse::status(self.status())
    }

    #[cfg(test)]
    pub(crate) fn set_seek_parity_position(&mut self, position: f64) {
        self.playback.time_pos = Some(position);
    }
}
