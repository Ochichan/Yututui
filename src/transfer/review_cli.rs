//! CLI review actions for persisted import sessions.

use anyhow::{anyhow, bail};

use super::checkpoint::{Checkpoint, ReviewDecision};
use super::cli::{EXIT_FAILED, EXIT_OK, EXIT_USAGE};
use super::matching::{MatchOutcome, MatchScoreBreakdown};
use super::session::{ImportSession, ImportSessionRow, ImportSessionRowStatus};

const USAGE: &str = "\
Usage:
  ytt transfer review <JOB-ID> [--all|--review|--accepted|--rejected|--skipped|--undecided]
  ytt transfer review <JOB-ID> accept <ROW> [CANDIDATE]
  ytt transfer review <JOB-ID> choose <ROW> <CANDIDATE>
  ytt transfer review <JOB-ID> reject <ROW>
  ytt transfer review <JOB-ID> skip <ROW>

ROW is a source-order number like `12` or a row id like `row-00012`.
CANDIDATE is a 1-based candidate number or a candidate key.";

pub fn run(args: &[&str]) -> i32 {
    match run_inner(args) {
        Ok(message) => {
            print!("{message}");
            EXIT_OK
        }
        Err(ReviewError::Usage(message)) => {
            eprintln!("ytt transfer review: {message}");
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
        Err(ReviewError::Failed(error)) => {
            eprintln!("ytt transfer review: {error:#}");
            EXIT_FAILED
        }
    }
}

fn run_inner(args: &[&str]) -> Result<String, ReviewError> {
    let Some(job_id) = args.first().copied() else {
        return Err(ReviewError::Usage("missing <JOB-ID>".to_owned()));
    };
    if matches!(
        args.get(1).copied(),
        None | Some(
            "--all" | "--review" | "--accepted" | "--rejected" | "--skipped" | "--undecided"
        )
    ) {
        let filter = args
            .get(1)
            .copied()
            .map(ReviewFilter::parse)
            .transpose()?
            .unwrap_or(ReviewFilter::Review);
        let session = ImportSession::load(job_id).map_err(ReviewError::Failed)?;
        return Ok(format_review(&session, filter));
    }

    let action = args[1];
    let Some(row_ref) = args.get(2).copied() else {
        return Err(ReviewError::Usage(format!("{action} needs <ROW>")));
    };
    let candidate_ref = args.get(3).copied();
    if args.len() > 4 {
        return Err(ReviewError::Usage("too many review arguments".to_owned()));
    }

    let mut cp = Checkpoint::load(job_id).map_err(ReviewError::Failed)?;
    let index = resolve_row_index(&cp, row_ref).map_err(ReviewError::Usage)?;
    let message = match action {
        "accept" => {
            let selected = selected_candidate(&cp.tracks[index].outcome, candidate_ref)
                .map_err(ReviewError::Usage)?;
            apply_accept(&mut cp, index, selected).map_err(ReviewError::Failed)?
        }
        "choose" => {
            let Some(candidate_ref) = candidate_ref else {
                return Err(ReviewError::Usage("choose needs <CANDIDATE>".to_owned()));
            };
            let selected = selected_candidate(&cp.tracks[index].outcome, Some(candidate_ref))
                .map_err(ReviewError::Usage)?;
            apply_accept(&mut cp, index, selected).map_err(ReviewError::Failed)?
        }
        "reject" => {
            if candidate_ref.is_some() {
                return Err(ReviewError::Usage(
                    "reject does not take <CANDIDATE>".to_owned(),
                ));
            }
            apply_terminal_decision(&mut cp, index, ReviewDecision::Rejected)
                .map_err(ReviewError::Failed)?
        }
        "skip" => {
            if candidate_ref.is_some() {
                return Err(ReviewError::Usage(
                    "skip does not take <CANDIDATE>".to_owned(),
                ));
            }
            apply_terminal_decision(&mut cp, index, ReviewDecision::Skipped)
                .map_err(ReviewError::Failed)?
        }
        _ => return Err(ReviewError::Usage(format!("unknown action `{action}`"))),
    };
    cp.save().map_err(|e| ReviewError::Failed(e.into()))?;
    ImportSession::from_checkpoint(&cp)
        .save()
        .map_err(|e| ReviewError::Failed(e.into()))?;
    Ok(format!("{message}\n"))
}

#[derive(Debug)]
enum ReviewError {
    Usage(String),
    Failed(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewFilter {
    Review,
    All,
    Accepted,
    Rejected,
    Skipped,
    Undecided,
}

impl ReviewFilter {
    fn parse(raw: &str) -> Result<Self, ReviewError> {
        match raw {
            "--review" => Ok(Self::Review),
            "--all" => Ok(Self::All),
            "--accepted" => Ok(Self::Accepted),
            "--rejected" => Ok(Self::Rejected),
            "--skipped" => Ok(Self::Skipped),
            "--undecided" => Ok(Self::Undecided),
            other => Err(ReviewError::Usage(format!("unknown filter `{other}`"))),
        }
    }

    fn matches(self, row: &ImportSessionRow) -> bool {
        match self {
            Self::All => true,
            Self::Review => {
                row.review_decision.is_some()
                    || !matches!(row.status, ImportSessionRowStatus::Matched)
            }
            Self::Accepted => matches!(row.review_decision, Some(ReviewDecision::Accepted { .. })),
            Self::Rejected => matches!(row.review_decision, Some(ReviewDecision::Rejected)),
            Self::Skipped => matches!(row.review_decision, Some(ReviewDecision::Skipped)),
            Self::Undecided => {
                row.review_decision.is_none()
                    && !matches!(row.status, ImportSessionRowStatus::Matched)
            }
        }
    }
}

fn format_review(session: &ImportSession, filter: ReviewFilter) -> String {
    let accepted = session
        .rows
        .iter()
        .filter(|row| matches!(row.review_decision, Some(ReviewDecision::Accepted { .. })))
        .count();
    let rejected = session
        .rows
        .iter()
        .filter(|row| matches!(row.review_decision, Some(ReviewDecision::Rejected)))
        .count();
    let skipped = session
        .rows
        .iter()
        .filter(|row| matches!(row.review_decision, Some(ReviewDecision::Skipped)))
        .count();
    let undecided = session
        .rows
        .iter()
        .filter(|row| {
            row.review_decision.is_none() && !matches!(row.status, ImportSessionRowStatus::Matched)
        })
        .count();

    let mut out = format!(
        "Review: {} ({accepted} accepted, {rejected} rejected, {skipped} skipped, {undecided} undecided)\n",
        session.session_id
    );
    let mut printed = 0usize;
    for row in session.rows.iter().filter(|row| filter.matches(row)) {
        printed += 1;
        out.push_str(&format!(
            "  {:>4}. {:<11} {:<9} {} - {}\n",
            row.source_order,
            row_status_label(row.status),
            row_decision_label(row.review_decision.as_ref()),
            row.artists.join(", "),
            row.title
        ));
        if let Some(album) = &row.album {
            out.push_str(&format!("        album: {album}\n"));
        }
        append_row_metadata(&mut out, row);
        for (idx, candidate) in row.candidates.iter().enumerate() {
            out.push_str(&format_candidate(idx + 1, candidate));
        }
    }
    if printed == 0 {
        out.push_str("No rows for this review filter.\n");
    }
    out
}

fn append_row_metadata(out: &mut String, row: &ImportSessionRow) {
    if !row.album_artists.is_empty() {
        out.push_str(&format!(
            "        album artist: {}\n",
            row.album_artists.join(", ")
        ));
    }
    if let Some(track) = format_track_position(row) {
        out.push_str(&format!("        track: {track}\n"));
    }
    if let Some(duration) = row.duration_secs {
        out.push_str(&format!(
            "        duration: {}\n",
            crate::util::format::time(f64::from(duration))
        ));
    }
    if let Some(isrc) = &row.isrc {
        out.push_str(&format!("        isrc: {isrc}\n"));
    }
    if let Some(explicit) = row.explicit {
        out.push_str(&format!(
            "        explicit: {}\n",
            if explicit { "yes" } else { "no" }
        ));
    }
    if !row.source_key.is_empty() {
        out.push_str(&format!("        source: {}\n", row.source_key));
    }
    if let Some(url) = row
        .source_url
        .as_deref()
        .filter(|url| *url != row.source_key)
    {
        out.push_str(&format!("        source url: {url}\n"));
    }
    if let Some(kind) = &row.source_kind {
        out.push_str(&format!("        source kind: {kind}\n"));
    }
    if let Some(tier) = &row.quality_tier {
        out.push_str(&format!("        quality: {tier}\n"));
    }
    if let Some(delta) = row.duration_delta_secs {
        out.push_str(&format!("        duration delta: {delta:+}s\n"));
    }
    if let Some(reason) = &row.reject_reason {
        out.push_str(&format!("        blocked: {reason}\n"));
    } else if !row.reason_codes.is_empty() {
        out.push_str(&format!(
            "        reasons: {}\n",
            row.reason_codes.join(", ")
        ));
    }
}

fn format_track_position(row: &ImportSessionRow) -> Option<String> {
    match (row.disc_number, row.track_number) {
        (Some(disc), Some(track)) => Some(format!("{disc}-{track:02}")),
        (Some(disc), None) => Some(format!("disc {disc}")),
        (None, Some(track)) => Some(track.to_string()),
        (None, None) => None,
    }
}

fn format_candidate(index: usize, candidate: &super::checkpoint::ReportCandidate) -> String {
    let breakdown = candidate
        .score_breakdown
        .as_ref()
        .map(|breakdown| {
            format!(
                " [title {:.2}, artist {:.2}, duration {:.2}, album +{:.2}",
                breakdown.title, breakdown.artist, breakdown.duration, breakdown.album_bonus
            ) + &format_quality_suffix(breakdown)
        })
        .unwrap_or_default();
    format!(
        "        candidate {index}: {:.2} {} ({}){breakdown}\n",
        candidate.score, candidate.display, candidate.key
    )
}

fn format_quality_suffix(breakdown: &MatchScoreBreakdown) -> String {
    let mut parts = Vec::new();
    if !breakdown.quality_tier.is_empty() {
        parts.push(format!("quality {}", breakdown.quality_tier));
    }
    if let Some(delta) = breakdown.duration_delta_secs {
        parts.push(format!("delta {delta:+}s"));
    }
    if let Some(reason) = &breakdown.reject_reason {
        parts.push(format!("blocked {reason}"));
    } else if breakdown.accept_blocked && !breakdown.reason_codes.is_empty() {
        parts.push(format!("blocked {}", breakdown.reason_codes.join(",")));
    }
    if parts.is_empty() {
        "]".to_owned()
    } else {
        format!(", {}]", parts.join(", "))
    }
}

fn row_status_label(status: ImportSessionRowStatus) -> &'static str {
    match status {
        ImportSessionRowStatus::Pending => "pending",
        ImportSessionRowStatus::Matched => "matched",
        ImportSessionRowStatus::Ambiguous => "review",
        ImportSessionRowStatus::NotFound => "not_found",
        ImportSessionRowStatus::SkippedLocal => "skipped",
    }
}

fn row_decision_label(decision: Option<&ReviewDecision>) -> &'static str {
    match decision {
        Some(ReviewDecision::Accepted { .. }) => "accepted",
        Some(ReviewDecision::Rejected) => "rejected",
        Some(ReviewDecision::Skipped) => "skipped",
        None => "undecided",
    }
}

#[derive(Debug, Clone)]
struct SelectedCandidate {
    key: String,
    score: f32,
    display: String,
    score_breakdown: Option<MatchScoreBreakdown>,
}

fn selected_candidate(
    outcome: &Option<MatchOutcome>,
    selector: Option<&str>,
) -> Result<SelectedCandidate, String> {
    let candidates = candidates_from_outcome(outcome);
    if candidates.is_empty() {
        return Err("row has no candidate to accept".to_owned());
    }
    let selected = match selector {
        None => candidates.first(),
        Some(raw) => match raw.parse::<usize>() {
            Ok(index) if index > 0 => candidates.get(index - 1),
            Ok(_) => None,
            Err(_) => candidates.iter().find(|candidate| candidate.key == raw),
        },
    };
    selected.cloned().ok_or_else(|| {
        let label = selector.unwrap_or("first candidate");
        format!("candidate `{label}` was not found on that row")
    })
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

fn resolve_row_index(cp: &Checkpoint, row_ref: &str) -> Result<usize, String> {
    if let Some(row_no) = row_ref.strip_prefix("row-") {
        let order: usize = row_no
            .parse()
            .map_err(|_| format!("bad row id `{row_ref}`"))?;
        return row_order_to_index(cp, order, row_ref);
    }
    let order: usize = row_ref
        .parse()
        .map_err(|_| format!("bad row `{row_ref}`"))?;
    row_order_to_index(cp, order, row_ref)
}

fn row_order_to_index(cp: &Checkpoint, order: usize, raw: &str) -> Result<usize, String> {
    let index = order
        .checked_sub(1)
        .ok_or_else(|| format!("row `{raw}` is out of range"))?;
    if index >= cp.tracks.len() {
        return Err(format!("row `{raw}` is out of range"));
    }
    Ok(index)
}

fn apply_accept(
    cp: &mut Checkpoint,
    index: usize,
    selected: SelectedCandidate,
) -> anyhow::Result<String> {
    ensure_not_written(cp, index)?;
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
        key: selected.key.clone(),
        score: selected.score,
        display: selected.display.clone(),
    });
    Ok(format!(
        "Accepted row {}: {} ({:.2})",
        index + 1,
        selected.display,
        selected.score
    ))
}

