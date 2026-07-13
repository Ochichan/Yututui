//! Admission-atomic recovery paths for the active audio player.
//!
//! Playback failures and live-radio re-syncs are especially sensitive to optimistic reducer
//! updates: consuming a one-shot retry or recording a failed seek before the player lane accepts
//! the matching command makes the next user action lie about what mpv actually received.  The
//! plans in this module keep those projections behind the same [`PlayerIntent`] boundary as
//! ordinary transport controls.

use super::*;

#[derive(Clone)]
pub struct SourceRecoveryPlan {
    expected_queue_rev: u64,
    expected_cursor: usize,
    expected_video_id: String,
    expected_loaded_video_id: Option<String>,
    logical_generation: u64,
    origin_file_generation: u64,
    episode_id: crate::player::recovery::RecoveryEpisodeId,
    transport_epoch: crate::player::recovery::TransportIntentEpoch,
    position_secs: f64,
    paused: bool,
}

#[derive(Clone)]
pub struct PrefetchWatchRetryPlan {
    expected_queue_rev: u64,
    expected_cursor: usize,
    expected_video_id: String,
    expected_loaded_video_id: Option<String>,
}

#[derive(Clone)]
pub struct RadioLiveSeekPlan {
    expected_queue_rev: u64,
    expected_cursor: usize,
    expected_video_id: String,
    expected_paused: bool,
    expected_video_paused_audio: bool,
    target: f64,
    attempted_at: Instant,
}

impl App {
    /// Advance the logical-item identity after an ordinary admitted load. A source-recovery
    /// replacement deliberately does not call this: its new file generation belongs to the
    /// same one-shot episode.
    pub(in crate::app) fn begin_source_logical_item(&mut self) {
        self.prefetch.source_logical_generation =
            self.prefetch.source_logical_generation.wrapping_add(1);
        self.prefetch.source_file_generation = self.prefetch.source_file_generation.wrapping_add(1);
        self.prefetch
            .source_recovery
            .begin_logical_item(self.prefetch.source_logical_generation);
    }

    /// Invalidate owner-side recovery correlation after a newer admitted user transport intent.
    pub(in crate::app) fn supersede_source_recovery(&mut self) {
        self.prefetch.source_recovery.supersede_transport();
    }

    /// Prepare one same-item, position-preserving reload for a conservatively classified
    /// mid-track source failure. Initial-load failures have no confirmed playhead and retain the
    /// existing retry/heal/skip behavior.
    pub(in crate::app) fn source_recovery_intent(&mut self, error: &str) -> Option<Vec<Cmd>> {
        let failure = crate::player::recovery::classify_source_failure(error)?;
        let position_secs = self
            .playback
            .time_pos
            .filter(|position| position.is_finite() && *position > 0.0)?;
        let song = self.queue.current()?;
        if song.is_radio_station()
            || self.prefetch.loaded_video_id.as_deref() != Some(song.video_id.as_str())
        {
            return None;
        }
        let watch_url = song.prefetch_target()?;
        let expected_video_id = song.video_id.clone();
        let logical_generation = self.prefetch.source_logical_generation;
        let origin_file_generation = self.prefetch.source_file_generation;
        let (episode_id, transport_epoch) = self.prefetch.source_recovery.begin_episode(
            error,
            logical_generation,
            origin_file_generation,
        )?;
        crate::player::diagnostics::source_recovery_attempt(failure.id());
        let plan = SourceRecoveryPlan {
            expected_queue_rev: self.queue.rev(),
            expected_cursor: self.queue.cursor_pos(),
            expected_video_id,
            expected_loaded_video_id: self.prefetch.loaded_video_id.clone(),
            logical_generation,
            origin_file_generation,
            episode_id,
            transport_epoch,
            position_secs,
            paused: self.playback.paused,
        };
        let request = crate::player::recovery::LoadWithResume {
            url: watch_url,
            position_secs,
            paused: plan.paused,
            source_context: crate::player::MediaSourceContext::OnDemand,
            episode_id,
            transport_epoch,
            force_ram_only: false,
        };
        Some(self.player_intent(
            "source_recovery",
            PlayerCmd::LoadWithResume(request),
            PlayerCommit::SourceRecovery(Box::new(plan)),
        ))
    }

    pub(in crate::app) fn source_recovery_is_current(&self, plan: &SourceRecoveryPlan) -> bool {
        self.queue.planned_transition_matches(
            plan.expected_queue_rev,
            plan.expected_cursor,
            Some(&plan.expected_video_id),
            Some(plan.expected_cursor),
        ) && self.prefetch.loaded_video_id == plan.expected_loaded_video_id
            && self.prefetch.source_logical_generation == plan.logical_generation
            && self.prefetch.source_file_generation == plan.origin_file_generation
            && self.prefetch.source_recovery.accepts_resolved_source(
                plan.episode_id,
                plan.logical_generation,
                plan.origin_file_generation,
                plan.transport_epoch,
            )
    }

