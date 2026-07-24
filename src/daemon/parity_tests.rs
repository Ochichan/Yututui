//! App↔DaemonEngine parity: the convention→contract upgrade (docs/gui/10 §4).
//!
//! One shared script of remote commands is applied to BOTH owner implementations; after
//! every command the two owners must project **equal** `PlayerModel`/`QueueModel` wire
//! models (and agree on the reply's `ok`/`reason`). This turns "the engine is kept in
//! sync with the reducer by convention" (docs/gui/01 §4) into an executable contract,
//! and it is the safety net every S1–S6 extraction step runs against: a parity failure
//! after an extraction means the extraction changed behavior.
//!
//! Harness scope and its deliberate normalization:
//! - The long shared script covers settings/toggle/queue-membership behavior without a live
//!   player. A focused fake-player matrix covers commands that load or seek a track.
//! - `position_epoch` and `elapsed_ms` are normalized out of wire-model equality. Epoch cadence
//!   is asserted separately, including exact zero- and one-bump contracts.
//! - The baseline is aligned before the script runs (volume and paused-at-rest differ
//!   by construction today: `App::new(volume)` vs config-seeded engine, `paused: false`
//!   vs `true` with nothing loaded). The script then must keep them equal.

mod harness;

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::api::Song;
use crate::app::{AiMsg, App, Cmd, Msg, PlayerControl, PlayerMsg};
use crate::config::Config;
use crate::media::MediaCommand;
use crate::queue::{Queue, Repeat};
use crate::remote::proto::{
    RateChange, RemoteCommand, RemoteSettingChange, ServerFrame, ToggleState, Topic,
};
use crate::remote::publish;
use crate::remote::{SessionLine, SessionTuning, test_command_reply, test_register};
use crate::signals::Signals;
use crate::station::StationStore;

use super::engine::{DaemonEngine, EngineEffect, EngineState};
use harness::*;

/// The B0 shared command script: settings, toggles, and queue membership — everything
/// both owners serve today without a live player.
fn b0_script() -> Vec<RemoteCommand> {
    vec![
        RemoteCommand::SetVolume { percent: 60 },
        RemoteCommand::VolumeUp,
        RemoteCommand::VolumeUp,
        RemoteCommand::VolumeDown,
        RemoteCommand::SetVolume { percent: 100 }, // upper clamp behavior
        RemoteCommand::VolumeUp,
        RemoteCommand::QueueRemove { position: 0 }, // before the cursor: no track load
        // Order surgery on the shared Queue methods (v8 GUI wires): reorder around the
        // cursor, out-of-range rejection, then trim everything upcoming — none of these
        // may touch the current track or need a player.
        RemoteCommand::QueueMove {
            from: 2,
            to: 0,
            expected_rev: None,
        },
        RemoteCommand::QueueMove {
            from: 0,
            to: 2,
            expected_rev: None,
        },
        RemoteCommand::QueueMove {
            from: 99,
            to: 0,
            expected_rev: None,
        }, // queue_index on both owners
        // The rating cycle on the current track ("b" since the seed): the projected
        // TrackModel favorite/disliked halves must stay equal through a full
        // neutral → like → dislike → neutral revolution, and the guards must agree.
        RemoteCommand::Rate {
            video_id: "b".to_owned(),
            rating: RateChange::Cycle,
        },
        RemoteCommand::Rate {
            video_id: "b".to_owned(),
            rating: RateChange::Cycle,
        },
        RemoteCommand::Rate {
            video_id: "b".to_owned(),
            rating: RateChange::Cycle,
        },
        RemoteCommand::Rate {
            video_id: "not-current".to_owned(),
            rating: RateChange::Cycle,
        }, // unknown_track on both owners
        RemoteCommand::Rate {
            video_id: "b".to_owned(),
            rating: RateChange::Up,
        }, // not_supported on both owners (only the cycle is wired today)
        RemoteCommand::ToggleShuffle,
        RemoteCommand::CycleRepeat,
        RemoteCommand::CycleRepeat,
        RemoteCommand::SetSetting {
            change: RemoteSettingChange::Speed { tenths: 15 },
        },
        RemoteCommand::SetSetting {
            change: RemoteSettingChange::Normalize { value: true },
        },
        RemoteCommand::SetSetting {
            change: RemoteSettingChange::Gapless { value: true },
        },
        RemoteCommand::Streaming {
            state: ToggleState::On,
        },
        RemoteCommand::SetSetting {
            change: RemoteSettingChange::AutoplayStreaming { value: true },
        },
        RemoteCommand::CycleRepeat, // One → Off: disabling repeat remains allowed
        RemoteCommand::Streaming {
            state: ToggleState::On,
        },
        RemoteCommand::CycleRepeat, // rejected while streaming is on
        RemoteCommand::Streaming {
            state: ToggleState::Toggle, // On → Off: disabling streaming remains allowed
        },
        RemoteCommand::ToggleShuffle, // back to natural order
        RemoteCommand::CycleRepeat,   // Off → All remains allowed after streaming is off
        // Last: membership trimming (leaves one track, so it must not weaken the
        // shuffle steps above). Cursor track survives; the repeat is an ok no-op.
        RemoteCommand::QueueClearUpcoming { expected_rev: None },
        RemoteCommand::QueueClearUpcoming { expected_rev: None },
    ]
}

#[tokio::test]
async fn shared_script_keeps_app_and_engine_projections_equal() {
    let (mut app, mut engine) = hermetic_pair();
    assert_parity("baseline", &app, &engine);

    for (index, cmd) in b0_script().into_iter().enumerate() {
        let step = format!("step {index}: {cmd:?}");
        let class = command_parity_class(&cmd);
        assert!(
            matches!(
                class,
                CommandParityClass::SharedStableEpoch | CommandParityClass::SharedMayRebase
            ),
            "{step}: B0 script contains a non-shared command ({class:?})"
        );
        let epochs_before = PositionEpochs::capture(&app, &engine);

        let (app_resp, app_cmds) = app_apply_with_cmds(&mut app, cmd.clone());
        let (engine_resp, shutdown, engine_effects) = engine.handle_remote(cmd).await;
        assert!(!shutdown, "{step}: script must not shut the engine down");

        assert_eq!(
            app_resp.ok, engine_resp.ok,
            "{step}: owners disagree on ok (app: {app_resp:?}, engine: {engine_resp:?})"
        );
        assert_eq!(
            app_resp.reason, engine_resp.reason,
            "{step}: owners disagree on the machine reason code"
        );
        if app_resp.reason.as_deref() == Some("incompatible_playback_modes") {
            assert!(app_cmds.is_empty(), "{step}: App rejection emitted effects");
            assert!(
                engine_effects.is_empty(),
                "{step}: daemon rejection emitted effects"
            );
        }
        epochs_before.assert_delta(&step, PositionEpochs::capture(&app, &engine), 0);

        assert_parity(&step, &app, &engine);
    }
}

