//! Remote-command admission and dispatch for the daemon playback owner.

use super::*;

impl DaemonEngine {
    pub async fn handle_remote(
        &mut self,
        command: RemoteCommand,
    ) -> (RemoteResponse, bool, Vec<EngineEffect>) {
        if command
            .expected_queue_rev()
            .is_some_and(|revision| revision != self.queue.rev())
        {
            return (RemoteResponse::err("stale_rev"), false, Vec::new());
        }
        if let Some(response) = self.preflight_remote_persistence(&command) {
            return (response, false, Vec::new());
        }
        let mut effects = Vec::new();
        let shutdown = matches!(command, RemoteCommand::Quit);
        let response = match command {
            RemoteCommand::ExportPersonalData { .. } => {
                unreachable!("personal export is intercepted by the daemon owner loop")
            }
            RemoteCommand::SyncNow | RemoteCommand::SyncRevokeDevice { .. } => {
                unreachable!("personal sync is intercepted by the daemon owner loop")
            }
            RemoteCommand::Status => RemoteResponse::status(self.status()),
            RemoteCommand::Quit => {
                self.stop_playback();
                // `stop_playback` rearms normal transport recovery for future loads. Process
                // teardown is terminal, so close that gate again before the stopped actor can
                // enqueue its final TransportClosed event.
                self.suppress_transport_recovery_for_shutdown();
                self.save_session();
                RemoteResponse::ok("stopping daemon".to_string())
            }
            RemoteCommand::Next => {
                let outgoing = self.prepare_outgoing(false);
                let response = self.next_track().await;
                if (response.ok || response.reason.as_deref() == Some("queue_end"))
                    && let Some(outgoing) = outgoing
                {
                    self.commit_outgoing(outgoing);
                }
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::Prev => self.prev_track().await,
            RemoteCommand::TogglePause => {
                let response = self.toggle_pause().await;
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::Play { query } => {
                let response = self.search_and_play(query).await;
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::Enqueue { query } => {
                let response = self.search_and_enqueue(query).await;
                effects.extend(self.maybe_autoplay_extend());
                response
            }
            RemoteCommand::VolumeUp => self.adjust_volume(VOLUME_STEP),
            RemoteCommand::VolumeDown => self.adjust_volume(-VOLUME_STEP),
            RemoteCommand::SetVolume { percent } => self.set_volume(percent),
            RemoteCommand::Sleep { minutes } => match minutes {
                Some(0) => self.cancel_sleep(),
                Some(minutes) => self.arm_sleep(minutes),
                None => {
                    let preset = self.config.sleep_timer.effective_default_minutes();
                    self.arm_sleep(preset)
                }
            },
            RemoteCommand::SeekBack => self.seek(-self.config.effective_seek_seconds()),
            RemoteCommand::SeekForward => self.seek(self.config.effective_seek_seconds()),
            RemoteCommand::SeekTo { ms } => self.seek_to(ms as f64 / 1000.0),
            RemoteCommand::ToggleShuffle => {
                self.queue.toggle_shuffle();
                self.config.shuffle = Some(self.queue.shuffle);
                self.save_config("daemon shuffle setting");
                self.save_session();
                RemoteResponse::status(self.status())
            }
            RemoteCommand::CycleRepeat => {
                let transition = PlaybackModeState::new(self.queue.repeat, self.streaming)
                    .transition(PlaybackModeAction::CycleRepeat);
                match transition {
                    Ok(transition) => {
                        self.queue.repeat = transition.state.repeat;
                        self.config.repeat = self.queue.repeat;
                        self.save_config("daemon repeat setting");
                        self.save_session();
                        RemoteResponse::status(self.status())
                    }
                    Err(_) => RemoteResponse::err("incompatible_playback_modes"),
                }
            }
            RemoteCommand::QueuePlay { position }
            | RemoteCommand::QueuePlayIfRevision { position, .. } => {
                let response = self.queue_play(position).await;
                if response.ok {
                    effects.extend(self.maybe_autoplay_extend());
                }
                response
            }
            RemoteCommand::QueueRemove { position }
            | RemoteCommand::QueueRemoveIfRevision { position, .. } => {
                let response = self.queue_remove(position).await;
                if response.ok {
                    effects.extend(self.maybe_autoplay_extend());
                }
                response
            }
            RemoteCommand::Streaming { state } => {
                let (response, streaming_effects) = self.set_streaming(state);
                effects.extend(streaming_effects);
                response
            }
            RemoteCommand::SetSetting { change } => {
                let (response, setting_effects) = self.set_setting(change);
                effects.extend(setting_effects);
                response
            }
            RemoteCommand::ResumeSession => {
                let response = self.resume_session().await;
                if response.ok {
                    effects.extend(self.force_autoplay_extend());
                }
                response
            }
            // Queue order surgery never touches the current track or playback position
            // (no position_epoch interaction); the shared Queue methods keep both
            // owners byte-identical for the parity harness.
            RemoteCommand::QueueMove { from, to, .. } => {
                if self.queue.move_item(from, to).is_none() {
                    RemoteResponse::err("queue_index")
                } else {
                    self.save_session();
                    RemoteResponse::status(self.status())
                }
            }
            RemoteCommand::QueueClearUpcoming { .. } => {
                if self.queue.clear_upcoming() > 0 {
                    self.save_session();
                }
                RemoteResponse::status(self.status())
            }
        };
        self.finish_remote_persistence(response, shutdown, effects)
    }
}
