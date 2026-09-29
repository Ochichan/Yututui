use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use tokio::sync::mpsc::Receiver;
use tokio::sync::watch;

use super::super::conductor::{Conductor, DeckSettings, ExtraDeck};
use super::super::gate::EventGate;
use super::super::proof::ExtraProof;
use crate::crossfade::{FadeLength, OverlapBlocker, TrackHandoff};
use crate::player::{EventSink, MediaSourceContext, PlaybackLoad, PlayerCmd, PlayerEvent};

struct BlackBoxHarness {
    conductor: Conductor,
    gate: Arc<EventGate>,
    owner_sink: EventSink,
    owner_events: Arc<Mutex<Vec<PlayerEvent>>>,
    proof_rx: Receiver<ExtraProof>,
    primary_rx: Option<Receiver<PlayerCmd>>,
    extra_rx: Option<Receiver<PlayerCmd>>,
}

impl BlackBoxHarness {
    fn new() -> Self {
        Self::with_owner_generation(4)
    }

    fn with_owner_generation(owner_generation: u64) -> Self {
        let (primary_tx, primary_rx) =
            crate::util::backpressure::bounded_channel(crate::util::backpressure::PLAYER_CMD_QUEUE);
        let (extra_tx, extra_rx) =
            crate::util::backpressure::bounded_channel(crate::util::backpressure::PLAYER_CMD_QUEUE);
        let (proof_tx, proof_rx) = tokio::sync::mpsc::channel(8);
        let owner_generation = Arc::new(AtomicU64::new(owner_generation));
        let gate = EventGate::with_proof(Arc::clone(&owner_generation), Some(proof_tx));
        let owner_events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&owner_events);
        let sink_generation = Arc::clone(&owner_generation);
        let owner_sink: EventSink = Arc::new(move |event| {
            if event
                .file_generation()
                .is_none_or(|generation| generation == sink_generation.load(Ordering::Acquire))
            {
                sink_events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event);
            }
        });
        let (_file_generation_tx, file_generation_rx) = watch::channel(0);
        let conductor = Conductor {
            primary_tx,
            primary_available: true,
            extra: Some(ExtraDeck {
                tx: extra_tx,
                _mpv: None,
            }),
            extra_is_lead: false,
            extra_has_file: false,
            fade: None,
            volume: 100,
            settings: DeckSettings::with_volume(100),
            next_deck_generation: 1,
            warming: None,
            gate: Arc::clone(&gate),
            emit: Arc::clone(&owner_sink),
            audio: crate::config::MpvAudioRuntimeConfig::default(),
            overlap_enabled: true,
            cookies_file: None,
            standby_cache_args: Vec::new(),
            intentional_close: Arc::new(AtomicBool::new(false)),
            file_generation_rx,
            pending_overlap: None,
            standby_spawn: Some(|| Err(OverlapBlocker::Mpv)),
        };
        Self {
            conductor,
            gate,
            owner_sink,
            owner_events,
            proof_rx,
            primary_rx: Some(primary_rx),
            extra_rx: Some(extra_rx),
        }
    }

    async fn handle_command(&mut self, command: PlayerCmd) -> bool {
        self.conductor.handle_command(command).await
    }

    fn emit(&self, from_extra: bool, event: PlayerEvent) {
        self.gate.emit(from_extra, event, &self.owner_sink);
    }

    async fn apply_extra_proofs(&mut self) {
        while let Ok(proof) = self.proof_rx.try_recv() {
            assert!(self.apply_extra_proof(proof).await);
        }
    }

    async fn apply_extra_proof(&mut self, proof: ExtraProof) -> bool {
        self.conductor.apply_extra_proof(proof).await
    }

    fn primary_commands(&mut self) -> Vec<PlayerCmd> {
        take_commands(
            self.primary_rx
                .as_mut()
                .expect("primary receiver remains open"),
        )
    }

    fn extra_commands(&mut self) -> Vec<PlayerCmd> {
        take_commands(self.extra_rx.as_mut().expect("extra receiver remains open"))
    }

    fn close_primary(&mut self) {
        drop(self.primary_rx.take());
    }

    fn close_extra(&mut self) {
        drop(self.extra_rx.take());
    }

    fn take_owner_events(&self) -> Vec<PlayerEvent> {
        self.owner_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }

    fn clear_owner_events(&self) {
        self.owner_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    async fn complete_overlap(&mut self, url: &str, generation: u64) {
        assert!(
            self.handle_command(overlap_load(url, generation)).await,
            "the incoming deck must accept the overlap load"
        );
        let _ = self.extra_commands();
        self.emit(
            true,
            PlayerEvent::file_scoped(generation, PlayerEvent::TimePos(0.1)),
        );
        self.apply_extra_proofs().await;
        let _ = self.primary_commands();
        let _ = self.extra_commands();
        self.clear_owner_events();
    }
}

