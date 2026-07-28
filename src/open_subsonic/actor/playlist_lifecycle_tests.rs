use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use age::secrecy::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::open_subsonic::bridge_store::{PlaylistLinkState, PlaylistShadow};
use crate::open_subsonic::{
    ConfiguredPrivateOrigin, OpenSubsonicBridgeState, OpenSubsonicPaths, OpenSubsonicPrivateState,
    OpenSubsonicProfile, ServerCredential, StoreRevisions, commit_store_set, load_store_set,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

enum Reply {
    Json(String),
    NotFound,
    Unavailable,
    DropConnection,
}

struct Step {
    endpoint: &'static str,
    reply: Reply,
}

struct Fixture {
    root: std::path::PathBuf,
    paths: OpenSubsonicPaths,
    store_set: OpenSubsonicStoreSet,
    client: OpenSubsonicClient,
    bridge: BridgeRuntime,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn fixture(port: u16) -> Fixture {
    let backend_id = BackendId::new("playlist-actor-backend").unwrap();
    let account_scope_id = AccountScopeId::new("playlist-actor-account").unwrap();
    let profile = OpenSubsonicProfile::with_ids(
        0,
        backend_id.clone(),
        account_scope_id.clone(),
        "Playlist actor",
        ConfiguredPrivateOrigin::new(&format!("http://127.0.0.1:{port}/"), true).unwrap(),
        None,
    )
    .unwrap();
    let private_state = OpenSubsonicPrivateState::new(
        backend_id.clone(),
        account_scope_id.clone(),
        ServerCredential::api_key(SecretString::from("playlist-api-key".to_owned())).unwrap(),
    );
    let bridge_state = OpenSubsonicBridgeState::new(backend_id, account_scope_id);
    let mut store_set = OpenSubsonicStoreSet::new(profile, private_state, bridge_state).unwrap();
    let root = std::env::temp_dir().join(format!(
        "yututui-playlist-actor-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ));
    let paths = OpenSubsonicPaths::for_data_root(root.clone());
    commit_store_set(&paths, StoreRevisions::MISSING, &mut store_set).unwrap();
    let client = OpenSubsonicClient::connect(&store_set.profile)
        .await
        .unwrap();
    let bridge = BridgeRuntime::writable(paths.clone(), None);
    Fixture {
        root,
        paths,
        store_set,
        client,
        bridge,
    }
}

fn bind_fixture_owner(fixture: &mut Fixture) {
    let expected = fixture.store_set.revisions();
    fixture
        .store_set
        .private_state
        .bind_api_key_username("alice")
        .unwrap();
    commit_store_set(&fixture.paths, expected, &mut fixture.store_set).unwrap();
}

async fn scripted_server(steps: Vec<Step>) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for step in steps {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let target = request
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap_or_default();
            assert!(target.starts_with(step.endpoint), "{target}");
            match step.reply {
                Reply::Json(body) => write_response(&mut stream, "200 OK", &body).await,
                Reply::NotFound => write_response(&mut stream, "404 Not Found", "").await,
                Reply::Unavailable => {
                    write_response(&mut stream, "503 Service Unavailable", "").await
                }
                Reply::DropConnection => {}
            }
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "replay-unsafe playlist mutations must not be retried blindly"
        );
    });
    (port, server)
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while request.len() < 32 * 1024 {
        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
            break;
        }
        request.push(byte[0]);
        if request.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(request).unwrap()
}

async fn write_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await.unwrap();
}

fn playlist_body(id: &str, name: &str, read_only: Option<bool>) -> String {
    playlist_body_with_access(id, name, Some("alice"), read_only)
}

