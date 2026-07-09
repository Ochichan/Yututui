use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::matching::{
    MatchOutcome, MatchScoreBreakdown, TrackInput, normalize, normalize_stripped,
};
use crate::api::Song;
use crate::api::ytmusic::{TransferAlbum, YoutubeSearchKind, YtdlpVideoMeta};
use crate::util::safe_fs;

const CACHE_SCHEMA_VERSION: u32 = 1;
const MATCHER_VERSION: u32 = 2;
const CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024;
const QUERY_CACHE_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const VIDEO_META_CACHE_TTL_SECS: i64 = 90 * 24 * 60 * 60;
const QUERY_CACHE_MAX: usize = 512;
const VIDEO_META_CACHE_MAX: usize = 2048;

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
        let cache = safe_fs::load_json_or_default_limited::<Self>(&path, CACHE_MAX_BYTES);
        if cache.schema_version == CACHE_SCHEMA_VERSION && cache.matcher_version == MATCHER_VERSION
        {
            cache
        } else {
            Self::default()
        }
    }

    pub fn save(&self) {
        let Some(path) = cache_path() else {
            return;
        };
        let mut cache = self.clone();
        cache.prune();
        if let Err(error) = safe_fs::write_private_atomic_json(&path, &cache) {
            tracing::warn!(error = %error, "failed to save transfer match cache");
        }
    }

    pub fn lookup_track(&self, input: &TrackInput) -> Option<CacheLookup> {
        if !input.source_key.trim().is_empty()
            && let Some(hit) = self.source_key.get(&input.source_key)
        {
            return Some(CacheLookup {
                outcome: hit.outcome(),
                kind: "source_key",
            });
        }
        if let Some(key) = isrc_key(input)
            && let Some(hit) = self.isrc.get(&key)
        {
            return Some(CacheLookup {
                outcome: hit.outcome(),
                kind: "isrc",
            });
        }
        if let Some(key) = normalized_track_key(input)
            && let Some(hit) = self.normalized.get(&key)
        {
            return Some(CacheLookup {
                outcome: hit.outcome(),
                kind: "normalized",
            });
        }
        None
    }

    pub fn save_match(&mut self, input: &TrackInput, outcome: &MatchOutcome) {
        let cached = match CachedMatch::from_outcome(outcome) {
            Some(cached) => cached,
            None => return,
        };
        if !input.source_key.trim().is_empty() {
            self.source_key
                .insert(input.source_key.clone(), cached.clone());
        }
        if let Some(key) = isrc_key(input) {
            self.isrc.insert(key, cached.clone());
        }
        if let Some(key) = normalized_track_key(input) {
            self.normalized.insert(key, cached);
        }
    }

    pub fn lookup_album(&self, key: &str) -> Option<&CachedAlbum> {
        self.albums.get(key)
    }

    pub fn save_album(&mut self, key: String, album: &TransferAlbum) {
        let tracks = album
            .tracks
            .iter()
            .map(|track| CachedAlbumTrack {
                video_id: track.video_id.clone(),
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                track_number: track.track_number,
                duration_secs: track.duration_secs,
            })
            .collect();
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
    }

    pub fn query_cache_entries(&self) -> Vec<(String, Vec<(Song, YoutubeSearchKind)>)> {
        let now = crate::signals::unix_now();
        self.query_results
            .iter()
            .filter(|(_, cached)| !is_expired(cached.updated_at, now, QUERY_CACHE_TTL_SECS))
            .filter_map(|(key, cached)| {
                let songs = cached
                    .songs
                    .iter()
                    .map(|entry| (entry.song.clone(), entry.kind))
                    .collect::<Vec<_>>();
                (!songs.is_empty()).then(|| (key.clone(), songs))
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
            if key.trim().is_empty() || songs.is_empty() {
                continue;
            }
            let songs = songs
                .into_iter()
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
            self.prune();
        }
        changed
    }

    fn prune(&mut self) {
        let now = crate::signals::unix_now();
        self.query_results
            .retain(|_, cached| !is_expired(cached.updated_at, now, QUERY_CACHE_TTL_SECS));
        self.video_metadata
            .retain(|_, cached| !is_expired(cached.updated_at, now, VIDEO_META_CACHE_TTL_SECS));
        prune_query_map(&mut self.query_results, QUERY_CACHE_MAX);
        prune_video_map(&mut self.video_metadata, VIDEO_META_CACHE_MAX);
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
        let breakdown = score_breakdown.as_deref();
        Some(Self {
            ytm_video_id: key.clone(),
            score: *score,
            display: display.clone(),
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
            duration_secs: *duration_secs,
            source_kind: breakdown.map(|score| score.source_kind.clone()),
            quality_tier: breakdown.map(|score| score.quality_tier.clone()),
            updated_at: crate::signals::unix_now(),
        })
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
    let title = normalize_stripped(&input.title);
    let artist = input.artists.first().map(|artist| normalize(artist))?;
    if title.is_empty() || artist.is_empty() {
        return None;
    }
    let duration = input
        .duration_secs
        .map(|duration| duration.to_string())
        .unwrap_or_default();
    Some(format!("{artist}|{title}|{duration}"))
}

fn is_expired(updated_at: i64, now: i64, ttl_secs: i64) -> bool {
    updated_at <= 0 || now.saturating_sub(updated_at) > ttl_secs
}

fn prune_query_map(map: &mut HashMap<String, CachedQueryResult>, max_entries: usize) {
    if map.len() <= max_entries {
        return;
    }
    let mut entries = map
        .iter()
        .map(|(key, cached)| (cached.updated_at, key.clone()))
        .collect::<Vec<_>>();
    entries.sort_by_key(|(updated_at, _)| *updated_at);
    for (_, key) in entries.into_iter().take(map.len() - max_entries) {
        map.remove(&key);
    }
}

fn prune_video_map(map: &mut HashMap<String, CachedVideoMeta>, max_entries: usize) {
    if map.len() <= max_entries {
        return;
    }
    let mut entries = map
        .iter()
        .map(|(key, cached)| (cached.updated_at, key.clone()))
        .collect::<Vec<_>>();
    entries.sort_by_key(|(updated_at, _)| *updated_at);
    for (_, key) in entries.into_iter().take(map.len() - max_entries) {
        map.remove(&key);
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
    fn query_cache_persists_positive_results_and_ignores_empty_results() {
        let mut cache = TransferMatchCache::default();
        let song = Song::from_search("video-id", "Song", "Artist", "3:00", Some("Album".into()));

        assert!(!cache.merge_query_cache(vec![("catalog:empty".to_owned(), Vec::new())], vec![]));
        assert!(cache.query_cache_entries().is_empty());

        assert!(cache.merge_query_cache(
            vec![(
                "catalog:artist song".to_owned(),
                vec![(song, YoutubeSearchKind::YtmCatalogSong)],
            )],
            vec![],
        ));
        let entries = cache.query_cache_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "catalog:artist song");
        assert_eq!(entries[0].1[0].0.video_id, "video-id");
        assert_eq!(entries[0].1[0].1, YoutubeSearchKind::YtmCatalogSong);

        cache
            .query_results
            .get_mut("catalog:artist song")
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
}
