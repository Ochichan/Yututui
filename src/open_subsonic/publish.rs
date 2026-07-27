//! The record of which downloaded tracks were copied into the server's music folder.
//!
//! This ledger is deliberately not part of the OpenSubsonic store-set transaction. Nothing in it
//! has to stay consistent with a rating shadow or a playlist link, and keeping it in its own file
//! is the only arrangement that survives a downgrade: `DiskProfile` and `DiskBridgeState` are both
//! `deny_unknown_fields` behind versioned decoders, so adding a field to either makes the whole
//! store unreadable to the previous release. A separate file an older binary never opens costs it
//! nothing.
//!
//! Losing this ledger is recoverable by construction. Publication is content-addressed at the
//! destination, so a rebuilt-from-empty ledger re-checks rather than re-copies, and the only thing
//! the user has to redo is pointing at the music folder again.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::profile::{OpenSubsonicPaths, StoreError};
use crate::transfer::library_publish::{
    PublishOutcome, PublishTrack, plan_publish_path, publish_into_library,
};
use crate::util::safe_fs;

const PUBLISH_KIND: &str = "yututui_open_subsonic_publish";
const PUBLISH_SCHEMA_VERSION: u32 = 1;
const MAX_PUBLISH_BYTES: u64 = 4 * 1024 * 1024;

/// Matches `MAX_PENDING_PLAYLIST_CREATES`; a library this size is already past what one TUI list
/// can usefully show.
const MAX_PUBLISHED_TRACKS: usize = 999;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PublishedTrack {
    pub(crate) relative_path: String,
    pub(crate) content_sha256: String,
    pub(crate) len: u64,
    pub(crate) published_at_unix: i64,
    /// Set only once the track was actually observed on the server. The configured folder cannot
    /// be verified against the server — `getMusicFolders` returns names, never paths — so a
    /// successful copy on its own proves nothing about whether the server will ever see it.
    #[serde(default)]
    pub(crate) confirmed_on_server: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingPublish {
    pub(crate) relative_path: String,
    pub(crate) stage_basename: String,
    pub(crate) started_at_unix: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublishLedger {
    kind: String,
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    music_folder: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    published: BTreeMap<String, PublishedTrack>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending: BTreeMap<String, PendingPublish>,
}

impl Default for PublishLedger {
    fn default() -> Self {
        Self {
            kind: PUBLISH_KIND.to_owned(),
            schema_version: PUBLISH_SCHEMA_VERSION,
            music_folder: None,
            published: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }
}

impl PublishLedger {
    pub(crate) fn music_folder(&self) -> Option<&Path> {
        self.music_folder.as_deref().map(Path::new)
    }

    pub(crate) fn published(&self) -> &BTreeMap<String, PublishedTrack> {
        &self.published
    }

    pub(crate) fn pending(&self) -> &BTreeMap<String, PendingPublish> {
        &self.pending
    }

    pub(crate) fn confirmed_count(&self) -> usize {
        self.published
            .values()
            .filter(|track| track.confirmed_on_server)
            .count()
    }
}

/// Read the ledger, or start an empty one when it does not exist yet.
///
/// A malformed or foreign file is an error rather than a silent reset, so a corrupt ledger is
/// reported instead of quietly re-publishing a whole library.
pub(crate) fn load(paths: &OpenSubsonicPaths) -> Result<PublishLedger, StoreError> {
    let bytes = match safe_fs::read_no_symlink_limited(paths.publish_store(), MAX_PUBLISH_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PublishLedger::default());
        }
        Err(_) => return Err(StoreError::StorageUnavailable),
    };
    let ledger: PublishLedger =
        serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidState)?;
    if ledger.kind != PUBLISH_KIND || ledger.schema_version != PUBLISH_SCHEMA_VERSION {
        return Err(StoreError::InvalidState);
    }
    Ok(ledger)
}

pub(crate) fn save(paths: &OpenSubsonicPaths, ledger: &PublishLedger) -> Result<(), StoreError> {
    safe_fs::ensure_private_dir_durable(paths.root())
        .map_err(|_| StoreError::StorageUnavailable)?;
    safe_fs::write_private_atomic_json(paths.publish_store(), ledger)
        .map_err(|_| StoreError::StorageUnavailable)
}

/// Record the music folder after the caller has validated it.
pub(crate) fn set_music_folder(paths: &OpenSubsonicPaths, folder: &Path) -> Result<(), StoreError> {
    let mut ledger = load(paths)?;
    ledger.music_folder = Some(folder.to_string_lossy().into_owned());
    save(paths, &ledger)
}

