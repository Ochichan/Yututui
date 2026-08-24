//! The one-shot command surface (v7-frozen) shared by `ytt -r`, the tray, and sessions.
//!
//! Byte shapes here are frozen: additive variants/fields only, guarded by the golden
//! corpus in [`super::freeze`].

use serde::{Deserialize, Serialize};

use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

use super::ToggleState;

/// Semantic cap on remote search strings. Frame caps bound bytes on the wire; this caps the
/// amount of search/provider work a syntactically valid command can request.
pub const REMOTE_MAX_QUERY_BYTES: usize = crate::util::query::MAX_SEARCH_QUERY_BYTES;
/// Queue positions are bounded by the queue cap.
pub const REMOTE_MAX_TRACK_IDS: usize = 999;
/// Export destinations travel inside the 4 KiB one-shot request frame. Keep enough headroom
/// for the request envelope, authentication token, and worst-case JSON escaping while supporting
/// long platform paths.
pub const REMOTE_MAX_EXPORT_DIRECTORY_BYTES: usize = 1536;
/// Session subscribe/unsubscribe frames should name each topic at most once.
pub const REMOTE_MAX_TOPICS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteCommandValidationError {
    reason: &'static str,
}

/// Whether a repeated stable request identity joins one retained owner outcome or safely starts a
/// fresh read-only/query execution. The exhaustive classifier makes adding a command an explicit
/// retry-semantics decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestRetryClass {
    RetainedOutcome,
    ReexecuteReadOnly,
}

impl RemoteCommandValidationError {
    pub fn reason(self) -> &'static str {
        self.reason
    }
}

/// A semantic player command. Applied through the same reducer path a keypress uses, so
/// it works regardless of the TUI's current input mode (Search text entry, Settings, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Play an order position only if it still belongs to the queue snapshot the
    /// caller rendered. Additive in v8; stale snapshots are rejected as `stale_rev`.
    QueuePlayIfRevision {
        position: usize,
        expected_rev: u64,
    },
    /// Remove an order position only if it still belongs to the queue snapshot the
    /// caller rendered. Additive in v8; stale snapshots are rejected as `stale_rev`.
    QueueRemoveIfRevision {
        position: usize,
        expected_rev: u64,
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
    /// Write a portable, credential-free snapshot to this existing absolute directory.
    /// Additive since v8 and capability-gated by `personal-export-v1`; schema 2 additionally
    /// requires `personal-state-v2`.
    ExportPersonalData {
        directory: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<u32>,
    },
    /// Run one bidirectional encrypted personal-state sync through the process which owns the
    /// writer lease. Additive since v8 and capability-gated by `webdav-sync-v1`.
    SyncNow,
    /// Revoke one explicitly selected device, rotate the encrypted vault membership, and sync
    /// the resulting state through the primary writer. The WebDAV credential never crosses IPC.
    SyncRevokeDevice {
        device_id: String,
    },
    /// Move an order position to another (queue drag-reorder). `expected_rev` guards
    /// against a stale queue snapshot like the *_if_revision commands.
    QueueMove {
        from: usize,
        to: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_rev: Option<u64>,
    },
    /// Drop everything after the current track.
    QueueClearUpcoming {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_rev: Option<u64>,
    },
    /// Arm (Some minutes, 1..=720) or cancel (None) the sleep timer. Additive (post-v8);
    /// an older owner rejects it as `bad_request` instead of misbehaving.
    Sleep {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minutes: Option<u32>,
    },
}

impl RemoteCommand {
    pub(crate) fn expected_queue_rev(&self) -> Option<u64> {
        match self {
            Self::QueuePlayIfRevision { expected_rev, .. }
            | Self::QueueRemoveIfRevision { expected_rev, .. } => Some(*expected_rev),
            // Optional guards: absent means the caller opted out of the stale check
            // (the keyboard path sends no revision; the drag path always does).
            Self::QueueMove { expected_rev, .. } | Self::QueueClearUpcoming { expected_rev } => {
                *expected_rev
            }
            _ => None,
        }
    }

    pub(crate) fn request_retry_class(&self) -> RequestRetryClass {
        match self {
            // The status read re-executes freshly on a same-ID retry — replaying a
            // retained snapshot would pin stale data.
            RemoteCommand::Status => RequestRetryClass::ReexecuteReadOnly,
            RemoteCommand::Next
            | RemoteCommand::Prev
            | RemoteCommand::TogglePause
            | RemoteCommand::Play { .. }
            | RemoteCommand::Enqueue { .. }
            | RemoteCommand::VolumeUp
            | RemoteCommand::VolumeDown
            | RemoteCommand::SetVolume { .. }
            | RemoteCommand::SeekBack
            | RemoteCommand::SeekForward
            | RemoteCommand::SeekTo { .. }
            | RemoteCommand::ToggleShuffle
            | RemoteCommand::CycleRepeat
            | RemoteCommand::QueuePlay { .. }
            | RemoteCommand::QueueRemove { .. }
            | RemoteCommand::QueuePlayIfRevision { .. }
            | RemoteCommand::QueueRemoveIfRevision { .. }
            | RemoteCommand::Streaming { .. }
            | RemoteCommand::SetSetting { .. }
            | RemoteCommand::ResumeSession
            | RemoteCommand::Quit
            | RemoteCommand::ExportPersonalData { .. }
            | RemoteCommand::SyncNow
            | RemoteCommand::SyncRevokeDevice { .. }
            | RemoteCommand::QueueMove { .. }
            | RemoteCommand::QueueClearUpcoming { .. }
            | RemoteCommand::Sleep { .. } => RequestRetryClass::RetainedOutcome,
        }
    }

