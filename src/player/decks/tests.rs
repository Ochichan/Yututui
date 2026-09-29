use super::conductor::scaled_volume;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Instant;

use tokio::sync::mpsc::Receiver;
use tokio::sync::watch;

use super::conductor::{Conductor, DeckSettings, ExtraDeck, PendingOverlap};
use super::gate::EventGate;
use super::proof::{ExtraProof, OVERLAP_PROOF_TIMEOUT};
use crate::crossfade::{FadeLength, OverlapBlocker, TrackHandoff};
use crate::player::{Chapter, long_form_seek::CacheReason};
use crate::player::{EventSink, PlayerCmd, PlayerEvent};
use std::sync::Mutex;

mod black_box;

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
fn late_extra_lead_eof_stays_stale_for_the_owner_generation_filter() {
    let admitted = Arc::new(AtomicU64::new(9));
    let gate = EventGate::new(Arc::clone(&admitted));
    gate.extra_is_lead.store(true, Ordering::Release);
    let collected = Arc::new(Mutex::new(Vec::new()));
    let owner_events = Arc::clone(&collected);
    let owner_generation = Arc::clone(&admitted);
    let sink: EventSink = Arc::new(move |event| {
        if event
            .file_generation()
            .is_none_or(|generation| generation == owner_generation.load(Ordering::Acquire))
        {
            owner_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
    });

    gate.emit(true, PlayerEvent::file_scoped(1, PlayerEvent::Eof), &sink);
    assert!(
        take(&collected).is_empty(),
        "a previous extra-lead EOF must keep its generation so the owner rejects it after a skip"
    );
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
        emit: sink,
        audio: crate::config::MpvAudioRuntimeConfig::default(),
        overlap_enabled: true,
        cookies_file: None,
        standby_cache_args: Vec::new(),
        intentional_close: Arc::new(AtomicBool::new(false)),
        file_generation_rx: fg_rx,
        pending_overlap: None,
        standby_spawn: None,
    };
    Harness {
        conductor,
        primary_rx,
        extra_rx,
        gate,
    }
}

#[tokio::test]
async fn disabled_overlap_does_not_warm_a_standby_on_cut() {
    let mut h = harness();
    h.conductor.extra = None;
    h.conductor.standby_spawn = Some(|| Err(OverlapBlocker::Mpv));

    assert!(
        h.conductor
            .handle_command(PlayerCmd::SetOverlap(false))
            .await
    );
    assert!(h.conductor.handle_command(cut_load("/music/c.flac")).await);

    assert!(
        h.conductor.warming.is_none(),
        "a Cut with overlap disabled must not leave an idle standby process warming"
    );
}

#[tokio::test]
async fn enabling_overlap_warms_a_standby() {
    let mut h = harness();
    h.conductor.extra = None;
    h.conductor.standby_spawn = Some(|| Err(OverlapBlocker::Mpv));

    assert!(
        h.conductor
            .handle_command(PlayerCmd::SetOverlap(true))
            .await
    );

    assert!(
        h.conductor.warming.is_some(),
        "enabling overlap must prepare the standby before the next overlap window"
    );
}

#[tokio::test]
async fn overlap_forwards_reserved_ingress_generation_not_latest_admitted() {
    let mut h = harness();
    h.gate.admitted.store(9, Ordering::Release);
    let load = crate::player::PlaybackLoad::new(
        "/music/b.flac",
        crate::player::MediaSourceContext::OnDemand,
    )
    .with_handoff(TrackHandoff::Overlap {
        fade: FadeLength::from_tenths(10).expect("test fade"),
    })
    .with_reserved_file_generation(4);
    assert!(h.conductor.handle_command(PlayerCmd::Load(load)).await);
    let extra = take_cmds(&mut h.extra_rx);
    assert!(
        extra.iter().any(|cmd| matches!(
            cmd,
            PlayerCmd::Load(load)
                if load.as_str() == "/music/b.flac"
                    && load.reserved_file_generation() == Some(4)
        )),
        "overlap must forward the load's reserved generation, not gate.admitted"
    );
    assert_eq!(h.gate.pending_generation.load(Ordering::Acquire), 4);
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

    let epoch = h
        .conductor
        .pending_overlap
        .as_ref()
        .expect("overlap is pending")
        .epoch;
    h.conductor
        .apply_extra_proof(ExtraProof::Ready { epoch })
        .await;
    assert!(h.conductor.extra_is_lead);
    assert!(h.gate.extra_is_lead.load(Ordering::Acquire));
    assert!(h.conductor.extra_has_file);
    assert!(h.conductor.pending_overlap.is_none());
}

