use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::api::Song;
use crate::api::ytmusic::{YoutubeSearchKind, YtMusicApi, YtdlpVideoMeta};
use crate::search_source::SearchConfig;
use crate::streaming::musicgate;
use crate::transfer::MatchPolicy;
use crate::transfer::checkpoint::{MatchTrace, ProbeTrace, ReportCandidate};

use super::{
    CandidateSourceKind, MatchCandidate, MatchConfig, MatchOutcome, MatchScoreBreakdown,
    TrackInput, best_outcome, hard_duration_limit, normalize, normalize_stripped,
    official_signal_score, push_query_variant, release_year, score_candidate_breakdown_with_config,
    similarity, soft_duration_limit, strip_annotations, title_artist_credit_prefix,
};

type QueryMemo = HashMap<String, Vec<(Song, YoutubeSearchKind)>>;
const OUTCOME_MEMO_MAX: usize = 8_192;
const QUERY_MEMO_MAX: usize = 2_048;
const VIDEO_MEMO_MAX: usize = 2_048;
const SINGLE_FLIGHT_LOCK_MAX: usize = 2_048;
const PUBLIC_QUERY_BUDGET: usize = 3;
/// This is a deadlock/systemic-stall guard, not a sum of provider timeouts. Four tracks share
/// paced catalog/public queues, so a 25-second wall-clock cap could expire a healthy track merely
/// while it waited behind its siblings. Individual video/public/preflight calls keep their much
/// shorter deadlines; this wider envelope allows the bounded quality stages to finish.
const TRACK_MATCH_DEADLINE: Duration = Duration::from_secs(60);
const PUBLIC_SEARCH_RETRY_DELAY: Duration = Duration::from_millis(1_200);
const PUBLIC_SEARCH_MAX_ATTEMPTS: u16 = 2;
const METADATA_PREFLIGHT_MAX_ATTEMPTS: u16 = 2;

