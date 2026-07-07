//! The one-shot command surface (v7-frozen) shared by `ytt -r`, the tray, and sessions.
//!
//! Byte shapes here are frozen: additive variants/fields only, guarded by the golden
//! corpus in [`super::freeze`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

use super::ToggleState;

/// Semantic cap on remote search strings. Frame caps bound bytes on the wire; this caps the
/// amount of search/provider work a syntactically valid command can request.
pub const REMOTE_MAX_QUERY_BYTES: usize = 2048;
/// Track ids in GUI commands are rows from prior search/library snapshots. Match the queue cap.
pub const REMOTE_MAX_TRACK_IDS: usize = 999;
/// A single row id should be tiny; this covers YouTube IDs and source-prefixed provider IDs.
pub const REMOTE_MAX_TRACK_ID_BYTES: usize = 256;
/// Gemini keys are normally well below this; reject large pasted blobs at the protocol edge.
pub const REMOTE_MAX_GEMINI_KEY_BYTES: usize = 256;
/// GUI setting group/field identifiers are short ASCII tokens.
pub const REMOTE_MAX_SETTING_NAME_BYTES: usize = 64;
/// Paths and provider app ids may be user-entered, but should never be frame-sized blobs.
pub const REMOTE_MAX_SETTING_STRING_BYTES: usize = 4096;
/// Session subscribe/unsubscribe frames should name each topic at most once.
pub const REMOTE_MAX_TOPICS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteCommandValidationError {
    reason: &'static str,
}

impl RemoteCommandValidationError {
    pub fn reason(self) -> &'static str {
        self.reason
    }
}

/// A semantic player command. Applied through the same reducer path a keypress uses, so
/// it works regardless of the TUI's current input mode (Search text entry, Settings, …).
///
/// `Eq` is deliberately absent: [`GuiSettingChange`] carries a free-form JSON value
/// (floats included), which only supports `PartialEq`.
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
    /// GUI settings mutation (v8 sessions): one `group.field = value` edit. Every
    /// accepted apply is followed by a `settings_snapshot` push carrying the new state
    /// (the GUI's optimistic pending overlay clears against it).
    Apply {
        change: GuiSettingChange,
    },
    /// Store (or clear, when empty) the Gemini API key. Write-only: snapshots carry
    /// only `has_gemini_key`.
    SetGeminiKey {
        key: String,
    },
    /// Danger zone: reset the whole config to defaults (the GUI double-confirms).
    ResetAllSettings,
}

impl RemoteCommand {
    pub fn validate(&self) -> Result<(), RemoteCommandValidationError> {
        match self {
            RemoteCommand::Play { query }
            | RemoteCommand::Enqueue { query }
            | RemoteCommand::RunSearch { query, .. } => validate_query(query),
            RemoteCommand::PlayTracks { video_ids }
            | RemoteCommand::EnqueueTracks { video_ids } => validate_track_ids(video_ids),
            RemoteCommand::Apply { change } => validate_gui_setting_change(change),
            RemoteCommand::SetGeminiKey { key } => validate_gemini_key(key),
            _ => Ok(()),
        }
    }
}

/// One GUI settings edit: `group` and `field` name a [`super::SettingsModelV8`] slot;
/// `value` is the raw JSON the frontend sent (validated by the owner per field).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuiSettingChange {
    pub group: String,
    pub field: String,
    pub value: serde_json::Value,
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
    if query.chars().any(forbidden_command_char) {
        return Err(validation_error("bad_request"));
    }
    Ok(())
}

fn validate_track_ids(video_ids: &[String]) -> Result<(), RemoteCommandValidationError> {
    if video_ids.is_empty() {
        return Err(validation_error("empty_selection"));
    }
    if video_ids.len() > REMOTE_MAX_TRACK_IDS {
        return Err(validation_error("too_many_tracks"));
    }
    for id in video_ids {
        let id = id.trim();
        if id.is_empty()
            || id.len() > REMOTE_MAX_TRACK_ID_BYTES
            || id.chars().any(forbidden_command_char)
        {
            return Err(validation_error("bad_track_id"));
        }
    }
    Ok(())
}

fn validate_gemini_key(key: &str) -> Result<(), RemoteCommandValidationError> {
    let key = key.trim();
    if key.len() > REMOTE_MAX_GEMINI_KEY_BYTES {
        return Err(validation_error("key_too_long"));
    }
    if key.chars().any(forbidden_command_char) {
        return Err(validation_error("bad_request"));
    }
    Ok(())
}

fn validate_gui_setting_change(
    change: &GuiSettingChange,
) -> Result<(), RemoteCommandValidationError> {
    if !valid_setting_token(&change.group) || !valid_setting_token(&change.field) {
        return Err(validation_error("bad_setting"));
    }
    if !known_setting_group(&change.group) {
        return Err(validation_error("unknown_setting"));
    }
    validate_setting_value(&change.group, &change.field, &change.value)
}

fn known_setting_group(group: &str) -> bool {
    matches!(
        group,
        "playback" | "eq" | "streaming" | "search" | "ui" | "storage" | "animations" | "theme"
    )
}

