use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use futures::{StreamExt, stream};
use tokio::sync::Mutex as AsyncMutex;

use crate::api::ytmusic::{TransferAlbum, TransferAlbumCandidate, TransferAlbumTrack, YtMusicApi};
use crate::search_source::SearchConfig;
use crate::transfer::checkpoint::Checkpoint;
use crate::transfer::match_cache::{CachedAlbum, TransferMatchCache, album_key};
use crate::transfer::matching::{
    CandidateSourceKind, MatchCandidate, MatchConfig, MatchOutcome, MatchScoreBreakdown, Pacing,
    TrackInput, normalize, normalize_stripped, score_candidate_breakdown_with_config, similarity,
};

mod stats;

pub(super) use stats::{merge_ytm_diagnostics, record_match_outcome_stats};

/// Album work shares the same 600 ms catalog start pacer as per-track matching. Two in-flight
/// groups hide network latency without increasing request start rate or creating a second pacer.
const ALBUM_LIVE_CONCURRENCY: usize = 2;
const ALBUM_LIVE_MIN_TRACKS: usize = 3;
const ALBUM_LIVE_GROUP_LIMIT: usize = 12;
const ALBUM_METADATA_MIN_SCORE: f32 = 0.82;
const ALBUM_METADATA_CLOSE_MARGIN: f32 = 0.04;
const ALBUM_DETAIL_SCORE_MARGIN: f32 = 0.02;

struct AlbumGroupWork {
    order: usize,
    key: String,
    indexes: Vec<usize>,
    inputs: Vec<TrackInput>,
    query: String,
}

struct AlbumGroupResolution {
    order: usize,
    key: String,
    indexes: Vec<usize>,
    album: Option<TransferAlbum>,
    catalog_searches: u32,
}

pub(super) fn apply_persistent_cache(cp: &mut Checkpoint, cache: &TransferMatchCache) -> bool {
    let mut changed = false;
    for entry in &mut cp.tracks {
        if entry.outcome.is_some() {
            continue;
        }
        if let Some(hit) = cache.lookup_track(&entry.input) {
            cp.match_stats.bump_cache_hit(hit.kind);
            record_match_outcome_stats(&mut cp.match_stats, &hit.outcome);
            entry.outcome = Some(hit.outcome);
            changed = true;
        }
    }
    changed
}

pub(super) async fn prefill_album_matches(
    cp: &mut Checkpoint,
    api: &YtMusicApi,
    cfg: &MatchConfig,
    search_config: &SearchConfig,
    cache: &mut TransferMatchCache,
    cache_read_enabled: bool,
    pace: &mut Pacing,
) -> anyhow::Result<bool> {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, entry) in cp.tracks.iter().enumerate() {
        if let Some(key) = album_key(&entry.input) {
            groups.entry(key).or_default().push(idx);
        }
    }

    // Largest groups have the greatest chance of avoiding per-track probes. Stable source-order
    // and key tie-breaks keep provider scheduling deterministic despite HashMap iteration order.
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by(compare_album_group_priority);

    let mut changed = false;
    let mut live_groups = Vec::new();
    for (key, indexes) in groups {
        if indexes.len() < 2 || indexes.iter().all(|idx| cp.tracks[*idx].outcome.is_some()) {
            continue;
        }
        cp.match_stats.album_groups_attempted += 1;
        if cache_read_enabled && let Some(album) = cache.lookup_album(&key).cloned() {
            let matched = apply_cached_album_matches(cp, cfg, cache, &album, &indexes);
            if matched > 0 {
                cp.match_stats.album_groups_matched += 1;
                changed = true;
            }
            continue;
        }

        // Two-track groups still benefit from a cache hit, but a live album lookup costs at
        // least a paced search plus one paced detail call. Spend that budget only where one
        // successful album can replace three or more per-track searches, and cap the queue.
        if !within_live_album_budget(&indexes, live_groups.len()) {
            continue;
        }

        let inputs = indexes
            .iter()
            .map(|idx| cp.tracks[*idx].input.clone())
            .collect::<Vec<_>>();
        let Some(query) = album_query(&inputs) else {
            continue;
        };
        live_groups.push(AlbumGroupWork {
            order: live_groups.len(),
            key,
            indexes,
            inputs,
            query,
        });
    }

    // Cached evidence above stays available in anonymous/degraded runs. Do not create queued
    // live work, acquire the pacer, or consume a search counter when the provider is unavailable.
    if live_groups.is_empty() || !api.transfer_catalog_available() {
        return Ok(changed);
    }

    let shared_pace = AsyncMutex::new(pace);
    let mut resolutions = stream::iter(live_groups)
        .map(|work| {
            let shared_pace = &shared_pace;
            async move {
                resolve_live_album_group(work, api, cfg, search_config, shared_pace).await
            }
        })
        .buffer_unordered(ALBUM_LIVE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    // Completion order depends on provider latency. Apply disjoint groups in launch-priority
    // order so cache/report state remains deterministic.
    resolutions.sort_by_key(|resolution| resolution.order);
    for resolution in resolutions {
        cp.match_stats.catalog_searches = cp
            .match_stats
            .catalog_searches
            .saturating_add(resolution.catalog_searches);
        let Some(album) = resolution.album else {
            continue;
        };
        cache.save_album(resolution.key, &album);
        changed = true;
        let matched = apply_transfer_album_matches(cp, cfg, cache, &album, &resolution.indexes);
        if matched > 0 {
            cp.match_stats.album_groups_matched += 1;
        }
    }
    Ok(changed)
}

fn compare_album_group_priority(
    (key_a, indexes_a): &(String, Vec<usize>),
    (key_b, indexes_b): &(String, Vec<usize>),
) -> Ordering {
    indexes_b
        .len()
        .cmp(&indexes_a.len())
        .then_with(|| indexes_a.first().cmp(&indexes_b.first()))
        .then_with(|| key_a.cmp(key_b))
}

