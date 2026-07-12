use super::*;

fn test_handle() -> PersistHandle {
    let (tx, _rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    PersistHandle {
        tx,
        pending: Arc::new(Mutex::new(HashMap::new())),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        dirty: Arc::new(Notify::new()),
        events: Arc::new(Mutex::new(None)),
        order_source: test_order_source(),
        admission_open: Arc::new(AtomicBool::new(true)),
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
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
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
