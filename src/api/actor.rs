//! API actor startup and provider-backed command loops.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, Receiver};

use crate::search_source::{SearchConfig, SearchSource};
use crate::streaming::{CandidateSource, StreamingMode};
use crate::util::sanitize;

use super::{ApiCmd, ApiEvent, ApiHandle, ApiMode, ArtistIntent, PlaylistIntent, Song, ytmusic};

const STREAMING_YTDLP_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
pub(super) const STREAMING_YTDLP_CACHE_MAX: usize = 512;
const API_INTERACTIVE_QUEUE: usize = 256;
const API_BULK_QUEUE: usize = 256;
const API_SEARCH_RESULT_LIMIT: usize = 50;
const ALL_SERVER_SEARCH_BUDGET: Duration = Duration::from_millis(1_500);

/// Spawn the API actor, returning its handle immediately.
///
/// A configured cookie is tried first; if it's rejected we fall back to anonymous
/// (yt-dlp) search so search + public playback still work. With no cookie we go straight
/// to anonymous. Commands sent before authentication settles are buffered by the channel.
pub fn spawn<F>(cookie: Option<String>, emit: F) -> ApiHandle
where
    F: Fn(ApiEvent) + Send + Sync + 'static,
{
    let had_cookie = cookie.is_some();
    // Bounded with a generous cap; the human-rate UI producer never fills it in normal use.
    // A stalled network + command burst rejects new commands through `ApiEnqueueError`, which
    // every owner maps to a visible terminal result instead of silently losing the request.
    let (interactive_tx, interactive_rx) = mpsc::channel(API_INTERACTIVE_QUEUE);
    let (bulk_tx, bulk_rx) = mpsc::channel(API_BULK_QUEUE);
    tokio::spawn(async move {
        let (api, mode) = init_api(cookie).await;
        emit(ApiEvent::ModeResolved { mode, had_cookie });
        let api = Arc::new(api);
        let emit = Arc::new(emit);
        tokio::spawn(run_interactive_actor(
            Arc::clone(&api),
            interactive_rx,
            Arc::clone(&emit),
        ));
        run_bulk_actor(api, bulk_rx, emit).await;
    });
    ApiHandle {
        interactive_tx,
        bulk_tx,
    }
}

async fn search_interactive_reported(
    api: &ytmusic::YtMusicApi,
    query: &str,
    source: SearchSource,
    config: &SearchConfig,
) -> anyhow::Result<(Vec<Song>, bool)> {
    if is_youtube_url_query(query) {
        return api.search_songs_reported(query, source, config).await;
    }
    match source {
        SearchSource::OpenSubsonic => {
            let (result, timed_out) =
                budget_server_search(search_open_subsonic(query, config)).await;
            Ok((result?, timed_out))
        }
        SearchSource::All => {
            let public_enabled = !config.enabled_public_sources().is_empty();
            let server_enabled = config.is_enabled(SearchSource::OpenSubsonic);
            let public = async {
                if public_enabled {
                    Some(
                        api.search_songs_reported(query, SearchSource::All, config)
                            .await,
                    )
                } else {
                    None
                }
            };
            let server = async {
                if server_enabled {
                    Some(budget_server_search(search_open_subsonic(query, config)).await)
                } else {
                    None
                }
            };
            let (public, server) = tokio::join!(public, server);
            merge_all_search_results(public, server)
        }
        _ => api.search_songs_reported(query, source, config).await,
    }
}

async fn search_open_subsonic(query: &str, config: &SearchConfig) -> anyhow::Result<Vec<Song>> {
    if !config.is_enabled(SearchSource::OpenSubsonic) {
        anyhow::bail!(
            "{} is disabled in Settings → General",
            SearchSource::OpenSubsonic.label()
        );
    }
    let handle = crate::open_subsonic::current_handle()
        .ok_or_else(|| anyhow::anyhow!("music server is off"))?;
    handle
        .search(query, API_SEARCH_RESULT_LIMIT as u32)
        .await
        .map(|songs| songs.into_iter().map(Song::from_open_subsonic).collect())
        .map_err(|error| anyhow::anyhow!("{error}"))
}

async fn budget_server_search<F>(future: F) -> (anyhow::Result<Vec<Song>>, bool)
where
    F: std::future::Future<Output = anyhow::Result<Vec<Song>>>,
{
    match tokio::time::timeout(ALL_SERVER_SEARCH_BUDGET, future).await {
        Ok(result) => (result, false),
        Err(_) => (Err(anyhow::anyhow!("music server search timed out")), true),
    }
}

