//! YouTube Music data layer. Search is served by ytmapi-rs when authenticated and by
//! yt-dlp (`ytsearch`) when anonymous — see [`ytmusic`]. A `MusicApi` trait + raw-Innertube
//! fallback arrive when home/charts need endpoints ytmapi-rs lacks.

pub mod ytmusic;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::app::Msg;
use crate::search_source::{SearchConfig, SearchSource};
use crate::streaming::{CandidateSource, StreamingConfig, StreamingMode};
use crate::util::sanitize;

const STREAMING_YTDLP_CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// A search result track, trimmed to what the UI needs. Serializable so the local
/// library (favorites/history) can persist tracks verbatim. `local_path` is set only for
/// downloaded/local audio files; old persisted JSON omits it and still deserializes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Song {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    /// Pre-formatted duration string (e.g. "3:45").
    pub duration: String,
    /// The provider this result came from. Defaults to YouTube for old persisted JSON.
    #[serde(default, skip_serializing_if = "SearchSource::is_youtube")]
    pub source: SearchSource,
    /// Non-YouTube playback target. Old/pure YouTube tracks omit it and use `video_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playable: Option<PlayableRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<PathBuf>,
    /// The original YouTube video ID, preserved when a catalog track is downloaded and its
    /// `video_id` becomes a `local:` identity. `None` for pure local files and for remote
    /// tracks (whose `video_id` is already the YouTube ID). Old persisted JSON omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yt_video_id: Option<String>,
}

/// How a non-local track should be handed to mpv/yt-dlp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PlayableRef {
    YoutubeVideo {
        id: String,
    },
    DirectUrl {
        source: SearchSource,
        url: String,
    },
    YtdlpUrl {
        source: SearchSource,
        url: String,
    },
    AudiusTrackId {
        id: String,
        app_name: String,
    },
    JamendoTrackId {
        id: String,
        url: String,
    },
    ArchiveFile {
        identifier: String,
        file: String,
        url: String,
    },
    RadioStream {
        url: String,
    },
}

impl Song {
    pub fn remote(
        video_id: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
        duration: impl Into<String>,
    ) -> Self {
        Self {
            video_id: video_id.into(),
            title: title.into(),
            artist: artist.into(),
            duration: duration.into(),
            source: SearchSource::Youtube,
            playable: None,
            local_path: None,
            yt_video_id: None,
        }
    }

    pub fn from_source(
        source: SearchSource,
        source_id: impl Into<String>,
        title: impl Into<String>,
        artist: impl Into<String>,
        duration: impl Into<String>,
        playable: PlayableRef,
    ) -> Self {
        let source_id = source_id.into();
        let video_id = if source == SearchSource::Youtube {
            source_id
        } else {
            format!("{}:{source_id}", source.id_prefix())
        };
        Self {
            video_id,
            title: title.into(),
            artist: artist.into(),
            duration: duration.into(),
            source,
            playable: Some(playable),
            local_path: None,
            yt_video_id: None,
        }
    }

    /// Build a locally playable track from a file path. Rich metadata parsing is intentionally
    /// avoided, but a YouTube id embedded in the filename by our downloader (`Title [<id>].m4a`,
    /// see [`crate::download`]) is recovered into `yt_video_id` and stripped from the displayed
    /// title — this is what lets a downloaded-and-online track still produce its share URL after
    /// a restart. Plain/foreign filenames are unaffected. Session downloads can additionally
    /// preserve richer metadata via [`Self::with_local_path`].
    pub fn local_file(path: PathBuf) -> Self {
        let stem = path.file_stem().and_then(|s| s.to_str());
        let (title, yt_video_id) = match stem.and_then(Self::parse_embedded_id) {
            Some((title, id)) => (title.to_owned(), Some(id.to_owned())),
            None => (
                stem.filter(|s| !s.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| path.display().to_string()),
                None,
            ),
        };
        Self {
            video_id: Self::local_id(&path),
            title,
            artist: "Local file".to_owned(),
            duration: String::new(),
            source: SearchSource::Youtube,
            playable: None,
            local_path: Some(path),
            yt_video_id,
        }
    }

