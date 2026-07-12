use super::*;
use crate::util::event_policy::{EventKey, EventLane};

#[derive(Debug, PartialEq, Eq)]
enum TestEvent {
    Control(u16),
    Protected(u16),
    Work(u16),
    Remote,
    Stale(u16),
    Telemetry(u16),
    Wake,
}

impl OwnerEvent for TestEvent {
    type CoalesceKey = EventKey;

    fn policy(&self) -> EventPolicy {
        match self {
            Self::Control(_) | Self::Wake => EventPolicy::MustDeliver {
                lane: EventLane::Control,
            },
            Self::Protected(_) => EventPolicy::CoalesceLatest {
                lane: EventLane::Control,
                key: EventKey::Signal,
            },
            Self::Work(_) => EventPolicy::MustDeliver {
                lane: EventLane::WorkResult,
            },
            Self::Remote => EventPolicy::MustReplyOrBusy {
                lane: EventLane::RemoteCommand,
            },
            Self::Stale(_) => EventPolicy::DropIfStale {
                stale_key: EventKey::SearchRequest,
            },
            Self::Telemetry(_) => EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::PlayerTimePos,
            },
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Control(_) => "control",
            Self::Protected(_) => "protected",
            Self::Work(_) => "work",
            Self::Remote => "remote",
            Self::Stale(_) => "stale",
            Self::Telemetry(_) => "telemetry",
            Self::Wake => "wake",
        }
    }

    fn coalesce_key(&self) -> Option<Self::CoalesceKey> {
        match self {
            Self::Stale(_) => Some(EventKey::SearchRequest),
            Self::Protected(_) => Some(EventKey::Signal),
            Self::Telemetry(_) => Some(EventKey::PlayerTimePos),
            _ => None,
        }
    }

    fn wake_event() -> Self {
        Self::Wake
    }
}

fn wait_for_blocked_drainer(ingress: &OwnerEventIngress<TestEvent>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let in_flight = ingress
            .deferred
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .in_flight_lane
            .is_some();
        if in_flight {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "delivery drainer did not block on the full owner queue"
        );
        std::thread::yield_now();
    }
}

#[test]
fn reply_or_busy_reports_saturation() {
    let (tx, _rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(1)).unwrap();
    let ingress = OwnerEventIngress::new("test", tx, 4);
    assert_eq!(ingress.emit(TestEvent::Remote), Err(DeliveryError::Busy));
}

#[test]
fn owned_must_deliver_rejection_returns_the_exact_payload_for_retry() {
    let (tx, _rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(1)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 0);

    let rejected = ingress
        .emit_must_deliver_owned(TestEvent::Work(77))
        .expect_err("full direct and spill lanes must reject");

    assert_eq!(rejected.0, DeliveryError::Saturated);
    assert_eq!(*rejected.1, TestEvent::Work(77));
}

#[test]
fn owned_terminal_api_rejects_non_must_deliver_without_consuming_it() {
    let (tx, _rx) = mpsc::channel(1);
    let ingress = OwnerEventIngress::new("test", tx, 4);

    let rejected = ingress
        .emit_must_deliver_owned(TestEvent::Remote)
        .expect_err("reply-or-busy work is not a retryable terminal completion");

    assert_eq!(rejected.0, DeliveryError::Busy);
    assert_eq!(*rejected.1, TestEvent::Remote);
}

#[test]
fn callback_terminal_event_waits_with_owned_payload_until_capacity_returns() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(1)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 0);
    let callback_ingress = ingress.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();

    let callback = std::thread::spawn(move || {
        done_tx
            .send(callback_ingress.emit_callback_blocking(TestEvent::Work(77)))
            .unwrap();
    });
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_millis(30))
            .is_err(),
        "the callback must retain its payload while both bounded lanes are full"
    );

    assert_eq!(rx.blocking_recv(), Some(TestEvent::Control(1)));
    assert_eq!(
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap(),
        Ok(DeliveryReceipt::Enqueued)
    );
    assert_eq!(rx.blocking_recv(), Some(TestEvent::Work(77)));
    callback.join().unwrap();
}

#[test]
fn callback_backpressure_stops_when_owner_closes() {
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(1)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 0);
    drop(rx);

    assert_eq!(
        ingress.emit_callback_blocking(TestEvent::Work(77)),
        Err(DeliveryError::Closed)
    );
}