fn within_live_album_budget(indexes: &[usize], queued_groups: usize) -> bool {
    indexes.len() >= ALBUM_LIVE_MIN_TRACKS && queued_groups < ALBUM_LIVE_GROUP_LIMIT
}

async fn wait_for_album_catalog_slot(api: &YtMusicApi, pace: &AsyncMutex<&mut Pacing>) -> bool {
    // Check both before queueing and after acquiring the shared pacer. A prior in-flight search
    // may have opened the global parser cooldown while this group was waiting.
    if !api.transfer_catalog_available() {
        return false;
    }
    let mut pace = pace.lock().await;
    if !api.transfer_catalog_available() {
        return false;
    }
    pace.tick().await;
    api.transfer_catalog_available()
}

async fn resolve_live_album_group(
    work: AlbumGroupWork,
    api: &YtMusicApi,
    cfg: &MatchConfig,
    search_config: &SearchConfig,
    pace: &AsyncMutex<&mut Pacing>,
) -> AlbumGroupResolution {
    let mut resolution = AlbumGroupResolution {
        order: work.order,
        key: work.key,
        indexes: work.indexes,
        album: None,
        catalog_searches: 0,
    };
    if !wait_for_album_catalog_slot(api, pace).await {
        return resolution;
    }
    resolution.catalog_searches = 1;
    let candidates = match api.search_transfer_albums(&work.query, search_config).await {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::debug!(
                query = %work.query,
                error = %crate::util::sanitize::sanitize_error_text(format!("{error:#}")),
                "transfer album search failed"
            );
            return resolution;
        }
    };
    let shortlist = album_candidate_shortlist(&work.inputs, &candidates);
    if shortlist.is_empty() {
        return resolution;
    }

    let expected_details = shortlist.len();
    let mut details = Vec::with_capacity(expected_details);
    for candidate in shortlist {
        // Search/detail calls use one shared start pacer. Capability is rechecked after every
        // response so a parser cooldown prevents queued detail work immediately.
        if !wait_for_album_catalog_slot(api, pace).await {
            break;
        }
        match api.transfer_album_tracks(&candidate.album_id).await {
            Ok(Some(album)) => details.push(album),
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(
                    album_id = %candidate.album_id,
                    error = %crate::util::sanitize::sanitize_error_text(format!("{error:#}")),
                    "transfer album detail fetch failed"
                );
                break;
            }
        }
    }
    // A close metadata race is intentionally unresolved unless every planned detail was
    // available; choosing the sole surviving candidate would recreate the old false-positive.
    if details.len() == expected_details {
        resolution.album = select_album_detail(&work.inputs, details, cfg);
    }
    resolution
}

fn apply_cached_album_matches(
    cp: &mut Checkpoint,
    cfg: &MatchConfig,
    cache: &mut TransferMatchCache,
    album: &CachedAlbum,
    indexes: &[usize],
) -> u32 {
    let candidates = album
        .tracks
        .iter()
        .map(|track| MatchCandidate {
            key: track.video_id.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: Some(track.album.clone()),
            duration_secs: track.duration_secs,
            track_number: track.track_number,
            release_year: album
                .year
                .as_deref()
                .and_then(|year| year.parse::<u16>().ok()),
            explicit: None,
            source_kind: CandidateSourceKind::YtmAlbumTrack,
            channel: Some(track.artist.clone()),
            channel_id: None,
            channel_verified: None,
            availability: None,
            has_audio_format: None,
            max_audio_bitrate_kbps: None,
            isrc: None,
            metadata_title: None,
            preflighted: false,
            preflight_reject_reason: None,
            preflight_reason_codes: Vec::new(),
        })
        .collect::<Vec<_>>();
    apply_album_candidates(cp, cfg, cache, &candidates, indexes)
}

fn apply_transfer_album_matches(
    cp: &mut Checkpoint,
    cfg: &MatchConfig,
    cache: &mut TransferMatchCache,
    album: &TransferAlbum,
    indexes: &[usize],
) -> u32 {
    let candidates = album
        .tracks
        .iter()
        .map(|track| album_track_candidate(track, album.year.as_deref()))
        .collect::<Vec<_>>();
    apply_album_candidates(cp, cfg, cache, &candidates, indexes)
}

fn apply_album_candidates(
    cp: &mut Checkpoint,
    cfg: &MatchConfig,
    cache: &mut TransferMatchCache,
    candidates: &[MatchCandidate],
    indexes: &[usize],
) -> u32 {
    let candidates = prepare_album_candidates(cp, candidates, indexes);
    let source_order = source_album_order(cp, indexes);
    let candidate_indexes = candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.key.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut used = vec![false; candidates.len()];
    for key in indexes
        .iter()
        .filter_map(|idx| match cp.tracks[*idx].outcome.as_ref() {
            Some(MatchOutcome::Matched { key, .. }) => Some(key.as_str()),
            _ => None,
        })
    {
        if let Some(index) = candidate_indexes.get(key) {
            used[*index] = true;
        }
    }
    let mut rows = indexes
        .iter()
        .copied()
        .filter(|idx| cp.tracks[*idx].outcome.is_none())
        .map(|index| AlbumRowState::new(index, &cp.tracks[index].input, &candidates, cfg, &used))
        .collect::<Vec<_>>();
    let mut matched = 0;

    // Every source×candidate breakdown is computed once in AlbumRowState. Candidate claims only
    // advance per-row ranked cursors and decrement precomputed viable counts, preserving the
    // constraints-first one-to-one behavior without rescoring the full matrix after every edge.
    loop {
        let mut proposals = rows
            .iter_mut()
            .filter(|row| row.active)
            .filter_map(|row| row.proposal(&candidates, cfg, &source_order, &used))
            .collect::<Vec<_>>();
        proposals.sort_by(compare_album_proposals);
        let Some(proposal) = proposals.into_iter().next() else {
            break;
        };

        let input = cp.tracks[proposal.index].input.clone();
        cp.match_stats.album_tracks_matched += 1;
        record_match_outcome_stats(&mut cp.match_stats, &proposal.outcome);
        cache.save_match(&input, &proposal.outcome);
        cp.tracks[proposal.index].outcome = Some(proposal.outcome);
        if let Some(row) = rows.iter_mut().find(|row| row.index == proposal.index) {
            row.active = false;
        }
        if !used[proposal.candidate_index] {
            used[proposal.candidate_index] = true;
            for row in rows.iter_mut().filter(|row| row.active) {
                row.mark_candidate_used(proposal.candidate_index);
            }
        }
        matched += 1;
    }
    matched
}

