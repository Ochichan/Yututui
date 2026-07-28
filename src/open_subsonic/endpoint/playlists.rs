//! Replay-safe playlist endpoints and strict write snapshots.

use age::secrecy::ExposeSecret as _;

use super::super::super::auth::{AuthParameters, common_parameters};
use super::super::super::model::{
    AlbumId, CoverArtId, ItemId, OpenSubsonicItemRef, ServerPlaylistId,
    ServerPlaylistWriteSnapshot, ServerSong,
};
use super::super::super::private_store::ServerCredential;
use super::super::super::wire::{RawChild, RawPlaylist, RawResponse, WireError};
use super::super::{
    MAX_JSON_BYTES, MutationDeliveryError, OpenSubsonicClient, REQUEST_TIMEOUT, ServerError,
    classify_mutation_request_error, map_origin_error, map_wire_error, read_limited,
    status_error_for,
};
use super::Endpoint;

const MAX_SERVER_PLAYLIST_NAME_CHARS: usize = 300;
const MAX_SERVER_PLAYLIST_OWNER_CHARS: usize = 1_024;
const MAX_SERVER_PLAYLIST_WRITE_ENTRIES: usize = 999;

impl OpenSubsonicClient {
    pub(crate) async fn get_playlist_write_snapshot(
        &self,
        credential: &ServerCredential,
        playlist_id: &ServerPlaylistId,
    ) -> Result<ServerPlaylistWriteSnapshot, ServerError> {
        let response = self
            .request_json(
                credential,
                Endpoint::Playlist,
                &[("id", playlist_id.as_str().to_owned())],
            )
            .await?;
        let playlist = response.playlist.ok_or(ServerError::InvalidResponse)?;
        self.strict_playlist_snapshot(playlist, Some(playlist_id))
    }

    pub(crate) async fn create_playlist(
        &self,
        credential: &ServerCredential,
        name: &str,
        entries: &[OpenSubsonicItemRef],
    ) -> Result<Option<ServerPlaylistWriteSnapshot>, MutationDeliveryError> {
        let name =
            validated_playlist_name(name).map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        if entries.len() > MAX_SERVER_PLAYLIST_WRITE_ENTRIES {
            return Err(MutationDeliveryError::DefinitelyNotApplied(
                ServerError::InvalidResponse,
            ));
        }
        let mut parameters = Vec::with_capacity(entries.len() + 1);
        parameters.push(("name", name));
        for entry in entries {
            self.validate_item_scope(entry)
                .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
            parameters.push(("songId", entry.item_id().as_str().to_owned()));
        }
        let response = self
            .request_playlist_mutation(credential, Endpoint::CreatePlaylist, &parameters)
            .await?;
        response
            .playlist
            .map(|playlist| self.strict_playlist_snapshot(playlist, None))
            .transpose()
            .map_err(MutationDeliveryError::Ambiguous)
    }

    pub(crate) async fn update_playlist(
        &self,
        credential: &ServerCredential,
        snapshot: &ServerPlaylistWriteSnapshot,
        name: Option<&str>,
        entries_to_add: &[OpenSubsonicItemRef],
        indexes_to_remove: &[u32],
    ) -> Result<(), MutationDeliveryError> {
        self.update_playlist_with_access(
            credential,
            snapshot,
            name,
            entries_to_add,
            indexes_to_remove,
            PlaylistWriteAccess::VerifiedAccountOwner,
        )
        .await
    }

    /// Mutates only a playlist whose durable link is already marked `managed_by_yututui`.
    ///
    /// The actor/store must prove that link before selecting this path. The current remote
    /// snapshot must still prove that the configured account is the exact writable owner.
    pub(crate) async fn update_managed_playlist(
        &self,
        credential: &ServerCredential,
        snapshot: &ServerPlaylistWriteSnapshot,
        name: Option<&str>,
        entries_to_add: &[OpenSubsonicItemRef],
        indexes_to_remove: &[u32],
    ) -> Result<(), MutationDeliveryError> {
        self.update_playlist_with_access(
            credential,
            snapshot,
            name,
            entries_to_add,
            indexes_to_remove,
            PlaylistWriteAccess::ManagedByYututui,
        )
        .await
    }

