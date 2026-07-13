//! Shared import-review mutations for CLI and TUI surfaces.

use anyhow::{Context as _, anyhow, bail};

use super::checkpoint::{Checkpoint, ReviewDecision};
use super::matching::{MatchOutcome, MatchScoreBreakdown};
use super::session::{
    ImportRecordGuard, ImportSession, ensure_ready_row_mutable_unlocked,
    ensure_review_row_mutable_unlocked,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewActionSummary {
    pub source_order: u32,
    pub label: &'static str,
    pub key: Option<String>,
    pub display: Option<String>,
    pub score: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewBatchSummary {
    pub session_id: String,
    pub accepted_count: u32,
    pub ready_count: u32,
    pub total_count: u32,
    pub review_left: u32,
    pub missing_left: u32,
    pub rows: Vec<ReviewActionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewReadyPlan {
    pub session_id: String,
    pub candidate_count: u32,
    pub ready_count: u32,
    pub resulting_ready_count: u32,
    pub total_count: u32,
    pub review_left: u32,
    pub missing_left: u32,
}

#[derive(Debug, Clone)]
struct SelectedCandidate {
    key: String,
    score: f32,
    display: String,
    score_breakdown: Option<MatchScoreBreakdown>,
}

pub fn accept_first_candidate(
    job_id: &str,
    source_order: u32,
) -> anyhow::Result<ReviewActionSummary> {
    let _guard = ImportRecordGuard::try_acquire(job_id)?;
    let mut cp = Checkpoint::load(job_id)?;
    let index = row_order_to_index(&cp, source_order)?;
    ensure_not_written(&cp, index)?;
    ensure_review_row_mutable_unlocked(job_id, source_order)?;
    let selected = candidates_from_outcome(&cp.tracks[index].outcome)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("row {source_order} has no candidate to accept"))?;
    accept_candidate(&mut cp, index, source_order, selected, "accepted")
}

pub fn choose_next_candidate(
    job_id: &str,
    source_order: u32,
) -> anyhow::Result<ReviewActionSummary> {
    let _guard = ImportRecordGuard::try_acquire(job_id)?;
    let mut cp = Checkpoint::load(job_id)?;
    let index = row_order_to_index(&cp, source_order)?;
    ensure_not_written(&cp, index)?;
    ensure_review_row_mutable_unlocked(job_id, source_order)?;
    let candidates = candidates_from_outcome(&cp.tracks[index].outcome);
    if candidates.is_empty() {
        bail!("row {source_order} has no candidate to choose");
    }
    let current_key = selected_key(&cp.tracks[index]);
    let next = current_key
        .and_then(|key| candidates.iter().position(|candidate| candidate.key == key))
        .map(|idx| (idx + 1) % candidates.len())
        .unwrap_or(0);
    accept_candidate(
        &mut cp,
        index,
        source_order,
        candidates[next].clone(),
        "selected",
    )
}

pub fn reject_row(job_id: &str, source_order: u32) -> anyhow::Result<ReviewActionSummary> {
    apply_terminal_decision(job_id, source_order, ReviewDecision::Rejected, "rejected")
}

pub fn skip_row(job_id: &str, source_order: u32) -> anyhow::Result<ReviewActionSummary> {
    apply_terminal_decision(job_id, source_order, ReviewDecision::Skipped, "skipped")
}

pub fn accept_all_candidates(job_id: &str) -> anyhow::Result<ReviewBatchSummary> {
    let _guard = ImportRecordGuard::try_acquire(job_id)?;
    let mut cp = Checkpoint::load(job_id)?;
    let selections: Vec<_> = cp
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, entry)| !review_decision_is_terminal(entry.review_decision.as_ref()))
        .filter_map(|(index, entry)| {
            best_safe_ambiguous_candidate(&cp.spec, &entry.outcome)
                .map(|candidate| (index, candidate))
        })
        .collect();
    let mut rows = Vec::new();
    for (index, selected) in selections {
        let source_order = u32::try_from(index + 1).unwrap_or(u32::MAX);
        ensure_ready_row_mutable_unlocked(job_id, source_order)?;
        apply_accepted_candidate(&mut cp, index, selected.clone());
        rows.push(ReviewActionSummary {
            source_order,
            label: "accepted",
            key: Some(selected.key),
            display: Some(selected.display),
            score: Some(selected.score),
        });
    }
    if !rows.is_empty() {
        save_checkpoint_and_session(&mut cp)?;
    }
    let plan = ready_plan(&cp);
    Ok(ReviewBatchSummary {
        session_id: job_id.to_owned(),
        accepted_count: rows.len() as u32,
        ready_count: plan.ready_count,
        total_count: plan.total_count,
        review_left: plan.review_left,
        missing_left: plan.missing_left,
        rows,
    })
}

