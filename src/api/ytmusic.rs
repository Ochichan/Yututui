//! Search backends, picked by auth mode.
//!
//! - **Authenticated** (browser cookie): ytmapi-rs `search_songs` → the clean YouTube
//!   Music *song* catalog.
//! - **Anonymous**: ytmapi-rs can't search unauthenticated (YTM gates the catalog and
//!   returns "No results"), so we shell out to `yt-dlp "ytsearch…"` — public YouTube,
//!   no auth, directly playable, and yt-dlp is already a dependency for playback.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::BrowserToken;
use ytmapi_rs::common::YoutubeID;

use super::Song;
use crate::util::format;

/// How many results an anonymous yt-dlp search asks for.
const ANON_SEARCH_LIMIT: usize = 20;

/// A YouTube Music client in one of two auth modes.
pub enum YtMusicApi {
    Browser(YtMusic<BrowserToken>),
    Anonymous,
}

impl YtMusicApi {
    /// Authenticate with a raw browser `Cookie:` header.
    pub async fn from_cookie(cookie: &str) -> Result<Self> {
        let client = YtMusic::from_cookie(cookie)
            .await
            .context("YouTube Music cookie authentication failed")?;
        Ok(Self::Browser(client))
    }

    /// Search for songs matching `query`, using the backend for this mode.
    pub async fn search_songs(&self, query: &str) -> Result<Vec<Song>> {
        match self {
            Self::Browser(c) => {
                let songs = c
                    .search_songs(query)
                    .await
                    .context("YouTube Music search failed")?;
                Ok(songs
                    .into_iter()
                    .map(|s| Song::remote(s.video_id.get_raw(), s.title, s.artist, s.duration))
                    .collect())
            }
            Self::Anonymous => ytdlp_search(query, ANON_SEARCH_LIMIT).await,
        }
    }
}

/// Anonymous search via `yt-dlp "ytsearchN:<query>" --flat-playlist --dump-single-json`.
/// Shared with the AI assistant actor, which resolves the model's tool queries the same
/// way (public YouTube, no auth) — hence `pub(crate)` and a caller-chosen `limit`.
pub(crate) async fn ytdlp_search(query: &str, limit: usize) -> Result<Vec<Song>> {
    let limit = limit.clamp(1, 50);
    let spec = format!("ytsearch{limit}:{query}");
    let output = tokio::process::Command::new("yt-dlp")
        .arg(&spec)
        .arg("--flat-playlist")
        .arg("--dump-single-json")
        .arg("--no-warnings")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .context("failed to run yt-dlp — is it installed and on PATH?")?;
    if !output.status.success() {
        bail!("yt-dlp search exited with status {}", output.status);
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("could not parse yt-dlp search output")?;
    let entries = json
        .get("entries")
        .and_then(|e| e.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(entries.iter().filter_map(parse_entry).collect())
}

/// Map one yt-dlp flat-playlist entry to a [`Song`]. Skips entries without an id.
fn parse_entry(e: &serde_json::Value) -> Option<Song> {
    let video_id = e.get("id")?.as_str()?.to_owned();
    let title = e
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("Unknown")
        .to_owned();
    let artist = e
        .get("uploader")
        .or_else(|| e.get("channel"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let duration = e
        .get("duration")
        .and_then(serde_json::Value::as_f64)
        .map(format::time)
        .unwrap_or_default();
    Some(Song::remote(video_id, title, artist, duration))
}
