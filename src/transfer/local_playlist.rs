//! Owner-neutral local-playlist patch planning and storage adapters.
//!
//! Transfer workers describe the rows they want to append. The active application owner applies
//! that patch to its current store and decides how durability is confirmed; the CLI keeps the
//! same direct, atomic on-disk behavior through [`DiskLocalPlaylistStore`].

use std::fmt;

use crate::api::Song;
use crate::playlists::{AddResult, Playlists};

/// One checkpoint row and the playable song it wants to append.
#[derive(Debug, Clone)]
pub(crate) struct LocalPlaylistPatchRow {
    pub(crate) checkpoint_index: usize,
    pub(crate) song: Song,
}

/// A create-or-append request which can be safely reapplied to a newer owner revision.
#[derive(Debug, Clone)]
pub(crate) struct LocalPlaylistPatch {
    pub(crate) observed_revision: u64,
    pub(crate) destination_id: Option<String>,
    pub(crate) destination_name: String,
    pub(crate) rows: Vec<LocalPlaylistPatchRow>,
}

/// Exact result for one checkpoint row after the destination commit became durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalPlaylistRowOutcome {
    pub(crate) checkpoint_index: usize,
    pub(crate) result: AddResult,
}

/// Exact destination and row outcomes returned to the transfer checkpoint owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalPlaylistWriteOutcome {
    pub(crate) destination_id: String,
    pub(crate) rows: Vec<LocalPlaylistRowOutcome>,
    pub(crate) rebased: bool,
}

/// Pure result of applying a patch to an immutable owner snapshot.
pub(crate) struct LocalPlaylistApplyPlan {
    pub(crate) candidate: Playlists,
    pub(crate) outcome: LocalPlaylistWriteOutcome,
}

/// Typed request sent from a transfer worker to the active playlist owner.
#[derive(Debug, Clone)]
pub(crate) enum LocalPlaylistOwnerRequest {
    Snapshot,
    Apply(LocalPlaylistPatch),
}

/// Typed owner reply. Apply replies are sent only after the candidate is durable.
#[derive(Debug)]
pub(crate) enum LocalPlaylistOwnerReply {
    Snapshot(Playlists),
    Applied(LocalPlaylistWriteOutcome),
}

/// Storage-boundary failure with an explicit checkpoint-resume policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalPlaylistStoreError {
    message: String,
    resumable: bool,
}

impl LocalPlaylistStoreError {
    pub(crate) fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            resumable: false,
        }
    }

    pub(crate) fn resumable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            resumable: true,
        }
    }

    pub(crate) fn is_resumable(&self) -> bool {
        self.resumable
    }
}

impl fmt::Display for LocalPlaylistStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for LocalPlaylistStoreError {}

/// Apply a patch to `current` without mutating it.
///
/// Destination resolution is id-first and then case-insensitive name, matching the historical
/// transfer behavior. Every row result stays correlated with its checkpoint index.
pub(crate) fn plan_apply_local_playlist_patch(
    current: &Playlists,
    patch: &LocalPlaylistPatch,
) -> Result<LocalPlaylistApplyPlan, LocalPlaylistStoreError> {
    let mut candidate = current.clone();
    let existing_id = patch
        .destination_id
        .as_deref()
        .and_then(|id| candidate.find(id))
        .or_else(|| candidate.find(&patch.destination_name))
        .map(|playlist| playlist.id.clone());
    let destination_id = match existing_id {
        Some(id) => id,
        None => candidate.create(&patch.destination_name).ok_or_else(|| {
            LocalPlaylistStoreError::fatal(format!(
                "could not create the local playlist `{}`",
                patch.destination_name
            ))
        })?,
    };
    let results = candidate.add_many(
        &destination_id,
        patch.rows.iter().map(|row| row.song.clone()).collect(),
    );
    if results.contains(&AddResult::NotFound) {
        return Err(LocalPlaylistStoreError::fatal(format!(
            "local playlist `{}` vanished while applying its patch",
            patch.destination_name
        )));
    }
    let rows = patch
        .rows
        .iter()
        .zip(results)
        .map(|(row, result)| LocalPlaylistRowOutcome {
            checkpoint_index: row.checkpoint_index,
            result,
        })
        .collect();
    Ok(LocalPlaylistApplyPlan {
        candidate,
        outcome: LocalPlaylistWriteOutcome {
            destination_id,
            rows,
            rebased: current.revision() != patch.observed_revision,
        },
    })
}

