use super::*;

impl ImportSession {
    pub fn from_checkpoint(cp: &Checkpoint) -> Self {
        Self::from_checkpoint_preserving(cp, None)
    }

    /// Project checkpoint-owned fields without erasing durable download/organize state.
    fn from_checkpoint_preserving(cp: &Checkpoint, existing: Option<&Self>) -> Self {
        let searched_ytm = !matches!(
            cp.spec.dest,
            TransferDest::SpotifyNewPlaylist { .. } | TransferDest::SpotifyMirrorPlaylist { .. }
        );
        let mut rows: Vec<ImportSessionRow> = cp
            .tracks
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                row_from_input(
                    idx,
                    &entry.input,
                    entry.outcome.as_ref(),
                    entry.review_decision.clone(),
                    entry.written,
                    searched_ytm,
                    cp.match_traces.get(&(idx as u32 + 1)),
                )
            })
            .collect();
        if let Some(existing) = existing {
            for row in &mut rows {
                let Some(previous) = existing.rows.iter().find(|previous| {
                    previous.source_order == row.source_order && previous.row_id == row.row_id
                }) else {
                    continue;
                };
                let target_changed = row_download_target(row) != row_download_target(previous);
                let has_local_artifact = super::claim::row_has_artifact_ownership(previous);
                row.revision = previous.revision.max(1);
                if target_changed && !has_local_artifact && previous.download_claim.is_none() {
                    row.revision = row.revision.saturating_add(1);
                }
                if has_local_artifact || previous.download_claim.is_some() {
                    preserve_review_identity(row, previous);
                }
                row.written = previous.written;
                row.local_path.clone_from(&previous.local_path);
                row.artifact_receipt.clone_from(&previous.artifact_receipt);
                row.download_claim.clone_from(&previous.download_claim);
                row.errors.clone_from(&previous.errors);
            }
        }
        let counts = ImportSessionCounts::from_rows(&rows);
        let mut destination = endpoint_from_dest(&cp.spec.dest, cp.dest_name.clone());
        if matches!(cp.spec.dest, TransferDest::LocalPlaylist { .. }) {
            destination.key = cp.dest_id.clone();
        }
        Self {
            schema_version: IMPORT_SESSION_SCHEMA_VERSION,
            session_id: cp.job_id.clone(),
            session_instance_id: existing
                .map(|session| session.session_instance_id.clone())
                .filter(|id| !id.is_empty())
                .unwrap_or_else(super::claim::new_durable_id),
            job_id: cp.job_id.clone(),
            created_at: cp.created_at,
            updated_at: cp.updated_at,
            stage: cp.stage,
            source: endpoint_from_source(&cp.spec.source, cp.source_name.clone()),
            destination,
            counts,
            defer_reason: cp.defer_reason.clone(),
            rows,
        }
    }

    /// Save a checkpoint projection while preserving fields owned by the session/artifact path.
    /// The caller must hold the import-record guard.
    pub(crate) fn save_checkpoint_projection_unlocked(cp: &Checkpoint) -> anyhow::Result<()> {
        let existing = session_path(&cp.job_id)
            .filter(|path| path.exists())
            .map(|_| Self::load(&cp.job_id))
            .transpose()?;
        Self::from_checkpoint_preserving(cp, existing.as_ref())
            .save_unlocked()
            .map_err(Into::into)
    }
}

fn row_download_target(row: &ImportSessionRow) -> (&ImportSessionRowStatus, Option<&str>) {
    (&row.status, row.selected_key.as_deref())
}

fn preserve_review_identity(row: &mut ImportSessionRow, previous: &ImportSessionRow) {
    row.status = previous.status;
    row.selected_key.clone_from(&previous.selected_key);
    row.selected_score.clone_from(&previous.selected_score);
    row.selected_display.clone_from(&previous.selected_display);
    row.review_decision.clone_from(&previous.review_decision);
    row.candidates.clone_from(&previous.candidates);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::artifact_identity::{ArtifactFileIdentity, test_import_claim};
    use crate::transfer::checkpoint::TrackEntry;
    use crate::transfer::{
        ImportMediaKind, JobSpec, MatchPolicy, TransferCacheMode, TransferDest, TransferSource,
    };

    fn spec() -> JobSpec {
        JobSpec {
            source: TransferSource::SpotifyLiked,
            dest: TransferDest::LocalPlaylist { name: None },
            media_kind: ImportMediaKind::Track,
            dry_run: false,
            min_score: 0.8,
            take_best: false,
            auto_accept_ambiguous_min_score: None,
            match_policy: MatchPolicy::Strict,
            allow_user_videos: false,
            cache_mode: TransferCacheMode::Use,
            rematch: false,
        }
    }

    fn input(order: u32) -> TrackInput {
        TrackInput {
            title: format!("Track {order}"),
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
            source_key: format!("spotify:track:{order}"),
            known_video_id: None,
        }
    }

    fn matched(key: &str) -> MatchOutcome {
        MatchOutcome::Matched {
            key: key.to_owned(),
            score: 0.9,
            display: key.to_owned(),
            title: None,
            artist: None,
            album: None,
            duration_secs: None,
            score_breakdown: None,
        }
    }

    #[test]
    fn delayed_checkpoint_projection_preserves_claimed_and_written_row_identity() {
        let mut cp = Checkpoint::new(
            "sp2yt-projection-fence".to_owned(),
            spec(),
            vec![
                TrackEntry {
                    input: input(1),
                    outcome: Some(matched("video-a")),
                    review_decision: None,
                    written: false,
                },
                TrackEntry {
                    input: input(2),
                    outcome: Some(matched("video-c")),
                    review_decision: None,
                    written: false,
                },
            ],
        );
        let mut existing = ImportSession::from_checkpoint(&cp);
        let claim = test_import_claim(&cp.job_id, "row-00001", 1, "video-a");
        existing.session_instance_id = claim.session_instance_id.clone();
        existing.rows[0].download_claim = Some(claim.clone());
        existing.rows[1].written = true;
        existing.rows[1].local_path = Some(PathBuf::from("/tmp/projection-c.m4a"));
        existing.rows[1].artifact_receipt = Some(ArtifactReceipt {
            audio: ArtifactFileIdentity {
                len: 7,
                sha256: "a".repeat(64),
            },
            sidecar_required: false,
            sidecar: None,
            claim: None,
        });

        cp.tracks[0].outcome = Some(matched("video-b"));
        cp.tracks[0].review_decision = Some(ReviewDecision::Rejected);
        cp.tracks[1].outcome = Some(matched("video-d"));
        cp.tracks[1].review_decision = Some(ReviewDecision::Skipped);
        let projected = ImportSession::from_checkpoint_preserving(&cp, Some(&existing));

        assert_eq!(projected.session_instance_id, existing.session_instance_id);
        assert_eq!(projected.rows[0].selected_key.as_deref(), Some("video-a"));
        assert_eq!(projected.rows[0].download_claim.as_ref(), Some(&claim));
        assert!(projected.rows[0].review_decision.is_none());
        assert_eq!(projected.rows[1].selected_key.as_deref(), Some("video-c"));
        assert!(projected.rows[1].written);
        assert_eq!(projected.rows[1].local_path, existing.rows[1].local_path);
        assert_eq!(
            projected.rows[1].artifact_receipt,
            existing.rows[1].artifact_receipt
        );
    }
}