fn take_commands(receiver: &mut Receiver<PlayerCmd>) -> Vec<PlayerCmd> {
    let mut commands = Vec::new();
    while let Ok(command) = receiver.try_recv() {
        commands.push(command);
    }
    commands
}

fn overlap_load(url: &str, generation: u64) -> PlayerCmd {
    PlayerCmd::Load(
        PlaybackLoad::new(url, MediaSourceContext::OnDemand)
            .with_handoff(TrackHandoff::Overlap {
                fade: FadeLength::from_tenths(10).expect("test fade"),
            })
            .with_reserved_file_generation(generation),
    )
}

fn cut_load(url: &str, generation: u64) -> PlayerCmd {
    PlayerCmd::Load(
        PlaybackLoad::new(url, MediaSourceContext::OnDemand)
            .with_reserved_file_generation(generation),
    )
}

fn pause(paused: bool) -> PlayerCmd {
    PlayerCmd::SetProperty {
        name: "pause".to_owned(),
        value: serde_json::Value::Bool(paused),
    }
}

fn audio_filter(value: &str) -> PlayerCmd {
    PlayerCmd::SetAudioFilter(value.to_owned())
}

fn af_command(label: &str, param: &str, value: &str) -> PlayerCmd {
    PlayerCmd::AfCommand {
        label: label.to_owned(),
        param: param.to_owned(),
        value: value.to_owned(),
    }
}

fn property(name: &str, value: serde_json::Value) -> PlayerCmd {
    PlayerCmd::SetProperty {
        name: name.to_owned(),
        value,
    }
}

fn has_cut_load(commands: &[PlayerCmd], url: &str) -> bool {
    commands.iter().any(|command| {
        matches!(
            command,
            PlayerCmd::Load(load)
                if load.as_str() == url && matches!(load.handoff(), TrackHandoff::Cut)
        )
    })
}

fn has_stop(commands: &[PlayerCmd]) -> bool {
    commands
        .iter()
        .any(|command| matches!(command, PlayerCmd::Stop))
}

fn has_pause(commands: &[PlayerCmd], paused: bool) -> bool {
    commands.iter().any(|command| {
        matches!(
            command,
            PlayerCmd::SetProperty { name, value }
                if name == "pause" && value == &serde_json::Value::Bool(paused)
        )
    })
}

fn same_setting(actual: &PlayerCmd, expected: &PlayerCmd) -> bool {
    match (actual, expected) {
        (PlayerCmd::SetAudioFilter(actual), PlayerCmd::SetAudioFilter(expected)) => {
            actual == expected
        }
        (
            PlayerCmd::AfCommand {
                label: actual_label,
                param: actual_param,
                value: actual_value,
            },
            PlayerCmd::AfCommand {
                label: expected_label,
                param: expected_param,
                value: expected_value,
            },
        ) => {
            actual_label == expected_label
                && actual_param == expected_param
                && actual_value == expected_value
        }
        (
            PlayerCmd::SetProperty {
                name: actual_name,
                value: actual_value,
            },
            PlayerCmd::SetProperty {
                name: expected_name,
                value: expected_value,
            },
        ) => actual_name == expected_name && actual_value == expected_value,
        (PlayerCmd::SetVolume(actual), PlayerCmd::SetVolume(expected)) => actual == expected,
        (
            PlayerCmd::ReplayAudioDevice { device: actual },
            PlayerCmd::ReplayAudioDevice { device: expected },
        ) => actual == expected,
        _ => false,
    }
}

