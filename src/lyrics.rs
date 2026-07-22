//! Synced lyrics: an LRC parser, a current-line lookup, and the fetch actor.
//!
//! Lyrics come from [lrclib.net](https://lrclib.net), using `/api/get` followed by bounded
//! `/api/search` fallbacks. Responses carry `syncedLyrics` in LRC format (`[mm:ss.xx] text`). We
//! parse that into time-stamped [`LyricLine`]s; the TUI's 100 ms lyric clock binary-searches the
//! current line from interpolated playback position and stores it for rendering. The actor caches
//! by `video_id` so re-opening the panel (or replaying) costs no network.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::mpsc;

use crate::util::backpressure::{LYRICS_QUEUE, bounded_channel};
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::util::{http, sanitize};

/// Session cache bounds. Found lyric payloads count their slice storage plus every retained
/// `String` capacity; empty results and transient failures still count toward the entry bound.
const CACHE_MAX_ENTRIES: usize = 64;
const CACHE_MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const LYRICS_JSON_MAX: usize = 512 * 1024;
const LRCLIB_API_BASE: &str = "https://lrclib.net";
/// Per-HTTP-request timeout; the whole fallback pipeline has a separate shared deadline.
const LYRICS_TIMEOUT: Duration = Duration::from_secs(8);
/// One budget shared by exact, fielded-search, and title-only-search stages.
const LYRICS_PIPELINE_TIMEOUT: Duration = Duration::from_secs(12);
/// How long a *transient* fetch failure is remembered before we retry. A genuine
/// "this track has no synced lyrics" is cached for the whole session; only failures
/// (network/parse) get this short cooldown so re-opening the panel later can recover.
const NEGATIVE_TTL: Duration = Duration::from_secs(120);

/// A per-track cache entry: a resolved result (kept for the session, even when empty — the
/// track genuinely has no synced lyrics) versus a transient failure retried after a cooldown.
enum CacheValue {
    Found(Arc<[LyricLine]>),
    Failed(Instant),
}

struct CacheEntry {
    value: CacheValue,
    payload_bytes: usize,
    last_used: u64,
}

/// Small dependency-free LRU. A monotonic touch stamp avoids a second owned copy of every video
/// id; finding the oldest entry is O(64), bounded by [`CACHE_MAX_ENTRIES`].
#[derive(Default)]
struct LyricsCache {
    entries: HashMap<String, CacheEntry>,
    payload_bytes: usize,
    clock: u64,
}

impl LyricsCache {
    fn get(&mut self, video_id: &str, now: Instant) -> Option<Arc<[LyricLine]>> {
        let stamp = self.next_stamp();
        let entry = self.entries.get_mut(video_id)?;
        entry.last_used = stamp;
        match &entry.value {
            CacheValue::Found(lines) => Some(Arc::clone(lines)),
            CacheValue::Failed(at) if now.saturating_duration_since(*at) < NEGATIVE_TTL => {
                Some(Arc::from([]))
            }
            CacheValue::Failed(_) => {
                self.remove(video_id);
                None
            }
        }
    }

    fn insert_found(&mut self, video_id: String, lines: Arc<[LyricLine]>) {
        let payload_bytes = lyrics_payload_bytes(&lines);
        self.remove(&video_id);

        // A single pathological response is still delivered to the current track, but retaining
        // it would make the byte cap meaningless. It can be fetched again if revisited.
        if payload_bytes > CACHE_MAX_PAYLOAD_BYTES {
            return;
        }

        let last_used = self.next_stamp();
        self.payload_bytes = self.payload_bytes.saturating_add(payload_bytes);
        self.entries.insert(
            video_id,
            CacheEntry {
                value: CacheValue::Found(lines),
                payload_bytes,
                last_used,
            },
        );
        self.evict_to_bounds();
    }

    fn insert_failed(&mut self, video_id: String, at: Instant) {
        self.remove(&video_id);
        let last_used = self.next_stamp();
        self.entries.insert(
            video_id,
            CacheEntry {
                value: CacheValue::Failed(at),
                payload_bytes: 0,
                last_used,
            },
        );
        self.evict_to_bounds();
    }

    fn remove(&mut self, video_id: &str) {
        if let Some(old) = self.entries.remove(video_id) {
            self.payload_bytes = self.payload_bytes.saturating_sub(old.payload_bytes);
        }
    }

    fn evict_to_bounds(&mut self) {
        while self.entries.len() > CACHE_MAX_ENTRIES || self.payload_bytes > CACHE_MAX_PAYLOAD_BYTES
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(video_id, _)| video_id.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
    }

    fn next_stamp(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }
}

fn lyrics_payload_bytes(lines: &[LyricLine]) -> usize {
    std::mem::size_of_val(lines).saturating_add(
        lines
            .iter()
            .map(|line| line.text.capacity())
            .fold(0usize, usize::saturating_add),
    )
}

/// A session-only lyric timing offset, stored as exact signed 100 ms steps.
///
/// Positive values delay the displayed lyrics: line selection observes
/// `playback_position - delay`, while clicking a lyric seeks to
/// `line_timestamp + delay`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LyricDelay(i32);

impl LyricDelay {
    pub const ZERO: Self = Self(0);
    pub const STEP_SECONDS: f64 = 0.1;

    #[must_use]
    pub const fn from_steps(steps: i32) -> Self {
        Self(steps)
    }

    #[must_use]
    pub const fn steps(self) -> i32 {
        self.0
    }