fn merge_all_search_results(
    public: Option<anyhow::Result<(Vec<Song>, bool)>>,
    server: Option<(anyhow::Result<Vec<Song>>, bool)>,
) -> anyhow::Result<(Vec<Song>, bool)> {
    let mut public_songs = Vec::new();
    let mut server_songs = Vec::new();
    let mut errors = Vec::new();
    let mut had_success = false;
    let mut timed_out = false;

    if let Some(result) = public {
        match result {
            Ok((incoming, public_timed_out)) => {
                had_success = true;
                timed_out = public_timed_out;
                public_songs = incoming;
            }
            Err(error) => errors.push(sanitize::sanitize_error_text(format!("{error:#}"))),
        }
    }
    if let Some((result, server_timed_out)) = server {
        timed_out |= server_timed_out;
        match result {
            Ok(incoming) => {
                had_success = true;
                server_songs = incoming;
            }
            Err(error) => errors.push(sanitize::sanitize_error_text(format!("{error:#}"))),
        }
    }
    let songs = interleave_unique(public_songs, server_songs, API_SEARCH_RESULT_LIMIT);
    if songs.is_empty() && !errors.is_empty() && !had_success {
        anyhow::bail!("all enabled sources failed ({})", errors.join("; "));
    }
    for error in errors {
        tracing::warn!(error = %error, "one search source failed; returning partial results");
    }
    Ok((songs, timed_out))
}

fn interleave_unique(public: Vec<Song>, server: Vec<Song>, limit: usize) -> Vec<Song> {
    if limit == 0 {
        return Vec::new();
    }
    let mut songs = Vec::with_capacity(limit.min(public.len().saturating_add(server.len())));
    let mut seen = HashSet::new();
    let mut public = public.into_iter();
    let mut server = server.into_iter();
    loop {
        let mut advanced = false;
        for next in [&mut public, &mut server] {
            if let Some(song) = next.next() {
                advanced = true;
                if seen.insert(song.video_id.clone()) {
                    songs.push(song);
                    if songs.len() >= limit {
                        return songs;
                    }
                }
            }
        }
        if !advanced {
            return songs;
        }
    }
}

fn is_youtube_url_query(query: &str) -> bool {
    crate::media::parse_youtube_playlist_id(query).is_some()
        || crate::media::parse_youtube_video_id(query).is_some()
}

async fn init_api(cookie: Option<String>) -> (ytmusic::YtMusicApi, ApiMode) {
    let (api, mode) = match cookie {
        Some(c) => match ytmusic::YtMusicApi::from_cookie(&c).await {
            Ok(api) => (api, ApiMode::Authenticated),
            Err(e) => {
                tracing::warn!(error = %sanitize::sanitize_error_text(format!("{e:#}")), "cookie auth failed; using anonymous search");
                (ytmusic::YtMusicApi::Anonymous, ApiMode::Anonymous)
            }
        },
        None => (ytmusic::YtMusicApi::Anonymous, ApiMode::Anonymous),
    };
    (api, mode)
}

async fn interactive_search_event(
    api: &ytmusic::YtMusicApi,
    request_id: u64,
    query: String,
    source: SearchSource,
    config: SearchConfig,
) -> ApiEvent {
    match search_interactive_reported(api, &query, source, &config).await {
        Ok((songs, timed_out)) => {
            let query_log = crate::util::query::query_log_preview(&query);
            tracing::info!(
                count = songs.len(),
                query_bytes = query_log.bytes,
                query_chars = query_log.chars,
                query_preview = %query_log.preview,
                query_truncated = query_log.truncated,
                source = %source.code(),
                timed_out,
                "search results"
            );
            ApiEvent::SearchResults {
                request_id,
                query,
                source,
                songs,
                timed_out,
            }
        }
        Err(error) => {
            let error = sanitize::sanitize_error_text(format!("{error:#}"));
            tracing::warn!(source = %source.code(), error = %error, "search failed");
            ApiEvent::SearchError {
                request_id,
                source,
                error,
            }
        }
    }
}

