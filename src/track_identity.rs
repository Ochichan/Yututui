//! Neutral, CJK-safe track-identity helpers shared by matching and lyrics lookup.
//!
//! This module deliberately stays below both playback owners and provider actors.  It keeps
//! normalization and explicit recording-version checks in one place without making a leaf actor
//! depend on the transfer feature.

use unicode_normalization::UnicodeNormalization;

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

/// Remove conservative metadata/video decoration while preserving identity-changing markers.
///
/// Live, remix, acoustic, instrumental and cover markers deliberately survive.  They are checked
/// separately by [`versions_compatible`] and must never be erased merely to improve recall.
pub fn normalize_stripped(s: &str) -> String {
    strip_video_noise(&normalize(&strip_translation_brackets(&strip_annotations(
        s,
    ))))
}

/// Phrase-level video-title noise, removed from an already normalized value.
pub(crate) fn strip_video_noise(normalized: &str) -> String {
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
}

pub(crate) fn strip_annotations(s: &str) -> String {
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
    if let Some(idx) = out.rfind(" - ")
        && is_noise_annotation(&out[idx + 3..])
    {
        out.truncate(idx);
    }
    out
}

fn strip_translation_brackets(s: &str) -> String {
    let mut outside = String::new();
    let mut segments: Vec<(usize, String)> = Vec::new();
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
    (inner_no_ascii && base_ascii && !base_other) || (inner_all_ascii && base_other && !base_ascii)
}

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum VocalVersion {
    #[default]
    Standard,
    Instrumental,
    Karaoke,
    Acapella,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum PerformanceVersion {
    #[default]
    Studio,
    Live,
    Acoustic,
    Demo,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum MixVersion {
    #[default]
    Original,
    Remix,
    SpedUp,
    Slowed,
    RadioEdit,
    Extended,
    Nightcore,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AuthorshipVersion {
    #[default]
    Original,
    Cover,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct VersionSignature {
    vocal: VocalVersion,
    performance: PerformanceVersion,
    mix: MixVersion,
    authorship: AuthorshipVersion,
    taylors_version: bool,
}

/// Reject explicit recording-version mismatches while treating presentation/remaster text as
/// metadata.  Single ambiguous words are recognized only inside annotations/dash suffixes;
/// unambiguous phrases such as "live at" and "sped up" may appear in the full title.
pub fn versions_compatible(source: &str, candidate: &str) -> bool {
    version_signature(source) == version_signature(candidate)
}

fn version_signature(title: &str) -> VersionSignature {
    let full = normalize(title);
    let contexts = version_contexts(title);
    let contextual = |phrase: &str| contexts.iter().any(|ctx| has_phrase(ctx, phrase));
    let anywhere = |phrase: &str| has_phrase(&full, phrase);

    let vocal = if contextual("karaoke") || anywhere("karaoke version") {
        VocalVersion::Karaoke
    } else if contextual("a cappella")
        || contextual("acapella")
        || anywhere("a cappella version")
        || anywhere("acapella version")
    {
        VocalVersion::Acapella
    } else if contextual("instrumental")
        || contextual("backing track")
        || contextual("vocal removed")
        || anywhere("instrumental version")
    {
        VocalVersion::Instrumental
    } else {
        VocalVersion::Standard
    };

    let performance = if contextual("live")
        || anywhere("live at")
        || anywhere("live from")
        || anywhere("live version")
        || anywhere("official live")
    {
        PerformanceVersion::Live
    } else if contextual("acoustic") || anywhere("acoustic version") {
        PerformanceVersion::Acoustic
    } else if contextual("demo") || contextual("rehearsal") || anywhere("demo version") {
        PerformanceVersion::Demo
    } else {
        PerformanceVersion::Studio
    };

    let mix = if contextual("sped up") || anywhere("sped up version") {
        MixVersion::SpedUp
    } else if contextual("slowed")
        || contextual("slowed reverb")
        || anywhere("slowed and reverb")
        || anywhere("slowed version")
    {
        MixVersion::Slowed
    } else if contextual("nightcore") || anywhere("nightcore version") {
        MixVersion::Nightcore
    } else if contextual("radio edit") || anywhere("radio edit version") {
        MixVersion::RadioEdit
    } else if contextual("extended") || anywhere("extended version") {
        MixVersion::Extended
    } else if contextual("remix") || anywhere("remix version") {
        MixVersion::Remix
    } else {
        MixVersion::Original
    };

    let authorship = if contextual("cover")
        || contextual("tribute")
        || anywhere("cover version")
        || anywhere("covered by")
    {
        AuthorshipVersion::Cover
    } else {
        AuthorshipVersion::Original
    };

    VersionSignature {
        vocal,
        performance,
        mix,
        authorship,
        taylors_version: contextual("taylor s version") || anywhere("taylor s version"),
    }
}

fn version_contexts(title: &str) -> Vec<String> {
    let mut contexts = Vec::new();
    let mut chars = title.chars().peekable();
    while let Some(open) = chars.next() {
        if !matches!(open, '(' | '[' | '{') {
            continue;
        }
        let close = match open {
            '(' => ')',
            '[' => ']',
            _ => '}',
        };
        let mut depth = 1usize;
        let mut inner = String::new();
        for next in chars.by_ref() {
            if next == open {
                depth += 1;
            } else if next == close {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            inner.push(next);
        }
        let normalized = normalize(&inner);
        if !normalized.is_empty() {
            contexts.push(normalized);
        }
    }
    if let Some((index, separator_len)) = [" - ", " – ", " — "]
        .into_iter()
        .filter_map(|separator| title.rfind(separator).map(|index| (index, separator.len())))
        .max_by_key(|(index, _)| *index)
    {
        let suffix = normalize(&title[index + separator_len..]);
        if !suffix.is_empty() {
            contexts.push(suffix);
        }
    }
    contexts
}

fn has_phrase(normalized: &str, phrase: &str) -> bool {
    let phrase = normalize(phrase);
    !phrase.is_empty() && format!(" {normalized} ").contains(&format!(" {phrase} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_is_cjk_safe_and_keeps_recording_versions() {
        assert_eq!(normalize("ＴＴ"), "tt");
        assert_eq!(normalize("사건의 지평선"), "사건의 지평선");
        assert_eq!(normalize_stripped("Celebrity (Official MV)"), "celebrity");
        assert_eq!(normalize_stripped("Song (Live)"), "song live");
        assert_eq!(normalize_stripped("Song (Remix)"), "song remix");
        assert_eq!(
            normalize_stripped("Song (Instrumental)"),
            "song instrumental"
        );
    }

    #[test]
    fn explicit_version_mismatches_fail_closed_without_misreading_plain_words() {
        assert!(versions_compatible("Live Forever", "Live Forever"));
        assert!(versions_compatible("Song (Live)", "Song - Live at Seoul"));
        assert!(!versions_compatible("Song", "Song (Live)"));
        assert!(!versions_compatible("Song (Remix)", "Song"));
        assert!(!versions_compatible("Song", "Song (Instrumental)"));
        assert!(!versions_compatible("Song", "Song (Cover)"));
        assert!(!versions_compatible(
            "Love Story (Taylor's Version)",
            "Love Story"
        ));
    }
}
