//! Commands accepted by the Gemini actor and events returned to an active owner.

use std::sync::Arc;

use crate::api::Song;
use crate::romanize::{RomanizeItem, RomanizedResult};

use super::dto::{AiContext, AiPick};
use super::model::GeminiModel;

/// Commands sent to the Gemini actor.
pub enum AiCmd {
    Ask {
        prompt: String,
        context: Box<AiContext>,
    },
    /// One-shot streaming rerank over a local candidate pack (the autoplay path); the model picks
    /// opaque cids the reducer resolves back to tracks.
    Rerank {
        seed_video_id: String,
        prompt: String,
    },
    /// Off-path: distill a recent-feedback digest into station avoid/boost artists.
    SummarizeFeedback { digest: String },
    /// Batch Latin-script display upgrades for CJK title/artist metadata.
    Romanize {
        request_id: u64,
        items: Vec<RomanizeItem>,
    },
    /// Switch the model used for subsequent requests (settings save).
    SetModel(GeminiModel),
}

/// Events emitted by the Gemini actor.
pub enum AiEvent {
    Thinking(bool),
    Chat(String),
    Error(String),
    PlayTracks(Vec<Song>),
    Enqueue(Vec<Song>),
    Suggestions(Vec<Song>),
    SetAutoplay(bool),
    SetStationProfile {
        query: String,
        explore: Option<String>,
        avoid_artists: Vec<String>,
    },
    CreatePlaylist(String),
    AddToPlaylist {
        playlist: String,
        songs: Vec<Song>,
    },
    PlayPlaylist(String),
    StreamingPicks {
        seed_video_id: String,
        picks: Vec<AiPick>,
        conf: Option<f32>,
    },
    StationPatch {
        down_artists: Vec<String>,
        boost_artists: Vec<String>,
    },
    RomanizedTitles {
        request_id: u64,
        keys: Vec<String>,
        entries: Vec<RomanizedResult>,
    },
}

pub(crate) type EventSink = Arc<dyn Fn(AiEvent) + Send + Sync>;
