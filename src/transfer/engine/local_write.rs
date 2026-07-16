//! Local-playlist patch construction and post-durability checkpoint settlement.

use super::*;

/// Hand a create-or-append patch to the active playlist owner. The returned row outcomes are
/// authoritative only after that owner has durably committed its candidate, so checkpoint rows
/// are never advanced ahead of `playlists.json`.
pub(super) async fn write_local(
    cp: &mut Checkpoint,
    writes: Vec<(usize, String)>,
    observed_revision: u64,
    local_playlists: &mut impl LocalPlaylistStore,
    progress: &mut (dyn FnMut(TransferProgress) + Send),
    report: &mut TransferReport,
) -> Result<(), JobError> {
    let name = cp
        .dest_name
        .clone()
        .unwrap_or_else(|| "Imported playlist".to_owned());
    let pending: Vec<(usize, String)> = writes
        .into_iter()
        .filter(|(idx, _)| !cp.tracks[*idx].written)
        .collect();
    let total = pending.len() as u32;
    let mut done = 0u32;
    let mut progress_counts = WriteProgressCounts::from_checkpoint(cp);
    let rows = pending
        .into_iter()
        .map(|(idx, video_id)| LocalPlaylistPatchRow {
            checkpoint_index: idx,
            song: song_for_entry(&cp.tracks[idx], &video_id, &cp.job_id, idx as u32 + 1),
        })
        .collect();
    let outcome = local_playlists
        .apply(LocalPlaylistPatch {
            observed_revision,
            destination_id: cp.dest_id.clone(),
            destination_name: name.clone(),
            rows,
        })
        .await
        .map_err(local_playlist_job_error)?;
    cp.dest_id = Some(outcome.destination_id);
    for row in outcome.rows {
        let idx = row.checkpoint_index;
        match row.result {
            crate::playlists::AddResult::Added => {
                cp.tracks[idx].written = true;
                report.written += 1;
            }
            crate::playlists::AddResult::Duplicate => cp.tracks[idx].written = true,
            crate::playlists::AddResult::Full => {
                tracing::warn!("local playlist `{name}` is full; track skipped at capacity");
                cp.tracks[idx].outcome = Some(MatchOutcome::SkippedCapacity);
                cp.tracks[idx].review_decision = None;
                record_capacity_skips(&mut cp.match_stats, 1);
            }
            crate::playlists::AddResult::NotFound => {
                return Err(JobError::fatal(anyhow!(
                    "local playlist `{name}` vanished mid-write"
                )));
            }
        }
        progress_counts.record_written(1);
        done += 1;
        progress(progress_write(
            cp,
            done,
            total,
            idx,
            report.auto_accepted,
            progress_counts,
        ));
    }
    // The owner commit above is durable. Advancing the checkpoint afterwards makes a crash in
    // this narrow gap resume through the store's Duplicate result rather than losing a row.
    cp.save().map_err(checkpoint_after_local_commit_error)?;
    Ok(())
}

fn checkpoint_after_local_commit_error(error: std::io::Error) -> JobError {
    JobError {
        resumable: true,
        error: anyhow!(
            "local playlist is durable but its transfer checkpoint was not saved: {error}"
        ),
    }
}

