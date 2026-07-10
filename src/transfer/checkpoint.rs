//! Per-job durability: `<data dir>/transfers/<job-id>.json` snapshots the fetched track
//! list, every match outcome, and per-track write status — so a crash, rate-limit abort,
//! or dry-run picks up exactly where it left off (`ytt transfer resume <job-id>`). The
//! human-facing summary lands next to it as `<job-id>.report.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::matching::{MatchOutcome, MatchScoreBreakdown, TrackInput};
use super::{JobSpec, Stage};
use crate::util::safe_fs;

pub const CHECKPOINT_VERSION: u32 = 1;
const CHECKPOINT_JOURNAL_VERSION: u32 = 1;
const CHECKPOINT_JOURNAL_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub version: u32,
    pub job_id: String,
    pub spec: JobSpec,
    pub created_at: i64,
    pub updated_at: i64,
    pub stage: Stage,
    /// The destination created for this job (idempotent creation: recorded before the
    /// first write, so a resume never creates a second playlist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_id: Option<String>,
    /// Human name of the destination (report/UX).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest_name: Option<String>,
    /// Human name of the fetched source, preserved for import-session review surfaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    /// Source items that were never matchable (Spotify local files / episodes / removed
    /// tracks), counted at fetch time for the report.
    #[serde(default)]
    pub skipped_local: u32,
    /// Diagnostic counters accumulated while matching. This is intentionally summary-level
    /// so resumed jobs can explain why rows landed in review without storing every probe.
    #[serde(default, skip_serializing_if = "MatchStats::is_empty")]
    pub match_stats: MatchStats,
    /// A systemic provider/budget failure pauses the job without fabricating
    /// `NotFound` outcomes. Completed rows remain durable and unresolved rows stay
    /// pending for the next resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_reason: Option<JobDeferReason>,
    /// Bounded, source-order keyed evidence for unresolved/review rows. Keeping this
    /// outside `TrackEntry` preserves the compact hot row and old checkpoint shape.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub match_traces: BTreeMap<u32, MatchTrace>,
    /// Snapshot generation makes journal compaction crash-safe: if the atomic
    /// snapshot wins but deleting the old journal does not, stale records are ignored.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    journal_generation: u64,
    pub tracks: Vec<TrackEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackEntry {
    pub input: TrackInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<MatchOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<ReviewDecision>,
    #[serde(default)]
    pub written: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReviewDecision {
    Accepted {
        key: String,
        score: f32,
        display: String,
    },
    Rejected,
    Skipped,
}

impl ReviewDecision {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Accepted { .. } => "accepted",
            Self::Rejected => "rejected",
            Self::Skipped => "skipped",
        }
    }
}

impl Checkpoint {
    pub fn new(job_id: String, spec: JobSpec, tracks: Vec<TrackEntry>) -> Self {
        let now = crate::signals::unix_now();
        Self {
            version: CHECKPOINT_VERSION,
            job_id,
            spec,
            created_at: now,
            updated_at: now,
            stage: Stage::Fetching,
            dest_id: None,
            dest_name: None,
            source_name: None,
            skipped_local: 0,
            match_stats: MatchStats::default(),
            defer_reason: None,
            match_traces: BTreeMap::new(),
            journal_generation: 0,
            tracks,
        }
    }

    pub fn load(job_id: &str) -> anyhow::Result<Self> {
        use anyhow::{Context, bail};
        let path = checkpoint_path(job_id)
            .ok_or_else(|| anyhow::anyhow!("no data directory on this platform"))?;
        let text = safe_fs::read_to_string_no_symlink(&path)
            .with_context(|| format!("no checkpoint for job `{job_id}`"))?;
        let mut cp: Checkpoint = serde_json::from_str(&text).context("checkpoint is corrupt")?;
        if cp.version != CHECKPOINT_VERSION {
            bail!(
                "checkpoint version {} is not supported (expected {CHECKPOINT_VERSION})",
                cp.version
            );
        }
        if let Some(path) = checkpoint_journal_path(job_id) {
            replay_checkpoint_journal(&mut cp, &path)
                .context("checkpoint outcome journal is corrupt")?;
        }
        Ok(cp)
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        self.updated_at = crate::signals::unix_now();
        let Some(path) = checkpoint_path(&self.job_id) else {
            return Ok(());
        };
        let previous_generation = self.journal_generation;
        self.journal_generation = self.journal_generation.saturating_add(1);
        if let Err(error) = safe_fs::write_private_atomic_json(&path, self) {
            self.journal_generation = previous_generation;
            return Err(error);
        }
        clear_checkpoint_journal(&self.job_id)
    }

