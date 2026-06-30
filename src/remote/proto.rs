//! Remote-control wire protocol: newline-delimited JSON over a per-user local socket.
//!
//! A `ytt -r <command>` client sends one [`RemoteRequest`] line and reads one
//! [`RemoteResponse`] line. The running instance authenticates the client with a per-run
//! token it published in the [`InstanceFile`]; the token guards against accidental /
//! cross-user cross-talk (the real boundary is the runtime dir's `0700` perms).

use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to the request/response shape. The server rejects a
/// mismatch with `bad_version`, so an old client against a new server fails loudly
/// instead of misbehaving.
pub const PROTOCOL_VERSION: u8 = 2;

/// A semantic player command. Applied through the same reducer path a keypress uses, so
/// it works regardless of the TUI's current input mode (Search text entry, Settings, …).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum RemoteCommand {
    Next,
    Prev,
    TogglePause,
    VolumeUp,
    VolumeDown,
    SeekBack,
    SeekForward,
    #[serde(alias = "radio")]
    Streaming {
        state: ToggleState,
    },
    Status,
    Quit,
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
        };
        let line = snap.human_line();
        assert!(line.contains("nothing playing"));
        assert!(line.contains("vol 80%"));
    }
}
