//! MusicGate: a rule-based content filter that keeps non-music videos (reactions, podcasts,
//! tutorials, …) and gimmick re-uploads (karaoke / nightcore / 8D / sped-up / slowed+reverb)
//! out of the streaming candidate pool.
//!
//! No DJ Gem, no YouTube Data API, no `regex` — just lowercased token matching over the title and
//! channel name, mirroring [`super::canonical::version_penalty`]. The only metadata available
//! is the (lean) [`crate::api::Song`]: `title` and `artist` (which holds the channel/uploader
//! name for yt-dlp-sourced rows). The heuristics are deliberately conservative — they prefer
//! to let a borderline track through over wrongly dropping a real song, and trusted channels
//! (`"- Topic"`, VEVO) bypass the checks entirely.

use crate::streaming::candidate::CandidateSource;
use crate::streaming::config::StreamingMode;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GateAction {
    Keep,
    Demote,
    Reject,
}

#[derive(Debug, Clone, Copy)]
pub struct MusicGateDecision {
    pub action: GateAction,
    pub reason: Option<&'static str>,
    pub risk: f32,
    pub confidence: f32,
}

/// Title keywords that mark a video as definitely non-music, with the reason tag returned on a
/// hit. Matched as whole words (see [`title_has_word`]) so e.g. "Chain Reaction" (a real song)
/// is NOT caught by "reaction". Words known to be dangerous as song-title substrings —
/// `review`, `news`, `lesson`, `trailer`, `commentary`, `edit` — are deliberately omitted.
const NON_MUSIC_WORDS: &[(&str, &str)] = &[
    ("reacts", "reaction video"),
    ("reacting", "reaction video"),
    ("podcast", "podcast"),
    ("tutorial", "tutorial"),
    ("interview", "interview"),
    ("vlog", "vlog"),
    ("documentary", "documentary"),
    ("gameplay", "gameplay"),
    ("unboxing", "unboxing"),
    ("mukbang", "mukbang"),
];

/// Multi-word non-music markers, matched as substrings — a space-bearing phrase can't
/// accidentally sit inside a single word, so the word-boundary guard isn't needed. Note: bare
/// "reaction" is intentionally NOT a marker — it's a standalone word in real songs ("Chain
/// Reaction", "Bad Reaction"). The reaction-*video* forms below (`(reaction`, `reaction to`,
/// `reacts to`, …) don't occur in those titles.
const NON_MUSIC_PHRASES: &[(&str, &str)] = &[
    ("how to ", "tutorial"),
    ("behind the scenes", "behind the scenes"),
    ("making of", "making of"),
    ("press conference", "press conference"),
    ("reaction video", "reaction video"),
    ("(reaction", "reaction video"),
    ("[reaction", "reaction video"),
    ("| reaction", "reaction video"),
    ("reaction to", "reaction video"),
    ("reacts to", "reaction video"),
    ("reacting to", "reaction video"),
    ("first time hearing", "reaction video"),
    ("#shorts", "youtube shorts"),
    ("| shorts", "youtube shorts"),
    ("리액션", "reaction video"),
    ("처음 듣고 반응", "reaction video"),
    ("처음 듣는", "reaction video"),
    ("해설", "commentary"),
    ("분석", "analysis"),
    ("인터뷰", "interview"),
    ("팟캐스트", "podcast"),
    ("브이로그", "vlog"),
    ("튜토리얼", "tutorial"),
    ("リアクション", "reaction video"),
    ("解説", "commentary"),
    ("インタビュー", "interview"),
    ("ポッドキャスト", "podcast"),
];

/// Softer risk markers that are too collision-prone for a global hard reject but useful for
/// demoting or preflighting public YouTube search results.
const RISK_PHRASES: &[(&str, &str, f32)] = &[
    ("review", "review", 0.35),
    ("commentary", "commentary", 0.50),
    ("explained", "explainer", 0.45),
    ("analysis", "analysis", 0.40),
    ("news", "news", 0.35),
    ("trailer", "trailer", 0.35),
    ("lesson", "tutorial", 0.35),
    ("top 10", "listicle", 0.45),
    ("episode", "episode", 0.35),
    ("podcast", "podcast", 0.75),
    ("interview", "interview", 0.70),
    ("reaction", "reaction video", 0.70),
    ("リビュー", "review", 0.35),
    ("レビュー", "review", 0.35),
];

