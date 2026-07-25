//! Search and Server Library adapters over the bounded client.

use super::client::{BinaryPayload, Endpoint, OpenSubsonicClient, ServerError};
use super::model::{
    AccountScopeId, AlbumId, ArtistId, BackendId, CoverArtId, OpenSubsonicItemRef, ServerAlbum,
    ServerArtist, ServerLibraryDetail, ServerLibraryPage, ServerLibraryRow, ServerLibrarySection,
    ServerPlaylist, ServerPlaylistId, ServerPlaylistSummary, ServerSong,
};
use super::private_store::ServerCredential;
use super::wire::{
    RawAlbum, RawAlbumWithSongs, RawArtist, RawArtistWithAlbums, RawChild, RawPlaylist,
    RawPlaylistSummary,
};

pub const MAX_PAGE_SIZE: u32 = 200;
const MAX_QUERY_CHARS: usize = 300;
const MAX_NESTED_ROWS: usize = 20_000;

pub struct OpenSubsonicCatalog<'a> {
    client: &'a OpenSubsonicClient,
    credential: &'a ServerCredential,
    backend_id: &'a BackendId,
    account_scope_id: &'a AccountScopeId,
}

impl<'a> OpenSubsonicCatalog<'a> {
    pub fn new(
        client: &'a OpenSubsonicClient,
        credential: &'a ServerCredential,
        backend_id: &'a BackendId,
        account_scope_id: &'a AccountScopeId,
    ) -> Self {
        Self {
            client,
            credential,
            backend_id,
            account_scope_id,
        }
    }

    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<ServerSong>, ServerError> {
        let query = crate::api::sanitize_metadata_text(query, MAX_QUERY_CHARS);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let limit = bounded_limit(limit);
        let parameters = vec![
            ("query", query),
            ("artistCount", "0".to_owned()),
            ("albumCount", "0".to_owned()),
            ("songCount", limit.to_string()),
            ("songOffset", "0".to_owned()),
        ];
        let response = self
            .client
            .request_json(self.credential, Endpoint::Search3, &parameters)
            .await?;
        let result = response
            .search_result3
            .ok_or(ServerError::InvalidResponse)?;
        Ok(result
            .song
            .into_iter()
            .take(limit as usize)
            .filter_map(|song| self.song(song))
            .collect())
    }

    pub async fn library_page(
        &self,
        section: ServerLibrarySection,
        offset: u32,
        limit: u32,
    ) -> Result<ServerLibraryPage, ServerError> {
        let limit = bounded_limit(limit);
        let result = match section {
            ServerLibrarySection::RecentlyPlayed => self.album_page("recent", offset, limit).await,
            ServerLibrarySection::Albums => {
                self.album_page("alphabeticalByName", offset, limit).await
            }
            ServerLibrarySection::Artists => self.artist_page(offset, limit).await,
            ServerLibrarySection::Songs => self.song_page(offset, limit).await,
            ServerLibrarySection::Playlists => self.playlist_page(offset, limit).await,
        };
        let (rows, next_offset) = match result {
            Ok(page) => page,
            Err(ServerError::UnsupportedFeature) => {
                return Ok(ServerLibraryPage {
                    section,
                    rows: Vec::new(),
                    next_offset: None,
                    warning: Some(super::model::LibraryWarning::FeatureUnsupported),
                });
            }
            Err(error) => return Err(error),
        };
        Ok(ServerLibraryPage {
            section,
            rows,
            next_offset,
            warning: None,
        })
    }

    pub async fn album_detail(&self, id: &AlbumId) -> Result<ServerLibraryDetail, ServerError> {
        let response = self
            .client
            .request_json(
                self.credential,
                Endpoint::Album,
                &[("id", id.as_str().to_owned())],
            )
            .await?;
        let raw = response.album.ok_or(ServerError::InvalidResponse)?;
        let album = album_with_songs_summary(&raw).ok_or(ServerError::InvalidResponse)?;
        let songs = raw
            .song
            .into_iter()
            .take(MAX_NESTED_ROWS)
            .filter_map(|song| self.song(song))
            .collect();
        Ok(ServerLibraryDetail::AlbumSongs { album, songs })
    }

