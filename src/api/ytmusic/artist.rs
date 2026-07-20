//! YouTube Music artist search, detail pages, and album-track mapping.

use anyhow::{Context, Result};
use ytmapi_rs::common::{AlbumID, ArtistChannelID, YoutubeID};
use ytmapi_rs::query::GetArtistQuery;

use super::{YtMusicApi, auth_search_degraded, transfer_api, ytdlp_search};
use crate::api::{ArtistPage, Song};
use crate::util::sanitize;

/// Rows for the artist screen's public yt-dlp top-songs fallback.
const ARTIST_FALLBACK_SONGS: usize = 15;

pub(super) async fn search_artists(api: &YtMusicApi, query: &str) -> Result<Vec<Song>> {
    if let YtMusicApi::Browser(client) = api
        && !auth_search_degraded()
    {
        match client.search_artists(query).await {
            Ok(results) if !results.is_empty() => {
                return Ok(results.into_iter().map(artist_row).collect());
            }
            Ok(_) => {}
            Err(e) => {
                let error = sanitize::sanitize_error_text(format!("{e:#}"));
                tracing::warn!(error = %error, "innertube artist search failed; trying anonymous");
            }
        }
    }
    let client = transfer_api::anonymous_ytmusic_client().await?;
    let results = client
        .search_artists(query)
        .await
        .context("anonymous YouTube Music artist search failed")?;
    Ok(results.into_iter().map(artist_row).collect())
}

pub(super) async fn artist_page(api: &YtMusicApi, channel_id: &str) -> Result<ArtistPage> {
    let mut page = 'fetched: {
        if let YtMusicApi::Browser(client) = api
            && !auth_search_degraded()
        {
            match client
                .get_artist(GetArtistQuery::new(ArtistChannelID::from_raw(channel_id)))
                .await
            {
                Ok(artist) => break 'fetched artist_page_rows(channel_id, artist),
                Err(e) => {
                    let error = sanitize::sanitize_error_text(format!("{e:#}"));
                    tracing::warn!(error = %error, "innertube artist page failed; trying anonymous");
                }
            }
        }
        let client = transfer_api::anonymous_ytmusic_client().await?;
        let artist = client
            .get_artist(GetArtistQuery::new(ArtistChannelID::from_raw(channel_id)))
            .await
            .context("fetching the YouTube Music artist page failed")?;
        artist_page_rows(channel_id, artist)
    };
    // Anonymous artist pages can omit the songs shelf. Preserve the albums-only page if
    // the public fallback fails; top songs are an enhancement, not page admission.
    if page.songs.is_empty() && !page.name.is_empty() {
        match ytdlp_search(&page.name, ARTIST_FALLBACK_SONGS).await {
            Ok(songs) => page.songs = songs,
            Err(e) => {
                let error = sanitize::sanitize_error_text(format!("{e:#}"));
                tracing::warn!(error = %error, "artist top-songs fallback search failed");
            }
        }
    }
    Ok(page)
}

/// Resolve one artist-page album browse id through the album endpoint.
pub(super) async fn album_tracks(api: &YtMusicApi, album_id: &str) -> Result<Vec<Song>> {
    if let YtMusicApi::Browser(client) = api
        && !auth_search_degraded()
    {
        match client.get_album(AlbumID::from_raw(album_id)).await {
            Ok(album) => return Ok(album_track_rows(album)),
            Err(e) => {
                let error = sanitize::sanitize_error_text(format!("{e:#}"));
                tracing::warn!(error = %error, "innertube album fetch failed; trying anonymous");
            }
        }
    }
    let client = transfer_api::anonymous_ytmusic_client().await?;
    let album = client
        .get_album(AlbumID::from_raw(album_id))
        .await
        .context("fetching the YouTube Music album failed")?;
    Ok(album_track_rows(album))
}

fn artist_row(result: ytmapi_rs::parse::SearchResultArtist) -> Song {
    artist_row_parts(
        result.artist,
        result.subscribers,
        result.browse_id.get_raw(),
    )
}

/// Plain-field seam for non-exhaustive ytmapi artist-result tests.
pub(super) fn artist_row_parts(
    name: String,
    subscribers: Option<String>,
    channel_id: &str,
) -> Song {
    Song::remote(
        format!("{}{channel_id}", crate::api::ARTIST_ID_PREFIX),
        name,
        String::new(),
        subscribers.unwrap_or_default(),
    )
}

fn artist_page_rows(channel_id: &str, artist: ytmapi_rs::parse::GetArtist) -> ArtistPage {
    let name = artist.name;
    let songs = artist
        .top_releases
        .songs
        .as_ref()
        .map(|songs| {
            songs
                .results
                .iter()
                .map(|song| {
                    let artists = song
                        .artists
                        .iter()
                        .map(|artist| artist.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Song::from_search(
                        song.video_id.get_raw(),
                        song.title.clone(),
                        if artists.is_empty() {
                            name.clone()
                        } else {
                            artists
                        },
                        song.plays.clone(),
                        Some(song.album.name.clone()),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let albums = artist
        .top_releases
        .albums
        .iter()
        .chain(artist.top_releases.singles.iter())
        .flat_map(|section| section.results.iter())
        .map(|album| {
            Song::remote(
                format!(
                    "{}{}",
                    crate::api::PLAYLIST_ID_PREFIX,
                    album.album_id.get_raw()
                ),
                album.title.clone(),
                name.clone(),
                album.year.clone(),
            )
        })
        .collect();
    ArtistPage {
        channel_id: channel_id.to_owned(),
        name,
        subscribers: artist.subscribers,
        songs,
        albums,
        songs_playlist_id: artist
            .top_releases
            .songs
            .map(|songs| songs.browse_id.get_raw().to_owned()),
    }
}

fn album_track_rows(album: ytmapi_rs::parse::GetAlbum) -> Vec<Song> {
    let artist = album
        .artists
        .iter()
        .map(|artist| artist.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    album
        .tracks
        .into_iter()
        .map(|track| {
            Song::from_search(
                track.video_id.get_raw(),
                track.title,
                artist.clone(),
                track.duration,
                Some(album.title.clone()),
            )
        })
        .collect()
}
