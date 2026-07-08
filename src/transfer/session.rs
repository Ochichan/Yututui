//! Review-oriented import sessions built from transfer checkpoints.
//!
//! Checkpoints remain the idempotent execution log. This module writes a second,
//! human/workflow-shaped JSON document that can drive match review, download
//! follow-up, and Local Deck inbox organization without making UI code reverse-engineer
//! checkpoint internals.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::checkpoint::{Checkpoint, ReportCandidate, ReviewDecision};
use super::matching::{MatchOutcome, TrackInput};
use super::{Stage, TransferDest, TransferSource};
use crate::util::safe_fs;

const IMPORT_SESSION_SCHEMA_VERSION: u32 = 1;
const IMPORT_SESSION_MAX_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportSession {
    pub schema_version: u32,
    pub session_id: String,
    pub job_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub stage: Stage,
    pub source: SessionEndpoint,
    pub destination: SessionEndpoint,
    pub counts: ImportSessionCounts,
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

impl Default for ImportSession {
    fn default() -> Self {
        Self {
            schema_version: IMPORT_SESSION_SCHEMA_VERSION,
            session_id: String::new(),
            job_id: String::new(),
            created_at: 0,
            updated_at: 0,
            stage: Stage::Fetching,
            source: SessionEndpoint::default(),
            destination: SessionEndpoint::default(),
            counts: ImportSessionCounts::default(),
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
    pub written: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportSessionRow {
    pub row_id: String,
    pub source_order: u32,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl Default for ImportSessionRow {
    fn default() -> Self {
        Self {
            row_id: String::new(),
            source_order: 0,
            status: ImportSessionRowStatus::Pending,
            title: String::new(),
            artists: Vec::new(),
            album_artists: Vec::new(),
            album: None,
            album_id: None,
            album_uri: None,
            album_release_date: None,
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
            warnings: Vec::new(),
            errors: Vec::new(),
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
}

impl ImportSession {
    pub fn from_checkpoint(cp: &Checkpoint) -> Self {
        let rows: Vec<ImportSessionRow> = cp
            .tracks
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                row_from_input(
                    idx,
                    &entry.input,
                    entry.outcome.as_ref(),
                    entry.review_decision.clone(),
                    entry.written,
                )
            })
            .collect();
        let counts = ImportSessionCounts::from_rows(&rows);
        Self {
            schema_version: IMPORT_SESSION_SCHEMA_VERSION,
            session_id: cp.job_id.clone(),
            job_id: cp.job_id.clone(),
            created_at: cp.created_at,
            updated_at: cp.updated_at,
            stage: cp.stage,
            source: endpoint_from_source(&cp.spec.source, cp.source_name.clone()),
            destination: endpoint_from_dest(&cp.spec.dest, cp.dest_name.clone()),
            counts,
            rows,
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = session_path(&self.session_id) else {
            return Ok(());
        };
        safe_fs::write_private_atomic_json(&path, self)
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
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    return None;
                }
                let bytes =
                    safe_fs::read_no_symlink_limited(&path, IMPORT_SESSION_MAX_BYTES).ok()?;
                let session: ImportSession = serde_json::from_slice(&bytes).ok()?;
                Some(ImportSessionSummary {
                    session_id: session.session_id,
                    stage: session.stage,
                    updated_at: session.updated_at,
                    source: session.source,
                    destination: session.destination,
                    counts: session.counts,
                })
            })
            .collect();
        out.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at));
        out
    }
}

pub fn record_download_done(
    session_id: &str,
    source_order: u32,
    local_path: PathBuf,
) -> anyhow::Result<()> {
    let mut session = ImportSession::load(session_id)?;
    let row = row_by_source_order_mut(&mut session, source_order)?;
    row.written = true;
    row.local_path = Some(local_path);
    row.errors.clear();
    save_updated_session(session)
}

pub fn record_download_error(
    session_id: &str,
    source_order: u32,
    error: &str,
) -> anyhow::Result<()> {
    let mut session = ImportSession::load(session_id)?;
    let row = row_by_source_order_mut(&mut session, source_order)?;
    row.written = false;
    row.local_path = None;
    row.errors.clear();
    let message = error.trim();
    if !message.is_empty() {
        row.errors.push(message.chars().take(500).collect());
    }
    save_updated_session(session)
}

fn row_by_source_order_mut(
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

fn save_updated_session(mut session: ImportSession) -> anyhow::Result<()> {
    session.updated_at = crate::signals::unix_now();
    session.counts = ImportSessionCounts::from_rows(&session.rows);
    session.save().map_err(Into::into)
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
                score_breakdown: *score_breakdown,
            });
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
                    score_breakdown: c.score_breakdown,
                })
                .collect();
        }
        Some(MatchOutcome::NotFound) => {
            row.status = ImportSessionRowStatus::NotFound;
        }
        Some(MatchOutcome::SkippedLocal) => {
            row.status = ImportSessionRowStatus::SkippedLocal;
            row.warnings
                .push("spotify local file or episode skipped".to_owned());
        }
        None => {
            row.status = ImportSessionRowStatus::Pending;
        }
    }
    row
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

fn sessions_dir() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join("transfers").join("sessions"))
}

fn safe_session_id(session_id: &str) -> Option<&str> {
    (!session_id.is_empty()
        && session_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
    .then_some(session_id)
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
            dry_run: false,
            min_score: 0.80,
            take_best: false,
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
            title: 0.90,
            artist: 0.75,
            duration: 1.0,
            album_bonus: 0.05,
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
                            score_breakdown: Some(breakdown),
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
}
