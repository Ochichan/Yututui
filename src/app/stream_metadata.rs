use serde_json::Value;

use super::StreamNowPlaying;

pub(in crate::app) fn parse_stream_now_playing(
    metadata: &Value,
    reject_labels: &[&str],
) -> Option<StreamNowPlaying> {
    let object = metadata.as_object()?;

    if let Some(raw) = first_value(
        object,
        &["icytitle", "streamtitle", "nowplaying", "songtitle"],
    )
    .and_then(clean_stream_title)
    .filter(|s| usable_now_playing(s, reject_labels))
    {
        return Some(split_now_playing(&raw));
    }

    let title = first_value(object, &["title", "icytitle", "songtitle"])
        .and_then(clean_stream_title)
        .filter(|s| usable_now_playing(s, reject_labels));
    let artist = first_value(object, &["artist", "icyartist"])
        .and_then(clean_stream_title)
        .filter(|s| usable_now_playing(s, reject_labels));

    match (title, artist) {
        (Some(title), Some(artist)) => Some(StreamNowPlaying {
            raw: format!("{title} — {artist}"),
            title: Some(title),
            artist: Some(artist),
        }),
        (Some(title), None) => Some(split_now_playing(&title)),
        _ => None,
    }
}

fn first_value(
    object: &serde_json::Map<String, Value>,
    normalized_keys: &[&str],
) -> Option<String> {
    normalized_keys.iter().find_map(|want| {
        object.iter().find_map(|(key, value)| {
            (normalize_key(key) == *want)
                .then(|| value_to_string(value))
                .flatten()
        })
    })
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn clean_stream_title(raw: String) -> Option<String> {
    let mut s = raw.replace('\0', " ");
    s = collapse_spaces(&s);
    s = strip_wrapping_quotes(s.trim()).to_owned();

    let lower = s.to_ascii_lowercase();
    if lower.starts_with("streamtitle=")
        && let Some((_, rest)) = s.split_once('=')
    {
        s = rest.split(';').next().unwrap_or(rest).trim().to_owned();
        s = strip_wrapping_quotes(s.trim()).to_owned();
    }

    let s = collapse_spaces(&s);
    (!s.is_empty()).then_some(s)
}

fn split_now_playing(raw: &str) -> StreamNowPlaying {
    for delimiter in [" - ", " – ", " — "] {
        if let Some((artist, title)) = raw.split_once(delimiter) {
            let artist = artist.trim();
            let title = title.trim();
            if !artist.is_empty() && !title.is_empty() {
                return StreamNowPlaying {
                    title: Some(title.to_owned()),
                    artist: Some(artist.to_owned()),
                    raw: raw.to_owned(),
                };
            }
        }
    }
    StreamNowPlaying {
        title: Some(raw.to_owned()),
        artist: None,
        raw: raw.to_owned(),
    }
}

fn usable_now_playing(s: &str, reject_labels: &[&str]) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.len() > 300 {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "unknown" | "unknown title" | "unknown artist" | "n/a" | "na" | "none" | "-"
    ) {
        return false;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return false;
    }
    let comparable = normalize_compare(trimmed);
    !reject_labels
        .iter()
        .map(|label| normalize_compare(label))
        .any(|label| !label.is_empty() && label == comparable)
}

fn normalize_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whitespace-collapsed, ASCII-lowercased comparison form. Preserves non-ASCII (CJK
/// titles must stay distinct), so it also keys the now-playing identify cache.
pub(in crate::app) fn normalize_compare(s: &str) -> String {
    collapse_spaces(s).to_ascii_lowercase()
}

fn collapse_spaces(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_wrapping_quotes(s: &str) -> &str {
    s.strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| s.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_icy_artist_dash_title() {
        let meta = json!({ "icy-title": "Artist - Song" });
        let parsed = parse_stream_now_playing(&meta, &[]).expect("metadata parsed");

        assert_eq!(parsed.title.as_deref(), Some("Song"));
        assert_eq!(parsed.artist.as_deref(), Some("Artist"));
        assert_eq!(parsed.label(), "Song — Artist");
    }

    #[test]
    fn unwraps_streamtitle_assignment() {
        let meta = json!({ "StreamTitle": "StreamTitle='The Artist - The Track';" });
        let parsed = parse_stream_now_playing(&meta, &[]).expect("metadata parsed");

        assert_eq!(parsed.label(), "The Track — The Artist");
    }

    #[test]
    fn unwraps_streamtitle_from_icy_metadata_block() {
        let meta = json!({ "StreamTitle": "StreamTitle='The Artist - The Track';StreamUrl='';" });
        let parsed = parse_stream_now_playing(&meta, &[]).expect("metadata parsed");

        assert_eq!(parsed.label(), "The Track — The Artist");
    }

    #[test]
    fn combines_separate_title_and_artist() {
        let meta = json!({ "title": "Track", "artist": "Artist" });
        let parsed = parse_stream_now_playing(&meta, &[]).expect("metadata parsed");

        assert_eq!(parsed.label(), "Track — Artist");
    }

    #[test]
    fn rejects_station_names_and_empty_values() {
        let meta = json!({ "icy-title": "Groove Radio" });
        assert!(parse_stream_now_playing(&meta, &["Groove Radio"]).is_none());

        let meta = json!({ "icy-title": "unknown" });
        assert!(parse_stream_now_playing(&meta, &[]).is_none());
    }
}
