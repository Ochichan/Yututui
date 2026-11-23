use tokio::sync::mpsc::Receiver;
use yututui_core::ab_loop::AbLoopAction;

use crate::app::{App, Msg};
use crate::daemon::engine::DaemonEngine;
use crate::player::{PlayerCmd, SeekPrecision};
use crate::remote::proto::{AbLoopModel, RemoteCommand, RemoteResponse};

use super::harness::{PositionEpochs, admit_app_player_intents, assert_parity, hermetic_pair};

#[derive(Debug, PartialEq)]
enum Sent {
    SetAbA(serde_json::Value),
    SetAbB(serde_json::Value),
    Seek(f64, SeekPrecision),
    Other,
}

fn summarize(command: &PlayerCmd) -> Sent {
    match command {
        PlayerCmd::SetProperty { name, value } if name == "ab-loop-a" => {
            Sent::SetAbA(value.clone())
        }
        PlayerCmd::SetProperty { name, value } if name == "ab-loop-b" => {
            Sent::SetAbB(value.clone())
        }
        PlayerCmd::SeekAbsolute { seconds, precision } => Sent::Seek(*seconds, *precision),
        _ => Sent::Other,
    }
}

struct Outcome {
    app_reply: RemoteResponse,
    engine_reply: RemoteResponse,
    app_cmds: Vec<Sent>,
    engine_cmds: Vec<Sent>,
}

async fn send_ab(
    app: &mut App,
    engine: &mut DaemonEngine,
    engine_player: &mut Receiver<PlayerCmd>,
    action: AbLoopAction,
) -> Outcome {
    let command = RemoteCommand::AbLoop { action };
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    let commands = app.update(Msg::Remote(command.clone(), reply_tx.into()));
    let app_cmds = commands
        .iter()
        .flat_map(crate::app::Cmd::player_commands)
        .map(summarize)
        .collect();
    admit_app_player_intents(app, commands);
    let app_reply = reply_rx.try_recv().expect("App ab_loop reply");
    let (engine_reply, shutdown, effects) = engine.handle_remote(command).await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    let mut engine_cmds = Vec::new();
    for _ in 0..8 {
        tokio::task::yield_now().await;
        while let Ok(command) = engine_player.try_recv() {
            engine_cmds.push(summarize(&command));
        }
    }
    Outcome {
        app_reply,
        engine_reply,
        app_cmds,
        engine_cmds,
    }
}

#[tokio::test]
async fn ab_loop_cycle_and_explicit_seek_stay_in_parity() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 30.0, 240.0);
    let mut engine_player = engine.install_seek_parity_player("b", 30.0, 240.0);
    let epochs = PositionEpochs::capture(&app, &engine);

    // 1. Cycle sets point A at 30.0 with no seek / epoch bump.
    let set_a = send_ab(
        &mut app,
        &mut engine,
        &mut engine_player,
        AbLoopAction::Cycle,
    )
    .await;
    assert!(set_a.app_reply.ok && set_a.engine_reply.ok);
    let expected_a = vec![
        Sent::SetAbA(serde_json::Value::from(30.0)),
        Sent::SetAbB(serde_json::Value::String("no".to_owned())),
    ];
    assert_eq!(set_a.app_cmds, expected_a);
    assert_eq!(set_a.engine_cmds, expected_a);
    epochs.assert_delta("set A", PositionEpochs::capture(&app, &engine), 0);
    assert_parity("set A", &app, &engine);
    assert_eq!(
        crate::remote::publish::player_model(&engine.core_view()).ab_loop,
        Some(AbLoopModel {
            a_ms: 30_000,
            b_ms: None
        })
    );

    // 2. Explicit SetB behind playhead (at 60s, set B=45s) sets A=30, B=45 and seeks to A=30.
    app.install_seek_parity_state("b", 60.0, 240.0);
    engine.set_seek_parity_position(60.0);
    let epochs = PositionEpochs::capture(&app, &engine);
    let set_b = send_ab(
        &mut app,
        &mut engine,
        &mut engine_player,
        AbLoopAction::SetB {
            seconds: Some(45.0),
        },
    )
    .await;
    assert!(set_b.app_reply.ok && set_b.engine_reply.ok);
    let expected_b = vec![
        Sent::SetAbA(serde_json::Value::from(30.0)),
        Sent::SetAbB(serde_json::Value::from(45.0)),
        Sent::Seek(30.0, SeekPrecision::Exact),
    ];
    assert_eq!(set_b.app_cmds, expected_b);
    assert_eq!(set_b.engine_cmds, expected_b);
    epochs.assert_delta(
        "set B behind playhead",
        PositionEpochs::capture(&app, &engine),
        1,
    );
    assert_parity("set B active", &app, &engine);
    assert_eq!(
        set_b.engine_reply.status.as_ref().and_then(|s| s.ab_loop),
        Some(AbLoopModel {
            a_ms: 30_000,
            b_ms: Some(45_000)
        })
    );

    // 3. Cycle again clears the loop with zero epoch bumps.
    let epochs = PositionEpochs::capture(&app, &engine);
    let cleared = send_ab(
        &mut app,
        &mut engine,
        &mut engine_player,
        AbLoopAction::Cycle,
    )
    .await;
    assert!(cleared.app_reply.ok && cleared.engine_reply.ok);
    let expected_clear = vec![
        Sent::SetAbA(serde_json::Value::String("no".to_owned())),
        Sent::SetAbB(serde_json::Value::String("no".to_owned())),
    ];
    assert_eq!(cleared.app_cmds, expected_clear);
    assert_eq!(cleared.engine_cmds, expected_clear);
    epochs.assert_delta("clear", PositionEpochs::capture(&app, &engine), 0);
    assert_parity("cleared", &app, &engine);
    assert_eq!(
        crate::remote::publish::player_model(&engine.core_view()).ab_loop,
        None
    );
}

