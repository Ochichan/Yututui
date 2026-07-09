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
use crate::api::ytmusic::{YoutubeSearchKind, YtMusicApi};
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
        score_breakdown: Option<MatchScoreBreakdown>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchScoreBreakdown {
    pub total: f32,
    pub raw_total: f32,
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

    gate
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
    let gate = identity_gate(input, cand);
    let (quality_bonus, non_music_penalty, quality_reasons) = quality_adjustment(cand);
    let identity_penalty = gate.penalty.min(0.75);
    let mut total = (raw_total + quality_bonus - non_music_penalty - identity_penalty).max(0.0);
    if gate.hard_reject.is_some() {
        total = 0.0;
    }
    let mut reason_codes: Vec<String> = gate.reasons.into_iter().map(str::to_owned).collect();
    reason_codes.extend(quality_reasons.into_iter().map(str::to_owned));
    MatchScoreBreakdown {
        total,
        raw_total,
        title,
        artist,
        duration,
        album_bonus,
        quality_bonus,
        identity_penalty,
        non_music_penalty,
        accept_blocked: gate.accept_blocked,
        reject_reason: gate.hard_reject.map(str::to_owned),
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
                score_breakdown: Some(score.clone()),
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

    push_query_variant(&mut out, &mut seen, stripped_title.to_owned());
    out
}

/// Find `input` on YouTube Music using a bounded query plan. The first query keeps the
/// old fast path (`"artist stripped-title"`); later queries add all-artist, album, and
/// title-only variants only while the best score remains below the accept threshold.
pub async fn match_track_ytm(
    api: &YtMusicApi,
    input: &TrackInput,
    cfg: &MatchConfig,
    search_config: &SearchConfig,
    memo: &mut HashMap<String, MatchOutcome>,
    pace: &mut Pacing,
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
    if let Some(hit) = memo.get(&key) {
        return Ok(hit.clone());
    }

    let mut candidates = Vec::<MatchCandidate>::new();
    let mut outcome = MatchOutcome::NotFound;
    for query in ytm_query_plan(input) {
        pace.tick().await;
        let songs = api.search_transfer_youtube(&query, search_config).await?;
        for (song, kind) in &songs {
            if !candidates.iter().any(|c| c.key == song.video_id) {
                candidates.push(MatchCandidate::from_song_with_kind(song, (*kind).into()));
            }
        }
        outcome = best_outcome(input, &candidates, cfg);
        if should_stop_ytm_search(&outcome) {
            break;
        }
    }

    memo.insert(key, outcome.clone());
    Ok(outcome)
}

fn should_stop_ytm_search(outcome: &MatchOutcome) -> bool {
    match outcome {
        MatchOutcome::Matched {
            score_breakdown: Some(breakdown),
            score: total,
            ..
        } => {
            let high_confidence = *total >= 0.95;
            let quality_source = score_breakdown_has(breakdown, "catalog_song")
                || score_breakdown_has(breakdown, "trusted_channel")
                || score_breakdown_has(breakdown, "official_like");
            high_confidence || quality_source
        }
        MatchOutcome::Matched { .. } => true,
        _ => false,
    }
}

fn score_breakdown_has(score: &MatchScoreBreakdown, reason: &str) -> bool {
    score.reason_codes.iter().any(|r| r == reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(title: &str, artists: &[&str], album: Option<&str>, dur: Option<u32>) -> TrackInput {
        TrackInput {
            title: title.to_owned(),
            artists: artists.iter().map(|s| (*s).to_owned()).collect(),
            album_artists: Vec::new(),
            album: album.map(str::to_owned),
            album_id: None,
            album_uri: None,
            album_release_date: None,
            album_release_date_precision: None,
            album_total_tracks: None,
            album_type: None,
            album_art_url: None,
            disc_number: None,
            track_number: None,
            duration_secs: dur,
            isrc: None,
            explicit: None,
            source_url: None,
            source_key: "src".to_owned(),
            known_video_id: None,
        }
    }

    fn cand(title: &str, artist: &str, album: Option<&str>, dur: Option<u32>) -> MatchCandidate {
        MatchCandidate {
            key: format!("key-{title}"),
            title: title.to_owned(),
            artist: artist.to_owned(),
            album: album.map(str::to_owned),
            duration_secs: dur,
            source_kind: CandidateSourceKind::Unknown,
            channel: Some(artist.to_owned()),
            isrc: None,
        }
    }

    #[test]
    fn normalize_goldens() {
        assert_eq!(normalize("ＴＴ"), "tt"); // NFKC fullwidth fold
        assert_eq!(normalize("Don’t Stop"), "don t stop");
        assert_eq!(normalize("R&B Mix"), "r and b mix");
        assert_eq!(normalize("사건의 지평선"), "사건의 지평선"); // CJK untouched
        assert_eq!(
            normalize_stripped("Song Title (feat. Someone)"),
            "song title"
        );
        assert_eq!(normalize_stripped("Track - 2011 Remaster"), "track");
        assert_eq!(
            normalize_stripped("Album Cut [Deluxe Edition]"),
            "album cut"
        );
        // Identity-changing markers survive.
        assert_eq!(normalize_stripped("Song (Live)"), "song live");
        assert_eq!(
            normalize_stripped("Love Story (Taylor's Version)"),
            "love story taylor s version"
        );
    }

    #[test]
    fn spotify_input_preserves_library_metadata() {
        let track = SpotifyTrack {
            id: Some("sp-track".to_owned()),
            uri: "spotify:track:sp-track".to_owned(),
            spotify_url: Some("https://open.spotify.com/track/sp-track".to_owned()),
            name: "Song".to_owned(),
            artists: vec!["Artist".to_owned()],
            artist_ids: vec!["sp-artist".to_owned()],
            album_artists: vec!["Album Artist".to_owned()],
            album_artist_ids: vec!["sp-album-artist".to_owned()],
            album: "Album".to_owned(),
            album_id: Some("sp-album".to_owned()),
            album_uri: Some("spotify:album:sp-album".to_owned()),
            album_url: Some("https://open.spotify.com/album/sp-album".to_owned()),
            album_type: Some("album".to_owned()),
            album_total_tracks: Some(10),
            album_release_date: Some("2026-07-01".to_owned()),
            album_release_date_precision: Some("day".to_owned()),
            album_images: vec![crate::spotify::models::SpotifyImage {
                url: "https://i.scdn.co/image/cover640".to_owned(),
                width: Some(640),
                height: Some(640),
            }],
            duration_ms: 123_456,
            disc_number: Some(1),
            track_number: Some(4),
            isrc: Some("ISRC1".to_owned()),
            explicit: true,
            added_at: Some("2026-07-02T00:00:00Z".to_owned()),
            is_playable: Some(true),
            restriction_reason: None,
        };

        let input = TrackInput::from_spotify(&track);

        assert_eq!(input.title, "Song");
        assert_eq!(input.artists, vec!["Artist".to_owned()]);
        assert_eq!(input.album_artists, vec!["Album Artist".to_owned()]);
        assert_eq!(input.album.as_deref(), Some("Album"));
        assert_eq!(input.album_id.as_deref(), Some("sp-album"));
        assert_eq!(input.album_uri.as_deref(), Some("spotify:album:sp-album"));
        assert_eq!(input.album_release_date.as_deref(), Some("2026-07-01"));
        assert_eq!(input.album_release_date_precision.as_deref(), Some("day"));
        assert_eq!(input.album_total_tracks, Some(10));
        assert_eq!(input.album_type.as_deref(), Some("album"));
        assert_eq!(
            input.album_art_url.as_deref(),
            Some("https://i.scdn.co/image/cover640")
        );
        assert_eq!(input.disc_number, Some(1));
        assert_eq!(input.track_number, Some(4));
        assert_eq!(input.duration_secs, Some(123));
        assert_eq!(input.isrc.as_deref(), Some("ISRC1"));
        assert_eq!(input.explicit, Some(true));
        assert_eq!(
            input.source_url.as_deref(),
            Some("https://open.spotify.com/track/sp-track")
        );
        assert_eq!(input.source_key, "spotify:track:sp-track");
    }

    #[test]
    fn song_input_preserves_catalog_metadata() {
        let song = Song::from_search(
            "dQw4w9WgXcQ",
            "Song",
            "Artist",
            "3:03",
            Some("Album".to_owned()),
        )
        .with_catalog_metadata(
            Some("Album Artist".to_owned()),
            Some(1),
            Some(4),
            Some("ISRC123".to_owned()),
            Some("spotify:track:abc".to_owned()),
            Some("https://open.spotify.com/track/abc".to_owned()),
        )
        .with_import_metadata(crate::api::SongImportMetadata {
            artists: vec!["Artist".to_owned(), "Guest".to_owned()],
            album_artists: vec!["Album Artist".to_owned(), "Label Ensemble".to_owned()],
            album_art_url: Some("https://i.scdn.co/image/song-cover".to_owned()),
            ..Default::default()
        });

        let input = TrackInput::from_song(&song);

        assert_eq!(input.title, "Song");
        assert_eq!(input.artists, vec!["Artist".to_owned(), "Guest".to_owned()]);
        assert_eq!(
            input.album_artists,
            vec!["Album Artist".to_owned(), "Label Ensemble".to_owned()]
        );
        assert_eq!(input.album.as_deref(), Some("Album"));
        assert_eq!(
            input.album_art_url.as_deref(),
            Some("https://i.scdn.co/image/song-cover")
        );
        assert_eq!(input.disc_number, Some(1));
        assert_eq!(input.track_number, Some(4));
        assert_eq!(input.duration_secs, Some(183));
        assert_eq!(input.isrc.as_deref(), Some("ISRC123"));
        assert_eq!(input.source_key, "spotify:track:abc");
        assert_eq!(
            input.source_url.as_deref(),
            Some("https://open.spotify.com/track/abc")
        );
        assert_eq!(input.known_video_id.as_deref(), Some("dQw4w9WgXcQ"));
    }

    #[test]
    fn ytm_query_plan_adds_all_artists_album_and_title_fallbacks() {
        let input = input(
            "Song Title (feat. Guest)",
            &["Primary", "Featured"],
            Some("Album Name"),
            Some(180),
        );

        assert_eq!(
            ytm_query_plan(&input),
            vec![
                "Primary Song Title".to_owned(),
                "Primary Featured Song Title".to_owned(),
                "Primary Song Title Album Name".to_owned(),
                "Song Title Album Name".to_owned(),
                "Primary Song Title official audio".to_owned(),
                "Primary Song Title topic".to_owned(),
                "Song Title".to_owned(),
            ]
        );
    }

    #[test]
    fn ytm_query_plan_dedupes_empty_and_repeated_variants() {
        let mut input = input("Song", &["Artist", "Artist"], Some("Song"), None);
        input.artists.push(" ".to_owned());

        assert_eq!(
            ytm_query_plan(&input),
            vec![
                "Artist Song".to_owned(),
                "Artist Artist Song".to_owned(),
                "Artist Song official audio".to_owned(),
                "Artist Song topic".to_owned(),
                "Song".to_owned(),
            ]
        );
    }

    #[test]
    fn ytm_query_plan_handles_missing_artists() {
        let input = input("Song", &[], Some("Album"), None);

        assert_eq!(
            ytm_query_plan(&input),
            vec!["Song Album".to_owned(), "Song".to_owned()]
        );
    }

    #[test]
    fn plain_track_does_not_auto_accept_instrumental_candidate() {
        let i = input("Song", &["Artist"], None, Some(180));
        let c = cand("Song (Instrumental)", "Artist", None, Some(180));
        let cfg = MatchConfig::default();

        let out = best_outcome(&i, &[c], &cfg);

        assert!(!matches!(out, MatchOutcome::Matched { .. }));
        let breakdown =
            score_candidate_breakdown(&i, &cand("Song (Instrumental)", "Artist", None, Some(180)));
        assert!(breakdown.accept_blocked);
        assert!(
            breakdown
                .reason_codes
                .contains(&"instrumental_mismatch".to_owned())
        );
    }

    #[test]
    fn karaoke_candidate_is_hard_rejected_for_plain_track() {
        let i = input("Song", &["Artist"], None, Some(180));
        let c = cand("Song (Karaoke Version)", "Artist", None, Some(180));

        assert!(matches!(
            best_outcome(&i, &[c], &MatchConfig::default()),
            MatchOutcome::NotFound
        ));
    }

    #[test]
    fn instrumental_source_can_match_instrumental_candidate() {
        let i = input("Song (Instrumental)", &["Artist"], None, Some(180));
        let c = cand("Song - Instrumental", "Artist", None, Some(180));

        assert!(matches!(
            best_outcome(&i, &[c], &MatchConfig::default()),
            MatchOutcome::Matched { .. }
        ));
    }

    #[test]
    fn top_gap_sends_close_candidates_to_review() {
        let i = input("Song", &["Artist"], None, Some(180));
        let a = cand("Song", "Artist", None, Some(180));
        let b = cand("Song", "Artist", Some("Other Album"), Some(180));
        let cfg = MatchConfig::default();

        assert!(matches!(
            best_outcome(&i, &[a, b], &cfg),
            MatchOutcome::Ambiguous { .. }
        ));
    }

    #[test]
    fn ytm_catalog_candidate_beats_plain_video_when_close() {
        let i = input("Song", &["Artist"], Some("Album"), Some(180));
        let mut catalog = cand("Song", "Artist", Some("Album"), Some(180));
        catalog.key = "catalog".to_owned();
        catalog.source_kind = CandidateSourceKind::YtmCatalogSong;
        let mut video = cand("Song", "Artist", Some("Album"), Some(180));
        video.key = "video".to_owned();

        match best_outcome(&i, &[video, catalog], &MatchConfig::default()) {
            MatchOutcome::Matched { key, .. } => assert_eq!(key, "catalog"),
            other => panic!("expected matched catalog candidate, got {other:?}"),
        }
    }

    #[test]
    fn dual_script_title_matches_either_form() {
        // "TT (티티)" vs "TT": the feat-stripper doesn't touch it (not noise), but the
        // full-form comparison still lands via similarity of normalized strings.
        let a = input("TT", &["TWICE"], None, Some(212));
        let c = cand("TT (티티)", "TWICE", None, Some(212));
        assert!(
            score_candidate(&a, &c) >= 0.80,
            "{}",
            score_candidate(&a, &c)
        );
    }

    #[test]
    fn exact_match_scores_high_and_wrong_artist_low() {
        let i = input("ETA", &["NewJeans"], Some("Get Up"), Some(151));
        let exact = cand("ETA", "NewJeans", Some("Get Up"), Some(151));
        assert!(score_candidate(&i, &exact) >= 0.95);

        let cover = cand("ETA", "Random Cover Band", None, Some(151));
        assert!(score_candidate(&i, &cover) < 0.80);
    }

    #[test]
    fn score_breakdown_exposes_weighted_components() {
        let i = input("ETA", &["NewJeans"], Some("Get Up"), Some(151));
        let exact = cand("ETA", "NewJeans", Some("Get Up"), Some(151));

        let breakdown = score_candidate_breakdown(&i, &exact);

        assert_eq!(breakdown.title, 1.0);
        assert_eq!(breakdown.artist, 1.0);
        assert_eq!(breakdown.duration, 1.0);
        assert_eq!(breakdown.album_bonus, 0.05);
        assert_eq!(breakdown.total, score_candidate(&i, &exact));
    }

    #[test]
    fn duration_delta_penalizes() {
        let i = input("Song", &["Artist"], None, Some(200));
        let close = cand("Song", "Artist", None, Some(202));
        let far = cand("Song", "Artist", None, Some(220));
        assert!(score_candidate(&i, &close) > score_candidate(&i, &far));
        assert!(score_candidate(&i, &far) < 0.90);
    }

    #[test]
    fn album_bonus_breaks_remaster_tie() {
        let i = input("Track", &["Artist"], Some("Original Album"), Some(200));
        let original = cand("Track", "Artist", Some("Original Album"), Some(200));
        let remaster = cand("Track", "Artist", Some("Greatest Hits"), Some(200));
        assert!(score_candidate(&i, &original) > score_candidate(&i, &remaster));
    }

    #[test]
    fn cjk_title_similarity_works() {
        let i = input("사건의 지평선", &["윤하"], None, Some(300));
        let exact = cand("사건의 지평선", "윤하 (YOUNHA)", None, Some(301));
        assert!(
            score_candidate(&i, &exact) >= 0.80,
            "containment on dual-script artist: {}",
            score_candidate(&i, &exact)
        );
    }

    #[test]
    fn multi_artist_containment() {
        let i = input("Duet", &["IU", "Someone Else"], None, None);
        let c = cand("Duet", "IU & Someone Else", None, None);
        assert!(score_candidate(&i, &c) >= 0.75);
    }

    #[test]
    fn classification_bands() {
        let cfg = MatchConfig::default();
        let i = input("ETA", &["NewJeans"], None, Some(151));
        // Accept.
        let out = best_outcome(&i, &[cand("ETA", "NewJeans", None, Some(151))], &cfg);
        match out {
            MatchOutcome::Matched {
                score,
                score_breakdown: Some(score_breakdown),
                ..
            } => assert_eq!(score_breakdown.total, score),
            other => panic!("got {other:?}"),
        }
        // Ambiguous band: same title, artist edit-distance-ish, duration off.
        let out = best_outcome(&i, &[cand("ETA", "NewJeanz Tribute", None, None)], &cfg);
        match out {
            MatchOutcome::Ambiguous { candidates } => {
                assert_eq!(
                    candidates[0].score_breakdown.as_ref().unwrap().total,
                    candidates[0].score
                );
            }
            other => panic!("got {other:?}"),
        }
        // Nothing close.
        let out = best_outcome(&i, &[cand("Different Song", "Other", None, Some(90))], &cfg);
        assert!(matches!(out, MatchOutcome::NotFound));
        // Empty candidate set.
        assert!(matches!(
            best_outcome(&i, &[], &cfg),
            MatchOutcome::NotFound
        ));
    }

    #[test]
    fn memo_key_folds_case_and_annotations() {
        let a = input("Song (feat. X)", &["Artist"], None, None);
        let b = input("SONG (FEAT. X)", &["artist"], None, None);
        assert_eq!(memo_key(&a), memo_key(&b));
    }

    /// Degraded (yt-dlp video) results: MV-decorated titles, artist-in-title, and
    /// channel names must still land above the accept threshold.
    #[test]
    fn video_result_shapes_still_match() {
        // "IU 'Celebrity' M/V" on the official channel, duration off by MV extras.
        let i = input("Celebrity", &["IU"], None, Some(195));
        let mv = cand(
            "IU 'Celebrity' M/V",
            "이지금 [IU Official]",
            None,
            Some(215),
        );
        assert!(
            score_candidate(&i, &mv) >= 0.80,
            "MV shape: {}",
            score_candidate(&i, &mv)
        );

        // Topic channel = catalog audio: artist is "<Artist> - Topic".
        let i = input("헤어진 후에", &["Y2K"], None, Some(272));
        let topic = cand("헤어진 후에", "Y2K - Topic", None, Some(273));
        assert!(
            score_candidate(&i, &topic) >= 0.90,
            "topic shape: {}",
            score_candidate(&i, &topic)
        );

        // Lyric-video decoration.
        let i = input("Way Back Home", &["SHAUN"], None, Some(217));
        let lyric = cand(
            "숀 (SHAUN) - 웨이백홈 (Way Back Home) [Lyric Video]",
            "Official dingo",
            None,
            Some(218),
        );
        assert!(
            score_candidate(&i, &lyric) >= 0.60,
            "lyric-video shape at least ambiguous: {}",
            score_candidate(&i, &lyric)
        );

        // Noise phrases never eat real titles: "Video Games" stays itself.
        assert_eq!(normalize_stripped("Video Games"), "video games");
        assert_eq!(normalize_stripped("Celebrity (Official MV)"), "celebrity");
        assert_eq!(normalize_stripped("Celebrity M/V"), "celebrity");
    }
}
