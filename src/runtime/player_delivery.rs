use std::collections::VecDeque;

use crate::app::{App, Cmd, PlayerIntent, PlayerMsg};
use crate::player::{PlayerCmd, pending};
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};

pub(super) const PENDING_PLAYER_CMDS_MAX: usize = pending::PLAYER_PENDING_MAX;
pub(super) const PENDING_PLAYER_INTENTS_MAX: usize = pending::PLAYER_PENDING_MAX;

#[derive(Clone, Default)]
pub(super) struct PendingPlayerCmds {
    cmds: VecDeque<PlayerCmd>,
}

impl PendingPlayerCmds {
    pub(super) fn push(&mut self, cmd: PlayerCmd) -> DeliveryResult {
        let coalesced = pending::push_pending_command(
            &mut self.cmds,
            cmd,
            PENDING_PLAYER_CMDS_MAX,
            DeliveryError::Saturated,
        )?;
        if coalesced {
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        } else {
            Ok(DeliveryReceipt::Deferred)
        }
    }

    pub(super) fn push_batch(&mut self, mut cmds: Vec<PlayerCmd>) -> DeliveryResult {
        match cmds.len() {
            0 => return Err(DeliveryError::Busy),
            1 => return self.push(cmds.pop().expect("batch length was checked")),
            _ => {}
        }
        let staged = pending::stage_pending_batch(
            &self.cmds,
            cmds,
            PENDING_PLAYER_CMDS_MAX,
            DeliveryError::Saturated,
        )?;
        self.cmds = staged.cmds;
        Ok(staged.receipt)
    }

    pub(super) fn drain(&mut self) -> Vec<PlayerCmd> {
        self.cmds.drain(..).collect()
    }

    pub(super) fn clear(&mut self) {
        self.cmds.clear();
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.cmds.len()
    }
}

/// One user-visible player transaction awaiting the first usable player actor.
///
/// Keep the intent whole until the real [`crate::player::PlayerHandle`] admits its command
/// batch. A second action cannot be planned faithfully from the uncommitted first action's
/// projected state (two quick `Next` presses would otherwise carry the same queue snapshot), so
/// startup/restart admits one complete transaction and explicitly rejects later work as Busy.
/// The command-count bound still prevents one large batch from bypassing the memory limit.
#[derive(Default)]
pub(super) struct PendingPlayerIntents {
    intents: VecDeque<PlayerIntent>,
    command_count: usize,
}

impl PendingPlayerIntents {
    pub(super) fn push(
        &mut self,
        intent: Box<PlayerIntent>,
    ) -> Result<DeliveryReceipt, (DeliveryError, Box<PlayerIntent>)> {
        let commands = intent.commands.len();
        if commands == 0 {
            return Err((DeliveryError::Busy, intent));
        }
        if !self.intents.is_empty() {
            return Err((DeliveryError::Busy, intent));
        }
        if commands > PENDING_PLAYER_INTENTS_MAX.saturating_sub(self.command_count) {
            return Err((DeliveryError::Saturated, intent));
        }
        self.command_count += commands;
        self.intents.push_back(*intent);
        Ok(DeliveryReceipt::Deferred)
    }

    pub(super) fn drain(&mut self) -> Vec<PlayerIntent> {
        self.command_count = 0;
        self.intents.drain(..).collect()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.intents.len()
    }

    #[cfg(test)]
    pub(super) fn command_count(&self) -> usize {
        self.command_count
    }
}

pub(super) fn report_player_delivery(app: &mut App, command: &'static str, result: DeliveryResult) {
    if let Err(error) = result {
        tracing::warn!(command, %error, "player command was not accepted");
        let message = match error {
            DeliveryError::Busy | DeliveryError::Saturated => crate::t!(
                "Player is busy; command was not accepted",
                "플레이어가 바빠 명령을 받지 못했습니다"
            ),
            _ => crate::t!(
                "Player is unavailable; command was not accepted",
                "플레이어를 사용할 수 없어 명령을 받지 못했습니다"
            ),
        };
        app.set_status_error(message);
    }
}