    /// Parse a trailing ` [<id>]` YouTube-id tag off a file stem, returning
    /// `(title_without_tag, id)`. The id must be exactly 11 URL-safe base64 chars (the YouTube
    /// video-id shape), which makes accidental matches on bracketed titles like `Mix [Vol. 3]`
    /// effectively impossible. Returns `None` when there is no tag or no real title before it.
    pub(crate) fn parse_embedded_id(stem: &str) -> Option<(&str, &str)> {
        let rest = stem.trim_end().strip_suffix(']')?;
        let open = rest.rfind('[')?;
        let id = &rest[open + 1..];
        let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        if id.len() != 11 || !id.bytes().all(is_id_char) {
            return None;
        }
        let title = rest[..open].trim_end();
        (!title.is_empty()).then_some((title, id))
    }

    /// Preserve known catalog metadata while making this entry play from `path`. The
    /// original YouTube ID is kept in `yt_video_id` so the track still knows its online
    /// URL even though `video_id` becomes a `local:` identity.
    pub fn with_local_path(&self, path: PathBuf) -> Self {
        Self {
            video_id: Self::local_id(&path),
            title: self.title.clone(),
            artist: self.artist.clone(),
            duration: self.duration.clone(),
            source: self.source,
            playable: self.playable.clone(),
            local_path: Some(path),
            yt_video_id: self.youtube_id().map(str::to_owned),
        }
    }

    /// Attach a recovered YouTube id to an otherwise id-less local track (used when the id is
    /// restored from the download manifest or a title match rather than the filename).
    pub fn with_yt_id(mut self, id: String) -> Self {
        self.yt_video_id = Some(id);
        self
    }

    pub fn is_local(&self) -> bool {
        self.local_path.is_some()
    }

    /// True for live radio stations from Radio Browser. These are streams, not tracks, so they
    /// stay out of song history and recommendation signals.
    pub fn is_radio_station(&self) -> bool {
        self.source == SearchSource::RadioBrowser
            || matches!(&self.playable, Some(PlayableRef::RadioStream { .. }))
    }

    /// The real YouTube video ID for this track, if it originated from YouTube. Pure local
    /// files (dropped into the library, never sourced online) return `None`. Downloaded
    /// catalog tracks keep their original ID via `yt_video_id` even though `video_id`
    /// becomes a `local:` identity.
    pub fn youtube_id(&self) -> Option<&str> {
        if let Some(id) = self.yt_video_id.as_deref() {
            return Some(id);
        }
        if self.local_path.is_some() || self.source != SearchSource::Youtube {
            return None;
        }
        (!self.video_id.starts_with("local:")).then_some(self.video_id.as_str())
    }

    /// A shareable public URL for this track, if it has a YouTube origin.
    pub fn share_url(&self) -> Option<String> {
        self.youtube_id()
            .map(|id| format!("https://www.youtube.com/watch?v={id}"))
    }

    /// The string passed to mpv: either a local file path or a YouTube Music watch URL.
    pub fn playback_target(&self) -> String {
        self.local_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.watch_url())
    }

    /// A URL mpv/yt-dlp can resolve and play for catalog tracks.
    pub fn watch_url(&self) -> String {
        match &self.playable {
            Some(PlayableRef::YoutubeVideo { id }) => {
                format!("https://music.youtube.com/watch?v={id}")
            }
            Some(PlayableRef::DirectUrl { url, .. })
            | Some(PlayableRef::YtdlpUrl { url, .. })
            | Some(PlayableRef::JamendoTrackId { url, .. })
            | Some(PlayableRef::ArchiveFile { url, .. })
            | Some(PlayableRef::RadioStream { url }) => url.clone(),
            Some(PlayableRef::AudiusTrackId { id, app_name }) => audius_stream_url(id, app_name),
            None => format!("https://music.youtube.com/watch?v={}", self.video_id),
        }
    }

    /// Direct streams/radio URLs do not benefit from yt-dlp pre-resolution. YouTube and
    /// yt-dlp-backed sources do, so the skip-ahead resolver can ask only for those.
    pub fn prefetch_target(&self) -> Option<String> {
        if self.is_local() {
            return None;
        }
        match &self.playable {
            Some(PlayableRef::DirectUrl { .. })
            | Some(PlayableRef::JamendoTrackId { .. })
            | Some(PlayableRef::ArchiveFile { .. })
            | Some(PlayableRef::RadioStream { .. }) => None,
            Some(PlayableRef::YoutubeVideo { .. })
            | Some(PlayableRef::YtdlpUrl { .. })
            | Some(PlayableRef::AudiusTrackId { .. })
            | None => Some(self.watch_url()),
        }
    }

    fn local_id(path: &Path) -> String {
        let stable = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        format!("local:{}", stable.to_string_lossy())
    }
}