    /// Move the displayed lyrics 100 ms earlier.
    #[must_use]
    pub fn earlier(self) -> Self {
        Self(self.0.saturating_sub(1))
    }

    /// Move the displayed lyrics 100 ms later.
    #[must_use]
    pub fn later(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    #[must_use]
    pub fn seconds(self) -> f64 {
        f64::from(self.0) * Self::STEP_SECONDS
    }

    /// Playback position used to select the highlighted lyric line.
    #[must_use]
    pub fn active_position(self, playback_position: f64) -> f64 {
        playback_position - self.seconds()
    }

    /// Seek position for a clicked lyric line, clamped to the finite track duration.
    #[must_use]
    pub fn seek_position(self, line_timestamp: f64, duration: f64) -> Option<f64> {
        if !line_timestamp.is_finite() || !duration.is_finite() || duration < 0.0 {
            return None;
        }
        Some((line_timestamp + self.seconds()).clamp(0.0, duration))
    }
}

impl fmt::Display for LyricDelay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0.0s");
        }
        let magnitude = self.0.unsigned_abs();
        let sign = if self.0 > 0 { '+' } else { '-' };
        write!(f, "{sign}{}.{:01}s", magnitude / 10, magnitude % 10)
    }
}

/// One timed lyric line.
#[derive(Debug, Clone)]
pub struct LyricLine {
    /// Offset from track start, in seconds.
    pub time: f64,
    pub text: String,
}

/// Parse an LRC body into time-sorted lines. Metadata tags (`[ar:…]`, `[length:…]`) and
/// malformed timestamps are ignored; a line may carry several timestamps (`[t1][t2] x`).
pub fn parse_lrc(raw: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let mut rest = line;
        let mut stamps = Vec::new();
        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else { break };
            if let Some(t) = parse_timestamp(&rest[1..end]) {
                stamps.push(t);
            }
            rest = &rest[end + 1..];
        }
        let text = rest.trim().to_owned();
        for t in stamps {
            out.push(LyricLine {
                time: t,
                text: text.clone(),
            });
        }
    }
    out.sort_by(|a, b| {
        a.time
            .partial_cmp(&b.time)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Parse a `mm:ss` / `mm:ss.xx` timestamp into seconds. Returns `None` for metadata tags.
fn parse_timestamp(tag: &str) -> Option<f64> {
    let (m, s) = tag.split_once(':')?;
    let mins: f64 = m.trim().parse().ok()?;
    let secs: f64 = s.trim().parse().ok()?;
    // Reject non-finite ("nan"/"inf" parse as f64) and out-of-range values. A non-finite time
    // breaks the sorted-vector invariant `current_index`/`partition_point` rely on; the
    // `secs` range and non-negative `mins` drop obviously malformed tags.
    if !mins.is_finite() || mins < 0.0 || !(0.0..60.0).contains(&secs) {
        return None;
    }
    Some(mins * 60.0 + secs)
}

/// Index of the line that should be highlighted at playback position `pos` — the last
/// line whose timestamp is `<= pos`. `None` before the first line (or when empty).
pub fn current_index(lines: &[LyricLine], pos: f64) -> Option<usize> {
    let pp = lines.partition_point(|l| l.time <= pos);
    (pp > 0).then(|| pp - 1)
}

/// Highlighted line at `playback_position` after applying the session lyric delay.
pub fn current_index_with_delay(
    lines: &[LyricLine],
    playback_position: f64,
    delay: LyricDelay,
) -> Option<usize> {
    current_index(lines, delay.active_position(playback_position))
}

// --- Actor ------------------------------------------------------------------

/// Neutral actor request. Playback owners project their current [`crate::api::Song`] into this
/// bounded metadata shape so the leaf actor never reaches into reducer state.
#[derive(Debug, Clone, PartialEq)]
pub struct LyricsRequest {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_secs: Option<f64>,
}

pub enum LyricsCmd {
    Fetch(LyricsRequest),
}

pub enum LyricsEvent {
    Result {
        video_id: String,
        lines: Arc<[LyricLine]>,
    },
}

pub struct LyricsHandle {
    /// A capacity-one wake queue. The command itself lives in `latest`, so replacing a
    /// pending fetch never needs to allocate another queue entry.
    wake_tx: mpsc::Sender<()>,
    latest: Arc<Mutex<Option<LyricsCmd>>>,
}

impl LyricsHandle {
    pub fn fetch(&self, request: LyricsRequest) -> DeliveryResult {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let replaced_existing = latest.replace(LyricsCmd::Fetch(request)).is_some();

        match self.wake_tx.try_send(()) {
            // A full wake queue already guarantees the actor will inspect `latest`. A full
            // queue with an empty previous slot is possible when a redundant wake remains
            // after the actor consumed a replacement; the new command is still accepted.
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {
                if replaced_existing {
                    Ok(DeliveryReceipt::Coalesced {
                        replaced_existing: true,
                        evicted_oldest: false,
                    })
                } else {
                    Ok(DeliveryReceipt::Enqueued)
                }
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                // Nothing can consume this slot after the actor has gone away.
                latest.take();
                Err(DeliveryError::Closed)
            }
        }
    }
}

fn actor_channel() -> (LyricsHandle, mpsc::Receiver<()>) {
    let (wake_tx, wake_rx) = bounded_channel(LYRICS_QUEUE);
    let latest = Arc::new(Mutex::new(None));
    (LyricsHandle { wake_tx, latest }, wake_rx)
}

/// Spawn the lyrics actor; results return as [`LyricsEvent`]s.
pub fn spawn<F>(emit: F) -> LyricsHandle
where
    F: Fn(LyricsEvent) + Send + Sync + 'static,
{
    let (handle, wake_rx) = actor_channel();
    tokio::spawn(run_actor(wake_rx, Arc::clone(&handle.latest), emit));
    handle
}

async fn run_actor<F>(
    mut wake_rx: mpsc::Receiver<()>,
    latest: Arc<Mutex<Option<LyricsCmd>>>,
    emit: F,
) where
    F: Fn(LyricsEvent) + Send + Sync + 'static,
{
    let client = reqwest::Client::builder()
        .user_agent("yututui/0.1 (https://github.com/Ochichan/Yututui)")
        .timeout(LYRICS_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut cache = LyricsCache::default();

    while wake_rx.recv().await.is_some() {
        // Latest-only: rapid track-skips replace this one slot, so the actor never walks a
        // backlog. A redundant wake can remain after a replacement was consumed; skip it.
        let Some(cmd) = latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            continue;
        };
        let LyricsCmd::Fetch(request) = cmd;
        let video_id = request.video_id.clone();
        // A resolved result (even empty) is reused; a transient failure is reused only until
        // its cooldown expires, after which we re-fetch instead of showing "no lyrics" forever.
        let cached = cache.get(&video_id, Instant::now());
        let lines = if let Some(lines) = cached {
            lines
        } else {
            match fetch(&client, &request).await {
                Ok(fetched) => {
                    let fetched: Arc<[LyricLine]> = fetched.into();
                    cache.insert_found(video_id.clone(), Arc::clone(&fetched));
                    fetched
                }
                Err(()) => {
                    cache.insert_failed(video_id.clone(), Instant::now());
                    Arc::from([])
                }
            }
        };
        tracing::info!(count = lines.len(), video_id = %video_id, "lyrics");
        emit(LyricsEvent::Result { video_id, lines });
    }
}

#[derive(Debug, Deserialize)]
struct LrcItem {
    #[serde(rename = "trackName", default)]
    track_name: Option<String>,
    #[serde(rename = "artistName", default)]
    artist_name: Option<String>,
    #[serde(rename = "albumName", default)]
    album_name: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(rename = "syncedLyrics", default)]
    synced: Option<String>,
}

#[derive(Debug)]
enum StageResult {
    Found(Vec<LyricLine>),
    Miss,
    Transient,
}

/// Query lrclib through a bounded exact -> fielded -> cleaned-title fallback pipeline.
/// `Ok(lines)` — a resolved answer (empty means the track genuinely has no synced lyrics).
/// `Err(())` — a *transient* failure (network/parse) the caller should retry after a cooldown.
async fn fetch(client: &reqwest::Client, request: &LyricsRequest) -> Result<Vec<LyricLine>, ()> {
    match tokio::time::timeout(
        LYRICS_PIPELINE_TIMEOUT,
        fetch_from_base(client, LRCLIB_API_BASE, request),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!("lyrics lookup pipeline timed out");
            Err(())
        }
    }
}

