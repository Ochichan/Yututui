//! SponsorBlock `music_offtopic` segments: the fetch actor and the response parser.
//!
//! The API is queried by a 4-character SHA-256 prefix of the video id, so the server never sees
//! the exact id. The response covers every video sharing that prefix; only the entry whose
//! `videoID` equals ours is kept. The actor mirrors the lyrics actor: latest request wins, a
//! per-video cache, and a short cooldown after a failure.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

pub use yututui_core::sponsorblock::{Segment, sanitize};

use crate::util::backpressure::{SPONSORBLOCK_QUEUE, bounded_channel};
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};
use crate::util::http;

const API_BASE: &str = "https://sponsor.ajay.app/api/skipSegments/";
const CATEGORY: &str = "music_offtopic";
const ACTION: &str = "skip";
const TIMEOUT: Duration = Duration::from_secs(8);
const JSON_MAX: usize = 512 * 1024;
const CACHE_MAX_ENTRIES: usize = 128;
/// A failed fetch is retried only after this cooldown. A resolved answer, including "no
/// segments", is kept for the session.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(120);
const USER_AGENT: &str = concat!(
    "yututui/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/Ochichan/Yututui)"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SponsorBlockRequest {
    pub video_id: String,
}

#[derive(Debug, Clone)]
pub enum SponsorBlockEvent {
    /// `segments` is empty when the track has none or the fetch failed.
    Result {
        video_id: String,
        segments: Vec<Segment>,
    },
}

/// YouTube id of a track that may have SponsorBlock data: a real YouTube video, not a local
/// file or a radio stream.
pub fn eligible_id(song: &crate::api::Song) -> Option<&str> {
    if song.is_local() || song.is_radio_station() {
        return None;
    }
    song.youtube_id()
}

/// First four lowercase hex characters of the SHA-256 of `video_id`.
pub fn hash_prefix(video_id: &str) -> String {
    let digest = Sha256::digest(video_id.as_bytes());
    format!("{:02x}{:02x}", digest[0], digest[1])
}

pub fn request_url(video_id: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("{API_BASE}{}", hash_prefix(video_id)))?;
    url.query_pairs_mut()
        .append_pair("categories", &format!("[\"{CATEGORY}\"]"))
        .append_pair("actionTypes", &format!("[\"{ACTION}\"]"));
    Ok(url)
}

#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(rename = "videoID", default)]
    video_id: String,
    #[serde(default)]
    segments: Vec<RawSegment>,
}

#[derive(Debug, Deserialize)]
struct RawSegment {
    #[serde(default)]
    segment: Vec<f64>,
    #[serde(default)]
    category: String,
    #[serde(rename = "actionType", default)]
    action_type: String,
}

/// Keep only our video's `music_offtopic` skip segments, sanitized without a duration (the
/// owners apply the duration rules at decision time).
fn segments_for(entries: &[Entry], video_id: &str) -> Vec<Segment> {
    let raw: Vec<(f64, f64)> = entries
        .iter()
        .filter(|entry| entry.video_id == video_id)
        .flat_map(|entry| &entry.segments)
        .filter(|raw| raw.category == CATEGORY && raw.action_type == ACTION)
        .filter_map(|raw| match raw.segment.as_slice() {
            [start, end] => Some((*start, *end)),
            _ => None,
        })
        .collect();
    sanitize(&raw, None)
}

/// Parse a response body for `video_id`. An empty or foreign-only array is "no segments".
pub fn parse_response(body: &[u8], video_id: &str) -> Result<Vec<Segment>, serde_json::Error> {
    let entries: Vec<Entry> = serde_json::from_slice(body)?;
    Ok(segments_for(&entries, video_id))
}

enum CacheValue {
    Found(Vec<Segment>),
    Failed(Instant),
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, (CacheValue, u64)>,
    clock: u64,
}

impl Cache {
    fn get(&mut self, video_id: &str, now: Instant) -> Option<Vec<Segment>> {
        self.clock += 1;
        let stamp = self.clock;
        let (value, used) = self.entries.get_mut(video_id)?;
        *used = stamp;
        match value {
            CacheValue::Found(segments) => Some(segments.clone()),
            CacheValue::Failed(at) if now.saturating_duration_since(*at) < FAILURE_COOLDOWN => {
                Some(Vec::new())
            }
            CacheValue::Failed(_) => {
                self.entries.remove(video_id);
                None
            }
        }
    }

