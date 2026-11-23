//! Daemon owner for track-scoped A–B loop control. The App mirrors this in `app/ab_loop.rs`;
//! both evaluate transitions via `yututui_core::ab_loop`.

use yututui_core::ab_loop::{AbLoopAction, AbLoopRefusal, AbLoopState};

use super::*;

impl DaemonEngine {
    /// Prefix `ab-loop-a` / `ab-loop-b` writes when a loop is currently set.
    ///
    /// Ordinary loads write [`AbLoopState::Off`]: mpv keeps loop properties across `loadfile`,
    /// so a new track clears them explicitly. Transport recovery writes the stored state so a
    /// replacement player stays in sync with owner state and observers.
    pub(super) fn with_ab_loop_for_load(
        &self,
        intent: transport::LoadCurrentIntent,
        mut commands: Vec<PlayerCmd>,
    ) -> Vec<PlayerCmd> {
        if self.playback.ab_loop.is_set() {
            let state = match intent {
                transport::LoadCurrentIntent::Ordinary => AbLoopState::Off,
                transport::LoadCurrentIntent::TransportRecovery => self.playback.ab_loop,
            };
            let mut bounds = crate::player::ab_loop_commands(state);
            bounds.append(&mut commands);
            bounds
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

#[cfg(test)]
mod tests {
    use super::super::transport_tests::{
        install_test_player, queue_test_player_start, recv_player_command,
    };
    use super::*;

    fn stored_ab_loop() -> AbLoopState {
        AbLoopState::Active {
            a_secs: 12.5,
            b_secs: 48.0,
        }
    }

    async fn assert_recovery_writes_stored_ab_loop(
        rx: &mut tokio::sync::mpsc::Receiver<PlayerCmd>,
        resume: bool,
    ) {
        assert!(matches!(
            recv_player_command(rx).await,
            PlayerCmd::SetVolume(50)
        ));
        assert!(matches!(
            recv_player_command(rx).await,
            PlayerCmd::SetAudioFilter(_)
        ));
        match recv_player_command(rx).await {
            PlayerCmd::SetProperty { name, value } if name == "ab-loop-a" => {
                assert_eq!(value, serde_json::Value::from(12.5));
            }
            _ => panic!("recovery must reapply ab-loop-a"),
        }
        match recv_player_command(rx).await {
            PlayerCmd::SetProperty { name, value } if name == "ab-loop-b" => {
                assert_eq!(value, serde_json::Value::from(48.0));
            }
            _ => panic!("recovery must reapply ab-loop-b"),
        }
        if resume {
            assert!(matches!(
                recv_player_command(rx).await,
                PlayerCmd::LoadWithResume(_)
            ));
        } else {
            assert!(matches!(recv_player_command(rx).await, PlayerCmd::Load(_)));
            assert!(matches!(
                recv_player_command(rx).await,
                PlayerCmd::CyclePause
            ));
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn transport_recovery_reapplies_stored_ab_loop_and_keeps_owner_state() {
        let stored = stored_ab_loop();
        let expected = crate::remote::proto::AbLoopModel::from_state(stored);

        let mut engine = super::super::tests::engine_with_queue(&["seed"]);
        engine.loaded_video_id = Some("seed".to_owned());
        engine.playback.paused = true;
        engine.playback.ab_loop = stored;
        let old_rx = install_test_player(&mut engine);
        let mut replacement_rx = queue_test_player_start(&mut engine);

        assert!(
            engine
                .handle_player_event(PlayerEvent::TransportClosed("ipc reset".to_owned()))
                .await
                .is_empty()
        );
        assert!(old_rx.is_closed());
        assert_eq!(engine.playback.ab_loop, stored);
        assert_eq!(engine.status().ab_loop, expected);
        assert_recovery_writes_stored_ab_loop(&mut replacement_rx, false).await;

        engine
            .load_current_loaded()
            .expect("ordinary reload should still clear the loop");
        assert_eq!(engine.playback.ab_loop, AbLoopState::Off);
        assert!(engine.status().ab_loop.is_none());
        match recv_player_command(&mut replacement_rx).await {
            PlayerCmd::SetProperty { name, value } if name == "ab-loop-a" => {
                assert_eq!(value, serde_json::Value::String("no".to_owned()));
            }
            _ => panic!("ordinary load must clear ab-loop-a"),
        }
        match recv_player_command(&mut replacement_rx).await {
            PlayerCmd::SetProperty { name, value } if name == "ab-loop-b" => {
                assert_eq!(value, serde_json::Value::String("no".to_owned()));
            }
            _ => panic!("ordinary load must clear ab-loop-b"),
        }
        assert!(matches!(
            recv_player_command(&mut replacement_rx).await,
            PlayerCmd::Load(_)
        ));
        assert!(replacement_rx.try_recv().is_err());

        let mut engine = super::super::tests::engine_with_queue(&["seed"]);
        engine.loaded_video_id = Some("seed".to_owned());
        engine.playback.paused = true;
        engine.playback.time_pos = Some(30.0);
        engine.playback.duration = Some(180.0);
        engine.playback.ab_loop = stored;
        let old_rx = install_test_player(&mut engine);
        let mut replacement_rx = queue_test_player_start(&mut engine);

        assert!(
            engine
                .handle_player_event(PlayerEvent::CacheEmergency {
                    file_generation: 0,
                    position_secs: 30.0,
                    paused: true,
                    reason: crate::player::long_form_seek::CacheReason::DisableFailed,
                })
                .await
                .is_empty()
        );
        assert!(old_rx.is_closed());
        assert_eq!(engine.playback.ab_loop, stored);
        assert_eq!(engine.playback.time_pos, Some(30.0));
        assert_eq!(engine.status().ab_loop, expected);
        assert_recovery_writes_stored_ab_loop(&mut replacement_rx, true).await;
    }
}
