//! Radio recorder state machine (a Shortwave-style feature), split out of the monolithic
//! `app.rs`. Pure in-memory transitions driven by the ICY-title diff in `PlayerMsg::Metadata`
//! and the 1 Hz `Msg::RecordingTick`; the actual disk work is emitted as `Cmd::Recorder` jobs
//! (run off the loop) and mpv writes the audio itself via the `stream-record` property.
//!
//! Rotation model: keep a rolling recording so the *next* track is captured from its start.
//! On each real title change we finalize the segment we were writing (the track that just
//! ended, whose title we knew) and open a fresh one for the new title. The very first segment
//! after tuning in was joined mid-song, so it is flagged incomplete and dropped — exactly like
//! Shortwave.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::*;

use crate::i18n::Language;
use crate::player::PlayerCmd;
use crate::recorder::barrier::CommandBarrier;
use crate::recorder::job::RecorderJob;
use crate::recorder::{
    OpenSegment, PlannedOpenSegment, RecordedTrack, RecorderFinalizePlan, RecorderSaveRequest,
    RecorderTransitionPlan, RecordingMode, RecordingState, codec_to_ext, track_filename_base,
};

static NEXT_RECORDER_TRANSITION_ID: AtomicU64 = AtomicU64::new(1);

impl App {
    pub(in crate::app) fn recorder_transition_commands(
        &self,
        plan: &RecorderTransitionPlan,
    ) -> Vec<PlayerCmd> {
        let mut commands = Vec::with_capacity(2);
        if let Some(barrier) = &plan.close_barrier {
            commands.push(PlayerCmd::tracked_property(
                "stream-record".to_owned(),
                serde_json::Value::from(""),
                barrier,
            ));
        }
        if let (Some(open), Some(barrier)) = (&plan.open, &plan.open_barrier) {
            commands.push(PlayerCmd::tracked_property(
                "stream-record".to_owned(),
                serde_json::Value::from(open.temp_path.to_string_lossy().into_owned()),
                barrier,
            ));
        }
        commands
    }

    /// A segment is currently being written (drives the guarded recording tick).
    pub fn recorder_active(&self) -> bool {
        self.recorder.current.is_some()
    }

    /// Recording should be running right now: mpv supports it, a mode is selected, and the
    /// active track is a radio stream.
    fn recorder_enabled(&self) -> bool {
        self.recorder.supported
            && !self.config.recording.mode.is_off()
            && (!matches!(self.config.recording.mode, RecordingMode::Everything)
                || !self.recorder.capacity_blocked)
            && !self.recorder.execution_blocked
            && self.current_is_radio_stream()
    }

    fn current_station_name(&self) -> Option<String> {
        self.queue
            .current()
            .filter(|s| s.is_radio_station())
            .map(|s| s.title.clone())
    }

    /// The ICY title changed (to `new`, or `None` for an ad / station-ID / lost metadata).
    /// Finalize the segment we were writing, then — for a real title — open the next one.
    pub(in crate::app) fn recorder_on_title(&mut self, new: Option<&StreamNowPlaying>) -> Vec<Cmd> {
        if self.recorder.pending_transition.is_some() {
            return Vec::new();
        }
        if !self.recorder_enabled() {
            // Recording was turned off (or we left radio) while a segment was open: stop it.
            return self.recorder_teardown();
        }
        if self.recorder.current.is_none() && new.is_none() {
            return Vec::new();
        }
        let finalize = self
            .recorder
            .current
            .is_some()
            .then(|| self.prepare_recorder_finalize(false, false));
        let incomplete = !self.recorder.saw_first_title;
        let saw_first_title = self.recorder.saw_first_title || new.is_some();
        if new.is_some() && !self.ensure_recorder_owner_for_open() {
            return self.recorder_teardown();
        }
        let plan = self.prepare_recorder_transition(
            finalize,
            new.map(|np| (np, incomplete)),
            saw_first_title,
        );
        self.recorder_transition_intent(plan)
    }

    /// 1 Hz while recording: force-split a track that has run past the max duration. The audio
    /// is kept; a fresh segment reopens for the same title (inheriting incomplete-ness).
    pub(in crate::app) fn recorder_on_tick(&mut self) -> Vec<Cmd> {
        if self.recorder.pending_transition.is_some() {
            return Vec::new();
        }
        let Some(seg) = self.recorder.current.as_ref() else {
            return Vec::new();
        };
        if (seg.started_at.elapsed().as_secs() as u32) < self.config.effective_recording_max() {
            return Vec::new();
        }
        // Snapshot the label + incomplete flag before finalize consumes the segment.
        let np = StreamNowPlaying {
            title: seg.title.clone(),
            artist: seg.artist.clone(),
            raw: seg.raw.clone(),
        };
        let incomplete = seg.incomplete;
        let open = self.recorder_enabled().then_some((&np, incomplete));
        if open.is_some() && !self.ensure_recorder_owner_for_open() {
            return self.recorder_teardown();
        }
        let plan = self.prepare_recorder_transition(
            Some(self.prepare_recorder_finalize(true, false)),
            open,
            self.recorder.saw_first_title,
        );
        self.recorder_transition_intent(plan)
    }

    fn prepare_recorder_transition(
        &self,
        finalize: Option<RecorderFinalizePlan>,
        open: Option<(&StreamNowPlaying, bool)>,
        saw_first_title: bool,
    ) -> RecorderTransitionPlan {
        let expected_temp_seq = self.recorder.temp_seq;
        let open = open.map(|(np, incomplete)| {
            let ext = codec_to_ext(
                self.playback.audio_codec.as_deref(),
                self.playback.file_format.as_deref(),
            );
            let id = expected_temp_seq + 1;
            PlannedOpenSegment {
                id,
                temp_path: self.recorder.temp_path(id, ext),
                title: np.title.clone(),
                artist: np.artist.clone(),
                raw: np.raw.clone(),
                station: self.current_station_name(),
                started_at: Instant::now(),
                incomplete,
                ext,
            }
        });
        let open_barrier = open.as_ref().map(|_| CommandBarrier::pending());
        let close_barrier = finalize.as_ref().map(|_| CommandBarrier::pending());
        RecorderTransitionPlan {
            transition_id: NEXT_RECORDER_TRANSITION_ID.fetch_add(1, Ordering::Relaxed),
            expected_current_id: self.recorder.current.as_ref().map(|segment| segment.id),
            expected_temp_seq,
            expected_saw_first_title: self.recorder.saw_first_title,
            finalize,
            open,
            saw_first_title,
            close_barrier,
            open_barrier,
            transport_fenced: self.recorder.transport_recovery_active,
        }
    }

