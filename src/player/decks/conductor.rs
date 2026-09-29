use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::super::cache_runtime;
use super::super::cache_support;
use super::super::ipc;
use super::super::mpv::{self};
use super::super::{EventSink, Mpv, PlaybackLoad, PlayerCmd, PlayerEvent};
use super::proof::FADE_TICK;
use crate::config::LongFormSeekOptimization;
use crate::crossfade::{FadeLength, OverlapBlocker, TrackHandoff, envelope};
use crate::util::backpressure;

use super::gate::EventGate;
use super::proof::{ExtraProof, OVERLAP_PROOF_TIMEOUT};

pub(super) struct ExtraDeck {
    pub(super) tx: Sender<PlayerCmd>,
    pub(super) _mpv: Option<Mpv>,
}

pub(super) struct PendingOverlap {
    pub(super) dest: PlaybackLoad,
    pub(super) fade: FadeLength,
    pub(super) deadline: Instant,
    pub(super) epoch: u64,
}

pub(super) struct Fade {
    length: FadeLength,
    started: Instant,
}

pub struct ConductorInput {
    pub cmd_rx: Receiver<PlayerCmd>,
    pub lead_tx: Sender<PlayerCmd>,
    pub emit: EventSink,
    pub audio: crate::config::MpvAudioRuntimeConfig,
    pub gate: Arc<EventGate>,
    pub intentional_close: Arc<AtomicBool>,
    pub file_generation_rx: watch::Receiver<u64>,
    pub proof_rx: Receiver<ExtraProof>,
}

pub(super) struct Conductor {
    pub(super) primary_tx: Sender<PlayerCmd>,
    pub(super) extra: Option<ExtraDeck>,
    pub(super) extra_is_lead: bool,
    pub(super) extra_has_file: bool,
    pub(super) fade: Option<Fade>,
    pub(super) volume: i64,
    pub(super) next_deck_generation: u64,
    pub(super) warming: Option<JoinHandle<Result<ExtraDeck, OverlapBlocker>>>,
    pub(super) gate: Arc<EventGate>,
    pub(super) emit: EventSink,
    pub(super) audio: crate::config::MpvAudioRuntimeConfig,
    pub(super) intentional_close: Arc<AtomicBool>,
    pub(super) file_generation_rx: watch::Receiver<u64>,
    pub(super) pending_overlap: Option<PendingOverlap>,
    #[cfg(test)]
    pub(super) standby_spawn: Option<fn() -> Result<ExtraDeck, OverlapBlocker>>,
}

pub async fn run_conductor(input: ConductorInput) {
    let ConductorInput {
        mut cmd_rx,
        lead_tx,
        emit,
        audio,
        gate,
        intentional_close,
        file_generation_rx,
        mut proof_rx,
    } = input;

    let mut conductor = Conductor {
        primary_tx: lead_tx,
        extra: None,
        extra_is_lead: false,
        extra_has_file: false,
        fade: None,
        volume: 100,
        next_deck_generation: 1,
        warming: None,
        gate,
        emit,
        audio,
        intentional_close,
        file_generation_rx,
        pending_overlap: None,
        #[cfg(test)]
        standby_spawn: None,
    };
    let mut tick = tokio::time::interval(FADE_TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    return;
                };
                if !conductor.handle_command(cmd).await {
                    return;
                }
            }
            proof = proof_rx.recv() => {
                let Some(proof) = proof else {
                    return;
                };
                conductor.apply_extra_proof(proof).await;
            }
            _ = tick.tick(), if conductor.fade.is_some() || conductor.pending_overlap.is_some() => {
                if conductor.pending_overlap.as_ref().is_some_and(|pending| {
                    Instant::now() >= pending.deadline
                }) {
                    let epoch = conductor
                        .pending_overlap
                        .as_ref()
                        .expect("deadline branch is guarded")
                        .epoch;
                    conductor
                        .apply_extra_proof(ExtraProof::Failed { epoch })
                        .await;
                    continue;
                }
                if conductor.fade.is_none() {
                    continue;
                }
                if let Some(current) = conductor.fade.as_ref() {
                    let elapsed = Instant::now().saturating_duration_since(current.started);
                    let length = current.length;
                    apply_volumes(
                        conductor.volume,
                        conductor.extra.as_ref(),
                        conductor.extra_is_lead,
                        &conductor.primary_tx,
                        Some((elapsed, length)),
                    )
                    .await;
                    if elapsed >= length.duration() {
                        finish_fade(
                            &conductor.primary_tx,
                            conductor.extra.as_ref(),
                            conductor.extra_is_lead,
                            conductor.volume,
                            &conductor.gate,
                        )
                        .await;
                        conductor.fade = None;
                    }
                }
            }
        }
    }
}

