//! Fail-closed, ID-targeted Spotify playlist mirroring.

use std::collections::HashMap;

use anyhow::anyhow;

use super::*;
use crate::transfer::checkpoint::{SpotifySyncPreview, SpotifySyncState};

pub(super) async fn prepare(cp: &mut Checkpoint, ctx: &mut JobCtx) -> Result<(), JobError> {
    let target_id = match &cp.spec.dest {
        TransferDest::SpotifyMirrorPlaylist { id } => id.clone(),
        _ => return Ok(()),
    };
    let mut blocked_reasons = completeness_blockers(cp);
    let desired = desired_uris(cp);

    let spotify = ctx.spotify()?;
    let me = spotify.me().await.map_err(spotify_job_error)?;
    let before = spotify
        .playlist_meta(&target_id)
        .await
        .map_err(spotify_job_error)?;
    let mut on_page = |_done: u32, _total: u32| {};
    let current = spotify
        .playlist_item_refs(&target_id, &mut on_page)
        .await
        .map_err(spotify_job_error)?;
    let after = spotify
        .playlist_meta(&target_id)
        .await
        .map_err(spotify_job_error)?;

    if before.snapshot_id != after.snapshot_id {
        return Err(checkpointed(
            cp,
            JobError {
                resumable: true,
                error: anyhow!(
                    "Spotify playlist changed while building the sync preview; run resume to preview it again"
                ),
            },
        ));
    }
    if after.owner_id.as_deref() != Some(me.id.as_str()) {
        blocked_reasons
            .push("target playlist is not owned by the connected Spotify user".to_owned());
    }
    let snapshot_id = after.snapshot_id.clone().unwrap_or_default();
    if snapshot_id.is_empty() {
        blocked_reasons.push("target playlist did not return a snapshot id".to_owned());
    }

    let current_uris = current
        .iter()
        .map(|item| item.uri.clone())
        .collect::<Vec<_>>();
    let (additions, removals) = multiset_delta(&current_uris, &desired);
    let order_changed = !ordered_equal(&current_uris, &desired);
    let preview = SpotifySyncPreview {
        target_id: target_id.clone(),
        target_name: after.name.clone(),
        snapshot_id,
        current_items: u32::try_from(current_uris.len()).unwrap_or(u32::MAX),
        desired_items: u32::try_from(desired.len()).unwrap_or(u32::MAX),
        additions,
        removals,
        order_changed,
        ready: blocked_reasons.is_empty(),
        blocked_reasons,
    };
    let started = cp.spotify_sync.as_ref().is_some_and(|state| state.started);
    cp.dest_id = Some(target_id);
    cp.dest_name = Some(after.name);
    cp.spotify_sync = Some(SpotifySyncState { preview, started });
    cp.save().map_err(|error| JobError::fatal(error.into()))?;
    save_import_session(cp);
    Ok(())
}