    pub(in crate::app) fn commit_source_recovery(&mut self, plan: SourceRecoveryPlan) -> Vec<Cmd> {
        self.queue.validate_planned_transition(
            plan.expected_queue_rev,
            plan.expected_cursor,
            Some(&plan.expected_video_id),
            Some(plan.expected_cursor),
        );
        assert_eq!(
            self.prefetch.loaded_video_id, plan.expected_loaded_video_id,
            "loaded track changed before source-recovery commit"
        );
        assert!(
            self.prefetch.source_recovery.accepts_resolved_source(
                plan.episode_id,
                plan.logical_generation,
                plan.origin_file_generation,
                plan.transport_epoch,
            ),
            "source-recovery correlation changed before commit"
        );
        assert!(
            self.prefetch
                .source_recovery
                .finish_episode(plan.episode_id)
        );
        crate::player::diagnostics::source_recovery_outcome(
            crate::player::diagnostics::SourceRecoveryOutcome::AdmissionAccepted,
        );
        self.prefetch.source_file_generation = self.prefetch.source_file_generation.wrapping_add(1);

        // The retry is the same logical play: keep queue/history/duration intact and publish the
        // saved position directly, with one central epoch bump and no transient zero projection.
        self.playback.time_pos = Some(plan.position_secs);
        self.playback.time_pos_at = Some(Instant::now());
        self.playback.paused = plan.paused;
        self.bump_position_epoch(PositionEpochReason::SourceRecovery);
        self.playback.cache_time = None;
        self.playback.cache_time_at = None;

        if self.prefetch.last_load_prefetched {
            self.prefetch.record_direct_url_failure();
        }
        self.prefetch.resolved.remove(&plan.expected_video_id);
        self.prefetch
            .watch_retry_attempted
            .insert(plan.expected_video_id.clone());
        self.prefetch.last_load_prefetched = false;
        self.status.kind = StatusKind::Info;
        self.status.text = t!(
            "Stream changed; resuming from the last position",
            "스트림이 변경되어 마지막 위치에서 다시 재생합니다"
        )
        .to_owned();
        self.dirty = true;
        Vec::new()
    }

    pub(crate) fn reject_source_recovery(&mut self, plan: &SourceRecoveryPlan) {
        self.prefetch
            .source_recovery
            .cancel_unadmitted_episode(plan.episode_id);
        crate::player::diagnostics::source_recovery_outcome(
            crate::player::diagnostics::SourceRecoveryOutcome::AdmissionRejected,
        );
    }

    /// Retry the current track through its watch URL without consuming the one-shot fallback
    /// until the replacement `Load` has entered the player lane.
    pub(in crate::app) fn prefetch_watch_retry_intent(
        &self,
        video_id: String,
        watch_url: String,
    ) -> Vec<Cmd> {
        let plan = PrefetchWatchRetryPlan {
            expected_queue_rev: self.queue.rev(),
            expected_cursor: self.queue.cursor_pos(),
            expected_video_id: video_id,
            expected_loaded_video_id: self.prefetch.loaded_video_id.clone(),
        };
        self.player_intent(
            "prefetch_watch_retry",
            PlayerCmd::load(
                watch_url,
                crate::player::MediaSourceContext::from_live(
                    self.queue.current().is_some_and(Song::is_radio_station),
                ),
            ),
            PlayerCommit::PrefetchWatchRetry(Box::new(plan)),
        )
    }

