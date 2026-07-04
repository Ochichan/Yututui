//! Per-track preference signals: a sidecar to the library recording how the user actually
//! engages with tracks — plays, completions, skips, dislikes — plus the raw play sequence
//! and a learned per-artist affinity. The streaming engine reads these to rank candidates.
//!
//! Capturing them is additive and backward-compatible: an absent/corrupt `signals.json`
//! loads to empty, and `Song`'s persisted shape is untouched (this is keyed by `video_id`
//! rather than widening the track type, which is stored verbatim in history/favorites/
//! playlists/the queue). Persistence mirrors [`crate::library`]/[`crate::config`]: pretty
//! JSON written atomically (temp file + rename).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::util::safe_fs;

/// Cap on per-track signal entries; oldest (non-disliked first, by `last_played_at`)
/// evicted past this so memory stays flat over long-lived installs.
const TRACKS_MAX: usize = 5000;
/// Cap on the raw play-sequence ring (for co-occurrence); oldest events evicted.
const PLAY_LOG_MAX: usize = 4000;
/// A play reaching at least this fraction of the track counts as "completed".
const COMPLETE_FRAC: f32 = 0.90;

/// Skip-completion band edges (fraction of the track heard before the skip). Tuned from
/// recsys research: a large share of skips land in the first seconds (a real rejection),
/// while a late skip is effectively a full listen (no penalty).
pub const STRONG_SKIP_FRAC: f32 = 0.10;
const MODERATE_SKIP_FRAC: f32 = 0.40;
const WEAK_SKIP_FRAC: f32 = 0.75;

/// Bounds on a learned per-artist affinity weight (kept small so no single event dominates).
const ARTIST_WEIGHT_MIN: f32 = -2.0;
const ARTIST_WEIGHT_MAX: f32 = 2.0;

// Artist-affinity deltas (small; they accumulate across many sessions). Written now so the
// streaming engine has history to rank on once it reads them.
const LIKE_DELTA: f32 = 0.30;
const FULL_PLAY_DELTA: f32 = 0.05;
const DISLIKE_DELTA: f32 = 0.60;
const SKIP_STRONG_DELTA: f32 = 0.30; // completion < 0.10
const SKIP_MODERATE_DELTA: f32 = 0.15; // 0.10..0.40
const SKIP_WEAK_DELTA: f32 = 0.05; // 0.40..0.75
// completion >= 0.75: a near-complete listen, treated as neutral (no penalty).

/// Engagement signals for a single track, keyed by `video_id` in [`Signals::tracks`].
#[derive(Debug, Default, Serialize, Deserialize, Clone)]
#[serde(default)]
struct TrackSignals {
    /// Total times this track started playing (full plays + skips).
    play_count: u32,
    /// Plays that reached at least [`COMPLETE_FRAC`].
    completed_count: u32,
    /// Times the track was skipped before completing.
    skip_count: u32,
    /// Unix seconds of the most recent play/skip/feedback (drives LRU eviction + recency).
    last_played_at: i64,
    /// Fraction (0..1) of the most recent play.
    last_completion: f32,
    /// Explicit user dislike: the streaming engine treats this as a hard block.
    disliked: bool,
}

/// All persisted preference signals, written to `<data dir>/signals.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Signals {
    /// Per-track engagement, keyed by `video_id`. Bounded by [`TRACKS_MAX`].
    tracks: HashMap<String, TrackSignals>,
    /// Learned affinity per normalized artist key, clamped to [-2, 2].
    artist_weight: HashMap<String, f32>,
    /// Raw play sequence (video_id, unix_ts) *with repeats* — the substrate for
    /// co-occurrence. Insta-skips are excluded (barely-heard tracks aren't real neighbors).
    /// Bounded ring of [`PLAY_LOG_MAX`] events, oldest evicted.
    play_log: VecDeque<(String, i64)>,
    /// In-memory version for caches derived from `play_log`.
    #[serde(skip)]
    play_log_generation: u64,
}

impl Signals {
    /// Load from disk, falling back to empty if absent or unreadable.
    pub fn load() -> Self {
        let Some(path) = signals_path() else {
            return Signals::default();
        };
        // Schema-drift tolerant: a renamed field can no longer wipe the whole play history.
        let mut sig = safe_fs::load_json_or_default::<Signals>(&path);
        sig.enforce_caps();
        sig
    }