    /// Whether losing the reply can leave the caller unsure whether observable state changed.
    /// The status read is excluded so a lost fetch reply surfaces as `timeout`, never as the
    /// alarming `confirmation_lost`.
    pub(crate) fn requires_confirmation(&self) -> bool {
        !matches!(self, RemoteCommand::Status)
    }

    pub fn validate(&self) -> Result<(), RemoteCommandValidationError> {
        match self {
            RemoteCommand::SetVolume { percent } if !(0..=100).contains(percent) => {
                Err(validation_error("bad_volume"))
            }
            RemoteCommand::QueuePlay { position }
            | RemoteCommand::QueueRemove { position }
            | RemoteCommand::QueuePlayIfRevision { position, .. }
            | RemoteCommand::QueueRemoveIfRevision { position, .. }
                if *position >= REMOTE_MAX_TRACK_IDS =>
            {
                Err(validation_error("bad_queue_position"))
            }
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::Speed { tenths },
            } if !(5..=20).contains(tenths) => Err(validation_error("bad_speed")),
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::SeekSeconds { seconds },
            } if !(1..=60).contains(seconds) => Err(validation_error("bad_seek_seconds")),
            RemoteCommand::Play { query } | RemoteCommand::Enqueue { query } => {
                validate_query(query)
            }
            RemoteCommand::ExportPersonalData { directory, schema } => {
                validate_export_directory(directory)?;
                if schema.is_some_and(|schema| !matches!(schema, 1 | 2)) {
                    return Err(validation_error("bad_export_schema"));
                }
                Ok(())
            }
            RemoteCommand::SyncRevokeDevice { device_id } => {
                if crate::personal_state::DeviceId::new(device_id).is_err() {
                    return Err(validation_error("bad_device_id"));
                }
                Ok(())
            }
            RemoteCommand::QueueMove { from, to, .. }
                if *from >= REMOTE_MAX_TRACK_IDS || *to >= REMOTE_MAX_TRACK_IDS =>
            {
                Err(validation_error("bad_queue_position"))
            }
            RemoteCommand::Sleep {
                minutes: Some(minutes),
            } if *minutes > yututui_core::sleep_timer::SLEEP_MAX_MINUTES => {
                Err(validation_error("bad_sleep_minutes"))
            }
            _ => Ok(()),
        }
    }
}

fn validation_error(reason: &'static str) -> RemoteCommandValidationError {
    RemoteCommandValidationError { reason }
}

fn validate_query(query: &str) -> Result<(), RemoteCommandValidationError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(validation_error("empty_query"));
    }
    if query.len() > REMOTE_MAX_QUERY_BYTES {
        return Err(validation_error("query_too_long"));
    }
    if query.chars().any(crate::util::query::forbidden_query_char) {
        return Err(validation_error("bad_request"));
    }
    Ok(())
}

fn validate_export_directory(directory: &str) -> Result<(), RemoteCommandValidationError> {
    if directory.is_empty() {
        return Err(validation_error("empty_export_directory"));
    }
    if directory.len() > REMOTE_MAX_EXPORT_DIRECTORY_BYTES {
        return Err(validation_error("export_directory_too_long"));
    }
    if directory.chars().any(forbidden_command_char) {
        return Err(validation_error("bad_export_directory"));
    }
    if !std::path::Path::new(directory).is_absolute() {
        return Err(validation_error("export_directory_not_absolute"));
    }
    Ok(())
}