#[test]
fn callback_generation_cancellation_returns_the_exact_payload_with_owner_open() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(1)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 0);
    let callback_ingress = ingress.clone();
    let cancellation = CallbackCancellation::new();
    let callback_cancellation = cancellation.clone();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);

    let callback = std::thread::spawn(move || {
        done_tx
            .send(
                callback_ingress
                    .emit_callback_owned_until(TestEvent::Work(77), &callback_cancellation),
            )
            .unwrap();
    });
    assert_eq!(
        done_rx.recv_timeout(std::time::Duration::from_millis(30)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout),
        "the callback must be blocked before its producer generation is retired"
    );

    cancellation.cancel();
    let rejected = done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("generation cancellation should release the callback")
        .expect_err("a retired generation must not admit its stale command");
    assert_eq!(rejected.0, DeliveryError::Closed);
    assert_eq!(*rejected.1, TestEvent::Work(77));
    callback.join().unwrap();

    // The application-wide owner remains open and its original item is untouched.
    assert_eq!(rx.try_recv(), Ok(TestEvent::Control(1)));
    assert!(!rx.is_closed());
}

#[test]
fn callback_cancellation_admission_race_has_one_owned_outcome() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(1)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 0);
    let cancellation = CallbackCancellation::new();
    let callback_cancellation = cancellation.clone();
    let callback = std::thread::spawn(move || {
        ingress.emit_callback_owned_until(TestEvent::Work(77), &callback_cancellation)
    });

    std::thread::sleep(std::time::Duration::from_millis(20));
    cancellation.cancel();
    assert_eq!(rx.blocking_recv(), Some(TestEvent::Control(1)));
    match callback.join().unwrap() {
        Ok(receipt) => {
            assert!(matches!(
                receipt,
                DeliveryReceipt::Enqueued | DeliveryReceipt::Deferred
            ));
            assert_eq!(rx.blocking_recv(), Some(TestEvent::Work(77)));
        }
        Err((error, event)) => {
            assert_eq!(error, DeliveryError::Closed);
            assert_eq!(*event, TestEvent::Work(77));
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected)
            ));
        }
    }
}

#[test]
fn cancelling_one_callback_generation_does_not_poison_its_successor() {
    let retired = CallbackCancellation::new();
    retired.cancel();
    let successor = CallbackCancellation::new();

    assert!(retired.is_cancelled());
    assert!(!successor.is_cancelled());

    let (tx, mut rx) = mpsc::channel(1);
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 0);
    let rejected = ingress
        .emit_callback_owned_until(TestEvent::Work(1), &retired)
        .expect_err("the retired generation must stay closed");
    assert_eq!(*rejected.1, TestEvent::Work(1));
    assert_eq!(
        ingress.emit_callback_blocking_until(TestEvent::Work(2), &successor),
        Ok(DeliveryReceipt::Enqueued)
    );
    assert_eq!(rx.blocking_recv(), Some(TestEvent::Work(2)));
}

#[test]
fn cancellable_callback_keeps_nonterminal_policy_nonblocking() {
    let (tx, _rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(1)).unwrap();
    let ingress = OwnerEventIngress::new("test", tx, 4);
    let cancellation = CallbackCancellation::new();

    assert_eq!(
        ingress.emit_callback_blocking_until(TestEvent::Remote, &cancellation),
        Err(DeliveryError::Busy)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn callback_backpressure_yields_a_runtime_worker_to_the_owner() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(1)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 0);

    let callback = tokio::spawn(async move { ingress.emit_callback_blocking(TestEvent::Work(77)) });
    tokio::task::yield_now().await;

    assert_eq!(rx.recv().await, Some(TestEvent::Control(1)));
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), callback)
            .await
            .expect("callback worker should be replaced while it waits")
            .expect("callback task panicked"),
        Ok(DeliveryReceipt::Enqueued)
    );
    assert_eq!(rx.recv().await, Some(TestEvent::Work(77)));
}