    /// Persist atomically (temp file + rename). A missing data dir is a no-op.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = signals_path() else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
    }

    /// Whether the track is explicitly disliked.
    pub fn is_disliked(&self, video_id: &str) -> bool {
        self.tracks.get(video_id).is_some_and(|t| t.disliked)
    }

    /// The raw play sequence (with repeats), for building the co-occurrence graph.
    pub fn play_log(&self) -> &VecDeque<(String, i64)> {
        &self.play_log
    }

    /// Monotonic in-memory version for streaming caches derived from [`Self::play_log`].
    pub fn play_log_generation(&self) -> u64 {
        self.play_log_generation
    }

    /// Learned affinity for a normalized artist key (0 when unseen), in [-2, 2].
    pub fn artist_weight(&self, artist_key: &str) -> f32 {
        self.artist_weight.get(artist_key).copied().unwrap_or(0.0)
    }

    /// How many times the track has started playing (0 when unseen) — for novelty.
    pub fn play_count(&self, video_id: &str) -> u32 {
        self.tracks.get(video_id).map_or(0, |t| t.play_count)
    }

    /// Unix seconds of the most recent play, if any — for recency decay.
    pub fn last_played_at(&self, video_id: &str) -> Option<i64> {
        self.tracks.get(video_id).map(|t| t.last_played_at)
    }

    /// Fraction of plays that completed (0 when unseen) — a mild positive ranking term.
    pub fn completion_rate(&self, video_id: &str) -> f32 {
        match self.tracks.get(video_id) {
            Some(t) if t.play_count > 0 => t.completed_count as f32 / t.play_count as f32,
            _ => 0.0,
        }
    }

    /// Record a play of `video_id` that reached `completion` (0..1). `now` is unix seconds
    /// (injected for testability). A play at/above [`COMPLETE_FRAC`] also counts as completed
    /// and nudges the artist affinity up.
    pub fn record_play(&mut self, video_id: &str, artist_key: &str, completion: f32, now: i64) {
        let completion = completion.clamp(0.0, 1.0);
        let entry = self.tracks.entry(video_id.to_owned()).or_default();
        entry.play_count = entry.play_count.saturating_add(1);
        entry.last_played_at = now;
        entry.last_completion = completion;
        if completion >= COMPLETE_FRAC {
            entry.completed_count = entry.completed_count.saturating_add(1);
            self.bump_artist(artist_key, FULL_PLAY_DELTA);
        }
        self.push_play_log(video_id, now);
        self.enforce_caps();
    }

    /// Record a skip of `video_id` at `completion` (0..1). `feedback_scale` (0..1) discounts
    /// the artist penalty for low-confidence skips (short/early sessions); the skip itself is
    /// always counted. The raw play-log entry is dropped for insta-skips (barely heard).
    pub fn record_skip(
        &mut self,
        video_id: &str,
        artist_key: &str,
        completion: f32,
        now: i64,
        feedback_scale: f32,
    ) {
        let completion = completion.clamp(0.0, 1.0);
        let entry = self.tracks.entry(video_id.to_owned()).or_default();
        entry.play_count = entry.play_count.saturating_add(1);
        entry.skip_count = entry.skip_count.saturating_add(1);
        entry.last_played_at = now;
        entry.last_completion = completion;
        if completion >= STRONG_SKIP_FRAC {
            self.push_play_log(video_id, now);
        }
        let penalty = skip_penalty(completion);
        if penalty > 0.0 {
            self.bump_artist(artist_key, -penalty * feedback_scale.clamp(0.0, 1.0));
        }
        self.enforce_caps();
    }

    /// Record a like/unlike of `video_id` (favorite membership lives in the library; this
    /// only nudges the learned artist affinity so the engine can use it).
    pub fn record_like(&mut self, video_id: &str, artist_key: &str, liked: bool, now: i64) {
        let entry = self.tracks.entry(video_id.to_owned()).or_default();
        entry.last_played_at = now; // keep fresh so a like isn't evicted
        self.bump_artist(artist_key, if liked { LIKE_DELTA } else { -LIKE_DELTA });
        self.enforce_caps();
    }

    /// Flip the dislike flag on `video_id`, returning the new state. A dislike pushes the
    /// artist affinity down; undoing one restores it.
    pub fn toggle_dislike(&mut self, video_id: &str, artist_key: &str, now: i64) -> bool {
        let entry = self.tracks.entry(video_id.to_owned()).or_default();
        entry.disliked = !entry.disliked;
        entry.last_played_at = now;
        let disliked = entry.disliked;
        self.bump_artist(
            artist_key,
            if disliked {
                -DISLIKE_DELTA
            } else {
                DISLIKE_DELTA
            },
        );
        self.enforce_caps();
        disliked
    }

    fn push_play_log(&mut self, video_id: &str, now: i64) {
        self.play_log.push_back((video_id.to_owned(), now));
        while self.play_log.len() > PLAY_LOG_MAX {
            self.play_log.pop_front();
        }
        self.play_log_generation = self.play_log_generation.wrapping_add(1);
    }

    fn bump_artist(&mut self, artist_key: &str, delta: f32) {
        if artist_key.is_empty() {
            return;
        }
        let w = self
            .artist_weight
            .entry(artist_key.to_owned())
            .or_insert(0.0);
        *w = (*w + delta).clamp(ARTIST_WEIGHT_MIN, ARTIST_WEIGHT_MAX);
    }

    /// Evict the oldest track entries past [`TRACKS_MAX`], preferring to keep disliked
    /// tracks (they're a deliberate, lasting signal) by evicting non-disliked first.
    fn enforce_caps(&mut self) {
        if self.tracks.len() <= TRACKS_MAX {
            return;
        }
        let mut by_age: Vec<(bool, i64, String)> = self
            .tracks
            .iter()
            .map(|(k, v)| (v.disliked, v.last_played_at, k.clone()))
            .collect();
        // (false, older) sorts first → non-disliked, oldest evicted first.
        by_age.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let remove = self.tracks.len() - TRACKS_MAX;
        for (_, _, k) in by_age.into_iter().take(remove) {
            self.tracks.remove(&k);
        }
    }
}

