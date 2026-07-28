use super::*;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use crate::open_subsonic::actor::{ServiceError, playlist_snapshot_fingerprint};
use crate::open_subsonic::bridge_store::{
    PendingPlaylistImportBatch, PendingPlaylistImportPurpose, PendingPlaylistProjection,
    PendingPlaylistProjectionStage, PlaylistLink, PlaylistLinkState, PlaylistShadow,
    PlaylistShadowOccurrence,
};
use crate::open_subsonic::model::ServerPlaylistWriteSnapshot;
use crate::personal_state::{
    ExternalOperationInput, Operation, PersonalPlaylistEntry, PersonalPlaylistSnapshot,
    PlaylistEntryId, PlaylistId,
};

mod backlog;
mod concurrent_merge;
mod recovery;

enum PlaylistReply {
    Json(String),
    NotFound,
}

async fn playlist_get_server(replies: Vec<PlaylistReply>) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for reply in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let request_line = request.lines().next().unwrap_or_default();
            assert!(
                request_line.contains("/rest/getPlaylist.view?"),
                "{request_line}"
            );
            match reply {
                PlaylistReply::Json(body) => write_json(&mut stream, &body).await,
                PlaylistReply::NotFound => {
                    stream
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .unwrap();
                }
            }
        }
    });
    (port, server)
}

fn item_id(value: &str) -> ItemId {
    ItemId::new(value).unwrap()
}

fn playlist_id(value: &str) -> PlaylistId {
    PlaylistId::new(value).unwrap()
}

fn entry_id(value: &str) -> PlaylistEntryId {
    PlaylistEntryId::new(value).unwrap()
}

fn playlist_song(store_set: &OpenSubsonicStoreSet, item: &str) -> ServerSong {
    ServerSong {
        item: OpenSubsonicItemRef::new(
            store_set.profile.backend_id().clone(),
            store_set.profile.account_scope_id().clone(),
            item_id(item),
        ),
        title: format!("Server {item}"),
        artist: "Server artist".to_owned(),
        artists: Vec::new(),
        album: Some("Server album".to_owned()),
        album_id: None,
        album_artist: None,
        duration_secs: Some(180),
        track_number: None,
        disc_number: None,
        year: None,
        cover_art_id: None,
        content_type: None,
        suffix: None,
        starred: false,
        user_rating: None,
        play_count: None,
        played_at: None,
    }
}

fn strict_snapshot(
    store_set: &OpenSubsonicStoreSet,
    name: &str,
    items: &[&str],
) -> ServerPlaylistWriteSnapshot {
    ServerPlaylistWriteSnapshot::new(
        store_set.profile.backend_id().clone(),
        store_set.profile.account_scope_id().clone(),
        ServerPlaylistId::new("server-playlist").unwrap(),
        name.to_owned(),
        Some("owner".to_owned()),
        Some(false),
        items
            .iter()
            .map(|item| playlist_song(store_set, item))
            .collect(),
    )
}

fn playlist_body(name: &str, items: &[&str]) -> String {
    playlist_body_with_access(name, items, Some("owner"), Some(false))
}

fn playlist_body_with_access(
    name: &str,
    items: &[&str],
    owner: Option<&str>,
    read_only: Option<bool>,
) -> String {
    let entries = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            serde_json::json!({
                "id": item,
                "title": format!("Server {item} occurrence {index}"),
                "artist": "Server artist",
                "type": "music"
            })
        })
        .collect::<Vec<_>>();
    let mut playlist = serde_json::json!({
        "id": "server-playlist",
        "name": name,
        "songCount": items.len(),
        "entry": entries
    });
    if let Some(owner) = owner {
        playlist["owner"] = owner.into();
    }
    if let Some(read_only) = read_only {
        playlist["readonly"] = read_only.into();
    }
    serde_json::json!({
        "subsonic-response": {
            "status": "ok",
            "playlist": playlist
        }
    })
    .to_string()
}

