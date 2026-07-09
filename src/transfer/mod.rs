//! Playlist transfer: Spotify ↔ YouTube Music ↔ files.
//!
//! The engine ([`run_job`]) is a plain async fn shared by the CLI (own current-thread
//! runtime) and the TUI (a dedicated actor) — progress flows through a callback,
//! durability through per-job [`checkpoint`]s, and matching through the
//! direction-agnostic [`matching`] engine.

pub mod actor;
pub mod checkpoint;
pub mod cli;
pub mod csv;
mod download_cli;
pub mod download_plan;
pub mod json;
pub mod matching;
mod organize_cli;
pub mod organize_plan;
mod report_cli;
pub mod review_action;
mod review_cli;
pub mod session;

mod engine;

pub use engine::{JobCtx, JobError, run_job, write_reviewed_local_job};

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransferSource {
    SpotifyPlaylist {
        id: String,
    },
    SpotifyLiked,
    /// A YouTube Music account playlist.
    YtmPlaylist {
        id: String,
    },
    /// A local (on-disk) yututui playlist, by its store key.
    LocalPlaylist {
        key: String,
    },
    File {
        path: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransferDest {
    /// Create a YTM playlist (name defaults to the source's).
    YtmNewPlaylist {
        name: Option<String>,
    },
    /// Append to an existing YTM playlist, found by exact title.
    YtmExistingPlaylist {
        name: String,
    },
    /// `rate_song(Liked)` each match (oldest-first to preserve like order).
    YtmLikes,
    /// The app's own local playlist store — browsable/playable in the TUI Library
    /// immediately (account playlists aren't browsable in-app). Create-or-append by name.
    LocalPlaylist {
        name: Option<String>,
    },
    SpotifyNewPlaylist {
        name: Option<String>,
    },
    File {
        path: PathBuf,
        format: FileFormat,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    Json,
    Csv,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub source: TransferSource,
    pub dest: TransferDest,
    /// Fetch + match + checkpoint, but skip every write. Resuming the job afterwards
    /// performs the writes reusing the saved matches — the recommended big-playlist flow.
    pub dry_run: bool,
    /// Match accept threshold (see [`matching::MatchConfig`]).
    pub min_score: f32,
    /// Accept the best ambiguous candidate instead of skipping it.
    pub take_best: bool,
    /// TUI fast-import policy: auto-accept only the first safe ambiguous candidate at or above
    /// this score. CLI defaults leave it unset; `take_best` keeps its existing behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_accept_ambiguous_min_score: Option<f32>,
    /// Ignore checkpointed outcomes and match afresh (also disables file fast-path ids).
    pub rematch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Fetching,
    Matching,
    Writing,
    Done,
    Failed,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Fetching => "fetching",
            Stage::Matching => "matching",
            Stage::Writing => "writing",
            Stage::Done => "done",
            Stage::Failed => "failed",
        }
    }
}

/// A progress beat for the UI/CLI: counts so far plus what's being worked on.
#[derive(Debug, Clone)]
pub struct TransferProgress {
    pub job_id: String,
    pub stage: Stage,
    pub done: u32,
    pub total: u32,
    pub matched: u32,
    pub auto_accepted: u32,
    pub ambiguous: u32,
    pub not_found: u32,
    pub written: u32,
    /// Display line for the current item ("Artist — Title").
    pub current: String,
}

/// `"{kind}-{unix}-{4 hex}"` — sortable, collision-safe, and shell-friendly.
pub fn new_job_id(kind: &str) -> String {
    let mut bytes = [0u8; 2];
    if getrandom::fill(&mut bytes).is_err() {
        // OS RNG unavailable (extremely rare) — fall back to the low bits of a high-res clock
        // so we still produce a usable id instead of killing the transfer actor with a panic.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        bytes = [(nanos >> 8) as u8, nanos as u8];
    }
    format!(
        "{kind}-{}-{:02x}{:02x}",
        crate::signals::unix_now(),
        bytes[0],
        bytes[1]
    )
}
