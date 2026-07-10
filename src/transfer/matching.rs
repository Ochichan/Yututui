//! The direction-agnostic track matcher.
//!
//! Normalization is CJK-safe by construction: NFKC folds fullwidth/compat forms (endemic
//! in K-pop metadata), similarity is char-level normalized Levenshtein (no ASCII
//! assumptions), and punctuation stripping never touches letters of any script. Titles
//! are compared as the max over (full, annotation-stripped) forms, so dual-script names
//! like `"TT (티티)"` match either half.
//!
//! Scoring is bounded to 1.0 and combines title, primary-artist identity, featured-artist
//! coverage, duration, album/release year, requested version, and track position. Policy
//! gates are symmetric: covers, karaoke/instrumental, live/remix, edit/remaster, language,
//! and low-quality evidence can force review or rejection even when text similarity is high.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::api::Song;
use crate::api::ytmusic::{YoutubeSearchKind, YtmMusicVideoType};
use crate::spotify::models::SpotifyTrack;
use crate::streaming::musicgate;

use super::{ImportMediaKind, MatchPolicy};

mod identity;
mod music_video;
mod quality;
mod ytm_retrieval;
use identity::{CandidateDisposition, disposition, parse_version_profile, version_agreement};
use music_video::{apply_music_video_gate, is_official_video_presentation};
use quality::{confidence_tier, official_signal_score, quality_adjustment, quality_tier};
pub(crate) use ytm_retrieval::YtmMatchDiagnostics;
pub use ytm_retrieval::{
    Pacing, memo_key, memo_key_for_media, ytm_catalog_query_plan, ytm_fallback_query_plan,
    ytm_music_video_query_plan, ytm_query_plan,
};
pub(crate) use ytm_retrieval::{SharedYtmMatchState, match_track_ytm_shared};

/// One track to find a counterpart for, whichever side it came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackInput {
    pub title: String,
    pub artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub album_artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_release_date_precision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_total_tracks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_art_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Where this input came from (Spotify URI, file row, YTM video id) — for reports.
    pub source_key: String,
    /// File-restore fast path: a known YouTube id skips matching entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_video_id: Option<String>,
}

impl TrackInput {
    pub fn from_spotify(t: &SpotifyTrack) -> Self {
        Self {
            title: t.name.clone(),
            artists: t.artists.clone(),
            album_artists: t.album_artists.clone(),
            album: Some(t.album.clone()).filter(|a| !a.is_empty()),
            album_id: t.album_id.clone(),
            album_uri: t.album_uri.clone(),
            album_release_date: t.album_release_date.clone(),
            album_release_date_precision: t.album_release_date_precision.clone(),
            album_total_tracks: t.album_total_tracks,
            album_type: t.album_type.clone(),
            album_art_url: t.best_album_image_url(),
            disc_number: t.disc_number,
            track_number: t.track_number,
            duration_secs: Some(t.duration_ms / 1000).filter(|d| *d > 0),
            isrc: t.isrc.clone(),
            explicit: Some(t.explicit),
            source_url: t.spotify_url.clone(),
            source_key: t.uri.clone(),
            known_video_id: None,
        }
    }

    pub fn from_song(s: &Song) -> Self {
        Self {
            title: s.title.clone(),
            artists: if s.artists.is_empty() {
                vec![s.artist.clone()]
            } else {
                s.artists.clone()
            },
            album_artists: if s.album_artists.is_empty() {
                s.album_artist.iter().cloned().collect()
            } else {
                s.album_artists.clone()
            },
            album: s.album.clone(),
            album_id: None,
            album_uri: None,
            album_release_date: None,
            album_release_date_precision: None,
            album_total_tracks: None,
            album_type: None,
            album_art_url: s.album_art_url.clone(),
            disc_number: s.disc_number,
            track_number: s.track_number,
            duration_secs: s
                .duration_secs
                .or_else(|| crate::streaming::candidate::parse_duration_secs(&s.duration)),
            isrc: s.isrc.clone(),
            explicit: None,
            source_url: s.origin_url.clone(),
            source_key: s.origin_key.clone().unwrap_or_else(|| s.video_id.clone()),
            known_video_id: s.youtube_id().map(str::to_owned),
        }
    }

