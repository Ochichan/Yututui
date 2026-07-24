use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::legacy::{
    ENGAGEMENT_EVENTS_MAX, FAVORITES_MAX, HISTORY_MAX, LegacyPlaylist, LegacyProjection,
    PLAYLIST_ENTRIES_MAX, PLAYLISTS_MAX, RADIO_MAX, SIGNAL_TRACKS_MAX, stable_hash,
};
use super::{
    CausalStamp, DeviceId, Dot, Operation, OperationEnvelope, OperationOrigin, PersonalStateError,
    PersonalStateV2, PortableTrack, PortableTrackKey, VersionVector, merge, project,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub operations_added: usize,
    pub duplicate_operations: usize,
    pub favorites_added: usize,
    pub history_added: usize,
    pub radio_favorites_added: usize,
    pub playlists_added: usize,
    pub playlist_entries_added: usize,
    pub signal_tracks_added: usize,
    pub changed: bool,
}

#[derive(Debug, Clone)]
pub struct ImportPlan {
    pub candidate: PersonalStateV2,
    pub summary: ImportSummary,
}

/// Plan an import without mutating either input.
///
/// Bundles from the same dataset use the ordinary causal merge. A foreign dataset or legacy
/// export becomes one deterministic, deletion-free legacy baseline operation in the current
/// dataset. The imported baseline observes prior baselines, but not later local edits; this keeps
/// explicit local changes authoritative while retaining every positive item from both baselines.
pub fn plan_import(
    current: &PersonalStateV2,
    imported: &PersonalStateV2,
) -> Result<ImportPlan, PersonalStateError> {
    current.validate()?;
    imported.validate()?;
    let before = project(current)?;

    let (mut candidate, operations_added, duplicate_operations) =
        if current.dataset_id == imported.dataset_id {
            let (candidate, merge_summary) = merge(current, imported)?;
            (
                candidate,
                merge_summary.added_operations,
                merge_summary.duplicate_operations,
            )
        } else {
            let imported_projection = project(imported)?.legacy;
            rewrite_foreign_projection(current, imported_projection)?
        };
    let changed = candidate.operations != current.operations
        || candidate.device_registry != current.device_registry
        || candidate.version_vector != current.version_vector
        || candidate.compaction_checkpoint != current.compaction_checkpoint;
    if changed {
        candidate.revision = candidate.revision.max(current.revision).saturating_add(1);
        candidate.projection_fingerprint = None;
    } else {
        candidate.revision = current.revision;
        candidate.projection_fingerprint = current.projection_fingerprint.clone();
    }

    let after = project(&candidate)?;
    let summary = summarize(
        &before.legacy,
        &after.legacy,
        operations_added,
        duplicate_operations,
        changed,
    );
    Ok(ImportPlan { candidate, summary })
}

fn rewrite_foreign_projection(
    current: &PersonalStateV2,
    imported: LegacyProjection,
) -> Result<(PersonalStateV2, usize, usize), PersonalStateError> {
    imported.validate()?;
    let imported_hash = stable_hash(&serde_json::to_string(&imported)?);
    let operation_id = format!("imported-baseline-{imported_hash}");
    if current
        .operations
        .iter()
        .any(|operation| operation.operation_id == operation_id)
    {
        return Ok((current.clone(), 0, 1));
    }

    let mut observed = VersionVector::default();
    let mut current_baseline = LegacyProjection::default();
    let mut current_baseline_envelope = None::<&OperationEnvelope>;
    for envelope in &current.operations {
        let Operation::LegacyBaseline { baseline } = &envelope.operation else {
            continue;
        };
        observed.observe(&envelope.stamp.dot);
        if current_baseline_envelope.is_none_or(|existing| {
            stamp_order(
                &envelope.stamp,
                &envelope.operation_id,
                &existing.stamp,
                &existing.operation_id,
            )
            .is_gt()
        }) {
            current_baseline = (**baseline).clone();
            current_baseline_envelope = Some(envelope);
        }
    }

    let combined = merge_baselines(current_baseline, imported);
    combined.validate()?;
    let mut candidate = current.clone();
    let import_device = DeviceId::new("000-import")?;
    let sequence = candidate
        .version_vector
        .observed(&import_device)
        .checked_add(1)
        .ok_or(PersonalStateError::InvalidOperation(
            "import operation sequence exhausted",
        ))?;
    let dot = Dot {
        device_id: import_device,
        sequence,
    };
    candidate.operations.push(OperationEnvelope {
        operation_id,
        stamp: CausalStamp {
            dot: dot.clone(),
            observed,
            recorded_at_unix: crate::signals::unix_now(),
        },
        origin: OperationOrigin::Imported,
        operation: Operation::LegacyBaseline {
            baseline: Box::new(combined),
        },
    });
    candidate.version_vector.observe(&dot);
    candidate.projection_fingerprint = None;
    candidate.normalize()?;
    Ok((candidate, 1, 0))
}