#[test]
fn telemetry_cannot_evict_a_protected_control_value() {
    let (tx, mut rx) = mpsc::channel(1);
    let ingress = OwnerEventIngress::new("test", tx, 1);
    assert!(ingress.emit(TestEvent::Protected(7)).is_ok());

    assert_eq!(
        ingress.emit(TestEvent::Telemetry(9)),
        Err(DeliveryError::Saturated)
    );
    assert_eq!(rx.try_recv(), Ok(TestEvent::Wake));
    assert_eq!(ingress.drain_coalesced(), vec![TestEvent::Protected(7)]);
}

#[test]
fn stale_work_result_cannot_evict_a_control_value() {
    let (tx, mut rx) = mpsc::channel(1);
    let ingress = OwnerEventIngress::new("test", tx, 1);
    assert!(ingress.emit(TestEvent::Protected(7)).is_ok());

    assert_eq!(
        ingress.emit(TestEvent::Stale(9)),
        Err(DeliveryError::StaleOrFull)
    );
    assert_eq!(rx.try_recv(), Ok(TestEvent::Wake));
    assert_eq!(ingress.drain_coalesced(), vec![TestEvent::Protected(7)]);
}

#[tokio::test(flavor = "current_thread")]
async fn stale_results_keep_the_latest_value_when_owner_is_full() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::new("test", tx, 4);
    assert_eq!(
        ingress.emit(TestEvent::Stale(1)),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: false,
            evicted_oldest: false,
        })
    );
    assert_eq!(
        ingress.emit(TestEvent::Stale(2)),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: true,
            evicted_oldest: false,
        })
    );
    assert_eq!(rx.recv().await, Some(TestEvent::Control(0)));
    assert_eq!(rx.recv().await, Some(TestEvent::Wake));
    assert_eq!(ingress.drain_coalesced(), vec![TestEvent::Stale(2)]);
}

#[tokio::test(flavor = "current_thread")]
async fn newer_stale_result_cannot_overtake_a_deferred_wake() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::new("test", tx, 4);
    assert!(ingress.emit(TestEvent::Stale(1)).is_ok());
    assert_eq!(rx.try_recv(), Ok(TestEvent::Control(0)));
    assert!(matches!(
        ingress.emit(TestEvent::Stale(2)),
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: true,
            ..
        })
    ));
    assert_eq!(rx.recv().await, Some(TestEvent::Wake));
    assert_eq!(ingress.drain_coalesced(), vec![TestEvent::Stale(2)]);
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn one_drainer_delivers_deferred_control_in_order() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(
        ingress.emit(TestEvent::Control(2)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(rx.recv().await, Some(TestEvent::Control(0)));
    assert_eq!(rx.recv().await, Some(TestEvent::Control(1)));
    assert_eq!(rx.recv().await, Some(TestEvent::Control(2)));
}

#[tokio::test(flavor = "current_thread")]
async fn closing_producer_admission_preserves_already_accepted_deferred_work() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Ok(DeliveryReceipt::Deferred)
    );

    assert!(ingress.close_admission());
    assert!(!ingress.close_admission());
    assert_eq!(
        ingress.emit(TestEvent::Control(2)),
        Err(DeliveryError::Closed)
    );
    assert_eq!(rx.recv().await, Some(TestEvent::Control(0)));
    assert_eq!(rx.recv().await, Some(TestEvent::Control(1)));
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !ingress.deferred_is_idle() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted deferred work drains after producer close");
}

#[test]
fn deferred_idle_barrier_does_not_wait_on_a_live_receiver_after_the_last_send() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert!(ingress.close_admission());
    wait_for_blocked_drainer(&ingress);

    // Keep the accounting lock after the drainer published its in-flight ownership. Once the
    // owner consumes both events, the drainer has completed its final send but cannot yet flip
    // idle. Waiting on `recv()` here would hang forever because the sender intentionally lives.
    let state = ingress
        .deferred
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(rx.blocking_recv(), Some(TestEvent::Control(0)));
    assert_eq!(rx.blocking_recv(), Some(TestEvent::Control(1)));
    assert!(state.in_flight_lane.is_some());
    drop(state);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime");
    runtime
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while !ingress.deferred_is_idle() {
                    tokio::task::yield_now().await;
                }
            })
            .await
        })
        .expect("idle accounting should finish without receiver closure or another send");
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn later_control_cannot_overtake_a_deferred_control() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(rx.try_recv(), Ok(TestEvent::Control(0)));
    assert_eq!(
        ingress.emit(TestEvent::Control(2)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(rx.recv().await, Some(TestEvent::Control(1)));
    assert_eq!(rx.recv().await, Some(TestEvent::Control(2)));
}

#[tokio::test(flavor = "current_thread")]
async fn reply_or_busy_cannot_overtake_an_earlier_deferred_control() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(rx.try_recv(), Ok(TestEvent::Control(0)));

    assert_eq!(ingress.emit(TestEvent::Remote), Err(DeliveryError::Busy));
    assert_eq!(rx.recv().await, Some(TestEvent::Control(1)));
}

