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
mod model;
mod model_player;
mod model_settings;
mod session;

pub(crate) use command::RequestRetryClass;
pub use command::{
    GuiSettingChange, REMOTE_MAX_EXPORT_DIRECTORY_BYTES, REMOTE_MAX_GEMINI_KEY_BYTES,
    REMOTE_MAX_PAGE_LIMIT, REMOTE_MAX_QUERY_BYTES, REMOTE_MAX_SETTING_NAME_BYTES,
    REMOTE_MAX_SETTING_STRING_BYTES, REMOTE_MAX_TOPICS, REMOTE_MAX_TRACK_ID_BYTES,
    REMOTE_MAX_TRACK_IDS, RateChange, RemoteCommand, RemoteSettingChange,
};
pub use model::{
    ArtworkRef, DownloadStateModel, DownloadStatusModel, LibraryPageModel, LyricLineModel,
    PlaylistDetailModel, PlaylistSummaryModel, TrackModel,
};
pub use model_player::{EqModel, PlayerModel, QueueModel};
pub use model_settings::{
    ActionInfoModel, AnimationsModel, AudioSettingsModel, KeymapSettingsModel,
    PlaybackSettingsModel, SearchSettingsModel, SettingsModelV8, StorageSettingsModel,
    StreamingSettingsModel, ThemePresetModel, ThemeSettingsModel, UiSettingsModel,
};
pub use session::{
    ClientFrame, ClientOp, HelloAck, HelloBody, HelloRequest, PushEvent, SearchGroup, ServerFrame,
    Topic,
};

/// The version this build speaks. Servers accept one-shot requests in the
/// `PROTOCOL_VERSION_V7..=PROTOCOL_VERSION` range (an old client keeps working forever);
/// anything outside fails loudly with `bad_version` instead of misbehaving.
pub const PROTOCOL_VERSION: u8 = 8;

/// The frozen legacy one-shot version, and the floor of the accepted range. Also the
/// conservative default for instance descriptors that predate the `protocol_version`
/// field — such files come from old builds, so assuming anything newer would overstate
/// what the owner can speak.
pub const PROTOCOL_VERSION_V7: u8 = 7;

/// Upper bound the one-shot `ytt -r` client reads for a single reply line. A `status` reply
/// embeds the full queue (hard-capped at 999 items ⇒ ~150 KB worst case), so the historic
/// 4 KB client bound rejected legitimate large-queue replies as malformed. Sized to the
/// session-frame ceiling so the client read bound and the server's notion of a valid reply
/// share one source, while still refusing an unbounded stream from a corrupt/hostile peer.
pub const MAX_ONESHOT_REPLY_BYTES: usize = 256 * 1024;

/// Machine reason returned when a mutating request may have been applied but no authoritative
/// reply was observed. Clients must ask the user to inspect current state before retrying.
pub const CONFIRMATION_LOST_REASON: &str = "confirmation_lost";

/// Instance capability proving that same-ID mutation retries return an explicitly marked
/// retained owner outcome. Clients must not automatically retry an ambiguous mutation unless
/// the published instance descriptor advertises this capability.
pub const RETAINED_REQUEST_OUTCOMES_CAPABILITY: &str = "retained-request-outcomes-v1";

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
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
    /// Stable identity for retaining state-changing outcomes within this advertised owner's
    /// current 60-second retention window. It is not safe to reuse after that window or after the
    /// descriptor token/owner changes.
    /// Status and RunSearch use it only for validation/correlation and execute afresh. Optional so
    /// the frozen v7 request stays byte-for-byte compatible; current clients always populate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub command: RemoteCommand,
}

/// One response line. `reason` is a stable machine code (e.g. `queue_empty`); `message`
/// is the human line printed to stdout; `status` carries the snapshot for `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteResponse {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusSnapshot>,
    /// Additive v8 per-command reply payload (docs/gui/02 §13): absent on every
    /// pre-existing reply (byte-identical v7/v8 wire) and deserializes to a genuine
    /// `None` from old servers. A named field on purpose — `#[serde(flatten)]` over an
    /// `Option` yields `Some(default)` on plain replies, breaking the freeze corpus's
    /// value-equality contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ResponseData>,
}

/// Typed per-command reply payloads riding [`RemoteResponse::data`]. Untagged: each
/// variant serializes as its bare shape — exactly the body the GUI's consumers read
/// (the gateway projects `data` as the `req` reply payload and folds it into the `cmd`
/// reply body). Variants land additively with their milestone streams; keep the shapes
/// structurally disjoint so untagged deserialization stays unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseData {
    /// `clear_romanization_cache` → `{ cleared }` (wired in the settings stream).
    Cleared {
        cleared: u64,
    },
    LibraryPage(LibraryPageModel),
    PlaylistDetail(PlaylistDetailModel),
}

impl RemoteResponse {
    /// A success carrying a human stdout line.
    pub fn ok(message: String) -> Self {
        Self {
            ok: true,
            reason: Some("ok".to_string()),
            message: Some(message),
            status: None,
            data: None,
        }
    }

    /// A semantic rejection carrying a stable machine code.
    pub fn err(reason: &str) -> Self {
        Self {
            ok: false,
            reason: Some(reason.to_string()),
            message: None,
            status: None,
            data: None,
        }
    }