/// Complete the reducer half of a player intent from the one authoritative admission result.
/// Accepted means admitted to the bounded in-process lane (not acknowledged by mpv); rejected
/// leaves reducer state untouched and gives a correlated remote caller a truthful error.
pub(crate) fn settle_player_intent(
    app: &mut App,
    intent: PlayerIntent,
    result: DeliveryResult,
) -> Vec<Cmd> {
    let PlayerIntent {
        commit,
        label,
        remote_reply,
        ..
    } = intent;
    if matches!(&commit, crate::app::PlayerCommit::Seek { .. }) {
        app.settle_mouse_seek_admission(result.is_ok());
    }
    match result {
        Ok(receipt) => {
            tracing::trace!(command = label, ?receipt, "player intent accepted");
            let follow_ups = app.update(PlayerMsg::IntentAdmitted(commit));
            if let Some(reply) = remote_reply {
                let _ = reply.sender.send(app.resolve_remote_reply(reply.response));
            }
            follow_ups
        }
        Err(error) => {
            report_player_delivery(app, label, Err(error));
            if let Some(reply) = remote_reply {
                let code = match error {
                    DeliveryError::Busy | DeliveryError::Saturated => "player_busy",
                    _ => "player_unavailable",
                };
                let _ = reply
                    .sender
                    .send(crate::remote::proto::RemoteResponse::err(code));
            }
            Vec::new()
        }
    }
}

/// Submit one complete intent to a live player and settle it from that authoritative result.
/// The returned commands remain causally after the commit and must be dispatched by the caller.
pub(crate) fn admit_player_intent(
    player: &crate::player::PlayerHandle,
    app: &mut App,
    mut intent: PlayerIntent,
) -> Vec<Cmd> {
    if !intent.commit.is_current_for(app) {
        tracing::warn!(
            command = intent.label,
            "player intent snapshot became stale before admission"
        );
        return settle_player_intent(app, intent, Err(DeliveryError::Busy));
    }
    let commands = std::mem::take(&mut intent.commands);
    let result = player.send_batch(commands);
    settle_player_intent(app, intent, result)
}

pub(super) fn reject_pending_player_intents(
    pending: &mut PendingPlayerIntents,
    app: &mut App,
) -> Vec<Cmd> {
    pending
        .drain()
        .into_iter()
        .flat_map(|intent| settle_player_intent(app, intent, Err(DeliveryError::Closed)))
        .collect()
}

/// Atomically close the owner-side player admission boundary for process teardown.
///
/// The handle must drop before the process guard: closing the command lane first tells the IPC
/// actor that the ensuing transport EOF is intentional. Pending typed intents are then settled as
/// unavailable so correlated remote callers do not wait through the slower actor/persistence
/// cleanup which follows owner-loop exit.
pub(super) fn begin_player_shutdown_state<H, G>(
    restart: &mut PlayerRestartGate,
    player_failed: &mut bool,
    player_handle: &mut Option<H>,
    mpv_guard: &mut Option<G>,
    pending_cmds: &mut PendingPlayerCmds,
    pending_intents: &mut PendingPlayerIntents,
    app: &mut App,
) -> Vec<Cmd> {
    restart.suppress_for_shutdown();
    *player_failed = true;
    drop(player_handle.take());
    drop(mpv_guard.take());
    pending_cmds.clear();
    let mut follow_ups = reject_pending_player_intents(pending_intents, app);
    follow_ups.extend(app.settle_recorder_owner_shutdown());
    follow_ups
}

#[derive(Default)]
pub(super) struct PlayerRestartGate {
    requested: bool,
    in_flight: bool,
    used: bool,
    shutdown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlayerRestartDecision {
    Start,
    AlreadyPending,
    Exhausted,
    Suppressed,
}

impl PlayerRestartGate {
    pub(super) fn request(&mut self) -> PlayerRestartDecision {
        if self.shutdown {
            return PlayerRestartDecision::Suppressed;
        }
        if self.requested || self.in_flight {
            return PlayerRestartDecision::AlreadyPending;
        }
        if self.used {
            return PlayerRestartDecision::Exhausted;
        }
        self.used = true;
        self.requested = true;
        PlayerRestartDecision::Start
    }

    pub(super) fn take_request(&mut self) -> bool {
        if self.shutdown {
            self.requested = false;
            return false;
        }
        if !std::mem::take(&mut self.requested) {
            return false;
        }
        self.in_flight = true;
        true
    }

    pub(super) fn complete_start(&mut self) -> bool {
        std::mem::take(&mut self.in_flight)
    }