    pub fn display(&self) -> String {
        format!("{} — {}", self.artists.join(", "), self.title)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CandidateSourceKind {
    #[default]
    Unknown,
    YtmAlbumTrack,
    YtmCatalogSong,
    /// Music-scoped video-filter result. This is better retrieval provenance than a
    /// generic public search, but is not by itself proof of an official artist upload.
    YtmCatalogVideo,
    YoutubeVideoSearch,
    SpotifyCatalog,
}

impl From<YoutubeSearchKind> for CandidateSourceKind {
    fn from(kind: YoutubeSearchKind) -> Self {
        match kind {
            YoutubeSearchKind::YtmCatalogSong => Self::YtmCatalogSong,
            YoutubeSearchKind::YtmCatalogVideo | YoutubeSearchKind::YtmCatalogTypedVideo(_) => {
                Self::YtmCatalogVideo
            }
            YoutubeSearchKind::YoutubeVideoSearch => Self::YoutubeVideoSearch,
        }
    }
}

/// A candidate from the destination catalog's search.
#[derive(Debug, Clone)]
pub struct MatchCandidate {
    /// Destination identity (YTM video id / Spotify URI).
    pub key: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_secs: Option<u32>,
    pub track_number: Option<u32>,
    /// Structured metadata when the destination provider exposes it.  Search surfaces
    /// that omit these fields leave them unknown rather than inferring them from noise.
    pub release_year: Option<u16>,
    pub explicit: Option<bool>,
    pub source_kind: CandidateSourceKind,
    /// First-party YTM presentation subtype, when the video-filter response exposed it.
    pub music_video_type: Option<YtmMusicVideoType>,
    pub channel: Option<String>,
    pub channel_id: Option<String>,
    /// Provider verification is corroboration only; it is not OAC/official-upload proof.
    pub channel_verified: Option<bool>,
    pub availability: Option<String>,
    pub has_audio_format: Option<bool>,
    pub max_audio_bitrate_kbps: Option<f64>,
    pub isrc: Option<String>,
    /// Canonical title returned by full video metadata. Flat public-search rows can expose
    /// a translated title while this contains the uploader's native title; scoring keeps
    /// both independent identity signals instead of overwriting either one.
    pub(crate) metadata_title: Option<String>,
    pub(crate) preflighted: bool,
    pub(crate) preflight_reject_reason: Option<String>,
    pub(crate) preflight_reason_codes: Vec<String>,
}

impl From<&Song> for MatchCandidate {
    fn from(s: &Song) -> Self {
        Self::from_song_with_kind(s, CandidateSourceKind::Unknown)
    }
}

impl MatchCandidate {
    pub fn from_song_with_kind(s: &Song, source_kind: CandidateSourceKind) -> Self {
        Self {
            key: s.video_id.clone(),
            title: s.title.clone(),
            artist: s.artist.clone(),
            album: s.album.clone(),
            duration_secs: s
                .duration_secs
                .or_else(|| crate::streaming::candidate::parse_duration_secs(&s.duration)),
            track_number: s.track_number,
            release_year: None,
            explicit: None,
            source_kind,
            music_video_type: None,
            channel: Some(s.artist.clone()).filter(|a| !a.trim().is_empty()),
            channel_id: None,
            channel_verified: None,
            availability: None,
            has_audio_format: None,
            max_audio_bitrate_kbps: None,
            isrc: s.isrc.clone(),
            metadata_title: None,
            preflighted: false,
            preflight_reject_reason: None,
            preflight_reason_codes: Vec::new(),
        }
    }

    pub fn from_youtube_search(s: &Song, kind: YoutubeSearchKind) -> Self {
        let mut candidate = Self::from_song_with_kind(s, kind.into());
        candidate.music_video_type = kind.music_video_type();
        candidate
    }
}

impl From<&SpotifyTrack> for MatchCandidate {
    fn from(t: &SpotifyTrack) -> Self {
        Self {
            key: t.uri.clone(),
            title: t.name.clone(),
            artist: t.artists.join(", "),
            album: Some(t.album.clone()).filter(|a| !a.is_empty()),
            duration_secs: Some(t.duration_ms / 1000).filter(|d| *d > 0),
            track_number: t.track_number,
            release_year: t
                .album_release_date
                .as_deref()
                .and_then(|date| date.get(0..4))
                .and_then(|year| year.parse::<u16>().ok()),
            explicit: Some(t.explicit),
            source_kind: CandidateSourceKind::SpotifyCatalog,
            music_video_type: None,
            channel: None,
            channel_id: None,
            channel_verified: None,
            availability: None,
            has_audio_format: None,
            max_audio_bitrate_kbps: None,
            isrc: t.isrc.clone(),
            metadata_title: None,
            preflighted: false,
            preflight_reject_reason: None,
            preflight_reason_codes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MatchOutcome {
    Matched {
        key: String,
        score: f32,
        display: String,
        /// The winning candidate's metadata, kept so local-store destinations can write
        /// a real `Song` without another lookup. Optional for old checkpoints.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artist: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        album: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_secs: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        score_breakdown: Option<Box<MatchScoreBreakdown>>,
    },
    Ambiguous {
        candidates: Vec<AmbiguousCandidate>,
    },
    NotFound,
    /// Spotify local file / episode — never searched.
    SkippedLocal,
    /// Destination capacity became certain before this row needed matching.
    SkippedCapacity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AmbiguousCandidate {
    pub key: String,
    pub score: f32,
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_breakdown: Option<MatchScoreBreakdown>,
}

#[derive(Debug, Clone, Copy)]
pub struct MatchConfig {
    pub accept: f32,
    pub ambiguous_floor: f32,
    pub accept_margin: f32,
    pub policy: MatchPolicy,
    pub allow_user_videos: bool,
    pub media_kind: ImportMediaKind,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            accept: 0.80,
            ambiguous_floor: 0.60,
            accept_margin: 0.06,
            policy: MatchPolicy::Strict,
            allow_user_videos: false,
            media_kind: ImportMediaKind::Track,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MatchScoreBreakdown {
    pub total: f32,
    pub raw_total: f32,
    #[serde(default)]
    pub source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub music_video_type: Option<String>,
    #[serde(default)]
    pub quality_tier: String,
    #[serde(default)]
    pub confidence_tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_delta_secs: Option<i32>,
    pub title: f32,
    /// Primary-artist agreement. `artist` is retained for checkpoint/report
    /// compatibility and carries the same value.
    pub artist: f32,
    #[serde(default)]
    pub artist_coverage: f32,
    pub duration: f32,
    #[serde(default)]
    pub album_year: f32,
    #[serde(default)]
    pub version_agreement: f32,
    #[serde(default)]
    pub evidence_completeness: f32,
    pub album_bonus: f32,
    #[serde(default)]
    pub track_number_bonus: f32,
    #[serde(default)]
    pub quality_bonus: f32,
    #[serde(default)]
    pub corroboration_bonus: f32,
    #[serde(default)]
    pub identity_penalty: f32,
    #[serde(default)]
    pub non_music_penalty: f32,
    #[serde(default)]
    pub accept_blocked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
}

// Normalization ------------------------------------------------------------------

/// NFKC → lowercase → `&`/`×` become " and " → drop punctuation/symbols (letters of any
/// script survive) → collapse whitespace.
pub fn normalize(s: &str) -> String {
    let nfkc: String = s.nfkc().collect();
    let lower = nfkc.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    for c in lower.chars() {
        match c {
            '&' | '×' => out.push_str(" and "),
            c if c.is_alphanumeric() => out.push(c),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove the conservative annotation set before normalizing: feat-credits and
/// remaster/edition/version boilerplate inside brackets or after a ` - ` dash, plus
/// translation glosses — a bracketed segment in a *different script* than the base
/// title (`"TT (티티)"`, `"사건의 지평선 (Event Horizon)"`) — plus YouTube-video noise
/// phrases ("Official MV", "Lyric Video", …), since degraded matching runs against
/// yt-dlp video results whose titles carry them. Identity-changing markers (live,
/// remix, acoustic, instrumental, covers) are deliberately kept — they participate in
/// similarity instead.
pub fn normalize_stripped(s: &str) -> String {
    strip_video_noise(&normalize(&strip_translation_brackets(&strip_annotations(
        s,
    ))))
}

/// Phrase-level video-title noise, removed from the *normalized* form (post-punctuation,
/// so `"M/V"` is already `"m v"`). Single generic words ("music", "video") are only ever
/// removed as part of these phrases — "Video Games" stays intact.
fn strip_video_noise(normalized: &str) -> String {
    const NOISE_PHRASES: [&str; 12] = [
        "official music video",
        "official lyric video",
        "official video",
        "official audio",
        "official mv",
        "music video",
        "lyric video",
        "lyrics video",
        "official visualizer",
        "visualizer",
        "official mv teaser",
        "뮤직비디오",
    ];
    let mut out = format!(" {normalized} ");
    for phrase in NOISE_PHRASES {
        out = out.replace(&format!(" {phrase} "), " ");
    }
    // Bare `m v` (from "M/V") and `mv` tokens.
    out = out.replace(" m v ", " ").replace(" mv ", " ");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_noise_annotation(inner: &str) -> bool {
    let inner = inner.trim().to_lowercase();
    const PREFIXES: [&str; 5] = ["feat.", "feat ", "ft.", "ft ", "featuring "];
    if PREFIXES.iter().any(|p| inner.starts_with(p)) || inner.starts_with("with ") {
        return true;
    }
    const NOISE: [&str; 12] = [
        "remaster",
        "remastered",
        "deluxe",
        "expanded",
        "anniversary",
        "special edition",
        "bonus track",
        "radio edit",
        "single version",
        "album version",
        "mono",
        "stereo",
    ];
    NOISE.iter().any(|n| inner.contains(n))
    // "2011 remaster" / "remastered 2015" style year combos hit `contains` above;
    // a bare year alone is kept (could be part of the title).
}

fn strip_annotations(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '(' || c == '[' {
            let close = if c == '(' { ')' } else { ']' };
            let mut inner = String::new();
            let mut depth = 1;
            for c2 in chars.by_ref() {
                if c2 == c {
                    depth += 1;
                } else if c2 == close {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                inner.push(c2);
            }
            if !is_noise_annotation(&inner) {
                out.push(c);
                out.push_str(&inner);
                out.push(close);
            }
        } else {
            out.push(c);
        }
    }
    // ` - 2011 Remaster` style dash suffixes.
    if let Some(idx) = out.rfind(" - ")
        && is_noise_annotation(&out[idx + 3..])
    {
        out.truncate(idx);
    }
    out
}

/// Drop bracketed segments whose script disagrees with the rest of the title (the
/// K-pop/J-pop dual-script gloss pattern) — unless the segment is an identity marker.
fn strip_translation_brackets(s: &str) -> String {
    let mut outside = String::new();
    let mut segments: Vec<(usize, String)> = Vec::new(); // (insert position in `outside`, inner)
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '(' || c == '[' {
            let close = if c == '(' { ')' } else { ']' };
            let mut inner = String::new();
            let mut depth = 1;
            for c2 in chars.by_ref() {
                if c2 == c {
                    depth += 1;
                } else if c2 == close {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                inner.push(c2);
            }
            segments.push((outside.len(), inner));
        } else {
            outside.push(c);
        }
    }
    if segments.is_empty() {
        return s.to_owned();
    }
    let base_ascii = outside
        .chars()
        .filter(|c| c.is_alphanumeric())
        .any(|c| c.is_ascii());
    let base_other = outside
        .chars()
        .filter(|c| c.is_alphanumeric())
        .any(|c| !c.is_ascii());
    let mut kept = outside.clone();
    let mut offset = 0usize;
    for (pos, inner) in segments {
        if is_translation_gloss(base_ascii, base_other, &inner) {
            continue;
        }
        let insert = format!("({inner})");
        kept.insert_str(pos + offset, &insert);
        offset += insert.len();
    }
    kept
}

fn is_translation_gloss(base_ascii: bool, base_other: bool, inner: &str) -> bool {
    let letters: Vec<char> = inner.chars().filter(|c| c.is_alphanumeric()).collect();
    if letters.is_empty() {
        return false;
    }
    // Identity markers stay even across scripts.
    const IDENTITY: [&str; 8] = [
        "live",
        "remix",
        "acoustic",
        "inst",
        "instrumental",
        "ver",
        "version",
        "cover",
    ];
    let lower = inner.to_lowercase();
    if IDENTITY.iter().any(|m| lower.contains(m)) {
        return false;
    }
    let inner_all_ascii = letters.iter().all(char::is_ascii);
    let inner_no_ascii = letters.iter().all(|c| !c.is_ascii());
    // Pure-ASCII base with a non-ASCII gloss, or the mirror image.
    (inner_no_ascii && base_ascii && !base_other) || (inner_all_ascii && base_other && !base_ascii)
}

// Similarity ---------------------------------------------------------------------

/// Char-level normalized Levenshtein: `1 − dist/max_chars`. CJK works unmodified.
pub fn similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let dist = prev[b.len()];
    1.0 - dist as f32 / a.len().max(b.len()) as f32
}

fn containment(a: &str, b: &str) -> bool {
    a.len() >= 2 && b.len() >= 2 && (a.contains(b) || b.contains(a))
}

fn token_sort_similarity(a: &str, b: &str) -> f32 {
    let sorted = |value: &str| -> String {
        let mut tokens = normalize(value)
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        tokens.sort();
        tokens.join(" ")
    };
    similarity(&sorted(a), &sorted(b))
}

fn token_set_similarity(a: &str, b: &str) -> f32 {
    let set = |value: &str| -> HashSet<String> {
        normalize(value)
            .split_whitespace()
            .filter(|token| token.chars().count() >= 2)
            .map(str::to_owned)
            .collect()
    };
    let a = set(a);
    let b = set(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(&b).count() as f32;
    2.0 * intersection / (a.len() + b.len()) as f32
}

fn title_similarity(input: &TrackInput, cand: &MatchCandidate) -> f32 {
    let search_title = title_similarity_text(input, &cand.title);
    cand.metadata_title
        .as_deref()
        .map(|title| search_title.max(title_similarity_text(input, title)))
        .unwrap_or(search_title)
}

fn title_similarity_text(input: &TrackInput, candidate_title: &str) -> f32 {
    let input_stripped = normalize_stripped(&input.title);
    let cand_stripped = normalize_stripped(candidate_title);
    let full = similarity(&normalize(&input.title), &normalize(candidate_title));
    let stripped = similarity(&input_stripped, &cand_stripped);
    let tokens = token_set_similarity(&input.title, candidate_title)
        .max(token_sort_similarity(&input.title, candidate_title));
    let mut best = full.max(stripped).max(tokens);

    let mut without_artists = format!(" {cand_stripped} ");
    for a in &input.artists {
        let a = normalize(a);
        if a.len() >= 2 {
            without_artists = without_artists.replace(&format!(" {a} "), " ");
        }
    }
    let without_artists = without_artists
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if without_artists != cand_stripped && !without_artists.is_empty() {
        best = best.max(similarity(&input_stripped, &without_artists));
    }

    let contain = |needle: &str, hay: &str| -> f32 {
        let n = needle.chars().count();
        if n >= 3 && hay.contains(needle) {
            0.75 + 0.25 * (n as f32 / hay.chars().count().max(1) as f32)
        } else {
            0.0
        }
    };
    let cand_clean = strip_video_noise(&normalize(&strip_annotations(candidate_title)));
    best = best
        .max(contain(&input_stripped, &cand_clean))
        .max(contain(&cand_stripped, &input_stripped));

    // Full YouTube metadata commonly uses `native artist - native title`, while Spotify may
    // spell the artist in another script. Compare the structured suffix directly instead of
    // requiring the source spelling to be removable from the prefix. The independent artist
    // gate still decides whether the upload belongs to the requested artist.
    if let Some(suffix) = title_artist_credit_suffix(candidate_title) {
        let suffix_stripped = normalize_stripped(suffix);
        let suffix_clean = strip_video_noise(&normalize(&strip_annotations(suffix)));
        best = best
            .max(similarity(&input_stripped, &suffix_stripped))
            .max(token_sort_similarity(&input.title, suffix))
            .max(token_set_similarity(&input.title, suffix))
            .max(contain(&input_stripped, &suffix_clean));
    }
    best
}

fn single_artist_similarity(candidate: &str, artist: &str) -> f32 {
    let artist = normalize(artist);
    if artist.is_empty() {
        return 0.0;
    }
    if containment(candidate, &artist) {
        1.0
    } else {
        similarity(&artist, candidate)
    }
}

fn candidate_artist_text(cand: &MatchCandidate) -> String {
    let cand_artist = normalize(&cand.artist);
    cand_artist
        .strip_suffix(" topic")
        .unwrap_or(&cand_artist)
        .to_owned()
}

fn title_artist_credit_prefix(title: &str) -> Option<&str> {
    [" - ", " – ", " — "]
        .into_iter()
        .filter_map(|separator| title.find(separator))
        .min()
        .map(|index| title[..index].trim())
        .filter(|prefix| !prefix.is_empty())
}

fn title_artist_credit_suffix(title: &str) -> Option<&str> {
    [" - ", " – ", " — "]
        .into_iter()
        .filter_map(|separator| title.find(separator).map(|index| (index, separator.len())))
        .min_by_key(|(index, _)| *index)
        .map(|(index, separator_len)| title[index + separator_len..].trim())
        .filter(|suffix| !suffix.is_empty())
}

/// Some official channels use a native-script channel name while spelling the artist in
/// the video title (`Aimyon - Marigold` on the `あいみょん` channel). Treat that title
/// credit as primary-artist evidence only when both halves are independently strong:
///
/// - the prefix is an almost-exact, symmetric match (never mere containment), and
/// - the upload has a strong official signal plus trusted/catalog provenance, or verified
///   metadata whose native title independently credits its channel.
///
/// This deliberately excludes an unverified user merely writing "Official" in a title,
/// and excludes prefixes such as `Aimyon Fan Channel` which only contain the artist name.
fn corroborated_title_artist_similarity(input: &TrackInput, cand: &MatchCandidate) -> Option<f32> {
    if official_signal_score(cand) < 0.7 {
        return None;
    }

    let channel = cand.channel.as_deref().unwrap_or(&cand.artist);
    let trusted_or_catalog = musicgate::is_trusted_music_channel(channel)
        || matches!(
            cand.source_kind,
            CandidateSourceKind::YtmAlbumTrack
                | CandidateSourceKind::YtmCatalogSong
                | CandidateSourceKind::SpotifyCatalog
        );
    let verified_preflight = cand.preflighted
        && cand.channel_verified == Some(true)
        && cand
            .preflight_reason_codes
            .iter()
            .any(|reason| reason == "metadata_title_channel_corroborated");
    if !trusted_or_catalog && !verified_preflight {
        return None;
    }

    let prefix = normalize(title_artist_credit_prefix(&cand.title)?);
    let primary_artist = normalize(input.artists.first()?);
    if prefix.is_empty() || primary_artist.is_empty() {
        return None;
    }
    let score =
        similarity(&prefix, &primary_artist).max(token_sort_similarity(&prefix, &primary_artist));
    (score >= 0.92).then_some(score)
}

fn artist_similarities(input: &TrackInput, cand: &MatchCandidate) -> (f32, f32, bool) {
    let cand_artist = candidate_artist_text(cand);
    let channel_primary = input
        .artists
        .first()
        .map(|artist| single_artist_similarity(&cand_artist, artist))
        .unwrap_or(0.0);
    let combined = format!("{cand_artist} {}", normalize(&cand.title));
    let coverage = if input.artists.is_empty() {
        0.0
    } else {
        input
            .artists
            .iter()
            .map(|artist| single_artist_similarity(&combined, artist))
            .sum::<f32>()
            / input.artists.len() as f32
    };
    // Compilation metadata occasionally names the album artist instead of the track
    // artist.  It is useful corroboration but can never fully replace the primary artist.
    let album_corroboration = input
        .album_artists
        .iter()
        .map(|artist| single_artist_similarity(&cand_artist, artist) * 0.85)
        .fold(0.0f32, f32::max);
    let baseline = channel_primary.max(album_corroboration);
    let title_credit = corroborated_title_artist_similarity(input, cand).unwrap_or(0.0);
    (
        baseline.max(title_credit),
        coverage,
        title_credit > baseline,
    )
}

#[derive(Default)]
struct IdentityGate {
    hard_reject: Option<&'static str>,
    accept_blocked: bool,
    penalty: f32,
    reasons: Vec<&'static str>,
}

fn identity_gate(input: &TrackInput, cand: &MatchCandidate) -> IdentityGate {
    let src = parse_version_profile(
        &input.title,
        &input.artists.join(", "),
        input.album.as_deref(),
        None,
        input.explicit,
    );
    let dst = parse_version_profile(
        &cand.title,
        &cand.artist,
        cand.album.as_deref(),
        cand.channel.as_deref(),
        cand.explicit,
    );
    let mut gate = IdentityGate::default();

    match disposition(&src, &dst) {
        CandidateDisposition::AutoEligible => {}
        CandidateDisposition::ReviewOnly(reason) => {
            gate.accept_blocked = true;
            gate.penalty = identity_penalty_for(reason);
            gate.reasons.push(reason);
        }
        CandidateDisposition::Rejected(reason) => {
            gate.hard_reject = Some(reason);
            gate.reasons.push(reason);
        }
    }

    gate
}

fn identity_penalty_for(reason: &str) -> f32 {
    match reason {
        "instrumental_mismatch" | "vocal_version_mismatch" => 0.30,
        "cover_mismatch" | "remix_mismatch" | "missing_mix_marker" => 0.24,
        "live_mismatch" | "missing_performance_marker" => 0.18,
        "clean_mismatch" | "explicit_mismatch" => 0.08,
        _ => 0.16,
    }
}

fn source_kind_code(kind: CandidateSourceKind) -> &'static str {
    match kind {
        CandidateSourceKind::Unknown => "unknown",
        CandidateSourceKind::YtmAlbumTrack => "ytm_album_track",
        CandidateSourceKind::YtmCatalogSong => "ytm_catalog_song",
        CandidateSourceKind::YtmCatalogVideo => "ytm_catalog_video",
        CandidateSourceKind::YoutubeVideoSearch => "youtube_video_search",
        CandidateSourceKind::SpotifyCatalog => "spotify_catalog",
    }
}

fn soft_duration_limit(input: &TrackInput) -> u32 {
    input
        .duration_secs
        .map(|duration| ((duration as f32 * 0.04).round() as u32).clamp(8, 20))
        .unwrap_or(10)
}

fn hard_duration_limit(input: &TrackInput) -> u32 {
    input
        .duration_secs
        .map(|duration| ((duration as f32 * 0.20).round() as u32).max(30))
        .unwrap_or(15 * 60)
}

fn duration_delta_secs(input: &TrackInput, cand: &MatchCandidate) -> Option<i32> {
    let input = i32::try_from(input.duration_secs?).ok()?;
    let cand = i32::try_from(cand.duration_secs?).ok()?;
    Some(cand - input)
}

fn official_video_runtime_limit(input: &TrackInput) -> u32 {
    input
        .duration_secs
        .map(|duration| ((duration as f32 * 0.16).round() as u32).clamp(20, 45))
        .unwrap_or(30)
}

fn duration_component(input: &TrackInput, cand: &MatchCandidate) -> f32 {
    match duration_delta_secs(input, cand) {
        Some(delta) if delta.unsigned_abs() <= 3 => 1.0,
        Some(delta) if delta.unsigned_abs() <= soft_duration_limit(input) => 0.72,
        Some(delta)
            if delta > 0
                && is_official_video_presentation(cand)
                && delta.unsigned_abs() <= official_video_runtime_limit(input) =>
        {
            0.58
        }
        Some(delta) if delta.unsigned_abs() <= hard_duration_limit(input) => 0.18,
        Some(_) => 0.0,
        None => 0.5,
    }
}

fn input_release_year(input: &TrackInput) -> Option<u16> {
    release_year(input)?.parse().ok()
}

fn album_year_component(input: &TrackInput, cand: &MatchCandidate) -> f32 {
    let album = match (&input.album, &cand.album) {
        (Some(source), Some(candidate)) => {
            let source = normalize_stripped(source);
            let candidate = normalize_stripped(candidate);
            if source.is_empty() || candidate.is_empty() {
                0.5
            } else if containment(&source, &candidate) {
                1.0
            } else {
                similarity(&source, &candidate)
            }
        }
        _ => 0.5,
    };
    match (input_release_year(input), cand.release_year) {
        (Some(source), Some(candidate)) => {
            let year = if source == candidate {
                1.0
            } else if source.abs_diff(candidate) <= 1 {
                0.7
            } else {
                0.0
            };
            0.75 * album + 0.25 * year
        }
        _ => album,
    }
}

fn version_component(input: &TrackInput, cand: &MatchCandidate) -> f32 {
    let source = parse_version_profile(
        &input.title,
        &input.artists.join(", "),
        input.album.as_deref(),
        None,
        input.explicit,
    );
    let candidate = parse_version_profile(
        &cand.title,
        &cand.artist,
        cand.album.as_deref(),
        cand.channel.as_deref(),
        cand.explicit,
    );
    version_agreement(&source, &candidate)
}

fn evidence_completeness(input: &TrackInput, cand: &MatchCandidate) -> f32 {
    let available = [
        input.duration_secs.is_some() && cand.duration_secs.is_some(),
        input.album.is_some() && cand.album.is_some(),
        input_release_year(input).is_some() && cand.release_year.is_some(),
        input.track_number.is_some() && cand.track_number.is_some(),
        input.explicit.is_some() && cand.explicit.is_some(),
        !matches!(cand.source_kind, CandidateSourceKind::Unknown),
    ];
    available.into_iter().filter(|present| *present).count() as f32 / available.len() as f32
}

/// Score one candidate against the input.
pub fn score_candidate(input: &TrackInput, cand: &MatchCandidate) -> f32 {
    score_candidate_breakdown(input, cand).total
}

/// Explain one candidate score against the input.
pub fn score_candidate_breakdown(input: &TrackInput, cand: &MatchCandidate) -> MatchScoreBreakdown {
    score_candidate_breakdown_with_config(input, cand, &MatchConfig::default())
}

pub fn score_candidate_breakdown_with_config(
    input: &TrackInput,
    cand: &MatchCandidate,
    cfg: &MatchConfig,
) -> MatchScoreBreakdown {
    let title = title_similarity(input, cand);
    let (artist, artist_coverage, used_title_artist_credit) = artist_similarities(input, cand);
    let duration = duration_component(input, cand);
    let album_year = album_year_component(input, cand);
    let version_agreement = version_component(input, cand);
    let evidence_completeness = evidence_completeness(input, cand);
    let track_number = match (input.track_number, cand.track_number) {
        (Some(a), Some(b)) if a == b => 1.0,
        (Some(_), Some(_)) => 0.0,
        _ => 0.5,
    };
    let album_bonus = 0.08 * album_year;
    let track_number_bonus = 0.02 * track_number;

    // Bounded seed model.  Each term has a distinct semantic role so missing metadata
    // cannot be mistaken for negative evidence and quality adjustments remain auditable.
    let raw_total = (0.42 * title
        + 0.23 * artist
        + 0.08 * artist_coverage
        + 0.12 * duration
        + album_bonus
        + 0.05 * version_agreement
        + track_number_bonus)
        .clamp(0.0, 1.0);
    let mut gate = identity_gate(input, cand);
    if cfg.media_kind == ImportMediaKind::MusicVideo {
        apply_music_video_gate(&mut gate, cand, title, artist);
    }
    let delta_secs = duration_delta_secs(input, cand);
    if let Some(delta) = delta_secs.map(i32::unsigned_abs) {
        if delta > hard_duration_limit(input)
            && !(delta_secs.is_some_and(|signed| signed > 0)
                && is_official_video_presentation(cand)
                && delta <= official_video_runtime_limit(input))
        {
            gate.hard_reject = Some("duration_mismatch");
            gate.reasons.push("duration_mismatch");
        } else if delta > soft_duration_limit(input)
            && !(delta_secs.is_some_and(|signed| signed > 0)
                && is_official_video_presentation(cand)
                && delta <= official_video_runtime_limit(input))
        {
            gate.accept_blocked = true;
            gate.reasons.push("duration_mismatch");
        } else if delta > soft_duration_limit(input) {
            gate.reasons.push("official_video_runtime");
        }
    }
    if input.duration_secs.is_none()
        && cand
            .duration_secs
            .is_some_and(|duration| duration > 15 * 60)
    {
        gate.accept_blocked = true;
        gate.reasons.push("long_mix_duration");
    }
    let generic_video_allowed = cfg.allow_user_videos || cfg.policy == MatchPolicy::Aggressive;
    let channel = cand.channel.as_deref().unwrap_or(&cand.artist);
    let needs_metadata_evidence = match cand.source_kind {
        CandidateSourceKind::YtmCatalogVideo => true,
        CandidateSourceKind::YoutubeVideoSearch | CandidateSourceKind::Unknown => {
            !musicgate::is_trusted_music_channel(channel)
        }
        CandidateSourceKind::YtmAlbumTrack
        | CandidateSourceKind::YtmCatalogSong
        | CandidateSourceKind::SpotifyCatalog => false,
    };
    if !generic_video_allowed
        && matches!(
            cand.source_kind,
            CandidateSourceKind::YtmCatalogVideo
                | CandidateSourceKind::YoutubeVideoSearch
                | CandidateSourceKind::Unknown
        )
        && (official_signal_score(cand) < 0.7 || (needs_metadata_evidence && !cand.preflighted))
    {
        gate.accept_blocked = true;
        gate.reasons.push("unverified_youtube_upload");
    }
    // A failed metadata lookup is unknown evidence, never positive evidence. This block is
    // policy-independent: `--allow-user-videos` may permit a complete generic row, but it must
    // not turn a finalist whose verification actually failed into an automatic match.
    if cand
        .preflight_reason_codes
        .iter()
        .any(|reason| reason == "metadata_preflight_failed")
    {
        gate.accept_blocked = true;
        gate.reasons.push("metadata_preflight_failed");
    }
    if cand
        .preflight_reason_codes
        .iter()
        .any(|reason| reason == "low_audio_ceiling")
        && !matches!(
            cand.source_kind,
            CandidateSourceKind::YtmAlbumTrack
                | CandidateSourceKind::YtmCatalogSong
                | CandidateSourceKind::SpotifyCatalog
        )
    {
        gate.accept_blocked = true;
        gate.penalty += 0.05;
        gate.reasons.push("low_audio_ceiling");
    }
    let (quality_bonus, non_music_penalty, quality_reasons) = quality_adjustment(cand);
    let identity_penalty = gate.penalty.min(0.75);
    let mut total =
        (raw_total + quality_bonus - non_music_penalty - identity_penalty).clamp(0.0, 1.0);
    let mut reject_reason = gate.hard_reject.map(str::to_owned);
    if reject_reason.is_none() {
        reject_reason = cand.preflight_reject_reason.clone();
    }
    if reject_reason.is_none()
        && let Some(reason) = musicgate::non_music_reason(&cand.title, channel)
    {
        reject_reason = Some(reason.to_owned());
    }
    if reject_reason.is_some() {
        total = 0.0;
    }
    let accept_blocked = gate.accept_blocked;
    let mut reason_codes: Vec<String> = gate.reasons.into_iter().map(str::to_owned).collect();
    reason_codes.extend(quality_reasons.into_iter().map(str::to_owned));
    reason_codes.extend(cand.preflight_reason_codes.iter().cloned());
    if used_title_artist_credit {
        reason_codes.push("corroborated_title_artist_credit".to_owned());
    }
    let mut seen_reason_codes = HashSet::new();
    reason_codes.retain(|reason| seen_reason_codes.insert(reason.clone()));
    let confidence_tier = confidence_tier(
        cand,
        total,
        title,
        artist,
        delta_secs.map(i32::unsigned_abs),
        accept_blocked,
        reject_reason.as_deref(),
    );
    MatchScoreBreakdown {
        total,
        raw_total,
        source_kind: source_kind_code(cand.source_kind).to_owned(),
        music_video_type: cand
            .music_video_type
            .map(|video_type| video_type.code().to_owned()),
        quality_tier: quality_tier(cand).to_owned(),
        confidence_tier: confidence_tier.to_owned(),
        duration_delta_secs: delta_secs,
        title,
        artist,
        artist_coverage,
        duration,
        album_year,
        version_agreement,
        evidence_completeness,
        album_bonus,
        track_number_bonus,
        quality_bonus,
        corroboration_bonus: 0.0,
        identity_penalty,
        non_music_penalty,
        accept_blocked,
        reject_reason,
        reason_codes,
    }
}

fn candidate_cluster_key(candidate: &MatchCandidate) -> String {
    let mut artist = candidate_artist_text(candidate);
    for suffix in [" official", " official artist", " records", " vevo"] {
        if let Some(stripped) = artist.strip_suffix(suffix) {
            artist = stripped.trim().to_owned();
        }
    }
    let mut title = normalize_stripped(&candidate.title);
    if let Some(stripped) = title.strip_prefix(&format!("{artist} ")) {
        title = stripped.trim().to_owned();
    }
    let version = parse_version_profile(
        &candidate.title,
        &candidate.artist,
        candidate.album.as_deref(),
        candidate.channel.as_deref(),
        candidate.explicit,
    );
    format!(
        "{artist}\u{1f}{title}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}\u{1f}{:?}",
        version.vocal,
        version.performance,
        version.mix,
        version.authorship,
        version.edition,
        version.explicit
    )
}

/// Rank candidates and classify against the thresholds.
pub fn best_outcome(
    input: &TrackInput,
    candidates: &[MatchCandidate],
    cfg: &MatchConfig,
) -> MatchOutcome {
    let mut scored: Vec<(MatchScoreBreakdown, &MatchCandidate)> = candidates
        .iter()
        .map(|c| (score_candidate_breakdown_with_config(input, c, cfg), c))
        .collect();
    let mut cluster_sources: HashMap<String, HashSet<&'static str>> = HashMap::new();
    for (score, candidate) in &scored {
        if score.reject_reason.is_none() {
            cluster_sources
                .entry(candidate_cluster_key(candidate))
                .or_default()
                .insert(source_kind_code(candidate.source_kind));
        }
    }
    for (score, candidate) in &mut scored {
        let sources = cluster_sources
            .get(&candidate_cluster_key(candidate))
            .map_or(0, HashSet::len);
        if sources >= 2 {
            let bonus = (0.01 * (sources.saturating_sub(1) as f32)).min(0.025);
            score.corroboration_bonus = bonus;
            score.quality_bonus += bonus;
            score.total = (score.total + bonus).clamp(0.0, 1.0);
            score.reason_codes.push("evidence_corroborated".to_owned());
        }
    }
    scored.sort_by(|a, b| {
        b.0.total
            .partial_cmp(&a.0.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let review_candidates = || {
        let mut seen_clusters = HashSet::new();
        scored
            .iter()
            .filter(|(score, candidate)| {
                score.reject_reason.is_none()
                    && score.total >= cfg.ambiguous_floor
                    && seen_clusters.insert(candidate_cluster_key(candidate))
            })
            .take(3)
            .map(|(score, candidate)| AmbiguousCandidate {
                key: candidate.key.clone(),
                score: score.total,
                display: format!("{} — {}", candidate.artist, candidate.title),
                score_breakdown: Some(score.clone()),
            })
            .collect::<Vec<_>>()
    };
    // Auto-selection is a decision among candidates which are actually eligible under the
    // current policy. A higher-scoring review-only upload must not hide a safe official result,
    // and it must not consume the acceptance margin as though it could win automatically.
    let auto_eligible =
        |score: &MatchScoreBreakdown| score.reject_reason.is_none() && !score.accept_blocked;
    let policy_independent = |score: &MatchScoreBreakdown| {
        auto_eligible(score)
            && (matches!(
                score.source_kind.as_str(),
                "ytm_album_track" | "ytm_catalog_song" | "spotify_catalog"
            ) || matches!(
                score.quality_tier.as_str(),
                "trusted_official" | "official_like"
            ))
    };
    let mut seen_eligible_clusters = HashSet::new();
    let mut eligible = scored.iter().filter(|(score, candidate)| {
        auto_eligible(score) && seen_eligible_clusters.insert(candidate_cluster_key(candidate))
    });
    if let Some((score, best)) = eligible.next() {
        let eligible_margin = eligible
            .next()
            .map_or(score.total, |(second, _)| score.total - second.total);
        if score.total >= cfg.accept && eligible_margin >= cfg.accept_margin {
            let mut score = score.clone();
            // Persistent matches are policy-independent only when they would also survive the
            // default Strict gate and margin. Loose-policy generic uploads are deliberately not
            // marked and therefore never enter the reusable track cache.
            let mut seen_policy_clusters = HashSet::new();
            let mut policy_safe = scored.iter().filter(|(candidate_score, candidate)| {
                policy_independent(candidate_score)
                    && seen_policy_clusters.insert(candidate_cluster_key(candidate))
            });
            if let Some((winner, candidate)) = policy_safe.next() {
                let policy_safe_margin =
                    policy_safe.next().map_or(winner.total, |(runner_up, _)| {
                        winner.total - runner_up.total
                    });
                if candidate.key == best.key
                    && winner.total >= MatchConfig::default().accept
                    && policy_safe_margin >= MatchConfig::default().accept_margin
                {
                    score.reason_codes.push("policy_safe_cache".to_owned());
                }
            }
            MatchOutcome::Matched {
                key: best.key.clone(),
                score: score.total,
                display: format!("{} — {}", best.artist, best.title),
                title: Some(best.title.clone()),
                artist: Some(best.artist.clone()),
                album: best.album.clone(),
                duration_secs: best.duration_secs,
                score_breakdown: Some(Box::new(score)),
            }
        } else {
            let candidates = review_candidates();
            if candidates.is_empty() {
                MatchOutcome::NotFound
            } else {
                MatchOutcome::Ambiguous { candidates }
            }
        }
    } else {
        let candidates = review_candidates();
        if candidates.is_empty() {
            MatchOutcome::NotFound
        } else {
            MatchOutcome::Ambiguous { candidates }
        }
    }
}

// Retrieval query helpers ---------------------------------------------------------

fn push_query_variant(out: &mut Vec<String>, seen: &mut HashSet<String>, query: String) {
    let query = query.trim();
    if query.is_empty() {
        return;
    }
    let key = normalize(query);
    if seen.insert(key) {
        out.push(query.to_owned());
    }
}

fn release_year(input: &TrackInput) -> Option<&str> {
    let date = input.album_release_date.as_deref()?.trim();
    let year = date.get(0..4)?;
    (year.bytes().all(|b| b.is_ascii_digit())).then_some(year)
}

pub fn spotify_query_plan(input: &TrackInput) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(isrc) = input
        .isrc
        .as_deref()
        .map(str::trim)
        .filter(|isrc| !isrc.is_empty())
    {
        push_query_variant(&mut out, &mut seen, format!("isrc:{isrc}"));
    }

    let title = normalize_stripped(&input.title);
    let artist = input.artists.first().cloned().unwrap_or_default();
    let album = input.album.as_deref().map(str::trim).unwrap_or_default();
    let mut fielded = format!("track:\"{}\"", spotify_query_escape(&title));
    if !artist.trim().is_empty() {
        fielded.push_str(&format!(" artist:\"{}\"", spotify_query_escape(&artist)));
    }
    if !album.is_empty() {
        fielded.push_str(&format!(" album:\"{}\"", spotify_query_escape(album)));
    }
    push_query_variant(&mut out, &mut seen, fielded);

    if let Some(year) = release_year(input) {
        let mut with_year = format!("track:\"{}\" year:{year}", spotify_query_escape(&title));
        if !artist.trim().is_empty() {
            with_year.push_str(&format!(" artist:\"{}\"", spotify_query_escape(&artist)));
        }
        push_query_variant(&mut out, &mut seen, with_year);
    }

    let plain = format!("{artist} {}", input.title);
    push_query_variant(&mut out, &mut seen, plain.trim().to_owned());
    out
}

fn spotify_query_escape(value: &str) -> String {
    value.replace('"', " ")
}

#[cfg(test)]
mod tests;