fn portable_track(store_set: &OpenSubsonicStoreSet, item: &str) -> PortableTrack {
    PortableTrack {
        key: PortableTrackKey::OpenSubsonic {
            backend_id: store_set.profile.backend_id().as_str().to_owned(),
            account_scope_id: store_set.profile.account_scope_id().as_str().to_owned(),
            item_id: item.to_owned(),
        },
        title: format!("Local {item}"),
        artist: "Local artist".to_owned(),
        album: None,
        duration_secs: Some(180),
        isrc: None,
    }
}

fn local_snapshot(
    store_set: &OpenSubsonicStoreSet,
    local: &str,
    name: &str,
    occurrences: &[(&str, &str)],
) -> PersonalPlaylistSnapshot {
    PersonalPlaylistSnapshot {
        playlist_id: playlist_id(local),
        name: name.to_owned(),
        entries: occurrences
            .iter()
            .map(|(entry, item)| PersonalPlaylistEntry {
                entry_id: entry_id(entry),
                track: portable_track(store_set, item),
            })
            .collect(),
    }
}

fn playlist_link(local: &str, name: &str, occurrences: &[(&str, &str)]) -> PlaylistLink {
    PlaylistLink {
        local_playlist_id: playlist_id(local),
        server_playlist_id: ServerPlaylistId::new("server-playlist").unwrap(),
        managed_by_yututui: true,
        state: PlaylistLinkState::Linked,
        content_needs_attention: false,
        shadow: PlaylistShadow {
            name: name.to_owned(),
            occurrences: occurrences
                .iter()
                .map(|(entry, item)| PlaylistShadowOccurrence {
                    entry_id: entry_id(entry),
                    item_id: item_id(item),
                })
                .collect(),
            verified_at_unix: 100,
        },
    }
}

fn projection(
    base: &ServerPlaylistWriteSnapshot,
    desired_name: &str,
    desired: &[(&str, &str)],
    stage: PendingPlaylistProjectionStage,
) -> PendingPlaylistProjection {
    PendingPlaylistProjection {
        desired_name: desired_name.to_owned(),
        ordered_entry_ids: desired.iter().map(|(entry, _)| entry_id(entry)).collect(),
        ordered_item_ids: desired.iter().map(|(_, item)| item_id(item)).collect(),
        stage,
        base_remote_fingerprint: playlist_snapshot_fingerprint(base),
    }
}

fn import_batch(operation_id: &str, local: &str) -> PendingPlaylistImportBatch {
    PendingPlaylistImportBatch {
        operation_id: operation_id.to_owned(),
        local_playlist_id: playlist_id(local),
        purpose: PendingPlaylistImportPurpose::InitialOrImportCopy,
        operations: vec![ExternalOperationInput {
            acknowledgement_id: format!("{operation_id}-ack"),
            operation: Operation::UpsertPlaylist {
                playlist_id: playlist_id(local),
                name: "Imported".to_owned(),
            },
            recorded_at_unix: 100,
        }],
    }
}

fn persist_store(paths: &OpenSubsonicPaths, store_set: &mut OpenSubsonicStoreSet) {
    let expected = store_set.revisions();
    commit_store_set(paths, expected, store_set).unwrap();
}

fn install_link_and_projection(
    paths: &OpenSubsonicPaths,
    store_set: &mut OpenSubsonicStoreSet,
    link: PlaylistLink,
    pending: PendingPlaylistProjection,
) {
    let local_playlist_id = link.local_playlist_id.clone();
    store_set.bridge_state.upsert_playlist_link(link).unwrap();
    store_set
        .bridge_state
        .queue_playlist_projection(local_playlist_id, pending)
        .unwrap();
    persist_store(paths, store_set);
}

