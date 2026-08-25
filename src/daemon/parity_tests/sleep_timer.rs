//! Sleep-timer parity: both owners arm/cancel the shared core state machine from the
//! same remote command and must report the same countdown through their status snapshots.

use crate::remote::proto::RemoteCommand;

use super::harness::*;

#[tokio::test]
async fn sleep_timer_status_agrees_across_owners() {
    let (mut app, mut engine) = hermetic_pair();

    let app_resp = app_apply(&mut app, RemoteCommand::Sleep { minutes: Some(30) });
    assert!(app_resp.ok, "app arm: {app_resp:?}");
    let (engine_resp, _, _) = engine
        .handle_remote(RemoteCommand::Sleep { minutes: Some(30) })
        .await;
    assert!(engine_resp.ok, "daemon arm: {engine_resp:?}");

    let app_snap = app_apply(&mut app, RemoteCommand::Status)
        .status
        .expect("app status");
    let (engine_resp, _, _) = engine.handle_remote(RemoteCommand::Status).await;
    let engine_snap = engine_resp.status.expect("engine status");
    let (Some(app_remaining), Some(engine_remaining)) = (
        app_snap.sleep_remaining_secs,
        engine_snap.sleep_remaining_secs,
    ) else {
        panic!("both owners must report an armed timer after a shared arm command");
    };
    // Both owners arm against `Instant::now()`; the whole-second floors may straddle a
    // boundary by one. Anything more is real drift.
    assert!(
        app_remaining.abs_diff(engine_remaining) <= 1,
        "sleep countdown drift: app {app_remaining}s vs daemon {engine_remaining}s"
    );

    let app_resp = app_apply(&mut app, RemoteCommand::Sleep { minutes: Some(0) });
    assert!(app_resp.ok, "app cancel: {app_resp:?}");
    let (engine_resp, _, _) = engine
        .handle_remote(RemoteCommand::Sleep { minutes: Some(0) })
        .await;
    assert!(engine_resp.ok, "daemon cancel: {engine_resp:?}");
    let app_snap = app_apply(&mut app, RemoteCommand::Status)
        .status
        .expect("app status");
    let (engine_resp, _, _) = engine.handle_remote(RemoteCommand::Status).await;
    assert!(
        app_snap.sleep_remaining_secs.is_none(),
        "app must clear the countdown on cancel"
    );
    assert!(
        engine_resp
            .status
            .expect("engine status")
            .sleep_remaining_secs
            .is_none(),
        "daemon must clear the countdown on cancel"
    );
}