fn prepare_album_candidates(
    cp: &Checkpoint,
    candidates: &[MatchCandidate],
    indexes: &[usize],
) -> Vec<MatchCandidate> {
    prepare_album_candidates_for_inputs(
        candidates,
        indexes.iter().map(|idx| &cp.tracks[*idx].input),
    )
}

fn prepare_album_candidates_for_inputs<'a>(
    candidates: &[MatchCandidate],
    inputs: impl IntoIterator<Item = &'a TrackInput>,
) -> Vec<MatchCandidate> {
    let mut seen_keys = HashSet::new();
    let mut candidates = candidates
        .iter()
        .filter(|candidate| seen_keys.insert(candidate.key.clone()))
        .cloned()
        .collect::<Vec<_>>();
    let mut source_track_numbers = HashMap::<u32, usize>::new();
    for track_number in inputs.into_iter().filter_map(|input| input.track_number) {
        *source_track_numbers.entry(track_number).or_default() += 1;
    }
    let mut candidate_track_numbers = HashMap::<u32, usize>::new();
    for track_number in candidates
        .iter()
        .filter_map(|candidate| candidate.track_number)
    {
        *candidate_track_numbers.entry(track_number).or_default() += 1;
    }

    // Destination album rows currently carry no disc number. Only retain the small track
    // number tie-break when that number is unique on both sides; a repeated "track 1" from
    // a multi-disc release is not a globally unique identity signal.
    for candidate in &mut candidates {
        if candidate.track_number.is_some_and(|track_number| {
            source_track_numbers.get(&track_number) != Some(&1)
                || candidate_track_numbers.get(&track_number) != Some(&1)
        }) {
            candidate.track_number = None;
        }
    }
    candidates
}

fn source_album_order(cp: &Checkpoint, indexes: &[usize]) -> HashMap<usize, usize> {
    let mut ordered = indexes.iter().copied().enumerate().collect::<Vec<_>>();
    ordered.sort_by_key(|(fallback_order, idx)| {
        let input = &cp.tracks[*idx].input;
        (
            input.disc_number.unwrap_or(u32::MAX),
            input.track_number.unwrap_or(u32::MAX),
            *fallback_order,
        )
    });
    ordered
        .into_iter()
        .enumerate()
        .map(|(album_order, (_, idx))| (idx, album_order))
        .collect()
}

fn input_album_order(inputs: &[TrackInput]) -> HashMap<usize, usize> {
    let mut ordered = inputs.iter().enumerate().collect::<Vec<_>>();
    ordered.sort_by_key(|(fallback_order, input)| {
        (
            input.disc_number.unwrap_or(u32::MAX),
            input.track_number.unwrap_or(u32::MAX),
            *fallback_order,
        )
    });
    ordered
        .into_iter()
        .enumerate()
        .map(|(album_order, (input_index, _))| (input_index, album_order))
        .collect()
}

struct AlbumMatchProposal {
    index: usize,
    candidate_index: usize,
    outcome: MatchOutcome,
    viable_candidates: usize,
    score: f32,
    title: f32,
    duration: f32,
    track_number_bonus: f32,
    order_distance: usize,
}

struct AlbumRowScore {
    candidate_index: usize,
    cluster_key: String,
    score: MatchScoreBreakdown,
}

struct AlbumRowState {
    index: usize,
    active: bool,
    ranked: Vec<AlbumRowScore>,
    viable: Vec<bool>,
    viable_remaining: usize,
    cursor: usize,
}