async fn run_interactive_actor<F>(
    api: Arc<ytmusic::YtMusicApi>,
    mut rx: Receiver<ApiCmd>,
    emit: Arc<F>,
) where
    F: Fn(ApiEvent) + Send + Sync + 'static,
{
    let mut dedicated_server_search: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ApiCmd::Search {
                request_id,
                query,
                source,
                config,
            } => {
                if source == SearchSource::OpenSubsonic {
                    if let Some(task) = dedicated_server_search.take() {
                        task.abort();
                    }
                    let api = Arc::clone(&api);
                    let emit = Arc::clone(&emit);
                    dedicated_server_search = Some(tokio::spawn(async move {
                        emit(
                            interactive_search_event(&api, request_id, query, source, config).await,
                        );
                    }));
                } else {
                    emit(interactive_search_event(&api, request_id, query, source, config).await);
                }
            }
            ApiCmd::ResolveTrack { seq, query, config } => {
                // The same innertube→yt-dlp search as the Search screen, but the answer
                // stays out of screen state — the caller matches it back up by `seq`.
                let result = api
                    .search_songs(&query, SearchSource::Youtube, &config)
                    .await
                    .map_err(|e| sanitize::sanitize_error_text(format!("{e:#}")));
                match &result {
                    Ok(songs) => {
                        let query_log = crate::util::query::query_log_preview(&query);
                        tracing::info!(
                            count = songs.len(),
                            query_bytes = query_log.bytes,
                            query_chars = query_log.chars,
                            query_preview = %query_log.preview,
                            query_truncated = query_log.truncated,
                            "track resolved"
                        )
                    }
                    Err(error) => tracing::warn!(error = %error, "track resolve failed"),
                }
                emit(ApiEvent::TrackResolved { seq, result });
            }
            ApiCmd::SearchPlaylists { request_id, query } => {
                let event = match api.search_playlists(&query).await {
                    Ok(songs) => {
                        let query_log = crate::util::query::query_log_preview(&query);
                        tracing::info!(
                            count = songs.len(),
                            query_bytes = query_log.bytes,
                            query_chars = query_log.chars,
                            query_preview = %query_log.preview,
                            query_truncated = query_log.truncated,
                            "playlist search results"
                        );
                        ApiEvent::SearchResults {
                            request_id,
                            query,
                            source: SearchSource::Youtube,
                            songs,
                            // Playlist search is a single provider; no multi-source deadline.
                            timed_out: false,
                        }
                    }
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(error = %error, "playlist search failed");
                        ApiEvent::SearchError {
                            request_id,
                            source: SearchSource::Youtube,
                            error,
                        }
                    }
                };
                emit(event);
            }
            ApiCmd::SearchArtists { request_id, query } => {
                let event = match api.search_artists(&query).await {
                    Ok(songs) => {
                        let query_log = crate::util::query::query_log_preview(&query);
                        tracing::info!(
                            count = songs.len(),
                            query_bytes = query_log.bytes,
                            query_chars = query_log.chars,
                            query_preview = %query_log.preview,
                            query_truncated = query_log.truncated,
                            "artist search results"
                        );
                        ApiEvent::SearchResults {
                            request_id,
                            query,
                            source: SearchSource::Youtube,
                            songs,
                            // Artist search is a single provider; no multi-source deadline.
                            timed_out: false,
                        }
                    }
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(error = %error, "artist search failed");
                        ApiEvent::SearchError {
                            request_id,
                            source: SearchSource::Youtube,
                            error,
                        }
                    }
                };
                emit(event);
            }
            ApiCmd::PlaylistTracks { .. }
            | ApiCmd::ArtistPage { .. }
            | ApiCmd::Streaming { .. }
            | ApiCmd::StreamingPreflight { .. } => {
                tracing::warn!(
                    kind = ?cmd.kind(),
                    "bulk API command arrived on interactive lane"
                );
            }
        }
    }
    if let Some(task) = dedicated_server_search {
        task.abort();
    }
}