impl Conductor {
    pub(super) async fn handle_command(&mut self, cmd: PlayerCmd) -> bool {
        match cmd {
            PlayerCmd::Load(load) => match load.handoff() {
                TrackHandoff::Overlap { fade: length } => self.arm_overlap(load, length).await,
                TrackHandoff::Cut => self.cut_load(load).await,
            },
            PlayerCmd::LoadWithResume(_) | PlayerCmd::Stop => {
                self.abort_current_fade().await;
                self.cancel_pending_overlap().await;
                self.forward_lead(cmd).await
            }
            PlayerCmd::SeekRelative(_) | PlayerCmd::SeekAbsolute { .. } => {
                self.abort_current_fade().await;
                self.forward_lead(cmd).await
            }
            PlayerCmd::RetireExtra => {
                self.retire_extra().await;
                true
            }
            PlayerCmd::SetVolume(next) => {
                self.volume = next;
                apply_volumes(
                    self.volume,
                    self.extra.as_ref(),
                    self.extra_is_lead,
                    &self.primary_tx,
                    self.fade.as_ref().map(|current| {
                        (
                            Instant::now().saturating_duration_since(current.started),
                            current.length,
                        )
                    }),
                )
                .await;
                true
            }
            cmd if is_pause_broadcast(&cmd) => {
                if !self.forward_lead(cmd.clone()).await {
                    return false;
                }
                if self.fade.is_some()
                    && let Some(retiring) =
                        retiring_tx(&self.primary_tx, self.extra.as_ref(), self.extra_is_lead)
                {
                    return forward(retiring, cmd).await;
                }
                true
            }
            other => self.forward_lead(other).await,
        }
    }

    async fn arm_overlap(&mut self, load: PlaybackLoad, length: FadeLength) -> bool {
        if let Err(blocker) = self.ensure_extra().await {
            (self.emit)(PlayerEvent::OverlapUnavailable(blocker));
            return self
                .forward_lead(PlayerCmd::Load(load.with_handoff(TrackHandoff::Cut)))
                .await;
        }
        let Some(incoming_tx) = self.incoming_tx() else {
            (self.emit)(PlayerEvent::OverlapUnavailable(OverlapBlocker::Mpv));
            return self
                .forward_lead(PlayerCmd::Load(load.with_handoff(TrackHandoff::Cut)))
                .await;
        };
        self.abort_current_fade().await;
        self.cancel_pending_overlap().await;
        if !forward(
            &incoming_tx,
            PlayerCmd::SetVolume(scaled_volume(self.volume, 0.0)),
        )
        .await
        {
            return false;
        }
        let incoming_generation = load
            .reserved_file_generation()
            .unwrap_or_else(|| self.gate.admitted.load(Ordering::Acquire));
        if !forward(
            &incoming_tx,
            PlayerCmd::Load(
                load.clone()
                    .with_handoff(TrackHandoff::Cut)
                    .with_reserved_file_generation(incoming_generation),
            ),
        )
        .await
        {
            return false;
        }
        if !self.extra_is_lead {
            self.extra_has_file = false;
        }
        let epoch = self
            .gate
            .arm_pending(!self.extra_is_lead, incoming_generation);
        self.pending_overlap = Some(PendingOverlap {
            dest: load,
            fade: length,
            deadline: Instant::now() + OVERLAP_PROOF_TIMEOUT,
            epoch,
        });
        true
    }

