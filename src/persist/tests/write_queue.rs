use std::sync::atomic::AtomicBool;

use super::*;

#[tokio::test]
async fn write_stores_clears_stale_due_entries_when_snapshot_is_missing() {
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let mut due = HashMap::from([(StoreKind::Library, tokio::time::Instant::now())]);
    let mut retries = HashMap::new();
    let events = Arc::new(Mutex::new(None));

    write_stores(&pending, &mut due, &mut retries, &events, false).await;

    assert!(due.is_empty());
    assert!(lock(&pending).is_empty());
}

#[test]
fn queued_saves_are_latest_wins_without_extending_deadline() {
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let mut due = HashMap::new();
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

    queue_pending_save(&pending, &mut due, Snapshot::Playlists(Arc::new(created)));
    let first_due = due[&StoreKind::Playlists];
    queue_pending_save(&pending, &mut due, Snapshot::Playlists(Arc::new(added)));

    assert_eq!(due[&StoreKind::Playlists], first_due);
    let guard = lock(&pending);
    let Some(OwnedSnapshot::Playlists(playlists)) = guard
        .get(&StoreKind::Playlists)
        .and_then(|operation| operation.snapshot())
    else {
        panic!("expected playlists snapshot");
    };
    let focus = playlists.find("Focus").expect("focus playlist");
    assert_eq!(focus.songs.len(), 1);
    assert_eq!(focus.songs[0].video_id, "id0");
}

#[tokio::test]
async fn write_stores_requeues_failed_snapshot_and_retries_until_success() {
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let mut due = HashMap::from([(StoreKind::Config, tokio::time::Instant::now())]);
    let mut retries = HashMap::new();
    let events = Arc::new(Mutex::new(None));
    let attempts = Arc::new(AtomicUsize::new(0));
    let writer_attempts = Arc::clone(&attempts);
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "config",
            storage_path: None,
            writer: Arc::new(move || {
                if writer_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(std::io::Error::other("disk full"))
                } else {
                    Ok(())
                }
            }),
        }),
    );

    let clean = write_stores(&pending, &mut due, &mut retries, &events, false).await;

    assert!(!clean);
    assert!(lock(&pending).contains_key(&StoreKind::Config));
    assert!(due.contains_key(&StoreKind::Config));
    assert_eq!(retries[&StoreKind::Config].retry_count, 1);

    let clean = write_stores(&pending, &mut due, &mut retries, &events, true).await;

    assert!(clean);
    assert!(lock(&pending).is_empty());
    assert!(!retries.contains_key(&StoreKind::Config));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn panicking_blocking_writer_is_requeued_and_can_succeed_on_retry() {
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let mut due = HashMap::from([(StoreKind::Config, tokio::time::Instant::now())]);
    let mut retries = HashMap::new();
    let events = Arc::new(Mutex::new(None));
    let attempts = Arc::new(AtomicUsize::new(0));
    let writer_attempts = Arc::clone(&attempts);
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "panicking config",
            storage_path: None,
            writer: Arc::new(move || {
                if writer_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("fault injection: blocking persistence writer panic");
                }
                Ok(())
            }),
        }),
    );

    assert!(!write_stores(&pending, &mut due, &mut retries, &events, false).await);
    assert!(lock(&pending).contains_key(&StoreKind::Config));
    assert!(due.contains_key(&StoreKind::Config));

    assert!(write_stores(&pending, &mut due, &mut retries, &events, true).await);
    assert!(lock(&pending).is_empty());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn disk_full_during_write_preserves_a_newer_coalesced_snapshot() {
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let captured_events = Arc::new(Mutex::new(Vec::new()));
    let event_log = Arc::clone(&captured_events);
    let events: EventSinkSlot = Arc::new(Mutex::new(Some(Arc::new(move |event| {
        event_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }))));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let writer_release = Arc::clone(&release_rx);
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "older config",
            storage_path: None,
            writer: Arc::new(move || {
                started_tx.send(()).expect("test observes writer start");
                writer_release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test releases injected disk write");
                Err(std::io::Error::other(
                    "fault injection: no space left on device",
                ))
            }),
        }),
    );

    let write_pending = Arc::clone(&pending);
    let write_events = Arc::clone(&events);
    let write_task = tokio::spawn(async move {
        let mut due = HashMap::from([(StoreKind::Config, tokio::time::Instant::now())]);
        let mut retries = HashMap::new();
        let clean =
            write_stores(&write_pending, &mut due, &mut retries, &write_events, false).await;
        (clean, due, retries)
    });

    tokio::task::spawn_blocking(move || {
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("injected writer starts")
    })
    .await
    .expect("writer-start observer joins");
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "newer config",
            storage_path: None,
            writer: Arc::new(|| Ok(())),
        }),
    );
    release_tx.send(()).expect("release injected writer");

    let (clean, mut due, mut retries) = write_task.await.expect("write task joins");
    assert!(!clean, "the newer snapshot is still dirty");
    {
        let guard = lock(&pending);
        let Some(OwnedSnapshot::Test { label, .. }) = guard
            .get(&StoreKind::Config)
            .and_then(|operation| operation.snapshot())
        else {
            panic!("expected injected config snapshot");
        };
        assert_eq!(*label, "newer config");
    }
    {
        let failures = captured_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert_eq!(failures.len(), 1);
        let (store, error) = failures[0].write_failure().unwrap();
        assert_eq!(*store, StoreKind::Config);
        assert!(error.contains("no space left on device"));
    }

    assert!(
        write_stores(&pending, &mut due, &mut retries, &events, true).await,
        "a later flush writes the retained latest snapshot"
    );
    assert!(lock(&pending).is_empty());
    assert!(!retries.contains_key(&StoreKind::Config));
}

