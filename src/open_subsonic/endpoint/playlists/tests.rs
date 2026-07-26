use std::time::Duration;

use age::secrecy::SecretString;
use tokio::io::AsyncWriteExt as _;

use super::super::test_support::*;
use super::*;
use crate::open_subsonic::{AccountScopeId, BackendId};

#[tokio::test]
async fn write_snapshot_preserves_duplicates_and_access_control() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        assert!(request_target(&request).starts_with("/rest/getPlaylist.view?"));
        write_json(
            &mut stream,
            r#"{"subsonic-response":{"status":"ok","playlist":{"id":"playlist","name":"Playlist","owner":"alice","readonly":false,"songCount":2,"entry":[{"id":"same","title":"First","type":"music"},{"id":"same","title":"Second","type":"music"}]}}}"#,
        )
        .await;
    });
    let fixture = test_client(port).await;
    let snapshot = fixture
        .client
        .get_playlist_write_snapshot(
            &fixture.credential,
            &ServerPlaylistId::new("playlist").unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(snapshot.owner(), Some("alice"));
    assert_eq!(snapshot.read_only(), Some(false));
    assert_eq!(snapshot.entries().len(), 2);
    assert_eq!(
        snapshot.entries()[0].item.item_id(),
        snapshot.entries()[1].item.item_id()
    );
    assert_eq!(snapshot.entries()[0].title, "First");
    assert_eq!(snapshot.entries()[1].title, "Second");
    server.await.unwrap();
}

#[tokio::test]
async fn write_snapshot_rejects_any_unsafe_occurrence() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = [
        r#"{"subsonic-response":{"status":"ok","playlist":{"id":"playlist","name":"Playlist","owner":"alice","readonly":false,"entry":[{"title":"Missing id"}]}}}"#,
        r#"{"subsonic-response":{"status":"ok","playlist":{"id":"playlist","name":"Playlist","owner":"alice","readonly":false,"entry":[{"id":"directory","title":"Directory","isDir":true}]}}}"#,
        r#"{"subsonic-response":{"status":"ok","playlist":{"id":"playlist","name":"Playlist","owner":"alice","readonly":false,"entry":[{"id":"video","title":"Video","type":"video"}]}}}"#,
        r#"{"subsonic-response":{"status":"ok","playlist":{"id":"playlist","name":"Playlist","owner":"alice","readonly":false,"songCount":2,"entry":[{"id":"only-one","title":"Song","type":"music"}]}}}"#,
    ];
    let server = tokio::spawn(async move {
        for body in bodies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            write_json(&mut stream, body).await;
        }
    });
    let fixture = test_client(port).await;
    let playlist_id = ServerPlaylistId::new("playlist").unwrap();

    for _ in bodies {
        assert!(matches!(
            fixture
                .client
                .get_playlist_write_snapshot(&fixture.credential, &playlist_id)
                .await,
            Err(ServerError::InvalidResponse)
        ));
    }
    server.await.unwrap();
}

