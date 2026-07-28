use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn queued_equal_local_and_remote_additions_stay_distinct_and_queue_union() {
    let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body(
        "Remote",
        &["a", "b"],
    ))])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let pending = projection(
        &base,
        "Remote",
        &[("entry-a", "a"), ("local-b", "b")],
        PendingPlaylistProjectionStage::Queued,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    let link = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(
        link.shadow
            .occurrences
            .iter()
            .map(|occurrence| occurrence.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"],
        "the durable shadow is the exact two-occurrence server readback"
    );
    let remote_b = link.shadow.occurrences[1].entry_id.clone();
    assert_ne!(remote_b, entry_id("local-b"));

    let follow_up = store_set
        .bridge_state
        .pending_playlist_projections()
        .get(&playlist_id("local"))
        .expect("the merged three-occurrence union still needs server projection");
    assert_eq!(
        follow_up
            .ordered_entry_ids
            .iter()
            .map(PlaylistEntryId::as_str)
            .collect::<Vec<_>>(),
        vec!["entry-a", "local-b", remote_b.as_str()]
    );
    assert_eq!(
        follow_up
            .ordered_item_ids
            .iter()
            .map(ItemId::as_str)
            .collect::<Vec<_>>(),
        vec!["a", "b", "b"]
    );
    let import = store_set
        .bridge_state
        .pending_playlist_import(&playlist_id("local"))
        .expect("the independent remote b is imported before projection");
    assert_eq!(
        import
            .operations
            .iter()
            .filter_map(|input| match &input.operation {
                Operation::UpsertPlaylistEntry {
                    entry_id, track, ..
                } => Some((entry_id, &track.key)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![(&remote_b, &portable_track(&store_set, "b").key)]
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(
        durable
            .bridge_state
            .pending_playlist_import(&playlist_id("local"))
            .is_some()
    );
    assert!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .contains_key(&playlist_id("local"))
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn local_delete_beats_remote_move_then_restart_ack_projects_merged_order() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    let server_requests = Arc::clone(&requests);
    let first = playlist_body("Remote", &["c", "b", "a"]);
    let readback = playlist_body("Remote", &["c", "a"]);
    let server = tokio::spawn(async move {
        for (step, body) in [
            (0, first.as_str()),
            (1, first.as_str()),
            (2, r#"{"subsonic-response":{"status":"ok"}}"#),
            (3, readback.as_str()),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            server_requests.fetch_add(1, Ordering::SeqCst);
            let request = read_request(&mut stream).await;
            let request_line = request.lines().next().unwrap_or_default();
            match step {
                0 | 1 | 3 => assert!(request_line.contains("/rest/getPlaylist.view?")),
                2 => {
                    assert!(request_line.contains("/rest/updatePlaylist.view?"));
                    assert!(request_line.contains("songIndexToRemove=1"));
                }
                _ => unreachable!(),
            }
            write_json(&mut stream, body).await;
        }
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let link = playlist_link(
        "local",
        "Remote",
        &[("entry-a", "a"), ("entry-b", "b"), ("entry-c", "c")],
    );
    let base = strict_snapshot(&store_set, "Remote", &["a", "b", "c"]);
    let pending = projection(
        &base,
        "Remote",
        &[("entry-a", "a"), ("entry-c", "c")],
        PendingPlaylistProjectionStage::Queued,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    let mut restarted = load_store_set(&paths).unwrap().unwrap();
    let link = restarted
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(
        link.shadow
            .occurrences
            .iter()
            .map(|occurrence| (occurrence.entry_id.as_str(), occurrence.item_id.as_str()))
            .collect::<Vec<_>>(),
        vec![("entry-c", "c"), ("entry-b", "b"), ("entry-a", "a")],
        "the pre-ack shadow remains the exact current server state"
    );
    let follow_up = restarted
        .bridge_state
        .pending_playlist_projections()
        .get(&playlist_id("local"))
        .unwrap();
    assert_eq!(
        follow_up
            .ordered_entry_ids
            .iter()
            .map(PlaylistEntryId::as_str)
            .collect::<Vec<_>>(),
        vec!["entry-c", "entry-a"],
        "the local deletion wins while the remote move order wins for survivors"
    );

    runtime
        .flush_one_playlist_projection(&mut restarted, &client)
        .await
        .unwrap();
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the pending import barrier blocks the follow-up network write"
    );
    let import_id = restarted
        .bridge_state
        .pending_playlist_import(&playlist_id("local"))
        .map(|pending| pending.operation_id.clone())
        .unwrap();
    runtime
        .acknowledge_import(&mut restarted, &import_id)
        .unwrap();
    runtime
        .flush_one_playlist_projection(&mut restarted, &client)
        .await
        .unwrap();
    server.await.unwrap();

    assert!(restarted.bridge_state.pending_playlist_imports().is_empty());
    assert!(
        restarted
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    let settled = restarted
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(
        settled
            .shadow
            .occurrences
            .iter()
            .map(|occurrence| (occurrence.entry_id.as_str(), occurrence.item_id.as_str()))
            .collect::<Vec<_>>(),
        vec![("entry-c", "c"), ("entry-a", "a")]
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn duplicate_reorder_reuses_every_stable_occurrence_without_manual_attention() {
    let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body(
        "Remote",
        &["a", "b", "a"],
    ))])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let link = playlist_link(
        "local",
        "Remote",
        &[("first-a", "a"), ("second-a", "a"), ("entry-b", "b")],
    );
    let base = strict_snapshot(&store_set, "Remote", &["a", "a", "b"]);
    let pending = projection(
        &base,
        "Remote",
        &[("entry-b", "b"), ("second-a", "a"), ("first-a", "a")],
        PendingPlaylistProjectionStage::Queued,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    let link = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(
        link.shadow
            .occurrences
            .iter()
            .map(|occurrence| (occurrence.entry_id.as_str(), occurrence.item_id.as_str()))
            .collect::<Vec<_>>(),
        vec![("first-a", "a"), ("entry-b", "b"), ("second-a", "a")]
    );
    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    let import = store_set
        .bridge_state
        .pending_playlist_import(&playlist_id("local"))
        .unwrap();
    assert!(import.operations.iter().all(|input| !matches!(
        input.operation,
        Operation::UpsertPlaylistEntry { .. } | Operation::RemovePlaylistEntry { .. }
    )));
    assert!(
        import
            .operations
            .iter()
            .any(|input| matches!(input.operation, Operation::MovePlaylistEntry { .. }))
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rename_merge_keeps_exact_remote_shadow_and_resolves_concurrent_winner() {
    for (remote_name, expected_merged_name, expects_follow_up) in [
        ("Base", "Local rename", true),
        ("Remote rename", "Remote rename", false),
    ] {
        let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body(
            remote_name,
            &["a", "remote"],
        ))])
        .await;
        let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
        let link = playlist_link("local", "Base", &[("entry-a", "a")]);
        let base = strict_snapshot(&store_set, "Base", &["a"]);
        let pending = projection(
            &base,
            "Local rename",
            &[("entry-a", "a")],
            PendingPlaylistProjectionStage::Queued,
        );
        install_link_and_projection(&paths, &mut store_set, link, pending);

        runtime
            .flush_one_playlist_projection(&mut store_set, &client)
            .await
            .unwrap();
        server.await.unwrap();

        let link = store_set
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .unwrap();
        assert_eq!(
            link.shadow.name, remote_name,
            "the durable shadow name is always the exact server readback"
        );
        let follow_up = store_set
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local"));
        assert_eq!(follow_up.is_some(), expects_follow_up);
        if let Some(follow_up) = follow_up {
            assert_eq!(follow_up.desired_name, expected_merged_name);
            assert_eq!(
                follow_up.base_remote_fingerprint,
                playlist_snapshot_fingerprint(&strict_snapshot(
                    &store_set,
                    remote_name,
                    &["a", "remote"]
                ))
            );
        }
        let imported_name = store_set
            .bridge_state
            .pending_playlist_import(&playlist_id("local"))
            .unwrap()
            .operations
            .iter()
            .find_map(|input| match &input.operation {
                Operation::UpsertPlaylist { name, .. } => Some(name.as_str()),
                _ => None,
            });
        if remote_name == "Remote rename" {
            assert_eq!(
                imported_name,
                Some("Remote rename"),
                "a concurrent remote rename deterministically wins"
            );
        } else {
            assert_eq!(
                imported_name, None,
                "an unchanged remote name leaves the pending local rename in the merged result"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }
}