#[test]
fn failed_snapshot_does_not_overwrite_newer_pending_snapshot() {
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let mut due = HashMap::new();
    let mut retries = HashMap::new();
    let events = Arc::new(Mutex::new(None));
    lock(&pending).insert(
        StoreKind::Config,
        pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "newer",
            storage_path: None,
            writer: Arc::new(|| Ok(())),
        }),
    );

    requeue_failed_operation(
        &pending,
        &mut due,
        &mut retries,
        &events,
        ShadowCoveredOperation::for_test(pending_save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "older",
            storage_path: None,
            writer: Arc::new(|| Ok(())),
        })),
        "transient".to_owned(),
    );

    let guard = lock(&pending);
    let Some(OwnedSnapshot::Test { label, .. }) = guard
        .get(&StoreKind::Config)
        .and_then(|operation| operation.snapshot())
    else {
        panic!("expected test snapshot");
    };
    assert_eq!(*label, "newer");
}

#[tokio::test]
async fn flush_returns_false_when_write_keeps_failing() {
    let handle = spawn();
    let _ = handle
        .save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "config",
            storage_path: None,
            writer: Arc::new(|| Err(std::io::Error::other("still full"))),
        })
        .unwrap();

    assert!(!handle.flush(Duration::from_secs(1)).await);
    assert!(lock(&handle.pending().inner).contains_key(&StoreKind::Config));
}

#[tokio::test]
async fn first_high_value_failure_emits_one_status_event() {
    let handle = spawn();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    handle.set_event_sink(move |event| {
        captured.lock().unwrap().push(event);
    });
    let _ = handle
        .save(Snapshot::Test {
            kind: StoreKind::Library,
            label: "library",
            storage_path: None,
            writer: Arc::new(|| Err(std::io::Error::other("permission denied"))),
        })
        .unwrap();

    assert!(!handle.flush(Duration::from_secs(1)).await);
    assert!(!handle.flush(Duration::from_secs(1)).await);

    let guard = events.lock().unwrap();
    assert_eq!(guard.len(), 1);
    let (store, error) = guard[0].write_failure().unwrap();
    assert_eq!(*store, StoreKind::Library);
    assert!(error.contains("permission denied"));
}

