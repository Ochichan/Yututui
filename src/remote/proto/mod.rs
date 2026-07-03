//! Remote-control wire protocol: newline-delimited JSON over a per-user local socket.
//!
//! A `ytt -r <command>` client sends one [`RemoteRequest`] line and reads one
//! [`RemoteResponse`] line (the frozen **one-shot** mode). The running instance
//! authenticates the client with a per-run token it published in the [`InstanceFile`];
//! the token guards against accidental / cross-user cross-talk (the real boundary is the
//! runtime dir's `0700` perms).
//!
//! Protocol v8 (docs/gui/02) adds a long-lived **session** mode beside one-shot on the
//! same listener. The v7 one-shot request/response byte shapes are frozen forever —
//! additive fields only (`#[serde(default)]` + `skip_serializing_if`); the golden corpus
//! in [`freeze`] locks them.

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::queue::Repeat;
use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

mod command;
#[cfg(test)]
mod freeze;

pub use command::{RemoteCommand, RemoteSettingChange};

/// The version this build speaks. Servers accept one-shot requests in the
/// `PROTOCOL_VERSION_V7..=PROTOCOL_VERSION` range (an old client keeps working forever);
/// anything outside fails loudly with `bad_version` instead of misbehaving.
pub const PROTOCOL_VERSION: u8 = 8;

/// The frozen legacy one-shot version, and the floor of the accepted range. Also the
/// conservative default for instance descriptors that predate the `protocol_version`
/// field — such files come from old builds, so assuming anything newer would overstate
/// what the owner can speak.
pub const PROTOCOL_VERSION_V7: u8 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceMode {
    #[default]
    StandaloneTui,
    Daemon,
}

/// A three-way toggle: flip the current value, or set it explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToggleState {
    #[default]
    Toggle,
    On,
    Off,
}

impl ToggleState {
    /// Resolve against the current value: `Toggle` flips it; `On`/`Off` set it.
    pub fn resolve(self, current: bool) -> bool {
        match self {
            ToggleState::On => true,
            ToggleState::Off => false,
            ToggleState::Toggle => !current,
        }
    }
}

/// One request line: protocol version, the published token, and the command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRequest {
    pub version: u8,
    pub token: String,
    pub command: RemoteCommand,
}

/// One response line. `reason` is a stable machine code (e.g. `queue_empty`); `message`
/// is the human line printed to stdout; `status` carries the snapshot for `status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusSnapshot>,
}

impl RemoteResponse {
    /// A success carrying a human stdout line.
    pub fn ok(message: String) -> Self {
        Self {
            ok: true,
            reason: Some("ok".to_string()),
            message: Some(message),
            status: None,
        }
    }

    /// A semantic rejection carrying a stable machine code.
    pub fn err(reason: &str) -> Self {
        Self {
            ok: false,
            reason: Some(reason.to_string()),
            message: None,
            status: None,
        }
    }

    /// A `status` success: the snapshot plus its one-line human rendering.
    pub fn status(snapshot: StatusSnapshot) -> Self {
        Self {
            ok: true,
            reason: Some("ok".to_string()),
            message: Some(snapshot.human_line()),
            status: Some(snapshot),
        }
    }
}

/// A point-in-time view of playback for `ytt -r status` (and, later, `--json` bars).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub paused: bool,
    pub volume: i64,
    /// 1-based position of the current track in the queue; `0` when the queue is empty.
    pub position: usize,
    pub total: usize,
    #[serde(alias = "radio")]
    pub streaming: bool,
    #[serde(default)]
    pub owner_mode: InstanceMode,
    #[serde(default)]
    pub settings: SettingsSnapshot,
    #[serde(default)]
    pub queue: Vec<QueueItemSnapshot>,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub repeat: Repeat,
    /// Playback position within the current track in milliseconds, sampled at
    /// response time; `None` when nothing is loaded or the position is unknown.
    /// Additive since v7 (older servers simply omit it).
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    /// Current track length in milliseconds; `None` for live streams / unknown.
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// A queue row in the currently effective play order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QueueItemSnapshot {
    pub title: String,
    pub artist: String,
    pub duration: String,
    pub current: bool,
}

/// The small settings surface exposed to the desktop mini player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SettingsSnapshot {
    pub autoplay_streaming: bool,
    pub streaming_mode: StreamingMode,
    pub streaming_source: SearchSource,
    pub speed_tenths: u16,
    pub seek_seconds: u16,
    pub normalize: bool,
    pub gapless: bool,
    pub ai_enabled: bool,
    pub radio_mode: bool,
}

impl SettingsSnapshot {
    pub fn from_config(config: &Config, radio_mode: bool) -> Self {
        let search = config.effective_search();
        Self {
            autoplay_streaming: config.effective_autoplay_streaming(),
            streaming_mode: config.streaming.mode,
            streaming_source: search.normalized_streaming_source(search.streaming_source),
            speed_tenths: speed_tenths(config.effective_speed()),
            seek_seconds: config.effective_seek_seconds().round() as u16,
            normalize: config.effective_normalize(),
            gapless: config.effective_gapless(),
            ai_enabled: config.effective_ai_enabled(),
            radio_mode,
        }
    }
}

impl Default for SettingsSnapshot {
    fn default() -> Self {
        Self::from_config(&Config::default(), false)
    }
}

fn speed_tenths(speed: f64) -> u16 {
    (speed * 10.0).round() as u16
}

