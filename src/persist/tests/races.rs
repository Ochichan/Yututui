use super::*;
use std::sync::Condvar;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

#[test]
fn older_actor_preparation_cannot_regress_a_newer_inflight_frontier() {
    let inflight: SharedInflight = Arc::new(Mutex::new(HashMap::new()));
    let older = test_operation(
        StoreKind::Config,
        journal_order_in_epoch(7, 1, 1),
        None,
        Arc::new(|| Ok(())),
    )
    .panic_operation()
    .unwrap();
    let newer = test_operation(
        StoreKind::Config,
        journal_order_in_epoch(7, 2, 2),
        None,
        Arc::new(|| Ok(())),
    )
    .panic_operation()
    .unwrap();
    let newer_order = newer.order;

    assert!(retain_newest_inflight(&inflight, newer));
    assert!(!retain_newest_inflight(&inflight, older));
    assert_eq!(
        lock_inflight(&inflight)[&StoreKind::Config].order,
        newer_order,
        "an actor resuming with an older preparation must preserve fallback ownership"
    );
}

#[test]
fn panic_shadow_is_independent_of_transition_locks_and_keeps_a_monotonic_frontier() {
    let older = test_operation(
        StoreKind::Config,
        journal_order_in_epoch(8, 1, 1),
        None,
        Arc::new(|| Ok(())),
    );
    let older_order = older.order;
    let prepared = older.panic_operation().unwrap();
    let newer = Arc::new(test_operation(
        StoreKind::Config,
        journal_order_in_epoch(8, 2, 2),
        None,
        Arc::new(|| Ok(())),
    ));
    let newer_order = newer.order;
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let inflight: SharedInflight = Arc::new(Mutex::new(HashMap::new()));
    let _pending_transition = lock(&pending);
    let _inflight_transition = lock_inflight(&inflight);
    let shadow = PanicShadow::new();

    shadow
        .publish(PanicOwnedOperation::Pending(Arc::new(older)))
        .unwrap();
    shadow
        .publish(PanicOwnedOperation::Prepared(Arc::new(prepared)))
        .unwrap();
    assert!(matches!(
        shadow.peek_for_test()[3],
        Some(PanicOwnedOperation::Prepared(_))
    ));

    shadow
        .publish(PanicOwnedOperation::Pending(Arc::clone(&newer)))
        .unwrap();
    shadow.clear_through(StoreKind::Config, older_order);
    let owned = shadow.peek_for_test()[3].clone().unwrap();
    assert_eq!(owned.order(), newer_order);
    assert!(matches!(owned, PanicOwnedOperation::Pending(_)));

    shadow.clear_through(StoreKind::Config, newer_order);
    assert!(shadow.peek_for_test()[3].is_none());
}