fn audius_stream_url(id: &str, app_name: &str) -> String {
    reqwest::Url::parse_with_params(
        &format!("https://discoveryprovider.audius.co/v1/tracks/{id}/stream"),
        &[("app_name", app_name)],
    )
    .map(|u| u.to_string())
    .unwrap_or_else(|_| {
        format!("https://discoveryprovider.audius.co/v1/tracks/{id}/stream?app_name={app_name}")
    })
}

/// Which auth mode the live API client ended up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiMode {
    Authenticated,
    Anonymous,
}

/// Commands the reducer sends to the API actor.
pub enum ApiCmd {
    Search {
        query: String,
        source: SearchSource,
        config: SearchConfig,
    },
    Streaming {
        seed: String,
        seed_video_id: String,
        exclude_ids: Vec<String>,
        limit: usize,
        mode: StreamingMode,
    },
    StreamingPreflight {
        seed_video_id: String,
        picks: Vec<Song>,
        fallback: Vec<Song>,
        mode: StreamingMode,
        config: StreamingConfig,
    },
}

/// Handle for issuing API requests; results return as [`Msg`]s.
pub struct ApiHandle {
    tx: UnboundedSender<ApiCmd>,
}

impl ApiHandle {
    pub fn search(&self, query: impl Into<String>, source: SearchSource, config: SearchConfig) {
        let _ = self.tx.send(ApiCmd::Search {
            query: query.into(),
            source,
            config,
        });
    }

    pub fn streaming(
        &self,
        seed: impl Into<String>,
        seed_video_id: impl Into<String>,
        exclude_ids: Vec<String>,
        limit: usize,
        mode: StreamingMode,
    ) {
        let _ = self.tx.send(ApiCmd::Streaming {
            seed: seed.into(),
            seed_video_id: seed_video_id.into(),
            exclude_ids,
            limit,
            mode,
        });
    }

    pub fn streaming_preflight(
        &self,
        seed_video_id: impl Into<String>,
        picks: Vec<Song>,
        fallback: Vec<Song>,
        mode: StreamingMode,
        config: StreamingConfig,
    ) {
        let _ = self.tx.send(ApiCmd::StreamingPreflight {
            seed_video_id: seed_video_id.into(),
            picks,
            fallback,
            mode,
            config,
        });
    }
}

/// Spawn the API actor, returning its handle immediately.
///
/// A configured cookie is tried first; if it's rejected we fall back to anonymous
/// (yt-dlp) search so search + public playback still work. With no cookie we go straight
/// to anonymous. Commands sent before authentication settles are buffered by the channel.
pub fn spawn(cookie: Option<String>, msg_tx: UnboundedSender<Msg>) -> ApiHandle {
    let had_cookie = cookie.is_some();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (api, mode) = init_api(cookie).await;
        let _ = msg_tx.send(Msg::ApiModeResolved { mode, had_cookie });
        run_actor(api, rx, msg_tx).await;
    });
    ApiHandle { tx }
}

async fn init_api(cookie: Option<String>) -> (ytmusic::YtMusicApi, ApiMode) {
    let (api, mode) = match cookie {
        Some(c) => match ytmusic::YtMusicApi::from_cookie(&c).await {
            Ok(api) => (api, ApiMode::Authenticated),
            Err(e) => {
                tracing::warn!(error = %sanitize::sanitize_error_text(format!("{e:#}")), "cookie auth failed; using anonymous search");
                (ytmusic::YtMusicApi::Anonymous, ApiMode::Anonymous)
            }
        },
        None => (ytmusic::YtMusicApi::Anonymous, ApiMode::Anonymous),
    };
    (api, mode)
}