impl StatusSnapshot {
    /// One-line human rendering for `ytt -r status` (without `--json`).
    pub fn human_line(&self) -> String {
        let state = if self.paused { "paused" } else { "playing" };
        let track = match (&self.title, &self.artist) {
            (Some(t), Some(a)) => format!("{t} — {a}"),
            (Some(t), None) => t.clone(),
            _ => "nothing playing".to_string(),
        };
        let pos = if self.total > 0 {
            format!("  •  {}/{}", self.position, self.total)
        } else {
            String::new()
        };
        let streaming = if self.streaming {
            "  •  streaming on"
        } else {
            ""
        };
        format!("[{state}] {track}  •  vol {}%{pos}{streaming}", self.volume)
    }
}

/// The on-disk descriptor a `ytt -r` client reads to find and authenticate to the running
/// instance. Written next to the socket in the per-user runtime dir, so client and server
/// always agree on its location without consulting the data dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceFile {
    pub app_pid: u32,
    pub endpoint: String,
    pub token: String,
    pub created_unix: u64,
    #[serde(default)]
    pub mode: InstanceMode,
    #[serde(default = "legacy_protocol_version")]
    pub protocol_version: u8,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

fn legacy_protocol_version() -> u8 {
    PROTOCOL_VERSION_V7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_state_resolves() {
        assert!(ToggleState::On.resolve(false));
        assert!(!ToggleState::Off.resolve(true));
        assert!(ToggleState::Toggle.resolve(false));
        assert!(!ToggleState::Toggle.resolve(true));
    }

    #[test]
    fn request_round_trips() {
        let req = RemoteRequest {
            version: PROTOCOL_VERSION,
            token: "abc".to_string(),
            command: RemoteCommand::Streaming {
                state: ToggleState::On,
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        let back: RemoteRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(
            back.command,
            RemoteCommand::Streaming {
                state: ToggleState::On
            }
        );
        assert_eq!(back.version, PROTOCOL_VERSION);
    }

    #[test]
    fn command_tag_is_snake_case() {
        let line = serde_json::to_string(&RemoteCommand::TogglePause).unwrap();
        assert!(line.contains("\"toggle_pause\""), "got {line}");
    }

    #[test]
    fn volume_and_seek_commands_round_trip() {
        for cmd in [
            RemoteCommand::SetVolume { percent: 42 },
            RemoteCommand::SeekTo { ms: 91_500 },
        ] {
            let line = serde_json::to_string(&cmd).unwrap();
            let back: RemoteCommand = serde_json::from_str(&line).unwrap();
            assert_eq!(back, cmd, "got {line}");
        }
    }

    #[test]
    fn legacy_status_without_position_fields_still_parses() {
        // A v7 server predating elapsed_ms/duration_ms: the fields default to None.
        let line = r#"{"title":"Song","artist":"A","paused":false,"volume":50,"position":1,"total":2,"streaming":false}"#;
        let snap: StatusSnapshot = serde_json::from_str(line).unwrap();
        assert_eq!(snap.elapsed_ms, None);
        assert_eq!(snap.duration_ms, None);
    }

    #[test]
    fn search_commands_carry_query() {
        let line = serde_json::to_string(&RemoteCommand::Play {
            query: "hello".to_string(),
        })
        .unwrap();
        assert!(line.contains("\"play\""), "got {line}");
        assert!(line.contains("\"hello\""), "got {line}");
    }

    #[test]
    fn response_omits_none_fields() {
        let line = serde_json::to_string(&RemoteResponse::err("queue_empty")).unwrap();
        assert!(line.contains("\"ok\":false"));
        assert!(line.contains("queue_empty"));
        assert!(!line.contains("message"));
        assert!(!line.contains("status"));
    }

    #[test]
    fn status_human_line_handles_empty() {
        let snap = StatusSnapshot {
            title: None,
            artist: None,
            paused: false,
            volume: 80,
            position: 0,
            total: 0,
            streaming: false,
            owner_mode: InstanceMode::StandaloneTui,
            settings: SettingsSnapshot::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Repeat::Off,
            elapsed_ms: None,
            duration_ms: None,
        };
        let line = snap.human_line();
        assert!(line.contains("nothing playing"));
        assert!(line.contains("vol 80%"));
    }

    #[test]
    fn status_json_exposes_owner_mode() {
        let snap = StatusSnapshot {
            title: None,
            artist: None,
            paused: false,
            volume: 80,
            position: 0,
            total: 0,
            streaming: false,
            owner_mode: InstanceMode::Daemon,
            settings: SettingsSnapshot::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Repeat::Off,
            elapsed_ms: None,
            duration_ms: None,
        };
        let line = serde_json::to_string(&RemoteResponse::status(snap)).unwrap();
        assert!(line.contains("\"owner_mode\":\"daemon\""), "got {line}");
    }

    #[test]
    fn legacy_instance_file_defaults_to_standalone_v3_shape() {
        let line = r#"{"app_pid":7,"endpoint":"sock","token":"tok","created_unix":1}"#;
        let file: InstanceFile = serde_json::from_str(line).unwrap();
        assert_eq!(file.mode, InstanceMode::StandaloneTui);
        // A descriptor predating the field comes from an old build: assume the frozen
        // legacy version, never the current one (a session gate must not fire on it).
        assert_eq!(file.protocol_version, PROTOCOL_VERSION_V7);
        assert!(file.capabilities.is_empty());
    }
}
