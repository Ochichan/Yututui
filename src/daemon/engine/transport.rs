use super::*;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct TransportRecovery {
    pub(super) video_id: String,
    pub(super) paused: bool,
    pub(super) position_secs: Option<f64>,
    pub(super) force_ram_only: bool,
    pub(super) generation: u64,
    pub(super) attempts: u8,
}

/// Owner-loop lifecycle for automatic player replacement.
///
/// The former `Option<TransportRecovery>` plus `transport_auto_recovery_armed` boolean allowed
/// tests and future call sites to construct contradictory combinations.  Keeping the payload in
/// the state which owns it makes every gate transition exhaustive and keeps shutdown absorbing.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum TransportRecoveryState {
    Armed,
    Recovering(TransportRecovery),
    Disarmed,
    Shutdown,
}

impl TransportRecoveryState {
    /// Consume the one-shot arm when a loaded item exists. A close from a replacement lifetime
    /// retires any pending payload and remains disarmed; shutdown is permanently absorbing.
    fn begin(&mut self, recovery: Option<TransportRecovery>) -> bool {
        let previous = std::mem::replace(self, Self::Disarmed);
        let (next, started) = match previous {
            Self::Armed => match recovery {
                Some(recovery) => (Self::Recovering(recovery), true),
                None => (Self::Armed, false),
            },
            Self::Recovering(_) => (Self::Disarmed, false),
            Self::Disarmed => (Self::Disarmed, false),
            Self::Shutdown => (Self::Shutdown, false),
        };
        *self = next;
        started
    }

    fn finish_recovery(&mut self) {
        *self = match self {
            Self::Armed => Self::Armed,
            Self::Recovering(_) => Self::Disarmed,
            Self::Disarmed => Self::Disarmed,
            Self::Shutdown => Self::Shutdown,
        };
    }

    pub(super) fn rearm_after_normal_load_or_stop(&mut self) {
        *self = match self {
            Self::Armed | Self::Recovering(_) | Self::Disarmed => Self::Armed,
            Self::Shutdown => Self::Shutdown,
        };
    }

    fn suppress_for_shutdown(&mut self) {
        *self = Self::Shutdown;
    }
}

const TRANSPORT_RECOVERY_MAX_ATTEMPTS: u8 = 2;
const TRANSPORT_RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(75);

#[derive(Clone, Copy)]
enum LoadCurrentIntent {
    Ordinary,
    TransportRecovery,
}

impl DaemonEngine {
    fn begin_source_logical_item(&mut self) {
        self.source_logical_generation = self.source_logical_generation.wrapping_add(1);
        self.source_file_generation = self.source_file_generation.wrapping_add(1);
        self.source_recovery
            .begin_logical_item(self.source_logical_generation);
    }

    /// Admit one same-item source replacement without logging either the failed or replacement
    /// URL. The player actor owns the correlated `file-loaded` boundary and deferred exact seek.
    pub(super) fn try_source_recovery(&mut self, error: &str) -> bool {
        let Some(failure) = crate::player::recovery::classify_source_failure(error) else {
            return false;
        };
        let Some(position_secs) = self
            .playback
            .time_pos
            .filter(|position| position.is_finite() && *position > 0.0)
        else {
            return false;
        };
        let Some(song) = self.queue.current() else {
            return false;
        };
        if song.is_radio_station()
            || self.loaded_video_id.as_deref() != Some(song.video_id.as_str())
        {
            return false;
        }
        let Some(url) = song.prefetch_target() else {
            return false;
        };
        let logical_generation = self.source_logical_generation;
        let origin_file_generation = self.source_file_generation;
        let Some(ticket) =
            self.source_recovery
                .begin_ticket(error, logical_generation, origin_file_generation)
        else {
            return false;
        };
        crate::player::diagnostics::source_recovery_attempt(failure.id());
        let request = crate::player::recovery::LoadWithResume::source_recovery(
            url,
            position_secs,
            self.playback.paused,
            crate::player::MediaSourceContext::OnDemand,
            ticket,
        );
        if let Err(delivery) =
            self.send_active_player_command("source_recovery", PlayerCmd::LoadWithResume(request))
        {
            self.source_recovery.cancel_unadmitted_ticket(ticket);
            crate::player::diagnostics::source_recovery_outcome(
                crate::player::diagnostics::SourceRecoveryOutcome::AdmissionRejected,
            );
            self.last_error = Some(delivery.to_string());
            return false;
        }
        assert!(self.source_recovery.accepts_ticket(
            ticket,
            logical_generation,
            origin_file_generation,
        ));
        assert!(self.source_recovery.finish_ticket(ticket));
        crate::player::diagnostics::source_recovery_outcome(
            crate::player::diagnostics::SourceRecoveryOutcome::AdmissionAccepted,
        );
        self.source_file_generation = self.source_file_generation.wrapping_add(1);
        self.playback.time_pos = Some(position_secs);
        self.playback.time_pos_at = Some(Instant::now());
        self.bump_position_epoch(PositionEpochReason::SourceRecovery);
        self.last_error = None;
        true
    }