    pub(in crate::app) fn commit_prefetch_watch_retry(
        &mut self,
        plan: PrefetchWatchRetryPlan,
    ) -> Vec<Cmd> {
        self.queue.validate_planned_transition(
            plan.expected_queue_rev,
            plan.expected_cursor,
            Some(&plan.expected_video_id),
            Some(plan.expected_cursor),
        );
        assert_eq!(
            self.prefetch.loaded_video_id, plan.expected_loaded_video_id,
            "loaded track changed before prefetch retry commit"
        );
        assert!(
            self.prefetch.last_load_prefetched,
            "prefetch retry committed after its failed-load marker was cleared"
        );

        let prefetch_paused = self.prefetch.record_direct_url_failure();
        self.prefetch.resolved.remove(&plan.expected_video_id);
        self.prefetch
            .watch_retry_attempted
            .insert(plan.expected_video_id.clone());
        self.prefetch.last_load_prefetched = false;
        self.begin_source_logical_item();
        // `Load` restarts the current file at position zero even though queue membership and
        // catalog bookkeeping stay unchanged. Project that discontinuity through the central
        // reset path only after admission.
        self.reset_progress();
        self.status.kind = StatusKind::Info;
        self.status.text = if prefetch_paused {
            t!(
                "Prefetched streams are being rejected — pausing prefetch and retrying this track",
                "미리 받은 스트림이 반복 거부됨 — 프리페치를 쉬고 같은 곡을 다시 시도"
            )
            .to_owned()
        } else {
            t!(
                "Prefetched stream was rejected — retrying this track",
                "미리 받은 스트림이 거부됨 — 같은 곡을 다시 시도"
            )
            .to_owned()
        };
        tracing::info!(
            video_id = %plan.expected_video_id,
            prefetch_paused,
            "retrying failed prefetched stream via watch URL"
        );
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn prefetch_watch_retry_is_current(
        &self,
        plan: &PrefetchWatchRetryPlan,
    ) -> bool {
        self.queue.planned_transition_matches(
            plan.expected_queue_rev,
            plan.expected_cursor,
            Some(&plan.expected_video_id),
            Some(plan.expected_cursor),
        ) && self.prefetch.loaded_video_id == plan.expected_loaded_video_id
            && self.prefetch.last_load_prefetched
    }

    /// Resume and seek to the known live edge as one ordered batch. The attempt timestamp is
    /// committed with the seek, so a rejected first press cannot make the next press falsely
    /// escalate to a reconnect.
    pub(in crate::app) fn radio_live_seek_intent(&self, target: f64) -> Vec<Cmd> {
        let Some(song) = self.queue.current() else {
            return Vec::new();
        };
        let plan = RadioLiveSeekPlan {
            expected_queue_rev: self.queue.rev(),
            expected_cursor: self.queue.cursor_pos(),
            expected_video_id: song.video_id.clone(),
            expected_paused: self.playback.paused,
            expected_video_paused_audio: self.video.paused_audio,
            target,
            attempted_at: Instant::now(),
        };
        vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
            PlayerIntent::batch(
                "radio_live_seek",
                vec![
                    PlayerCmd::SetProperty {
                        name: "pause".to_owned(),
                        value: serde_json::Value::Bool(false),
                    },
                    PlayerCmd::exact_seek(target),
                ],
                PlayerCommit::RadioLiveSeek(Box::new(plan)),
            ),
        )))]
    }

    pub(in crate::app) fn commit_radio_live_seek(&mut self, plan: RadioLiveSeekPlan) -> Vec<Cmd> {
        debug_assert!(plan.target.is_finite() && plan.target >= 0.0);
        self.queue.validate_planned_transition(
            plan.expected_queue_rev,
            plan.expected_cursor,
            Some(&plan.expected_video_id),
            Some(plan.expected_cursor),
        );
        assert_eq!(
            self.playback.paused, plan.expected_paused,
            "pause state changed before live-seek commit"
        );
        assert_eq!(
            self.video.paused_audio, plan.expected_video_paused_audio,
            "video pause ownership changed before live-seek commit"
        );

        self.playback.paused = false;
        self.video.paused_audio = false;
        // Admission is not an mpv acknowledgement. Keep the last reported playhead so a second
        // press can detect an unseekable live cache and escalate to reconnect; the central
        // admitted-seek path still bumps the position epoch.
        self.radio_resync_at = Some(plan.attempted_at);
        self.status.kind = StatusKind::Info;
        self.status.text = t!("Re-synced to live", "실시간으로 다시 맞췄어요").to_owned();
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn radio_live_seek_is_current(&self, plan: &RadioLiveSeekPlan) -> bool {
        self.queue.planned_transition_matches(
            plan.expected_queue_rev,
            plan.expected_cursor,
            Some(&plan.expected_video_id),
            Some(plan.expected_cursor),
        ) && self.playback.paused == plan.expected_paused
            && self.video.paused_audio == plan.expected_video_paused_audio
    }

    /// A reconnect is an ordinary reload of the current track. Reuse the track transaction so
    /// recorder teardown, `Load`, AF, progress reset, and the live-status toast share one
    /// admission result.
    pub(in crate::app) fn reconnect_radio_to_live(&mut self) -> Vec<Cmd> {
        let mut cmds = self.stay_on_current_track();
        Self::attach_track_commit_status(
            &mut cmds,
            StatusKind::Info,
            t!(
                "Reconnected to the live stream",
                "라이브 스트림에 다시 연결했어요"
            )
            .to_owned(),
        );
        cmds
    }
}
