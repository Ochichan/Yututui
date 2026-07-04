//! The one-shot command surface (v7-frozen) shared by `ytt -r`, the tray, and sessions.
//!
//! Byte shapes here are frozen: additive variants/fields only, guarded by the golden
//! corpus in [`super::freeze`].

use serde::{Deserialize, Serialize};

use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

use super::ToggleState;

/// A semantic player command. Applied through the same reducer path a keypress uses, so
/// it works regardless of the TUI's current input mode (Search text entry, Settings, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum RemoteCommand {
    Next,
    Prev,
    TogglePause,
    Play {
        query: String,
    },
    Enqueue {
        query: String,
    },
    VolumeUp,
    VolumeDown,
    /// Set the output volume to an absolute percent (`0..=100`). Additive since v7:
    /// an older server rejects it as `bad_request` instead of misbehaving.
    SetVolume {
        percent: i64,
    },
    SeekBack,
    SeekForward,
    /// Absolute seek within the current track, in milliseconds. Additive since v7.
    SeekTo {
        ms: u64,
    },
    ToggleShuffle,
    CycleRepeat,
    QueuePlay {
        position: usize,
    },
    QueueRemove {
        position: usize,
    },
    #[serde(alias = "radio")]
    Streaming {
        state: ToggleState,
    },
    SetSetting {
        change: RemoteSettingChange,
    },
    ResumeSession,
    Status,
    Quit,
    /// GUI search (additive, v8 sessions): run a grouped multi-catalog search and push
    /// the outcome on the `search` topic as
    /// [`PushEvent::SearchCompleted`](super::PushEvent::SearchCompleted), keyed by
    /// `ticket`. Fire-and-forget: the reply only acknowledges the dispatch.
    RunSearch {
        ticket: u64,
        query: String,
        source: SearchSource,
    },
    /// Play these exact rows now: first replaces the current track, the rest queue up
    /// next. Ids come from rows a prior search/library push handed the client.
    PlayTracks {
        video_ids: Vec<String>,
    },
    /// Append these exact rows to the queue (honoring the enqueue-next setting).
    EnqueueTracks {
        video_ids: Vec<String>,
    },
}

/// A single persisted/live setting mutation from companion surfaces such as the tray panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "setting", rename_all = "snake_case")]
pub enum RemoteSettingChange {
    AutoplayStreaming {
        value: bool,
    },
    StreamingMode {
        value: StreamingMode,
    },
    StreamingSource {
        value: SearchSource,
    },
    /// Playback speed in tenths: `10` means `1.0x`, `15` means `1.5x`.
    Speed {
        tenths: u16,
    },
    SeekSeconds {
        seconds: u16,
    },
    Normalize {
        value: bool,
    },
    Gapless {
        value: bool,
    },
    AiEnabled {
        value: bool,
    },
    RadioMode {
        state: ToggleState,
    },
}
