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
            if let crate::app::PlayerCommit::SourceRecovery(plan) = &commit {
                app.reject_source_recovery(plan);
            }
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

/// Admit replacement restore as one barrier before the user transaction deferred during startup.
///
/// `PendingPlayerIntents` admits at most one whole transaction, so collecting its follow-ups
/// preserves the same reducer/dispatch order while making the recovery-before-user boundary one
/// directly testable operation.
pub(super) fn admit_ready_player_work(
    player: &crate::player::PlayerHandle,
    app: &mut App,
    restore: Vec<PlayerCmd>,
    pending: &mut PendingPlayerIntents,
) -> Vec<Cmd> {
    if !restore.is_empty() {
        report_player_delivery(app, "transport_restore", player.send_batch(restore));
    }
    pending
        .drain()
        .into_iter()
        .flat_map(|intent| admit_player_intent(player, app, intent))
        .collect()
}

/// Atomically close the owner-side player admission boundary for process teardown.
///
/// The handle must drop before the process guard: closing the command lane first tells the IPC
/// actor that the ensuing transport EOF is intentional. Pending typed intents are then settled as
/// unavailable so correlated remote callers do not wait through the slower actor/persistence
/// cleanup which follows owner-loop exit.
pub(super) fn begin_player_shutdown_state<H, G>(
    player: &mut RuntimePlayerLifecycle<H, G>,
    pending_intents: &mut PendingPlayerIntents,
    app: &mut App,
) -> Vec<Cmd> {
    player.begin_shutdown();
    let mut follow_ups = reject_pending_player_intents(pending_intents, app);
    follow_ups.extend(app.settle_recorder_owner_shutdown());
    follow_ups
}

pub(super) struct ActivePlayer<H, G> {
    handle: H,
    guard: G,
}

impl<H, G> ActivePlayer<H, G> {
    fn new(handle: H, guard: G) -> Self {
        Self { handle, guard }
    }

    /// Close actor ingress before dropping the process guard which kills mpv.
    fn retire(self) {
        drop(self.handle);
        drop(self.guard);
    }
}