    /// Permanently close the automatic-replacement gate for process teardown.
    pub(super) fn suppress_for_shutdown(&mut self) {
        self.shutdown = true;
        self.requested = false;
    }
}

impl super::RuntimeHandles {
    pub fn player_event_is_current(&self, event: &crate::player::PlayerEvent) -> bool {
        let Some(event_generation) = event.file_generation() else {
            return true;
        };
        let current_generation = self
            .player_handle
            .as_ref()
            .map(crate::player::PlayerHandle::current_file_generation);
        let current = self
            .player_handle
            .as_ref()
            .is_some_and(|player| player.event_is_current(event));
        if !current {
            tracing::debug!(
                event_generation,
                ?current_generation,
                "ignored stale audio terminal event"
            );
        }
        current
    }

    pub(super) fn admit_player_restore_batch(
        &mut self,
        commands: Vec<PlayerCmd>,
    ) -> DeliveryResult {
        if let Some(player) = &self.player_handle {
            player.send_batch(commands)
        } else if self.player_failed {
            Err(DeliveryError::Closed)
        } else {
            self.pending_player_cmds.push_batch(commands)
        }
    }

    pub(super) fn dispatch_player_intent(&mut self, app: &mut App, intent: Box<PlayerIntent>) {
        let follow_ups = if let Some(player) = &self.player_handle {
            admit_player_intent(player, app, *intent)
        } else if self.player_failed {
            settle_player_intent(app, *intent, Err(DeliveryError::Closed))
        } else {
            match self.pending_player_intents.push(intent) {
                Ok(receipt) => {
                    tracing::trace!(?receipt, "player intent deferred until player readiness");
                    return;
                }
                Err((error, intent)) => settle_player_intent(app, *intent, Err(error)),
            }
        };
        for follow_up in follow_ups {
            self.dispatch(app, follow_up);
        }
    }

    fn settle_pending_player_intents(&mut self, app: &mut App) {
        let pending = self.pending_player_intents.drain();
        for intent in pending {
            let player = self
                .player_handle
                .as_ref()
                .expect("pending intents settle only after a player is installed");
            let follow_ups = admit_player_intent(player, app, intent);
            for follow_up in follow_ups {
                self.dispatch(app, follow_up);
            }
        }
    }

    fn reject_pending_player_intents(&mut self, app: &mut App) {
        let follow_ups = reject_pending_player_intents(&mut self.pending_player_intents, app);
        for follow_up in follow_ups {
            self.dispatch(app, follow_up);
        }
    }

