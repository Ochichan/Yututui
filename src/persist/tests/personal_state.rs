use super::*;

#[test]
fn panic_operation_commits_the_complete_personal_state_transaction() {
    let directory = temp_dir("personal-state-panic");
    let override_value = directory.to_string_lossy().into_owned();
    crate::test_util::env::with_var("YTM_DATA_DIR", Some(&override_value), || {
        let mut library = crate::library::Library::default();
        library.toggle_favorite(&crate::api::Song::remote(
            "panic-track",
            "Panic Track",
            "Artist",
            "3:00",
        ));
        let state = crate::personal_state::legacy_state(
            &library,
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        let commit = crate::personal_state::PersonalStateCommit::prepare(state).unwrap();
        let operation = pending_save(Snapshot::PersonalState(Box::new(commit)));
        let panic_operation = operation.panic_operation().unwrap();

        write_panic_operation(&panic_operation).unwrap();

        let paths = crate::personal_state::PersonalStatePaths::current().unwrap();
        let installed = crate::personal_state::load_ledger(&paths)
            .unwrap()
            .expect("panic fallback installed the ledger");
        let persisted: crate::library::Library =
            serde_json::from_slice(&std::fs::read(&paths.library).unwrap()).unwrap();
        let playlists: crate::playlists::Playlists =
            serde_json::from_slice(&std::fs::read(&paths.playlists).unwrap()).unwrap();
        let signals: crate::signals::Signals =
            serde_json::from_slice(&std::fs::read(&paths.signals).unwrap()).unwrap();
        let station: crate::station::StationStore =
            serde_json::from_slice(&std::fs::read(&paths.station).unwrap()).unwrap();
        assert_eq!(
            crate::personal_state::runtime_fingerprint(&persisted, &playlists, &signals, &station,)
                .unwrap(),
            crate::personal_state::project(&installed)
                .unwrap()
                .fingerprint
        );
    });
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn actor_emits_a_mutation_only_for_an_ordinary_personal_state_commit() {
    let directory = temp_dir("personal-state-commit-event");
    let override_value = directory.to_string_lossy().into_owned();
    crate::test_util::env::with_var("YTM_DATA_DIR", Some(&override_value), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let state = crate::personal_state::legacy_state(
                    &crate::library::Library::default(),
                    &crate::playlists::Playlists::default(),
                    &crate::signals::Signals::default(),
                    &crate::station::StationStore::default(),
                )
                .unwrap();
                let commit = crate::personal_state::PersonalStateCommit::prepare(state).unwrap();
                let committed_state = commit.state().clone();
                let expected_revision = committed_state.revision;
                let expected_identity = committed_state.identity().unwrap();
                let events = Arc::new(Mutex::new(Vec::new()));
                let captured = Arc::clone(&events);
                let handle = spawn();
                handle.set_event_sink(move |event| {
                    captured.lock().unwrap().push(event);
                });

                let _ = handle
                    .save(Snapshot::PersonalState(Box::new(commit)))
                    .unwrap();
                assert!(handle.flush(Duration::from_secs(5)).await);
                assert!(handle.flush(Duration::from_secs(5)).await);
                {
                    let emitted = events.lock().unwrap();
                    assert!(matches!(
                        emitted.as_slice(),
                        [PersistEvent::PersonalStateCommitted {
                            revision,
                            state_identity,
                        }] if *revision == expected_revision
                            && state_identity == &expected_identity
                    ));
                }

                events.lock().unwrap().clear();
                let personal_paths = crate::personal_state::PersonalStatePaths::current().unwrap();
                let sync_paths = crate::sync::SyncPaths::current().unwrap();
                let targeted = crate::sync::service::PersonalSyncPersistence::reconcile(
                    committed_state.clone(),
                    committed_state.clone(),
                    committed_state,
                    0,
                    personal_paths,
                    sync_paths,
                )
                .unwrap();
                assert!(
                    OwnedSnapshot::from(Snapshot::PersonalSync(targeted))
                        .ordinary_personal_state_commit()
                        .is_none(),
                    "targeted sync persistence must not re-arm automatic sync"
                );
            });
    });
    let _ = std::fs::remove_dir_all(directory);
}
