//! Test-only fixtures and command contracts shared by App↔daemon parity tests.

use std::sync::Arc;

use tokio::sync::oneshot;

use crate::api::Song;
use crate::app::{App, Cmd, Msg, PlayerControl};
use crate::config::Config;
use crate::library::Library;
use crate::queue::{Queue, QueueSnapshot, Repeat};
use crate::remote::proto::{
    InstanceMode, PlayerModel, QueueModel, RemoteCommand, RemoteResponse, RemoteSettingChange,
    ToggleState,
};
use crate::remote::publish;
use crate::signals::Signals;
use crate::station::StationStore;
use crate::util::delivery::DeliveryReceipt;

use super::super::engine::{DaemonEngine, EngineState};

pub(super) const RNG_SEED: u64 = 20260703;

/// The owner boundary a command exercises in the parity suite.
///
/// This classifier is intentionally exhaustive. Adding a wire command therefore requires an
/// explicit decision about whether it is shared, rejected by the standalone owner, intercepted
/// before either reducer, or intentionally owner-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CommandParityClass {
    /// Both reducers implement the command and must leave `position_epoch` unchanged.
    SharedStableEpoch,
    /// Both reducers implement the command, but an accepted transport transition may rebase
    /// position and advance `position_epoch`.
    SharedMayRebase,
    /// The standalone App deliberately rejects this daemon-hosted command.
    StandaloneRejected,
    /// Both owner loops intercept this command before their playback reducers.
    BothOwnerLoopIntercepted,
    /// The two owners intentionally have different lifecycle or mode semantics.
    OwnerSpecific,
}