impl AlbumRowState {
    fn new(
        index: usize,
        input: &TrackInput,
        candidates: &[MatchCandidate],
        cfg: &MatchConfig,
        used: &[bool],
    ) -> Self {
        let mut viable = vec![false; candidates.len()];
        let mut ranked = candidates
            .iter()
            .enumerate()
            .filter_map(|(candidate_index, candidate)| {
                let score = score_candidate_breakdown_with_config(input, candidate, cfg);
                let eligible = score.reject_reason.is_none() && !score.accept_blocked;
                viable[candidate_index] = eligible && score.total >= cfg.accept;
                eligible.then(|| AlbumRowScore {
                    candidate_index,
                    cluster_key: album_candidate_cluster_key(candidate),
                    score,
                })
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| {
            b.score
                .total
                .total_cmp(&a.score.total)
                .then_with(|| a.candidate_index.cmp(&b.candidate_index))
        });
        let viable_remaining = viable
            .iter()
            .enumerate()
            .filter(|(candidate_index, viable)| **viable && !used[*candidate_index])
            .count();
        Self {
            index,
            active: true,
            ranked,
            viable,
            viable_remaining,
            cursor: 0,
        }
    }

    fn mark_candidate_used(&mut self, candidate_index: usize) {
        if self.viable.get(candidate_index) == Some(&true) {
            self.viable_remaining = self.viable_remaining.saturating_sub(1);
        }
    }

    fn proposal(
        &mut self,
        candidates: &[MatchCandidate],
        cfg: &MatchConfig,
        source_order: &HashMap<usize, usize>,
        used: &[bool],
    ) -> Option<AlbumMatchProposal> {
        while self
            .ranked
            .get(self.cursor)
            .is_some_and(|score| used[score.candidate_index])
        {
            self.cursor += 1;
        }
        let best = self.ranked.get(self.cursor)?;
        let second = self.ranked[self.cursor + 1..].iter().find(|candidate| {
            !used[candidate.candidate_index] && candidate.cluster_key != best.cluster_key
        });
        let margin = second.map_or(best.score.total, |second| {
            best.score.total - second.score.total
        });
        if best.score.total < cfg.accept || margin < cfg.accept_margin {
            return None;
        }

        let candidate = &candidates[best.candidate_index];
        let mut score = best.score.clone();
        let default_cfg = MatchConfig::default();
        if score.total >= default_cfg.accept
            && margin >= default_cfg.accept_margin
            && !score
                .reason_codes
                .iter()
                .any(|code| code == "policy_safe_cache")
        {
            score.reason_codes.push("policy_safe_cache".to_owned());
        }
        let source_order = source_order.get(&self.index).copied().unwrap_or(self.index);
        Some(AlbumMatchProposal {
            index: self.index,
            candidate_index: best.candidate_index,
            outcome: MatchOutcome::Matched {
                key: candidate.key.clone(),
                score: score.total,
                display: format!("{} — {}", candidate.artist, candidate.title),
                title: Some(candidate.title.clone()),
                artist: Some(candidate.artist.clone()),
                album: candidate.album.clone(),
                duration_secs: candidate.duration_secs,
                score_breakdown: Some(Box::new(score.clone())),
            },
            viable_candidates: self.viable_remaining,
            score: score.total,
            title: score.title,
            duration: score.duration,
            track_number_bonus: score.track_number_bonus,
            order_distance: source_order.abs_diff(best.candidate_index),
        })
    }
}

fn album_candidate_cluster_key(candidate: &MatchCandidate) -> String {
    format!(
        "{}\u{1f}{}",
        normalize(&candidate.artist),
        normalize_stripped(&candidate.title)
    )
}

fn compare_album_proposals(a: &AlbumMatchProposal, b: &AlbumMatchProposal) -> Ordering {
    a.viable_candidates
        .cmp(&b.viable_candidates)
        .then_with(|| b.score.total_cmp(&a.score))
        .then_with(|| b.title.total_cmp(&a.title))
        .then_with(|| b.duration.total_cmp(&a.duration))
        .then_with(|| b.track_number_bonus.total_cmp(&a.track_number_bonus))
        .then_with(|| a.order_distance.cmp(&b.order_distance))
        .then_with(|| a.index.cmp(&b.index))
}

fn album_track_candidate(track: &TransferAlbumTrack, release_year: Option<&str>) -> MatchCandidate {
    MatchCandidate {
        key: track.video_id.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: Some(track.album.clone()),
        duration_secs: track
            .duration_secs
            .or_else(|| crate::streaming::candidate::parse_duration_secs(track.duration.as_str())),
        track_number: track.track_number,
        release_year: release_year.and_then(|year| year.parse::<u16>().ok()),
        explicit: None,
        source_kind: CandidateSourceKind::YtmAlbumTrack,
        channel: Some(track.artist.clone()),
        channel_id: None,
        channel_verified: None,
        availability: None,
        has_audio_format: None,
        max_audio_bitrate_kbps: None,
        isrc: None,
        metadata_title: None,
        preflighted: false,
        preflight_reject_reason: None,
        preflight_reason_codes: Vec::new(),
    }
}

fn album_query(inputs: &[TrackInput]) -> Option<String> {
    let album = consensus_value(
        inputs.iter().filter_map(|input| input.album.clone()),
        normalize_stripped,
    )?;
    let artist = consensus_value(
        inputs.iter().filter_map(|input| {
            input
                .album_artists
                .first()
                .or_else(|| input.artists.first())
                .cloned()
        }),
        normalize,
    )?;
    let mut query = format!("{artist} {album}");
    if let Some(year) = consensus_value(
        inputs.iter().filter_map(|input| {
            input
                .album_release_date
                .as_deref()
                .and_then(|date| date.get(0..4))
                .filter(|year| year.bytes().all(|b| b.is_ascii_digit()))
                .map(str::to_owned)
        }),
        normalize,
    ) {
        query.push(' ');
        query.push_str(&year);
    }
    Some(query)
}

fn consensus_value(
    values: impl IntoIterator<Item = String>,
    normalize_key: fn(&str) -> String,
) -> Option<String> {
    let mut counts = HashMap::<String, (usize, usize, String)>::new();
    for (order, value) in values.into_iter().enumerate() {
        let value = value.trim();
        let key = normalize_key(value);
        if value.is_empty() || key.is_empty() {
            continue;
        }
        let entry = counts
            .entry(key)
            .or_insert_with(|| (0, order, value.to_owned()));
        entry.0 += 1;
    }
    counts
        .into_values()
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)))
        .map(|(_, _, value)| value)
}

#[cfg(test)]
fn best_album_candidate(
    inputs: &[TrackInput],
    candidates: &[TransferAlbumCandidate],
) -> Option<TransferAlbumCandidate> {
    let shortlist = album_candidate_shortlist(inputs, candidates);
    if shortlist.len() == 1 {
        shortlist.into_iter().next()
    } else {
        None
    }
}

fn album_candidate_shortlist(
    inputs: &[TrackInput],
    candidates: &[TransferAlbumCandidate],
) -> Vec<TransferAlbumCandidate> {
    let mut seen = HashSet::new();
    let mut scored = candidates
        .iter()
        .filter(|candidate| seen.insert(candidate.album_id.as_str()))
        .map(|candidate| (album_candidate_score(inputs, candidate), candidate))
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.album_id.cmp(&b.1.album_id))
    });
    let Some((score, best)) = scored.first() else {
        return Vec::new();
    };
    if *score < ALBUM_METADATA_MIN_SCORE {
        return Vec::new();
    }
    let mut shortlist = vec![(*best).clone()];
    if let Some((second, _)) = scored.get(1)
        && score - second < ALBUM_METADATA_CLOSE_MARGIN
    {
        shortlist.push((*scored[1].1).clone());
    }
    shortlist
}

#[derive(Debug, Clone, Copy)]
struct AlbumGroupEvidence {
    matched: usize,
    average_score: f32,
    track_count_distance: Option<u32>,
}