/// Reconstruct a playable `Song` for a write target. The candidate supplies the playable key;
/// the source input supplies canonical music metadata for Local Deck grouping and ordering.
pub(super) fn song_for_entry(
    entry: &TrackEntry,
    video_id: &str,
    job_id: &str,
    source_order: u32,
) -> Song {
    let (matched_title, matched_artist, matched_album, matched_duration_secs) = match &entry.outcome
    {
        Some(MatchOutcome::Matched {
            title,
            artist,
            album,
            duration_secs,
            ..
        }) => (title.clone(), artist.clone(), album.clone(), *duration_secs),
        _ => (None, None, None, None),
    };
    let title = if entry.input.title.trim().is_empty() {
        matched_title.unwrap_or_default()
    } else {
        entry.input.title.clone()
    };
    let artist = if entry.input.artists.is_empty() {
        matched_artist.unwrap_or_default()
    } else {
        entry.input.artists.join(", ")
    };
    let album = entry.input.album.clone().or(matched_album);
    let duration_secs = entry.input.duration_secs.or(matched_duration_secs);
    let duration = duration_secs
        .map(|seconds| crate::util::format::time(f64::from(seconds)))
        .unwrap_or_default();
    let mut song = Song::from_search(video_id, title, artist, duration, album);
    song.duration_secs = duration_secs;
    let album_artist =
        (!entry.input.album_artists.is_empty()).then(|| entry.input.album_artists.join(", "));
    song.with_catalog_metadata(
        album_artist,
        entry.input.disc_number,
        entry.input.track_number,
        entry.input.isrc.clone(),
        Some(entry.input.source_key.clone()),
        entry.input.source_url.clone(),
    )
    .with_import_metadata(SongImportMetadata {
        artists: entry.input.artists.clone(),
        album_artists: entry.input.album_artists.clone(),
        album_release_date: entry.input.album_release_date.clone(),
        album_release_date_precision: entry.input.album_release_date_precision.clone(),
        album_total_tracks: entry.input.album_total_tracks,
        album_type: entry.input.album_type.clone(),
        album_art_url: entry.input.album_art_url.clone(),
        explicit: entry.input.explicit,
    })
    .with_import_session(Some(job_id.to_owned()), Some(source_order))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::local_playlist::{LocalPlaylistRowOutcome, LocalPlaylistWriteOutcome};

    struct FakeStore {
        result: Result<LocalPlaylistWriteOutcome, LocalPlaylistStoreError>,
    }

    impl LocalPlaylistStore for FakeStore {
        async fn snapshot(
            &mut self,
        ) -> Result<crate::playlists::Playlists, LocalPlaylistStoreError> {
            Ok(crate::playlists::Playlists::default())
        }

        async fn apply(
            &mut self,
            _patch: LocalPlaylistPatch,
        ) -> Result<LocalPlaylistWriteOutcome, LocalPlaylistStoreError> {
            self.result.clone()
        }
    }

    fn spec() -> JobSpec {
        JobSpec {
            source: TransferSource::SpotifyLiked,
            dest: TransferDest::LocalPlaylist {
                name: Some("Transfer".to_owned()),
            },
            media_kind: ImportMediaKind::Track,
            dry_run: false,
            min_score: 0.8,
            take_best: false,
            auto_accept_ambiguous_min_score: None,
            match_policy: MatchPolicy::Strict,
            allow_user_videos: false,
            cache_mode: crate::transfer::TransferCacheMode::Use,
            rematch: false,
        }
    }

    fn track(index: usize) -> TrackEntry {
        TrackEntry {
            input: TrackInput {
                title: format!("Track {index}"),
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
                duration_secs: Some(60),
                isrc: None,
                explicit: None,
                source_url: None,
                source_key: format!("source-{index}"),
                known_video_id: None,
            },
            outcome: Some(MatchOutcome::Matched {
                key: format!("video-{index}"),
                score: 1.0,
                display: format!("Track {index}"),
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

    fn checkpoint(count: usize) -> Checkpoint {
        let mut checkpoint = Checkpoint::new(
            "local_write_test_no_disk".to_owned(),
            spec(),
            (0..count).map(track).collect(),
        );
        checkpoint.dest_name = Some("Transfer".to_owned());
        checkpoint
    }

    #[tokio::test]
    async fn owner_failure_leaves_checkpoint_rows_and_report_untouched() {
        let mut checkpoint = checkpoint(1);
        let mut store = FakeStore {
            result: Err(LocalPlaylistStoreError::resumable("owner unavailable")),
        };
        let mut report = TransferReport::default();
        let mut progress = Vec::new();

        let error = write_local(
            &mut checkpoint,
            vec![(0, "video-0".to_owned())],
            0,
            &mut store,
            &mut |item| progress.push(item),
            &mut report,
        )
        .await
        .expect_err("owner failure");

        assert!(error.resumable);
        assert!(!checkpoint.tracks[0].written);
        assert!(checkpoint.dest_id.is_none());
        assert_eq!(report.written, 0);
        assert!(progress.is_empty());
    }

    #[tokio::test]
    async fn exact_owner_outcomes_advance_checkpoint_only_after_reply() {
        let mut checkpoint = checkpoint(3);
        let mut store = FakeStore {
            result: Ok(LocalPlaylistWriteOutcome {
                destination_id: "transfer".to_owned(),
                rows: vec![
                    LocalPlaylistRowOutcome {
                        checkpoint_index: 0,
                        result: crate::playlists::AddResult::Added,
                    },
                    LocalPlaylistRowOutcome {
                        checkpoint_index: 1,
                        // A crash after the owner commit and before checkpoint save resumes here.
                        result: crate::playlists::AddResult::Duplicate,
                    },
                    LocalPlaylistRowOutcome {
                        checkpoint_index: 2,
                        result: crate::playlists::AddResult::Full,
                    },
                ],
                rebased: true,
            }),
        };
        let mut report = TransferReport::default();
        let mut progress = Vec::new();

        write_local(
            &mut checkpoint,
            vec![
                (0, "video-0".to_owned()),
                (1, "video-1".to_owned()),
                (2, "video-2".to_owned()),
            ],
            0,
            &mut store,
            &mut |item| progress.push(item),
            &mut report,
        )
        .await
        .unwrap_or_else(|error| panic!("durable owner reply failed: {}", error.error));

        assert_eq!(checkpoint.dest_id.as_deref(), Some("transfer"));
        assert!(checkpoint.tracks[0].written);
        assert!(checkpoint.tracks[1].written);
        assert!(!checkpoint.tracks[2].written);
        assert!(matches!(
            checkpoint.tracks[2].outcome,
            Some(MatchOutcome::SkippedCapacity)
        ));
        assert_eq!(checkpoint.match_stats.capacity_skipped, 1);
        assert_eq!(report.written, 1);
        assert_eq!(progress.len(), 3);
    }

    #[test]
    fn checkpoint_failure_after_owner_commit_is_resumable() {
        let error = checkpoint_after_local_commit_error(std::io::Error::other("injected"));
        assert!(error.resumable);
        assert!(error.error.to_string().contains("playlist is durable"));
    }
}
