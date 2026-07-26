use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

use super::test_support::*;
use super::*;
use crate::open_subsonic::{AccountScopeId, BackendId, ItemId};

#[tokio::test]
async fn get_song_serializes_opaque_id_and_preserves_exact_server_fields() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let target = request_target(&request);
        assert!(target.starts_with("/rest/getSong.view?"));
        assert!(target.contains("id=song+id%26admin%3Dtrue"));
        assert!(!target.contains("&admin=true"));
        write_json(
            &mut stream,
            r#"{"subsonic-response":{"status":"ok","song":{"id":"song id&admin=true","title":"Song","userRating":3,"starred":"2026-07-26T00:00:00Z","playCount":7,"played":"2026-07-26T00:00:00Z"}}}"#,
        )
        .await;
    });
    let fixture = test_client(port).await;
    let song = fixture
        .client
        .get_song_raw(&fixture.credential, &fixture.item("song id&admin=true"))
        .await
        .unwrap();
    assert_eq!(song.user_rating, Some(3));
    assert_eq!(song.play_count, Some(7));
    assert!(song.starred.is_some());
    server.await.unwrap();
}

#[tokio::test]
async fn token_info_proves_api_key_owner_without_sending_a_username() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        assert!(request_target(&request).starts_with("/rest/tokenInfo.view?"));
        assert_eq!(query_values(&request, "apiKey"), ["sentinel-api-key"]);
        assert!(query_values(&request, "u").is_empty());
        write_json(
            &mut stream,
            r#"{"subsonic-response":{"status":"ok","tokenInfo":{"username":"alice"}}}"#,
        )
        .await;
    });
    let fixture = test_client(port).await;

    assert_eq!(
        fixture
            .client
            .api_key_username(&fixture.credential)
            .await
            .unwrap(),
        "alice"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn mutations_use_exact_standard_parameters_and_validate_status() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let expected = [
            ("/rest/setRating.view?", &["id=song-1", "rating=5"][..]),
            ("/rest/star.view?", &["id=song-1"][..]),
            ("/rest/unstar.view?", &["id=song-1"][..]),
            (
                "/rest/scrobble.view?",
                &["id=song-1", "submission=false"][..],
            ),
            (
                "/rest/scrobble.view?",
                &["id=song-1", "submission=true", "time=1785024000123"][..],
            ),
        ];
        for (index, (endpoint, parameters)) in expected.into_iter().enumerate() {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let target = request_target(&request);
            assert!(target.starts_with(endpoint), "{target}");
            for parameter in parameters {
                assert!(target.contains(parameter), "{target}");
            }
            if index == 3 {
                assert!(!target.contains("&time="), "{target}");
            }
            let body = if index == 4 {
                r#"{"subsonic-response":{"status":"failed","error":{"code":50}}}"#
            } else {
                r#"{"subsonic-response":{"status":"ok"}}"#
            };
            write_json(&mut stream, body).await;
        }
    });
    let fixture = test_client(port).await;
    let item = fixture.item("song-1");
    fixture
        .client
        .set_rating(&fixture.credential, &item, 5)
        .await
        .unwrap();
    fixture
        .client
        .star(&fixture.credential, &item)
        .await
        .unwrap();
    fixture
        .client
        .unstar(&fixture.credential, &item)
        .await
        .unwrap();
    fixture
        .client
        .scrobble(&fixture.credential, &item, false, None)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .client
            .scrobble(&fixture.credential, &item, true, Some(1_785_024_000_123),)
            .await,
        Err(MutationDeliveryError::DefinitelyNotApplied(
            ServerError::PermissionDenied
        ))
    );
    server.await.unwrap();
}

#[tokio::test]
async fn item_scope_invalid_rating_and_time_fail_before_network() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let fixture = test_client(port).await;
    let wrong_backend = OpenSubsonicItemRef::new(
        BackendId::new("another-backend").unwrap(),
        fixture.account_scope_id.clone(),
        ItemId::new("song").unwrap(),
    );
    let wrong_account = OpenSubsonicItemRef::new(
        fixture.backend_id.clone(),
        AccountScopeId::new("another-account").unwrap(),
        ItemId::new("song").unwrap(),
    );
    assert!(matches!(
        fixture
            .client
            .get_song_raw(&fixture.credential, &wrong_backend)
            .await,
        Err(ServerError::WrongAccountScope)
    ));
    assert_eq!(
        fixture
            .client
            .star(&fixture.credential, &wrong_account)
            .await,
        Err(ServerError::WrongAccountScope)
    );
    let item = fixture.item("song");
    assert_eq!(
        fixture
            .client
            .set_rating(&fixture.credential, &item, 6)
            .await,
        Err(ServerError::InvalidResponse)
    );
    assert_eq!(
        fixture
            .client
            .scrobble(&fixture.credential, &item, true, Some(u64::MAX))
            .await,
        Err(MutationDeliveryError::DefinitelyNotApplied(
            ServerError::InvalidResponse
        ))
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn scrobble_connect_failure_is_definitely_not_applied() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let fixture = test_client(port).await;
    assert_eq!(
        fixture
            .client
            .scrobble(&fixture.credential, &fixture.item("song"), true, None)
            .await,
        Err(MutationDeliveryError::DefinitelyNotApplied(
            ServerError::Offline
        ))
    );
}

#[tokio::test]
async fn scrobble_response_loss_and_server_errors_are_ambiguous() {
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(request_target(&request).starts_with("/rest/scrobble.view?"));
            if let Some(response) = response {
                stream.write_all(response).await.unwrap();
            }
        });
        let fixture = test_client(port).await;
        let error = fixture
            .client
            .scrobble(&fixture.credential, &fixture.item("song"), true, None)
            .await
            .unwrap_err();
        assert!(matches!(error, MutationDeliveryError::Ambiguous(_)));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn scrobble_redirects_are_ambiguous_and_never_followed() {
    for redirect_kind in ["same-origin", "loop", "cross-origin"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cross_target = if redirect_kind == "cross-origin" {
            Some(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        } else {
            None
        };
        let location = match (&cross_target, redirect_kind) {
            (Some(target), _) => format!(
                "http://127.0.0.1:{}/rest/scrobble.view",
                target.local_addr().unwrap().port()
            ),
            (None, "loop") => "/rest/scrobble.view".to_owned(),
            _ => "/rest/scrobble-again".to_owned(),
        };
        let fixture = test_client(port).await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(request_target(&request).starts_with("/rest/scrobble.view?"));
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "redirected scrobble must issue exactly one request"
            );
        });
        assert_eq!(
            fixture
                .client
                .scrobble(&fixture.credential, &fixture.item("song"), true, None)
                .await,
            Err(MutationDeliveryError::Ambiguous(
                ServerError::OriginRejected
            ))
        );
        server.await.unwrap();
        if let Some(target) = cross_target {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), target.accept())
                    .await
                    .is_err(),
                "cross-origin redirect must not receive a credentialed request"
            );
        }
    }
}

#[tokio::test]
async fn get_song_rejects_a_different_returned_item_id() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut stream).await;
        write_json(
            &mut stream,
            r#"{"subsonic-response":{"status":"ok","song":{"id":"different","title":"Song"}}}"#,
        )
        .await;
    });
    let fixture = test_client(port).await;
    assert!(matches!(
        fixture
            .client
            .get_song_raw(&fixture.credential, &fixture.item("requested"))
            .await,
        Err(ServerError::InvalidResponse)
    ));
    server.await.unwrap();
}
