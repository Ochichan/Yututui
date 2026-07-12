//! Review-oriented import sessions built from transfer checkpoints.
//!
//! Checkpoints remain the idempotent execution log. This module writes a second,
//! human/workflow-shaped JSON document that can drive match review, download
//! follow-up, and Local Deck inbox organization without making UI code reverse-engineer
//! checkpoint internals.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use super::artifact_identity::{ArtifactReceipt, ImportDownloadClaim};
use super::checkpoint::{
    Checkpoint, JobDeferReason, MatchTrace, ProbeTrace, ReportCandidate, ReviewDecision,
};
use super::matching::{MatchOutcome, TrackInput};
use super::{Stage, TransferDest, TransferSource};
use crate::util::safe_fs;

mod artifact_receipt;
mod claim;
mod projection;
pub(crate) use artifact_receipt::{
    clear_missing_artifact_unlocked, import_song_for_row, promote_artifact_receipt_unlocked,
    record_artifact_move_done_unlocked, record_import_download_error,
    record_import_download_interruption,
};
pub(crate) use claim::{
    claim_import_download, ensure_review_row_mutable_unlocked,
    validate_import_download_claim_unlocked,
};

const IMPORT_SESSION_SCHEMA_VERSION: u32 = 1;
const IMPORT_SESSION_SUMMARY_SCHEMA_VERSION: u32 = 1;
const IMPORT_SESSION_MAX_BYTES: u64 = 50 * 1024 * 1024;
const IMPORT_SESSION_SUMMARY_MAX_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportSession {
    pub schema_version: u32,
    pub session_id: String,
    /// Random incarnation id. Unlike `session_id`, this changes if a deleted record is recreated.
    #[serde(default)]
    pub(crate) session_instance_id: String,
    pub job_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub stage: Stage,
    pub source: SessionEndpoint,
    pub destination: SessionEndpoint,
    pub counts: ImportSessionCounts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_reason: Option<JobDeferReason>,
    pub rows: Vec<ImportSessionRow>,
}

#[derive(Debug, Clone)]
pub struct ImportSessionSummary {
    pub session_id: String,
    pub stage: Stage,
    pub updated_at: i64,
    pub source: SessionEndpoint,
    pub destination: SessionEndpoint,
    pub counts: ImportSessionCounts,
}

/// One persisted import-history record, including incomplete/orphaned records that no
/// longer have a readable session document. Local Deck uses this lightweight index to keep
/// every checkpoint/report/journal/summary cleanup target visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRecordEntry {
    pub session_id: String,
    pub updated_at: i64,
}