#[tokio::test]
async fn queued_ready_after_cancel_does_not_promote_replacement_overlap() {
    let (proof_tx, mut proof_rx) = tokio::sync::mpsc::channel(8);
    let gate = EventGate::with_proof(Arc::new(AtomicU64::new(4)), Some(proof_tx));
    gate.arm_pending(true, 4);
    let (sink, _) = collecting_sink();
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::TimePos(0.1)),
        &sink,
    );
    let stale = proof_rx
        .try_recv()
        .expect("pending TimePos must enqueue Ready");

    let mut h = harness();
    h.gate = Arc::clone(&gate);
    h.conductor.gate = gate;
    assert!(
        h.conductor
            .handle_command(overlap_load("/music/b.flac"))
            .await
    );
    assert!(h.conductor.pending_overlap.is_some());
    h.conductor.apply_extra_proof(stale).await;
    assert!(
        !h.conductor.extra_is_lead,
        "Ready from a cancelled overlap must not promote the replacement"
    );
    assert!(
        h.conductor.pending_overlap.is_some(),
        "replacement overlap must stay pending until its own proof"
    );
}

#[tokio::test]
async fn transport_closed_failed_overlap_drops_dead_standby() {
    let mut h = harness();
    assert!(
        h.conductor
            .handle_command(overlap_load("/music/b.flac"))
            .await
    );
    drop(h.extra_rx);
    let epoch = h
        .conductor
        .pending_overlap
        .as_ref()
        .expect("overlap is pending")
        .epoch;
    h.conductor
        .apply_extra_proof(ExtraProof::TransportClosed {
            epoch,
            from_extra: true,
        })
        .await;
    assert!(
        h.conductor.extra.is_none(),
        "standby TransportClosed must drop the dead extra deck"
    );
    assert!(!h.conductor.extra_is_lead);
    assert!(h.conductor.pending_overlap.is_none());
    h.conductor.standby_spawn = Some(|| Err(OverlapBlocker::Mpv));
    let survived = h
        .conductor
        .handle_command(overlap_load("/music/c.flac"))
        .await;
    assert!(
        survived,
        "next overlap must Cut-fall back instead of ending the conductor"
    );
    let primary = take_cmds(&mut h.primary_rx);
    assert!(
        primary
            .iter()
            .any(|cmd| load_url(cmd) == Some("/music/c.flac") && load_is_cut(cmd)),
        "dead standby must Cut the next destination onto primary"
    );
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
    let epoch = h
        .conductor
        .pending_overlap
        .as_ref()
        .expect("overlap is pending")
        .epoch;
    h.conductor
        .apply_extra_proof(ExtraProof::Failed { epoch })
        .await;
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
    assert!(matches!(
        proof_rx.try_recv(),
        Ok(ExtraProof::Ready { epoch: 1 })
    ));
}

#[test]
fn observe_pending_uses_one_generation_epoch_snapshot() {
    let (proof_tx, mut proof_rx) = tokio::sync::mpsc::channel(8);
    let gate = EventGate::with_proof(Arc::new(AtomicU64::new(4)), Some(proof_tx));
    let first = gate.arm_pending(true, 4);
    let second = gate.arm_pending(true, 9);
    assert_ne!(first, second);
    let (sink, _) = collecting_sink();
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::TimePos(0.1)),
        &sink,
    );
    assert!(
        proof_rx.try_recv().is_err(),
        "old-generation TimePos must not stamp Ready with the replacement epoch"
    );
    gate.emit(
        true,
        PlayerEvent::file_scoped(9, PlayerEvent::TimePos(0.1)),
        &sink,
    );
    assert!(matches!(
        proof_rx.try_recv(),
        Ok(ExtraProof::Ready { epoch }) if epoch == second
    ));
}

#[test]
fn pending_identity_is_none_while_epoch_is_invalidated() {
    let (proof_tx, mut proof_rx) = tokio::sync::mpsc::channel(8);
    let gate = EventGate::with_proof(Arc::new(AtomicU64::new(4)), Some(proof_tx));
    gate.arm_pending(true, 4);
    gate.pending_epoch.store(0, Ordering::Release);
    assert_eq!(gate.pending_generation.load(Ordering::Acquire), 4);
    let (sink, _) = collecting_sink();
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::TimePos(0.1)),
        &sink,
    );
    assert!(
        proof_rx.try_recv().is_err(),
        "generation without a published epoch is not a coherent pending snapshot"
    );
}