async fn run_bulk_actor<F>(api: Arc<ytmusic::YtMusicApi>, mut rx: Receiver<ApiCmd>, emit: Arc<F>)
where
    F: Fn(ApiEvent) + Send + Sync + 'static,
{
    let mut streaming_ytdlp_cache = StreamingCache::new();
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ApiCmd::PlaylistTracks {
                playlist_id,
                title,
                intent,
            } => {
                let event = match api.playlist_tracks(&playlist_id).await {
                    Ok(songs) => {
                        tracing::info!(count = songs.len(), id = %playlist_id, "playlist tracks fetched");
                        ApiEvent::PlaylistTracks {
                            title,
                            intent,
                            songs,
                        }
                    }
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(id = %playlist_id, error = %error, "playlist tracks fetch failed");
                        ApiEvent::PlaylistTracksError { title, error }
                    }
                };
                emit(event);
            }
            ApiCmd::ArtistPage {
                channel_id,
                title,
                intent,
            } => {
                let event = match api.artist_page(&channel_id).await {
                    Ok(page) => match intent {
                        ArtistIntent::Open => {
                            tracing::info!(
                                songs = page.songs.len(),
                                albums = page.albums.len(),
                                id = %channel_id,
                                "artist page fetched"
                            );
                            ApiEvent::ArtistPage { page }
                        }
                        // Enqueue/Import act on the artist's full songs playlist, chained
                        // here so the reducer reuses its playlist-row path unchanged.
                        ArtistIntent::Enqueue | ArtistIntent::Import => {
                            let intent = if intent == ArtistIntent::Enqueue {
                                PlaylistIntent::Enqueue
                            } else {
                                PlaylistIntent::Import
                            };
                            let title = if page.name.is_empty() {
                                title
                            } else {
                                page.name
                            };
                            match &page.songs_playlist_id {
                                Some(playlist_id) => match api.playlist_tracks(playlist_id).await {
                                    Ok(songs) => {
                                        tracing::info!(
                                            count = songs.len(),
                                            id = %channel_id,
                                            "artist songs playlist fetched"
                                        );
                                        ApiEvent::PlaylistTracks {
                                            title,
                                            intent,
                                            songs,
                                        }
                                    }
                                    Err(e) => {
                                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                                        tracing::warn!(id = %channel_id, error = %error, "artist songs playlist fetch failed");
                                        ApiEvent::ArtistPageError { title, error }
                                    }
                                },
                                // No full playlist exposed — fall back to the page's top songs.
                                None if !page.songs.is_empty() => ApiEvent::PlaylistTracks {
                                    title,
                                    intent,
                                    songs: page.songs,
                                },
                                None => ApiEvent::ArtistPageError {
                                    title,
                                    error: "the artist page lists no songs".to_owned(),
                                },
                            }
                        }
                    },
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(id = %channel_id, error = %error, "artist page fetch failed");
                        ApiEvent::ArtistPageError { title, error }
                    }
                };
                emit(event);
            }
            ApiCmd::Streaming {
                request_id,
                seed,
                seed_video_id,
                exclude_ids,
                limit,
                mode,
                config,
            } => {
                // Build one pool from the configured streaming source(s), tagged by provenance so
                // the local engine can weight them. YouTube gets the strongest source first:
                // YTM's own watch-playlist continuation, then search-based top-up. Other providers
                // use their Search-screen backends with the same seed query variants.
                let config = config.normalized();
                let streaming_source = config.normalized_streaming_source(config.streaming_source);
                let split_budget = streaming_source == SearchSource::All;
                let selected_sources = if split_budget {
                    config.streaming_enabled_sources()
                } else {
                    vec![streaming_source]
                };
                // With every source enabled, split the budget evenly but never below a handful
                // per source, so a provider with thin results still contributes variety.
                const MIN_PER_SOURCE_CANDIDATES: usize = 4;
                let per_source_limit = if split_budget {
                    (limit / selected_sources.len().max(1))
                        .max(MIN_PER_SOURCE_CANDIDATES)
                        .min(limit)
                } else {
                    limit
                };
                let mut pool = CandidatePool::new(exclude_ids, limit);
                let query = RelatedQuery {
                    seed: &seed,
                    config: &config,
                    mode,
                };

                if selected_sources.contains(&SearchSource::Youtube) {
                    let yt_added = match api.streaming_continuation(&seed_video_id).await {
                        Ok(songs) => {
                            pool.admit(songs, CandidateSource::WatchPlaylist, per_source_limit)
                        }
                        Err(e) => {
                            tracing::warn!(error = %sanitize::sanitize_error_text(format!("{e:#}")), "watch-playlist streaming unavailable; using search top-up");
                            0
                        }
                    };
                    let want = if split_budget {
                        per_source_limit.saturating_sub(yt_added)
                    } else {
                        pool.remaining()
                    };
                    if want > 0 && !pool.is_full() {
                        top_up_from_source(
                            &mut streaming_ytdlp_cache,
                            &mut pool,
                            &query,
                            SearchSource::Youtube,
                            want,
                        )
                        .await;
                    }
                }

                for source in selected_sources
                    .iter()
                    .copied()
                    .filter(|source| *source != SearchSource::Youtube)
                {
                    if pool.is_full() {
                        break;
                    }
                    let want = if split_budget {
                        per_source_limit
                    } else {
                        pool.remaining()
                    };
                    if want == 0 {
                        continue;
                    }
                    top_up_from_source(&mut streaming_ytdlp_cache, &mut pool, &query, source, want)
                        .await;
                }

                let CandidatePool {
                    candidates, errors, ..
                } = pool;
                let event = if candidates.is_empty() {
                    let error = if errors.is_empty() {
                        "no related tracks found".to_owned()
                    } else {
                        errors.join("; ")
                    };
                    tracing::warn!(seed = %seed, %error, "streaming search yielded nothing");
                    ApiEvent::StreamingError {
                        request_id,
                        seed_video_id,
                        error,
                    }
                } else {
                    tracing::info!(count = candidates.len(), seed = %seed, "streaming results");
                    ApiEvent::StreamingResults {
                        request_id,
                        seed_video_id,
                        candidates,
                    }
                };
                emit(event);
            }
            ApiCmd::StreamingPreflight {
                request_id,
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => {
                let songs =
                    ytmusic::preflight_streaming_picks(picks, fallback, mode, &config).await;
                emit(ApiEvent::StreamingPreflighted {
                    request_id,
                    seed_video_id,
                    songs,
                });
            }
            ApiCmd::Search { .. }
            | ApiCmd::ResolveTrack { .. }
            | ApiCmd::SearchPlaylists { .. }
            | ApiCmd::SearchArtists { .. } => {
                tracing::warn!(
                    kind = ?cmd.kind(),
                    "interactive API command arrived on bulk lane"
                );
            }
        }
    }
}

