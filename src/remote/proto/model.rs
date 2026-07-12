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
    /// A genuine endless live stream (radio station). THE live signal — clients must
    /// not infer "live" from a missing duration, which is just a track mpv hasn't
    /// measured yet (paused-at-rest restore, mid-load). Omitted when false so the
    /// common case stays byte-identical to the pre-field wire.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_live: bool,
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

/// One synced-lyrics line on the `lyrics` topic (docs/gui/02 §7). `ms` is `None` for
/// unsynced lines — the client renders them without a highlight clock.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLineModel {
    // ms fit a JS number safely (same rule as TrackModel::duration_ms).
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub ms: Option<u64>,
    pub text: String,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStateModel {
    Running,
    Done,
    Failed,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DownloadStatusModel {
    pub video_id: String,
    pub title: String,
    pub state: DownloadStateModel,
    pub pct: f64,
    #[cfg_attr(feature = "ts-export", ts(type = "string | null"))]
    pub error: Option<String>,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryPageModel {
    pub scope: String,
    pub filter: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub offset: u64,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub total: u64,
    pub tracks: Vec<TrackModel>,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistSummaryModel {
    pub id: String,
    pub name: String,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub count: u64,
    pub description: Option<String>,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistDetailModel {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub tracks: Vec<TrackModel>,
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
    fn download_status_serializes_absent_error_as_null() {
        let status = DownloadStatusModel {
            video_id: "v".to_owned(),
            title: "Track".to_owned(),
            state: DownloadStateModel::Running,
            pct: 0.0,
            error: None,
        };
        let value = serde_json::to_value(status).unwrap();
        assert_eq!(value["state"], "running");
        assert!(value["error"].is_null());
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

    #[test]
    fn playlist_descriptions_serialize_absence_as_null() {
        let summary = PlaylistSummaryModel {
            id: "mix".to_owned(),
            name: "Mix".to_owned(),
            count: 0,
            description: None,
        };
        assert_eq!(
            serde_json::to_value(summary).unwrap()["description"],
            serde_json::Value::Null
        );
        let detail = PlaylistDetailModel {
            id: "mix".to_owned(),
            name: "Mix".to_owned(),
            description: None,
            tracks: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(detail).unwrap()["description"],
            serde_json::Value::Null
        );
    }
}