#[test]
fn observe_pending_does_not_fail_replacement_from_stale_error() {
    let (proof_tx, mut proof_rx) = tokio::sync::mpsc::channel(8);
    let gate = EventGate::with_proof(Arc::new(AtomicU64::new(4)), Some(proof_tx));
    let first = gate.arm_pending(true, 4);
    let second = gate.arm_pending(true, 9);
    assert_ne!(first, second);
    let (sink, _) = collecting_sink();
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::Error("stale dest".to_owned())),
        &sink,
    );
    assert!(
        proof_rx.try_recv().is_err(),
        "old-generation Error must not stamp Failed with the replacement epoch"
    );
    gate.emit(
        true,
        PlayerEvent::file_scoped(9, PlayerEvent::Error("incoming dest".to_owned())),
        &sink,
    );
    assert!(matches!(
        proof_rx.try_recv(),
        Ok(ExtraProof::Failed { epoch }) if epoch == second
    ));
    gate.emit(
        true,
        PlayerEvent::TransportClosed("dead extra".to_owned()),
        &sink,
    );
    assert!(matches!(
        proof_rx.try_recv(),
        Ok(ExtraProof::TransportClosed { epoch, .. }) if epoch == second
    ));
}

#[test]
fn event_gate_admits_pending_incoming_file_facts() {
    let gate = EventGate::new(Arc::new(AtomicU64::new(4)));
    gate.arm_pending(true, 4);
    let (sink, collected) = collecting_sink();
    let chapters = vec![Chapter {
        title: "intro".to_owned(),
        start_secs: 0.0,
    }];

    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::Metadata(serde_json::json!({"title": "b"}))),
        &sink,
    );
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::Chapters(chapters.clone())),
        &sink,
    );
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::CacheTime(Some(1.5))),
        &sink,
    );
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::AudioCodec(Some("flac".to_owned()))),
        &sink,
    );
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::FileFormat(Some("flac".to_owned()))),
        &sink,
    );
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::Duration(None)),
        &sink,
    );

    let events = take(&collected);
    assert!(
        events.iter().any(|event| matches!(
            event,
            PlayerEvent::FileScoped {
                file_generation: 4,
                event
            } if matches!(event.as_ref(), PlayerEvent::Metadata(value) if value["title"] == "b")
        )),
        "pending overlap must surface incoming Metadata before lead flip"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        PlayerEvent::FileScoped {
            file_generation: 4,
            event
        } if matches!(event.as_ref(), PlayerEvent::Chapters(got) if got == &chapters)
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        PlayerEvent::FileScoped {
            file_generation: 4,
            event
        } if matches!(event.as_ref(), PlayerEvent::CacheTime(Some(t)) if *t == 1.5)
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        PlayerEvent::FileScoped {
            file_generation: 4,
            event
        } if matches!(event.as_ref(), PlayerEvent::AudioCodec(Some(codec)) if codec == "flac")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        PlayerEvent::FileScoped {
            file_generation: 4,
            event
        } if matches!(event.as_ref(), PlayerEvent::FileFormat(Some(format)) if format == "flac")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        PlayerEvent::FileScoped {
            file_generation: 4,
            event
        } if matches!(event.as_ref(), PlayerEvent::Duration(None))
    )));
}

#[test]
fn pending_extra_facts_keep_verified_generation_when_admitted_moves() {
    let admitted = Arc::new(AtomicU64::new(4));
    let gate = EventGate::new(Arc::clone(&admitted));
    gate.arm_pending(true, 4);
    admitted.store(9, Ordering::Release);
    let (sink, collected) = collecting_sink();

    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::Metadata(serde_json::json!({"title": "b"}))),
        &sink,
    );
    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::Chapters(Vec::new())),
        &sink,
    );

    let events = take(&collected);
    assert!(
        events.iter().all(|event| matches!(
            event,
            PlayerEvent::FileScoped {
                file_generation: 4,
                ..
            }
        )),
        "pending extra facts must keep the verified generation, not the newer admitted counter"
    );
    assert_eq!(events.len(), 2);
}