#[test]
fn command_classifier_pins_shared_and_owner_boundary_exceptions() {
    assert_eq!(
        command_parity_class(&RemoteCommand::SetVolume { percent: 50 }),
        CommandParityClass::SharedStableEpoch
    );
    assert_eq!(
        command_parity_class(&RemoteCommand::SeekForward),
        CommandParityClass::SharedMayRebase
    );
    assert_eq!(
        command_parity_class(&RemoteCommand::RunSearch {
            ticket: 7,
            query: "query".to_owned(),
            source: crate::search_source::SearchSource::Youtube,
        }),
        CommandParityClass::StandaloneRejected
    );
    assert_eq!(
        command_parity_class(&RemoteCommand::ExportPersonalData {
            directory: "/tmp".to_owned(),
            schema: None,
        }),
        CommandParityClass::BothOwnerLoopIntercepted
    );
    assert_eq!(
        command_parity_class(&RemoteCommand::Quit),
        CommandParityClass::OwnerSpecific
    );
    assert_eq!(
        command_parity_class(&RemoteCommand::SetSetting {
            change: RemoteSettingChange::RadioMode {
                state: ToggleState::Toggle,
            },
        }),
        CommandParityClass::OwnerSpecific
    );
}

#[tokio::test]
async fn shared_transport_commands_pin_position_epoch_cadence() {
    type CommandFactory = fn(u64) -> RemoteCommand;

    let cases: [(&str, CommandFactory, u64); 10] = [
        ("toggle pause", |_| RemoteCommand::TogglePause, 0),
        ("seek back", |_| RemoteCommand::SeekBack, 1),
        ("seek forward", |_| RemoteCommand::SeekForward, 1),
        ("absolute seek", |_| RemoteCommand::SeekTo { ms: 90_000 }, 1),
        ("next", |_| RemoteCommand::Next, 1),
        ("previous", |_| RemoteCommand::Prev, 1),
        (
            "queue play",
            |_| RemoteCommand::QueuePlay { position: 2 },
            1,
        ),
        (
            "revision-checked queue play",
            |expected_rev| RemoteCommand::QueuePlayIfRevision {
                position: 2,
                expected_rev,
            },
            1,
        ),
        (
            "current queue remove",
            |_| RemoteCommand::QueueRemove { position: 1 },
            1,
        ),
        (
            "revision-checked current queue remove",
            |expected_rev| RemoteCommand::QueueRemoveIfRevision {
                position: 1,
                expected_rev,
            },
            1,
        ),
    ];

    for (step, command, expected_delta) in cases {
        let (mut app, mut engine) = hermetic_pair();
        app.install_seek_parity_state("b", 30.0, 225.0);
        let _engine_player = engine.install_seek_parity_player("b", 30.0, 225.0);
        let epochs_before = PositionEpochs::capture(&app, &engine);

        let app_command = command(app.core_view().queue.rev());
        let engine_command = command(engine.core_view().queue.rev());
        assert_eq!(
            command_parity_class(&app_command),
            CommandParityClass::SharedMayRebase,
            "{step}: transport matrix command was misclassified"
        );

        let app_response = app_apply(&mut app, app_command);
        let (engine_response, shutdown, engine_effects) =
            engine.handle_remote(engine_command).await;

        assert!(!shutdown, "{step}: shared command requested shutdown");
        assert_eq!(app_response.ok, engine_response.ok, "{step}: ok diverged");
        assert_eq!(
            app_response.reason, engine_response.reason,
            "{step}: reason diverged"
        );
        assert!(app_response.ok, "{step}: command was not admitted");
        assert!(
            engine_effects.is_empty(),
            "{step}: unexpected daemon effect"
        );
        epochs_before.assert_delta(step, PositionEpochs::capture(&app, &engine), expected_delta);
        assert_parity(step, &app, &engine);
    }
}

#[tokio::test]
async fn accepted_seek_uses_fast_precision_and_one_epoch_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 10.0, 180.0);
    let app_epoch = app.playback.position_epoch;
    let mut engine_player = engine.install_seek_parity_player("b", 10.0, 180.0);
    let (_, engine_epoch) = engine.seek_parity_projection();

    let (reply_tx, mut reply_rx) = oneshot::channel();
    let app_commands = app.update(Msg::Remote(
        RemoteCommand::SeekTo { ms: 90_000 },
        reply_tx.into(),
    ));
    let app_seek = app_commands
        .iter()
        .find_map(Cmd::player_command)
        .expect("App seek command");
    assert!(matches!(
        app_seek,
        crate::player::PlayerCmd::SeekAbsolute {
            seconds: 90.0,
            precision: crate::player::SeekPrecision::InteractiveFast,
        }
    ));
    admit_app_player_intents(&mut app, app_commands);
    assert!(reply_rx.try_recv().expect("App seek reply").ok);

    let (engine_reply, shutdown, effects) = engine
        .handle_remote(RemoteCommand::SeekTo { ms: 90_000 })
        .await;
    assert!(engine_reply.ok);
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert!(matches!(
        engine_player.try_recv(),
        Ok(crate::player::PlayerCmd::SeekAbsolute {
            seconds: 90.0,
            precision: crate::player::SeekPrecision::InteractiveFast,
        })
    ));

    assert_eq!(app.playback.time_pos, Some(90.0));
    let (engine_position, engine_position_epoch) = engine.seek_parity_projection();
    assert_eq!(engine_position, Some(90.0));
    assert_eq!(app.playback.position_epoch, app_epoch + 1);
    assert_eq!(engine_position_epoch, engine_epoch + 1);
    assert_parity("accepted fast seek", &app, &engine);
}

#[tokio::test]
async fn midtrack_source_recovery_trace_and_epoch_match_in_both_owners() {
    const ERROR: &str = "connection reset while reading source";
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 90.5, 225.0);
    app.playback.paused = true;
    let app_epoch = app.playback.position_epoch;
    let mut engine_player = engine.install_seek_parity_player("b", 90.5, 225.0);
    assert!(
        engine
            .handle_player_event(crate::player::PlayerEvent::Paused(true))
            .await
            .is_empty()
    );
    let (_, engine_epoch) = engine.seek_parity_projection();

    let app_commands = app.update(PlayerMsg::Error(ERROR.to_owned()));
    let app_request = app_commands
        .iter()
        .find_map(Cmd::player_command)
        .and_then(|command| match command {
            crate::player::PlayerCmd::LoadWithResume(request) => Some(request.clone()),
            _ => None,
        })
        .expect("App source-recovery command");
    admit_app_player_intents(&mut app, app_commands);

    let effects = engine
        .handle_player_event(crate::player::PlayerEvent::Error(ERROR.to_owned()))
        .await;
    assert!(effects.is_empty());
    let engine_request = match engine_player.try_recv() {
        Ok(crate::player::PlayerCmd::LoadWithResume(request)) => request,
        _ => panic!("daemon source-recovery command"),
    };

    assert_eq!(app_request.position_secs, engine_request.position_secs);
    assert_eq!(app_request.paused, engine_request.paused);
    assert_eq!(
        app_request.episode_id.get(),
        engine_request.episode_id.get()
    );
    assert_eq!(
        app_request.transport_epoch.get(),
        engine_request.transport_epoch.get()
    );
    assert_eq!(app.playback.time_pos, Some(90.5));
    let (engine_position, engine_position_epoch) = engine.seek_parity_projection();
    assert_eq!(engine_position, Some(90.5));
    assert!(app.playback.paused);
    assert!(engine_request.paused);
    assert_eq!(app.playback.position_epoch, app_epoch + 1);
    assert_eq!(engine_position_epoch, engine_epoch + 1);
}

