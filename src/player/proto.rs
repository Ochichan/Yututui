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
    PropertyChange { name: String, value: Value },
    /// Playback of the current file ended; `reason` is mpv's `end-file` reason
    /// (`eof`, `stop`, `error`, `quit`, ...). `file_error` is mpv's error detail, present
    /// only when `reason` is `error` (e.g. "Failed to open", "Unrecognized file format").
    EndFile {
        reason: String,
        file_error: Option<String>,
    },
    /// A command reply or an event we don't act on.
    Other,
}

/// Parse one line of mpv IPC output. Returns `None` for blank/garbage lines.
pub fn parse_line(line: &str) -> Option<MpvIncoming> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    match v.get("event").and_then(Value::as_str) {
        Some("property-change") => {
            let name = v.get("name")?.as_str()?.to_owned();
            let value = v.get("data").cloned().unwrap_or(Value::Null);
            Some(MpvIncoming::PropertyChange { name, value })
        }
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
            Some(MpvIncoming::EndFile { reason, file_error })
        }
        _ => Some(MpvIncoming::Other),
    }
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

/// `cycle <property>` — e.g. toggle `pause`.
pub fn cmd_cycle(property: &str, request_id: u64) -> String {
    json!({ "command": ["cycle", property], "request_id": request_id }).to_string()
}

/// `seek <seconds> relative` — jump forward (positive) or back (negative).
pub fn cmd_seek_relative(seconds: f64, request_id: u64) -> String {
    json!({ "command": ["seek", seconds, "relative"], "request_id": request_id }).to_string()
}

/// `seek <seconds> absolute` — jump to an absolute position (click-to-seek).
pub fn cmd_seek_absolute(seconds: f64, request_id: u64) -> String {
    json!({ "command": ["seek", seconds, "absolute"], "request_id": request_id }).to_string()
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
            Some(MpvIncoming::PropertyChange { name, value }) => {
                assert_eq!(name, "time-pos");
                assert_eq!(value.as_f64(), Some(12.5));
            }
            _ => panic!("expected property-change"),
        }
    }

    #[test]
    fn parses_end_file() {
        let line = r#"{"event":"end-file","reason":"eof"}"#;
        match parse_line(line) {
            Some(MpvIncoming::EndFile { reason, file_error }) => {
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
            Some(MpvIncoming::EndFile { reason, file_error }) => {
                assert_eq!(reason, "error");
                assert_eq!(file_error.as_deref(), Some("Failed to open"));
            }
            _ => panic!("expected end-file"),
        }
    }

    #[test]
    fn command_reply_is_other() {
        let line = r#"{"error":"success","request_id":11}"#;
        assert!(matches!(parse_line(line), Some(MpvIncoming::Other)));
    }

    #[test]
    fn blank_line_is_none() {
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn builds_seek_absolute() {
        let s = cmd_seek_absolute(42.5, 12);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["command"][0], "seek");
        assert_eq!(v["command"][1], 42.5);
        assert_eq!(v["command"][2], "absolute");
        assert_eq!(v["request_id"], 12);
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