async fn run_actor(
    api: ytmusic::YtMusicApi,
    mut rx: UnboundedReceiver<ApiCmd>,
    msg_tx: UnboundedSender<Msg>,
) {
    let mut streaming_ytdlp_cache: HashMap<(String, StreamingMode), (Instant, Vec<Song>)> =
        HashMap::new();
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ApiCmd::Search {
                query,
                source,
                config,
            } => {
                let msg = match api.search_songs(&query, source, &config).await {
                    Ok(songs) => {
                        tracing::info!(count = songs.len(), query = %query, source = %source.code(), "search results");
                        Msg::SearchResults {
                            query,
                            source,
                            songs,
                        }
                    }
                    Err(e) => {
                        let error = sanitize::sanitize_error_text(format!("{e:#}"));
                        tracing::warn!(source = %source.code(), error = %error, "search failed");
                        Msg::SearchError { source, error }
                    }
                };
                let _ = msg_tx.send(msg);
            }
            ApiCmd::Streaming {
                seed,
                seed_video_id,
                exclude_ids,
                limit,
                mode,
            } => {
                // Build one pool from two sources, tagged by provenance so the local engine can
                // weight them: YTM's own watch-playlist continuation first (best), then yt-dlp text search
                // to top up. `seen` carries the caller's exclusions and dedups across sources.
                let mut seen: HashSet<String> = exclude_ids.into_iter().collect();
                let mut candidates: Vec<(Song, CandidateSource)> = Vec::new();

                match api.streaming_continuation(&seed_video_id).await {
                    Ok(songs) => {
                        for s in songs {
                            if seen.insert(s.video_id.clone()) {
                                candidates.push((s, CandidateSource::WatchPlaylist));
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %sanitize::sanitize_error_text(format!("{e:#}")), "watch-playlist streaming unavailable; using yt-dlp only");
                    }
                }

                // Top up the remainder from yt-dlp (skip entirely if the continuation already filled
                // the pool). Only this source's failure can fail the whole request.
                let want = limit.saturating_sub(candidates.len());
                let mut ytdlp_err = None;
                if want > 0 {
                    let now = Instant::now();
                    streaming_ytdlp_cache.retain(|_, (stored, _)| {
                        now.duration_since(*stored) < STREAMING_YTDLP_CACHE_TTL
                    });
                    let cache_key = (seed.clone(), mode);
                    let cached = streaming_ytdlp_cache
                        .get(&cache_key)
                        .filter(|(stored, _)| {
                            now.duration_since(*stored) < STREAMING_YTDLP_CACHE_TTL
                        })
                        .map(|(_, songs)| songs.clone());
                    let ytdlp_pool = match cached {
                        Some(songs) => Ok(songs),
                        None => {
                            let empty = HashSet::new();
                            match ytmusic::related_tracks(&seed, limit, &empty, mode).await {
                                Ok(songs) => {
                                    streaming_ytdlp_cache.insert(cache_key, (now, songs.clone()));
                                    Ok(songs)
                                }
                                Err(e) => Err(e),
                            }
                        }
                    };
                    match ytdlp_pool {
                        Ok(songs) => {
                            for s in songs {
                                if seen.insert(s.video_id.clone()) {
                                    candidates.push((s, CandidateSource::YtdlpStreaming));
                                    if candidates.len() >= limit {
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => ytdlp_err = Some(e),
                    }
                }

                let msg = if candidates.is_empty() {
                    let error = ytdlp_err
                        .map(|e| sanitize::sanitize_error_text(format!("{e:#}")))
                        .unwrap_or_else(|| "no related tracks found".to_owned());
                    tracing::warn!(seed = %seed, %error, "streaming search yielded nothing");
                    Msg::StreamingError {
                        seed_video_id,
                        error,
                    }
                } else {
                    tracing::info!(count = candidates.len(), seed = %seed, "streaming results");
                    Msg::StreamingResults {
                        seed_video_id,
                        candidates,
                    }
                };
                let _ = msg_tx.send(msg);
            }
            ApiCmd::StreamingPreflight {
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => {
                let songs =
                    ytmusic::preflight_streaming_picks(picks, fallback, mode, &config).await;
                let _ = msg_tx.send(Msg::StreamingPreflighted {
                    seed_video_id,
                    songs,
                });
            }
        }
    }
}
