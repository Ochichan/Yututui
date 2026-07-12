use super::*;
use crate::app::{PendingRemoteReply, PlayerCommit, PlayerIntent, RemoteReplyPlan};
use crate::runtime::player_delivery::{
    PendingPlayerCmds, PendingPlayerIntents, PlayerRestartDecision, PlayerRestartGate,
    begin_player_shutdown_state,
};

fn seek_intent(position: f64, remote_reply: Option<PendingRemoteReply>) -> Box<PlayerIntent> {
    Box::new(PlayerIntent {
        commands: vec![PlayerCmd::SeekAbsolute(position)],
        commit: PlayerCommit::Seek {
            optimistic_position: Some(position),
        },
        label: "seek_absolute",
        remote_reply,
    })
}

#[test]
fn pre_ready_intent_does_not_commit_or_reply() {
    let app = App::new(50);
    let epoch = app.playback.position_epoch;
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    let mut pending = PendingPlayerIntents::default();

    assert!(matches!(
        pending.push(seek_intent(
            42.0,
            Some(PendingRemoteReply {
                sender: reply,
                response: RemoteReplyPlan::Status,
            }),
        )),
        Ok(DeliveryReceipt::Deferred)
    ));

    assert_eq!(app.playback.time_pos, None);
    assert_eq!(app.playback.position_epoch, epoch);
    assert!(matches!(
        reply_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.command_count(), 1);
}

#[tokio::test]
async fn ready_player_admission_commits_and_replies_once() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let player = PlayerHandle::test_handle(tx);
    let mut app = App::new(50);
    let epoch = app.playback.position_epoch;
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    let mut pending = PendingPlayerIntents::default();
    assert!(
        pending
            .push(seek_intent(
                42.0,
                Some(PendingRemoteReply {
                    sender: reply,
                    response: RemoteReplyPlan::Status,
                }),
            ))
            .is_ok()
    );

    let intent = pending.drain().pop().expect("one deferred intent");
    assert!(admit_player_intent(&player, &mut app, intent).is_empty());

    assert!(matches!(
        rx.recv().await,
        Some(PlayerCmd::SeekAbsolute(position))
            if (position - 42.0).abs() < f64::EPSILON
    ));
    assert_eq!(app.playback.time_pos, Some(42.0));
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert!(reply_rx.try_recv().expect("remote success").ok);
    assert!(matches!(
        reply_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
    ));
}

#[test]
fn startup_failure_rejects_the_deferred_intent_without_state_commit() {
    let mut app = App::new(50);
    let epoch = app.playback.position_epoch;
    let (first_reply, mut first_rx) = tokio::sync::oneshot::channel();
    let mut pending = PendingPlayerIntents::default();
    assert!(
        pending
            .push(seek_intent(
                10.0,
                Some(PendingRemoteReply {
                    sender: first_reply,
                    response: RemoteReplyPlan::Status,
                }),
            ))
            .is_ok()
    );
    assert!(reject_pending_player_intents(&mut pending, &mut app).is_empty());

    assert_eq!(app.playback.time_pos, None);
    assert_eq!(app.playback.position_epoch, epoch);
    let response = first_rx.try_recv().expect("terminal reply");
    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("player_unavailable"));
    assert_eq!(pending.len(), 0);
    assert_eq!(pending.command_count(), 0);
}