/// Complete runtime ownership for the primary audio player.
///
/// Each variant is one reachable owner-loop phase. Keeping the live handle and process guard in
/// `ActivePlayer`, and the restore batch inside replacement startup, makes invalid flag/Option
/// combinations unrepresentable.
#[derive(Default)]
pub(super) enum RuntimePlayerLifecycle<H, G> {
    #[default]
    StartingInitial,
    LiveInitial(ActivePlayer<H, G>),
    FailedInitial,
    RestartQueued {
        restore: PendingPlayerCmds,
    },
    StartingReplacement {
        restore: PendingPlayerCmds,
    },
    LiveReplacement(ActivePlayer<H, G>),
    FailedReplacement,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlayerRestartDecision {
    Start,
    AlreadyPending,
    Exhausted,
    Suppressed,
}

pub(super) enum PlayerAdmission<'a, H> {
    Live(&'a H),
    Deferred,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlayerStartupKind {
    Initial,
    Replacement,
}

impl PlayerStartupKind {
    const fn is_replacement(self) -> bool {
        matches!(self, Self::Replacement)
    }
}

pub(super) enum PlayerStartupCompletion<E> {
    Ready {
        kind: PlayerStartupKind,
        restore: Vec<PlayerCmd>,
    },
    Failed {
        kind: PlayerStartupKind,
        error: E,
    },
    Discarded,
}

impl<H, G> RuntimePlayerLifecycle<H, G> {
    pub(super) fn admission(&self) -> PlayerAdmission<'_, H> {
        match self {
            Self::LiveInitial(player) | Self::LiveReplacement(player) => {
                PlayerAdmission::Live(&player.handle)
            }
            Self::StartingInitial
            | Self::RestartQueued { .. }
            | Self::StartingReplacement { .. } => PlayerAdmission::Deferred,
            Self::FailedInitial | Self::FailedReplacement | Self::Shutdown => {
                PlayerAdmission::Closed
            }
        }
    }

    pub(super) fn handle(&self) -> Option<&H> {
        match self.admission() {
            PlayerAdmission::Live(handle) => Some(handle),
            PlayerAdmission::Deferred | PlayerAdmission::Closed => None,
        }
    }

    pub(super) fn request_restart(
        &mut self,
        commands: Vec<PlayerCmd>,
    ) -> (PlayerRestartDecision, Option<DeliveryResult>) {
        let state = std::mem::replace(self, Self::Shutdown);
        match state {
            Self::StartingInitial | Self::FailedInitial => {
                let mut restore = PendingPlayerCmds::default();
                let delivery = (!commands.is_empty()).then(|| restore.push_batch(commands));
                *self = Self::RestartQueued { restore };
                (PlayerRestartDecision::Start, delivery)
            }
            Self::LiveInitial(player) => {
                player.retire();
                let mut restore = PendingPlayerCmds::default();
                let delivery = (!commands.is_empty()).then(|| restore.push_batch(commands));
                *self = Self::RestartQueued { restore };
                (PlayerRestartDecision::Start, delivery)
            }
            Self::RestartQueued { restore } => {
                *self = Self::RestartQueued { restore };
                (PlayerRestartDecision::AlreadyPending, None)
            }
            Self::StartingReplacement { restore } => {
                *self = Self::StartingReplacement { restore };
                (PlayerRestartDecision::AlreadyPending, None)
            }
            Self::LiveReplacement(player) => {
                player.retire();
                *self = Self::FailedReplacement;
                (PlayerRestartDecision::Exhausted, None)
            }
            Self::FailedReplacement => {
                *self = Self::FailedReplacement;
                (PlayerRestartDecision::Exhausted, None)
            }
            Self::Shutdown => {
                *self = Self::Shutdown;
                (PlayerRestartDecision::Suppressed, None)
            }
        }
    }

    pub(super) fn take_restart_request(&mut self) -> bool {
        let state = std::mem::replace(self, Self::Shutdown);
        match state {
            Self::RestartQueued { restore } => {
                *self = Self::StartingReplacement { restore };
                true
            }
            state => {
                *self = state;
                false
            }
        }
    }

    pub(super) fn complete_start<E>(
        &mut self,
        result: Result<(H, G), E>,
    ) -> PlayerStartupCompletion<E> {
        let state = std::mem::replace(self, Self::Shutdown);
        match (state, result) {
            (Self::StartingInitial, Ok((handle, guard))) => {
                *self = Self::LiveInitial(ActivePlayer::new(handle, guard));
                PlayerStartupCompletion::Ready {
                    kind: PlayerStartupKind::Initial,
                    restore: Vec::new(),
                }
            }
            (Self::StartingInitial, Err(error)) => {
                *self = Self::FailedInitial;
                PlayerStartupCompletion::Failed {
                    kind: PlayerStartupKind::Initial,
                    error,
                }
            }
            (Self::StartingReplacement { mut restore }, Ok((handle, guard))) => {
                let restore = restore.drain();
                *self = Self::LiveReplacement(ActivePlayer::new(handle, guard));
                PlayerStartupCompletion::Ready {
                    kind: PlayerStartupKind::Replacement,
                    restore,
                }
            }
            (Self::StartingReplacement { .. }, Err(error)) => {
                *self = Self::FailedReplacement;
                PlayerStartupCompletion::Failed {
                    kind: PlayerStartupKind::Replacement,
                    error,
                }
            }
            (state, Ok((handle, guard))) => {
                ActivePlayer::new(handle, guard).retire();
                *self = state;
                PlayerStartupCompletion::Discarded
            }
            (state, Err(_)) => {
                *self = state;
                PlayerStartupCompletion::Discarded
            }
        }
    }

    /// Permanently close admission and retire any live player in handle-before-guard order.
    pub(super) fn begin_shutdown(&mut self) {
        let state = std::mem::replace(self, Self::Shutdown);
        match state {
            Self::LiveInitial(player) | Self::LiveReplacement(player) => player.retire(),
            Self::StartingInitial
            | Self::FailedInitial
            | Self::RestartQueued { .. }
            | Self::StartingReplacement { .. }
            | Self::FailedReplacement
            | Self::Shutdown => {}
        }
    }
}

impl super::RuntimeHandles {
    /// Cache safety terminals are process-scoped. Project them against the latest synchronous
    /// admission immediately before reduction: same generation resumes the captured media,
    /// while a newer Load/Stop must recover solely from the owner's committed destination.
    pub fn reconcile_cache_safety_event(&self, event: &mut crate::player::PlayerEvent) {
        let current_generation = self
            .player
            .handle()
            .map(crate::player::PlayerHandle::current_file_generation);
        reconcile_cache_safety_event(event, current_generation);
    }