fn forbidden_command_char(ch: char) -> bool {
    ch == '\0' || ch.is_control()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_class_separates_reexecuted_queries_from_retained_mutations() {
        assert_eq!(
            RemoteCommand::Status.request_retry_class(),
            RequestRetryClass::ReexecuteReadOnly
        );
        assert!(!RemoteCommand::Status.requires_confirmation());
        assert_eq!(
            RemoteCommand::TogglePause.request_retry_class(),
            RequestRetryClass::RetainedOutcome
        );
        assert!(RemoteCommand::TogglePause.requires_confirmation());
        let export = RemoteCommand::ExportPersonalData {
            directory: std::env::temp_dir().to_string_lossy().into_owned(),
            schema: None,
        };
        assert_eq!(
            export.request_retry_class(),
            RequestRetryClass::RetainedOutcome
        );
        assert!(export.requires_confirmation());
        assert_eq!(
            RemoteCommand::SyncNow.request_retry_class(),
            RequestRetryClass::RetainedOutcome
        );
        assert!(RemoteCommand::SyncNow.requires_confirmation());
        let revoke = RemoteCommand::SyncRevokeDevice {
            device_id: "device-a".to_owned(),
        };
        assert_eq!(
            revoke.request_retry_class(),
            RequestRetryClass::RetainedOutcome
        );
        assert!(revoke.requires_confirmation());
    }

    #[test]
    fn command_validation_caps_queries() {
        assert_eq!(
            RemoteCommand::Play {
                query: String::new()
            }
            .validate()
            .unwrap_err()
            .reason(),
            "empty_query"
        );
        assert_eq!(
            RemoteCommand::Play {
                query: "q".repeat(REMOTE_MAX_QUERY_BYTES + 1),
            }
            .validate()
            .unwrap_err()
            .reason(),
            "query_too_long"
        );
    }

    #[test]
    fn export_command_round_trips_and_requires_a_bounded_absolute_directory() {
        let directory = std::env::temp_dir().to_string_lossy().into_owned();
        let command = RemoteCommand::ExportPersonalData {
            directory: directory.clone(),
            schema: Some(2),
        };
        assert!(command.validate().is_ok());

        let line = serde_json::to_string(&command).unwrap();
        let back: RemoteCommand = serde_json::from_str(&line).unwrap();
        assert_eq!(back, command);
        assert!(line.contains(r#""cmd":"export_personal_data""#));

        for (directory, reason) in [
            (String::new(), "empty_export_directory"),
            ("relative/path".to_string(), "export_directory_not_absolute"),
            ("bad\npath".to_string(), "bad_export_directory"),
            (
                format!("/{}", "x".repeat(REMOTE_MAX_EXPORT_DIRECTORY_BYTES)),
                "export_directory_too_long",
            ),
        ] {
            let error = RemoteCommand::ExportPersonalData {
                directory,
                schema: None,
            }
            .validate()
            .unwrap_err();
            assert_eq!(error.reason(), reason);
        }
    }

    #[test]
    fn sync_commands_round_trip_and_validate_device_ids() {
        for (command, expected_wire) in [
            (RemoteCommand::SyncNow, r#"{"cmd":"sync_now"}"#),
            (
                RemoteCommand::SyncRevokeDevice {
                    device_id: "device-a".to_owned(),
                },
                r#"{"cmd":"sync_revoke_device","device_id":"device-a"}"#,
            ),
        ] {
            assert!(command.validate().is_ok());
            let line = serde_json::to_string(&command).unwrap();
            assert_eq!(line, expected_wire);
            let back: RemoteCommand = serde_json::from_str(&line).unwrap();
            assert_eq!(back, command);
        }
        assert_eq!(
            RemoteCommand::SyncRevokeDevice {
                device_id: "\n".to_owned(),
            }
            .validate()
            .unwrap_err()
            .reason(),
            "bad_device_id"
        );
    }

    #[test]
    fn desktop_control_ranges_are_rejected_at_the_protocol_edge() {
        for percent in [-1, 101] {
            assert_eq!(
                RemoteCommand::SetVolume { percent }
                    .validate()
                    .unwrap_err()
                    .reason(),
                "bad_volume"
            );
        }
        assert_eq!(
            RemoteCommand::QueueRemove {
                position: REMOTE_MAX_TRACK_IDS,
            }
            .validate()
            .unwrap_err()
            .reason(),
            "bad_queue_position"
        );
        assert_eq!(
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::Speed { tenths: 21 },
            }
            .validate()
            .unwrap_err()
            .reason(),
            "bad_speed"
        );
        assert_eq!(
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::SeekSeconds { seconds: 0 },
            }
            .validate()
            .unwrap_err()
            .reason(),
            "bad_seek_seconds"
        );
        for command in [
            RemoteCommand::SetVolume { percent: 100 },
            RemoteCommand::QueuePlay {
                position: REMOTE_MAX_TRACK_IDS - 1,
            },
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::Speed { tenths: 5 },
            },
            RemoteCommand::SetSetting {
                change: RemoteSettingChange::SeekSeconds { seconds: 60 },
            },
        ] {
            assert!(command.validate().is_ok());
        }
    }

    #[test]
    fn command_validation_handles_deterministic_fuzz_corpus() {
        let mut state = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..512 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let query_len = (state as usize) % (REMOTE_MAX_QUERY_BYTES + 64);
            let query = "q".repeat(query_len);
            let cmd = RemoteCommand::Play { query };
            let result = cmd.validate();
            if query_len == 0 {
                assert_eq!(result.unwrap_err().reason(), "empty_query");
            } else if query_len > REMOTE_MAX_QUERY_BYTES {
                assert_eq!(result.unwrap_err().reason(), "query_too_long");
            } else {
                assert!(result.is_ok());
            }
        }
    }
}