async fn fetch_from_base(
    client: &reqwest::Client,
    base: &str,
    request: &LyricsRequest,
) -> Result<Vec<LyricLine>, ()> {
    let mut saw_transient = false;

    if exact_eligible(request) {
        match fetch_exact(client, base, request).await {
            StageResult::Found(lines) => return Ok(lines),
            StageResult::Miss => {}
            StageResult::Transient => saw_transient = true,
        }
    }

    match fetch_search(client, base, request, SearchKind::Fielded).await {
        StageResult::Found(lines) => return Ok(lines),
        StageResult::Miss => {}
        StageResult::Transient => saw_transient = true,
    }

    let cleaned_title = crate::track_identity::normalize_stripped(&request.title);
    if !cleaned_title.is_empty() {
        match fetch_search(client, base, request, SearchKind::Title(cleaned_title)).await {
            StageResult::Found(lines) => return Ok(lines),
            StageResult::Miss => {}
            StageResult::Transient => saw_transient = true,
        }
    }

    if saw_transient {
        Err(())
    } else {
        Ok(Vec::new())
    }
}

fn exact_eligible(request: &LyricsRequest) -> bool {
    !request.title.trim().is_empty()
        && !request.artist.trim().is_empty()
        && request
            .album
            .as_deref()
            .is_some_and(|album| !album.trim().is_empty())
        && request
            .duration_secs
            .is_some_and(|duration| duration.is_finite() && (1.0..=3600.0).contains(&duration))
}

async fn fetch_exact(client: &reqwest::Client, base: &str, request: &LyricsRequest) -> StageResult {
    let title = request.title.trim();
    let artist = request.artist.trim();
    let album = request.album.as_deref().unwrap_or_default().trim();
    let duration = request.duration_secs.unwrap_or_default().round() as u64;
    let response = match client
        .get(format!("{}/api/get", base.trim_end_matches('/')))
        .query(&[
            ("track_name", title),
            ("artist_name", artist),
            ("album_name", album),
        ])
        .query(&[("duration", duration)])
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            warn_request_error("exact", error);
            return StageResult::Transient;
        }
    };

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return StageResult::Miss;
    }
    if !response.status().is_success() {
        warn_status("exact", response.status());
        return StageResult::Transient;
    }

    match http::json_limited::<LrcItem>(response, LYRICS_JSON_MAX).await {
        Ok(item) => select_candidate(request, std::iter::once(item))
            .map_or(StageResult::Miss, StageResult::Found),
        Err(error) => {
            warn_parse_error("exact", error);
            StageResult::Transient
        }
    }
}