    async fn update_playlist_with_access(
        &self,
        credential: &ServerCredential,
        snapshot: &ServerPlaylistWriteSnapshot,
        name: Option<&str>,
        entries_to_add: &[OpenSubsonicItemRef],
        indexes_to_remove: &[u32],
        access: PlaylistWriteAccess,
    ) -> Result<(), MutationDeliveryError> {
        self.validate_playlist_snapshot_scope(snapshot)
            .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        let name = name
            .map(validated_playlist_name)
            .transpose()
            .map_err(MutationDeliveryError::DefinitelyNotApplied)?
            .filter(|name| name != snapshot.name());
        for entry in entries_to_add {
            self.validate_item_scope(entry)
                .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        }
        let indexes_to_remove =
            descending_unique_indexes(indexes_to_remove, snapshot.entries().len())
                .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        let final_entry_count = snapshot
            .entries()
            .len()
            .saturating_sub(indexes_to_remove.len())
            .saturating_add(entries_to_add.len());
        if final_entry_count > MAX_SERVER_PLAYLIST_WRITE_ENTRIES {
            return Err(MutationDeliveryError::DefinitelyNotApplied(
                ServerError::InvalidResponse,
            ));
        }
        if name.is_none() && entries_to_add.is_empty() && indexes_to_remove.is_empty() {
            return Ok(());
        }
        ensure_playlist_write_allowed(snapshot, credential, access)
            .map_err(MutationDeliveryError::DefinitelyNotApplied)?;

        let mut parameters = Vec::with_capacity(
            1 + usize::from(name.is_some()) + entries_to_add.len() + indexes_to_remove.len(),
        );
        parameters.push(("playlistId", snapshot.id().as_str().to_owned()));
        if let Some(name) = name {
            parameters.push(("name", name));
        }
        for entry in entries_to_add {
            // Repeated IDs are positional occurrences. Never deduplicate them.
            parameters.push(("songIdToAdd", entry.item_id().as_str().to_owned()));
        }
        for index in indexes_to_remove {
            parameters.push(("songIndexToRemove", index.to_string()));
        }
        self.request_playlist_mutation(credential, Endpoint::UpdatePlaylist, &parameters)
            .await
            .map(drop)
    }

    pub(crate) async fn delete_playlist(
        &self,
        credential: &ServerCredential,
        snapshot: &ServerPlaylistWriteSnapshot,
    ) -> Result<(), MutationDeliveryError> {
        self.delete_playlist_with_access(
            credential,
            snapshot,
            PlaylistWriteAccess::VerifiedAccountOwner,
        )
        .await
    }

    /// Deletes only a playlist whose durable link is already marked `managed_by_yututui`.
    ///
    /// The actor/store must prove that link before selecting this path. The current remote
    /// snapshot must still prove that the configured account is the exact writable owner.
    pub(crate) async fn delete_managed_playlist(
        &self,
        credential: &ServerCredential,
        snapshot: &ServerPlaylistWriteSnapshot,
    ) -> Result<(), MutationDeliveryError> {
        self.delete_playlist_with_access(
            credential,
            snapshot,
            PlaylistWriteAccess::ManagedByYututui,
        )
        .await
    }

    async fn delete_playlist_with_access(
        &self,
        credential: &ServerCredential,
        snapshot: &ServerPlaylistWriteSnapshot,
        access: PlaylistWriteAccess,
    ) -> Result<(), MutationDeliveryError> {
        self.validate_playlist_snapshot_scope(snapshot)
            .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        ensure_playlist_write_allowed(snapshot, credential, access)
            .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        self.request_playlist_mutation(
            credential,
            Endpoint::DeletePlaylist,
            &[("id", snapshot.id().as_str().to_owned())],
        )
        .await
        .map(drop)
    }

