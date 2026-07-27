//! App↔daemon parity for the personal-sync owner boundary.
//!
//! `SyncNow` and `SyncRevokeDevice` are classified `BothOwnerLoopIntercepted`: neither
//! playback reducer ever sees them, so the generic parity sweep over `RemoteCommand` cannot
//! reach them. They are instead implemented twice — `app::personal_sync` (the standalone TUI
//! owner) and `daemon::personal_sync` (the daemon owner) — and each side shipped with its own
//! unit tests asserting its own reply strings. Nothing asserted that the two agree.
//!
//! These tests drive both owners from the same logical state with the same command and pin the
//! replies to each other, so changing one side's rejection contract fails here instead of
//! silently diverging the two owners.

use tokio::sync::oneshot;

use crate::app::App;
use crate::app::Msg;
use crate::personal_state::DeviceId;
use crate::player::lifetime::ShutdownLatch;
use crate::remote::proto::{RemoteCommand, RemoteResponse};

use super::super::DaemonEventSender;
use super::super::personal_sync::PersonalSync;

/// Both owners answer synchronously on the rejection paths under test, so a `try_recv` here
/// is a real assertion: a reply that had to wait for background work would fail it.
fn app_reply(app: &mut App, command: RemoteCommand) -> (RemoteResponse, usize) {
    let (tx, mut rx) = oneshot::channel();
    let commands = app.update(Msg::Remote(command, tx.into()));
    let response = rx
        .try_recv()
        .expect("App answers personal-sync rejections without deferring");
    (response, commands.len())
}

fn daemon_events() -> (
    DaemonEventSender,
    tokio::sync::mpsc::Receiver<super::super::DaemonEvent>,
) {
    let (raw, rx) =
        crate::util::backpressure::bounded_channel(crate::util::backpressure::DAEMON_EVENT_QUEUE);
    (DaemonEventSender::new(raw), rx)
}

/// Drives the daemon owner through the exact entry point `daemon::mod` uses for these
/// commands, and reports whether it published any event (its analogue of App emitting a `Cmd`).
fn daemon_reply(host: &mut PersonalSync, command: RemoteCommand) -> (RemoteResponse, bool) {
    let (events, mut rx) = daemon_events();
    let shutdown = ShutdownLatch::new();
    let mut engine = super::super::engine::tests::engine_with_queue(&[]);
    let (tx, mut response) = oneshot::channel();

    host.start_command(command, tx.into(), &mut engine, &events, &shutdown);

    let response = response
        .try_recv()
        .expect("daemon answers personal-sync rejections without deferring");
    (response, rx.try_recv().is_ok())
}

fn assert_reply_parity(step: &str, app: &RemoteResponse, daemon: &RemoteResponse) {
    assert_eq!(
        app.ok, daemon.ok,
        "{step}: owners disagree on whether the command was accepted"
    );
    assert_eq!(
        app.reason, daemon.reason,
        "{step}: owners disagree on the rejection reason"
    );
}

#[tokio::test]
async fn unconfigured_sync_now_is_rejected_identically_by_both_owners() {
    let mut app = App::new(50);
    let mut host = PersonalSync::default();

    let (app_response, app_commands) = app_reply(&mut app, RemoteCommand::SyncNow);
    let (daemon_response, daemon_published) = daemon_reply(&mut host, RemoteCommand::SyncNow);

    assert_reply_parity("unconfigured sync now", &app_response, &daemon_response);
    assert_eq!(
        app_response.reason.as_deref(),
        Some("sync_not_configured"),
        "the shared rejection contract moved; update both owners together"
    );

    // Neither owner may start work it cannot finish.
    assert_eq!(
        app_commands, 0,
        "App queued work for an unconfigured device"
    );
    assert!(
        !daemon_published,
        "daemon published an event for an unconfigured device"
    );
    assert!(!app.personal_state.sync.in_progress);
}

#[tokio::test]
async fn unconfigured_revoke_is_rejected_identically_by_both_owners() {
    let mut app = App::new(50);
    let mut host = PersonalSync::default();
    let command = || RemoteCommand::SyncRevokeDevice {
        device_id: "device-b".to_owned(),
    };

    let (app_response, app_commands) = app_reply(&mut app, command());
    let (daemon_response, daemon_published) = daemon_reply(&mut host, command());

    assert_reply_parity("unconfigured revoke", &app_response, &daemon_response);
    assert_eq!(app_commands, 0);
    assert!(!daemon_published);
}

#[tokio::test]
async fn malformed_device_id_is_rejected_identically_before_any_owner_work() {
    let mut app = App::new(50);
    let mut host = PersonalSync::default();
    // Empty is not a valid canonical device id, and both owners must reject it by the same
    // name rather than surfacing it as a generic sync failure.
    let command = || RemoteCommand::SyncRevokeDevice {
        device_id: String::new(),
    };

    let (app_response, app_commands) = app_reply(&mut app, command());
    let (daemon_response, daemon_published) = daemon_reply(&mut host, command());

    assert_reply_parity("malformed device id", &app_response, &daemon_response);
    assert_eq!(
        app_response.reason.as_deref(),
        Some("bad_device_id"),
        "the shared malformed-id contract moved; update both owners together"
    );
    assert_eq!(app_commands, 0);
    assert!(!daemon_published);
}

#[tokio::test]
async fn a_second_sync_now_is_rejected_as_busy_by_both_owners() {
    // App: configuring a device id is all it takes for the first request to be admitted.
    let mut app = App::new(50);
    app.personal_state.device_id = Some(DeviceId::new("device-a").unwrap());
    let (first_tx, _first_rx) = oneshot::channel();
    let admitted = app.update(Msg::Remote(RemoteCommand::SyncNow, first_tx.into()));
    assert_eq!(
        admitted.len(),
        1,
        "test setup: the App must admit the first manual sync"
    );
    let (app_response, app_commands) = app_reply(&mut app, RemoteCommand::SyncNow);

    // Daemon: the equivalent state is an owner host that already holds an in-flight request.
    let mut scheduler = crate::sync::AutomaticSyncScheduler::default();
    let token = scheduler
        .begin_manual(std::time::Instant::now())
        .expect("test setup: the scheduler must admit the first manual sync");
    let (held_tx, _held_rx) = oneshot::channel();
    let mut host = PersonalSync::with_pending_for_test(scheduler, token, held_tx.into());
    let (daemon_response, _) = daemon_reply(&mut host, RemoteCommand::SyncNow);

    assert_reply_parity("duplicate sync now", &app_response, &daemon_response);
    assert_eq!(
        app_response.reason.as_deref(),
        Some("sync_busy"),
        "the shared single-flight contract moved; update both owners together"
    );
    assert_eq!(
        app_commands, 0,
        "App started a second concurrent sync attempt"
    );

    host.shutdown();
}
