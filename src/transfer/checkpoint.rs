//! Per-job durability: `<data dir>/transfers/<job-id>.json` snapshots the fetched track
//! list, every match outcome, and per-track write status — so a crash, rate-limit abort,
//! or dry-run picks up exactly where it left off (`ytt transfer resume <job-id>`). The
//! human-facing summary lands next to it as `<job-id>.report.json`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::matching::{MatchOutcome, MatchScoreBreakdown, TrackInput};
use super::{JobSpec, Stage};
use crate::util::safe_fs;

pub const CHECKPOINT_VERSION: u32 = 1;

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
            tracks,
        }
    }

    pub fn load(job_id: &str) -> anyhow::Result<Self> {
        use anyhow::{Context, bail};
        let path = checkpoint_path(job_id)
            .ok_or_else(|| anyhow::anyhow!("no data directory on this platform"))?;
        let text = safe_fs::read_to_string_no_symlink(&path)
            .with_context(|| format!("no checkpoint for job `{job_id}`"))?;
        let cp: Checkpoint = serde_json::from_str(&text).context("checkpoint is corrupt")?;
        if cp.version != CHECKPOINT_VERSION {
            bail!(
                "checkpoint version {} is not supported (expected {CHECKPOINT_VERSION})",
                cp.version
            );
        }
        Ok(cp)
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        self.updated_at = crate::signals::unix_now();
        let Some(path) = checkpoint_path(&self.job_id) else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
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

const TRANSFER_REPORT_SCHEMA_VERSION: u32 = 4;

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
    pub preflight_lookups: u32,
    pub authenticated_catalog_degraded: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_kinds: BTreeMap<String, u32>,
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
            && self.preflight_lookups == 0
            && self.authenticated_catalog_degraded == 0
            && self.source_kinds.is_empty()
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

    pub fn bump_reason_code(&mut self, code: &str) {
        if !code.is_empty() {
            *self.reason_codes.entry(code.to_owned()).or_default() += 1;
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
    pub skipped_local: u32,
    pub duplicates_dropped: u32,
    pub elapsed_secs: u64,
    #[serde(default, skip_serializing_if = "MatchStats::is_empty")]
    pub match_stats: MatchStats,
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
            skipped_local: 0,
            duplicates_dropped: 0,
            elapsed_secs: 0,
            match_stats: MatchStats::default(),
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