fn app_transport_restore(app: &mut App, reason: &str) -> Vec<crate::player::PlayerCmd> {
    app.update(PlayerMsg::TransportClosed(reason.to_owned()))
        .into_iter()
        .find_map(|command| match command {
            Cmd::PlayerControl(PlayerControl::Restart { restore }) => Some(restore),
            _ => None,
        })
        .expect("App transport loss must request one replacement player")
}

async fn recv_parity_player_command(
    receiver: &mut tokio::sync::mpsc::Receiver<crate::player::PlayerCmd>,
) -> crate::player::PlayerCmd {
    tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .expect("player command timed out")
        .expect("player command lane closed")
}

#[tokio::test]
async fn loaded_transport_loss_has_the_same_restore_trace_and_projection_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 42.0, 225.0);
    app.playback.paused = true;
    let app_epoch = app.playback.position_epoch;

    let old_engine_player = engine.install_seek_parity_player("b", 42.0, 225.0);
    assert!(
        engine
            .handle_player_event(crate::player::PlayerEvent::Paused(true))
            .await
            .is_empty()
    );
    let (_, engine_epoch) = engine.seek_parity_projection();
    let mut replacement_player = engine.queue_transport_recovery_parity_player();

    let app_restore = app_transport_restore(&mut app, "unexpected EOF");
    let engine_effects = engine
        .handle_player_event(crate::player::PlayerEvent::TransportClosed(
            "unexpected EOF".to_owned(),
        ))
        .await;

    assert!(engine_effects.is_empty());
    assert!(old_engine_player.is_closed());
    assert!(matches!(
        recv_parity_player_command(&mut replacement_player).await,
        crate::player::PlayerCmd::SetVolume(_)
    ));
    assert!(matches!(
        recv_parity_player_command(&mut replacement_player).await,
        crate::player::PlayerCmd::SetAudioFilter(_)
    ));

    let app_load = match app_restore.as_slice() {
        [
            crate::player::PlayerCmd::Load(load),
            crate::player::PlayerCmd::CyclePause,
        ] => load,
        _ => panic!("paused App recovery must restore exactly Load then CyclePause"),
    };
    let engine_load = match recv_parity_player_command(&mut replacement_player).await {
        crate::player::PlayerCmd::Load(load) => load,
        _ => panic!("paused daemon recovery must restore Load after player setup"),
    };
    assert_eq!(app_load, &engine_load);
    assert!(matches!(
        recv_parity_player_command(&mut replacement_player).await,
        crate::player::PlayerCmd::CyclePause
    ));
    assert!(
        replacement_player.try_recv().is_err(),
        "daemon recovery emitted an unexpected command after CyclePause"
    );

    assert_eq!(app.playback.time_pos, None);
    let (engine_position, engine_position_epoch) = engine.seek_parity_projection();
    assert_eq!(engine_position, None);
    assert!(app.playback.paused);
    assert_eq!(app.playback.position_epoch, app_epoch + 1);
    assert_eq!(engine_position_epoch, engine_epoch + 1);
    assert_parity("loaded transport recovery", &app, &engine);
}

#[tokio::test]
async fn stopped_current_item_is_not_resurrected_by_late_transport_loss_in_either_owner() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 42.0, 225.0);
    app.playback.paused = false;
    let app_epoch = app.playback.position_epoch;

    let mut engine_player = engine.install_seek_parity_player("b", 42.0, 225.0);
    assert!(
        engine
            .handle_player_event(crate::player::PlayerEvent::Paused(false))
            .await
            .is_empty()
    );
    let (_, engine_epoch) = engine.seek_parity_projection();

    let app_stop = app.update(Msg::Media(MediaCommand::Stop));
    assert!(
        app_stop
            .iter()
            .flat_map(Cmd::player_commands)
            .any(|command| matches!(command, crate::player::PlayerCmd::Stop)),
        "App media Stop must reach the player before clearing loaded identity"
    );
    admit_app_player_intents(&mut app, app_stop);
    let (shutdown, engine_effects) = engine.handle_media(MediaCommand::Stop).await;
    assert!(!shutdown);
    assert!(engine_effects.is_empty());
    assert!(matches!(
        recv_parity_player_command(&mut engine_player).await,
        crate::player::PlayerCmd::Stop
    ));
    assert!(engine_player.is_closed());

    assert_eq!(app.playback.time_pos, None);
    let (engine_position, engine_position_epoch) = engine.seek_parity_projection();
    assert_eq!(engine_position, None);
    assert!(app.playback.paused);
    assert_eq!(app.playback.position_epoch, app_epoch + 1);
    assert_eq!(engine_position_epoch, engine_epoch + 1);

    let app_restore = app_transport_restore(&mut app, "late close after Stop");
    let engine_effects = engine
        .handle_player_event(crate::player::PlayerEvent::TransportClosed(
            "late close after Stop".to_owned(),
        ))
        .await;

    assert!(
        app_restore.is_empty(),
        "App must not reload the queue cursor after Stop cleared loaded identity"
    );
    assert!(engine_effects.is_empty());
    let app_media = app.media_snapshot();
    let engine_media = engine.media_snapshot();
    assert!(!app_media.caps.can_seek);
    assert!(!engine_media.caps.can_seek);
    assert_eq!(app_media.status, crate::media::MediaPlaybackStatus::Paused);
    assert_eq!(
        engine_media.status,
        crate::media::MediaPlaybackStatus::Paused
    );
    assert_parity("late transport close after Stop", &app, &engine);
}

fn assert_reply_before_player_event(
    owner: &str,
    frame_id: u64,
    rx: &mut tokio::sync::mpsc::Receiver<SessionLine>,
) {
    let reply = rx.try_recv().expect("command reply was enqueued");
    let event = rx.try_recv().expect("same-turn player event was enqueued");
    assert!(rx.try_recv().is_err(), "{owner}: unexpected third frame");

    match reply {
        SessionLine::Raw(bytes) | SessionLine::TrackedRaw { bytes, .. } => {
            match serde_json::from_slice::<ServerFrame>(&bytes)
                .unwrap_or_else(|error| panic!("{owner}: invalid reply frame: {error}"))
            {
                ServerFrame::Reply { id, resp } => {
                    assert_eq!(id, frame_id, "{owner}: wrong reply id");
                    assert!(resp.ok, "{owner}: mutation reply failed: {resp:?}");
                }
                other => panic!("{owner}: expected Reply first, got {other:?}"),
            }
        }
        SessionLine::Event { .. } => panic!("{owner}: Event overtook Reply"),
    }
    match event {
        SessionLine::Event {
            topic: Topic::Player,
            ..
        } => {}
        SessionLine::Event { topic, .. } => {
            panic!("{owner}: expected player event, got {topic:?}")
        }
        SessionLine::Raw(bytes) | SessionLine::TrackedRaw { bytes, .. } => panic!(
            "{owner}: expected Event second, got {}",
            String::from_utf8_lossy(&bytes)
        ),
    }
}