#[tokio::test]
async fn mutations_preserve_add_occurrences_and_sort_removals() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut create, _) = listener.accept().await.unwrap();
        let create_request = read_request(&mut create).await;
        assert!(request_target(&create_request).starts_with("/rest/createPlaylist.view?"));
        assert_eq!(query_values(&create_request, "name"), ["Road trip"]);
        assert_eq!(
            query_values(&create_request, "songId"),
            ["same", "same", "other"]
        );
        write_json(
            &mut create,
            r#"{"subsonic-response":{"status":"ok","playlist":{"id":"remote","name":"Road trip","owner":"alice","readonly":false,"songCount":3,"entry":[{"id":"same","title":"First","type":"music"},{"id":"same","title":"Second","type":"music"},{"id":"other","title":"Other","type":"music"}]}}}"#,
        )
        .await;

        let (mut update, _) = listener.accept().await.unwrap();
        let update_request = read_request(&mut update).await;
        assert!(request_target(&update_request).starts_with("/rest/updatePlaylist.view?"));
        assert_eq!(query_values(&update_request, "playlistId"), ["remote"]);
        assert_eq!(query_values(&update_request, "name"), ["Renamed"]);
        assert_eq!(
            query_values(&update_request, "songIdToAdd"),
            ["same", "same"]
        );
        assert_eq!(
            query_values(&update_request, "songIndexToRemove"),
            ["2", "0"]
        );
        write_json(&mut update, r#"{"subsonic-response":{"status":"ok"}}"#).await;

        let (mut delete, _) = listener.accept().await.unwrap();
        let delete_request = read_request(&mut delete).await;
        assert!(request_target(&delete_request).starts_with("/rest/deletePlaylist.view?"));
        assert_eq!(query_values(&delete_request, "id"), ["remote"]);
        write_json(&mut delete, r#"{"subsonic-response":{"status":"ok"}}"#).await;
    });
    let fixture = test_client_with_password(port).await;
    let same = fixture.item("same");
    let other = fixture.item("other");
    let snapshot = fixture
        .client
        .create_playlist(
            &fixture.credential,
            "Road trip",
            &[same.clone(), same.clone(), other],
        )
        .await
        .unwrap()
        .unwrap();
    fixture
        .client
        .update_playlist(
            &fixture.credential,
            &snapshot,
            Some("Renamed"),
            &[same.clone(), same],
            &[0, 2, 0],
        )
        .await
        .unwrap();
    fixture
        .client
        .delete_playlist(&fixture.credential, &snapshot)
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn writes_require_explicit_writable_and_exact_account_owner() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = [
        r#"{"subsonic-response":{"status":"ok","playlist":{"id":"playlist","name":"Playlist","owner":"bob","readonly":false,"entry":[]}}}"#,
        r#"{"subsonic-response":{"status":"ok","playlist":{"id":"playlist","name":"Playlist","owner":"alice","entry":[]}}}"#,
        r#"{"subsonic-response":{"status":"ok","playlist":{"id":"playlist","name":"Playlist","owner":"alice","readonly":false,"entry":[]}}}"#,
    ];
    let server = tokio::spawn(async move {
        for body in bodies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            write_json(&mut stream, body).await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "rejected playlist writes must not reach the network"
        );
    });
    let fixture = test_client_with_password(port).await;
    let playlist_id = ServerPlaylistId::new("playlist").unwrap();
    let wrong_owner = fixture
        .client
        .get_playlist_write_snapshot(&fixture.credential, &playlist_id)
        .await
        .unwrap();
    let unknown_readonly = fixture
        .client
        .get_playlist_write_snapshot(&fixture.credential, &playlist_id)
        .await
        .unwrap();
    let api_key_snapshot = fixture
        .client
        .get_playlist_write_snapshot(&fixture.credential, &playlist_id)
        .await
        .unwrap();

    for snapshot in [&wrong_owner, &unknown_readonly] {
        assert_eq!(
            fixture
                .client
                .update_playlist(&fixture.credential, snapshot, Some("Changed"), &[], &[])
                .await,
            Err(MutationDeliveryError::DefinitelyNotApplied(
                ServerError::PermissionDenied
            ))
        );
    }
    let api_key =
        ServerCredential::api_key(SecretString::from("different-api-key".to_owned())).unwrap();
    assert_eq!(
        fixture
            .client
            .delete_playlist(&api_key, &api_key_snapshot)
            .await,
        Err(MutationDeliveryError::DefinitelyNotApplied(
            ServerError::PermissionDenied
        ))
    );
    server.await.unwrap();
}

#[test]
fn token_info_bound_api_key_proves_only_its_exact_owner() {
    let fixture = test_snapshot("alice", Some(false));
    let mut matching =
        ServerCredential::api_key(SecretString::from("matching-api-key".to_owned())).unwrap();
    matching.bind_api_key_username("alice").unwrap();
    let mut different =
        ServerCredential::api_key(SecretString::from("different-api-key".to_owned())).unwrap();
    different.bind_api_key_username("bob").unwrap();

    assert!(
        ensure_playlist_write_allowed(
            &fixture,
            &matching,
            PlaylistWriteAccess::VerifiedAccountOwner
        )
        .is_ok()
    );
    assert_eq!(
        ensure_playlist_write_allowed(
            &fixture,
            &different,
            PlaylistWriteAccess::VerifiedAccountOwner
        ),
        Err(ServerError::PermissionDenied)
    );
    assert_eq!(
        ensure_playlist_write_allowed(
            &test_snapshot("alice", None),
            &matching,
            PlaylistWriteAccess::VerifiedAccountOwner
        ),
        Err(ServerError::PermissionDenied)
    );
}

