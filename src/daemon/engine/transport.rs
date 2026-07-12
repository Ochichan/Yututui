use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TransportRecovery {
    pub(super) video_id: String,
    pub(super) paused: bool,
    pub(super) generation: u64,
    pub(super) attempts: u8,
}

const TRANSPORT_RECOVERY_MAX_ATTEMPTS: u8 = 2;
const TRANSPORT_RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(75);

impl DaemonEngine {
    /// Permanently disarm transport replacement for process teardown and retire any live player.
    /// External signal handling calls this from the owner loop after the out-of-band latch wins,
    /// so a TransportClosed already waiting in the bounded event lane cannot recreate mpv.
    pub(crate) fn suppress_transport_recovery_for_shutdown(&mut self) {
        self.transport_recovery = None;
        self.transport_auto_recovery_armed = false;
        if let Some(player) = self.player.take() {
            let PlayerRuntime { handle, _guard } = player;
            drop(handle);
            drop(_guard);
        }
    }

    /// Retire the sole active player after its actor's final event and arm one bounded
    /// automatic restart. A normal user-driven/track-transition load must commit before
    /// another automatic restart is allowed; late telemetry cannot accidentally rearm it.
    pub(super) fn handle_transport_closed(&mut self, reason: String) -> Option<u64> {
        let Some(player) = self.player.take() else {
            tracing::debug!("ignored duplicate player transport terminal event");
            return None;
        };

        // Close actor command senders before killing the guard. This makes teardown
        // intentional from the actor's perspective even when mpv has not exited yet.
        let PlayerRuntime { handle, _guard } = player;
        drop(handle);
        drop(_guard);

        let reason = sanitize::sanitize_error_text(reason);
        tracing::warn!(%reason, "daemon player transport closed");
        let paused = self.playback.paused;
        let loaded_video_id = self.loaded_video_id.take();
        self.transport_recovery_generation = self.transport_recovery_generation.wrapping_add(1);
        let generation = self.transport_recovery_generation;
        let should_recover = loaded_video_id.is_some() && self.transport_auto_recovery_armed;
        self.transport_recovery = if should_recover {
            self.transport_auto_recovery_armed = false;
            loaded_video_id.map(|video_id| TransportRecovery {
                video_id,
                paused,
                generation,
                attempts: 0,
            })
        } else {
            if loaded_video_id.is_some() && !self.transport_auto_recovery_armed {
                tracing::error!(
                    generation,
                    "suppressed repeated daemon player restart before playback became stable"
                );
            }
            None
        };
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.playback.duration = None;
        self.bump_position_epoch(PositionEpochReason::TransportRecovery);
        // Pause is user intent, not a report owned by the dead transport.
        self.playback.paused = paused;
        self.last_error = Some(format!("mpv transport closed: {reason}"));
        should_recover.then_some(generation)
    }

    /// Try to recreate the transport and replay the current track. The initial attempt runs
    /// immediately; at most one later attempt is scheduled through the daemon owner lane.
    pub(crate) async fn attempt_transport_recovery(
        &mut self,
        generation: u64,
    ) -> Vec<EngineEffect> {
        let attempt = match self.transport_recovery.as_mut() {
            Some(recovery)
                if recovery.generation == generation
                    && recovery.attempts < TRANSPORT_RECOVERY_MAX_ATTEMPTS =>
            {
                recovery.attempts += 1;
                recovery.attempts
            }
            _ => return Vec::new(),
        };

        let result = match self.ensure_player().await {
            Ok(()) => self.load_current_loaded(),
            Err(error) => Err(error),
        };
        if result.is_ok() {
            tracing::info!(generation, attempt, "daemon player transport recovered");
            return Vec::new();
        }

        let error = result.expect_err("failed recovery result was checked");
        let detail = sanitize::sanitize_error_text(error.to_string());
        self.last_error = Some(format!("mpv transport recovery failed: {detail}"));
        let retry = self.transport_recovery.as_ref().is_some_and(|recovery| {
            recovery.generation == generation && recovery.attempts < TRANSPORT_RECOVERY_MAX_ATTEMPTS
        });
        if retry {
            tracing::warn!(
                generation,
                attempt,
                retry_ms = TRANSPORT_RECOVERY_RETRY_DELAY.as_millis(),
                %detail,
                "daemon player transport recovery will retry"
            );
            vec![EngineEffect::TransportRecoveryRetry {
                generation,
                retry_after: TRANSPORT_RECOVERY_RETRY_DELAY,
            }]
        } else {
            tracing::error!(
                generation,
                attempt,
                %detail,
                "daemon player transport recovery exhausted"
            );
            Vec::new()
        }
    }

    /// Load the queue cursor into an already-created player. A same-track transport
    /// recovery restores pause after `loadfile` and deliberately skips history/signals.
    pub(super) fn load_current_loaded(&mut self) -> Result<(), EngineError> {
        let Some(song) = self.queue.current().cloned() else {
            self.stop_playback();
            return Ok(());
        };
        let target = match song.playback_target_checked() {
            Ok(target) => target,
            Err(error) => {
                tracing::warn!(
                    video_id = %song.video_id,
                    title = %song.title,
                    artist = %song.artist,
                    %error,
                    "refusing to load track with invalid playback URL"
                );
                self.last_error = Some(format!("invalid playback URL: {error}"));
                self.stop_playback();
                return Ok(());
            }
        };

        let recovery_paused = self
            .transport_recovery
            .as_ref()
            .filter(|recovery| recovery.video_id == song.video_id)
            .map(|recovery| recovery.paused);
        let mut commands = vec![PlayerCmd::Load(target)];
        if recovery_paused == Some(true) {
            commands.push(PlayerCmd::CyclePause);
        }
        self.send_active_player_batch("load_current", commands)?;

        self.playback.paused = recovery_paused.unwrap_or(false);
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.bump_position_epoch(PositionEpochReason::TrackRestart);
        self.playback.duration = None;
        self.loaded_video_id = Some(song.video_id.clone());
        self.transport_recovery = None;

        if recovery_paused.is_none() {
            self.transport_auto_recovery_armed = true;
            self.library.record_play(&song);
            self.save_library("daemon library history");
        } else {
            self.last_error = None;
        }
        self.save_session();
        Ok(())
    }
}