    /// A semantic rejection with sanitized, actionable text for human-facing one-shot clients.
    /// `reason` remains the stable machine contract; `message` is deliberately optional on the
    /// wire, so adding it does not change how older clients parse errors.
    pub fn err_with_message(reason: &str, message: String) -> Self {
        Self {
            ok: false,
            reason: Some(reason.to_string()),
            message: Some(message),
            status: None,
            data: None,
        }
    }

    /// A `status` success: the snapshot plus its one-line human rendering.
    pub fn status(snapshot: StatusSnapshot) -> Self {
        Self {
            ok: true,
            reason: Some("ok".to_string()),
            message: Some(snapshot.human_line()),
            status: Some(snapshot),
            data: None,
        }
    }
}

/// Additive one-shot transport metadata around the frozen [`RemoteResponse`] body.
///
/// `retained_replay` is set only when this exchange observed the completed result of a prior
/// same-ID mutation admission. It stays absent for ordinary responses, pre-admission rejections,
/// and old servers, preserving the shipped v7/v8 byte shapes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RemoteResponseEnvelope {
    #[serde(flatten)]
    pub response: RemoteResponse,
    #[serde(default, skip_serializing_if = "is_false")]
    pub retained_replay: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    /// Whether the current item is a genuine endless live stream. Duration absence is
    /// deliberately not sufficient: a normal track may have no measured duration while loading.
    /// Omitted when false so existing v7 status bytes remain unchanged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_live: bool,
    /// Revision of the queue snapshot used to render row positions. Newer clients attach it to
    /// destructive/indexed commands; absent means an older v7 owner without revision support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_rev: Option<u64>,
    /// Stable identity of the current track. Unlike title/artist, this does not change for ICY
    /// metadata and does distinguish different tracks with identical display text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    /// Discontinuity counter for seek/track restarts. Omitted at zero to preserve legacy bytes.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub position_epoch: u64,
    /// Current track's cached artwork file, when the media-art cache has resolved
    /// one for it. Additive since v8; skip-serialized so pre-artwork shapes (and
    /// the freeze goldens) stay byte-identical when it is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artwork: Option<ArtworkRef>,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
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
            request_id: None,
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
    fn personal_export_command_is_an_additive_round_trip() {
        let command = RemoteCommand::ExportPersonalData {
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
        };
        let line = serde_json::to_string(&command).unwrap();
        let back: RemoteCommand = serde_json::from_str(&line).unwrap();
        assert_eq!(back, command);
    }

    #[test]
    fn revision_checked_queue_commands_are_additive_v8_shapes() {
        let cases = [
            (
                RemoteCommand::QueuePlayIfRevision {
                    position: 3,
                    expected_rev: 41,
                },
                r#"{"cmd":"queue_play_if_revision","position":3,"expected_rev":41}"#,
            ),
            (
                RemoteCommand::QueueRemoveIfRevision {
                    position: 0,
                    expected_rev: 42,
                },
                r#"{"cmd":"queue_remove_if_revision","position":0,"expected_rev":42}"#,
            ),
        ];
        for (command, expected) in cases {
            let line = serde_json::to_string(&command).unwrap();
            assert_eq!(line, expected);
            assert_eq!(
                serde_json::from_str::<RemoteCommand>(&line).unwrap(),
                command
            );
        }
    }

    #[test]
    fn legacy_status_without_position_fields_still_parses() {
        // A v7 server predating elapsed_ms/duration_ms: the fields default to None.
        let line = r#"{"title":"Song","artist":"A","paused":false,"volume":50,"position":1,"total":2,"streaming":false}"#;
        let snap: StatusSnapshot = serde_json::from_str(line).unwrap();
        assert_eq!(snap.elapsed_ms, None);
        assert_eq!(snap.duration_ms, None);
        assert_eq!(snap.artwork, None);
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
    fn response_data_lane_is_byte_invisible_when_absent() {
        // The v8 data lane must not change any pre-existing reply's bytes (v7 freeze)…
        let ok = serde_json::to_string(&RemoteResponse::ok("pong".to_string())).unwrap();
        assert!(!ok.contains("data"), "None data must not serialize: {ok}");
        // …and a plain reply from an old server must deserialize to a genuine None so
        // value-equality against constructor-built responses keeps holding.
        let back: RemoteResponse =
            serde_json::from_str(r#"{"ok":true,"reason":"ok","message":"pong"}"#).unwrap();
        assert_eq!(back, RemoteResponse::ok("pong".to_string()));
        assert!(back.data.is_none());
    }

    #[test]
    fn response_data_lane_round_trips_typed_payloads() {
        let mut resp = RemoteResponse::ok("cleared".to_string());
        resp.data = Some(ResponseData::Cleared { cleared: 42 });
        let line = serde_json::to_string(&resp).unwrap();
        assert!(
            line.contains(r#""data":{"cleared":42}"#),
            "untagged variant serializes as its bare shape: {line}"
        );
        let back: RemoteResponse = serde_json::from_str(&line).unwrap();
        assert_eq!(back, resp);
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
    fn response_error_message_is_additive_and_keeps_machine_reason() {
        let response = RemoteResponse::err_with_message(
            "personal_export_failed",
            "destination is not writable".to_string(),
        );
        let line = serde_json::to_string(&response).unwrap();
        assert!(line.contains(r#""reason":"personal_export_failed""#));
        assert!(line.contains(r#""message":"destination is not writable""#));
        let back: RemoteResponse = serde_json::from_str(&line).unwrap();
        assert_eq!(back, response);
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
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
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
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
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
