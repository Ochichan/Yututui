//! mpv JSON IPC wire format.
//!
//! Commands are newline-delimited JSON objects `{"command":[...],"request_id":N}`.
//! mpv answers each with `{"error":"success","request_id":N,...}` and emits
//! unsolicited event objects `{"event":"..."}`. We only model the few messages the
//! player actor reacts to; everything else collapses to [`MpvIncoming::Other`].

use serde_json::{Value, json};

/// A message read from mpv that the player actor cares about.
pub enum MpvIncoming {
    /// An observed property changed (e.g. `time-pos`, `duration`, `pause`).
    PropertyChange {
        id: Option<u64>,
        name: String,
        value: Value,
    },
    /// mpv began a new playlist entry. Audio IPC uses this ordering boundary to activate the
    /// generation assigned to the corresponding accepted `loadfile` command.
    StartFile { playlist_entry_id: Option<u64> },
    /// The current file completed loading. Deferred recovery seek/pause commands may start only
    /// after this boundary.
    FileLoaded { playlist_entry_id: Option<u64> },
    /// Playback of the current file ended; `reason` is mpv's `end-file` reason
    /// (`eof`, `stop`, `error`, `quit`, ...). `file_error` is mpv's error detail, present
    /// only when `reason` is `error` (e.g. "Failed to open", "Unrecognized file format").
    EndFile {
        reason: String,
        file_error: Option<String>,
        playlist_entry_id: Option<u64>,
        playlist_insert_id: Option<u64>,
        playlist_insert_num_entries: Option<u64>,
    },
    /// mpv entered its explicit `--idle` loop with no file loaded. Unlike the observed
    /// `idle-active` property, this lifecycle event cannot collapse across a fast failed load.
    Idle,
    /// mpv resumed decoding/output after a seek or load interruption.
    PlaybackRestart,
    /// A `script-message …` fired inside mpv (e.g. by a rebound key in the video
    /// overlay); `args` are the message name and its arguments.
    ClientMessage { args: Vec<String> },
    /// mpv's reply to one of our commands (`{"error":"success","request_id":N,...}`).
    CommandReply {
        request_id: u64,
        error: String,
        data: Option<Value>,
    },
    /// An event we don't act on.
    Other,
}

