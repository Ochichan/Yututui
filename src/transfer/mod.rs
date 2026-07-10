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
mod match_cache;
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
    /// Replace the contents of one explicitly selected, owned Spotify playlist.
    /// CLI parsing requires an accompanying destructive `--sync` acknowledgement.
    SpotifyMirrorPlaylist {
        id: String,
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

/// Destination media intent for Spotify -> YouTube Music/local imports.
///
/// The default preserves the historical song-first matcher for old settings and checkpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImportMediaKind {
    #[default]
    Track,
    MusicVideo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub source: TransferSource,
    pub dest: TransferDest,
    /// Select the normal song catalog or the official-music-video retrieval path.
    #[serde(default)]
    pub media_kind: ImportMediaKind,
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
    /// Matching preset. Old checkpoints deserialize to `Strict`; new CLI/TUI imports
    /// choose a more useful default explicitly.
    #[serde(default)]
    pub match_policy: MatchPolicy,
    /// Permit generic, non-official public YouTube uploads to auto-match when policy
    /// gates otherwise agree. Off by default; such rows normally stay review-only.
    #[serde(default)]
    pub allow_user_videos: bool,
    /// Persistent transfer cache behavior for Spotify -> YouTube Music matching.
    #[serde(default)]
    pub cache_mode: TransferCacheMode,
    /// Ignore checkpointed outcomes and match afresh (also disables file fast-path ids).
    pub rematch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchPolicy {
    #[default]
    Strict,
    Balanced,
    Aggressive,
    Exhaustive,
}

impl std::str::FromStr for MatchPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "strict" => Ok(Self::Strict),
            "balanced" => Ok(Self::Balanced),
            "aggressive" => Ok(Self::Aggressive),
            "exhaustive" => Ok(Self::Exhaustive),
            other => Err(format!(
                "--policy expects `strict`, `balanced`, `aggressive`, or `exhaustive` (got `{other}`)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransferCacheMode {
    #[default]
    Use,
    Refresh,
    Off,
}

impl TransferCacheMode {
    pub fn read_enabled(self) -> bool {
        matches!(self, Self::Use)
    }

    pub fn write_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl std::str::FromStr for TransferCacheMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "use" => Ok(Self::Use),
            "refresh" => Ok(Self::Refresh),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "--cache expects `use`, `refresh`, or `off` (got `{other}`)"
            )),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_cache_mode_read_write_matrix() {
        assert!(TransferCacheMode::Use.read_enabled());
        assert!(TransferCacheMode::Use.write_enabled());
        assert!(!TransferCacheMode::Refresh.read_enabled());
        assert!(TransferCacheMode::Refresh.write_enabled());
        assert!(!TransferCacheMode::Off.read_enabled());
        assert!(!TransferCacheMode::Off.write_enabled());
    }

    #[test]
    fn match_policy_and_cache_mode_parse_matrix() {
        assert_eq!("strict".parse(), Ok(MatchPolicy::Strict));
        assert_eq!("balanced".parse(), Ok(MatchPolicy::Balanced));
        assert_eq!("aggressive".parse(), Ok(MatchPolicy::Aggressive));
        assert_eq!("exhaustive".parse(), Ok(MatchPolicy::Exhaustive));
        assert!("reckless".parse::<MatchPolicy>().is_err());

        assert_eq!("use".parse(), Ok(TransferCacheMode::Use));
        assert_eq!("refresh".parse(), Ok(TransferCacheMode::Refresh));
        assert_eq!("off".parse(), Ok(TransferCacheMode::Off));
        assert!("stale".parse::<TransferCacheMode>().is_err());
    }

    #[test]
    fn old_job_specs_default_to_track_media() {
        let spec: JobSpec = serde_json::from_value(serde_json::json!({
            "source": { "kind": "spotify_liked" },
            "dest": { "kind": "local_playlist", "name": null },
            "dry_run": true,
            "min_score": 0.8,
            "take_best": false,
            "match_policy": "strict",
            "allow_user_videos": false,
            "cache_mode": "use",
            "rematch": false
        }))
        .expect("legacy job spec");

        assert_eq!(spec.media_kind, ImportMediaKind::Track);
        assert_eq!(
            serde_json::to_value(ImportMediaKind::MusicVideo).expect("serialize media kind"),
            serde_json::json!("music_video")
        );
    }

    #[test]
    fn spotify_mirror_destination_keeps_the_explicit_id() {
        let dest = TransferDest::SpotifyMirrorPlaylist {
            id: "0123456789ABCDEFGHIJKL".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(dest).expect("serialize mirror destination"),
            serde_json::json!({
                "kind": "spotify_mirror_playlist",
                "id": "0123456789ABCDEFGHIJKL"
            })
        );
    }
}