/// Exclusive per-job guard shared by every import-history writer and record deletion.
///
/// The lock file is deliberately permanent: unlinking a locked inode would let a new caller
/// create and lock a different inode for the same job while the first guard is still alive.
pub(crate) struct ImportRecordGuard {
    _file: File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ImportSessionSummaryFile {
    schema_version: u32,
    session_id: String,
    stage: Stage,
    updated_at: i64,
    source: SessionEndpoint,
    destination: SessionEndpoint,
    counts: ImportSessionCounts,
}

impl Default for ImportSessionSummaryFile {
    fn default() -> Self {
        Self {
            schema_version: IMPORT_SESSION_SUMMARY_SCHEMA_VERSION,
            session_id: String::new(),
            stage: Stage::Fetching,
            updated_at: 0,
            source: SessionEndpoint::default(),
            destination: SessionEndpoint::default(),
            counts: ImportSessionCounts::default(),
        }
    }
}

impl From<&ImportSession> for ImportSessionSummary {
    fn from(session: &ImportSession) -> Self {
        Self {
            session_id: session.session_id.clone(),
            stage: session.stage,
            updated_at: session.updated_at,
            source: session.source.clone(),
            destination: session.destination.clone(),
            counts: session.counts.clone(),
        }
    }
}

impl From<&ImportSession> for ImportSessionSummaryFile {
    fn from(session: &ImportSession) -> Self {
        ImportSessionSummary::from(session).into()
    }
}

impl From<ImportSessionSummary> for ImportSessionSummaryFile {
    fn from(summary: ImportSessionSummary) -> Self {
        Self {
            schema_version: IMPORT_SESSION_SUMMARY_SCHEMA_VERSION,
            session_id: summary.session_id,
            stage: summary.stage,
            updated_at: summary.updated_at,
            source: summary.source,
            destination: summary.destination,
            counts: summary.counts,
        }
    }
}

impl From<ImportSessionSummaryFile> for ImportSessionSummary {
    fn from(file: ImportSessionSummaryFile) -> Self {
        Self {
            session_id: file.session_id,
            stage: file.stage,
            updated_at: file.updated_at,
            source: file.source,
            destination: file.destination,
            counts: file.counts,
        }
    }
}

impl Default for ImportSession {
    fn default() -> Self {
        Self {
            schema_version: IMPORT_SESSION_SCHEMA_VERSION,
            session_id: String::new(),
            session_instance_id: String::new(),
            job_id: String::new(),
            created_at: 0,
            updated_at: 0,
            stage: Stage::Fetching,
            source: SessionEndpoint::default(),
            destination: SessionEndpoint::default(),
            counts: ImportSessionCounts::default(),
            defer_reason: None,
            rows: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionEndpoint {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl SessionEndpoint {
    pub fn display(&self) -> String {
        self.label
            .clone()
            .or_else(|| self.key.clone())
            .unwrap_or_else(|| self.kind.clone())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportSessionCounts {
    pub total: u32,
    pub pending: u32,
    pub matched: u32,
    pub ambiguous: u32,
    pub not_found: u32,
    pub skipped_local: u32,
    pub capacity_skipped: u32,
    pub written: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportSessionRow {
    pub row_id: String,
    pub source_order: u32,
    /// Monotonic identity for the row's review-selected download target.
    #[serde(default = "default_row_revision")]
    pub(crate) revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) download_claim: Option<ImportDownloadClaim>,
    pub status: ImportSessionRowStatus,
    pub title: String,
    pub artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub album_artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_release_date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_release_date_precision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_total_tracks: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_art_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isrc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    pub source_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_display: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<ReviewDecision>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<ReportCandidate>,
    #[serde(default)]
    pub written: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) artifact_receipt: Option<ArtifactReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search_queries: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reason_codes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_delta_secs: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub executed_probes: Vec<ProbeTrace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
}

impl Default for ImportSessionRow {
    fn default() -> Self {
        Self {
            row_id: String::new(),
            source_order: 0,
            revision: default_row_revision(),
            download_claim: None,
            status: ImportSessionRowStatus::Pending,
            title: String::new(),
            artists: Vec::new(),
            album_artists: Vec::new(),
            album: None,
            album_id: None,
            album_uri: None,
            album_release_date: None,
            album_release_date_precision: None,
            album_total_tracks: None,
            album_type: None,
            album_art_url: None,
            disc_number: None,
            track_number: None,
            duration_secs: None,
            isrc: None,
            explicit: None,
            source_key: String::new(),
            source_url: None,
            selected_key: None,
            selected_score: None,
            selected_display: None,
            review_decision: None,
            candidates: Vec::new(),
            written: false,
            local_path: None,
            artifact_receipt: None,
            warnings: Vec::new(),
            errors: Vec::new(),
            search_queries: Vec::new(),
            source_kind: None,
            quality_tier: None,
            reject_reason: None,
            reason_codes: Vec::new(),
            duration_delta_secs: None,
            executed_probes: Vec::new(),
            terminal_reason: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSessionRowStatus {
    #[default]
    Pending,
    Matched,
    Ambiguous,
    NotFound,
    SkippedLocal,
    SkippedCapacity,
}

impl ImportSession {
    pub fn save(&self) -> std::io::Result<()> {
        if session_path(&self.session_id).is_none() {
            return Ok(());
        }
        let _guard = ImportRecordGuard::try_acquire(&self.session_id)?;
        self.save_unlocked()
    }

    /// Save while the caller already holds this session's [`ImportRecordGuard`].
    /// Keeping this separate avoids platform-dependent recursive file-lock behavior in
    /// long-running import/review operations.
    pub(crate) fn save_unlocked(&self) -> std::io::Result<()> {
        let Some(path) = session_path(&self.session_id) else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)?;
        if let Some(path) = session_summary_path(&self.session_id)
            && let Err(error) =
                safe_fs::write_private_atomic_json(&path, &ImportSessionSummaryFile::from(self))
        {
            // The summary is a rebuildable discovery cache. The authoritative session has
            // already committed, and list_all ignores a summary older than that session.
            tracing::warn!(
                %error,
                session_id = %self.session_id,
                "import session saved but its derived summary is stale"
            );
        }
        Ok(())
    }

    pub fn load(session_id: &str) -> anyhow::Result<Self> {
        use anyhow::{Context, bail};
        let path = session_path(session_id)
            .ok_or_else(|| anyhow::anyhow!("no data directory on this platform"))?;
        let bytes = safe_fs::read_no_symlink_limited(&path, IMPORT_SESSION_MAX_BYTES)
            .with_context(|| format!("no import session for job `{session_id}`"))?;
        let session: ImportSession =
            serde_json::from_slice(&bytes).context("import session is corrupt")?;
        if session.schema_version != IMPORT_SESSION_SCHEMA_VERSION {
            bail!(
                "import session schema version {} is not supported (expected {IMPORT_SESSION_SCHEMA_VERSION})",
                session.schema_version
            );
        }
        Ok(session)
    }

    pub fn list_all() -> Vec<ImportSessionSummary> {
        let Some(dir) = sessions_dir() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<ImportSessionSummary> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                let name = path.file_name()?.to_str()?;
                let session_id = name.strip_suffix(".json")?;
                if session_id.ends_with(".summary") {
                    return None;
                }
                read_fresh_summary(session_id, &path)
                    .or_else(|| {
                        let bytes =
                            safe_fs::read_no_symlink_limited(&path, IMPORT_SESSION_MAX_BYTES)
                                .ok()?;
                        let session: ImportSession = serde_json::from_slice(&bytes).ok()?;
                        // Keep discovery read-only. Recreating a missing sidecar here can race
                        // with `delete_record`: a reader that loaded the session just before
                        // deletion could otherwise resurrect an orphan summary afterwards.
                        Some(ImportSessionSummary::from(&session))
                    })
                    .or_else(|| read_session_summary(session_id))
            })
            .collect();
        out.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at));
        out
    }

    /// List every persisted import-history id, even when only an orphan checkpoint, report,
    /// journal, or summary remains. Imported Local Deck tracks are merged by the UI separately.
    pub fn list_record_entries() -> Vec<ImportRecordEntry> {
        let Some(transfers) = transfers_dir() else {
            return Vec::new();
        };
        let sessions = transfers.join("sessions");
        let mut records = BTreeMap::<String, i64>::new();
        collect_record_artifacts(&transfers, RecordArtifactDir::Transfers, &mut records);
        collect_record_artifacts(&sessions, RecordArtifactDir::Sessions, &mut records);
        let mut out: Vec<_> = records
            .into_iter()
            .map(|(session_id, updated_at)| ImportRecordEntry {
                session_id,
                updated_at,
            })
            .collect();
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        out
    }

    /// Delete the persisted history for one import job without touching any imported audio,
    /// Local Deck index row, download sidecar, or destination playlist. The session document is
    /// removed last so an interrupted cleanup remains visible and can be retried.
    pub fn delete_record(session_id: &str) -> std::io::Result<usize> {
        let Some(paths) = import_record_paths(session_id) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid import session id",
            ));
        };
        reject_symlinked_record_parents()?;
        let _guard = ImportRecordGuard::try_acquire(session_id)?;
        if session_path(session_id).is_some_and(|path| path.exists()) {
            let session = Self::load(session_id).map_err(std::io::Error::other)?;
            if session.rows.iter().any(|row| row.download_claim.is_some()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("import job `{session_id}` has an active download claim"),
                ));
            }
        }
        let mut removed = 0;
        for path in paths {
            match std::fs::remove_file(path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(removed)
    }

    pub fn record_exists(session_id: &str) -> bool {
        import_record_paths(session_id).is_some_and(|paths| {
            paths
                .iter()
                .any(|path| std::fs::symlink_metadata(path).is_ok())
        })
    }
}

pub fn record_download_done(
    session_id: &str,
    source_order: u32,
    local_path: PathBuf,
) -> anyhow::Result<()> {
    let _guard = ImportRecordGuard::try_acquire(session_id)?;
    record_download_done_unlocked(session_id, source_order, local_path)
}

pub(crate) fn record_download_done_unlocked(
    session_id: &str,
    source_order: u32,
    local_path: PathBuf,
) -> anyhow::Result<()> {
    let mut session = ImportSession::load(session_id)?;
    let row = row_by_source_order_mut(&mut session, source_order)?;
    row.written = true;
    row.local_path = Some(local_path);
    row.artifact_receipt = None;
    row.errors.clear();
    save_updated_session(session)
}

pub fn record_download_error(
    session_id: &str,
    source_order: u32,
    error: &str,
) -> anyhow::Result<()> {
    let _guard = ImportRecordGuard::try_acquire(session_id)?;
    record_download_error_unlocked(session_id, source_order, error)
}

fn record_download_error_unlocked(
    session_id: &str,
    source_order: u32,
    error: &str,
) -> anyhow::Result<()> {
    let mut session = ImportSession::load(session_id)?;
    let row = row_by_source_order_mut(&mut session, source_order)?;
    row.written = false;
    row.local_path = None;
    row.artifact_receipt = None;
    row.errors.clear();
    let message = error.trim();
    if !message.is_empty() {
        row.errors.push(message.chars().take(500).collect());
    }
    save_updated_session(session)
}

pub(crate) fn row_by_source_order_mut(
    session: &mut ImportSession,
    source_order: u32,
) -> anyhow::Result<&mut ImportSessionRow> {
    use anyhow::Context as _;
    let session_id = session.session_id.clone();
    session
        .rows
        .iter_mut()
        .find(|row| row.source_order == source_order)
        .with_context(|| {
            format!("import session `{session_id}` has no row with source order {source_order}")
        })
}

pub(crate) fn save_updated_session(mut session: ImportSession) -> anyhow::Result<()> {
    session.updated_at = crate::signals::unix_now();
    session.counts = ImportSessionCounts::from_rows(&session.rows);
    session.save_unlocked().map_err(Into::into)
}

impl ImportSessionCounts {
    fn from_rows(rows: &[ImportSessionRow]) -> Self {
        let mut counts = ImportSessionCounts {
            total: rows.len() as u32,
            ..ImportSessionCounts::default()
        };
        for row in rows {
            match row.status {
                ImportSessionRowStatus::Pending => counts.pending += 1,
                ImportSessionRowStatus::Matched => counts.matched += 1,
                ImportSessionRowStatus::Ambiguous => counts.ambiguous += 1,
                ImportSessionRowStatus::NotFound => counts.not_found += 1,
                ImportSessionRowStatus::SkippedLocal => counts.skipped_local += 1,
                ImportSessionRowStatus::SkippedCapacity => counts.capacity_skipped += 1,
            }
            if row.written {
                counts.written += 1;
            }
        }
        counts
    }
}

fn row_from_input(
    idx: usize,
    input: &TrackInput,
    outcome: Option<&MatchOutcome>,
    review_decision: Option<ReviewDecision>,
    written: bool,
    searched_ytm: bool,
    trace: Option<&MatchTrace>,
) -> ImportSessionRow {
    let mut row = ImportSessionRow {
        row_id: format!("row-{:05}", idx + 1),
        source_order: idx as u32 + 1,
        title: input.title.clone(),
        artists: input.artists.clone(),
        album_artists: input.album_artists.clone(),
        album: input.album.clone(),
        album_id: input.album_id.clone(),
        album_uri: input.album_uri.clone(),
        album_release_date: input.album_release_date.clone(),
        album_release_date_precision: input.album_release_date_precision.clone(),
        album_total_tracks: input.album_total_tracks,
        album_type: input.album_type.clone(),
        album_art_url: input.album_art_url.clone(),
        disc_number: input.disc_number,
        track_number: input.track_number,
        duration_secs: input.duration_secs,
        isrc: input.isrc.clone(),
        explicit: input.explicit,
        source_key: input.source_key.clone(),
        source_url: input.source_url.clone(),
        review_decision,
        written,
        ..ImportSessionRow::default()
    };

    match outcome {
        Some(MatchOutcome::Matched {
            key,
            score,
            display,
            score_breakdown,
            ..
        }) => {
            row.status = ImportSessionRowStatus::Matched;
            row.selected_key = Some(key.clone());
            row.selected_score = Some(*score);
            row.selected_display = Some(display.clone());
            row.candidates.push(ReportCandidate {
                key: key.clone(),
                score: *score,
                display: display.clone(),
                score_breakdown: score_breakdown.as_deref().cloned(),
            });
            if let Some(score) = score_breakdown {
                apply_row_quality(&mut row, score);
            }
        }
        Some(MatchOutcome::Ambiguous { candidates }) => {
            row.status = ImportSessionRowStatus::Ambiguous;
            row.selected_key = candidates.first().map(|c| c.key.clone());
            row.selected_score = candidates.first().map(|c| c.score);
            row.selected_display = candidates.first().map(|c| c.display.clone());
            row.candidates = candidates
                .iter()
                .map(|c| ReportCandidate {
                    key: c.key.clone(),
                    score: c.score,
                    display: c.display.clone(),
                    score_breakdown: c.score_breakdown.clone(),
                })
                .collect();
            if let Some(score) = candidates
                .first()
                .and_then(|candidate| candidate.score_breakdown.as_ref())
            {
                apply_row_quality(&mut row, score);
            }
        }
        Some(MatchOutcome::NotFound) => {
            row.status = ImportSessionRowStatus::NotFound;
        }
        Some(MatchOutcome::SkippedLocal) => {
            row.status = ImportSessionRowStatus::SkippedLocal;
            row.warnings
                .push("spotify local file or episode skipped".to_owned());
        }
        Some(MatchOutcome::SkippedCapacity) => {
            row.status = ImportSessionRowStatus::SkippedCapacity;
            row.warnings
                .push("destination playlist capacity reached".to_owned());
        }
        None => {
            row.status = ImportSessionRowStatus::Pending;
        }
    }
    if let Some(trace) = trace {
        row.executed_probes = trace.probes.clone();
        row.terminal_reason = trace.terminal_reason.clone();
        row.search_queries = trace
            .probes
            .iter()
            .map(|probe| probe.query.clone())
            .filter(|query| !query.is_empty())
            .collect();
        if row.candidates.is_empty() {
            row.candidates = trace.candidates.clone();
        }
    } else if !searched_ytm || input.known_video_id.is_some() {
        row.search_queries.clear();
    }
    row
}

fn default_row_revision() -> u64 {
    1
}

fn apply_row_quality(row: &mut ImportSessionRow, score: &super::matching::MatchScoreBreakdown) {
    if !score.source_kind.is_empty() {
        row.source_kind = Some(score.source_kind.clone());
    }
    if !score.quality_tier.is_empty() {
        row.quality_tier = Some(score.quality_tier.clone());
    }
    row.reject_reason = score.reject_reason.clone();
    row.reason_codes = score.reason_codes.clone();
    row.duration_delta_secs = score.duration_delta_secs;
}

fn endpoint_from_source(source: &TransferSource, label: Option<String>) -> SessionEndpoint {
    match source {
        TransferSource::SpotifyPlaylist { id } => SessionEndpoint {
            kind: "spotify_playlist".to_owned(),
            key: Some(id.clone()),
            label,
        },
        TransferSource::SpotifyLiked => SessionEndpoint {
            kind: "spotify_liked".to_owned(),
            key: None,
            label: label.or_else(|| Some("Spotify Liked Songs".to_owned())),
        },
        TransferSource::YtmPlaylist { id } => SessionEndpoint {
            kind: "ytm_playlist".to_owned(),
            key: Some(id.clone()),
            label,
        },
        TransferSource::LocalPlaylist { key } => SessionEndpoint {
            kind: "local_playlist".to_owned(),
            key: Some(key.clone()),
            label,
        },
        TransferSource::File { path } => SessionEndpoint {
            kind: "file".to_owned(),
            key: Some(path.display().to_string()),
            label,
        },
    }
}

fn endpoint_from_dest(dest: &TransferDest, label: Option<String>) -> SessionEndpoint {
    match dest {
        TransferDest::YtmNewPlaylist { name } => SessionEndpoint {
            kind: "ytm_new_playlist".to_owned(),
            key: None,
            label: label.or_else(|| name.clone()),
        },
        TransferDest::YtmExistingPlaylist { name } => SessionEndpoint {
            kind: "ytm_existing_playlist".to_owned(),
            key: Some(name.clone()),
            label: label.or_else(|| Some(name.clone())),
        },
        TransferDest::YtmLikes => SessionEndpoint {
            kind: "ytm_likes".to_owned(),
            key: None,
            label: label.or_else(|| Some("Liked Music".to_owned())),
        },
        TransferDest::LocalPlaylist { name } => SessionEndpoint {
            kind: "local_playlist".to_owned(),
            key: None,
            label: label.or_else(|| name.clone()),
        },
        TransferDest::SpotifyNewPlaylist { name } => SessionEndpoint {
            kind: "spotify_new_playlist".to_owned(),
            key: None,
            label: label.or_else(|| name.clone()),
        },
        TransferDest::SpotifyMirrorPlaylist { id } => SessionEndpoint {
            kind: "spotify_mirror_playlist".to_owned(),
            key: Some(id.clone()),
            label,
        },
        TransferDest::File { path, format } => SessionEndpoint {
            kind: format!("file_{format:?}").to_ascii_lowercase(),
            key: Some(path.display().to_string()),
            label,
        },
    }
}

pub fn session_path(session_id: &str) -> Option<PathBuf> {
    Some(sessions_dir()?.join(format!("{}.json", safe_session_id(session_id)?)))
}

fn session_summary_path(session_id: &str) -> Option<PathBuf> {
    Some(sessions_dir()?.join(format!("{}.summary.json", safe_session_id(session_id)?)))
}

fn read_fresh_summary(
    session_id: &str,
    session_path: &std::path::Path,
) -> Option<ImportSessionSummary> {
    let summary_path = session_summary_path(session_id)?;
    if summary_is_older_than_session(&summary_path, session_path) {
        return None;
    }
    read_session_summary(session_id)
}

fn read_session_summary(session_id: &str) -> Option<ImportSessionSummary> {
    let summary_path = session_summary_path(session_id)?;
    let bytes =
        safe_fs::read_no_symlink_limited(&summary_path, IMPORT_SESSION_SUMMARY_MAX_BYTES).ok()?;
    let file: ImportSessionSummaryFile = serde_json::from_slice(&bytes).ok()?;
    if file.schema_version != IMPORT_SESSION_SUMMARY_SCHEMA_VERSION || file.session_id != session_id
    {
        return None;
    }
    Some(file.into())
}

fn summary_is_older_than_session(
    summary_path: &std::path::Path,
    session_path: &std::path::Path,
) -> bool {
    let Ok(summary_modified) = std::fs::metadata(summary_path).and_then(|meta| meta.modified())
    else {
        return true;
    };
    let Ok(session_modified) = std::fs::metadata(session_path).and_then(|meta| meta.modified())
    else {
        return false;
    };
    summary_modified < session_modified
}

fn sessions_dir() -> Option<PathBuf> {
    Some(transfers_dir()?.join("sessions"))
}

fn transfers_dir() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("transfers"))
}