/// Parse one line of mpv IPC output. Returns `None` for blank/garbage lines.
pub fn parse_line(line: &str) -> Option<MpvIncoming> {
    let mut v: Value = serde_json::from_str(line.trim()).ok()?;
    let event = v.get("event").and_then(Value::as_str).map(str::to_owned);
    match event.as_deref() {
        Some("property-change") => {
            let id = v.get("id").and_then(Value::as_u64);
            let name = v.get("name")?.as_str()?.to_owned();
            let value = v
                .as_object_mut()
                .and_then(|object| object.remove("data"))
                .unwrap_or(Value::Null);
            Some(MpvIncoming::PropertyChange { id, name, value })
        }
        Some("start-file") => Some(MpvIncoming::StartFile {
            playlist_entry_id: v.get("playlist_entry_id").and_then(Value::as_u64),
        }),
        Some("file-loaded") => Some(MpvIncoming::FileLoaded {
            playlist_entry_id: v.get("playlist_entry_id").and_then(Value::as_u64),
        }),
        Some("end-file") => {
            let reason = v
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let file_error = v
                .get("file_error")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Some(MpvIncoming::EndFile {
                reason,
                file_error,
                playlist_entry_id: v.get("playlist_entry_id").and_then(Value::as_u64),
                playlist_insert_id: v.get("playlist_insert_id").and_then(Value::as_u64),
                playlist_insert_num_entries: v
                    .get("playlist_insert_num_entries")
                    .and_then(Value::as_u64),
            })
        }
        Some("idle") => Some(MpvIncoming::Idle),
        Some("playback-restart") => Some(MpvIncoming::PlaybackRestart),
        Some("client-message") => {
            let args = v
                .get("args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            Some(MpvIncoming::ClientMessage { args })
        }
        _ => {
            if v.get("event").is_none()
                && let Some(request_id) = v.get("request_id").and_then(Value::as_u64)
            {
                let error = v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                // Move potentially large demuxer-cache-state payloads out of the parsed
                // object. Cloning here used to duplicate every seekable-ranges entry on the
                // watchdog path before the pending request could decide how much it needed.
                let data = v.as_object_mut().and_then(|object| object.remove("data"));
                Some(MpvIncoming::CommandReply {
                    request_id,
                    error,
                    data,
                })
            } else {
                Some(MpvIncoming::Other)
            }
        }
    }
}

/// `keybind <key> <command>` — (re)bind a key inside mpv at runtime. Used by the video
/// overlay to route its playlist-next/prev keys to the app queue via `script-message`.
pub fn cmd_keybind(key: &str, command: &str, request_id: u64) -> String {
    json!({ "command": ["keybind", key, command], "request_id": request_id }).to_string()
}

/// `loadfile <url> <mode>` — `mode` is `replace` or `append`.
pub fn cmd_loadfile(url: &str, mode: &str, request_id: u64) -> String {
    json!({ "command": ["loadfile", url, mode], "request_id": request_id }).to_string()
}

/// `stop` — stop the current file.
pub fn cmd_stop(request_id: u64) -> String {
    json!({ "command": ["stop"], "request_id": request_id }).to_string()
}

/// `observe_property <id> <name>` — subscribe to property-change events.
pub fn cmd_observe(id: u64, name: &str) -> String {
    json!({ "command": ["observe_property", id, name], "request_id": id }).to_string()
}

/// `get_property <name>` — retained protocol robustness for legacy mpv 0.32, whose successful
/// `loadfile` reply predates the direct `playlist_entry_id` result.
pub fn cmd_get_property(name: &str, request_id: u64) -> String {
    json!({ "command": ["get_property", name], "request_id": request_id }).to_string()
}

/// `cycle <property>` — e.g. toggle `pause`.
pub fn cmd_cycle(property: &str, request_id: u64) -> String {
    json!({ "command": ["cycle", property], "request_id": request_id }).to_string()
}

/// `seek <seconds> relative` — jump forward (positive) or back (negative).
pub fn cmd_seek_relative(seconds: f64, request_id: u64) -> String {
    json!({ "command": ["seek", seconds, "relative"], "request_id": request_id }).to_string()
}

/// `seek <seconds> <mode>` — interaction intent stays typed even while the wire fails closed.
///
/// The long-form plan permits `absolute+keyframes` only after the compressed-fixture landing
/// accuracy gate passes. That evidence is not ship-eligible yet, so both intents currently use
/// the release-only exact fallback. Keeping the intent here lets the actor preserve latest-wins
/// semantics without silently promoting the unverified keyframe wire.
pub fn cmd_seek_absolute(seconds: f64, precision: super::SeekPrecision, request_id: u64) -> String {
    let mode = match precision {
        super::SeekPrecision::InteractiveFast | super::SeekPrecision::Exact => "absolute",
    };
    json!({ "command": ["seek", seconds, mode], "request_id": request_id }).to_string()
}

/// `set_property volume <0-100>`.
pub fn cmd_set_volume(volume: i64, request_id: u64) -> String {
    json!({ "command": ["set_property", "volume", volume], "request_id": request_id }).to_string()
}

/// `set_property <name> <value>` — generic; `value` is any JSON scalar. Drives the EQ
/// (`af` ← a filter-chain string) and playback `speed` (a number).
pub fn cmd_set_property(name: &str, value: &Value, request_id: u64) -> String {
    json!({ "command": ["set_property", name, value], "request_id": request_id }).to_string()
}

/// `af-command <label> <command> <arg>` — send a command to one labeled filter (e.g.
/// nudge `@eqN`'s `gain`) without rebuilding the whole `af` chain, so a live slider
/// edit doesn't click. The label is the bare name (no `@`), matching mpv's convention.
pub fn cmd_af_command(label: &str, command: &str, arg: &str, request_id: u64) -> String {
    json!({ "command": ["af-command", label, command, arg], "request_id": request_id }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_property_change() {
        let line = r#"{"event":"property-change","id":1,"name":"time-pos","data":12.5}"#;
        match parse_line(line) {
            Some(MpvIncoming::PropertyChange { id, name, value }) => {
                assert_eq!(id, Some(1));
                assert_eq!(name, "time-pos");
                assert_eq!(value.as_f64(), Some(12.5));
            }
            _ => panic!("expected property-change"),
        }
    }

    #[test]
    fn parses_start_file() {
        assert!(matches!(
            parse_line(r#"{"event":"start-file","playlist_entry_id":17}"#),
            Some(MpvIncoming::StartFile {
                playlist_entry_id: Some(17)
            })
        ));
    }

    #[test]
    fn parses_file_loaded() {
        assert!(matches!(
            parse_line(r#"{"event":"file-loaded","playlist_entry_id":17}"#),
            Some(MpvIncoming::FileLoaded {
                playlist_entry_id: Some(17)
            })
        ));
    }

    #[test]
    fn parses_end_file() {
        let line = r#"{"event":"end-file","reason":"eof"}"#;
        match parse_line(line) {
            Some(MpvIncoming::EndFile {
                reason, file_error, ..
            }) => {
                assert_eq!(reason, "eof");
                assert!(file_error.is_none());
            }
            _ => panic!("expected end-file"),
        }
    }

    #[test]
    fn parses_end_file_error_with_detail() {
        let line = r#"{"event":"end-file","reason":"error","file_error":"Failed to open"}"#;
        match parse_line(line) {
            Some(MpvIncoming::EndFile {
                reason, file_error, ..
            }) => {
                assert_eq!(reason, "error");
                assert_eq!(file_error.as_deref(), Some("Failed to open"));
            }
            _ => panic!("expected end-file"),
        }
    }

    #[test]
    fn parses_explicit_idle_lifecycle_event() {
        assert!(matches!(
            parse_line(r#"{"event":"idle"}"#),
            Some(MpvIncoming::Idle)
        ));
    }

    #[test]
    fn command_reply_parses() {
        let line = r#"{"error":"success","request_id":11}"#;
        match parse_line(line) {
            Some(MpvIncoming::CommandReply {
                request_id, error, ..
            }) => {
                assert_eq!(request_id, 11);
                assert_eq!(error, "success");
            }
            _ => panic!("expected command reply"),
        }
    }

    #[test]
    fn command_reply_preserves_nested_cache_state_data() {
        let line = r#"{"error":"success","request_id":13,"data":{"raw-input-rate":42,"seekable-ranges":[{"start":0,"end":1},{"start":2,"end":3}]}}"#;
        match parse_line(line) {
            Some(MpvIncoming::CommandReply {
                request_id,
                data: Some(data),
                ..
            }) => {
                assert_eq!(request_id, 13);
                assert_eq!(data["raw-input-rate"], 42);
                assert_eq!(data["seekable-ranges"].as_array().map(Vec::len), Some(2));
            }
            _ => panic!("expected command reply with nested data"),
        }
    }

    #[test]
    fn failed_command_reply_parses() {
        let line = r#"{"error":"invalid parameter","request_id":12}"#;
        match parse_line(line) {
            Some(MpvIncoming::CommandReply {
                request_id, error, ..
            }) => {
                assert_eq!(request_id, 12);
                assert_eq!(error, "invalid parameter");
            }
            _ => panic!("expected command reply"),
        }
    }

    #[test]
    fn parses_playback_restart() {
        assert!(matches!(
            parse_line(r#"{"event":"playback-restart"}"#),
            Some(MpvIncoming::PlaybackRestart)
        ));
    }

    #[test]
    fn blank_line_is_none() {
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn builds_seek_absolute() {
        let s = cmd_seek_absolute(42.5, super::super::SeekPrecision::Exact, 12);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["command"][0], "seek");
        assert_eq!(v["command"][1], 42.5);
        assert_eq!(v["command"][2], "absolute");
        assert_eq!(v["request_id"], 12);
    }

    #[test]
    fn interactive_seek_fails_closed_to_exact_until_accuracy_is_ship_eligible() {
        let s = cmd_seek_absolute(42.5, super::super::SeekPrecision::InteractiveFast, 13);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["command"], json!(["seek", 42.5, "absolute"]));
        assert_eq!(v["request_id"], 13);
    }

    #[test]
    fn builds_set_property_string_and_number() {
        let af = cmd_set_property("af", &Value::from("dynaudnorm"), 20);
        let v: Value = serde_json::from_str(&af).unwrap();
        assert_eq!(v["command"][0], "set_property");
        assert_eq!(v["command"][1], "af");
        assert_eq!(v["command"][2], "dynaudnorm");
        assert_eq!(v["request_id"], 20);

        let speed = cmd_set_property("speed", &Value::from(1.25), 21);
        let v: Value = serde_json::from_str(&speed).unwrap();
        assert_eq!(v["command"][2], 1.25);
    }

    #[test]
    fn builds_af_command() {
        let s = cmd_af_command("eq3", "gain", "5", 22);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["command"][0], "af-command");
        assert_eq!(v["command"][1], "eq3");
        assert_eq!(v["command"][2], "gain");
        assert_eq!(v["command"][3], "5");
        assert_eq!(v["request_id"], 22);
    }

    #[test]
    fn builds_loadfile() {
        let s = cmd_loadfile("http://x/a.mp3", "replace", 11);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["command"][0], "loadfile");
        assert_eq!(v["command"][1], "http://x/a.mp3");
        assert_eq!(v["command"][2], "replace");
        assert_eq!(v["request_id"], 11);
    }
}
