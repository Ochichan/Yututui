//! Shared v8 read-model shapes.
//!
//! These are wire-stable *projections* of core state — never the internal types. The
//! internal `Song` (`src/api/mod.rs`) stays off the wire; [`TrackModel`] is the one
//! track shape every topic and fetch uses.

use serde::{Deserialize, Serialize};

use crate::search_source::SearchSource;

/// The one wire shape for a track, used by the player/queue models.
///
/// Rating note: there is no stored tri-state rating in the core —
/// the TUI's 👍/–/👎 cycle is synthesized from library-favorite membership plus
/// `signals.disliked`. The wire carries exactly those two booleans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackModel {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    // Milliseconds fit a JSON number safely; clients parse them as JS numbers.
    pub duration_ms: Option<u64>,
    pub source: SearchSource,
    #[serde(default)]
    pub is_local: bool,
    #[serde(default)]
    pub downloaded: bool,
    /// Library favorite membership (the "like" half of the rating cycle).
    #[serde(default)]
    pub favorite: bool,
    /// `signals.disliked` (the "dislike" half of the rating cycle).
    #[serde(default)]
    pub disliked: bool,
    /// Romanized display override, resolved core-side per the user's romanized-titles
    /// setting. Clients render `display_*` when present and never romanize themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork: Option<ArtworkRef>,
    /// Built server-side, for copy-link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch_url: Option<String>,
    /// A genuine endless live stream (radio station). THE live signal — clients must
    /// not infer "live" from a missing duration, which is just a track mpv hasn't
    /// measured yet (paused-at-rest restore, mid-load). Omitted when false so the
    /// common case stays byte-identical to the pre-field wire.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_live: bool,
}

/// A reference to the on-disk artwork cache — bytes never ride the socket
///; clients read the cached file from `path` once resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtworkRef {
    /// Cache key (`video_id`, or `local:<path>`-derived for local files).
    pub key: String,
    /// Absolute cached-file path once resolved; `None` while the fetch is in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

/// One synced-lyrics line on the `lyrics` topic. `ms` is `None` for
/// unsynced lines — the client renders them without a highlight clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLineModel {
    // Milliseconds fit a JSON number safely (same rule as TrackModel::duration_ms).
    pub ms: Option<u64>,
    pub text: String,
}

/// Why a track sits in the queue: the DJ Gem / autoplay pick rationale behind the
/// "why?" view. v1 provenance carries the slot label and whatever the pick context
/// knew; `reasons` may be empty and `confidence` is null when no model score exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhyGemModel {
    // The autoplay slot / origin label, e.g. "DJ Gem" or the streaming mode.
    pub slot: String,
    pub reasons: Vec<String>,
    // Model confidence `0..1`; null when the pick had no model score. A JSON `Number`
    // (not `f64`) so the ledger keeps `Eq` — `serde_json::Number` is `Eq`, floats are not.
    pub confidence: Option<serde_json::Number>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_track_parses_with_defaults() {
        let line = r#"{"video_id":"v","title":"T","artist":"A","source":"youtube"}"#;
        let track: TrackModel = serde_json::from_str(line).unwrap();
        assert!(!track.favorite);
        assert!(!track.disliked);
        assert_eq!(track.duration_ms, None);
        assert_eq!(track.artwork, None);
    }

    #[test]
    fn track_omits_absent_options() {
        let track = TrackModel {
            video_id: "v".to_string(),
            title: "T".to_string(),
            artist: "A".to_string(),
            album: None,
            duration_ms: None,
            source: SearchSource::Audius,
            is_local: false,
            downloaded: false,
            favorite: false,
            disliked: false,
            display_title: None,
            display_artist: None,
            artwork: None,
            watch_url: None,
            is_live: false,
        };
        let line = serde_json::to_string(&track).unwrap();
        assert_eq!(
            line,
            r#"{"video_id":"v","title":"T","artist":"A","source":"audius","is_local":false,"downloaded":false,"favorite":false,"disliked":false}"#
        );
    }

    #[test]
    fn artwork_ref_round_trips() {
        let art = ArtworkRef {
            key: "vid".to_string(),
            path: Some("/tmp/media-art/vid.jpg".to_string()),
            mime: Some("image/jpeg".to_string()),
        };
        let line = serde_json::to_string(&art).unwrap();
        let back: ArtworkRef = serde_json::from_str(&line).unwrap();
        assert_eq!(back, art);
        // Unresolved refs omit path/mime entirely.
        let pending = ArtworkRef {
            key: "vid".to_string(),
            path: None,
            mime: None,
        };
        assert_eq!(serde_json::to_string(&pending).unwrap(), r#"{"key":"vid"}"#);
    }
}