fn safe_session_id(session_id: &str) -> Option<&str> {
    (!session_id.is_empty()
        && session_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then_some(session_id)
}

fn import_record_paths(session_id: &str) -> Option<[PathBuf; 5]> {
    let safe_id = safe_session_id(session_id)?;
    let transfers = transfers_dir()?;
    let sessions = transfers.join("sessions");
    Some([
        transfers.join(format!("{safe_id}.json")),
        transfers.join(format!("{safe_id}.report.json")),
        transfers.join(format!("{safe_id}.journal.jsonl")),
        sessions.join(format!("{safe_id}.summary.json")),
        sessions.join(format!("{safe_id}.json")),
    ])
}

impl ImportRecordGuard {
    pub(crate) fn try_acquire_if_persistable(session_id: &str) -> std::io::Result<Option<Self>> {
        if safe_session_id(session_id).is_none() || transfers_dir().is_none() {
            return Ok(None);
        }
        Self::try_acquire(session_id).map(Some)
    }

    pub(crate) fn try_acquire(session_id: &str) -> std::io::Result<Self> {
        let file = open_record_lock(session_id)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("import job `{session_id}` is still active"),
            )),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

impl Drop for ImportRecordGuard {
    fn drop(&mut self) {
        // Closing only this handle is insufficient when a concurrent process spawn or an
        // explicit clone duplicated the locked file description: the duplicate can retain the
        // advisory lock after the guard is gone. Unlock first so the guard's lexical lifetime,
        // rather than the last duplicate handle's lifetime, remains the ownership boundary.
        if let Err(error) = self._file.unlock() {
            tracing::warn!(%error, "failed to explicitly release import record lock");
        }
    }
}

fn open_record_lock(session_id: &str) -> std::io::Result<File> {
    let safe_id = safe_session_id(session_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid import session id",
        )
    })?;
    let transfers = transfers_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no data directory on this platform",
        )
    })?;
    reject_existing_symlink_or_non_directory(&transfers)?;
    let lock_dir = transfers.join("record-locks");
    safe_fs::ensure_private_dir(&lock_dir)?;
    let lock_path = lock_dir.join(format!("{safe_id}.lock"));
    if std::fs::symlink_metadata(&lock_path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing symlink lock file {}", lock_path.display()),
        ));
    }

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(lock_path)
}