/// Gimmick / altered-version markers: re-uploads that aren't the real track. Distinctive
/// enough for substring matching. `instrumental` / `live` / `cover` / `acoustic` / `slowed`
/// (alone) are deliberately absent — those stay SOFT (a [`super::canonical::version_penalty`]
/// demote, not a hard reject).
const GIMMICK_MARKERS: &[(&str, &str)] = &[
    ("karaoke", "karaoke"),
    ("nightcore", "nightcore"),
    ("8d audio", "8d audio"),
    ("8d version", "8d audio"),
    (" 8d", "8d audio"),
    ("sped up", "sped up"),
    ("sped-up", "sped up"),
    ("slowed + reverb", "slowed reverb"),
    ("slowed+reverb", "slowed reverb"),
    ("slowed reverb", "slowed reverb"),
];

/// Hard non-music reject. Returns `Some(reason)` when the title or channel identifies the
/// content as definitely not a music track; `None` to pass. Trusted music channels
/// (`"- Topic"`, VEVO) short-circuit to `None` — they're pre-curated and any keyword hit there
/// would be a false positive.
pub fn non_music_reason(title: &str, channel: &str) -> Option<&'static str> {
    if is_trusted_music_channel(channel) {
        return None;
    }
    let t = title.to_lowercase();
    for &(word, reason) in NON_MUSIC_WORDS {
        if title_has_word(&t, word) {
            return Some(reason);
        }
    }
    for &(phrase, reason) in NON_MUSIC_PHRASES {
        if t.contains(phrase) {
            return Some(reason);
        }
    }
    // Channel-name signal: only "podcast" is safe to reject on (other channel words risk FPs).
    if title_has_word(&channel.to_lowercase(), "podcast") {
        return Some("podcast channel");
    }
    None
}

/// Full gate decision. Strong markers still reject, but public YouTube search candidates with
/// weaker risk are demoted first so a sparse station is not starved by false positives.
pub fn decide(
    title: &str,
    channel: &str,
    source: CandidateSource,
    mode: StreamingMode,
) -> MusicGateDecision {
    if let Some(reason) = altered_version_reason(title) {
        let action = match mode {
            StreamingMode::Focused | StreamingMode::Balanced => GateAction::Reject,
            StreamingMode::Discovery => GateAction::Demote,
        };
        return MusicGateDecision {
            action,
            reason: Some(reason),
            risk: 0.85,
            confidence: 0.90,
        };
    }
    if is_trusted_music_channel(channel) {
        return MusicGateDecision {
            action: GateAction::Keep,
            reason: None,
            risk: 0.0,
            confidence: 0.95,
        };
    }
    if let Some(reason) = non_music_reason(title, channel) {
        return MusicGateDecision {
            action: GateAction::Reject,
            reason: Some(reason),
            risk: 0.95,
            confidence: 0.90,
        };
    }

    let risk = non_music_risk_score(title, channel);
    let music_tier = music_tier_score(title, channel);
    let action = if source == CandidateSource::YtdlpStreaming && risk >= 0.85 {
        GateAction::Reject
    } else if source == CandidateSource::YtdlpStreaming && risk >= 0.55 && music_tier <= 0.0 {
        match mode {
            StreamingMode::Focused => GateAction::Reject,
            StreamingMode::Balanced | StreamingMode::Discovery => GateAction::Demote,
        }
    } else if risk >= 0.35 && music_tier <= 0.0 {
        GateAction::Demote
    } else {
        GateAction::Keep
    };

    MusicGateDecision {
        action,
        reason: (action != GateAction::Keep).then_some(risk_reason(title, channel)),
        risk,
        confidence: risk.max(music_tier),
    }
}

/// Risk score in [0,1] for borderline non-music. Kept separate from [`non_music_reason`] so the
/// ranker can demote questionable public YouTube candidates without hard-dropping every one.
pub fn non_music_risk_score(title: &str, channel: &str) -> f32 {
    if is_trusted_music_channel(channel) {
        return 0.0;
    }
    if non_music_reason(title, channel).is_some() {
        return 0.95;
    }
    let t = title.to_lowercase();
    let c = channel.to_lowercase();
    let mut risk = 0.0f32;
    for &(phrase, _, w) in RISK_PHRASES {
        if t.contains(phrase) || c.contains(phrase) {
            risk = risk.max(w);
        }
    }
    if title_has_word(&c, "podcast") {
        risk = risk.max(0.80);
    }
    if t.contains("1 hour") || t.contains("one hour") || t.contains("full album") {
        risk = risk.max(0.50);
    }
    risk.min(1.0)
}