pub(super) async fn write(
    cp: &mut Checkpoint,
    ctx: &mut JobCtx,
    writes: Vec<(usize, String)>,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
    report: &mut TransferReport,
) -> Result<(), JobError> {
    let state = cp
        .spotify_sync
        .clone()
        .ok_or_else(|| JobError::fatal(anyhow!("Spotify exact sync has no prepared preview")))?;
    if !state.preview.ready {
        let detail = state.preview.blocked_reasons.join("; ");
        return Err(checkpointed(
            cp,
            JobError {
                resumable: true,
                error: anyhow!("Spotify exact sync is blocked: {detail}"),
            },
        ));
    }
    if writes.len() != cp.tracks.len() {
        return Err(checkpointed(
            cp,
            JobError {
                resumable: true,
                error: anyhow!("Spotify exact sync requires every source row to be resolved"),
            },
        ));
    }
    let desired = writes
        .iter()
        .map(|(_, uri)| uri.clone())
        .collect::<Vec<_>>();

    // Every write pass, including recovery from a partially completed replacement, receives a
    // fresh preview first. Re-check that preview's authorization snapshot immediately before
    // either declaring a no-op or issuing the destructive PUT.
    let current = ctx
        .spotify()?
        .playlist_meta(&state.preview.target_id)
        .await
        .map_err(spotify_job_error)?;
    if current.snapshot_id.as_deref() != Some(state.preview.snapshot_id.as_str()) {
        return Err(checkpointed(
            cp,
            JobError {
                resumable: true,
                error: anyhow!(
                    "Spotify playlist changed after confirmation; run resume to generate and confirm a fresh preview"
                ),
            },
        ));
    }

    if !state.preview.order_changed {
        let mut on_page = |_done: u32, _total: u32| {};
        let actual = ctx
            .spotify()?
            .playlist_item_refs(&state.preview.target_id, &mut on_page)
            .await
            .map_err(|error| checkpointed(cp, spotify_job_error(error)))?
            .into_iter()
            .map(|item| item.uri)
            .collect::<Vec<_>>();
        if !ordered_equal(&actual, &desired) {
            return Err(checkpointed(
                cp,
                JobError {
                    resumable: true,
                    error: anyhow!(
                        "Spotify exact sync no-op verification failed; resume generates a fresh preview"
                    ),
                },
            ));
        }
        finish_verified(cp, &writes, report)?;
        return Ok(());
    }

    cp.spotify_sync.as_mut().expect("checked above").started = true;
    cp.save().map_err(|error| JobError::fatal(error.into()))?;
    save_import_session(cp);

    let first_len = desired.len().min(SPOTIFY_ADD_CHUNK);
    ctx.spotify()?
        .replace_playlist_items(&state.preview.target_id, &desired[..first_len])
        .await
        .map_err(|error| checkpointed(cp, spotify_job_error(error)))?;
    let tail = &desired[first_len..];
    for chunk in tail.chunks(SPOTIFY_ADD_CHUNK) {
        ctx.spotify()?
            .add_tracks(&state.preview.target_id, chunk)
            .await
            .map_err(|error| checkpointed(cp, spotify_job_error(error)))?;
    }

    let mut on_page = |_done: u32, _total: u32| {};
    let actual = ctx
        .spotify()?
        .playlist_item_refs(&state.preview.target_id, &mut on_page)
        .await
        .map_err(|error| checkpointed(cp, spotify_job_error(error)))?
        .into_iter()
        .map(|item| item.uri)
        .collect::<Vec<_>>();
    if !ordered_equal(&actual, &desired) {
        return Err(checkpointed(
            cp,
            JobError {
                resumable: true,
                error: anyhow!(
                    "Spotify exact sync verification failed; resume rebuilds the destination"
                ),
            },
        ));
    }

    finish_verified(cp, &writes, report)?;
    if let Some((idx, _)) = writes.last() {
        let counts = WriteProgressCounts {
            matched: u32::try_from(writes.len()).unwrap_or(u32::MAX),
            ambiguous: 0,
            not_found: 0,
            written: u32::try_from(writes.len()).unwrap_or(u32::MAX),
        };
        progress(progress_write(
            cp,
            counts.written,
            counts.written,
            *idx,
            report.auto_accepted,
            counts,
        ));
    }
    Ok(())
}

fn finish_verified(
    cp: &mut Checkpoint,
    writes: &[(usize, String)],
    report: &mut TransferReport,
) -> Result<(), JobError> {
    let mut indexes = Vec::with_capacity(writes.len());
    for (idx, _) in writes {
        cp.tracks[*idx].written = true;
        indexes.push(*idx);
    }
    append_write_journal(cp, &indexes)?;
    report.written = u32::try_from(writes.len()).unwrap_or(u32::MAX);
    Ok(())
}

fn desired_uris(cp: &Checkpoint) -> Vec<String> {
    cp.tracks
        .iter()
        .filter_map(|entry| effective_write_key(entry, cp.spec.take_best).map(str::to_owned))
        .collect()
}

fn completeness_blockers(cp: &Checkpoint) -> Vec<String> {
    let mut reasons = Vec::new();
    if cp.source_truncated {
        reasons.push("source exceeded the 10,000-track transfer limit".to_owned());
    }
    if cp.skipped_local > 0 {
        reasons.push(format!(
            "{} source items were not transferable",
            cp.skipped_local
        ));
    }
    if cp.defer_reason.is_some() {
        reasons.push("matching is deferred".to_owned());
    }
    let unresolved = cp
        .tracks
        .iter()
        .filter(|entry| effective_write_key(entry, cp.spec.take_best).is_none())
        .count();
    if unresolved > 0 {
        reasons.push(if unresolved == 1 {
            "1 source row is unresolved or rejected".to_owned()
        } else {
            format!("{unresolved} source rows are unresolved or rejected")
        });
    }
    reasons
}

fn ordered_equal(current: &[Option<String>], desired: &[String]) -> bool {
    current.len() == desired.len()
        && current
            .iter()
            .zip(desired)
            .all(|(left, right)| left.as_deref() == Some(right.as_str()))
}