#[test]
fn shutdown_retires_active_player_before_slow_cleanup_and_replies_to_pending_intents() {
    #[derive(Clone)]
    struct DropOrder {
        label: &'static str,
        log: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl Drop for DropOrder {
        fn drop(&mut self) {
            self.log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(self.label);
        }
    }

    let drops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handle = Some(DropOrder {
        label: "handle",
        log: std::sync::Arc::clone(&drops),
    });
    let mut guard = Some(DropOrder {
        label: "guard",
        log: std::sync::Arc::clone(&drops),
    });
    let mut gate = PlayerRestartGate::default();
    assert_eq!(gate.request(), PlayerRestartDecision::Start);
    let mut failed = false;
    let mut pending_cmds = PendingPlayerCmds::default();
    assert!(pending_cmds.push(PlayerCmd::SetVolume(10)).is_ok());
    let mut pending_intents = PendingPlayerIntents::default();
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    assert!(
        pending_intents
            .push(seek_intent(
                20.0,
                Some(PendingRemoteReply {
                    sender: reply,
                    response: RemoteReplyPlan::Status,
                }),
            ))
            .is_ok()
    );
    let mut app = App::new(50);

    let follow_ups = begin_player_shutdown_state(
        &mut gate,
        &mut failed,
        &mut handle,
        &mut guard,
        &mut pending_cmds,
        &mut pending_intents,
        &mut app,
    );
    drops
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push("slow_cleanup");

    assert!(follow_ups.is_empty());
    assert!(failed);
    assert!(handle.is_none());
    assert!(guard.is_none());
    assert_eq!(pending_cmds.len(), 0);
    assert_eq!(pending_intents.len(), 0);
    assert_eq!(gate.request(), PlayerRestartDecision::Suppressed);
    assert_eq!(
        *drops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        ["handle", "guard", "slow_cleanup"]
    );
    let response = reply_rx.try_recv().expect("shutdown terminal reply");
    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("player_unavailable"));
}

#[test]
fn pending_intent_slot_rejects_a_second_snapshot_without_evicting_the_first() {
    let mut pending = PendingPlayerIntents::default();
    assert!(matches!(
        pending.push(seek_intent(1.0, None)),
        Ok(DeliveryReceipt::Deferred)
    ));

    let mut app = App::new(50);
    let epoch = app.playback.position_epoch;
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    let Err((error, rejected)) = pending.push(seek_intent(
        2.0,
        Some(PendingRemoteReply {
            sender: reply,
            response: RemoteReplyPlan::Status,
        }),
    )) else {
        panic!("a second uncommitted snapshot must be rejected");
    };
    assert_eq!(error, DeliveryError::Busy);
    assert!(matches!(
        rejected.commands.as_slice(),
        [PlayerCmd::SeekAbsolute(position)] if (*position - 2.0).abs() < f64::EPSILON
    ));
    assert_eq!(pending.len(), 1);
    assert_eq!(pending.command_count(), 1);
    assert!(settle_player_intent(&mut app, *rejected, Err(error)).is_empty());
    assert_eq!(app.playback.time_pos, None);
    assert_eq!(app.playback.position_epoch, epoch);
    let response = reply_rx.try_recv().expect("capacity rejection reply");
    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("player_busy"));

    let admitted = pending.drain();
    assert!(matches!(
        admitted.as_slice(),
        [intent]
            if matches!(intent.commands.as_slice(), [PlayerCmd::SeekAbsolute(position)]
                if (*position - 1.0).abs() < f64::EPSILON)
    ));
}

#[test]
fn pending_intent_command_bound_rejects_one_oversized_transaction() {
    let mut pending = PendingPlayerIntents::default();
    let oversized = Box::new(PlayerIntent {
        commands: (0..=PENDING_PLAYER_INTENTS_MAX)
            .map(|_| PlayerCmd::SetVolume(10))
            .collect(),
        commit: PlayerCommit::Volume {
            volume: 10,
            pre_mute_volume: None,
        },
        label: "oversized",
        remote_reply: None,
    });

    let Err((error, _)) = pending.push(oversized) else {
        panic!("one oversized transaction must not bypass the command bound");
    };
    assert_eq!(error, DeliveryError::Saturated);
    assert_eq!(pending.len(), 0);
    assert_eq!(pending.command_count(), 0);
}

