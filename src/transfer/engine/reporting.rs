//! Capacity, outcome accounting, and transfer report construction.

use super::*;

pub(super) fn outcome_counts(tracks: &[TrackEntry]) -> (u32, u32, u32) {
    let mut matched = 0;
    let mut ambiguous = 0;
    let mut not_found = 0;
    for track in tracks {
        match &track.outcome {
            Some(MatchOutcome::Matched { .. }) => matched += 1,
            Some(MatchOutcome::Ambiguous { .. }) => ambiguous += 1,
            Some(MatchOutcome::NotFound) => not_found += 1,
            _ => {}
        }
    }
    (matched, ambiguous, not_found)
}

pub(super) fn local_destination_keys(cp: &Checkpoint) -> Option<HashSet<String>> {
    if !matches!(cp.spec.dest, TransferDest::LocalPlaylist { .. }) {
        return None;
    }
    let store = crate::playlists::Playlists::load();
    Some(
        find_local_destination(&store, cp)
            .map(|playlist| {
                playlist
                    .songs
                    .iter()
                    .map(|song| song.video_id.clone())
                    .collect()
            })
            .unwrap_or_default(),
    )
}

pub(super) fn find_local_destination<'a>(
    store: &'a crate::playlists::Playlists,
    cp: &Checkpoint,
) -> Option<&'a crate::playlists::Playlist> {
    cp.dest_id
        .as_deref()
        .and_then(|id| store.find(id))
        .or_else(|| store.find(cp.dest_name.as_deref().unwrap_or("Imported playlist")))
}

#[cfg(test)]
pub(super) fn extend_matched_keys(keys: &mut HashSet<String>, tracks: &[TrackEntry]) {
    keys.extend(tracks.iter().filter_map(|entry| match &entry.outcome {
        Some(MatchOutcome::Matched { key, .. }) => Some(key.clone()),
        _ => None,
    }));
}

/// Keep source order deterministic when a local destination has fewer free slots than
/// already-resolved matches. Existing destination songs and repeated matches do not consume
/// another slot; excess unique matches become an explicit capacity outcome, never "missing".
pub(super) fn enforce_matched_capacity(
    cp: &mut Checkpoint,
    keys: &mut HashSet<String>,
) -> Vec<usize> {
    let mut indexes = Vec::new();
    let take_best = cp.spec.take_best;
    for (index, entry) in cp.tracks.iter_mut().enumerate() {
        let Some(key) = effective_write_key(entry, take_best).map(str::to_owned) else {
            continue;
        };
        if entry.written || keys.contains(&key) {
            continue;
        }
        if keys.len() < crate::playlists::SONGS_PER_PLAYLIST_MAX {
            keys.insert(key);
            continue;
        }
        entry.outcome = Some(MatchOutcome::SkippedCapacity);
        entry.review_decision = None;
        indexes.push(index);
    }
    record_capacity_skips(
        &mut cp.match_stats,
        u32::try_from(indexes.len()).unwrap_or(u32::MAX),
    );
    indexes
}

pub(super) fn record_capacity_skips(stats: &mut MatchStats, count: u32) {
    stats.capacity_skipped = stats.capacity_skipped.saturating_add(count);
    if count > 0 {
        stats.bump_terminal_reason("destination_capacity");
    }
}

pub(super) fn mark_pending_capacity_skipped(cp: &mut Checkpoint) -> Vec<usize> {
    let mut indexes = Vec::new();
    for (index, entry) in cp.tracks.iter_mut().enumerate() {
        if entry.outcome.is_none() {
            entry.outcome = Some(MatchOutcome::SkippedCapacity);
            indexes.push(index);
        }
    }
    let count = u32::try_from(indexes.len()).unwrap_or(u32::MAX);
    record_capacity_skips(&mut cp.match_stats, count);
    indexes
}

pub(super) fn increment_outcome_counts(
    outcome: &MatchOutcome,
    matched: &mut u32,
    ambiguous: &mut u32,
    not_found: &mut u32,
) {
    match outcome {
        MatchOutcome::Matched { .. } => *matched = matched.saturating_add(1),
        MatchOutcome::Ambiguous { .. } => *ambiguous = ambiguous.saturating_add(1),
        MatchOutcome::NotFound => *not_found = not_found.saturating_add(1),
        MatchOutcome::SkippedLocal | MatchOutcome::SkippedCapacity => {}
    }
}

