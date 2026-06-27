//! YouTube Music data layer. Search is served by ytmapi-rs when authenticated and by
//! yt-dlp (`ytsearch`) when anonymous — see [`ytmusic`]. A `MusicApi` trait + raw-Innertube
//! fallback arrive when home/charts need endpoints ytmapi-rs lacks.

pub mod ytmusic;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::app::Msg;

/// A search result track, trimmed to what the UI needs. Serializable so the local
/// library (favorites/history) can persist tracks verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Song {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    /// Pre-formatted duration string (e.g. "3:45").
    pub duration: String,
}

impl Song {
    /// A watch URL mpv/yt-dlp can resolve and play.
    pub fn watch_url(&self) -> String {
        format!("https://music.youtube.com/watch?v={}", self.video_id)
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
