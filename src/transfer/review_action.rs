//! Shared import-review mutations for CLI and TUI surfaces.

use anyhow::{Context as _, anyhow, bail};

use super::checkpoint::{Checkpoint, ReviewDecision};
use super::matching::{MatchOutcome, MatchScoreBreakdown};
use super::session::ImportSession;

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
    pub rows: Vec<ReviewActionSummary>,
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
    let mut cp = Checkpoint::load(job_id)?;
    let index = row_order_to_index(&cp, source_order)?;
    ensure_not_written(&cp, index)?;
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
    let mut cp = Checkpoint::load(job_id)?;
    let index = row_order_to_index(&cp, source_order)?;
    ensure_not_written(&cp, index)?;
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
    let mut cp = Checkpoint::load(job_id)?;
    let mut rows = Vec::new();
    for index in 0..cp.tracks.len() {
        if cp.tracks[index].written {
            continue;
        }
        let source_order = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let Some(selected) = first_ambiguous_candidate(&cp.tracks[index].outcome) else {
            continue;
        };
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
    Ok(ReviewBatchSummary {
        session_id: job_id.to_owned(),
        accepted_count: rows.len() as u32,
        rows,
    })
}

fn accept_candidate(
    cp: &mut Checkpoint,
    index: usize,
    source_order: u32,
    selected: SelectedCandidate,
    label: &'static str,
) -> anyhow::Result<ReviewActionSummary> {
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

fn apply_accepted_candidate(cp: &mut Checkpoint, index: usize, selected: SelectedCandidate) {
    cp.tracks[index].outcome = Some(MatchOutcome::Matched {
        key: selected.key.clone(),
        score: selected.score,
        display: selected.display.clone(),
        title: None,
        artist: None,
        album: None,
        duration_secs: None,
        score_breakdown: selected.score_breakdown,
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
    let mut cp = Checkpoint::load(job_id)?;
    let index = row_order_to_index(&cp, source_order)?;
    ensure_not_written(&cp, index)?;
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
    ImportSession::from_checkpoint(cp)
        .save()
        .context("save import session")?;
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
            score_breakdown: score_breakdown.clone(),
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

fn first_ambiguous_candidate(outcome: &Option<MatchOutcome>) -> Option<SelectedCandidate> {
    let Some(MatchOutcome::Ambiguous { candidates }) = outcome else {
        return None;
    };
    candidates.first().map(|candidate| SelectedCandidate {
        key: candidate.key.clone(),
        score: candidate.score,
        display: candidate.display.clone(),
        score_breakdown: candidate.score_breakdown.clone(),
    })
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
