//! API actor startup and provider-backed command loops.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, Receiver};

use crate::search_source::{SearchConfig, SearchSource};
use crate::streaming::{CandidateSource, StreamingMode};
use crate::util::sanitize;

use super::{
    ApiCmd, ApiEvent, ApiHandle, ApiMode, ArtistIntent, GuiSearchGroup, GuiSearchRequestId,
    PlaylistIntent, Song, ytmusic,
};

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

/// Run one GUI search: per-catalog groups. A concrete `source` yields one group; `All`
/// fans out over the enabled catalogs (like the TUI's merged path) but keeps results
/// separated and failures per-source. A pasted YouTube URL short-circuits to a single
/// youtube group — resolving it once, not once per catalog.
async fn gui_search_groups(
    api: &ytmusic::YtMusicApi,
    query: &str,
    source: SearchSource,
    config: &SearchConfig,
) -> Vec<GuiSearchGroup> {
    let targets = gui_search_targets(query, source, config);
    futures::future::join_all(targets.into_iter().map(|target| async move {
        let result = if target == SearchSource::OpenSubsonic {
            budget_server_search(search_open_subsonic(query, config))
                .await
                .0
        } else {
            search_one_interactive_source(api, query, target, config).await
        };
        match result {
            Ok(songs) => GuiSearchGroup {
                source: target,
                songs,
                error: None,
            },
            Err(error) => GuiSearchGroup {
                source: target,
                songs: Vec::new(),
                error: Some(sanitize::sanitize_error_text(format!("{error:#}"))),
            },
        }
    }))
    .await
}

fn gui_search_targets(
    query: &str,
    source: SearchSource,
    config: &SearchConfig,
) -> Vec<SearchSource> {
    if is_youtube_url_query(query) {
        vec![SearchSource::Youtube]
    } else if source == SearchSource::All {
        let enabled = config.enabled_sources();
        if enabled.is_empty() {
            vec![SearchSource::Youtube]
        } else {
            enabled
        }
    } else {
        vec![source]
    }
}