    async fn request_playlist_mutation(
        &self,
        credential: &ServerCredential,
        endpoint: Endpoint,
        parameters: &[(&str, String)],
    ) -> Result<RawResponse, MutationDeliveryError> {
        debug_assert!(matches!(
            endpoint,
            Endpoint::CreatePlaylist | Endpoint::UpdatePlaylist | Endpoint::DeletePlaylist
        ));
        let target = self
            .transport
            .origin()
            .endpoint(endpoint.method_name())
            .map_err(map_origin_error)
            .map_err(MutationDeliveryError::DefinitelyNotApplied)?;
        let mut request = self
            .transport
            .client()
            .request(reqwest::Method::GET, target)
            .timeout(REQUEST_TIMEOUT)
            .query(&common_parameters());
        let auth = AuthParameters::fresh(credential)
            .map_err(|_| MutationDeliveryError::DefinitelyNotApplied(ServerError::Offline))?;
        request = request.query(auth.fields()).query(parameters);
        let response = request
            .send()
            .await
            .map_err(classify_mutation_request_error)?;

        // Playlist mutations are replay-unsafe (especially repeated adds and index removals).
        // A server may have committed before returning a redirect, so no redirect is followed.
        if response.status().is_redirection() {
            return Err(MutationDeliveryError::Ambiguous(
                ServerError::OriginRejected,
            ));
        }
        if !response.status().is_success() {
            let error = status_error_for(endpoint, &response);
            return if response.status().is_server_error() {
                Err(MutationDeliveryError::Ambiguous(error))
            } else {
                Err(MutationDeliveryError::DefinitelyNotApplied(error))
            };
        }
        let bytes = read_limited(response, MAX_JSON_BYTES)
            .await
            .map_err(MutationDeliveryError::Ambiguous)?;
        match super::super::super::wire::decode(&bytes) {
            Ok(response) => Ok(response),
            Err(WireError::ApiFailure(error)) => Err(MutationDeliveryError::DefinitelyNotApplied(
                map_wire_error(WireError::ApiFailure(error)),
            )),
            Err(error) => Err(MutationDeliveryError::Ambiguous(map_wire_error(error))),
        }
    }