#[test]
fn pending_readiness_requires_the_incoming_deck_and_exact_generation() {
    let (proof_tx, mut proof_rx) = tokio::sync::mpsc::channel(8);
    let gate = EventGate::with_proof(Arc::new(AtomicU64::new(4)), Some(proof_tx));
    gate.arm_pending(true, 4);
    let (sink, collected) = collecting_sink();

    gate.emit(
        false,
        PlayerEvent::file_scoped(4, PlayerEvent::TimePos(0.1)),
        &sink,
    );
    gate.emit(
        true,
        PlayerEvent::file_scoped(3, PlayerEvent::TimePos(0.2)),
        &sink,
    );
    assert!(proof_rx.try_recv().is_err());
    assert!(
        take(&collected).is_empty(),
        "wrong deck or generation must not admit pending file facts"
    );

    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::Duration(None)),
        &sink,
    );
    assert!(
        proof_rx.try_recv().is_err(),
        "Duration(None) is incoming file fact, not Ready proof"
    );
    assert!(matches!(
        take(&collected).as_slice(),
        [PlayerEvent::FileScoped {
            file_generation: 4,
            event
        }] if matches!(event.as_ref(), PlayerEvent::Duration(None))
    ));

    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::Paused(false)),
        &sink,
    );
    assert!(
        proof_rx.try_recv().is_err(),
        "pause telemetry is useful but does not prove the new file can advance"
    );
    assert!(matches!(
        take(&collected).as_slice(),
        [PlayerEvent::FileScoped {
            file_generation: 4,
            event
        }] if matches!(event.as_ref(), PlayerEvent::Paused(false))
    ));

    gate.emit(
        true,
        PlayerEvent::file_scoped(4, PlayerEvent::TimePos(0.3)),
        &sink,
    );
    assert!(matches!(
        proof_rx.try_recv(),
        Ok(ExtraProof::Ready { epoch: _ })
    ));
    assert!(matches!(
        take(&collected).as_slice(),
        [PlayerEvent::FileScoped {
            file_generation: 4,
            event
        }] if matches!(event.as_ref(), PlayerEvent::TimePos(pos) if *pos == 0.3)
    ));
}

#[test]
fn only_the_pending_incoming_deck_can_report_overlap_failure() {
    let (proof_tx, mut proof_rx) = tokio::sync::mpsc::channel(8);
    let gate = EventGate::with_proof(Arc::new(AtomicU64::new(4)), Some(proof_tx));
    gate.arm_pending(true, 4);
    let (sink, collected) = collecting_sink();

    gate.emit(
        false,
        PlayerEvent::TransportClosed("outgoing closed".to_owned()),
        &sink,
    );
    assert!(matches!(
        proof_rx.try_recv(),
        Ok(ExtraProof::DeckClosed { from_extra: false })
    ));

    gate.emit(
        true,
        PlayerEvent::Error("incoming failed".to_owned()),
        &sink,
    );
    assert!(matches!(
        proof_rx.try_recv(),
        Ok(ExtraProof::Failed { epoch: _ })
    ));
    assert!(
        take(&collected).is_empty(),
        "deck-local terminal events must be converted into fallback proof, not leaked"
    );
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
async fn disabling_overlap_keeps_playing_extra_lead_until_next_cut() {
    let mut h = harness();
    h.conductor.set_extra_is_lead(true);
    h.conductor.extra_has_file = true;
    assert!(
        h.conductor
            .handle_command(PlayerCmd::SetOverlap(false))
            .await
    );
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
        epoch: 1,
    });
    assert!(
        h.conductor
            .handle_command(PlayerCmd::SetOverlap(false))
            .await
    );
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
    assert!(
        extra
            .iter()
            .any(|cmd| load_url(cmd) == Some("/music/b.flac") && load_is_cut(cmd)),
        "Off must Cut the already-committed pending destination onto the extra lead"
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
        "Off must return the subsequent Cut to primary"
    );
}

#[tokio::test]
async fn replacing_a_pending_overlap_stops_it_before_loading_the_new_destination() {
    let mut h = harness();
    assert!(
        h.conductor
            .handle_command(overlap_load("/music/b.flac"))
            .await
    );
    let _ = take_cmds(&mut h.extra_rx);

    h.gate.admitted.store(5, Ordering::Release);
    assert!(
        h.conductor
            .handle_command(overlap_load("/music/c.flac"))
            .await
    );

    let extra = take_cmds(&mut h.extra_rx);
    assert_eq!(extra.len(), 4);
    assert!(matches!(extra[0], PlayerCmd::Stop));
    assert!(matches!(extra[1], PlayerCmd::SetVolume(100)));
    assert!(matches!(extra[2], PlayerCmd::SetVolume(0)));
    assert!(
        load_url(&extra[3]) == Some("/music/c.flac") && load_is_cut(&extra[3]),
        "the replacement must load only after the abandoned incoming file is stopped"
    );
    assert_eq!(h.gate.pending_generation.load(Ordering::Acquire), 5);
    assert!(take_cmds(&mut h.primary_rx).is_empty());
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