    /// Permanently disarm transport replacement for process teardown and retire any live player.
    /// External signal handling calls this from the owner loop after the out-of-band latch wins,
    /// so a TransportClosed already waiting in the bounded event lane cannot recreate mpv.
    pub(crate) fn suppress_transport_recovery_for_shutdown(&mut self) {
        self.transport_recovery.suppress_for_shutdown();
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
        self.source_recovery.supersede_transport();
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
        let had_loaded_video = loaded_video_id.is_some();
        let should_recover = self
            .transport_recovery
            .begin(loaded_video_id.map(|video_id| TransportRecovery {
                video_id,
                paused,
                position_secs: None,
                force_ram_only: false,
                generation,
                attempts: 0,
            }));
        if had_loaded_video && !should_recover {
            tracing::error!(
                generation,
                "suppressed repeated daemon player restart before playback became stable"
            );
        }
        self.playback.time_pos = None;
        self.playback.time_pos_at = None;
        self.playback.duration = None;
        self.bump_position_epoch(PositionEpochReason::TransportRecovery);
        // Pause is user intent, not a report owned by the dead transport.
        self.playback.paused = paused;
        self.last_error = Some(format!("mpv transport closed: {reason}"));
        should_recover.then_some(generation)
    }

    /// Retire a cache-unsafe actor and arm a single same-item, exact RAM-only resume. Unlike a
    /// generic transport loss, the captured position remains visible and the sole epoch bump is
    /// committed when the unsafe transport is frozen, even if replacement startup later fails.
    pub(super) fn handle_cache_emergency(
        &mut self,
        position_secs: f64,
        paused: bool,
        reason: crate::player::long_form_seek::CacheReason,
    ) -> Option<u64> {
        self.handle_cache_safety_recycle(Some(position_secs), paused, reason)
    }

    /// Recover the destination already committed by the daemon owner. The old actor's position
    /// is intentionally unavailable at this boundary, preventing cross-item resume corruption.
    pub(super) fn handle_cache_replacement_emergency(
        &mut self,
        reason: crate::player::long_form_seek::CacheReason,
    ) -> Option<u64> {
        self.handle_cache_safety_recycle(self.playback.time_pos, self.playback.paused, reason)
    }

    fn handle_cache_safety_recycle(
        &mut self,
        position_secs: Option<f64>,
        paused: bool,
        reason: crate::player::long_form_seek::CacheReason,
    ) -> Option<u64> {
        self.source_recovery.supersede_transport();
        let Some(player) = self.player.take() else {
            tracing::debug!("ignored duplicate cache emergency terminal event");
            return None;
        };
        let PlayerRuntime { handle, _guard } = player;
        drop(handle);
        drop(_guard);

        let position_secs = position_secs.map(crate::playback_policy::norm_position);
        let loaded_video_id = self.loaded_video_id.take();
        self.transport_recovery_generation = self.transport_recovery_generation.wrapping_add(1);
        let generation = self.transport_recovery_generation;
        let should_recover = self
            .transport_recovery
            .begin(loaded_video_id.map(|video_id| TransportRecovery {
                video_id,
                paused,
                position_secs,
                force_ram_only: true,
                generation,
                attempts: 0,
            }));
        self.playback.time_pos = position_secs;
        self.playback.time_pos_at = None;
        self.bump_position_epoch(PositionEpochReason::TransportRecovery);
        self.playback.duration = None;
        self.playback.paused = paused;
        self.last_error = Some(format!(
            "managed cache safety recycle required: {}",
            reason.id()
        ));
        should_recover.then_some(generation)
    }