/// One track to publish, as the caller knows it.
#[derive(Clone, Debug)]
pub(crate) struct PublishRequest {
    pub(crate) video_id: String,
    pub(crate) title: String,
    pub(crate) artist: Option<String>,
    pub(crate) album_artist: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) track_number: Option<u32>,
    pub(crate) source: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PublishReport {
    Published { relative_path: String },
    AlreadyPublished { relative_path: String },
    Conflict { relative_path: String },
}

#[derive(Debug)]
pub(crate) enum PublishError {
    /// No music folder has been chosen yet.
    NotConfigured,
    /// The ledger is full.
    TooManyPublishedTracks,
    Store(StoreError),
    /// The copy itself failed. The intent stays journalled so the next status shows it.
    Transport(anyhow::Error),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(
                formatter,
                "no music folder is set; run `ytt server publish setup` first"
            ),
            Self::TooManyPublishedTracks => write!(
                formatter,
                "the publish ledger already holds {MAX_PUBLISHED_TRACKS} tracks"
            ),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::Transport(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for PublishError {}

impl From<StoreError> for PublishError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Copy one downloaded track into the configured music folder and record the result.
///
/// The intent is journalled durably *before* the copy, so an interrupted publish leaves evidence
/// that something was in flight rather than looking like it never started. On a transport failure
/// the journal is deliberately kept: that is what `publish status` reports as needing attention.
pub(crate) fn publish_track(
    paths: &OpenSubsonicPaths,
    request: &PublishRequest,
    now_unix: i64,
) -> Result<PublishReport, PublishError> {
    let mut ledger = load(paths)?;
    let Some(music_folder) = ledger.music_folder().map(Path::to_path_buf) else {
        return Err(PublishError::NotConfigured);
    };
    if ledger.published.len() >= MAX_PUBLISHED_TRACKS
        && !ledger.published.contains_key(&request.video_id)
    {
        return Err(PublishError::TooManyPublishedTracks);
    }

    let extension = request
        .source
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_owned);
    let plan = plan_publish_path(
        &PublishTrack {
            video_id: &request.video_id,
            title: &request.title,
            artist: request.artist.as_deref(),
            album_artist: request.album_artist.as_deref(),
            album: request.album.as_deref(),
            track_number: request.track_number,
        },
        extension.as_deref(),
    );

    ledger.pending.insert(
        request.video_id.clone(),
        PendingPublish {
            relative_path: plan.relative_path(),
            stage_basename: plan.stage_basename.to_string_lossy().into_owned(),
            started_at_unix: now_unix,
        },
    );
    save(paths, &ledger)?;

    let outcome = publish_into_library(&request.source, &music_folder, &plan)
        .map_err(PublishError::Transport)?;

    // Reload before recording: the copy is the slow part, and something else may have changed the
    // music folder or another track's state while it ran.
    let mut ledger = load(paths)?;
    ledger.pending.remove(&request.video_id);
    let report = match outcome {
        PublishOutcome::Published(artifact) => {
            record_publication(&mut ledger, request, artifact.clone(), now_unix);
            PublishReport::Published {
                relative_path: artifact.relative_path,
            }
        }
        PublishOutcome::AlreadyPublished(artifact) => {
            record_publication(&mut ledger, request, artifact.clone(), now_unix);
            PublishReport::AlreadyPublished {
                relative_path: artifact.relative_path,
            }
        }
        PublishOutcome::Conflict { relative_path, .. } => PublishReport::Conflict { relative_path },
    };
    save(paths, &ledger)?;
    Ok(report)
}

/// Record one publication, preserving a server confirmation only while the bytes are unchanged.
fn record_publication(
    ledger: &mut PublishLedger,
    request: &PublishRequest,
    artifact: crate::transfer::library_publish::PublishedArtifact,
    now_unix: i64,
) {
    let still_confirmed = ledger
        .published
        .get(&request.video_id)
        .is_some_and(|existing| {
            existing.confirmed_on_server && existing.content_sha256 == artifact.sha256
        });
    ledger.published.insert(
        request.video_id.clone(),
        PublishedTrack {
            relative_path: artifact.relative_path,
            content_sha256: artifact.sha256,
            len: artifact.len,
            published_at_unix: now_unix,
            confirmed_on_server: still_confirmed,
        },
    );
}

/// Mark a published track as actually observed on the server.
pub(crate) fn record_server_confirmation(
    paths: &OpenSubsonicPaths,
    video_id: &str,
) -> Result<(), StoreError> {
    let mut ledger = load(paths)?;
    if let Some(track) = ledger.published.get_mut(video_id) {
        if track.confirmed_on_server {
            return Ok(());
        }
        track.confirmed_on_server = true;
        return save(paths, &ledger);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
