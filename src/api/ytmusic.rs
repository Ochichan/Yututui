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
use ytmapi_rs::common::{VideoID, YoutubeID};

use super::Song;
use crate::util::format;

/// How many results a search returns, for both backends. The anonymous yt-dlp path asks
/// for exactly this many; the authenticated path pages through continuations until it has
/// at least this many (or runs out). Capped at 50 — `ytdlp_search` clamps to the same.
const SEARCH_RESULT_LIMIT: usize = 50;

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

    /// Search for songs matching `query`, using the backend for this mode. Returns up to
    /// [`SEARCH_RESULT_LIMIT`] tracks.
    pub async fn search_songs(&self, query: &str) -> Result<Vec<Song>> {
        match self {
            // The simplified `search_songs` wrapper only fetches the first page (~20). Drive
            // the continuation stream directly so we can collect up to SEARCH_RESULT_LIMIT,
            // stopping early once we have enough (or the pages run out).
            Self::Browser(c) => {
                use futures::StreamExt;
                use ytmapi_rs::query::SearchQuery;
                use ytmapi_rs::query::search::{FilteredSearch, SongsFilter};

                // The blanket `From<&str>` builds the songs-filtered query (same conversion the
                // `search_songs` wrapper does) without the deprecated `new`/`with_filter`.
                let q: SearchQuery<FilteredSearch<SongsFilter>> = query.into();
                let mut pages = std::pin::pin!(c.stream(&q));
                let mut songs = Vec::new();
                while songs.len() < SEARCH_RESULT_LIMIT
                    && let Some(page) = pages.next().await
                {
                    let page = page.context("YouTube Music search failed")?;
                    for s in page {
                        songs.push(Song::remote(
                            s.video_id.get_raw(),
                            s.title,
                            s.artist,
                            s.duration,
                        ));
                        if songs.len() >= SEARCH_RESULT_LIMIT {
                            break;
                        }
                    }
                }
                Ok(songs)
            }
            Self::Anonymous => ytdlp_search(query, SEARCH_RESULT_LIMIT).await,
        }
    }

    /// The real YouTube Music radio continuation for a seed track
    /// (`get_watch_playlist_from_video_id`) — YTM's own "up next" mix, far better seeded than a
    /// blind text search. Authenticated uses the logged-in client; anonymous spins up an
    /// unauthenticated client (the query isn't login-gated, though YTM may still return nothing
    /// without a cookie — the caller treats an error/empty result as "fall back to yt-dlp").
    pub(crate) async fn radio_continuation(&self, seed_video_id: &str) -> Result<Vec<Song>> {
        let tracks = match self {
            Self::Browser(c) => c
                .get_watch_playlist_from_video_id(VideoID::from_raw(seed_video_id))
                .await
                .context("watch-playlist (authenticated) failed")?,
            Self::Anonymous => {
                let client = YtMusic::new_unauthenticated()
                    .await
                    .context("anonymous YouTube Music client init failed")?;
                client
                    .get_watch_playlist_from_video_id(VideoID::from_raw(seed_video_id))
                    .await
                    .context("watch-playlist (anonymous) failed")?
            }
        };
        Ok(tracks
            .into_iter()
            .map(|t| Song::remote(t.video_id.get_raw(), t.title, t.author, t.duration))
            .collect())
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
    // Allow up to 50 so the local radio engine gets a real candidate pool to rank (the
    // engine, not this fetch, decides the final few picks).
    let limit = limit.clamp(1, 50);
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
            "popular songs".to_owned(),
        ];
    }

    // Note: no "… mix" queries — those pull 1-hour compilations / megamixes that the radio
    // engine then has to filter out. "… radio" / "… songs" surface individual tracks instead.
    let mut queries = Vec::new();
    push_query(&mut queries, format!("{seed} radio"));

    if let Some((title, artist)) = split_seed(seed) {
        push_query(&mut queries, format!("{artist} radio"));
        push_query(&mut queries, format!("{artist} songs"));
        push_query(&mut queries, format!("{artist} similar songs"));
        push_query(&mut queries, format!("{title} {artist}"));
    } else {
        push_query(&mut queries, format!("{seed} songs"));
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
                "Artist radio",
                "Artist songs",
                "Artist similar songs",
                "Song Artist",
            ]
        );
        // No "mix" queries — they pull long compilations.
        assert!(!queries.iter().any(|q| q.contains("mix")));
    }

    #[test]
    fn radio_queries_handle_plain_seed() {
        let queries = radio_queries("lo-fi beats");
        assert_eq!(
            queries,
            vec![
                "lo-fi beats radio",
                "lo-fi beats songs",
                "lo-fi beats similar songs",
            ]
        );
        assert!(!queries.iter().any(|q| q.contains("mix")));
    }
}