    fn strict_playlist_snapshot(
        &self,
        raw: RawPlaylist,
        expected_id: Option<&ServerPlaylistId>,
    ) -> Result<ServerPlaylistWriteSnapshot, ServerError> {
        let id = ServerPlaylistId::new(raw.id.ok_or(ServerError::InvalidResponse)?)
            .map_err(|_| ServerError::InvalidResponse)?;
        if expected_id.is_some_and(|expected| expected != &id) {
            return Err(ServerError::InvalidResponse);
        }
        let name = validated_playlist_name(&raw.name.ok_or(ServerError::InvalidResponse)?)?;
        let owner = validated_playlist_owner(raw.owner)?;
        if raw.entry.len() > MAX_SERVER_PLAYLIST_WRITE_ENTRIES
            || raw
                .song_count
                .is_some_and(|count| count != raw.entry.len() as u64)
        {
            return Err(ServerError::InvalidResponse);
        }
        let entries = raw
            .entry
            .into_iter()
            .map(|entry| self.strict_playlist_song(entry))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ServerPlaylistWriteSnapshot::new(
            self.backend_id.clone(),
            self.account_scope_id.clone(),
            id,
            name,
            owner,
            raw.readonly,
            entries,
        ))
    }

    fn strict_playlist_song(&self, raw: RawChild) -> Result<ServerSong, ServerError> {
        if raw.is_dir == Some(true)
            || raw
                .media_type
                .as_deref()
                .is_some_and(|media_type| media_type != "music")
        {
            return Err(ServerError::InvalidResponse);
        }
        let item_id = ItemId::new(raw.id.ok_or(ServerError::InvalidResponse)?)
            .map_err(|_| ServerError::InvalidResponse)?;
        let title = raw
            .title
            .map(|value| crate::api::sanitize_title(&value))
            .filter(|value| !value.is_empty())
            .ok_or(ServerError::InvalidResponse)?;
        let artist = raw
            .artist
            .map(|value| crate::api::sanitize_artist(&value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Unknown artist".to_owned());
        let artists = raw
            .artists
            .into_iter()
            .filter_map(|artist| artist.name)
            .map(|name| crate::api::sanitize_artist(&name))
            .filter(|name| !name.is_empty())
            .collect();
        let album_id = raw
            .album_id
            .map(AlbumId::new)
            .transpose()
            .map_err(|_| ServerError::InvalidResponse)?;
        let cover_art_id = raw
            .cover_art
            .map(CoverArtId::new)
            .transpose()
            .map_err(|_| ServerError::InvalidResponse)?;
        Ok(ServerSong {
            item: OpenSubsonicItemRef::new(
                self.backend_id.clone(),
                self.account_scope_id.clone(),
                item_id,
            ),
            title,
            artist,
            artists,
            album: sanitize_optional(raw.album, crate::api::MAX_ALBUM_CHARS),
            album_id,
            album_artist: None,
            duration_secs: raw.duration.and_then(|value| u32::try_from(value).ok()),
            track_number: raw.track.and_then(|value| u32::try_from(value).ok()),
            disc_number: raw.disc_number.and_then(|value| u32::try_from(value).ok()),
            year: raw.year.and_then(|value| u32::try_from(value).ok()),
            cover_art_id,
            content_type: sanitize_optional(raw.content_type, 256),
            suffix: sanitize_optional(raw.suffix, 256),
            starred: raw.starred.is_some(),
            user_rating: raw.user_rating,
            play_count: raw.play_count,
            played_at: sanitize_optional(raw.played, 64),
        })
    }

    fn validate_playlist_snapshot_scope(
        &self,
        snapshot: &ServerPlaylistWriteSnapshot,
    ) -> Result<(), ServerError> {
        if snapshot.backend_id() != &self.backend_id
            || snapshot.account_scope_id() != &self.account_scope_id
        {
            return Err(ServerError::WrongAccountScope);
        }
        Ok(())
    }
}

fn validated_playlist_name(name: &str) -> Result<String, ServerError> {
    let sanitized = crate::api::sanitize_metadata_text(name, MAX_SERVER_PLAYLIST_NAME_CHARS);
    if sanitized.is_empty() || sanitized != name {
        return Err(ServerError::InvalidResponse);
    }
    Ok(sanitized)
}

fn validated_playlist_owner(owner: Option<String>) -> Result<Option<String>, ServerError> {
    let Some(owner) = owner else {
        return Ok(None);
    };
    if owner.is_empty() {
        return Ok(None);
    }
    let sanitized = crate::api::sanitize_metadata_text(&owner, MAX_SERVER_PLAYLIST_OWNER_CHARS);
    if sanitized != owner {
        return Err(ServerError::InvalidResponse);
    }
    Ok(Some(owner))
}

fn sanitize_optional(value: Option<String>, max_chars: usize) -> Option<String> {
    value
        .map(|value| crate::api::sanitize_metadata_text(&value, max_chars))
        .filter(|value| !value.is_empty())
}

fn descending_unique_indexes(indexes: &[u32], entry_count: usize) -> Result<Vec<u32>, ServerError> {
    let mut indexes = indexes.to_vec();
    if indexes
        .iter()
        .any(|index| usize::try_from(*index).map_or(true, |index| index >= entry_count))
    {
        return Err(ServerError::InvalidResponse);
    }
    indexes.sort_unstable_by(|left, right| right.cmp(left));
    indexes.dedup();
    Ok(indexes)
}

#[derive(Clone, Copy)]
enum PlaylistWriteAccess {
    VerifiedAccountOwner,
    ManagedByYututui,
}

fn ensure_playlist_write_allowed(
    snapshot: &ServerPlaylistWriteSnapshot,
    credential: &ServerCredential,
    _access: PlaylistWriteAccess,
) -> Result<(), ServerError> {
    // A durable managed link proves which remote object the user selected; it cannot prove that
    // the server still grants this account write access. Missing access metadata therefore fails
    // closed on both paths.
    let permitted = snapshot.read_only() == Some(false)
        && credential
            .username()
            .is_some_and(|username| snapshot.owner() == Some(username.expose_secret()));
    if !permitted {
        return Err(ServerError::PermissionDenied);
    }
    Ok(())
}

#[cfg(test)]
#[path = "playlists/tests.rs"]
mod tests;
