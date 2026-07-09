use std::collections::HashMap;

use crate::api::ytmusic::{TransferAlbum, TransferAlbumCandidate, TransferAlbumTrack, YtMusicApi};
use crate::search_source::SearchConfig;
use crate::transfer::checkpoint::{Checkpoint, MatchStats};
use crate::transfer::match_cache::{CachedAlbum, TransferMatchCache, album_key};
use crate::transfer::matching::{
    CandidateSourceKind, MatchCandidate, MatchConfig, MatchOutcome, Pacing, TrackInput,
    YtmMatchDiagnostics, best_outcome, normalize, normalize_stripped, similarity,
};

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
        if entry.outcome.is_some() {
            continue;
        }
        if let Some(key) = album_key(&entry.input) {
            groups.entry(key).or_default().push(idx);
        }
    }

    let mut changed = false;
    for (key, indexes) in groups {
        if indexes.len() < 2 {
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

        let Some(query) = album_query(&cp.tracks[indexes[0]].input) else {
            continue;
        };
        pace.tick().await;
        cp.match_stats.catalog_searches += 1;
        let candidates = match api.search_transfer_albums(&query, search_config).await {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::debug!(
                    query = %query,
                    error = %crate::util::sanitize::sanitize_error_text(format!("{error:#}")),
                    "transfer album search failed"
                );
                continue;
            }
        };
        let Some(album_candidate) = best_album_candidate(&cp.tracks[indexes[0]].input, &candidates)
        else {
            continue;
        };
        pace.tick().await;
        let album = match api.transfer_album_tracks(&album_candidate.album_id).await {
            Ok(Some(album)) => album,
            Ok(None) => continue,
            Err(error) => {
                tracing::debug!(
                    album_id = %album_candidate.album_id,
                    error = %crate::util::sanitize::sanitize_error_text(format!("{error:#}")),
                    "transfer album detail fetch failed"
                );
                continue;
            }
        };
        cache.save_album(key, &album);
        changed = true;
        let matched = apply_transfer_album_matches(cp, cfg, cache, &album, &indexes);
        if matched > 0 {
            cp.match_stats.album_groups_matched += 1;
            changed = true;
        }
    }
    Ok(changed)
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
            source_kind: CandidateSourceKind::YtmAlbumTrack,
            channel: Some(track.artist.clone()),
            isrc: None,
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
        .map(album_track_candidate)
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
    let mut matched = 0;
    for idx in indexes {
        if cp.tracks[*idx].outcome.is_some() {
            continue;
        }
        let input = cp.tracks[*idx].input.clone();
        let outcome = best_outcome(&input, candidates, cfg);
        if matches!(outcome, MatchOutcome::Matched { .. }) {
            cp.match_stats.album_tracks_matched += 1;
            record_match_outcome_stats(&mut cp.match_stats, &outcome);
            cache.save_match(&input, &outcome);
            cp.tracks[*idx].outcome = Some(outcome);
            matched += 1;
        }
    }
    matched
}

fn album_track_candidate(track: &TransferAlbumTrack) -> MatchCandidate {
    MatchCandidate {
        key: track.video_id.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: Some(track.album.clone()),
        duration_secs: track
            .duration_secs
            .or_else(|| crate::streaming::candidate::parse_duration_secs(track.duration.as_str())),
        track_number: track.track_number,
        source_kind: CandidateSourceKind::YtmAlbumTrack,
        channel: Some(track.artist.clone()),
        isrc: None,
        preflighted: false,
        preflight_reject_reason: None,
        preflight_reason_codes: Vec::new(),
    }
}

fn album_query(input: &TrackInput) -> Option<String> {
    let album = input.album.as_deref()?.trim();
    if album.is_empty() {
        return None;
    }
    let artist = input
        .album_artists
        .first()
        .or_else(|| input.artists.first())
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty())?;
    let mut query = format!("{artist} {album}");
    if let Some(year) = input
        .album_release_date
        .as_deref()
        .and_then(|date| date.get(0..4))
        .filter(|year| year.bytes().all(|b| b.is_ascii_digit()))
    {
        query.push(' ');
        query.push_str(year);
    }
    Some(query)
}

fn best_album_candidate(
    input: &TrackInput,
    candidates: &[TransferAlbumCandidate],
) -> Option<TransferAlbumCandidate> {
    let mut scored = candidates
        .iter()
        .map(|candidate| (album_candidate_score(input, candidate), candidate))
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let (score, best) = scored.first()?;
    if *score < 0.82 {
        return None;
    }
    if let Some((second, _)) = scored.get(1)
        && score - second < 0.04
    {
        return None;
    }
    Some((*best).clone())
}

fn album_candidate_score(input: &TrackInput, candidate: &TransferAlbumCandidate) -> f32 {
    let input_album = input.album.as_deref().unwrap_or_default();
    let title = similarity(
        &normalize_stripped(input_album),
        &normalize_stripped(&candidate.title),
    );
    let input_artist = input
        .album_artists
        .first()
        .or_else(|| input.artists.first())
        .map(|artist| normalize(artist))
        .unwrap_or_default();
    let candidate_artist = normalize(&candidate.artist);
    let artist = if !input_artist.is_empty() && !candidate_artist.is_empty() {
        if input_artist.contains(&candidate_artist) || candidate_artist.contains(&input_artist) {
            1.0
        } else {
            similarity(&input_artist, &candidate_artist)
        }
    } else {
        0.5
    };
    let year = match (
        input
            .album_release_date
            .as_deref()
            .and_then(|date| date.get(0..4)),
        candidate.year.as_deref(),
    ) {
        (Some(a), Some(b)) if a == b => 1.0,
        (Some(_), Some(_)) => 0.4,
        _ => 0.6,
    };
    let type_bonus = match (input.album_type.as_deref(), candidate.album_type.as_deref()) {
        (Some(a), Some(b)) if normalize(a) == normalize(b) => 0.03,
        _ => 0.0,
    };
    0.60 * title + 0.30 * artist + 0.10 * year + type_bonus
}

pub(super) fn merge_ytm_diagnostics(stats: &mut MatchStats, diagnostics: YtmMatchDiagnostics) {
    stats.catalog_searches += diagnostics.catalog_searches;
    stats.video_searches += diagnostics.video_searches;
    stats.preflight_lookups += diagnostics.preflight_lookups;
    stats.authenticated_catalog_degraded += diagnostics.authenticated_catalog_degraded;
    stats.query_cache_hits += diagnostics.query_cache_hits;
    stats.video_meta_cache_hits += diagnostics.video_meta_cache_hits;
}

pub(super) fn record_match_outcome_stats(stats: &mut MatchStats, outcome: &MatchOutcome) {
    match outcome {
        MatchOutcome::Matched {
            score_breakdown: Some(score),
            ..
        } => record_score_stats(stats, score),
        MatchOutcome::Ambiguous { candidates } => {
            for candidate in candidates {
                if let Some(score) = &candidate.score_breakdown {
                    record_score_stats(stats, score);
                }
            }
        }
        _ => {}
    }
}

fn record_score_stats(
    stats: &mut MatchStats,
    score: &crate::transfer::matching::MatchScoreBreakdown,
) {
    stats.bump_source_kind(&score.source_kind);
    stats.bump_quality_tier(&score.quality_tier);
    for code in &score.reason_codes {
        stats.bump_reason_code(code);
    }
    if let Some(reason) = &score.reject_reason {
        stats.bump_reason_code(reason);
    }
}
