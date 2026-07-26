//! Typed standard OpenSubsonic endpoint calls.
//!
//! All item mutations take a fully scoped identity. Query serialization remains owned by
//! `reqwest`; opaque server IDs are never interpolated into URLs.

use super::super::model::OpenSubsonicItemRef;
use super::super::private_store::ServerCredential;
use super::super::wire::RawChild;
use super::{MutationDeliveryError, OpenSubsonicClient, ServerError};

const MAX_SCROBBLE_TIME_UNIX_MS: u64 = i64::MAX as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Ping,
    Extensions,
    Search3,
    AlbumList2,
    Artists,
    Playlists,
    Playlist,
    Album,
    Artist,
    GetSong,
    SetRating,
    Star,
    Unstar,
    Scrobble,
    CoverArt,
    Stream,
}

impl Endpoint {
    pub(super) const fn method_name(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Extensions => "getOpenSubsonicExtensions",
            Self::Search3 => "search3",
            Self::AlbumList2 => "getAlbumList2",
            Self::Artists => "getArtists",
            Self::Playlists => "getPlaylists",
            Self::Playlist => "getPlaylist",
            Self::Album => "getAlbum",
            Self::Artist => "getArtist",
            Self::GetSong => "getSong",
            Self::SetRating => "setRating",
            Self::Star => "star",
            Self::Unstar => "unstar",
            Self::Scrobble => "scrobble",
            Self::CoverArt => "getCoverArt",
            Self::Stream => "stream",
        }
    }
}

impl OpenSubsonicClient {
    pub(crate) async fn get_song_raw(
        &self,
        credential: &ServerCredential,
        item: &OpenSubsonicItemRef,
    ) -> Result<RawChild, ServerError> {
        self.validate_item_scope(item)?;
        let response = self
            .request_json(
                credential,
                Endpoint::GetSong,
                &[("id", item.item_id().as_str().to_owned())],
            )
            .await?;
        let song = response.song.ok_or(ServerError::InvalidResponse)?;
        if song.id.as_deref() != Some(item.item_id().as_str()) {
            return Err(ServerError::InvalidResponse);
        }
        Ok(song)
    }

    pub(crate) async fn set_rating(
        &self,
        credential: &ServerCredential,
        item: &OpenSubsonicItemRef,
        rating: u8,
    ) -> Result<(), ServerError> {
        self.validate_item_scope(item)?;
        if rating > 5 {
            return Err(ServerError::InvalidResponse);
        }
        self.request_mutation(
            credential,
            Endpoint::SetRating,
            &[
                ("id", item.item_id().as_str().to_owned()),
                ("rating", rating.to_string()),
            ],
        )
        .await
    }

    pub(crate) async fn star(
        &self,
        credential: &ServerCredential,
        item: &OpenSubsonicItemRef,
    ) -> Result<(), ServerError> {
        self.item_mutation(credential, Endpoint::Star, item).await
    }

    pub(crate) async fn unstar(
        &self,
        credential: &ServerCredential,
        item: &OpenSubsonicItemRef,
    ) -> Result<(), ServerError> {
        self.item_mutation(credential, Endpoint::Unstar, item).await
    }

    pub(crate) async fn scrobble(
        &self,
        credential: &ServerCredential,
        item: &OpenSubsonicItemRef,
        submission: bool,
        time_unix_ms: Option<u64>,
    ) -> Result<(), MutationDeliveryError> {
        self.validate_item_scope(item)
            .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        if time_unix_ms.is_some_and(|time| time > MAX_SCROBBLE_TIME_UNIX_MS) {
            return Err(MutationDeliveryError::DefinitelyNotApplied(
                ServerError::InvalidResponse,
            ));
        }
        let mut parameters = vec![
            ("id", item.item_id().as_str().to_owned()),
            ("submission", submission.to_string()),
        ];
        if let Some(time) = time_unix_ms {
            parameters.push(("time", time.to_string()));
        }
        self.request_scrobble_mutation(credential, &parameters)
            .await
    }

