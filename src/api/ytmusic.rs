//! Search backends, picked by auth mode.
//!
//! - **Authenticated** (browser cookie): ytmapi-rs `search_songs` → the clean YouTube
//!   Music *song* catalog.
//! - **Anonymous**: ytmapi-rs can't search unauthenticated (YTM gates the catalog and
//!   returns "No results"), so we shell out to `yt-dlp "ytsearch…"` — public YouTube,
//!   no auth, directly playable, and yt-dlp is already a dependency for playback.

use std::collections::HashSet;
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

/// Best-effort related tracks for radio/autoplay without Gemini.
///
/// There is no stable public recommendation API in the app today, so the anonymous
/// fallback uses the same yt-dlp search boundary as normal anonymous search. It asks for
/// common "radio/mix/similar" queries and de-dupes against the caller's exclusions.
pub(crate) async fn related_tracks(
    seed: &str,
    limit: usize,
    excluded: &HashSet<String>,
) -> Result<Vec<Song>> {
    let limit = limit.clamp(1, 20);
    let mut out = Vec::with_capacity(limit);
    let mut seen = excluded.clone();
    let mut had_success = false;
    let mut last_err = None;

    for query in radio_queries(seed) {
        let search_limit = (limit * 2).clamp(limit, 50);
        match ytdlp_search(&query, search_limit).await {
            Ok(songs) => {
                had_success = true;
                for song in songs {
                    if seen.insert(song.video_id.clone()) {
                        out.push(song);
                        if out.len() >= limit {
                            return Ok(out);
                        }
                    }
                }
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
    }

    if !had_success
        && let Some(e) = last_err
    {
        return Err(e).context("related-track search failed");
    }
    Ok(out)
}

fn radio_queries(seed: &str) -> Vec<String> {
    let seed = seed.trim();
    if seed.is_empty() {
        return vec![
            "popular music radio".to_owned(),
            "popular music mix".to_owned(),
        ];
    }

    let mut queries = Vec::new();
    push_query(&mut queries, format!("{seed} radio"));
    push_query(&mut queries, format!("{seed} mix"));

    if let Some((title, artist)) = split_seed(seed) {
        push_query(&mut queries, format!("{artist} radio"));
        push_query(&mut queries, format!("{artist} similar songs"));
        push_query(&mut queries, format!("{title} {artist} mix"));
    } else {
        push_query(&mut queries, format!("{seed} similar songs"));
    }

    queries
}

fn split_seed(seed: &str) -> Option<(&str, &str)> {
    seed.split_once(" — ")
        .or_else(|| seed.split_once(" - "))
        .and_then(|(title, artist)| {
            let title = title.trim();
            let artist = artist.trim();
            (!title.is_empty() && !artist.is_empty()).then_some((title, artist))
        })
}

fn push_query(queries: &mut Vec<String>, query: String) {
    if !queries.iter().any(|q| q == &query) {
        queries.push(query);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radio_queries_expand_title_artist_seed() {
        let queries = radio_queries("Song — Artist");
        assert_eq!(
            queries,
            vec![
                "Song — Artist radio",
                "Song — Artist mix",
                "Artist radio",
                "Artist similar songs",
                "Song Artist mix",
            ]
        );
    }

    #[test]
    fn radio_queries_handle_plain_seed() {
        let queries = radio_queries("lo-fi beats");
        assert_eq!(
            queries,
            vec![
                "lo-fi beats radio",
                "lo-fi beats mix",
                "lo-fi beats similar songs",
            ]
        );
    }
}
