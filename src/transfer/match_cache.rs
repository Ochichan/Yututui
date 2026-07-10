use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::matching::{
    MatchConfig, MatchOutcome, MatchScoreBreakdown, TrackInput, normalize, normalize_stripped,
};
use crate::api::Song;
use crate::api::ytmusic::{TransferAlbum, YoutubeSearchKind, YtdlpVideoMeta};
use crate::util::safe_fs;

const CACHE_SCHEMA_VERSION: u32 = 1;
const MATCHER_VERSION: u32 = 3;
const CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024;
const MATCH_CACHE_TTL_SECS: i64 = 365 * 24 * 60 * 60;
const ALBUM_CACHE_TTL_SECS: i64 = 180 * 24 * 60 * 60;
const QUERY_CACHE_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const EMPTY_QUERY_CACHE_TTL_SECS: i64 = 30 * 60;
const VIDEO_META_CACHE_TTL_SECS: i64 = 90 * 24 * 60 * 60;
const SOURCE_KEY_CACHE_MAX: usize = 16_384;
const ISRC_CACHE_MAX: usize = 16_384;
const NORMALIZED_CACHE_MAX: usize = 16_384;
const ALBUM_CACHE_MAX: usize = 1_024;
const QUERY_CACHE_MAX: usize = 512;
const VIDEO_META_CACHE_MAX: usize = 2048;
const ALBUM_TRACKS_MAX: usize = 512;
const QUERY_RESULT_SONGS_MAX: usize = 50;
/// Keep insertion hot paths O(1) while retaining a strict bound at prune/save time.
/// Once slack is exhausted, one bulk eviction amortizes the full-map ordering cost.
const MATCH_CACHE_PRUNE_SLACK: usize = 256;
const ALBUM_CACHE_PRUNE_SLACK: usize = 32;

/// Counts records removed while making the on-disk cache safe to reuse and bounded.
///
/// Successfully empty query results use a short negative TTL. Failed provider/video lookups are
/// never persisted as empty, so an outage cannot poison future imports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CacheMaintenanceStats {
    pub expired: usize,
    pub incompatible: usize,
    pub capacity_evictions: usize,
    pub size_evictions: usize,
}

impl CacheMaintenanceStats {
    fn add(&mut self, other: Self) {
        self.expired += other.expired;
        self.incompatible += other.incompatible;
        self.capacity_evictions += other.capacity_evictions;
        self.size_evictions += other.size_evictions;
    }

