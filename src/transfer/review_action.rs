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
    let selected = first_candidate(&cp.tracks[index].outcome)
        .ok_or_else(|| anyhow!("row {source_order} has no candidate to accept"))?;
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
        key: selected.key.clone(),
        score: selected.score,
        display: selected.display.clone(),
    });
    save_checkpoint_and_session(&mut cp)?;
    Ok(ReviewActionSummary {
        source_order,
        label: "accepted",
        key: Some(selected.key),
        display: Some(selected.display),
        score: Some(selected.score),
    })
}

pub fn reject_row(job_id: &str, source_order: u32) -> anyhow::Result<ReviewActionSummary> {
    apply_terminal_decision(job_id, source_order, ReviewDecision::Rejected, "rejected")
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

fn first_candidate(outcome: &Option<MatchOutcome>) -> Option<SelectedCandidate> {
    match outcome {
        Some(MatchOutcome::Matched {
            key,
            score,
            display,
            score_breakdown,
            ..
        }) => Some(SelectedCandidate {
            key: key.clone(),
            score: *score,
            display: display.clone(),
            score_breakdown: *score_breakdown,
        }),
        Some(MatchOutcome::Ambiguous { candidates }) => {
            candidates.first().map(|candidate| SelectedCandidate {
                key: candidate.key.clone(),
                score: candidate.score,
                display: candidate.display.clone(),
                score_breakdown: candidate.score_breakdown,
            })
        }
        _ => None,
    }
}