    async fn item_mutation(
        &self,
        credential: &ServerCredential,
        endpoint: Endpoint,
        item: &OpenSubsonicItemRef,
    ) -> Result<(), ServerError> {
        self.validate_item_scope(item)?;
        self.request_mutation(
            credential,
            endpoint,
            &[("id", item.item_id().as_str().to_owned())],
        )
        .await
    }

    async fn request_mutation(
        &self,
        credential: &ServerCredential,
        endpoint: Endpoint,
        parameters: &[(&str, String)],
    ) -> Result<(), ServerError> {
        // Standard mutations return an otherwise-empty envelope. `request_json` is intentional:
        // accepting an HTTP success without checking `subsonic-response.status` would silently
        // acknowledge rejected writes.
        self.request_json(credential, endpoint, parameters)
            .await
            .map(drop)
    }

    async fn request_scrobble_mutation(
        &self,
        credential: &ServerCredential,
        parameters: &[(&str, String)],
    ) -> Result<(), MutationDeliveryError> {
        let response = self
            .request_response_with_delivery(
                Some(credential),
                Endpoint::Scrobble,
                parameters,
                reqwest::Method::GET,
                None,
            )
            .await?;
        if !response.status().is_success() {
            let error = super::status_error_for(Endpoint::Scrobble, &response);
            return if response.status().is_server_error() {
                Err(MutationDeliveryError::Ambiguous(error))
            } else {
                Err(MutationDeliveryError::DefinitelyNotApplied(error))
            };
        }
        let bytes = super::read_limited(response, super::MAX_JSON_BYTES)
            .await
            .map_err(MutationDeliveryError::Ambiguous)?;
        match super::super::wire::decode(&bytes) {
            Ok(_) => Ok(()),
            Err(super::super::wire::WireError::ApiFailure(error)) => {
                Err(MutationDeliveryError::DefinitelyNotApplied(
                    super::map_wire_error(super::super::wire::WireError::ApiFailure(error)),
                ))
            }
            Err(error) => Err(MutationDeliveryError::Ambiguous(super::map_wire_error(
                error,
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use age::secrecy::SecretString;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::open_subsonic::{
        AccountScopeId, BackendId, ConfiguredPrivateOrigin, ItemId, OpenSubsonicProfile,
        ServerCredential,
    };

    struct TestClient {
        client: OpenSubsonicClient,
        credential: ServerCredential,
        backend_id: BackendId,
        account_scope_id: AccountScopeId,
    }

    impl TestClient {
        fn item(&self, item_id: &str) -> OpenSubsonicItemRef {
            OpenSubsonicItemRef::new(
                self.backend_id.clone(),
                self.account_scope_id.clone(),
                ItemId::new(item_id).unwrap(),
            )
        }
    }

    async fn test_client(port: u16) -> TestClient {
        let profile = OpenSubsonicProfile::new(
            "Test server",
            ConfiguredPrivateOrigin::new(&format!("http://127.0.0.1:{port}/"), true).unwrap(),
            None,
        )
        .unwrap();
        let backend_id = profile.backend_id().clone();
        let account_scope_id = profile.account_scope_id().clone();
        let client = OpenSubsonicClient::connect(&profile).await.unwrap();
        let credential =
            ServerCredential::api_key(SecretString::from("sentinel-api-key".to_owned())).unwrap();
        TestClient {
            client,
            credential,
            backend_id,
            account_scope_id,
        }
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while request.len() < 32 * 1024 {
            if stream.read(&mut byte).await.unwrap() == 0 {
                break;
            }
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(request).unwrap()
    }

    async fn write_json(stream: &mut tokio::net::TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    fn request_target(request: &str) -> &str {
        request
            .lines()
            .next()
            .and_then(|line| line.split_ascii_whitespace().nth(1))
            .unwrap()
    }

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
}