    async fn cut_load(&mut self, load: PlaybackLoad) -> bool {
        self.abort_current_fade().await;
        self.cancel_pending_overlap().await;
        if self.extra_is_lead && (self.extra.is_none() || !self.extra_has_file) {
            self.set_extra_is_lead(false);
            if let Some(extra) = self.extra.as_ref() {
                let _ = forward(&extra.tx, PlayerCmd::Stop).await;
            }
        }
        self.warm_extra();
        self.forward_lead(PlayerCmd::Load(load)).await
    }

    async fn cancel_pending_overlap(&mut self) {
        if self.pending_overlap.take().is_none() {
            self.gate.clear_pending();
            return;
        }
        self.gate.clear_pending();
        if !self.extra_is_lead {
            self.extra_has_file = false;
        }
        if let Some(incoming) = self.incoming_tx() {
            let _ = forward(&incoming, PlayerCmd::Stop).await;
        }
    }

    pub(super) async fn apply_extra_proof(&mut self, proof: ExtraProof) {
        let Some(pending) = self.pending_overlap.as_ref() else {
            return;
        };
        if proof.epoch() != pending.epoch {
            return;
        }
        let pending = self
            .pending_overlap
            .take()
            .expect("pending overlap was just matched");
        self.gate.clear_pending();
        match proof {
            ExtraProof::Ready { .. } => {
                self.extra_has_file = true;
                self.set_extra_is_lead(!self.extra_is_lead);
                self.gate.fading.store(true, Ordering::Release);
                self.fade = Some(Fade {
                    length: pending.fade,
                    started: Instant::now(),
                });
                apply_volumes(
                    self.volume,
                    self.extra.as_ref(),
                    self.extra_is_lead,
                    &self.primary_tx,
                    Some((Duration::ZERO, pending.fade)),
                )
                .await;
            }
            ExtraProof::Failed { .. } => {
                if !self.extra_is_lead {
                    self.extra_has_file = false;
                }
                let _ = self
                    .forward_lead(PlayerCmd::Load(
                        pending.dest.with_handoff(TrackHandoff::Cut),
                    ))
                    .await;
            }
            ExtraProof::TransportClosed { .. } => {
                if !self.extra_is_lead {
                    self.extra_has_file = false;
                    self.extra.take();
                }
                let _ = self
                    .forward_lead(PlayerCmd::Load(
                        pending.dest.with_handoff(TrackHandoff::Cut),
                    ))
                    .await;
            }
        }
    }

    async fn retire_extra(&mut self) {
        tracing::info!(
            extra_was_lead = self.extra_is_lead,
            extra_present = self.extra.is_some(),
            warming = self.warming.is_some(),
            pending = self.pending_overlap.is_some(),
            fading = self.fade.is_some(),
            "retire_extra"
        );
        self.abort_current_fade().await;
        self.cancel_pending_overlap().await;
        self.gate.fading.store(false, Ordering::Release);
        if let Some(task) = self.warming.take() {
            task.abort();
        }
        if self.extra_owns_playback() {
            self.extra_has_file = false;
        } else {
            self.set_extra_is_lead(false);
            self.extra_has_file = false;
            if let Some(extra) = self.extra.take() {
                let _ = forward(&extra.tx, PlayerCmd::Stop).await;
            }
        }
        tracing::info!(
            extra_is_lead = self.extra_is_lead,
            extra_present = self.extra.is_some(),
            extra_has_file = self.extra_has_file,
            pending = self.pending_overlap.is_some(),
            fading = self.fade.is_some(),
            "deck_state"
        );
    }

    fn extra_owns_playback(&self) -> bool {
        self.extra_is_lead && self.extra.is_some()
    }

    pub(super) fn set_extra_is_lead(&mut self, extra_is_lead: bool) {
        self.extra_is_lead = extra_is_lead;
        self.gate
            .extra_is_lead
            .store(extra_is_lead, Ordering::Release);
    }