fn has_overlap_unavailable(events: &[PlayerEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, PlayerEvent::OverlapUnavailable(_)))
}

#[tokio::test]
async fn completed_overlap_then_cut_loads_primary_and_stops_extra() {
    let mut harness = BlackBoxHarness::new();
    harness.complete_overlap("/music/b.flac", 4).await;

    assert!(harness.handle_command(cut_load("/music/c.flac", 5)).await);

    assert!(
        has_cut_load(&harness.primary_commands(), "/music/c.flac"),
        "a Cut after extra leads must reload the primary deck"
    );
    assert!(
        has_stop(&harness.extra_commands()),
        "a Cut after extra leads must stop the old standby deck"
    );
}

#[tokio::test]
async fn completed_overlap_then_resume_loads_primary_and_stops_extra() {
    let mut harness = BlackBoxHarness::new();
    harness.complete_overlap("/music/b.flac", 4).await;

    let resume = crate::player::recovery::LoadWithResume::emergency(
        "/music/c.flac",
        12.0,
        false,
        MediaSourceContext::OnDemand,
    );
    assert!(
        harness
            .handle_command(PlayerCmd::LoadWithResume(resume))
            .await
    );

    assert!(
        harness
            .primary_commands()
            .iter()
            .any(|command| matches!(command, PlayerCmd::LoadWithResume(_))),
        "a resume load after extra leads must reload the primary deck"
    );
    assert!(
        has_stop(&harness.extra_commands()),
        "a resume load after extra leads must stop the old standby deck"
    );
}

#[tokio::test]
async fn idle_standby_close_then_overlap_falls_back_to_primary() {
    let mut harness = BlackBoxHarness::new();
    harness.emit(
        true,
        PlayerEvent::TransportClosed("standby closed while idle".to_owned()),
    );
    harness.apply_extra_proofs().await;
    harness.emit(false, PlayerEvent::file_scoped(4, PlayerEvent::Eof));

    assert!(
        harness
            .handle_command(overlap_load("/music/b.flac", 5))
            .await
    );

    assert!(
        has_cut_load(&harness.primary_commands(), "/music/b.flac"),
        "a dead standby must make the next overlap fall back to a primary Cut"
    );
    assert!(
        has_overlap_unavailable(&harness.take_owner_events()),
        "the owner must learn that overlap is unavailable"
    );
}

#[tokio::test]
async fn non_lead_forward_failure_falls_back_without_exiting() {
    let mut harness = BlackBoxHarness::new();
    harness.close_extra();

    assert!(
        harness
            .handle_command(overlap_load("/music/b.flac", 5))
            .await
    );

    assert!(
        has_cut_load(&harness.primary_commands(), "/music/b.flac"),
        "a failed standby send must fall back to a primary Cut"
    );
    assert!(
        has_overlap_unavailable(&harness.take_owner_events()),
        "a failed non-lead send must be surfaced as overlap unavailable"
    );
}

#[tokio::test]
async fn primary_close_while_extra_leads_then_overlap_falls_back_to_extra() {
    let mut harness = BlackBoxHarness::new();
    harness.complete_overlap("/music/b.flac", 4).await;
    harness.close_primary();
    harness.emit(
        false,
        PlayerEvent::TransportClosed("primary closed while standby leads".to_owned()),
    );
    harness.apply_extra_proofs().await;

    assert!(
        harness
            .handle_command(overlap_load("/music/c.flac", 5))
            .await
    );

    assert!(
        has_cut_load(&harness.extra_commands(), "/music/c.flac"),
        "a dead non-lead primary must make the next overlap Cut on the extra lead"
    );
    assert!(
        has_overlap_unavailable(&harness.take_owner_events()),
        "the owner must learn that no incoming deck remains"
    );
}

