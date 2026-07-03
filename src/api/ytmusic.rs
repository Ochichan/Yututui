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
use crate::search_source::{SearchConfig, SearchSource};
use crate::streaming::{self, StreamingConfig, StreamingMode};
use crate::util::{format, http, process, sanitize};

/// How many results a search returns, for both backends. The anonymous yt-dlp path asks
/// for exactly this many; the authenticated path pages through continuations until it has
/// at least this many (or runs out). Capped at 50 — `ytdlp_search` clamps to the same.
const SEARCH_RESULT_LIMIT: usize = 50;
const STREAMING_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(8);
/// Latched when an authenticated innertube search fails to parse (empty/attested-only
/// responses): every later search this process goes straight to yt-dlp. Restarting the
/// app retries innertube, so a future un-gating heals on its own.
static AUTH_SEARCH_DEGRADED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

const PROVIDER_SEARCH_TIMEOUT: Duration = Duration::from_secs(12);
const PROVIDER_JSON_MAX: usize = 2 * 1024 * 1024;
const YTDLP_SEARCH_TIMEOUT: Duration = Duration::from_secs(12);
const YTDLP_JSON_MAX: usize = 2 * 1024 * 1024;
/// Flat playlist extraction budget: hundreds of entries and a slower endpoint than a
/// plain search, so a longer timeout and a larger JSON ceiling.
const PLAYLIST_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const PLAYLIST_JSON_MAX: usize = 8 * 1024 * 1024;
/// Cap imported/enqueued playlist tracks at the local-playlist song cap.
const PLAYLIST_TRACKS_MAX: usize = 999;

/// A YouTube Music client in one of two auth modes.
pub enum YtMusicApi {
    Browser(YtMusic<BrowserToken>),
    Anonymous,
}

impl YtMusicApi {
    /// Authenticate with a raw browser `Cookie:` header.
    pub async fn from_cookie(cookie: &str) -> Result<Self> {
        // A cookies.txt exported without being signed in carries only visitor cookies
        // (PREF/SOCS/YSC/…). ytmapi-rs would fail with an opaque "Error parsing header";
        // say what's actually wrong instead.
        if !cookie.contains("SAPISID=") {
            bail!(
                "the cookie has no login session (no SAPISID) — sign in to music.youtube.com \
                 in your browser, then export cookies.txt again"
            );
        }
        // ytmapi-rs extracts SAPISID by scanning for the `;` after its value; append one
        // so a cookie string that happens to END with SAPISID still parses.
        let cookie = if cookie.trim_end().ends_with(';') {
            cookie.trim_end().to_owned()
        } else {
            format!("{};", cookie.trim_end())
        };
        let client = YtMusic::from_cookie(&cookie)
            .await
            .context("YouTube Music cookie authentication failed")?;
        Ok(Self::Browser(client))
    }

    /// The authenticated client, or the one error every account operation shares.
    /// Anonymous mode can play and search, but reading/writing the user's library
    /// requires the cookie.
    fn browser(&self) -> Result<&YtMusic<BrowserToken>> {
        match self {
            Self::Browser(c) => Ok(c),
            Self::Anonymous => bail!(
                "this needs a YouTube Music cookie — add cookies.txt (or `cookie`) in Settings › General"
            ),
        }
    }

    // Account playlist/library operations (the transfer feature) ---------------------

    /// The user's own playlists as `(id, title, track-count-string)`.
    pub async fn library_playlists(&self) -> Result<Vec<(String, String, String)>> {
        let playlists = self
            .browser()?
            .get_library_playlists()
            .await
            .context("listing YouTube Music playlists failed")?;
        Ok(playlists
            .into_iter()
            .map(|p| (p.playlist_id.get_raw().to_owned(), p.title, p.tracks))
            .collect())
    }

