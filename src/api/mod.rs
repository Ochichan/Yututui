//! YouTube Music data layer. Search is served by ytmapi-rs when authenticated and by
//! yt-dlp (`ytsearch`) when anonymous — see [`ytmusic`]. A `MusicApi` trait + raw-Innertube
//! fallback arrive when home/charts need endpoints ytmapi-rs lacks.

pub mod ytmusic;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::app::Msg;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<PathBuf>,
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
            local_path: None,
        }
    }

    /// Build a locally playable track from a file path. Metadata parsing is intentionally
    /// avoided here; downloaded tracks created during this session can preserve richer
    /// metadata via [`Self::with_local_path`].
    pub fn local_file(path: PathBuf) -> Self {
        let title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| path.display().to_string());
        Self {
            video_id: Self::local_id(&path),
            title,
            artist: "Local file".to_owned(),
            duration: String::new(),
            local_path: Some(path),
        }
    }

    /// Preserve known catalog metadata while making this entry play from `path`.
    pub fn with_local_path(&self, path: PathBuf) -> Self {
        Self {
            video_id: Self::local_id(&path),
            title: self.title.clone(),
            artist: self.artist.clone(),
            duration: self.duration.clone(),
            local_path: Some(path),
        }
    }

    pub fn is_local(&self) -> bool {
        self.local_path.is_some()
    }

    /// The string passed to mpv: either a local file path or a YouTube Music watch URL.
    pub fn playback_target(&self) -> String {
        self.local_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.watch_url())
    }

    /// A watch URL mpv/yt-dlp can resolve and play for catalog tracks.
    pub fn watch_url(&self) -> String {
        format!("https://music.youtube.com/watch?v={}", self.video_id)
    }

    fn local_id(path: &Path) -> String {
        let stable = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        format!("local:{}", stable.to_string_lossy())
    }
}

/// Which auth mode the live API client ended up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiMode {
    Authenticated,
    Anonymous,
}

/// Commands the reducer sends to the API actor.
pub enum ApiCmd {
    Search(String),
}

/// Handle for issuing API requests; results return as [`Msg`]s.
pub struct ApiHandle {
    tx: UnboundedSender<ApiCmd>,
}

impl ApiHandle {
    pub fn search(&self, query: impl Into<String>) {
        let _ = self.tx.send(ApiCmd::Search(query.into()));
    }
}

/// Spawn the API actor, returning its handle and the auth mode it settled on.
///
/// A configured cookie is tried first; if it's rejected we fall back to anonymous
/// (yt-dlp) search so search + public playback still work. With no cookie we go straight
/// to anonymous. Anonymous needs no network at startup, so this never fails.
pub async fn spawn(cookie: Option<String>, msg_tx: UnboundedSender<Msg>) -> (ApiHandle, ApiMode) {
    let (api, mode) = match cookie {
        Some(c) => match ytmusic::YtMusicApi::from_cookie(&c).await {
            Ok(api) => (api, ApiMode::Authenticated),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "cookie auth failed; using anonymous search");
                (ytmusic::YtMusicApi::Anonymous, ApiMode::Anonymous)
            }
        },
        None => (ytmusic::YtMusicApi::Anonymous, ApiMode::Anonymous),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(run_actor(api, rx, msg_tx));
    (ApiHandle { tx }, mode)
}

async fn run_actor(
    api: ytmusic::YtMusicApi,
    mut rx: UnboundedReceiver<ApiCmd>,
    msg_tx: UnboundedSender<Msg>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ApiCmd::Search(query) => {
                let msg = match api.search_songs(&query).await {
                    Ok(songs) => {
                        tracing::info!(count = songs.len(), query = %query, "search results");
                        Msg::SearchResults { query, songs }
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "search failed");
                        Msg::SearchError(format!("{e:#}"))
                    }
                };
                let _ = msg_tx.send(msg);
            }
        }
    }
}
