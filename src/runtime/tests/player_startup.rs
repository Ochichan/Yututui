use super::*;
use crate::app::{PendingRemoteReply, PlayerCommit, PlayerIntent, RemoteReplyPlan};
use crate::runtime::player_delivery::{
    PendingPlayerIntents, PlayerAdmission, PlayerStartupCompletion, PlayerStartupKind,
    RuntimePlayerLifecycle, admit_ready_player_work, begin_player_shutdown_state,
};

type DropLog = std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>;
type TestLifecycle = RuntimePlayerLifecycle<&'static str, &'static str>;

struct DropOrder {
    label: &'static str,
    log: DropLog,
}

impl DropOrder {
    fn new(label: &'static str, log: &DropLog) -> Self {
        Self {
            label,
            log: std::sync::Arc::clone(log),
        }
    }
}

impl Drop for DropOrder {
    fn drop(&mut self) {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(self.label);
    }
}

fn live_initial_lifecycle() -> TestLifecycle {
    let mut player = RuntimePlayerLifecycle::default();
    assert!(matches!(
        player.complete_start::<&'static str>(Ok(("initial-handle", "initial-guard"))),
        PlayerStartupCompletion::Ready {
            kind: PlayerStartupKind::Initial,
            restore,
        } if restore.is_empty()
    ));
    player
}

fn restart_queued_lifecycle() -> TestLifecycle {
    let mut player = live_initial_lifecycle();
    assert_eq!(
        player.request_restart(Vec::new()).0,
        PlayerRestartDecision::Start
    );
    player
}

fn starting_replacement_lifecycle() -> TestLifecycle {
    let mut player = restart_queued_lifecycle();
    assert!(player.take_restart_request());
    player
}

fn live_replacement_lifecycle() -> TestLifecycle {
    let mut player = starting_replacement_lifecycle();
    assert!(matches!(
        player.complete_start::<&'static str>(Ok(("replacement-handle", "replacement-guard"))),
        PlayerStartupCompletion::Ready {
            kind: PlayerStartupKind::Replacement,
            restore,
        } if restore.is_empty()
    ));
    player
}

fn failed_initial_lifecycle() -> TestLifecycle {
    let mut player = RuntimePlayerLifecycle::default();
    assert!(matches!(
        player.complete_start::<&'static str>(Err("initial failure")),
        PlayerStartupCompletion::Failed {
            kind: PlayerStartupKind::Initial,
            error: "initial failure",
        }
    ));
    player
}

fn failed_replacement_lifecycle() -> TestLifecycle {
    let mut player = starting_replacement_lifecycle();
    assert!(matches!(
        player.complete_start::<&'static str>(Err("replacement failure")),
        PlayerStartupCompletion::Failed {
            kind: PlayerStartupKind::Replacement,
            error: "replacement failure",
        }
    ));
    player
}

fn seek_intent(position: f64, remote_reply: Option<PendingRemoteReply>) -> Box<PlayerIntent> {
    Box::new(PlayerIntent {
        commands: vec![PlayerCmd::interactive_seek(position)],
        commit: PlayerCommit::Seek {
            optimistic_position: Some(position),
        },
        label: "seek_absolute",
        remote_reply,
    })
}

