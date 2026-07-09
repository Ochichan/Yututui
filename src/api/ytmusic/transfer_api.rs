use anyhow::{Context, Result, bail};
use futures::StreamExt;
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::BrowserToken;
use ytmapi_rs::common::{AlbumID, YoutubeID};
use ytmapi_rs::query::SearchQuery;
use ytmapi_rs::query::search::{FilteredSearch, SongsFilter};

use super::{
    SEARCH_RESULT_LIMIT, YtMusicApi, auth_search_degraded, mark_auth_search_degraded, ytdlp_search,
};
use crate::api::Song;
use crate::search_source::{SearchConfig, SearchSource};
use crate::util::sanitize;

#[derive(Debug, Clone)]
pub struct TransferAlbumCandidate {
    pub album_id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<String>,
    pub album_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TransferAlbum {
    pub album_id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<String>,
    pub tracks: Vec<TransferAlbumTrack>,
}

#[derive(Debug, Clone)]
pub struct TransferAlbumTrack {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track_number: Option<u32>,
    pub duration: String,
    pub duration_secs: Option<u32>,
}

impl YtMusicApi {
    pub async fn search_transfer_catalog(
        &self,
        query: &str,
        config: &SearchConfig,
    ) -> Result<(Vec<Song>, bool)> {
        if !config.is_enabled(SearchSource::Youtube) {
            bail!(
                "{} is disabled in Settings → General",
                SearchSource::Youtube.label()
            );
        }
        if auth_search_degraded() {
            return Ok((Vec::new(), true));
        }
        let Self::Browser(client) = self else {
            return Ok((Vec::new(), false));
        };
        let songs = match search_catalog_songs(client, query).await {
            Ok(songs) => songs,
            Err(error) => {
                mark_auth_search_degraded();
                tracing::warn!(
                    error = %sanitize::sanitize_error_text(format!("{error:#}")),
                    "authenticated transfer catalog search failed"
                );
                return Ok((Vec::new(), true));
            }
        };
        Ok((songs, false))
    }

    pub async fn search_transfer_video(
        &self,
        query: &str,
        config: &SearchConfig,
    ) -> Result<Vec<Song>> {
        if !config.is_enabled(SearchSource::Youtube) {
            bail!(
                "{} is disabled in Settings → General",
                SearchSource::Youtube.label()
            );
        }
        ytdlp_search(query, SEARCH_RESULT_LIMIT).await
    }

    pub async fn search_transfer_albums(
        &self,
        query: &str,
        config: &SearchConfig,
    ) -> Result<Vec<TransferAlbumCandidate>> {
        if !config.is_enabled(SearchSource::Youtube) {
            bail!(
                "{} is disabled in Settings → General",
                SearchSource::Youtube.label()
            );
        }
        if auth_search_degraded() {
            return Ok(Vec::new());
        }
        let Self::Browser(client) = self else {
            return Ok(Vec::new());
        };
        let albums = match client.search_albums(query).await {
            Ok(albums) => albums,
            Err(error) => {
                mark_auth_search_degraded();
                tracing::warn!(
                    error = %sanitize::sanitize_error_text(format!("{error:#}")),
                    "authenticated transfer album search failed"
                );
                return Ok(Vec::new());
            }
        };
        Ok(albums
            .into_iter()
            .take(10)
            .map(|album| TransferAlbumCandidate {
                album_id: album.album_id.get_raw().to_owned(),
                title: album.title,
                artist: album.artist,
                year: Some(album.year).filter(|year| !year.trim().is_empty()),
                album_type: Some(format!("{:?}", album.album_type).to_ascii_lowercase()),
            })
            .collect())
    }

    pub async fn transfer_album_tracks(&self, album_id: &str) -> Result<Option<TransferAlbum>> {
        if auth_search_degraded() {
            return Ok(None);
        }
        let Self::Browser(client) = self else {
            return Ok(None);
        };
        let album = client
            .get_album(AlbumID::from_raw(album_id))
            .await
            .context("fetching YouTube Music album tracks failed")?;
        let artist = album
            .artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let title = album.title;
        let tracks = album
            .tracks
            .into_iter()
            .map(|track| {
                let duration_secs =
                    crate::streaming::candidate::parse_duration_secs(&track.duration);
                TransferAlbumTrack {
                    video_id: track.video_id.get_raw().to_owned(),
                    title: track.title,
                    artist: artist.clone(),
                    album: title.clone(),
                    track_number: u32::try_from(track.track_no).ok(),
                    duration: track.duration,
                    duration_secs,
                }
            })
            .collect();
        Ok(Some(TransferAlbum {
            album_id: album_id.to_owned(),
            title,
            artist,
            year: Some(album.year).filter(|year| !year.trim().is_empty()),
            tracks,
        }))
    }
}

pub(super) async fn search_catalog_songs(
    client: &YtMusic<BrowserToken>,
    query: &str,
) -> Result<Vec<Song>> {
    let q: SearchQuery<FilteredSearch<SongsFilter>> = query.into();
    let mut pages = std::pin::pin!(client.stream(&q));
    let mut songs = Vec::new();
    while songs.len() < SEARCH_RESULT_LIMIT
        && let Some(page) = pages.next().await
    {
        let page = match page {
            Ok(page) => page,
            Err(e) if songs.is_empty() => return Err(e.into()),
            Err(_) => break,
        };
        for s in page {
            songs.push(Song::from_search(
                s.video_id.get_raw(),
                s.title,
                s.artist,
                s.duration,
                s.album.map(|a| a.name),
            ));
            if songs.len() >= SEARCH_RESULT_LIMIT {
                break;
            }
        }
    }
    Ok(songs)
}
