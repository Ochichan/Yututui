use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::api::Song;
use crate::api::ytmusic::{YoutubeSearchKind, YtMusicApi, YtdlpVideoMeta};
use crate::search_source::SearchConfig;
use crate::streaming::musicgate;
use crate::transfer::MatchPolicy;

use super::{
    CandidateSourceKind, MatchCandidate, MatchConfig, MatchOutcome, MatchScoreBreakdown,
    TrackInput, best_outcome, hard_duration_limit, normalize, normalize_stripped,
    push_query_variant, release_year, score_candidate_breakdown_with_config, soft_duration_limit,
    strip_annotations,
};

type QueryMemo = HashMap<String, Vec<(Song, YoutubeSearchKind)>>;

#[derive(Clone)]
pub(crate) struct SharedYtmMatchState {
    memo: Arc<AsyncMutex<HashMap<String, MatchOutcome>>>,
    query_memo: Arc<AsyncMutex<QueryMemo>>,
    video_memo: Arc<AsyncMutex<HashMap<String, Option<YtdlpVideoMeta>>>>,
    pace: Arc<AsyncMutex<Pacing>>,
    diagnostics: Arc<AsyncMutex<YtmMatchDiagnostics>>,
    catalog_gate: Arc<Semaphore>,
    video_gate: Arc<Semaphore>,
    preflight_gate: Arc<Semaphore>,
}

impl SharedYtmMatchState {
    pub(crate) fn new(
        pace: Pacing,
        catalog_concurrency: usize,
        video_concurrency: usize,
        preflight_concurrency: usize,
    ) -> Self {
        Self {
            memo: Arc::new(AsyncMutex::new(HashMap::new())),
            query_memo: Arc::new(AsyncMutex::new(HashMap::new())),
            video_memo: Arc::new(AsyncMutex::new(HashMap::new())),
            pace: Arc::new(AsyncMutex::new(pace)),
            diagnostics: Arc::new(AsyncMutex::new(YtmMatchDiagnostics::default())),
            catalog_gate: Arc::new(Semaphore::new(catalog_concurrency.max(1))),
            video_gate: Arc::new(Semaphore::new(video_concurrency.max(1))),
            preflight_gate: Arc::new(Semaphore::new(preflight_concurrency.max(1))),
        }
    }

    pub(crate) async fn diagnostics(&self) -> YtmMatchDiagnostics {
        self.diagnostics.lock().await.clone()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct YtmMatchDiagnostics {
    pub catalog_searches: u32,
    pub video_searches: u32,
    pub preflight_lookups: u32,
    pub authenticated_catalog_degraded: u32,
}

/// Self-pacing between catalog searches. YTM default 600 ms (~1.6 rps), overridable via
/// `YTM_TRANSFER_PACE_MS` when a run trips throttling (or for the brave).
pub struct Pacing {
    min_interval: Duration,
    last: Option<Instant>,
}

impl Pacing {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last: None,
        }
    }

    pub fn ytm_default() -> Self {
        let ms = std::env::var("YTM_TRANSFER_PACE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(600);
        Self::new(Duration::from_millis(ms))
    }

    pub async fn tick(&mut self) {
        if let Some(last) = self.last {
            let since = last.elapsed();
            if since < self.min_interval {
                tokio::time::sleep(self.min_interval - since).await;
            }
        }
        self.last = Some(Instant::now());
    }
}

/// Memo key: repeated tracks across playlists/runs resolve once per engine run.
pub fn memo_key(input: &TrackInput) -> String {
    format!(
        "{}|{}",
        normalize(&input.artists.join(" ")),
        normalize_stripped(&input.title)
    )
}

/// Build the bounded YTM query plan for a source track.
///
/// Easy tracks still stop after the first successful query. The extra variants are only
/// reached when scoring is uncertain, and they target common Spotify-to-YouTube failure
/// modes: featured-artist credits, album-specific uploads, and artist romanization drift.
pub fn ytm_query_plan(input: &TrackInput) -> Vec<String> {
    let stripped_title = strip_annotations(&input.title);
    let stripped_title = stripped_title.trim();
    let original_title = input.title.trim();
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let first_artist = input
        .artists
        .first()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty());
    if let Some(artist) = first_artist {
        push_query_variant(&mut out, &mut seen, format!("{artist} {stripped_title}"));
    }