#[tokio::test]
async fn preview_commit_is_atomic_durable_and_replay_safe() {
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
    let import = import_batch("preview-import", "local");

    runtime
        .commit_playlist_preview(
            &mut store_set,
            import.clone(),
            Some(link.clone()),
            Some(pending.clone()),
        )
        .unwrap();
    runtime
        .commit_playlist_preview(
            &mut store_set,
            import.clone(),
            Some(link.clone()),
            Some(pending.clone()),
        )
        .unwrap();

    assert_eq!(store_set.bridge_state.pending_playlist_imports().len(), 1);
    assert_eq!(store_set.bridge_state.playlist_links().len(), 1);
    assert_eq!(
        store_set.bridge_state.pending_playlist_projections().len(),
        1
    );
    assert_eq!(events.lock().unwrap().len(), 2);
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .pending_playlist_imports()
            .get("preview-import"),
        Some(&import)
    );
    assert_eq!(
        durable.bridge_state.playlist_link(&playlist_id("local")),
        Some(&link)
    );
    assert_eq!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local")),
        Some(&pending)
    );

    let before_failure = store_set.bridge_state.clone();
    assert!(
        runtime
            .commit_playlist_preview(
                &mut store_set,
                import_batch("must-roll-back", "other-local"),
                None,
                Some(pending),
            )
            .is_err()
    );
    assert_eq!(store_set.bridge_state, before_failure);
    assert!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .pending_playlist_imports()
            .get("must-roll-back")
            .is_none()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn local_reconcile_preserves_exact_duplicates_and_safely_unlinks_missing_local() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    store_set
        .bridge_state
        .upsert_playlist_link(playlist_link(
            "duplicate-local",
            "Old",
            &[("old-entry", "same")],
        ))
        .unwrap();
    store_set
        .bridge_state
        .upsert_playlist_link(PlaylistLink {
            local_playlist_id: playlist_id("missing-local"),
            server_playlist_id: ServerPlaylistId::new("missing-server").unwrap(),
            managed_by_yututui: true,
            state: PlaylistLinkState::Linked,
            content_needs_attention: false,
            shadow: PlaylistShadow {
                name: "Keep remote".to_owned(),
                occurrences: Vec::new(),
                verified_at_unix: 100,
            },
        })
        .unwrap();
    persist_store(&paths, &mut store_set);
    let duplicate = local_snapshot(
        &store_set,
        "duplicate-local",
        "Duplicates",
        &[("first", "same"), ("second", "same")],
    );

    runtime
        .reconcile_linked_playlists(&mut store_set, &[duplicate])
        .unwrap();

    assert!(
        store_set
            .bridge_state
            .playlist_link(&playlist_id("missing-local"))
            .is_none()
    );
    let pending = store_set
        .bridge_state
        .pending_playlist_projections()
        .get(&playlist_id("duplicate-local"))
        .unwrap();
    assert_eq!(pending.desired_name, "Duplicates");
    assert_eq!(
        pending
            .ordered_item_ids
            .iter()
            .map(ItemId::as_str)
            .collect::<Vec<_>>(),
        vec!["same", "same"]
    );
    assert_eq!(
        pending
            .ordered_entry_ids
            .iter()
            .map(PlaylistEntryId::as_str)
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    assert_ne!(pending.ordered_entry_ids[0], pending.ordered_entry_ids[1]);
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("missing-local"))
            .is_none()
    );
    assert!(durable.bridge_state.pending_playlist_imports().is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn ambiguous_projection_is_not_replaced_by_a_new_local_snapshot() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let ambiguous = projection(
        &base,
        "In flight",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Ambiguous,
    );
    install_link_and_projection(&paths, &mut store_set, link, ambiguous.clone());
    let latest = local_snapshot(&store_set, "local", "Newer local", &[("entry-c", "c")]);

    runtime
        .reconcile_linked_playlists(&mut store_set, &[latest])
        .unwrap();

    assert_eq!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local")),
        Some(&ambiguous)
    );
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local")),
        Some(&ambiguous)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn queued_projection_is_removed_when_local_returns_to_verified_shadow() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let queued = projection(
        &base,
        "Stale local",
        &[("entry-b", "b")],
        PendingPlaylistProjectionStage::Queued,
    );
    install_link_and_projection(&paths, &mut store_set, link, queued);
    let reverted = local_snapshot(&store_set, "local", "Remote", &[("entry-a", "a")]);

    runtime
        .reconcile_linked_playlists(&mut store_set, &[reverted])
        .unwrap();

    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local"))
            .is_none()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn attention_projection_is_resolved_when_local_returns_to_verified_shadow() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let attention = projection(
        &base,
        "Stale local",
        &[("entry-b", "b")],
        PendingPlaylistProjectionStage::NeedsAttention,
    );
    install_link_and_projection(&paths, &mut store_set, link, attention);
    let reverted = local_snapshot(&store_set, "local", "Remote", &[("entry-a", "a")]);

    runtime
        .reconcile_linked_playlists(&mut store_set, &[reverted])
        .unwrap();

    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .get(&playlist_id("local"))
            .is_none()
    );
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .playlist_projections_needing_attention(),
        0
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn remote_add_remove_reorder_becomes_one_stable_batch_and_verified_shadow() {
    let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body(
        "Remote",
        &["b", "a", "d", "d"],
    ))])
    .await;
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
        &[("entry-a", "a"), ("entry-b", "b"), ("entry-c", "c")],
        PendingPlaylistProjectionStage::Queued,
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
        vec!["b", "a", "d", "d"]
    );
    assert_eq!(
        linked.shadow.occurrences[0].entry_id,
        entry_id("entry-b"),
        "a moved exact occurrence keeps its stable local ID"
    );
    assert_eq!(linked.shadow.occurrences[1].entry_id, entry_id("entry-a"));
    assert_ne!(
        linked.shadow.occurrences[2].entry_id, linked.shadow.occurrences[3].entry_id,
        "duplicate remote additions are distinct occurrences"
    );
    let batch = store_set
        .bridge_state
        .pending_playlist_imports()
        .values()
        .next()
        .unwrap();
    let removed = batch
        .operations
        .iter()
        .filter_map(|input| match &input.operation {
            Operation::RemovePlaylistEntry { entry_id, .. } => Some(entry_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(removed, vec!["entry-c"]);
    let inserted = batch
        .operations
        .iter()
        .filter_map(|input| match &input.operation {
            Operation::UpsertPlaylistEntry {
                entry_id, track, ..
            } => {
                let PortableTrackKey::OpenSubsonic { item_id, .. } = &track.key else {
                    return None;
                };
                Some((entry_id, item_id.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        inserted.iter().map(|(_, item)| *item).collect::<Vec<_>>(),
        vec!["d", "d"]
    );
    assert_eq!(inserted[0].0, &linked.shadow.occurrences[2].entry_id);
    assert_eq!(inserted[1].0, &linked.shadow.occurrences[3].entry_id);
    let moved = batch
        .operations
        .iter()
        .filter_map(|input| match &input.operation {
            Operation::MovePlaylistEntry {
                entry_id,
                after_entry_id,
                ..
            } => Some((entry_id, after_entry_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        moved
            .iter()
            .map(|(entry_id, after)| (
                entry_id.as_str(),
                after.as_ref().map(PlaylistEntryId::as_str)
            ))
            .collect::<Vec<_>>(),
        vec![("entry-b", None), ("entry-a", Some("entry-b"))]
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .unwrap()
            .shadow,
        linked.shadow
    );
    assert_eq!(
        durable
            .bridge_state
            .pending_playlist_imports()
            .values()
            .next(),
        Some(batch)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn idle_link_poll_imports_mobile_reorder_without_a_pending_local_write() {
    let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body(
        "Mobile edit",
        &["b", "a"],
    ))])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    store_set
        .bridge_state
        .upsert_playlist_link(playlist_link(
            "local",
            "Remote",
            &[("entry-a", "a"), ("entry-b", "b")],
        ))
        .unwrap();
    persist_store(&paths, &mut store_set);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    let linked = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(linked.shadow.name, "Mobile edit");
    let generated_entry_id = &linked.shadow.occurrences[0].entry_id;
    assert!(generated_entry_id.as_str().starts_with("server-entry-"));
    assert_ne!(generated_entry_id, &entry_id("entry-b"));
    assert_eq!(linked.shadow.occurrences[1].entry_id, entry_id("entry-a"));
    assert_eq!(store_set.bridge_state.pending_playlist_imports().len(), 1);
    let batch = store_set
        .bridge_state
        .pending_playlist_imports()
        .values()
        .next()
        .unwrap();
    assert!(batch.operations.iter().any(|input| matches!(
        &input.operation,
        Operation::RemovePlaylistEntry {
            entry_id: removed_entry_id,
            removed: true,
            ..
        } if removed_entry_id == &entry_id("entry-b")
    )));
    assert!(batch.operations.iter().any(|input| matches!(
        &input.operation,
        Operation::UpsertPlaylistEntry {
            entry_id: inserted_entry_id,
            after_entry_id: None,
            ..
        } if inserted_entry_id == generated_entry_id
    )));
    assert!(batch.operations.iter().any(|input| matches!(
        &input.operation,
        Operation::MovePlaylistEntry {
            entry_id: moved_entry_id,
            after_entry_id: Some(after_entry_id),
            ..
        } if moved_entry_id == &entry_id("entry-a")
            && after_entry_id == generated_entry_id
    )));
    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .unwrap()
            .shadow,
        linked.shadow
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn unchanged_idle_link_poll_does_not_rewrite_durable_state() {
    let (port, server) =
        playlist_get_server(vec![PlaylistReply::Json(playlist_body("Remote", &["a"]))]).await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    store_set
        .bridge_state
        .upsert_playlist_link(playlist_link("local", "Remote", &[("entry-a", "a")]))
        .unwrap();
    persist_store(&paths, &mut store_set);
    let before = store_set.bridge_state.clone();

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    assert_eq!(store_set.bridge_state, before);
    assert_eq!(
        load_store_set(&paths).unwrap().unwrap().bridge_state,
        before
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn idle_access_evidence_change_is_durable_and_stays_dormant() {
    let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body_with_access(
        "Remote",
        &["a"],
        Some("mallory"),
        Some(false),
    ))])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    store_set
        .bridge_state
        .upsert_playlist_link(playlist_link("local", "Remote", &[("entry-a", "a")]))
        .unwrap();
    persist_store(&paths, &mut store_set);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    assert_eq!(
        store_set
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .map(|link| link.state),
        Some(PlaylistLinkState::AccessNeedsAttention)
    );
    assert!(
        store_set.bridge_state.pending_playlist_imports().is_empty(),
        "an owner-only change is not a remote content edit"
    );
    assert_eq!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .map(|link| link.state),
        Some(PlaylistLinkState::AccessNeedsAttention)
    );
    let status = read_status(&paths).unwrap();
    assert_eq!(status.kind, OpenSubsonicStatusKind::NeedsAttention);
    assert_eq!(status.playlist_projections_needing_attention, 1);

    let dormant = store_set.bridge_state.clone();
    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    assert_eq!(
        store_set.bridge_state, dormant,
        "automatic retry must exclude access-attention links"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn inaccessible_remote_content_delta_is_imported_but_keeps_attention() {
    let (port, server) = playlist_get_server(vec![PlaylistReply::Json(playlist_body_with_access(
        "Mobile edit",
        &["b"],
        Some("mallory"),
        Some(false),
    ))])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    store_set
        .bridge_state
        .upsert_playlist_link(playlist_link("local", "Remote", &[("entry-a", "a")]))
        .unwrap();
    persist_store(&paths, &mut store_set);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    let linked = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(linked.state, PlaylistLinkState::AccessNeedsAttention);
    assert_eq!(linked.shadow.name, "Mobile edit");
    assert_eq!(
        linked
            .shadow
            .occurrences
            .iter()
            .map(|occurrence| occurrence.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["b"]
    );
    assert_eq!(store_set.bridge_state.pending_playlist_imports().len(), 1);
    assert_eq!(
        store_set
            .bridge_state
            .playlist_projections_needing_attention(),
        1
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .map(|link| link.state),
        Some(PlaylistLinkState::AccessNeedsAttention)
    );
    assert_eq!(durable.bridge_state.pending_playlist_imports().len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn repeated_remote_snapshot_cycle_gets_a_fresh_durable_observation_id() {
    let (port, server) = playlist_get_server(vec![
        PlaylistReply::Json(playlist_body("B", &["a"])),
        PlaylistReply::Json(playlist_body("A", &["a"])),
    ])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    store_set
        .bridge_state
        .upsert_playlist_link(playlist_link("local", "A", &[("entry-a", "a")]))
        .unwrap();
    persist_store(&paths, &mut store_set);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    let (first_id, first_acknowledgements) = store_set
        .bridge_state
        .pending_playlist_imports()
        .iter()
        .next()
        .map(|(operation_id, batch)| {
            (
                operation_id.clone(),
                batch
                    .operations
                    .iter()
                    .map(|input| input.acknowledgement_id.clone())
                    .collect::<BTreeSet<_>>(),
            )
        })
        .expect("first remote observation");
    runtime
        .acknowledge_import(&mut store_set, &first_id)
        .unwrap();

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    let pending = store_set.bridge_state.pending_playlist_imports();
    assert_eq!(pending.len(), 1);
    let (second_id, second) = pending.iter().next().unwrap();
    assert_ne!(second_id, &first_id);
    assert!(second.operations.iter().any(|input| matches!(
        input,
        ExternalOperationInput {
            operation: Operation::UpsertPlaylist { name, .. },
            ..
        } if name == "A"
    )));
    assert!(
        second
            .operations
            .iter()
            .all(|input| !first_acknowledgements.contains(&input.acknowledgement_id)),
        "a later A snapshot must not reuse acknowledgement IDs from the earlier transition"
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .pending_playlist_imports()
            .keys()
            .next(),
        Some(second_id)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn deleted_remote_occurrence_readded_at_same_position_gets_a_fresh_entry_id() {
    let (port, server) = playlist_get_server(vec![
        PlaylistReply::Json(playlist_body("A", &[])),
        PlaylistReply::Json(playlist_body("A", &["a"])),
    ])
    .await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    store_set
        .bridge_state
        .upsert_playlist_link(playlist_link("local", "A", &[("entry-a", "a")]))
        .unwrap();
    persist_store(&paths, &mut store_set);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    let first_import_id = store_set
        .bridge_state
        .pending_playlist_imports()
        .keys()
        .next()
        .cloned()
        .expect("remote removal import");
    runtime
        .acknowledge_import(&mut store_set, &first_import_id)
        .unwrap();

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    let linked = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    let replacement_entry_id = linked.shadow.occurrences[0].entry_id.clone();
    assert_ne!(
        replacement_entry_id,
        entry_id("entry-a"),
        "a later occurrence must not reuse the tombstoned local entry identity"
    );
    let pending = store_set
        .bridge_state
        .pending_playlist_imports()
        .values()
        .next()
        .expect("remote re-add import");
    assert!(pending.operations.iter().any(|input| matches!(
        &input.operation,
        Operation::UpsertPlaylistEntry { entry_id, .. } if entry_id == &replacement_entry_id
    )));
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .unwrap()
            .shadow
            .occurrences[0]
            .entry_id,
        replacement_entry_id
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn missing_remote_marks_attention_without_deleting_the_local_playlist() {
    let (port, server) = playlist_get_server(vec![PlaylistReply::NotFound]).await;
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let original_shadow = link.shadow.clone();
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let pending = projection(
        &base,
        "Local edit",
        &[("entry-a", "a")],
        PendingPlaylistProjectionStage::Queued,
    );
    install_link_and_projection(&paths, &mut store_set, link, pending);

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();
    server.await.unwrap();

    let missing = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(missing.state, PlaylistLinkState::ServerMissing);
    assert_eq!(missing.shadow, original_shadow);
    assert!(
        store_set
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    assert!(
        store_set.bridge_state.pending_playlist_imports().is_empty(),
        "a missing server copy must not queue a local DeletePlaylist"
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .unwrap()
            .state,
        PlaylistLinkState::ServerMissing
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn server_missing_link_stays_dormant_until_explicit_recovery() {
    let (root, paths, mut store_set, client, runtime) = fixture(9, None).await;
    let mut missing = playlist_link("local", "Missing server copy", &[("entry-a", "a")]);
    missing.state = PlaylistLinkState::ServerMissing;
    store_set
        .bridge_state
        .upsert_playlist_link(missing.clone())
        .unwrap();
    persist_store(&paths, &mut store_set);
    let before = store_set.bridge_state.clone();

    runtime
        .flush_one_playlist_projection(&mut store_set, &client)
        .await
        .unwrap();

    assert_eq!(store_set.bridge_state, before);
    assert_eq!(
        load_store_set(&paths).unwrap().unwrap().bridge_state,
        before
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn reappeared_server_copy_reactivates_the_original_link_and_imports_its_snapshot() {
    let (root, paths, mut store_set, _client, runtime) = fixture(9, None).await;
    let mut missing = playlist_link("local", "Missing server copy", &[("entry-a", "a")]);
    missing.state = PlaylistLinkState::ServerMissing;
    store_set
        .bridge_state
        .upsert_playlist_link(missing.clone())
        .unwrap();
    persist_store(&paths, &mut store_set);
    let reappeared = strict_snapshot(&store_set, "Reappeared server copy", &["b"]);

    runtime
        .restore_reappeared_playlist(&mut store_set, missing, reappeared)
        .unwrap();

    let link = store_set
        .bridge_state
        .playlist_link(&playlist_id("local"))
        .unwrap();
    assert_eq!(link.state, PlaylistLinkState::Linked);
    assert_eq!(link.server_playlist_id.as_str(), "server-playlist");
    assert_eq!(link.shadow.name, "Reappeared server copy");
    assert_eq!(
        link.shadow
            .occurrences
            .iter()
            .map(|occurrence| occurrence.item_id.as_str())
            .collect::<Vec<_>>(),
        vec!["b"]
    );
    assert_eq!(store_set.bridge_state.pending_playlist_imports().len(), 1);
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .map(|link| link.state),
        Some(PlaylistLinkState::Linked)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn automatic_managed_update_fails_closed_without_exact_remote_access_evidence() {
    for (owner, read_only) in [
        (None, Some(false)),
        (Some("owner"), None),
        (Some("mallory"), Some(false)),
    ] {
        let remote = playlist_body_with_access("Remote", &["a"], owner, read_only);
        let (port, server) = playlist_get_server(vec![PlaylistReply::Json(remote)]).await;
        let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
        store_set
            .private_state
            .bind_api_key_username("owner")
            .unwrap();
        persist_store(&paths, &mut store_set);
        let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
        let base = strict_snapshot(&store_set, "Remote", &["a"]);
        let pending = projection(
            &base,
            "Desired",
            &[("entry-a", "a"), ("entry-b", "b")],
            PendingPlaylistProjectionStage::Queued,
        );
        install_link_and_projection(&paths, &mut store_set, link.clone(), pending);

        runtime
            .flush_one_playlist_projection(&mut store_set, &client)
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(
            store_set
                .bridge_state
                .pending_playlist_projections()
                .get(&playlist_id("local"))
                .unwrap()
                .stage,
            PendingPlaylistProjectionStage::Queued
        );
        assert_eq!(
            store_set
                .bridge_state
                .playlist_link(&playlist_id("local"))
                .map(|link| link.state),
            Some(PlaylistLinkState::AccessNeedsAttention)
        );
        assert!(
            store_set.bridge_state.pending_playlist_imports().is_empty(),
            "failed access verification must not turn a local intent into a remote edit"
        );
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
        assert_eq!(
            load_store_set(&paths)
                .unwrap()
                .unwrap()
                .bridge_state
                .playlist_link(&playlist_id("local"))
                .map(|link| link.state),
            Some(PlaylistLinkState::AccessNeedsAttention)
        );
        let status = read_status(&paths).unwrap();
        assert_eq!(status.kind, OpenSubsonicStatusKind::NeedsAttention);
        assert_eq!(status.playlist_projections_needing_attention, 1);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn exact_readback_updates_shadow_and_removes_the_pending_projection() {
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
    assert_eq!(linked.state, PlaylistLinkState::Linked);
    assert_eq!(linked.shadow.name, "Desired");
    assert_eq!(
        linked
            .shadow
            .occurrences
            .iter()
            .map(|occurrence| (occurrence.entry_id.as_str(), occurrence.item_id.as_str()))
            .collect::<Vec<_>>(),
        vec![("entry-a", "a"), ("entry-b", "b")]
    );
    let durable = load_store_set(&paths).unwrap().unwrap();
    assert!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn exact_readback_with_changed_access_settles_but_keeps_link_attention() {
    for stage in [
        PendingPlaylistProjectionStage::Ambiguous,
        PendingPlaylistProjectionStage::Readback,
    ] {
        let (port, server) = playlist_get_server(vec![PlaylistReply::Json(
            playlist_body_with_access("Desired", &["a", "b"], Some("mallory"), Some(false)),
        )])
        .await;
        let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
        let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
        let base = strict_snapshot(&store_set, "Remote", &["a"]);
        let pending = projection(
            &base,
            "Desired",
            &[("entry-a", "a"), ("entry-b", "b")],
            stage,
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
                .is_empty(),
            "an exact desired readback acknowledges the projection at stage {stage:?}"
        );
        let linked = store_set
            .bridge_state
            .playlist_link(&playlist_id("local"))
            .unwrap();
        assert_eq!(linked.state, PlaylistLinkState::AccessNeedsAttention);
        assert_eq!(
            linked
                .shadow
                .occurrences
                .iter()
                .map(|occurrence| (occurrence.entry_id.as_str(), occurrence.item_id.as_str()))
                .collect::<Vec<_>>(),
            vec![("entry-a", "a"), ("entry-b", "b")],
            "settlement must keep the pending local occurrence identities"
        );
        assert_eq!(
            read_status(&paths)
                .unwrap()
                .playlist_projections_needing_attention,
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn post_update_readback_rechecks_access_before_marking_link_up_to_date() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base_body = playlist_body("Remote", &["a"]);
    let changed_access_body =
        playlist_body_with_access("Desired", &["a", "b"], Some("mallory"), Some(false));
    let server = tokio::spawn(async move {
        let (mut initial, _) = listener.accept().await.unwrap();
        let initial_request = read_request(&mut initial).await;
        assert!(
            initial_request
                .lines()
                .next()
                .unwrap_or_default()
                .contains("/rest/getPlaylist.view?")
        );
        write_json(&mut initial, &base_body).await;

        let (mut update, _) = listener.accept().await.unwrap();
        let update_request = read_request(&mut update).await;
        assert!(
            update_request
                .lines()
                .next()
                .unwrap_or_default()
                .contains("/rest/updatePlaylist.view?")
        );
        write_json(&mut update, r#"{"subsonic-response":{"status":"ok"}}"#).await;

        let (mut readback, _) = listener.accept().await.unwrap();
        let readback_request = read_request(&mut readback).await;
        assert!(
            readback_request
                .lines()
                .next()
                .unwrap_or_default()
                .contains("/rest/getPlaylist.view?")
        );
        write_json(&mut readback, &changed_access_body).await;
    });
    let (root, paths, mut store_set, client, runtime) = fixture(port, None).await;
    let link = playlist_link("local", "Remote", &[("entry-a", "a")]);
    let base = strict_snapshot(&store_set, "Remote", &["a"]);
    let pending = projection(
        &base,
        "Desired",
        &[("entry-a", "a"), ("entry-b", "b")],
        PendingPlaylistProjectionStage::Queued,
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
    assert_eq!(linked.state, PlaylistLinkState::AccessNeedsAttention);
    assert_eq!(
        linked
            .shadow
            .occurrences
            .iter()
            .map(|occurrence| occurrence.entry_id.as_str())
            .collect::<Vec<_>>(),
        vec!["entry-a", "entry-b"]
    );
    assert_eq!(
        read_status(&paths)
            .unwrap()
            .playlist_projections_needing_attention,
        1
    );
    let _ = std::fs::remove_dir_all(root);
}