/// Regression for the v8 owner-loop ordering contract (docs/gui/02 §6): the command mutation and
/// its response happen on the owner turn, and the post-turn publisher observes the new state only
/// afterwards. Repeat enough times that the old oneshot wakeup race was easy to reproduce while
/// keeping the assertion deterministic against each session's single outbound lane.
#[tokio::test]
async fn persistent_v8_mutation_reply_precedes_event_for_both_owners() {
    let (mut app, mut engine) = hermetic_pair();

    let (app_hub, app_session, mut app_rx) = test_register(SessionTuning::default());
    let mut app_publisher = publish::Publisher::new(Arc::clone(&app_hub));
    app_publisher.observe(&app.core_view());
    app_publisher.handle_subscribe(&app.core_view(), &app_session, None, 1, &[Topic::Player]);
    while app_rx.try_recv().is_ok() {}

    let (engine_hub, engine_session, mut engine_rx) = test_register(SessionTuning::default());
    let mut engine_publisher = publish::Publisher::new(Arc::clone(&engine_hub));
    engine_publisher.observe(&engine.core_view());
    engine_publisher.handle_subscribe(
        &engine.core_view(),
        &engine_session,
        None,
        1,
        &[Topic::Player],
    );
    while engine_rx.try_recv().is_ok() {}

    for turn in 0..64u64 {
        let volume = if turn % 2 == 0 { 23 } else { 77 };
        let frame_id = 100 + turn;

        // Standalone TUI owner path: the runtime admits the typed player intent, whose commit
        // completes the direct session reply before runner.rs observes the accepted state.
        let app_reply = test_command_reply(Arc::clone(&app_hub), app_session.clone(), frame_id);
        let effects = app.update(Msg::Remote(
            RemoteCommand::SetVolume { percent: volume },
            app_reply,
        ));
        admit_app_player_intents(&mut app, effects);
        app_publisher.observe(&app.core_view());
        assert_reply_before_player_event("tui", frame_id, &mut app_rx);

        // Daemon owner path: daemon/mod.rs sends the engine response synchronously before its
        // common post-turn Publisher::observe call.
        let (response, shutdown, _effects) = engine
            .handle_remote(RemoteCommand::SetVolume { percent: volume })
            .await;
        assert!(!shutdown);
        let engine_reply =
            test_command_reply(Arc::clone(&engine_hub), engine_session.clone(), frame_id);
        let _ = engine_reply.send(response);
        engine_publisher.observe(&engine.core_view());
        assert_reply_before_player_event("daemon", frame_id, &mut engine_rx);
    }
}

#[tokio::test]
async fn revision_checked_queue_remove_is_stale_safe_and_owner_parity_holds() {
    let (mut app, mut engine) = hermetic_pair();

    let app_resp = app_apply(
        &mut app,
        RemoteCommand::QueueRemoveIfRevision {
            position: 0,
            expected_rev: u64::MAX,
        },
    );
    let (engine_resp, shutdown, effects) = engine
        .handle_remote(RemoteCommand::QueueRemoveIfRevision {
            position: 0,
            expected_rev: u64::MAX,
        })
        .await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(app_resp.reason.as_deref(), Some("stale_rev"));
    assert_eq!(app_resp.reason, engine_resp.reason);
    assert_parity("stale revision-checked remove", &app, &engine);

    let app_rev = app.core_view().queue.rev();
    let engine_rev = engine.core_view().queue.rev();
    let app_resp = app_apply(
        &mut app,
        RemoteCommand::QueueRemoveIfRevision {
            position: 0,
            expected_rev: app_rev,
        },
    );
    let (engine_resp, shutdown, effects) = engine
        .handle_remote(RemoteCommand::QueueRemoveIfRevision {
            position: 0,
            expected_rev: engine_rev,
        })
        .await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert!(app_resp.ok && engine_resp.ok);
    assert_eq!(app_resp.reason, engine_resp.reason);
    assert_parity("fresh revision-checked remove", &app, &engine);
}

#[tokio::test]
async fn revision_checked_queue_play_rejects_stale_without_owner_mutation() {
    let (mut app, mut engine) = hermetic_pair();
    let app_before = serde_json::to_value(app.core_view().queue.snapshot()).unwrap();
    let engine_before = serde_json::to_value(engine.core_view().queue.snapshot()).unwrap();
    let app_rev_before = app.core_view().queue.rev();
    let engine_rev_before = engine.core_view().queue.rev();
    let stale_epochs = PositionEpochs::capture(&app, &engine);

    let command = RemoteCommand::QueuePlayIfRevision {
        position: 2,
        expected_rev: u64::MAX,
    };
    let app_resp = app_apply(&mut app, command.clone());
    let (engine_resp, shutdown, effects) = engine.handle_remote(command).await;

    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(app_resp.reason.as_deref(), Some("stale_rev"));
    assert_eq!(app_resp.reason, engine_resp.reason);
    assert_eq!(
        serde_json::to_value(app.core_view().queue.snapshot()).unwrap(),
        app_before
    );
    assert_eq!(
        serde_json::to_value(engine.core_view().queue.snapshot()).unwrap(),
        engine_before
    );
    assert_eq!(app.core_view().queue.rev(), app_rev_before);
    assert_eq!(engine.core_view().queue.rev(), engine_rev_before);
    stale_epochs.assert_delta(
        "stale revision-checked play",
        PositionEpochs::capture(&app, &engine),
        0,
    );
    assert_parity("stale revision-checked play", &app, &engine);

    // A fresh revision must pass the optimistic-concurrency gate and reach the shared queue
    // index validation. Use an invalid position here so the hermetic parity harness does not
    // spawn a real daemon mpv merely to prove the revision gate was accepted.
    let app_rev = app.core_view().queue.rev();
    let engine_rev = engine.core_view().queue.rev();
    let invalid_position = app.core_view().queue.len();
    let invalid_epochs = PositionEpochs::capture(&app, &engine);
    let app_resp = app_apply(
        &mut app,
        RemoteCommand::QueuePlayIfRevision {
            position: invalid_position,
            expected_rev: app_rev,
        },
    );
    let (engine_resp, shutdown, effects) = engine
        .handle_remote(RemoteCommand::QueuePlayIfRevision {
            position: invalid_position,
            expected_rev: engine_rev,
        })
        .await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(app_resp.reason.as_deref(), Some("queue_index"));
    assert_eq!(app_resp.reason, engine_resp.reason);
    assert_eq!(app.core_view().queue.rev(), app_rev);
    assert_eq!(engine.core_view().queue.rev(), engine_rev);
    invalid_epochs.assert_delta(
        "fresh revision-checked play validation",
        PositionEpochs::capture(&app, &engine),
        0,
    );
    assert_parity("fresh revision-checked play validation", &app, &engine);
}