    fn incoming_tx(&self) -> Option<Sender<PlayerCmd>> {
        if self.extra_is_lead {
            Some(self.primary_tx.clone())
        } else {
            self.extra.as_ref().map(|deck| deck.tx.clone())
        }
    }

    async fn forward_lead(&self, cmd: PlayerCmd) -> bool {
        forward(
            lead_tx(&self.primary_tx, self.extra.as_ref(), self.extra_is_lead),
            cmd,
        )
        .await
    }

    async fn abort_current_fade(&mut self) {
        abort_fade(
            &mut self.fade,
            &self.gate,
            &self.primary_tx,
            self.extra.as_ref(),
            self.extra_is_lead,
            self.volume,
        )
        .await;
    }

    pub(super) fn warm_extra(&mut self) {
        if self.extra.is_some() || self.warming.is_some() {
            return;
        }
        let audio = self.audio.clone();
        let gate = Arc::clone(&self.gate);
        let emit = Arc::clone(&self.emit);
        let intentional_close = Arc::clone(&self.intentional_close);
        let file_generation_rx = self.file_generation_rx.clone();
        let generation = self.next_deck_generation;
        self.next_deck_generation = self.next_deck_generation.saturating_add(1);
        self.warming = Some(tokio::spawn(async move {
            spawn_extra(
                &audio,
                generation,
                &gate,
                &emit,
                &intentional_close,
                file_generation_rx,
            )
            .await
        }));
    }

    pub(super) async fn ensure_extra(&mut self) -> Result<(), OverlapBlocker> {
        if self.extra.is_some() {
            return Ok(());
        }
        if let Some(task) = self.warming.take()
            && let Ok(Ok(deck)) = task.await
        {
            self.extra = Some(deck);
            return Ok(());
        }
        #[cfg(test)]
        if let Some(spawn) = self.standby_spawn {
            let deck = spawn()?;
            self.extra = Some(deck);
            return Ok(());
        }
        match spawn_extra(
            &self.audio,
            self.next_deck_generation,
            &self.gate,
            &self.emit,
            &self.intentional_close,
            self.file_generation_rx.clone(),
        )
        .await
        {
            Ok(deck) => {
                self.next_deck_generation = self.next_deck_generation.saturating_add(1);
                self.extra = Some(deck);
                Ok(())
            }
            Err(blocker) => Err(blocker),
        }
    }
}

fn lead_tx<'a>(
    primary: &'a Sender<PlayerCmd>,
    extra: Option<&'a ExtraDeck>,
    extra_is_lead: bool,
) -> &'a Sender<PlayerCmd> {
    if extra_is_lead {
        extra.map(|deck| &deck.tx).unwrap_or(primary)
    } else {
        primary
    }
}

fn retiring_tx<'a>(
    primary: &'a Sender<PlayerCmd>,
    extra: Option<&'a ExtraDeck>,
    extra_is_lead: bool,
) -> Option<&'a Sender<PlayerCmd>> {
    extra?;
    Some(lead_tx(primary, extra, !extra_is_lead))
}

async fn abort_fade(
    fade: &mut Option<Fade>,
    gate: &EventGate,
    primary_tx: &Sender<PlayerCmd>,
    extra: Option<&ExtraDeck>,
    extra_is_lead: bool,
    volume: i64,
) {
    if fade.take().is_none() {
        return;
    }
    gate.fading.store(false, Ordering::Release);
    if extra.is_none() {
        let _ = forward(primary_tx, PlayerCmd::SetVolume(volume)).await;
        return;
    }
    if let Some(retiring) = retiring_tx(primary_tx, extra, extra_is_lead) {
        let _ = forward(retiring, PlayerCmd::Stop).await;
    }
    let _ = forward(
        lead_tx(primary_tx, extra, extra_is_lead),
        PlayerCmd::SetVolume(volume),
    )
    .await;
}