fn multiset_delta(current: &[Option<String>], desired: &[String]) -> (u32, u32) {
    let mut counts: HashMap<&str, i64> = HashMap::new();
    let mut removals = current.iter().filter(|uri| uri.is_none()).count() as u32;
    for uri in current.iter().filter_map(Option::as_deref) {
        *counts.entry(uri).or_default() -= 1;
    }
    for uri in desired {
        *counts.entry(uri).or_default() += 1;
    }
    let additions = counts
        .values()
        .filter(|count| **count > 0)
        .map(|count| u32::try_from(*count).unwrap_or(u32::MAX))
        .fold(0u32, u32::saturating_add);
    removals = counts
        .values()
        .filter(|count| **count < 0)
        .map(|count| u32::try_from(-*count).unwrap_or(u32::MAX))
        .fold(removals, u32::saturating_add);
    (additions, removals)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(key: &str) -> TrackInput {
        TrackInput {
            title: key.to_owned(),
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
            duration_secs: Some(180),
            isrc: None,
            explicit: None,
            source_url: None,
            source_key: key.to_owned(),
            known_video_id: Some(key.to_owned()),
        }
    }

    fn matched_entry(source: &str, uri: &str) -> TrackEntry {
        TrackEntry {
            input: input(source),
            outcome: Some(MatchOutcome::Matched {
                key: uri.to_owned(),
                score: 0.95,
                display: source.to_owned(),
                title: None,
                artist: None,
                album: None,
                duration_secs: None,
                score_breakdown: None,
            }),
            review_decision: None,
            written: false,
        }
    }

    fn checkpoint(tracks: Vec<TrackEntry>) -> Checkpoint {
        Checkpoint::new(
            "yt2sp-sync-unit".to_owned(),
            JobSpec {
                source: TransferSource::LocalPlaylist {
                    key: "source".to_owned(),
                },
                dest: TransferDest::SpotifyMirrorPlaylist {
                    id: "2xvhzLfxmV7N8G2mWThBzG".to_owned(),
                },
                media_kind: ImportMediaKind::Track,
                dry_run: true,
                min_score: 0.8,
                take_best: false,
                auto_accept_ambiguous_min_score: None,
                match_policy: MatchPolicy::Strict,
                allow_user_videos: false,
                cache_mode: crate::transfer::TransferCacheMode::Use,
                rematch: false,
            },
            tracks,
        )
    }

    #[test]
    fn multiset_delta_counts_duplicates_and_unkeyed_rows() {
        let current = vec![
            Some("a".to_owned()),
            Some("a".to_owned()),
            Some("b".to_owned()),
            None,
        ];
        let desired = vec!["a".to_owned(), "b".to_owned(), "b".to_owned()];
        assert_eq!(multiset_delta(&current, &desired), (1, 2));
        assert!(!ordered_equal(&current, &desired));
    }

    #[test]
    fn ordered_equal_preserves_duplicate_positions() {
        let current = vec![
            Some("a".to_owned()),
            Some("b".to_owned()),
            Some("a".to_owned()),
        ];
        let same = vec!["a".to_owned(), "b".to_owned(), "a".to_owned()];
        let reordered = vec!["a".to_owned(), "a".to_owned(), "b".to_owned()];
        assert!(ordered_equal(&current, &same));
        assert!(!ordered_equal(&current, &reordered));
    }

    #[test]
    fn desired_sync_uris_preserve_source_order_and_duplicate_occurrences() {
        let cp = checkpoint(vec![
            matched_entry("first", "spotify:track:a"),
            matched_entry("duplicate", "spotify:track:a"),
            matched_entry("last", "spotify:track:b"),
        ]);

        assert_eq!(
            desired_uris(&cp),
            vec![
                "spotify:track:a".to_owned(),
                "spotify:track:a".to_owned(),
                "spotify:track:b".to_owned(),
            ]
        );
    }

    #[test]
    fn exact_sync_completeness_gate_collects_every_fail_closed_reason() {
        let mut unresolved = TrackEntry {
            input: input("missing"),
            outcome: Some(MatchOutcome::NotFound),
            review_decision: None,
            written: false,
        };
        unresolved.review_decision = Some(ReviewDecision::Rejected);
        let mut cp = checkpoint(vec![matched_entry("ok", "spotify:track:a"), unresolved]);
        cp.source_truncated = true;
        cp.skipped_local = 2;
        cp.defer_reason = Some(crate::transfer::checkpoint::JobDeferReason::new(
            "backend_unavailable",
            Some("spotify"),
            "retry later",
        ));

        let blockers = completeness_blockers(&cp);

        assert_eq!(blockers.len(), 4);
        assert!(blockers.iter().any(|reason| reason.contains("10,000")));
        assert!(
            blockers
                .iter()
                .any(|reason| reason.contains("2 source items"))
        );
        assert!(blockers.iter().any(|reason| reason.contains("deferred")));
        assert!(
            blockers
                .iter()
                .any(|reason| reason.contains("1 source row is"))
        );
    }
}