pub fn plan_ready_candidates(job_id: &str) -> anyhow::Result<ReviewReadyPlan> {
    let cp = Checkpoint::load(job_id)?;
    Ok(ready_plan(&cp))
}

fn ready_plan(cp: &Checkpoint) -> ReviewReadyPlan {
    let mut ready_count = 0_u32;
    let mut candidate_count = 0_u32;
    let mut ambiguous_count = 0_u32;
    let mut missing_left = 0_u32;
    for entry in &cp.tracks {
        if review_decision_is_terminal(entry.review_decision.as_ref()) {
            continue;
        }
        match &entry.outcome {
            Some(MatchOutcome::Matched { .. }) => {
                ready_count = ready_count.saturating_add(1);
            }
            Some(MatchOutcome::Ambiguous { .. }) => {
                ambiguous_count = ambiguous_count.saturating_add(1);
                if best_safe_ambiguous_candidate(&cp.spec, &entry.outcome).is_some() {
                    candidate_count = candidate_count.saturating_add(1);
                }
            }
            Some(MatchOutcome::NotFound) => {
                missing_left = missing_left.saturating_add(1);
            }
            _ => {}
        }
    }
    ReviewReadyPlan {
        session_id: cp.job_id.clone(),
        candidate_count,
        ready_count,
        resulting_ready_count: ready_count.saturating_add(candidate_count),
        total_count: cp.tracks.len() as u32,
        review_left: ambiguous_count.saturating_sub(candidate_count),
        missing_left,
    }
}

fn accept_candidate(
    cp: &mut Checkpoint,
    index: usize,
    source_order: u32,
    selected: SelectedCandidate,
    label: &'static str,
) -> anyhow::Result<ReviewActionSummary> {
    ensure_review_candidate_allowed(&cp.spec, selected.score_breakdown.as_ref())?;
    apply_accepted_candidate(cp, index, selected.clone());
    save_checkpoint_and_session(cp)?;
    Ok(ReviewActionSummary {
        source_order,
        label,
        key: Some(selected.key),
        display: Some(selected.display),
        score: Some(selected.score),
    })
}

pub(crate) fn ensure_review_candidate_allowed(
    spec: &super::JobSpec,
    score: Option<&MatchScoreBreakdown>,
) -> anyhow::Result<()> {
    if spec.media_kind == super::ImportMediaKind::MusicVideo {
        let score = score.ok_or_else(|| {
            anyhow!("candidate has no music-video eligibility evidence; rematch it first")
        })?;
        if let Some(reason) = score.reject_reason.as_deref() {
            bail!("candidate is not eligible for music-video import: {reason}");
        }
    }
    Ok(())
}

fn apply_accepted_candidate(cp: &mut Checkpoint, index: usize, selected: SelectedCandidate) {
    cp.tracks[index].outcome = Some(MatchOutcome::Matched {
        key: selected.key.clone(),
        score: selected.score,
        display: selected.display.clone(),
        title: None,
        artist: None,
        album: None,
        duration_secs: None,
        score_breakdown: selected.score_breakdown.map(Box::new),
    });
    cp.tracks[index].review_decision = Some(ReviewDecision::Accepted {
        key: selected.key,
        score: selected.score,
        display: selected.display,
    });
}