pub(super) fn record_match_trace_stats(stats: &mut MatchStats, trace: &MatchTrace) {
    for probe in &trace.probes {
        if probe.status == "empty" {
            stats.successful_empty_probes = stats.successful_empty_probes.saturating_add(1);
        }
        // Metadata preflight inspects existing finalists; it does not fetch a new
        // candidate set and must not inflate retrieval volume.
        if probe.backend != "youtube_metadata" {
            stats.candidates_fetched = stats.candidates_fetched.saturating_add(probe.result_count);
        }
        stats.add_elapsed_ms(&probe.backend, probe.elapsed_ms);
    }
    for candidate in &trace.candidates {
        let Some(score) = candidate.score_breakdown.as_ref() else {
            continue;
        };
        if score.reject_reason.is_some() {
            stats.candidates_rejected = stats.candidates_rejected.saturating_add(1);
        } else if score.accept_blocked {
            stats.candidates_review_only = stats.candidates_review_only.saturating_add(1);
        }
    }
    if let Some(reason) = trace.terminal_reason.as_deref() {
        stats.bump_terminal_reason(reason);
    }
}

pub(super) fn build_report(cp: &Checkpoint, skipped_local: u32) -> TransferReport {
    let (matched, _, _) = outcome_counts(&cp.tracks);
    let mut report = TransferReport {
        job_id: cp.job_id.clone(),
        total: cp.tracks.len() as u32,
        matched,
        skipped_local,
        source_truncated: cp.source_truncated,
        spotify_sync: cp.spotify_sync.as_ref().map(|state| state.preview.clone()),
        match_stats: cp.match_stats.clone(),
        defer_reason: cp.defer_reason.clone(),
        ..TransferReport::default()
    };
    for (idx, entry) in cp.tracks.iter().enumerate() {
        match &entry.outcome {
            Some(MatchOutcome::Ambiguous { candidates }) => {
                let report_candidates: Vec<ReportCandidate> = candidates
                    .iter()
                    .map(|candidate| ReportCandidate {
                        key: candidate.key.clone(),
                        score: candidate.score,
                        display: candidate.display.clone(),
                        score_breakdown: candidate.score_breakdown.clone(),
                    })
                    .collect();
                let note = candidates
                    .iter()
                    .map(|candidate| format!("{} ({:.2})", candidate.display, candidate.score))
                    .collect::<Vec<_>>()
                    .join(" | ");
                let mut row = report_row_base(entry, idx, note);
                row.selected_key = report_candidates
                    .first()
                    .map(|candidate| candidate.key.clone());
                row.selected_score = report_candidates.first().map(|candidate| candidate.score);
                if let Some(score) = report_candidates
                    .first()
                    .and_then(|candidate| candidate.score_breakdown.as_ref())
                {
                    apply_report_quality(&mut row, score);
                }
                row.candidates = report_candidates;
                if let Some(trace) = cp.match_traces.get(&(idx as u32 + 1)) {
                    apply_trace_to_report_row(&mut row, trace);
                }
                report.ambiguous.push(row);
            }
            Some(MatchOutcome::NotFound) => {
                let trace = cp.match_traces.get(&(idx as u32 + 1));
                let note = not_found_note(trace);
                let mut row = report_row_base(entry, idx, note);
                if let Some(trace) = trace {
                    apply_trace_to_report_row(&mut row, trace);
                }
                report.not_found.push(row);
            }
            None if cp.defer_reason.is_some() => {
                let reason = cp.defer_reason.as_ref().expect("checked above");
                let mut row = report_row_base(
                    entry,
                    idx,
                    format!("deferred before completion: {}", reason.code),
                );
                if let Some(trace) = cp.match_traces.get(&(idx as u32 + 1)) {
                    apply_trace_to_report_row(&mut row, trace);
                } else {
                    row.terminal_reason = Some(reason.code.clone());
                }
                report.deferred.push(row);
            }
            Some(MatchOutcome::SkippedCapacity) => {
                let mut row = report_row_base(
                    entry,
                    idx,
                    "destination capacity was already satisfied".to_owned(),
                );
                row.terminal_reason = Some("destination_capacity".to_owned());
                report.capacity_skipped.push(row);
            }
            _ => {}
        }
    }
    report
}

