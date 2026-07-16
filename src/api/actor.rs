//! API actor startup and provider-backed command loops.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, Receiver};

use crate::search_source::{SearchConfig, SearchSource};
use crate::streaming::{CandidateSource, StreamingMode};
use crate::util::sanitize;

use super::{ApiCmd, ApiEvent, ApiHandle, ApiMode, GuiSearchGroup, Song, ytmusic};

const STREAMING_YTDLP_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
pub(super) const STREAMING_YTDLP_CACHE_MAX: usize = 512;
const API_INTERACTIVE_QUEUE: usize = 256;
const API_BULK_QUEUE: usize = 256;

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
    let url_like = crate::media::parse_youtube_playlist_id(query).is_some()
        || crate::media::parse_youtube_video_id(query).is_some();
    let targets: Vec<SearchSource> = if source == SearchSource::All && !url_like {
        let enabled = config.enabled_sources();
        if enabled.is_empty() {
            vec![SearchSource::Youtube]
        } else {
            enabled
        }
    } else if source == SearchSource::All {
        vec![SearchSource::Youtube]
    } else {
        vec![source]
    };
    let mut groups = Vec::with_capacity(targets.len());
    for target in targets {
        match api.search_songs(query, target, config).await {
            Ok(songs) => groups.push(GuiSearchGroup {
                source: target,
                songs,
                error: None,
            }),
            Err(e) => groups.push(GuiSearchGroup {
                source: target,
                songs: Vec::new(),
                error: Some(sanitize::sanitize_error_text(format!("{e:#}"))),
            }),
        }
    }
    groups
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

async fn run_interactive_actor<F>(
    api: Arc<ytmusic::YtMusicApi>,
    mut rx: Receiver<ApiCmd>,
    emit: Arc<F>,
) where
    F: Fn(ApiEvent) + Send + Sync + 'static,
{
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ApiCmd::Search {
                request_id,
                query,
                source,
                config,
            } => {
                let event = match api.search_songs_reported(&query, source, &config).await {
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
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(source = %source.code(), error = %error, "search failed");
                        ApiEvent::SearchError {
                            request_id,
                            source,
                            error,
                        }
                    }
                };
                emit(event);
            }
            ApiCmd::GuiSearch {
                request_id,
                query,
                source,
                config,
            } => {
                let groups = gui_search_groups(&api, &query, source, &config).await;
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
                emit(ApiEvent::GuiSearchCompleted { request_id, groups });
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
            ApiCmd::PlaylistTracks { .. }
            | ApiCmd::Streaming { .. }
            | ApiCmd::StreamingPreflight { .. } => {
                tracing::warn!(
                    kind = ?cmd.kind(),
                    "bulk API command arrived on interactive lane"
                );
            }
        }
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
            ApiCmd::Streaming {
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
                        seed_video_id,
                        error,
                    }
                } else {
                    tracing::info!(count = candidates.len(), seed = %seed, "streaming results");
                    ApiEvent::StreamingResults {
                        seed_video_id,
                        candidates,
                    }
                };
                emit(event);
            }
            ApiCmd::StreamingPreflight {
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => {
                let songs =
                    ytmusic::preflight_streaming_picks(picks, fallback, mode, &config).await;
                emit(ApiEvent::StreamingPreflighted {
                    seed_video_id,
                    songs,
                });
            }
            ApiCmd::Search { .. }
            | ApiCmd::GuiSearch { .. }
            | ApiCmd::ResolveTrack { .. }
            | ApiCmd::SearchPlaylists { .. } => {
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