    pub async fn artist_detail(&self, id: &ArtistId) -> Result<ServerLibraryDetail, ServerError> {
        let response = self
            .client
            .request_json(
                self.credential,
                Endpoint::Artist,
                &[("id", id.as_str().to_owned())],
            )
            .await?;
        let raw = response.artist.ok_or(ServerError::InvalidResponse)?;
        let artist = artist_with_albums_summary(&raw).ok_or(ServerError::InvalidResponse)?;
        let albums = raw
            .album
            .into_iter()
            .take(MAX_NESTED_ROWS)
            .filter_map(album)
            .collect();
        Ok(ServerLibraryDetail::ArtistAlbums { artist, albums })
    }

    pub async fn playlist_detail(
        &self,
        id: &ServerPlaylistId,
    ) -> Result<ServerLibraryDetail, ServerError> {
        let response = self
            .client
            .request_json(
                self.credential,
                Endpoint::Playlist,
                &[("id", id.as_str().to_owned())],
            )
            .await?;
        let raw = response.playlist.ok_or(ServerError::InvalidResponse)?;
        let summary = playlist(&raw).ok_or(ServerError::InvalidResponse)?;
        let entries = raw
            .entry
            .into_iter()
            .take(MAX_NESTED_ROWS)
            .filter_map(|song| self.song(song))
            .collect();
        Ok(ServerLibraryDetail::PlaylistEntries(ServerPlaylist {
            summary,
            entries,
        }))
    }