    let all_artists = input
        .artists
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !all_artists.is_empty() {
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{all_artists} {stripped_title}"),
        );
        if normalize(original_title) != normalize(stripped_title) {
            push_query_variant(
                &mut out,
                &mut seen,
                format!("{all_artists} {original_title}"),
            );
        }
    }

    let album_artists = input
        .album_artists
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !album_artists.is_empty() {
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{album_artists} {stripped_title}"),
        );
    }

    if let Some(album) = input
        .album
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        && normalize(album) != normalize(stripped_title)
    {
        if let Some(artist) = first_artist {
            push_query_variant(
                &mut out,
                &mut seen,
                format!("{artist} {stripped_title} {album}"),
            );
        }
        push_query_variant(&mut out, &mut seen, format!("{stripped_title} {album}"));
    }

    if let Some(year) = release_year(input) {
        if let Some(artist) = first_artist {
            push_query_variant(
                &mut out,
                &mut seen,
                format!("{artist} {stripped_title} {year}"),
            );
        }
        if !all_artists.is_empty() {
            push_query_variant(
                &mut out,
                &mut seen,
                format!("{all_artists} {stripped_title} {year}"),
            );
        }
    }

    if let Some(artist) = first_artist {
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{artist} {stripped_title} official audio"),
        );
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{artist} {stripped_title} topic"),
        );
    }

    if normalize(original_title) != normalize(stripped_title) {
        push_query_variant(&mut out, &mut seen, original_title.to_owned());
    }
    push_query_variant(&mut out, &mut seen, stripped_title.to_owned());
    out
}

pub fn ytm_catalog_query_plan(input: &TrackInput) -> Vec<String> {
    let stripped_title = strip_annotations(&input.title);
    let stripped_title = stripped_title.trim();
    let original_title = input.title.trim();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(artist) = input
        .artists
        .first()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
    {
        push_query_variant(&mut out, &mut seen, format!("{artist} {stripped_title}"));
        if normalize(original_title) != normalize(stripped_title) {
            push_query_variant(&mut out, &mut seen, format!("{artist} {original_title}"));
        }
        push_query_variant(&mut out, &mut seen, format!("{stripped_title} {artist}"));
    } else {
        push_query_variant(&mut out, &mut seen, stripped_title.to_owned());
    }
    out
}

pub fn ytm_fallback_query_plan(input: &TrackInput) -> Vec<String> {
    let fast: HashSet<String> = ytm_catalog_query_plan(input)
        .into_iter()
        .map(|query| normalize(&query))
        .collect();
    ytm_query_plan(input)
        .into_iter()
        .filter(|query| !fast.contains(&normalize(query)))
        .collect()
}

pub(crate) async fn match_track_ytm_shared(
    api: &YtMusicApi,
    input: &TrackInput,
    cfg: &MatchConfig,
    search_config: &SearchConfig,
    state: &SharedYtmMatchState,
) -> anyhow::Result<MatchOutcome> {
    if let Some(id) = &input.known_video_id {
        return Ok(MatchOutcome::Matched {
            key: id.clone(),
            score: 1.0,
            display: input.display(),
            title: Some(input.title.clone()),
            artist: Some(input.artists.join(", ")),
            album: input.album.clone(),
            duration_secs: input.duration_secs,
            score_breakdown: None,
        });
    }
    let key = memo_key(input);
    if let Some(hit) = state.memo.lock().await.get(&key).cloned() {
        return Ok(hit);
    }

    let mut candidates = Vec::<MatchCandidate>::new();
    let mut outcome = MatchOutcome::NotFound;
    for query in ytm_catalog_query_plan(input) {
        let songs = shared_catalog_songs(api, &query, search_config, state).await?;
        for (song, kind) in &songs {
            if !candidates.iter().any(|c| c.key == song.video_id) {
                candidates.push(MatchCandidate::from_song_with_kind(song, (*kind).into()));
            }
        }
        outcome = best_outcome(input, &candidates, cfg);
        if should_stop_ytm_search(&outcome, cfg) {
            break;
        }
    }

    if !matches!(outcome, MatchOutcome::Matched { .. }) {
        for query in ytm_fallback_query_plan(input) {
            let songs = shared_video_songs(api, &query, search_config, state).await?;
            for (song, kind) in &songs {
                if !candidates.iter().any(|c| c.key == song.video_id) {
                    candidates.push(MatchCandidate::from_song_with_kind(song, (*kind).into()));
                }
            }
            outcome = best_outcome(input, &candidates, cfg);
            if should_stop_ytm_search(&outcome, cfg) {
                break;
            }
        }
        let preflighted =
            preflight_ytm_candidates_shared(api, input, cfg, &mut candidates, state).await;
        state.diagnostics.lock().await.preflight_lookups += preflighted;
        outcome = best_outcome(input, &candidates, cfg);
    }

    state.memo.lock().await.insert(key, outcome.clone());
    Ok(outcome)
}