fn playlist_body_with_access(
    id: &str,
    name: &str,
    owner: Option<&str>,
    read_only: Option<bool>,
) -> String {
    let mut playlist = serde_json::json!({
        "id": id,
        "name": name,
        "songCount": 0,
        "entry": []
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

fn local_snapshot() -> PersonalPlaylistSnapshot {
    PersonalPlaylistSnapshot {
        playlist_id: PlaylistId::new("local-playlist").unwrap(),
        name: "Local playlist".to_owned(),
        entries: Vec::new(),
    }
}

fn pending_restore(link: &PlaylistLink) -> PendingPlaylistCreate {
    PendingPlaylistCreate {
        local_playlist_id: link.local_playlist_id.clone(),
        expected_missing_server_id: Some(link.server_playlist_id.clone()),
        created_server_playlist_id: None,
        desired_name: "Local playlist".to_owned(),
        ordered_entry_ids: Vec::new(),
        ordered_item_ids: Vec::new(),
        started_at_unix: 100,
    }
}

fn install_link(fixture: &mut Fixture, managed: bool, state: PlaylistLinkState) -> PlaylistLink {
    let link = PlaylistLink {
        local_playlist_id: PlaylistId::new("local-playlist").unwrap(),
        server_playlist_id: ServerPlaylistId::new("server-playlist").unwrap(),
        managed_by_yututui: managed,
        state,
        content_needs_attention: false,
        shadow: PlaylistShadow {
            name: "Local playlist".to_owned(),
            occurrences: Vec::new(),
            verified_at_unix: 100,
        },
    };
    fixture
        .store_set
        .bridge_state
        .upsert_playlist_link(link.clone())
        .unwrap();
    let expected = fixture.store_set.revisions();
    commit_store_set(&fixture.paths, expected, &mut fixture.store_set).unwrap();
    link
}

#[tokio::test]
async fn ambiguous_create_is_durable_and_never_blindly_retried() {
    let (port, server) = scripted_server(vec![Step {
        endpoint: "/rest/createPlaylist.view?",
        reply: Reply::DropConnection,
    }])
    .await;
    let mut fixture = fixture(port).await;

    assert!(
        create_linked(
            &mut fixture.store_set,
            &fixture.client,
            &fixture.bridge,
            local_snapshot(),
            false,
            None,
        )
        .await
        .is_err()
    );
    assert_eq!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_creates()
            .len(),
        1
    );
    let status = super::super::status_from_store_set(&fixture.store_set);
    assert_eq!(
        status.kind,
        crate::open_subsonic::OpenSubsonicStatusKind::NeedsAttention
    );
    assert_eq!(status.playlist_creates_needing_attention, 1);
    assert!(
        create_linked(
            &mut fixture.store_set,
            &fixture.client,
            &fixture.bridge,
            local_snapshot(),
            false,
            None,
        )
        .await
        .is_err()
    );
    server.await.unwrap();
    assert!(fixture.store_set.bridge_state.playlist_links().is_empty());
    let durable = load_store_set(&fixture.paths).unwrap().unwrap();
    assert!(durable.bridge_state.playlist_links().is_empty());
    assert_eq!(durable.bridge_state.pending_playlist_creates().len(), 1);

    fixture
        .bridge
        .cancel_playlist_create(
            &mut fixture.store_set,
            &PlaylistId::new("local-playlist").unwrap(),
        )
        .unwrap();
    assert!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_creates()
            .is_empty()
    );
    let status = super::super::status_from_store_set(&fixture.store_set);
    assert_eq!(
        status.kind,
        crate::open_subsonic::OpenSubsonicStatusKind::UpToDate
    );
    assert_eq!(status.playlist_creates_needing_attention, 0);
}

#[tokio::test]
async fn confirmed_create_id_resumes_readback_without_creating_twice() {
    let body = playlist_body("created-playlist", "Local playlist", Some(false));
    let (port, server) = scripted_server(vec![
        Step {
            endpoint: "/rest/createPlaylist.view?",
            reply: Reply::Json(body.clone()),
        },
        Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::Unavailable,
        },
        Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::Json(body),
        },
    ])
    .await;
    let mut fixture = fixture(port).await;
    bind_fixture_owner(&mut fixture);

    assert!(
        create_linked(
            &mut fixture.store_set,
            &fixture.client,
            &fixture.bridge,
            local_snapshot(),
            false,
            None,
        )
        .await
        .is_err()
    );
    let pending = fixture
        .store_set
        .bridge_state
        .pending_playlist_creates()
        .values()
        .next()
        .unwrap();
    assert_eq!(
        pending
            .created_server_playlist_id
            .as_ref()
            .map(ServerPlaylistId::as_str),
        Some("created-playlist")
    );

    fixture.store_set = load_store_set(&fixture.paths).unwrap().unwrap();
    let mut edited = local_snapshot();
    edited.name = "Edited after create".to_owned();
    let created = create_linked(
        &mut fixture.store_set,
        &fixture.client,
        &fixture.bridge,
        edited.clone(),
        false,
        None,
    )
    .await
    .unwrap();
    server.await.unwrap();

    assert_eq!(created.as_str(), "created-playlist");
    assert!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_creates()
            .is_empty()
    );
    assert_eq!(fixture.store_set.bridge_state.playlist_links().len(), 1);
    assert_eq!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_projections()
            .values()
            .next()
            .map(|pending| pending.desired_name.as_str()),
        Some("Edited after create")
    );
}

