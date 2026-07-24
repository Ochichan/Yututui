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