fn select_album_detail(
    inputs: &[TrackInput],
    mut albums: Vec<TransferAlbum>,
    cfg: &MatchConfig,
) -> Option<TransferAlbum> {
    if albums.len() == 1 {
        return albums.pop();
    }
    if albums.len() != 2 {
        return None;
    }
    let evidence = albums
        .iter()
        .map(|album| album_group_evidence(inputs, album, cfg))
        .collect::<Vec<_>>();
    let selected = compare_album_detail_evidence(evidence[0], evidence[1]);
    match selected {
        Ordering::Greater => Some(albums.remove(0)),
        Ordering::Less => Some(albums.remove(1)),
        Ordering::Equal => None,
    }
}

fn compare_album_detail_evidence(a: AlbumGroupEvidence, b: AlbumGroupEvidence) -> Ordering {
    let coverage = a.matched.cmp(&b.matched);
    if coverage != Ordering::Equal {
        return coverage;
    }
    if a.matched == 0 {
        return Ordering::Equal;
    }
    if let (Some(a_distance), Some(b_distance)) = (a.track_count_distance, b.track_count_distance) {
        let count_fit = b_distance.cmp(&a_distance);
        if count_fit != Ordering::Equal {
            return count_fit;
        }
    }
    let score_delta = a.average_score - b.average_score;
    if score_delta.abs() >= ALBUM_DETAIL_SCORE_MARGIN {
        score_delta.total_cmp(&0.0)
    } else {
        Ordering::Equal
    }
}

fn album_group_evidence(
    inputs: &[TrackInput],
    album: &TransferAlbum,
    cfg: &MatchConfig,
) -> AlbumGroupEvidence {
    let raw_candidates = album
        .tracks
        .iter()
        .map(|track| album_track_candidate(track, album.year.as_deref()))
        .collect::<Vec<_>>();
    let candidates = prepare_album_candidates_for_inputs(&raw_candidates, inputs.iter());
    let source_order = input_album_order(inputs);
    let mut used = vec![false; candidates.len()];
    let mut rows = inputs
        .iter()
        .enumerate()
        .map(|(index, input)| AlbumRowState::new(index, input, &candidates, cfg, &used))
        .collect::<Vec<_>>();
    let mut matched = 0usize;
    let mut score_sum = 0.0f32;
    loop {
        let mut proposals = rows
            .iter_mut()
            .filter(|row| row.active)
            .filter_map(|row| row.proposal(&candidates, cfg, &source_order, &used))
            .collect::<Vec<_>>();
        proposals.sort_by(compare_album_proposals);
        let Some(proposal) = proposals.into_iter().next() else {
            break;
        };
        matched += 1;
        score_sum += proposal.score;
        if let Some(row) = rows.iter_mut().find(|row| row.index == proposal.index) {
            row.active = false;
        }
        used[proposal.candidate_index] = true;
        for row in rows.iter_mut().filter(|row| row.active) {
            row.mark_candidate_used(proposal.candidate_index);
        }
    }
    let expected_tracks = consensus_u32(inputs.iter().filter_map(|input| input.album_total_tracks));
    AlbumGroupEvidence {
        matched,
        average_score: if matched > 0 {
            score_sum / matched as f32
        } else {
            0.0
        },
        track_count_distance: expected_tracks.map(|expected| {
            expected.abs_diff(u32::try_from(album.tracks.len()).unwrap_or(u32::MAX))
        }),
    }
}

fn consensus_u32(values: impl IntoIterator<Item = u32>) -> Option<u32> {
    let mut counts = HashMap::<u32, (usize, usize)>::new();
    for (order, value) in values.into_iter().enumerate() {
        let entry = counts.entry(value).or_insert((0, order));
        entry.0 += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.0.cmp(&b.1.0).then_with(|| b.1.1.cmp(&a.1.1)))
        .map(|(value, _)| value)
}

fn album_candidate_score(inputs: &[TrackInput], candidate: &TransferAlbumCandidate) -> f32 {
    let candidate_title = normalize_stripped(&candidate.title);
    let title = median_score(inputs.iter().filter_map(|input| {
        let input_album = normalize_stripped(input.album.as_deref()?);
        (!input_album.is_empty()).then(|| similarity(&input_album, &candidate_title))
    }))
    .unwrap_or(0.5);
    let candidate_artist = normalize(&candidate.artist);
    let artist = median_score(inputs.iter().filter_map(|input| {
        let artists = if input.album_artists.is_empty() {
            &input.artists
        } else {
            &input.album_artists
        };
        artists
            .iter()
            .map(|artist| normalize(artist))
            .filter(|artist| !artist.is_empty() && !candidate_artist.is_empty())
            .map(|input_artist| {
                if input_artist.contains(&candidate_artist)
                    || candidate_artist.contains(&input_artist)
                {
                    1.0
                } else {
                    similarity(&input_artist, &candidate_artist)
                }
            })
            .max_by(f32::total_cmp)
    }))
    .unwrap_or(0.5);
    let year = median_score(inputs.iter().filter_map(|input| {
        let input_year = input
            .album_release_date
            .as_deref()
            .and_then(|date| date.get(0..4))?;
        Some(match candidate.year.as_deref() {
            Some(candidate_year) if input_year == candidate_year => 1.0,
            Some(_) => 0.4,
            None => 0.6,
        })
    }))
    .unwrap_or(0.6);
    let type_bonus = 0.03
        * median_score(inputs.iter().filter_map(|input| {
            let input_type = input.album_type.as_deref()?;
            let candidate_type = candidate.album_type.as_deref()?;
            Some(if normalize(input_type) == normalize(candidate_type) {
                1.0
            } else {
                0.0
            })
        }))
        .unwrap_or(0.0);
    0.60 * title + 0.30 * artist + 0.10 * year + type_bonus
}

