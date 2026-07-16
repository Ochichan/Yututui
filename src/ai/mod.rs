//! The DJ Gem assistant: a multi-turn Gemini function-calling agent that drives playback.
//!
//! Mirrors `youtube-music-cli`'s LLM service, adapted to the dual-owner architecture: the
//! actor cannot touch either owner, so tool side-effects flow back as [`AiEvent`]s for the
//! active owner to reduce. The model invokes tools (search, play, queue, streaming, playlists);
//! resolves run inside the actor via yt-dlp; mutations are reported back as intents.
//!
//! The loop (`converse`):
//! 1. Send the conversation + tool schemas to Gemini.
//! 2. If the reply has `functionCall` parts, **echo the model turn verbatim** (preserving
//!    `thoughtSignature`), execute each tool, append the results as a new `user` turn, and
//!    loop — up to the actor's round cap.
//! 3. Otherwise emit the reply text and stop.
//!
//! Safety rails: a client-side rate limiter; an RAII guard that always clears the
//! thinking spinner; and model fallback that is **disabled once a side-effecting tool has
//! run** (so a retry on another model can't double-apply a playback change).

mod actor;
pub mod client;
mod context;
mod dto;
pub mod model;
mod model_control;
mod protocol;
mod structured;
pub mod tools;
pub mod usage;

pub use actor::{AiHandle, spawn};
pub use dto::{AiContext, AiPick, PlaylistInfo};
pub use model::GeminiModel;
pub use protocol::{AiCmd, AiEvent};

pub(crate) use protocol::EventSink;

#[cfg(test)]
use actor::{
    AiActor, HISTORY_MAX_CHARS, HISTORY_MAX_TURNS, HistoryRole, HistoryTurn, RATE_WINDOW,
    ThinkingGuard, chat_contents, trim_history,
};
#[cfg(test)]
use client::GeminiClient;
#[cfg(test)]
use context::context_summary;
#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::time::{Duration, Instant};
#[cfg(test)]
use structured::{
    FEEDBACK_MAX_TOKENS, RERANK_MAX_TOKENS, ROMANIZE_MAX_TOKENS, build_feedback_request,
    build_rerank_request, build_romanize_request, parse_feedback_patch, parse_rerank_picks,
    parse_romanized_titles,
};
#[cfg(test)]
use tokio::sync::mpsc;

#[cfg(test)]
use crate::romanize::RomanizeItem;

#[cfg(test)]
mod tests;