    pub(crate) fn removed(self) -> usize {
        self.expired + self.incompatible + self.capacity_evictions + self.size_evictions
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TransferMatchCache {
    pub schema_version: u32,
    pub matcher_version: u32,
    pub source_key: HashMap<String, CachedMatch>,
    pub isrc: HashMap<String, CachedMatch>,
    pub normalized: HashMap<String, CachedMatch>,
    pub albums: HashMap<String, CachedAlbum>,
    pub query_results: HashMap<String, CachedQueryResult>,
    pub video_metadata: HashMap<String, CachedVideoMeta>,
    #[serde(skip)]
    maintenance: CacheMaintenanceStats,
}

impl Default for TransferMatchCache {
    fn default() -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            matcher_version: MATCHER_VERSION,
            source_key: HashMap::new(),
            isrc: HashMap::new(),
            normalized: HashMap::new(),
            albums: HashMap::new(),
            query_results: HashMap::new(),
            video_metadata: HashMap::new(),
            maintenance: CacheMaintenanceStats::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedMatch {
    pub ytm_video_id: String,
    pub score: f32,
    pub display: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: Option<u32>,
    pub source_kind: Option<String>,
    pub quality_tier: Option<String>,
    /// True only when selection also satisfied the default Strict provenance and margin.
    /// Older entries default to false and cannot cross a policy boundary.
    #[serde(default)]
    pub policy_safe: bool,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedAlbum {
    pub ytm_album_id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<String>,
    pub tracks: Vec<CachedAlbumTrack>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedAlbumTrack {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track_number: Option<u32>,
    pub duration_secs: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedQueryResult {
    pub songs: Vec<CachedQuerySong>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedQuerySong {
    pub song: Song,
    pub kind: YoutubeSearchKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedVideoMeta {
    pub meta: YtdlpVideoMeta,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct CacheLookup {
    pub outcome: MatchOutcome,
    pub kind: &'static str,
}

impl TransferMatchCache {
    pub fn load() -> Self {
        let Some(path) = cache_path() else {
            return Self::default();
        };
        let mut cache = safe_fs::load_json_or_default_limited::<Self>(&path, CACHE_MAX_BYTES);
        if cache.schema_version == CACHE_SCHEMA_VERSION && cache.matcher_version == MATCHER_VERSION
        {
            let stats = cache.prune();
            cache.maintenance.add(stats);
            log_maintenance("load", stats);
            cache
        } else {
            Self::default()
        }
    }

    /// Persist a compact cache and return the accumulated maintenance. Pruning in place avoids
    /// cloning a cache that may legitimately approach the 32 MiB disk ceiling.
    pub fn save(&mut self) -> CacheMaintenanceStats {
        let mut stats = self.maintenance;
        stats.add(self.prune());
        self.maintenance = CacheMaintenanceStats::default();
        log_maintenance("save", stats);
        let Some(path) = cache_path() else {
            return stats;
        };
        if let Err(error) = safe_fs::write_private_atomic_json(&path, self) {
            tracing::warn!(error = %error, "failed to save transfer match cache");
        }
        stats
    }

    pub fn lookup_track(&self, input: &TrackInput) -> Option<CacheLookup> {
        let now = crate::signals::unix_now();
        if !input.source_key.trim().is_empty()
            && let Some(hit) = self.source_key.get(&input.source_key)
            && hit.is_fresh_policy_safe(now)
        {
            return Some(CacheLookup {
                outcome: hit.outcome(),
                kind: "source_key",
            });
        }
        if let Some(key) = isrc_key(input)
            && let Some(hit) = self.isrc.get(&key)
            && hit.is_fresh_policy_safe(now)
        {
            return Some(CacheLookup {
                outcome: hit.outcome(),
                kind: "isrc",
            });
        }
        if let Some(key) = normalized_track_key(input)
            && let Some(hit) = self.normalized.get(&key)
            && hit.is_fresh_policy_safe(now)
        {
            return Some(CacheLookup {
                outcome: hit.outcome(),
                kind: "normalized",
            });
        }
        None
    }

    /// Drop accepted outcomes which cannot satisfy the current run's threshold. Match entries
    /// are already restricted to policy-independent Strict-safe selections on write; this final
    /// filter prevents a prior lower `min_score` from bypassing the current configuration.
    pub fn retain_compatible_matches(&mut self, cfg: &MatchConfig) -> bool {
        let now = crate::signals::unix_now();
        let before = self.source_key.len() + self.isrc.len() + self.normalized.len();
        let compatible =
            |hit: &CachedMatch| hit.is_fresh_policy_safe(now) && hit.score >= cfg.accept;
        self.source_key.retain(|_, hit| compatible(hit));
        self.isrc.retain(|_, hit| compatible(hit));
        self.normalized.retain(|_, hit| compatible(hit));
        let removed = before - (self.source_key.len() + self.isrc.len() + self.normalized.len());
        self.maintenance.incompatible += removed;
        removed > 0
    }

    pub fn save_match(&mut self, input: &TrackInput, outcome: &MatchOutcome) {
        let cached = match CachedMatch::from_outcome(outcome) {
            Some(cached) => cached,
            None => return,
        };
        if !input.source_key.trim().is_empty() {
            self.source_key
                .insert(input.source_key.clone(), cached.clone());
            self.maintenance.capacity_evictions += prune_map_batched(
                &mut self.source_key,
                SOURCE_KEY_CACHE_MAX,
                MATCH_CACHE_PRUNE_SLACK,
                |hit| hit.updated_at,
            );
        }
        if let Some(key) = isrc_key(input) {
            self.isrc.insert(key, cached.clone());
            self.maintenance.capacity_evictions += prune_map_batched(
                &mut self.isrc,
                ISRC_CACHE_MAX,
                MATCH_CACHE_PRUNE_SLACK,
                |hit| hit.updated_at,
            );
        }
        if let Some(key) = normalized_track_key(input) {
            self.normalized.insert(key, cached);
            self.maintenance.capacity_evictions += prune_map_batched(
                &mut self.normalized,
                NORMALIZED_CACHE_MAX,
                MATCH_CACHE_PRUNE_SLACK,
                |hit| hit.updated_at,
            );
        }
    }

    pub fn lookup_album(&self, key: &str) -> Option<&CachedAlbum> {
        let now = crate::signals::unix_now();
        self.albums
            .get(key)
            .filter(|album| !is_expired(album.updated_at, now, ALBUM_CACHE_TTL_SECS))
    }

    pub fn save_album(&mut self, key: String, album: &TransferAlbum) {
        if key.trim().is_empty() || album.album_id.trim().is_empty() || album.tracks.is_empty() {
            return;
        }
        let tracks = album
            .tracks
            .iter()
            .filter(|track| !track.video_id.trim().is_empty() && !track.title.trim().is_empty())
            .take(ALBUM_TRACKS_MAX)
            .map(|track| CachedAlbumTrack {
                video_id: track.video_id.clone(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                track_number: track.track_number,
                duration_secs: track.duration_secs,
            })
            .collect::<Vec<_>>();
        if tracks.is_empty() {
            return;
        }
        self.albums.insert(
            key,
            CachedAlbum {
                ytm_album_id: album.album_id.clone(),
                title: album.title.clone(),
                artist: album.artist.clone(),
                year: album.year.clone(),
                tracks,
                updated_at: crate::signals::unix_now(),
            },
        );
        self.maintenance.capacity_evictions += prune_map_batched(
            &mut self.albums,
            ALBUM_CACHE_MAX,
            ALBUM_CACHE_PRUNE_SLACK,
            |album| album.updated_at,
        );
    }

    pub fn query_cache_entries(&self) -> Vec<(String, Vec<(Song, YoutubeSearchKind)>)> {
        let now = crate::signals::unix_now();
        self.query_results
            .iter()
            .filter(|(_, cached)| {
                let ttl = if cached.songs.is_empty() {
                    EMPTY_QUERY_CACHE_TTL_SECS
                } else {
                    QUERY_CACHE_TTL_SECS
                };
                !is_expired(cached.updated_at, now, ttl)
            })
            .map(|(key, cached)| {
                let songs = cached
                    .songs
                    .iter()
                    .map(|entry| (entry.song.clone(), entry.kind))
                    .collect::<Vec<_>>();
                (key.clone(), songs)
            })
            .collect()
    }

    pub fn video_meta_cache_entries(&self) -> Vec<(String, YtdlpVideoMeta)> {
        let now = crate::signals::unix_now();
        self.video_metadata
            .iter()
            .filter(|(_, cached)| !is_expired(cached.updated_at, now, VIDEO_META_CACHE_TTL_SECS))
            .map(|(key, cached)| (key.clone(), cached.meta.clone()))
            .collect()
    }

    pub fn merge_query_cache(
        &mut self,
        queries: Vec<(String, Vec<(Song, YoutubeSearchKind)>)>,
        videos: Vec<(String, YtdlpVideoMeta)>,
    ) -> bool {
        let now = crate::signals::unix_now();
        let mut changed = false;
        for (key, songs) in queries {
            if key.trim().is_empty() {
                continue;
            }
            let songs = songs
                .into_iter()
                .take(QUERY_RESULT_SONGS_MAX)
                .map(|(song, kind)| CachedQuerySong { song, kind })
                .collect::<Vec<_>>();
            self.query_results.insert(
                key,
                CachedQueryResult {
                    songs,
                    updated_at: now,
                },
            );
            changed = true;
        }
        for (key, meta) in videos {
            if key.trim().is_empty() {
                continue;
            }
            self.video_metadata.insert(
                key,
                CachedVideoMeta {
                    meta,
                    updated_at: now,
                },
            );
            changed = true;
        }
        if changed {
            let stats = self.prune();
            self.maintenance.add(stats);
            log_maintenance("merge", stats);
        }
        changed
    }

    pub(crate) fn prune(&mut self) -> CacheMaintenanceStats {
        let now = crate::signals::unix_now();
        let mut stats = CacheMaintenanceStats::default();
        stats.add(prune_match_entries(
            &mut self.source_key,
            now,
            SOURCE_KEY_CACHE_MAX,
        ));
        stats.add(prune_match_entries(&mut self.isrc, now, ISRC_CACHE_MAX));
        stats.add(prune_match_entries(
            &mut self.normalized,
            now,
            NORMALIZED_CACHE_MAX,
        ));
        stats.add(prune_album_entries(&mut self.albums, now, ALBUM_CACHE_MAX));
        stats.add(prune_query_entries(
            &mut self.query_results,
            now,
            QUERY_CACHE_MAX,
        ));
        stats.add(prune_video_entries(
            &mut self.video_metadata,
            now,
            VIDEO_META_CACHE_MAX,
        ));
        stats.size_evictions += self.prune_to_serialized_size(CACHE_MAX_BYTES as usize);
        stats
    }

    fn prune_to_serialized_size(&mut self, max_bytes: usize) -> usize {
        let mut evicted = 0;
        loop {
            let Ok(json) = serde_json::to_vec_pretty(self) else {
                return evicted;
            };
            if json.len() <= max_bytes {
                return evicted;
            }

            let mut candidates = eviction_candidates(self);
            if candidates.is_empty() {
                return evicted;
            }
            candidates.sort_by(|left, right| {
                (left.updated_at, left.kind, left.key.as_str()).cmp(&(
                    right.updated_at,
                    right.kind,
                    right.key.as_str(),
                ))
            });

            // Remove approximately the excess fraction, then measure again. This avoids one
            // full-cache serialization per record while still evicting strictly oldest-first.
            let excess = json.len().saturating_sub(max_bytes);
            let remove_count = candidates
                .len()
                .saturating_mul(excess)
                .div_ceil(json.len())
                .max(1);
            for candidate in candidates.into_iter().take(remove_count) {
                if remove_candidate(self, &candidate) {
                    evicted += 1;
                }
            }
        }
    }
}

impl CachedMatch {
    fn from_outcome(outcome: &MatchOutcome) -> Option<Self> {
        let MatchOutcome::Matched {
            key,
            score,
            display,
            title,
            artist,
            album,
            duration_secs,
            score_breakdown,
        } = outcome
        else {
            return None;
        };
        let breakdown = score_breakdown.as_deref()?;
        if breakdown.accept_blocked
            || breakdown.reject_reason.is_some()
            || !breakdown
                .reason_codes
                .iter()
                .any(|reason| reason == "policy_safe_cache")
        {
            return None;
        }
        Some(Self {
            ytm_video_id: key.clone(),
            score: *score,
            display: display.clone(),
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
            duration_secs: *duration_secs,
            source_kind: Some(breakdown.source_kind.clone()),
            quality_tier: Some(breakdown.quality_tier.clone()),
            policy_safe: true,
            updated_at: crate::signals::unix_now(),
        })
    }

    fn is_policy_safe(&self) -> bool {
        if !self.policy_safe || !self.score.is_finite() {
            return false;
        }
        match self.source_kind.as_deref() {
            Some("ytm_album_track" | "ytm_catalog_song" | "spotify_catalog") => true,
            Some("ytm_catalog_video" | "youtube_video_search" | "unknown") => matches!(
                self.quality_tier.as_deref(),
                Some("trusted_official" | "official_like")
            ),
            _ => false,
        }
    }

    fn is_fresh_policy_safe(&self, now: i64) -> bool {
        self.is_policy_safe() && !is_expired(self.updated_at, now, MATCH_CACHE_TTL_SECS)
    }

    fn outcome(&self) -> MatchOutcome {
        let score_breakdown = self.source_kind.as_ref().map(|source_kind| {
            Box::new(MatchScoreBreakdown {
                total: self.score,
                raw_total: self.score,
                source_kind: source_kind.clone(),
                quality_tier: self.quality_tier.clone().unwrap_or_default(),
                confidence_tier: "cached".to_owned(),
                ..MatchScoreBreakdown::default()
            })
        });
        MatchOutcome::Matched {
            key: self.ytm_video_id.clone(),
            score: self.score,
            display: self.display.clone(),
            title: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            duration_secs: self.duration_secs,
            score_breakdown,
        }
    }
}

pub(crate) fn album_key(input: &TrackInput) -> Option<String> {
    let album = input.album.as_deref().map(normalize_stripped)?;
    if album.is_empty() {
        return None;
    }
    if let Some(id) = input.album_id.as_deref().filter(|id| !id.trim().is_empty()) {
        return Some(format!("spotify_album:{id}"));
    }
    let artist = input
        .album_artists
        .first()
        .or_else(|| input.artists.first())
        .map(|artist| normalize(artist))
        .unwrap_or_default();
    if artist.is_empty() {
        return None;
    }
    let year = input
        .album_release_date
        .as_deref()
        .and_then(|date| date.get(0..4))
        .filter(|year| year.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or_default();
    Some(format!("album:{artist}|{album}|{year}"))
}

fn isrc_key(input: &TrackInput) -> Option<String> {
    input
        .isrc
        .as_deref()
        .map(str::trim)
        .filter(|isrc| !isrc.is_empty())
        .map(|isrc| isrc.to_ascii_uppercase())
}

fn normalized_track_key(input: &TrackInput) -> Option<String> {
    let title = normalize(&input.title);
    let stripped_title = normalize_stripped(&input.title);
    let artists = input
        .artists
        .iter()
        .map(|artist| normalize(artist))
        .collect::<Vec<_>>()
        .join("\u{1f}");
    if title.is_empty() || stripped_title.is_empty() || artists.is_empty() {
        return None;
    }
    let album = input.album.as_deref().map(normalize).unwrap_or_default();
    let duration = input
        .duration_secs
        .map(|duration| duration.to_string())
        .unwrap_or_default();
    let disc = input
        .disc_number
        .map(|disc| disc.to_string())
        .unwrap_or_default();
    let track = input
        .track_number
        .map(|track| track.to_string())
        .unwrap_or_default();
    Some(format!(
        "artists={artists}|title={title}|stripped={stripped_title}|album={album}|duration={duration}|disc={disc}|track={track}"
    ))
}

fn is_expired(updated_at: i64, now: i64, ttl_secs: i64) -> bool {
    updated_at <= 0 || now.saturating_sub(updated_at) > ttl_secs
}

fn prune_oldest<T>(
    map: &mut HashMap<String, T>,
    max_entries: usize,
    updated_at: impl Fn(&T) -> i64,
) -> usize {
    let remove_count = map.len().saturating_sub(max_entries);
    if remove_count == 0 {
        return 0;
    }
    let mut entries = map
        .iter()
        .map(|(key, cached)| (updated_at(cached), key.clone()))
        .collect::<Vec<_>>();
    entries.sort();
    for (_, key) in entries.into_iter().take(remove_count) {
        map.remove(&key);
    }
    remove_count
}

fn prune_map_batched<T>(
    map: &mut HashMap<String, T>,
    max_entries: usize,
    slack: usize,
    updated_at: impl Fn(&T) -> i64,
) -> usize {
    if map.len() <= max_entries.saturating_add(slack) {
        return 0;
    }
    prune_oldest(map, max_entries, updated_at)
}

fn prune_match_map(map: &mut HashMap<String, CachedMatch>, max_entries: usize) -> usize {
    prune_oldest(map, max_entries, |cached| cached.updated_at)
}

fn prune_album_map(map: &mut HashMap<String, CachedAlbum>, max_entries: usize) -> usize {
    prune_oldest(map, max_entries, |cached| cached.updated_at)
}

fn prune_match_entries(
    map: &mut HashMap<String, CachedMatch>,
    now: i64,
    max_entries: usize,
) -> CacheMaintenanceStats {
    let mut stats = CacheMaintenanceStats::default();
    map.retain(|key, cached| {
        if is_expired(cached.updated_at, now, MATCH_CACHE_TTL_SECS) {
            stats.expired += 1;
            false
        } else if key.trim().is_empty()
            || cached.ytm_video_id.trim().is_empty()
            || !cached.is_policy_safe()
        {
            stats.incompatible += 1;
            false
        } else {
            true
        }
    });
    stats.capacity_evictions += prune_match_map(map, max_entries);
    stats
}

fn prune_album_entries(
    map: &mut HashMap<String, CachedAlbum>,
    now: i64,
    max_entries: usize,
) -> CacheMaintenanceStats {
    let mut stats = CacheMaintenanceStats::default();
    map.retain(|key, cached| {
        if is_expired(cached.updated_at, now, ALBUM_CACHE_TTL_SECS) {
            stats.expired += 1;
            return false;
        }
        cached.tracks.retain(|track| {
            let compatible = !track.video_id.trim().is_empty() && !track.title.trim().is_empty();
            if !compatible {
                stats.incompatible += 1;
            }
            compatible
        });
        if cached.tracks.len() > ALBUM_TRACKS_MAX {
            stats.capacity_evictions += cached.tracks.len() - ALBUM_TRACKS_MAX;
            cached.tracks.truncate(ALBUM_TRACKS_MAX);
        }
        if key.trim().is_empty()
            || cached.ytm_album_id.trim().is_empty()
            || cached.tracks.is_empty()
        {
            stats.incompatible += 1;
            false
        } else {
            true
        }
    });
    stats.capacity_evictions += prune_album_map(map, max_entries);
    stats
}

fn prune_query_entries(
    map: &mut HashMap<String, CachedQueryResult>,
    now: i64,
    max_entries: usize,
) -> CacheMaintenanceStats {
    let mut stats = CacheMaintenanceStats::default();
    map.retain(|key, cached| {
        let ttl = if cached.songs.is_empty() {
            EMPTY_QUERY_CACHE_TTL_SECS
        } else {
            QUERY_CACHE_TTL_SECS
        };
        if is_expired(cached.updated_at, now, ttl) {
            stats.expired += 1;
            return false;
        }
        if cached.songs.len() > QUERY_RESULT_SONGS_MAX {
            stats.capacity_evictions += cached.songs.len() - QUERY_RESULT_SONGS_MAX;
            cached.songs.truncate(QUERY_RESULT_SONGS_MAX);
        }
        if key.trim().is_empty() {
            stats.incompatible += 1;
            false
        } else {
            true
        }
    });
    stats.capacity_evictions += prune_oldest(map, max_entries, |cached| cached.updated_at);
    stats
}

fn prune_video_entries(
    map: &mut HashMap<String, CachedVideoMeta>,
    now: i64,
    max_entries: usize,
) -> CacheMaintenanceStats {
    let mut stats = CacheMaintenanceStats::default();
    map.retain(|key, cached| {
        if is_expired(cached.updated_at, now, VIDEO_META_CACHE_TTL_SECS) {
            stats.expired += 1;
            false
        } else if key.trim().is_empty() {
            stats.incompatible += 1;
            false
        } else {
            true
        }
    });
    stats.capacity_evictions += prune_oldest(map, max_entries, |cached| cached.updated_at);
    stats
}

#[derive(Debug)]
struct EvictionCandidate {
    updated_at: i64,
    kind: u8,
    key: String,
}

fn eviction_candidates(cache: &TransferMatchCache) -> Vec<EvictionCandidate> {
    let total = cache.source_key.len()
        + cache.isrc.len()
        + cache.normalized.len()
        + cache.albums.len()
        + cache.query_results.len()
        + cache.video_metadata.len();
    let mut candidates = Vec::with_capacity(total);
    macro_rules! extend_candidates {
        ($map:expr, $kind:expr) => {
            candidates.extend($map.iter().map(|(key, cached)| EvictionCandidate {
                updated_at: cached.updated_at,
                kind: $kind,
                key: key.clone(),
            }));
        };
    }
    extend_candidates!(&cache.source_key, 0);
    extend_candidates!(&cache.isrc, 1);
    extend_candidates!(&cache.normalized, 2);
    extend_candidates!(&cache.albums, 3);
    extend_candidates!(&cache.query_results, 4);
    extend_candidates!(&cache.video_metadata, 5);
    candidates
}

fn remove_candidate(cache: &mut TransferMatchCache, candidate: &EvictionCandidate) -> bool {
    match candidate.kind {
        0 => cache.source_key.remove(&candidate.key).is_some(),
        1 => cache.isrc.remove(&candidate.key).is_some(),
        2 => cache.normalized.remove(&candidate.key).is_some(),
        3 => cache.albums.remove(&candidate.key).is_some(),
        4 => cache.query_results.remove(&candidate.key).is_some(),
        5 => cache.video_metadata.remove(&candidate.key).is_some(),
        _ => false,
    }
}

fn log_maintenance(operation: &'static str, stats: CacheMaintenanceStats) {
    if stats.removed() > 0 {
        tracing::debug!(
            operation,
            expired = stats.expired,
            incompatible = stats.incompatible,
            capacity_evictions = stats.capacity_evictions,
            size_evictions = stats.size_evictions,
            "maintained transfer match cache"
        );
    }
}

fn cache_path() -> Option<PathBuf> {
    crate::paths::cache_dir().map(|dir| dir.join("transfer-match-cache.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;
    use crate::api::ytmusic::{YoutubeSearchKind, YtdlpVideoMeta};

    fn input(title: &str, artist: &str) -> TrackInput {
        TrackInput {
            title: title.to_owned(),
            artists: vec![artist.to_owned()],
            album_artists: Vec::new(),
            album: Some("Album".to_owned()),
            album_id: None,
            album_uri: None,
            album_release_date: Some("2024-01-01".to_owned()),
            album_release_date_precision: None,
            album_total_tracks: None,
            album_type: None,
            album_art_url: None,
            disc_number: None,
            track_number: Some(1),
            duration_secs: Some(180),
            isrc: None,
            explicit: None,
            source_url: None,
            source_key: "spotify:track:source".to_owned(),
            known_video_id: None,
        }
    }

    fn matched() -> MatchOutcome {
        MatchOutcome::Matched {
            key: "ytm-video".to_owned(),
            score: 0.96,
            display: "Artist — Song".to_owned(),
            title: Some("Song".to_owned()),
            artist: Some("Artist".to_owned()),
            album: Some("Album".to_owned()),
            duration_secs: Some(180),
            score_breakdown: Some(Box::new(MatchScoreBreakdown {
                source_kind: "ytm_catalog_song".to_owned(),
                quality_tier: "catalog".to_owned(),
                total: 0.96,
                raw_total: 0.96,
                reason_codes: vec!["policy_safe_cache".to_owned()],
                ..MatchScoreBreakdown::default()
            })),
        }
    }

    fn video_meta() -> YtdlpVideoMeta {
        YtdlpVideoMeta {
            title: "Song".to_owned(),
            channel: "Artist - Topic".to_owned(),
            duration_secs: Some(180),
            live_status: None,
            is_live: None,
            was_live: None,
            media_type: None,
            description: None,
            ..YtdlpVideoMeta::default()
        }
    }

    #[test]
    fn track_cache_looks_up_by_source_isrc_then_normalized_identity() {
        let mut cache = TransferMatchCache::default();
        let mut source = input("Song", "Artist");
        source.isrc = Some("usrc17607839".to_owned());
        cache.save_match(&source, &matched());

        let mut by_source = input("Different", "Different");
        by_source.source_key = "spotify:track:source".to_owned();
        assert_eq!(cache.lookup_track(&by_source).unwrap().kind, "source_key");

        let mut by_isrc = input("Different", "Different");
        by_isrc.source_key = "spotify:track:other".to_owned();
        by_isrc.isrc = Some("USRC17607839".to_owned());
        assert_eq!(cache.lookup_track(&by_isrc).unwrap().kind, "isrc");

        let mut by_identity = input("Song", "Artist");
        by_identity.source_key = "spotify:track:third".to_owned();
        assert_eq!(cache.lookup_track(&by_identity).unwrap().kind, "normalized");
    }

    #[test]
    fn track_cache_refuses_non_policy_safe_or_generic_matches() {
        let input = input("Song", "Artist");
        let mut unsafe_outcome = matched();
        if let MatchOutcome::Matched {
            score_breakdown: Some(score),
            ..
        } = &mut unsafe_outcome
        {
            score.reason_codes.clear();
        } else {
            panic!("test match should contain a score breakdown");
        }

        let mut cache = TransferMatchCache::default();
        cache.save_match(&input, &unsafe_outcome);
        assert!(cache.lookup_track(&input).is_none());

        if let MatchOutcome::Matched {
            score_breakdown: Some(score),
            ..
        } = &mut unsafe_outcome
        {
            score.reason_codes.push("policy_safe_cache".to_owned());
            score.source_kind = "youtube_video_search".to_owned();
            score.quality_tier = "unverified_upload".to_owned();
        }
        cache.save_match(&input, &unsafe_outcome);
        assert!(cache.lookup_track(&input).is_none());
    }

    #[test]
    fn current_accept_threshold_invalidates_lower_scored_cached_match() {
        let input = input("Song", "Artist");
        let mut cache = TransferMatchCache::default();
        cache.save_match(&input, &matched());
        assert!(cache.lookup_track(&input).is_some());

        let cfg = MatchConfig {
            accept: 0.99,
            ..MatchConfig::default()
        };
        assert!(cache.retain_compatible_matches(&cfg));
        assert!(cache.lookup_track(&input).is_none());
    }

    #[test]
    fn successful_mapping_is_long_lived_but_expired_entries_never_hit() {
        let input = input("Song", "Artist");
        let mut cache = TransferMatchCache::default();
        cache.save_match(&input, &matched());

        let stale = crate::signals::unix_now() - MATCH_CACHE_TTL_SECS - 1;
        for cached in cache.source_key.values_mut() {
            cached.updated_at = stale;
        }
        for cached in cache.isrc.values_mut() {
            cached.updated_at = stale;
        }
        for cached in cache.normalized.values_mut() {
            cached.updated_at = stale;
        }

        assert!(cache.lookup_track(&input).is_none());
        let stats = cache.prune();
        assert_eq!(stats.expired, 2);
        assert!(cache.source_key.is_empty());
        assert!(cache.normalized.is_empty());
    }

    #[test]
    fn pruning_all_maps_is_deterministic_for_equal_timestamps() {
        let now = crate::signals::unix_now();
        let base = CachedMatch::from_outcome(&matched()).expect("safe match");
        let mut matches = HashMap::new();
        for key in ["c", "a", "b"] {
            matches.insert(
                key.to_owned(),
                CachedMatch {
                    updated_at: now,
                    ..base.clone()
                },
            );
        }
        assert_eq!(prune_match_map(&mut matches, 2), 1);
        assert!(!matches.contains_key("a"));

        let mut albums = HashMap::new();
        for key in ["c", "a", "b"] {
            albums.insert(
                key.to_owned(),
                CachedAlbum {
                    ytm_album_id: format!("album-{key}"),
                    title: "Album".to_owned(),
                    artist: "Artist".to_owned(),
                    year: Some("2024".to_owned()),
                    tracks: vec![CachedAlbumTrack {
                        video_id: format!("video-{key}"),
                        title: "Song".to_owned(),
                        artist: "Artist".to_owned(),
                        album: "Album".to_owned(),
                        track_number: Some(1),
                        duration_secs: Some(180),
                    }],
                    updated_at: now,
                },
            );
        }
        assert_eq!(prune_album_map(&mut albums, 2), 1);
        assert!(!albums.contains_key("a"));
    }

    #[test]
    fn incremental_pruning_uses_bounded_slack_then_evicts_in_bulk() {
        let mut entries = (0..4)
            .map(|index| (format!("key-{index}"), index as i64))
            .collect::<HashMap<_, _>>();

        assert_eq!(prune_map_batched(&mut entries, 2, 2, |updated| *updated), 0);
        assert_eq!(entries.len(), 4);

        entries.insert("key-4".to_owned(), 4);
        assert_eq!(prune_map_batched(&mut entries, 2, 2, |updated| *updated), 3);
        assert_eq!(entries.len(), 2);
        assert!(entries.contains_key("key-3"));
        assert!(entries.contains_key("key-4"));
    }

    #[test]
    fn save_match_uses_bounded_slack_then_bulk_prunes_real_cache_maps() {
        let mut cache = TransferMatchCache::default();
        let outcome = matched();
        for index in 0..SOURCE_KEY_CACHE_MAX + MATCH_CACHE_PRUNE_SLACK {
            let mut source = input(&format!("Song {index}"), "Artist");
            source.source_key = format!("spotify:track:{index}");
            cache.save_match(&source, &outcome);
        }
        assert_eq!(
            cache.source_key.len(),
            SOURCE_KEY_CACHE_MAX + MATCH_CACHE_PRUNE_SLACK
        );
        assert_eq!(
            cache.normalized.len(),
            NORMALIZED_CACHE_MAX + MATCH_CACHE_PRUNE_SLACK
        );
        assert_eq!(cache.maintenance.capacity_evictions, 0);

        let mut source = input("Song overflow", "Artist");
        source.source_key = "spotify:track:overflow".to_owned();
        cache.save_match(&source, &outcome);

        assert_eq!(cache.source_key.len(), SOURCE_KEY_CACHE_MAX);
        assert_eq!(cache.normalized.len(), NORMALIZED_CACHE_MAX);
        assert_eq!(
            cache.maintenance.capacity_evictions,
            2 * (MATCH_CACHE_PRUNE_SLACK + 1)
        );
    }

    #[test]
    fn prune_removes_incompatible_and_expired_records_from_every_map() {
        let now = crate::signals::unix_now();
        let stale_match = now - MATCH_CACHE_TTL_SECS - 1;
        let stale_album = now - ALBUM_CACHE_TTL_SECS - 1;
        let mut cache = TransferMatchCache::default();
        let mut cached_match = CachedMatch::from_outcome(&matched()).expect("safe match");
        cached_match.updated_at = stale_match;
        cache.source_key.insert("old".to_owned(), cached_match);
        cache.albums.insert(
            "old".to_owned(),
            CachedAlbum {
                ytm_album_id: "album".to_owned(),
                title: "Album".to_owned(),
                artist: "Artist".to_owned(),
                year: None,
                tracks: vec![CachedAlbumTrack {
                    video_id: "video".to_owned(),
                    title: "Song".to_owned(),
                    artist: "Artist".to_owned(),
                    album: "Album".to_owned(),
                    track_number: Some(1),
                    duration_secs: Some(180),
                }],
                updated_at: stale_album,
            },
        );
        cache.query_results.insert(
            " ".to_owned(),
            CachedQueryResult {
                songs: Vec::new(),
                updated_at: now,
            },
        );
        cache.video_metadata.insert(
            String::new(),
            CachedVideoMeta {
                meta: video_meta(),
                updated_at: now,
            },
        );

        let stats = cache.prune();

        assert_eq!(stats.expired, 2);
        assert_eq!(stats.incompatible, 2);
        assert_eq!(stats.removed(), 4);
        assert!(cache.source_key.is_empty());
        assert!(cache.albums.is_empty());
        assert!(cache.query_results.is_empty());
        assert!(cache.video_metadata.is_empty());
    }

    #[test]
    fn serialized_size_cap_evicts_oldest_records_until_bounded() {
        let now = crate::signals::unix_now();
        let base = CachedMatch::from_outcome(&matched()).expect("safe match");
        let mut cache = TransferMatchCache::default();
        for idx in 0..6 {
            cache.source_key.insert(
                format!("source-{idx}"),
                CachedMatch {
                    display: "x".repeat(2_000),
                    updated_at: now + idx,
                    ..base.clone()
                },
            );
        }

        let evicted = cache.prune_to_serialized_size(4_000);
        let bytes = serde_json::to_vec_pretty(&cache).expect("serialize bounded cache");

        assert!(evicted > 0);
        assert!(bytes.len() <= 4_000);
        assert!(!cache.source_key.contains_key("source-0"));
        assert!(cache.source_key.contains_key("source-5"));
    }

    #[test]
    fn top_level_cache_schema_defaults_newer_maps_for_older_json() {
        let cache: TransferMatchCache = serde_json::from_str(&format!(
            r#"{{"schema_version":{CACHE_SCHEMA_VERSION},"matcher_version":{MATCHER_VERSION}}}"#
        ))
        .expect("deserialize sparse prior cache schema");

        assert!(cache.source_key.is_empty());
        assert!(cache.albums.is_empty());
        assert!(cache.query_results.is_empty());
        assert!(cache.video_metadata.is_empty());
    }

    #[test]
    fn normalized_cache_identity_preserves_versions_album_and_artist_order() {
        let base = input("Song", "Artist");

        let mut version = base.clone();
        version.title = "Song (Instrumental)".to_owned();
        assert_ne!(normalized_track_key(&base), normalized_track_key(&version));

        let mut album = base.clone();
        album.album = Some("Other Album".to_owned());
        assert_ne!(normalized_track_key(&base), normalized_track_key(&album));

        let mut artists = base.clone();
        artists.artists = vec!["Guest".to_owned(), "Artist".to_owned()];
        assert_ne!(normalized_track_key(&base), normalized_track_key(&artists));
    }

    #[test]
    fn album_key_prefers_spotify_album_id_and_falls_back_to_identity() {
        let mut with_id = input("Song", "Artist");
        with_id.album_id = Some("sp-album".to_owned());
        assert_eq!(
            album_key(&with_id).as_deref(),
            Some("spotify_album:sp-album")
        );

        let mut without_id = input("Song", "Artist");
        without_id.album_artists = vec!["Album Artist".to_owned()];
        assert_eq!(
            album_key(&without_id).as_deref(),
            Some("album:album artist|album|2024")
        );
    }

    #[test]
    fn query_cache_persists_positive_and_short_lived_empty_results() {
        let mut cache = TransferMatchCache::default();
        let song = Song::from_search("video-id", "Song", "Artist", "3:00", Some("Album".into()));

        assert!(cache.merge_query_cache(vec![("catalog:empty".to_owned(), Vec::new())], vec![]));
        assert_eq!(cache.query_cache_entries().len(), 1);

        assert!(cache.merge_query_cache(
            vec![(
                "catalog:artist song".to_owned(),
                vec![(song, YoutubeSearchKind::YtmCatalogSong)],
            )],
            vec![],
        ));
        let entries = cache.query_cache_entries();
        assert_eq!(entries.len(), 2);
        let positive = entries
            .iter()
            .find(|(key, _)| key == "catalog:artist song")
            .expect("positive entry");
        assert_eq!(positive.1[0].0.video_id, "video-id");
        assert_eq!(positive.1[0].1, YoutubeSearchKind::YtmCatalogSong);

        cache
            .query_results
            .get_mut("catalog:artist song")
            .unwrap()
            .updated_at = 1;
        cache
            .query_results
            .get_mut("catalog:empty")
            .unwrap()
            .updated_at = 1;
        assert!(cache.query_cache_entries().is_empty());
    }

    #[test]
    fn video_metadata_cache_returns_only_fresh_positive_metadata() {
        let mut cache = TransferMatchCache::default();
        assert!(cache.merge_query_cache(Vec::new(), vec![("video-id".to_owned(), video_meta())]));

        let entries = cache.video_meta_cache_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "video-id");
        assert_eq!(entries[0].1.channel, "Artist - Topic");

        cache.video_metadata.get_mut("video-id").unwrap().updated_at = 1;
        assert!(cache.video_meta_cache_entries().is_empty());
    }

    #[test]
    fn prune_drops_expired_query_entries_and_caps_to_max() {
        let mut cache = TransferMatchCache::default();
        let song = Song::from_search("video-id", "Song", "Artist", "3:00", Some("Album".into()));
        let now = crate::signals::unix_now();

        cache.query_results.insert(
            "expired".to_owned(),
            CachedQueryResult {
                songs: vec![CachedQuerySong {
                    song: song.clone(),
                    kind: YoutubeSearchKind::YtmCatalogSong,
                }],
                updated_at: 1,
            },
        );
        for idx in 0..(QUERY_CACHE_MAX + 2) {
            cache.query_results.insert(
                format!("fresh-{idx}"),
                CachedQueryResult {
                    songs: vec![CachedQuerySong {
                        song: song.clone(),
                        kind: YoutubeSearchKind::YtmCatalogSong,
                    }],
                    updated_at: now - (QUERY_CACHE_MAX as i64 + 2 - idx as i64),
                },
            );
        }

        cache.prune();

        assert!(!cache.query_results.contains_key("expired"));
        assert_eq!(cache.query_results.len(), QUERY_CACHE_MAX);
        assert!(!cache.query_results.contains_key("fresh-0"));
        assert!(
            cache
                .query_results
                .contains_key(&format!("fresh-{}", QUERY_CACHE_MAX + 1))
        );
    }

    #[test]
    fn prune_drops_expired_video_meta_and_caps_to_max() {
        let mut cache = TransferMatchCache::default();
        let now = crate::signals::unix_now();

        cache.video_metadata.insert(
            "expired".to_owned(),
            CachedVideoMeta {
                meta: video_meta(),
                updated_at: 1,
            },
        );
        for idx in 0..(VIDEO_META_CACHE_MAX + 2) {
            cache.video_metadata.insert(
                format!("fresh-{idx}"),
                CachedVideoMeta {
                    meta: video_meta(),
                    updated_at: now - (VIDEO_META_CACHE_MAX as i64 + 2 - idx as i64),
                },
            );
        }

        cache.prune();

        assert!(!cache.video_metadata.contains_key("expired"));
        assert_eq!(cache.video_metadata.len(), VIDEO_META_CACHE_MAX);
        assert!(!cache.video_metadata.contains_key("fresh-0"));
        assert!(
            cache
                .video_metadata
                .contains_key(&format!("fresh-{}", VIDEO_META_CACHE_MAX + 1))
        );
    }
}
