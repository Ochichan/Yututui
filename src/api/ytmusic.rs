//! Search backends, picked by auth mode.
//!
//! - **Authenticated** (browser cookie): ytmapi-rs `search_songs` → the clean YouTube
//!   Music *song* catalog.
//! - **Anonymous**: ytmapi-rs can't search unauthenticated (YTM gates the catalog and
//!   returns "No results"), so we shell out to `yt-dlp "ytsearch…"` — public YouTube,
//!   no auth, directly playable, and yt-dlp is already a dependency for playback.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ytmapi_rs::YtMusic;
use ytmapi_rs::auth::BrowserToken;
use ytmapi_rs::common::{VideoID, YoutubeID};

use super::Song;
use crate::radio::{self, RadioConfig, RadioMode};
use crate::util::format;

/// How many results a search returns, for both backends. The anonymous yt-dlp path asks
/// for exactly this many; the authenticated path pages through continuations until it has
/// at least this many (or runs out). Capped at 50 — `ytdlp_search` clamps to the same.
const SEARCH_RESULT_LIMIT: usize = 50;
const RADIO_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(8);

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
    mode: RadioMode,
) -> Result<Vec<Song>> {
    // Allow up to 50 so the local radio engine gets a real candidate pool to rank (the
    // engine, not this fetch, decides the final few picks).
    let limit = limit.clamp(1, 50);
    let mut out = Vec::with_capacity(limit);
    let mut seen = excluded.clone();
    let mut had_success = false;
    let mut last_err = None;

    for query in radio_queries(seed, mode) {
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

    if !had_success && let Some(e) = last_err {
        return Err(e).context("related-track search failed");
    }
    Ok(out)
}

/// Final radio safety pass for public-YouTube candidates. Cheap title/channel checks have
/// already run in the reducer; this only does full yt-dlp metadata extraction for candidates
/// whose title/channel/duration made them risky, then tops up from fallback picks.
pub(crate) async fn preflight_radio_picks(
    picks: Vec<Song>,
    fallback: Vec<Song>,
    mode: RadioMode,
    cfg: &RadioConfig,
) -> Vec<Song> {
    let target = picks.len();
    let mut out = Vec::with_capacity(target);
    let mut taken = HashSet::new();

    for song in picks.iter().chain(fallback.iter()) {
        if out.len() >= target {
            break;
        }
        if !taken.insert(song.video_id.clone()) {
            continue;
        }
        if radio::sanitize_final_picks(vec![song.clone()], &[], mode, cfg).is_empty() {
            continue;
        }
        if radio::needs_metadata_preflight(song, mode, cfg) {
            let risk = radio::musicgate::non_music_risk_score(&song.title, &song.artist);
            match song.youtube_id().map(enrich_video_meta) {
                Some(fut) => match fut.await {
                    Ok(meta) => {
                        if reject_enriched(&meta, mode, cfg) {
                            tracing::debug!(
                                id = %song.video_id,
                                title = %song.title,
                                "radio preflight rejected candidate"
                            );
                            continue;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            id = %song.video_id,
                            error = %format!("{e:#}"),
                            "radio preflight metadata lookup failed"
                        );
                        if risk >= 0.55 {
                            continue;
                        }
                    }
                },
                None => continue,
            }
        }
        out.push(song.clone());
    }

    out
}

#[derive(Debug)]
struct EnrichedVideoMeta {
    title: String,
    channel: String,
    duration_secs: Option<u32>,
    live_status: Option<String>,
    is_live: Option<bool>,
    was_live: Option<bool>,
    media_type: Option<String>,
    description: Option<String>,
}

async fn enrich_video_meta(video_id: &str) -> Result<EnrichedVideoMeta> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let output = tokio::time::timeout(
        RADIO_PREFLIGHT_TIMEOUT,
        tokio::process::Command::new("yt-dlp")
            .arg("--dump-single-json")
            .arg("--no-playlist")
            .arg("--no-warnings")
            .arg(&url)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .context("yt-dlp metadata lookup timed out")?
    .context("failed to run yt-dlp metadata lookup")?;
    if !output.status.success() {
        bail!(
            "yt-dlp metadata lookup exited with status {}",
            output.status
        );
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("could not parse yt-dlp metadata output")?;
    Ok(EnrichedVideoMeta {
        title: json_string(&json, &["title"]).unwrap_or_default(),
        channel: json_string(&json, &["channel", "uploader"]).unwrap_or_default(),
        duration_secs: json
            .get("duration")
            .and_then(serde_json::Value::as_f64)
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| d.round() as u32),
        live_status: json_string(&json, &["live_status"]),
        is_live: json_bool(&json, &["is_live"]),
        was_live: json_bool(&json, &["was_live"]),
        media_type: json_string(&json, &["media_type"]),
        description: json_string(&json, &["description"]),
    })
}

fn json_string(json: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| json.get(key).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

fn json_bool(json: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| json.get(key).and_then(serde_json::Value::as_bool))
}

fn reject_enriched(meta: &EnrichedVideoMeta, mode: RadioMode, cfg: &RadioConfig) -> bool {
    if meta.is_live == Some(true) {
        return true;
    }
    if matches!(
        meta.live_status.as_deref(),
        Some("is_live" | "is_upcoming" | "post_live")
    ) {
        return true;
    }
    if matches!(meta.media_type.as_deref(), Some("playlist" | "multi_video")) {
        return true;
    }
    if let Some(duration) = meta.duration_secs {
        let mode_max = match mode {
            RadioMode::Focused => 8 * 60,
            RadioMode::Balanced => 12 * 60,
            RadioMode::Discovery => 15 * 60,
        };
        let max_duration = cfg.max_duration_secs.min(mode_max);
        if duration < cfg.min_duration_secs || duration > max_duration {
            return true;
        }
    }
    let rich_title = match meta.description.as_deref() {
        Some(desc) if !desc.trim().is_empty() => format!("{} {}", meta.title, desc),
        _ => meta.title.clone(),
    };
    let decision = radio::musicgate::decide(
        &rich_title,
        &meta.channel,
        radio::CandidateSource::YtdlpRadio,
        mode,
    );
    if decision.action == radio::musicgate::GateAction::Reject {
        return true;
    }
    let risk = radio::musicgate::non_music_risk_score(&rich_title, &meta.channel);
    let music_tier = radio::musicgate::music_tier_score(&meta.title, &meta.channel);
    if mode == RadioMode::Focused && decision.action == radio::musicgate::GateAction::Demote {
        return true;
    }
    risk >= 0.70 && music_tier <= 0.0 && meta.was_live != Some(true)
}

fn radio_queries(seed: &str, mode: RadioMode) -> Vec<String> {
    let seed = seed.trim();
    if seed.is_empty() {
        return match mode {
            RadioMode::Focused => vec![
                "popular songs official audio".to_owned(),
                "popular music official video".to_owned(),
            ],
            RadioMode::Balanced => {
                vec!["popular music radio".to_owned(), "popular songs".to_owned()]
            }
            RadioMode::Discovery => vec![
                "new music similar songs".to_owned(),
                "popular music radio".to_owned(),
                "deep cuts songs".to_owned(),
            ],
        };
    }

    // Note: no "… mix" queries — those pull 1-hour compilations / megamixes that the radio
    // engine then has to filter out. "… radio" / "… songs" surface individual tracks instead.
    let mut queries = Vec::new();

    if let Some((title, artist)) = split_seed(seed) {
        match mode {
            RadioMode::Focused => {
                push_query(&mut queries, format!("{title} {artist} official audio"));
                push_query(&mut queries, format!("{title} {artist} official video"));
                push_query(&mut queries, format!("{artist} songs"));
                push_query(&mut queries, format!("{artist} radio"));
                push_query(&mut queries, format!("{title} {artist} song"));
            }
            RadioMode::Balanced => {
                push_query(&mut queries, format!("{seed} radio"));
                push_query(&mut queries, format!("{artist} radio"));
                push_query(&mut queries, format!("{artist} songs"));
                push_query(&mut queries, format!("{artist} similar songs"));
                push_query(&mut queries, format!("{title} {artist}"));
            }
            RadioMode::Discovery => {
                push_query(&mut queries, format!("{artist} similar songs"));
                push_query(&mut queries, format!("{artist} artist radio"));
                push_query(&mut queries, format!("{artist} deep cuts"));
                push_query(&mut queries, format!("{seed} similar songs"));
                push_query(&mut queries, format!("{title} {artist} official audio"));
                push_query(&mut queries, format!("{artist} songs"));
            }
        }
    } else {
        match mode {
            RadioMode::Focused => {
                push_query(&mut queries, format!("{seed} official audio"));
                push_query(&mut queries, format!("{seed} official video"));
                push_query(&mut queries, format!("{seed} song"));
            }
            RadioMode::Balanced => {
                push_query(&mut queries, format!("{seed} radio"));
                push_query(&mut queries, format!("{seed} songs"));
                push_query(&mut queries, format!("{seed} similar songs"));
            }
            RadioMode::Discovery => {
                push_query(&mut queries, format!("{seed} similar songs"));
                push_query(&mut queries, format!("{seed} artist radio"));
                push_query(&mut queries, format!("{seed} deep cuts"));
                push_query(&mut queries, format!("{seed} songs"));
            }
        }
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
        let queries = radio_queries("Song — Artist", RadioMode::Balanced);
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
        let queries = radio_queries("lo-fi beats", RadioMode::Balanced);
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

    #[test]
    fn radio_queries_are_mode_specific() {
        let focused = radio_queries("Song — Artist", RadioMode::Focused);
        assert_eq!(focused[0], "Song Artist official audio");
        assert!(focused.iter().any(|q| q.contains("official video")));

        let discovery = radio_queries("Song — Artist", RadioMode::Discovery);
        assert_eq!(discovery[0], "Artist similar songs");
        assert!(discovery.iter().any(|q| q.contains("deep cuts")));
        assert!(!discovery.iter().any(|q| q.contains(" mix")));
    }

    #[test]
    fn preflight_metadata_rejects_live_and_long_non_music() {
        let cfg = RadioConfig::default();
        let mut meta = EnrichedVideoMeta {
            title: "Episode 12 interview".to_owned(),
            channel: "Music Podcast".to_owned(),
            duration_secs: Some(1_800),
            live_status: None,
            is_live: None,
            was_live: None,
            media_type: None,
            description: Some("conversation and commentary".to_owned()),
        };
        assert!(reject_enriched(&meta, RadioMode::Balanced, &cfg));

        meta = EnrichedVideoMeta {
            title: "Artist - Song".to_owned(),
            channel: "Artist".to_owned(),
            duration_secs: Some(180),
            live_status: Some("is_live".to_owned()),
            is_live: Some(true),
            was_live: None,
            media_type: None,
            description: None,
        };
        assert!(reject_enriched(&meta, RadioMode::Discovery, &cfg));
    }

    #[test]
    fn preflight_metadata_keeps_trusted_music_track() {
        let cfg = RadioConfig::default();
        let meta = EnrichedVideoMeta {
            title: "Artist - Song (Official Audio)".to_owned(),
            channel: "Artist - Topic".to_owned(),
            duration_secs: Some(210),
            live_status: None,
            is_live: None,
            was_live: None,
            media_type: None,
            description: None,
        };
        assert!(!reject_enriched(&meta, RadioMode::Focused, &cfg));
    }
}