/// Negative artist-affinity delta for a skip at `completion`, by severity band.
fn skip_penalty(completion: f32) -> f32 {
    if completion < STRONG_SKIP_FRAC {
        SKIP_STRONG_DELTA
    } else if completion < MODERATE_SKIP_FRAC {
        SKIP_MODERATE_DELTA
    } else if completion < WEAK_SKIP_FRAC {
        SKIP_WEAK_DELTA
    } else {
        0.0
    }
}

/// Normalize an artist string into a stable key for the affinity map (case/whitespace-
/// insensitive). Kept simple here; richer canonicalization lands with the streaming engine.
pub fn normalize_artist(artist: &str) -> String {
    artist
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Current unix time in seconds (saturating to 0 before the epoch / on clock errors).
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn signals_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui").map(|d| d.data_dir().join("signals.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_default_to_empty() {
        let s: Signals = serde_json::from_str("{}").unwrap();
        assert!(s.tracks.is_empty());
        assert!(s.artist_weight.is_empty());
        assert!(s.play_log.is_empty());
    }

    #[test]
    fn json_round_trips() {
        let mut s = Signals::default();
        s.record_play("a", "artist x", 1.0, 100);
        s.toggle_dislike("b", "artist y", 200);
        let text = serde_json::to_string(&s).unwrap();
        let back: Signals = serde_json::from_str(&text).unwrap();
        assert_eq!(back.tracks.get("a").unwrap().play_count, 1);
        assert!(back.is_disliked("b"));
        assert_eq!(back.play_log.len(), 1); // only the full play logged
    }

    #[test]
    fn full_play_counts_as_completed_and_lifts_artist() {
        let mut s = Signals::default();
        s.record_play("a", "x", 0.95, 10);
        let t = s.tracks.get("a").unwrap();
        assert_eq!(t.play_count, 1);
        assert_eq!(t.completed_count, 1);
        assert!(*s.artist_weight.get("x").unwrap() > 0.0);
    }

    #[test]
    fn short_play_is_not_completed() {
        let mut s = Signals::default();
        s.record_play("a", "x", 0.5, 10);
        let t = s.tracks.get("a").unwrap();
        assert_eq!(t.play_count, 1);
        assert_eq!(t.completed_count, 0);
        // A non-completing *play* doesn't move artist affinity (only completions do).
        assert!(!s.artist_weight.contains_key("x"));
    }

    #[test]
    fn skip_severity_scales_the_artist_penalty() {
        let strong = {
            let mut s = Signals::default();
            s.record_skip("a", "x", 0.02, 10, 1.0);
            *s.artist_weight.get("x").unwrap()
        };
        let moderate = {
            let mut s = Signals::default();
            s.record_skip("a", "x", 0.25, 10, 1.0);
            *s.artist_weight.get("x").unwrap()
        };
        let weak = {
            let mut s = Signals::default();
            s.record_skip("a", "x", 0.6, 10, 1.0);
            *s.artist_weight.get("x").unwrap()
        };
        // More negative the earlier the skip.
        assert!(strong < moderate);
        assert!(moderate < weak);
        assert!(weak < 0.0);
    }

    #[test]
    fn late_skip_is_neutral() {
        let mut s = Signals::default();
        s.record_skip("a", "x", 0.9, 10, 1.0);
        // No penalty for a near-complete listen.
        assert!(!s.artist_weight.contains_key("x"));
        assert_eq!(s.tracks.get("a").unwrap().skip_count, 1);
    }

    #[test]
    fn feedback_scale_discounts_the_penalty() {
        let full = {
            let mut s = Signals::default();
            s.record_skip("a", "x", 0.02, 10, 1.0);
            *s.artist_weight.get("x").unwrap()
        };
        let scaled = {
            let mut s = Signals::default();
            s.record_skip("a", "x", 0.02, 10, 0.3);
            *s.artist_weight.get("x").unwrap()
        };
        assert!(scaled > full); // less negative when down-weighted
        assert!(scaled < 0.0);
    }

    #[test]
    fn insta_skip_is_excluded_from_play_log() {
        let mut s = Signals::default();
        s.record_skip("a", "x", 0.02, 10, 1.0); // strong skip → not logged
        s.record_skip("b", "x", 0.5, 11, 1.0); // heard enough → logged
        let logged: Vec<&str> = s.play_log.iter().map(|(v, _)| v.as_str()).collect();
        assert_eq!(logged, vec!["b"]);
    }

    #[test]
    fn toggle_dislike_flips_and_clears() {
        let mut s = Signals::default();
        assert!(!s.is_disliked("a"));
        assert!(s.toggle_dislike("a", "x", 10));
        assert!(s.is_disliked("a"));
        let after_dislike = *s.artist_weight.get("x").unwrap();
        assert!(after_dislike < 0.0);
        assert!(!s.toggle_dislike("a", "x", 11));
        assert!(!s.is_disliked("a"));
        // Undo restores the affinity to ~zero.
        assert!((*s.artist_weight.get("x").unwrap()).abs() < f32::EPSILON);
    }

    #[test]
    fn artist_weight_is_clamped() {
        let mut s = Signals::default();
        for i in 0..50 {
            s.record_like(&format!("t{i}"), "x", true, 10 + i as i64);
        }
        assert_eq!(*s.artist_weight.get("x").unwrap(), ARTIST_WEIGHT_MAX);
    }

    #[test]
    fn play_log_is_bounded() {
        let mut s = Signals::default();
        for i in 0..(PLAY_LOG_MAX + 25) {
            s.record_play(&format!("t{i}"), "x", 1.0, i as i64);
        }
        assert_eq!(s.play_log.len(), PLAY_LOG_MAX);
        // Oldest evicted; the most recent survives.
        assert_eq!(
            s.play_log.back().unwrap().0,
            format!("t{}", PLAY_LOG_MAX + 24)
        );
    }

    #[test]
    fn tracks_are_capped_keeping_disliked() {
        let mut s = Signals::default();
        // One old disliked entry that must survive eviction.
        s.toggle_dislike("keep-disliked", "z", 1);
        for i in 0..(TRACKS_MAX + 10) {
            s.record_play(&format!("t{i}"), "x", 1.0, 1000 + i as i64);
        }
        assert_eq!(s.tracks.len(), TRACKS_MAX);
        assert!(s.is_disliked("keep-disliked"));
    }

    #[test]
    fn normalize_artist_collapses_case_and_space() {
        assert_eq!(normalize_artist("  The   Strokes "), "the strokes");
        assert_eq!(normalize_artist("BTS"), "bts");
    }
}
