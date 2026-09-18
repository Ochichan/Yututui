use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::{EventSink, Mpv, PlayerCmd, PlayerEvent, cache_runtime, cache_support, ipc, mpv};
use crate::config::LongFormSeekOptimization;
use crate::crossfade::{FadeLength, OverlapBlocker, TrackHandoff, envelope};
use crate::util::backpressure;

const FADE_TICK: Duration = Duration::from_millis(25);

pub(super) struct EventGate {
    extra_is_lead: AtomicBool,
    fading: AtomicBool,
    admitted: Arc<AtomicU64>,
}

impl EventGate {
    pub(super) fn new(admitted: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            extra_is_lead: AtomicBool::new(false),
            fading: AtomicBool::new(false),
            admitted,
        })
    }

    pub(super) fn sink(self: &Arc<Self>, from_extra: bool, emit: EventSink) -> EventSink {
        let gate = Arc::clone(self);
        Arc::new(move |event| gate.emit(from_extra, event, &emit))
    }

    fn emit(&self, from_extra: bool, event: PlayerEvent, sink: &EventSink) {
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
    _mpv: Mpv,
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
}

struct Conductor {
    primary_tx: Sender<PlayerCmd>,
    extra: Option<ExtraDeck>,
    extra_is_lead: bool,
    fade: Option<Fade>,
    volume: i64,
    next_deck_generation: u64,
    warming: Option<JoinHandle<Result<ExtraDeck, OverlapBlocker>>>,
    gate: Arc<EventGate>,
    emit: EventSink,
    audio: crate::config::MpvAudioRuntimeConfig,
    intentional_close: Arc<AtomicBool>,
    file_generation_rx: watch::Receiver<u64>,
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
    } = input;

    let mut conductor = Conductor {
        primary_tx: lead_tx,
        extra: None,
        extra_is_lead: false,
        fade: None,
        volume: 100,
        next_deck_generation: 1,
        warming: None,
        gate,
        emit,
        audio,
        intentional_close,
        file_generation_rx,
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
            _ = tick.tick(), if conductor.fade.is_some() => {
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
                TrackHandoff::Overlap { fade: length } => {
                    if let Err(blocker) = self.ensure_extra().await {
                        (self.emit)(PlayerEvent::OverlapUnavailable(blocker));
                        let cut = PlayerCmd::Load(load.with_handoff(TrackHandoff::Cut));
                        return self.forward_lead(cut).await;
                    }
                    let Some(incoming_tx) = self.incoming_tx() else {
                        (self.emit)(PlayerEvent::OverlapUnavailable(OverlapBlocker::Mpv));
                        let cut = PlayerCmd::Load(load.with_handoff(TrackHandoff::Cut));
                        return self.forward_lead(cut).await;
                    };
                    self.abort_current_fade().await;
                    self.gate.fading.store(true, Ordering::Release);
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
                        PlayerCmd::Load(load.with_handoff(TrackHandoff::Cut)),
                    )
                    .await
                    {
                        return false;
                    }
                    self.extra_is_lead = !self.extra_is_lead;
                    self.gate
                        .extra_is_lead
                        .store(self.extra_is_lead, Ordering::Release);
                    self.fade = Some(Fade {
                        length,
                        started: Instant::now(),
                    });
                    apply_volumes(
                        self.volume,
                        self.extra.as_ref(),
                        self.extra_is_lead,
                        &self.primary_tx,
                        Some((Duration::ZERO, length)),
                    )
                    .await;
                    true
                }
                TrackHandoff::Cut => {
                    self.abort_current_fade().await;
                    self.warm_extra();
                    self.forward_lead(PlayerCmd::Load(load)).await
                }
            },
            PlayerCmd::LoadWithResume(_) | PlayerCmd::Stop => {
                self.abort_current_fade().await;
                self.forward_lead(cmd).await
            }
            PlayerCmd::SeekRelative(_) | PlayerCmd::SeekAbsolute { .. } => {
                self.abort_current_fade().await;
                self.forward_lead(cmd).await
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
        _mpv: extra_mpv,
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