#[test]
fn managed_path_requires_exact_owner_and_explicit_writable_evidence() {
    let matching = test_snapshot("alice", Some(false));
    let password =
        ServerCredential::password("alice", SecretString::from("password".to_owned())).unwrap();
    let mut api_key = ServerCredential::api_key(SecretString::from("api-key".to_owned())).unwrap();
    api_key.bind_api_key_username("alice").unwrap();

    for credential in [&password, &api_key] {
        assert!(
            ensure_playlist_write_allowed(
                &matching,
                credential,
                PlaylistWriteAccess::ManagedByYututui,
            )
            .is_ok()
        );
        assert_eq!(
            ensure_playlist_write_allowed(
                &test_snapshot("mallory", Some(false)),
                credential,
                PlaylistWriteAccess::ManagedByYututui,
            ),
            Err(ServerError::PermissionDenied)
        );
        assert_eq!(
            ensure_playlist_write_allowed(
                &test_snapshot("alice", None),
                credential,
                PlaylistWriteAccess::ManagedByYututui,
            ),
            Err(ServerError::PermissionDenied)
        );
        assert_eq!(
            ensure_playlist_write_allowed(
                &test_snapshot_with_access(None, Some(false)),
                credential,
                PlaylistWriteAccess::ManagedByYututui,
            ),
            Err(ServerError::PermissionDenied)
        );
        assert_eq!(
            ensure_playlist_write_allowed(
                &test_snapshot("alice", Some(true)),
                credential,
                PlaylistWriteAccess::ManagedByYututui,
            ),
            Err(ServerError::PermissionDenied)
        );
    }
    let unbound =
        ServerCredential::api_key(SecretString::from("unbound-api-key".to_owned())).unwrap();
    assert_eq!(
        ensure_playlist_write_allowed(&matching, &unbound, PlaylistWriteAccess::ManagedByYututui,),
        Err(ServerError::PermissionDenied)
    );
}

#[tokio::test]
async fn managed_update_and_delete_use_exact_verified_owner_policy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut update, _) = listener.accept().await.unwrap();
        let update_request = read_request(&mut update).await;
        assert!(request_target(&update_request).starts_with("/rest/updatePlaylist.view?"));
        write_json(&mut update, r#"{"subsonic-response":{"status":"ok"}}"#).await;

        let (mut delete, _) = listener.accept().await.unwrap();
        let delete_request = read_request(&mut delete).await;
        assert!(request_target(&delete_request).starts_with("/rest/deletePlaylist.view?"));
        write_json(&mut delete, r#"{"subsonic-response":{"status":"ok"}}"#).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "verified managed mutations must issue exactly one request each"
        );
    });
    let fixture = test_client_with_password(port).await;
    let managed = ServerPlaylistWriteSnapshot::new(
        fixture.backend_id.clone(),
        fixture.account_scope_id.clone(),
        ServerPlaylistId::new("managed").unwrap(),
        "Managed".to_owned(),
        Some("alice".to_owned()),
        Some(false),
        Vec::new(),
    );
    fixture
        .client
        .update_managed_playlist(
            &fixture.credential,
            &managed,
            Some("Managed renamed"),
            &[],
            &[],
        )
        .await
        .unwrap();
    fixture
        .client
        .delete_managed_playlist(&fixture.credential, &managed)
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn legacy_empty_create_response_has_no_confirmable_identity() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        assert!(request_target(&request).starts_with("/rest/createPlaylist.view?"));
        write_json(&mut stream, r#"{"subsonic-response":{"status":"ok"}}"#).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "an unconfirmed create must never be retried blindly"
        );
    });
    let fixture = test_client(port).await;

    assert!(
        fixture
            .client
            .create_playlist(&fixture.credential, "Empty", &[])
            .await
            .unwrap()
            .is_none()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn response_loss_and_server_errors_are_ambiguous_and_single_shot() {
    for response in [
        None,
        Some(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .as_slice(),
        ),
        Some(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx"
                .as_slice(),
        ),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(request_target(&request).starts_with("/rest/createPlaylist.view?"));
            if let Some(response) = response {
                stream.write_all(response).await.unwrap();
            }
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "an ambiguous create must issue exactly one request"
            );
        });
        let fixture = test_client(port).await;

        assert!(matches!(
            fixture
                .client
                .create_playlist(&fixture.credential, "Ambiguous", &[])
                .await,
            Err(MutationDeliveryError::Ambiguous(_))
        ));
        server.await.unwrap();
    }
}

fn test_snapshot(owner: &str, read_only: Option<bool>) -> ServerPlaylistWriteSnapshot {
    test_snapshot_with_access(Some(owner), read_only)
}

fn test_snapshot_with_access(
    owner: Option<&str>,
    read_only: Option<bool>,
) -> ServerPlaylistWriteSnapshot {
    ServerPlaylistWriteSnapshot::new(
        BackendId::new("backend").unwrap(),
        AccountScopeId::new("account").unwrap(),
        ServerPlaylistId::new("playlist").unwrap(),
        "Playlist".to_owned(),
        owner.map(str::to_owned),
        read_only,
        Vec::new(),
    )
}