#[test]
fn shutdown_retires_events_from_the_ended_player_generation() {
    for event in [
        RuntimeEvent::Player(crate::player::PlayerEvent::Eof),
        RuntimeEvent::Player(crate::player::PlayerEvent::TransportClosed(
            "late close".to_owned(),
        )),
    ] {
        assert!(shutdown_event_is_retired(&event));
    }
    assert!(!shutdown_event_is_retired(&RuntimeEvent::Download(
        crate::download::DownloadEvent::Error {
            video_id: "id".to_owned(),
            error: "late failure".to_owned(),
        },
    )));
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
                sender: reply.into(),
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
                    sender: reply.into(),
                    response: RemoteReplyPlan::Status,
                }),
            ))
            .is_ok()
    );

    let intent = pending.drain().pop().expect("one deferred intent");
    assert!(admit_player_intent(&player, &mut app, intent).is_empty());

    assert!(matches!(
        rx.recv().await,
        Some(PlayerCmd::SeekAbsolute {
            seconds: position,
            precision: crate::player::SeekPrecision::InteractiveFast,
        })
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
                    sender: first_reply.into(),
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
fn lifecycle_initial_success_installs_a_live_player_without_restore_work() {
    let mut player: TestLifecycle = RuntimePlayerLifecycle::default();

    assert!(matches!(player.admission(), PlayerAdmission::Deferred));
    assert!(matches!(
        player.complete_start::<&'static str>(Ok(("handle", "guard"))),
        PlayerStartupCompletion::Ready {
            kind: PlayerStartupKind::Initial,
            restore,
        } if restore.is_empty()
    ));

    assert!(matches!(
        player.admission(),
        PlayerAdmission::Live(handle) if *handle == "handle"
    ));
    assert!(matches!(player, RuntimePlayerLifecycle::LiveInitial(_)));
}

#[test]
fn lifecycle_initial_failure_closes_admission_but_still_allows_one_replacement() {
    let mut player: TestLifecycle = RuntimePlayerLifecycle::default();

    assert!(matches!(
        player.complete_start::<&'static str>(Err("mpv unavailable")),
        PlayerStartupCompletion::Failed {
            kind: PlayerStartupKind::Initial,
            error: "mpv unavailable",
        }
    ));
    assert!(matches!(player.admission(), PlayerAdmission::Closed));
    assert!(matches!(player, RuntimePlayerLifecycle::FailedInitial));

    let (decision, delivery) = player.request_restart(Vec::new());
    assert_eq!(decision, PlayerRestartDecision::Start);
    assert!(delivery.is_none());
    assert!(matches!(
        player,
        RuntimePlayerLifecycle::RestartQueued { .. }
    ));
}

#[test]
fn lifecycle_replacement_start_failure_is_terminal_for_the_automatic_retry_budget() {
    let mut player = starting_replacement_lifecycle();

    assert!(matches!(
        player.complete_start::<&'static str>(Err("replacement failed")),
        PlayerStartupCompletion::Failed {
            kind: PlayerStartupKind::Replacement,
            error: "replacement failed",
        }
    ));
    assert!(matches!(player.admission(), PlayerAdmission::Closed));
    assert!(matches!(player, RuntimePlayerLifecycle::FailedReplacement));
    let (decision, delivery) = player.request_restart(vec![PlayerCmd::Stop]);
    assert_eq!(decision, PlayerRestartDecision::Exhausted);
    assert!(delivery.is_none());
}

#[test]
fn lifecycle_replacement_preserves_restore_order_and_ignores_duplicate_requests() {
    let mut player = live_initial_lifecycle();
    let restore = vec![
        PlayerCmd::SetVolume(10),
        PlayerCmd::SetAudioFilter("lavfi=[volume=1]".to_owned()),
        PlayerCmd::CyclePause,
    ];

    let (decision, delivery) = player.request_restart(restore);
    assert_eq!(decision, PlayerRestartDecision::Start);
    assert!(delivery.is_some_and(|result| result.is_ok()));
    assert!(matches!(
        player,
        RuntimePlayerLifecycle::RestartQueued { .. }
    ));

    let (decision, delivery) = player.request_restart(vec![PlayerCmd::SetVolume(99)]);
    assert_eq!(decision, PlayerRestartDecision::AlreadyPending);
    assert!(delivery.is_none());
    assert!(player.take_restart_request());
    assert!(matches!(
        player,
        RuntimePlayerLifecycle::StartingReplacement { .. }
    ));

    let (decision, delivery) = player.request_restart(vec![PlayerCmd::Stop]);
    assert_eq!(decision, PlayerRestartDecision::AlreadyPending);
    assert!(delivery.is_none());
    assert!(!player.take_restart_request());

    let completion =
        player.complete_start::<&'static str>(Ok(("replacement-handle", "replacement-guard")));
    let PlayerStartupCompletion::Ready {
        kind: PlayerStartupKind::Replacement,
        restore,
    } = completion
    else {
        panic!("replacement readiness must install the queued generation");
    };
    assert!(matches!(
        restore.as_slice(),
        [
            PlayerCmd::SetVolume(10),
            PlayerCmd::SetAudioFilter(filter),
            PlayerCmd::CyclePause,
        ] if filter == "lavfi=[volume=1]"
    ));
    assert!(matches!(
        player.admission(),
        PlayerAdmission::Live(handle) if *handle == "replacement-handle"
    ));
    assert!(matches!(player, RuntimePlayerLifecycle::LiveReplacement(_)));
}

#[test]
fn lifecycle_replacement_disconnect_exhausts_the_single_retry_and_retires_in_order() {
    let drops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut player = RuntimePlayerLifecycle::default();
    assert!(matches!(
        player.complete_start::<&'static str>(Ok((
            DropOrder::new("initial-handle", &drops),
            DropOrder::new("initial-guard", &drops),
        ))),
        PlayerStartupCompletion::Ready {
            kind: PlayerStartupKind::Initial,
            ..
        }
    ));
    assert_eq!(
        player.request_restart(Vec::new()).0,
        PlayerRestartDecision::Start
    );
    assert!(player.take_restart_request());
    assert!(matches!(
        player.complete_start::<&'static str>(Ok((
            DropOrder::new("replacement-handle", &drops),
            DropOrder::new("replacement-guard", &drops),
        ))),
        PlayerStartupCompletion::Ready {
            kind: PlayerStartupKind::Replacement,
            ..
        }
    ));

    assert_eq!(
        player.request_restart(Vec::new()).0,
        PlayerRestartDecision::Exhausted
    );
    assert!(matches!(player.admission(), PlayerAdmission::Closed));
    assert!(matches!(player, RuntimePlayerLifecycle::FailedReplacement));
    assert_eq!(
        player.request_restart(Vec::new()).0,
        PlayerRestartDecision::Exhausted
    );
    assert_eq!(
        *drops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        [
            "initial-handle",
            "initial-guard",
            "replacement-handle",
            "replacement-guard",
        ]
    );
}