#[test]
fn reader_started_from_an_old_base_reloads_after_a_committed_frontier() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("reader-commit-race");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    crate::util::safe_fs::write_private_atomic(
        &path,
        serde_json::to_vec(&Tiny { value: 1 }).unwrap().as_slice(),
    )
    .unwrap();
    let old_base = crate::util::safe_fs::load_json_or_default_limited::<Tiny>(&path, 1024);

    let writer_path = path.clone();
    let order = journal_order(1, 41);
    let operation = test_operation(
        StoreKind::Config,
        order,
        Some(path.clone()),
        Arc::new(move || {
            crate::util::safe_fs::write_private_atomic(
                &writer_path,
                serde_json::to_vec(&Tiny { value: 9 }).unwrap().as_slice(),
            )
        }),
    );
    write_operation_durable(&operation).unwrap();
    let state = read_journal_state(StoreKind::Config, &path).unwrap();
    assert_eq!(state.committed_through, Some(order));
    assert!(state.candidate.is_none());

    let loaded = load_with_journal_recovery(StoreKind::Config, &path, 1024, || {
        crate::util::safe_fs::load_json_or_default_limited::<Tiny>(&path, 1024)
    });
    assert_eq!(old_base, Tiny { value: 1 });
    assert_eq!(loaded, Tiny { value: 9 });
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reader_retries_past_lock_timeout_and_loads_writer_commit() {
    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct Tiny {
        value: u8,
    }

    let dir = temp_dir("reader-lock-retry");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tiny.json");
    crate::util::safe_fs::write_private_atomic(
        &path,
        serde_json::to_vec(&Tiny { value: 1 }).unwrap().as_slice(),
    )
    .unwrap();

    let writer_path = path.clone();
    let order = journal_order(2, 42);
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (reader_started_tx, reader_started_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let _held = acquire_intent_lock(&writer_path).unwrap();
        held_tx.send(()).unwrap();
        reader_started_rx.recv().unwrap();

        // Hold ownership beyond one complete lock-acquisition timeout. The reader must retry,
        // not expose the old base snapshot while this writer is still in its critical section.
        std::thread::sleep(Duration::from_secs(6));
        let bytes = serde_json::to_vec(&Tiny { value: 9 }).unwrap();
        let intent = JournalIntent::Replace {
            order,
            kind: StoreKind::Config,
            path: writer_path.clone(),
            bytes: bytes.clone(),
        };
        let record = prepare_journal_record(&intent).unwrap();
        let state =
            replace_journal_with_record_locked(StoreKind::Config, &writer_path, &record).unwrap();
        assert_eq!(
            verify_intent_state(&state, order).unwrap(),
            IntentState::Current
        );
        crate::util::safe_fs::write_private_atomic(&writer_path, &bytes).unwrap();
        commit_journal_generation_locked(StoreKind::Config, &writer_path, order).unwrap();
    });
    held_rx.recv().unwrap();

    let base_calls = Arc::new(AtomicUsize::new(0));
    let observed_base_calls = Arc::clone(&base_calls);
    let finalizer_calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = Arc::clone(&finalizer_calls);
    let base_path = path.clone();
    let finalizer_path = path.clone();

    let reader_started_at = Instant::now();
    reader_started_tx.send(()).unwrap();
    let loaded = load_with_journal_recovery_then(
        StoreKind::Config,
        &path,
        1024,
        move || {
            observed_base_calls.fetch_add(1, Ordering::SeqCst);
            crate::util::safe_fs::load_json_or_default_limited::<Tiny>(&base_path, 1024)
        },
        move |coherent| {
            observed_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(coherent, &Tiny { value: 9 });
            crate::util::safe_fs::write_private_atomic(
                &finalizer_path,
                serde_json::to_vec(coherent).unwrap().as_slice(),
            )
            .unwrap();
        },
    );
    writer.join().unwrap();

    assert!(
        reader_started_at.elapsed() >= Duration::from_secs(5),
        "the reader must not return the exposed old base during the first lock timeout"
    );
    assert_eq!(loaded, Tiny { value: 9 });
    assert_eq!(base_calls.load(Ordering::SeqCst), 1);
    assert_eq!(finalizer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        crate::util::safe_fs::load_json_or_default_limited::<Tiny>(&path, 1024),
        Tiny { value: 9 },
        "the finalizer may only persist the coherent post-writer snapshot"
    );
    let state = read_journal_state(StoreKind::Config, &path).unwrap();
    assert_eq!(state.committed_through, Some(order));
    assert!(state.candidate.is_none());
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn panic_flush_owns_unjournaled_write_while_blocking_pool_is_full() {
    let dir = temp_dir("panic-inflight-ownership");
    std::fs::create_dir_all(&dir).unwrap();
    let late_parent = dir.join("created-after-journal-failure");
    let path = late_parent.join("config.json");
    std::fs::write(&late_parent, b"not a directory").unwrap();
    let writes = Arc::new(AtomicUsize::new(0));
    let writer_writes = Arc::clone(&writes);
    let writer_path = path.clone();
    let operation = pending_save(Snapshot::Test {
        kind: StoreKind::Config,
        label: "latest config after journal failure",
        storage_path: Some(path.clone()),
        writer: Arc::new(move || {
            writer_writes.fetch_add(1, Ordering::SeqCst);
            crate::util::safe_fs::write_private_atomic(&writer_path, b"null")
        }),
    });
    let expected_order = operation.order;
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    lock(&pending).insert(StoreKind::Config, operation);
    let inflight: SharedInflight = Arc::new(Mutex::new(HashMap::new()));

    journal_pending_operations(&pending).await;
    assert!(
        matches!(
            lock(&pending)[&StoreKind::Config].publication(),
            SnapshotPublication::NeedsJournal
        ),
        "the missing parent injects a journal failure before pool admission"
    );
    std::fs::remove_file(&late_parent).unwrap();
    std::fs::create_dir_all(&late_parent).unwrap();

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let blocker_gate = Arc::clone(&gate);
        let blocker_started = started_tx.clone();
        blockers.push(tokio::spawn(async move {
            crate::util::blocking::spawn_io(move || {
                blocker_started.send(()).unwrap();
                let (lock, wake) = &*blocker_gate;
                let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
                drop(
                    wake.wait_while(guard, |released| !*released)
                        .unwrap_or_else(PoisonError::into_inner),
                );
            })
            .await
            .unwrap();
        }));
    }
    drop(started_tx);
    tokio::task::spawn_blocking(move || {
        for _ in 0..2 {
            started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("both I/O permits become occupied");
        }
    })
    .await
    .unwrap();

    let write_pending = Arc::clone(&pending);
    let write_inflight = Arc::clone(&inflight);
    let panic_shadow = Arc::new(PanicShadow::new());
    panic_shadow
        .publish(PanicOwnedOperation::Pending(Arc::new(
            lock(&pending)[&StoreKind::Config].pending_clone(),
        )))
        .unwrap();
    let write_shadow = Arc::clone(&panic_shadow);
    let write_task = tokio::spawn(async move {
        let mut due = HashMap::from([(StoreKind::Config, tokio::time::Instant::now())]);
        let mut retries = HashMap::new();
        let events = Arc::new(Mutex::new(None));
        write_stores_with_inflight(
            &write_pending,
            &write_inflight,
            &write_shadow,
            &mut due,
            &mut retries,
            &events,
            false,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while lock_inflight(&inflight)
            .get(&StoreKind::Config)
            .is_none_or(|operation| operation.order != expected_order)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("writer retains shared ownership before blocking-pool admission");

    let panic_pending = PanicPending {
        inner: Arc::clone(&pending),
        shadow: Arc::clone(&panic_shadow),
    };
    let panic_owned = panic_pending.shadow.seal_and_snapshot().unwrap();
    let panic_owned = panic_owned[3].as_ref().expect("config shadow is owned");
    assert_eq!(panic_owned.order(), expected_order);
    assert_eq!(panic_owned.kind(), StoreKind::Config);
    panic_owned.write().unwrap();
    assert_eq!(writes.load(Ordering::SeqCst), 0);

    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap_or_else(PoisonError::into_inner) = true;
        wake.notify_all();
    }
    for blocker in blockers {
        blocker.await.unwrap();
    }
    assert!(write_task.await.unwrap());
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "the resumed actor observes the committed order instead of duplicating the hook write"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "null");
    clear_store_journal(&path);
    let _ = std::fs::remove_dir_all(dir);
}
