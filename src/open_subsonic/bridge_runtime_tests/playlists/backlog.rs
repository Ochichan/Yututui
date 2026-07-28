use super::*;

#[tokio::test]
async fn initial_link_import_survives_restart_reconcile_until_owner_acknowledges_it() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let link = playlist_link("new-local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let projection = projection(
        &base,
        "Initial merged name",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Queued,
    );
    let import = import_batch("z-initial-link", "new-local");
    runtime
        .commit_playlist_preview(
            &mut store_set,
            import.clone(),
            Some(link.clone()),
            Some(projection.clone()),
        )
        .unwrap();

    let mut restarted = load_store_set(&paths).unwrap().unwrap();
    runtime
        .reconcile_linked_playlists(&mut restarted, &[])
        .unwrap();

    assert_eq!(
        restarted
            .bridge_state
            .pending_playlist_imports()
            .get("z-initial-link"),
        Some(&import)
    );
    assert_eq!(
        restarted
            .bridge_state
            .playlist_link(&playlist_id("new-local")),
        Some(&link),
        "absence before owner acknowledgement is not a local deletion"
    );
    assert_eq!(
        restarted
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("new-local")),
        Some(&projection),
        "pre-import reconciliation must not replace the deletion-free initial merge"
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("new-local")),
        Some(&link)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn pending_remote_observation_defers_newer_remote_snapshot_until_owner_ack() {
    let (port, server) = playlist_get_server(vec![
        PlaylistReply::Json(playlist_body_with_access(
            "Remote B",
            &["b"],
            Some("owner"),
            Some(false),
        )),
        PlaylistReply::Json(playlist_body_with_access(
            "Remote C",
            &["c"],
            Some("owner"),
            Some(false),
        )),
    ])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    store_set
        .bridge_state
        .upsert_playlist_link(playlist_link("local", "Remote A", &[("entry-a", "a")]))
        .unwrap();
    persist_store(&paths, &mut store_set);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    let first_operation_id = store_set
        .bridge_state
        .pending_playlist_import(&playlist_id("local"))
        .map(|pending| pending.operation_id.clone())
        .unwrap();
    assert_eq!(
        store_set
            .bridge_state
            .pending_playlist_import(&playlist_id("local"))
            .map(|pending| pending.purpose),
        Some(PendingPlaylistImportPurpose::RemoteObservation)
    );

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    assert_eq!(
        store_set
            .bridge_state
            .pending_playlist_import(&playlist_id("local"))
            .map(|pending| pending.operation_id.as_str()),
        Some(first_operation_id.as_str()),
        "the B observation must remain the only batch until its owner acknowledgement"
    );

    runtime
        .acknowledge_import(&mut store_set, &first_operation_id)
        .unwrap();
    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    let link = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(link.shadow.name, "Remote C");
    assert_eq!(
        link.shadow
            .occurrences
            .iter()
            .map(|occurrence| occurrence.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["c"]
    );
    let pending = store_set
        .bridge_state
        .pending_playlist_import(&playlist_id("local"))
        .unwrap();
    assert_ne!(pending.operation_id, first_operation_id);
    assert_eq!(
        pending.purpose,
        PendingPlaylistImportPurpose::RemoteObservation
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn missing_local_beats_unacknowledged_remote_observation_and_keeps_server_copy() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    store_set
        .bridge_state
        .upsert_playlist_link(playlist_link("local", "Remote", &[("entry-a", "a")]))
        .unwrap();
    let mut observation = import_batch("remote-observation", "local");
    observation.purpose = PendingPlaylistImportPurpose::RemoteObservation;
    store_set
        .bridge_state
        .queue_playlist_import(observation)
        .unwrap();
    persist_store(&paths, &mut store_set);

    runtime
        .reconcile_linked_playlists(&mut store_set, &[])
        .unwrap();

    assert!(store_set.bridge_state.pending_playlist_imports().is_empty());
    assert!(
        store_set
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .is_none()
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(durable.bridge_state.pending_playlist_imports().is_empty());
    assert!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .is_none()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn incompatible_local_content_isolated_per_link_and_recovers_after_content_changes() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    for (local, server, name) in [
        ("a-missing", "server-a", "A"),
        ("middle-valid", "server-middle", "Middle"),
        ("z-invalid", "server-z", "Z"),
    ] {
        store_set
            .bridge_state
            .upsert_playlist_link(PlaylistLink {
                local_playlist_id: playlist_id(local),
                server_playlist_id: ServerPlaylistId::new(server).unwrap(),
                managed_by_yututui: true,
                state: PlaylistLinkState::Linked,
                content_needs_attention: false,
                shadow: PlaylistShadow {
                    name: name.to_owned(),
                    occurrences: Vec::new(),
                    verified_at_unix: 100,
                },
            })
            .unwrap();
    }
    persist_store(&paths, &mut store_set);
    let mut invalid = local_snapshot(&store_set, "z-invalid", "Z", &[("entry", "item")]);
    let valid = local_snapshot(
        &store_set,
        "middle-valid",
        "Middle changed",
        &[("valid-entry", "valid-item")],
    );
    let PortableTrackKey::OpenSubsonic { backend_id, .. } = &mut invalid.entries[0].track.key
    else {
        unreachable!();
    };
    *backend_id = "wrong-backend".to_owned();
    runtime
        .reconcile_linked_playlists(&mut store_set, &[valid, invalid])
        .unwrap();

    assert!(
        store_set
            .bridge_state
            .playlist_link(&playlist_id("a-missing"))
            .is_none(),
        "an unrelated missing local still follows the safe unlink policy"
    );
    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .contains_key(&playlist_id("middle-valid")),
        "the compatible link still advances"
    );
    assert_eq!(
        store_set
            .bridge_state
            .playlist_link(&playlist_id("z-invalid"))
            .map(|link| (link.state, link.content_needs_attention)),
        Some((PlaylistLinkState::Linked, true))
    );
    assert_eq!(
        store_set.bridge_state.playlist_contents_needing_attention(),
        1
    );
    let status = read_status(&paths).unwrap();
    assert_eq!(status.kind, OpenSubsonicStatusKind::NeedsAttention);
    assert_eq!(status.playlist_contents_needing_attention, 1);
    assert_eq!(
        store_set
            .bridge_state
            .requeue_playlist_projections_needing_attention(),
        0,
        "connection setup must not clear a content error"
    );

    let repaired = local_snapshot(&store_set, "z-invalid", "Z repaired", &[("entry", "item")]);
    let valid = local_snapshot(
        &store_set,
        "middle-valid",
        "Middle changed",
        &[("valid-entry", "valid-item")],
    );
    runtime
        .reconcile_linked_playlists(&mut store_set, &[valid, repaired])
        .unwrap();

    assert_eq!(
        store_set
            .bridge_state
            .playlist_link(&playlist_id("z-invalid"))
            .map(|link| (link.state, link.content_needs_attention)),
        Some((PlaylistLinkState::Linked, false))
    );
    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .contains_key(&playlist_id("z-invalid")),
        "compatible replacement content automatically requeues projection"
    );
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .playlist_contents_needing_attention(),
        0
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn incompatible_content_does_not_abandon_ambiguous_or_readback_projection() {
    for stage in [
        PendingPlaylistProjectionStage::Ambiguous,
        PendingPlaylistProjectionStage::Readback,
    ] {
        let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
        let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
        let base = strict_snapshot(&store_set, "Remote", &["a"]);
        let pending = projection(
            &base,
            "In flight",
            &[("entry-a", "a"), ("entry-b", "b")],
            stage,
        );
        install_link_and_projection(&paths, &mut store_set, link.clone(), pending.clone());
        let mut incompatible =
            local_snapshot(&store_set, "local", "Bad", &[("entry-wrong", "wrong")]);
        let PortableTrackKey::OpenSubsonic { backend_id, .. } =
            &mut incompatible.entries[0].track.key
        else {
            unreachable!();
        };
        *backend_id = "wrong-backend".to_owned();

        runtime
            .reconcile_linked_playlists(&mut store_set, &[incompatible])
            .unwrap();

        let durable_link = store_set
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .unwrap();
        assert_eq!(durable_link.state, link.state);
        assert!(
            durable_link.content_needs_attention,
            "stage {stage:?} must report the independent content blocker"
        );
        assert_eq!(
            store_set
                .bridge_state
                .pending_playlist_projections()
                .get(&playlist_id("local")),
            Some(&pending)
        );
        let status = read_status(&paths).unwrap();
        assert_eq!(status.kind, OpenSubsonicStatusKind::NeedsAttention);
        assert_eq!(status.playlist_contents_needing_attention, 1);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn access_attention_and_content_attention_are_independent_and_fail_closed() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let mut link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    link.state = PlaylistLinkState::AccessNeedsAttention;
    store_set.bridge_state.upsert_playlist_link(link).unwrap();
    persist_store(&paths, &mut store_set);
    let mut incompatible = local_snapshot(&store_set, "local", "Bad", &[("entry-wrong", "wrong")]);
    let PortableTrackKey::OpenSubsonic {
        account_scope_id, ..
    } = &mut incompatible.entries[0].track.key
    else {
        unreachable!();
    };
    *account_scope_id = "wrong-account".to_owned();

    runtime
        .reconcile_linked_playlists(&mut store_set, &[incompatible])
        .unwrap();

    let link = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(link.state, PlaylistLinkState::AccessNeedsAttention);
    assert!(link.content_needs_attention);
    assert_eq!(
        store_set
            .bridge_state
            .requeue_playlist_projections_needing_attention(),
        1
    );
    let link = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(link.state, PlaylistLinkState::Linked);
    assert!(
        link.content_needs_attention,
        "reconnecting must not clear the separate content blocker"
    );

    let repaired = local_snapshot(&store_set, "local", "Repaired", &[("entry-b", "b")]);
    runtime
        .reconcile_linked_playlists(&mut store_set, &[repaired])
        .unwrap();
    let link = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(link.state, PlaylistLinkState::Linked);
    assert!(!link.content_needs_attention);
    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .contains_key(&playlist_id("local"))
    );
    let _ = std::fs::remove_dir_all(root);
}