fn apply_terminal_decision(
    job_id: &str,
    source_order: u32,
    decision: ReviewDecision,
    label: &'static str,
) -> anyhow::Result<ReviewActionSummary> {
    let _guard = ImportRecordGuard::try_acquire(job_id)?;
    let mut cp = Checkpoint::load(job_id)?;
    let index = row_order_to_index(&cp, source_order)?;
    ensure_not_written(&cp, index)?;
    ensure_review_row_mutable_unlocked(job_id, source_order)?;
    cp.tracks[index].review_decision = Some(decision);
    save_checkpoint_and_session(&mut cp)?;
    Ok(ReviewActionSummary {
        source_order,
        label,
        key: None,
        display: None,
        score: None,
    })
}

fn save_checkpoint_and_session(cp: &mut Checkpoint) -> anyhow::Result<()> {
    cp.save().context("save checkpoint")?;
    ImportSession::save_checkpoint_projection_unlocked(cp).context("save import session")?;
    Ok(())
}

fn row_order_to_index(cp: &Checkpoint, source_order: u32) -> anyhow::Result<usize> {
    let index = usize::try_from(source_order)
        .ok()
        .and_then(|order| order.checked_sub(1))
        .ok_or_else(|| anyhow!("row {source_order} is out of range"))?;
    if index >= cp.tracks.len() {
        bail!("row {source_order} is out of range");
    }
    Ok(index)
}

fn ensure_not_written(cp: &Checkpoint, index: usize) -> anyhow::Result<()> {
    let entry = cp
        .tracks
        .get(index)
        .ok_or_else(|| anyhow!("row {} is out of range", index + 1))?;
    if entry.written {
        bail!(
            "row {} is already written; review cannot change it",
            index + 1
        );
    }
    Ok(())
}