fn apply_terminal_decision(
    cp: &mut Checkpoint,
    index: usize,
    decision: ReviewDecision,
) -> anyhow::Result<String> {
    ensure_not_written(cp, index)?;
    cp.tracks[index].review_decision = Some(decision.clone());
    Ok(format!(
        "{} row {}",
        decision.label().to_ascii_uppercase(),
        index + 1
    ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::checkpoint::TrackEntry;
    use crate::transfer::matching::{AmbiguousCandidate, TrackInput};
    use crate::transfer::{JobSpec, Stage, TransferDest, TransferSource};

    fn spec() -> JobSpec {
        JobSpec {
            source: TransferSource::SpotifyPlaylist {
                id: "spotify-playlist".to_owned(),
            },
            dest: TransferDest::LocalPlaylist {
                name: Some("Imported".to_owned()),
            },
            dry_run: true,
            min_score: 0.80,
            take_best: false,
            rematch: false,
        }
    }

    fn input(title: &str) -> TrackInput {
        TrackInput {
            title: title.to_owned(),
            artists: vec!["Artist".to_owned()],
            album_artists: vec!["Album Artist".to_owned()],
            album: Some("Album".to_owned()),
            album_id: None,
            album_uri: None,
            album_release_date: None,
            album_release_date_precision: None,
            album_total_tracks: None,
            album_type: None,
            album_art_url: None,
            disc_number: Some(1),
            track_number: Some(2),
            duration_secs: Some(180),
            isrc: Some("USRC17607839".to_owned()),
            explicit: Some(false),
            source_url: Some(format!("https://open.spotify.com/track/{title}")),
            source_key: format!("spotify:track:{title}"),
            known_video_id: None,
        }
    }

    fn ambiguous_entry(title: &str) -> TrackEntry {
        TrackEntry {
            input: input(title),
            outcome: Some(MatchOutcome::Ambiguous {
                candidates: vec![
                    AmbiguousCandidate {
                        key: "vid-first".to_owned(),
                        score: 0.78,
                        display: "Artist - First".to_owned(),
                        score_breakdown: Some(MatchScoreBreakdown {
                            total: 0.78,
                            raw_total: 0.78,
                            title: 0.90,
                            artist: 1.0,
                            duration: 0.8,
                            album_bonus: 0.05,
                            quality_bonus: 0.0,
                            identity_penalty: 0.0,
                            non_music_penalty: 0.0,
                            accept_blocked: false,
                            reject_reason: None,
                            reason_codes: Vec::new(),
                            ..MatchScoreBreakdown::default()
                        }),
                    },
                    AmbiguousCandidate {
                        key: "vid-second".to_owned(),
                        score: 0.73,
                        display: "Artist - Second".to_owned(),
                        score_breakdown: None,
                    },
                ],
            }),
            review_decision: None,
            written: false,
        }
    }

    fn save_job(job_id: &str, entry: TrackEntry) {
        let mut cp = Checkpoint::new(job_id.to_owned(), spec(), vec![entry]);
        cp.stage = Stage::Writing;
        cp.save().expect("save checkpoint");
        ImportSession::from_checkpoint(&cp)
            .save()
            .expect("save session");
    }

    #[test]
    fn review_output_includes_source_metadata_and_score_breakdown() {
        let job_id = "sp2yt-review-metadata";
        save_job(job_id, ambiguous_entry("Maybe"));

        let output = run_inner(&[job_id, "--review"]).expect("review output");

        for expected in [
            "album: Album",
            "album artist: Album Artist",
            "track: 1-02",
            "duration: 3:00",
            "isrc: USRC17607839",
            "explicit: no",
            "source: spotify:track:Maybe",
            "source url: https://open.spotify.com/track/Maybe",
            "candidate 1: 0.78 Artist - First (vid-first) [title 0.90, artist 1.00, duration 0.80, album +0.05]",
        ] {
            assert!(
                output.contains(expected),
                "missing {expected:?} in {output}"
            );
        }
    }

    #[test]
    fn choose_candidate_updates_checkpoint_and_session() {
        let job_id = "sp2yt-review-choose";
        save_job(job_id, ambiguous_entry("Maybe"));

        let output = run_inner(&[job_id, "choose", "1", "2"]).expect("review choose");

        assert!(output.contains("Accepted row 1"));
        let cp = Checkpoint::load(job_id).expect("load checkpoint");
        assert!(matches!(
            cp.tracks[0].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "vid-second"
        ));
        assert!(matches!(
            cp.tracks[0].review_decision,
            Some(ReviewDecision::Accepted { ref key, .. }) if key == "vid-second"
        ));
        let session = ImportSession::load(job_id).expect("load session");
        assert!(matches!(
            session.rows[0].review_decision,
            Some(ReviewDecision::Accepted { ref key, .. }) if key == "vid-second"
        ));
    }

    #[test]
    fn reject_preserves_candidates_but_marks_decision() {
        let job_id = "sp2yt-review-reject";
        save_job(job_id, ambiguous_entry("Maybe"));

        let output = run_inner(&[job_id, "reject", "row-00001"]).expect("review reject");

        assert!(output.contains("REJECTED row 1"));
        let cp = Checkpoint::load(job_id).expect("load checkpoint");
        assert!(matches!(
            cp.tracks[0].outcome,
            Some(MatchOutcome::Ambiguous { .. })
        ));
        assert_eq!(cp.tracks[0].review_decision, Some(ReviewDecision::Rejected));
        let session = ImportSession::load(job_id).expect("load session");
        assert_eq!(
            session.rows[0].review_decision,
            Some(ReviewDecision::Rejected)
        );
    }

    #[test]
    fn filters_report_review_decision_counts() {
        let job_id = "sp2yt-review-filter";
        save_job(job_id, ambiguous_entry("Maybe"));
        run_inner(&[job_id, "accept", "1", "vid-first"]).expect("review accept");

        let accepted = run_inner(&[job_id, "--accepted"]).expect("accepted filter");
        let undecided = run_inner(&[job_id, "--undecided"]).expect("undecided filter");

        assert!(accepted.contains("1 accepted"));
        assert!(accepted.contains("Artist - First"));
        assert!(undecided.contains("No rows for this review filter."));
    }
}