    /// Try to recreate the transport and replay the current track. The initial attempt runs
    /// immediately; at most one later attempt is scheduled through the daemon owner lane.
    pub(crate) async fn attempt_transport_recovery(
        &mut self,
        generation: u64,
    ) -> Vec<EngineEffect> {
        let attempt = match &mut self.transport_recovery {
            TransportRecoveryState::Recovering(recovery)
                if recovery.generation == generation
                    && recovery.attempts < TRANSPORT_RECOVERY_MAX_ATTEMPTS =>
            {
                recovery.attempts += 1;
                recovery.attempts
            }
            TransportRecoveryState::Recovering(_)
            | TransportRecoveryState::Armed
            | TransportRecoveryState::Disarmed
            | TransportRecoveryState::Shutdown => return Vec::new(),
        };

        let result = match self.ensure_player().await {
            Ok(()) => self.load_current_loaded_for(LoadCurrentIntent::TransportRecovery),
            Err(error) => Err(error),
        };
        if result.is_ok() {
            tracing::info!(generation, attempt, "daemon player transport recovered");
            return Vec::new();
        }

        let error = result.expect_err("failed recovery result was checked");
        let detail = sanitize::sanitize_error_text(error.to_string());
        self.last_error = Some(format!("mpv transport recovery failed: {detail}"));
        let retry = match &self.transport_recovery {
            TransportRecoveryState::Recovering(recovery) => {
                recovery.generation == generation
                    && recovery.attempts < TRANSPORT_RECOVERY_MAX_ATTEMPTS
            }
            TransportRecoveryState::Armed
            | TransportRecoveryState::Disarmed
            | TransportRecoveryState::Shutdown => false,
        };
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

    /// Admit an ordinary queue load into an already-created player and rearm transport recovery.
    /// Automatic replacement uses the typed recovery intent below so it cannot replay a
    /// different queue item or duplicate history/signals.
    pub(super) fn load_current_loaded(&mut self) -> Result<(), EngineError> {
        self.load_current_loaded_for(LoadCurrentIntent::Ordinary)
    }

    fn load_current_loaded_for(&mut self, intent: LoadCurrentIntent) -> Result<(), EngineError> {
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

        let recovery = match (&self.transport_recovery, intent) {
            (
                TransportRecoveryState::Recovering(recovery),
                LoadCurrentIntent::TransportRecovery,
            ) => Some(recovery.clone()),
            (
                TransportRecoveryState::Armed
                | TransportRecoveryState::Disarmed
                | TransportRecoveryState::Shutdown,
                LoadCurrentIntent::TransportRecovery,
            )
            | (_, LoadCurrentIntent::Ordinary) => None,
        };
        let recovery_paused = recovery.as_ref().map(|recovery| recovery.paused);
        let recovery_position = recovery
            .as_ref()
            .and_then(|recovery| recovery.position_secs);
        let planned_loaded_video_id = match intent {
            LoadCurrentIntent::TransportRecovery => {
                recovery.as_ref().map(|recovery| recovery.video_id.as_str())
            }
            LoadCurrentIntent::Ordinary => match &self.transport_recovery {
                TransportRecoveryState::Shutdown => None,
                TransportRecoveryState::Armed
                | TransportRecoveryState::Recovering(_)
                | TransportRecoveryState::Disarmed => Some(song.video_id.as_str()),
            },
        };
        let source_context = crate::player::MediaSourceContext::from_live(song.is_radio_station());
        let restore = if recovery
            .as_ref()
            .is_some_and(|recovery| recovery.force_ram_only)
        {
            crate::player::recovery::TransportRestorePlan::resume_ram_only_if_loaded(
                planned_loaded_video_id,
                &song.video_id,
                target,
                recovery_position.unwrap_or(0.0),
                recovery_paused.unwrap_or(true),
                source_context,
            )
        } else {
            crate::player::recovery::TransportRestorePlan::reload_if_loaded(
                planned_loaded_video_id,
                &song.video_id,
                target,
                recovery_paused.unwrap_or(false),
                source_context,
            )
        };
        self.send_active_player_batch("load_current", restore.into_commands(None))?;

        self.playback.paused = recovery_paused.unwrap_or(false);
        self.playback.time_pos = recovery_position;
        self.playback.time_pos_at = None;
        if matches!(intent, LoadCurrentIntent::Ordinary) {
            self.bump_position_epoch(PositionEpochReason::TrackRestart);
        }
        self.playback.duration = None;
        self.loaded_video_id = Some(song.video_id.clone());

        match intent {
            LoadCurrentIntent::Ordinary => {
                self.transport_recovery.rearm_after_normal_load_or_stop();
                self.begin_source_logical_item();
                self.library.record_play(&song);
                self.save_library("daemon library history");
                // History changed: a subscribed GUI's paged library view is stale.
                self.library_invalidations = self.library_invalidations.wrapping_add(1);
            }
            LoadCurrentIntent::TransportRecovery => {
                self.transport_recovery.finish_recovery();
                self.source_file_generation = self.source_file_generation.wrapping_add(1);
                self.source_recovery.supersede_transport();
                self.last_error = None;
            }
        }
        self.save_session();
        Ok(())
    }
}
