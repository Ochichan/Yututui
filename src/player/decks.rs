use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::{
    EventSink, Mpv, PlaybackLoad, PlayerCmd, PlayerEvent, cache_runtime, cache_support, ipc, mpv,
};
use crate::config::LongFormSeekOptimization;
use crate::crossfade::{FadeLength, OverlapBlocker, TrackHandoff, envelope};
use crate::util::backpressure;

const FADE_TICK: Duration = Duration::from_millis(25);
const OVERLAP_PROOF_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExtraProof {
    Ready,
    Failed,
}

pub(super) struct EventGate {
    extra_is_lead: AtomicBool,
    fading: AtomicBool,
    admitted: Arc<AtomicU64>,
    pending_incoming_extra: AtomicBool,
    pending_generation: AtomicU64,
    proof: Option<Sender<ExtraProof>>,
}

impl EventGate {
    #[cfg(test)]
    pub(super) fn new(admitted: Arc<AtomicU64>) -> Arc<Self> {
        Self::with_proof(admitted, None)
    }

    pub(super) fn with_proof(
        admitted: Arc<AtomicU64>,
        proof: Option<Sender<ExtraProof>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            extra_is_lead: AtomicBool::new(false),
            fading: AtomicBool::new(false),
            admitted,
            pending_incoming_extra: AtomicBool::new(false),
            pending_generation: AtomicU64::new(0),
            proof,
        })
    }

    pub(super) fn sink(self: &Arc<Self>, from_extra: bool, emit: EventSink) -> EventSink {
        let gate = Arc::clone(self);
        Arc::new(move |event| gate.emit(from_extra, event, &emit))
    }

    fn arm_pending(&self, incoming_extra: bool, generation: u64) {
        self.pending_incoming_extra
            .store(incoming_extra, Ordering::Release);
        self.pending_generation.store(generation, Ordering::Release);
    }

    fn clear_pending(&self) {
        self.pending_generation.store(0, Ordering::Release);
    }

    fn observe_pending(&self, from_extra: bool, event: &PlayerEvent) -> Option<ExtraProof> {
        let generation = self.pending_generation.load(Ordering::Acquire);
        if generation == 0 {
            return None;
        }
        if from_extra != self.pending_incoming_extra.load(Ordering::Acquire) {
            return None;
        }
        match event.unscoped() {
            PlayerEvent::Error(_) | PlayerEvent::TransportClosed(_) => Some(ExtraProof::Failed),
            PlayerEvent::TimePos(_) | PlayerEvent::Duration(Some(_))
                if event.file_generation() == Some(generation) =>
            {
                Some(ExtraProof::Ready)
            }
            _ => None,
        }
    }

    fn emit(&self, from_extra: bool, event: PlayerEvent, sink: &EventSink) {
        if let Some(proof) = self.observe_pending(from_extra, &event)
            && let Some(tx) = &self.proof
        {
            let _ = tx.try_send(proof);
        }
        let unscoped = event.unscoped();
        if matches!(
            unscoped,
            PlayerEvent::CacheEmergency { .. } | PlayerEvent::CacheReplacementEmergency { .. }
        ) {
            sink(event);
            return;
        }
        if matches!(unscoped, PlayerEvent::Volume(_)) && self.fading.load(Ordering::Acquire) {
            return;
        }
        if self.pending_generation.load(Ordering::Acquire) != 0 {
            if self.admit_pending_incoming(from_extra, &event) {
                if from_extra {
                    let generation = self.admitted.load(Ordering::Acquire);
                    sink(rewrite_to_admitted_generation(event, generation));
                } else {
                    sink(event);
                }
            }
            return;
        }
        let extra_leads = self.extra_is_lead.load(Ordering::Acquire);
        if from_extra != extra_leads {
            return;
        }
        if from_extra {
            let generation = self.admitted.load(Ordering::Acquire);
            sink(rewrite_to_admitted_generation(event, generation));
        } else {
            sink(event);
        }
    }

    fn admit_pending_incoming(&self, from_extra: bool, event: &PlayerEvent) -> bool {
        let generation = self.pending_generation.load(Ordering::Acquire);
        if generation == 0
            || from_extra != self.pending_incoming_extra.load(Ordering::Acquire)
            || event.file_generation() != Some(generation)
        {
            return false;
        }
        matches!(
            event.unscoped(),
            PlayerEvent::TimePos(_) | PlayerEvent::Duration(Some(_)) | PlayerEvent::Paused(_)
        )
    }
}