    fn prepare_recorder_finalize(
        &self,
        reached_max: bool,
        force_incomplete: bool,
    ) -> RecorderFinalizePlan {
        RecorderFinalizePlan {
            reached_max,
            force_incomplete,
            duration_secs: self
                .recorder
                .current
                .as_ref()
                .map(|segment| segment.started_at.elapsed().as_secs() as u32)
                .unwrap_or_default(),
            minimum_duration_secs: self.config.effective_recording_min(),
            automatic_final_dir: matches!(self.config.recording.mode, RecordingMode::Everything)
                .then(|| self.config.effective_recording_dir()),
        }
    }

    fn ensure_recorder_owner_for_open(&mut self) -> bool {
        if let Err(error) = self.recorder.ensure_owner_active() {
            tracing::warn!(%error, "could not activate recorder owner namespace");
            self.status.kind = StatusKind::Error;
            self.status.text = match crate::i18n::current() {
                Language::Korean => format!("녹음 임시 저장소를 준비하지 못했어요: {error}"),
                Language::Japanese => format!("録音の一時保存領域を準備できませんでした: {error}"),
                _ => format!("Could not prepare recording storage: {error}"),
            };
            self.dirty = true;
            return false;
        }
        true
    }

    pub(in crate::app) fn prepare_recorder_teardown(&self) -> RecorderTransitionPlan {
        self.prepare_recorder_transition(
            self.recorder
                .current
                .is_some()
                .then(|| self.prepare_recorder_finalize(false, true)),
            None,
            false,
        )
    }