    /// A playlist's playable tracks in order, with the album/duration enrichment the
    /// matcher wants. Episodes and unavailable entries are skipped.
    pub async fn playlist_tracks_full(&self, playlist_id: &str) -> Result<Vec<Song>> {
        use ytmapi_rs::parse::PlaylistItem;
        let items = self
            .browser()?
            .get_playlist_tracks(ytmapi_rs::common::PlaylistID::from_raw(playlist_id))
            .await
            .context("fetching YouTube Music playlist tracks failed")?;
        Ok(items
            .into_iter()
            .filter_map(|item| match item {
                PlaylistItem::Song(s) => {
                    if !s.is_available {
                        return None;
                    }
                    let artist = s
                        .artists
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Some(Song::from_search(
                        s.video_id.get_raw(),
                        s.title,
                        artist,
                        s.duration,
                        Some(s.album.name),
                    ))
                }
                PlaylistItem::Video(v) => {
                    if !v.is_available {
                        return None;
                    }
                    Some(Song::from_search(
                        v.video_id.get_raw(),
                        v.title,
                        v.channel_name,
                        v.duration,
                        None,
                    ))
                }
                PlaylistItem::Episode(_) | PlaylistItem::UploadSong(_) => None,
            })
            .collect())
    }

    /// Create a private playlist in the user's account; returns its id.
    pub async fn create_account_playlist(&self, title: &str, description: &str) -> Result<String> {
        use ytmapi_rs::query::playlist::{CreatePlaylistQuery, PrivacyStatus};
        let id = self
            .browser()?
            .create_playlist(CreatePlaylistQuery::new(
                title,
                Some(description),
                PrivacyStatus::Private,
            ))
            .await
            .context("creating the YouTube Music playlist failed")?;
        Ok(id.get_raw().to_owned())
    }

    /// Append tracks (order preserved within the call). Caller chunks to a polite size.
    pub async fn add_items_to_account_playlist(
        &self,
        playlist_id: &str,
        video_ids: &[String],
    ) -> Result<()> {
        if video_ids.is_empty() {
            return Ok(());
        }
        self.browser()?
            .add_video_items_to_playlist(
                ytmapi_rs::common::PlaylistID::from_raw(playlist_id),
                video_ids.iter().map(|id| VideoID::from_raw(id.as_str())),
            )
            .await
            .context("adding tracks to the YouTube Music playlist failed")?;
        Ok(())
    }

    /// Like a song (adds it to the account's Liked Music). Idempotent server-side.
    pub async fn rate_song_liked(&self, video_id: &str) -> Result<()> {
        self.browser()?
            .rate_song(
                VideoID::from_raw(video_id),
                ytmapi_rs::common::LikeStatus::Liked,
            )
            .await
            .context("liking the song on YouTube Music failed")?;
        Ok(())
    }

    /// Search for songs matching `query`, using the backend for this mode. Returns up to
    /// [`SEARCH_RESULT_LIMIT`] tracks.
    pub async fn search_songs(
        &self,
        query: &str,
        source: SearchSource,
        config: &SearchConfig,
    ) -> Result<Vec<Song>> {
        // A pasted YouTube watch/share URL is not a text query: resolve that exact video
        // and return it as the only result, whatever source is selected (the URL already
        // names the provider). Metadata comes from yt-dlp; a failed lookup still yields
        // a playable bare entry (mpv resolves the id at load time).
        if let Some(id) = crate::media::parse_youtube_playlist_id(query) {
            return Ok(vec![lookup_playlist_row(&id).await]);
        }
        if let Some(id) = crate::media::parse_youtube_video_id(query) {
            return Ok(vec![lookup_video_song(&id).await]);
        }
        match source {
            SearchSource::All => self.search_all_sources(query, config).await,
            source => self.search_one_source(query, source, config).await,
        }
    }