async fn shared_catalog_songs(
    api: &YtMusicApi,
    query: &str,
    search_config: &SearchConfig,
    state: &SharedYtmMatchState,
) -> anyhow::Result<Vec<(Song, YoutubeSearchKind)>> {
    let memo_key = format!("catalog:{query}");
    if let Some(hit) = state.query_memo.lock().await.get(&memo_key).cloned() {
        return Ok(hit);
    }
    let _permit = state
        .catalog_gate
        .acquire()
        .await
        .expect("catalog semaphore should not close");
    state.pace.lock().await.tick().await;
    let (songs, degraded) = api.search_transfer_catalog(query, search_config).await?;
    {
        let mut diagnostics = state.diagnostics.lock().await;
        diagnostics.catalog_searches += 1;
        if degraded {
            diagnostics.authenticated_catalog_degraded += 1;
        }
    }
    let songs = songs
        .into_iter()
        .map(|song| (song, YoutubeSearchKind::YtmCatalogSong))
        .collect::<Vec<_>>();
    state
        .query_memo
        .lock()
        .await
        .insert(memo_key, songs.clone());
    Ok(songs)
}

async fn shared_video_songs(
    api: &YtMusicApi,
    query: &str,
    search_config: &SearchConfig,
    state: &SharedYtmMatchState,
) -> anyhow::Result<Vec<(Song, YoutubeSearchKind)>> {
    let memo_key = format!("video:{query}");
    if let Some(hit) = state.query_memo.lock().await.get(&memo_key).cloned() {
        return Ok(hit);
    }
    let _permit = state
        .video_gate
        .acquire()
        .await
        .expect("video semaphore should not close");
    state.pace.lock().await.tick().await;
    let songs = api.search_transfer_video(query, search_config).await?;
    state.diagnostics.lock().await.video_searches += 1;
    let songs = songs
        .into_iter()
        .map(|song| (song, YoutubeSearchKind::YoutubeVideoSearch))
        .collect::<Vec<_>>();
    state
        .query_memo
        .lock()
        .await
        .insert(memo_key, songs.clone());
    Ok(songs)
}

const TRANSFER_PREFLIGHT_TOP_N: usize = 2;
const TRANSFER_EXHAUSTIVE_PREFLIGHT_TOP_N: usize = 5;

async fn preflight_ytm_candidates_shared(
    api: &YtMusicApi,
    input: &TrackInput,
    cfg: &MatchConfig,
    candidates: &mut [MatchCandidate],
    state: &SharedYtmMatchState,
) -> u32 {
    let mut ranked: Vec<(f32, usize)> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| needs_transfer_preflight(candidate))
        .map(|(idx, candidate)| {
            (
                score_candidate_breakdown_with_config(input, candidate, cfg).total,
                idx,
            )
        })
        .filter(|(score, _)| *score >= cfg.ambiguous_floor)
        .collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut lookups = 0;
    let preflight_top_n = match cfg.policy {
        MatchPolicy::Exhaustive => TRANSFER_EXHAUSTIVE_PREFLIGHT_TOP_N,
        _ => TRANSFER_PREFLIGHT_TOP_N,
    };
    for (_, idx) in ranked.into_iter().take(preflight_top_n) {
        let key = candidates[idx].key.clone();
        let meta = match state.video_memo.lock().await.get(&key).cloned() {
            Some(hit) => hit,
            None => {
                let _permit = state
                    .preflight_gate
                    .acquire()
                    .await
                    .expect("preflight semaphore should not close");
                lookups += 1;
                let hit = match api.youtube_video_metadata(&key).await {
                    Ok(meta) => Some(meta),
                    Err(error) => {
                        tracing::debug!(
                            video_id = %key,
                            error = %crate::util::sanitize::sanitize_error_text(format!("{error:#}")),
                            "transfer metadata preflight failed"
                        );
                        None
                    }
                };
                state
                    .video_memo
                    .lock()
                    .await
                    .insert(key.clone(), hit.clone());
                hit
            }
        };
        candidates[idx].preflighted = true;
        if let Some(meta) = meta {
            apply_transfer_preflight(input, &mut candidates[idx], &meta);
        }
    }
    lookups
}