#[derive(Clone)]
pub(crate) struct SharedYtmMatchState {
    memo: Arc<AsyncMutex<HashMap<String, MatchOutcome>>>,
    query_memo: Arc<AsyncMutex<QueryMemo>>,
    video_memo: Arc<AsyncMutex<HashMap<String, Option<YtdlpVideoMeta>>>>,
    persistent_query_keys: Arc<AsyncMutex<HashSet<String>>>,
    persistent_video_keys: Arc<AsyncMutex<HashSet<String>>>,
    query_locks: Arc<AsyncMutex<HashMap<String, Weak<AsyncMutex<()>>>>>,
    video_locks: Arc<AsyncMutex<HashMap<String, Weak<AsyncMutex<()>>>>>,
    catalog_pace: Arc<AsyncMutex<Pacing>>,
    public_pace: Arc<AsyncMutex<Pacing>>,
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
            persistent_query_keys: Arc::new(AsyncMutex::new(HashSet::new())),
            persistent_video_keys: Arc::new(AsyncMutex::new(HashSet::new())),
            query_locks: Arc::new(AsyncMutex::new(HashMap::new())),
            video_locks: Arc::new(AsyncMutex::new(HashMap::new())),
            catalog_pace: Arc::new(AsyncMutex::new(pace)),
            public_pace: Arc::new(AsyncMutex::new(Pacing::public_default())),
            diagnostics: Arc::new(AsyncMutex::new(YtmMatchDiagnostics::default())),
            catalog_gate: Arc::new(Semaphore::new(catalog_concurrency.max(1))),
            video_gate: Arc::new(Semaphore::new(video_concurrency.max(1))),
            preflight_gate: Arc::new(Semaphore::new(preflight_concurrency.max(1))),
        }
    }

    pub(crate) async fn diagnostics(&self) -> YtmMatchDiagnostics {
        self.diagnostics.lock().await.clone()
    }

    pub(crate) async fn seed_persistent_cache(
        &self,
        queries: Vec<(String, Vec<(Song, YoutubeSearchKind)>)>,
        videos: Vec<(String, YtdlpVideoMeta)>,
    ) {
        if !queries.is_empty() {
            let mut memo = self.query_memo.lock().await;
            let mut persistent = self.persistent_query_keys.lock().await;
            for (key, songs) in queries.into_iter().take(QUERY_MEMO_MAX) {
                persistent.insert(key.clone());
                memo.insert(key, songs);
            }
        }
        if !videos.is_empty() {
            let mut memo = self.video_memo.lock().await;
            let mut persistent = self.persistent_video_keys.lock().await;
            for (key, meta) in videos.into_iter().take(VIDEO_MEMO_MAX) {
                persistent.insert(key.clone());
                memo.insert(key, Some(meta));
            }
        }
    }

    pub(crate) async fn cache_snapshot(&self) -> YtmCacheSnapshot {
        let queries = self
            .query_memo
            .lock()
            .await
            .iter()
            .take(QUERY_MEMO_MAX)
            .map(|(key, songs)| (key.clone(), songs.clone()))
            .collect();
        let video_metadata = self
            .video_memo
            .lock()
            .await
            .iter()
            .take(VIDEO_MEMO_MAX)
            .filter_map(|(key, meta)| meta.clone().map(|meta| (key.clone(), meta)))
            .collect();
        YtmCacheSnapshot {
            queries,
            video_metadata,
        }
    }

    async fn outcome(&self, key: &str) -> Option<MatchOutcome> {
        self.memo.lock().await.get(key).cloned()
    }

    async fn remember_outcome(&self, key: String, outcome: MatchOutcome) {
        let mut memo = self.memo.lock().await;
        if memo.contains_key(&key) || memo.len() < OUTCOME_MEMO_MAX {
            memo.insert(key, outcome);
        }
    }

    async fn remember_query(&self, key: String, songs: Vec<(Song, YoutubeSearchKind)>) {
        let mut memo = self.query_memo.lock().await;
        if memo.contains_key(&key) || memo.len() < QUERY_MEMO_MAX {
            memo.insert(key, songs);
        }
    }

    async fn remember_video(&self, key: String, meta: Option<YtdlpVideoMeta>) {
        let mut memo = self.video_memo.lock().await;
        if memo.contains_key(&key) || memo.len() < VIDEO_MEMO_MAX {
            memo.insert(key, meta);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct YtmMatchDiagnostics {
    pub catalog_searches: u32,
    pub video_searches: u32,
    pub ytm_video_searches: u32,
    pub public_video_searches: u32,
    pub public_video_retries: u32,
    pub preflight_lookups: u32,
    pub preflight_retries: u32,
    pub preflight_failures: u32,
    pub authenticated_catalog_degraded: u32,
    pub query_cache_hits: u32,
    pub video_meta_cache_hits: u32,
}

pub(crate) struct YtmCacheSnapshot {
    pub queries: Vec<(String, Vec<(Song, YoutubeSearchKind)>)>,
    pub video_metadata: Vec<(String, YtdlpVideoMeta)>,
}

pub(crate) struct YtmMatchResult {
    pub outcome: MatchOutcome,
    pub trace: MatchTrace,
}

pub(crate) struct YtmMatchFailure {
    pub error: anyhow::Error,
    pub trace: MatchTrace,
}

impl YtmMatchFailure {
    fn new(error: anyhow::Error, trace: MatchTrace) -> Self {
        Self { error, trace }
    }
}

impl From<anyhow::Error> for YtmMatchFailure {
    fn from(error: anyhow::Error) -> Self {
        Self::new(error, MatchTrace::default())
    }
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

    /// Space public yt-dlp process starts independently from first-party YTM requests.
    /// Two searches may still overlap, but a large import cannot burst several extractor
    /// processes at YouTube in the same instant. Set `YTM_TRANSFER_PUBLIC_PACE_MS=0` to
    /// disable this guard for diagnostics.
    fn public_default() -> Self {
        let ms = std::env::var("YTM_TRANSFER_PUBLIC_PACE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
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

/// Memo key: repeated instances of the same source recording resolve once per engine run,
/// without aliasing different versions, releases, or source identities.
pub fn memo_key(input: &TrackInput) -> String {
    let artists = input
        .artists
        .iter()
        .map(|artist| normalize(artist))
        .collect::<Vec<_>>()
        .join("\u{1f}");
    format!(
        "source={}|isrc={}|artists={artists}|title={}|stripped={}|album={}|duration={}|album_id={}|disc={}|track={}",
        normalize(&input.source_key),
        input
            .isrc
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_uppercase(),
        normalize(&input.title),
        normalize_stripped(&input.title),
        input.album.as_deref().map(normalize).unwrap_or_default(),
        input
            .duration_secs
            .map_or_else(String::new, |value| value.to_string()),
        input.album_id.as_deref().map(normalize).unwrap_or_default(),
        input
            .disc_number
            .map_or_else(String::new, |value| value.to_string()),
        input
            .track_number
            .map_or_else(String::new, |value| value.to_string()),
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

/// Build the three-query public YouTube rescue plan.
///
/// Query deduplication is deliberately scoped to this backend. Even when the YTM song
/// catalog already tried `artist title`, public YouTube may expose a different (and often
/// official) result set, so the same text remains the highest-recall first public query.
pub fn ytm_fallback_query_plan(input: &TrackInput) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let stripped_title = strip_annotations(&input.title);
    let stripped_title = stripped_title.trim();
    let first_artist = input
        .artists
        .first()
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty());

    if let Some(artist) = first_artist {
        let base = format!("{artist} {stripped_title}");
        push_query_variant(&mut out, &mut seen, base.clone());
        push_query_variant(&mut out, &mut seen, format!("{base} official audio"));
        // Drop the source artist spelling for the final official-video probe. Official
        // channels and titles often use a native-script name while Spotify exposes a Latin
        // transliteration (for example Aimyon vs. あいみょん); repeating the Latin artist in
        // every query can hide the real official upload. Ranking still requires independent
        // artist/provenance evidence, so this raises recall without relaxing acceptance.
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{stripped_title} official music video"),
        );
    } else {
        let base = input
            .album
            .as_deref()
            .map(str::trim)
            .filter(|album| !album.is_empty())
            .map_or_else(
                || stripped_title.to_owned(),
                |album| format!("{stripped_title} {album}"),
            );
        push_query_variant(&mut out, &mut seen, base);
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{stripped_title} official audio"),
        );
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{stripped_title} official music video"),
        );
    }
    out
}

/// Whether a failed public-video query is equivalent to a successful empty result.
///
/// yt-dlp already returns `Ok([])` for a valid search with no rows. Process failures,
/// timeouts, HTTP rejection, and malformed output are provider failures and must remain
/// errors so the engine's consecutive-failure circuit can stop an unhealthy import.
pub(crate) fn video_search_error_is_soft(_err: &anyhow::Error) -> bool {
    false
}

fn public_video_search_error_kind(error: &anyhow::Error) -> &'static str {
    let detail = format!("{error:#}").to_ascii_lowercase();
    if detail.contains("http error 403") || detail.contains("403 forbidden") {
        "http_403"
    } else if detail.contains("http error 429")
        || detail.contains("too many requests")
        || detail.contains("rate-limited")
    {
        "http_429"
    } else if detail.contains("timed out") || detail.contains("timeout") {
        "timeout"
    } else if detail.contains("unable to download api page")
        || detail.contains("connection reset")
        || detail.contains("connection refused")
        || detail.contains("temporary failure")
    {
        "network"
    } else if detail.contains("invalid json") || detail.contains("malformed") {
        "invalid_response"
    } else if detail.contains("not found") && detail.contains("yt-dlp") {
        "tool_missing"
    } else {
        "provider_error"
    }
}

fn public_video_search_is_retryable(error: &anyhow::Error) -> bool {
    matches!(
        public_video_search_error_kind(error),
        "http_403" | "http_429" | "network"
    )
}

struct SharedPublicVideos {
    songs: Vec<(Song, YoutubeSearchKind)>,
    attempts: u16,
}

struct PublicSearchFailure {
    error: anyhow::Error,
    attempts: u16,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PreflightSummary {
    attempts: u32,
    successes: u32,
    failures: u32,
    first_error_kind: Option<String>,
}

impl PreflightSummary {
    fn status(&self) -> &'static str {
        if self.failures == 0 {
            "success"
        } else if self.successes > 0 {
            "partial"
        } else {
            "error"
        }
    }
}

enum SharedCatalogSongs {
    Complete(Vec<(Song, YoutubeSearchKind)>),
    Unavailable,
}

enum SharedCatalogVideos {
    Complete(Vec<(Song, YoutubeSearchKind)>),
    Unavailable,
}

fn provider_scope(api: &YtMusicApi) -> &'static str {
    match api {
        YtMusicApi::Browser(_) => "browser",
        YtMusicApi::Anonymous => "anonymous",
    }
}