fn median_score(scores: impl IntoIterator<Item = f32>) -> Option<f32> {
    let mut scores = scores.into_iter().collect::<Vec<_>>();
    if scores.is_empty() {
        return None;
    }
    scores.sort_by(f32::total_cmp);
    let middle = scores.len() / 2;
    if scores.len() % 2 == 0 {
        Some((scores[middle - 1] + scores[middle]) / 2.0)
    } else {
        Some(scores[middle])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::checkpoint::{Checkpoint, TrackEntry};
    use crate::transfer::match_cache::CachedAlbumTrack;
    use crate::transfer::matching::{MatchOutcome, MatchScoreBreakdown, TrackInput};
    use crate::transfer::{JobSpec, MatchPolicy, TransferCacheMode, TransferDest, TransferSource};

    fn track_input(title: &str, artist: &str, album: &str, track_number: u32) -> TrackInput {
        TrackInput {
            title: title.to_owned(),
            artists: vec![artist.to_owned()],
            album_artists: vec![artist.to_owned()],
            album: Some(album.to_owned()),
            album_id: Some("sp-album".to_owned()),
            album_uri: None,
            album_release_date: Some("2024-01-01".to_owned()),
            album_release_date_precision: None,
            album_total_tracks: Some(2),
            album_type: Some("album".to_owned()),
            album_art_url: None,
            disc_number: Some(1),
            track_number: Some(track_number),
            duration_secs: Some(180),
            isrc: None,
            explicit: None,
            source_url: None,
            source_key: format!("spotify:track:{title}"),
            known_video_id: None,
        }
    }

    fn matched(key: &str) -> MatchOutcome {
        MatchOutcome::Matched {
            key: key.to_owned(),
            score: 0.96,
            display: "Artist — Song".to_owned(),
            title: Some("Song".to_owned()),
            artist: Some("Artist".to_owned()),
            album: Some("Album".to_owned()),
            duration_secs: Some(180),
            score_breakdown: Some(Box::new(MatchScoreBreakdown {
                source_kind: "ytm_album_track".to_owned(),
                quality_tier: "catalog".to_owned(),
                total: 0.96,
                raw_total: 0.96,
                reason_codes: vec!["policy_safe_cache".to_owned()],
                ..MatchScoreBreakdown::default()
            })),
        }
    }

    fn job_spec() -> JobSpec {
        JobSpec {
            source: TransferSource::SpotifyLiked,
            dest: TransferDest::YtmLikes,
            dry_run: true,
            min_score: 0.80,
            take_best: false,
            auto_accept_ambiguous_min_score: None,
            match_policy: MatchPolicy::Strict,
            allow_user_videos: false,
            cache_mode: TransferCacheMode::Use,
            rematch: false,
        }
    }

    fn transfer_album(album_id: &str, titles: &[&str]) -> TransferAlbum {
        TransferAlbum {
            album_id: album_id.to_owned(),
            title: "Album".to_owned(),
            artist: "Artist".to_owned(),
            year: Some("2024".to_owned()),
            tracks: titles
                .iter()
                .enumerate()
                .map(|(index, title)| TransferAlbumTrack {
                    video_id: format!("{album_id}-{index}"),
                    title: (*title).to_owned(),
                    artist: "Artist".to_owned(),
                    album: "Album".to_owned(),
                    track_number: u32::try_from(index + 1).ok(),
                    duration: "3:00".to_owned(),
                    duration_secs: Some(180),
                })
                .collect(),
        }
    }

    #[test]
    fn live_album_budget_selects_largest_groups_and_stops_at_limit() {
        let mut groups = (0..14)
            .map(|order| {
                (
                    format!("album-{order:02}"),
                    vec![order; ALBUM_LIVE_MIN_TRACKS + order],
                )
            })
            .collect::<Vec<_>>();
        groups.push(("two-track-cache-only".to_owned(), vec![99, 100]));
        groups.sort_by(compare_album_group_priority);

        let mut selected = Vec::new();
        for (key, indexes) in groups {
            if within_live_album_budget(&indexes, selected.len()) {
                selected.push((key, indexes.len()));
            }
        }

        assert_eq!(selected.len(), ALBUM_LIVE_GROUP_LIMIT);
        assert_eq!(selected.first().map(|(_, size)| *size), Some(16));
        assert_eq!(selected.last().map(|(_, size)| *size), Some(5));
        assert!(selected.iter().all(|(_, size)| *size >= 5));
        assert!(
            selected
                .iter()
                .all(|(key, _)| key != "two-track-cache-only")
        );
    }

    #[test]
    fn apply_persistent_cache_fills_unmatched_rows_and_skips_existing() {
        let mut cache = TransferMatchCache::default();
        let hit_input = track_input("Song A", "Artist", "Album", 1);
        cache.save_match(&hit_input, &matched("ytm-a"));

        let mut cp = Checkpoint::new(
            "job-cache".to_owned(),
            job_spec(),
            vec![
                TrackEntry {
                    input: hit_input.clone(),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: track_input("Song B", "Artist", "Album", 2),
                    outcome: Some(matched("already")),
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: track_input("Song C", "Other", "Elsewhere", 1),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
            ],
        );

        assert!(apply_persistent_cache(&mut cp, &cache));
        assert!(matches!(
            cp.tracks[0].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "ytm-a"
        ));
        assert!(matches!(
            cp.tracks[1].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "already"
        ));
        assert!(cp.tracks[2].outcome.is_none());
        assert_eq!(cp.match_stats.cache_hits.get("source_key"), Some(&1));
    }

    #[test]
    fn best_album_candidate_accepts_clear_hit_and_rejects_ambiguous_or_weak() {
        let input = track_input("Song A", "Artist", "Album", 1);
        let clear = TransferAlbumCandidate {
            album_id: "album-1".to_owned(),
            title: "Album".to_owned(),
            artist: "Artist".to_owned(),
            year: Some("2024".to_owned()),
            album_type: Some("album".to_owned()),
        };
        assert_eq!(
            best_album_candidate(std::slice::from_ref(&input), std::slice::from_ref(&clear))
                .map(|c| c.album_id),
            Some("album-1".to_owned())
        );

        let weak = TransferAlbumCandidate {
            album_id: "album-weak".to_owned(),
            title: "Totally Different".to_owned(),
            artist: "Someone Else".to_owned(),
            year: Some("1999".to_owned()),
            album_type: None,
        };
        assert!(
            best_album_candidate(std::slice::from_ref(&input), std::slice::from_ref(&weak))
                .is_none()
        );

        let twin_a = TransferAlbumCandidate {
            album_id: "album-a".to_owned(),
            title: "Album".to_owned(),
            artist: "Artist".to_owned(),
            year: Some("2024".to_owned()),
            album_type: Some("album".to_owned()),
        };
        let twin_b = TransferAlbumCandidate {
            album_id: "album-b".to_owned(),
            title: "Album".to_owned(),
            artist: "Artist".to_owned(),
            year: Some("2024".to_owned()),
            album_type: Some("album".to_owned()),
        };
        let twins = [twin_a, twin_b];
        assert_eq!(
            album_candidate_shortlist(std::slice::from_ref(&input), &twins)
                .iter()
                .map(|candidate| candidate.album_id.as_str())
                .collect::<Vec<_>>(),
            vec!["album-a", "album-b"]
        );
        assert!(best_album_candidate(std::slice::from_ref(&input), &twins).is_none());
    }

    #[test]
    fn album_selection_uses_group_consensus_instead_of_first_row() {
        let mut noisy_first = track_input("Song A", "Wrong Artist", "Wrong Collection", 1);
        noisy_first.album_artists = vec!["Wrong Artist".to_owned()];
        noisy_first.album_release_date = Some("1999-01-01".to_owned());
        noisy_first.album_type = Some("single".to_owned());
        let inputs = vec![
            noisy_first,
            track_input("Song B", "Artist", "Album", 2),
            track_input("Song C", "Artist", "Album", 3),
        ];
        let wrong = TransferAlbumCandidate {
            album_id: "wrong".to_owned(),
            title: "Wrong Collection".to_owned(),
            artist: "Wrong Artist".to_owned(),
            year: Some("1999".to_owned()),
            album_type: Some("single".to_owned()),
        };
        let consensus = TransferAlbumCandidate {
            album_id: "consensus".to_owned(),
            title: "Album".to_owned(),
            artist: "Artist".to_owned(),
            year: Some("2024".to_owned()),
            album_type: Some("album".to_owned()),
        };

        assert_eq!(album_query(&inputs).as_deref(), Some("Artist Album 2024"));
        assert_eq!(
            best_album_candidate(&inputs, &[wrong, consensus]).map(|candidate| candidate.album_id),
            Some("consensus".to_owned())
        );
    }

    #[test]
    fn close_album_metadata_uses_group_coverage_then_track_count() {
        let inputs = vec![
            track_input("Alpha Theme", "Artist", "Album", 1),
            track_input("Beta Groove", "Artist", "Album", 2),
        ];
        let partial = transfer_album("partial", &["Alpha Theme", "Unrelated Noise"]);
        let complete = transfer_album("complete", &["Alpha Theme", "Beta Groove"]);

        let selected = select_album_detail(
            &inputs,
            vec![partial, complete.clone()],
            &MatchConfig::default(),
        )
        .expect("complete track coverage should disambiguate close album metadata");
        assert_eq!(selected.album_id, "complete");

        let deluxe = transfer_album("deluxe", &["Alpha Theme", "Beta Groove", "Unrelated Bonus"]);
        let selected = select_album_detail(
            &inputs,
            vec![deluxe, complete.clone()],
            &MatchConfig::default(),
        )
        .expect("expected track count should break equal-coverage ties");
        assert_eq!(selected.album_id, "complete");

        let indistinguishable = transfer_album("same-evidence", &["Alpha Theme", "Beta Groove"]);
        assert!(
            select_album_detail(
                &inputs,
                vec![complete, indistinguishable],
                &MatchConfig::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn apply_cached_album_matches_fills_matching_tracks_only() {
        let mut cache = TransferMatchCache::default();
        let mut cp = Checkpoint::new(
            "job-album".to_owned(),
            job_spec(),
            vec![
                TrackEntry {
                    input: track_input("Alpha Theme", "Artist", "Album", 1),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: track_input("Beta Groove", "Artist", "Album", 2),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: track_input("Missing Track", "Artist", "Album", 9),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
            ],
        );
        let album = CachedAlbum {
            ytm_album_id: "ytm-album".to_owned(),
            title: "Album".to_owned(),
            artist: "Artist".to_owned(),
            year: Some("2024".to_owned()),
            tracks: vec![
                CachedAlbumTrack {
                    video_id: "vid-a".to_owned(),
                    title: "Alpha Theme".to_owned(),
                    artist: "Artist".to_owned(),
                    album: "Album".to_owned(),
                    track_number: Some(1),
                    duration_secs: Some(180),
                },
                CachedAlbumTrack {
                    video_id: "vid-b".to_owned(),
                    title: "Beta Groove".to_owned(),
                    artist: "Artist".to_owned(),
                    album: "Album".to_owned(),
                    track_number: Some(2),
                    duration_secs: Some(180),
                },
            ],
            updated_at: crate::signals::unix_now(),
        };
        let cfg = MatchConfig::default();
        let matched = apply_cached_album_matches(&mut cp, &cfg, &mut cache, &album, &[0, 1, 2]);
        assert_eq!(matched, 2);
        assert!(matches!(
            cp.tracks[0].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "vid-a"
        ));
        assert!(matches!(
            cp.tracks[1].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "vid-b"
        ));
        assert!(cp.tracks[2].outcome.is_none());
    }

    #[test]
    fn album_assignment_never_reuses_one_video_id_for_two_rows() {
        let mut cache = TransferMatchCache::default();
        let mut second = track_input("Repeated Song", "Artist", "Album", 1);
        second.disc_number = Some(2);
        second.source_key = "spotify:track:repeated-2".to_owned();
        let mut cp = Checkpoint::new(
            "job-album-unique".to_owned(),
            job_spec(),
            vec![
                TrackEntry {
                    input: track_input("Repeated Song", "Artist", "Album", 1),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: second,
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
            ],
        );
        let album = CachedAlbum {
            ytm_album_id: "ytm-album".to_owned(),
            title: "Album".to_owned(),
            artist: "Artist".to_owned(),
            year: Some("2024".to_owned()),
            tracks: vec![CachedAlbumTrack {
                video_id: "only-video".to_owned(),
                title: "Repeated Song".to_owned(),
                artist: "Artist".to_owned(),
                album: "Album".to_owned(),
                track_number: Some(1),
                duration_secs: Some(180),
            }],
            updated_at: crate::signals::unix_now(),
        };

        assert_eq!(
            apply_cached_album_matches(
                &mut cp,
                &MatchConfig::default(),
                &mut cache,
                &album,
                &[0, 1]
            ),
            1
        );
        assert_eq!(
            cp.tracks
                .iter()
                .filter(|entry| matches!(entry.outcome, Some(MatchOutcome::Matched { .. })))
                .count(),
            1
        );
    }

    #[test]
    fn repeated_track_numbers_across_discs_use_identity_and_duration() {
        let mut cache = TransferMatchCache::default();
        let mut disc_two = track_input("Disc Two Finale", "Artist", "Album", 1);
        disc_two.disc_number = Some(2);
        disc_two.duration_secs = Some(241);
        let mut cp = Checkpoint::new(
            "job-multi-disc".to_owned(),
            job_spec(),
            vec![
                TrackEntry {
                    input: track_input("Disc One Intro", "Artist", "Album", 1),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: disc_two,
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
            ],
        );
        let album = CachedAlbum {
            ytm_album_id: "ytm-multi-disc".to_owned(),
            title: "Album".to_owned(),
            artist: "Artist".to_owned(),
            year: Some("2024".to_owned()),
            tracks: vec![
                CachedAlbumTrack {
                    video_id: "disc-one".to_owned(),
                    title: "Disc One Intro".to_owned(),
                    artist: "Artist".to_owned(),
                    album: "Album".to_owned(),
                    track_number: Some(1),
                    duration_secs: Some(180),
                },
                CachedAlbumTrack {
                    video_id: "disc-two".to_owned(),
                    title: "Disc Two Finale".to_owned(),
                    artist: "Artist".to_owned(),
                    album: "Album".to_owned(),
                    track_number: Some(1),
                    duration_secs: Some(241),
                },
            ],
            updated_at: crate::signals::unix_now(),
        };

        assert_eq!(
            apply_cached_album_matches(
                &mut cp,
                &MatchConfig::default(),
                &mut cache,
                &album,
                &[0, 1]
            ),
            2
        );
        assert!(matches!(
            cp.tracks[0].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "disc-one"
        ));
        assert!(matches!(
            cp.tracks[1].outcome,
            Some(MatchOutcome::Matched { ref key, .. }) if key == "disc-two"
        ));
    }

    #[tokio::test]
    async fn unavailable_catalog_skips_live_album_calls_and_pacing() {
        let mut cp = Checkpoint::new(
            "job-anonymous".to_owned(),
            job_spec(),
            vec![
                TrackEntry {
                    input: track_input("Song A", "Artist", "Album", 1),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: track_input("Song B", "Artist", "Album", 2),
                    outcome: None,
                    review_decision: None,
                    written: false,
                },
            ],
        );
        let mut cache = TransferMatchCache::default();
        let mut pace = Pacing::new(std::time::Duration::from_secs(60));

        assert!(
            !prefill_album_matches(
                &mut cp,
                &YtMusicApi::Anonymous,
                &MatchConfig::default(),
                &SearchConfig::default(),
                &mut cache,
                false,
                &mut pace,
            )
            .await
            .unwrap()
        );
        assert_eq!(cp.match_stats.catalog_searches, 0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), pace.tick())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn unavailable_catalog_still_applies_cached_album() {
        let inputs = [
            track_input("Alpha Theme", "Artist", "Album", 1),
            track_input("Beta Groove", "Artist", "Album", 2),
        ];
        let mut cp = Checkpoint::new(
            "job-anonymous-cache".to_owned(),
            job_spec(),
            inputs
                .iter()
                .cloned()
                .map(|input| TrackEntry {
                    input,
                    outcome: None,
                    review_decision: None,
                    written: false,
                })
                .collect(),
        );
        let album = TransferAlbum {
            album_id: "ytm-album".to_owned(),
            title: "Album".to_owned(),
            artist: "Artist".to_owned(),
            year: Some("2024".to_owned()),
            tracks: vec![
                TransferAlbumTrack {
                    video_id: "video-a".to_owned(),
                    title: "Alpha Theme".to_owned(),
                    artist: "Artist".to_owned(),
                    album: "Album".to_owned(),
                    track_number: Some(1),
                    duration: "3:00".to_owned(),
                    duration_secs: Some(180),
                },
                TransferAlbumTrack {
                    video_id: "video-b".to_owned(),
                    title: "Beta Groove".to_owned(),
                    artist: "Artist".to_owned(),
                    album: "Album".to_owned(),
                    track_number: Some(2),
                    duration: "3:00".to_owned(),
                    duration_secs: Some(180),
                },
            ],
        };
        let mut cache = TransferMatchCache::default();
        cache.save_album(album_key(&inputs[0]).unwrap(), &album);
        let mut pace = Pacing::new(std::time::Duration::from_secs(60));

        assert!(
            prefill_album_matches(
                &mut cp,
                &YtMusicApi::Anonymous,
                &MatchConfig::default(),
                &SearchConfig::default(),
                &mut cache,
                true,
                &mut pace,
            )
            .await
            .unwrap()
        );
        assert_eq!(cp.match_stats.album_tracks_matched, 2);
        assert_eq!(cp.match_stats.catalog_searches, 0);
    }
}