fn needs_transfer_preflight(candidate: &MatchCandidate) -> bool {
    !candidate.preflighted
        && matches!(
            candidate.source_kind,
            CandidateSourceKind::YoutubeVideoSearch | CandidateSourceKind::Unknown
        )
}

fn apply_transfer_preflight(
    input: &TrackInput,
    candidate: &mut MatchCandidate,
    meta: &YtdlpVideoMeta,
) {
    if !meta.title.trim().is_empty() && candidate.title.trim().is_empty() {
        candidate.title = meta.title.clone();
    }
    if !meta.channel.trim().is_empty() {
        candidate.channel = Some(meta.channel.clone());
        if candidate.artist.trim().is_empty() {
            candidate.artist = meta.channel.clone();
        }
    }
    if candidate.duration_secs.is_none() {
        candidate.duration_secs = meta.duration_secs;
    }
    candidate
        .preflight_reason_codes
        .push("metadata_preflight".to_owned());

    if meta.is_live == Some(true)
        || matches!(
            meta.live_status.as_deref(),
            Some("is_live" | "is_upcoming" | "post_live")
        )
    {
        reject_preflight(candidate, "live_or_upcoming");
    }
    if matches!(meta.media_type.as_deref(), Some("playlist" | "multi_video")) {
        reject_preflight(candidate, "non_track_media");
    }
    if let Some(duration) = meta.duration_secs {
        if let Some(source_duration) = input.duration_secs {
            let delta = source_duration.abs_diff(duration);
            if delta > hard_duration_limit(input) {
                reject_preflight(candidate, "duration_mismatch");
            } else if delta > soft_duration_limit(input) {
                push_preflight_reason(candidate, "duration_mismatch");
            }
        } else if duration > 15 * 60 {
            reject_preflight(candidate, "long_mix_duration");
        }
    }
    let channel = meta.channel.as_str();
    let rich_title = match meta.description.as_deref() {
        Some(desc) if !desc.trim().is_empty() => format!("{} {}", meta.title, desc),
        _ => meta.title.clone(),
    };
    if let Some(reason) = musicgate::non_music_reason(&rich_title, channel) {
        reject_preflight(candidate, reason);
    }
}

fn push_preflight_reason(candidate: &mut MatchCandidate, reason: &str) {
    if !candidate
        .preflight_reason_codes
        .iter()
        .any(|code| code == reason)
    {
        candidate.preflight_reason_codes.push(reason.to_owned());
    }
}

fn reject_preflight(candidate: &mut MatchCandidate, reason: &str) {
    if candidate.preflight_reject_reason.is_none() {
        candidate.preflight_reject_reason = Some(reason.to_owned());
    }
    push_preflight_reason(candidate, reason);
}

fn should_stop_ytm_search(outcome: &MatchOutcome, cfg: &MatchConfig) -> bool {
    if cfg.policy == MatchPolicy::Exhaustive {
        return false;
    }
    match outcome {
        MatchOutcome::Matched {
            score_breakdown: Some(breakdown),
            score: total,
            ..
        } => {
            let quality_source = score_breakdown_has(breakdown, "catalog_song")
                || score_breakdown_has(breakdown, "trusted_channel")
                || score_breakdown_has(breakdown, "official_like");
            let target = match cfg.policy {
                MatchPolicy::Strict => 0.90,
                MatchPolicy::Balanced => cfg.accept.max(0.84),
                MatchPolicy::Aggressive => cfg.accept,
                MatchPolicy::Exhaustive => unreachable!("handled above"),
            };
            *total >= target && quality_source
        }
        MatchOutcome::Matched { .. } => true,
        _ => false,
    }
}

fn score_breakdown_has(score: &MatchScoreBreakdown, reason: &str) -> bool {
    score.reason_codes.iter().any(|r| r == reason)
}