#[tokio::test(flavor = "current_thread")]
async fn saturated_work_cannot_consume_the_reserved_control_slot() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 4);
    assert_eq!(
        ingress.emit(TestEvent::Work(1)),
        Ok(DeliveryReceipt::Deferred)
    );
    // The drainer has removed Work(1) from its VecDeque but cannot send it while Control(0)
    // occupies the owner slot. That accepted in-flight item must still consume work capacity.
    wait_for_blocked_drainer(&ingress);
    for work in 2..=3 {
        assert_eq!(
            ingress.emit(TestEvent::Work(work)),
            Ok(DeliveryReceipt::Deferred)
        );
    }
    assert_eq!(
        ingress.emit(TestEvent::Work(4)),
        Err(DeliveryError::Saturated)
    );
    assert_eq!(
        ingress.emit(TestEvent::Control(9)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(rx.recv().await, Some(TestEvent::Control(0)));
    for work in 1..=3 {
        assert_eq!(rx.recv().await, Some(TestEvent::Work(work)));
    }
    assert_eq!(rx.recv().await, Some(TestEvent::Control(9)));
}

#[tokio::test(flavor = "current_thread")]
async fn one_shot_coalesced_result_keeps_a_wake_when_both_queues_are_full() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 1);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert!(matches!(
        ingress.emit(TestEvent::Telemetry(9)),
        Ok(DeliveryReceipt::Coalesced { .. })
    ));
    assert_eq!(rx.recv().await, Some(TestEvent::Control(0)));
    assert_eq!(rx.recv().await, Some(TestEvent::Control(1)));
    assert_eq!(rx.recv().await, Some(TestEvent::Wake));
    assert_eq!(ingress.drain_coalesced(), vec![TestEvent::Telemetry(9)]);
}