fn candidates_from_outcome(outcome: &Option<MatchOutcome>) -> Vec<SelectedCandidate> {
    match outcome {
        Some(MatchOutcome::Matched {
            key,
            score,
            display,
            score_breakdown,
            ..
        }) => vec![SelectedCandidate {
            key: key.clone(),
            score: *score,
            display: display.clone(),
            score_breakdown: score_breakdown.as_deref().cloned(),
        }],
        Some(MatchOutcome::Ambiguous { candidates }) => candidates
            .iter()
            .map(|candidate| SelectedCandidate {
                key: candidate.key.clone(),
                score: candidate.score,
                display: candidate.display.clone(),
                score_breakdown: candidate.score_breakdown.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn best_safe_ambiguous_candidate(
    spec: &super::JobSpec,
    outcome: &Option<MatchOutcome>,
) -> Option<SelectedCandidate> {
    let Some(MatchOutcome::Ambiguous { candidates }) = outcome else {
        return None;
    };
    let mut best = None::<SelectedCandidate>;
    for candidate in candidates {
        if !candidate.score.is_finite()
            || candidate
                .score_breakdown
                .as_ref()
                .is_some_and(|score| score.accept_blocked || score.reject_reason.is_some())
            || ensure_review_candidate_allowed(spec, candidate.score_breakdown.as_ref()).is_err()
        {
            continue;
        }
        if best
            .as_ref()
            .is_some_and(|selected| candidate.score <= selected.score)
        {
            continue;
        }
        best = Some(SelectedCandidate {
            key: candidate.key.clone(),
            score: candidate.score,
            display: candidate.display.clone(),
            score_breakdown: candidate.score_breakdown.clone(),
        });
    }
    best
}

fn review_decision_is_terminal(decision: Option<&ReviewDecision>) -> bool {
    matches!(
        decision,
        Some(ReviewDecision::Rejected | ReviewDecision::Skipped)
    )
}

fn selected_key(entry: &super::checkpoint::TrackEntry) -> Option<&str> {
    match &entry.review_decision {
        Some(ReviewDecision::Accepted { key, .. }) => Some(key.as_str()),
        _ => match &entry.outcome {
            Some(MatchOutcome::Matched { key, .. }) => Some(key.as_str()),
            Some(MatchOutcome::Ambiguous { candidates }) => {
                candidates.first().map(|c| c.key.as_str())
            }
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::artifact_identity::{ArtifactFileIdentity, ArtifactReceipt};
    use crate::transfer::checkpoint::TrackEntry;
    use crate::transfer::matching::{AmbiguousCandidate, TrackInput};
    use crate::transfer::{
        ImportMediaKind, JobSpec, MatchPolicy, TransferCacheMode, TransferDest, TransferSource,
    };

    fn spec(media_kind: ImportMediaKind) -> JobSpec {
        JobSpec {
            source: TransferSource::SpotifyLiked,
            dest: TransferDest::LocalPlaylist { name: None },
            media_kind,
            dry_run: true,
            min_score: 0.8,
            take_best: false,
            auto_accept_ambiguous_min_score: None,
            match_policy: MatchPolicy::Strict,
            allow_user_videos: false,
            cache_mode: TransferCacheMode::Use,
            rematch: false,
        }
    }

    #[test]
    fn music_video_review_requires_eligibility_evidence() {
        let mv = spec(ImportMediaKind::MusicVideo);
        assert!(ensure_review_candidate_allowed(&mv, None).is_err());

        let rejected = MatchScoreBreakdown {
            reject_reason: Some("music_video_type_ugc".to_owned()),
            ..MatchScoreBreakdown::default()
        };
        assert!(ensure_review_candidate_allowed(&mv, Some(&rejected)).is_err());

        let review_only = MatchScoreBreakdown {
            accept_blocked: true,
            ..MatchScoreBreakdown::default()
        };
        assert!(ensure_review_candidate_allowed(&mv, Some(&review_only)).is_ok());
        assert!(
            ensure_review_candidate_allowed(&spec(ImportMediaKind::Track), None).is_ok(),
            "legacy track reviews do not require the new MV evidence field"
        );
    }

    fn input(title: &str) -> TrackInput {
        TrackInput {
            title: title.to_owned(),
            artists: vec!["Artist".to_owned()],
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
            source_url: None,
            source_key: format!("spotify:track:{title}"),
            known_video_id: None,
        }
    }

    fn ambiguous_candidate(key: &str, score: f32, accept_blocked: bool) -> AmbiguousCandidate {
        AmbiguousCandidate {
            key: key.to_owned(),
            score,
            display: key.to_owned(),
            score_breakdown: Some(MatchScoreBreakdown {
                total: score,
                accept_blocked,
                ..MatchScoreBreakdown::default()
            }),
        }
    }

    #[test]
    fn bulk_ready_uses_checkpoint_state_and_picks_highest_safe_candidate() {
        let job_id = "sp2yt-review-action-ready-all";
        let mut cp = Checkpoint::new(
            job_id.to_owned(),
            spec(ImportMediaKind::Track),
            vec![
                TrackEntry {
                    input: input("Already Ready"),
                    outcome: Some(MatchOutcome::Matched {
                        key: "ready".to_owned(),
                        score: 0.95,
                        display: "Ready".to_owned(),
                        title: None,
                        artist: None,
                        album: None,
                        duration_secs: None,
                        score_breakdown: None,
                    }),
                    review_decision: None,
                    written: true,
                },
                TrackEntry {
                    input: input("Choose Safe"),
                    outcome: Some(MatchOutcome::Ambiguous {
                        candidates: vec![
                            ambiguous_candidate("blocked-first", 0.99, true),
                            ambiguous_candidate("safe-low", 0.75, false),
                            ambiguous_candidate("safe-high", 0.88, false),
                        ],
                    }),
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: input("Missing"),
                    outcome: Some(MatchOutcome::NotFound),
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: input("Pending"),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: input("Rejected"),
                    outcome: Some(MatchOutcome::Ambiguous {
                        candidates: vec![ambiguous_candidate("rejected-safe", 0.91, false)],
                    }),
                    review_decision: Some(ReviewDecision::Rejected),
                    written: false,
                },
                TrackEntry {
                    input: input("Capacity"),
                    outcome: Some(MatchOutcome::SkippedCapacity),
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: input("Unsafe"),
                    outcome: Some(MatchOutcome::Ambiguous {
                        candidates: vec![ambiguous_candidate("unsafe-only", 0.97, true)],
                    }),
                    review_decision: None,
                    written: false,
                },
            ],
        );
        cp.save().unwrap();
        ImportSession::from_checkpoint(&cp).save().unwrap();

        assert_eq!(
            plan_ready_candidates(job_id).unwrap(),
            ReviewReadyPlan {
                session_id: job_id.to_owned(),
                candidate_count: 1,
                ready_count: 1,
                resulting_ready_count: 2,
                total_count: 7,
                review_left: 1,
                missing_left: 1,
            }
        );

        let summary = accept_all_candidates(job_id).unwrap();
        assert_eq!(summary.accepted_count, 1);
        assert_eq!(summary.ready_count, 2);
        assert_eq!(summary.total_count, 7);
        assert_eq!(summary.review_left, 1);
        assert_eq!(summary.missing_left, 1);
        let saved = Checkpoint::load(job_id).unwrap();
        assert!(matches!(
            saved.tracks[1].review_decision,
            Some(ReviewDecision::Accepted { ref key, .. }) if key == "safe-high"
        ));
        assert_eq!(
            saved.tracks[4].review_decision,
            Some(ReviewDecision::Rejected)
        );
        assert!(matches!(
            saved.tracks[6].outcome,
            Some(MatchOutcome::Ambiguous { .. })
        ));
        ImportSession::delete_record(job_id).unwrap();
    }

    #[test]
    fn bulk_ready_applies_music_video_eligibility_to_every_candidate() {
        let outcome = Some(MatchOutcome::Ambiguous {
            candidates: vec![
                AmbiguousCandidate {
                    key: "no-evidence".to_owned(),
                    score: 0.99,
                    display: "No evidence".to_owned(),
                    score_breakdown: None,
                },
                ambiguous_candidate("eligible", 0.80, false),
            ],
        });

        let selected = best_safe_ambiguous_candidate(&spec(ImportMediaKind::MusicVideo), &outcome)
            .expect("eligible candidate");
        assert_eq!(selected.key, "eligible");
    }

    #[test]
    fn bulk_ready_projects_legacy_playlist_write_and_keeps_it_downloadable() {
        let job_id = "sp2yt-review-action-ready-legacy-written";
        let mut cp = Checkpoint::new(
            job_id.to_owned(),
            spec(ImportMediaKind::Track),
            vec![TrackEntry {
                input: input("Legacy playlist row"),
                outcome: Some(MatchOutcome::Ambiguous {
                    candidates: vec![ambiguous_candidate("safe-download", 0.91, false)],
                }),
                review_decision: None,
                written: true,
            }],
        );
        cp.save().unwrap();
        ImportSession::from_checkpoint(&cp).save().unwrap();

        let summary = accept_all_candidates(job_id).unwrap();
        assert_eq!(summary.ready_count, 1);
        let saved_cp = Checkpoint::load(job_id).unwrap();
        assert!(
            saved_cp.tracks[0].written,
            "playlist idempotency is retained"
        );
        assert!(matches!(
            saved_cp.tracks[0].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "safe-download"
        ));

        let session = ImportSession::load(job_id).unwrap();
        assert!(matches!(
            session.rows[0].status,
            super::super::session::ImportSessionRowStatus::Matched
        ));
        assert_eq!(session.rows[0].revision, 2);
        assert_eq!(
            session.rows[0].selected_key.as_deref(),
            Some("safe-download")
        );
        let plan = super::super::download_plan::build_import_download_plan(
            &session,
            &super::super::download_plan::ImportDownloadDedupeIndex::default(),
        );
        assert!(matches!(
            plan.rows[0].decision,
            super::super::download_plan::ImportDownloadDecision::Enqueue
        ));

        let claim = super::super::session::claim_import_download(job_id, 1, "safe-download")
            .expect("playlist-only write can enter the artifact lifecycle");
        assert!(!ImportSession::load(job_id).unwrap().rows[0].written);
        assert!(
            super::super::session::record_import_download_error(&claim, "test cleanup").unwrap()
        );
        ImportSession::delete_record(job_id).unwrap();
    }

    #[test]
    fn bulk_ready_refuses_to_retarget_an_owned_local_artifact() {
        let job_id = "sp2yt-review-action-ready-owned-artifact";
        let mut cp = Checkpoint::new(
            job_id.to_owned(),
            spec(ImportMediaKind::Track),
            vec![TrackEntry {
                input: input("Owned artifact"),
                outcome: Some(MatchOutcome::Ambiguous {
                    candidates: vec![ambiguous_candidate("replacement", 0.93, false)],
                }),
                review_decision: None,
                written: false,
            }],
        );
        cp.save().unwrap();
        let mut session = ImportSession::from_checkpoint(&cp);
        session.rows[0].written = true;
        session.rows[0].local_path = Some(std::path::PathBuf::from("/tmp/owned-artifact.m4a"));
        session.save().unwrap();

        let error = accept_all_candidates(job_id).unwrap_err();
        assert!(format!("{error:#}").contains("already owns a local artifact"));
        assert!(matches!(
            Checkpoint::load(job_id).unwrap().tracks[0].outcome,
            Some(MatchOutcome::Ambiguous { .. })
        ));
        ImportSession::delete_record(job_id).unwrap();
    }

    #[test]
    fn tui_review_projection_preserves_another_rows_artifact_state() {
        let job_id = "sp2yt-review-action-artifact-preserve";
        let mut cp = Checkpoint::new(
            job_id.to_owned(),
            spec(ImportMediaKind::Track),
            vec![
                TrackEntry {
                    input: input("Review"),
                    outcome: Some(MatchOutcome::Ambiguous {
                        candidates: vec![AmbiguousCandidate {
                            key: "video-review".to_owned(),
                            score: 0.9,
                            display: "Review".to_owned(),
                            score_breakdown: None,
                        }],
                    }),
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: input("Written"),
                    outcome: Some(MatchOutcome::Matched {
                        key: "video-written".to_owned(),
                        score: 0.95,
                        display: "Written".to_owned(),
                        title: None,
                        artist: None,
                        album: None,
                        duration_secs: None,
                        score_breakdown: None,
                    }),
                    review_decision: None,
                    written: false,
                },
            ],
        );
        cp.save().unwrap();
        let mut session = ImportSession::from_checkpoint(&cp);
        let artifact = std::path::PathBuf::from("/tmp/review-action-written.m4a");
        session.rows[1].written = true;
        session.rows[1].local_path = Some(artifact.clone());
        session.rows[1].artifact_receipt = Some(ArtifactReceipt {
            audio: ArtifactFileIdentity {
                len: 5,
                sha256: "b".repeat(64),
            },
            sidecar_required: false,
            sidecar: None,
            claim: None,
        });
        session.save().unwrap();

        accept_first_candidate(job_id, 1).unwrap();

        let saved = ImportSession::load(job_id).unwrap();
        assert_eq!(saved.rows[1].local_path, Some(artifact));
        assert!(saved.rows[1].written);
        assert!(saved.rows[1].artifact_receipt.is_some());
        ImportSession::delete_record(job_id).unwrap();
    }
}
