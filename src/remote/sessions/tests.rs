use super::*;

use std::pin::Pin;
use std::sync::{Arc, Barrier, Mutex};
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;
use tokio::sync::Notify;

fn hub(tuning: SessionTuning) -> RemoteSessionHub {
    RemoteSessionHub::new(
        InstanceMode::StandaloneTui,
        vec!["events-v8".to_string()],
        tuning,
    )
}

fn raw(len: usize) -> SessionLine {
    SessionLine::Raw(vec![b'a'; len])
}

#[test]
fn command_reply_timeouts_keep_quick_playback_and_export_classes_distinct() {
    let tuning = SessionTuning {
        reply_timeout: Duration::from_millis(10),
        playback_reply_timeout: Duration::from_millis(20),
        personal_export_reply_timeout: Duration::from_millis(30),
        manual_sync_reply_timeout: Duration::from_millis(40),
        ..SessionTuning::default()
    };
    assert_eq!(
        tuning.command_reply_timeout(&crate::remote::proto::RemoteCommand::ToggleShuffle),
        Duration::from_millis(10)
    );
    assert_eq!(
        tuning.command_reply_timeout(&crate::remote::proto::RemoteCommand::TogglePause),
        Duration::from_millis(20)
    );
    assert_eq!(
        tuning.command_reply_timeout(&crate::remote::proto::RemoteCommand::ExportPersonalData {
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
            schema: None,
        }),
        Duration::from_millis(30)
    );
    assert_eq!(
        tuning.command_reply_timeout(&crate::remote::proto::RemoteCommand::SyncNow),
        Duration::from_millis(40)
    );
}

#[derive(Clone)]
struct PartialWriterState {
    bytes: Arc<Mutex<Vec<u8>>>,
    started: Arc<Notify>,
}

impl PartialWriterState {
    fn new() -> Self {
        Self {
            bytes: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(Notify::new()),
        }
    }

