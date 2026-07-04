//! Player and queue read models (docs/gui/02 §11.3).

use serde::{Deserialize, Serialize};

use crate::queue::Repeat;

use super::InstanceMode;
use super::model::TrackModel;

/// Full player state, pushed on the `player` topic on discontinuity only — never on the
/// 1 Hz time tick. Clients interpolate position from `elapsed_ms` + a wall-clock anchor
/// and rebase whenever `position_epoch` changes or a new event arrives.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerModel {
    pub track: Option<TrackModel>,
    pub paused: bool,
    /// `0..=100`.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub volume: i64,
    /// Playback speed in tenths, the established v7 unit (`10` = 1.0×).
    pub speed_tenths: u16,
    /// Sampled at emit; interpolate while playing.
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub elapsed_ms: Option<u64>,
    /// `None` ⇒ live stream ("ON AIR" mode).
    #[cfg_attr(feature = "ts-export", ts(type = "number | null"))]
    pub duration_ms: Option<u64>,
    /// Discontinuity counter — rebase interpolation when it changes.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub position_epoch: u64,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// DJ Gem autoplay on/off.
    pub streaming: bool,
    pub radio_mode: bool,
    /// ICY title for live radio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_now_playing: Option<String>,
    pub owner_mode: InstanceMode,
    pub eq: EqModel,
    /// Queue cursor (order position). The cursor rides the player topic — a track
    /// advance is a small player push, never a queue-snapshot push (docs/gui/02 §7).
    pub queue_pos: usize,
    pub queue_len: usize,
}

/// Full queue contents, pushed on membership/order change only. `rev` is assigned from
/// an owner-global monotonic counter (bumped by every membership/order mutation,
/// including restore-snapshot and the radio-mode queue swaps) so whole-queue swaps are
/// always observable; it is never persisted. The current row is derived client-side from
/// [`PlayerModel::queue_pos`] — rows carry no current flag.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueModel {
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub rev: u64,
    /// In effective play order.
    pub items: Vec<TrackModel>,
}

/// Equalizer state as the player topic exposes it.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqModel {
    pub preset: String,
    /// Gain per ISO band, dB.
    pub bands: [f64; 10],
    pub normalize: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_source::SearchSource;

    fn track(id: &str) -> TrackModel {
        TrackModel {
            video_id: id.to_string(),
            title: "T".to_string(),
            artist: "A".to_string(),
            album: None,
            duration_ms: Some(1_000),
            source: SearchSource::Youtube,
            is_local: false,
            downloaded: false,
            favorite: false,
            disliked: false,
            display_title: None,
            display_artist: None,
            artwork: None,
            watch_url: None,
            is_live: false,
        }
    }

    #[test]
    fn player_model_round_trips() {
        let model = PlayerModel {
            track: Some(track("v1")),
            paused: true,
            volume: 80,
            speed_tenths: 15,
            elapsed_ms: None,
            duration_ms: None,
            position_epoch: 9,
            shuffle: false,
            repeat: Repeat::One,
            streaming: true,
            radio_mode: false,
            stream_now_playing: Some("ICY Title".to_string()),
            owner_mode: InstanceMode::Daemon,
            eq: EqModel {
                preset: "Rock".to_string(),
                bands: [1.5, 0.0, -2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 3.0],
                normalize: true,
            },
            queue_pos: 0,
            queue_len: 0,
        };
        let line = serde_json::to_string(&model).unwrap();
        let back: PlayerModel = serde_json::from_str(&line).unwrap();
        assert_eq!(back, model);
        assert!(line.contains("\"queue_pos\":0"), "got {line}");
    }

    #[test]
    fn queue_model_round_trips_and_has_no_current_flag() {
        let model = QueueModel {
            rev: 41,
            items: vec![track("v1"), track("v2")],
        };
        let line = serde_json::to_string(&model).unwrap();
        assert!(
            !line.contains("current"),
            "cursor rides PlayerModel: {line}"
        );
        let back: QueueModel = serde_json::from_str(&line).unwrap();
        assert_eq!(back, model);
    }

    #[test]
    fn eq_model_carries_ten_bands() {
        let eq = EqModel {
            preset: "Flat".to_string(),
            bands: [0.25; 10],
            normalize: false,
        };
        let line = serde_json::to_string(&eq).unwrap();
        let back: EqModel = serde_json::from_str(&line).unwrap();
        assert_eq!(back.bands.len(), 10);
        assert_eq!(back, eq);
    }
}
