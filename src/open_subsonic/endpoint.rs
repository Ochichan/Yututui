//! Typed standard OpenSubsonic endpoint calls.
//!
//! All item mutations take a fully scoped identity. Query serialization remains owned by
//! `reqwest`; opaque server IDs are never interpolated into URLs.

#[path = "endpoint/playlists.rs"]
mod playlists;
#[cfg(test)]
#[path = "endpoint/test_support.rs"]
mod test_support;

use super::super::model::OpenSubsonicItemRef;
use super::super::private_store::{CredentialKind, ServerCredential};
use super::super::wire::RawChild;
use super::{MutationDeliveryError, OpenSubsonicClient, ServerError};

const MAX_SCROBBLE_TIME_UNIX_MS: u64 = i64::MAX as u64;
const MAX_TOKEN_INFO_USERNAME_BYTES: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Ping,
    Extensions,
    TokenInfo,
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
    CreatePlaylist,
    UpdatePlaylist,
    DeletePlaylist,
    CoverArt,
    Stream,
}

impl Endpoint {
    pub(super) const fn method_name(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Extensions => "getOpenSubsonicExtensions",
            Self::TokenInfo => "tokenInfo",
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
            Self::CreatePlaylist => "createPlaylist",
            Self::UpdatePlaylist => "updatePlaylist",
            Self::DeletePlaylist => "deletePlaylist",
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

    /// Resolve the exact account identity associated with an API key.
    ///
    /// This endpoint itself is authenticated only with `apiKey`; the returned username is kept as
    /// private ownership evidence and is never added to subsequent API-key requests.
    pub(crate) async fn api_key_username(
        &self,
        credential: &ServerCredential,
    ) -> Result<String, ServerError> {
        if credential.kind() != CredentialKind::ApiKey {
            return Err(ServerError::InvalidResponse);
        }
        let response = self
            .request_json(credential, Endpoint::TokenInfo, &[])
            .await?;
        let username = response
            .token_info
            .and_then(|info| info.username)
            .ok_or(ServerError::InvalidResponse)?;
        let sanitized =
            crate::api::sanitize_metadata_text(&username, MAX_TOKEN_INFO_USERNAME_BYTES);
        if username.is_empty()
            || username.len() > MAX_TOKEN_INFO_USERNAME_BYTES
            || sanitized != username
        {
            return Err(ServerError::InvalidResponse);
        }
        Ok(username)
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
#[path = "endpoint/tests.rs"]
mod tests;