#[tokio::test(flavor = "current_thread")]
async fn latched_wake_keeps_its_sequence_point_ahead_of_later_control() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    assert!(ingress.emit(TestEvent::Control(1)).is_ok());
    assert!(ingress.emit(TestEvent::Control(2)).is_ok());
    assert!(ingress.emit(TestEvent::Telemetry(9)).is_ok());

    assert_eq!(rx.recv().await, Some(TestEvent::Control(0)));
    assert_eq!(rx.recv().await, Some(TestEvent::Control(1)));
    assert_eq!(
        // Receiving the item and the drainer retiring its in-flight accounting are separate
        // thread steps. Retain this terminal payload across that bounded hand-off instead of
        // making the test depend on their scheduler order.
        ingress.emit_callback_blocking(TestEvent::Control(3)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(rx.recv().await, Some(TestEvent::Control(2)));
    assert_eq!(rx.recv().await, Some(TestEvent::Wake));
    assert_eq!(rx.recv().await, Some(TestEvent::Control(3)));
}

#[test]
fn must_deliver_reports_a_closed_owner() {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let ingress = OwnerEventIngress::new("test", tx, 4);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Err(DeliveryError::Closed)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn active_drainer_does_not_admit_after_owner_closes() {
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Ok(DeliveryReceipt::Deferred)
    );
    drop(rx);
    assert_eq!(
        ingress.emit(TestEvent::Control(2)),
        Err(DeliveryError::Closed)
    );
    assert_eq!(
        ingress.emit(TestEvent::Control(3)),
        Err(DeliveryError::Closed)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_drainer_observes_owner_close_and_latches_it() {
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Ok(DeliveryReceipt::Deferred)
    );
    tokio::task::yield_now().await;
    drop(rx);
    tokio::task::yield_now().await;
    assert_eq!(
        ingress.emit(TestEvent::Control(2)),
        Err(DeliveryError::Closed)
    );
}

#[test]
fn deferred_capacity_returns_saturated_without_spawning_fallback_work() {
    let (tx, _rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 0);
    assert_eq!(
        ingress.emit(TestEvent::Control(1)),
        Err(DeliveryError::Saturated)
    );
}

#[test]
fn drainer_spawn_failure_releases_non_control_capacity_for_retry() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    ingress.deferred.fail_next_drainer_spawn();
    assert_eq!(
        ingress.emit(TestEvent::Work(1)),
        Err(DeliveryError::Saturated)
    );
    assert_eq!(
        ingress.emit(TestEvent::Work(2)),
        Ok(DeliveryReceipt::Deferred)
    );
    assert_eq!(rx.blocking_recv(), Some(TestEvent::Control(0)));
    assert_eq!(rx.blocking_recv(), Some(TestEvent::Work(2)));
}

#[test]
fn telemetry_keeps_latest_value_behind_one_wake() {
    let (tx, mut rx) = mpsc::channel(1);
    let ingress = OwnerEventIngress::new("test", tx, 4);
    assert!(ingress.emit(TestEvent::Telemetry(1)).is_ok());
    assert!(ingress.emit(TestEvent::Telemetry(2)).is_ok());
    assert_eq!(rx.try_recv(), Ok(TestEvent::Wake));
    assert_eq!(ingress.drain_coalesced(), vec![TestEvent::Telemetry(2)]);
}

#[test]
fn closed_wake_rearms_and_reports_closed_on_every_attempt() {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let ingress = OwnerEventIngress::new("test", tx, 4);
    assert_eq!(
        ingress.emit(TestEvent::Telemetry(1)),
        Err(DeliveryError::Closed)
    );
    assert_eq!(
        ingress.emit(TestEvent::Telemetry(2)),
        Err(DeliveryError::Closed)
    );
    assert!(ingress.drain_coalesced().is_empty());
}

#[test]
fn coalesced_update_reports_closed_even_while_an_old_wake_is_pending() {
    let (tx, rx) = mpsc::channel(1);
    let ingress = OwnerEventIngress::new("test", tx, 4);
    assert!(ingress.emit(TestEvent::Telemetry(1)).is_ok());
    drop(rx);

    assert_eq!(
        ingress.emit(TestEvent::Telemetry(2)),
        Err(DeliveryError::Closed)
    );
    assert!(ingress.drain_coalesced().is_empty());
}

#[test]
fn deferred_drainer_survives_the_admitting_tokio_runtime() {
    let (tx, mut rx) = mpsc::channel(1);
    tx.try_send(TestEvent::Control(0)).unwrap();
    let ingress = OwnerEventIngress::with_deferred_capacity("test", tx, 4, 2);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime");

    runtime.block_on(async {
        assert_eq!(
            ingress.emit(TestEvent::Control(1)),
            Ok(DeliveryReceipt::Deferred)
        );
    });
    drop(runtime);

    assert_eq!(rx.blocking_recv(), Some(TestEvent::Control(0)));
    assert_eq!(rx.blocking_recv(), Some(TestEvent::Control(1)));
}

#[test]
fn drain_rearms_a_single_wake_for_the_next_cycle() {
    let (tx, mut rx) = mpsc::channel(1);
    let ingress = OwnerEventIngress::new("test", tx, 4);
    assert!(ingress.emit(TestEvent::Telemetry(1)).is_ok());
    assert_eq!(rx.try_recv(), Ok(TestEvent::Wake));
    assert_eq!(ingress.drain_coalesced(), vec![TestEvent::Telemetry(1)]);
    assert!(ingress.emit(TestEvent::Telemetry(2)).is_ok());
    assert_eq!(rx.try_recv(), Ok(TestEvent::Wake));
    assert_eq!(ingress.drain_coalesced(), vec![TestEvent::Telemetry(2)]);
}

#[test]
#[should_panic(expected = "latest-event capacity must be non-zero")]
fn zero_coalesced_capacity_is_rejected() {
    let (tx, _rx) = mpsc::channel(1);
    let _ = OwnerEventIngress::<TestEvent>::new("test", tx, 0);
}