#[tokio::test]
async fn revision_guarded_move_and_clear_reject_stale_and_accept_fresh_or_absent() {
    let (mut app, mut engine) = hermetic_pair();

    // Stale guard: both owners reject before any mutation.
    let stale = RemoteCommand::QueueMove {
        from: 0,
        to: 1,
        expected_rev: Some(u64::MAX),
    };
    let app_resp = app_apply(&mut app, stale.clone());
    let (engine_resp, shutdown, effects) = engine.handle_remote(stale).await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(app_resp.reason.as_deref(), Some("stale_rev"));
    assert_eq!(app_resp.reason, engine_resp.reason);
    assert_parity("stale revision-checked move", &app, &engine);

    // Fresh guard: accepted on both owners.
    let app_rev = app.core_view().queue.rev();
    let engine_rev = engine.core_view().queue.rev();
    let app_resp = app_apply(
        &mut app,
        RemoteCommand::QueueMove {
            from: 0,
            to: 2,
            expected_rev: Some(app_rev),
        },
    );
    let (engine_resp, shutdown, _) = engine
        .handle_remote(RemoteCommand::QueueMove {
            from: 0,
            to: 2,
            expected_rev: Some(engine_rev),
        })
        .await;
    assert!(!shutdown);
    assert!(app_resp.ok && engine_resp.ok);
    assert_eq!(app_resp.reason, engine_resp.reason);
    assert_parity("fresh revision-checked move", &app, &engine);

    // Absent guard (the keyboard path): stale check skipped, clear applies.
    let stale_clear = RemoteCommand::QueueClearUpcoming {
        expected_rev: Some(u64::MAX),
    };
    let app_resp = app_apply(&mut app, stale_clear.clone());
    let (engine_resp, ..) = engine.handle_remote(stale_clear).await;
    assert_eq!(app_resp.reason.as_deref(), Some("stale_rev"));
    assert_eq!(app_resp.reason, engine_resp.reason);
    let unguarded = RemoteCommand::QueueClearUpcoming { expected_rev: None };
    let app_resp = app_apply(&mut app, unguarded.clone());
    let (engine_resp, ..) = engine.handle_remote(unguarded).await;
    assert!(app_resp.ok && engine_resp.ok);
    assert_eq!(app_resp.reason, engine_resp.reason);
    assert_parity("unguarded clear-upcoming", &app, &engine);
}

#[tokio::test]
async fn track_rating_cycle_and_recommendation_projection_stay_in_parity() {
    let (mut app, mut engine) = hermetic_pair();
    assert_parity("track rating baseline", &app, &engine);

    for step in ["liked", "disliked", "neutral"] {
        let command = RemoteCommand::Rate {
            video_id: "v1".to_owned(),
            rating: RateChange::Cycle,
        };
        let app_response = app_apply(&mut app, command.clone());
        let (engine_response, shutdown, effects) = engine.handle_remote(command).await;
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert_eq!(
            app_response.reason, engine_response.reason,
            "track rating {step}: owners disagree on the reason"
        );
        assert_eq!(
            serde_json::to_value(&*app.signals).unwrap(),
            serde_json::to_value(engine.signals()).unwrap(),
            "track rating {step}: recommendation projections diverged"
        );
        assert_parity(&format!("track rating {step}"), &app, &engine);
    }
}

#[tokio::test]
async fn radio_station_rating_cycle_stays_in_parity() {
    // The radio branch of the rating cycle is dual-implemented by hand (App
    // player.rs vs daemon gui_rate) — pin it: the cycle toggles radio-favorite
    // membership, which projects through TrackModel.favorite on both owners.
    let (mut app, mut engine) = hermetic_pair();
    let mut station = Song::remote("st1", "station-st1", "", "");
    station.playable = Some(crate::api::PlayableRef::RadioStream {
        url: "https://radio.example/st1.mp3".to_owned(),
    });
    let mut queue = Queue::default();
    queue.set(vec![station], 0);
    let snap = queue.snapshot();
    engine.restore_queue_snapshot(snap.clone(), RNG_SEED);
    app.queue.restore_snapshot(snap);
    assert_parity("radio baseline", &app, &engine);

    for step in ["favorite", "unfavorite"] {
        let cmd = RemoteCommand::Rate {
            video_id: "st1".to_owned(),
            rating: RateChange::Cycle,
        };
        let app_resp = app_apply(&mut app, cmd.clone());
        let (engine_resp, shutdown, _) = engine.handle_remote(cmd).await;
        assert!(!shutdown);
        assert_eq!(
            app_resp.reason, engine_resp.reason,
            "radio rating {step}: owners disagree on the reason"
        );
        assert_parity(&format!("radio rating {step}"), &app, &engine);
    }
}

#[tokio::test]
async fn daemon_conflicts_reject_without_state_or_effects() {
    for command in [
        RemoteCommand::Streaming {
            state: ToggleState::On,
        },
        RemoteCommand::Streaming {
            state: ToggleState::Toggle,
        },
        RemoteCommand::SetSetting {
            change: RemoteSettingChange::AutoplayStreaming { value: true },
        },
    ] {
        let mut engine = engine_with_modes(Repeat::All, false).await;
        let before = engine.status();
        let (response, shutdown, effects) = engine.handle_remote(command).await;
        assert!(!response.ok && !shutdown);
        assert_eq!(
            response.reason.as_deref(),
            Some("incompatible_playback_modes")
        );
        assert!(effects.is_empty());
        assert_eq!(engine.status(), before, "rejection mutated daemon state");
    }

    for (repeat, streaming, command) in [
        (Repeat::Off, true, RemoteCommand::CycleRepeat),
        (Repeat::Off, true, gui_repeat(Repeat::All)),
    ] {
        let mut engine = engine_with_modes(repeat, streaming).await;
        let before = engine.status();
        let (response, shutdown, effects) = engine.handle_remote(command).await;
        assert!(!response.ok && !shutdown);
        assert_eq!(
            response.reason.as_deref(),
            Some("incompatible_playback_modes")
        );
        assert!(effects.is_empty());
        assert_eq!(engine.status(), before, "rejection mutated daemon state");
    }
}

#[tokio::test]
async fn daemon_conflict_disables_remain_allowed() {
    for command in [
        RemoteCommand::Streaming {
            state: ToggleState::Off,
        },
        RemoteCommand::Streaming {
            state: ToggleState::Toggle,
        },
        RemoteCommand::SetSetting {
            change: RemoteSettingChange::AutoplayStreaming { value: false },
        },
    ] {
        let mut engine = engine_with_modes(Repeat::One, true).await;
        let (response, shutdown, effects) = engine.handle_remote(command).await;
        let status = engine.status();
        assert!(response.ok && !shutdown);
        assert!(effects.is_empty());
        assert!(!status.streaming);
        assert_eq!(status.repeat, Repeat::One);
    }

    for command in [RemoteCommand::CycleRepeat, gui_repeat(Repeat::Off)] {
        let mut engine = engine_with_modes(Repeat::One, true).await;
        let (response, shutdown, effects) = engine.handle_remote(command).await;
        let status = engine.status();
        assert!(response.ok && !shutdown);
        assert!(effects.is_empty());
        assert!(status.streaming);
        assert_eq!(status.repeat, Repeat::Off);
    }
}