#[test]
fn lifecycle_shutdown_absorbs_every_owner_phase() {
    let mut shutdown = RuntimePlayerLifecycle::default();
    shutdown.begin_shutdown();
    let states = vec![
        ("starting_initial", RuntimePlayerLifecycle::default()),
        ("live_initial", live_initial_lifecycle()),
        ("failed_initial", failed_initial_lifecycle()),
        ("restart_queued", restart_queued_lifecycle()),
        ("starting_replacement", starting_replacement_lifecycle()),
        ("live_replacement", live_replacement_lifecycle()),
        ("failed_replacement", failed_replacement_lifecycle()),
        ("shutdown", shutdown),
    ];

    for (phase, mut player) in states {
        player.begin_shutdown();
        player.begin_shutdown();
        assert!(
            matches!(player, RuntimePlayerLifecycle::Shutdown),
            "phase {phase} did not enter shutdown"
        );
        assert!(matches!(player.admission(), PlayerAdmission::Closed));
        assert!(player.handle().is_none());
        assert_eq!(
            player.request_restart(vec![PlayerCmd::Stop]).0,
            PlayerRestartDecision::Suppressed,
            "phase {phase} admitted a restart after shutdown"
        );
        assert!(!player.take_restart_request());
        assert!(matches!(
            player.complete_start::<&'static str>(Ok(("late-handle", "late-guard"))),
            PlayerStartupCompletion::Discarded
        ));
        assert!(matches!(
            player.complete_start::<&'static str>(Err("late failure")),
            PlayerStartupCompletion::Discarded
        ));
        assert!(matches!(player, RuntimePlayerLifecycle::Shutdown));
    }
}