    fn recorder_transition_intent(&mut self, plan: RecorderTransitionPlan) -> Vec<Cmd> {
        let commands = self.recorder_transition_commands(&plan);
        if commands.is_empty() {
            return self.commit_recorder_transition(plan);
        }
        vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
            PlayerIntent::batch(
                "stream_record",
                commands,
                PlayerCommit::Recorder(Box::new(plan)),
            ),
        )))]
    }

    pub(in crate::app) fn validate_recorder_transition(&self, plan: &RecorderTransitionPlan) {
        assert!(
            self.recorder.pending_transition.is_none(),
            "a recorder transition overtook an unacknowledged transition"
        );
        assert_eq!(
            self.recorder.current.as_ref().map(|segment| segment.id),
            plan.expected_current_id,
            "recorder segment changed before player transition commit"
        );
        assert_eq!(self.recorder.temp_seq, plan.expected_temp_seq);
        assert_eq!(self.recorder.saw_first_title, plan.expected_saw_first_title);
    }

    pub(in crate::app) fn recorder_transition_is_current(
        &self,
        plan: &RecorderTransitionPlan,
    ) -> bool {
        self.recorder.pending_transition.is_none()
            && self.recorder.current.as_ref().map(|segment| segment.id) == plan.expected_current_id
            && self.recorder.temp_seq == plan.expected_temp_seq
            && self.recorder.saw_first_title == plan.expected_saw_first_title
    }

    pub(in crate::app) fn commit_recorder_transition(
        &mut self,
        plan: RecorderTransitionPlan,
    ) -> Vec<Cmd> {
        self.validate_recorder_transition(&plan);
        if plan.close_barrier.is_none() && plan.open_barrier.is_none() {
            self.recorder.saw_first_title = plan.saw_first_title;
            self.dirty = true;
            return Vec::new();
        }
        let wait = RecorderJob::AwaitTransition {
            transition_id: plan.transition_id,
            close: plan.close_barrier.clone(),
            open: plan.open_barrier.clone(),
        };
        self.recorder.pending_transition = Some(plan);
        self.dirty = true;
        vec![Cmd::Recorder(wait)]
    }

    fn resolve_recorder_transition(
        &mut self,
        transition_id: u64,
        close: Option<Result<(), String>>,
        open: Option<Result<(), String>>,
    ) -> Vec<Cmd> {
        let Some(plan) = self.recorder.pending_transition.take() else {
            tracing::warn!(
                transition_id,
                "ignored recorder outcome with no pending transition"
            );
            return Vec::new();
        };
        if plan.transition_id != transition_id {
            tracing::warn!(
                transition_id,
                expected = plan.transition_id,
                "ignored stale recorder outcome"
            );
            self.recorder.pending_transition = Some(plan);
            return Vec::new();
        }
        assert_eq!(
            self.recorder.current.as_ref().map(|segment| segment.id),
            plan.expected_current_id,
            "recorder segment changed before mpv outcome"
        );
        assert_eq!(self.recorder.temp_seq, plan.expected_temp_seq);
        assert_eq!(self.recorder.saw_first_title, plan.expected_saw_first_title);
        let close_ok = close.as_ref().is_some_and(Result::is_ok);
        let open_ok = open.as_ref().is_some_and(Result::is_ok);
        let had_finalize = plan.finalize.is_some();
        let had_open = plan.open.is_some();
        let transport_fenced = plan.transport_fenced;
        let closure_proven = had_finalize && (close_ok || open_ok || transport_fenced);
        let command_failed = (had_finalize && !closure_proven) || (had_open && !open_ok);
        let retirement_barrier = if command_failed && !transport_fenced {
            let barrier = CommandBarrier::pending();
            self.recorder.retirement_barrier = Some(barrier.clone());
            Some(barrier)
        } else {
            None
        };
        let RecorderTransitionPlan {
            finalize,
            open: planned_open,
            saw_first_title,
            close_barrier,
            ..
        } = plan;
        let mut effects = Vec::new();
        let failed_open_temp = (!open_ok)
            .then(|| planned_open.as_ref().map(|open| open.temp_path.clone()))
            .flatten();
        if let Some(open) = planned_open.as_ref() {
            // Never reuse an admitted output path: a timed-out mpv reply can arrive late.
            self.recorder.temp_seq = open.id;
        }
        if let Some(finalize) = finalize {
            let mut segment = self
                .recorder
                .current
                .take()
                .expect("validated recorder finalize must have a segment");
            if finalize.force_incomplete {
                segment.incomplete = true;
            }
            let close_barrier = retirement_barrier
                .clone()
                .or_else(|| (!closure_proven).then_some(close_barrier).flatten());
            effects.extend(self.commit_finalized_segment(
                segment,
                finalize,
                close_barrier.clone(),
                close_barrier,
            ));
        }
        if open_ok && !transport_fenced {
            if let Some(open) = planned_open {
                self.recorder.current = Some(open.into_open_segment());
                self.recorder.execution_blocked = false;
            }
        } else if transport_fenced && let Some(open) = planned_open {
            // The replacement fence proves this old-generation writer cannot still be live. Its
            // partial incoming segment was never published as current and is safe to reclaim.
            effects.push(Cmd::Recorder(RecorderJob::Discard {
                temp: open.temp_path,
                close_barrier: None,
            }));
        }
        if command_failed
            && !transport_fenced
            && let Some(temp) = failed_open_temp
        {
            // The Restart command is returned ahead of this effect below. Once its synchronous
            // guard drop retires the uncertain writer, an uninstalled incoming path is garbage.
            effects.push(Cmd::Recorder(RecorderJob::Discard {
                temp,
                close_barrier: retirement_barrier.clone(),
            }));
        }
        if had_open || had_finalize {
            self.recorder.saw_first_title = if transport_fenced {
                false
            } else {
                saw_first_title
            };
        }
        self.dirty = true;

        if command_failed && !transport_fenced {
            let detail = close
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .or_else(|| open.as_ref().and_then(|result| result.as_ref().err()))
                .cloned()
                .unwrap_or_else(|| "mpv did not confirm the recorder command".to_owned());
            // Return the Restart first. Runtime dispatch synchronously drops/reaps the old mpv
            // guard before it reaches any following Save acceptance, so source fsync/journal
            // publication can never race a writer whose close command failed.
            let mut recovery =
                self.recover_player_transport(format!("recorder command failed: {detail}"));
            recovery.extend(effects);
            self.recorder.execution_blocked = true;
            self.recorder.restart_unblock_pending = true;
            self.status.kind = StatusKind::Error;
            self.status.text = match crate::i18n::current() {
                Language::Korean => {
                    format!("녹음을 중지했어요. 플레이어를 다시 시작합니다: {detail}")
                }
                Language::Japanese => {
                    format!("録音を停止しました。プレイヤーを再起動します: {detail}")
                }
                _ => format!("Recording paused; restarting the player: {detail}"),
            };
            return recovery;
        }

        effects.extend(self.reconcile_recorder());
        effects
    }

    /// Mark every recorder command owned by the failed actor as fenced by its replacement. The
    /// exact replies may arrive after replacement readiness, but can no longer justify another
    /// restart or install an old-generation open segment.
    pub(in crate::app) fn recorder_player_transport_recovery_started(&mut self) {
        self.recorder.transport_recovery_active = true;
        self.recorder.execution_blocked = true;
        self.recorder.restart_unblock_pending = true;
        if let Some(plan) = self.recorder.pending_transition.as_mut() {
            plan.transport_fenced = true;
        }
        self.dirty = true;
    }

    /// Complete the recorder-specific failure latch only after the runtime installed a fresh
    /// player generation. Fresh metadata from that actor may then open a new segment.
    pub(crate) fn recorder_player_restart_completed(&mut self, succeeded: bool) {
        self.recorder.transport_recovery_active = false;
        if !std::mem::take(&mut self.recorder.restart_unblock_pending) {
            return;
        }
        if succeeded {
            self.recorder.execution_blocked = false;
        }
        self.dirty = true;
    }

    /// Reconcile recorder execution with the latest config/capacity/metadata projection. Every
    /// external state change uses this one path; a pending exact-reply transition coalesces the
    /// request and its resolver runs reconciliation once more.
    pub(in crate::app) fn reconcile_recorder(&mut self) -> Vec<Cmd> {
        if self.recorder.pending_transition.is_some() {
            return Vec::new();
        }
        if !self.recorder_enabled() {
            return self.recorder_teardown();
        }
        let latest = self.playback.stream_now_playing.clone();
        let current_raw = self
            .recorder
            .current
            .as_ref()
            .map(|segment| segment.raw.as_str());
        if current_raw == latest.as_ref().map(|now| now.raw.as_str()) {
            Vec::new()
        } else {
            self.recorder_on_title(latest.as_ref())
        }
    }

    /// Retire a durable-spool backpressure latch and resume from the latest radio metadata.
    /// The first segment after the pause remains incomplete because the capacity gap means we
    /// necessarily joined it mid-song. If teardown is still awaiting mpv, its resolver performs
    /// the same latest-metadata reconciliation once that transition completes.
    fn remember_recorder_health(&mut self, warning: String, sticky: bool) {
        if sticky || !self.recorder.health_sticky {
            self.recorder.health_warning = Some(warning);
        }
        self.recorder.health_sticky |= sticky;
    }

    fn clear_live_recorder_health(&mut self) {
        if !self.recorder.health_sticky {
            self.recorder.health_warning = None;
        }
    }

    fn release_recorder_capacity(&mut self) -> Vec<Cmd> {
        self.recorder.capacity_blocked = false;
        self.recorder.capacity_blocked_id = None;
        self.recorder.capacity_owner_settled = false;
        self.recorder.capacity_probe_pending = false;
        self.recorder.capacity_retry_id = None;
        self.clear_live_recorder_health();
        self.dirty = true;
        self.reconcile_recorder()
    }

    fn recorder_capacity_available(
        &mut self,
        trigger_id: u64,
        capacity_available: bool,
        owner_terminal: bool,
    ) -> Vec<Cmd> {
        if !self.recorder.capacity_blocked {
            return Vec::new();
        }
        let exact_terminal =
            owner_terminal && self.recorder.capacity_blocked_id == Some(trigger_id);
        if exact_terminal {
            self.recorder.capacity_owner_settled = true;
            self.recorder.capacity_retry_id = None;
            if capacity_available {
                return self.release_recorder_capacity();
            }
            return Vec::new();
        }
        if !capacity_available {
            return Vec::new();
        }
        let Some(owner_id) = self.recorder.capacity_blocked_id else {
            // Startup inventory uncertainty has no correlated live owner. Only a fresh startup
            // recovery scan may clear it; an unrelated worker's stale capacity bit cannot.
            return Vec::new();
        };
        if self.recorder.capacity_owner_settled {
            if self.recorder.capacity_probe_pending {
                return Vec::new();
            }
            self.recorder.capacity_probe_pending = true;
            let final_dir = self
                .recorder
                .history
                .iter()
                .find(|track| track.id == owner_id)
                .and_then(|track| track.automatic_final_dir.clone())
                .unwrap_or_else(|| self.config.effective_recording_dir());
            return vec![Cmd::Recorder(RecorderJob::ProbeCapacity {
                owner_id,
                temp_dir: self.recorder.temp_dir.clone(),
                final_dir,
            })];
        }
        if self.recorder.capacity_retry_id.is_some() {
            return Vec::new();
        }
        if let Some(position) = self.recorder.history.iter().position(|track| {
            track.id == owner_id && matches!(track.state, RecordingState::AutomaticSaveBlocked)
        }) {
            // Keep capacity_blocked set until this exact retry reports Saved/AlreadySettled.
            // A synchronous second CapacityBlocked can therefore never race a precomputed open.
            self.recorder.history[position].state = RecordingState::AutomaticSaveRetrying;
            let id = self.recorder.history[position].id;
            self.recorder.capacity_retry_id = Some(id);
            let temp_dir = self.recorder.temp_dir.clone();
            let final_dir = self.recorder.history[position]
                .automatic_final_dir
                .clone()
                .unwrap_or_else(|| self.config.effective_recording_dir());
            self.recorder.history[position].save_request = Some(RecorderSaveRequest {
                final_dir,
                automatic: true,
                bypass_limits: false,
            });
            return vec![save_requested_cmd(
                &self.recorder.history[position],
                &temp_dir,
            )];
        }
        Vec::new()
    }

    fn settle_recorder_retired_segments(
        &mut self,
        save_barrier: CommandBarrier,
        discard_barrier: CommandBarrier,
    ) -> Vec<Cmd> {
        let mut effects = Vec::new();
        if let Some(plan) = self.recorder.pending_transition.take() {
            if let Some(open) = plan.open.as_ref() {
                self.recorder.temp_seq = self.recorder.temp_seq.max(open.id);
            }
            if let Some(finalize) = plan.finalize
                && let Some(mut segment) = self.recorder.current.take()
            {
                if finalize.force_incomplete {
                    segment.incomplete = true;
                }
                effects.extend(self.commit_finalized_segment(
                    segment,
                    finalize,
                    Some(save_barrier.clone()),
                    Some(discard_barrier.clone()),
                ));
            }
            if let Some(open) = plan.open {
                effects.push(Cmd::Recorder(RecorderJob::Discard {
                    temp: open.temp_path,
                    close_barrier: Some(discard_barrier.clone()),
                }));
            }
        } else if self.recorder.current.is_some() {
            let finalize = self.prepare_recorder_finalize(false, true);
            let mut segment = self
                .recorder
                .current
                .take()
                .expect("current recorder segment was checked above");
            segment.incomplete = true;
            effects.extend(self.commit_finalized_segment(
                segment,
                finalize,
                Some(save_barrier),
                Some(discard_barrier),
            ));
        }
        self.recorder.saw_first_title = false;
        self.dirty = true;
        effects
    }

    /// The runtime calls this only after dropping the failed actor and synchronously reaping its
    /// mpv process. A pending title boundary may therefore keep its outgoing segment, while an
    /// unplanned current segment is forced incomplete and never promoted to history.
    pub(crate) fn settle_recorder_player_retired(&mut self) -> Vec<Cmd> {
        if let Some(barrier) = self.recorder.retirement_barrier.take() {
            barrier.signal().succeed();
        }
        let barrier = CommandBarrier::pending();
        barrier.signal().succeed();
        self.settle_recorder_retired_segments(barrier.clone(), barrier)
    }

    /// Settle recorder ownership during orderly owner teardown. Player retirement happens first;
    /// a deliberately failed barrier makes accepted Saves defer their copy to barrier-free startup
    /// recovery, while dispatch still publishes each journal synchronously before admission closes.
    pub(crate) fn settle_recorder_owner_shutdown(&mut self) -> Vec<Cmd> {
        if let Some(barrier) = self.recorder.retirement_barrier.take() {
            barrier.signal().succeed();
        }
        let save_barrier = CommandBarrier::pending();
        save_barrier
            .signal()
            .fail("recording deferred until startup after player shutdown");
        let discard_barrier = CommandBarrier::pending();
        discard_barrier.signal().succeed();
        let mut effects =
            self.settle_recorder_retired_segments(save_barrier.clone(), discard_barrier.clone());
        self.recorder.execution_blocked = true;
        self.recorder.restart_unblock_pending = false;
        self.recorder.transport_recovery_active = false;
        self.recorder.capacity_retry_id = None;

        // Decide-mode history is session-scoped. Once the player process is retired, reclaim
        // every ordinary unsaved source explicitly so the next startup does not mistake a clean
        // quit for a hard-cut orphan. Durable Save ownership states remain in history below.
        let ordinary = self
            .recorder
            .history
            .iter()
            .enumerate()
            .filter_map(|(index, track)| {
                matches!(
                    track.state,
                    RecordingState::Recorded | RecordingState::RecordedReachedMaxDuration
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        for index in ordinary.into_iter().rev() {
            let track = self
                .recorder
                .history
                .remove(index)
                .expect("ordinary recorder history position was validated");
            effects.push(Cmd::Recorder(RecorderJob::Discard {
                temp: track.temp_path,
                close_barrier: Some(discard_barrier.clone()),
            }));
        }

        let already_emitted_save_ids = effects
            .iter()
            .filter_map(|effect| match effect {
                Cmd::Recorder(RecorderJob::Save { id, .. }) => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let requested = self
            .recorder
            .history
            .iter()
            .enumerate()
            .filter_map(|(index, track)| {
                (matches!(
                    track.state,
                    RecordingState::SaveRequested | RecordingState::AutomaticSaveRetrying
                ) && !already_emitted_save_ids.contains(&track.id))
                .then_some(index)
            })
            .collect::<Vec<_>>();
        for index in requested {
            if self.recorder.history[index].save_request.is_some() {
                // The exact request snapshot is preserved, but shutdown replaces any earlier
                // successful close acknowledgement with an execution fence. Acceptance still
                // publishes the journal synchronously; copy/tag work waits for startup recovery.
                self.recorder.history[index].close_barrier = Some(save_barrier.clone());
                effects.push(save_requested_cmd(
                    &self.recorder.history[index],
                    &self.recorder.temp_dir,
                ));
            }
        }

        let blocked = self
            .recorder
            .history
            .iter()
            .enumerate()
            .filter_map(|(index, track)| {
                matches!(track.state, RecordingState::AutomaticSaveBlocked).then_some(index)
            })
            .collect::<Vec<_>>();
        match blocked.as_slice() {
            [] => {}
            [_] if self.recorder.shutdown_bypass_attempted => {}
            [index] => {
                let index = *index;
                self.recorder.shutdown_bypass_attempted = true;
                self.recorder.history[index].state = RecordingState::SaveRequested;
                self.recorder.history[index].close_barrier = Some(save_barrier);
                let dir = self.recorder.history[index]
                    .automatic_final_dir
                    .clone()
                    .unwrap_or_else(|| self.config.effective_recording_dir());
                self.recorder.history[index].save_request = Some(RecorderSaveRequest {
                    final_dir: dir,
                    automatic: true,
                    bypass_limits: true,
                });
                let temp_dir = self.recorder.temp_dir.clone();
                // This is the sole source which caused recording to pause. Its bytes already
                // count toward owned inventory; bypass only numeric/inventory admission so one
                // tiny recovery journal can protect it during graceful exit.
                effects.push(save_requested_cmd(&self.recorder.history[index], &temp_dir));
            }
            _ => {
                let paths = blocked
                    .iter()
                    .map(|index| {
                        self.recorder.history[*index]
                            .temp_path
                            .display()
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::error!(%paths, "multiple blocked automatic recordings require manual recovery");
                self.status.kind = StatusKind::Error;
                self.status.text =
                    format!("Multiple blocked recording sources require manual recovery: {paths}");
            }
        }
        self.dirty = true;
        effects
    }

    /// Apply the non-player half of a finalized segment only after exact mpv replies prove the
    /// old writer was replaced or closed. When closure remains uncertain, the failed barrier is
    /// carried into the disk job so the source cannot be copied or removed.
    fn commit_finalized_segment(
        &mut self,
        seg: OpenSegment,
        finalize: RecorderFinalizePlan,
        save_close_barrier: Option<CommandBarrier>,
        discard_close_barrier: Option<CommandBarrier>,
    ) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let dur = finalize.duration_secs;

        let below_min = !finalize.reached_max && dur < finalize.minimum_duration_secs;
        if seg.incomplete || below_min {
            cmds.push(Cmd::Recorder(RecorderJob::Discard {
                temp: seg.temp_path,
                close_barrier: discard_close_barrier,
            }));
            return cmds;
        }

        let automatic_final_dir = finalize.automatic_final_dir.clone();
        let track = RecordedTrack {
            id: seg.id,
            title: seg.title,
            artist: seg.artist,
            raw: seg.raw,
            station: seg.station,
            temp_path: seg.temp_path,
            ext: seg.ext,
            duration_secs: dur,
            state: if automatic_final_dir.is_some() {
                RecordingState::SaveRequested
            } else if finalize.reached_max {
                RecordingState::RecordedReachedMaxDuration
            } else {
                RecordingState::Recorded
            },
            final_path: None,
            automatic_final_dir: automatic_final_dir.clone(),
            close_barrier: save_close_barrier,
            save_request: automatic_final_dir
                .clone()
                .map(|final_dir| RecorderSaveRequest {
                    final_dir,
                    automatic: true,
                    bypass_limits: false,
                }),
        };
        cmds.extend(self.recorder_push_history(track));

        if automatic_final_dir.is_some() {
            let temp_dir = self.recorder.temp_dir.clone();
            if let Some(track) = self
                .recorder
                .history
                .iter()
                .find(|track| track.id == seg.id)
            {
                cmds.push(save_requested_cmd(track, &temp_dir));
            }
        }
        cmds
    }

    /// Cut recording immediately (leaving radio, stopping, or app quit). The open segment is
    /// mid-song and is discarded only after mpv confirms the stream-record clear.
    pub(in crate::app) fn recorder_teardown(&mut self) -> Vec<Cmd> {
        if self.recorder.pending_transition.is_some() {
            return Vec::new();
        }
        let plan = self.prepare_recorder_teardown();
        if plan.finalize.is_none()
            && plan.open.is_none()
            && plan.saw_first_title == plan.expected_saw_first_title
        {
            return Vec::new();
        }
        self.recorder_transition_intent(plan)
    }

    /// Push a finished track to the front of the bounded history, de-duplicating a consecutive
    /// same-title unsaved entry (station re-sent the title / a flap) and evicting past the cap.
    /// Returns discard jobs for any temp file that is no longer reachable.
    fn recorder_push_history(&mut self, track: RecordedTrack) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let dup = matches!(track.state, RecordingState::Recorded)
            && self.recorder.history.front().is_some_and(|front| {
                matches!(front.state, RecordingState::Recorded)
                    && front.title == track.title
                    && front.artist == track.artist
                    && front.raw == track.raw
            });
        if dup && let Some(old) = self.recorder.history.pop_front() {
            cmds.push(Cmd::Recorder(RecorderJob::Discard {
                temp: old.temp_path,
                close_barrier: old.close_barrier,
            }));
        }
        self.recorder.history.push_front(track);

        let cap = self.config.effective_recording_past_tracks();
        while self.recorder.history.len() > cap {
            // Pending/blocked Saves are ownership records, not disposable UI history. Evict the
            // oldest ordinary row around them; if every row is protected, allow the bounded
            // durable-spool inventory to exceed the presentation cap until settlement.
            let Some(position) = self
                .recorder
                .history
                .iter()
                .rposition(|candidate| !recording_save_pending(candidate))
            else {
                break;
            };
            if let Some(old) = self.recorder.history.remove(position) {
                cmds.push(Cmd::Recorder(RecorderJob::Discard {
                    temp: old.temp_path,
                    close_barrier: old.close_barrier,
                }));
            }
        }
        cmds
    }

    /// Save a kept history track (Decide-mode "save"), or cancel the in-progress recording when
    /// `id` is the open segment.
    pub(in crate::app) fn recorder_save(&mut self, id: u64) -> Vec<Cmd> {
        let dir = self.config.effective_recording_dir();
        let temp_dir = self.recorder.temp_dir.clone();
        if self
            .recorder
            .current
            .as_ref()
            .is_some_and(|track| track.id == id)
        {
            self.status.kind = StatusKind::Error;
            self.status.text = t!(
                "This track is still recording",
                "이 트랙은 아직 녹음 중이에요",
                "この曲はまだ録音中です"
            )
            .to_owned();
            self.dirty = true;
            return Vec::new();
        }
        if let Some(track) = self
            .recorder
            .history
            .iter_mut()
            .find(|track| track.id == id)
        {
            if track.state.is_recorded() {
                if matches!(track.state, RecordingState::AutomaticSaveBlocked) {
                    self.recorder.capacity_retry_id = Some(id);
                }
                track.state = RecordingState::SaveRequested;
                track.save_request = Some(RecorderSaveRequest {
                    final_dir: dir,
                    automatic: false,
                    bypass_limits: false,
                });
                self.dirty = true;
                return vec![save_requested_cmd(track, &temp_dir)];
            }
            self.status.kind = StatusKind::Error;
            self.status.text = match track.state {
                RecordingState::Saved => t!(
                    "This recording is already saved",
                    "이미 저장된 녹음이에요",
                    "この録音は保存済みです"
                ),
                RecordingState::SaveRequested | RecordingState::AutomaticSaveRetrying => t!(
                    "This save is already requested and being prepared",
                    "이 저장 요청은 이미 준비 중이에요",
                    "この保存は既にリクエスト済みで準備中です"
                ),
                RecordingState::SavePending => t!(
                    "This save is already accepted",
                    "이 저장 요청은 이미 접수됐어요",
                    "この保存は既に受付済みです"
                ),
                _ => t!(
                    "This recording cannot be saved",
                    "이 녹음은 저장할 수 없어요",
                    "この録音は保存できません"
                ),
            }
            .to_owned();
            self.dirty = true;
        }
        Vec::new()
    }

    /// Discard a track: cancel it if it is the in-progress segment, else remove it from the
    /// browser (deleting its temp; an already-saved final file is kept).
    pub(in crate::app) fn recorder_discard(&mut self, id: u64) -> Vec<Cmd> {
        if self.recorder.current.as_ref().is_some_and(|s| s.id == id) {
            let plan = self.prepare_recorder_transition(
                Some(self.prepare_recorder_finalize(false, true)),
                None,
                self.recorder.saw_first_title,
            );
            return self.recorder_transition_intent(plan);
        }
        if let Some(pos) = self.recorder.history.iter().position(|t| t.id == id) {
            if matches!(
                self.recorder.history[pos].state,
                RecordingState::AutomaticSaveBlocked
            ) {
                let track = self
                    .recorder
                    .history
                    .remove(pos)
                    .expect("recording history position was validated");
                self.recorder.capacity_retry_id = None;
                self.recorder.capacity_blocked = false;
                self.recorder.capacity_blocked_id = None;
                self.recorder.capacity_owner_settled = false;
                self.recorder.capacity_probe_pending = false;
                self.clear_live_recorder_health();
                self.status.kind = StatusKind::Info;
                self.status.text = match crate::i18n::current() {
                    Language::Korean => {
                        "저장 대기 중인 녹음을 버렸어요. 자동 녹음을 다시 시작합니다".to_owned()
                    }
                    Language::Japanese => {
                        "保存待ちの録音を破棄しました。自動録音を再開します".to_owned()
                    }
                    _ => "Discarded the blocked recording; automatic recording resumed".to_owned(),
                };
                self.dirty = true;
                let mut cmds = vec![Cmd::Recorder(RecorderJob::Discard {
                    temp: track.temp_path,
                    close_barrier: track.close_barrier,
                })];
                cmds.extend(self.reconcile_recorder());
                return cmds;
            }
            if recording_save_pending(&self.recorder.history[pos]) {
                self.status.kind = StatusKind::Error;
                self.status.text = match crate::i18n::current() {
                    Language::Korean => {
                        "저장이 이미 접수되어 지금은 원본을 버릴 수 없어요".to_owned()
                    }
                    Language::Japanese => {
                        "保存が既に受け付けられているため、まだ元ファイルを破棄できません"
                            .to_owned()
                    }
                    _ => "This save is already accepted; its source cannot be discarded yet"
                        .to_owned(),
                };
                self.dirty = true;
                return Vec::new();
            }
            if let Some(track) = self.recorder.history.remove(pos) {
                self.dirty = true;
                return vec![Cmd::Recorder(RecorderJob::Discard {
                    temp: track.temp_path,
                    close_barrier: track.close_barrier,
                })];
            }
        }
        Vec::new()
    }

    /// Ids addressable in the recordings browser: the in-progress segment (if any) first,
    /// then the finished-track history (most-recent first).
    pub(in crate::app) fn recordings_browser_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        if let Some(seg) = self.recorder.current.as_ref() {
            ids.push(seg.id);
        }
        ids.extend(self.recorder.history.iter().map(|t| t.id));
        ids
    }

    /// Open a saved recording with the OS default handler; a not-yet-saved track hints to save.
    pub(in crate::app) fn recorder_reveal(&mut self, id: u64) {
        let path = self
            .recorder
            .history
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.final_path.clone());
        match path {
            Some(p) => crate::util::browser::open_path(&p),
            None => {
                self.status.kind = StatusKind::Info;
                self.status.text = match crate::i18n::current() {
                    Language::Korean => "재생하려면 먼저 저장하세요".to_owned(),
                    Language::Japanese => "再生するには先に保存してください".to_owned(),
                    _ => "Save the track first to play it".to_owned(),
                };
                self.dirty = true;
            }
        }
    }

    /// A recorder disk job finished: update the history row and toast (if enabled).
    pub(in crate::app) fn on_recorder_event(
        &mut self,
        ev: crate::recorder::job::RecorderEvent,
    ) -> Vec<Cmd> {
        use crate::recorder::job::RecorderEvent;
        let mut cmds = Vec::new();
        match ev {
            RecorderEvent::TransitionResolved {
                transition_id,
                close,
                open,
            } => return self.resolve_recorder_transition(transition_id, close, open),
            RecorderEvent::SaveAccepted { id } => {
                if let Some(track) = self
                    .recorder
                    .history
                    .iter_mut()
                    .find(|track| track.id == id)
                    && matches!(
                        track.state,
                        RecordingState::SaveRequested | RecordingState::AutomaticSaveRetrying
                    )
                {
                    track.state = RecordingState::SavePending;
                    track.save_request = None;
                    self.dirty = true;
                }
            }
            RecorderEvent::Saved {
                id,
                final_path,
                recovery_owned,
                durability_warning,
                capacity_available,
            } => {
                if let Some(track) = self.recorder.history.iter_mut().find(|t| t.id == id) {
                    track.state = if recovery_owned {
                        RecordingState::SavePending
                    } else {
                        RecordingState::Saved
                    };
                    track.final_path = Some(final_path.clone());
                    track.save_request = None;
                }
                if self.config.recording.notify && !recovery_owned {
                    let name = final_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let (title, body) = match crate::i18n::current() {
                        Language::Korean => ("녹음 저장됨".to_owned(), name.clone()),
                        Language::Japanese => ("録音を保存しました".to_owned(), name.clone()),
                        _ => ("Recording saved".to_owned(), name.clone()),
                    };
                    // In-app toast (always visible in the terminal) + a real desktop notification
                    // (OSC / native, resolved in the main loop). The toast is the final fallback.
                    self.status.kind = StatusKind::Info;
                    self.status.text = format!("{title}: {name}");
                    cmds.push(Cmd::DesktopNotify { title, body });
                }
                if let Some(warning) = durability_warning {
                    tracing::warn!(recording = %final_path.display(), %warning, "recording committed with durability warning");
                    self.status.kind = StatusKind::Error;
                    self.status.text = match crate::i18n::current() {
                        Language::Korean => {
                            format!("녹음은 저장됐지만 내구성 확인에 실패했어요: {warning}")
                        }
                        Language::Japanese => {
                            format!(
                                "録音は保存されましたが、永続性を確認できませんでした: {warning}"
                            )
                        }
                        _ => format!(
                            "Recording saved, but durability could not be confirmed: {warning}"
                        ),
                    };
                    self.remember_recorder_health(self.status.text.clone(), true);
                }
                self.dirty = true;
                cmds.extend(self.recorder_capacity_available(
                    id,
                    capacity_available,
                    !recovery_owned,
                ));
            }
            RecorderEvent::AlreadySettled {
                id,
                capacity_available,
            } => {
                if let Some(track) = self.recorder.history.iter_mut().find(|t| t.id == id) {
                    track.state = RecordingState::Saved;
                    track.save_request = None;
                }
                tracing::info!(
                    recording_id = id,
                    "stale recording worker observed peer settlement"
                );
                // A peer removed the exact journal while holding the same destination lock. Keep
                // the optimistic Saved state and never offer a duplicate retry.
                self.status.kind = StatusKind::Info;
                self.status.text = match crate::i18n::current() {
                    Language::Korean => "녹음 저장은 다른 복구 작업에서 이미 완료됐어요".to_owned(),
                    Language::Japanese => "録音の保存は別の復旧処理で既に完了しています".to_owned(),
                    _ => {
                        "Recording save was already completed by another recovery worker".to_owned()
                    }
                };
                self.dirty = true;
                cmds.extend(self.recorder_capacity_available(id, capacity_available, true));
            }
            RecorderEvent::SaveDeferred { id, error } => {
                if let Some(track) = self
                    .recorder
                    .history
                    .iter_mut()
                    .find(|track| track.id == id)
                    && matches!(
                        track.state,
                        RecordingState::SaveRequested | RecordingState::AutomaticSaveRetrying
                    )
                {
                    track.state = RecordingState::SavePending;
                    track.save_request = None;
                }
                // Keep the accepted/recovery-owned state: the original intent still owns this
                // source and will retry at startup. Re-enabling Save would create a second intent.
                self.status.kind = StatusKind::Error;
                self.status.text = match crate::i18n::current() {
                    Language::Korean => {
                        format!("녹음 저장이 지연됐어요(다음 시작 때 복구): {error}")
                    }
                    Language::Japanese => {
                        format!("録音の保存を延期しました(次回起動時に復旧): {error}")
                    }
                    _ => format!("Recording save deferred until startup recovery: {error}"),
                };
                self.remember_recorder_health(self.status.text.clone(), true);
                self.dirty = true;
            }
            RecorderEvent::SaveFailed {
                id,
                error,
                automatic,
            } => {
                let blocked_owner = self.recorder.capacity_blocked_id == Some(id);
                if self.recorder.capacity_retry_id == Some(id) {
                    self.recorder.capacity_retry_id = None;
                }
                let source = self
                    .recorder
                    .history
                    .iter()
                    .find(|track| track.id == id)
                    .map(|track| track.temp_path.display().to_string());
                if let Some(track) = self.recorder.history.iter_mut().find(|t| t.id == id) {
                    track.state = if automatic || blocked_owner {
                        RecordingState::AutomaticSaveBlocked
                    } else {
                        RecordingState::Recorded
                    };
                    track.save_request = None;
                }
                if automatic || blocked_owner {
                    self.recorder.capacity_blocked = true;
                    self.recorder.capacity_blocked_id = Some(id);
                    self.recorder.capacity_owner_settled = false;
                    self.recorder.capacity_probe_pending = false;
                    cmds.extend(self.recorder_teardown());
                }
                self.status.kind = StatusKind::Error;
                self.status.text = match crate::i18n::current() {
                    Language::Korean => {
                        if automatic {
                            format!(
                                "자동 녹음을 일시 중지했어요. 저장 준비 실패: {error} (원본: {})",
                                source.as_deref().unwrap_or("알 수 없음")
                            )
                        } else {
                            format!("녹음 저장 실패: {error}")
                        }
                    }
                    Language::Japanese => {
                        if automatic {
                            format!(
                                "自動録音を一時停止しました。保存の準備に失敗: {error} (元ファイル: {})",
                                source.as_deref().unwrap_or("不明")
                            )
                        } else {
                            format!("録音の保存に失敗しました: {error}")
                        }
                    }
                    _ => {
                        if automatic {
                            format!(
                                "Automatic recording paused; could not prepare save: {error} (source: {})",
                                source.as_deref().unwrap_or("unknown")
                            )
                        } else {
                            format!("Recording save failed: {error}")
                        }
                    }
                };
                if automatic || blocked_owner {
                    self.remember_recorder_health(self.status.text.clone(), false);
                }
                self.dirty = true;
            }
            RecorderEvent::CapacityBlocked {
                id,
                pending_count,
                pending_bytes,
            } => {
                if let Some(track) = self
                    .recorder
                    .history
                    .iter_mut()
                    .find(|track| track.id == id)
                {
                    track.state = RecordingState::AutomaticSaveBlocked;
                    track.save_request = None;
                }
                if self.recorder.capacity_retry_id == Some(id) {
                    self.recorder.capacity_retry_id = None;
                }
                self.recorder.capacity_blocked = true;
                self.recorder.capacity_blocked_id = Some(id);
                self.recorder.capacity_owner_settled = false;
                self.recorder.capacity_probe_pending = false;
                self.status.kind = StatusKind::Error;
                self.status.text = match crate::i18n::current() {
                    Language::Korean => format!(
                        "자동 녹음을 일시 중지했어요: 저장 대기 {pending_count}개 / {pending_bytes}바이트"
                    ),
                    Language::Japanese => format!(
                        "自動録音を一時停止しました: 保存待ち{pending_count}件 / {pending_bytes}バイト"
                    ),
                    _ => format!(
                        "Automatic recording paused: {pending_count} pending / {pending_bytes} bytes"
                    ),
                };
                self.remember_recorder_health(self.status.text.clone(), false);
                self.dirty = true;
                cmds.extend(self.recorder_teardown());
            }
            RecorderEvent::CapacityProbed {
                owner_id,
                capacity_available,
            } => {
                if self.recorder.capacity_blocked_id == Some(owner_id)
                    && self.recorder.capacity_owner_settled
                {
                    self.recorder.capacity_probe_pending = false;
                    if capacity_available {
                        cmds.extend(self.release_recorder_capacity());
                    }
                }
            }
        }
        cmds
    }
}

/// Build the off-loop copy+tag job for a kept track.
fn save_cmd_for(
    track: &RecordedTrack,
    temp_dir: &Path,
    dir: &Path,
    automatic: bool,
    bypass_limits: bool,
) -> Cmd {
    Cmd::Recorder(RecorderJob::Save {
        id: track.id,
        temp: track.temp_path.clone(),
        temp_dir: temp_dir.to_path_buf(),
        final_dir: dir.to_path_buf(),
        filename: track_filename_base(track.title.as_deref(), &track.raw),
        ext: track.ext,
        title: track.title.clone(),
        artist: track.artist.clone(),
        station: track.station.clone(),
        close_barrier: track.close_barrier.clone(),
        automatic,
        bypass_limits,
    })
}

fn save_requested_cmd(track: &RecordedTrack, temp_dir: &Path) -> Cmd {
    let request = track
        .save_request
        .as_ref()
        .expect("SaveRequested rows retain their exact request snapshot");
    save_cmd_for(
        track,
        temp_dir,
        &request.final_dir,
        request.automatic,
        request.bypass_limits,
    )
}

fn recording_save_pending(track: &RecordedTrack) -> bool {
    matches!(
        track.state,
        RecordingState::SaveRequested
            | RecordingState::SavePending
            | RecordingState::AutomaticSaveBlocked
            | RecordingState::AutomaticSaveRetrying
    )
}
