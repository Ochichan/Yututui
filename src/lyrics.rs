//! Synced lyrics: an LRC parser, a current-line lookup, and the fetch actor.
//!
//! Lyrics come from [lrclib.net](https://lrclib.net) (`/api/search`), which returns
//! candidates carrying `syncedLyrics` in LRC format (`[mm:ss.xx] text`). We parse that
//! into time-stamped [`LyricLine`]s; the player view binary-searches the current line by
//! playback position each frame. The actor caches by `video_id` so re-opening the panel
//! (or replaying) costs no network.

use std::collections::HashMap;

use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::app::Msg;
use crate::util::{http, sanitize};

/// Session cache cap (bounded memory; cleared wholesale when exceeded).
const CACHE_MAX: usize = 999;
const LYRICS_JSON_MAX: usize = 512 * 1024;

/// One timed lyric line.
#[derive(Debug, Clone)]
pub struct LyricLine {
    /// Offset from track start, in seconds.
    pub time: f64,
    pub text: String,
}

/// Parse an LRC body into time-sorted lines. Metadata tags (`[ar:…]`, `[length:…]`) and
/// malformed timestamps are ignored; a line may carry several timestamps (`[t1][t2] x`).
pub fn parse_lrc(raw: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let mut rest = line;
        let mut stamps = Vec::new();
        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else { break };
            if let Some(t) = parse_timestamp(&rest[1..end]) {
                stamps.push(t);
            }
            rest = &rest[end + 1..];
        }
        let text = rest.trim().to_owned();
        for t in stamps {
            out.push(LyricLine {
                time: t,
                text: text.clone(),
            });
        }
    }
    out.sort_by(|a, b| {
        a.time
            .partial_cmp(&b.time)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Parse a `mm:ss` / `mm:ss.xx` timestamp into seconds. Returns `None` for metadata tags.
fn parse_timestamp(tag: &str) -> Option<f64> {
    let (m, s) = tag.split_once(':')?;
    let mins: f64 = m.trim().parse().ok()?;
    let secs: f64 = s.trim().parse().ok()?;
    Some(mins * 60.0 + secs)
}

/// Index of the line that should be highlighted at playback position `pos` — the last
/// line whose timestamp is `<= pos`. `None` before the first line (or when empty).
pub fn current_index(lines: &[LyricLine], pos: f64) -> Option<usize> {
    let pp = lines.partition_point(|l| l.time <= pos);
    (pp > 0).then(|| pp - 1)
}

// --- Actor ------------------------------------------------------------------

pub enum LyricsCmd {
    Fetch {
        video_id: String,
        artist: String,
        title: String,
    },
}

pub struct LyricsHandle {
    tx: UnboundedSender<LyricsCmd>,
}

impl LyricsHandle {
    pub fn fetch(&self, video_id: String, artist: String, title: String) {
        let _ = self.tx.send(LyricsCmd::Fetch {
            video_id,
            artist,
            title,
        });
    }
}

/// Spawn the lyrics actor; results return as [`Msg::LyricsResult`].
pub fn spawn(msg_tx: UnboundedSender<Msg>) -> LyricsHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run_actor(rx, msg_tx));
    LyricsHandle { tx }
}

async fn run_actor(mut rx: UnboundedReceiver<LyricsCmd>, msg_tx: UnboundedSender<Msg>) {
    let client = reqwest::Client::builder()
        .user_agent("ytm-tui/0.1 (https://github.com/ytm-tui/ytm-tui)")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut cache: HashMap<String, Vec<LyricLine>> = HashMap::new();

    while let Some(LyricsCmd::Fetch {
        video_id,
        artist,
        title,
    }) = rx.recv().await
    {
        let lines = if let Some(cached) = cache.get(&video_id) {
            cached.clone()
        } else {
            let fetched = fetch(&client, &artist, &title).await;
            if cache.len() >= CACHE_MAX {
                cache.clear();
            }
            cache.insert(video_id.clone(), fetched.clone());
            fetched
        };
        tracing::info!(count = lines.len(), video_id = %video_id, "lyrics");
        let _ = msg_tx.send(Msg::LyricsResult { video_id, lines });
    }
}

#[derive(Deserialize)]
struct LrcItem {
    #[serde(rename = "syncedLyrics")]
    synced: Option<String>,
}

/// Query lrclib and return the first candidate's parsed synced lyrics (empty if none).
async fn fetch(client: &reqwest::Client, artist: &str, title: &str) -> Vec<LyricLine> {
    let resp = client
        .get("https://lrclib.net/api/search")
        .query(&[("track_name", title), ("artist_name", artist)])
        .send()
        .await;
    let items: Vec<LrcItem> = match resp {
        Ok(r) => http::json_limited(r, LYRICS_JSON_MAX)
            .await
            .unwrap_or_default(),
        Err(e) => {
            tracing::warn!(error = %sanitize::sanitize_error_text(e.to_string()), "lyrics fetch failed");
            return Vec::new();
        }
    };
    for item in items {
        if let Some(s) = item.synced
            && !s.trim().is_empty()
        {
            let lines = parse_lrc(&s);
            if !lines.is_empty() {
                return lines;
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamps_and_sorts() {
        let raw = "[ti:Song]\n[00:12.50]first\n[00:05.00]earlier\n[01:00.00]later";
        let lines = parse_lrc(raw);
        // Metadata line dropped; the three timed lines sorted ascending.
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "earlier");
        assert!((lines[0].time - 5.0).abs() < 1e-6);
        assert!((lines[1].time - 12.5).abs() < 1e-6);
        assert!((lines[2].time - 60.0).abs() < 1e-6);
    }

    #[test]
    fn multiple_timestamps_per_line_expand() {
        let lines = parse_lrc("[00:01.00][00:10.00]repeat");
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.text == "repeat"));
    }

    #[test]
    fn current_index_tracks_position() {
        let lines = parse_lrc("[00:00.00]a\n[00:05.00]b\n[00:10.00]c");
        assert_eq!(current_index(&lines, -1.0), None); // before first (can't happen, but safe)
        assert_eq!(current_index(&lines, 0.0), Some(0));
        assert_eq!(current_index(&lines, 4.9), Some(0));
        assert_eq!(current_index(&lines, 5.0), Some(1));
        assert_eq!(current_index(&lines, 9.9), Some(1));
        assert_eq!(current_index(&lines, 100.0), Some(2));
    }

    #[test]
    fn empty_is_handled() {
        assert!(parse_lrc("").is_empty());
        assert_eq!(current_index(&[], 5.0), None);
    }
}
