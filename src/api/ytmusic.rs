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

use super::{PlayableRef, Song};
use crate::radio::{self, RadioConfig, RadioMode};
use crate::search_source::{SearchConfig, SearchSource};
use crate::util::format;

/// How many results a search returns, for both backends. The anonymous yt-dlp path asks
/// for exactly this many; the authenticated path pages through continuations until it has
/// at least this many (or runs out). Capped at 50 — `ytdlp_search` clamps to the same.
const SEARCH_RESULT_LIMIT: usize = 50;
const RADIO_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(8);
const PROVIDER_SEARCH_TIMEOUT: Duration = Duration::from_secs(12);

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
    pub async fn search_songs(
        &self,
        query: &str,
        source: SearchSource,
        config: &SearchConfig,
    ) -> Result<Vec<Song>> {
        match source {
            SearchSource::All => self.search_all_sources(query, config).await,
            source => self.search_one_source(query, source, config).await,
        }
    }

    async fn search_all_sources(&self, query: &str, config: &SearchConfig) -> Result<Vec<Song>> {
        let mut songs = Vec::new();
        let mut seen = HashSet::new();
        let mut errors = Vec::new();
        for source in config.enabled_sources() {
            match self.search_one_source(query, source, config).await {
                Ok(results) => {
                    for song in results {
                        if seen.insert(song.video_id.clone()) {
                            songs.push(song);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(source = %source.code(), error = %format!("{e:#}"), "source search failed");
                    errors.push(format!("{}: {e:#}", source.code()));
                }
            }
            if songs.len() >= SEARCH_RESULT_LIMIT {
                songs.truncate(SEARCH_RESULT_LIMIT);
                break;
            }
        }
        if songs.is_empty() && !errors.is_empty() {
            bail!("all enabled sources failed ({})", errors.join("; "));
        }
        Ok(songs)
    }

    async fn search_one_source(
        &self,
        query: &str,
        source: SearchSource,
        config: &SearchConfig,
    ) -> Result<Vec<Song>> {
        if !config.is_enabled(source) {
            bail!("{} is disabled in Settings → General", source.label());
        }
        match source {
            SearchSource::Youtube => self.search_youtube(query).await,
            SearchSource::SoundCloud => {
                ytdlp_flat_search(
                    SearchSource::SoundCloud,
                    "scsearch",
                    query,
                    SEARCH_RESULT_LIMIT,
                )
                .await
            }
            SearchSource::Audius => audius_search(query, config, SEARCH_RESULT_LIMIT).await,
            SearchSource::Jamendo => jamendo_search(query, config, SEARCH_RESULT_LIMIT).await,
            SearchSource::InternetArchive => archive_search(query, SEARCH_RESULT_LIMIT).await,
            SearchSource::RadioBrowser => radio_browser_search(query, SEARCH_RESULT_LIMIT).await,
            SearchSource::All => bail!("internal error: nested ALL source search"),
        }
    }

    async fn search_youtube(&self, query: &str) -> Result<Vec<Song>> {
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
/// Shared with the DJ Gem assistant actor, which resolves the model's tool queries the same
/// way (public YouTube, no auth) — hence `pub(crate)` and a caller-chosen `limit`.
pub(crate) async fn ytdlp_search(query: &str, limit: usize) -> Result<Vec<Song>> {
    ytdlp_flat_search(SearchSource::Youtube, "ytsearch", query, limit).await
}

async fn ytdlp_flat_search(
    source: SearchSource,
    prefix: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<Song>> {
    let limit = limit.clamp(1, 50);
    let spec = format!("ytsearch{limit}:{query}");
    let spec = if prefix == "ytsearch" {
        spec
    } else {
        format!("{prefix}{limit}:{query}")
    };
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
    Ok(entries
        .iter()
        .filter_map(|entry| parse_ytdlp_entry(source, entry))
        .collect())
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

async fn audius_search(query: &str, config: &SearchConfig, limit: usize) -> Result<Vec<Song>> {
    let app_name = config.effective_audius_app_name();
    let client = provider_client()?;
    let limit = limit.clamp(1, 50).to_string();
    let json: serde_json::Value = client
        .get("https://discoveryprovider.audius.co/v1/tracks/search")
        .query(&[
            ("query", query),
            ("app_name", app_name.as_str()),
            ("limit", limit.as_str()),
        ])
        .send()
        .await
        .context("Audius search request failed")?
        .error_for_status()
        .context("Audius search returned an error")?
        .json()
        .await
        .context("could not parse Audius search response")?;
    let entries = json
        .get("data")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(entries
        .iter()
        .filter_map(|entry| parse_audius_track(entry, &app_name))
        .collect())
}

async fn jamendo_search(query: &str, config: &SearchConfig, limit: usize) -> Result<Vec<Song>> {
    let Some(client_id) = config.jamendo_client_id() else {
        bail!("Jamendo client_id is missing. Add it in Settings → General.");
    };
    let client = provider_client()?;
    let limit = limit.clamp(1, 50).to_string();
    let json: serde_json::Value = client
        .get("https://api.jamendo.com/v3.0/tracks/")
        .query(&[
            ("client_id", client_id),
            ("format", "json"),
            ("limit", limit.as_str()),
            ("namesearch", query),
            ("audioformat", "mp32"),
        ])
        .send()
        .await
        .context("Jamendo search request failed")?
        .error_for_status()
        .context("Jamendo search returned an error")?
        .json()
        .await
        .context("could not parse Jamendo search response")?;
    if json
        .pointer("/headers/status")
        .and_then(serde_json::Value::as_str)
        == Some("failed")
    {
        let msg = json
            .pointer("/headers/error_message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Jamendo API error");
        bail!("{msg}");
    }
    let entries = json
        .get("results")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(entries.iter().filter_map(parse_jamendo_track).collect())
}

async fn archive_search(query: &str, limit: usize) -> Result<Vec<Song>> {
    let client = provider_client()?;
    let rows = limit.clamp(1, 20).to_string();
    let q = format!("{query} AND mediatype:audio");
    let json: serde_json::Value = client
        .get("https://archive.org/advancedsearch.php")
        .query(&[
            ("q", q.as_str()),
            ("fl[]", "identifier"),
            ("fl[]", "title"),
            ("fl[]", "creator"),
            ("rows", rows.as_str()),
            ("page", "1"),
            ("output", "json"),
        ])
        .send()
        .await
        .context("Internet Archive search request failed")?
        .error_for_status()
        .context("Internet Archive search returned an error")?
        .json()
        .await
        .context("could not parse Internet Archive search response")?;
    let docs = json
        .pointer("/response/docs")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut out = Vec::new();
    for doc in docs {
        let Some(identifier) = json_string(doc, &["identifier"]) else {
            continue;
        };
        let title = json_string(doc, &["title"]).unwrap_or_else(|| identifier.clone());
        let artist = json_string(doc, &["creator"]).unwrap_or_default();
        if let Some((file, duration)) = archive_audio_file(&client, &identifier).await {
            let url = archive_file_url(&identifier, &file);
            out.push(Song::from_source(
                SearchSource::InternetArchive,
                format!("{identifier}:{file}"),
                title,
                artist,
                duration.unwrap_or_default(),
                PlayableRef::ArchiveFile {
                    identifier,
                    file,
                    url,
                },
            ));
        }
    }
    Ok(out)
}

async fn radio_browser_search(query: &str, limit: usize) -> Result<Vec<Song>> {
    let client = provider_client()?;
    let limit = limit.clamp(1, 50).to_string();
    let json: serde_json::Value = client
        .get("https://de1.api.radio-browser.info/json/stations/search")
        .query(&[
            ("name", query),
            ("limit", limit.as_str()),
            ("hidebroken", "true"),
            ("order", "clickcount"),
            ("reverse", "true"),
        ])
        .send()
        .await
        .context("Radio Browser search request failed")?
        .error_for_status()
        .context("Radio Browser search returned an error")?
        .json()
        .await
        .context("could not parse Radio Browser search response")?;
    let entries = json.as_array().map(Vec::as_slice).unwrap_or_default();
    Ok(entries.iter().filter_map(parse_radio_station).collect())
}

fn provider_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(PROVIDER_SEARCH_TIMEOUT)
        .user_agent(format!("ytm-tui/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build provider HTTP client")
}

/// Map one yt-dlp flat-playlist entry to a [`Song`]. Skips entries without an id.
fn parse_ytdlp_entry(source: SearchSource, e: &serde_json::Value) -> Option<Song> {
    let id = e.get("id")?.as_str()?.to_owned();
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
    if source == SearchSource::Youtube {
        return Some(Song::remote(id, title, artist, duration));
    }
    let url = e
        .get("webpage_url")
        .or_else(|| e.get("url"))
        .and_then(serde_json::Value::as_str)?
        .to_owned();
    Some(Song::from_source(
        source,
        id,
        title,
        artist,
        duration,
        PlayableRef::YtdlpUrl { source, url },
    ))
}

fn parse_audius_track(e: &serde_json::Value, app_name: &str) -> Option<Song> {
    let id = e.get("id")?.as_str()?.to_owned();
    let title = json_string(e, &["title"]).unwrap_or_else(|| "Unknown".to_owned());
    let artist = e
        .get("user")
        .and_then(|u| json_string(u, &["name", "handle"]))
        .unwrap_or_default();
    let duration = e
        .get("duration")
        .and_then(serde_json::Value::as_f64)
        .map(format::time)
        .unwrap_or_default();
    Some(Song::from_source(
        SearchSource::Audius,
        id.clone(),
        title,
        artist,
        duration,
        PlayableRef::AudiusTrackId {
            id,
            app_name: app_name.to_owned(),
        },
    ))
}

fn parse_jamendo_track(e: &serde_json::Value) -> Option<Song> {
    let id = json_string(e, &["id"])?;
    let url = json_string(e, &["audio"])?;
    let title = json_string(e, &["name"]).unwrap_or_else(|| "Unknown".to_owned());
    let artist = json_string(e, &["artist_name"]).unwrap_or_default();
    let duration = e
        .get("duration")
        .and_then(serde_json::Value::as_f64)
        .map(format::time)
        .unwrap_or_default();
    Some(Song::from_source(
        SearchSource::Jamendo,
        id.clone(),
        title,
        artist,
        duration,
        PlayableRef::JamendoTrackId { id, url },
    ))
}

fn parse_radio_station(e: &serde_json::Value) -> Option<Song> {
    let id = json_string(e, &["stationuuid"])?;
    let url = json_string(e, &["url_resolved"]).or_else(|| json_string(e, &["url"]))?;
    let title = json_string(e, &["name"]).unwrap_or_else(|| "Unknown station".to_owned());
    let codec = json_string(e, &["codec"]).unwrap_or_default();
    let bitrate = e
        .get("bitrate")
        .and_then(serde_json::Value::as_u64)
        .filter(|b| *b > 0)
        .map(|b| format!("{b}k"))
        .unwrap_or_default();
    let country = json_string(e, &["country"]).unwrap_or_default();
    let artist = [country.as_str(), codec.as_str(), bitrate.as_str()]
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    Some(Song::from_source(
        SearchSource::RadioBrowser,
        id,
        title,
        artist,
        String::new(),
        PlayableRef::RadioStream { url },
    ))
}

async fn archive_audio_file(
    client: &reqwest::Client,
    identifier: &str,
) -> Option<(String, Option<String>)> {
    let url = format!("https://archive.org/metadata/{identifier}");
    let json: serde_json::Value = client.get(url).send().await.ok()?.json().await.ok()?;
    let files = json.get("files")?.as_array()?;
    files
        .iter()
        .filter_map(|file| {
            let name = json_string(file, &["name"])?;
            let lower = name.to_ascii_lowercase();
            let format_name = json_string(file, &["format"])
                .unwrap_or_default()
                .to_ascii_lowercase();
            let playable = ["mp3", "m4a", "ogg", "opus", "flac"]
                .iter()
                .any(|ext| lower.ends_with(&format!(".{ext}")))
                || ["mp3", "mpeg", "ogg", "flac", "opus", "audio"]
                    .iter()
                    .any(|needle| format_name.contains(needle));
            if !playable {
                return None;
            }
            let duration = json_string(file, &["length"]).and_then(|s| {
                s.parse::<f64>()
                    .ok()
                    .filter(|d| d.is_finite() && *d > 0.0)
                    .map(format::time)
            });
            let rank = if lower.ends_with(".mp3") {
                0
            } else if lower.ends_with(".m4a") {
                1
            } else if lower.ends_with(".ogg") || lower.ends_with(".opus") {
                2
            } else {
                3
            };
            Some((rank, name, duration))
        })
        .min_by_key(|(rank, _, _)| *rank)
        .map(|(_, name, duration)| (name, duration))
}

fn archive_file_url(identifier: &str, file: &str) -> String {
    let mut url = reqwest::Url::parse("https://archive.org/download/").unwrap();
    if let Ok(mut segments) = url.path_segments_mut() {
        segments.push(identifier).push(file);
    }
    url.to_string()
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