pub(super) fn rebuild_report_after_write(
    cp: &Checkpoint,
    skipped_local: u32,
    report: &mut TransferReport,
) {
    let auto_accepted = report.auto_accepted;
    let duplicates_dropped = report.duplicates_dropped;
    let elapsed_secs = report.elapsed_secs;
    let written = report.written;
    let mut rebuilt = build_report(cp, skipped_local);
    rebuilt.auto_accepted = auto_accepted;
    rebuilt.duplicates_dropped = duplicates_dropped;
    rebuilt.elapsed_secs = elapsed_secs;
    rebuilt.written = written;
    *report = rebuilt;
}

fn apply_trace_to_report_row(row: &mut ReportRow, trace: &MatchTrace) {
    row.executed_probes = trace.probes.clone();
    row.terminal_reason = trace.terminal_reason.clone();
    let mut seen = HashSet::new();
    row.search_queries = trace
        .probes
        .iter()
        .filter_map(|probe| {
            let query = probe.query.trim();
            (!query.is_empty() && seen.insert(query.to_owned())).then(|| query.to_owned())
        })
        .collect();
    if row.candidates.is_empty() {
        row.candidates = trace.candidates.clone();
        row.selected_key = row
            .candidates
            .first()
            .map(|candidate| candidate.key.clone());
        row.selected_score = row.candidates.first().map(|candidate| candidate.score);
        if let Some(score) = row
            .candidates
            .first()
            .and_then(|candidate| candidate.score_breakdown.as_ref())
            .cloned()
        {
            apply_report_quality(row, &score);
        }
    }
}

fn not_found_note(trace: Option<&MatchTrace>) -> String {
    let Some(trace) = trace else {
        return "no match on the destination".to_owned();
    };
    if trace.candidates.is_empty() {
        return "all executed providers completed without a candidate".to_owned();
    }
    if trace.candidates.iter().all(|candidate| {
        candidate
            .score_breakdown
            .as_ref()
            .is_some_and(|score| score.reject_reason.is_some())
    }) {
        return "all retrieved candidates were rejected by safety/version gates".to_owned();
    }
    "retrieved candidates stayed below the review threshold".to_owned()
}

fn report_row_base(entry: &TrackEntry, idx: usize, note: String) -> ReportRow {
    ReportRow {
        title: entry.input.title.clone(),
        artists: entry.input.artists.join(", "),
        note,
        source_order: Some(idx as u32 + 1),
        source_key: Some(entry.input.source_key.clone()),
        source_url: entry.input.source_url.clone(),
        album: entry.input.album.clone(),
        album_artists: entry.input.album_artists.clone(),
        album_id: entry.input.album_id.clone(),
        album_uri: entry.input.album_uri.clone(),
        album_release_date: entry.input.album_release_date.clone(),
        album_release_date_precision: entry.input.album_release_date_precision.clone(),
        album_total_tracks: entry.input.album_total_tracks,
        album_type: entry.input.album_type.clone(),
        album_art_url: entry.input.album_art_url.clone(),
        disc_number: entry.input.disc_number,
        track_number: entry.input.track_number,
        duration_secs: entry.input.duration_secs,
        isrc: entry.input.isrc.clone(),
        explicit: entry.input.explicit,
        candidates: Vec::new(),
        selected_key: None,
        selected_score: None,
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

fn apply_report_quality(row: &mut ReportRow, score: &super::super::matching::MatchScoreBreakdown) {
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

pub(super) fn progress_write(
    cp: &Checkpoint,
    done: u32,
    total: u32,
    idx: usize,
    auto_accepted: u32,
    counts: WriteProgressCounts,
) -> TransferProgress {
    TransferProgress {
        job_id: cp.job_id.clone(),
        stage: Stage::Writing,
        done,
        total,
        matched: counts.matched,
        auto_accepted,
        ambiguous: counts.ambiguous,
        not_found: counts.not_found,
        written: counts.written,
        current: cp
            .tracks
            .get(idx)
            .map(|track| track.input.display())
            .unwrap_or_default(),
    }
}

pub(super) fn written_count(tracks: &[TrackEntry]) -> u32 {
    tracks.iter().filter(|track| track.written).count() as u32
}
