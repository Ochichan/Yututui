//! Canonicalization helpers: a normalized `title + artist` key used to dedupe the same
//! song arriving from multiple sources / in multiple uploads, plus a light penalty for
//! non-canonical versions (live / remix / sped-up / cover) so streaming prefers the studio cut.

/// Words that mark a non-primary version of a track. Matched case-insensitively against the
/// title's bracketed/parenthetical qualifiers.
const VERSION_MARKERS: &[&str] = &[
    "live",
    "remix",
    "sped up",
    "sped-up",
    "slowed",
    "reverb",
    "cover",
    "karaoke",
    "instrumental",
    "acoustic",
    "8d",
    "nightcore",
    "mashup",
    "edit",
    "bootleg",
    "demo",
];

/// A stable, comparison-friendly key for a track: lowercased title+artist with bracketed
/// qualifiers and non-alphanumerics stripped, whitespace collapsed. Two uploads of the same
/// song (e.g. "Song (Official Video)" vs "Song") map to the same key.
pub fn canonical_key(title: &str, artist: &str) -> String {
    format!("{}|{}", normalize_title(title), normalize_token(artist))
}

/// A penalty in [0,1] for a non-canonical version, from the title's qualifiers. 0 = a clean
/// studio title; higher = more clearly an alternate cut a station should de-prioritize.
pub fn version_penalty(title: &str) -> f32 {
    let lower = title.to_lowercase();
    let hits = VERSION_MARKERS
        .iter()
        .filter(|m| lower.contains(*m))
        .count();
    (hits as f32 * 0.5).min(1.0)
}

/// Normalize a title for keying: drop bracketed/parenthetical qualifiers, then normalize.
fn normalize_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut depth = 0i32;
    for c in title.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    normalize_token(&out)
}

/// Lowercase, keep only alphanumerics + spaces, collapse whitespace.
fn normalize_token(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifiers_and_punctuation_are_ignored_for_keying() {
        assert_eq!(
            canonical_key("Song (Official Video)", "The Band"),
            canonical_key("song", "the band")
        );
        assert_eq!(
            canonical_key("Café — Déjà", "Artist!"),
            canonical_key("café — déjà", "artist")
        );
    }

    #[test]
    fn different_songs_keep_distinct_keys() {
        assert_ne!(canonical_key("A", "x"), canonical_key("B", "x"));
        assert_ne!(canonical_key("A", "x"), canonical_key("A", "y"));
    }

    #[test]
    fn version_markers_are_penalized() {
        assert_eq!(version_penalty("Song"), 0.0);
        assert!(version_penalty("Song (Live)") > 0.0);
        assert!(
            version_penalty("Song (Live at Wembley) [Remix]") >= version_penalty("Song (Live)")
        );
        assert!(version_penalty("Song (Sped Up)") > 0.0);
    }
}