    /// Artwork is independently optional. Unsupported or unsafe images return `Ok(None)`.
    pub async fn cover_art(&self, id: &CoverArtId) -> Result<Option<BinaryPayload>, ServerError> {
        let payload = match self.client.get_cover_art(self.credential, id).await {
            Ok(payload) => payload,
            Err(
                ServerError::UnsupportedFeature
                | ServerError::NotFound
                | ServerError::InvalidResponse,
            ) => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(content_type) = payload
            .content_type
            .as_deref()
            .filter(|content_type| supported_image_type(content_type))
            .map(str::to_owned)
        else {
            return Ok(None);
        };
        let bytes = payload.bytes;
        let bytes = tokio::task::spawn_blocking(move || {
            crate::util::art::decode_untrusted(&bytes).map(|_| bytes)
        })
        .await
        .map_err(|_| ServerError::InvalidResponse)?;
        Ok(bytes.map(|bytes| BinaryPayload {
            bytes,
            content_type: Some(content_type),
        }))
    }

    async fn album_page(
        &self,
        list_type: &str,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<ServerLibraryRow>, Option<u32>), ServerError> {
        let response = self
            .client
            .request_json(
                self.credential,
                Endpoint::AlbumList2,
                &[
                    ("type", list_type.to_owned()),
                    ("size", limit.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        let albums = response
            .album_list2
            .ok_or(ServerError::InvalidResponse)?
            .album;
        let raw_count = albums.len();
        let rows = albums
            .into_iter()
            .take(limit as usize)
            .filter_map(album)
            .map(ServerLibraryRow::Album)
            .collect();
        Ok((rows, next_network_offset(offset, limit, raw_count)))
    }

    async fn song_page(
        &self,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<ServerLibraryRow>, Option<u32>), ServerError> {
        let response = self
            .client
            .request_json(
                self.credential,
                Endpoint::Search3,
                &[
                    ("query", String::new()),
                    ("artistCount", "0".to_owned()),
                    ("albumCount", "0".to_owned()),
                    ("songCount", limit.to_string()),
                    ("songOffset", offset.to_string()),
                ],
            )
            .await?;
        let songs = response
            .search_result3
            .ok_or(ServerError::InvalidResponse)?
            .song;
        let raw_count = songs.len();
        let rows = songs
            .into_iter()
            .take(limit as usize)
            .filter_map(|song| self.song(song))
            .map(ServerLibraryRow::Song)
            .collect();
        Ok((rows, next_network_offset(offset, limit, raw_count)))
    }

    async fn artist_page(
        &self,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<ServerLibraryRow>, Option<u32>), ServerError> {
        let response = self
            .client
            .request_json(self.credential, Endpoint::Artists, &[])
            .await?;
        let artists = response
            .artists
            .ok_or(ServerError::InvalidResponse)?
            .index
            .into_iter()
            .flat_map(|index| index.artist)
            .take(MAX_NESTED_ROWS)
            .collect::<Vec<_>>();
        let (slice, next) = local_page(artists, offset, limit);
        Ok((
            slice
                .into_iter()
                .filter_map(artist)
                .map(ServerLibraryRow::Artist)
                .collect(),
            next,
        ))
    }

    async fn playlist_page(
        &self,
        offset: u32,
        limit: u32,
    ) -> Result<(Vec<ServerLibraryRow>, Option<u32>), ServerError> {
        let response = self
            .client
            .request_json(self.credential, Endpoint::Playlists, &[])
            .await?;
        let playlists = response
            .playlists
            .ok_or(ServerError::InvalidResponse)?
            .playlist
            .into_iter()
            .take(MAX_NESTED_ROWS)
            .collect();
        let (slice, next) = local_page(playlists, offset, limit);
        Ok((
            slice
                .into_iter()
                .filter_map(playlist_summary)
                .map(ServerLibraryRow::Playlist)
                .collect(),
            next,
        ))
    }

    fn song(&self, raw: RawChild) -> Option<ServerSong> {
        if raw.is_dir == Some(true)
            || raw
                .media_type
                .as_deref()
                .is_some_and(|media_type| media_type != "music")
        {
            return None;
        }
        let item_id = super::model::ItemId::new(raw.id?).ok()?;
        let title = nonempty_title(raw.title?)?;
        let artist_name = raw
            .artist
            .map(|value| crate::api::sanitize_artist(&value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Unknown artist".to_owned());
        let artists = raw
            .artists
            .into_iter()
            .take(32)
            .filter_map(|artist| artist.name)
            .map(|name| crate::api::sanitize_artist(&name))
            .filter(|name| !name.is_empty())
            .collect();
        Some(ServerSong {
            item: OpenSubsonicItemRef::new(
                self.backend_id.clone(),
                self.account_scope_id.clone(),
                item_id,
            ),
            title,
            artist: artist_name,
            artists,
            album: raw
                .album
                .map(|value| crate::api::sanitize_album(&value))
                .filter(|value| !value.is_empty()),
            album_id: raw.album_id.and_then(|id| AlbumId::new(id).ok()),
            album_artist: None,
            duration_secs: raw.duration.and_then(to_u32),
            track_number: raw.track.and_then(to_u32),
            disc_number: raw.disc_number.and_then(to_u32),
            year: raw.year.and_then(to_u32),
            cover_art_id: raw.cover_art.and_then(|id| CoverArtId::new(id).ok()),
            content_type: raw.content_type.and_then(safe_short_value),
            suffix: raw.suffix.and_then(safe_short_value),
            starred: raw.starred.is_some(),
            user_rating: raw
                .user_rating
                .and_then(|rating| u8::try_from(rating).ok())
                .filter(|rating| *rating <= 5),
        })
    }
}

fn album(raw: RawAlbum) -> Option<ServerAlbum> {
    Some(ServerAlbum {
        id: AlbumId::new(raw.id?).ok()?,
        name: nonempty_album(raw.name.or(raw.title)?)?,
        artist: raw
            .artist
            .map(|value| crate::api::sanitize_artist(&value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Unknown artist".to_owned()),
        artist_id: raw.artist_id.and_then(|id| ArtistId::new(id).ok()),
        song_count: raw.song_count.and_then(to_u32),
        duration_secs: raw.duration.and_then(to_u32),
        year: raw.year.and_then(to_u32),
        cover_art_id: raw.cover_art.and_then(|id| CoverArtId::new(id).ok()),
    })
}

fn artist(raw: RawArtist) -> Option<ServerArtist> {
    Some(ServerArtist {
        id: ArtistId::new(raw.id?).ok()?,
        name: raw
            .name
            .map(|value| crate::api::sanitize_artist(&value))
            .filter(|value| !value.is_empty())?,
        album_count: raw.album_count.and_then(to_u32),
        cover_art_id: raw.cover_art.and_then(|id| CoverArtId::new(id).ok()),
    })
}

fn playlist_summary(raw: RawPlaylistSummary) -> Option<ServerPlaylistSummary> {
    playlist_fields(
        raw.id,
        raw.name,
        raw.owner,
        raw.song_count,
        raw.duration,
        raw.public,
        raw.cover_art,
    )
}

fn playlist(raw: &RawPlaylist) -> Option<ServerPlaylistSummary> {
    playlist_fields(
        raw.id.clone(),
        raw.name.clone(),
        raw.owner.clone(),
        raw.song_count,
        raw.duration,
        raw.public,
        raw.cover_art.clone(),
    )
}

fn playlist_fields(
    id: Option<String>,
    name: Option<String>,
    owner: Option<String>,
    song_count: Option<u64>,
    duration: Option<u64>,
    public: Option<bool>,
    cover_art: Option<String>,
) -> Option<ServerPlaylistSummary> {
    Some(ServerPlaylistSummary {
        id: ServerPlaylistId::new(id?).ok()?,
        name: name
            .map(|value| crate::api::sanitize_metadata_text(&value, 300))
            .filter(|value| !value.is_empty())?,
        owner: owner
            .map(|value| crate::api::sanitize_metadata_text(&value, 200))
            .filter(|value| !value.is_empty()),
        song_count: song_count.and_then(to_u32),
        duration_secs: duration.and_then(to_u32),
        public,
        cover_art_id: cover_art.and_then(|id| CoverArtId::new(id).ok()),
    })
}

fn album_with_songs_summary(raw: &RawAlbumWithSongs) -> Option<ServerAlbum> {
    Some(ServerAlbum {
        id: AlbumId::new(raw.id.clone()?).ok()?,
        name: nonempty_album(raw.name.clone()?)?,
        artist: raw
            .artist
            .as_deref()
            .map(crate::api::sanitize_artist)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "Unknown artist".to_owned()),
        artist_id: raw
            .artist_id
            .as_ref()
            .and_then(|id| ArtistId::new(id.clone()).ok()),
        song_count: raw.song_count.and_then(to_u32),
        duration_secs: raw.duration.and_then(to_u32),
        year: raw.year.and_then(to_u32),
        cover_art_id: raw
            .cover_art
            .as_ref()
            .and_then(|id| CoverArtId::new(id.clone()).ok()),
    })
}

fn artist_with_albums_summary(raw: &RawArtistWithAlbums) -> Option<ServerArtist> {
    Some(ServerArtist {
        id: ArtistId::new(raw.id.clone()?).ok()?,
        name: raw
            .name
            .as_deref()
            .map(crate::api::sanitize_artist)
            .filter(|value| !value.is_empty())?,
        album_count: raw.album_count.and_then(to_u32),
        cover_art_id: raw
            .cover_art
            .as_ref()
            .and_then(|id| CoverArtId::new(id.clone()).ok()),
    })
}

fn bounded_limit(limit: u32) -> u32 {
    limit.clamp(1, MAX_PAGE_SIZE)
}

fn next_network_offset(offset: u32, limit: u32, raw_count: usize) -> Option<u32> {
    (raw_count >= limit as usize)
        .then(|| offset.checked_add(limit))
        .flatten()
}

fn local_page<T>(items: Vec<T>, offset: u32, limit: u32) -> (Vec<T>, Option<u32>) {
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(items.len());
    let end = start.saturating_add(limit as usize).min(items.len());
    let has_more = end < items.len();
    let page = items.into_iter().skip(start).take(end - start).collect();
    let next = has_more.then(|| offset.saturating_add(limit));
    (page, next)
}

fn nonempty_title(value: String) -> Option<String> {
    let value = crate::api::sanitize_title(&value);
    (!value.is_empty()).then_some(value)
}

fn nonempty_album(value: String) -> Option<String> {
    let value = crate::api::sanitize_album(&value);
    (!value.is_empty()).then_some(value)
}

fn safe_short_value(value: String) -> Option<String> {
    let value = crate::api::sanitize_metadata_text(&value, 256);
    (!value.is_empty()).then_some(value)
}

fn to_u32(value: u64) -> Option<u32> {
    u32::try_from(value).ok()
}

fn supported_image_type(content_type: &str) -> bool {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        media_type.as_str(),
        "image/jpeg" | "image/png" | "image/webp"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_allowlist_excludes_active_or_unknown_content() {
        assert!(supported_image_type("image/png"));
        assert!(supported_image_type("image/jpeg; charset=binary"));
        assert!(!supported_image_type("image/svg+xml"));
        assert!(!supported_image_type("text/html"));
    }

    #[test]
    fn local_pagination_is_bounded_and_progresses() {
        let (page, next) = local_page((0..9).collect::<Vec<_>>(), 3, 4);
        assert_eq!(page, vec![3, 4, 5, 6]);
        assert_eq!(next, Some(7));
    }

    #[test]
    fn duplicate_playlist_positions_are_not_deduplicated() {
        let raw = RawPlaylist {
            id: Some("playlist".to_owned()),
            name: Some("Playlist".to_owned()),
            entry: vec![
                RawChild {
                    id: Some("same-song".to_owned()),
                    title: Some("Song".to_owned()),
                    ..RawChild::default()
                },
                RawChild {
                    id: Some("same-song".to_owned()),
                    title: Some("Song".to_owned()),
                    ..RawChild::default()
                },
            ],
            ..RawPlaylist::default()
        };
        assert_eq!(raw.entry.len(), 2);
    }
}
