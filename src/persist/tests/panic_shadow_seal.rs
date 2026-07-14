use super::*;

fn test_handle() -> PersistHandle {
    let (tx, _rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    PersistHandle {
        tx,
        pending: Arc::new(Mutex::new(PendingQueue::new())),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        dirty: Arc::new(Notify::new()),
        events: Arc::new(Mutex::new(None)),
        order_source: test_order_source(),
        panic_shadow: Arc::new(PanicShadow::new()),
    }
}

fn snapshot(kind: StoreKind, label: &'static str, writes: &Arc<AtomicUsize>) -> Snapshot {
    let writes = Arc::clone(writes);
    Snapshot::Test {
        kind,
        label,
        storage_path: None,
        writer: Arc::new(move || {
            writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    }
}

#[tokio::test]
async fn paused_panic_hook_snapshot_rejects_every_late_admission_without_partial_owner_batch() {
    let handle = test_handle();
    let writes = Arc::new(AtomicUsize::new(0));
    let _ = handle
        .save(snapshot(StoreKind::Config, "accepted config", &writes))
        .unwrap();

    let shadow = Arc::clone(&handle.panic_shadow);
    let (sealed_tx, sealed_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let paused_hook = std::thread::spawn(move || {
        let snapshot = shadow
            .seal_and_snapshot()
            .expect("first panic hook seals and owns the frontier");
        sealed_tx.send(()).expect("test observes panic seal");
        release_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("test releases paused panic hook");
        snapshot
    });
    tokio::task::spawn_blocking(move || {
        sealed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("panic hook reaches its sealed snapshot boundary")
    })
    .await
    .unwrap();

    assert_eq!(
        handle.save(snapshot(StoreKind::Library, "late library", &writes)),
        Err(crate::util::delivery::DeliveryError::Closed),
        "normal save must report rejection after the panic boundary"
    );
    assert_eq!(
        handle.delete_romanized_titles(),
        Err(crate::util::delivery::DeliveryError::Closed),
        "romanize deletion must report rejection after the panic boundary"
    );
    assert_eq!(
        handle.seal_with_snapshots([
            snapshot(StoreKind::Downloads, "late downloads", &writes),
            snapshot(StoreKind::Signals, "late signals", &writes),
        ]),
        Err(crate::util::delivery::DeliveryError::Closed),
        "owner sealing must reject its entire batch after the panic boundary"
    );
    let fallback_error = handle
        .save_ordered_fallback(snapshot(StoreKind::Station, "late fallback", &writes))
        .await
        .expect_err("ordered fallback cannot publish after the panic boundary");
    assert_eq!(fallback_error.kind(), std::io::ErrorKind::WouldBlock);

    let pending = lock(&handle.pending);
    assert_eq!(
        pending.len(),
        1,
        "late attempts must not enter pending state"
    );
    assert!(pending.contains_key(&StoreKind::Config));
    assert!(!pending.contains_key(&StoreKind::Library));
    assert!(!pending.contains_key(&StoreKind::RomanizedTitles));
    assert!(!pending.contains_key(&StoreKind::Downloads));
    assert!(!pending.contains_key(&StoreKind::Signals));
    assert!(!pending.contains_key(&StoreKind::Station));
    drop(pending);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert!(matches!(
        handle.panic_shadow.seal_and_snapshot(),
        Err(PanicShadowSealed)
    ));

    release_tx.send(()).expect("release paused panic hook");
    let sealed = paused_hook.join().expect("paused panic hook joins");
    let owned = sealed[3].as_ref().expect("accepted config is snapshotted");
    assert_eq!(owned.kind(), StoreKind::Config);
    assert!(matches!(owned, PanicOwnedOperation::Pending(_)));
}

#[tokio::test]
async fn actor_prepared_rejection_preserves_the_pending_operation_captured_by_the_hook() {
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let inflight: SharedInflight = Arc::new(Mutex::new(HashMap::new()));
    let shadow = PanicShadow::new();
    let writes = Arc::new(AtomicUsize::new(0));
    let operation = test_operation(StoreKind::Config, journal_order_in_epoch(11, 1, 1), None, {
        let writes = Arc::clone(&writes);
        Arc::new(move || {
            writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    let order = operation.order;
    lock(&pending).insert(StoreKind::Config, operation.clone());
    shadow
        .publish(PanicOwnedOperation::Pending(Arc::new(operation)))
        .unwrap();
    let sealed = shadow
        .seal_and_snapshot()
        .expect("hook seals the accepted Pending frontier");
    let captured = sealed[3]
        .as_ref()
        .expect("hook captured actor-owned config")
        .clone();

    let mut due = HashMap::from([(StoreKind::Config, tokio::time::Instant::now())]);
    let mut retries = HashMap::new();
    let events = Arc::new(Mutex::new(None));
    assert!(
        write_stores_with_inflight(
            &pending,
            &inflight,
            &shadow,
            &mut due,
            &mut retries,
            &events,
            false,
        )
        .await,
        "actor continues an operation whose Pending form is already panic-owned"
    );

    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert!(lock(&pending).is_empty());
    assert!(lock_inflight(&inflight).is_empty());
    assert_eq!(captured.order(), order);
    assert!(matches!(captured, PanicOwnedOperation::Pending(_)));
}

fn pending_owner(order: JournalOrder) -> PanicOwnedOperation {
    PanicOwnedOperation::Pending(Arc::new(test_operation(
        StoreKind::Config,
        order,
        None,
        Arc::new(|| Ok(())),
    )))
}

fn prepared_owner(order: JournalOrder) -> PanicOwnedOperation {
    let operation = test_operation(StoreKind::Config, order, None, Arc::new(|| Ok(())));
    PanicOwnedOperation::Prepared(Arc::new(operation.panic_operation().unwrap()))
}

#[test]
fn panic_shadow_slot_transitions_are_monotonic_and_exhaustive() {
    let shadow = PanicShadow::new();
    let older = journal_order_in_epoch(31, 1, 0xb1);
    let current = journal_order_in_epoch(31, 2, 0xb2);
    let newer = journal_order_in_epoch(31, 3, 0xb3);

    shadow.publish(pending_owner(current)).unwrap();
    assert!(matches!(
        shadow.peek_for_test()[3],
        Some(PanicOwnedOperation::Pending(_))
    ));
    shadow.publish(prepared_owner(current)).unwrap();
    assert!(matches!(
        shadow.peek_for_test()[3],
        Some(PanicOwnedOperation::Prepared(_))
    ));

    shadow.publish(pending_owner(current)).unwrap();
    shadow.publish(prepared_owner(older)).unwrap();
    let retained = shadow.peek_for_test()[3].clone().unwrap();
    assert_eq!(retained.order(), current);
    assert!(matches!(retained, PanicOwnedOperation::Prepared(_)));

    shadow.publish(pending_owner(newer)).unwrap();
    let retained = shadow.peek_for_test()[3].clone().unwrap();
    assert_eq!(retained.order(), newer);
    assert!(matches!(retained, PanicOwnedOperation::Pending(_)));

    shadow.clear_through(StoreKind::Config, current);
    assert_eq!(shadow.peek_for_test()[3].as_ref().unwrap().order(), newer);
    shadow.clear_through(StoreKind::Config, newer);
    assert!(shadow.peek_for_test()[3].is_none());

    shadow.publish(pending_owner(current)).unwrap();
    shadow.clear_through(StoreKind::Config, newer);
    assert!(
        shadow.peek_for_test()[3].is_none(),
        "a durable frontier above the retained order must clear stale panic ownership"
    );
}

#[test]
fn open_snapshot_does_not_seal_but_terminal_seal_rejects_every_publisher() {
    let shadow = PanicShadow::new();
    let first = journal_order_in_epoch(32, 1, 0xc1);
    let second = journal_order_in_epoch(32, 2, 0xc2);
    shadow.publish(pending_owner(first)).unwrap();
    assert_eq!(shadow.snapshot()[3].as_ref().unwrap().order(), first);
    shadow.publish(pending_owner(second)).unwrap();

    let sealed = shadow.seal_and_snapshot().unwrap();
    assert_eq!(sealed[3].as_ref().unwrap().order(), second);
    assert!(matches!(
        shadow.publish(prepared_owner(second)),
        Err(PanicShadowSealed)
    ));
    assert!(matches!(
        shadow.publish_batch(vec![pending_owner(journal_order_in_epoch(32, 3, 0xc3))]),
        Err(PanicShadowSealed)
    ));
    assert!(matches!(shadow.seal_and_snapshot(), Err(PanicShadowSealed)));

    shadow.clear_through(StoreKind::Config, second);
    assert!(shadow.snapshot()[3].is_none());
}

#[test]
fn concurrent_publish_and_seal_have_one_linearized_frontier() {
    let shadow = Arc::new(PanicShadow::new());
    let order = journal_order_in_epoch(33, 1, 0xd1);
    let (publish_locked_tx, publish_locked_rx) = std::sync::mpsc::channel();
    let (release_publish_tx, release_publish_rx) = std::sync::mpsc::channel();

    let publish_shadow = Arc::clone(&shadow);
    let publisher = std::thread::spawn(move || {
        publish_shadow.publish_with_lock_hook_for_test(pending_owner(order), || {
            publish_locked_tx.send(()).unwrap();
            release_publish_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
        })
    });
    publish_locked_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("publisher reaches the production write-lock critical section");

    let seal_shadow = Arc::clone(&shadow);
    let (seal_started_tx, seal_started_rx) = std::sync::mpsc::channel();
    let sealer = std::thread::spawn(move || {
        seal_started_tx.send(()).unwrap();
        seal_shadow.seal_and_snapshot().unwrap()
    });
    seal_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("sealer starts while the publisher owns the write lock");
    release_publish_tx.send(()).unwrap();

    assert_eq!(publisher.join().unwrap(), Ok(()));
    let sealed = sealer.join().unwrap();
    let Some(PanicOwnedOperation::Pending(operation)) = sealed[3].as_ref() else {
        panic!("publish-first frontier must retain the exact pending form");
    };
    assert_eq!(operation.order, order);
}