#[tokio::test]
async fn two_next_actions_during_startup_reject_the_second_without_stale_commit() {
    let mut app = App::new(50);
    app.queue.set(
        vec![
            song("first000001"),
            song("second00001"),
            song("third000001"),
        ],
        0,
    );
    let mut pending = PendingPlayerIntents::default();

    let (first_reply, mut first_rx) = tokio::sync::oneshot::channel();
    let mut first_cmds = app.update(Msg::Remote(RemoteCommand::Next, first_reply));
    let Cmd::PlayerControl(PlayerControl::Intent(first)) = first_cmds.remove(0) else {
        panic!("Next must produce one player intent");
    };
    assert!(pending.push(first).is_ok());

    let (second_reply, mut second_rx) = tokio::sync::oneshot::channel();
    let mut second_cmds = app.update(Msg::Remote(RemoteCommand::Next, second_reply));
    let Cmd::PlayerControl(PlayerControl::Intent(second)) = second_cmds.remove(0) else {
        panic!("second Next must produce one player intent");
    };
    let Err((error, second)) = pending.push(second) else {
        panic!("second startup action must be rejected explicitly");
    };
    assert_eq!(error, DeliveryError::Busy);
    assert!(settle_player_intent(&mut app, *second, Err(error)).is_empty());
    let response = second_rx.try_recv().expect("second Next terminal reply");
    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("player_busy"));

    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let player = PlayerHandle::test_handle(tx);
    let first = pending.drain().pop().expect("first Next remains deferred");
    let _follow_ups = admit_player_intent(&player, &mut app, first);
    assert!(matches!(rx.recv().await, Some(PlayerCmd::Load(_))));
    assert_eq!(app.queue.cursor_pos(), 1);
    let first_response = first_rx.try_recv().expect("first Next terminal reply");
    assert_ne!(first_response.reason.as_deref(), Some("player_busy"));
}

#[tokio::test]
async fn stale_track_snapshot_is_rejected_before_any_player_command_is_sent() {
    let mut app = App::new(50);
    app.queue
        .set(vec![song("first000001"), song("second00001")], 0);
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    let mut cmds = app.update(Msg::Remote(RemoteCommand::Next, reply));
    let Cmd::PlayerControl(PlayerControl::Intent(intent)) = cmds.remove(0) else {
        panic!("Next must produce one player intent");
    };

    assert!(app.queue.goto(1).is_some());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let player = PlayerHandle::test_handle(tx);
    assert!(admit_player_intent(&player, &mut app, *intent).is_empty());

    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(app.queue.cursor_pos(), 1);
    let response = reply_rx.try_recv().expect("stale Next terminal reply");
    assert!(!response.ok);
    assert_eq!(response.reason.as_deref(), Some("player_busy"));
}

#[tokio::test]
async fn recovery_restore_batch_reaches_player_before_deferred_user_intent() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let player = PlayerHandle::test_handle(tx);
    let restore = vec![
        PlayerCmd::Load("https://example.invalid/recovered".to_owned()),
        PlayerCmd::SetAudioFilter("lavfi=[volume=1]".to_owned()),
    ];
    assert!(player.send_batch(restore).is_ok());

    let mut app = App::new(50);
    assert!(admit_player_intent(&player, &mut app, *seek_intent(15.0, None)).is_empty());

    assert!(matches!(
        rx.recv().await,
        Some(PlayerCmd::Load(url)) if url == "https://example.invalid/recovered"
    ));
    assert!(matches!(
        rx.recv().await,
        Some(PlayerCmd::SetAudioFilter(filter)) if filter == "lavfi=[volume=1]"
    ));
    assert!(matches!(
        rx.recv().await,
        Some(PlayerCmd::SeekAbsolute(position)) if (position - 15.0).abs() < f64::EPSILON
    ));
}

#[tokio::test]
async fn admitted_intent_returns_commit_follow_ups_to_the_runtime() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let player = PlayerHandle::test_handle(tx);
    let mut app = App::new(50);
    let intent = Box::new(PlayerIntent {
        commands: vec![PlayerCmd::SetProperty {
            name: "speed".to_owned(),
            value: serde_json::json!(1.2),
        }],
        commit: PlayerCommit::Speed {
            speed: 1.2,
            announce: false,
            persist: true,
        },
        label: "set_speed",
        remote_reply: None,
    });

    let follow_ups = admit_player_intent(&player, &mut app, *intent);

    assert!(matches!(
        rx.recv().await,
        Some(PlayerCmd::SetProperty { name, value })
            if name == "speed" && value == serde_json::json!(1.2)
    ));
    assert!(matches!(
        follow_ups.as_slice(),
        [Cmd::Persist(PersistCmd::Config(_))]
    ));
}
