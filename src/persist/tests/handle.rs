use super::*;

#[test]
fn save_replaces_pending_without_queueing_payloads() {
    let (tx, mut rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let handle = PersistHandle {
        tx,
        pending: Arc::clone(&pending),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        dirty: Arc::new(Notify::new()),
        events: Arc::new(Mutex::new(None)),
        order_source: test_order_source(),
        panic_shadow: Arc::new(PanicShadow::new()),
    };
    let mut created = crate::playlists::Playlists::default();
    created.create("Focus").expect("playlist created");
    let mut added = created.clone();
    assert_eq!(
        added.add(
            "Focus",
            crate::api::Song::remote("id0", "Track", "Artist", "3:00")
        ),
        crate::playlists::AddResult::Added
    );

    let _ = handle.save(Snapshot::Playlists(created)).unwrap();
    let _ = handle.save(Snapshot::Playlists(added)).unwrap();

    assert!(rx.try_recv().is_err(), "save must not enqueue snapshots");
    let guard = lock(&pending);
    let Some(OwnedSnapshot::Playlists(playlists)) = guard
        .get(&StoreKind::Playlists)
        .and_then(|operation| operation.snapshot())
    else {
        panic!("expected playlists snapshot");
    };
    let focus = playlists.find("Focus").expect("focus playlist");
    assert_eq!(focus.songs.len(), 1);
}

fn injected_snapshot(kind: StoreKind, label: &'static str) -> Snapshot {
    Snapshot::Test {
        kind,
        label,
        storage_path: None,
        writer: Arc::new(|| Ok(())),
    }
}

fn detached_test_handle() -> PersistHandle {
    PersistHandle {
        tx: crate::util::backpressure::bounded_channel(
            crate::util::backpressure::PERSIST_CONTROL_QUEUE,
        )
        .0,
        pending: Arc::new(Mutex::new(PendingQueue::new())),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        dirty: Arc::new(Notify::new()),
        events: Arc::new(Mutex::new(None)),
        order_source: test_order_source(),
        panic_shadow: Arc::new(PanicShadow::new()),
    }
}

#[test]
fn ordering_failure_precedes_intent_lock_acquisition() {
    let dir = temp_dir("ordering-before-intent-lock");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let _held = acquire_intent_lock(&path).unwrap();
    let reason: Arc<str> = Arc::from("injected ordering allocation failure");
    let operation = PendingOperation::save(
        Snapshot::Test {
            kind: StoreKind::Config,
            label: "ordering before intent lock",
            storage_path: Some(path.clone()),
            writer: Arc::new(|| panic!("an unordered operation must not reach its writer")),
        },
        AcceptedJournalOrder {
            order: journal_order(1, 1),
            error: Some(Arc::clone(&reason)),
        },
    );
    let (contended_tx, contended_rx) = std::sync::mpsc::channel();

    let error =
        with_intent_lock_contention_observer(contended_tx, || write_operation_durable(&operation))
            .unwrap_err();

    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(error.to_string(), reason.as_ref());
    assert!(
        contended_rx.try_recv().is_err(),
        "ordering failure must return before attempting the contended intent lock"
    );
    drop(_held);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn snapshot_publication_transitions_are_monotonic_and_clone_local() {
    let order = journal_order_in_epoch(21, 1, 0xa1);
    let mut operation = PendingOperation::save(
        injected_snapshot(StoreKind::Config, "publication"),
        accepted_order(order),
    );
    let shadow_clone = operation.clone();
    assert_eq!(operation.publication(), &SnapshotPublication::NeedsJournal);

    operation.resolve_journal_for_test();
    operation.resolve_journal_for_test();
    assert_eq!(
        operation.publication(),
        &SnapshotPublication::JournalResolved
    );
    assert_eq!(
        shadow_clone.publication(),
        &SnapshotPublication::NeedsJournal,
        "map-local journal completion must not mutate the panic-owned clone"
    );

    let reason: Arc<str> = Arc::from("ordering allocation failed");
    let mut unavailable = PendingOperation::save(
        injected_snapshot(StoreKind::Config, "unavailable"),
        AcceptedJournalOrder {
            order,
            error: Some(Arc::clone(&reason)),
        },
    );
    unavailable.resolve_journal_for_test();
    assert_eq!(
        unavailable.publication(),
        &SnapshotPublication::OrderingUnavailable(Arc::clone(&reason))
    );
    assert_eq!(
        unavailable
            .publication()
            .ensure_ordering()
            .unwrap_err()
            .to_string(),
        reason.as_ref()
    );
}

#[tokio::test]
async fn exact_journal_completion_resolves_the_pending_snapshot_once() {
    let directory = temp_dir("snapshot-publication-state");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("config.json");
    let order = journal_order_in_epoch(22, 1, 0xa2);
    let operation = test_operation(
        StoreKind::Config,
        order,
        Some(path.clone()),
        Arc::new(|| Ok(())),
    );
    let shadow = PanicShadow::new();
    let operation = publish_pending_operation(&shadow, operation).unwrap();
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    lock(&pending).insert_owned(operation);

    journal_pending_operations(&pending).await;
    assert_eq!(
        lock(&pending)[&StoreKind::Config].publication(),
        &SnapshotPublication::JournalResolved
    );
    let journal_path = intent_journal_path(&path).unwrap();
    let first_publication = std::fs::read(&journal_path).unwrap();

    journal_pending_operations(&pending).await;
    assert_eq!(std::fs::read(&journal_path).unwrap(), first_publication);
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn superseded_completion_resolves_exact_operation_but_stale_does_not_resolve_replacement() {
    let directory = temp_dir("snapshot-publication-superseded");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("config.json");
    let old_order = journal_order_in_epoch(24, 1, 0xa4);
    let newer_order = journal_order_in_epoch(24, 2, 0xa5);
    write_journal_intent(&JournalIntent::Replace {
        order: newer_order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: b"newer".to_vec(),
    })
    .unwrap();
    commit_journal_generation(StoreKind::Config, &path, newer_order).unwrap();

    let shadow = PanicShadow::new();
    let old = test_operation(
        StoreKind::Config,
        old_order,
        Some(path.clone()),
        Arc::new(|| Ok(())),
    );
    let old = publish_pending_operation(&shadow, old).unwrap();
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    lock(&pending).insert_owned(old);
    journal_pending_operations(&pending).await;
    assert_eq!(
        lock(&pending)[&StoreKind::Config].publication(),
        &SnapshotPublication::JournalResolved,
        "a confirmed superseded append settles the exact map operation"
    );

    let stale_intent = JournalIntent::Replace {
        order: old_order,
        kind: StoreKind::Config,
        path: path.clone(),
        bytes: b"stale".to_vec(),
    };
    let replacement = test_operation(
        StoreKind::Config,
        newer_order,
        Some(path.clone()),
        Arc::new(|| Ok(())),
    );
    lock(&pending).insert(StoreKind::Config, replacement);
    assert!(matches!(
        write_journal_intent_if_current(&stale_intent, &pending).unwrap(),
        JournalAppend::Stale
    ));
    {
        let mut pending = lock(&pending);
        assert!(
            !pending.resolve_journal(JournalCompletion::confirmed(StoreKind::Config, old_order,)),
            "an exact-order completion token must not settle a replacement"
        );
        assert_eq!(
            pending[&StoreKind::Config].publication(),
            &SnapshotPublication::NeedsJournal
        );
    }

    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(directory);
}

#[tokio::test]
async fn ordering_unavailable_stays_pending_without_inflight_transition() {
    let order = journal_order_in_epoch(23, 1, 0xa3);
    let reason: Arc<str> = Arc::from("injected ordering allocation failure");
    let operation = PendingOperation::save(
        injected_snapshot(StoreKind::Config, "unordered config"),
        AcceptedJournalOrder {
            order,
            error: Some(Arc::clone(&reason)),
        },
    );
    let shadow = PanicShadow::new();
    let operation = publish_pending_operation(&shadow, operation).unwrap();
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    lock(&pending).insert_owned(operation);
    let inflight: SharedInflight = Arc::new(Mutex::new(HashMap::new()));
    let mut due = HashMap::new();
    let mut retries = RetryMap::new();
    let events: EventSinkSlot = Arc::new(Mutex::new(None));

    assert!(
        !write_stores_with_inflight(
            &pending,
            &inflight,
            &shadow,
            &mut due,
            &mut retries,
            &events,
            true,
        )
        .await
    );

    let pending = lock(&pending);
    let retained = pending
        .get(&StoreKind::Config)
        .expect("unordered operation remains pending");
    assert_eq!(retained.order, order);
    assert_eq!(
        retained.publication(),
        &SnapshotPublication::OrderingUnavailable(Arc::clone(&reason))
    );
    drop(pending);
    assert!(lock_inflight(&inflight).is_empty());
    assert!(due.contains_key(&StoreKind::Config));
    assert_eq!(retries[&StoreKind::Config].retry_count, 1);

    let owned = shadow.peek_for_test();
    let Some(PanicOwnedOperation::Pending(operation)) = owned[3].as_ref() else {
        panic!("unordered operation must stay in pending panic-shadow form");
    };
    assert_eq!(operation.order, order);
    assert_eq!(
        operation.publication(),
        &SnapshotPublication::OrderingUnavailable(reason)
    );
}

#[test]
fn clean_seal_is_monotonic_while_repeated_final_batches_stay_refreshable() {
    let handle = PersistHandle {
        tx: crate::util::backpressure::bounded_channel(
            crate::util::backpressure::PERSIST_CONTROL_QUEUE,
        )
        .0,
        pending: Arc::new(Mutex::new(PendingQueue::new())),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        dirty: Arc::new(Notify::new()),
        events: Arc::new(Mutex::new(None)),
        order_source: test_order_source(),
        panic_shadow: Arc::new(PanicShadow::new()),
    };

    handle
        .seal_with_snapshots([injected_snapshot(StoreKind::Config, "first final")])
        .unwrap();
    assert_eq!(lock(&handle.pending).admission(), SnapshotAdmission::Sealed);
    assert_eq!(
        handle.save(injected_snapshot(StoreKind::Library, "late ordinary")),
        Err(crate::util::delivery::DeliveryError::Closed)
    );

    handle
        .seal_with_snapshots([injected_snapshot(StoreKind::Config, "refreshed final")])
        .unwrap();
    let (pending_order, pending_label) = {
        let pending = lock(&handle.pending);
        let operation = pending
            .get(&StoreKind::Config)
            .expect("expected refreshed final operation");
        let Some(OwnedSnapshot::Test { label, .. }) = operation.snapshot() else {
            panic!("expected refreshed final snapshot");
        };
        (operation.order, *label)
    };
    assert_eq!(pending_label, "refreshed final");

    let shadow = handle.panic_shadow.peek_for_test();
    let Some(PanicOwnedOperation::Pending(shadow_operation)) = shadow[3].as_ref() else {
        panic!("panic shadow must own the refreshed final in pending form");
    };
    assert_eq!(shadow_operation.order, pending_order);
    assert_eq!(shadow_operation.label(), pending_label);
}

#[test]
fn failed_clean_seal_closes_admission_without_publishing_snapshots() {
    let handle = detached_test_handle();
    let sequence_before = *handle
        .order_source
        .next_sequence
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    assert!(
        handle
            .panic_shadow
            .peek_for_test()
            .iter()
            .all(Option::is_none)
    );

    let worker_handle = handle.clone();
    let (sealed_tx, sealed_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        worker_handle.seal_with_snapshots_after_check_and_hook_for_test(
            std::iter::once_with(|| {
                panic!("failed mutation check must not consume the snapshots iterator")
            }),
            Err(std::io::Error::other("mutation gate revoked")),
            || {
                sealed_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            },
        )
    });
    sealed_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("failed seal reaches the production admission critical section");

    let late_handle = handle.clone();
    let (save_started_tx, save_started_rx) = std::sync::mpsc::channel();
    let late_saver = std::thread::spawn(move || {
        save_started_tx.send(()).unwrap();
        late_handle.save(injected_snapshot(StoreKind::Config, "late ordinary"))
    });
    save_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("late saver starts while the failed seal owns admission");
    release_tx.send(()).unwrap();

    assert_eq!(
        worker.join().unwrap(),
        Err(crate::util::delivery::DeliveryError::Closed)
    );
    assert_eq!(
        late_saver.join().unwrap(),
        Err(crate::util::delivery::DeliveryError::Closed)
    );
    assert_eq!(lock(&handle.pending).admission(), SnapshotAdmission::Sealed);
    assert!(lock(&handle.pending).is_empty());
    assert_eq!(
        *handle
            .order_source
            .next_sequence
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
        sequence_before,
        "mutation rejection must not allocate an order"
    );
    assert!(
        handle
            .panic_shadow
            .peek_for_test()
            .iter()
            .all(Option::is_none),
        "mutation rejection must not publish a panic batch"
    );
}