enum SearchKind {
    Fielded,
    Title(String),
}

async fn fetch_search(
    client: &reqwest::Client,
    base: &str,
    request: &LyricsRequest,
    kind: SearchKind,
) -> StageResult {
    let builder = client.get(format!("{}/api/search", base.trim_end_matches('/')));
    let (stage, builder) = match &kind {
        SearchKind::Fielded => {
            let title = crate::track_identity::normalize_stripped(&request.title);
            let artist = clean_artist(&request.artist);
            (
                "fielded",
                builder.query(&[("track_name", title), ("artist_name", artist)]),
            )
        }
        SearchKind::Title(title) => ("title", builder.query(&[("q", title.as_str())])),
    };
    let response = match builder.send().await {
        Ok(response) => response,
        Err(error) => {
            warn_request_error(stage, error);
            return StageResult::Transient;
        }
    };
    if !response.status().is_success() {
        warn_status(stage, response.status());
        return StageResult::Transient;
    }

    match http::json_limited::<Vec<LrcItem>>(response, LYRICS_JSON_MAX).await {
        Ok(items) => select_candidate(request, items).map_or(StageResult::Miss, StageResult::Found),
        Err(error) => {
            warn_parse_error(stage, error);
            StageResult::Transient
        }
    }
}

fn warn_request_error(stage: &str, error: reqwest::Error) {
    tracing::warn!(
        stage,
        error = %sanitize::sanitize_error_text(error.to_string()),
        "lyrics request failed"
    );
}

fn warn_parse_error(stage: &str, error: impl fmt::Display) {
    tracing::warn!(
        stage,
        error = %sanitize::sanitize_error_text(error.to_string()),
        "lyrics response parse failed"
    );
}