async fn search_one_interactive_source(
    api: &ytmusic::YtMusicApi,
    query: &str,
    source: SearchSource,
    config: &SearchConfig,
) -> anyhow::Result<Vec<Song>> {
    if source == SearchSource::OpenSubsonic {
        search_open_subsonic(query, config).await
    } else {
        api.search_songs(query, source, config).await
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

async fn gui_search_event(
    api: &ytmusic::YtMusicApi,
    request_id: GuiSearchRequestId,
    query: String,
    source: SearchSource,
    config: SearchConfig,
) -> ApiEvent {
    let groups = gui_search_groups(api, &query, source, &config).await;
    let query_log = crate::util::query::query_log_preview(&query);
    tracing::info!(
        request_id = ?request_id,
        query_bytes = query_log.bytes,
        query_chars = query_log.chars,
        query_preview = %query_log.preview,
        query_truncated = query_log.truncated,
        source = %source.code(),
        groups = groups.len(),
        "gui search completed"
    );
    ApiEvent::GuiSearchCompleted { request_id, groups }
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
            ApiCmd::GuiSearch {
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
                        emit(gui_search_event(&api, request_id, query, source, config).await);
                    }));
                } else {
                    emit(gui_search_event(&api, request_id, query, source, config).await);
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
    let mut streaming_ytdlp_cache: HashMap<
        (String, StreamingMode, SearchSource),
        (Instant, Vec<Song>),
    > = HashMap::new();
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
                let selected_sources = if streaming_source == SearchSource::All {
                    config.streaming_enabled_sources()
                } else {
                    vec![streaming_source]
                };
                let per_source_limit = if streaming_source == SearchSource::All {
                    (limit / selected_sources.len().max(1)).max(4).min(limit)
                } else {
                    limit
                };
                let mut candidate_ids: HashSet<String> = exclude_ids.into_iter().collect();
                let mut candidates: Vec<(Song, CandidateSource)> = Vec::new();
                let mut errors = Vec::new();

                if selected_sources.contains(&SearchSource::Youtube) {
                    let mut yt_added = 0usize;
                    match api.streaming_continuation(&seed_video_id).await {
                        Ok(songs) => {
                            for s in songs {
                                if candidate_ids.insert(s.video_id.clone()) {
                                    candidates.push((s, CandidateSource::WatchPlaylist));
                                    yt_added += 1;
                                    if yt_added >= per_source_limit || candidates.len() >= limit {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %sanitize::sanitize_error_text(format!("{e:#}")), "watch-playlist streaming unavailable; using search top-up");
                        }
                    }

                    let want = if streaming_source == SearchSource::All {
                        per_source_limit.saturating_sub(yt_added)
                    } else {
                        limit.saturating_sub(candidates.len())
                    };
                    if want > 0 && candidates.len() < limit {
                        match cached_related_tracks(
                            &mut streaming_ytdlp_cache,
                            &seed,
                            SearchSource::Youtube,
                            &config,
                            want,
                            mode,
                        )
                        .await
                        {
                            Ok(songs) => {
                                let mut added = 0usize;
                                for s in songs {
                                    if candidate_ids.insert(s.video_id.clone()) {
                                        candidates.push((s, CandidateSource::YtdlpStreaming));
                                        added += 1;
                                        if added >= want || candidates.len() >= limit {
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(e) => errors.push(format!(
                                "{}: {}",
                                SearchSource::Youtube.code(),
                                sanitize::sanitize_error_text(format!("{e:#}"))
                            )),
                        }
                    }
                }

                for source in selected_sources
                    .iter()
                    .copied()
                    .filter(|source| *source != SearchSource::Youtube)
                {
                    if candidates.len() >= limit {
                        break;
                    }
                    let source_limit = if streaming_source == SearchSource::All {
                        per_source_limit
                    } else {
                        limit.saturating_sub(candidates.len())
                    };
                    if source_limit == 0 {
                        continue;
                    }
                    match cached_related_tracks(
                        &mut streaming_ytdlp_cache,
                        &seed,
                        source,
                        &config,
                        source_limit,
                        mode,
                    )
                    .await
                    {
                        Ok(songs) => {
                            let mut added = 0usize;
                            for s in songs {
                                if candidate_ids.insert(s.video_id.clone()) {
                                    candidates.push((s, CandidateSource::YtdlpStreaming));
                                    added += 1;
                                    if added >= source_limit || candidates.len() >= limit {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => errors.push(format!(
                            "{}: {}",
                            source.code(),
                            sanitize::sanitize_error_text(format!("{e:#}"))
                        )),
                    }
                }

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
            | ApiCmd::GuiSearch { .. }
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

async fn cached_related_tracks(
    cache: &mut HashMap<(String, StreamingMode, SearchSource), (Instant, Vec<Song>)>,
    seed: &str,
    source: SearchSource,
    config: &SearchConfig,
    limit: usize,
    mode: StreamingMode,
) -> anyhow::Result<Vec<Song>> {
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

pub(super) fn enforce_streaming_cache_cap(
    cache: &mut HashMap<(String, StreamingMode, SearchSource), (Instant, Vec<Song>)>,
) {
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

    #[test]
    fn youtube_urls_short_circuit_every_gui_source_selection() {
        let mut config = SearchConfig::default();
        config.set_enabled(SearchSource::OpenSubsonic, true);
        assert_eq!(
            gui_search_targets(
                "https://youtu.be/dQw4w9WgXcQ",
                SearchSource::OpenSubsonic,
                &config,
            ),
            vec![SearchSource::Youtube]
        );
        let all = gui_search_targets("ordinary query", SearchSource::All, &config);
        assert!(all.contains(&SearchSource::Youtube));
        assert!(all.contains(&SearchSource::OpenSubsonic));
    }
}