fn valid_setting_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= REMOTE_MAX_SETTING_NAME_BYTES
        && token
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn validate_setting_value(
    group: &str,
    field: &str,
    value: &Value,
) -> Result<(), RemoteCommandValidationError> {
    match value {
        Value::Null if nullable_setting(group, field) => Ok(()),
        Value::Null => Err(validation_error("bad_setting_value")),
        Value::Bool(_) => Ok(()),
        Value::Number(n) if n.as_f64().is_some_and(f64::is_finite) => Ok(()),
        Value::Number(_) => Err(validation_error("bad_setting_value")),
        Value::String(s) => validate_setting_string(s),
        Value::Array(values) if group == "eq" && field == "bands" => {
            if values.len() != 10 {
                return Err(validation_error("bad_setting_value"));
            }
            for value in values {
                let Value::Number(n) = value else {
                    return Err(validation_error("bad_setting_value"));
                };
                if !n.as_f64().is_some_and(f64::is_finite) {
                    return Err(validation_error("bad_setting_value"));
                }
            }
            Ok(())
        }
        Value::Array(_) | Value::Object(_) => Err(validation_error("bad_setting_value")),
    }
}

fn nullable_setting(group: &str, field: &str) -> bool {
    matches!(
        (group, field),
        ("search", "audius_app_name")
            | ("search", "jamendo_client_id")
            | ("storage", "download_dir")
            | ("storage", "cookies_file")
    )
}

fn validate_setting_string(raw: &str) -> Result<(), RemoteCommandValidationError> {
    if raw.len() > REMOTE_MAX_SETTING_STRING_BYTES {
        return Err(validation_error("bad_setting_value"));
    }
    if raw.chars().any(forbidden_command_char) {
        return Err(validation_error("bad_setting_value"));
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
    fn command_validation_caps_queries_keys_and_track_lists() {
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
            RemoteCommand::RunSearch {
                ticket: 1,
                query: "q".repeat(REMOTE_MAX_QUERY_BYTES + 1),
                source: SearchSource::Youtube,
            }
            .validate()
            .unwrap_err()
            .reason(),
            "query_too_long"
        );
        assert_eq!(
            RemoteCommand::SetGeminiKey {
                key: "k".repeat(REMOTE_MAX_GEMINI_KEY_BYTES + 1)
            }
            .validate()
            .unwrap_err()
            .reason(),
            "key_too_long"
        );
        assert_eq!(
            RemoteCommand::PlayTracks {
                video_ids: vec!["id".to_string(); REMOTE_MAX_TRACK_IDS + 1]
            }
            .validate()
            .unwrap_err()
            .reason(),
            "too_many_tracks"
        );
    }

    #[test]
    fn command_validation_rejects_structured_setting_values() {
        let bad = RemoteCommand::Apply {
            change: GuiSettingChange {
                group: "search".to_string(),
                field: "default_source".to_string(),
                value: serde_json::json!({"nested": "object"}),
            },
        };
        assert_eq!(bad.validate().unwrap_err().reason(), "bad_setting_value");

        let good_null = RemoteCommand::Apply {
            change: GuiSettingChange {
                group: "storage".to_string(),
                field: "download_dir".to_string(),
                value: Value::Null,
            },
        };
        assert!(good_null.validate().is_ok());

        let bad_array = RemoteCommand::Apply {
            change: GuiSettingChange {
                group: "eq".to_string(),
                field: "bands".to_string(),
                value: serde_json::json!([0, 0, 0]),
            },
        };
        assert_eq!(
            bad_array.validate().unwrap_err().reason(),
            "bad_setting_value"
        );
    }

    #[test]
    fn command_validation_handles_deterministic_fuzz_corpus() {
        let mut state = 0x243f_6a88_85a3_08d3u64;
        let fields = [
            ("player", "volume"),
            ("streaming", "enabled"),
            ("storage", "download_dir"),
            ("eq", "bands"),
            ("unknown", "field"),
        ];

        for _ in 0..512 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let query_len = (state as usize) % (REMOTE_MAX_QUERY_BYTES + 64);
            let query = "q".repeat(query_len);
            let cmd = RemoteCommand::RunSearch {
                ticket: state,
                query,
                source: SearchSource::Youtube,
            };
            let result = cmd.validate();
            if query_len == 0 {
                assert_eq!(result.unwrap_err().reason(), "empty_query");
            } else if query_len > REMOTE_MAX_QUERY_BYTES {
                assert_eq!(result.unwrap_err().reason(), "query_too_long");
            } else {
                assert!(result.is_ok());
            }

            let count = (state.rotate_left(7) as usize) % (REMOTE_MAX_TRACK_IDS + 8);
            let ids: Vec<String> = (0..count)
                .map(|idx| {
                    if idx % 13 == 0 {
                        "x".repeat(REMOTE_MAX_TRACK_ID_BYTES + 1)
                    } else {
                        format!("id{idx}")
                    }
                })
                .collect();
            let result = RemoteCommand::PlayTracks { video_ids: ids }.validate();
            if count > REMOTE_MAX_TRACK_IDS {
                assert_eq!(result.unwrap_err().reason(), "too_many_tracks");
            }

            let (group, field) = fields[(state as usize) % fields.len()];
            let value = match state % 6 {
                0 => Value::Bool(state & 1 == 0),
                1 => Value::from((state % 100) as i64),
                2 => Value::from(
                    "x".repeat((state as usize) % (REMOTE_MAX_SETTING_STRING_BYTES + 32)),
                ),
                3 => Value::Null,
                4 => serde_json::json!({"nested": state}),
                _ => serde_json::json!([state, state + 1]),
            };
            let _ = RemoteCommand::Apply {
                change: GuiSettingChange {
                    group: group.to_string(),
                    field: field.to_string(),
                    value,
                },
            }
            .validate();
        }
    }
}
