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
use crate::app::{App, Msg};
use crate::config::Config;
use crate::library::Library;
use crate::queue::{Queue, QueueSnapshot};
use crate::remote::proto::{
    InstanceMode, PlayerModel, QueueModel, RemoteCommand, RemoteResponse, RemoteSettingChange,
    ToggleState,
};
use crate::remote::publish;
use crate::signals::Signals;
use crate::station::StationStore;

use super::engine::{DaemonEngine, EngineState};

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
            signals: Signals::default(),
        },
        Arc::new(|_event| {}),
    );
    const RNG_SEED: u64 = 20260703;
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

/// Apply one command to the App through its real path (`Msg::Remote` → `apply_remote`).
fn app_apply(app: &mut App, cmd: RemoteCommand) -> RemoteResponse {
    let (tx, mut rx) = oneshot::channel();
    // Side-effect Cmds are deliberately dropped: parity compares state projections;
    // effect parity (which PlayerCmds each owner emits) is an S1 extension.
    let _cmds = app.update(Msg::Remote(cmd, tx));
    rx.try_recv().expect("apply_remote replies synchronously")
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
        RemoteCommand::Streaming {
            state: ToggleState::Toggle,
        },
        RemoteCommand::ToggleShuffle, // back to natural order
        RemoteCommand::CycleRepeat,   // Off → All → One → Off completes
    ]
}

#[tokio::test]
async fn shared_script_keeps_app_and_engine_projections_equal() {
    let (mut app, mut engine) = hermetic_pair();
    assert_parity("baseline", &app, &engine);

    for (index, cmd) in b0_script().into_iter().enumerate() {
        let step = format!("step {index}: {cmd:?}");

        let app_resp = app_apply(&mut app, cmd.clone());
        let (engine_resp, shutdown, _effects) = engine.handle_remote(cmd).await;
        assert!(!shutdown, "{step}: script must not shut the engine down");

        assert_eq!(
            app_resp.ok, engine_resp.ok,
            "{step}: owners disagree on ok (app: {app_resp:?}, engine: {engine_resp:?})"
        );
        assert_eq!(
            app_resp.reason, engine_resp.reason,
            "{step}: owners disagree on the machine reason code"
        );

        assert_parity(&step, &app, &engine);
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
