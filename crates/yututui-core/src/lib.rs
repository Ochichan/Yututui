#![forbid(unsafe_code)]
//! Pure domain policy shared by yututui's interactive and headless playback owners.
//!
//! This crate deliberately contains no I/O, runtime, UI, or owner state. It is an internal
//! workspace boundary, not a separately published API.

use serde::{Deserialize, Serialize};

pub mod playback;

/// Repeat mode, cycled by the `r` key.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    /// The next mode in the Off → All → One → Off cycle.
    pub fn cycled(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }

    /// Whether any repeat mode is active (i.e. not `Off`).
    pub fn is_on(self) -> bool {
        self != Self::Off
    }

    /// Compatibility query for callers that only need to know whether a set request would be
    /// rejected. The canonical rule lives in [`playback::PlaybackModeState`].
    pub fn set_blocked_by_streaming(self, streaming: bool) -> bool {
        playback::PlaybackModeState::new(self, streaming)
            .transition(playback::PlaybackModeAction::SetRepeat(self))
            .is_err()
    }

    /// Compatibility query for a repeat cycle. New owner code should consume the full pure
    /// transition so the accepted next state and rejection decision cannot be separated.
    pub fn cycle_blocked_by_streaming(self, streaming: bool) -> bool {
        playback::PlaybackModeState::new(self, streaming)
            .transition(playback::PlaybackModeAction::CycleRepeat)
            .is_err()
    }
}