pub(crate) async fn match_track_ytm_shared(
    api: &YtMusicApi,
    input: &TrackInput,
    cfg: &MatchConfig,
    search_config: &SearchConfig,
    state: &SharedYtmMatchState,
) -> Result<YtmMatchResult, YtmMatchFailure> {
    let mut trace = MatchTrace::default();
    match tokio::time::timeout(
        TRACK_MATCH_DEADLINE,
        match_track_ytm_shared_inner(api, input, cfg, search_config, state, &mut trace),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            trace.push_probe(ProbeTrace {
                backend: "matching_budget".to_owned(),
                intent: "bounded_track_match".to_owned(),
                status: "error".to_owned(),
                elapsed_ms: TRACK_MATCH_DEADLINE.as_millis() as u64,
                attempt: 1,
                error_kind: Some("deadline_exceeded".to_owned()),
                ..ProbeTrace::default()
            });
            trace.terminal_reason = Some("provider_budget_exhausted".to_owned());
            Err(YtmMatchFailure::new(
                anyhow::anyhow!(
                    "transfer matching exceeded the per-track {} second provider budget",
                    TRACK_MATCH_DEADLINE.as_secs()
                ),
                trace,
            ))
        }
    }
}

async fn match_track_ytm_shared_inner(
    api: &YtMusicApi,
    input: &TrackInput,
    cfg: &MatchConfig,
    search_config: &SearchConfig,
    state: &SharedYtmMatchState,
    trace: &mut MatchTrace,
) -> Result<YtmMatchResult, YtmMatchFailure> {
    if let Some(id) = &input.known_video_id {
        let outcome = MatchOutcome::Matched {
            key: id.clone(),
            score: 1.0,
            display: input.display(),
            title: Some(input.title.clone()),
            artist: Some(input.artists.join(", ")),
            album: input.album.clone(),
            duration_secs: input.duration_secs,
            score_breakdown: None,
        };
        trace.terminal_reason = Some("known_video_id".to_owned());
        return Ok(YtmMatchResult {
            outcome,
            trace: trace.clone(),
        });
    }
    let key = memo_key(input);
    if let Some(hit) = state.outcome(&key).await {
        trace.terminal_reason = Some("in_run_memo_hit".to_owned());
        return Ok(YtmMatchResult {
            outcome: hit,
            trace: trace.clone(),
        });
    }

    let mut candidates = Vec::<MatchCandidate>::new();
    let mut candidate_keys = HashSet::new();
    let mut outcome = MatchOutcome::NotFound;
    for query in ytm_catalog_query_plan(input) {
        let started = Instant::now();
        let songs = match shared_catalog_songs(api, &query, search_config, state)
            .await
            .map_err(|error| YtmMatchFailure::new(error, trace.clone()))?
        {
            SharedCatalogSongs::Complete(songs) => {
                trace.push_probe(ProbeTrace {
                    backend: "ytm_song".to_owned(),
                    intent: "primary_or_targeted".to_owned(),
                    query: query.clone(),
                    status: if songs.is_empty() { "empty" } else { "success" }.to_owned(),
                    result_count: songs.len() as u32,
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    attempt: 1,
                    ..ProbeTrace::default()
                });
                songs
            }
            SharedCatalogSongs::Unavailable => {
                trace.push_probe(ProbeTrace {
                    backend: "ytm_song".to_owned(),
                    intent: "primary_or_targeted".to_owned(),
                    query,
                    status: "unavailable".to_owned(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    attempt: 1,
                    ..ProbeTrace::default()
                });
                break;
            }
        };
        for (song, kind) in &songs {
            if candidate_keys.insert(song.video_id.clone()) {
                candidates.push(MatchCandidate::from_song_with_kind(song, (*kind).into()));
            }
        }
        outcome = best_outcome(input, &candidates, cfg);
        if should_stop_ytm_search(&outcome, cfg) {
            break;
        }
    }

    // Music-scoped video rescue precedes generic public search. A VideosFilter row is
    // deliberately review-only until finalist metadata confirms authority/playability.
    if !matches!(outcome, MatchOutcome::Matched { .. }) {
        for query in ytm_catalog_query_plan(input).into_iter().take(2) {
            let started = Instant::now();
            let songs = match shared_ytm_video_songs(api, &query, search_config, state)
                .await
                .map_err(|error| YtmMatchFailure::new(error, trace.clone()))?
            {
                SharedCatalogVideos::Complete(songs) => {
                    trace.push_probe(ProbeTrace {
                        backend: "ytm_video".to_owned(),
                        intent: "music_scoped_rescue".to_owned(),
                        query: query.clone(),
                        status: if songs.is_empty() { "empty" } else { "success" }.to_owned(),
                        result_count: songs.len() as u32,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        attempt: 1,
                        ..ProbeTrace::default()
                    });
                    songs
                }
                SharedCatalogVideos::Unavailable => {
                    trace.push_probe(ProbeTrace {
                        backend: "ytm_video".to_owned(),
                        intent: "music_scoped_rescue".to_owned(),
                        query,
                        status: "unavailable".to_owned(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        attempt: 1,
                        ..ProbeTrace::default()
                    });
                    break;
                }
            };
            for (song, kind) in &songs {
                if candidate_keys.insert(song.video_id.clone()) {
                    candidates.push(MatchCandidate::from_song_with_kind(song, (*kind).into()));
                }
            }
            outcome = best_outcome(input, &candidates, cfg);
            if songs.is_empty() {
                continue;
            }
            let preflight_started = Instant::now();
            let preflight =
                preflight_ytm_candidates_shared(api, input, cfg, &mut candidates, state, None)
                    .await;
            push_preflight_probe(trace, "ytm_video_finalist", preflight_started, &preflight);
            outcome = best_outcome(input, &candidates, cfg);
            if should_stop_ytm_search(&outcome, cfg) {
                break;
            }
        }
    }

    if !matches!(outcome, MatchOutcome::Matched { .. }) {
        for query in ytm_fallback_query_plan(input)
            .into_iter()
            .take(PUBLIC_QUERY_BUDGET)
        {
            let started = Instant::now();
            let search = match shared_video_songs(api, &query, search_config, state).await {
                Ok(search) => search,
                Err(failure) => {
                    let error_kind = public_video_search_error_kind(&failure.error);
                    trace.push_probe(ProbeTrace {
                        backend: "public_youtube".to_owned(),
                        intent: "bounded_rescue".to_owned(),
                        query: query.clone(),
                        status: "error".to_owned(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        attempt: failure.attempts,
                        error_kind: Some(error_kind.to_owned()),
                        ..ProbeTrace::default()
                    });
                    if video_search_error_is_soft(&failure.error) {
                        tracing::warn!(
                            query = %query,
                            error = %crate::util::sanitize::sanitize_error_text(format!("{:#}", failure.error)),
                            "transfer video query was rejected; continuing with remaining fallback queries"
                        );
                        continue;
                    }
                    trace.set_candidates(trace_candidates(input, cfg, &candidates));
                    trace.terminal_reason = Some("public_youtube_provider_error".to_owned());
                    let error = failure.error.context(format!(
                        "public YouTube transfer search failed for query {query:?}"
                    ));
                    return Err(YtmMatchFailure::new(error, trace.clone()));
                }
            };
            let songs = search.songs;
            trace.push_probe(ProbeTrace {
                backend: "public_youtube".to_owned(),
                intent: "bounded_rescue".to_owned(),
                query: query.clone(),
                status: if songs.is_empty() { "empty" } else { "success" }.to_owned(),
                result_count: songs.len() as u32,
                elapsed_ms: started.elapsed().as_millis() as u64,
                attempt: search.attempts,
                ..ProbeTrace::default()
            });
            for (song, kind) in &songs {
                if candidate_keys.insert(song.video_id.clone()) {
                    candidates.push(MatchCandidate::from_song_with_kind(song, (*kind).into()));
                }
            }
            let provider_first = songs.first().and_then(|(song, _)| {
                candidates
                    .iter()
                    .position(|candidate| candidate.key == song.video_id)
                    .filter(|idx| official_signal_score(&candidates[*idx]) >= 0.7)
            });
            // `--allow-user-videos` relaxes provenance, not hard safety. Always preflight the
            // best auto-eligible public finalist before accepting it so full metadata can still
            // reject live/private/no-audio/description-only cover and low-quality uploads.
            let preflight_started = Instant::now();
            let preflight = preflight_ytm_candidates_shared(
                api,
                input,
                cfg,
                &mut candidates,
                state,
                provider_first,
            )
            .await;
            push_preflight_probe(
                trace,
                "public_video_finalist",
                preflight_started,
                &preflight,
            );
            outcome = best_outcome(input, &candidates, cfg);
            if should_stop_ytm_search(&outcome, cfg) {
                break;
            }
        }
    }

    trace.set_candidates(trace_candidates(input, cfg, &candidates));
    trace.terminal_reason = Some(
        match &outcome {
            MatchOutcome::Matched { .. } => "matched",
            MatchOutcome::Ambiguous { .. } => "ambiguous",
            MatchOutcome::NotFound => "successful_exhaustion",
            MatchOutcome::SkippedLocal => "skipped_local",
            MatchOutcome::SkippedCapacity => "destination_capacity",
        }
        .to_owned(),
    );
    state.remember_outcome(key, outcome.clone()).await;
    Ok(YtmMatchResult {
        outcome,
        trace: trace.clone(),
    })
}

fn push_preflight_probe(
    trace: &mut MatchTrace,
    intent: &str,
    started: Instant,
    summary: &PreflightSummary,
) {
    if summary.attempts == 0 {
        return;
    }
    trace.push_probe(ProbeTrace {
        backend: "youtube_metadata".to_owned(),
        intent: intent.to_owned(),
        status: summary.status().to_owned(),
        result_count: summary.successes,
        elapsed_ms: started.elapsed().as_millis() as u64,
        attempt: u16::try_from(summary.attempts).unwrap_or(u16::MAX),
        error_kind: summary.first_error_kind.clone(),
        ..ProbeTrace::default()
    });
}

fn trace_candidates(
    input: &TrackInput,
    cfg: &MatchConfig,
    candidates: &[MatchCandidate],
) -> Vec<ReportCandidate> {
    let mut candidates = candidates
        .iter()
        .map(|candidate| {
            let score = score_candidate_breakdown_with_config(input, candidate, cfg);
            ReportCandidate {
                key: candidate.key.clone(),
                score: score.total,
                display: format!("{} — {}", candidate.artist, candidate.title),
                score_breakdown: Some(score),
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.score.total_cmp(&left.score));
    candidates
}

async fn shared_catalog_songs(
    api: &YtMusicApi,
    query: &str,
    search_config: &SearchConfig,
    state: &SharedYtmMatchState,
) -> anyhow::Result<SharedCatalogSongs> {
    let memo_key = format!("v3:{}:ytm-song:{}", provider_scope(api), normalize(query));
    if let Some(hit) = cached_query(state, &memo_key).await {
        count_persistent_query_hit(state, &memo_key).await;
        return Ok(SharedCatalogSongs::Complete(hit));
    }
    let lock = query_lock(state, &memo_key).await;
    let _guard = lock.lock().await;
    if let Some(hit) = cached_query(state, &memo_key).await {
        count_persistent_query_hit(state, &memo_key).await;
        return Ok(SharedCatalogSongs::Complete(hit));
    }
    // Keep persistent catalog hits usable in anonymous/degraded runs, but do not take a
    // semaphore slot or the shared pacing delay when a live catalog request is impossible.
    if !api.transfer_catalog_available() {
        return Ok(SharedCatalogSongs::Unavailable);
    }
    let _permit = state
        .catalog_gate
        .acquire()
        .await
        .expect("catalog semaphore should not close");
    // Another concurrent query may have opened the authenticated-parser cooldown while
    // this one waited for the gate.
    if !api.transfer_catalog_available() {
        return Ok(SharedCatalogSongs::Unavailable);
    }
    state.catalog_pace.lock().await.tick().await;
    let (songs, degraded) = api.search_transfer_catalog(query, search_config).await?;
    {
        let mut diagnostics = state.diagnostics.lock().await;
        diagnostics.catalog_searches += 1;
        if degraded {
            diagnostics.authenticated_catalog_degraded += 1;
        }
    }
    if degraded {
        // A parser/provider failure is not a successful negative lookup. Do not memoize it,
        // and let the same primary query cross the provider boundary into public fallback.
        return Ok(SharedCatalogSongs::Unavailable);
    }
    let songs = songs
        .into_iter()
        .map(|song| (song, YoutubeSearchKind::YtmCatalogSong))
        .collect::<Vec<_>>();
    state.remember_query(memo_key, songs.clone()).await;
    Ok(SharedCatalogSongs::Complete(songs))
}

async fn shared_video_songs(
    api: &YtMusicApi,
    query: &str,
    search_config: &SearchConfig,
    state: &SharedYtmMatchState,
) -> Result<SharedPublicVideos, PublicSearchFailure> {
    let memo_key = format!("v3:public:youtube-video:{}", normalize(query));
    if let Some(hit) = cached_query(state, &memo_key).await {
        count_persistent_query_hit(state, &memo_key).await;
        return Ok(SharedPublicVideos {
            songs: hit,
            attempts: 1,
        });
    }
    let lock = query_lock(state, &memo_key).await;
    let _guard = lock.lock().await;
    if let Some(hit) = cached_query(state, &memo_key).await {
        count_persistent_query_hit(state, &memo_key).await;
        return Ok(SharedPublicVideos {
            songs: hit,
            attempts: 1,
        });
    }
    let mut attempt = 1_u16;
    let songs = loop {
        let permit = state
            .video_gate
            .acquire()
            .await
            .expect("video semaphore should not close");
        state.public_pace.lock().await.tick().await;
        let result = api.search_transfer_video(query, search_config).await;
        // A retry must not occupy scarce public-search capacity during its backoff.
        drop(permit);
        {
            let mut diagnostics = state.diagnostics.lock().await;
            diagnostics.video_searches = diagnostics.video_searches.saturating_add(1);
            diagnostics.public_video_searches = diagnostics.public_video_searches.saturating_add(1);
        }
        match result {
            Ok(songs) => break songs,
            Err(error)
                if attempt < PUBLIC_SEARCH_MAX_ATTEMPTS
                    && public_video_search_is_retryable(&error) =>
            {
                let mut diagnostics = state.diagnostics.lock().await;
                diagnostics.public_video_retries =
                    diagnostics.public_video_retries.saturating_add(1);
                drop(diagnostics);
                tracing::warn!(
                    query = %query,
                    attempt,
                    error_kind = public_video_search_error_kind(&error),
                    error = %crate::util::sanitize::sanitize_error_text(format!("{error:#}")),
                    retry_after_ms = PUBLIC_SEARCH_RETRY_DELAY.as_millis(),
                    "transient public YouTube search failure; retrying once"
                );
                tokio::time::sleep(PUBLIC_SEARCH_RETRY_DELAY).await;
                attempt = attempt.saturating_add(1);
            }
            Err(error) => {
                return Err(PublicSearchFailure {
                    error,
                    attempts: attempt,
                });
            }
        }
    };
    // Cache successful empty searches, but never cache a process/network/provider failure
    // as an empty result. The latter must remain retryable and visible to the engine circuit.
    let songs = songs
        .into_iter()
        .map(|song| (song, YoutubeSearchKind::YoutubeVideoSearch))
        .collect::<Vec<_>>();
    state.remember_query(memo_key, songs.clone()).await;
    Ok(SharedPublicVideos {
        songs,
        attempts: attempt,
    })
}

async fn shared_ytm_video_songs(
    api: &YtMusicApi,
    query: &str,
    search_config: &SearchConfig,
    state: &SharedYtmMatchState,
) -> anyhow::Result<SharedCatalogVideos> {
    let memo_key = format!("v3:{}:ytm-video:{}", provider_scope(api), normalize(query));
    if let Some(hit) = cached_query(state, &memo_key).await {
        count_persistent_query_hit(state, &memo_key).await;
        return Ok(SharedCatalogVideos::Complete(hit));
    }
    let lock = query_lock(state, &memo_key).await;
    let _guard = lock.lock().await;
    if let Some(hit) = cached_query(state, &memo_key).await {
        count_persistent_query_hit(state, &memo_key).await;
        return Ok(SharedCatalogVideos::Complete(hit));
    }
    if !api.transfer_video_catalog_available() {
        return Ok(SharedCatalogVideos::Unavailable);
    }
    let _permit = state
        .catalog_gate
        .acquire()
        .await
        .expect("catalog semaphore should not close");
    if !api.transfer_video_catalog_available() {
        return Ok(SharedCatalogVideos::Unavailable);
    }
    state.catalog_pace.lock().await.tick().await;
    let (songs, degraded) = api
        .search_transfer_video_catalog(query, search_config)
        .await?;
    {
        let mut diagnostics = state.diagnostics.lock().await;
        diagnostics.video_searches += 1;
        diagnostics.ytm_video_searches += 1;
    }
    if degraded {
        return Ok(SharedCatalogVideos::Unavailable);
    }
    let songs = songs
        .into_iter()
        .map(|song| (song, YoutubeSearchKind::YtmCatalogVideo))
        .collect::<Vec<_>>();
    state.remember_query(memo_key, songs.clone()).await;
    Ok(SharedCatalogVideos::Complete(songs))
}

async fn query_lock(state: &SharedYtmMatchState, key: &str) -> Arc<AsyncMutex<()>> {
    let mut locks = state.query_locks.lock().await;
    single_flight_lock(&mut locks, key)
}

async fn video_lock(state: &SharedYtmMatchState, key: &str) -> Arc<AsyncMutex<()>> {
    let mut locks = state.video_locks.lock().await;
    single_flight_lock(&mut locks, key)
}

fn single_flight_lock(
    locks: &mut HashMap<String, Weak<AsyncMutex<()>>>,
    key: &str,
) -> Arc<AsyncMutex<()>> {
    if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
        return lock;
    }
    if locks.len() >= SINGLE_FLIGHT_LOCK_MAX {
        locks.retain(|_, lock| lock.strong_count() > 0);
    }
    let lock = Arc::new(AsyncMutex::new(()));
    if locks.len() < SINGLE_FLIGHT_LOCK_MAX {
        locks.insert(key.to_owned(), Arc::downgrade(&lock));
    }
    lock
}

async fn count_persistent_query_hit(state: &SharedYtmMatchState, key: &str) {
    if state.persistent_query_keys.lock().await.remove(key) {
        state.diagnostics.lock().await.query_cache_hits += 1;
    }
}

/// Clone a query-cache entry behind a short lexical guard. Awaiting diagnostics while a
/// temporary `query_memo` guard is still alive can otherwise retain the cache lock longer than
/// intended and makes a cache hit vulnerable to future lock-order changes.
async fn cached_query(
    state: &SharedYtmMatchState,
    key: &str,
) -> Option<Vec<(Song, YoutubeSearchKind)>> {
    let memo = state.query_memo.lock().await;
    memo.get(key).cloned()
}

async fn count_persistent_video_hit(state: &SharedYtmMatchState, key: &str) {
    if state.persistent_video_keys.lock().await.remove(key) {
        state.diagnostics.lock().await.video_meta_cache_hits += 1;
    }
}

/// Clone a metadata-cache entry behind a short lexical guard. Keeping this lookup separate is
/// important: matching directly on `video_memo.lock().await.get(...)` extends the temporary
/// guard through the entire `match` arm, and the cache-miss arm then deadlocks when it checks the
/// same mutex again after acquiring its single-flight lock.
async fn cached_video_metadata(state: &SharedYtmMatchState, key: &str) -> Option<YtdlpVideoMeta> {
    let mut memo = state.video_memo.lock().await;
    match memo.get(key).cloned() {
        Some(Some(meta)) if usable_transfer_preflight(&meta) => Some(meta),
        // Old/partial cache rows must not be treated as verified metadata. Remove them so a
        // fresh provider lookup can repair the evidence instead of silently auto-accepting it.
        Some(_) => {
            memo.remove(key);
            None
        }
        None => None,
    }
}

const TRANSFER_PREFLIGHT_TOP_N: usize = 1;
const TRANSFER_EXHAUSTIVE_PREFLIGHT_TOP_N: usize = 3;

async fn preflight_ytm_candidates_shared(
    api: &YtMusicApi,
    input: &TrackInput,
    cfg: &MatchConfig,
    candidates: &mut [MatchCandidate],
    state: &SharedYtmMatchState,
    provider_first: Option<usize>,
) -> PreflightSummary {
    let mut ranked: Vec<(bool, f32, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(idx, candidate)| {
            transfer_preflight_rank(input, candidate, cfg)
                .map(|score| (provider_first == Some(idx), score, idx))
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut summary = PreflightSummary::default();
    let preflight_top_n = match cfg.policy {
        MatchPolicy::Exhaustive => TRANSFER_EXHAUSTIVE_PREFLIGHT_TOP_N,
        _ => TRANSFER_PREFLIGHT_TOP_N,
    };
    for (_, _, idx) in ranked.into_iter().take(preflight_top_n) {
        let key = candidates[idx].key.clone();
        let meta = match cached_video_metadata(state, &key).await {
            Some(hit) => {
                count_persistent_video_hit(state, &key).await;
                Some(hit)
            }
            None => {
                let lock = video_lock(state, &key).await;
                let _guard = lock.lock().await;
                if let Some(hit) = cached_video_metadata(state, &key).await {
                    count_persistent_video_hit(state, &key).await;
                    Some(hit)
                } else {
                    match fetch_transfer_preflight(api, &key, state, &mut summary).await {
                        Ok(meta) => {
                            state.remember_video(key.clone(), Some(meta.clone())).await;
                            summary.successes = summary.successes.saturating_add(1);
                            Some(meta)
                        }
                        Err(error) => {
                            let error_kind = public_video_search_error_kind(&error);
                            summary.failures = summary.failures.saturating_add(1);
                            summary
                                .first_error_kind
                                .get_or_insert_with(|| error_kind.to_owned());
                            {
                                let mut diagnostics = state.diagnostics.lock().await;
                                diagnostics.preflight_failures =
                                    diagnostics.preflight_failures.saturating_add(1);
                            }
                            push_preflight_reason(
                                &mut candidates[idx],
                                "metadata_preflight_failed",
                            );
                            tracing::warn!(
                                video_id = %key,
                                error_kind,
                                error = %crate::util::sanitize::sanitize_error_text(format!("{error:#}")),
                                "transfer metadata preflight unavailable; keeping candidate review-only"
                            );
                            None
                        }
                    }
                }
            }
        };
        if let Some(meta) = meta {
            candidates[idx].preflighted = true;
            apply_transfer_preflight(input, &mut candidates[idx], &meta);
        }
    }
    summary
}

fn transfer_preflight_rank(
    input: &TrackInput,
    candidate: &MatchCandidate,
    cfg: &MatchConfig,
) -> Option<f32> {
    if !needs_transfer_preflight(candidate, cfg) {
        return None;
    }
    let score = score_candidate_breakdown_with_config(input, candidate, cfg).total;
    // A localized flat-search title can make a real official upload appear weak until full
    // metadata reveals its native title/channel. Let one official-like finalist through that
    // bootstrap boundary; acceptance still happens only after metadata is reapplied and scored.
    (score >= cfg.ambiguous_floor || official_signal_score(candidate) >= 0.7).then_some(score)
}

async fn fetch_transfer_preflight(
    api: &YtMusicApi,
    key: &str,
    state: &SharedYtmMatchState,
    summary: &mut PreflightSummary,
) -> anyhow::Result<YtdlpVideoMeta> {
    let mut attempt = 1_u16;
    loop {
        let permit = state
            .preflight_gate
            .acquire()
            .await
            .expect("preflight semaphore should not close");
        // Public search and metadata extraction share a start-rate budget; otherwise their
        // independent concurrency limits can still burst five yt-dlp processes at once.
        state.public_pace.lock().await.tick().await;
        summary.attempts = summary.attempts.saturating_add(1);
        {
            let mut diagnostics = state.diagnostics.lock().await;
            diagnostics.preflight_lookups = diagnostics.preflight_lookups.saturating_add(1);
        }
        let result = api.youtube_video_metadata(key).await.and_then(|meta| {
            usable_transfer_preflight(&meta)
                .then_some(meta)
                .ok_or_else(|| {
                    anyhow::anyhow!("YouTube metadata response lacked usable audio evidence")
                })
        });
        // Neither the provider permit nor the start-rate mutex may be held during backoff.
        drop(permit);
        match result {
            Ok(meta) => return Ok(meta),
            Err(error)
                if attempt < METADATA_PREFLIGHT_MAX_ATTEMPTS
                    && public_video_search_is_retryable(&error) =>
            {
                {
                    let mut diagnostics = state.diagnostics.lock().await;
                    diagnostics.preflight_retries = diagnostics.preflight_retries.saturating_add(1);
                }
                tokio::time::sleep(PUBLIC_SEARCH_RETRY_DELAY).await;
                attempt = attempt.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    }
}

fn usable_transfer_preflight(meta: &YtdlpVideoMeta) -> bool {
    let has_audio_evidence = meta.audio.available_audio_formats.is_some()
        || meta.audio.available_audio_only_formats.is_some()
        || meta
            .audio
            .selected_audio_codec
            .as_ref()
            .is_some_and(|codec| !codec.trim().is_empty());
    !meta.title.trim().is_empty() && !meta.channel.trim().is_empty() && has_audio_evidence
}

fn needs_transfer_preflight(candidate: &MatchCandidate, cfg: &MatchConfig) -> bool {
    let missing_core_metadata = candidate.duration_secs.is_none()
        || candidate
            .channel
            .as_ref()
            .is_none_or(|channel| channel.trim().is_empty());
    !candidate.preflighted
        && !candidate
            .preflight_reason_codes
            .iter()
            .any(|reason| reason == "metadata_preflight_failed")
        && matches!(
            candidate.source_kind,
            CandidateSourceKind::YtmCatalogVideo
                | CandidateSourceKind::YoutubeVideoSearch
                | CandidateSourceKind::Unknown
        )
        && (candidate.source_kind == CandidateSourceKind::YtmCatalogVideo
            || cfg.allow_user_videos
            || cfg.policy == MatchPolicy::Aggressive
            || official_signal_score(candidate) >= 0.7
            || missing_core_metadata)
}

fn apply_transfer_preflight(
    input: &TrackInput,
    candidate: &mut MatchCandidate,
    meta: &YtdlpVideoMeta,
) {
    if metadata_title_credits_channel(meta) {
        push_preflight_reason(candidate, "metadata_title_channel_corroborated");
    }
    if !meta.title.trim().is_empty() {
        candidate.metadata_title = Some(meta.title.clone());
        if candidate.title.trim().is_empty() {
            candidate.title = meta.title.clone();
        }
    }
    if !meta.channel.trim().is_empty() {
        candidate.channel = Some(meta.channel.clone());
        if candidate.artist.trim().is_empty() {
            candidate.artist = meta.channel.clone();
        }
    }
    candidate.channel_id = meta.channel_id.clone().or_else(|| meta.uploader_id.clone());
    candidate.channel_verified = meta.channel_is_verified;
    candidate.availability = meta.availability.clone();
    candidate.has_audio_format = meta
        .audio
        .available_audio_formats
        .map(|count| count > 0)
        .or_else(|| meta.audio.selected_audio_codec.as_ref().map(|_| true));
    candidate.max_audio_bitrate_kbps = meta
        .audio
        .max_audio_bitrate_kbps
        .or(meta.audio.selected_audio_bitrate_kbps);
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
    if matches!(
        meta.availability.as_deref(),
        Some("private" | "needs_auth" | "premium_only" | "subscriber_only")
    ) {
        reject_preflight(candidate, "unavailable_video");
    }
    if candidate.has_audio_format == Some(false) {
        reject_preflight(candidate, "no_audio_format");
    } else if candidate
        .max_audio_bitrate_kbps
        .is_some_and(|bitrate| bitrate < 48.0)
        && !musicgate::is_trusted_music_channel(&meta.channel)
    {
        push_preflight_reason(candidate, "low_audio_ceiling");
    }
    if meta.channel_is_verified == Some(true) {
        push_preflight_reason(candidate, "verified_channel_corroboration");
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

fn metadata_title_credits_channel(meta: &YtdlpVideoMeta) -> bool {
    let Some(prefix) = title_artist_credit_prefix(&meta.title) else {
        return false;
    };
    let prefix = normalize(prefix);
    let channel = normalize(&meta.channel);
    !prefix.is_empty()
        && !channel.is_empty()
        && (prefix == channel || similarity(&prefix, &channel) >= 0.92)
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
            // Under authenticated-catalog degrade, video-only matches never carry
            // catalog_song/trusted_channel/official_like. Requiring quality_source then
            // forces every fallback query (multi-second yt-dlp each) and looks hung.
            // Strict still wants a quality signal; Balanced/Aggressive stop on score alone.
            match cfg.policy {
                MatchPolicy::Strict => *total >= target && quality_source,
                MatchPolicy::Balanced | MatchPolicy::Aggressive => *total >= target,
                MatchPolicy::Exhaustive => unreachable!("handled above"),
            }
        }
        MatchOutcome::Matched { .. } => true,
        _ => false,
    }
}

fn score_breakdown_has(score: &MatchScoreBreakdown, reason: &str) -> bool {
    score.reason_codes.iter().any(|r| r == reason)
}

#[cfg(test)]
mod retrieval_tests;
