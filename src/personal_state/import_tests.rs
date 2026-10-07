//! Import authorship: the vault can only publish operations owned by a membership device.

use super::tests::state_with_keyed_devices;
use super::{
    DeviceId, OperationOrigin, PersonalStateError, PortableTrack, PortableTrackKey,
    append_listening, legacy_state, merge, plan_import, plan_join_import,
};

fn bookmark_change() -> crate::listening::ListeningOperation {
    crate::listening::ListeningOperation::UpsertBookmark {
        bookmark: crate::listening::BookmarkRecord {
            bookmark_id: crate::listening::BookmarkId::new("imported-bookmark").unwrap(),
            track: PortableTrack {
                key: PortableTrackKey::Catalog {
                    provider: "ytmusic".to_owned(),
                    exact_catalog_id: "long-form".to_owned(),
                },
                title: "Long form".to_owned(),
                artist: "Artist".to_owned(),
                album: None,
                duration_secs: Some(3_600),
                isrc: None,
            },
            position_ms: 90_000,
            label: "Chapter".to_owned(),
        },
    }
}

#[test]
fn foreign_import_is_owned_by_a_registry_device_so_sync_can_publish_it() {
    // A version-vector entry for a device the vault's membership does not know makes `ytt sync`
    // reject the whole dataset, so an imported baseline must never invent its own author.
    let mut imported_library = crate::library::Library::default();
    imported_library.toggle_favorite(&crate::api::Song::remote(
        "remote".to_owned(),
        "Remote".to_owned(),
        "Artist".to_owned(),
        "3:00".to_owned(),
    ));
    let mut imported = legacy_state(
        &imported_library,
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();
    imported.dataset_id = "foreign-dataset".to_owned();

    let single = legacy_state(
        &crate::library::Library::default(),
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();
    let candidate = plan_import(&single, &imported, None).unwrap().candidate;
    for device_id in candidate.version_vector.0.keys() {
        assert!(
            device_id.as_str() == "legacy" || candidate.device_registry.contains_key(device_id),
            "{device_id:?} is not a registry device"
        );
    }

    let first = DeviceId::new("device-a").unwrap();
    let second = DeviceId::new("device-b").unwrap();
    let paired = state_with_keyed_devices(&[first.as_str(), second.as_str()]);
    let bound = plan_import(&paired, &imported, Some(&second))
        .unwrap()
        .candidate;
    let owner = bound
        .operations
        .iter()
        .find(|envelope| envelope.origin == OperationOrigin::Imported)
        .expect("the foreign baseline is one imported operation")
        .stamp
        .dot
        .device_id
        .clone();
    assert_eq!(owner, second, "the bound local device owns the import");
    assert!(bound.version_vector.0.contains_key(&second));

    assert!(
        matches!(
            plan_import(&paired, &imported, None),
            Err(PersonalStateError::InvalidOperation(
                "multiple active devices require an explicit local device binding"
            ))
        ),
        "an ambiguous registry must be refused, not authored by a synthetic device"
    );
    assert!(matches!(
        plan_import(
            &paired,
            &imported,
            Some(&DeviceId::new("000-import").unwrap())
        ),
        Err(PersonalStateError::InvalidOperation(
            "local device binding is not in the registry"
        ))
    ));
}

#[test]
fn foreign_and_join_imports_reauthor_listening_records_without_dropping_them() {
    let empty = || {
        legacy_state(
            &crate::library::Library::default(),
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap()
    };
    let initial = append_listening(&empty(), None, bookmark_change(), 1).unwrap();
    let deleted = append_listening(
        &initial,
        None,
        crate::listening::ListeningOperation::DeleteBookmark {
            bookmark_id: crate::listening::BookmarkId::new("imported-bookmark").unwrap(),
        },
        2,
    )
    .unwrap();
    let mut imported = append_listening(&deleted, None, bookmark_change(), 3).unwrap();
    imported.dataset_id = "foreign-listening".to_owned();

    let current = empty();
    let foreign = plan_import(&current, &imported, None).unwrap().candidate;
    let projected = crate::listening::ListeningProjection::from_ledger(&foreign).unwrap();
    assert!(
        projected
            .bookmarks
            .contains_key(&crate::listening::BookmarkId::new("imported-bookmark").unwrap())
    );
    assert!(foreign.operations.iter().any(|operation| {
        operation.origin == OperationOrigin::Imported
            && matches!(operation.operation, super::Operation::Listening { .. })
    }));
    assert!(foreign.operations.iter().any(|operation| {
        operation.origin == OperationOrigin::Imported
            && matches!(
                operation.operation,
                super::Operation::Listening {
                    change: crate::listening::ListeningOperation::DeleteBookmark { .. }
                }
            )
    }));

    let device = DeviceId::new("device-a").unwrap();
    let remote = state_with_keyed_devices(&[device.as_str()]);
    let joined = plan_join_import(&remote, &imported, &device)
        .unwrap()
        .candidate;
    let projected = crate::listening::ListeningProjection::from_ledger(&joined).unwrap();
    assert!(
        projected
            .bookmarks
            .contains_key(&crate::listening::BookmarkId::new("imported-bookmark").unwrap())
    );
    super::validate_join_import_extension(&remote, &joined, &device).unwrap();
}

#[test]
fn two_devices_can_import_the_same_foreign_listening_bundle_and_merge() {
    let empty = legacy_state(
        &crate::library::Library::default(),
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();
    let mut imported = append_listening(&empty, None, bookmark_change(), 1).unwrap();
    imported.dataset_id = "foreign-two-device-listening".to_owned();

    let device_a = DeviceId::new("device-a").unwrap();
    let device_b = DeviceId::new("device-b").unwrap();
    let paired = state_with_keyed_devices(&[device_a.as_str(), device_b.as_str()]);
    let from_a = plan_import(&paired, &imported, Some(&device_a))
        .unwrap()
        .candidate;
    let from_b = plan_import(&paired, &imported, Some(&device_b))
        .unwrap()
        .candidate;
    let (merged, _) = merge(&from_a, &from_b).unwrap();

    let imported_ids = merged
        .operations
        .iter()
        .filter(|operation| {
            operation.origin == OperationOrigin::Imported
                && matches!(operation.operation, super::Operation::Listening { .. })
        })
        .map(|operation| operation.operation_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(imported_ids.len(), 2);
    assert!(
        crate::listening::ListeningProjection::from_ledger(&merged)
            .unwrap()
            .bookmarks
            .contains_key(&crate::listening::BookmarkId::new("imported-bookmark").unwrap())
    );
}