fn rewrite_to_admitted_generation(event: PlayerEvent, generation: u64) -> PlayerEvent {
    match event {
        PlayerEvent::FileScoped { event, .. } => PlayerEvent::FileScoped {
            file_generation: generation,
            event,
        },
        event => event,
    }
}

struct ExtraDeck {
    tx: Sender<PlayerCmd>,
    _mpv: Option<Mpv>,
}

struct PendingOverlap {
    dest: PlaybackLoad,
    fade: FadeLength,
    deadline: Instant,
}

struct Fade {
    length: FadeLength,
    started: Instant,
}

pub(super) struct ConductorInput {
    pub cmd_rx: Receiver<PlayerCmd>,
    pub lead_tx: Sender<PlayerCmd>,
    pub emit: EventSink,
    pub audio: crate::config::MpvAudioRuntimeConfig,
    pub gate: Arc<EventGate>,
    pub intentional_close: Arc<AtomicBool>,
    pub file_generation_rx: watch::Receiver<u64>,
    pub proof_rx: Receiver<ExtraProof>,
}

struct Conductor {
    primary_tx: Sender<PlayerCmd>,
    extra: Option<ExtraDeck>,
    extra_is_lead: bool,
    extra_has_file: bool,
    fade: Option<Fade>,
    volume: i64,
    next_deck_generation: u64,
    warming: Option<JoinHandle<Result<ExtraDeck, OverlapBlocker>>>,
    gate: Arc<EventGate>,
    emit: EventSink,
    audio: crate::config::MpvAudioRuntimeConfig,
    intentional_close: Arc<AtomicBool>,
    file_generation_rx: watch::Receiver<u64>,
    pending_overlap: Option<PendingOverlap>,
}