    pub fn handle_player_ready(
        &mut self,
        result: Result<(crate::player::PlayerHandle, crate::player::Mpv), String>,
        app: &mut App,
    ) {
        let was_recovery = self.player_restart.complete_start();
        match result {
            Ok((handle, guard)) => {
                self.player_failed = false;
                // Install the live actor before settling any deferred intent. A commit may
                // produce a follow-up player command, which must target this actor immediately
                // instead of being re-deferred behind later startup work.
                self.player_handle = Some(handle);
                self._mpv_guard = Some(guard);
                if was_recovery {
                    app.set_status_info(crate::t!(
                        "Player connection restored",
                        "플레이어 연결을 복구했습니다"
                    ));
                }
                report_player_delivery(
                    app,
                    "set_volume",
                    self.player_handle
                        .as_ref()
                        .expect("player handle was installed above")
                        .send(PlayerCmd::SetVolume(app.playback.volume)),
                );
                if (app.playback.speed - 1.0).abs() > f64::EPSILON {
                    report_player_delivery(
                        app,
                        "set_speed",
                        self.player_handle
                            .as_ref()
                            .expect("player handle was installed above")
                            .send(PlayerCmd::SetProperty {
                                name: "speed".to_owned(),
                                value: serde_json::Value::from(app.playback.speed),
                            }),
                    );
                }
                if let Some(af) = crate::eq::build_af_string(&app.audio.bands, app.audio.normalize)
                {
                    report_player_delivery(
                        app,
                        "set_audio_filter",
                        self.player_handle
                            .as_ref()
                            .expect("player handle was installed above")
                            .send(PlayerCmd::SetAudioFilter(af)),
                    );
                }
                if !was_recovery && let Ok(url) = std::env::var("YTM_PLAY_URL") {
                    report_player_delivery(
                        app,
                        "load",
                        self.player_handle
                            .as_ref()
                            .expect("player handle was installed above")
                            .load(url),
                    );
                }
                let restore = self.pending_player_cmds.drain();
                if !restore.is_empty() {
                    // Recovery state is one atomic barrier ahead of every user intent accepted
                    // while the replacement actor was starting.
                    let result = self
                        .player_handle
                        .as_ref()
                        .expect("player handle was installed above")
                        .send_batch(restore);
                    report_player_delivery(app, "transport_restore", result);
                }
                self.settle_pending_player_intents(app);
                if was_recovery {
                    // Only this readiness result can retire the recorder failure latch: the
                    // restart gate proves it belongs to the replacement generation, and all
                    // queued restore work has already been admitted to that live actor.
                    app.recorder_player_restart_completed(true);
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to start mpv");
                self.player_failed = true;
                self.pending_player_cmds.clear();
                // Every deferred caller receives the same authoritative terminal result. No
                // reducer state or remote success was published while readiness was unknown.
                self.reject_pending_player_intents(app);
                if was_recovery {
                    app.recorder_player_restart_completed(false);
                    app.set_status_error(format!(
                        "{}: {e}",
                        crate::t!(
                            "Player restart failed",
                            "플레이어를 다시 시작하지 못했습니다"
                        )
                    ));
                } else if app.status.text.is_empty() {
                    app.set_status_error(crate::t!(
                        "Playback tool setup is required",
                        "재생 도구 설치가 필요합니다"
                    ));
                }
            }
        }
    }

    pub(super) fn handle_player_transport_closed(&mut self, app: &mut App) -> bool {
        // Close the command channel before dropping the process guard. The actor treats that
        // ordering as intentional shutdown, while its already-emitted TransportClosed remains
        // the final event from the old, single active actor.
        drop(self.player_handle.take());
        drop(self._mpv_guard.take());
        for effect in app.settle_recorder_player_retired() {
            self.dispatch(app, effect);
        }

        match self.player_restart.request() {
            PlayerRestartDecision::Start => {
                self.player_failed = false;
                true
            }
            PlayerRestartDecision::AlreadyPending => {
                tracing::debug!("player replacement already pending; ignoring duplicate request");
                false
            }
            PlayerRestartDecision::Exhausted => {
                self.player_failed = true;
                self.pending_player_cmds.clear();
                self.reject_pending_player_intents(app);
                app.set_status_error(crate::t!(
                    "Player stopped after the replacement also disconnected",
                    "교체한 플레이어 연결도 끊어져 재생을 중지했습니다"
                ));
                false
            }
            PlayerRestartDecision::Suppressed => {
                tracing::debug!("player replacement suppressed during shutdown");
                false
            }
        }
    }

    /// Close owner/player/background admission and retire the active process before slower
    /// shutdown.
    /// Repeated calls are harmless, which lets every owner-exit branch latch teardown eagerly and
    /// the common cleanup path enforce it once more defensively.
    pub fn begin_player_shutdown(&mut self, app: &mut App) {
        // This is the shutdown ordering boundary for every owner producer. Close the shared
        // ingress first, while task admission is still open: completions which finish from this
        // point onward retain their exact event in the task outbox. Consequently no later actor
        // event can enter the main queue and overtake that fallback during the final drain.
        self.worker_tx.close_admission();
        let follow_ups = begin_player_shutdown_state(
            &mut self.player_restart,
            &mut self.player_failed,
            &mut self.player_handle,
            &mut self._mpv_guard,
            &mut self.pending_player_cmds,
            &mut self.pending_player_intents,
            app,
        );
        for follow_up in follow_ups {
            self.dispatch(app, follow_up);
        }
        // A first-pass automatic Save can itself fail synchronously before journal acceptance.
        // Give that sole protected row the shutdown-only bounded bypass before closing admission.
        for follow_up in app.settle_recorder_owner_shutdown() {
            self.dispatch(app, follow_up);
        }
        // Automatic recorder Saves cross their synchronous journal boundary above. Their terminal
        // result now targets the fallback outbox because owner ingress is already closed. Closing
        // the pool rejects any later worker while the accepted journal/source remains recoverable.
        self.background_tasks.close_admission();
    }

    /// Consume the one automatic restart request. The runner replaces its readiness slot,
    /// which drops any obsolete startup result before launching the sole replacement actor.
    pub fn take_player_restart_request(&mut self) -> bool {
        self.player_restart.take_request()
    }
}
