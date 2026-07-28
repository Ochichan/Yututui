use crate::personal_state::{DeviceRecord, Operation, append_operation_as, legacy_state};

use super::{
    CheckpointAnchor, DeviceSecretMaterial, MembershipAnchor, MembershipChain, RecoveryKit,
    SignedCheckpoint, SignedMembershipRoot,
};

#[test]
fn legacy_local_device_enrollment_matches_the_signed_membership_root() {
    let state = legacy_state(
        &crate::library::Library::default(),
        &crate::playlists::Playlists::default(),
        &crate::signals::Signals::default(),
        &crate::station::StationStore::default(),
    )
    .unwrap();
    let local = state
        .device_registry
        .values()
        .find(|device| device.device_id.as_str() != "legacy")
        .unwrap();
    assert_eq!(local.name, "This device");
    assert!(local.public_identity.is_none());

    let secrets = DeviceSecretMaterial::generate_for(local.device_id.as_str()).unwrap();
    let enrolled = DeviceRecord {
        device_id: local.device_id.clone(),
        name: "Work laptop".to_owned(),
        revoked: false,
        public_identity: Some(secrets.public_identity()),
    };
    let state = append_operation_as(
        &state,
        &enrolled.device_id,
        Operation::AddDevice {
            device: enrolled.clone(),
        },
        1,
    )
    .unwrap();
    assert_eq!(state.device_registry[&enrolled.device_id], enrolled);

    let recovery = RecoveryKit::generate(state.dataset_id.clone(), None).unwrap();
    let root = SignedMembershipRoot::create(
        state.dataset_id.clone(),
        recovery.recovery_recipient(),
        &recovery.signing_key().unwrap(),
        enrolled.clone(),
    )
    .unwrap();
    let membership_anchor = MembershipAnchor::RootHash(root.hash().unwrap());
    let checkpoint = SignedCheckpoint::create(
        MembershipChain::new(root),
        &membership_anchor,
        enrolled.device_id,
        secrets.signing_key(),
        &CheckpointAnchor::default(),
        state,
    )
    .unwrap();

    checkpoint.verify(&membership_anchor).unwrap();
}