#[tokio::test]
async fn closed_reply_receiver_does_not_cancel_a_replay_unsafe_create() {
    let body = playlist_body("created-playlist", "Local playlist", Some(false));
    let (port, server) = scripted_server(vec![
        Step {
            endpoint: "/rest/createPlaylist.view?",
            reply: Reply::Json(body.clone()),
        },
        Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::Json(body),
        },
    ])
    .await;
    let mut fixture = fixture(port).await;
    bind_fixture_owner(&mut fixture);
    let (reply, receiver) = tokio::sync::oneshot::channel();
    drop(receiver);

    handle_command(
        PlaylistActorCommand::CreateLinked {
            snapshot: local_snapshot(),
            replace_missing: false,
            expected_missing_server_id: None,
            reply,
        },
        &mut PlaylistPreviewCache::default(),
        &mut fixture.store_set,
        &fixture.client,
        &fixture.bridge,
    )
    .await;
    server.await.unwrap();

    assert!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_creates()
            .is_empty()
    );
    assert_eq!(fixture.store_set.bridge_state.playlist_links().len(), 1);
    let durable = load_store_set(&fixture.paths).unwrap().unwrap();
    assert!(durable.bridge_state.pending_playlist_creates().is_empty());
    assert_eq!(durable.bridge_state.playlist_links().len(), 1);
}

#[tokio::test]
async fn ambiguous_delete_commits_only_after_exact_not_found_readback() {
    let body = playlist_body("server-playlist", "Local playlist", Some(false));
    let (port, server) = scripted_server(vec![
        Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::Json(body),
        },
        Step {
            endpoint: "/rest/deletePlaylist.view?",
            reply: Reply::Unavailable,
        },
        Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::NotFound,
        },
    ])
    .await;
    let mut fixture = fixture(port).await;
    bind_fixture_owner(&mut fixture);
    install_link(&mut fixture, true, PlaylistLinkState::Linked);
    let server_id = ServerPlaylistId::new("server-playlist").unwrap();

    delete_both(
        &mut fixture.store_set,
        &fixture.client,
        &fixture.bridge,
        &server_id,
    )
    .await
    .unwrap();
    server.await.unwrap();

    assert!(fixture.store_set.bridge_state.playlist_links().is_empty());
    let batch = fixture
        .store_set
        .bridge_state
        .pending_playlist_imports()
        .values()
        .next()
        .unwrap();
    assert!(matches!(
        batch.operations.as_slice(),
        [ExternalOperationInput {
            operation: Operation::DeletePlaylist { deleted: true, .. },
            ..
        }]
    ));
}