fn reject_symlinked_record_parents() -> std::io::Result<()> {
    let transfers = transfers_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no data directory on this platform",
        )
    })?;
    reject_symlinked_record_parents_at(&transfers)
}

fn reject_symlinked_record_parents_at(transfers: &Path) -> std::io::Result<()> {
    reject_existing_symlink_or_non_directory(transfers)?;
    reject_existing_symlink_or_non_directory(&transfers.join("sessions"))
}

fn reject_existing_symlink_or_non_directory(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing symlink import-record directory {}",
                path.display()
            ),
        )),
        Ok(metadata) if !metadata.is_dir() => Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!(
                "import-record parent is not a directory: {}",
                path.display()
            ),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
enum RecordArtifactDir {
    Transfers,
    Sessions,
}

fn collect_record_artifacts(
    dir: &Path,
    kind: RecordArtifactDir,
    records: &mut BTreeMap<String, i64>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let session_id = match kind {
            RecordArtifactDir::Transfers => name
                .strip_suffix(".journal.jsonl")
                .or_else(|| name.strip_suffix(".report.json"))
                .or_else(|| name.strip_suffix(".json")),
            RecordArtifactDir::Sessions => name
                .strip_suffix(".summary.json")
                .or_else(|| name.strip_suffix(".json")),
        };
        let Some(session_id) = session_id.and_then(safe_session_id) else {
            continue;
        };
        let updated_at = artifact_modified_unix(&path);
        records
            .entry(session_id.to_owned())
            .and_modify(|previous| *previous = (*previous).max(updated_at))
            .or_insert(updated_at);
    }
}

