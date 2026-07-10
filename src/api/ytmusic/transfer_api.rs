use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::noauth::NoAuthToken;
use ytmapi_rs::auth::{AuthToken, BrowserToken};
use ytmapi_rs::common::{AlbumID, YoutubeID};
use ytmapi_rs::parse::{SearchResultAlbum, SearchResultSong, SearchResultVideo, SearchResults};
use ytmapi_rs::query::SearchQuery;
use ytmapi_rs::query::search::{
    AlbumsFilter, BasicSearch, FilteredSearch, SongsFilter, VideosFilter,
};

use super::{
    SEARCH_RESULT_LIMIT, YtMusicApi, auth_search_degraded, mark_auth_search_degraded,
    mark_auth_search_healthy, ytdlp_search,
};
use crate::api::Song;
use crate::search_source::{SearchConfig, SearchSource};
use crate::util::sanitize;

/// The video catalog is a best-effort quality stage, so it gets a deliberately short
/// budget before transfer matching falls through to public yt-dlp search.
const TRANSFER_VIDEO_SEARCH_TIMEOUT: Duration = Duration::from_secs(6);
/// Authenticated song lookup is an optional high-quality source during transfer matching.
/// Keep it bounded so a stalled first-party request cannot consume the whole per-track
/// matching budget; timeout follows the same breaker path as other hard catalog failures.
const TRANSFER_SONG_SEARCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Album prefill is an optimization, not a prerequisite for per-track matching. Keep both
/// first-party album calls bounded independently so an unresponsive catalog cannot consume
/// the import's per-track progress indefinitely.
const TRANSFER_ALBUM_SEARCH_TIMEOUT: Duration = Duration::from_secs(8);
const TRANSFER_ALBUM_DETAIL_TIMEOUT: Duration = Duration::from_secs(8);
const TRANSFER_VIDEO_DEGRADE_COOLDOWN: Duration = Duration::from_secs(120);
const TRANSFER_VIDEO_FAILURES_BEFORE_DEGRADE: u8 = 2;
/// Transfer ranking only needs a bounded finalist pool; the interactive Search screen keeps
/// its wider 50-row result set through `SEARCH_RESULT_LIMIT`.
const TRANSFER_YTM_RESULT_LIMIT: usize = 20;
const TRANSFER_PUBLIC_RESULT_LIMIT: usize = 15;

/// ytmapi-rs 0.3.2 treats several legitimate empty filtered-search layouts as parser errors.
/// Keep this deliberately exact: unrelated parser, authentication, and transport errors must
/// still trip the appropriate provider breaker instead of being hidden by the broad fallback.
const FILTERED_SEARCH_MISSING_MUSIC_SHELF: &str = "Expected /contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents to contain a /musicShelfRenderer/contents";

fn filtered_search_missing_music_shelf(error: &str) -> bool {
    error == FILTERED_SEARCH_MISSING_MUSIC_SHELF
}

