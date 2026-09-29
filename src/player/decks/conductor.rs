use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use serde_json::Value;
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

struct ExtraSpawnInput<'a> {
    audio: &'a crate::config::MpvAudioRuntimeConfig,
    cookies_file: Option<&'a Path>,
    standby_cache_args: &'a [String],
    generation: u64,
    gate: &'a Arc<EventGate>,
    owner_emit: &'a EventSink,
    intentional_close: &'a Arc<AtomicBool>,
    file_generation_rx: watch::Receiver<u64>,
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

#[derive(Default)]
pub(super) struct DeckSettings {
    audio_filter: Option<String>,
    af_commands: Vec<(String, String, String)>,
    properties: Vec<(String, Value)>,
    pause: Option<Value>,
    volume: i64,
    selected_device: Option<Option<String>>,
}

impl DeckSettings {
    pub(super) fn with_volume(volume: i64) -> Self {
        Self {
            volume,
            ..Self::default()
        }
    }

    fn observe(&mut self, cmd: &PlayerCmd) {
        match cmd {
            PlayerCmd::SetAudioFilter(filter) => {
                self.audio_filter = Some(filter.clone());
                self.af_commands.clear();
            }
            PlayerCmd::AfCommand {
                label,
                param,
                value,
            } if self.audio_filter.is_some() => {
                self.af_commands
                    .push((label.clone(), param.clone(), value.clone()));
            }
            PlayerCmd::SetProperty { name, value } if name == "pause" => {
                self.pause = Some(value.clone());
            }
            PlayerCmd::CyclePause => {
                if let Some(Value::Bool(paused)) = self.pause.as_mut() {
                    *paused = !*paused;
                }
            }
            PlayerCmd::SetProperty { name, value } => {
                if let Some((_, current)) = self
                    .properties
                    .iter_mut()
                    .find(|(current_name, _)| current_name == name)
                {
                    *current = value.clone();
                } else {
                    self.properties.push((name.clone(), value.clone()));
                }
            }
            PlayerCmd::SetVolume(volume) => self.volume = *volume,
            PlayerCmd::SelectAudioDevice { device, .. } => {
                self.selected_device = Some(device.clone());
            }
            _ => {}
        }
    }

    async fn replay(&self, tx: &Sender<PlayerCmd>) -> bool {
        if let Some(filter) = &self.audio_filter
            && !forward(tx, PlayerCmd::SetAudioFilter(filter.clone())).await
        {
            return false;
        }
        for (label, param, value) in &self.af_commands {
            if !forward(
                tx,
                PlayerCmd::AfCommand {
                    label: label.clone(),
                    param: param.clone(),
                    value: value.clone(),
                },
            )
            .await
            {
                return false;
            }
        }
        for (name, value) in &self.properties {
            if !forward(
                tx,
                PlayerCmd::SetProperty {
                    name: name.clone(),
                    value: value.clone(),
                },
            )
            .await
            {
                return false;
            }
        }
        if !forward(tx, PlayerCmd::SetVolume(self.volume)).await {
            return false;
        }
        if let Some(pause) = &self.pause
            && !forward(
                tx,
                PlayerCmd::SetProperty {
                    name: "pause".to_owned(),
                    value: pause.clone(),
                },
            )
            .await
        {
            return false;
        }
        if let Some(device) = &self.selected_device
            && !forward(
                tx,
                PlayerCmd::ReplayAudioDevice {
                    device: device.clone(),
                },
            )
            .await
        {
            return false;
        }
        true
    }
}

pub struct ConductorInput {
    pub cmd_rx: Receiver<PlayerCmd>,
    pub lead_tx: Sender<PlayerCmd>,
    pub emit: EventSink,
    pub audio: crate::config::MpvAudioRuntimeConfig,
    pub overlap_enabled: bool,
    pub cookies_file: Option<PathBuf>,
    pub standby_cache_args: Vec<String>,
    pub gate: Arc<EventGate>,
    pub intentional_close: Arc<AtomicBool>,
    pub file_generation_rx: watch::Receiver<u64>,
    pub proof_rx: Receiver<ExtraProof>,
}