pub(super) fn command_parity_class(command: &RemoteCommand) -> CommandParityClass {
    use CommandParityClass::{
        BothOwnerLoopIntercepted, OwnerSpecific, SharedMayRebase, SharedStableEpoch,
        StandaloneRejected,
    };

    match command {
        RemoteCommand::Next
        | RemoteCommand::Prev
        | RemoteCommand::TogglePause
        | RemoteCommand::SeekBack
        | RemoteCommand::SeekForward
        | RemoteCommand::SeekTo { .. }
        | RemoteCommand::QueuePlay { .. }
        | RemoteCommand::QueueRemove { .. }
        | RemoteCommand::QueuePlayIfRevision { .. }
        | RemoteCommand::QueueRemoveIfRevision { .. }
        | RemoteCommand::ResumeSession => SharedMayRebase,
        RemoteCommand::VolumeUp
        | RemoteCommand::VolumeDown
        | RemoteCommand::SetVolume { .. }
        | RemoteCommand::ToggleShuffle
        | RemoteCommand::CycleRepeat
        | RemoteCommand::Streaming { .. }
        | RemoteCommand::Sleep { .. }
        | RemoteCommand::Status
        | RemoteCommand::QueueMove { .. }
        | RemoteCommand::QueueClearUpcoming { .. } => SharedStableEpoch,
        RemoteCommand::SetSetting { change } => match change {
            RemoteSettingChange::AutoplayStreaming { .. }
            | RemoteSettingChange::StreamingMode { .. }
            | RemoteSettingChange::StreamingSource { .. }
            | RemoteSettingChange::Speed { .. }
            | RemoteSettingChange::SeekSeconds { .. }
            | RemoteSettingChange::Normalize { .. }
            | RemoteSettingChange::Gapless { .. }
            | RemoteSettingChange::AiEnabled { .. } => SharedStableEpoch,
            RemoteSettingChange::RadioMode { .. } => OwnerSpecific,
        },
        RemoteCommand::ExportPersonalData { .. }
        | RemoteCommand::SyncNow
        | RemoteCommand::SyncRevokeDevice { .. } => BothOwnerLoopIntercepted,
        RemoteCommand::Quit => OwnerSpecific,
        RemoteCommand::Play { .. } | RemoteCommand::Enqueue { .. } => StandaloneRejected,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PositionEpochs {
    app: u64,
    daemon: u64,
}

impl PositionEpochs {
    pub(super) fn capture(app: &App, engine: &DaemonEngine) -> Self {
        Self {
            app: app.core_view().position_epoch,
            daemon: engine.core_view().position_epoch,
        }
    }

    pub(super) fn assert_delta(self, step: &str, after: Self, expected: u64) {
        assert_eq!(
            after.app,
            self.app.wrapping_add(expected),
            "App position_epoch delta diverged after {step}"
        );
        assert_eq!(
            after.daemon,
            self.daemon.wrapping_add(expected),
            "daemon position_epoch delta diverged after {step}"
        );
    }
}

pub(super) fn song(id: &str) -> Song {
    Song::remote(id, format!("title-{id}"), format!("artist-{id}"), "3:45")
}

pub(super) fn seed_snapshot() -> QueueSnapshot {
    let mut queue = Queue::default();
    queue.set(vec![song("a"), song("b"), song("c"), song("d")], 1);
    queue.snapshot()
}

/// Stop the parity engine writing through real stores.
///
/// Under `#[cfg(test)]` every store resolves inside one process-wide sandbox, so the engine's
/// `save_session` — which `queue_remove` ends in — targets one `<cache dir>/session.json` shared by
/// the whole suite. On Windows a concurrent replace there can fail, and a daemon command whose save
/// fails does not merely lose a write: `finish_remote_persistence` replaces its success-shaped
/// response with `durability_unconfirmed`. The App owner has no such gate and still reports
/// success, so the parity assertion reads it as the two owners disagreeing about a queue edit when
/// nothing about playback diverged.
///
/// These tests compare projections and responses, never durability; the persistence gate has its
/// own coverage in `engine/persistence_gate_tests.rs`. Same reason and same mechanism as
/// `engine/accounts.rs`, which sets this flag so "saves become no-ops in tests".
fn silence_engine_persistence(engine: &mut DaemonEngine) {
    engine.silence_remote_persistence_for_test();
}

/// Both owners on identical default config + the same restored queue, with the known baseline
/// differences aligned.
pub(super) fn hermetic_pair() -> (App, DaemonEngine) {
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
    silence_engine_persistence(&mut engine);
    engine.restore_queue_snapshot(snap.clone(), RNG_SEED);

    let mut app = App::new(Config::default().volume);
    app.queue.restore_snapshot(snap);
    app.queue.seed_rng(RNG_SEED);
    app.playback.paused = true;

    (app, engine)
}

/// Construct both owners as a fresh process from one persisted config + queue snapshot.
pub(super) fn hermetic_pair_from_config(
    config: Config,
    snap: QueueSnapshot,
) -> (App, DaemonEngine) {
    let mut engine = DaemonEngine::with_state(
        EngineState {
            config: config.clone(),
            station: StationStore::default(),
            library: Library::default(),
            playlists: crate::playlists::Playlists::default(),
            signals: Signals::default(),
        },
        Arc::new(|_event| {}),
    );
    silence_engine_persistence(&mut engine);
    engine.restore_queue_snapshot(snap.clone(), RNG_SEED);

    let mut app = App::new(config.volume);
    app.apply_config(&config);
    app.queue.restore_snapshot(snap);
    app.queue.seed_rng(RNG_SEED);
    app.playback.paused = true;

    (app, engine)
}

/// Admit every two-phase player intent emitted by a reducer turn, including intents emitted by
/// an accepted commit. Other side effects remain outside this state-projection parity harness.
pub(super) fn admit_app_player_intents(app: &mut App, commands: Vec<Cmd>) {
    let _ = admit_app_player_intents_collect(app, commands);
}

/// Admit a complete player-intent chain and retain every non-player effect emitted by its
/// accepted commits.
pub(super) fn admit_app_player_intents_collect(app: &mut App, commands: Vec<Cmd>) -> Vec<Cmd> {
    let mut pending = std::collections::VecDeque::from(commands);
    let mut effects = Vec::new();
    while let Some(command) = pending.pop_front() {
        match command {
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => {
                pending.extend(crate::runtime::player_delivery::settle_player_intent(
                    app,
                    *intent,
                    Ok(DeliveryReceipt::Enqueued),
                ));
            }
            effect => effects.push(effect),
        }
    }
    effects
}

/// Apply one command to the App through its real path (`Msg::Remote` → `apply_remote`) and
/// deterministically model a player lane that accepts every typed intent.
pub(super) fn app_apply(app: &mut App, command: RemoteCommand) -> RemoteResponse {
    app_apply_with_cmds(app, command).0
}

pub(super) fn app_apply_with_cmds(
    app: &mut App,
    command: RemoteCommand,
) -> (RemoteResponse, Vec<Cmd>) {
    let (tx, mut rx) = oneshot::channel();
    let commands = app.update(Msg::Remote(command, tx.into()));
    let commands = admit_app_player_intents_collect(app, commands);
    (
        rx.try_recv()
            .expect("remote reply is ready after accepted player intents settle"),
        commands,
    )
}

fn models_of(view: &publish::CoreView<'_>) -> (PlayerModel, QueueModel) {
    (publish::player_model(view), publish::queue_model(view))
}

/// Strip fields that legitimately differ across owners in a hermetic harness.
fn normalize(player: &mut PlayerModel, queue: &mut QueueModel) {
    player.owner_mode = InstanceMode::StandaloneTui;
    player.position_epoch = 0;
    player.elapsed_ms = None;
    // Owner-global rev counters are process-wide; two live queues in one test process never
    // share values. Contents equality is the contract here.
    queue.rev = 0;
}

/// Asserts both owners accepted, naming the side that refused and why.
///
/// `assert!(app_resp.ok && engine_resp.ok)` says nothing about which owner rejected the command
/// or on what grounds, which is exactly the question when it fires on CI and not on any machine
/// you can attach to. The revision-guarded cases have exactly two ways to refuse — `stale_rev`
/// when the guard saw a queue revision other than the one handed to it, and `queue_index` when
/// the queue was shorter than the position — so the reason alone separates them. The live queue
/// revision and length are printed too, because a mismatch says whether the queue moved under
/// the test between reading the revision and issuing the command.
pub(super) fn assert_accepted(
    step: &str,
    app: &App,
    engine: &DaemonEngine,
    app_resp: &RemoteResponse,
    engine_resp: &RemoteResponse,
) {
    if app_resp.ok && engine_resp.ok {
        return;
    }
    let app_queue = app.core_view().queue;
    let engine_queue = engine.core_view().queue;
    panic!(
        "{step}: both owners had to accept.\n  \
         App:    ok={} reason={:?} queue rev={} len={}\n  \
         daemon: ok={} reason={:?} queue rev={} len={}",
        app_resp.ok,
        app_resp.reason,
        app_queue.rev(),
        app_queue.len(),
        engine_resp.ok,
        engine_resp.reason,
        engine_queue.rev(),
        engine_queue.len(),
    );
}

pub(super) fn assert_parity(step: &str, app: &App, engine: &DaemonEngine) {
    let (mut app_player, mut app_queue) = models_of(&app.core_view());
    let (mut daemon_player, mut daemon_queue) = models_of(&engine.core_view());
    normalize(&mut app_player, &mut app_queue);
    normalize(&mut daemon_player, &mut daemon_queue);
    assert_eq!(
        app_player, daemon_player,
        "PlayerModel diverged after {step}"
    );
    assert_eq!(app_queue, daemon_queue, "QueueModel diverged after {step}");
}

pub(super) async fn engine_with_modes(repeat: Repeat, streaming: bool) -> DaemonEngine {
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

#[cfg(test)]
mod persistence_isolation_tests {
    use super::*;

    /// `queue_remove` ends in `save_session`, and under `#[cfg(test)]` that targets the one
    /// `<cache dir>/session.json` the whole suite shares. On Windows a concurrent replace there can
    /// fail, and `finish_remote_persistence` then answers `durability_unconfirmed` instead of the
    /// command's success — which the parity assertion reads as the two owners disagreeing.
    ///
    /// Injecting the failure proves the seam rather than inferring it: with saves silenced the
    /// engine never attempts one, so a failure that would otherwise rewrite the response cannot.
    /// Asserting on the shared file itself would measure whatever else the suite is writing.
    #[tokio::test]
    async fn a_silenced_engine_survives_a_session_save_failure() {
        let _fail =
            crate::daemon::engine::fail_store_saves_for_test(crate::persist::StoreKind::Session);

        let (_app, mut engine) = hermetic_pair();
        let (response, _shutdown, _effects) = engine
            .handle_remote(RemoteCommand::QueueRemove { position: 0 })
            .await;

        assert!(response.ok, "{response:?}");
        assert_ne!(
            response.reason.as_deref(),
            Some("durability_unconfirmed"),
            "the parity engine still persisted, so a failed save rewrote its response"
        );
    }
}