/// Minimal store boundary needed by the transfer engine.
pub(crate) trait LocalPlaylistStore: Send {
    async fn snapshot(&mut self) -> Result<Playlists, LocalPlaylistStoreError>;

    async fn apply(
        &mut self,
        patch: LocalPlaylistPatch,
    ) -> Result<LocalPlaylistWriteOutcome, LocalPlaylistStoreError>;
}

/// CLI adapter retaining the historical atomic `playlists.json` commit.
#[derive(Default)]
pub(crate) struct DiskLocalPlaylistStore;

impl LocalPlaylistStore for DiskLocalPlaylistStore {
    async fn snapshot(&mut self) -> Result<Playlists, LocalPlaylistStoreError> {
        Ok(Playlists::load())
    }

    async fn apply(
        &mut self,
        patch: LocalPlaylistPatch,
    ) -> Result<LocalPlaylistWriteOutcome, LocalPlaylistStoreError> {
        // Reload at commit time so a long-running CLI match rebases instead of writing its stale
        // start-of-job snapshot over another owner's newer rows.
        let current = Playlists::load();
        let plan = plan_apply_local_playlist_patch(&current, &patch)?;
        plan.candidate.save().map_err(|error| {
            // Preserve the CLI's existing non-resumable save-failure exit behavior.
            LocalPlaylistStoreError::fatal(format!("saving playlists.json: {error}"))
        })?;
        Ok(plan.outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(id: &str) -> Song {
        Song::remote(id, format!("Track {id}"), "Artist", "3:00")
    }

    fn patch(current: &Playlists, ids: &[&str]) -> LocalPlaylistPatch {
        LocalPlaylistPatch {
            observed_revision: current.revision(),
            destination_id: None,
            destination_name: "Transfer".to_owned(),
            rows: ids
                .iter()
                .enumerate()
                .map(|(checkpoint_index, id)| LocalPlaylistPatchRow {
                    checkpoint_index,
                    song: song(id),
                })
                .collect(),
        }
    }

    #[test]
    fn planner_reports_added_then_duplicate_without_mutating_input() {
        let current = Playlists::default();
        let first = plan_apply_local_playlist_patch(&current, &patch(&current, &["a"]))
            .expect("first patch");
        assert!(current.find("Transfer").is_none());
        assert_eq!(first.outcome.rows[0].result, AddResult::Added);

        let duplicate =
            plan_apply_local_playlist_patch(&first.candidate, &patch(&first.candidate, &["a"]))
                .expect("duplicate patch");
        assert_eq!(
            duplicate.outcome.destination_id,
            first.outcome.destination_id
        );
        assert_eq!(duplicate.outcome.rows[0].result, AddResult::Duplicate);
    }

    #[test]
    fn planner_reports_full_at_the_store_boundary() {
        let mut current = Playlists::default();
        let id = current.create("Transfer").expect("playlist");
        for index in 0..crate::playlists::SONGS_PER_PLAYLIST_MAX {
            assert_eq!(
                current.add(&id, song(&format!("existing-{index}"))),
                AddResult::Added
            );
        }
        let plan = plan_apply_local_playlist_patch(&current, &patch(&current, &["overflow"]))
            .expect("capacity is a row outcome");
        assert_eq!(plan.outcome.rows[0].result, AddResult::Full);
    }

    #[test]
    fn planner_rebases_on_current_owner_state_without_clobbering_rows() {
        let initial = Playlists::default();
        let stale_patch = patch(&initial, &["transfer-row"]);
        let mut current = initial;
        let id = current.create("Transfer").expect("concurrent playlist");
        assert_eq!(current.add(&id, song("owner-row")), AddResult::Added);

        let plan = plan_apply_local_playlist_patch(&current, &stale_patch).expect("rebased patch");
        let playlist = plan.candidate.find(&id).expect("same destination");
        assert!(plan.outcome.rebased);
        assert_eq!(playlist.songs.len(), 2);
        assert!(
            playlist
                .songs
                .iter()
                .any(|song| song.video_id == "owner-row")
        );
        assert!(
            playlist
                .songs
                .iter()
                .any(|song| song.video_id == "transfer-row")
        );
    }
}
