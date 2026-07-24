use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use super::transaction::{CommitPoint, TargetFile};
use super::*;

fn track(id: &str) -> PortableTrack {
    PortableTrack {
        key: PortableTrackKey::Catalog {
            provider: "youtube".to_owned(),
            exact_catalog_id: id.to_owned(),
        },
        title: format!("title-{id}"),
        artist: "artist".to_owned(),
        album: None,
        duration_secs: Some(180),
        isrc: None,
    }
}

fn rating_state(
    device: &str,
    sequence: u64,
    observed: VersionVector,
    rating: Rating,
    recorded_at_unix: i64,
) -> PersonalStateV2 {
    let device_id = DeviceId::new(device).unwrap();
    let dot = Dot {
        device_id,
        sequence,
    };
    let mut state = PersonalStateV2::empty("dataset".to_owned()).unwrap();
    state.version_vector = observed.clone();
    state.operations.push(OperationEnvelope {
        operation_id: format!("{device}:{sequence}"),
        stamp: CausalStamp {
            dot: dot.clone(),
            observed,
            recorded_at_unix,
        },
        origin: OperationOrigin::Local,
        operation: Operation::SetRating {
            track: track("same"),
            rating,
        },
    });
    state.version_vector.observe(&dot);
    state.normalize().unwrap();
    state
}

#[test]
fn merge_is_idempotent_commutative_and_associative() {
    let a = rating_state("a", 1, VersionVector::default(), Rating::Liked, 10);
    let b = rating_state("b", 1, VersionVector::default(), Rating::Disliked, 20);
    let c = rating_state("c", 1, VersionVector::default(), Rating::Neutral, 30);

    let (aa, _) = merge(&a, &a).unwrap();
    assert_eq!(aa.operations, a.operations);

    let (ab, _) = merge(&a, &b).unwrap();
    let (ba, _) = merge(&b, &a).unwrap();
    assert_eq!(ab.operations, ba.operations);
    assert_eq!(
        project(&ab).unwrap().fingerprint,
        project(&ba).unwrap().fingerprint
    );

    let (ab, _) = merge(&a, &b).unwrap();
    let (left, _) = merge(&ab, &c).unwrap();
    let (bc, _) = merge(&b, &c).unwrap();
    let (right, _) = merge(&a, &bc).unwrap();
    assert_eq!(left.operations, right.operations);
    assert_eq!(
        project(&left).unwrap().fingerprint,
        project(&right).unwrap().fingerprint
    );
}

#[test]
fn causal_order_wins_and_concurrent_ties_ignore_wall_clock() {
    let liked = rating_state("z", 1, VersionVector::default(), Rating::Liked, 9_999);
    let mut observed = VersionVector::default();
    observed.observe(&liked.operations[0].stamp.dot);
    let neutral = rating_state("a", 1, observed, Rating::Neutral, 1);
    let (merged, _) = merge(&liked, &neutral).unwrap();
    assert!(project(&merged).unwrap().legacy.favorites.is_empty());

    let early_high_dot = rating_state("z", 1, VersionVector::default(), Rating::Liked, 1);
    let late_low_dot = rating_state("a", 1, VersionVector::default(), Rating::Disliked, 9_999);
    let (merged, _) = merge(&early_high_dot, &late_low_dot).unwrap();
    let projection = project(&merged).unwrap();
    assert_eq!(projection.legacy.favorites.len(), 1);
    assert!(
        !projection
            .legacy
            .signals
            .tracks
            .values()
            .any(|signal| signal.disliked)
    );
}