    fn insert(&mut self, video_id: String, value: CacheValue) {
        self.clock += 1;
        self.entries.insert(video_id, (value, self.clock));
        while self.entries.len() > CACHE_MAX_ENTRIES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

pub struct SponsorBlockHandle {
    /// Capacity-one wake queue; the request itself lives in `latest` so a newer request
    /// replaces a pending one without a second queue entry.
    wake_tx: mpsc::Sender<()>,
    latest: Arc<Mutex<Option<SponsorBlockRequest>>>,
}

impl SponsorBlockHandle {
    pub fn fetch(&self, request: SponsorBlockRequest) -> DeliveryResult {
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let replaced_existing = latest.replace(request).is_some();
        match self.wake_tx.try_send(()) {
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
                latest.take();
                Err(DeliveryError::Closed)
            }
        }
    }
}

/// Spawn the actor; results return as [`SponsorBlockEvent`]s.
pub fn spawn<F>(emit: F) -> SponsorBlockHandle
where
    F: Fn(SponsorBlockEvent) + Send + Sync + 'static,
{
    let (wake_tx, wake_rx) = bounded_channel(SPONSORBLOCK_QUEUE);
    let latest = Arc::new(Mutex::new(None));
    tokio::spawn(run_actor(wake_rx, Arc::clone(&latest), emit));
    SponsorBlockHandle { wake_tx, latest }
}

async fn run_actor<F>(
    mut wake_rx: mpsc::Receiver<()>,
    latest: Arc<Mutex<Option<SponsorBlockRequest>>>,
    emit: F,
) where
    F: Fn(SponsorBlockEvent) + Send + Sync + 'static,
{
    let client = match http::build_no_redirect_client(USER_AGENT, TIMEOUT) {
        Ok(client) => Some(client),
        Err(error) => {
            tracing::warn!(%error, "sponsorblock http client unavailable");
            None
        }
    };
    let mut cache = Cache::default();

    while wake_rx.recv().await.is_some() {
        let Some(request) = latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            continue;
        };
        let video_id = request.video_id;
        let segments = if let Some(cached) = cache.get(&video_id, Instant::now()) {
            cached
        } else {
            match fetch(client.as_ref(), &video_id).await {
                Ok(found) => {
                    cache.insert(video_id.clone(), CacheValue::Found(found.clone()));
                    found
                }
                Err(error) => {
                    tracing::debug!(%error, video_id = %video_id, "sponsorblock fetch failed");
                    cache.insert(video_id.clone(), CacheValue::Failed(Instant::now()));
                    Vec::new()
                }
            }
        };
        tracing::info!(count = segments.len(), video_id = %video_id, "sponsorblock");
        emit(SponsorBlockEvent::Result { video_id, segments });
    }
}

async fn fetch(
    client: Option<&reqwest::Client>,
    video_id: &str,
) -> Result<Vec<Segment>, anyhow::Error> {
    let client = client.ok_or_else(|| anyhow::anyhow!("http client unavailable"))?;
    let response = client.get(request_url(video_id)?).send().await?;
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    if !status.is_success() {
        anyhow::bail!("unexpected status {status}");
    }
    let entries: Vec<Entry> = http::json_limited(response, JSON_MAX).await?;
    Ok(segments_for(&entries, video_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"[
        {"videoID": "otherVideo01", "segments": [
            {"segment": [10.0, 40.0], "category": "music_offtopic", "actionType": "skip", "UUID": "a"}
        ]},
        {"videoID": "dQw4w9WgXcQ", "segments": [
            {"segment": [60.5, 90.0], "category": "music_offtopic", "actionType": "skip", "UUID": "b"},
            {"segment": [10.0, 20.0], "category": "sponsor", "actionType": "skip"},
            {"segment": [100.0, 130.0], "category": "music_offtopic", "actionType": "mute"},
            {"segment": [5.0], "category": "music_offtopic", "actionType": "skip"},
            {"segment": [85.0, 120.0], "category": "music_offtopic", "actionType": "skip"}
        ]},
        {"videoID": "thirdVideo01", "segments": []}
    ]"#;

    #[test]
    fn hash_prefix_is_four_lowercase_hex_characters_of_sha256() {
        assert_eq!(hash_prefix("dQw4w9WgXcQ"), "5f6b");
    }

    #[test]
    fn request_url_encodes_the_query_values() {
        let url = request_url("dQw4w9WgXcQ").unwrap();
        assert_eq!(
            url.as_str(),
            "https://sponsor.ajay.app/api/skipSegments/5f6b\
             ?categories=%5B%22music_offtopic%22%5D&actionTypes=%5B%22skip%22%5D"
        );
    }

    #[test]
    fn parse_keeps_only_our_video_and_matching_segments() {
        let segments = parse_response(FIXTURE.as_bytes(), "dQw4w9WgXcQ").unwrap();
        assert_eq!(
            segments,
            vec![Segment {
                start: 60.5,
                end: 120.0
            }]
        );
        assert_eq!(
            parse_response(FIXTURE.as_bytes(), "otherVideo01")
                .unwrap()
                .len(),
            1
        );
        assert!(
            parse_response(FIXTURE.as_bytes(), "missingVid1")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn empty_array_and_garbage_bodies() {
        assert!(parse_response(b"[]", "x").unwrap().is_empty());
        assert!(parse_response(b"not json", "x").is_err());
    }

    #[test]
    fn cache_reuses_found_and_expires_failures() {
        let mut cache = Cache::default();
        let now = Instant::now();
        cache.insert("a".to_owned(), CacheValue::Found(Vec::new()));
        assert_eq!(cache.get("a", now), Some(Vec::new()));
        cache.insert("b".to_owned(), CacheValue::Failed(now));
        assert_eq!(cache.get("b", now), Some(Vec::new()));
        assert_eq!(cache.get("b", now + FAILURE_COOLDOWN), None);
        assert!(cache.get("b", now).is_none());
    }

    #[test]
    fn cache_is_bounded() {
        let mut cache = Cache::default();
        for i in 0..(CACHE_MAX_ENTRIES + 10) {
            cache.insert(format!("v{i}"), CacheValue::Found(Vec::new()));
        }
        assert_eq!(cache.entries.len(), CACHE_MAX_ENTRIES);
    }
}
