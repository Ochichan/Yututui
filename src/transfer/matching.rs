//! The direction-agnostic track matcher.
//!
//! Normalization is CJK-safe by construction: NFKC folds fullwidth/compat forms (endemic
//! in K-pop metadata), similarity is char-level normalized Levenshtein (no ASCII
//! assumptions), and punctuation stripping never touches letters of any script. Titles
//! are compared as the max over (full, annotation-stripped) forms, so dual-script names
//! like `"TT (티티)"` match either half.
//!
//! Scoring: title 0.55 + artist 0.30 + duration 0.15, plus a 0.05 album bonus as the
//! remaster-vs-original tie-breaker. Accept ≥ 0.80; 0.60..0.80 is reported as ambiguous
//! (top 3 candidates) rather than silently guessed.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::api::Song;
use crate::api::ytmusic::{YoutubeSearchKind, YtMusicApi, YtdlpVideoMeta};
use crate::search_source::SearchConfig;
use crate::spotify::models::SpotifyTrack;
use crate::streaming::musicgate;

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
    YtmCatalogSong,
    YoutubeVideoSearch,
    SpotifyCatalog,
}

impl From<YoutubeSearchKind> for CandidateSourceKind {
    fn from(kind: YoutubeSearchKind) -> Self {
        match kind {
            YoutubeSearchKind::YtmCatalogSong => Self::YtmCatalogSong,
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
    pub source_kind: CandidateSourceKind,
    pub channel: Option<String>,
    pub isrc: Option<String>,
    preflighted: bool,
    preflight_reject_reason: Option<String>,
    preflight_reason_codes: Vec<String>,
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
            source_kind,
            channel: Some(s.artist.clone()).filter(|a| !a.trim().is_empty()),
            isrc: s.isrc.clone(),
            preflighted: false,
            preflight_reject_reason: None,
            preflight_reason_codes: Vec::new(),
        }
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
            source_kind: CandidateSourceKind::SpotifyCatalog,
            channel: None,
            isrc: t.isrc.clone(),
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AmbiguousCandidate {
    pub key: String,
    pub score: f32,
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_breakdown: Option<MatchScoreBreakdown>,
}

pub(crate) struct YtmMatchState<'a> {
    memo: &'a mut HashMap<String, MatchOutcome>,
    query_memo: &'a mut HashMap<String, Vec<(Song, YoutubeSearchKind)>>,
    video_memo: &'a mut HashMap<String, Option<YtdlpVideoMeta>>,
    pace: &'a mut Pacing,
}

impl<'a> YtmMatchState<'a> {
    pub(crate) fn new(
        memo: &'a mut HashMap<String, MatchOutcome>,
        query_memo: &'a mut HashMap<String, Vec<(Song, YoutubeSearchKind)>>,
        video_memo: &'a mut HashMap<String, Option<YtdlpVideoMeta>>,
        pace: &'a mut Pacing,
    ) -> Self {
        Self {
            memo,
            query_memo,
            video_memo,
            pace,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MatchConfig {
    pub accept: f32,
    pub ambiguous_floor: f32,
    pub accept_margin: f32,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            accept: 0.80,
            ambiguous_floor: 0.60,
            accept_margin: 0.06,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MatchScoreBreakdown {
    pub total: f32,
    pub raw_total: f32,
    #[serde(default)]
    pub source_kind: String,
    #[serde(default)]
    pub quality_tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_delta_secs: Option<i32>,
    pub title: f32,
    pub artist: f32,
    pub duration: f32,
    pub album_bonus: f32,
    #[serde(default)]
    pub quality_bonus: f32,
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

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
struct IdentityFlags {
    instrumental: bool,
    karaoke: bool,
    backing_track: bool,
    cover: bool,
    remix: bool,
    live: bool,
    acoustic: bool,
    sped_up: bool,
    slowed: bool,
    slowed_reverb: bool,
    nightcore: bool,
    eight_d: bool,
    demo: bool,
    taylor_version: bool,
    clean: bool,
    explicit: bool,
}

#[derive(Default)]
struct IdentityGate {
    hard_reject: Option<&'static str>,
    accept_blocked: bool,
    penalty: f32,
    reasons: Vec<&'static str>,
}

fn identity_flags(text: &str) -> IdentityFlags {
    let normalized = normalize(text);
    let lower = text.to_lowercase();
    let phrase = |needle: &str| normalized_contains_phrase(&normalized, needle);
    let raw = |needle: &str| lower.contains(needle);
    let live_marker = phrase("live")
        && (raw("(live")
            || raw("[live")
            || raw(" live at ")
            || raw(" live from ")
            || raw(" live in ")
            || raw(" live on ")
            || raw(" live version")
            || raw("concert")
            || raw("라이브")
            || raw("ライブ")
            || raw("现场")
            || raw("現場"));
    IdentityFlags {
        instrumental: phrase("instrumental")
            || phrase("inst")
            || phrase("off vocal")
            || phrase("no vocal")
            || raw("instrumental ver")
            || raw("반주")
            || raw("伴奏"),
        karaoke: phrase("karaoke") || raw("노래방") || raw("カラオケ"),
        backing_track: phrase("backing track")
            || phrase("accompaniment")
            || phrase("off vocal")
            || raw("伴奏")
            || raw(" mr ")
            || lower.ends_with(" mr"),
        cover: phrase("cover")
            || phrase("covered by")
            || phrase("cover by")
            || raw("歌ってみた")
            || raw("커버"),
        remix: phrase("remix") || raw(" 리믹스") || raw("リミックス"),
        live: live_marker,
        acoustic: phrase("acoustic") || raw("어쿠스틱") || raw("アコースティック"),
        sped_up: phrase("sped up") || phrase("spedup") || phrase("speed up"),
        slowed: phrase("slowed") || raw("느리게"),
        slowed_reverb: phrase("slowed reverb") || raw("slowed + reverb") || raw("slowed+reverb"),
        nightcore: phrase("nightcore"),
        eight_d: phrase("8d") || phrase("8d audio") || phrase("8d version"),
        demo: phrase("demo"),
        taylor_version: raw("taylor's version") || raw("taylors version"),
        clean: phrase("clean"),
        explicit: phrase("explicit"),
    }
}

fn normalized_contains_phrase(normalized: &str, phrase: &str) -> bool {
    let needle = normalize(phrase);
    if needle.is_empty() {
        return false;
    }
    let hay = format!(" {normalized} ");
    hay.contains(&format!(" {needle} "))
}

fn identity_gate(input: &TrackInput, cand: &MatchCandidate) -> IdentityGate {
    let source_text = format!(
        "{} {} {}",
        input.title,
        input.artists.join(" "),
        input.album.as_deref().unwrap_or_default()
    );
    let candidate_text = format!(
        "{} {} {}",
        cand.title,
        cand.artist,
        cand.channel.as_deref().unwrap_or_default()
    );
    let src = identity_flags(&source_text);
    let dst = identity_flags(&candidate_text);
    let mut gate = IdentityGate::default();

    if !src.karaoke && dst.karaoke {
        gate.hard_reject = Some("karaoke_mismatch");
        gate.reasons.push("karaoke_mismatch");
    }
    if !src.backing_track && dst.backing_track {
        gate.hard_reject = Some("backing_track_mismatch");
        gate.reasons.push("backing_track_mismatch");
    }
    if !src.sped_up && dst.sped_up {
        gate.hard_reject = Some("sped_up_mismatch");
        gate.reasons.push("sped_up_mismatch");
    }
    if !src.nightcore && dst.nightcore {
        gate.hard_reject = Some("nightcore_mismatch");
        gate.reasons.push("nightcore_mismatch");
    }
    if !src.eight_d && dst.eight_d {
        gate.hard_reject = Some("8d_mismatch");
        gate.reasons.push("8d_mismatch");
    }
    if !src.slowed_reverb && dst.slowed_reverb {
        gate.hard_reject = Some("slowed_reverb_mismatch");
        gate.reasons.push("slowed_reverb_mismatch");
    }

    let mut block = |condition: bool, reason: &'static str, penalty: f32| {
        if condition {
            gate.accept_blocked = true;
            gate.penalty += penalty;
            gate.reasons.push(reason);
        }
    };
    block(
        !src.instrumental && dst.instrumental,
        "instrumental_mismatch",
        0.35,
    );
    block(
        src.instrumental && !dst.instrumental,
        "missing_instrumental_marker",
        0.25,
    );
    block(!src.cover && dst.cover, "cover_mismatch", 0.25);
    block(!src.live && dst.live, "live_mismatch", 0.20);
    block(!src.remix && dst.remix, "remix_mismatch", 0.25);
    block(!src.acoustic && dst.acoustic, "acoustic_mismatch", 0.18);
    block(!src.demo && dst.demo, "demo_mismatch", 0.18);
    block(!src.slowed && dst.slowed, "slowed_mismatch", 0.20);
    block(
        src.taylor_version && !dst.taylor_version,
        "missing_taylor_version_marker",
        0.20,
    );
    block(
        !src.taylor_version && dst.taylor_version,
        "taylor_version_mismatch",
        0.20,
    );
    block(src.clean && dst.explicit, "explicit_mismatch", 0.10);
    block(src.explicit && dst.clean, "clean_mismatch", 0.10);
    block(
        input.explicit == Some(false) && dst.explicit,
        "explicit_mismatch",
        0.10,
    );
    block(
        input.explicit == Some(true) && dst.clean,
        "clean_mismatch",
        0.10,
    );

    gate
}

fn source_kind_code(kind: CandidateSourceKind) -> &'static str {
    match kind {
        CandidateSourceKind::Unknown => "unknown",
        CandidateSourceKind::YtmCatalogSong => "ytm_catalog_song",
        CandidateSourceKind::YoutubeVideoSearch => "youtube_video_search",
        CandidateSourceKind::SpotifyCatalog => "spotify_catalog",
    }
}

fn official_signal_score(cand: &MatchCandidate) -> f32 {
    if matches!(
        cand.source_kind,
        CandidateSourceKind::YtmCatalogSong | CandidateSourceKind::SpotifyCatalog
    ) {
        return 1.0;
    }
    let channel = cand.channel.as_deref().unwrap_or(&cand.artist);
    if musicgate::is_trusted_music_channel(channel) {
        return 1.0;
    }
    let title_tier = musicgate::music_tier_score(&cand.title, channel);
    if title_tier >= 0.7 {
        return title_tier;
    }
    let channel_lower = channel.to_lowercase();
    if channel_lower.contains("official") || channel_lower.ends_with(" records") {
        return 0.7;
    }
    title_tier
}

fn quality_tier(cand: &MatchCandidate) -> &'static str {
    match cand.source_kind {
        CandidateSourceKind::YtmCatalogSong | CandidateSourceKind::SpotifyCatalog => "catalog",
        CandidateSourceKind::YoutubeVideoSearch | CandidateSourceKind::Unknown => {
            let official = official_signal_score(cand);
            if official >= 1.0 {
                "trusted_official"
            } else if official >= 0.7 {
                "official_like"
            } else if official >= 0.4 {
                "music_signal"
            } else {
                "unverified_upload"
            }
        }
    }
}

fn duration_delta_secs(input: &TrackInput, cand: &MatchCandidate) -> Option<i32> {
    let input = i32::try_from(input.duration_secs?).ok()?;
    let cand = i32::try_from(cand.duration_secs?).ok()?;
    Some(cand - input)
}

fn quality_adjustment(cand: &MatchCandidate) -> (f32, f32, Vec<&'static str>) {
    let mut bonus = 0.0f32;
    let mut penalty = 0.0f32;
    let mut reasons = Vec::new();
    let channel = cand.channel.as_deref().unwrap_or(&cand.artist);

    match cand.source_kind {
        CandidateSourceKind::YtmCatalogSong | CandidateSourceKind::SpotifyCatalog => {
            bonus += 0.07;
            reasons.push("catalog_song");
        }
        CandidateSourceKind::YoutubeVideoSearch | CandidateSourceKind::Unknown => {}
    }

    let tier = musicgate::music_tier_score(&cand.title, channel);
    if tier >= 1.0 {
        bonus += 0.05;
        reasons.push("trusted_channel");
    } else if tier >= 0.7 {
        bonus += 0.03;
        reasons.push("official_like");
    } else if tier >= 0.4 {
        bonus += 0.01;
        reasons.push("music_video_signal");
    }

    let risk = musicgate::non_music_risk_score(&cand.title, channel);
    if risk >= 0.85 {
        penalty += 0.35;
        reasons.push("non_music_risk");
    } else if risk >= 0.55 {
        penalty += 0.15;
        reasons.push("non_music_demote");
    }
    if let Some(reason) = musicgate::gimmick_reason(&cand.title) {
        penalty += 0.35;
        reasons.push(reason);
    }

    (bonus, penalty, reasons)
}

/// Score one candidate against the input.
pub fn score_candidate(input: &TrackInput, cand: &MatchCandidate) -> f32 {
    score_candidate_breakdown(input, cand).total
}

/// Explain one candidate score against the input.
pub fn score_candidate_breakdown(input: &TrackInput, cand: &MatchCandidate) -> MatchScoreBreakdown {
    let input_stripped = normalize_stripped(&input.title);
    // Title: best of full vs annotation-stripped comparisons. Video results often embed
    // the artist in the title ("IU 'Celebrity' MV") — a third form with the input's
    // artists removed from the candidate title covers that. And dual-script video
    // titles ("숀 - 웨이백홈 (Way Back Home) [Lyric Video]") are handled by a
    // containment path against the gloss-KEPT cleaned form: a clean input title fully
    // contained in the candidate scores by how much of the candidate it explains.
    let title = {
        let full = similarity(&normalize(&input.title), &normalize(&cand.title));
        let cand_stripped = normalize_stripped(&cand.title);
        let stripped = similarity(&input_stripped, &cand_stripped);
        let mut best = full.max(stripped);
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
        let cand_clean = strip_video_noise(&normalize(&strip_annotations(&cand.title)));
        best = best
            .max(contain(&input_stripped, &cand_clean))
            .max(contain(&cand_stripped, &input_stripped));
        best
    };

    // Artist: candidate side is one display string (possibly several names joined, or a
    // YouTube channel — "<Artist> - Topic" is the auto-generated catalog channel);
    // containment either way is a full score, else edit similarity; max over inputs.
    let cand_artist = normalize(&cand.artist);
    let cand_artist = cand_artist
        .strip_suffix(" topic")
        .unwrap_or(&cand_artist)
        .to_owned();
    let artist = input
        .artists
        .iter()
        .map(|a| {
            let a = normalize(a);
            if containment(&cand_artist, &a) {
                1.0
            } else {
                similarity(&a, &cand_artist)
            }
        })
        .fold(0.0f32, f32::max);

    // Duration proximity; neutral when either side is unknown.
    let duration = match (input.duration_secs, cand.duration_secs) {
        (Some(a), Some(b)) => {
            let delta = a.abs_diff(b);
            if delta <= 3 {
                1.0
            } else if delta <= 10 {
                0.5
            } else {
                0.0
            }
        }
        _ => 0.5,
    };

    let album_bonus = match (&input.album, &cand.album) {
        (Some(a), Some(b)) => {
            let (a, b) = (normalize_stripped(a), normalize_stripped(b));
            if !a.is_empty() && (a == b || containment(&a, &b)) {
                0.05
            } else {
                0.0
            }
        }
        _ => 0.0,
    };

    // Deliberately starts unclamped (max 1.05): clamping would erase the album bonus exactly
    // where it matters — as the tie-breaker between two otherwise-perfect candidates.
    let raw_total = 0.55 * title + 0.30 * artist + 0.15 * duration + album_bonus;
    let mut gate = identity_gate(input, cand);
    let delta_secs = duration_delta_secs(input, cand);
    if delta_secs.is_some_and(|delta| delta.unsigned_abs() > 10) {
        gate.accept_blocked = true;
        gate.reasons.push("duration_mismatch");
    }
    if input.duration_secs.is_none()
        && cand
            .duration_secs
            .is_some_and(|duration| duration > 15 * 60)
    {
        gate.accept_blocked = true;
        gate.reasons.push("long_mix_duration");
    }
    if matches!(
        cand.source_kind,
        CandidateSourceKind::YoutubeVideoSearch | CandidateSourceKind::Unknown
    ) && official_signal_score(cand) < 0.7
    {
        gate.accept_blocked = true;
        gate.reasons.push("unverified_youtube_upload");
    }
    let (quality_bonus, non_music_penalty, quality_reasons) = quality_adjustment(cand);
    let identity_penalty = gate.penalty.min(0.75);
    let mut total = (raw_total + quality_bonus - non_music_penalty - identity_penalty).max(0.0);
    let mut reject_reason = gate.hard_reject.map(str::to_owned);
    if reject_reason.is_none() {
        reject_reason = cand.preflight_reject_reason.clone();
    }
    let channel = cand.channel.as_deref().unwrap_or(&cand.artist);
    if reject_reason.is_none()
        && let Some(reason) = musicgate::non_music_reason(&cand.title, channel)
    {
        reject_reason = Some(reason.to_owned());
    }
    if reject_reason.is_some() {
        total = 0.0;
    }
    let mut reason_codes: Vec<String> = gate.reasons.into_iter().map(str::to_owned).collect();
    reason_codes.extend(quality_reasons.into_iter().map(str::to_owned));
    reason_codes.extend(cand.preflight_reason_codes.iter().cloned());
    MatchScoreBreakdown {
        total,
        raw_total,
        source_kind: source_kind_code(cand.source_kind).to_owned(),
        quality_tier: quality_tier(cand).to_owned(),
        duration_delta_secs: delta_secs,
        title,
        artist,
        duration,
        album_bonus,
        quality_bonus,
        identity_penalty,
        non_music_penalty,
        accept_blocked: gate.accept_blocked,
        reject_reason,
        reason_codes,
    }
}

/// Rank candidates and classify against the thresholds.
pub fn best_outcome(
    input: &TrackInput,
    candidates: &[MatchCandidate],
    cfg: &MatchConfig,
) -> MatchOutcome {
    let mut scored: Vec<(MatchScoreBreakdown, &MatchCandidate)> = candidates
        .iter()
        .map(|c| (score_candidate_breakdown(input, c), c))
        .collect();
    scored.sort_by(|a, b| {
        b.0.total
            .partial_cmp(&a.0.total)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let review_candidates = || {
        scored
            .iter()
            .filter(|(s, _)| s.reject_reason.is_none() && s.total >= cfg.ambiguous_floor)
            .take(3)
            .map(|(score, c)| AmbiguousCandidate {
                key: c.key.clone(),
                score: score.total,
                display: format!("{} — {}", c.artist, c.title),
                score_breakdown: Some(score.clone()),
            })
            .collect::<Vec<_>>()
    };
    match scored.first() {
        Some((score, best))
            if score.reject_reason.is_none()
                && !score.accept_blocked
                && score.total >= cfg.accept
                && scored
                    .iter()
                    .filter(|(s, _)| s.reject_reason.is_none())
                    .nth(1)
                    .is_none_or(|(second, _)| score.total - second.total >= cfg.accept_margin) =>
        {
            MatchOutcome::Matched {
                key: best.key.clone(),
                score: score.total,
                display: format!("{} — {}", best.artist, best.title),
                title: Some(best.title.clone()),
                artist: Some(best.artist.clone()),
                album: best.album.clone(),
                duration_secs: best.duration_secs,
                score_breakdown: Some(Box::new(score.clone())),
            }
        }
        Some((score, _)) if score.total >= cfg.ambiguous_floor => {
            let candidates = review_candidates();
            if candidates.is_empty() {
                MatchOutcome::NotFound
            } else {
                MatchOutcome::Ambiguous { candidates }
            }
        }
        _ => MatchOutcome::NotFound,
    }
}

// Retrieval (YTM direction) ---------------------------------------------------------

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

/// Find `input` on YouTube Music using a bounded query plan. The first query keeps the
/// old fast path (`"artist stripped-title"`); later queries add all-artist, album, and
/// title-only variants only while the best score remains below the accept threshold.
pub(crate) async fn match_track_ytm(
    api: &YtMusicApi,
    input: &TrackInput,
    cfg: &MatchConfig,
    search_config: &SearchConfig,
    state: &mut YtmMatchState<'_>,
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
    if let Some(hit) = state.memo.get(&key) {
        return Ok(hit.clone());
    }

    let mut candidates = Vec::<MatchCandidate>::new();
    let mut outcome = MatchOutcome::NotFound;
    for query in ytm_query_plan(input) {
        let songs = if let Some(hit) = state.query_memo.get(&query) {
            hit.clone()
        } else {
            state.pace.tick().await;
            let songs = api.search_transfer_youtube(&query, search_config).await?;
            state.query_memo.insert(query.clone(), songs.clone());
            songs
        };
        for (song, kind) in &songs {
            if !candidates.iter().any(|c| c.key == song.video_id) {
                candidates.push(MatchCandidate::from_song_with_kind(song, (*kind).into()));
            }
        }
        preflight_ytm_candidates(api, input, cfg, &mut candidates, state.video_memo).await;
        outcome = best_outcome(input, &candidates, cfg);
        if should_stop_ytm_search(&outcome) {
            break;
        }
    }

    state.memo.insert(key, outcome.clone());
    Ok(outcome)
}

const TRANSFER_PREFLIGHT_TOP_N: usize = 5;

async fn preflight_ytm_candidates(
    api: &YtMusicApi,
    input: &TrackInput,
    cfg: &MatchConfig,
    candidates: &mut [MatchCandidate],
    video_memo: &mut HashMap<String, Option<YtdlpVideoMeta>>,
) {
    let mut ranked: Vec<(f32, usize)> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| needs_transfer_preflight(candidate))
        .map(|(idx, candidate)| (score_candidate_breakdown(input, candidate).total, idx))
        .filter(|(score, _)| *score >= cfg.ambiguous_floor)
        .collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for (_, idx) in ranked.into_iter().take(TRANSFER_PREFLIGHT_TOP_N) {
        let key = candidates[idx].key.clone();
        let meta = match video_memo.get(&key) {
            Some(hit) => hit.clone(),
            None => {
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
                video_memo.insert(key.clone(), hit.clone());
                hit
            }
        };
        candidates[idx].preflighted = true;
        if let Some(meta) = meta {
            apply_transfer_preflight(input, &mut candidates[idx], &meta);
        }
    }
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

    let mut reject = |reason: &str| {
        if candidate.preflight_reject_reason.is_none() {
            candidate.preflight_reject_reason = Some(reason.to_owned());
        }
        if !candidate
            .preflight_reason_codes
            .iter()
            .any(|code| code == reason)
        {
            candidate.preflight_reason_codes.push(reason.to_owned());
        }
    };

    if meta.is_live == Some(true)
        || matches!(
            meta.live_status.as_deref(),
            Some("is_live" | "is_upcoming" | "post_live")
        )
    {
        reject("live_or_upcoming");
    }
    if matches!(meta.media_type.as_deref(), Some("playlist" | "multi_video")) {
        reject("non_track_media");
    }
    if let Some(duration) = meta.duration_secs {
        if let Some(source_duration) = input.duration_secs {
            if source_duration.abs_diff(duration) > 10 {
                reject("duration_mismatch");
            }
        } else if duration > 15 * 60 {
            reject("long_mix_duration");
        }
    }
    let channel = meta.channel.as_str();
    let rich_title = match meta.description.as_deref() {
        Some(desc) if !desc.trim().is_empty() => format!("{} {}", meta.title, desc),
        _ => meta.title.clone(),
    };
    if let Some(reason) = musicgate::non_music_reason(&rich_title, channel) {
        reject(reason);
    }
}

fn should_stop_ytm_search(outcome: &MatchOutcome) -> bool {
    match outcome {
        MatchOutcome::Matched {
            score_breakdown: Some(breakdown),
            score: total,
            ..
        } => {
            let quality_source = score_breakdown_has(breakdown, "catalog_song")
                || score_breakdown_has(breakdown, "trusted_channel")
                || score_breakdown_has(breakdown, "official_like");
            *total >= 0.90 && quality_source
        }
        MatchOutcome::Matched { .. } => true,
        _ => false,
    }
}

fn score_breakdown_has(score: &MatchScoreBreakdown, reason: &str) -> bool {
    score.reason_codes.iter().any(|r| r == reason)
}

#[cfg(test)]
mod tests;