    fn writer(&self) -> PartialThenPendingWriter {
        PartialThenPendingWriter {
            state: self.clone(),
            wrote_once: false,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        self.bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

struct PartialThenPendingWriter {
    state: PartialWriterState,
    wrote_once: bool,
}

impl AsyncWrite for PartialThenPendingWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.wrote_once {
            return Poll::Pending;
        }
        self.wrote_once = true;
        let written = usize::from(!buf.is_empty());
        self.state
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(&buf[..written]);
        self.state.started.notify_one();
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[test]
fn registry_caps_sessions_and_frees_slots() {
    let hub = hub(SessionTuning::default());
    let mut held = Vec::new();
    for _ in 0..MAX_SESSIONS {
        held.push(hub.register().expect("under the cap"));
    }
    assert!(
        matches!(hub.register(), Err(RegisterError::SessionsFull)),
        "9th session must be rejected"
    );
    let (id, _, _) = held.pop().unwrap();
    hub.unregister(id);
    assert!(hub.register().is_ok(), "slot frees on unregister");
    assert_eq!(hub.active(), MAX_SESSIONS);
}

#[test]
fn session_id_exhaustion_never_wraps_or_reuses_a_stale_identity() {
    let hub = hub(SessionTuning::default());
    hub.next_id.store(u64::MAX - 1, Ordering::Relaxed);

    let (last_id, _, _) = hub.register().expect("the last safe id is admitted");
    assert_eq!(last_id, u64::MAX - 1);
    hub.unregister(last_id);

    assert!(matches!(hub.register(), Err(RegisterError::SessionsFull)));
    assert_eq!(hub.next_id.load(Ordering::Relaxed), u64::MAX);
    assert!(matches!(hub.register(), Err(RegisterError::SessionsFull)));
}

#[test]
fn shutdown_latch_is_monotonic_and_rejects_future_registration() {
    let hub = hub(SessionTuning::default());
    let (_, handle, _rx) = hub.register().unwrap();

    hub.shutdown_all();
    hub.shutdown_all();

    assert!(handle.close.is_closed());
    assert_eq!(handle.budget.snapshot(), (false, 0, 0));
    assert!(matches!(hub.register(), Err(RegisterError::ShuttingDown)));
    assert_eq!(hub.active(), 0);
}

#[tokio::test]
async fn shutdown_wait_is_lost_wake_safe_for_early_and_late_waiters() {
    let hub = hub(SessionTuning::default());

    // Construct the future before shutdown but do not poll it. `notify_waiters` stores no permit
    // for that state, so the post-registration latch check is what makes this complete.
    let early_waiter = hub.wait_for_shutdown();
    hub.shutdown_all();
    hub.shutdown_all();
    timeout(Duration::from_millis(100), early_waiter)
        .await
        .expect("an unpolled waiter must observe the monotonic latch");

    timeout(Duration::from_millis(100), hub.wait_for_shutdown())
        .await
        .expect("a waiter created after shutdown must complete immediately");
}

#[test]
fn owner_emit_gate_never_runs_after_shutdown_latches() {
    let hub = hub(SessionTuning::default());
    let emitted = std::sync::atomic::AtomicUsize::new(0);
    assert!(hub.emit_if_running(|| {
        emitted.fetch_add(1, Ordering::Relaxed);
        true
    }));

    hub.shutdown_all();

    assert!(!hub.emit_if_running(|| {
        emitted.fetch_add(1, Ordering::Relaxed);
        true
    }));
    assert_eq!(emitted.load(Ordering::Relaxed), 1);
}

#[test]
fn owner_gate_distinguishes_temporary_ingress_pressure_from_shutdown() {
    let hub = hub(SessionTuning::default());
    let attempts = std::sync::atomic::AtomicUsize::new(0);

    let busy = hub.run_if_running(|| {
        attempts.fetch_add(1, Ordering::Relaxed);
        false
    });
    assert_eq!(busy, Some(false), "a live but saturated owner is busy");

    hub.shutdown_all();
    let stopped = hub.run_if_running(|| {
        attempts.fetch_add(1, Ordering::Relaxed);
        true
    });
    assert_eq!(stopped, None, "only the shutdown latch closes a session");
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
}

#[test]
fn quiesce_and_wire_token_creation_share_one_monotonic_admission_frontier() {
    let hub = hub(SessionTuning::default());
    let accepted = hub
        .admit_tracked(std::convert::identity)
        .expect("running hub admits a tracked request");
    assert_eq!(hub.active_wire_settlements(), 1);

    assert!(hub.quiesce_owner_admission());
    assert!(!hub.quiesce_owner_admission());
    assert!(matches!(hub.register(), Err(RegisterError::ShuttingDown)));
    assert!(
        hub.admit_tracked(std::convert::identity).is_none(),
        "no token may be created beyond the quiesce linearization point"
    );
    assert_eq!(hub.active_wire_settlements(), 1);

    drop(accepted);
    assert_eq!(hub.active_wire_settlements(), 0);
}

#[test]
fn outbound_queue_trips_on_exact_items_and_bytes_without_underflow() {
    let tuning = SessionTuning {
        max_queued_items: 2,
        max_queued_bytes: 64,
        ..SessionTuning::default()
    };
    let hub = hub(tuning);
    let (_, handle, _rx) = hub.register().unwrap();

    assert!(handle.try_send_line(raw(8)));
    assert!(handle.try_send_line(raw(8)));
    assert_eq!(handle.budget.snapshot(), (true, 2, 16));
    assert!(
        !handle.try_send_line(raw(8)),
        "item cap trips without an extra in-flight slot"
    );

    // Cost-inclusive reservation rejects a single oversized frame before it owns memory.
    let (_, fat, mut fat_rx) = hub.register().unwrap();
    assert!(!fat.try_send_line(raw(65)));
    assert!(fat_rx.try_recv().is_err());
    assert_eq!(fat.budget.snapshot(), (true, 0, 0));

    // A closed receiver exercises reserve + failed-send rollback. The single budget lock keeps a
    // concurrent writer reset from interleaving between those operations, so zero cannot wrap.
    let (_, disconnected, disconnected_rx) = hub.register().unwrap();
    drop(disconnected_rx);
    assert!(!disconnected.try_send_line(raw(8)));
    assert_eq!(disconnected.budget.snapshot(), (true, 0, 0));
    disconnected.request_close(CloseReason::ClientGone);
    assert!(!disconnected.try_send_line(raw(8)));
    assert_eq!(disconnected.budget.snapshot(), (false, 0, 0));
}

#[test]
fn concurrent_send_close_and_writer_finish_never_underflow_or_admit_after_close() {
    // The barrier makes admission and teardown contend for the same budget mutex. Either order is
    // legal: a send that wins is drained by finish_writer; a close that wins rejects it. Every
    // iteration must converge to one closed zero state, with no reset-between-reserve-and-rollback
    // window and no subsequent admission against the dead writer.
    for _ in 0..512 {
        let hub = hub(SessionTuning {
            max_queued_items: 1,
            max_queued_bytes: 8,
            ..SessionTuning::default()
        });
        let (_, handle, mut rx) = hub.register().unwrap();
        let gate = Arc::new(Barrier::new(3));

        let accepted = std::thread::scope(|scope| {
            let sender_handle = Arc::clone(&handle);
            let sender_gate = Arc::clone(&gate);
            let sender = scope.spawn(move || {
                sender_gate.wait();
                sender_handle.try_send_line(raw(8))
            });

            let close_handle = Arc::clone(&handle);
            let close_budget = Arc::clone(&handle.budget);
            let close_gate = Arc::clone(&gate);
            let closer = scope.spawn(move || {
                close_gate.wait();
                close_handle.request_close(CloseReason::ShuttingDown);
                close_budget.finish_writer(&mut rx);
            });

            gate.wait();
            let accepted = sender.join().unwrap();
            closer.join().unwrap();
            accepted
        });

        assert_eq!(handle.budget.snapshot(), (false, 0, 0));
        assert!(!handle.try_send_line(raw(1)));
        if accepted {
            assert!(handle.close.is_closed(), "accepted work must precede close");
        }
    }
}

#[test]
fn event_budget_matches_exact_max_width_wire_frame() {
    let payload = Arc::new(br#"{"kind":"shutting_down"}"#.to_vec());
    let seq = u64::MAX;
    let prefix = format!(
        "{{\"frame\":\"event\",\"seq\":{seq},\"topic\":\"{}\",\"event\":",
        Topic::Downloads.wire_str()
    )
    .into_bytes();
    let exact_wire_len = prefix.len() + payload.len() + 2;
    assert_eq!(prefix.len() + 2, 74, "pin the maximum envelope width");

    let tuning = SessionTuning {
        max_queued_items: 1,
        max_queued_bytes: exact_wire_len,
        ..SessionTuning::default()
    };
    let exact_hub = hub(tuning);
    let (_, handle, mut rx) = exact_hub.register().unwrap();
    handle.seq.store(u64::MAX - 1, Ordering::Relaxed);

    assert!(handle.push_event(Topic::Downloads, &payload));
    assert_eq!(handle.budget.snapshot(), (true, 1, exact_wire_len));
    let line = rx.try_recv().expect("event admitted at the exact byte cap");
    assert_eq!(line.cost(), Some(exact_wire_len));
    let SessionLine::Event {
        prefix: actual_prefix,
        payload: actual_payload,
        ..
    } = line
    else {
        panic!("expected event");
    };
    let mut actual = actual_prefix;
    actual.extend_from_slice(&actual_payload);
    actual.extend_from_slice(b"}\n");
    assert_eq!(actual.len(), exact_wire_len);
    assert!(actual.ends_with(b"}\n"));

    let too_small = hub(SessionTuning {
        max_queued_items: 1,
        max_queued_bytes: exact_wire_len - 1,
        ..SessionTuning::default()
    });
    let (_, rejected, mut rejected_rx) = too_small.register().unwrap();
    rejected.seq.store(u64::MAX - 1, Ordering::Relaxed);
    assert!(!rejected.push_event(Topic::Downloads, &payload));
    assert!(rejected_rx.try_recv().is_err());
    assert_eq!(rejected.budget.snapshot(), (true, 0, 0));

    let overflow = hub(SessionTuning::default());
    let (_, exhausted, mut exhausted_rx) = overflow.register().unwrap();
    exhausted.seq.store(u64::MAX, Ordering::Relaxed);
    assert!(!exhausted.push_event(Topic::Downloads, &payload));
    assert!(exhausted_rx.try_recv().is_err());
}

#[test]
fn broadcast_reaches_only_subscribers_with_per_session_seq_and_evicts_overflow() {
    let tuning = SessionTuning {
        max_queued_items: 2,
        max_queued_bytes: 1024,
        ..SessionTuning::default()
    };
    let hub = hub(tuning);
    let (_, sub, mut sub_rx) = hub.register().unwrap();
    let (_, other, mut other_rx) = hub.register().unwrap();
    assert_eq!(sub.subscribe(&[Topic::Player]), vec![Topic::Player]);
    assert!(sub.subscribe(&[Topic::Player]).is_empty(), "idempotent");
    other.subscribe(&[Topic::Queue]);

    let payload = Arc::new(br#"{"kind":"shutting_down"}"#.to_vec());
    for _ in 0..4 {
        hub.broadcast(Topic::Player, &payload);
    }
    assert_eq!(hub.active(), 1, "overflowing subscriber evicted");
    assert!(other_rx.try_recv().is_err(), "non-subscriber got nothing");
    assert!(
        !sub.try_send_line(SessionLine::Raw(vec![b'x'])),
        "evicted sessions accept nothing"
    );
    for want_seq in 1..=2u64 {
        match sub_rx.try_recv().expect("queued events survive eviction") {
            SessionLine::Event { seq, topic, .. } => {
                assert_eq!(seq, want_seq, "per-session monotonic");
                assert_eq!(topic, Topic::Player);
            }
            SessionLine::Raw(_) | SessionLine::TrackedRaw { .. } => panic!("expected event"),
        }
    }
}

#[test]
fn one_oversized_push_is_rejected_and_evicts_without_buffering() {
    let tuning = SessionTuning {
        max_queued_items: 4,
        max_queued_bytes: 128,
        ..SessionTuning::default()
    };
    let hub = hub(tuning);
    let (_, handle, mut rx) = hub.register().unwrap();
    handle.subscribe(&[Topic::Player]);

    hub.broadcast(Topic::Player, &Arc::new(vec![b'x'; 128]));

    assert_eq!(hub.active(), 0);
    assert!(handle.close.is_closed());
    assert_eq!(handle.budget.snapshot(), (false, 0, 0));
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn in_flight_frame_counts_against_item_and_byte_caps() {
    let tuning = SessionTuning {
        max_queued_items: 1,
        max_queued_bytes: 8,
        write_timeout: Duration::from_secs(1),
        ..SessionTuning::default()
    };
    let hub = hub(tuning);
    let (_, handle, rx) = hub.register().unwrap();
    assert!(handle.try_send_line(raw(8)));

    let partial = PartialWriterState::new();
    let started = partial.started.notified();
    let budget = Arc::clone(&handle.budget);
    let close = Arc::clone(&handle.close);
    let writer = tokio::spawn(run_session_writer(
        partial.writer(),
        rx,
        Arc::clone(&budget),
        close,
        tuning.write_timeout,
        Duration::ZERO,
    ));
    timeout(Duration::from_millis(100), started)
        .await
        .expect("writer must own the first frame");

    assert_eq!(budget.snapshot(), (true, 1, 8));
    assert!(
        !handle.try_send_line(SessionLine::Raw(vec![b'b'])),
        "the in-flight item must still consume the sole item slot"
    );
    handle.request_close(CloseReason::ClientGone);
    timeout(Duration::from_millis(100), writer)
        .await
        .expect("cancellation must stop the writer")
        .unwrap();
    assert_eq!(budget.snapshot(), (false, 0, 0));
}

#[tokio::test]
async fn cancelled_partial_frame_never_appends_a_goodbye() {
    let tuning = SessionTuning {
        max_queued_items: 1,
        max_queued_bytes: 1024,
        write_timeout: Duration::from_secs(1),
        ..SessionTuning::default()
    };
    let hub = hub(tuning);
    let (_, handle, rx) = hub.register().unwrap();
    assert!(handle.try_send_line(SessionLine::Raw(b"{\"frame\":\"reply\"}\n".to_vec())));

    let partial = PartialWriterState::new();
    let started = partial.started.notified();
    let budget = Arc::clone(&handle.budget);
    let close = Arc::clone(&handle.close);
    let writer = tokio::spawn(run_session_writer(
        partial.writer(),
        rx,
        Arc::clone(&budget),
        close,
        tuning.write_timeout,
        Duration::ZERO,
    ));
    timeout(Duration::from_millis(100), started)
        .await
        .expect("writer must emit a partial prefix");

    handle.request_close(CloseReason::SlowConsumer);
    timeout(Duration::from_millis(100), writer)
        .await
        .expect("close must interrupt the partial write")
        .unwrap();

    assert_eq!(partial.bytes(), b"{");
    assert!(!String::from_utf8_lossy(&partial.bytes()).contains("goodbye"));
    assert_eq!(budget.snapshot(), (false, 0, 0));
}

#[tokio::test]
async fn aborting_writer_clears_retained_accounting() {
    let tuning = SessionTuning {
        max_queued_items: 1,
        max_queued_bytes: 64,
        write_timeout: Duration::from_secs(60),
        ..SessionTuning::default()
    };
    let hub = hub(tuning);
    let (_, handle, rx) = hub.register().unwrap();
    assert!(handle.try_send_line(raw(8)));
    let partial = PartialWriterState::new();
    let started = partial.started.notified();
    let budget = Arc::clone(&handle.budget);
    let writer = tokio::spawn(run_session_writer(
        partial.writer(),
        rx,
        Arc::clone(&budget),
        Arc::clone(&handle.close),
        tuning.write_timeout,
        Duration::ZERO,
    ));
    timeout(Duration::from_millis(100), started)
        .await
        .expect("writer must retain the in-flight frame");
    assert_eq!(budget.snapshot(), (true, 1, 8));

    writer.abort();
    assert!(writer.await.unwrap_err().is_cancelled());
    assert_eq!(budget.snapshot(), (false, 0, 0));
}

#[tokio::test]
async fn non_reading_peer_cannot_pin_writer_past_deadline() {
    let (server, _non_reading_peer) = tokio::io::duplex(1);
    let (line_tx, line_rx) = mpsc::channel(1);
    let budget = Arc::new(OutboundBudget::new(1, 64 * 1024));
    let close = Arc::new(SessionClose::default());
    assert!(budget.try_send(&line_tx, &close, SessionLine::Raw(vec![b'x'; 64 * 1024])));

    timeout(
        Duration::from_millis(100),
        run_session_writer(
            server,
            line_rx,
            Arc::clone(&budget),
            Arc::clone(&close),
            Duration::from_millis(5),
            Duration::ZERO,
        ),
    )
    .await
    .expect("write and shutdown deadlines must bound a non-reading peer");

    assert!(close.is_closed());
    assert_eq!(budget.snapshot(), (false, 0, 0));
}

#[tokio::test]
async fn hello_ack_write_is_deadline_bounded() {
    let (mut writer, _non_reading_peer) = tokio::io::duplex(1);
    let ack = HelloAck {
        ok: true,
        version: PROTOCOL_VERSION,
        session_id: 1,
        capabilities: vec!["events-v8".to_string()],
        owner_mode: InstanceMode::StandaloneTui,
        reason: None,
    };

    let error = write_ack(&mut writer, &ack, Duration::from_millis(5))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}
