//! Synced lyrics: an LRC parser, a current-line lookup, and the fetch actor.
//!
//! Lyrics come from [lrclib.net](https://lrclib.net) (`/api/search`), which returns
//! candidates carrying `syncedLyrics` in LRC format (`[mm:ss.xx] text`). We parse that
//! into time-stamped [`LyricLine`]s; the player view binary-searches the current line by
//! playback position each frame. The actor caches by `video_id` so re-opening the panel
//! (or replaying) costs no network.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::util::{http, sanitize};

/// Session cache cap (bounded memory; cleared wholesale when exceeded).
const CACHE_MAX: usize = 999;
const LYRICS_JSON_MAX: usize = 512 * 1024;
/// Per-request timeout so a hung lrclib connection can't wedge the actor indefinitely.
const LYRICS_TIMEOUT: Duration = Duration::from_secs(8);
/// How long a *transient* fetch failure is remembered before we retry. A genuine
/// "this track has no synced lyrics" is cached for the whole session; only failures
/// (network/parse) get this short cooldown so re-opening the panel later can recover.
const NEGATIVE_TTL: Duration = Duration::from_secs(120);

/// A per-track cache entry: a resolved result (kept for the session, even when empty — the
/// track genuinely has no synced lyrics) versus a transient failure retried after a cooldown.
enum CacheEntry {
    Found(Vec<LyricLine>),
    Failed(Instant),
}

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
    // Reject non-finite ("nan"/"inf" parse as f64) and out-of-range values. A non-finite time
    // breaks the sorted-vector invariant `current_index`/`partition_point` rely on; the
    // `secs` range and non-negative `mins` drop obviously malformed tags.
    if !mins.is_finite() || mins < 0.0 || !(0.0..60.0).contains(&secs) {
        return None;
    }
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

pub enum LyricsEvent {
    Result {
        video_id: String,
        lines: Vec<LyricLine>,
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

/// Collapse a burst of queued fetches to the newest one (rapid skips) — see the artwork
/// actor. Non-blocking: drains only already-buffered commands, never awaits a new one.
fn take_latest(first: LyricsCmd, rx: &mut UnboundedReceiver<LyricsCmd>) -> LyricsCmd {
    let mut cmd = first;
    while let Ok(next) = rx.try_recv() {
        cmd = next;
    }
    cmd
}

/// Spawn the lyrics actor; results return as [`LyricsEvent`]s.
pub fn spawn<F>(emit: F) -> LyricsHandle
where
    F: Fn(LyricsEvent) + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run_actor(rx, emit));
    LyricsHandle { tx }
}

async fn run_actor<F>(mut rx: UnboundedReceiver<LyricsCmd>, emit: F)
where
    F: Fn(LyricsEvent) + Send + Sync + 'static,
{
    let client = reqwest::Client::builder()
        .user_agent("yututui/0.1 (https://github.com/Ochichan/Yututui)")
        .timeout(LYRICS_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut cache: HashMap<String, CacheEntry> = HashMap::new();

    while let Some(cmd) = rx.recv().await {
        // Latest-only: rapid track-skips queue several fetches, but only the current track's
        // lyrics are shown, so drain to the newest (see the artwork actor) instead of walking
        // a backlog. The per-`video_id` cache still serves a track revisited later.
        let LyricsCmd::Fetch {
            video_id,
            artist,
            title,
        } = take_latest(cmd, &mut rx);
        // A resolved result (even empty) is reused; a transient failure is reused only until
        // its cooldown expires, after which we re-fetch instead of showing "no lyrics" forever.
        let cached = match cache.get(&video_id) {
            Some(CacheEntry::Found(lines)) => Some(lines.clone()),
            Some(CacheEntry::Failed(at)) if at.elapsed() < NEGATIVE_TTL => Some(Vec::new()),
            _ => None,
        };
        let lines = if let Some(lines) = cached {
            lines
        } else {
            if cache.len() >= CACHE_MAX {
                cache.clear();
            }
            match fetch(&client, &artist, &title).await {
                Ok(fetched) => {
                    cache.insert(video_id.clone(), CacheEntry::Found(fetched.clone()));
                    fetched
                }
                Err(()) => {
                    cache.insert(video_id.clone(), CacheEntry::Failed(Instant::now()));
                    Vec::new()
                }
            }
        };
        tracing::info!(count = lines.len(), video_id = %video_id, "lyrics");
        emit(LyricsEvent::Result { video_id, lines });
    }
}

#[derive(Deserialize)]
struct LrcItem {
    #[serde(rename = "syncedLyrics")]
    synced: Option<String>,
}

/// Query lrclib and return the first candidate's parsed synced lyrics (empty if none).
/// `Ok(lines)` — a resolved answer (empty means the track genuinely has no synced lyrics).
/// `Err(())` — a *transient* failure (network/parse) the caller should retry after a cooldown.
async fn fetch(client: &reqwest::Client, artist: &str, title: &str) -> Result<Vec<LyricLine>, ()> {
    let resp = client
        .get("https://lrclib.net/api/search")
        .query(&[("track_name", title), ("artist_name", artist)])
        .send()
        .await
        .map_err(|e| {
            tracing::warn!(error = %sanitize::sanitize_error_text(e.to_string()), "lyrics fetch failed");
        })?;
    let items: Vec<LrcItem> = http::json_limited(resp, LYRICS_JSON_MAX)
        .await
        .map_err(|e| {
            tracing::warn!(error = %sanitize::sanitize_error_text(e.to_string()), "lyrics parse failed");
        })?;
    for item in items {
        if let Some(s) = item.synced
            && !s.trim().is_empty()
        {
            let lines = parse_lrc(&s);
            if !lines.is_empty() {
                return Ok(lines);
            }
        }
    }
    Ok(Vec::new())
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
    fn malformed_timestamps_are_dropped_not_treated_as_non_finite() {
        // "nan"/"inf" parse as f64; a non-finite time would break the sorted-vector invariant.
        assert_eq!(parse_timestamp("nan:00"), None);
        assert_eq!(parse_timestamp("00:inf"), None);
        assert_eq!(parse_timestamp("-1:00"), None); // negative minutes
        assert_eq!(parse_timestamp("00:75"), None); // seconds out of range
        // A well-formed tag still parses.
        assert!((parse_timestamp("01:30.5").unwrap() - 90.5).abs() < 1e-6);
        // A whole line carrying a bad stamp yields no timed line, without panicking.
        assert!(parse_lrc("[nan:00]lyric").is_empty());
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
