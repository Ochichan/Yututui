//! Shared v8 read-model shapes (docs/gui/02 §11.2, §12).
//!
//! These are wire-stable *projections* of core state — never the internal types. The
//! internal `Song` (`src/api/mod.rs`) stays off the wire; [`TrackModel`] is the one
//! track shape every topic and fetch uses.

use serde::{Deserialize, Serialize};

use crate::search_source::SearchSource;

/// The one wire shape for a track, used by player/queue/library/search/AI models alike.
///
/// Rating note (docs/gui/02 §11.2): there is no stored tri-state rating in the core —
/// the TUI's 👍/–/👎 cycle is synthesized from library-favorite membership plus
/// `signals.disliked`. The wire carries exactly those two booleans.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackModel {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    // ms fit a JS number safely; ts-rs would otherwise map u64 → bigint, but JSON.parse
    // yields a number at runtime (docs/gui/05 §12 risk 3 — keep the wire type honest).
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
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
}

/// A reference to the on-disk artwork cache — bytes never ride the socket
/// (docs/gui/02 §12). The GUI serves it to its webview at `ytm://app/art/<key>`.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
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