type StreamingCache = HashMap<(String, StreamingMode, SearchSource), (Instant, Vec<Song>)>;

/// The seed one streaming request expands, shared by every source consulted for it.
struct RelatedQuery<'a> {
    seed: &'a str,
    config: &'a SearchConfig,
    mode: StreamingMode,
}

/// Provenance-tagged streaming candidates, deduplicated by video id against the request's
/// exclusions and capped at its overall budget.
struct CandidatePool {
    seen: HashSet<String>,
    candidates: Vec<(Song, CandidateSource)>,
    errors: Vec<String>,
    limit: usize,
}

impl CandidatePool {
    fn new(exclude_ids: impl IntoIterator<Item = String>, limit: usize) -> Self {
        Self {
            seen: exclude_ids.into_iter().collect(),
            candidates: Vec::new(),
            errors: Vec::new(),
            limit,
        }
    }

    fn is_full(&self) -> bool {
        self.candidates.len() >= self.limit
    }

    fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.candidates.len())
    }

    /// Appends unseen songs until `want` were added or the pool is full; returns how many
    /// were added. The caps are checked after each admission, so a zero `want` or budget
    /// still lets the first unseen song through.
    fn admit(&mut self, songs: Vec<Song>, source: CandidateSource, want: usize) -> usize {
        let mut added = 0;
        for song in songs {
            if !self.seen.insert(song.video_id.clone()) {
                continue;
            }
            self.candidates.push((song, source));
            added += 1;
            if added >= want || self.is_full() {
                break;
            }
        }
        added
    }
}

async fn top_up_from_source(
    cache: &mut StreamingCache,
    pool: &mut CandidatePool,
    query: &RelatedQuery<'_>,
    source: SearchSource,
    want: usize,
) {
    match cached_related_tracks(cache, query, source, want).await {
        Ok(songs) => {
            pool.admit(songs, CandidateSource::YtdlpStreaming, want);
        }
        Err(e) => pool.errors.push(format!(
            "{}: {}",
            source.code(),
            sanitize::sanitize_error_text(format!("{e:#}"))
        )),
    }
}

async fn cached_related_tracks(
    cache: &mut StreamingCache,
    query: &RelatedQuery<'_>,
    source: SearchSource,
    limit: usize,
) -> anyhow::Result<Vec<Song>> {
    let RelatedQuery { seed, config, mode } = *query;
    let now = Instant::now();
    cache.retain(|_, (stored, _)| now.duration_since(*stored) < STREAMING_YTDLP_CACHE_TTL);
    let cache_key = (seed.to_owned(), mode, source);
    if let Some(songs) = cache
        .get(&cache_key)
        .filter(|(stored, _)| now.duration_since(*stored) < STREAMING_YTDLP_CACHE_TTL)
        .map(|(_, songs)| songs.clone())
    {
        return Ok(songs);
    }
    let empty = HashSet::new();
    let songs =
        ytmusic::related_tracks_from_source(seed, source, config, limit, &empty, mode).await?;
    cache.insert(cache_key, (now, songs.clone()));
    enforce_streaming_cache_cap(cache);
    Ok(songs)
}