    pub fn player_event_is_current(&self, event: &crate::player::PlayerEvent) -> bool {
        let Some(event_generation) = event.file_generation() else {
            return true;
        };
        let current_generation = self
            .player
            .handle()
            .map(crate::player::PlayerHandle::current_file_generation);
        let current = self
            .player
            .handle()
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

    pub(super) fn dispatch_player_intent(&mut self, app: &mut App, intent: Box<PlayerIntent>) {
        let follow_ups = match self.player.admission() {
            PlayerAdmission::Live(player) => admit_player_intent(player, app, *intent),
            PlayerAdmission::Closed => {
                settle_player_intent(app, *intent, Err(DeliveryError::Closed))
            }
            PlayerAdmission::Deferred => match self.pending_player_intents.push(intent) {
                Ok(receipt) => {
                    tracing::trace!(?receipt, "player intent deferred until player readiness");
                    return;
                }
                Err((error, intent)) => settle_player_intent(app, *intent, Err(error)),
            },
        };
        for follow_up in follow_ups {
            self.dispatch(app, follow_up);
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
        match self.player.complete_start(result) {
            PlayerStartupCompletion::Ready { kind, restore } => {
                let was_recovery = kind.is_replacement();
                // The lifecycle installs the live actor before settling any deferred intent. A
                // commit may produce a follow-up player command, which must target this actor
                // immediately instead of being re-deferred behind later startup work.
                if was_recovery {
                    app.set_status_info(crate::t!(
                        "Player connection restored",
                        "플레이어 연결을 복구했습니다"
                    ));
                }
                report_player_delivery(
                    app,
                    "set_volume",
                    self.player
                        .handle()
                        .expect("player handle was installed above")
                        .send(PlayerCmd::SetVolume(app.playback.volume)),
                );
                if (app.playback.speed - 1.0).abs() > f64::EPSILON {
                    report_player_delivery(
                        app,
                        "set_speed",
                        self.player
                            .handle()
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
                        self.player
                            .handle()
                            .expect("player handle was installed above")
                            .send(PlayerCmd::SetAudioFilter(af)),
                    );
                }
                if !was_recovery && let Ok(url) = std::env::var("YTM_PLAY_URL") {
                    report_player_delivery(
                        app,
                        "load",
                        self.player
                            .handle()
                            .expect("player handle was installed above")
                            .load(url, crate::player::MediaSourceContext::OnDemand),
                    );
                }
                let follow_ups = admit_ready_player_work(
                    self.player
                        .handle()
                        .expect("player handle was installed above"),
                    app,
                    restore,
                    &mut self.pending_player_intents,
                );
                for follow_up in follow_ups {
                    self.dispatch(app, follow_up);
                }
                if was_recovery {
                    // Only this readiness result can retire the recorder failure latch: the
                    // lifecycle proves it belongs to the replacement generation, and all
                    // queued restore work has already been admitted to that live actor.
                    app.recorder_player_restart_completed(true);
                }
            }
            PlayerStartupCompletion::Failed { kind, error: e } => {
                let was_recovery = kind.is_replacement();
                tracing::error!(error = %e, "failed to start mpv");
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
            PlayerStartupCompletion::Discarded => {
                tracing::debug!("discarded player readiness outside its lifecycle phase");
            }
        }
    }

    pub(super) fn handle_player_transport_closed(
        &mut self,
        app: &mut App,
        restore: Vec<PlayerCmd>,
    ) -> bool {
        // Requesting replacement retires any live player in handle-before-guard order and moves
        // the ordered restore batch into the same lifecycle transition.
        let (decision, restore_delivery) = self.player.request_restart(restore);
        for effect in app.settle_recorder_player_retired() {
            self.dispatch(app, effect);
        }

        match decision {
            PlayerRestartDecision::Start => {
                if let Some(result) = restore_delivery {
                    report_player_delivery(app, "transport_restore", result);
                }
                true
            }
            PlayerRestartDecision::AlreadyPending => {
                tracing::debug!("player replacement already pending; ignoring duplicate request");
                false
            }
            PlayerRestartDecision::Exhausted => {
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
        let follow_ups =
            begin_player_shutdown_state(&mut self.player, &mut self.pending_player_intents, app);
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
        self.player.take_restart_request()
    }
}

pub(super) fn reconcile_cache_safety_event(
    event: &mut crate::player::PlayerEvent,
    current_generation: Option<u64>,
) {
    let crate::player::PlayerEvent::CacheEmergency {
        file_generation,
        reason,
        ..
    } = event
    else {
        return;
    };
    if current_generation != Some(*file_generation) {
        *event = crate::player::PlayerEvent::CacheReplacementEmergency { reason: *reason };
    }
}