#[test]
fn contradictory_legacy_rating_repairs_to_disliked() {
    let song = crate::api::Song::remote(
        "same".to_owned(),
        "title".to_owned(),
        "artist".to_owned(),
        "3:00".to_owned(),
    );
    let mut library = crate::library::Library::default();
    library.toggle_favorite(&song);
    let mut signals = crate::signals::Signals::default();
    signals.toggle_dislike("same", "artist", 10);
    let state = legacy_state(
        &library,
        &crate::playlists::Playlists::default(),
        &signals,
        &crate::station::StationStore::default(),
    )
    .unwrap();
    let projection = project(&state).unwrap().legacy;
    assert!(projection.favorites.is_empty());
    assert!(
        projection
            .signals
            .tracks
            .values()
            .any(|signal| signal.disliked)
    );
}

#[test]
fn validation_rejects_private_claims_and_deserialized_identifier_bypasses() {
    let mut state = PersonalStateV2::empty("dataset".to_owned()).unwrap();
    state.metadata.filesystem_paths_included = true;
    assert!(state.validate().is_err());

    let mut state = PersonalStateV2::empty("dataset".to_owned()).unwrap();
    let invalid_device = DeviceId("x".repeat(513));
    state.version_vector.0.insert(invalid_device.clone(), 1);
    state.operations.push(OperationEnvelope {
        operation_id: "invalid-device".to_owned(),
        stamp: CausalStamp {
            dot: Dot {
                device_id: invalid_device,
                sequence: 1,
            },
            observed: VersionVector::default(),
            recorded_at_unix: 0,
        },
        origin: OperationOrigin::Local,
        operation: Operation::SetRating {
            track: track("same"),
            rating: Rating::Liked,
        },
    });
    assert!(state.validate().is_err());
}

#[test]
fn legacy_artist_affinity_is_preserved_once_without_migration_reweighting() {
    let mut baseline = LegacyProjection::default();
    baseline
        .signals
        .artist_affinity
        .insert("artist".to_owned(), 0.7);
    let state = super::legacy::legacy_state_from_projection(baseline).unwrap();
    let first = project(&state).unwrap();
    assert_eq!(first.legacy.signals.artist_affinity["artist"], 0.7);

    let (library, playlists, signals, station) = first.into_runtime();
    let reconciled = reconcile_runtime(&state, &library, &playlists, &signals, &station).unwrap();
    assert_eq!(reconciled.operations, state.operations);
    assert_eq!(
        project(&reconciled).unwrap().legacy.signals.artist_affinity["artist"],
        0.7
    );
}

#[test]
fn engagement_projection_keeps_only_the_recent_twenty_thousand_events() {
    const DAY: i64 = 24 * 60 * 60;
    const NOW: i64 = 400 * DAY;

    let device = DeviceId::new("events").unwrap();
    let mut state = PersonalStateV2::empty("event-retention".to_owned()).unwrap();
    let mut observed = VersionVector::default();
    for index in 0..20_002_u64 {
        let dot = Dot {
            device_id: device.clone(),
            sequence: index + 1,
        };
        let recorded_at_unix = if index == 0 {
            NOW - 365 * DAY - 1
        } else {
            NOW - 365 * DAY + index as i64
        };
        state.operations.push(OperationEnvelope {
            operation_id: format!("engagement-{index}"),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: observed.clone(),
                recorded_at_unix,
            },
            origin: OperationOrigin::Local,
            operation: Operation::RecordEngagement {
                event_id: format!("event-{index}"),
                track: track("same"),
                engagement: EngagementKind::Play,
                played_duration_ms: Some(1_000),
                total_duration_ms: Some(180_000),
                artist_key: "artist".to_owned(),
            },
        });
        observed.observe(&dot);
        state.version_vector.observe(&dot);
    }
    state.normalize().unwrap();

    let projection = super::reducer::project_at(&state, NOW).unwrap();
    let signal = projection
        .legacy
        .signals
        .tracks
        .get(&track("same").key)
        .unwrap();
    assert_eq!(signal.play_count, 20_000);
    assert_eq!(projection.legacy.signals.play_log.len(), 20_000);
    assert!(
        projection
            .legacy
            .signals
            .play_log
            .iter()
            .all(|event| event.event_id != "event-0")
    );
}