fn artifact_modified_unix(path: &Path) -> i64 {
    std::fs::symlink_metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::checkpoint::TrackEntry;
    use crate::transfer::matching::{AmbiguousCandidate, MatchScoreBreakdown};
    use crate::transfer::{FileFormat, JobSpec};

    fn spec(dest: TransferDest) -> JobSpec {
        JobSpec {
            source: TransferSource::SpotifyPlaylist {
                id: "spotify-playlist".to_owned(),
            },
            dest,
            media_kind: crate::transfer::ImportMediaKind::Track,
            dry_run: false,
            min_score: 0.80,
            take_best: false,
            auto_accept_ambiguous_min_score: None,
            match_policy: crate::transfer::MatchPolicy::Strict,
            allow_user_videos: false,
            cache_mode: crate::transfer::TransferCacheMode::Use,
            rematch: false,
        }
    }

    fn input(title: &str, artists: &[&str]) -> TrackInput {
        TrackInput {
            title: title.to_owned(),
            artists: artists.iter().map(|s| (*s).to_owned()).collect(),
            album_artists: vec!["Album Artist".to_owned()],
            album: Some("Album".to_owned()),
            album_id: Some("spotify:album-id".to_owned()),
            album_uri: Some("spotify:album:uri".to_owned()),
            album_release_date: Some("2026-07-01".to_owned()),
            album_release_date_precision: Some("day".to_owned()),
            album_total_tracks: Some(10),
            album_type: Some("album".to_owned()),
            album_art_url: Some("https://i.scdn.co/image/cover".to_owned()),
            disc_number: Some(1),
            track_number: Some(2),
            duration_secs: Some(180),
            isrc: Some("ISRC".to_owned()),
            explicit: Some(true),
            source_url: Some("https://open.spotify.com/track/t".to_owned()),
            source_key: format!("spotify:track:{title}"),
            known_video_id: None,
        }
    }

    fn entry(input: TrackInput, outcome: Option<MatchOutcome>, written: bool) -> TrackEntry {
        TrackEntry {
            input,
            outcome,
            review_decision: None,
            written,
        }
    }

    #[test]
    fn session_projects_checkpoint_rows_for_review() {
        let breakdown = MatchScoreBreakdown {
            total: 0.78,
            raw_total: 0.78,
            title: 0.90,
            artist: 0.75,
            duration: 1.0,
            album_bonus: 0.05,
            quality_bonus: 0.0,
            identity_penalty: 0.0,
            non_music_penalty: 0.0,
            accept_blocked: false,
            reject_reason: None,
            reason_codes: Vec::new(),
            ..MatchScoreBreakdown::default()
        };
        let mut cp = Checkpoint::new(
            "sp2yt-20260708-abcd".to_owned(),
            spec(TransferDest::LocalPlaylist {
                name: Some("Imported".to_owned()),
            }),
            vec![
                entry(
                    input("Matched", &["A"]),
                    Some(MatchOutcome::Matched {
                        key: "vid-a".to_owned(),
                        score: 0.94,
                        display: "A — Matched".to_owned(),
                        title: Some("Matched".to_owned()),
                        artist: Some("A".to_owned()),
                        album: Some("Album".to_owned()),
                        duration_secs: Some(180),
                        score_breakdown: None,
                    }),
                    true,
                ),
                entry(
                    input("Maybe", &["B"]),
                    Some(MatchOutcome::Ambiguous {
                        candidates: vec![AmbiguousCandidate {
                            key: "vid-b".to_owned(),
                            score: 0.78,
                            display: "B — Maybe".to_owned(),
                            score_breakdown: Some(breakdown.clone()),
                        }],
                    }),
                    false,
                ),
                entry(
                    input("Missing", &["C"]),
                    Some(MatchOutcome::NotFound),
                    false,
                ),
                entry(
                    input("Skipped", &["D"]),
                    Some(MatchOutcome::SkippedLocal),
                    false,
                ),
                entry(input("Pending", &["E"]), None, false),
            ],
        );
        cp.stage = Stage::Writing;
        cp.source_name = Some("Source Playlist".to_owned());
        cp.dest_name = Some("Imported".to_owned());

        let session = ImportSession::from_checkpoint(&cp);

        assert_eq!(session.schema_version, IMPORT_SESSION_SCHEMA_VERSION);
        assert_eq!(session.session_id, "sp2yt-20260708-abcd");
        assert_eq!(session.source.kind, "spotify_playlist");
        assert_eq!(session.source.label.as_deref(), Some("Source Playlist"));
        assert_eq!(session.destination.kind, "local_playlist");
        assert_eq!(session.destination.label.as_deref(), Some("Imported"));
        assert_eq!(
            session.counts,
            ImportSessionCounts {
                total: 5,
                pending: 1,
                matched: 1,
                ambiguous: 1,
                not_found: 1,
                skipped_local: 1,
                capacity_skipped: 0,
                written: 1,
            }
        );
        assert_eq!(session.rows[0].status, ImportSessionRowStatus::Matched);
        assert_eq!(session.rows[0].selected_key.as_deref(), Some("vid-a"));
        assert_eq!(session.rows[0].album_artists, vec!["Album Artist"]);
        assert_eq!(session.rows[1].status, ImportSessionRowStatus::Ambiguous);
        assert_eq!(
            session.rows[1].candidates[0].score_breakdown,
            Some(breakdown)
        );
        assert_eq!(session.rows[1].review_decision, None);
        assert_eq!(session.rows[2].status, ImportSessionRowStatus::NotFound);
        assert_eq!(session.rows[3].status, ImportSessionRowStatus::SkippedLocal);
        assert_eq!(session.rows[4].status, ImportSessionRowStatus::Pending);
    }

    #[test]
    fn session_path_rejects_hostile_ids() {
        assert!(session_path("../escape").is_none());
        assert!(session_path("UPPER").is_none());
        assert!(session_path("").is_none());
        assert!(session_path("sp2yt-20260708-abcd").is_some());
        assert_eq!(
            ImportSession::delete_record("../escape")
                .expect_err("hostile id must be rejected")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn file_destination_endpoint_names_format() {
        let cp = Checkpoint::new(
            "sp2yt-20260708-ef01".to_owned(),
            spec(TransferDest::File {
                path: "out.csv".into(),
                format: FileFormat::Csv,
            }),
            Vec::new(),
        );
        let session = ImportSession::from_checkpoint(&cp);
        assert_eq!(session.destination.kind, "file_csv");
    }

    #[test]
    fn endpoint_display_prefers_label_then_key_then_kind() {
        assert_eq!(
            SessionEndpoint {
                kind: "kind".to_owned(),
                key: Some("key".to_owned()),
                label: Some("label".to_owned()),
            }
            .display(),
            "label"
        );
        assert_eq!(
            SessionEndpoint {
                kind: "kind".to_owned(),
                key: Some("key".to_owned()),
                label: None,
            }
            .display(),
            "key"
        );
        assert_eq!(
            SessionEndpoint {
                kind: "kind".to_owned(),
                key: None,
                label: None,
            }
            .display(),
            "kind"
        );
    }

    #[test]
    fn save_writes_summary_sidecar_used_by_list_all() {
        let cp = Checkpoint::new(
            "sp2yt-session-summary-sidecar".to_owned(),
            spec(TransferDest::LocalPlaylist {
                name: Some("Imported".to_owned()),
            }),
            vec![entry(input("Matched", &["A"]), None, false)],
        );
        let session = ImportSession::from_checkpoint(&cp);
        session.save().expect("save import session");
        let summary_path = session_summary_path(&session.session_id).expect("summary path");
        assert!(summary_path.exists(), "summary sidecar should be written");

        let summary = ImportSession::list_all()
            .into_iter()
            .find(|summary| summary.session_id == session.session_id)
            .expect("summary from list_all");
        assert_eq!(summary.session_id, session.session_id);
        assert_eq!(summary.counts.total, 1);
    }

    #[test]
    fn derived_summary_failure_does_not_turn_authoritative_session_commit_into_failure() {
        let job_id = "sp2yt-session-summary-derived-failure";
        let summary = session_summary_path(job_id).expect("summary path");
        std::fs::create_dir_all(&summary).expect("block summary pathname with a directory");
        let session = ImportSession {
            session_id: job_id.to_owned(),
            job_id: job_id.to_owned(),
            updated_at: 42,
            ..ImportSession::default()
        };

        session
            .save()
            .expect("authoritative session commit ignores derived summary failure");

        assert_eq!(ImportSession::load(job_id).unwrap().updated_at, 42);
        assert!(
            ImportSession::list_all()
                .iter()
                .any(|summary| summary.session_id == job_id)
        );
        std::fs::remove_dir_all(summary).unwrap();
        ImportSession::delete_record(job_id).unwrap();
    }

    #[test]
    fn list_all_does_not_recreate_a_missing_summary_sidecar() {
        let cp = Checkpoint::new(
            "sp2yt-session-read-only-list".to_owned(),
            spec(TransferDest::LocalPlaylist {
                name: Some("Imported".to_owned()),
            }),
            vec![entry(input("Matched", &["A"]), None, false)],
        );
        let session = ImportSession::from_checkpoint(&cp);
        session.save().expect("save import session");
        let summary_path = session_summary_path(&session.session_id).expect("summary path");
        std::fs::remove_file(&summary_path).expect("remove summary sidecar");

        assert!(
            ImportSession::list_all()
                .iter()
                .any(|summary| summary.session_id == session.session_id),
            "the full session remains discoverable without its sidecar"
        );
        assert!(
            !summary_path.exists(),
            "read-only discovery must not resurrect a removed sidecar"
        );
    }

    #[test]
    fn download_updates_mark_rows_done_or_failed() {
        let cp = Checkpoint::new(
            "sp2yt-session-download-row".to_owned(),
            spec(TransferDest::LocalPlaylist {
                name: Some("Imported".to_owned()),
            }),
            vec![entry(
                input("Matched", &["A"]),
                Some(MatchOutcome::Matched {
                    key: "vid-a".to_owned(),
                    score: 0.94,
                    display: "A - Matched".to_owned(),
                    title: Some("Matched".to_owned()),
                    artist: Some("A".to_owned()),
                    album: Some("Album".to_owned()),
                    duration_secs: Some(180),
                    score_breakdown: None,
                }),
                false,
            )],
        );
        ImportSession::from_checkpoint(&cp)
            .save()
            .expect("save import session");

        let path = PathBuf::from("/tmp/imported/Matched.m4a");
        record_download_done(&cp.job_id, 1, path.clone()).expect("record download done");

        let done = ImportSession::load(&cp.job_id).expect("load done session");
        assert!(done.rows[0].written);
        assert_eq!(done.rows[0].local_path, Some(path));
        assert!(done.rows[0].errors.is_empty());
        assert_eq!(done.counts.written, 1);

        record_download_error(&cp.job_id, 1, "network failed").expect("record download error");

        let failed = ImportSession::load(&cp.job_id).expect("load failed session");
        assert!(!failed.rows[0].written);
        assert_eq!(failed.rows[0].local_path, None);
        assert_eq!(failed.rows[0].errors, vec!["network failed"]);
        assert_eq!(failed.counts.written, 0);
    }

    #[test]
    fn delete_record_removes_history_artifacts_but_keeps_imported_audio() {
        let job_id = "sp2yt-session-delete-record";
        let mut cp = Checkpoint::new(
            job_id.to_owned(),
            spec(TransferDest::LocalPlaylist {
                name: Some("Imported".to_owned()),
            }),
            vec![entry(input("Keep Me", &["Artist"]), None, false)],
        );
        cp.save().expect("save checkpoint");
        cp.append_track_journal(0, false)
            .expect("append checkpoint journal");
        super::super::checkpoint::TransferReport {
            job_id: job_id.to_owned(),
            ..super::super::checkpoint::TransferReport::default()
        }
        .save()
        .expect("save report");

        let data_dir = crate::paths::data_dir().expect("test data dir");
        std::fs::create_dir_all(&data_dir).expect("create test data dir");
        let audio = data_dir.join("kept-import-after-record-delete.m4a");
        std::fs::write(&audio, b"audio remains").expect("write imported audio fixture");
        let mut session = ImportSession::from_checkpoint(&cp);
        session.rows[0].local_path = Some(audio.clone());
        session.save().expect("save import session");

        let transfers = data_dir.join("transfers");
        let artifacts = [
            super::super::checkpoint::checkpoint_path(job_id).expect("checkpoint path"),
            super::super::checkpoint::report_path(job_id).expect("report path"),
            transfers.join(format!("{job_id}.journal.jsonl")),
            session_summary_path(job_id).expect("summary path"),
            session_path(job_id).expect("session path"),
        ];
        assert!(artifacts.iter().all(|path| path.exists()));

        assert_eq!(
            ImportSession::delete_record(job_id).unwrap(),
            artifacts.len()
        );
        assert!(artifacts.iter().all(|path| !path.exists()));
        assert!(
            audio.exists(),
            "record deletion must not delete imported audio"
        );
        assert!(
            ImportSession::list_all()
                .into_iter()
                .all(|summary| summary.session_id != job_id)
        );
        let _ = std::fs::remove_file(audio);
    }

    #[test]
    fn active_import_record_lock_blocks_delete_until_writer_finishes() {
        let job_id = "sp2yt-session-delete-active-lock";
        let session = ImportSession {
            session_id: job_id.to_owned(),
            job_id: job_id.to_owned(),
            ..ImportSession::default()
        };
        session.save().expect("save locked deletion fixture");

        let guard = ImportRecordGuard::try_acquire(job_id).expect("hold active writer lock");
        let error = ImportSession::delete_record(job_id)
            .expect_err("active writer must prevent record deletion");
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(ImportSession::record_exists(job_id));

        let lock_path = transfers_dir()
            .expect("transfers dir")
            .join("record-locks")
            .join(format!("{job_id}.lock"));
        assert!(lock_path.exists());
        drop(guard);

        assert_eq!(ImportSession::delete_record(job_id).unwrap(), 2);
        assert!(!ImportSession::record_exists(job_id));
        assert!(
            lock_path.exists(),
            "the permanent lock inode must never be deleted"
        );
    }

    #[test]
    fn record_entry_index_includes_orphan_history_artifacts() {
        let transfers = transfers_dir().expect("transfers dir");
        let sessions = transfers.join("sessions");
        std::fs::create_dir_all(&sessions).expect("create sessions dir");
        let fixtures = [
            (
                "sp2yt-orphan-checkpoint-only",
                transfers.join("sp2yt-orphan-checkpoint-only.json"),
            ),
            (
                "sp2yt-orphan-report-only",
                transfers.join("sp2yt-orphan-report-only.report.json"),
            ),
            (
                "sp2yt-orphan-journal-only",
                transfers.join("sp2yt-orphan-journal-only.journal.jsonl"),
            ),
            (
                "sp2yt-orphan-summary-only",
                sessions.join("sp2yt-orphan-summary-only.summary.json"),
            ),
        ];
        for (_, path) in &fixtures {
            std::fs::write(path, b"orphan").expect("write orphan artifact");
        }

        let entries = ImportSession::list_record_entries();
        for (session_id, _) in &fixtures {
            assert!(
                entries.iter().any(|entry| entry.session_id == *session_id),
                "missing orphan record {session_id}"
            );
        }

        for (session_id, _) in fixtures {
            ImportSession::delete_record(session_id).expect("clean orphan fixture");
        }
    }

    #[cfg(unix)]
    #[test]
    fn deletion_rejects_symlinked_transfer_or_session_parent() {
        use std::os::unix::fs::symlink;

        let root = crate::paths::data_dir()
            .expect("test data dir")
            .join("record-delete-symlink-parent-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("real")).expect("create symlink target");

        let transfers_link = root.join("transfers-link");
        symlink(root.join("real"), &transfers_link).expect("link transfers parent");
        assert_eq!(
            reject_symlinked_record_parents_at(&transfers_link)
                .expect_err("symlinked transfers parent must be rejected")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );

        let transfers = root.join("transfers");
        std::fs::create_dir_all(&transfers).expect("create transfers parent");
        symlink(root.join("real"), transfers.join("sessions")).expect("link sessions parent");
        assert_eq!(
            reject_symlinked_record_parents_at(&transfers)
                .expect_err("symlinked sessions parent must be rejected")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
