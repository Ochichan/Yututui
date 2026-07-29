//! Import authorship: the vault can only publish operations owned by a membership device.

use super::tests::state_with_keyed_devices;
use super::{DeviceId, OperationOrigin, PersonalStateError, legacy_state, plan_import};

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