#[test]
fn foreign_import_is_deletion_free_and_repeated_import_is_a_noop() {
    let mut local_library = crate::library::Library::default();
    local_library.toggle_favorite(&crate::api::Song::remote(
        "local".to_owned(),
        "Local".to_owned(),
        "Artist".to_owned(),
        "3:00".to_owned(),
    ));
    let local = legacy_state(
        &local_library,
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();

    let mut imported_library = crate::library::Library::default();
    imported_library.toggle_favorite(&crate::api::Song::remote(
        "remote".to_owned(),
        "Remote".to_owned(),
        "Artist".to_owned(),
        "3:00".to_owned(),
    ));
    let mut imported = legacy_state(
        &imported_library,
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();
    imported.dataset_id = "foreign-dataset".to_owned();

    let first = plan_import(&local, &imported).unwrap();
    assert!(first.summary.changed);
    let ids = project(&first.candidate)
        .unwrap()
        .legacy
        .favorites
        .into_iter()
        .filter_map(|track| match track.key {
            PortableTrackKey::Catalog {
                exact_catalog_id, ..
            } => Some(exact_catalog_id),
            _ => None,
        })
        .collect::<HashSet<_>>();
    assert_eq!(
        ids,
        HashSet::from(["local".to_owned(), "remote".to_owned()])
    );

    let second = plan_import(&first.candidate, &imported).unwrap();
    assert!(!second.summary.changed);
    assert_eq!(second.candidate.operations, first.candidate.operations);
}

#[test]
fn foreign_baseline_does_not_override_an_explicit_local_rating() {
    let song = crate::api::Song::remote(
        "same".to_owned(),
        "Same".to_owned(),
        "Artist".to_owned(),
        "3:00".to_owned(),
    );
    let mut liked = crate::library::Library::default();
    liked.toggle_favorite(&song);
    let baseline = legacy_state(
        &liked,
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();
    let explicit_neutral = reconcile_runtime(
        &baseline,
        &crate::library::Library::default(),
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();

    let mut imported = baseline.clone();
    imported.dataset_id = "foreign".to_owned();
    let plan = plan_import(&explicit_neutral, &imported).unwrap();
    assert!(
        project(&plan.candidate)
            .unwrap()
            .legacy
            .favorites
            .is_empty(),
        "an imported baseline must not resurrect an explicitly cleared rating"
    );
}

#[test]
fn same_dataset_import_persists_causal_changes_even_when_projection_is_unchanged() {
    let root = temp_root("redundant-import");
    let paths = PersonalStatePaths::for_data_root(root.clone());
    let local = PersonalStateCommit::prepare(rating_state(
        "a",
        1,
        VersionVector::default(),
        Rating::Liked,
        1,
    ))
    .unwrap()
    .commit(&paths)
    .unwrap();

    let mut remote = local.clone();
    let dot = Dot {
        device_id: DeviceId::new("b").unwrap(),
        sequence: 1,
    };
    let device = DeviceRecord {
        device_id: dot.device_id.clone(),
        name: "Second device".to_owned(),
        revoked: false,
    };
    remote
        .device_registry
        .insert(device.device_id.clone(), device.clone());
    remote.operations.push(OperationEnvelope {
        operation_id: "b:1".to_owned(),
        stamp: CausalStamp {
            dot: dot.clone(),
            observed: remote.version_vector.clone(),
            recorded_at_unix: 2,
        },
        origin: OperationOrigin::Imported,
        operation: Operation::AddDevice { device },
    });
    remote.version_vector.observe(&dot);
    remote.projection_fingerprint = None;
    remote.normalize().unwrap();

    let plan = plan_import(&local, &remote).unwrap();
    assert!(plan.summary.changed);
    assert_eq!(
        project(&plan.candidate).unwrap().fingerprint,
        project(&local).unwrap().fingerprint
    );
    assert!(plan.candidate.revision > local.revision);
    let installed = PersonalStateCommit::prepare(plan.candidate)
        .unwrap()
        .commit(&paths)
        .unwrap();
    assert!(
        installed
            .operations
            .iter()
            .any(|operation| operation.operation_id == "b:1")
    );
    assert_projection_files_match(&paths, &installed);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn duplicate_playlist_entries_survive_and_cycles_are_repaired_deterministically() {
    let device = DeviceId::new("device").unwrap();
    let playlist_id = PlaylistId::new("playlist").unwrap();
    let first = PlaylistEntryId::new("first").unwrap();
    let second = PlaylistEntryId::new("second").unwrap();
    let operations = vec![
        Operation::UpsertPlaylist {
            playlist_id: playlist_id.clone(),
            name: "Duplicates".to_owned(),
        },
        Operation::UpsertPlaylistEntry {
            playlist_id: playlist_id.clone(),
            entry_id: first.clone(),
            track: track("same"),
            after_entry_id: Some(second.clone()),
        },
        Operation::UpsertPlaylistEntry {
            playlist_id,
            entry_id: second,
            track: track("same"),
            after_entry_id: Some(first),
        },
    ];
    let mut state = PersonalStateV2::empty("playlist-dataset".to_owned()).unwrap();
    let mut observed = VersionVector::default();
    for (index, operation) in operations.into_iter().enumerate() {
        let dot = Dot {
            device_id: device.clone(),
            sequence: index as u64 + 1,
        };
        state.operations.push(OperationEnvelope {
            operation_id: format!("playlist-op-{}", index + 1),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: observed.clone(),
                recorded_at_unix: 10,
            },
            origin: OperationOrigin::Local,
            operation,
        });
        observed.observe(&dot);
        state.version_vector.observe(&dot);
    }
    state.normalize().unwrap();

    let projection = project(&state).unwrap();
    assert_eq!(projection.repaired_playlist_cycles, 1);
    assert_eq!(projection.legacy.playlists.len(), 1);
    assert_eq!(projection.legacy.playlists[0].entries.len(), 2);
    assert_eq!(
        projection.legacy.playlists[0].entries[0].entry_id.as_str(),
        "first",
        "the lowest causal edge is moved to the root"
    );
}

#[test]
fn crash_recovery_returns_only_the_old_or_complete_new_snapshot() {
    for point in [
        CommitPoint::Staged,
        CommitPoint::Manifest,
        CommitPoint::Committed,
        CommitPoint::Installed(TargetFile::Ledger),
        CommitPoint::Installed(TargetFile::Library),
        CommitPoint::Installed(TargetFile::Signals),
        CommitPoint::Installed(TargetFile::Playlists),
        CommitPoint::Installed(TargetFile::Station),
        CommitPoint::Completed,
    ] {
        let root = temp_root("transaction");
        let paths = PersonalStatePaths::for_data_root(root.clone());
        let old = PersonalStateCommit::prepare(
            legacy_state(
                &crate::library::Library::default(),
                &crate::playlists::Playlists::default(),
                &crate::signals::Signals::default(),
                &crate::station::StationStore::default(),
            )
            .unwrap(),
        )
        .unwrap();
        let old_state = old.commit(&paths).unwrap();

        let mut library = crate::library::Library::default();
        library.toggle_favorite(&crate::api::Song::remote(
            "new".to_owned(),
            "New".to_owned(),
            "Artist".to_owned(),
            "3:00".to_owned(),
        ));
        let candidate = reconcile_runtime(
            &old_state,
            &library,
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        let new = PersonalStateCommit::prepare(candidate).unwrap();
        let new_revision = new.state().revision;
        assert!(new.commit_with_failure_at(&paths, point).is_err());

        recover_pending_transactions(&paths).unwrap();
        let recovered = load_ledger(&paths).unwrap().expect("recovered ledger");
        let expected_revision = if matches!(point, CommitPoint::Staged | CommitPoint::Manifest) {
            old_state.revision
        } else {
            new_revision
        };
        assert_eq!(recovered.revision, expected_revision, "at {point:?}");
        assert_projection_files_match(&paths, &recovered);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn an_older_commit_cannot_overwrite_a_newer_installed_revision() {
    let root = temp_root("revision-order");
    let paths = PersonalStatePaths::for_data_root(root.clone());
    let empty = PersonalStateCommit::prepare(
        legacy_state(
            &crate::library::Library::default(),
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap(),
    )
    .unwrap()
    .commit(&paths)
    .unwrap();

    let mut first_library = crate::library::Library::default();
    first_library.toggle_favorite(&crate::api::Song::remote(
        "first".to_owned(),
        "First".to_owned(),
        "Artist".to_owned(),
        "3:00".to_owned(),
    ));
    let first_state = reconcile_runtime(
        &empty,
        &first_library,
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();
    let first = PersonalStateCommit::prepare(first_state).unwrap();

    let mut second_library = first_library;
    second_library.toggle_favorite(&crate::api::Song::remote(
        "second".to_owned(),
        "Second".to_owned(),
        "Artist".to_owned(),
        "3:00".to_owned(),
    ));
    let second_state = reconcile_runtime(
        first.state(),
        &second_library,
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();
    let second = PersonalStateCommit::prepare(second_state).unwrap();
    assert!(second.state().revision > first.state().revision);

    second.commit(&paths).unwrap();
    let result = first.commit(&paths).unwrap();
    assert_eq!(result.revision, second.state().revision);
    let installed = load_ledger(&paths).unwrap().unwrap();
    assert_eq!(installed, *second.state());
    assert_projection_files_match(&paths, &installed);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn divergent_commits_at_the_same_revision_are_rejected() {
    let root = temp_root("revision-collision");
    let paths = PersonalStatePaths::for_data_root(root.clone());
    let empty = PersonalStateCommit::prepare(
        legacy_state(
            &crate::library::Library::default(),
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap(),
    )
    .unwrap()
    .commit(&paths)
    .unwrap();

    let candidate = |video_id: &str| {
        let mut library = crate::library::Library::default();
        library.toggle_favorite(&crate::api::Song::remote(
            video_id.to_owned(),
            video_id.to_owned(),
            "Artist".to_owned(),
            "3:00".to_owned(),
        ));
        let state = reconcile_runtime(
            &empty,
            &library,
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        PersonalStateCommit::prepare(state).unwrap()
    };
    let first = candidate("first");
    let conflicting = candidate("conflicting");
    assert_eq!(first.state().revision, conflicting.state().revision);

    first.commit(&paths).unwrap();
    let error = conflicting.commit(&paths).unwrap_err();
    assert!(error.to_string().contains("revision collision"));
    let installed = load_ledger(&paths).unwrap().unwrap();
    assert_eq!(installed, *first.state());
    assert_projection_files_match(&paths, &installed);
    std::fs::remove_dir_all(root).unwrap();
}

fn assert_projection_files_match(paths: &PersonalStatePaths, state: &PersonalStateV2) {
    let library: crate::library::Library =
        serde_json::from_slice(&std::fs::read(&paths.library).unwrap()).unwrap();
    let playlists: crate::playlists::Playlists =
        serde_json::from_slice(&std::fs::read(&paths.playlists).unwrap()).unwrap();
    let signals: crate::signals::Signals =
        serde_json::from_slice(&std::fs::read(&paths.signals).unwrap()).unwrap();
    let station: crate::station::StationStore =
        serde_json::from_slice(&std::fs::read(&paths.station).unwrap()).unwrap();
    assert_eq!(
        runtime_fingerprint(&library, &playlists, &signals, &station).unwrap(),
        project(state).unwrap().fingerprint
    );
}

fn temp_root(label: &str) -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "yututui-personal-state-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, AtomicOrdering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    root
}