#[tokio::test]
async fn ai_autoplay_revalidation_rejects_and_recovers_in_lockstep() {
    let (mut app, _) = hermetic_pair();
    app.queue.repeat = Repeat::All;
    app.config.repeat = Repeat::All;
    app.autoplay_streaming = false;
    app.config.autoplay_streaming = Some(false);
    app.status.text = "before".to_owned();
    app.dirty = false;
    let mut engine = engine_with_modes(Repeat::All, false).await;

    let app_effects = app.update(Msg::Ai(AiMsg::SetAutoplay(true)));
    let (engine_response, engine_effects) = engine.ai_set_autoplay(true);

    assert!(app_effects.is_empty(), "App rejection emitted effects");
    assert!(!engine_response.ok);
    assert_eq!(
        engine_response.reason.as_deref(),
        Some("incompatible_playback_modes")
    );
    assert!(
        engine_effects.is_empty(),
        "daemon rejection emitted effects"
    );
    assert!(!app.autoplay_streaming);
    assert!(matches!(
        app.status.text.as_str(),
        "Can't use autoplay while repeat is on" | "반복 재생 중에는 자동재생을 켤 수 없어요"
    ));
    assert!(app.dirty, "App rejection toast did not request redraw");
    assert_parity("AI autoplay repeat rejection", &app, &engine);

    // A legacy invalid state can still recover by disabling streaming through the same action.
    let (mut app, _) = hermetic_pair();
    app.queue.repeat = Repeat::One;
    app.config.repeat = Repeat::One;
    app.autoplay_streaming = true;
    app.config.autoplay_streaming = Some(true);
    let mut engine = engine_with_modes(Repeat::One, true).await;

    let app_effects = app.update(Msg::Ai(AiMsg::SetAutoplay(false)));
    let (engine_response, engine_effects) = engine.ai_set_autoplay(false);

    assert!(engine_response.ok);
    assert!(engine_effects.is_empty());
    assert!(
        app_effects
            .iter()
            .all(|effect| { matches!(effect, Cmd::Persist(crate::app::PersistCmd::Config(_))) })
    );
    assert!(!app.autoplay_streaming);
    assert_eq!(app.queue.repeat, Repeat::One);
    assert_parity("AI autoplay legacy disable", &app, &engine);
}

#[tokio::test]
async fn legacy_all_plus_streaming_cycle_to_one_preserves_shipped_owner_semantics() {
    let (mut app, _) = hermetic_pair();
    app.queue.repeat = Repeat::All;
    app.config.repeat = Repeat::All;
    app.autoplay_streaming = true;
    app.config.autoplay_streaming = Some(true);
    let mut engine = engine_with_modes(Repeat::All, true).await;

    let (app_response, app_effects) = app_apply_with_cmds(&mut app, RemoteCommand::CycleRepeat);
    let (engine_response, shutdown, engine_effects) =
        engine.handle_remote(RemoteCommand::CycleRepeat).await;

    assert!(app_response.ok && engine_response.ok && !shutdown);
    assert!(engine_effects.is_empty());
    assert!(matches!(
        app_effects.as_slice(),
        [Cmd::Persist(crate::app::PersistCmd::Config(config))]
            if config.repeat == Repeat::One && config.autoplay_streaming == Some(true)
    ));
    assert_eq!(app.queue.repeat, Repeat::One);
    assert!(app.autoplay_streaming);
    assert_eq!(engine.status().repeat, Repeat::One);
    assert!(engine.status().settings.autoplay_streaming);
    assert_eq!(engine.core_view().config.repeat, Repeat::One);
    assert_eq!(
        engine.core_view().config.autoplay_streaming,
        Some(true),
        "daemon cycle did not persist the legacy-compatible state"
    );
    assert_parity("legacy All+streaming cycle", &app, &engine);
}

#[tokio::test]
async fn local_mode_streaming_is_effectively_off_and_all_toggles_reject_in_lockstep() {
    for initially_enabled in [false, true] {
        let (mut app, mut engine) = hermetic_pair();
        if initially_enabled {
            let command = RemoteCommand::Streaming {
                state: ToggleState::On,
            };
            let app_response = app_apply(&mut app, command.clone());
            let (engine_response, shutdown, _) = engine.handle_remote(command).await;
            assert!(app_response.ok && engine_response.ok && !shutdown);
        }

        app.local_dedicated_mode = true;
        engine.restore_last_mode_for_test(crate::session::LastMode::Local);

        let app_status = app_apply(&mut app, RemoteCommand::Status)
            .status
            .expect("App status");
        let (engine_response, shutdown, effects) =
            engine.handle_remote(RemoteCommand::Status).await;
        let engine_status = engine_response.status.expect("daemon status");
        assert!(!shutdown && effects.is_empty());
        assert!(!app_status.streaming && !engine_status.streaming);
        assert_eq!(app_status.settings.autoplay_streaming, initially_enabled);
        assert_eq!(engine_status.settings.autoplay_streaming, initially_enabled);
        assert_parity("Local effective streaming projection", &app, &engine);

        for command in [
            RemoteCommand::Streaming {
                state: ToggleState::On,
            },
            RemoteCommand::Streaming {
                state: ToggleState::Off,
            },
            RemoteCommand::Streaming {
                state: ToggleState::Toggle,
            },
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::AutoplayStreaming { value: true },
            },
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::AutoplayStreaming { value: false },
            },
        ] {
            let (app_response, app_cmds) = app_apply_with_cmds(&mut app, command.clone());
            let (engine_response, shutdown, engine_effects) = engine.handle_remote(command).await;

            assert!(!app_response.ok && !engine_response.ok && !shutdown);
            assert_eq!(app_response.reason, engine_response.reason);
            assert_eq!(
                app_response.reason.as_deref(),
                Some("streaming_unavailable_in_local_mode")
            );
            assert!(app_cmds.is_empty());
            assert!(engine_effects.is_empty());
            assert_eq!(app.autoplay_streaming, initially_enabled);
            assert_eq!(
                app.config.autoplay_streaming.unwrap_or(false),
                initially_enabled
            );
            let status = engine.status();
            assert!(!status.streaming);
            assert_eq!(status.settings.autoplay_streaming, initially_enabled);
            assert_parity("Local streaming toggle rejection", &app, &engine);
        }
    }
}