#[tokio::test]
async fn retiring_extra_with_a_ready_proof_queued_cuts_the_committed_destination() {
    let mut harness = BlackBoxHarness::with_owner_generation(5);
    assert!(
        harness
            .handle_command(overlap_load("/music/b.flac", 5))
            .await
    );
    let _ = harness.extra_commands();
    harness.emit(true, PlayerEvent::file_scoped(5, PlayerEvent::TimePos(0.1)));
    assert!(matches!(
        harness.take_owner_events().as_slice(),
        [PlayerEvent::FileScoped {
            file_generation: 5,
            event
        }] if matches!(event.as_ref(), PlayerEvent::TimePos(position) if *position == 0.1)
    ));

    assert!(harness.handle_command(PlayerCmd::SetOverlap(false)).await);
    harness.apply_extra_proofs().await;

    assert!(
        has_cut_load(&harness.primary_commands(), "/music/b.flac"),
        "retiring pending overlap must start a fresh primary Cut for its committed destination"
    );
    assert!(
        has_stop(&harness.extra_commands()),
        "retiring pending overlap must stop the incoming standby"
    );
}

#[tokio::test]
async fn pause_during_pending_overlap_reaches_the_incoming_deck() {
    let mut harness = BlackBoxHarness::new();
    assert!(
        harness
            .handle_command(overlap_load("/music/b.flac", 4))
            .await
    );
    let _ = harness.extra_commands();

    assert!(harness.handle_command(pause(true)).await);

    assert!(has_pause(&harness.primary_commands(), true));
    assert!(
        has_pause(&harness.extra_commands(), true),
        "the incoming deck must receive pause before it can become lead"
    );
}

#[tokio::test]
async fn pause_during_fade_reaches_both_decks() {
    let mut harness = BlackBoxHarness::new();
    harness.complete_overlap("/music/b.flac", 4).await;

    assert!(harness.handle_command(pause(true)).await);

    assert!(has_pause(&harness.primary_commands(), true));
    assert!(has_pause(&harness.extra_commands(), true));
}

#[tokio::test]
async fn pending_overlap_applies_track_filter_and_unpause_to_the_incoming_deck() {
    let mut harness = BlackBoxHarness::new();
    assert!(
        harness
            .handle_command(overlap_load("/music/b.flac", 4))
            .await
    );
    let _ = harness.extra_commands();

    assert!(
        harness
            .handle_command(audio_filter("lavfi=[volume=1]"))
            .await
    );
    assert!(harness.handle_command(pause(false)).await);

    let incoming = harness.extra_commands();
    assert!(
        incoming.iter().any(
            |command| matches!(command, PlayerCmd::SetAudioFilter(filter) if filter == "lavfi=[volume=1]")
        ),
        "the incoming deck must receive the track filter while the overlap is pending"
    );
    assert!(
        has_pause(&incoming, false),
        "the incoming deck must receive the track unpause while the overlap is pending"
    );
}

#[tokio::test]
async fn overlap_replays_deck_settings_before_loading_the_incoming_deck() {
    let mut harness = BlackBoxHarness::new();
    assert!(
        harness
            .handle_command(audio_filter("@norm:lavfi=[dynaudnorm]"))
            .await
    );
    assert!(
        harness
            .handle_command(af_command("norm", "framelen", "250"))
            .await
    );
    assert!(
        harness
            .handle_command(property("speed", serde_json::Value::from(1.2)))
            .await
    );
    assert!(
        harness
            .handle_command(property("replaygain-preamp", serde_json::Value::from(3.0),))
            .await
    );
    assert!(harness.handle_command(PlayerCmd::SetVolume(73)).await);
    assert!(harness.handle_command(pause(true)).await);
    assert!(
        harness
            .handle_command(PlayerCmd::SelectAudioDevice {
                correlation_id: 41,
                device: Some("pipewire/42".to_owned()),
            })
            .await
    );
    let _ = harness.primary_commands();

    assert!(
        harness
            .handle_command(overlap_load("/music/b.flac", 4))
            .await
    );
    let incoming = harness.extra_commands();
    let load_at = incoming
        .iter()
        .position(|command| has_cut_load(std::slice::from_ref(command), "/music/b.flac"))
        .expect("incoming overlap Load");

    for command in [
        PlayerCmd::SetAudioFilter("@norm:lavfi=[dynaudnorm]".to_owned()),
        af_command("norm", "framelen", "250"),
        property("speed", serde_json::Value::from(1.2)),
        property("replaygain-preamp", serde_json::Value::from(3.0)),
        PlayerCmd::SetVolume(73),
        pause(true),
        PlayerCmd::ReplayAudioDevice {
            device: Some("pipewire/42".to_owned()),
        },
    ] {
        let setting_at = incoming
            .iter()
            .position(|candidate| same_setting(candidate, &command))
            .expect("incoming deck missed a persisted setting");
        assert!(
            setting_at < load_at,
            "incoming setting must be replayed before its Load"
        );
    }
    assert!(
        !incoming.iter().any(|command| matches!(
            command,
            PlayerCmd::SelectAudioDevice {
                correlation_id: 41,
                ..
            }
        )),
        "the incoming replay must not reuse the owner's device-selection correlation"
    );
}