pub(super) async fn run_conductor(input: ConductorInput) {
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
                    conductor.apply_extra_proof(ExtraProof::Failed).await;
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
    async fn handle_command(&mut self, cmd: PlayerCmd) -> bool {
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
        if !forward(
            &incoming_tx,
            PlayerCmd::Load(load.clone().with_handoff(TrackHandoff::Cut)),
        )
        .await
        {
            return false;
        }
        if !self.extra_is_lead {
            self.extra_has_file = false;
        }
        let generation = self.gate.admitted.load(Ordering::Acquire);
        self.gate.arm_pending(!self.extra_is_lead, generation);
        self.pending_overlap = Some(PendingOverlap {
            dest: load,
            fade: length,
            deadline: Instant::now() + OVERLAP_PROOF_TIMEOUT,
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

    async fn apply_extra_proof(&mut self, proof: ExtraProof) {
        let Some(pending) = self.pending_overlap.take() else {
            if matches!(proof, ExtraProof::Ready) {
                self.extra_has_file = true;
            }
            return;
        };
        self.gate.clear_pending();
        match proof {
            ExtraProof::Ready => {
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
            ExtraProof::Failed => {
                if !self.extra_is_lead {
                    self.extra_has_file = false;
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

    fn set_extra_is_lead(&mut self, extra_is_lead: bool) {
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

    fn warm_extra(&mut self) {
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

    async fn ensure_extra(&mut self) -> Result<(), OverlapBlocker> {
        if self.extra.is_some() {
            return Ok(());
        }
        if let Some(task) = self.warming.take()
            && let Ok(Ok(deck)) = task.await
        {
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

fn scaled_volume(user: i64, gain: f64) -> i64 {
    ((user as f64) * gain.clamp(0.0, 1.0)).round() as i64
}

fn is_pause_broadcast(cmd: &PlayerCmd) -> bool {
    matches!(cmd, PlayerCmd::CyclePause)
        || matches!(cmd, PlayerCmd::SetProperty { name, .. } if name == "pause")
}

async fn spawn_extra(
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
        super::SharedLongFormSeekStatus::isolated(cache_runtime.status()),
    ));
    let extra_sink = gate.sink(true, Arc::clone(owner_emit));
    tokio::spawn(ipc::run_actor(ipc::ActorInput {
        conn,
        cmd_rx: rx,
        emit: extra_sink,
        intentional_close: Arc::clone(intentional_close),
        file_generation_rx,
        route_provider: crate::playback_target::PlaybackRouteProviderHandle::disabled(),
        route_revocations: Arc::new(super::RouteRevocationRegistry::default()),
        cache_runtime,
        cache_status,
    }));
    Ok(ExtraDeck {
        tx,
        _mpv: Some(extra_mpv),
    })
}

async fn forward(tx: &Sender<PlayerCmd>, cmd: PlayerCmd) -> bool {
    tx.send(cmd).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::long_form_seek::CacheReason;
    use std::sync::Mutex;

    fn collecting_sink() -> (EventSink, Arc<Mutex<Vec<PlayerEvent>>>) {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::clone(&collected);
        let sink: EventSink = Arc::new(move |event| {
            events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        });
        (sink, collected)
    }

    fn take(collected: &Arc<Mutex<Vec<PlayerEvent>>>) -> Vec<PlayerEvent> {
        collected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    #[test]
    fn envelope_volume_is_silent_at_zero_gain_and_full_at_one() {
        assert_eq!(scaled_volume(80, 0.0), 0);
        assert_eq!(scaled_volume(80, 1.0), 80);
        assert_eq!(scaled_volume(80, 0.5), 40);
    }

    #[test]
    fn event_gate_drops_non_lead_time_pos_and_eof() {
        let gate = EventGate::new(Arc::new(AtomicU64::new(4)));
        let (sink, collected) = collecting_sink();

        gate.emit(
            true,
            PlayerEvent::file_scoped(1, PlayerEvent::TimePos(12.0)),
            &sink,
        );
        gate.emit(true, PlayerEvent::file_scoped(1, PlayerEvent::Eof), &sink);
        assert!(take(&collected).is_empty());

        gate.emit(
            false,
            PlayerEvent::file_scoped(4, PlayerEvent::TimePos(12.0)),
            &sink,
        );
        let events = take(&collected);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            PlayerEvent::FileScoped {
                file_generation: 4,
                event
            } if matches!(event.as_ref(), PlayerEvent::TimePos(pos) if *pos == 12.0)
        ));
    }

    #[test]
    fn event_gate_rewrites_extra_lead_file_scoped_to_admitted_generation() {
        let admitted = Arc::new(AtomicU64::new(9));
        let gate = EventGate::new(Arc::clone(&admitted));
        gate.extra_is_lead.store(true, Ordering::Release);
        let (sink, collected) = collecting_sink();

        gate.emit(
            true,
            PlayerEvent::file_scoped(1, PlayerEvent::TimePos(3.5)),
            &sink,
        );
        let events = take(&collected);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            PlayerEvent::FileScoped {
                file_generation: 9,
                event
            } if matches!(event.as_ref(), PlayerEvent::TimePos(pos) if *pos == 3.5)
        ));
    }

    #[test]
    fn event_gate_drops_volume_while_fading() {
        let gate = EventGate::new(Arc::new(AtomicU64::new(2)));
        gate.fading.store(true, Ordering::Release);
        let (sink, collected) = collecting_sink();

        gate.emit(false, PlayerEvent::Volume(40.0), &sink);
        assert!(take(&collected).is_empty());

        gate.fading.store(false, Ordering::Release);
        gate.emit(false, PlayerEvent::Volume(40.0), &sink);
        let events = take(&collected);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], PlayerEvent::Volume(v) if *v == 40.0));
    }

    fn overlap_load(url: &str) -> PlayerCmd {
        PlayerCmd::Load(
            crate::player::PlaybackLoad::new(url, crate::player::MediaSourceContext::OnDemand)
                .with_handoff(TrackHandoff::Overlap {
                    fade: FadeLength::from_tenths(10).expect("test fade"),
                }),
        )
    }

    fn cut_load(url: &str) -> PlayerCmd {
        PlayerCmd::Load(crate::player::PlaybackLoad::new(
            url,
            crate::player::MediaSourceContext::OnDemand,
        ))
    }

    fn take_cmds(rx: &mut Receiver<PlayerCmd>) -> Vec<PlayerCmd> {
        let mut cmds = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            cmds.push(cmd);
        }
        cmds
    }

    fn load_url(cmd: &PlayerCmd) -> Option<&str> {
        match cmd {
            PlayerCmd::Load(load) => Some(load.as_str()),
            _ => None,
        }
    }

    fn load_is_cut(cmd: &PlayerCmd) -> bool {
        matches!(
            cmd,
            PlayerCmd::Load(load) if matches!(load.handoff(), TrackHandoff::Cut)
        )
    }

    struct Harness {
        conductor: Conductor,
        primary_rx: Receiver<PlayerCmd>,
        extra_rx: Receiver<PlayerCmd>,
        gate: Arc<EventGate>,
    }

    fn harness() -> Harness {
        let (primary_tx, primary_rx) =
            crate::util::backpressure::bounded_channel(crate::util::backpressure::PLAYER_CMD_QUEUE);
        let (extra_tx, extra_rx) =
            crate::util::backpressure::bounded_channel(crate::util::backpressure::PLAYER_CMD_QUEUE);
        let (proof_tx, _proof_rx) = tokio::sync::mpsc::channel(8);
        let gate = EventGate::with_proof(Arc::new(AtomicU64::new(4)), Some(proof_tx));
        let (sink, _) = collecting_sink();
        let (_fg_tx, fg_rx) = watch::channel(0);
        let conductor = Conductor {
            primary_tx,
            extra: Some(ExtraDeck {
                tx: extra_tx,
                _mpv: None,
            }),
            extra_is_lead: false,
            extra_has_file: false,
            fade: None,
            volume: 100,
            next_deck_generation: 1,
            warming: None,
            gate: Arc::clone(&gate),
            emit: sink,
            audio: crate::config::MpvAudioRuntimeConfig::default(),
            intentional_close: Arc::new(AtomicBool::new(false)),
            file_generation_rx: fg_rx,
            pending_overlap: None,
        };
        Harness {
            conductor,
            primary_rx,
            extra_rx,
            gate,
        }
    }

    #[tokio::test]
    async fn overlap_does_not_lead_before_file_loaded() {
        let mut h = harness();
        assert!(
            h.conductor
                .handle_command(overlap_load("/music/b.flac"))
                .await
        );
        assert!(!h.conductor.extra_is_lead);
        assert!(!h.gate.extra_is_lead.load(Ordering::Acquire));
        assert!(h.conductor.pending_overlap.is_some());
        let extra = take_cmds(&mut h.extra_rx);
        assert!(
            extra
                .iter()
                .any(|cmd| load_url(cmd) == Some("/music/b.flac") && load_is_cut(cmd)),
            "incoming extra receives the destination as Cut"
        );
        assert!(take_cmds(&mut h.primary_rx).is_empty());

        h.conductor.apply_extra_proof(ExtraProof::Ready).await;
        assert!(h.conductor.extra_is_lead);
        assert!(h.gate.extra_is_lead.load(Ordering::Acquire));
        assert!(h.conductor.extra_has_file);
        assert!(h.conductor.pending_overlap.is_none());
    }

    #[tokio::test]
    async fn failed_extra_keeps_primary_and_cuts_destination() {
        let mut h = harness();
        assert!(
            h.conductor
                .handle_command(overlap_load("/music/b.flac"))
                .await
        );
        let _ = take_cmds(&mut h.extra_rx);
        h.conductor.apply_extra_proof(ExtraProof::Failed).await;
        assert!(!h.conductor.extra_is_lead);
        assert!(!h.gate.extra_is_lead.load(Ordering::Acquire));
        assert!(!h.conductor.extra_has_file);
        assert!(h.conductor.pending_overlap.is_none());
        let primary = take_cmds(&mut h.primary_rx);
        assert!(
            primary
                .iter()
                .any(|cmd| load_url(cmd) == Some("/music/b.flac") && load_is_cut(cmd)),
            "failed extra Cut-falls back onto primary"
        );
    }

    #[tokio::test]
    async fn cut_recovers_stuck_extra_lead() {
        let mut h = harness();
        h.conductor.set_extra_is_lead(true);
        h.conductor.extra_has_file = false;
        assert!(h.conductor.handle_command(cut_load("/music/c.flac")).await);
        assert!(!h.conductor.extra_is_lead);
        assert!(!h.gate.extra_is_lead.load(Ordering::Acquire));
        let extra = take_cmds(&mut h.extra_rx);
        assert!(
            extra.iter().any(|cmd| matches!(cmd, PlayerCmd::Stop)),
            "stuck extra is stopped"
        );
        let primary = take_cmds(&mut h.primary_rx);
        assert!(
            primary
                .iter()
                .any(|cmd| load_url(cmd) == Some("/music/c.flac") && load_is_cut(cmd)),
            "Cut recovers onto primary"
        );
    }

    #[test]
    fn event_gate_admits_pending_extra_duration_before_lead_flip() {
        let (proof_tx, mut proof_rx) = tokio::sync::mpsc::channel(8);
        let gate = EventGate::with_proof(Arc::new(AtomicU64::new(4)), Some(proof_tx));
        gate.arm_pending(true, 4);
        let (sink, collected) = collecting_sink();

        gate.emit(
            true,
            PlayerEvent::file_scoped(4, PlayerEvent::Duration(Some(10.0))),
            &sink,
        );
        gate.emit(
            true,
            PlayerEvent::file_scoped(4, PlayerEvent::TimePos(0.2)),
            &sink,
        );

        assert!(!gate.extra_is_lead.load(Ordering::Acquire));
        let events = take(&collected);
        assert!(
            events.iter().any(|event| matches!(
                event,
                PlayerEvent::FileScoped {
                    file_generation: 4,
                    event
                } if matches!(event.as_ref(), PlayerEvent::Duration(Some(duration)) if *duration == 10.0)
            )),
            "overlap must surface extra Duration before lead flip, not leave --:--"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::FileScoped {
                file_generation: 4,
                event
            } if matches!(event.as_ref(), PlayerEvent::TimePos(pos) if *pos == 0.2)
        )));
        assert!(matches!(proof_rx.try_recv(), Ok(ExtraProof::Ready)));
    }

    #[test]
    fn event_gate_drops_outgoing_time_pos_and_admits_incoming_primary_during_pending() {
        let gate = EventGate::new(Arc::new(AtomicU64::new(5)));
        gate.extra_is_lead.store(true, Ordering::Release);
        gate.arm_pending(false, 5);
        let (sink, collected) = collecting_sink();

        gate.emit(
            true,
            PlayerEvent::file_scoped(4, PlayerEvent::TimePos(9.0)),
            &sink,
        );
        assert!(
            take(&collected).is_empty(),
            "outgoing extra TimePos must not flash onto the incoming generation"
        );

        gate.emit(
            false,
            PlayerEvent::file_scoped(5, PlayerEvent::Duration(Some(10.0))),
            &sink,
        );
        gate.emit(
            false,
            PlayerEvent::file_scoped(5, PlayerEvent::TimePos(0.1)),
            &sink,
        );
        let events = take(&collected);
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::FileScoped {
                file_generation: 5,
                event
            } if matches!(event.as_ref(), PlayerEvent::Duration(Some(duration)) if *duration == 10.0)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            PlayerEvent::FileScoped {
                file_generation: 5,
                event
            } if matches!(event.as_ref(), PlayerEvent::TimePos(pos) if *pos == 0.1)
        )));
    }

    #[tokio::test]
    async fn retire_extra_keeps_playing_extra_lead_until_next_cut() {
        let mut h = harness();
        h.conductor.set_extra_is_lead(true);
        h.conductor.extra_has_file = true;
        assert!(h.conductor.handle_command(PlayerCmd::RetireExtra).await);
        assert!(
            h.conductor.extra_is_lead,
            "Off after a completed overlap must keep extra as lead"
        );
        assert!(
            h.gate.extra_is_lead.load(Ordering::Acquire),
            "event gate must keep extra as lead so TimePos still flows"
        );
        assert!(
            h.conductor.extra.is_some(),
            "Off must not drop the deck that still owns the current file"
        );
        let extra = take_cmds(&mut h.extra_rx);
        assert!(
            extra.iter().all(|cmd| !matches!(cmd, PlayerCmd::Stop)),
            "Off mid-track must not Stop the extra lead"
        );
        assert!(take_cmds(&mut h.primary_rx).is_empty());

        assert!(h.conductor.handle_command(cut_load("/music/c.flac")).await);
        assert!(!h.conductor.extra_is_lead);
        let extra = take_cmds(&mut h.extra_rx);
        assert!(
            extra.iter().any(|cmd| matches!(cmd, PlayerCmd::Stop)),
            "next Cut stops the retired extra lead"
        );
        let primary = take_cmds(&mut h.primary_rx);
        assert!(
            primary
                .iter()
                .any(|cmd| load_url(cmd) == Some("/music/c.flac") && load_is_cut(cmd)),
            "Off then next Cut must play on primary"
        );
    }

    #[tokio::test]
    async fn retire_extra_then_cut_plays_on_primary() {
        let mut h = harness();
        h.conductor.set_extra_is_lead(true);
        h.conductor.extra_has_file = true;
        h.conductor.pending_overlap = Some(PendingOverlap {
            dest: crate::player::PlaybackLoad::new(
                "/music/b.flac",
                crate::player::MediaSourceContext::OnDemand,
            ),
            fade: FadeLength::from_tenths(10).expect("test fade"),
            deadline: Instant::now() + OVERLAP_PROOF_TIMEOUT,
        });
        assert!(h.conductor.handle_command(PlayerCmd::RetireExtra).await);
        assert!(h.conductor.extra_is_lead);
        assert!(h.gate.extra_is_lead.load(Ordering::Acquire));
        assert!(!h.conductor.extra_has_file);
        assert!(h.conductor.pending_overlap.is_none());
        assert!(h.conductor.extra.is_some());
        assert!(h.conductor.warming.is_none());
        let extra = take_cmds(&mut h.extra_rx);
        assert!(
            extra.iter().all(|cmd| !matches!(cmd, PlayerCmd::Stop)),
            "Off must not Stop the extra lead while it still owns the current file"
        );
        let primary = take_cmds(&mut h.primary_rx);
        assert!(
            primary.iter().any(|cmd| matches!(cmd, PlayerCmd::Stop)),
            "Off cancels the pending incoming load on primary"
        );
        assert_eq!(h.gate.pending_generation.load(Ordering::Acquire), 0);
        assert!(!h.gate.fading.load(Ordering::Acquire));

        h.conductor.warming = Some(tokio::spawn(async { Err(OverlapBlocker::Mpv) }));
        assert!(h.conductor.handle_command(cut_load("/music/c.flac")).await);
        assert!(!h.conductor.extra_is_lead);
        let extra = take_cmds(&mut h.extra_rx);
        assert!(extra.iter().any(|cmd| matches!(cmd, PlayerCmd::Stop)));
        let primary = take_cmds(&mut h.primary_rx);
        assert!(
            primary
                .iter()
                .any(|cmd| load_url(cmd) == Some("/music/c.flac") && load_is_cut(cmd)),
            "Off → RetireExtra must return subsequent Cut to primary"
        );
    }

    #[tokio::test]
    async fn cut_recovers_when_extra_lead_but_deck_already_gone() {
        let mut h = harness();
        h.conductor.set_extra_is_lead(true);
        h.conductor.extra_has_file = true;
        h.conductor.extra = None;
        h.conductor.warming = Some(tokio::spawn(async { Err(OverlapBlocker::Mpv) }));
        assert!(h.conductor.handle_command(cut_load("/music/d.flac")).await);
        assert!(!h.conductor.extra_is_lead);
        assert!(!h.gate.extra_is_lead.load(Ordering::Acquire));
        let primary = take_cmds(&mut h.primary_rx);
        assert!(
            primary
                .iter()
                .any(|cmd| load_url(cmd) == Some("/music/d.flac") && load_is_cut(cmd)),
            "sticky extra lead with no deck must Cut on primary"
        );
    }

    #[test]
    fn event_gate_forwards_cache_emergencies_from_the_non_lead_deck() {
        let gate = EventGate::new(Arc::new(AtomicU64::new(2)));
        let (sink, collected) = collecting_sink();

        gate.emit(
            true,
            PlayerEvent::CacheEmergency {
                file_generation: 1,
                position_secs: 8.0,
                paused: false,
                reason: CacheReason::DisableFailed,
            },
            &sink,
        );
        gate.emit(
            true,
            PlayerEvent::CacheReplacementEmergency {
                reason: CacheReason::PropertyTimeout,
            },
            &sink,
        );
        let events = take(&collected);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            PlayerEvent::CacheEmergency {
                file_generation: 1,
                position_secs,
                paused: false,
                reason: CacheReason::DisableFailed
            } if *position_secs == 8.0
        ));
        assert!(matches!(
            &events[1],
            PlayerEvent::CacheReplacementEmergency {
                reason: CacheReason::PropertyTimeout
            }
        ));
    }
}