fn risk_reason(title: &str, channel: &str) -> &'static str {
    if let Some(reason) = non_music_reason(title, channel) {
        return reason;
    }
    let text = format!("{} {}", title.to_lowercase(), channel.to_lowercase());
    for &(phrase, reason, _) in RISK_PHRASES {
        if text.contains(phrase) {
            return reason;
        }
    }
    "non-music risk"
}

/// Gimmick / altered-version reject (karaoke / nightcore / 8D / sped-up / slowed+reverb).
/// Separate from [`non_music_reason`] because these are *less* objectionable — the caller only
/// applies them when the pool is healthy enough to spare them (never starving the station).
pub fn gimmick_reason(title: &str) -> Option<&'static str> {
    let t = title.to_lowercase();
    GIMMICK_MARKERS
        .iter()
        .find(|(m, _)| t.contains(m))
        .map(|&(_, reason)| reason)
}

/// Altered-version marker used by the stricter streaming gate. This is broader than
/// [`gimmick_reason`]: Focused/Balanced reject these by default, while Discovery only demotes.
pub fn altered_version_reason(title: &str) -> Option<&'static str> {
    let t = title.to_lowercase();
    for (needle, reason) in [
        ("karaoke", "karaoke"),
        ("backing track", "backing track"),
        ("instrumental", "instrumental"),
        ("off vocal", "instrumental"),
        ("no vocal", "instrumental"),
        ("cover", "cover"),
        ("nightcore", "nightcore"),
        ("8d audio", "8d audio"),
        ("8d version", "8d audio"),
        (" 8d", "8d audio"),
        ("sped up", "sped up"),
        ("sped-up", "sped up"),
        ("slowed", "slowed"),
        ("slowed + reverb", "slowed reverb"),
        ("slowed+reverb", "slowed reverb"),
        ("slowed reverb", "slowed reverb"),
        ("remix", "remix"),
        ("acoustic", "acoustic"),
    ] {
        if t.contains(needle) {
            return Some(reason);
        }
    }
    if live_version_marker(&t) {
        return Some("live");
    }
    None
}

fn live_version_marker(lower_title: &str) -> bool {
    lower_title.contains("(live")
        || lower_title.contains("[live")
        || lower_title.contains(" live at ")
        || lower_title.contains(" live from ")
        || lower_title.contains(" live in ")
        || lower_title.contains(" live on ")
        || lower_title.contains(" live version")
        || lower_title.ends_with(" live")
}

/// A positive [0,1] "this is real, official music" signal from the title + channel. Used as a
/// small *additive ranking bonus* (never a filter). Trusted channels (`"- Topic"` / VEVO)
/// score highest; explicit "official audio/video" titles next; weaker music markers lowest;
/// an ordinary upload scores 0.
pub fn music_tier_score(title: &str, channel: &str) -> f32 {
    if is_trusted_music_channel(channel) {
        return 1.0;
    }
    let t = title.to_lowercase();
    const STRONG: &[&str] = &[
        "official audio",
        "official music video",
        "official video",
        "official mv",
    ];
    const WEAK: &[&str] = &[
        "official",
        "music video",
        "lyric video",
        "lyrics",
        "visualizer",
        "m/v",
        " mv",
    ];
    if STRONG.iter().any(|m| t.contains(m)) {
        0.7
    } else if WEAK.iter().any(|m| t.contains(m)) {
        0.4
    } else {
        0.0
    }
}

/// Whether the channel is a trusted, pre-curated music source: YouTube `"- Topic"`
/// auto-channels and VEVO. Used to short-circuit the non-music check.
pub fn is_trusted_music_channel(channel: &str) -> bool {
    let c = channel.to_lowercase();
    c.ends_with("- topic") || c.ends_with("-topic") || c.contains("vevo")
}