#[tokio::test]
async fn overlap_replays_a_known_cycle_pause_state() {
    let mut harness = BlackBoxHarness::new();
    assert!(harness.handle_command(pause(false)).await);
    assert!(harness.handle_command(PlayerCmd::CyclePause).await);
    let _ = harness.primary_commands();

    assert!(
        harness
            .handle_command(overlap_load("/music/b.flac", 4))
            .await
    );
    assert!(
        has_pause(&harness.extra_commands(), true),
        "a known CyclePause state must be replayed onto the incoming deck"
    );
}

#[tokio::test]
async fn overlap_never_replays_a_tracked_property() {
    let mut harness = BlackBoxHarness::new();
    let barrier = crate::util::command_barrier::CommandBarrier::pending();
    assert!(
        harness
            .handle_command(PlayerCmd::tracked_property(
                "stream-record".to_owned(),
                serde_json::Value::from("next.mkv"),
                &barrier,
            ))
            .await
    );
    let _ = harness.primary_commands();

    assert!(
        harness
            .handle_command(overlap_load("/music/b.flac", 4))
            .await
    );
    assert!(
        harness
            .extra_commands()
            .iter()
            .all(|command| !matches!(command, PlayerCmd::TrackedProperty(_))),
        "recorder barriers must never be replayed onto the incoming deck"
    );
}

#[tokio::test]
async fn proof_timeout_failure_stops_incoming_before_cut_fallback() {
    let mut harness = BlackBoxHarness::new();
    assert!(
        harness
            .handle_command(overlap_load("/music/b.flac", 4))
            .await
    );
    let _ = harness.extra_commands();
    assert!(
        harness
            .apply_extra_proof(ExtraProof::Failed { epoch: 1 })
            .await,
        "the timeout proof must keep the conductor alive on its primary lead"
    );

    assert!(
        has_stop(&harness.extra_commands()),
        "a failed proof must retire the incoming deck before the fallback Cut"
    );
    assert!(
        has_cut_load(&harness.primary_commands(), "/music/b.flac"),
        "a failed proof must still play the committed destination"
    );
}

#[tokio::test]
async fn lead_forward_failure_emits_transport_closed_before_conductor_exit() {
    let mut harness = BlackBoxHarness::new();
    harness.close_primary();

    assert!(
        !harness.handle_command(cut_load("/music/b.flac", 5)).await,
        "the conductor may exit only after its lead deck is gone"
    );
    assert!(
        harness.take_owner_events().iter().any(
            |event| matches!(event, PlayerEvent::TransportClosed(reason) if !reason.is_empty())
        ),
        "a conductor exit must leave the owner with a terminal reason"
    );
}

#[tokio::test]
async fn lead_volume_forward_failure_emits_transport_closed_before_conductor_exit() {
    let mut harness = BlackBoxHarness::new();
    harness.close_primary();

    assert!(
        !harness.handle_command(PlayerCmd::SetVolume(42)).await,
        "a failed volume forward to the lead must stop the conductor"
    );
    assert!(
        harness.take_owner_events().iter().any(
            |event| matches!(event, PlayerEvent::TransportClosed(reason) if !reason.is_empty())
        ),
        "a failed lead volume forward must give the owner a terminal reason"
    );
}
