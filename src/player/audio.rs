//! Normalization for mpv's cross-platform audio-device properties.

use std::collections::HashSet;

use serde_json::Value;

/// A local output endpoint reported by mpv's `audio-device-list` property.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDevice {
    /// Stable mpv identifier, including its output-driver prefix (for example `wasapi/...`).
    pub name: String,
    /// Human-readable label supplied by the operating-system audio backend.
    pub description: String,
}

pub(super) const MAX_AUDIO_DEVICES: usize = 256;
const MAX_AUDIO_DEVICE_NAME_BYTES: usize = 512;
const MAX_AUDIO_DEVICE_DESCRIPTION_BYTES: usize = 512;

pub(super) fn parse_audio_device_list(value: &Value) -> Vec<AudioDevice> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };
    let mut seen = HashSet::with_capacity(entries.len().min(MAX_AUDIO_DEVICES));
    entries
        .iter()
        .filter_map(parse_audio_device)
        .filter(|device| seen.insert(device.name.clone()))
        .take(MAX_AUDIO_DEVICES)
        .collect()
}

pub(super) fn normalize_selected_audio_device(value: &Value) -> Option<String> {
    let name = value.as_str()?;
    normalize_audio_device_request(Some(name.to_owned())).ok()?
}

pub(super) fn normalize_current_audio_output(value: &Value) -> Option<String> {
    let output = value.as_str()?.trim();
    valid_audio_text(output, MAX_AUDIO_DEVICE_NAME_BYTES).then(|| output.to_owned())
}

pub(super) fn normalize_audio_device_request(
    device: Option<String>,
) -> Result<Option<String>, &'static str> {
    let Some(device) = device else {
        return Ok(None);
    };
    let device = device.trim();
    if device.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    if device.is_empty() {
        return Err("audio device ID is empty");
    }
    if device.len() > MAX_AUDIO_DEVICE_NAME_BYTES {
        return Err("audio device ID is too long");
    }
    if contains_unsafe_terminal_text(device) {
        return Err("audio device ID contains unsupported control characters");
    }
    Ok(Some(device.to_owned()))
}

pub(super) fn sanitize_audio_error_text(value: impl AsRef<str>) -> String {
    sanitize_description(&crate::util::sanitize::sanitize_error_text(value))
}

fn parse_audio_device(value: &Value) -> Option<AudioDevice> {
    let name = value.get("name")?.as_str()?.trim();
    if !valid_audio_text(name, MAX_AUDIO_DEVICE_NAME_BYTES) {
        return None;
    }
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(sanitize_description)
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| name.to_owned());
    Some(AudioDevice {
        name: name.to_owned(),
        description,
    })
}

fn valid_audio_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !contains_unsafe_terminal_text(value)
}

fn sanitize_description(value: &str) -> String {
    let mut out = String::with_capacity(value.len().min(MAX_AUDIO_DEVICE_DESCRIPTION_BYTES));
    let mut separated = false;
    for ch in value.trim().chars() {
        if ch.is_whitespace() || ch.is_control() || is_invisible_control(ch) {
            separated = !out.is_empty();
            continue;
        }
        if separated {
            out.push(' ');
            separated = false;
        }
        if out.len() + ch.len_utf8() > MAX_AUDIO_DEVICE_DESCRIPTION_BYTES {
            break;
        }
        out.push(ch);
    }
    out
}

fn contains_unsafe_terminal_text(value: &str) -> bool {
    value
        .chars()
        .any(|ch| ch.is_control() || is_invisible_control(ch))
}

fn is_invisible_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_deduplicates_and_sanitizes_device_entries() {
        let devices = parse_audio_device_list(&json!([
            {"name":"auto", "description":"Autoselect device"},
            {"name":"wasapi/{speaker}", "description":"  Speakers\n  (USB)  "},
            {"name":"wasapi/{speaker}", "description":"duplicate"},
            {"name":"coreaudio/output", "description":"Built-in\u{202e} Output"},
            {"name":"bad\u{1b}[2J", "description":"unsafe ID"},
            {"description":"missing name"}
        ]));

        assert_eq!(
            devices,
            vec![
                AudioDevice {
                    name: "auto".to_owned(),
                    description: "Autoselect device".to_owned(),
                },
                AudioDevice {
                    name: "wasapi/{speaker}".to_owned(),
                    description: "Speakers (USB)".to_owned(),
                },
                AudioDevice {
                    name: "coreaudio/output".to_owned(),
                    description: "Built-in Output".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn device_list_is_bounded_and_falls_back_to_safe_name() {
        let entries = (0..MAX_AUDIO_DEVICES + 8)
            .map(|index| json!({"name": format!("pipewire/{index}")}))
            .collect::<Vec<_>>();
        let devices = parse_audio_device_list(&Value::Array(entries));

        assert_eq!(devices.len(), MAX_AUDIO_DEVICES);
        assert_eq!(devices[0].description, "pipewire/0");
        assert_eq!(
            devices.last().map(|device| device.name.as_str()),
            Some("pipewire/255")
        );
    }

    #[test]
    fn selection_normalizes_auto_and_rejects_unsafe_ids() {
        assert_eq!(normalize_audio_device_request(None), Ok(None));
        assert_eq!(
            normalize_audio_device_request(Some(" auto ".to_owned())),
            Ok(None)
        );
        assert_eq!(
            normalize_audio_device_request(Some(" alsa/default ".to_owned())),
            Ok(Some("alsa/default".to_owned()))
        );
        assert!(normalize_audio_device_request(Some("bad\nname".to_owned())).is_err());
        assert!(normalize_audio_device_request(Some("\u{202e}bad".to_owned())).is_err());
        assert!(normalize_audio_device_request(Some("가".repeat(171))).is_err());
    }

    #[test]
    fn audio_errors_are_redacted_and_terminal_safe() {
        let error = sanitize_audio_error_text("access_token=secret\u{1b}[2J failed");
        assert!(error.contains("<redacted>"));
        assert!(!error.contains("secret"));
        assert!(!error.contains('\u{1b}'));
    }
}
