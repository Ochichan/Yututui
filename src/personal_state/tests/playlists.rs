use super::*;

fn envelope(
    operation_id: &str,
    device_id: &str,
    sequence: u64,
    observed: VersionVector,
    operation: Operation,
) -> OperationEnvelope {
    OperationEnvelope {
        operation_id: operation_id.to_owned(),
        stamp: CausalStamp {
            dot: Dot {
                device_id: DeviceId::new(device_id).unwrap(),
                sequence,
            },
            observed,
            recorded_at_unix: 10,
        },
        origin: OperationOrigin::Local,
        operation,
    }
}

fn vector(entries: &[(&str, u64)]) -> VersionVector {
    VersionVector(
        entries
            .iter()
            .map(|(device_id, sequence)| (DeviceId::new(*device_id).unwrap(), *sequence))
            .collect(),
    )
}

fn state_with_operations(operations: Vec<OperationEnvelope>) -> PersonalStateV2 {
    let mut state = PersonalStateV2::empty("playlist-ordering".to_owned()).unwrap();
    for operation in &operations {
        state.version_vector.merge(&operation.stamp.observed);
        state.version_vector.observe(&operation.stamp.dot);
    }
    state.operations = operations;
    state
}

fn remove_after_upsert_operations() -> Vec<OperationEnvelope> {
    let playlist_id = PlaylistId::new("playlist").unwrap();
    let entry_id = PlaylistEntryId::new("entry").unwrap();
    vec![
        envelope(
            "upsert-playlist",
            "z-device",
            1,
            VersionVector::default(),
            Operation::UpsertPlaylist {
                playlist_id: playlist_id.clone(),
                name: "Playlist".to_owned(),
            },
        ),
        envelope(
            "upsert-entry",
            "z-device",
            2,
            vector(&[("z-device", 1)]),
            Operation::UpsertPlaylistEntry {
                playlist_id: playlist_id.clone(),
                entry_id: entry_id.clone(),
                track: track("song"),
                after_entry_id: None,
            },
        ),
        envelope(
            "remove-entry",
            "a-device",
            1,
            vector(&[("z-device", 2)]),
            Operation::RemovePlaylistEntry {
                playlist_id,
                entry_id,
                removed: true,
            },
        ),
    ]
}

#[test]
fn causally_later_remove_survives_every_operation_iteration_order() {
    let operations = remove_after_upsert_operations();
    for permutation in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let state = state_with_operations(
            permutation
                .into_iter()
                .map(|index| operations[index].clone())
                .collect(),
        );
        let projection = project(&state).unwrap();
        assert_eq!(projection.legacy.playlists.len(), 1);
        assert!(
            projection.legacy.playlists[0].entries.is_empty(),
            "causally later removal was lost for iteration order {permutation:?}"
        );
    }
}

#[test]
fn causally_later_move_survives_normalized_dot_order() {
    let playlist_id = PlaylistId::new("playlist").unwrap();
    let first_id = PlaylistEntryId::new("z-first").unwrap();
    let second_id = PlaylistEntryId::new("a-second").unwrap();
    let mut state = state_with_operations(vec![
        envelope(
            "upsert-playlist",
            "z-device",
            1,
            VersionVector::default(),
            Operation::UpsertPlaylist {
                playlist_id: playlist_id.clone(),
                name: "Playlist".to_owned(),
            },
        ),
        envelope(
            "upsert-first",
            "z-device",
            2,
            vector(&[("z-device", 1)]),
            Operation::UpsertPlaylistEntry {
                playlist_id: playlist_id.clone(),
                entry_id: first_id.clone(),
                track: track("first"),
                after_entry_id: None,
            },
        ),
        envelope(
            "upsert-second",
            "z-device",
            3,
            vector(&[("z-device", 2)]),
            Operation::UpsertPlaylistEntry {
                playlist_id: playlist_id.clone(),
                entry_id: second_id.clone(),
                track: track("second"),
                after_entry_id: Some(first_id.clone()),
            },
        ),
        envelope(
            "move-second",
            "a-device",
            1,
            vector(&[("z-device", 3)]),
            Operation::MovePlaylistEntry {
                playlist_id,
                entry_id: second_id,
                after_entry_id: None,
            },
        ),
    ]);
    state.normalize().unwrap();
    assert_eq!(
        state.operations[0].operation_id, "move-second",
        "the regression requires the move to be reduced before its upsert"
    );

    let entries = &project(&state).unwrap().legacy.playlists[0].entries;
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.track.key.clone())
            .collect::<Vec<_>>(),
        vec![track("second").key, track("first").key]
    );
}

#[test]
fn concurrent_pending_move_keeps_the_existing_dot_tie_break() {
    let playlist_id = PlaylistId::new("playlist").unwrap();
    let first_id = PlaylistEntryId::new("z-first").unwrap();
    let second_id = PlaylistEntryId::new("a-second").unwrap();
    let mut state = state_with_operations(vec![
        envelope(
            "upsert-playlist",
            "z-device",
            1,
            VersionVector::default(),
            Operation::UpsertPlaylist {
                playlist_id: playlist_id.clone(),
                name: "Playlist".to_owned(),
            },
        ),
        envelope(
            "upsert-first",
            "z-device",
            2,
            vector(&[("z-device", 1)]),
            Operation::UpsertPlaylistEntry {
                playlist_id: playlist_id.clone(),
                entry_id: first_id.clone(),
                track: track("first"),
                after_entry_id: None,
            },
        ),
        envelope(
            "upsert-second",
            "z-device",
            3,
            vector(&[("z-device", 2)]),
            Operation::UpsertPlaylistEntry {
                playlist_id: playlist_id.clone(),
                entry_id: second_id.clone(),
                track: track("second"),
                after_entry_id: Some(first_id),
            },
        ),
        envelope(
            "concurrent-move-second",
            "a-device",
            1,
            vector(&[("z-device", 1)]),
            Operation::MovePlaylistEntry {
                playlist_id,
                entry_id: second_id,
                after_entry_id: None,
            },
        ),
    ]);
    state.normalize().unwrap();

    let entries = &project(&state).unwrap().legacy.playlists[0].entries;
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.track.key.clone())
            .collect::<Vec<_>>(),
        vec![track("first").key, track("second").key],
        "the higher concurrent upsert dot must keep its position"
    );
}