async fn finish_fade(
    primary_tx: &Sender<PlayerCmd>,
    extra: Option<&ExtraDeck>,
    extra_is_lead: bool,
    volume: i64,
    gate: &EventGate,
) {
    gate.fading.store(false, Ordering::Release);
    if let Some(retiring) = retiring_tx(primary_tx, extra, extra_is_lead) {
        let _ = forward(retiring, PlayerCmd::Stop).await;
    }
    let _ = forward(
        lead_tx(primary_tx, extra, extra_is_lead),
        PlayerCmd::SetVolume(volume),
    )
    .await;
}

async fn apply_volumes(
    user_volume: i64,
    extra: Option<&ExtraDeck>,
    extra_is_lead: bool,
    primary_tx: &Sender<PlayerCmd>,
    fade: Option<(Duration, FadeLength)>,
) {
    let (out_gain, in_gain) = match fade {
        Some((elapsed, length)) => {
            let (out, incoming) = envelope(elapsed, length);
            (out.get(), incoming.get())
        }
        None => (0.0, 1.0),
    };
    let lead = lead_tx(primary_tx, extra, extra_is_lead);
    let _ = forward(
        lead,
        PlayerCmd::SetVolume(scaled_volume(user_volume, in_gain)),
    )
    .await;
    if fade.is_some()
        && let Some(retiring) = retiring_tx(primary_tx, extra, extra_is_lead)
    {
        let _ = forward(
            retiring,
            PlayerCmd::SetVolume(scaled_volume(user_volume, out_gain)),
        )
        .await;
    }
}

pub(super) fn scaled_volume(user: i64, gain: f64) -> i64 {
    ((user as f64) * gain.clamp(0.0, 1.0)).round() as i64
}

pub(super) fn is_pause_broadcast(cmd: &PlayerCmd) -> bool {
    matches!(cmd, PlayerCmd::CyclePause)
        || matches!(cmd, PlayerCmd::SetProperty { name, .. } if name == "pause")
}

pub(super) async fn spawn_extra(
    audio: &crate::config::MpvAudioRuntimeConfig,
    generation: u64,
    gate: &Arc<EventGate>,
    owner_emit: &EventSink,
    intentional_close: &Arc<AtomicBool>,
    file_generation_rx: watch::Receiver<u64>,
) -> Result<ExtraDeck, OverlapBlocker> {
    let ipc_path = mpv::deck_ipc_path(generation).map_err(|_| OverlapBlocker::Mpv)?;
    let guarded = mpv::spawn_standby(&ipc_path, audio).map_err(|_| OverlapBlocker::Mpv)?;
    let extra_mpv = Mpv::from_guarded(guarded, ipc_path.clone());
    let conn = ipc::connect_retry(&ipc_path)
        .await
        .map_err(|_| OverlapBlocker::OutputBusy)?;
    let (tx, rx) = backpressure::bounded_channel(backpressure::PLAYER_CMD_QUEUE);
    let cache_support =
        cache_support::prepare_cache_support(audio, None, None, mpv::flag_supported);
    let cache_runtime = cache_runtime::CacheRuntime::for_standby_process(
        cache_support,
        LongFormSeekOptimization::Off,
    );
    let cache_status = Arc::new(std::sync::Mutex::new(
        super::super::SharedLongFormSeekStatus::isolated(cache_runtime.status()),
    ));
    let extra_sink = gate.sink(true, Arc::clone(owner_emit));
    tokio::spawn(ipc::run_actor(ipc::ActorInput {
        conn,
        cmd_rx: rx,
        emit: extra_sink,
        intentional_close: Arc::clone(intentional_close),
        file_generation_rx,
        route_provider: crate::playback_target::PlaybackRouteProviderHandle::disabled(),
        route_revocations: Arc::new(super::super::RouteRevocationRegistry::default()),
        cache_runtime,
        cache_status,
    }));
    Ok(ExtraDeck {
        tx,
        _mpv: Some(extra_mpv),
    })
}

pub(super) async fn forward(tx: &Sender<PlayerCmd>, cmd: PlayerCmd) -> bool {
    tx.send(cmd).await.is_ok()
}