#[tokio::test]
async fn ab_loop_refusals_match_across_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 15.0, 180.0);
    let mut engine_player = engine.install_seek_parity_player("b", 15.0, 180.0);
    let epochs = PositionEpochs::capture(&app, &engine);

    // SetB before A refuses with ab_loop_missing_a.
    let missing_a = send_ab(
        &mut app,
        &mut engine,
        &mut engine_player,
        AbLoopAction::SetB {
            seconds: Some(30.0),
        },
    )
    .await;
    for reply in [&missing_a.app_reply, &missing_a.engine_reply] {
        assert!(!reply.ok);
        assert_eq!(reply.reason.as_deref(), Some("ab_loop_missing_a"));
    }
    assert!(missing_a.app_cmds.is_empty() && missing_a.engine_cmds.is_empty());
    epochs.assert_delta("missing A", PositionEpochs::capture(&app, &engine), 0);

    // Set A at 15s, then try Cycle at 15.2s -> ab_loop_too_short.
    let _ = send_ab(
        &mut app,
        &mut engine,
        &mut engine_player,
        AbLoopAction::Cycle,
    )
    .await;
    app.install_seek_parity_state("b", 15.2, 180.0);
    engine.set_seek_parity_position(15.2);
    let too_short = send_ab(
        &mut app,
        &mut engine,
        &mut engine_player,
        AbLoopAction::Cycle,
    )
    .await;
    for reply in [&too_short.app_reply, &too_short.engine_reply] {
        assert!(!reply.ok);
        assert_eq!(reply.reason.as_deref(), Some("ab_loop_too_short"));
    }
    assert!(too_short.app_cmds.is_empty() && too_short.engine_cmds.is_empty());
    assert_parity("after refusal", &app, &engine);
}

#[tokio::test]
async fn ab_loop_auto_wrap_bumps_position_epoch_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 30.0, 240.0);
    let mut engine_player = engine.install_seek_parity_player("b", 30.0, 240.0);

    let _ = send_ab(
        &mut app,
        &mut engine,
        &mut engine_player,
        AbLoopAction::SetA {
            seconds: Some(30.0),
        },
    )
    .await;
    let _ = send_ab(
        &mut app,
        &mut engine,
        &mut engine_player,
        AbLoopAction::SetB {
            seconds: Some(45.0),
        },
    )
    .await;

    // Normal forward tick from 30.0 -> 44.6 must not bump position_epoch.
    let epochs = PositionEpochs::capture(&app, &engine);
    let _ = app.update(Msg::Player(crate::app::PlayerMsg::TimePos(44.6)));
    let _ = engine
        .handle_player_event(crate::player::PlayerEvent::TimePos(44.6))
        .await;
    epochs.assert_delta(
        "forward tick inside loop",
        PositionEpochs::capture(&app, &engine),
        0,
    );

    // Automatic mpv wrap from 44.6 -> 30.1 must bump position_epoch by 1 in both owners.
    let epochs = PositionEpochs::capture(&app, &engine);
    let _ = app.update(Msg::Player(crate::app::PlayerMsg::TimePos(30.1)));
    let _ = engine
        .handle_player_event(crate::player::PlayerEvent::TimePos(30.1))
        .await;
    epochs.assert_delta(
        "automatic A-B loop wrap",
        PositionEpochs::capture(&app, &engine),
        1,
    );
    assert_parity("after automatic wrap", &app, &engine);
}
