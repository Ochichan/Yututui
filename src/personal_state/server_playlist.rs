//! Exact playlist snapshots derived from the canonical personal-state ledger.
//!
//! Runtime `playlists.json` intentionally remains a compatibility projection and does not carry
//! permanent entry identifiers. Server bridges must use this view so duplicate occurrences,
//! removals, and moves never fall back to title or runtime `video_id` matching.

use super::{
    PersonalStateError, PersonalStateV2, PlaylistEntryId, PlaylistId, PortableTrack, project,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalPlaylistEntry {
    pub entry_id: PlaylistEntryId,
    pub track: PortableTrack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalPlaylistSnapshot {
    pub playlist_id: PlaylistId,
    pub name: String,
    pub entries: Vec<PersonalPlaylistEntry>,
}

pub fn personal_playlist_snapshots(
    state: &PersonalStateV2,
) -> Result<Vec<PersonalPlaylistSnapshot>, PersonalStateError> {
    Ok(project(state)?
        .legacy
        .playlists
        .into_iter()
        .map(snapshot_from_legacy)
        .collect())
}

/// Resolves the compatibility playlist ID shown by the local Library back to its permanent ID.
///
/// Runtime `playlists.json` stores the projection slug in `Playlist::id`; it must not be parsed
/// as a [`PlaylistId`] because a v2 playlist's permanent ID and compatibility slug are separate.
pub fn personal_playlist_snapshot_for_runtime_id(
    state: &PersonalStateV2,
    runtime_id: &str,
) -> Result<Option<PersonalPlaylistSnapshot>, PersonalStateError> {
    Ok(project(state)?
        .legacy
        .playlists
        .into_iter()
        .find(|playlist| playlist.slug == runtime_id)
        .map(snapshot_from_legacy))
}

pub fn personal_playlist_snapshot(
    state: &PersonalStateV2,
    playlist_id: &PlaylistId,
) -> Result<Option<PersonalPlaylistSnapshot>, PersonalStateError> {
    Ok(personal_playlist_snapshots(state)?
        .into_iter()
        .find(|playlist| &playlist.playlist_id == playlist_id))
}

fn snapshot_from_legacy(playlist: super::legacy::LegacyPlaylist) -> PersonalPlaylistSnapshot {
    PersonalPlaylistSnapshot {
        playlist_id: playlist.playlist_id,
        name: playlist.name,
        entries: playlist
            .entries
            .into_iter()
            .map(|entry| PersonalPlaylistEntry {
                entry_id: entry.entry_id,
                track: entry.track,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::personal_state::{
        ExternalOperationInput, Operation, OperationOrigin, PlaylistEntryId, PortableTrackKey,
        append_external_operations, legacy_state,
    };

    fn track(id: &str) -> PortableTrack {
        PortableTrack {
            key: PortableTrackKey::OpenSubsonic {
                backend_id: "backend".to_owned(),
                account_scope_id: "account".to_owned(),
                item_id: id.to_owned(),
            },
            title: format!("Track {id}"),
            artist: "Artist".to_owned(),
            album: None,
            duration_secs: Some(180),
            isrc: None,
        }
    }

    #[test]
    fn canonical_snapshot_preserves_duplicate_occurrence_ids() {
        let state = legacy_state(
            &crate::library::Library::default(),
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        let playlist_id = PlaylistId::new("playlist").unwrap();
        let first = PlaylistEntryId::new("first").unwrap();
        let second = PlaylistEntryId::new("second").unwrap();
        let operations = [
            ExternalOperationInput {
                acknowledgement_id: "playlist".to_owned(),
                operation: Operation::UpsertPlaylist {
                    playlist_id: playlist_id.clone(),
                    name: "Mix".to_owned(),
                },
                recorded_at_unix: 1,
            },
            ExternalOperationInput {
                acknowledgement_id: "first".to_owned(),
                operation: Operation::UpsertPlaylistEntry {
                    playlist_id: playlist_id.clone(),
                    entry_id: first.clone(),
                    track: track("same"),
                    after_entry_id: None,
                },
                recorded_at_unix: 2,
            },
            ExternalOperationInput {
                acknowledgement_id: "second".to_owned(),
                operation: Operation::UpsertPlaylistEntry {
                    playlist_id: playlist_id.clone(),
                    entry_id: second.clone(),
                    track: track("same"),
                    after_entry_id: Some(first.clone()),
                },
                recorded_at_unix: 2,
            },
        ];
        let (state, _) = append_external_operations(
            &state,
            OperationOrigin::OpenSubsonic {
                backend_id: "backend".to_owned(),
            },
            &operations,
        )
        .unwrap();

        let snapshot = personal_playlist_snapshot(&state, &playlist_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot
                .entries
                .iter()
                .map(|entry| entry.entry_id.clone())
                .collect::<Vec<_>>(),
            vec![first, second]
        );
        assert_eq!(snapshot.entries[0].track.key, snapshot.entries[1].track.key);
    }

    #[test]
    fn runtime_projection_slug_resolves_to_permanent_playlist_id() {
        let state = legacy_state(
            &crate::library::Library::default(),
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        let playlist_id = PlaylistId::new("permanent-id").unwrap();
        let (state, _) = append_external_operations(
            &state,
            OperationOrigin::Imported,
            &[ExternalOperationInput {
                acknowledgement_id: "runtime-slug".to_owned(),
                operation: Operation::UpsertPlaylist {
                    playlist_id: playlist_id.clone(),
                    name: "Mix".to_owned(),
                },
                recorded_at_unix: 1,
            }],
        )
        .unwrap();
        let runtime = project(&state).unwrap().legacy.playlists.remove(0);

        assert_ne!(runtime.slug, playlist_id.as_str());
        assert_eq!(
            personal_playlist_snapshot_for_runtime_id(&state, &runtime.slug)
                .unwrap()
                .unwrap()
                .playlist_id,
            playlist_id
        );
    }
}