#[tokio::test]
async fn ambiguous_delete_that_still_exists_is_not_retried_or_committed() {
    let body = playlist_body("server-playlist", "Local playlist", Some(false));
    let (port, server) = scripted_server(vec![
        Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::Json(body.clone()),
        },
        Step {
            endpoint: "/rest/deletePlaylist.view?",
            reply: Reply::Unavailable,
        },
        Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::Json(body),
        },
    ])
    .await;
    let mut fixture = fixture(port).await;
    bind_fixture_owner(&mut fixture);
    let link = install_link(&mut fixture, true, PlaylistLinkState::Linked);
    let server_id = link.server_playlist_id.clone();

    assert_eq!(
        delete_both(
            &mut fixture.store_set,
            &fixture.client,
            &fixture.bridge,
            &server_id,
        )
        .await,
        Err(ServerError::TemporarilyUnavailable)
    );
    server.await.unwrap();
    assert_eq!(
        fixture
            .store_set
            .bridge_state
            .playlist_link(&link.local_playlist_id),
        Some(&link)
    );
    assert!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_imports()
            .is_empty()
    );
}

#[tokio::test]
async fn delete_local_for_missing_link_never_contacts_a_reappeared_server_copy() {
    // The empty scripted server fails if any request arrives. A remote copy may have reappeared
    // since the durable missing observation, but DeleteLocal must never inspect or mutate it.
    let (port, server) = scripted_server(Vec::new()).await;
    let mut fixture = fixture(port).await;
    let link = install_link(&mut fixture, true, PlaylistLinkState::ServerMissing);
    let status = super::super::status_from_store_set(&fixture.store_set);
    assert_eq!(
        status.kind,
        crate::open_subsonic::OpenSubsonicStatusKind::NeedsAttention
    );
    assert_eq!(status.playlist_links_needing_decision, 1);
    assert_eq!(status.playlist_projections_needing_attention, 0);
    fixture
        .bridge
        .begin_playlist_create(&mut fixture.store_set, pending_restore(&link))
        .unwrap();
    let restoring = super::super::status_from_store_set(&fixture.store_set);
    assert_eq!(restoring.playlist_creates_needing_attention, 1);
    assert_eq!(restoring.playlist_links_needing_decision, 0);

    delete_missing_local(
        &mut fixture.store_set,
        &fixture.bridge,
        &link.server_playlist_id,
    )
    .unwrap();
    server.await.unwrap();

    assert!(
        fixture
            .store_set
            .bridge_state
            .playlist_link(&link.local_playlist_id)
            .is_none()
    );
    assert!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_creates()
            .is_empty()
    );
    let batch = fixture
        .store_set
        .bridge_state
        .pending_playlist_imports()
        .values()
        .next()
        .expect("local delete tombstone");
    assert!(matches!(
        batch.operations.as_slice(),
        [ExternalOperationInput {
            operation: Operation::DeletePlaylist { deleted: true, .. },
            ..
        }]
    ));
    let durable = load_store_set(&fixture.paths).unwrap().unwrap();
    assert!(
        durable
            .bridge_state
            .playlist_link(&link.local_playlist_id)
            .is_none()
    );
    assert!(durable.bridge_state.pending_playlist_creates().is_empty());
}