/// Reuse the anonymous handle: constructing it fetches YTM visitor configuration and the
/// underlying HTTP client owns a connection pool. Failed construction is deliberately not
/// cached; the video cooldown below prevents a hot retry loop while still allowing recovery.
static ANONYMOUS_YTM_CLIENT: tokio::sync::OnceCell<YtMusic<NoAuthToken>> =
    tokio::sync::OnceCell::const_new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransferVideoAuthMode {
    Browser,
    Anonymous,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TransferVideoHealth {
    consecutive_failures: u8,
    degraded_until: Option<Instant>,
}

#[derive(Debug, Default)]
struct TransferVideoCooldowns {
    browser: TransferVideoHealth,
    anonymous: TransferVideoHealth,
}

impl TransferVideoCooldowns {
    fn slot_mut(&mut self, mode: TransferVideoAuthMode) -> &mut TransferVideoHealth {
        match mode {
            TransferVideoAuthMode::Browser => &mut self.browser,
            TransferVideoAuthMode::Anonymous => &mut self.anonymous,
        }
    }

    fn degraded_at(&mut self, mode: TransferVideoAuthMode, now: Instant) -> bool {
        let slot = self.slot_mut(mode);
        match slot.degraded_until {
            Some(until) if now < until => true,
            Some(_) => {
                *slot = TransferVideoHealth::default();
                false
            }
            None => false,
        }
    }

    fn record_failure_at(&mut self, mode: TransferVideoAuthMode, now: Instant) {
        if self.degraded_at(mode, now) {
            return;
        }
        let slot = self.slot_mut(mode);
        slot.consecutive_failures = slot.consecutive_failures.saturating_add(1);
        if slot.consecutive_failures >= TRANSFER_VIDEO_FAILURES_BEFORE_DEGRADE {
            slot.degraded_until = Some(
                now.checked_add(TRANSFER_VIDEO_DEGRADE_COOLDOWN)
                    .unwrap_or(now),
            );
        }
    }

    fn record_success(&mut self, mode: TransferVideoAuthMode) {
        *self.slot_mut(mode) = TransferVideoHealth::default();
    }
}

static TRANSFER_VIDEO_COOLDOWNS: Mutex<TransferVideoCooldowns> =
    Mutex::new(TransferVideoCooldowns {
        browser: TransferVideoHealth {
            consecutive_failures: 0,
            degraded_until: None,
        },
        anonymous: TransferVideoHealth {
            consecutive_failures: 0,
            degraded_until: None,
        },
    });

fn transfer_video_auth_mode(api: &YtMusicApi) -> TransferVideoAuthMode {
    match api {
        YtMusicApi::Browser(_) => TransferVideoAuthMode::Browser,
        YtMusicApi::Anonymous => TransferVideoAuthMode::Anonymous,
    }
}

fn transfer_video_search_degraded(mode: TransferVideoAuthMode) -> bool {
    TRANSFER_VIDEO_COOLDOWNS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .degraded_at(mode, Instant::now())
}

fn record_transfer_video_search_failure(mode: TransferVideoAuthMode) {
    TRANSFER_VIDEO_COOLDOWNS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .record_failure_at(mode, Instant::now());
}

fn record_transfer_video_search_success(mode: TransferVideoAuthMode) {
    TRANSFER_VIDEO_COOLDOWNS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .record_success(mode);
}

pub(super) async fn anonymous_ytmusic_client() -> Result<&'static YtMusic<NoAuthToken>> {
    ANONYMOUS_YTM_CLIENT
        .get_or_try_init(|| async {
            YtMusic::new_unauthenticated()
                .await
                .context("anonymous YouTube Music client init failed")
        })
        .await
}

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
    /// Whether this process can currently query the authenticated YTM song catalog.
    ///
    /// Transfer matching uses this as a cheap capability check before taking a pacing
    /// slot. Anonymous sessions have no catalog backend, and an authenticated parser
    /// failure opens a cooldown during which retrying every query is known to fail.
    pub(crate) fn transfer_catalog_available(&self) -> bool {
        matches!(self, Self::Browser(_)) && !auth_search_degraded()
    }

    /// Whether the first-party YTM video catalog can be attempted without entering a
    /// known-failing cooldown. Video failures are isolated from the song-catalog cooldown:
    /// a layout or attestation failure here must not suppress otherwise healthy song results.
    pub(crate) fn transfer_video_catalog_available(&self) -> bool {
        !transfer_video_search_degraded(transfer_video_auth_mode(self))
    }

    /// Search the authenticated song catalog. The boolean is `true` when an authenticated
    /// catalog became unavailable (or was already in its degraded cooldown), rather than
    /// when a successful query merely returned no rows. Anonymous callers retain the
    /// existing `(empty, false)` result; routing code should use
    /// [`Self::transfer_catalog_available`] to distinguish that capability state.
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
        let search = search_catalog_songs_first_page(client, query, TRANSFER_YTM_RESULT_LIMIT);
        match tokio::time::timeout(TRANSFER_SONG_SEARCH_TIMEOUT, search).await {
            Ok(Ok(songs)) => {
                mark_auth_search_healthy();
                Ok((songs, false))
            }
            Ok(Err(error)) => {
                mark_auth_search_degraded();
                tracing::warn!(
                    error = %sanitize::sanitize_error_text(format!("{error:#}")),
                    "authenticated transfer catalog search failed"
                );
                Ok((Vec::new(), true))
            }
            Err(_) => {
                mark_auth_search_degraded();
                tracing::warn!(
                    timeout_ms = TRANSFER_SONG_SEARCH_TIMEOUT.as_millis(),
                    "authenticated transfer catalog search timed out"
                );
                Ok((Vec::new(), true))
            }
        }
    }

    /// Search YTM's first-party **Videos** filter before the public yt-dlp fallback.
    ///
    /// The boolean reports a degraded/unavailable provider attempt. A successful empty
    /// result is `(empty, false)`, allowing callers to distinguish a real empty page from
    /// a timeout/parser/anonymous-attestation failure while falling through in either case.
    /// Podcast `VideoEpisode` rows are intentionally excluded.
    pub async fn search_transfer_video_catalog(
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

        let mode = transfer_video_auth_mode(self);
        if transfer_video_search_degraded(mode) {
            return Ok((Vec::new(), true));
        }

        let search = async {
            match self {
                Self::Browser(client) => search_catalog_videos(client, query).await,
                Self::Anonymous => {
                    let client = anonymous_ytmusic_client().await?;
                    search_catalog_videos(client, query).await
                }
            }
        };

        let result = tokio::time::timeout(TRANSFER_VIDEO_SEARCH_TIMEOUT, search).await;
        match result {
            Ok(Ok(songs)) => {
                record_transfer_video_search_success(mode);
                Ok((songs, false))
            }
            Ok(Err(error)) => {
                record_transfer_video_search_failure(mode);
                tracing::warn!(
                    error = %sanitize::sanitize_error_text(format!("{error:#}")),
                    auth_mode = ?mode,
                    "YTM transfer video catalog search failed"
                );
                Ok((Vec::new(), true))
            }
            Err(_) => {
                record_transfer_video_search_failure(mode);
                tracing::warn!(
                    auth_mode = ?mode,
                    timeout_ms = TRANSFER_VIDEO_SEARCH_TIMEOUT.as_millis(),
                    "YTM transfer video catalog search timed out"
                );
                Ok((Vec::new(), true))
            }
        }
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
        ytdlp_search(query, TRANSFER_PUBLIC_RESULT_LIMIT).await
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
        let Self::Browser(client) = self else {
            return Ok(Vec::new());
        };
        let search = search_catalog_albums_first_page(client, query);
        let albums = match tokio::time::timeout(TRANSFER_ALBUM_SEARCH_TIMEOUT, search).await {
            Ok(Ok(albums)) => albums,
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %sanitize::sanitize_error_text(format!("{error:#}")),
                    "authenticated transfer album search failed"
                );
                return Ok(Vec::new());
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = TRANSFER_ALBUM_SEARCH_TIMEOUT.as_millis(),
                    "authenticated transfer album search timed out"
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
        let Self::Browser(client) = self else {
            return Ok(None);
        };
        let fetch = client.get_album(AlbumID::from_raw(album_id));
        let album = match tokio::time::timeout(TRANSFER_ALBUM_DETAIL_TIMEOUT, fetch).await {
            Ok(Ok(album)) => album,
            Ok(Err(error)) => {
                tracing::warn!(
                    error = %sanitize::sanitize_error_text(format!("{error:#}")),
                    "authenticated transfer album detail failed"
                );
                return Ok(None);
            }
            Err(_) => {
                tracing::warn!(
                    timeout_ms = TRANSFER_ALBUM_DETAIL_TIMEOUT.as_millis(),
                    "authenticated transfer album detail timed out"
                );
                return Ok(None);
            }
        };
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

async fn basic_search_first_page<A: AuthToken>(
    client: &YtMusic<A>,
    query: &str,
) -> Result<SearchResults> {
    let q: SearchQuery<BasicSearch> = query.into();
    Ok(client.query(q).await?)
}

async fn search_catalog_songs_first_page<A: AuthToken>(
    client: &YtMusic<A>,
    query: &str,
    limit: usize,
) -> Result<Vec<Song>> {
    let q: SearchQuery<FilteredSearch<SongsFilter>> = query.into();
    let rows = match client.query(q).await {
        Ok(rows) => rows,
        Err(error) if filtered_search_missing_music_shelf(&error.to_string()) => {
            tracing::debug!(
                "YTM filtered song search omitted its music shelf; retrying as a basic search"
            );
            basic_search_first_page(client, query).await?.songs
        }
        Err(error) => return Err(error.into()),
    };
    Ok(rows
        .into_iter()
        .take(limit)
        .map(song_from_catalog_search)
        .collect())
}

async fn search_catalog_albums_first_page<A: AuthToken>(
    client: &YtMusic<A>,
    query: &str,
) -> Result<Vec<SearchResultAlbum>> {
    let q: SearchQuery<FilteredSearch<AlbumsFilter>> = query.into();
    match client.query(q).await {
        Ok(rows) => Ok(rows),
        Err(error) if filtered_search_missing_music_shelf(&error.to_string()) => {
            tracing::debug!(
                "YTM filtered album search omitted its music shelf; retrying as a basic search"
            );
            Ok(basic_search_first_page(client, query).await?.albums)
        }
        Err(error) => Err(error.into()),
    }
}

fn song_from_catalog_search(row: SearchResultSong) -> Song {
    Song::from_search(
        row.video_id.get_raw(),
        row.title,
        row.artist,
        row.duration,
        row.album.map(|album| album.name),
    )
}

pub(super) async fn search_catalog_songs(
    client: &YtMusic<BrowserToken>,
    query: &str,
) -> Result<Vec<Song>> {
    search_catalog_songs_bounded(client, query, SEARCH_RESULT_LIMIT).await
}

async fn search_catalog_songs_bounded(
    client: &YtMusic<BrowserToken>,
    query: &str,
    limit: usize,
) -> Result<Vec<Song>> {
    let q: SearchQuery<FilteredSearch<SongsFilter>> = query.into();
    let mut pages = std::pin::pin!(client.stream(&q));
    let mut songs = Vec::new();
    while songs.len() < limit
        && let Some(page) = pages.next().await
    {
        let page = match page {
            Ok(page) => page,
            Err(e) if songs.is_empty() && filtered_search_missing_music_shelf(&e.to_string()) => {
                tracing::debug!(
                    "YTM filtered song search omitted its music shelf; retrying as a basic search"
                );
                return Ok(basic_search_first_page(client, query)
                    .await?
                    .songs
                    .into_iter()
                    .take(limit)
                    .map(song_from_catalog_search)
                    .collect());
            }
            Err(e) if songs.is_empty() => return Err(e.into()),
            Err(_) => break,
        };
        for s in page {
            songs.push(song_from_catalog_search(s));
            if songs.len() >= limit {
                break;
            }
        }
    }
    Ok(songs)
}

async fn search_catalog_videos<A: AuthToken>(
    client: &YtMusic<A>,
    query: &str,
) -> Result<Vec<Song>> {
    let q: SearchQuery<FilteredSearch<VideosFilter>> = query.into();
    let rows = match client.query(q).await {
        Ok(rows) => rows,
        Err(error) if filtered_search_missing_music_shelf(&error.to_string()) => {
            tracing::debug!(
                "YTM filtered video search omitted its music shelf; retrying as a basic search"
            );
            basic_search_first_page(client, query).await?.videos
        }
        Err(error) => return Err(error.into()),
    };
    let mut videos = Vec::new();
    append_catalog_videos(&mut videos, rows);
    Ok(videos)
}

fn append_catalog_videos(
    videos: &mut Vec<Song>,
    rows: impl IntoIterator<Item = SearchResultVideo>,
) {
    for row in rows {
        match row {
            SearchResultVideo::Video {
                title,
                channel_name,
                video_id,
                length,
                ..
            } => videos.push(Song::from_search(
                video_id.get_raw(),
                title,
                channel_name,
                length,
                None,
            )),
            SearchResultVideo::VideoEpisode { .. } => {}
        }
        if videos.len() >= TRANSFER_YTM_RESULT_LIMIT {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_result_budgets_stay_below_interactive_search() {
        assert_eq!(TRANSFER_YTM_RESULT_LIMIT, 20);
        assert_eq!(TRANSFER_PUBLIC_RESULT_LIMIT, 15);
        assert_eq!(TRANSFER_SONG_SEARCH_TIMEOUT, Duration::from_secs(8));
        assert_eq!(TRANSFER_ALBUM_SEARCH_TIMEOUT, Duration::from_secs(8));
        assert_eq!(TRANSFER_ALBUM_DETAIL_TIMEOUT, Duration::from_secs(8));
        const {
            assert!(TRANSFER_YTM_RESULT_LIMIT < SEARCH_RESULT_LIMIT);
            assert!(TRANSFER_PUBLIC_RESULT_LIMIT < TRANSFER_YTM_RESULT_LIMIT);
        }
    }

    #[test]
    fn filtered_search_classifier_only_accepts_the_known_missing_shelf_error() {
        assert!(filtered_search_missing_music_shelf(
            FILTERED_SEARCH_MISSING_MUSIC_SHELF
        ));
        assert!(!filtered_search_missing_music_shelf(
            "Expected /contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents to contain a /musicCardShelfRenderer/contents"
        ));
        assert!(!filtered_search_missing_music_shelf(
            "authenticated search failed: Expected /contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents to contain a /musicShelfRenderer/contents"
        ));
        assert!(!filtered_search_missing_music_shelf(
            "Web error <HTTP status client error (401 Unauthorized)> received."
        ));
    }

    #[test]
    fn video_breaker_requires_two_consecutive_failures_and_success_resets_it() {
        let now = Instant::now();
        let mut cooldowns = TransferVideoCooldowns::default();

        cooldowns.record_failure_at(TransferVideoAuthMode::Browser, now);
        assert!(!cooldowns.degraded_at(TransferVideoAuthMode::Browser, now));
        assert_eq!(cooldowns.browser.consecutive_failures, 1);

        cooldowns.record_success(TransferVideoAuthMode::Browser);
        assert_eq!(cooldowns.browser, TransferVideoHealth::default());

        cooldowns.record_failure_at(TransferVideoAuthMode::Browser, now);
        cooldowns.record_failure_at(TransferVideoAuthMode::Browser, now);
        assert!(cooldowns.degraded_at(TransferVideoAuthMode::Browser, now));
    }

    #[test]
    fn video_breaker_is_per_auth_mode_and_expiry_starts_a_fresh_streak() {
        let now = Instant::now();
        let mut cooldowns = TransferVideoCooldowns::default();
        cooldowns.record_failure_at(TransferVideoAuthMode::Anonymous, now);
        cooldowns.record_failure_at(TransferVideoAuthMode::Anonymous, now);

        assert!(cooldowns.degraded_at(TransferVideoAuthMode::Anonymous, now));
        assert!(!cooldowns.degraded_at(TransferVideoAuthMode::Browser, now));
        assert!(!cooldowns.degraded_at(
            TransferVideoAuthMode::Anonymous,
            now + TRANSFER_VIDEO_DEGRADE_COOLDOWN + Duration::from_millis(1)
        ));
        assert_eq!(cooldowns.anonymous, TransferVideoHealth::default());

        let after_cooldown = now + TRANSFER_VIDEO_DEGRADE_COOLDOWN + Duration::from_millis(2);
        cooldowns.record_failure_at(TransferVideoAuthMode::Anonymous, after_cooldown);
        assert!(!cooldowns.degraded_at(TransferVideoAuthMode::Anonymous, after_cooldown));
        assert_eq!(cooldowns.anonymous.consecutive_failures, 1);
    }

    #[test]
    fn catalog_video_mapper_excludes_video_episodes() {
        // SearchResultVideo's variants are non-exhaustive to downstream Rust code, so
        // deserialize the public serde representation to exercise the real mapper without
        // constructing an upstream private-shape value or touching the network.
        let rows: Vec<SearchResultVideo> = serde_json::from_value(serde_json::json!([
            {
                "Video": {
                    "title": "Artist - Song (Official Video)",
                    "channel_name": "Artist",
                    "video_id": "aaa111bbb22",
                    "views": "12M views",
                    "length": "3:42",
                    "thumbnails": []
                }
            },
            {
                "VideoEpisode": {
                    "title": "Episode 4",
                    "date": { "Recorded": { "date": "2026" } },
                    "channel_name": "Podcast",
                    "episode_id": "bbb222ccc33",
                    "thumbnails": []
                }
            }
        ]))
        .expect("valid upstream video result representation");
        let mut songs = Vec::new();
        append_catalog_videos(&mut songs, rows);

        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].video_id, "aaa111bbb22");
        assert_eq!(songs[0].title, "Artist - Song (Official Video)");
        assert_eq!(songs[0].artist, "Artist");
        assert_eq!(songs[0].duration, "3:42");
    }

    #[test]
    fn catalog_video_mapper_stops_at_transfer_budget() {
        let rows = (0..(TRANSFER_YTM_RESULT_LIMIT + 5))
            .map(|index| {
                serde_json::json!({
                    "Video": {
                        "title": format!("Song {index}"),
                        "channel_name": "Artist",
                        "video_id": format!("vid{index:08}"),
                        "views": "1 view",
                        "length": "3:00",
                        "thumbnails": []
                    }
                })
            })
            .collect::<Vec<_>>();
        let rows: Vec<SearchResultVideo> = serde_json::from_value(serde_json::Value::Array(rows))
            .expect("valid upstream video rows");
        let mut songs = Vec::new();

        append_catalog_videos(&mut songs, rows);

        assert_eq!(songs.len(), TRANSFER_YTM_RESULT_LIMIT);
        assert_eq!(
            songs.last().map(|song| song.title.as_str()),
            Some("Song 19")
        );
    }
}