fn warn_status(stage: &str, status: reqwest::StatusCode) {
    tracing::warn!(
        stage,
        status = status.as_u16(),
        "lyrics request returned an error"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CandidateRank {
    score_milli: u16,
    duration_bucket: u8,
    title_milli: u16,
    artist_milli: u16,
    album_milli: u16,
    api_order: Reverse<usize>,
}

fn select_candidate(
    request: &LyricsRequest,
    items: impl IntoIterator<Item = LrcItem>,
) -> Option<Vec<LyricLine>> {
    let mut best: Option<(CandidateRank, Vec<LyricLine>)> = None;
    for (index, item) in items.into_iter().enumerate() {
        let Some(synced) = item
            .synced
            .as_deref()
            .filter(|lyrics| !lyrics.trim().is_empty())
        else {
            continue;
        };
        let lines = parse_lrc(synced);
        if lines.is_empty() {
            continue;
        }
        let Some(rank) = candidate_rank(request, &item, index) else {
            continue;
        };
        if best.as_ref().is_none_or(|(best_rank, _)| rank > *best_rank) {
            best = Some((rank, lines));
        }
    }
    best.map(|(_, lines)| lines)
}

fn candidate_rank(
    request: &LyricsRequest,
    candidate: &LrcItem,
    index: usize,
) -> Option<CandidateRank> {
    use crate::track_identity::{normalize, normalize_stripped, similarity, versions_compatible};

    let candidate_track_name = candidate.track_name.as_deref().unwrap_or_default();
    if candidate_track_name.trim().is_empty()
        || !versions_compatible(&request.title, candidate_track_name)
    {
        return None;
    }

    let source_title = normalize(&request.title);
    let candidate_title = normalize(candidate_track_name);
    let source_title_stripped = normalize_stripped(&request.title);
    let candidate_title_stripped = normalize_stripped(candidate_track_name);
    let mut title_score = similarity(&source_title, &candidate_title).max(similarity(
        &source_title_stripped,
        &candidate_title_stripped,
    ));

    let candidate_artist = clean_artist(candidate.artist_name.as_deref().unwrap_or_default());
    let aliases = artist_aliases(&request.artist);
    let mut artist_score = if candidate_artist.is_empty() {
        None
    } else {
        aliases
            .iter()
            .map(|alias| identity_similarity(alias, &candidate_artist))
            .reduce(f32::max)
    };

    if let Some((prefix, credited_title)) = split_title_credit(&request.title) {
        let prefix = clean_artist(prefix);
        let prefix_score = identity_similarity(&prefix, &candidate_artist);
        if prefix_score >= 0.90 {
            title_score = title_score
                .max(similarity(&normalize(credited_title), &candidate_title))
                .max(similarity(
                    &normalize_stripped(credited_title),
                    &candidate_title_stripped,
                ));
            artist_score = Some(artist_score.unwrap_or(0.0).max(prefix_score));
        }
    }

    if artist_score.is_some_and(|score| score < 0.72) {
        return None;
    }

    let (duration_score, duration_bucket) =
        duration_score(request.duration_secs, candidate.duration)?;
    let album_score = request.album.as_deref().and_then(|source_album| {
        let source = normalize_stripped(source_album);
        let candidate = normalize_stripped(candidate.album_name.as_deref().unwrap_or_default());
        (!source.is_empty() && !candidate.is_empty()).then(|| similarity(&source, &candidate))
    });

    let mut weighted_score = 0.55 * title_score + 0.15 * duration_score;
    let mut weight = 0.70;
    if let Some(score) = artist_score {
        weighted_score += 0.25 * score;
        weight += 0.25;
    }
    if let Some(score) = album_score {
        weighted_score += 0.05 * score;
        weight += 0.05;
    }
    let score = weighted_score / weight;
    if title_score < 0.86
        || score < 0.84
        || (artist_score.is_none() && (title_score < 0.94 || score < 0.88))
    {
        return None;
    }

    Some(CandidateRank {
        score_milli: quantize_score(score),
        duration_bucket,
        title_milli: quantize_score(title_score),
        artist_milli: quantize_score(artist_score.unwrap_or(0.0)),
        album_milli: quantize_score(album_score.unwrap_or(0.0)),
        api_order: Reverse(index),
    })
}

fn clean_artist(artist: &str) -> String {
    let mut normalized = crate::track_identity::normalize(artist);
    for suffix in [" topic", " official"] {
        if let Some(stripped) = normalized.strip_suffix(suffix) {
            normalized = stripped.trim_end().to_owned();
        }
    }
    if let Some(stripped) = normalized.strip_suffix("vevo")
        && !stripped.trim().is_empty()
    {
        normalized = stripped.trim_end().to_owned();
    }
    normalized
}

fn artist_aliases(artist: &str) -> Vec<String> {
    let cleaned = clean_artist(artist);
    const UNTRUSTED: [&str; 5] = [
        "unknown artist",
        "various artists",
        "youtube music",
        "youtube",
        "music",
    ];
    if cleaned.is_empty() || UNTRUSTED.contains(&cleaned.as_str()) {
        return Vec::new();
    }
    let mut aliases = vec![cleaned.clone()];
    for separator in [" feat ", " featuring "] {
        if let Some((primary, _)) = cleaned.split_once(separator)
            && !primary.trim().is_empty()
            && !aliases.iter().any(|alias| alias == primary.trim())
        {
            aliases.push(primary.trim().to_owned());
        }
    }
    aliases
}

fn identity_similarity(a: &str, b: &str) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let padded_a = format!(" {a} ");
    let padded_b = format!(" {b} ");
    if padded_a.contains(&padded_b) || padded_b.contains(&padded_a) {
        1.0
    } else {
        crate::track_identity::similarity(a, b)
    }
}

fn split_title_credit(title: &str) -> Option<(&str, &str)> {
    [" - ", " – ", " — "]
        .into_iter()
        .filter_map(|separator| title.find(separator).map(|index| (index, separator.len())))
        .min_by_key(|(index, _)| *index)
        .and_then(|(index, separator_len)| {
            let prefix = title[..index].trim();
            let suffix = title[index + separator_len..].trim();
            (!prefix.is_empty() && !suffix.is_empty()).then_some((prefix, suffix))
        })
}

fn duration_score(source: Option<f64>, candidate: Option<f64>) -> Option<(f32, u8)> {
    let source = source.filter(|duration| duration.is_finite() && *duration > 0.0);
    let candidate = candidate.filter(|duration| duration.is_finite() && *duration > 0.0);
    let (Some(source), Some(candidate)) = (source, candidate) else {
        return Some((0.50, 0));
    };
    let delta = (source - candidate).abs();
    if delta > (0.04 * source).clamp(8.0, 15.0) {
        return None;
    }
    Some(if delta <= 2.0 {
        (1.0, 3)
    } else if delta <= 5.0 {
        (0.85, 2)
    } else {
        (0.65, 1)
    })
}

fn quantize_score(score: f32) -> u16 {
    (score.clamp(0.0, 1.0) * 1000.0).round() as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn request(video_id: &str, artist: &str, title: &str) -> LyricsRequest {
        LyricsRequest {
            video_id: video_id.to_owned(),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: None,
            duration_secs: None,
        }
    }

    fn item(
        track_name: &str,
        artist_name: &str,
        album_name: &str,
        duration: Option<f64>,
        lyric: &str,
    ) -> LrcItem {
        LrcItem {
            track_name: Some(track_name.to_owned()),
            artist_name: Some(artist_name.to_owned()),
            album_name: Some(album_name.to_owned()),
            duration,
            synced: Some(format!("[00:01.00]{lyric}")),
        }
    }

    struct TestHttpResponse {
        status: &'static str,
        body: String,
    }

    async fn local_http_server(
        responses: Vec<TestHttpResponse>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request_bytes = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request_bytes.extend_from_slice(&buffer[..read]);
                    if request_bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                    assert!(request_bytes.len() < 16 * 1024, "request headers too large");
                }
                let request = String::from_utf8(request_bytes).unwrap();
                requests.push(request.lines().next().unwrap_or_default().to_owned());
                let wire = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                stream.write_all(wire.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), server)
    }

    async fn finish_server(server: tokio::task::JoinHandle<Vec<String>>) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("test server should receive every expected request")
            .unwrap()
    }

    #[test]
    fn parses_timestamps_and_sorts() {
        let raw = "[ti:Song]\n[00:12.50]first\n[00:05.00]earlier\n[01:00.00]later";
        let lines = parse_lrc(raw);
        // Metadata line dropped; the three timed lines sorted ascending.
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "earlier");
        assert!((lines[0].time - 5.0).abs() < 1e-6);
        assert!((lines[1].time - 12.5).abs() < 1e-6);
        assert!((lines[2].time - 60.0).abs() < 1e-6);
    }

    #[test]
    fn multiple_timestamps_per_line_expand() {
        let lines = parse_lrc("[00:01.00][00:10.00]repeat");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.text == "repeat"));
    }

    #[test]
    fn malformed_timestamps_are_dropped_not_treated_as_non_finite() {
        // "nan"/"inf" parse as f64; a non-finite time would break the sorted-vector invariant.
        assert_eq!(parse_timestamp("nan:00"), None);
        assert_eq!(parse_timestamp("00:inf"), None);
        assert_eq!(parse_timestamp("-1:00"), None); // negative minutes
        assert_eq!(parse_timestamp("00:75"), None); // seconds out of range
        // A well-formed tag still parses.
        assert!((parse_timestamp("01:30.5").unwrap() - 90.5).abs() < 1e-6);
        // A whole line carrying a bad stamp yields no timed line, without panicking.
        assert!(parse_lrc("[nan:00]lyric").is_empty());
    }

    #[test]
    fn current_index_tracks_position() {
        let lines = parse_lrc("[00:00.00]a\n[00:05.00]b\n[00:10.00]c");
        assert_eq!(current_index(&lines, -1.0), None); // before first (can't happen, but safe)
        assert_eq!(current_index(&lines, 0.0), Some(0));
        assert_eq!(current_index(&lines, 4.9), Some(0));
        assert_eq!(current_index(&lines, 5.0), Some(1));
        assert_eq!(current_index(&lines, 9.9), Some(1));
        assert_eq!(current_index(&lines, 100.0), Some(2));
    }

    #[test]
    fn lyric_delay_moves_in_exact_saturating_steps() {
        let mut delay = LyricDelay::ZERO;
        for _ in 0..10_000 {
            delay = delay.later();
        }
        assert_eq!(delay.steps(), 10_000);
        assert_eq!(delay.seconds(), 1_000.0);
        assert_eq!(LyricDelay::ZERO.earlier().steps(), -1);
        assert_eq!(LyricDelay::ZERO.later().steps(), 1);
        assert_eq!(LyricDelay::from_steps(i32::MAX).later().steps(), i32::MAX);
        assert_eq!(LyricDelay::from_steps(i32::MIN).earlier().steps(), i32::MIN);
    }

    #[test]
    fn lyric_delay_active_position_uses_display_sign() {
        let lines = parse_lrc("[00:00.00]a\n[00:05.00]b");
        let later = LyricDelay::from_steps(1);
        let earlier = LyricDelay::from_steps(-1);

        assert_eq!(later.active_position(5.0), 4.9);
        assert_eq!(current_index_with_delay(&lines, 5.0, later), Some(0));
        assert_eq!(earlier.active_position(5.0), 5.1);
        assert_eq!(current_index_with_delay(&lines, 5.0, earlier), Some(1));
    }

    #[test]
    fn lyric_delay_seek_position_uses_opposite_sign_and_clamps() {
        let later = LyricDelay::from_steps(2);
        let earlier = LyricDelay::from_steps(-2);

        assert_eq!(later.seek_position(5.0, 10.0), Some(5.2));
        assert_eq!(earlier.seek_position(5.0, 10.0), Some(4.8));
        assert_eq!(earlier.seek_position(0.1, 10.0), Some(0.0));
        assert_eq!(later.seek_position(9.9, 10.0), Some(10.0));
        assert_eq!(later.seek_position(f64::NAN, 10.0), None);
        assert_eq!(later.seek_position(5.0, f64::INFINITY), None);
        assert_eq!(later.seek_position(5.0, -1.0), None);
    }

    #[test]
    fn lyric_delay_display_is_exact_to_one_tenth() {
        assert_eq!(LyricDelay::ZERO.to_string(), "0.0s");
        assert_eq!(LyricDelay::from_steps(1).to_string(), "+0.1s");
        assert_eq!(LyricDelay::from_steps(-1).to_string(), "-0.1s");
        assert_eq!(LyricDelay::from_steps(123).to_string(), "+12.3s");
        assert_eq!(
            LyricDelay::from_steps(i32::MIN).to_string(),
            "-214748364.8s"
        );
    }

    #[test]
    fn empty_is_handled() {
        assert!(parse_lrc("").is_empty());
        assert_eq!(current_index(&[], 5.0), None);
    }

    #[test]
    fn candidate_selection_rejects_versions_and_large_duration_drift_then_ranks() {
        let mut request = request("id", "Artist", "Song");
        request.album = Some("Album".to_owned());
        request.duration_secs = Some(200.0);
        let selected = select_candidate(
            &request,
            [
                item("Song", "Artist", "Album", Some(209.0), "too long"),
                item("Song (Live)", "Artist", "Album", Some(200.0), "live"),
                item("Song", "Artist", "Album", Some(204.0), "close"),
                item("Song", "Artist", "Album", Some(200.0), "best"),
            ],
        )
        .expect("an eligible synced candidate");
        assert_eq!(selected[0].text, "best");
    }

    #[test]
    fn candidate_selection_is_stable_for_equal_api_results() {
        let request = request("id", "Artist", "Song");
        let selected = select_candidate(
            &request,
            [
                item("Song", "Artist", "", None, "first"),
                item("Song", "Artist", "", None, "second"),
            ],
        )
        .unwrap();
        assert_eq!(selected[0].text, "first");
    }

    #[test]
    fn title_credit_and_uploader_suffixes_recover_clean_identity() {
        let request = request("id", "BjörkVEVO", "Björk - Jóga (Official Video)");
        let selected = select_candidate(
            &request,
            [item("Jóga", "Björk", "Homogenic", Some(307.0), "found")],
        )
        .unwrap();
        assert_eq!(selected[0].text, "found");
    }

    #[test]
    fn candidate_thresholds_fail_closed_for_weak_titles_and_missing_artists() {
        let mut exact_metadata = request("id", "Artist", "abcdef");
        exact_metadata.album = Some("Album".to_owned());
        exact_metadata.duration_secs = Some(180.0);
        assert!(
            candidate_rank(
                &exact_metadata,
                &item("abcxef", "Artist", "Album", Some(180.0), "weak"),
                0,
            )
            .is_none(),
            "strong auxiliary metadata must not rescue title similarity below 0.86"
        );

        let mut request = request("id", "Artist", "My Song");
        request.duration_secs = Some(180.0);
        assert!(
            candidate_rank(
                &request,
                &item("My Songs", "", "", Some(180.0), "missing artist"),
                0,
            )
            .is_none(),
            "missing candidate artist uses the stricter title-only threshold"
        );
        assert!(
            candidate_rank(
                &request,
                &item("My Songs", "Artist", "", Some(180.0), "known artist"),
                0,
            )
            .is_some(),
            "the same close title is eligible when its artist identity is known"
        );
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
    )]
    async fn lookup_falls_back_exact_then_fielded_then_clean_title() {
        let candidate = serde_json::json!({
            "trackName": "Song",
            "artistName": "Artist",
            "albumName": "Album",
            "duration": 200.0,
            "syncedLyrics": "[00:01.00]fallback"
        });
        let (base, server) = local_http_server(vec![
            TestHttpResponse {
                status: "404 Not Found",
                body: "{}".to_owned(),
            },
            TestHttpResponse {
                status: "200 OK",
                body: "[]".to_owned(),
            },
            TestHttpResponse {
                status: "200 OK",
                body: serde_json::to_string(&vec![candidate]).unwrap(),
            },
        ])
        .await;
        let request = LyricsRequest {
            video_id: "id".to_owned(),
            title: "Song (Official Video)".to_owned(),
            artist: "Artist - Topic".to_owned(),
            album: Some("Album".to_owned()),
            duration_secs: Some(200.0),
        };

        let lines = fetch_from_base(&reqwest::Client::new(), &base, &request)
            .await
            .unwrap();
        assert_eq!(lines[0].text, "fallback");
        let requests = finish_server(server).await;
        assert_eq!(requests.len(), 3);
        assert!(requests[0].starts_with("GET /api/get?"));
        let exact_target = requests[0].split_whitespace().nth(1).unwrap();
        let exact_url = reqwest::Url::parse(&format!("http://fixture{exact_target}")).unwrap();
        let exact_query: Vec<_> = exact_url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(
            exact_query,
            [
                ("track_name".to_owned(), "Song (Official Video)".to_owned()),
                ("artist_name".to_owned(), "Artist - Topic".to_owned()),
                ("album_name".to_owned(), "Album".to_owned()),
                ("duration".to_owned(), "200".to_owned()),
            ],
            "exact query parameters must survive URL encoding in deterministic order"
        );
        assert!(requests[1].starts_with("GET /api/search?"));
        let fielded_target = requests[1].split_whitespace().nth(1).unwrap();
        let fielded_url = reqwest::Url::parse(&format!("http://fixture{fielded_target}")).unwrap();
        assert_eq!(
            fielded_url
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>(),
            [
                ("track_name".to_owned(), "song".to_owned()),
                ("artist_name".to_owned(), "artist".to_owned()),
            ],
            "fielded search uses conservative cleaned identities"
        );
        assert_eq!(requests[2], "GET /api/search?q=song HTTP/1.1");
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
    )]
    async fn lookup_distinguishes_resolved_miss_from_transient_failure() {
        let request = request("id", "Artist", "Song");
        let (base, server) = local_http_server(vec![
            TestHttpResponse {
                status: "200 OK",
                body: "[]".to_owned(),
            },
            TestHttpResponse {
                status: "200 OK",
                body: "[]".to_owned(),
            },
        ])
        .await;
        assert!(
            fetch_from_base(&reqwest::Client::new(), &base, &request)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(finish_server(server).await.len(), 2);

        let success = serde_json::json!({
            "trackName": "Song",
            "artistName": "Artist",
            "albumName": null,
            "duration": null,
            "syncedLyrics": "[00:01.00]recovered"
        });
        let (base, server) = local_http_server(vec![
            TestHttpResponse {
                status: "500 Internal Server Error",
                body: "{}".to_owned(),
            },
            TestHttpResponse {
                status: "200 OK",
                body: serde_json::to_string(&vec![success]).unwrap(),
            },
        ])
        .await;
        let recovered = fetch_from_base(&reqwest::Client::new(), &base, &request)
            .await
            .expect("a successful fallback supersedes an earlier transient stage");
        assert_eq!(recovered[0].text, "recovered");
        assert_eq!(finish_server(server).await.len(), 2);

        let (base, server) = local_http_server(vec![
            TestHttpResponse {
                status: "500 Internal Server Error",
                body: "{}".to_owned(),
            },
            TestHttpResponse {
                status: "200 OK",
                body: "[]".to_owned(),
            },
        ])
        .await;
        assert!(
            fetch_from_base(&reqwest::Client::new(), &base, &request)
                .await
                .is_err(),
            "a transient stage keeps the final result retryable when no fallback succeeds"
        );
        assert_eq!(finish_server(server).await.len(), 2);
    }

    #[test]
    fn fetches_coalesce_to_one_latest_pending_command() {
        let (handle, mut wake_rx) = actor_channel();

        assert_eq!(
            handle.fetch(request("old", "old artist", "old title")),
            Ok(DeliveryReceipt::Enqueued)
        );
        let mut newest = request("new", "new artist", "new title");
        newest.album = Some("new album".to_owned());
        newest.duration_secs = Some(187.0);
        assert_eq!(
            handle.fetch(newest),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        );

        wake_rx.try_recv().expect("one wake is queued");
        assert!(wake_rx.try_recv().is_err(), "the wake queue stays bounded");
        let latest = handle
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("latest command is retained");
        let LyricsCmd::Fetch(request) = latest;
        assert_eq!(request.video_id, "new");
        assert_eq!(request.artist, "new artist");
        assert_eq!(request.title, "new title");
        assert_eq!(request.album.as_deref(), Some("new album"));
        assert_eq!(request.duration_secs, Some(187.0));
    }

    #[test]
    fn fetch_reports_closed_actor_and_discards_unconsumable_command() {
        let (handle, wake_rx) = actor_channel();
        drop(wake_rx);

        assert_eq!(
            handle.fetch(request("video", "artist", "title")),
            Err(DeliveryError::Closed)
        );
        assert!(
            handle
                .latest
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_none()
        );
    }

    fn retained_lines(text_capacity: usize) -> Arc<[LyricLine]> {
        let mut text = String::with_capacity(text_capacity);
        text.push('x');
        vec![LyricLine { time: 0.0, text }].into()
    }

    #[test]
    fn cache_hits_share_the_same_lyric_payload() {
        let mut cache = LyricsCache::default();
        let lines = retained_lines(128);
        cache.insert_found("track".to_owned(), Arc::clone(&lines));

        let hit = cache.get("track", Instant::now()).unwrap();
        assert!(Arc::ptr_eq(&lines, &hit));
        assert_eq!(cache.payload_bytes, lyrics_payload_bytes(&lines));
    }

    #[test]
    fn cache_evicts_least_recently_used_at_entry_bound() {
        let mut cache = LyricsCache::default();
        let now = Instant::now();
        for i in 0..CACHE_MAX_ENTRIES {
            cache.insert_found(format!("track-{i}"), Arc::from([]));
        }
        assert_eq!(cache.entries.len(), CACHE_MAX_ENTRIES);

        // Keep the original oldest entry hot, making track-1 the eviction candidate.
        assert!(cache.get("track-0", now).is_some());
        cache.insert_found("newest".to_owned(), Arc::from([]));

        assert_eq!(cache.entries.len(), CACHE_MAX_ENTRIES);
        assert!(cache.entries.contains_key("track-0"));
        assert!(!cache.entries.contains_key("track-1"));
        assert!(cache.entries.contains_key("newest"));
    }

    #[test]
    fn negative_cache_hits_are_touched_for_lru_eviction() {
        let mut cache = LyricsCache::default();
        let now = Instant::now();
        cache.insert_failed("failed".to_owned(), now);
        for i in 0..CACHE_MAX_ENTRIES - 1 {
            cache.insert_found(format!("track-{i}"), Arc::from([]));
        }

        assert!(cache.get("failed", now).is_some());
        cache.insert_found("newest".to_owned(), Arc::from([]));

        assert!(cache.entries.contains_key("failed"));
        assert!(!cache.entries.contains_key("track-0"));
        assert!(cache.entries.contains_key("newest"));
    }

    #[test]
    fn cache_evicts_by_retained_payload_bytes() {
        let mut cache = LyricsCache::default();
        let each = CACHE_MAX_PAYLOAD_BYTES / 2 + 1024;
        let first = retained_lines(each);
        let second = retained_lines(each);

        cache.insert_found("first".to_owned(), first);
        cache.insert_found("second".to_owned(), Arc::clone(&second));

        assert!(!cache.entries.contains_key("first"));
        assert!(cache.entries.contains_key("second"));
        assert_eq!(cache.payload_bytes, lyrics_payload_bytes(&second));
        assert!(cache.payload_bytes <= CACHE_MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn replacement_updates_payload_accounting() {
        let mut cache = LyricsCache::default();
        cache.insert_found("track".to_owned(), retained_lines(128));
        let replacement = retained_lines(4096);
        cache.insert_found("track".to_owned(), Arc::clone(&replacement));

        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.payload_bytes, lyrics_payload_bytes(&replacement));
    }

    #[test]
    fn empty_and_negative_entries_count_but_not_toward_payload() {
        let mut cache = LyricsCache::default();
        let now = Instant::now();
        cache.insert_found("empty".to_owned(), Arc::from([]));
        cache.insert_failed("failed".to_owned(), now);

        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.payload_bytes, 0);
        assert!(cache.get("empty", now).unwrap().is_empty());
        assert!(cache.get("failed", now).unwrap().is_empty());
    }

    #[test]
    fn transient_failure_expires_after_existing_ttl() {
        let mut cache = LyricsCache::default();
        let at = Instant::now();
        cache.insert_failed("failed".to_owned(), at);

        assert!(
            cache
                .get("failed", at + NEGATIVE_TTL - Duration::from_millis(1))
                .is_some()
        );
        assert!(cache.get("failed", at + NEGATIVE_TTL).is_none());
        assert!(!cache.entries.contains_key("failed"));
    }

    #[test]
    fn oversized_payload_is_delivered_by_caller_but_not_retained() {
        let mut cache = LyricsCache::default();
        let oversized = retained_lines(CACHE_MAX_PAYLOAD_BYTES);
        assert!(lyrics_payload_bytes(&oversized) > CACHE_MAX_PAYLOAD_BYTES);

        cache.insert_found("huge".to_owned(), Arc::clone(&oversized));

        assert!(!oversized.is_empty());
        assert!(cache.entries.is_empty());
        assert_eq!(cache.payload_bytes, 0);
    }
}