#[tokio::test]
async fn delete_restore_delete_uses_a_fresh_causal_acknowledgement() {
    let (port, server) = scripted_server(Vec::new()).await;
    let mut fixture = fixture(port).await;
    let link = install_link(&mut fixture, true, PlaylistLinkState::ServerMissing);

    delete_missing_local(
        &mut fixture.store_set,
        &fixture.bridge,
        &link.server_playlist_id,
    )
    .unwrap();
    let first = fixture
        .store_set
        .bridge_state
        .pending_playlist_imports()
        .values()
        .next()
        .cloned()
        .unwrap();
    fixture
        .bridge
        .acknowledge_import(&mut fixture.store_set, &first.operation_id)
        .unwrap();

    fixture
        .store_set
        .bridge_state
        .upsert_playlist_link(link.clone())
        .unwrap();
    let expected = fixture.store_set.revisions();
    commit_store_set(&fixture.paths, expected, &mut fixture.store_set).unwrap();
    delete_missing_local(
        &mut fixture.store_set,
        &fixture.bridge,
        &link.server_playlist_id,
    )
    .unwrap();
    let second = fixture
        .store_set
        .bridge_state
        .pending_playlist_imports()
        .values()
        .next()
        .cloned()
        .unwrap();
    server.await.unwrap();

    assert_ne!(first.operation_id, second.operation_id);
    assert_ne!(
        first.operations[0].acknowledgement_id,
        second.operations[0].acknowledgement_id
    );

    let initial = crate::personal_state::legacy_state(
        &crate::library::Library::default(),
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();
    let origin = crate::personal_state::OperationOrigin::OpenSubsonic {
        backend_id: fixture.store_set.profile.backend_id().as_str().to_owned(),
    };
    let (first_deleted, _) = crate::personal_state::append_external_operations(
        &initial,
        origin.clone(),
        &first.operations,
    )
    .unwrap();
    let restored = crate::personal_state::append_external_operation(
        &first_deleted,
        "restore-between-deletes".to_owned(),
        origin.clone(),
        Operation::UpsertPlaylist {
            playlist_id: link.local_playlist_id.clone(),
            name: "Restored".to_owned(),
        },
        200,
    )
    .unwrap();
    assert!(
        crate::personal_state::personal_playlist_snapshot(&restored, &link.local_playlist_id)
            .unwrap()
            .is_some()
    );
    let (second_deleted, _) =
        crate::personal_state::append_external_operations(&restored, origin, &second.operations)
            .unwrap();
    assert!(
        crate::personal_state::personal_playlist_snapshot(
            &second_deleted,
            &link.local_playlist_id,
        )
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
async fn read_only_runtime_rejects_delete_before_any_playlist_request_or_store_change() {
    let (port, server) = scripted_server(vec![
        Step {
            endpoint: "/rest/getOpenSubsonicExtensions.view?",
            reply: Reply::Json(
                r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[]}}"#.to_owned(),
            ),
        },
        Step {
            endpoint: "/rest/ping.view?",
            reply: Reply::Json(
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.to_owned(),
            ),
        },
    ])
    .await;
    let mut fixture = fixture(port).await;
    let link = install_link(&mut fixture, true, PlaylistLinkState::Linked);
    let revisions_before = fixture.store_set.revisions();

    let runtime = super::super::load_actor_read_only(&fixture.paths)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        runtime
            .handle()
            .delete_linked_playlist(link.server_playlist_id.clone())
            .await,
        Err(ServerError::PermissionDenied)
    );
    runtime.shutdown().await;
    server.await.unwrap();

    let durable = load_store_set(&fixture.paths).unwrap().unwrap();
    assert_eq!(durable.revisions(), revisions_before);
    assert_eq!(
        durable.bridge_state.playlist_link(&link.local_playlist_id),
        Some(&link)
    );
}

#[tokio::test]
async fn unlink_missing_playlist_clears_an_unresolved_restore_intent() {
    let (port, server) = scripted_server(Vec::new()).await;
    let mut fixture = fixture(port).await;
    let link = install_link(&mut fixture, true, PlaylistLinkState::ServerMissing);
    fixture
        .bridge
        .begin_playlist_create(&mut fixture.store_set, pending_restore(&link))
        .unwrap();

    fixture
        .bridge
        .unlink_playlist(&mut fixture.store_set, &link.local_playlist_id)
        .unwrap();
    server.await.unwrap();

    assert!(
        fixture
            .store_set
            .bridge_state
            .playlist_link(&link.local_playlist_id)
            .is_none()
    );
    assert!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_creates()
            .is_empty()
    );
    let durable = load_store_set(&fixture.paths).unwrap().unwrap();
    assert!(durable.bridge_state.playlist_links().is_empty());
    assert!(durable.bridge_state.pending_playlist_creates().is_empty());
}

#[tokio::test]
async fn delete_local_rejects_a_link_that_is_no_longer_server_missing() {
    let (port, server) = scripted_server(Vec::new()).await;
    let mut fixture = fixture(port).await;
    let link = install_link(&mut fixture, true, PlaylistLinkState::Linked);

    assert_eq!(
        delete_missing_local(
            &mut fixture.store_set,
            &fixture.bridge,
            &link.server_playlist_id,
        ),
        Err(ServerError::NotFound)
    );
    server.await.unwrap();

    assert_eq!(
        fixture
            .store_set
            .bridge_state
            .playlist_link(&link.local_playlist_id),
        Some(&link)
    );
    assert!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_imports()
            .is_empty()
    );
    assert_eq!(
        load_store_set(&fixture.paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .playlist_link(&link.local_playlist_id),
        Some(&link)
    );
}