pub(super) fn enforce_streaming_cache_cap(cache: &mut StreamingCache) {
    while cache.len() > STREAMING_YTDLP_CACHE_MAX {
        let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (stored, _))| *stored)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        cache.remove(&oldest);
    }
}

#[cfg(test)]
mod candidate_pool_tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song::remote(id, "Title", "Artist", "3:00")
    }

    #[test]
    fn admit_skips_excluded_and_repeated_ids() {
        let mut pool = CandidatePool::new(["x".to_owned()], 10);
        let added = pool.admit(
            vec![song("x"), song("a"), song("a"), song("b")],
            CandidateSource::WatchPlaylist,
            10,
        );
        assert_eq!(added, 2);
        let ids: Vec<&str> = pool
            .candidates
            .iter()
            .map(|(song, _)| song.video_id.as_str())
            .collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(pool.remaining(), 8);
    }

    #[test]
    fn admit_stops_at_want_then_at_budget() {
        let mut pool = CandidatePool::new([], 3);
        let songs = vec![song("a"), song("b"), song("c"), song("d")];
        assert_eq!(pool.admit(songs, CandidateSource::YtdlpStreaming, 2), 2);
        assert!(!pool.is_full());
        assert_eq!(
            pool.admit(
                vec![song("e"), song("f")],
                CandidateSource::YtdlpStreaming,
                5
            ),
            1
        );
        assert!(pool.is_full());
        assert_eq!(pool.remaining(), 0);
        assert_eq!(
            pool.admit(vec![song("g")], CandidateSource::YtdlpStreaming, 5),
            1
        );
        assert_eq!(pool.candidates.len(), 4);
    }
}

#[cfg(test)]
mod open_subsonic_search_tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song::remote(id, format!("Song {id}"), "Artist", "3:00")
    }

    #[test]
    fn all_search_keeps_public_results_when_server_is_offline() {
        let result = merge_all_search_results(
            Some(Ok((vec![song("public")], false))),
            Some((Err(anyhow::anyhow!("music server is offline")), false)),
        )
        .unwrap();
        assert_eq!(result.0.len(), 1);
        assert_eq!(result.0[0].video_id, "public");
        assert!(!result.1);
    }

    #[test]
    fn all_search_keeps_server_results_when_public_sources_fail() {
        let result = merge_all_search_results(
            Some(Err(anyhow::anyhow!("public source failed"))),
            Some((Ok(vec![song("server")]), false)),
        )
        .unwrap();
        assert_eq!(result.0[0].video_id, "server");
    }

    #[test]
    fn all_search_deduplicates_and_only_fails_when_every_source_fails() {
        let result = merge_all_search_results(
            Some(Ok((vec![song("same")], true))),
            Some((Ok(vec![song("same"), song("other")]), false)),
        )
        .unwrap();
        assert_eq!(
            result
                .0
                .iter()
                .map(|song| song.video_id.as_str())
                .collect::<Vec<_>>(),
            vec!["same", "other"]
        );
        assert!(result.1);
        assert!(
            merge_all_search_results(
                Some(Err(anyhow::anyhow!("public failed"))),
                Some((Err(anyhow::anyhow!("server failed")), false)),
            )
            .is_err()
        );
    }

    #[test]
    fn all_search_fairly_includes_server_rows_when_public_hits_the_limit() {
        let public = (0..API_SEARCH_RESULT_LIMIT)
            .map(|index| song(&format!("public-{index}")))
            .collect();
        let result = merge_all_search_results(
            Some(Ok((public, false))),
            Some((Ok(vec![song("server-only")]), false)),
        )
        .unwrap();
        assert_eq!(result.0.len(), API_SEARCH_RESULT_LIMIT);
        assert!(result.0.iter().any(|song| song.video_id == "server-only"));
        assert_eq!(result.0[0].video_id, "public-0");
        assert_eq!(result.0[1].video_id, "server-only");
    }

    #[tokio::test(start_paused = true)]
    async fn all_search_server_branch_has_an_independent_latency_budget() {
        let task = tokio::spawn(budget_server_search(std::future::pending::<
            anyhow::Result<Vec<Song>>,
        >()));
        tokio::time::advance(ALL_SERVER_SEARCH_BUDGET).await;
        let (result, timed_out) = task.await.unwrap();
        assert!(result.is_err());
        assert!(timed_out);
    }
}
