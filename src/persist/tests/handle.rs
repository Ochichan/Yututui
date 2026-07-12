use super::*;

#[test]
fn save_replaces_pending_without_queueing_payloads() {
    let (tx, mut rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let handle = PersistHandle {
        tx,
        pending: Arc::clone(&pending),
        inflight: Arc::new(Mutex::new(HashMap::new())),
        dirty: Arc::new(Notify::new()),
        events: Arc::new(Mutex::new(None)),
        order_source: test_order_source(),
        admission_open: Arc::new(AtomicBool::new(true)),
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
        .and_then(PendingOperation::snapshot)
    else {
        panic!("expected playlists snapshot");
    };
    let focus = playlists.find("Focus").expect("focus playlist");
    assert_eq!(focus.songs.len(), 1);
}