#[tokio::test]
async fn restored_local_mode_suppresses_topups_and_exit_restores_them_in_lockstep() {
    use crate::session::{LastMode, SessionCache};

    let config = Config {
        autoplay_streaming: Some(true),
        repeat: Repeat::Off,
        ..Config::default()
    };
    let snapshot = |ids: &[&str], cursor| {
        let mut queue = Queue::default();
        queue.set(ids.iter().map(|id| song(id)).collect(), cursor);
        queue.snapshot()
    };
    let local_queue = snapshot(&["local-seed", "local-next"], 0);
    let normal_queue = snapshot(&["normal-seed", "normal-next"], 0);
    let mut cache = SessionCache::from_last_mode(LastMode::Local);
    cache.local_queue = Some(local_queue.clone());
    cache.normal_queue = Some(normal_queue.clone());

    // Model a fresh App/daemon process restoring a persisted Local session with the user's raw
    // normal-mode autoplay preference still on. Neither owner may start its initial refill.
    let (mut app, mut engine) = hermetic_pair_from_config(config, local_queue.clone());
    app.restore_last_session_from_cache(&cache);
    app.queue.seed_rng(RNG_SEED);
    engine.restore_queue_snapshot(local_queue, RNG_SEED);
    engine.restore_last_mode_for_test(cache.last_mode);

    assert!(app.local_dedicated_mode);
    assert!(app.autoplay_streaming);
    assert_eq!(app.config.autoplay_streaming, Some(true));
    assert_eq!(
        engine.core_view().config.autoplay_streaming,
        Some(true),
        "daemon restart must retain the raw preference"
    );
    assert!(
        engine.initial_effects().is_empty(),
        "a Local daemon restart emitted an initial top-up"
    );
    assert_parity("restored Local session", &app, &engine);

    // Removing the only upcoming row puts both queues at the lowest possible refill boundary.
    // The ordinary maybe-top-up path still runs in both owners, but the effective Local policy
    // must turn it into zero network effects.
    let command = RemoteCommand::QueueRemove { position: 1 };
    let (app_response, app_effects) = app_apply_with_cmds(&mut app, command.clone());
    let (engine_response, shutdown, engine_effects) = engine.handle_remote(command).await;
    assert!(app_response.ok && engine_response.ok && !shutdown);
    assert_eq!(app_response.reason, engine_response.reason);
    assert!(
        app_effects
            .iter()
            .all(|effect| !matches!(effect, Cmd::StreamingFallback { .. })),
        "App emitted a low-queue Local top-up"
    );
    assert!(engine_effects.is_empty(), "daemon emitted a Local top-up");
    assert_parity("Local low-queue top-up suppression", &app, &engine);

    // The forced path is reached by a live streaming-mode edit. Persistence is expected on the
    // App side; only StreamingFallback/EngineEffect would violate the Local boundary.
    let command = RemoteCommand::SetSetting {
        change: RemoteSettingChange::StreamingMode {
            value: crate::streaming::StreamingMode::Discovery,
        },
    };
    let (app_response, app_effects) = app_apply_with_cmds(&mut app, command.clone());
    let (engine_response, shutdown, engine_effects) = engine.handle_remote(command).await;
    assert!(app_response.ok && engine_response.ok && !shutdown);
    assert_eq!(app_response.reason, engine_response.reason);
    assert!(
        app_effects
            .iter()
            .all(|effect| !matches!(effect, Cmd::StreamingFallback { .. })),
        "App emitted a forced Local top-up"
    );
    assert!(
        engine_effects.is_empty(),
        "daemon emitted a forced Local top-up"
    );
    assert_parity("Local forced top-up suppression", &app, &engine);

    // Effective streaming is off, but the saved raw preference still owns the central
    // streaming/repeat invariant. Both owners reject the conflict without rewriting either.
    let (app_response, app_effects) = app_apply_with_cmds(&mut app, RemoteCommand::CycleRepeat);
    let (engine_response, shutdown, engine_effects) =
        engine.handle_remote(RemoteCommand::CycleRepeat).await;
    assert!(!app_response.ok && !engine_response.ok && !shutdown);
    assert_eq!(app_response.reason, engine_response.reason);
    assert_eq!(
        app_response.reason.as_deref(),
        Some("incompatible_playback_modes")
    );
    assert!(app_effects.is_empty() && engine_effects.is_empty());
    assert_eq!(app.queue.repeat, Repeat::Off);
    assert!(app.autoplay_streaming);
    assert_eq!(app.config.autoplay_streaming, Some(true));
    assert_eq!(engine.status().repeat, Repeat::Off);
    assert!(engine.status().settings.autoplay_streaming);
    assert_eq!(engine.core_view().config.autoplay_streaming, Some(true));
    assert_parity("Local repeat conflict rejection", &app, &engine);

    // Drive the App through its real admission-atomic Local exit. The target mode commits before
    // the restored normal track's refill check, so autoplay must become effective immediately.
    let request_exit = crossterm::event::KeyEvent {
        code: crossterm::event::KeyCode::Char('l'),
        modifiers: crossterm::event::KeyModifiers::ALT | crossterm::event::KeyModifiers::SHIFT,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    assert!(app.update(Msg::Key(request_exit)).is_empty());
    let confirm_exit = crossterm::event::KeyEvent {
        code: crossterm::event::KeyCode::Enter,
        modifiers: crossterm::event::KeyModifiers::NONE,
        kind: crossterm::event::KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    };
    let exit_intent = app.update(Msg::Key(confirm_exit));
    let app_exit_effects = admit_app_player_intents_collect(&mut app, exit_intent);
    assert!(!app.local_dedicated_mode);
    assert!(app_exit_effects.iter().any(|effect| {
        matches!(
            effect,
            Cmd::StreamingFallback { seed_video_id, .. } if seed_video_id == "normal-seed"
        )
    }));

    // The daemon restores the same persisted normal queue/mode at its owner boundary. Its next
    // initial refill must now agree with the App exit commit.
    engine.restore_queue_snapshot(normal_queue, RNG_SEED);
    engine.restore_last_mode_for_test(LastMode::Normal);
    let engine_exit_effects = engine.initial_effects();
    assert!(engine_exit_effects.iter().any(|effect| {
        matches!(
            effect,
            EngineEffect::StreamingFallback { seed_video_id, .. }
                if seed_video_id == "normal-seed"
        )
    }));
    assert!(app.streaming_active());
    assert!(engine.status().streaming);
    assert_eq!(app.config.autoplay_streaming, Some(true));
    assert_eq!(engine.core_view().config.autoplay_streaming, Some(true));
    // The App's admitted mode switch actively resumes its restored queue, while this hermetic
    // daemon restart projection deliberately has no player and remains paused. Align that known
    // transport-only baseline after checking both owners' real exit effects above.
    app.playback.paused = engine.status().paused;
    assert_parity("Local exit restores effective streaming", &app, &engine);
}

#[tokio::test]
async fn media_option_commands_keep_both_owners_in_lockstep() {
    let (mut app, mut engine) = hermetic_pair();
    for (step, command) in [
        ("media shuffle on", MediaCommand::SetShuffle(true)),
        ("media shuffle same-value", MediaCommand::SetShuffle(true)),
        ("media shuffle off", MediaCommand::SetShuffle(false)),
        ("media repeat all", MediaCommand::SetRepeat(Repeat::All)),
        ("media repeat one", MediaCommand::SetRepeat(Repeat::One)),
        ("media repeat off", MediaCommand::SetRepeat(Repeat::Off)),
        ("media volume fractional", MediaCommand::SetVolume(0.37)),
        ("media volume upper clamp", MediaCommand::SetVolume(9.0)),
        ("media volume lower clamp", MediaCommand::SetVolume(-3.0)),
        ("media volume NaN", MediaCommand::SetVolume(f64::NAN)),
        (
            "media volume infinity",
            MediaCommand::SetVolume(f64::INFINITY),
        ),
    ] {
        let app_commands = app.update(Msg::Media(command.clone()));
        admit_app_player_intents(&mut app, app_commands);
        let (shutdown, effects) = engine.handle_media(command).await;
        assert!(!shutdown, "{step} requested shutdown");
        assert!(effects.is_empty(), "{step} emitted daemon effects");
        assert_parity(step, &app, &engine);
    }
}

#[tokio::test]
async fn media_repeat_streaming_conflict_rejects_in_lockstep() {
    let (mut app, mut engine) = hermetic_pair();
    app.autoplay_streaming = true;
    app.config.autoplay_streaming = Some(true);
    app.status.text = "before".to_owned();
    app.dirty = false;
    let (response, shutdown, _) = engine
        .handle_remote(RemoteCommand::Streaming {
            state: ToggleState::On,
        })
        .await;
    assert!(response.ok && !shutdown, "test setup must enable streaming");
    assert_parity("media repeat streaming setup", &app, &engine);

    let app_commands = app.update(Msg::Media(MediaCommand::SetRepeat(Repeat::All)));
    let (shutdown, effects) = engine
        .handle_media(MediaCommand::SetRepeat(Repeat::All))
        .await;

    assert!(app_commands.is_empty(), "App rejection emitted effects");
    assert!(!shutdown);
    assert!(effects.is_empty(), "daemon rejection emitted effects");
    assert!(matches!(
        app.status.text.as_str(),
        "Can't use repeat while autoplay is on" | "자동재생 중에는 반복을 켤 수 없어요"
    ));
    assert!(app.dirty, "the App rejection toast must redraw");
    assert_parity("media repeat streaming rejection", &app, &engine);

    // Legacy sessions may restore both modes enabled; turning repeat off must recover
    // the invariant in both owners without disabling streaming.
    let (mut app, _) = hermetic_pair();
    app.autoplay_streaming = true;
    app.config.autoplay_streaming = Some(true);
    app.queue.repeat = Repeat::One;
    let mut engine = engine_with_modes(Repeat::One, true).await;
    let _ = app.update(Msg::Media(MediaCommand::SetRepeat(Repeat::Off)));
    let (shutdown, effects) = engine
        .handle_media(MediaCommand::SetRepeat(Repeat::Off))
        .await;
    assert!(!shutdown && effects.is_empty());
    assert!(app.autoplay_streaming && app.queue.repeat == Repeat::Off);
    assert_parity("media repeat legacy disable", &app, &engine);
}

#[tokio::test]
async fn media_shuffle_and_repeat_are_radio_noops_in_lockstep() {
    let (mut app, mut engine) = hermetic_pair();
    let mut station = Song::remote("radio", "Radio", "", "");
    station.playable = Some(crate::api::PlayableRef::RadioStream {
        url: "https://radio.example/stream".to_owned(),
    });
    let mut queue = Queue::default();
    queue.set(vec![station], 0);
    let snapshot = queue.snapshot();
    app.queue.restore_snapshot(snapshot.clone());
    engine.restore_queue_snapshot(snapshot, RNG_SEED);

    for (step, command) in [
        ("radio media shuffle", MediaCommand::SetShuffle(true)),
        ("radio media repeat", MediaCommand::SetRepeat(Repeat::All)),
    ] {
        assert!(app.update(Msg::Media(command.clone())).is_empty());
        let (shutdown, effects) = engine.handle_media(command).await;
        assert!(!shutdown);
        assert!(effects.is_empty());
        assert_parity(step, &app, &engine);
    }
}

#[tokio::test]
async fn status_snapshots_agree_too() {
    // The v7 surface rides the same states — cheap to pin while we're here. Fields that
    // encode owner identity are excluded.
    let (mut app, mut engine) = hermetic_pair();

    let app_resp = app_apply(&mut app, RemoteCommand::Status);
    let (engine_resp, _, _) = engine.handle_remote(RemoteCommand::Status).await;
    let app_snap = app_resp.status.expect("app status");
    let engine_snap = engine_resp.status.expect("engine status");

    assert_eq!(app_snap.title, engine_snap.title);
    assert_eq!(app_snap.artist, engine_snap.artist);
    assert_eq!(app_snap.position, engine_snap.position);
    assert_eq!(app_snap.total, engine_snap.total);
    assert_eq!(app_snap.volume, engine_snap.volume);
    assert_eq!(app_snap.shuffle, engine_snap.shuffle);
    assert_eq!(app_snap.repeat, engine_snap.repeat);
    assert_eq!(app_snap.queue.len(), engine_snap.queue.len());
}

/// Autoplay's exclusion set is now one shared function (`streaming::exclude_ids`); both
/// owners must project it identically for the same queue + library + streaming config. This
/// locks each owner's wiring (passing its own config/queue/library) as a contract, on a
/// player-path helper the B0 command script never reaches.
#[test]
fn streaming_exclude_ids_matches_across_owners() {
    use crate::library::Library;
    use std::collections::VecDeque;

    let snap = seed_snapshot();
    let library = Library {
        // "a" is also in the seeded queue — it must appear once, from either source.
        favorites: vec![song("fav1"), song("fav2")],
        history: VecDeque::from(vec![song("h1"), song("a"), song("h2")]),
        ..Library::default()
    };

    let mut engine = DaemonEngine::with_state(
        EngineState {
            config: Config::default(),
            station: StationStore::default(),
            library: library.clone(),
            playlists: crate::playlists::Playlists::default(),
            signals: Signals::default(),
        },
        Arc::new(|_event| {}),
    );
    engine.restore_queue_snapshot(snap.clone(), 20260703);

    let mut app = App::new(Config::default().volume);
    app.queue.restore_snapshot(snap);
    app.library = Arc::new(library);

    let mut app_ids = app.streaming_exclude_ids("seed-x");
    let mut eng_ids = engine.streaming_exclude_ids("seed-x");
    app_ids.sort();
    eng_ids.sort();
    assert_eq!(app_ids, eng_ids, "exclude sets diverged across owners");
    assert!(app_ids.iter().any(|id| id == "seed-x"), "seed excluded");
    assert!(app_ids.iter().any(|id| id == "a"), "queued track excluded");
    assert!(
        app_ids.iter().any(|id| id == "h1"),
        "recent history excluded"
    );
}
