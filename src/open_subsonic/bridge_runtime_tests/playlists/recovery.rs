use super::*;

#[tokio::test]
async fn ambiguous_crash_state_settles_exact_applied_readback_with_pending_entry_ids() {
    let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body(
        "Desired",
        &["a", "b"],
    ))])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let pending = projection(
        &base,
        "Desired",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Ambiguous,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending);

    // Model a stop after updatePlaylist may have reached the server. The durable ambiguous stage
    // makes an exact desired readback reuse the pending local identities without resubmission.
    let mut restarted = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        restarted
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local"))
            .unwrap()
            .stage,
        PendingPlaylistProjectionStage::Ambiguous
    );

    runtime
        .flush_one_playlist_projection(&mut restarted, &client)
        .await
        .unwrap();
    server.await.unwrap();

    assert!(
        restarted
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    assert!(
        restarted.bridge_state.pending_playlist_imports().is_empty(),
        "an already-applied local projection must not become a new remote observation"
    );
    let linked = restarted
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(
        linked
            .shadow
            .occurrences
            .iter()
            .map(|occurrence| (occurrence.entry_id.as_str(), occurrence.item_id.as_str()))
            .collect::<Vec<_>>(),
        vec![("entry-a", "a"), ("entry-b", "b")],
        "settlement must retain the durable local occurrence identities"
    );

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .unwrap()
            .shadow
            .occurrences,
        linked.shadow.occurrences
    );
    assert!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    assert!(durable.bridge_state.pending_playlist_imports().is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn third_state_readback_reuses_pending_occurrences_and_imports_only_remote_additions() {
    let events = Arc::new(Mutex::new(Vec::<OpenSubsonicBridgeImport>::new()));
    let sink_events = Arc::clone(&events);
    let sink: OpenSubsonicBridgeSink = Arc::new(move |event| {
        sink_events.lock().unwrap().push(event);
    });
    let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body(
        "Desired",
        &["a", "b", "c"],
    ))])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, Some(sink)).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let pending = projection(
        &base,
        "Desired",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Readback,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    let linked = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(
        linked
            .shadow
            .occurrences
            .iter()
            .map(|occurrence| occurrence.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c"]
    );
    assert_eq!(linked.shadow.occurrences[0].entry_id.as_str(), "entry-a");
    assert_eq!(linked.shadow.occurrences[1].entry_id.as_str(), "entry-b");
    let remote_c_entry_id = linked.shadow.occurrences[2].entry_id.clone();
    assert_ne!(remote_c_entry_id, entry_id("entry-b"));

    let pending_import = store_set
        .bridge_state
        .pending_playlist_imports()
        .values()
        .next()
        .expect("remote-only c import");
    assert_eq!(
        pending_import
            .operations
            .iter()
            .filter_map(|input| match &input.operation {
                Operation::UpsertPlaylistEntry {
                    entry_id,
                    track:
                        crate::personal_state::PortableTrack {
                            key: PortableTrackKey::OpenSubsonic { item_id, .. },
                            ..
                        },
                    ..
                } => Some((entry_id.as_str(), item_id.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![(remote_c_entry_id.as_str(), "c")],
        "the already-delivered pending b occurrence must not be imported under a second EntryId"
    );

    let observed = events.lock().unwrap()[0].clone();
    let initial = OpenSubsonicBridgeImport::Playlist {
        operation_id: "initial-local-desired".to_owned(),
        backend_id: store_set.profile.backend_id().clone(),
        local_playlist_id: playlist_id("local"),
        purpose: PendingPlaylistImportPurpose::InitialOrImportCopy,
        operations: vec![
            ExternalOperationInput {
                acknowledgement_id: "initial-playlist".to_owned(),
                operation: Operation::UpsertPlaylist {
                    playlist_id: playlist_id("local"),
                    name: "Desired".to_owned(),
                },
                recorded_at_unix: 50,
            },
            ExternalOperationInput {
                acknowledgement_id: "initial-entry-a".to_owned(),
                operation: Operation::UpsertPlaylistEntry {
                    playlist_id: playlist_id("local"),
                    entry_id: entry_id("entry-a"),
                    track: portable_track(&store_set, "a"),
                    after_entry_id: None,
                },
                recorded_at_unix: 50,
            },
            ExternalOperationInput {
                acknowledgement_id: "initial-entry-b".to_owned(),
                operation: Operation::UpsertPlaylistEntry {
                    playlist_id: playlist_id("local"),
                    entry_id: entry_id("entry-b"),
                    track: portable_track(&store_set, "b"),
                    after_entry_id: Some(entry_id("entry-a")),
                },
                recorded_at_unix: 50,
            },
        ],
    };
    let mut app = crate::app::App::new(50);
    let initial_state = crate::personal_state::legacy_state(
        &app.library,
        &app.playlists,
        &app.signals,
        &app.station,
    )
    .unwrap();
    app.install_personal_state_runtime(initial_state).unwrap();
    app.apply_open_subsonic_bridge_import(&initial).unwrap();
    let first_envelopes = app.apply_open_subsonic_bridge_import(&observed).unwrap();
    assert_eq!(
        app.apply_open_subsonic_bridge_import(&observed).unwrap(),
        first_envelopes
    );
    let snapshot = crate::personal_state::personal_playlist_snapshot(
        &app.personal_state.ledger,
        &playlist_id("local"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        snapshot
            .entries
            .iter()
            .map(|entry| {
                let PortableTrackKey::OpenSubsonic { item_id, .. } = &entry.track.key else {
                    unreachable!("server playlist contains server tracks")
                };
                (entry.entry_id.as_str(), item_id.as_str())
            })
            .collect::<Vec<_>>(),
        vec![
            ("entry-a", "a"),
            ("entry-b", "b"),
            (remote_c_entry_id.as_str(), "c")
        ]
    );

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    assert_eq!(durable.bridge_state.pending_playlist_imports().len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn ambiguous_duplicate_third_state_converges_without_manual_review() {
    let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body(
        "Desired",
        &["a", "b", "b"],
    ))])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let pending = projection(
        &base,
        "Desired",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Ambiguous,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    let linked = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(
        linked
            .shadow
            .occurrences
            .iter()
            .map(|occurrence| (occurrence.entry_id.as_str(), occurrence.item_id.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("entry-a", "a"),
            ("entry-b", "b"),
            (linked.shadow.occurrences[2].entry_id.as_str(), "b")
        ]
    );
    assert_ne!(linked.shadow.occurrences[2].entry_id, entry_id("entry-b"));
    let import = store_set
        .bridge_state
        .pending_playlist_imports()
        .values()
        .next()
        .expect("the second exact b occurrence is a remote addition");
    assert_eq!(
        import
            .operations
            .iter()
            .filter(|input| matches!(input.operation, Operation::UpsertPlaylistEntry { .. }))
            .count(),
        1
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    assert_eq!(durable.bridge_state.pending_playlist_imports().len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn ambiguous_base_readback_only_requeues_after_proving_no_remote_change() {
    let (port, server) =
        playlist_get_server(vec![PlaylistReply::Json(playlist_body("Remote", &["a"]))]).await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let pending = projection(
        &base,
        "Desired",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Ambiguous,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    let pending = store_set
        .bridge_state
        .pending_playlist_projections()
        .get(&playlist_id("local"))
        .unwrap();
    assert_eq!(pending.stage, PendingPlaylistProjectionStage::Queued);
    assert_eq!(
        store_set
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .unwrap()
            .shadow
            .name,
        "Remote"
    );
    assert!(store_set.bridge_state.pending_playlist_imports().is_empty());
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local"))
            .unwrap()
            .stage,
        PendingPlaylistProjectionStage::Queued
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn mismatched_readback_never_blindly_resends_or_discards_pending_state() {
    let (port, server) =
        playlist_get_server(vec![PlaylistReply::Json(playlist_body("Remote", &["a"]))]).await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let pending = projection(
        &base,
        "Desired",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Readback,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending.clone());

    assert!(matches!(
        runtime
            .flush_one_playlist_projection(&mut store_set, &client)
            .await,
        Err(ServiceError::Server(ServerError::InvalidResponse))
    ));
    server.await.unwrap();

    assert_eq!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local")),
        Some(&pending)
    );
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local")),
        Some(&pending)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn unlink_preserves_both_copies_and_removes_durable_link_state() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let local = local_snapshot(
        &store_set,
        "local",
        "Local copy",
        &[("entry-a", "a"), ("entry-b", "b")],
    );
    let local_before = local.clone();
    let link = playlist_link("local", "Remote copy", &[("entry-a", "a")]);
    let server_before = link.shadow.clone();
    let base = strict_snapshot(&store_set, "Remote copy", &["a"]);
    let pending = projection(
        &base,
        "Local copy",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Queued,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending);

    runtime
        .unlink_playlist(&mut store_set, &playlist_id("local"))
        .unwrap();

    assert_eq!(local, local_before);
    assert_eq!(server_before.name, "Remote copy");
    assert!(
        store_set
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .is_none()
    );
    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    assert!(store_set.bridge_state.pending_playlist_imports().is_empty());

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .is_none()
    );
    assert!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    assert!(durable.bridge_state.pending_playlist_imports().is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn committed_delete_removes_link_and_queues_one_local_delete() {
    let events = Arc::new(Mutex::new(Vec::<OpenSubsonicBridgeImport>::new()));
    let sink_events = Arc::clone(&events);
    let sink: OpenSubsonicBridgeSink = Arc::new(move |event| {
        sink_events.lock().unwrap().push(event);
    });
    let (root, paths, mut store_set, _client, runtime) = fixture(9, Some(sink)).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let pending = projection(
        &base,
        "Local",
        &[("entry-a", "a")],
        PendingPlaylistProjectionStage::Queued,
    );
    let mut stale_observation = import_batch("a-older-remote-observation", "local");
    stale_observation.purpose = PendingPlaylistImportPurpose::RemoteObservation;
    store_set
        .bridge_state
        .queue_playlist_import(stale_observation)
        .unwrap();
    install_link_and_projection(&paths, &mut store_set, link.clone(), pending);

    runtime
        .commit_deleted_playlist(&mut store_set, &link, "delete-both-operation".to_owned())
        .unwrap();

    assert!(
        store_set
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .is_none()
    );
    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    let pending = store_set.bridge_state.pending_playlist_imports();
    assert_eq!(pending.len(), 1);
    let pending = pending.get("delete-both-operation").unwrap();
    assert_eq!(pending.local_playlist_id, playlist_id("local"));
    assert_eq!(pending.purpose, PendingPlaylistImportPurpose::Delete);
    assert_eq!(pending.operations.len(), 1);
    assert_eq!(
        pending.operations[0].acknowledgement_id,
        "delete-both-operation-delete"
    );
    assert!(matches!(
        &pending.operations[0].operation,
        Operation::DeletePlaylist {
            playlist_id: deleted_playlist_id,
            deleted: true,
        } if deleted_playlist_id == &playlist_id("local")
    ));

    let emitted = events.lock().unwrap();
    assert_eq!(emitted.len(), 1);
    assert!(matches!(
        &emitted[0],
        OpenSubsonicBridgeImport::Playlist {
            operation_id,
            operations,
            ..
        } if operation_id == "delete-both-operation"
            && operations.len() == 1
            && matches!(
                &operations[0].operation,
                Operation::DeletePlaylist {
                    playlist_id: deleted_playlist_id,
                    deleted: true,
                } if deleted_playlist_id == &playlist_id("local")
            )
    ));
    drop(emitted);

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .is_none()
    );
    assert!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    let durable_pending = durable.bridge_state.pending_playlist_imports();
    assert_eq!(durable_pending.len(), 1);
    assert_eq!(
        durable_pending
            .get("delete-both-operation")
            .unwrap()
            .operations
            .len(),
        1
    );
    assert!(
        !durable_pending.contains_key("a-older-remote-observation"),
        "the older observation must be retired in the same durable delete transaction"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn restoring_server_missing_link_replaces_server_identity_durably() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let mut missing = playlist_link("local", "Missing server copy", &[("entry-a", "a")]);
    missing.server_playlist_id = ServerPlaylistId::new("missing-server-playlist").unwrap();
    missing.state = PlaylistLinkState::ServerMissing;
    let base = strict_snapshot(&store_set, "Missing server copy", &["a"]);
    let pending = projection(
        &base,
        "Local copy",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Queued,
    );
    install_link_and_projection(&paths, &mut store_set, missing, pending);

    let mut replacement = playlist_link(
        "local",
        "Restored server copy",
        &[("entry-a", "a"), ("entry-b", "b")],
    );
    replacement.server_playlist_id = ServerPlaylistId::new("created-server-playlist").unwrap();
    replacement.shadow.verified_at_unix = 200;
    let current = local_snapshot(
        &store_set,
        "local",
        "Restored server copy",
        &[("entry-a", "a"), ("entry-b", "b")],
    );
    runtime
        .commit_created_playlist_link(&mut store_set, replacement.clone(), &current, true)
        .unwrap();

    assert_eq!(
        store_set.bridge_state.playlist_link(&playlist_id("local")),
        Some(&replacement)
    );
    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    assert!(store_set.bridge_state.pending_playlist_imports().is_empty());

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable.bridge_state.playlist_link(&playlist_id("local")),
        Some(&replacement)
    );
    assert!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    assert!(durable.bridge_state.pending_playlist_imports().is_empty());
    let _ = std::fs::remove_dir_all(root);
}