#[test]
fn lifecycle_discards_late_replacement_readiness_handle_before_guard() {
    let drops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut player: RuntimePlayerLifecycle<DropOrder, DropOrder> =
        RuntimePlayerLifecycle::default();
    assert_eq!(
        player.request_restart(vec![PlayerCmd::SetVolume(10)]).0,
        PlayerRestartDecision::Start
    );
    assert!(player.take_restart_request());
    player.begin_shutdown();

    assert!(matches!(
        player.complete_start::<&'static str>(Ok((
            DropOrder::new("late-handle", &drops),
            DropOrder::new("late-guard", &drops),
        ))),
        PlayerStartupCompletion::Discarded
    ));
    assert!(matches!(player, RuntimePlayerLifecycle::Shutdown));
    assert_eq!(
        *drops
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        ["late-handle", "late-guard"]
    );
}

#[test]
fn shutdown_retires_active_player_before_slow_cleanup_and_replies_to_pending_intents() {
    let drops = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut player = RuntimePlayerLifecycle::default();
    assert!(matches!(
        player.complete_start::<&'static str>(Ok((
            DropOrder::new("handle", &drops),
            DropOrder::new("guard", &drops),
        ))),
        PlayerStartupCompletion::Ready {
            kind: PlayerStartupKind::Initial,
            restore,
        } if restore.is_empty()
    ));
    let mut pending_intents = PendingPlayerIntents::default();
    let (reply, mut reply_rx) = tokio::sync::oneshot::channel();
    assert!(
        pending_intents
            .push(seek_intent(
                20.0,
                Some(PendingRemoteReply {
                    sender: reply.into(),
                    response: RemoteReplyPlan::Status,
                }),
            ))
            .is_ok()
    );
    let mut app = App::new(50);

    let follow_ups = begin_player_shutdown_state(&mut player, &mut pending_intents, &mut app);
    drops
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push("slow_cleanup");

    assert!(follow_ups.is_empty());
    assert_eq!(pending_intents.len(), 0);
    assert!(matches!(player, RuntimePlayerLifecycle::Shutdown));
    assert_eq!(
        player.request_restart(vec![PlayerCmd::SetVolume(10)]).0,
        PlayerRestartDecision::Suppressed
    );
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
            sender: reply.into(),
            response: RemoteReplyPlan::Status,
        }),
    )) else {
        panic!("a second uncommitted snapshot must be rejected");
    };
    assert_eq!(error, DeliveryError::Busy);
    assert!(matches!(
        rejected.commands.as_slice(),
        [PlayerCmd::SeekAbsolute { seconds: position, .. }]
            if (*position - 2.0).abs() < f64::EPSILON
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
            if matches!(intent.commands.as_slice(), [PlayerCmd::SeekAbsolute { seconds: position, .. }]
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
    let mut first_cmds = app.update(Msg::Remote(RemoteCommand::Next, first_reply.into()));
    let Cmd::PlayerControl(PlayerControl::Intent(first)) = first_cmds.remove(0) else {
        panic!("Next must produce one player intent");
    };
    assert!(pending.push(first).is_ok());

    let (second_reply, mut second_rx) = tokio::sync::oneshot::channel();
    let mut second_cmds = app.update(Msg::Remote(RemoteCommand::Next, second_reply.into()));
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
    let mut cmds = app.update(Msg::Remote(RemoteCommand::Next, reply.into()));
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
        PlayerCmd::load(
            "https://example.invalid/recovered",
            crate::player::MediaSourceContext::OnDemand,
        ),
        PlayerCmd::SetAudioFilter("lavfi=[volume=1]".to_owned()),
    ];
    let mut app = App::new(50);
    let mut pending = PendingPlayerIntents::default();
    assert!(pending.push(seek_intent(15.0, None)).is_ok());

    assert!(admit_ready_player_work(&player, &mut app, restore, &mut pending).is_empty());
    assert_eq!(pending.len(), 0);

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
        Some(PlayerCmd::SeekAbsolute { seconds: position, .. })
            if (position - 15.0).abs() < f64::EPSILON
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