    /// Append one changed track to the compact checkpoint's outcome journal. A later
    /// durable append syncs all earlier non-durable records in the same file, allowing
    /// the engine to coalesce fsync while keeping a bounded crash-loss window.
    pub fn append_track_journal(&self, index: usize, durable: bool) -> std::io::Result<u64> {
        self.append_tracks_journal(std::slice::from_ref(&index), durable)
    }

    /// Append several changed tracks with one file open and, when requested, one fsync.
    /// Each row remains an independently checksummed JSONL record for crash replay.
    pub fn append_tracks_journal(&self, indexes: &[usize], durable: bool) -> std::io::Result<u64> {
        if indexes.is_empty() {
            return Ok(0);
        }
        let Some(path) = checkpoint_journal_path(&self.job_id) else {
            return Ok(0);
        };
        let block = self.track_journal_block(indexes)?;
        if durable {
            safe_fs::append_private_jsonl_durable(&path, &block)?;
        } else {
            safe_fs::append_private_jsonl(&path, &block)?;
        }
        Ok(block.len() as u64 + 1)
    }

    fn track_journal_block(&self, indexes: &[usize]) -> std::io::Result<String> {
        let mut lines = Vec::with_capacity(indexes.len());
        for &index in indexes {
            let Some(entry) = self.tracks.get(index) else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("track index {index} is out of range"),
                ));
            };
            let payload = CheckpointJournalPayload {
                checkpoint_version: self.version,
                journal_generation: self.journal_generation,
                job_id: self.job_id.clone(),
                source_order: index as u32 + 1,
                outcome: entry.outcome.clone(),
                review_decision: entry.review_decision.clone(),
                written: entry.written,
            };
            let checksum = journal_checksum(&payload)?;
            let record = CheckpointJournalRecord {
                version: CHECKPOINT_JOURNAL_VERSION,
                payload,
                checksum,
            };
            lines.push(serde_json::to_string(&record).map_err(std::io::Error::other)?);
        }
        Ok(lines.join("\n"))
    }

    /// Known jobs, newest first: `(job_id, stage, updated_at)`.
    pub fn list_all() -> Vec<(String, Stage, i64)> {
        let Some(dir) = transfers_dir() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<(String, Stage, i64)> = entries
            .filter_map(|e| {
                let path = e.ok()?.path();
                let name = path.file_name()?.to_str()?;
                let job_id = name.strip_suffix(".json")?;
                if job_id.ends_with(".report") {
                    return None;
                }
                let text = safe_fs::read_to_string_no_symlink(&path).ok()?;
                let cp: Checkpoint = serde_json::from_str(&text).ok()?;
                Some((job_id.to_owned(), cp.stage, cp.updated_at))
            })
            .collect();
        out.sort_by_key(|(_, _, updated)| std::cmp::Reverse(*updated));
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointJournalPayload {
    checkpoint_version: u32,
    #[serde(default)]
    journal_generation: u64,
    job_id: String,
    source_order: u32,
    outcome: Option<MatchOutcome>,
    review_decision: Option<ReviewDecision>,
    written: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointJournalRecord {
    version: u32,
    payload: CheckpointJournalPayload,
    checksum: String,
}

fn journal_checksum(payload: &CheckpointJournalPayload) -> std::io::Result<String> {
    let bytes = serde_json::to_vec(payload).map_err(std::io::Error::other)?;
    let digest = Sha256::digest(bytes);
    Ok(format!("{digest:x}"))
}

fn checkpoint_journal_path(job_id: &str) -> Option<PathBuf> {
    Some(transfers_dir()?.join(format!("{}.journal.jsonl", safe_job_id(job_id)?)))
}

fn replay_checkpoint_journal(cp: &mut Checkpoint, path: &Path) -> anyhow::Result<()> {
    let bytes = match safe_fs::read_no_symlink_limited(path, CHECKPOINT_JOURNAL_MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let ends_with_newline = bytes.ends_with(b"\n");
    // Lossy decoding lets a crash in the middle of a multi-byte character become the
    // same ignored final fragment as any other truncated JSON. Interior corruption is
    // still rejected when that line fails JSON decoding.
    let text = String::from_utf8_lossy(&bytes);
    let lines = text.split('\n').collect::<Vec<_>>();
    for (line_index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let is_last_fragment = line_index + 1 == lines.len() && !ends_with_newline;
        let record = match serde_json::from_str::<CheckpointJournalRecord>(line) {
            Ok(record) => record,
            Err(_) if is_last_fragment => {
                tracing::warn!(
                    job = %cp.job_id,
                    "ignoring truncated final checkpoint journal record"
                );
                break;
            }
            Err(error) => return Err(error.into()),
        };
        if record.version != CHECKPOINT_JOURNAL_VERSION
            || record.payload.checkpoint_version != cp.version
            || record.payload.job_id != cp.job_id
        {
            anyhow::bail!("checkpoint journal record scope/version mismatch");
        }
        if journal_checksum(&record.payload)? != record.checksum {
            anyhow::bail!("checkpoint journal checksum mismatch");
        }
        if record.payload.journal_generation < cp.journal_generation {
            continue;
        }
        if record.payload.journal_generation > cp.journal_generation {
            anyhow::bail!("checkpoint journal generation is newer than its snapshot");
        }
        let Some(index) = record
            .payload
            .source_order
            .checked_sub(1)
            .map(|value| value as usize)
        else {
            anyhow::bail!("checkpoint journal source order is zero");
        };
        let Some(entry) = cp.tracks.get_mut(index) else {
            anyhow::bail!(
                "checkpoint journal source order {} is out of range",
                record.payload.source_order
            );
        };
        entry.outcome = record.payload.outcome;
        entry.review_decision = record.payload.review_decision;
        entry.written = record.payload.written;
    }
    Ok(())
}

fn clear_checkpoint_journal(job_id: &str) -> std::io::Result<()> {
    let Some(path) = checkpoint_journal_path(job_id) else {
        return Ok(());
    };
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn transfers_dir() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("transfers"))
}

/// Job ids are generated by [`super::new_job_id`] ([a-z0-9-]); reject anything else so a
/// hostile `--resume ../../x` can't escape the transfers directory.
fn safe_job_id(job_id: &str) -> Option<&str> {
    (!job_id.is_empty()
        && job_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then_some(job_id)
}

pub fn checkpoint_path(job_id: &str) -> Option<PathBuf> {
    Some(transfers_dir()?.join(format!("{}.json", safe_job_id(job_id)?)))
}

pub fn report_path(job_id: &str) -> Option<PathBuf> {
    Some(transfers_dir()?.join(format!("{}.report.json", safe_job_id(job_id)?)))
}

const TRANSFER_REPORT_SCHEMA_VERSION: u32 = 5;

pub const MAX_MATCH_TRACE_PROBES: usize = 16;
pub const MAX_MATCH_TRACE_CANDIDATES: usize = 5;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobDeferReason {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Sanitized, bounded human-facing context; never store provider bodies/cookies.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<u64>,
    pub deferred_at: i64,
}

impl JobDeferReason {
    pub fn new(code: impl Into<String>, backend: Option<&str>, message: impl AsRef<str>) -> Self {
        let sanitized = crate::util::sanitize::sanitize_error_text(message.as_ref());
        Self {
            code: code.into(),
            backend: backend.map(str::to_owned),
            message: sanitized.chars().take(500).collect(),
            retry_after_secs: None,
            deferred_at: crate::signals::unix_now(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProbeTrace {
    pub backend: String,
    pub intent: String,
    pub query: String,
    pub status: String,
    pub result_count: u32,
    pub elapsed_ms: u64,
    pub queue_ms: u64,
    pub pace_ms: u64,
    pub attempt: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MatchTrace {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<ProbeTrace>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<ReportCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
}

impl MatchTrace {
    pub fn push_probe(&mut self, mut probe: ProbeTrace) {
        if self.probes.len() >= MAX_MATCH_TRACE_PROBES {
            return;
        }
        probe.query = probe.query.chars().take(300).collect();
        self.probes.push(probe);
    }

    pub fn set_candidates(&mut self, candidates: impl IntoIterator<Item = ReportCandidate>) {
        self.candidates = candidates
            .into_iter()
            .take(MAX_MATCH_TRACE_CANDIDATES)
            .collect();
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MatchStats {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cache_hits: BTreeMap<String, u32>,
    pub album_groups_attempted: u32,
    pub album_groups_matched: u32,
    pub album_tracks_matched: u32,
    pub catalog_searches: u32,
    pub video_searches: u32,
    pub ytm_video_catalog_searches: u32,
    pub public_video_searches: u32,
    pub preflight_lookups: u32,
    pub authenticated_catalog_degraded: u32,
    pub query_cache_hits: u32,
    pub video_meta_cache_hits: u32,
    pub catalog_http_pages: u32,
    pub video_process_spawns: u32,
    pub preflight_process_spawns: u32,
    pub successful_empty_probes: u32,
    pub retries: u32,
    pub circuit_opens: u32,
    pub deferred_jobs: u32,
    pub capacity_skipped: u32,
    pub candidates_fetched: u32,
    pub candidates_deduped: u32,
    pub candidates_rejected: u32,
    pub candidates_review_only: u32,
    pub cache_evictions: u32,
    pub checkpoint_bytes: u64,
    pub checkpoint_flushes: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_errors: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub terminal_reasons: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub elapsed_ms: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_kinds: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub quality_tiers: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reason_codes: BTreeMap<String, u32>,
}

impl MatchStats {
    pub fn is_empty(&self) -> bool {
        self.cache_hits.is_empty()
            && self.album_groups_attempted == 0
            && self.album_groups_matched == 0
            && self.album_tracks_matched == 0
            && self.catalog_searches == 0
            && self.video_searches == 0
            && self.ytm_video_catalog_searches == 0
            && self.public_video_searches == 0
            && self.preflight_lookups == 0
            && self.authenticated_catalog_degraded == 0
            && self.query_cache_hits == 0
            && self.video_meta_cache_hits == 0
            && self.catalog_http_pages == 0
            && self.video_process_spawns == 0
            && self.preflight_process_spawns == 0
            && self.successful_empty_probes == 0
            && self.retries == 0
            && self.circuit_opens == 0
            && self.deferred_jobs == 0
            && self.capacity_skipped == 0
            && self.candidates_fetched == 0
            && self.candidates_deduped == 0
            && self.candidates_rejected == 0
            && self.candidates_review_only == 0
            && self.cache_evictions == 0
            && self.checkpoint_bytes == 0
            && self.checkpoint_flushes == 0
            && self.provider_errors.is_empty()
            && self.terminal_reasons.is_empty()
            && self.elapsed_ms.is_empty()
            && self.source_kinds.is_empty()
            && self.quality_tiers.is_empty()
            && self.reason_codes.is_empty()
    }

    pub fn bump_cache_hit(&mut self, kind: &str) {
        *self.cache_hits.entry(kind.to_owned()).or_default() += 1;
    }

    pub fn bump_source_kind(&mut self, kind: &str) {
        if !kind.is_empty() {
            *self.source_kinds.entry(kind.to_owned()).or_default() += 1;
        }
    }

    pub fn bump_quality_tier(&mut self, tier: &str) {
        if !tier.is_empty() {
            *self.quality_tiers.entry(tier.to_owned()).or_default() += 1;
        }
    }

    pub fn bump_reason_code(&mut self, code: &str) {
        if !code.is_empty() {
            *self.reason_codes.entry(code.to_owned()).or_default() += 1;
        }
    }

    pub fn bump_provider_error(&mut self, code: &str) {
        if !code.is_empty() {
            *self.provider_errors.entry(code.to_owned()).or_default() += 1;
        }
    }

    pub fn bump_terminal_reason(&mut self, code: &str) {
        if !code.is_empty() {
            *self.terminal_reasons.entry(code.to_owned()).or_default() += 1;
        }
    }

    pub fn add_elapsed_ms(&mut self, stage: &str, elapsed_ms: u64) {
        if !stage.is_empty() {
            *self.elapsed_ms.entry(stage.to_owned()).or_default() += elapsed_ms;
        }
    }
}

/// The end-of-job summary (also serialized next to the checkpoint).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferReport {
    pub schema_version: u32,
    pub job_id: String,
    pub total: u32,
    pub matched: u32,
    pub auto_accepted: u32,
    pub written: u32,
    pub ambiguous: Vec<ReportRow>,
    pub not_found: Vec<ReportRow>,
    /// Pending rows that were not searched to completion because the job was deferred.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<ReportRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capacity_skipped: Vec<ReportRow>,
    pub skipped_local: u32,
    pub duplicates_dropped: u32,
    pub elapsed_secs: u64,
    #[serde(default, skip_serializing_if = "MatchStats::is_empty")]
    pub match_stats: MatchStats,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_reason: Option<JobDeferReason>,
}

impl Default for TransferReport {
    fn default() -> Self {
        Self {
            schema_version: TRANSFER_REPORT_SCHEMA_VERSION,
            job_id: String::new(),
            total: 0,
            matched: 0,
            auto_accepted: 0,
            written: 0,
            ambiguous: Vec::new(),
            not_found: Vec::new(),
            deferred: Vec::new(),
            capacity_skipped: Vec::new(),
            skipped_local: 0,
            duplicates_dropped: 0,
            elapsed_secs: 0,
            match_stats: MatchStats::default(),
            defer_reason: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportRow {
    pub title: String,
    pub artists: String,
    /// Why it needs attention (top ambiguous candidates / "no match").
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_order: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub album_artists: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<ReportCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_score: Option<f32>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportCandidate {
    pub key: String,
    pub score: f32,
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_breakdown: Option<MatchScoreBreakdown>,
}

impl TransferReport {
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = report_path(&self.job_id) else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
    }

    /// A terminal/status-line friendly summary.
    pub fn render_text(&self) -> String {
        let mut out = format!(
            "{}/{} matched, {} written, {} ambiguous, {} not found",
            self.matched,
            self.total,
            self.written,
            self.ambiguous.len(),
            self.not_found.len(),
        );
        if self.auto_accepted > 0 {
            out.push_str(&format!(", {} auto-accepted", self.auto_accepted));
        }
        if !self.deferred.is_empty() {
            out.push_str(&format!(", {} deferred", self.deferred.len()));
        }
        if !self.capacity_skipped.is_empty() {
            out.push_str(&format!(
                ", {} skipped at destination capacity",
                self.capacity_skipped.len()
            ));
        }
        if self.skipped_local > 0 {
            out.push_str(&format!(", {} local/episode skipped", self.skipped_local));
        }
        if self.duplicates_dropped > 0 {
            out.push_str(&format!(", {} duplicates dropped", self.duplicates_dropped));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::{FileFormat, TransferDest, TransferSource};

    fn spec() -> JobSpec {
        JobSpec {
            source: TransferSource::SpotifyPlaylist {
                id: "37i9dQ".to_owned(),
            },
            dest: TransferDest::YtmNewPlaylist { name: None },
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

    fn entry(title: &str) -> TrackEntry {
        TrackEntry {
            input: TrackInput {
                title: title.to_owned(),
                artists: vec!["A".to_owned()],
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
                duration_secs: Some(200),
                isrc: None,
                explicit: None,
                source_url: None,
                source_key: format!("spotify:track:{title}"),
                known_video_id: None,
            },
            outcome: None,
            review_decision: None,
            written: false,
        }
    }

    #[test]
    fn round_trips_with_mixed_outcomes() {
        let mut cp = Checkpoint::new(
            "sp2yt-test-0001".to_owned(),
            spec(),
            vec![entry("one"), entry("two")],
        );
        cp.tracks[0].outcome = Some(MatchOutcome::Matched {
            key: "vid1".to_owned(),
            score: 0.93,
            display: "A — one".to_owned(),
            title: Some("one".to_owned()),
            artist: Some("A".to_owned()),
            album: None,
            duration_secs: Some(200),
            score_breakdown: None,
        });
        cp.tracks[0].written = true;
        cp.tracks[1].outcome = Some(MatchOutcome::NotFound);
        cp.tracks[1].review_decision = Some(ReviewDecision::Rejected);
        cp.stage = Stage::Writing;
        cp.dest_id = Some("PL123".to_owned());

        let json = serde_json::to_string(&cp).unwrap();
        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tracks.len(), 2);
        assert!(back.tracks[0].written);
        assert!(matches!(
            back.tracks[0].outcome,
            Some(MatchOutcome::Matched { .. })
        ));
        assert!(!back.tracks[1].written);
        assert_eq!(
            back.tracks[1].review_decision,
            Some(ReviewDecision::Rejected)
        );
        assert_eq!(back.dest_id.as_deref(), Some("PL123"));
        assert_eq!(back.stage, Stage::Writing);
        // FileFormat round-trip sanity for specs that carry it.
        let dest = TransferDest::File {
            path: std::path::PathBuf::from("/tmp/x.csv"),
            format: FileFormat::Csv,
        };
        let s = serde_json::to_string(&dest).unwrap();
        assert!(matches!(
            serde_json::from_str::<TransferDest>(&s).unwrap(),
            TransferDest::File {
                format: FileFormat::Csv,
                ..
            }
        ));
    }

    #[test]
    fn job_spec_defaults_new_matching_fields_for_old_json() {
        let json = r#"{
            "source": {"kind": "spotify_liked"},
            "dest": {"kind": "ytm_likes"},
            "dry_run": false,
            "min_score": 0.8,
            "take_best": false,
            "rematch": false
        }"#;
        let spec: JobSpec = serde_json::from_str(json).expect("old job spec should deserialize");

        assert_eq!(spec.match_policy, crate::transfer::MatchPolicy::Strict);
        assert_eq!(spec.cache_mode, crate::transfer::TransferCacheMode::Use);
        assert!(!spec.allow_user_videos);
    }

    #[test]
    fn hostile_job_ids_are_rejected() {
        assert!(checkpoint_path("../escape").is_none());
        assert!(checkpoint_path("UPPER").is_none());
        assert!(checkpoint_path("").is_none());
        assert!(checkpoint_path("sp2yt-20260702-abcd").is_some());
    }

    #[test]
    fn report_renders_counts() {
        let report = TransferReport {
            job_id: "j".to_owned(),
            total: 10,
            matched: 8,
            auto_accepted: 2,
            written: 8,
            ambiguous: vec![ReportRow {
                title: "t".to_owned(),
                artists: "a".to_owned(),
                note: "n".to_owned(),
                ..ReportRow::default()
            }],
            not_found: Vec::new(),
            skipped_local: 1,
            duplicates_dropped: 0,
            elapsed_secs: 42,
            ..TransferReport::default()
        };
        let text = report.render_text();
        assert!(text.contains("8/10 matched"));
        assert!(text.contains("2 auto-accepted"));
        assert!(text.contains("1 ambiguous"));
        assert!(text.contains("local/episode skipped"));
    }

    #[test]
    fn old_checkpoint_json_defaults_trace_and_defer_fields() {
        let mut cp = Checkpoint::new("sp2yt-old-shape".to_owned(), spec(), vec![entry("one")]);
        cp.defer_reason = Some(JobDeferReason::new(
            "backend_unavailable",
            Some("public_video"),
            "HTTP 403 cookie=secret",
        ));
        cp.match_traces.insert(
            1,
            MatchTrace {
                terminal_reason: Some("backend_unavailable".to_owned()),
                ..MatchTrace::default()
            },
        );
        let mut value = serde_json::to_value(&cp).unwrap();
        value.as_object_mut().unwrap().remove("defer_reason");
        value.as_object_mut().unwrap().remove("match_traces");

        let old: Checkpoint = serde_json::from_value(value).unwrap();

        assert!(old.defer_reason.is_none());
        assert!(old.match_traces.is_empty());
    }

    #[test]
    fn match_trace_is_bounded_and_sanitized() {
        let mut trace = MatchTrace::default();
        for index in 0..(MAX_MATCH_TRACE_PROBES + 5) {
            trace.push_probe(ProbeTrace {
                backend: "catalog".to_owned(),
                intent: "primary".to_owned(),
                query: format!("{index}-{}", "x".repeat(500)),
                status: "empty".to_owned(),
                ..ProbeTrace::default()
            });
        }
        trace.set_candidates((0..10).map(|index| ReportCandidate {
            key: format!("key-{index}"),
            score: 0.5,
            display: "candidate".to_owned(),
            score_breakdown: None,
        }));

        assert_eq!(trace.probes.len(), MAX_MATCH_TRACE_PROBES);
        assert!(trace.probes.iter().all(|probe| probe.query.len() <= 300));
        assert_eq!(trace.candidates.len(), MAX_MATCH_TRACE_CANDIDATES);
    }

    fn journal_record(cp: &Checkpoint, index: usize) -> String {
        let entry = &cp.tracks[index];
        let payload = CheckpointJournalPayload {
            checkpoint_version: cp.version,
            journal_generation: cp.journal_generation,
            job_id: cp.job_id.clone(),
            source_order: index as u32 + 1,
            outcome: entry.outcome.clone(),
            review_decision: entry.review_decision.clone(),
            written: entry.written,
        };
        let checksum = journal_checksum(&payload).unwrap();
        serde_json::to_string(&CheckpointJournalRecord {
            version: CHECKPOINT_JOURNAL_VERSION,
            payload,
            checksum,
        })
        .unwrap()
    }

    fn journal_temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yututui-transfer-journal-tests-{}",
            std::process::id()
        ));
        safe_fs::ensure_private_dir(&dir).unwrap();
        dir.join(format!(
            "yututui-transfer-journal-{name}-{}-{}.jsonl",
            std::process::id(),
            crate::signals::unix_now()
        ))
    }

    #[test]
    fn journal_replays_latest_track_state() {
        let mut source = Checkpoint::new(
            "sp2yt-journal-replay".to_owned(),
            spec(),
            vec![entry("one")],
        );
        source.tracks[0].outcome = Some(MatchOutcome::NotFound);
        source.tracks[0].written = true;
        let line = journal_record(&source, 0);
        let path = journal_temp("replay");
        safe_fs::append_private_jsonl(&path, &line).unwrap();

        let mut target = Checkpoint::new(
            "sp2yt-journal-replay".to_owned(),
            spec(),
            vec![entry("one")],
        );
        replay_checkpoint_journal(&mut target, &path).unwrap();

        assert!(matches!(
            target.tracks[0].outcome,
            Some(MatchOutcome::NotFound)
        ));
        assert!(target.tracks[0].written);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn batched_journal_block_replays_every_row_and_latest_record_wins() {
        let mut source = Checkpoint::new(
            "sp2yt-journal-batch".to_owned(),
            spec(),
            vec![entry("one"), entry("two")],
        );
        source.tracks[0].outcome = Some(MatchOutcome::NotFound);
        source.tracks[1].outcome = Some(MatchOutcome::NotFound);
        source.tracks[1].written = true;
        let first_block = source.track_journal_block(&[0, 1]).unwrap();
        assert_eq!(first_block.lines().count(), 2);

        source.tracks[0].outcome = Some(MatchOutcome::Matched {
            key: "latest-video".to_owned(),
            score: 0.95,
            display: "A — one".to_owned(),
            title: Some("one".to_owned()),
            artist: Some("A".to_owned()),
            album: None,
            duration_secs: Some(200),
            score_breakdown: None,
        });
        let latest_block = source.track_journal_block(&[0]).unwrap();
        assert!(source.track_journal_block(&[2]).is_err());

        let path = journal_temp("batch");
        safe_fs::append_private_jsonl(&path, &first_block).unwrap();
        safe_fs::append_private_jsonl(&path, &latest_block).unwrap();
        let physical = std::fs::read_to_string(&path).unwrap();
        assert_eq!(physical.lines().count(), 3);

        let mut target = Checkpoint::new(
            "sp2yt-journal-batch".to_owned(),
            spec(),
            vec![entry("one"), entry("two")],
        );
        replay_checkpoint_journal(&mut target, &path).unwrap();

        assert!(matches!(
            target.tracks[0].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "latest-video"
        ));
        assert!(matches!(
            target.tracks[1].outcome,
            Some(MatchOutcome::NotFound)
        ));
        assert!(target.tracks[1].written);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn journal_ignores_stale_record_after_snapshot_generation_advances() {
        let mut stale =
            Checkpoint::new("sp2yt-journal-stale".to_owned(), spec(), vec![entry("one")]);
        stale.journal_generation = 7;
        stale.tracks[0].outcome = Some(MatchOutcome::NotFound);
        stale.tracks[0].written = true;
        let path = journal_temp("stale");
        safe_fs::append_private_jsonl(&path, &journal_record(&stale, 0)).unwrap();

        // This models the state after an atomic snapshot write succeeded but removing
        // the compacted journal did not: the snapshot generation is now newer than
        // every record left in the old journal.
        let mut snapshot =
            Checkpoint::new("sp2yt-journal-stale".to_owned(), spec(), vec![entry("one")]);
        snapshot.journal_generation = 8;
        snapshot.tracks[0].outcome = Some(MatchOutcome::Matched {
            key: "snapshot-video".to_owned(),
            score: 0.95,
            display: "A — one".to_owned(),
            title: Some("one".to_owned()),
            artist: Some("A".to_owned()),
            album: None,
            duration_secs: Some(200),
            score_breakdown: None,
        });

        replay_checkpoint_journal(&mut snapshot, &path).unwrap();

        assert!(matches!(
            snapshot.tracks[0].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "snapshot-video"
        ));
        assert!(!snapshot.tracks[0].written);
        assert_eq!(snapshot.journal_generation, 8);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn journal_rejects_generation_newer_than_snapshot() {
        let mut future = Checkpoint::new(
            "sp2yt-journal-future".to_owned(),
            spec(),
            vec![entry("one")],
        );
        future.journal_generation = 4;
        future.tracks[0].outcome = Some(MatchOutcome::NotFound);
        let path = journal_temp("future");
        safe_fs::append_private_jsonl(&path, &journal_record(&future, 0)).unwrap();

        let mut snapshot = Checkpoint::new(
            "sp2yt-journal-future".to_owned(),
            spec(),
            vec![entry("one")],
        );
        snapshot.journal_generation = 3;

        let error = replay_checkpoint_journal(&mut snapshot, &path).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("journal generation is newer than its snapshot"),
            "{error:#}"
        );
        assert!(snapshot.tracks[0].outcome.is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn journal_rejects_record_from_another_job_scope() {
        let foreign = Checkpoint::new(
            "sp2yt-journal-foreign".to_owned(),
            spec(),
            vec![entry("one")],
        );
        let path = journal_temp("foreign");
        safe_fs::append_private_jsonl(&path, &journal_record(&foreign, 0)).unwrap();

        let mut target = Checkpoint::new(
            "sp2yt-journal-target".to_owned(),
            spec(),
            vec![entry("one")],
        );
        let error = replay_checkpoint_journal(&mut target, &path).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("journal record scope/version mismatch"),
            "{error:#}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn journal_ignores_only_a_truncated_final_record() {
        let cp = Checkpoint::new(
            "sp2yt-journal-truncated".to_owned(),
            spec(),
            vec![entry("one")],
        );
        let path = journal_temp("truncated");
        safe_fs::append_private_jsonl(&path, &journal_record(&cp, 0)).unwrap();
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"version\":1")
            .unwrap();

        let mut target = Checkpoint::new(
            "sp2yt-journal-truncated".to_owned(),
            spec(),
            vec![entry("one")],
        );
        replay_checkpoint_journal(&mut target, &path).unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn journal_rejects_corrupt_interior_record() {
        let cp = Checkpoint::new(
            "sp2yt-journal-corrupt".to_owned(),
            spec(),
            vec![entry("one")],
        );
        let path = journal_temp("corrupt");
        safe_fs::append_private_jsonl(&path, "{not-json").unwrap();
        safe_fs::append_private_jsonl(&path, &journal_record(&cp, 0)).unwrap();

        let mut target = Checkpoint::new(
            "sp2yt-journal-corrupt".to_owned(),
            spec(),
            vec![entry("one")],
        );
        assert!(replay_checkpoint_journal(&mut target, &path).is_err());
        let _ = std::fs::remove_file(path);
    }
}
