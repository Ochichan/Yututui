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
            best_album_candidate(&input, &[clear.clone()]).map(|c| c.album_id),
            Some("album-1".to_owned())
        );

        let weak = TransferAlbumCandidate {
            album_id: "album-weak".to_owned(),
            title: "Totally Different".to_owned(),
            artist: "Someone Else".to_owned(),
            year: Some("1999".to_owned()),
            album_type: None,
        };
        assert!(best_album_candidate(&input, &[weak]).is_none());

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
        assert!(best_album_candidate(&input, &[twin_a, twin_b]).is_none());
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
}