#[tokio::test]
async fn unmanaged_link_never_selects_the_managed_api_key_bypass() {
    let (port, server) = scripted_server(vec![Step {
        endpoint: "/rest/getPlaylist.view?",
        reply: Reply::Json(playlist_body(
            "server-playlist",
            "Local playlist",
            Some(false),
        )),
    }])
    .await;
    let mut fixture = fixture(port).await;
    let link = install_link(&mut fixture, false, PlaylistLinkState::Linked);

    assert_eq!(
        delete_both(
            &mut fixture.store_set,
            &fixture.client,
            &fixture.bridge,
            &link.server_playlist_id,
        )
        .await,
        Err(ServerError::PermissionDenied)
    );
    server.await.unwrap();
    assert_eq!(
        fixture
            .store_set
            .bridge_state
            .playlist_link(&link.local_playlist_id),
        Some(&link)
    );
}

#[tokio::test]
async fn managed_delete_both_fails_closed_without_exact_remote_access_evidence() {
    for (owner, read_only) in [
        (None, Some(false)),
        (Some("alice"), None),
        (Some("mallory"), Some(false)),
    ] {
        let (port, server) = scripted_server(vec![Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::Json(playlist_body_with_access(
                "server-playlist",
                "Local playlist",
                owner,
                read_only,
            )),
        }])
        .await;
        let mut fixture = fixture(port).await;
        bind_fixture_owner(&mut fixture);
        let link = install_link(&mut fixture, true, PlaylistLinkState::Linked);

        assert_eq!(
            delete_both(
                &mut fixture.store_set,
                &fixture.client,
                &fixture.bridge,
                &link.server_playlist_id,
            )
            .await,
            Err(ServerError::PermissionDenied)
        );
        server.await.unwrap();
        assert_eq!(
            fixture
                .store_set
                .bridge_state
                .playlist_link(&link.local_playlist_id),
            Some(&link)
        );
        assert!(
            fixture
                .store_set
                .bridge_state
                .pending_playlist_imports()
                .is_empty()
        );
        let durable = load_store_set(&fixture.paths).unwrap().unwrap();
        assert_eq!(
            durable.bridge_state.playlist_link(&link.local_playlist_id),
            Some(&link)
        );
        assert!(durable.bridge_state.pending_playlist_imports().is_empty());
    }
}

#[tokio::test]
async fn restore_reuses_a_reappeared_original_server_playlist_without_creating_a_duplicate() {
    let original = playlist_body("server-playlist", "Local playlist", Some(false));
    let (port, server) = scripted_server(vec![Step {
        endpoint: "/rest/getPlaylist.view?",
        reply: Reply::Json(original),
    }])
    .await;
    let mut fixture = fixture(port).await;
    bind_fixture_owner(&mut fixture);
    let missing = install_link(&mut fixture, false, PlaylistLinkState::ServerMissing);

    let restored_id = create_linked(
        &mut fixture.store_set,
        &fixture.client,
        &fixture.bridge,
        local_snapshot(),
        true,
        Some(&missing.server_playlist_id),
    )
    .await
    .unwrap();
    server.await.unwrap();

    assert_eq!(restored_id, missing.server_playlist_id);
    let link = fixture
        .store_set
        .bridge_state
        .playlist_link(&missing.local_playlist_id)
        .unwrap();
    assert_eq!(link.server_playlist_id, missing.server_playlist_id);
    assert_eq!(link.state, PlaylistLinkState::Linked);
    assert!(
        fixture
            .store_set
            .bridge_state
            .pending_playlist_creates()
            .is_empty()
    );
}

