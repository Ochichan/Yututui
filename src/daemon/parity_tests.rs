//! App↔DaemonEngine parity: the convention→contract upgrade (docs/gui/10 §4).
//!
//! One shared script of remote commands is applied to BOTH owner implementations; after
//! every command the two owners must project **equal** `PlayerModel`/`QueueModel` wire
//! models (and agree on the reply's `ok`/`reason`). This turns "the engine is kept in
//! sync with the reducer by convention" (docs/gui/01 §4) into an executable contract,
//! and it is the safety net every S1–S6 extraction step runs against: a parity failure
//! after an extraction means the extraction changed behavior.
//!
//! B0 scope and its known limits, on purpose:
//! - Commands that load a track (`QueuePlay`, `Next` into playback, seeks) need a player
//!   stub — they join the script with S1, driven via `handle_player_event` on the engine
//!   side. The B0 script covers the settings/toggle/queue-membership surface.
//! - `position_epoch` counts and `elapsed_ms` interpolation are normalized out: without
//!   a player both sit at rest, and epoch *cadence* (not equality) is an S1 concern.
//! - The baseline is aligned before the script runs (volume and paused-at-rest differ
//!   by construction today: `App::new(volume)` vs config-seeded engine, `paused: false`
//!   vs `true` with nothing loaded). The script then must keep them equal.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::api::Song;
use crate::app::{App, Cmd, Msg, PlayerControl, PlayerMsg};
use crate::config::Config;
use crate::library::Library;
use crate::queue::{Queue, QueueSnapshot, Repeat};
use crate::remote::proto::{
    GuiSettingChange, InstanceMode, PlayerModel, QueueModel, RateChange, RemoteCommand,
    RemoteResponse, RemoteSettingChange, ServerFrame, ToggleState, Topic,
};
use crate::remote::publish;
use crate::remote::{SessionLine, SessionTuning, test_command_reply, test_register};
use crate::signals::Signals;
use crate::station::StationStore;
use crate::util::delivery::DeliveryReceipt;

use super::engine::{DaemonEngine, EngineState};

const RNG_SEED: u64 = 20260703;

fn song(id: &str) -> Song {
    Song::remote(id, format!("title-{id}"), format!("artist-{id}"), "3:45")
}

fn seed_snapshot() -> QueueSnapshot {
    let mut queue = Queue::default();
    queue.set(vec![song("a"), song("b"), song("c"), song("d")], 1);
    queue.snapshot()
}

/// Both owners on identical default config + the same restored queue, with the known
/// baseline differences aligned (see module docs).
fn hermetic_pair() -> (App, DaemonEngine) {
    let snap = seed_snapshot();

    let mut engine = DaemonEngine::with_state(
        EngineState {
            config: Config::default(),
            station: StationStore::default(),
            library: Library::default(),
            playlists: crate::playlists::Playlists::default(),
            signals: Signals::default(),
        },
        Arc::new(|_event| {}),
    );
    engine.restore_queue_snapshot(snap.clone(), RNG_SEED);

    let mut app = App::new(Config::default().volume);
    app.queue.restore_snapshot(snap);
    // Shuffle policy is under test; shuffle *randomness* is not. Same seed both sides
    // so the shared script's ToggleShuffle draws identical permutations.
    app.queue.seed_rng(RNG_SEED);
    // The engine starts paused with nothing loaded; the App's transport defaults to
    // unpaused-at-rest. Align the baseline — keeping them equal afterwards is the
    // script's job (and S1's, permanently).
    app.playback.paused = true;

    (app, engine)
}

/// Admit every two-phase player intent emitted by a reducer turn, including intents emitted by
/// an accepted commit. Other side effects remain outside this state-projection parity harness.
fn admit_app_player_intents(app: &mut App, commands: Vec<Cmd>) {
    let mut pending = std::collections::VecDeque::from(commands);
    while let Some(command) = pending.pop_front() {
        if let Cmd::PlayerControl(PlayerControl::Intent(intent)) = command {
            pending.extend(crate::runtime::player_delivery::settle_player_intent(
                app,
                *intent,
                Ok(DeliveryReceipt::Enqueued),
            ));
        }
    }
}

/// Apply one command to the App through its real path (`Msg::Remote` → `apply_remote`) and
/// deterministically model a player lane that accepts every typed intent.
fn app_apply(app: &mut App, cmd: RemoteCommand) -> RemoteResponse {
    app_apply_with_cmds(app, cmd).0
}

fn app_apply_with_cmds(app: &mut App, cmd: RemoteCommand) -> (RemoteResponse, Vec<Cmd>) {
    let (tx, mut rx) = oneshot::channel();
    let commands = app.update(Msg::Remote(cmd, tx.into()));
    let commands = if commands
        .iter()
        .any(|command| matches!(command, Cmd::PlayerControl(PlayerControl::Intent(_))))
    {
        admit_app_player_intents(app, commands);
        Vec::new()
    } else {
        commands
    };
    (
        rx.try_recv()
            .expect("remote reply is ready after accepted player intents settle"),
        commands,
    )
}

fn models_of(view: &publish::CoreView<'_>) -> (PlayerModel, QueueModel) {
    (publish::player_model(view), publish::queue_model(view))
}

/// Strip the fields that legitimately differ across owners in a hermetic B0 harness.
fn normalize(player: &mut PlayerModel, queue: &mut QueueModel) {
    player.owner_mode = InstanceMode::StandaloneTui;
    player.position_epoch = 0;
    player.elapsed_ms = None;
    // Owner-global rev counters are process-wide; two live queues in one test process
    // never share values. Contents equality is the contract here.
    queue.rev = 0;
}

fn assert_parity(step: &str, app: &App, engine: &DaemonEngine) {
    let (mut app_player, mut app_queue) = models_of(&app.core_view());
    let (mut eng_player, mut eng_queue) = models_of(&engine.core_view());
    normalize(&mut app_player, &mut app_queue);
    normalize(&mut eng_player, &mut eng_queue);
    assert_eq!(app_player, eng_player, "PlayerModel diverged after {step}");
    assert_eq!(app_queue, eng_queue, "QueueModel diverged after {step}");
}

async fn engine_with_modes(repeat: Repeat, streaming: bool) -> DaemonEngine {
    let (_, mut engine) = hermetic_pair();
    if streaming {
        let (response, shutdown, _) = engine
            .handle_remote(RemoteCommand::Streaming {
                state: ToggleState::On,
            })
            .await;
        assert!(response.ok && !shutdown, "test setup must enable streaming");
    }
    let mut snapshot = seed_snapshot();
    snapshot.repeat = repeat;
    engine.restore_queue_snapshot(snapshot, RNG_SEED);
    engine
}

fn gui_repeat(repeat: Repeat) -> RemoteCommand {
    RemoteCommand::Apply {
        change: GuiSettingChange {
            group: "playback".to_owned(),
            field: "repeat".to_owned(),
            value: serde_json::to_value(repeat).unwrap(),
        },
    }
}

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

        assert_parity(&step, &app, &engine);
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
    assert_parity("stale revision-checked play", &app, &engine);

    // A fresh revision must pass the optimistic-concurrency gate and reach the shared queue
    // index validation. Use an invalid position here so the hermetic parity harness does not
    // spawn a real daemon mpv merely to prove the revision gate was accepted.
    let app_rev = app.core_view().queue.rev();
    let engine_rev = engine.core_view().queue.rev();
    let invalid_position = app.core_view().queue.len();
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
    app.library = library;

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