    /// Search public YouTube playlists by name. Authenticated innertube (community
    /// playlists) answers first; anonymous or degraded sessions fall back to a flat
    /// yt-dlp extraction of YouTube's own results page with the playlist-type filter.
    pub async fn search_playlists(&self, query: &str) -> Result<Vec<Song>> {
        // A pasted playlist URL names the playlist directly — same short-circuit as
        // `search_songs`, so the kind toggle doesn't change what a URL paste means.
        if let Some(id) = crate::media::parse_youtube_playlist_id(query) {
            return Ok(vec![lookup_playlist_row(&id).await]);
        }
        if let YtMusicApi::Browser(client) = self
            && !AUTH_SEARCH_DEGRADED.load(std::sync::atomic::Ordering::Relaxed)
        {
            match client.search_community_playlists(query).await {
                Ok(results) if !results.is_empty() => {
                    return Ok(results.into_iter().filter_map(playlist_row).collect());
                }
                Ok(_) => {}
                Err(e) => {
                    let error = sanitize::sanitize_error_text(format!("{e:#}"));
                    tracing::warn!(error = %error, "innertube playlist search failed; trying yt-dlp");
                }
            }
        }
        ytdlp_playlist_search(query).await
    }

    /// A remote playlist's playable tracks. Authenticated sessions ask innertube (rich
    /// album/duration metadata); anonymous sessions — or an innertube miss — use a flat
    /// yt-dlp extraction of the public playlist page.
    pub async fn playlist_tracks(&self, playlist_id: &str) -> Result<Vec<Song>> {
        let raw = playlist_id
            .strip_prefix(super::PLAYLIST_ID_PREFIX)
            .unwrap_or(playlist_id);
        if matches!(self, YtMusicApi::Browser(_)) {
            match self.playlist_tracks_full(raw).await {
                Ok(songs) if !songs.is_empty() => return Ok(songs),
                Ok(_) => {}
                Err(e) => {
                    let error = sanitize::sanitize_error_text(format!("{e:#}"));
                    tracing::warn!(error = %error, "innertube playlist fetch failed; trying yt-dlp");
                }
            }
        }
        ytdlp_playlist_tracks(raw).await
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
                    let error = sanitize::sanitize_error_text(format!("{e:#}"));
                    tracing::warn!(source = %source.code(), error = %error, "source search failed");
                    errors.push(format!("{}: {error}", source.code()));
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
        // Once one authenticated search comes back empty/unparseable this process, they
        // all will (Google gates innertube search behind browser attestation as of
        // mid-2026) — skip the wasted round-trip and go straight to yt-dlp.
        if AUTH_SEARCH_DEGRADED.load(std::sync::atomic::Ordering::Relaxed) {
            return ytdlp_search(query, SEARCH_RESULT_LIMIT).await;
        }
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
                    let page = match page {
                        Ok(page) => page,
                        // ytmapi-rs response parsers rot as YouTube shifts layouts
                        // (0.3.2 is current upstream). Degrade instead of failing the
                        // search: keep whatever pages parsed; with nothing at all,
                        // fall back to the anonymous yt-dlp path (no album metadata,
                        // but results).
                        Err(e) if songs.is_empty() => {
                            AUTH_SEARCH_DEGRADED.store(true, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!(
                                error = %sanitize::sanitize_error_text(format!("{e:#}")),
                                "authenticated search parse failed; using yt-dlp for the rest of this session"
                            );
                            return ytdlp_search(query, SEARCH_RESULT_LIMIT).await;
                        }
                        Err(_) => break,
                    };
                    for s in page {
                        songs.push(Song::from_search(
                            s.video_id.get_raw(),
                            s.title,
                            s.artist,
                            s.duration,
                            s.album.map(|a| a.name),
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

    /// The upstream YouTube Music watch-playlist continuation for a seed track.
    /// (`get_watch_playlist_from_video_id`) — YTM's own "up next" mix, far better seeded than a
    /// blind text search. Authenticated uses the logged-in client; anonymous spins up an
    /// unauthenticated client (the query isn't login-gated, though YTM may still return nothing
    /// without a cookie — the caller treats an error/empty result as "fall back to yt-dlp").
    pub(crate) async fn streaming_continuation(&self, seed_video_id: &str) -> Result<Vec<Song>> {
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

async fn search_external_source(
    source: SearchSource,
    query: &str,
    config: &SearchConfig,
    limit: usize,
) -> Result<Vec<Song>> {
    match source {
        SearchSource::SoundCloud => {
            ytdlp_flat_search(SearchSource::SoundCloud, "scsearch", query, limit).await
        }
        SearchSource::Audius => audius_search(query, config, limit).await,
        SearchSource::Jamendo => jamendo_search(query, config, limit).await,
        SearchSource::InternetArchive => archive_search(query, limit).await,
        SearchSource::Youtube => ytdlp_search(query, limit).await,
        SearchSource::RadioBrowser | SearchSource::All => {
            bail!("{} is not a track recommendation source", source.label())
        }
    }
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
    let mut cmd = process::tokio_command("yt-dlp", process::ProcessProfile::YtDlp);
    cmd.arg(&spec)
        .arg("--flat-playlist")
        .arg("--dump-single-json")
        .arg("--no-warnings")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let output = process::tokio_output_limited(cmd, YTDLP_SEARCH_TIMEOUT, YTDLP_JSON_MAX)
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

/// Best-effort related tracks for streaming/autoplay without Gemini.
///
/// There is no stable public recommendation API in the app today, so the anonymous
/// fallback uses the same yt-dlp search boundary as normal anonymous search. It asks for
/// related-search query variants and de-dupes against the caller's exclusions.
pub(crate) async fn related_tracks(
    seed: &str,
    limit: usize,
    excluded: &HashSet<String>,
    mode: StreamingMode,
) -> Result<Vec<Song>> {
    // Allow up to 50 so the local streaming engine gets a real candidate pool to rank (the
    // engine, not this fetch, decides the final few picks).
    let limit = limit.clamp(1, 50);
    let mut out = Vec::with_capacity(limit);
    let mut accepted_ids = excluded.clone();
    let mut had_success = false;
    let mut last_err = None;

    for query in streaming_queries(seed, mode) {
        let search_limit = (limit * 2).clamp(limit, 50);
        match ytdlp_search(&query, search_limit).await {
            Ok(songs) => {
                had_success = true;
                for song in songs {
                    if accepted_ids.insert(song.video_id.clone()) {
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

/// Related-track search through one configured Search-screen source.
///
/// This is intentionally search-based rather than a provider-specific recommendation API: the app
/// already has playable search adapters for these sources, while recommendation endpoints differ
/// wildly by provider or do not exist. The local streaming engine still ranks and filters the
/// merged pool before anything is queued.
pub(crate) async fn related_tracks_from_source(
    seed: &str,
    source: SearchSource,
    config: &SearchConfig,
    limit: usize,
    excluded: &HashSet<String>,
    mode: StreamingMode,
) -> Result<Vec<Song>> {
    match source {
        SearchSource::Youtube => related_tracks(seed, limit, excluded, mode).await,
        SearchSource::SoundCloud
        | SearchSource::Audius
        | SearchSource::Jamendo
        | SearchSource::InternetArchive => {
            if !config.is_enabled(source) {
                bail!("{} is disabled in Settings → General", source.label());
            }
            let limit = limit.clamp(1, 50);
            let mut out = Vec::with_capacity(limit);
            let mut accepted_ids = excluded.clone();
            let mut had_success = false;
            let mut last_err = None;

            for query in streaming_queries(seed, mode) {
                let search_limit = (limit * 2).clamp(limit, 50);
                match search_external_source(source, &query, config, search_limit).await {
                    Ok(songs) => {
                        had_success = true;
                        for song in songs {
                            if accepted_ids.insert(song.video_id.clone()) {
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
                return Err(e).context("provider related-track search failed");
            }
            Ok(out)
        }
        SearchSource::RadioBrowser => {
            bail!("Radio Browser streams are not used for track recommendations")
        }
        SearchSource::All => bail!("internal error: nested ALL streaming source search"),
    }
}

/// Final streaming safety pass for public-YouTube candidates. Cheap title/channel checks have
/// already run in the reducer; this only does full yt-dlp metadata extraction for candidates
/// whose title/channel/duration made them risky, then tops up from fallback picks.
pub(crate) async fn preflight_streaming_picks(
    picks: Vec<Song>,
    fallback: Vec<Song>,
    mode: StreamingMode,
    cfg: &StreamingConfig,
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
        if streaming::sanitize_final_picks(vec![song.clone()], &[], mode, cfg).is_empty() {
            continue;
        }
        if streaming::needs_metadata_preflight(song, mode, cfg) {
            let risk = streaming::musicgate::non_music_risk_score(&song.title, &song.artist);
            match song.youtube_id().map(enrich_video_meta) {
                Some(fut) => match fut.await {
                    Ok(meta) => {
                        if reject_enriched(&meta, mode, cfg) {
                            tracing::debug!(
                                id = %song.video_id,
                                title = %song.title,
                                "streaming preflight rejected candidate"
                            );
                            continue;
                        }
                    }
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(
                            id = %song.video_id,
                            error = %error,
                            "streaming preflight metadata lookup failed"
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

/// Map one innertube playlist search result to a `ytpl:` row. The views / track-count
/// string rides in the duration slot (rows render it in parentheses).
fn playlist_row(result: ytmapi_rs::parse::SearchResultPlaylist) -> Option<Song> {
    use ytmapi_rs::parse::SearchResultPlaylist as P;
    let (title, author, extra, id) = match result {
        P::Community(p) => (p.title, p.author, p.views, p.playlist_id),
        P::Featured(p) => (p.title, p.author, p.songs, p.playlist_id),
        _ => return None, // podcasts (and future kinds) aren't playable track lists here
    };
    Some(Song::remote(
        format!("{}{}", super::PLAYLIST_ID_PREFIX, id.get_raw()),
        title,
        author,
        extra,
    ))
}

/// Anonymous playlist search: YouTube's own results page with the playlist-type filter
/// (`sp=EgIQAw==`), flat-extracted by yt-dlp — the only playlist search available
/// without innertube auth.
async fn ytdlp_playlist_search(query: &str) -> Result<Vec<Song>> {
    let url = reqwest::Url::parse_with_params(
        "https://www.youtube.com/results",
        &[("search_query", query), ("sp", "EgIQAw==")],
    )
    .context("could not build the playlist search URL")?;
    let mut cmd = process::tokio_command("yt-dlp", process::ProcessProfile::YtDlp);
    cmd.arg(url.as_str())
        .arg("--flat-playlist")
        .arg("--dump-single-json")
        .arg("--no-warnings")
        .arg("--playlist-end")
        .arg("20")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let output = process::tokio_output_limited(cmd, YTDLP_SEARCH_TIMEOUT, YTDLP_JSON_MAX)
        .await
        .context("failed to run yt-dlp — is it installed and on PATH?")?;
    if !output.status.success() {
        bail!(
            "yt-dlp playlist search exited with status {}",
            output.status
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("could not parse yt-dlp playlist search output")?;
    Ok(parse_ytdlp_playlist_search(&json))
}

/// Entries of a flat-extracted results page → playlist rows. A filtered results page
/// can still interleave videos, so entries are kept only when they look like playlists
/// (a `list=` URL or a playlist-shaped id — video ids are 11 chars).
fn parse_ytdlp_playlist_search(json: &serde_json::Value) -> Vec<Song> {
    let entries = json
        .get("entries")
        .and_then(|e| e.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(serde_json::Value::as_str)?;
            let url = entry
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if !url.contains("list=") && id.len() <= 16 {
                return None;
            }
            let title = entry
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if title.trim().is_empty() {
                return None;
            }
            let author = json_string(entry, &["channel", "uploader"]).unwrap_or_default();
            let count = entry
                .get("playlist_count")
                .and_then(serde_json::Value::as_u64)
                .map(|n| format!("{n} tracks"))
                .unwrap_or_default();
            Some(Song::remote(
                format!("{}{id}", super::PLAYLIST_ID_PREFIX),
                title,
                author,
                count,
            ))
        })
        .collect()
}

/// Flat yt-dlp extraction of a public playlist page → its tracks in order.
async fn ytdlp_playlist_tracks(playlist_id: &str) -> Result<Vec<Song>> {
    let json = ytdlp_playlist_json(playlist_id, None).await?;
    let entries = json
        .get("entries")
        .and_then(|e| e.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(entries
        .iter()
        .filter_map(parse_ytdlp_playlist_track)
        .take(PLAYLIST_TRACKS_MAX)
        .collect())
}

/// One flat playlist entry → a track row; private/deleted placeholders are skipped.
fn parse_ytdlp_playlist_track(entry: &serde_json::Value) -> Option<Song> {
    let id = entry.get("id").and_then(serde_json::Value::as_str)?;
    let title = entry.get("title").and_then(serde_json::Value::as_str)?;
    if title.is_empty() || title == "[Private video]" || title == "[Deleted video]" {
        return None;
    }
    let artist = json_string(entry, &["channel", "uploader"]).unwrap_or_default();
    let duration = entry
        .get("duration")
        .and_then(serde_json::Value::as_f64)
        .filter(|d| d.is_finite() && *d > 0.0)
        .map(format::time)
        .unwrap_or_default();
    Some(Song::from_search(id, title, artist, duration, None))
}

/// One pasted playlist URL → a single playlist row. Failure degrades to a bare row —
/// the id is what makes it fetchable, the title is only the label.
async fn lookup_playlist_row(playlist_id: &str) -> Song {
    let row_id = format!("{}{playlist_id}", super::PLAYLIST_ID_PREFIX);
    match ytdlp_playlist_json(playlist_id, Some("0")).await {
        Ok(json) => {
            let title = json_string(&json, &["title"])
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| format!("YouTube playlist {playlist_id}"));
            let author = json_string(&json, &["channel", "uploader"]).unwrap_or_default();
            let count = json
                .get("playlist_count")
                .and_then(serde_json::Value::as_u64)
                .map(|n| format!("{n} tracks"))
                .unwrap_or_default();
            Song::remote(row_id, title, author, count)
        }
        Err(e) => {
            let error = sanitize::sanitize_error_text(format!("{e:#}"));
            tracing::warn!(id = %playlist_id, error = %error, "pasted-URL playlist lookup failed");
            Song::remote(row_id, format!("YouTube playlist {playlist_id}"), "", "")
        }
    }
}

/// Flat-extract a public playlist page. `items` limits extraction (`"0"` → metadata
/// only, for the fast title probe). Innertube browse ids ("VLPL…") and share URLs
/// ("PL…") differ by the VL prefix; the public page wants the bare form.
async fn ytdlp_playlist_json(playlist_id: &str, items: Option<&str>) -> Result<serde_json::Value> {
    let id = playlist_id.strip_prefix("VL").unwrap_or(playlist_id);
    let url = format!("https://www.youtube.com/playlist?list={id}");
    let mut cmd = process::tokio_command("yt-dlp", process::ProcessProfile::YtDlp);
    cmd.arg(&url)
        .arg("--flat-playlist")
        .arg("--dump-single-json")
        .arg("--no-warnings")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(items) = items {
        cmd.arg("--playlist-items").arg(items);
    }
    let output = process::tokio_output_limited(cmd, PLAYLIST_FETCH_TIMEOUT, PLAYLIST_JSON_MAX)
        .await
        .context("failed to run yt-dlp — is it installed and on PATH?")?;
    if !output.status.success() {
        bail!(
            "yt-dlp playlist extraction exited with status {}",
            output.status
        );
    }
    serde_json::from_slice(&output.stdout).context("could not parse yt-dlp playlist output")
}

/// Resolve one pasted watch/share URL's video id into a full search row. Failure
/// degrades to a bare-but-playable entry instead of an error: the id itself is what
/// makes the row playable, the metadata is only the label.
async fn lookup_video_song(video_id: &str) -> Song {
    match enrich_video_meta(video_id).await {
        Ok(meta) if !meta.title.trim().is_empty() => {
            let duration = meta
                .duration_secs
                .map(|s| format::time(f64::from(s)))
                .unwrap_or_default();
            Song::from_search(video_id, meta.title, meta.channel, duration, None)
        }
        Ok(_) => Song::remote(video_id, format!("YouTube {video_id}"), "", ""),
        Err(e) => {
            let error = sanitize::sanitize_error_text(format!("{e:#}"));
            tracing::warn!(id = %video_id, error = %error, "pasted-URL metadata lookup failed");
            Song::remote(video_id, format!("YouTube {video_id}"), "", "")
        }
    }
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
    let mut cmd = process::tokio_command("yt-dlp", process::ProcessProfile::YtDlp);
    cmd.arg("--dump-single-json")
        .arg("--no-playlist")
        .arg("--no-warnings")
        .arg(&url)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let output = process::tokio_output_limited(cmd, STREAMING_PREFLIGHT_TIMEOUT, YTDLP_JSON_MAX)
        .await
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

fn reject_enriched(meta: &EnrichedVideoMeta, mode: StreamingMode, cfg: &StreamingConfig) -> bool {
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
            StreamingMode::Focused => 8 * 60,
            StreamingMode::Balanced => 12 * 60,
            StreamingMode::Discovery => 15 * 60,
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
    let decision = streaming::musicgate::decide(
        &rich_title,
        &meta.channel,
        streaming::CandidateSource::YtdlpStreaming,
        mode,
    );
    if decision.action == streaming::musicgate::GateAction::Reject {
        return true;
    }
    let risk = streaming::musicgate::non_music_risk_score(&rich_title, &meta.channel);
    let music_tier = streaming::musicgate::music_tier_score(&meta.title, &meta.channel);
    if mode == StreamingMode::Focused && decision.action == streaming::musicgate::GateAction::Demote
    {
        return true;
    }
    risk >= 0.70 && music_tier <= 0.0 && meta.was_live != Some(true)
}

fn streaming_queries(seed: &str, mode: StreamingMode) -> Vec<String> {
    let seed = seed.trim();
    if seed.is_empty() {
        return match mode {
            StreamingMode::Focused => vec![
                "popular songs official audio".to_owned(),
                "popular music official video".to_owned(),
            ],
            StreamingMode::Balanced => {
                vec!["popular music radio".to_owned(), "popular songs".to_owned()]
            }
            StreamingMode::Discovery => vec![
                "new music similar songs".to_owned(),
                "popular music radio".to_owned(),
                "deep cuts songs".to_owned(),
            ],
        };
    }

    // Note: no "… mix" queries — those pull 1-hour compilations / megamixes that the streaming
    // engine then has to filter out. The literal "… radio" search term surfaces individual tracks.
    let mut queries = Vec::new();

    if let Some((title, artist)) = split_seed(seed) {
        match mode {
            StreamingMode::Focused => {
                push_query(&mut queries, format!("{title} {artist} official audio"));
                push_query(&mut queries, format!("{title} {artist} official video"));
                push_query(&mut queries, format!("{artist} songs"));
                push_query(&mut queries, format!("{artist} radio"));
                push_query(&mut queries, format!("{title} {artist} song"));
            }
            StreamingMode::Balanced => {
                push_query(&mut queries, format!("{seed} radio"));
                push_query(&mut queries, format!("{artist} radio"));
                push_query(&mut queries, format!("{artist} songs"));
                push_query(&mut queries, format!("{artist} similar songs"));
                push_query(&mut queries, format!("{title} {artist}"));
            }
            StreamingMode::Discovery => {
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
            StreamingMode::Focused => {
                push_query(&mut queries, format!("{seed} official audio"));
                push_query(&mut queries, format!("{seed} official video"));
                push_query(&mut queries, format!("{seed} song"));
            }
            StreamingMode::Balanced => {
                push_query(&mut queries, format!("{seed} radio"));
                push_query(&mut queries, format!("{seed} songs"));
                push_query(&mut queries, format!("{seed} similar songs"));
            }
            StreamingMode::Discovery => {
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
    let resp = client
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
        .context("Audius search returned an error")?;
    let json: serde_json::Value = http::json_limited(resp, PROVIDER_JSON_MAX)
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
    let resp = client
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
        .context("Jamendo search returned an error")?;
    let json: serde_json::Value = http::json_limited(resp, PROVIDER_JSON_MAX)
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
    let resp = client
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
        .context("Internet Archive search returned an error")?;
    let json: serde_json::Value = http::json_limited(resp, PROVIDER_JSON_MAX)
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
    let resp = client
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
        .context("Radio Browser search returned an error")?;
    let json: serde_json::Value = http::json_limited(resp, PROVIDER_JSON_MAX)
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
    let resp = client.get(url).send().await.ok()?.error_for_status().ok()?;
    let json: serde_json::Value = http::json_limited(resp, PROVIDER_JSON_MAX).await.ok()?;
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
    fn playlist_search_entries_keep_playlists_and_drop_videos() {
        let json = serde_json::json!({
            "entries": [
                {
                    "id": "PLabcdefgh1234567890abcdefgh12345",
                    "url": "https://www.youtube.com/playlist?list=PLabcdefgh1234567890abcdefgh12345",
                    "title": "Chill Mix",
                    "uploader": "Some Curator",
                    "playlist_count": 42
                },
                // An interleaved plain video (11-char id, no list=): dropped.
                { "id": "abc12345678", "url": "https://www.youtube.com/watch?v=abc12345678", "title": "A video" },
                // Untitled playlist entry: dropped.
                { "id": "PLzz", "url": "https://www.youtube.com/playlist?list=PLzz", "title": "" }
            ]
        });
        let rows = parse_ytdlp_playlist_search(&json);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].video_id, "ytpl:PLabcdefgh1234567890abcdefgh12345");
        assert_eq!(rows[0].title, "Chill Mix");
        assert_eq!(rows[0].artist, "Some Curator");
        assert_eq!(rows[0].duration, "42 tracks");
        assert_eq!(
            rows[0].youtube_playlist_id(),
            Some("PLabcdefgh1234567890abcdefgh12345")
        );
        // A playlist row must never read as a playable YouTube video.
        assert_eq!(rows[0].youtube_id(), None);
    }

    #[test]
    fn playlist_track_entries_skip_private_and_format_duration() {
        let track = parse_ytdlp_playlist_track(&serde_json::json!({
            "id": "abc12345678",
            "title": "A Song",
            "channel": "An Artist",
            "duration": 245.0
        }))
        .expect("a playable track");
        assert_eq!(track.video_id, "abc12345678");
        assert_eq!(track.duration, "4:05");
        assert_eq!(track.duration_secs, Some(245));
        for title in ["[Private video]", "[Deleted video]", ""] {
            assert!(
                parse_ytdlp_playlist_track(&serde_json::json!({
                    "id": "abc12345678",
                    "title": title,
                }))
                .is_none()
            );
        }
    }

    #[test]
    fn streaming_queries_expand_title_artist_seed() {
        let queries = streaming_queries("Song — Artist", StreamingMode::Balanced);
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
    fn streaming_queries_handle_plain_seed() {
        let queries = streaming_queries("lo-fi beats", StreamingMode::Balanced);
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
    fn streaming_queries_are_mode_specific() {
        let focused = streaming_queries("Song — Artist", StreamingMode::Focused);
        assert_eq!(focused[0], "Song Artist official audio");
        assert!(focused.iter().any(|q| q.contains("official video")));

        let discovery = streaming_queries("Song — Artist", StreamingMode::Discovery);
        assert_eq!(discovery[0], "Artist similar songs");
        assert!(discovery.iter().any(|q| q.contains("deep cuts")));
        assert!(!discovery.iter().any(|q| q.contains(" mix")));
    }

    #[test]
    fn preflight_metadata_rejects_live_and_long_non_music() {
        let cfg = StreamingConfig::default();
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
        assert!(reject_enriched(&meta, StreamingMode::Balanced, &cfg));

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
        assert!(reject_enriched(&meta, StreamingMode::Discovery, &cfg));
    }

    #[test]
    fn preflight_metadata_keeps_trusted_music_track() {
        let cfg = StreamingConfig::default();
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
        assert!(!reject_enriched(&meta, StreamingMode::Focused, &cfg));
    }
}