#[tokio::test]
async fn restore_keeps_a_reappeared_original_missing_without_exact_write_access() {
    for (owner, read_only) in [
        (None, Some(false)),
        (Some("alice"), None),
        (Some("alice"), Some(true)),
        (Some("mallory"), Some(false)),
    ] {
        let (port, server) = scripted_server(vec![Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::Json(playlist_body_with_access(
                "server-playlist",
                "Local playlist",
                owner,
                read_only,
            )),
        }])
        .await;
        let mut fixture = fixture(port).await;
        bind_fixture_owner(&mut fixture);
        let missing = install_link(&mut fixture, false, PlaylistLinkState::ServerMissing);

        assert_eq!(
            create_linked(
                &mut fixture.store_set,
                &fixture.client,
                &fixture.bridge,
                local_snapshot(),
                true,
                Some(&missing.server_playlist_id),
            )
            .await,
            Err(ServerError::PermissionDenied)
        );
        server.await.unwrap();

        assert_eq!(
            fixture
                .store_set
                .bridge_state
                .playlist_link(&missing.local_playlist_id),
            Some(&missing)
        );
        assert!(
            fixture
                .store_set
                .bridge_state
                .pending_playlist_creates()
                .is_empty()
        );
        assert!(
            fixture
                .store_set
                .bridge_state
                .pending_playlist_imports()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn restore_replaces_only_the_expected_missing_link_with_verified_managed_state() {
    let restored = playlist_body("restored-playlist", "Local playlist", Some(false));
    let (port, server) = scripted_server(vec![
        Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::NotFound,
        },
        Step {
            endpoint: "/rest/createPlaylist.view?",
            reply: Reply::Json(restored.clone()),
        },
        Step {
            endpoint: "/rest/getPlaylist.view?",
            reply: Reply::Json(restored),
        },
    ])
    .await;
    let mut fixture = fixture(port).await;
    bind_fixture_owner(&mut fixture);
    let missing = install_link(&mut fixture, false, PlaylistLinkState::ServerMissing);

    let restored_id = create_linked(
        &mut fixture.store_set,
        &fixture.client,
        &fixture.bridge,
        local_snapshot(),
        true,
        Some(&missing.server_playlist_id),
    )
    .await
    .unwrap();
    server.await.unwrap();

    assert_eq!(restored_id.as_str(), "restored-playlist");
    let link = fixture
        .store_set
        .bridge_state
        .playlist_link(&missing.local_playlist_id)
        .unwrap();
    assert_eq!(link.server_playlist_id, restored_id);
    assert_eq!(link.state, PlaylistLinkState::Linked);
    assert!(link.managed_by_yututui);
}

#[test]
fn created_readback_requires_exact_owner_and_explicit_writable_evidence() {
    let pending = PendingPlaylistCreate {
        local_playlist_id: PlaylistId::new("local-playlist").unwrap(),
        expected_missing_server_id: None,
        created_server_playlist_id: Some(ServerPlaylistId::new("server").unwrap()),
        desired_name: "Local playlist".to_owned(),
        ordered_entry_ids: Vec::new(),
        ordered_item_ids: Vec::new(),
        started_at_unix: 100,
    };
    let credential =
        ServerCredential::password("alice", SecretString::from("password".to_owned())).unwrap();
    let remote = |owner: Option<&str>, read_only| {
        ServerPlaylistWriteSnapshot::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
            ServerPlaylistId::new("server").unwrap(),
            "Local playlist".to_owned(),
            owner.map(str::to_owned),
            read_only,
            Vec::new(),
        )
    };

    assert_eq!(
        verify_created_snapshot(&pending, &remote(Some("alice"), Some(true)), &credential),
        Err(ServerError::PermissionDenied)
    );
    assert_eq!(
        verify_created_snapshot(&pending, &remote(Some("mallory"), Some(false)), &credential),
        Err(ServerError::PermissionDenied)
    );
    assert_eq!(
        verify_created_snapshot(&pending, &remote(None, Some(false)), &credential),
        Err(ServerError::PermissionDenied)
    );
    assert_eq!(
        verify_created_snapshot(&pending, &remote(Some("alice"), None), &credential),
        Err(ServerError::PermissionDenied)
    );
    assert_eq!(
        verify_created_snapshot(&pending, &remote(Some("alice"), Some(false)), &credential),
        Ok(())
    );
}