fn merge_baselines(mut local: LegacyProjection, imported: LegacyProjection) -> LegacyProjection {
    local.favorites = merge_tracks(imported.favorites, local.favorites, FAVORITES_MAX);
    local.history = merge_tracks(imported.history, local.history, HISTORY_MAX);
    local.radio_favorites =
        merge_tracks(imported.radio_favorites, local.radio_favorites, RADIO_MAX);
    local.radio_history = merge_tracks(imported.radio_history, local.radio_history, RADIO_MAX);
    local.playlists = merge_playlists(local.playlists, imported.playlists);

    for (key, imported_signal) in imported.signals.tracks {
        match local.signals.tracks.get_mut(&key) {
            Some(local_signal) => {
                local_signal.play_count = local_signal.play_count.max(imported_signal.play_count);
                local_signal.completed_count = local_signal
                    .completed_count
                    .max(imported_signal.completed_count);
                local_signal.skip_count = local_signal.skip_count.max(imported_signal.skip_count);
                if imported_signal.last_played_at >= local_signal.last_played_at {
                    local_signal.track = imported_signal.track;
                    local_signal.last_played_at = imported_signal.last_played_at;
                    local_signal.last_completion = imported_signal.last_completion;
                }
                local_signal.disliked |= imported_signal.disliked;
            }
            None => {
                local.signals.tracks.insert(key, imported_signal);
            }
        }
    }
    local.signals.tracks = local
        .signals
        .tracks
        .into_iter()
        .take(SIGNAL_TRACKS_MAX)
        .collect();
    for (artist, weight) in imported.signals.artist_affinity {
        local.signals.artist_affinity.insert(artist, weight);
    }
    let mut event_ids = HashSet::new();
    local.signals.play_log = imported
        .signals
        .play_log
        .into_iter()
        .chain(local.signals.play_log)
        .filter(|event| event_ids.insert(event.event_id.clone()))
        .take(ENGAGEMENT_EVENTS_MAX)
        .collect();

    if imported.station.query.is_some() {
        local.station.query = imported.station.query;
        local.station.explore = imported.station.explore;
    }
    local.station.avoid_artist_keys = imported
        .station
        .avoid_artist_keys
        .into_iter()
        .chain(local.station.avoid_artist_keys)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    local
}

fn merge_tracks(
    preferred: Vec<PortableTrack>,
    existing: Vec<PortableTrack>,
    limit: usize,
) -> Vec<PortableTrack> {
    let mut seen = HashSet::<PortableTrackKey>::new();
    preferred
        .into_iter()
        .chain(existing)
        .filter(|track| seen.insert(track.key.clone()))
        .take(limit)
        .collect()
}

fn merge_playlists(
    local: Vec<LegacyPlaylist>,
    imported: Vec<LegacyPlaylist>,
) -> Vec<LegacyPlaylist> {
    let mut playlists = local
        .into_iter()
        .map(|playlist| (playlist.playlist_id.clone(), playlist))
        .collect::<BTreeMap<_, _>>();
    for mut incoming in imported {
        if let Some(existing) = playlists.get_mut(&incoming.playlist_id) {
            let mut seen = incoming
                .entries
                .iter()
                .map(|entry| entry.entry_id.clone())
                .collect::<HashSet<_>>();
            incoming.entries.extend(
                existing
                    .entries
                    .iter()
                    .filter(|entry| seen.insert(entry.entry_id.clone()))
                    .cloned(),
            );
            incoming.entries.truncate(PLAYLIST_ENTRIES_MAX);
            *existing = incoming;
        } else {
            incoming.entries.truncate(PLAYLIST_ENTRIES_MAX);
            playlists.insert(incoming.playlist_id.clone(), incoming);
        }
    }
    playlists.into_values().take(PLAYLISTS_MAX).collect()
}

fn summarize(
    before: &LegacyProjection,
    after: &LegacyProjection,
    operations_added: usize,
    duplicate_operations: usize,
    changed: bool,
) -> ImportSummary {
    ImportSummary {
        operations_added,
        duplicate_operations,
        favorites_added: added_tracks(&before.favorites, &after.favorites),
        history_added: added_tracks(&before.history, &after.history),
        radio_favorites_added: added_tracks(&before.radio_favorites, &after.radio_favorites),
        playlists_added: after.playlists.len().saturating_sub(before.playlists.len()),
        playlist_entries_added: after
            .playlists
            .iter()
            .map(|playlist| playlist.entries.len())
            .sum::<usize>()
            .saturating_sub(
                before
                    .playlists
                    .iter()
                    .map(|playlist| playlist.entries.len())
                    .sum(),
            ),
        signal_tracks_added: after
            .signals
            .tracks
            .len()
            .saturating_sub(before.signals.tracks.len()),
        changed,
    }
}

fn added_tracks(before: &[PortableTrack], after: &[PortableTrack]) -> usize {
    let before = before
        .iter()
        .map(|track| &track.key)
        .collect::<HashSet<_>>();
    after
        .iter()
        .filter(|track| !before.contains(&track.key))
        .count()
}

fn stamp_order(
    candidate: &CausalStamp,
    candidate_id: &str,
    current: &CausalStamp,
    current_id: &str,
) -> Ordering {
    if candidate.happens_after(current) {
        Ordering::Greater
    } else if current.happens_after(candidate) {
        Ordering::Less
    } else {
        candidate
            .dot
            .cmp(&current.dot)
            .then(candidate_id.cmp(current_id))
    }
}