pub(super) struct Conductor {
    pub(super) primary_tx: Sender<PlayerCmd>,
    pub(super) primary_available: bool,
    pub(super) extra: Option<ExtraDeck>,
    pub(super) extra_is_lead: bool,
    pub(super) extra_has_file: bool,
    pub(super) fade: Option<Fade>,
    pub(super) volume: i64,
    pub(super) settings: DeckSettings,
    pub(super) next_deck_generation: u64,
    pub(super) warming: Option<JoinHandle<Result<ExtraDeck, OverlapBlocker>>>,
    pub(super) gate: Arc<EventGate>,
    pub(super) emit: EventSink,
    pub(super) audio: crate::config::MpvAudioRuntimeConfig,
    pub(super) overlap_enabled: bool,
    pub(super) cookies_file: Option<PathBuf>,
    pub(super) standby_cache_args: Vec<String>,
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
        overlap_enabled,
        cookies_file,
        standby_cache_args,
        gate,
        intentional_close,
        file_generation_rx,
        mut proof_rx,
    } = input;

    let mut conductor = Conductor {
        primary_tx: lead_tx,
        primary_available: true,
        extra: None,
        extra_is_lead: false,
        extra_has_file: false,
        fade: None,
        volume: 100,
        settings: DeckSettings::with_volume(100),
        next_deck_generation: 1,
        warming: None,
        gate,
        emit,
        audio,
        overlap_enabled,
        cookies_file,
        standby_cache_args,
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
                if !conductor.apply_extra_proof(proof).await {
                    return;
                }
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
                    if !conductor
                        .apply_extra_proof(ExtraProof::Failed { epoch })
                        .await
                    {
                        return;
                    }
                    continue;
                }
                if conductor.fade.is_none() {
                    continue;
                }
                if let Some((elapsed, length)) = conductor.fade.as_ref().map(|current| {
                    (
                        Instant::now().saturating_duration_since(current.started),
                        current.length,
                    )
                }) {
                    if !conductor.apply_volumes(Some((elapsed, length))).await {
                        return;
                    }
                    if elapsed >= length.duration() && !conductor.finish_current_fade().await {
                        return;
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
            PlayerCmd::LoadWithResume(_) => {
                if !self.abort_current_fade().await {
                    return false;
                }
                let _ = self.cancel_pending_overlap().await;
                let _ = self.return_lead_to_primary().await;
                self.forward_lead(cmd).await
            }
            PlayerCmd::Stop => {
                if !self.abort_current_fade().await {
                    return false;
                }
                let _ = self.cancel_pending_overlap().await;
                self.forward_lead(cmd).await
            }
            PlayerCmd::SeekRelative(_) | PlayerCmd::SeekAbsolute { .. } => {
                if !self.abort_current_fade().await {
                    return false;
                }
                self.forward_lead(cmd).await
            }
            PlayerCmd::SetOverlap(enabled) => {
                self.overlap_enabled = enabled;
                if enabled {
                    self.warm_extra();
                    true
                } else {
                    self.retire_extra().await
                }
            }
            PlayerCmd::SetVolume(next) => {
                self.volume = next;
                self.settings.observe(&PlayerCmd::SetVolume(next));
                let fade = self.fade.as_ref().map(|current| {
                    (
                        Instant::now().saturating_duration_since(current.started),
                        current.length,
                    )
                });
                self.apply_volumes(fade).await
            }
            cmd if is_pause_broadcast(&cmd) => {
                self.settings.observe(&cmd);
                if !self.forward_lead(cmd.clone()).await {
                    return false;
                }
                if self.pending_overlap.is_some() {
                    return self.forward_pending_command(cmd).await;
                }
                if self.fade.is_some()
                    && let Some(retiring) =
                        retiring_tx(&self.primary_tx, self.extra.as_ref(), self.extra_is_lead)
                {
                    if forward(retiring, cmd).await {
                        return true;
                    }
                    self.drop_non_lead(!self.extra_is_lead);
                }
                true
            }
            PlayerCmd::SetAudioFilter(_)
            | PlayerCmd::AfCommand { .. }
            | PlayerCmd::SetProperty { .. } => self.forward_deck_setting(cmd).await,
            PlayerCmd::SelectAudioDevice {
                correlation_id,
                device,
            } => {
                self.forward_audio_device_selection(correlation_id, device)
                    .await
            }
            other => self.forward_lead(other).await,
        }
    }

    async fn arm_overlap(&mut self, load: PlaybackLoad, length: FadeLength) -> bool {
        if !self.overlap_enabled {
            return self
                .forward_lead(PlayerCmd::Load(load.with_handoff(TrackHandoff::Cut)))
                .await;
        }
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
        let incoming_is_extra = !self.extra_is_lead;
        if !self.abort_current_fade().await {
            return false;
        }
        let _ = self.cancel_pending_overlap().await;
        if !self.settings.replay(&incoming_tx).await {
            self.drop_non_lead(incoming_is_extra);
            (self.emit)(PlayerEvent::OverlapUnavailable(OverlapBlocker::Mpv));
            return self
                .forward_lead(PlayerCmd::Load(load.with_handoff(TrackHandoff::Cut)))
                .await;
        }
        if !forward(
            &incoming_tx,
            PlayerCmd::SetVolume(scaled_volume(self.volume, 0.0)),
        )
        .await
        {
            self.drop_non_lead(incoming_is_extra);
            (self.emit)(PlayerEvent::OverlapUnavailable(OverlapBlocker::Mpv));
            return self
                .forward_lead(PlayerCmd::Load(load.with_handoff(TrackHandoff::Cut)))
                .await;
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
            self.drop_non_lead(incoming_is_extra);
            (self.emit)(PlayerEvent::OverlapUnavailable(OverlapBlocker::Mpv));
            return self
                .forward_lead(PlayerCmd::Load(load.with_handoff(TrackHandoff::Cut)))
                .await;
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
        if !self.abort_current_fade().await {
            return false;
        }
        let _ = self.cancel_pending_overlap().await;
        if self.return_lead_to_primary().await {
            self.warm_extra();
            self.forward_lead(PlayerCmd::Load(load)).await
        } else {
            self.forward_lead(PlayerCmd::Load(load)).await
        }
    }

    async fn return_lead_to_primary(&mut self) -> bool {
        if !self.primary_available {
            return false;
        }
        // The fade left the retiring primary at zero volume, and setting changes made while the
        // extra led never reached it; mpv keeps both across `loadfile`.
        if self.extra_is_lead && !self.settings.replay(&self.primary_tx).await {
            self.primary_available = false;
            return false;
        }
        self.set_extra_is_lead(false);
        self.extra_has_file = false;
        if let Some(extra_tx) = self.extra.as_ref().map(|deck| deck.tx.clone())
            && !forward(&extra_tx, PlayerCmd::Stop).await
        {
            self.extra.take();
        }
        if !self.overlap_enabled {
            self.extra.take();
        }
        true
    }

    async fn cancel_pending_overlap(&mut self) -> Option<PlaybackLoad> {
        let Some(pending) = self.pending_overlap.take() else {
            self.gate.clear_pending();
            return None;
        };
        self.gate.clear_pending();
        if !self.extra_is_lead {
            self.extra_has_file = false;
        }
        let incoming_is_extra = !self.extra_is_lead;
        if let Some(incoming) = self.incoming_tx()
            && !forward(&incoming, PlayerCmd::Stop).await
        {
            self.drop_non_lead(incoming_is_extra);
        }
        Some(pending.dest)
    }

    async fn forward_pending_command(&mut self, cmd: PlayerCmd) -> bool {
        let incoming_is_extra = !self.extra_is_lead;
        let Some(incoming) = self.incoming_tx() else {
            return self
                .abandon_pending_after_incoming_failure(incoming_is_extra)
                .await;
        };
        if forward(&incoming, cmd).await {
            return true;
        }
        self.abandon_pending_after_incoming_failure(incoming_is_extra)
            .await
    }

    async fn forward_deck_setting(&mut self, cmd: PlayerCmd) -> bool {
        self.settings.observe(&cmd);
        if !self.forward_lead(cmd.clone()).await {
            return false;
        }
        if self.pending_overlap.is_some() {
            self.forward_pending_command(cmd).await
        } else {
            true
        }
    }

    async fn forward_audio_device_selection(
        &mut self,
        correlation_id: u64,
        device: Option<String>,
    ) -> bool {
        self.settings.observe(&PlayerCmd::SelectAudioDevice {
            correlation_id,
            device: device.clone(),
        });
        if !self
            .forward_lead(PlayerCmd::SelectAudioDevice {
                correlation_id,
                device: device.clone(),
            })
            .await
        {
            return false;
        }
        if self.pending_overlap.is_some() {
            self.forward_pending_command(PlayerCmd::ReplayAudioDevice { device })
                .await
        } else {
            true
        }
    }

    async fn abandon_pending_after_incoming_failure(&mut self, incoming_is_extra: bool) -> bool {
        self.drop_non_lead(incoming_is_extra);
        let pending = self.pending_overlap.take();
        self.gate.clear_pending();
        let Some(pending) = pending else {
            return true;
        };
        (self.emit)(PlayerEvent::OverlapUnavailable(OverlapBlocker::Mpv));
        self.forward_lead(PlayerCmd::Load(
            pending.dest.with_handoff(TrackHandoff::Cut),
        ))
        .await
    }

    pub(super) async fn apply_extra_proof(&mut self, proof: ExtraProof) -> bool {
        if let ExtraProof::DeckClosed { from_extra } = &proof {
            self.handle_deck_closed(*from_extra);
            return true;
        }
        let Some(pending) = self.pending_overlap.as_ref() else {
            return true;
        };
        if proof.epoch() != Some(pending.epoch) {
            return true;
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
                self.apply_volumes(Some((Duration::ZERO, pending.fade)))
                    .await
            }
            ExtraProof::Failed { .. } => {
                let incoming_is_extra = !self.extra_is_lead;
                if let Some(incoming) = self.incoming_tx()
                    && !forward(&incoming, PlayerCmd::Stop).await
                {
                    self.drop_non_lead(incoming_is_extra);
                }
                if !self.extra_is_lead {
                    self.extra_has_file = false;
                }
                self.forward_lead(PlayerCmd::Load(
                    pending.dest.with_handoff(TrackHandoff::Cut),
                ))
                .await
            }
            ExtraProof::TransportClosed { from_extra, .. } => {
                self.drop_non_lead(from_extra);
                (self.emit)(PlayerEvent::OverlapUnavailable(OverlapBlocker::Mpv));
                self.forward_lead(PlayerCmd::Load(
                    pending.dest.with_handoff(TrackHandoff::Cut),
                ))
                .await
            }
            ExtraProof::DeckClosed { .. } => {
                unreachable!("deck close is handled before pending proof")
            }
        }
    }

    async fn retire_extra(&mut self) -> bool {
        tracing::info!(
            extra_was_lead = self.extra_is_lead,
            extra_present = self.extra.is_some(),
            warming = self.warming.is_some(),
            pending = self.pending_overlap.is_some(),
            fading = self.fade.is_some(),
            "retire_extra"
        );
        if !self.abort_current_fade().await {
            return false;
        }
        let pending_destination = self.cancel_pending_overlap().await;
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
        if let Some(destination) = pending_destination {
            return self
                .forward_lead(PlayerCmd::Load(destination.with_handoff(TrackHandoff::Cut)))
                .await;
        }
        tracing::info!(
            extra_is_lead = self.extra_is_lead,
            extra_present = self.extra.is_some(),
            extra_has_file = self.extra_has_file,
            pending = self.pending_overlap.is_some(),
            fading = self.fade.is_some(),
            "deck_state"
        );
        true
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
            self.primary_available.then(|| self.primary_tx.clone())
        } else {
            self.extra.as_ref().map(|deck| deck.tx.clone())
        }
    }

    async fn forward_lead(&mut self, cmd: PlayerCmd) -> bool {
        let from_extra = self.extra_is_lead;
        let tx = if from_extra {
            self.extra.as_ref().map(|deck| deck.tx.clone())
        } else {
            self.primary_available.then(|| self.primary_tx.clone())
        };
        let Some(tx) = tx else {
            self.emit_lead_closed(from_extra);
            return false;
        };
        if forward(&tx, cmd).await {
            return true;
        }
        self.handle_lead_closed(from_extra);
        false
    }

    fn drop_non_lead(&mut self, from_extra: bool) {
        if from_extra {
            self.extra_has_file = false;
            self.extra.take();
        } else {
            self.primary_available = false;
        }
    }

    fn handle_deck_closed(&mut self, from_extra: bool) {
        if from_extra == self.extra_is_lead {
            self.drop_lead(from_extra);
        } else {
            self.drop_non_lead(from_extra);
        }
    }

    fn handle_lead_closed(&mut self, from_extra: bool) {
        self.drop_lead(from_extra);
        self.emit_lead_closed(from_extra);
    }

    fn drop_lead(&mut self, from_extra: bool) {
        if from_extra {
            self.extra_has_file = false;
            self.extra.take();
        } else {
            self.primary_available = false;
        }
    }

    fn emit_lead_closed(&self, from_extra: bool) {
        if self.intentional_close.load(Ordering::Acquire) {
            return;
        }
        let deck = if from_extra { "standby" } else { "primary" };
        (self.emit)(PlayerEvent::TransportClosed(format!(
            "{deck} deck transport closed"
        )));
    }

    async fn abort_current_fade(&mut self) -> bool {
        if self.fade.take().is_none() {
            return true;
        }
        self.gate.fading.store(false, Ordering::Release);
        if let Some(retiring) =
            retiring_tx(&self.primary_tx, self.extra.as_ref(), self.extra_is_lead).cloned()
            && !forward(&retiring, PlayerCmd::Stop).await
        {
            self.drop_non_lead(!self.extra_is_lead);
        }
        self.forward_lead(PlayerCmd::SetVolume(self.volume)).await
    }

    async fn finish_current_fade(&mut self) -> bool {
        self.gate.fading.store(false, Ordering::Release);
        if let Some(retiring) =
            retiring_tx(&self.primary_tx, self.extra.as_ref(), self.extra_is_lead).cloned()
            && !forward(&retiring, PlayerCmd::Stop).await
        {
            self.drop_non_lead(!self.extra_is_lead);
        }
        self.fade = None;
        self.forward_lead(PlayerCmd::SetVolume(self.volume)).await
    }

    async fn apply_volumes(&mut self, fade: Option<(Duration, FadeLength)>) -> bool {
        let (out_gain, in_gain) = match fade {
            Some((elapsed, length)) => {
                let (out, incoming) = envelope(elapsed, length);
                (out.get(), incoming.get())
            }
            None => (0.0, 1.0),
        };
        if !self
            .forward_lead(PlayerCmd::SetVolume(scaled_volume(self.volume, in_gain)))
            .await
        {
            return false;
        }
        let Some(retiring) = fade
            .and_then(|_| retiring_tx(&self.primary_tx, self.extra.as_ref(), self.extra_is_lead))
            .cloned()
        else {
            return true;
        };
        if forward(
            &retiring,
            PlayerCmd::SetVolume(scaled_volume(self.volume, out_gain)),
        )
        .await
        {
            return true;
        }
        self.drop_non_lead(!self.extra_is_lead);
        self.fade = None;
        self.gate.fading.store(false, Ordering::Release);
        self.forward_lead(PlayerCmd::SetVolume(self.volume)).await
    }

    pub(super) fn warm_extra(&mut self) {
        if !self.overlap_enabled || self.extra.is_some() || self.warming.is_some() {
            return;
        }
        #[cfg(test)]
        if let Some(spawn) = self.standby_spawn {
            self.warming = Some(tokio::spawn(async move { spawn() }));
            return;
        }
        let audio = self.audio.clone();
        let cookies_file = self.cookies_file.clone();
        let standby_cache_args = self.standby_cache_args.clone();
        let gate = Arc::clone(&self.gate);
        let emit = Arc::clone(&self.emit);
        let intentional_close = Arc::clone(&self.intentional_close);
        let file_generation_rx = self.file_generation_rx.clone();
        let generation = self.next_deck_generation;
        self.next_deck_generation = self.next_deck_generation.saturating_add(1);
        self.warming = Some(tokio::spawn(async move {
            spawn_extra(ExtraSpawnInput {
                audio: &audio,
                cookies_file: cookies_file.as_deref(),
                standby_cache_args: &standby_cache_args,
                generation,
                gate: &gate,
                owner_emit: &emit,
                intentional_close: &intentional_close,
                file_generation_rx,
            })
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
        match spawn_extra(ExtraSpawnInput {
            audio: &self.audio,
            cookies_file: self.cookies_file.as_deref(),
            standby_cache_args: &self.standby_cache_args,
            generation: self.next_deck_generation,
            gate: &self.gate,
            owner_emit: &self.emit,
            intentional_close: &self.intentional_close,
            file_generation_rx: self.file_generation_rx.clone(),
        })
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

pub(super) fn scaled_volume(user: i64, gain: f64) -> i64 {
    ((user as f64) * gain.clamp(0.0, 1.0)).round() as i64
}

pub(super) fn is_pause_broadcast(cmd: &PlayerCmd) -> bool {
    matches!(cmd, PlayerCmd::CyclePause)
        || matches!(cmd, PlayerCmd::SetProperty { name, .. } if name == "pause")
}

async fn spawn_extra(input: ExtraSpawnInput<'_>) -> Result<ExtraDeck, OverlapBlocker> {
    let ExtraSpawnInput {
        audio,
        cookies_file,
        standby_cache_args,
        generation,
        gate,
        owner_emit,
        intentional_close,
        file_generation_rx,
    } = input;
    let ipc_path = mpv::deck_ipc_path(generation).map_err(|_| OverlapBlocker::Mpv)?;
    let guarded = mpv::spawn_standby(&ipc_path, cookies_file, audio, standby_cache_args)
        .map_err(|_| OverlapBlocker::Mpv)?;
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