/// Whether `haystack` (already lowercased) contains `word` as a standalone token — not as a
/// substring of a larger word. Avoids false positives like "chain reaction" matching
/// "reaction" or "extract" matching "react". No regex: scan for the substring and accept only
/// when neither neighboring byte is an ASCII letter.
fn title_has_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let before_ok = start == 0 || !bytes[start - 1].is_ascii_alphabetic();
        let after_ok = end >= bytes.len() || !bytes[end].is_ascii_alphabetic();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_reaction_video() {
        assert_eq!(
            non_music_reason("Artist - Song (Reaction)", "Reactor"),
            Some("reaction video")
        );
        assert_eq!(
            non_music_reason("My reaction to this", "X"),
            Some("reaction video")
        );
    }

    #[test]
    fn word_boundary_passes_chain_reaction() {
        // "Chain Reaction" contains the substring "reaction" but is a real song — must pass.
        assert_eq!(non_music_reason("Chain Reaction", "Diana Ross"), None);
        assert_eq!(non_music_reason("Overreaction", "Band"), None);
        assert_eq!(non_music_reason("Reactor", "Band"), None);
    }

    #[test]
    fn rejects_podcast_in_title_and_channel() {
        assert_eq!(
            non_music_reason("The Joe Show Podcast #12", "Some Channel"),
            Some("podcast")
        );
        assert_eq!(
            non_music_reason("Episode 5", "The Music Podcast"),
            Some("podcast channel")
        );
    }

    #[test]
    fn rejects_tutorial_interview_vlog() {
        assert_eq!(
            non_music_reason("Guitar Tutorial for Beginners", "X"),
            Some("tutorial")
        );
        assert_eq!(non_music_reason("How to play piano", "X"), Some("tutorial"));
        assert_eq!(
            non_music_reason("Exclusive Interview", "X"),
            Some("interview")
        );
        assert_eq!(non_music_reason("Tour Vlog day 3", "X"), Some("vlog"));
    }

    #[test]
    fn rejects_shorts() {
        assert_eq!(
            non_music_reason("Cool dance #shorts", "X"),
            Some("youtube shorts")
        );
    }

    #[test]
    fn trusted_channel_bypasses_keyword() {
        // A topic / VEVO channel is pre-curated music: any keyword hit is a false positive.
        assert_eq!(non_music_reason("Interview", "Radiohead - Topic"), None);
        assert_eq!(non_music_reason("Reaction", "TaylorSwiftVEVO"), None);
    }

    #[test]
    fn passes_real_song_titles() {
        let titles = [
            ("Bohemian Rhapsody", "Queen"),
            ("Everything", "The Black Skirts"),
            ("News", "Paramore"),      // "news" deliberately excluded
            ("Under Review", "Band"),  // "review" deliberately excluded
            ("Live Forever", "Oasis"), // "live" stays soft, not a hard reject
            ("Editorial", "Artist"),   // "edit" deliberately excluded
        ];
        for (t, c) in titles {
            assert_eq!(non_music_reason(t, c), None, "{t} should pass");
        }
    }

    #[test]
    fn gimmick_rejects_altered_versions() {
        assert_eq!(gimmick_reason("Song (Nightcore)"), Some("nightcore"));
        assert_eq!(gimmick_reason("Song (Sped Up)"), Some("sped up"));
        assert_eq!(
            gimmick_reason("Song (Slowed + Reverb)"),
            Some("slowed reverb")
        );
        assert_eq!(gimmick_reason("Song (Karaoke Version)"), Some("karaoke"));
        assert_eq!(gimmick_reason("Song (8D Audio)"), Some("8d audio"));
    }

    #[test]
    fn gimmick_passes_legitimate_versions() {
        // Slowed-only, instrumental, live, acoustic stay (soft-penalized elsewhere, never hard).
        assert_eq!(gimmick_reason("Song (Slowed)"), None);
        assert_eq!(gimmick_reason("Song (Instrumental)"), None);
        assert_eq!(gimmick_reason("Song (Live at Wembley)"), None);
        assert_eq!(gimmick_reason("Song (Acoustic)"), None);
    }

    #[test]
    fn music_tier_ranks_official_over_plain() {
        // Trusted channel = top tier regardless of title.
        assert_eq!(music_tier_score("Anything", "Radiohead - Topic"), 1.0);
        assert_eq!(music_tier_score("whatever", "ArianaGrandeVevo"), 1.0);
        // Explicit "official …" titles = strong tier.
        assert_eq!(music_tier_score("Song (Official Audio)", "Band"), 0.7);
        assert_eq!(music_tier_score("Song (Official Music Video)", "Band"), 0.7);
        // Weaker music markers = mid tier.
        assert_eq!(music_tier_score("Song (Lyric Video)", "Band"), 0.4);
        assert_eq!(music_tier_score("Song MV", "Band"), 0.4);
        // A plain upload earns no bonus.
        assert_eq!(music_tier_score("Just A Song", "Random User"), 0.0);
    }

    #[test]
    fn trusted_channel_detection() {
        assert!(is_trusted_music_channel("Radiohead - Topic"));
        assert!(is_trusted_music_channel("ArianaGrandeVevo"));
        assert!(!is_trusted_music_channel("Some Reaction Channel"));
    }
}