#[tokio::test]
async fn delete_is_latest_wins_with_save_in_both_orders() {
    let (tx, _rx) = crate::util::backpressure::bounded_channel(
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
    let saves = Arc::new(AtomicUsize::new(0));
    let old_save_count = Arc::clone(&saves);
    let deletes = Arc::new(AtomicUsize::new(0));
    let delete_count = Arc::clone(&deletes);
    let _ = handle
        .save(Snapshot::Test {
            kind: StoreKind::RomanizedTitles,
            label: "romanized title cache",
            storage_path: None,
            writer: Arc::new(move || {
                old_save_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        })
        .unwrap();
    handle.delete_romanized_titles_with(move || {
        delete_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });

    let mut due = HashMap::new();
    let mut retries = HashMap::new();
    assert!(write_stores(&pending, &mut due, &mut retries, &handle.events, true).await);
    assert_eq!(saves.load(Ordering::SeqCst), 0);
    assert_eq!(deletes.load(Ordering::SeqCst), 1);

    let replaced_delete_count = Arc::clone(&deletes);
    handle.delete_romanized_titles_with(move || {
        replaced_delete_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let new_save_count = Arc::clone(&saves);
    let _ = handle
        .save(Snapshot::Test {
            kind: StoreKind::RomanizedTitles,
            label: "newer romanized title cache",
            storage_path: None,
            writer: Arc::new(move || {
                new_save_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        })
        .unwrap();

    assert!(write_stores(&pending, &mut due, &mut retries, &handle.events, true).await);
    assert_eq!(deletes.load(Ordering::SeqCst), 1);
    assert_eq!(saves.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn delete_queued_during_an_older_save_runs_after_that_save() {
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let events = Arc::new(Mutex::new(None));
    let order = Arc::new(Mutex::new(Vec::new()));
    let save_order = Arc::clone(&order);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let save_release = Arc::clone(&release_rx);
    lock(&pending).insert(
        StoreKind::RomanizedTitles,
        pending_save(Snapshot::Test {
            kind: StoreKind::RomanizedTitles,
            label: "older romanized title cache",
            storage_path: None,
            writer: Arc::new(move || {
                started_tx.send(()).expect("test observes save start");
                save_release
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .recv_timeout(Duration::from_secs(5))
                    .expect("test releases old save");
                save_order.lock().unwrap().push("save");
                Ok(())
            }),
        }),
    );
    let write_pending = Arc::clone(&pending);
    let write_events = Arc::clone(&events);
    let write_task = tokio::spawn(async move {
        let mut due = HashMap::from([(StoreKind::RomanizedTitles, tokio::time::Instant::now())]);
        let mut retries = HashMap::new();
        let clean =
            write_stores(&write_pending, &mut due, &mut retries, &write_events, false).await;
        (clean, due, retries)
    });
    tokio::task::spawn_blocking(move || {
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("old save starts")
    })
    .await
    .expect("save-start observer joins");

    let (tx, _rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    let handle = PersistHandle {
        tx,
        pending: Arc::clone(&pending),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        dirty: Arc::new(Notify::new()),
        events: Arc::clone(&events),
        order_source: test_order_source(),
        panic_shadow: Arc::new(PanicShadow::new()),
    };
    let delete_order = Arc::clone(&order);
    handle.delete_romanized_titles_with(move || {
        delete_order.lock().unwrap().push("delete");
        Ok(())
    });
    release_tx.send(()).expect("release old save");

    let (clean, mut due, mut retries) = write_task.await.expect("old save task joins");
    assert!(!clean, "the newer delete must remain pending");
    assert!(
        write_stores(&pending, &mut due, &mut retries, &events, true).await,
        "the newer delete completes on the next drain"
    );
    assert_eq!(*order.lock().unwrap(), ["save", "delete"]);
}

#[tokio::test]
async fn failed_delete_remains_pending_and_flush_retry_confirms_success() {
    let handle = spawn();
    let attempts = Arc::new(AtomicUsize::new(0));
    let delete_attempts = Arc::clone(&attempts);
    let fail = Arc::new(AtomicBool::new(true));
    let delete_fail = Arc::clone(&fail);
    handle.delete_romanized_titles_with(move || {
        delete_attempts.fetch_add(1, Ordering::SeqCst);
        if delete_fail.load(Ordering::SeqCst) {
            Err(std::io::Error::other(
                "fault injection: read-only filesystem",
            ))
        } else {
            Ok(())
        }
    });

    assert!(!handle.flush(Duration::from_secs(1)).await);
    assert!(lock(&handle.pending().inner).contains_key(&StoreKind::RomanizedTitles));
    fail.store(false, Ordering::SeqCst);
    assert!(handle.flush(Duration::from_secs(1)).await);
    assert!(attempts.load(Ordering::SeqCst) >= 2);
    assert!(!lock(&handle.pending().inner).contains_key(&StoreKind::RomanizedTitles));
}

#[tokio::test]
async fn saturated_control_queue_cannot_lose_delete_before_immediate_flush() {
    let (tx, rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    let capacity = crate::util::backpressure::PERSIST_CONTROL_QUEUE
        .capacity()
        .expect("bounded control queue");
    for _ in 0..capacity {
        let (ack, ack_rx) = oneshot::channel();
        drop(ack_rx);
        tx.try_send(PersistMsg::Flush(ack))
            .expect("prefill persist control queue");
    }
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let inflight: SharedInflight = Arc::new(Mutex::new(HashMap::new()));
    let dirty = Arc::new(Notify::new());
    let events = Arc::new(Mutex::new(None));
    let handle = PersistHandle {
        tx,
        pending: Arc::clone(&pending),
        inflight: Arc::clone(&inflight),
        dirty: Arc::clone(&dirty),
        events: Arc::clone(&events),
        order_source: test_order_source(),
        panic_shadow: Arc::new(PanicShadow::new()),
    };
    let deletes = Arc::new(AtomicUsize::new(0));
    let delete_count = Arc::clone(&deletes);
    handle.delete_romanized_titles_with(move || {
        delete_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let actor = tokio::spawn(run_actor(
        rx,
        pending,
        inflight,
        dirty,
        events,
        Arc::clone(&handle.panic_shadow),
    ));

    assert!(handle.flush(Duration::from_secs(2)).await);
    assert_eq!(
        deletes.load(Ordering::SeqCst),
        1,
        "flush must not acknowledge before the shared delete is applied"
    );
    drop(handle);
    actor.await.expect("persist actor stops after sender drop");
}

#[tokio::test]
async fn flush_acknowledges_when_there_is_no_pending_work() {
    let handle = spawn();

    assert!(handle.flush(Duration::from_secs(1)).await);
    assert!(lock(&handle.pending().inner).is_empty());
}
