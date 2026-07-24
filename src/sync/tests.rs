use std::sync::atomic::{AtomicU64, Ordering};

use crate::personal_state::{
    CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateV2, PortableTrack, PortableTrackKey, Rating, VersionVector, append_operation_as,
};

use super::*;

static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct VaultFixture {
    kit: RecoveryKit,
    device: DeviceSecretMaterial,
    device_record: DeviceRecord,
    membership: MembershipChain,
    membership_anchor: MembershipAnchor,
    state: PersonalStateV2,
    checkpoint: SignedCheckpoint,
    encrypted_checkpoint: EncryptedObject,
}

fn bootstrap() -> VaultFixture {
    let dataset_id = "dataset-integration";
    let kit = RecoveryKit::generate(dataset_id, None).unwrap();
    let device = DeviceSecretMaterial::generate_for("device-one").unwrap();
    let device_record = DeviceRecord {
        device_id: DeviceId::new(device.device_id()).unwrap(),
        name: "First device".to_owned(),
        revoked: false,
        public_identity: Some(device.public_identity()),
    };
    let recovery_signing_key = kit.signing_key().unwrap();
    let root = SignedMembershipRoot::create(
        dataset_id,
        kit.recovery_recipient(),
        &recovery_signing_key,
        device_record.clone(),
    )
    .unwrap();
    let membership_anchor = MembershipAnchor::RootHash(root.hash().unwrap());
    let membership = MembershipChain::new(root);
    let state = initial_state(dataset_id, &device_record);
    let checkpoint = SignedCheckpoint::create(
        membership.clone(),
        &membership_anchor,
        device_record.device_id.clone(),
        device.signing_key(),
        &CheckpointAnchor::default(),
        state.clone(),
    )
    .unwrap();
    let encrypted_checkpoint = checkpoint.encrypt(&membership_anchor).unwrap();
    VaultFixture {
        kit,
        device,
        device_record,
        membership,
        membership_anchor,
        state,
        checkpoint,
        encrypted_checkpoint,
    }
}

fn initial_state(dataset_id: &str, device: &DeviceRecord) -> PersonalStateV2 {
    let dot = Dot {
        device_id: device.device_id.clone(),
        sequence: 1,
    };
    let mut state = PersonalStateV2::empty(dataset_id.to_owned()).unwrap();
    state.operations.push(OperationEnvelope {
        operation_id: "initial-device".to_owned(),
        stamp: CausalStamp {
            dot: dot.clone(),
            observed: VersionVector::default(),
            recorded_at_unix: 0,
        },
        origin: OperationOrigin::Local,
        operation: Operation::AddDevice {
            device: device.clone(),
        },
    });
    state.version_vector.observe(&dot);
    crate::personal_state::refresh_device_registry(&mut state).unwrap();
    state.normalize().unwrap();
    state
}

fn test_directory(label: &str) -> std::path::PathBuf {
    let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "ytt-vault-{label}-{}-{sequence}",
        std::process::id()
    ));
    crate::util::safe_fs::ensure_private_dir(&path).unwrap();
    path
}

#[test]
fn pairing_is_single_use_and_delivers_an_anchored_checkpoint() {
    let fixture = bootstrap();
    let invite = PairingInvite::create(
        fixture.state.dataset_id.clone(),
        fixture.membership_anchor_root_hash(),
        fixture.membership_anchor_root_hash(),
        1_000,
    )
    .unwrap();
    let join_code = PairingCode::parse(invite.code().expose_secret()).unwrap();
    let joining = DeviceSecretMaterial::generate_for("device-two").unwrap();
    let (request, nonce) =
        PairingInvite::create_request(&join_code, "Second device", &joining, 1_001).unwrap();
    let resumed_request = PairingInvite::resume_request(
        &join_code,
        EncryptedObject::from_bytes(request.encrypted.as_bytes().to_vec()).unwrap(),
        &nonce,
        &joining,
        1_002,
    )
    .unwrap();
    assert_eq!(
        resumed_request.encrypted.as_bytes(),
        request.encrypted.as_bytes(),
        "restart reuses the exact immutable request ciphertext"
    );
    assert!(
        PairingInvite::resume_request(
            &join_code,
            EncryptedObject::from_bytes(request.encrypted.as_bytes().to_vec()).unwrap(),
            &"f".repeat(32),
            &joining,
            1_002,
        )
        .is_err()
    );
    let reviewed = invite.review_request(&request, 1_002).unwrap();

    let mut membership = fixture.membership.clone();
    membership
        .append_device_action(
            &fixture.membership_anchor,
            &fixture.device_record.device_id,
            fixture.device.signing_key(),
            MembershipAction::AddDevice {
                device: reviewed.device.clone(),
            },
        )
        .unwrap();
    let state = append_operation_as(
        &fixture.state,
        &fixture.device_record.device_id,
        Operation::AddDevice {
            device: reviewed.device,
        },
        1_002,
    )
    .unwrap();
    let checkpoint_anchor = CheckpointAnchor::from_trusted(
        fixture.checkpoint.payload.checkpoint_sequence,
        fixture.checkpoint.hash().unwrap(),
    )
    .unwrap();
    let checkpoint = SignedCheckpoint::create(
        membership.clone(),
        &fixture.membership_anchor,
        fixture.device_record.device_id.clone(),
        fixture.device.signing_key(),
        &checkpoint_anchor,
        state,
    )
    .unwrap();
    let encrypted = checkpoint.encrypt(&fixture.membership_anchor).unwrap();
    let approval = invite
        .approve(
            &request,
            membership.clone(),
            &encrypted,
            &fixture.device,
            1_003,
        )
        .unwrap();
    assert!(
        invite
            .approve(
                &request,
                membership.clone(),
                &encrypted,
                &fixture.device,
                1_003
            )
            .is_ok(),
        "approval construction stays retryable until publication is finalized"
    );
    invite
        .finalize_approval(&request, &approval, 1_003)
        .unwrap();
    assert_eq!(
        invite
            .approve(&request, membership, &encrypted, &fixture.device, 1_003)
            .err(),
        Some(VaultError::PairingConsumed)
    );

    let approved =
        ApprovedPairing::open(&join_code, &approval, &nonce, &joining, &encrypted, 1_004).unwrap();
    let opened = SignedCheckpoint::decrypt_for_device(
        approved.encrypted_checkpoint(),
        &joining,
        &MembershipAnchor::RootHash(approved.membership_root_hash().to_owned()),
    )
    .unwrap();
    assert_eq!(opened.payload.state.device_registry.len(), 2);

    let resumed_after_expiry = ApprovedPairing::open(
        &join_code,
        &approval,
        &nonce,
        &joining,
        &encrypted,
        invite.expires_at_unix() + 1,
    )
    .unwrap();
    assert_eq!(
        resumed_after_expiry.signed_checkpoint().hash().unwrap(),
        approved.signed_checkpoint().hash().unwrap(),
        "an already committed exact handoff remains resumable after the code expires"
    );

    let wrong_code = PairingCode::generate().unwrap();
    assert!(
        ApprovedPairing::open(&wrong_code, &approval, &nonce, &joining, &encrypted, 1_004).is_err()
    );

    let joining_copy = DeviceSecretMaterial::from_encoded(
        joining.device_id(),
        joining.age_identity_secret(),
        &joining.signing_key_secret_b64(),
    )
    .unwrap();
    let mut mismatched =
        PrivateStoreSnapshot::pending_approval(fixture.state.dataset_id.clone(), joining_copy)
            .unwrap();
    mismatched
        .set_pending_pairing(invite.invite_id(), "f".repeat(32))
        .unwrap();
    assert_eq!(
        mismatched.approve(&approved),
        Err(VaultError::InvalidPrivateStore)
    );
    assert!(mismatched.pending_pairing().unwrap().is_some());

    let private_root = test_directory("pairing-private-store");
    let private_store = PrivateStore::new(private_root.join("private.json")).unwrap();
    let mut private =
        PrivateStoreSnapshot::pending_approval(fixture.state.dataset_id.clone(), joining).unwrap();
    private
        .set_pending_pairing(invite.invite_id(), nonce)
        .unwrap();
    private_store.create(&mut private).unwrap();
    private.approve(&approved).unwrap();
    private_store.save(&mut private).unwrap();
    let mut resumed = private_store.load().unwrap();
    assert_eq!(resumed.enrollment(), EnrollmentState::PendingLedgerCommit);
    assert!(
        resumed.pending_pairing().unwrap().is_some(),
        "approved join retains enough context to resume after restart"
    );
    resumed
        .mark_active(approved.signed_checkpoint(), &opened.payload.state)
        .unwrap();
    private_store.save(&mut resumed).unwrap();
    let expected_hash = approved.signed_checkpoint().hash().unwrap();
    assert_eq!(
        private_store.load().unwrap().checkpoint_hash(),
        Some(expected_hash.as_str())
    );
    let _ = std::fs::remove_dir_all(private_root);
}

#[test]
fn expired_pairing_never_changes_membership() {
    let fixture = bootstrap();
    let invite = PairingInvite::create(
        fixture.state.dataset_id.clone(),
        fixture.membership_anchor_root_hash(),
        fixture.membership_anchor_root_hash(),
        10,
    )
    .unwrap();
    let join_code = PairingCode::parse(invite.code().expose_secret()).unwrap();
    let joining = DeviceSecretMaterial::generate_for("device-late").unwrap();
    let (request, _) =
        PairingInvite::create_request(&join_code, "Late device", &joining, 11).unwrap();
    assert_eq!(
        invite.review_request(&request, 611).err(),
        Some(VaultError::PairingExpired)
    );
    assert_eq!(fixture.membership.changes.len(), 0);
}

#[test]
fn expired_pairing_request_is_rejected_while_invite_is_still_valid() {
    let fixture = bootstrap();
    let invite = PairingInvite::create(
        fixture.state.dataset_id.clone(),
        fixture.membership_anchor_root_hash(),
        fixture.membership_anchor_root_hash(),
        1_000,
    )
    .unwrap();
    let join_code = PairingCode::parse(invite.code().expose_secret()).unwrap();
    let joining = DeviceSecretMaterial::generate_for("device-stale-request").unwrap();
    let (request, _) =
        PairingInvite::create_request(&join_code, "Stale request", &joining, 0).unwrap();

    assert_eq!(
        invite.review_request(&request, 601).err(),
        Some(VaultError::PairingExpired)
    );
}

#[test]
fn pairing_rejects_a_membership_fork_older_than_the_invite() {
    let fixture = bootstrap();
    let preexisting = DeviceSecretMaterial::generate_for("device-preexisting").unwrap();
    let preexisting_record = DeviceRecord {
        device_id: DeviceId::new(preexisting.device_id()).unwrap(),
        name: "Preexisting".to_owned(),
        revoked: false,
        public_identity: Some(preexisting.public_identity()),
    };
    let mut current_membership = fixture.membership.clone();
    let current = current_membership
        .append_device_action(
            &fixture.membership_anchor,
            &fixture.device_record.device_id,
            fixture.device.signing_key(),
            MembershipAction::AddDevice {
                device: preexisting_record,
            },
        )
        .unwrap();
    let invite = PairingInvite::create(
        fixture.state.dataset_id.clone(),
        current.root_hash,
        current.head_hash,
        2_000,
    )
    .unwrap();
    let code = PairingCode::parse(invite.code().expose_secret()).unwrap();
    let joining = DeviceSecretMaterial::generate_for("device-fork-join").unwrap();
    let (request, _) = PairingInvite::create_request(&code, "Fork join", &joining, 2_001).unwrap();
    let reviewed = invite.review_request(&request, 2_002).unwrap();

    let mut stale_fork = fixture.membership.clone();
    stale_fork
        .append_device_action(
            &fixture.membership_anchor,
            &fixture.device_record.device_id,
            fixture.device.signing_key(),
            MembershipAction::AddDevice {
                device: reviewed.device,
            },
        )
        .unwrap();
    assert_eq!(
        invite
            .approve(
                &request,
                stale_fork,
                &fixture.encrypted_checkpoint,
                &fixture.device,
                2_003,
            )
            .err(),
        Some(VaultError::InvalidMembership)
    );
}

#[test]
fn recovery_kit_replaces_all_devices_and_rotates_recipients() {
    let fixture = bootstrap();
    let kit = RecoveryKit::from_json(&fixture.kit.to_json().unwrap()).unwrap();
    let first_anchor = CheckpointAnchor::from_trusted(
        fixture.checkpoint.payload.checkpoint_sequence,
        fixture.checkpoint.hash().unwrap(),
    )
    .unwrap();
    let second = SignedCheckpoint::create(
        fixture.membership.clone(),
        &fixture.membership_anchor,
        fixture.device_record.device_id.clone(),
        fixture.device.signing_key(),
        &first_anchor,
        fixture.state.clone(),
    )
    .unwrap();
    let second_encrypted = second.encrypt(&fixture.membership_anchor).unwrap();
    let canonical_manifest = manual::SignedVaultManifest::create(
        &fixture.state.dataset_id,
        2,
        fixture.membership.clone(),
        &fixture.membership_anchor,
        fixture.device_record.device_id.clone(),
        &fixture.device,
        &second,
    )
    .unwrap();
    let encrypted_manifest = canonical_manifest
        .encrypt(&fixture.membership_anchor)
        .unwrap();
    let orphan_state = append_operation_as(
        &fixture.state,
        &fixture.device_record.device_id,
        Operation::SetRating {
            track: PortableTrack {
                key: PortableTrackKey::Catalog {
                    provider: "youtube".to_owned(),
                    exact_catalog_id: "orphan-checkpoint".to_owned(),
                },
                title: "Lost manifest race".to_owned(),
                artist: "Concurrent writer".to_owned(),
                album: None,
                duration_secs: Some(180),
                isrc: None,
            },
            rating: Rating::Liked,
        },
        1_003,
    )
    .unwrap();
    let orphan = SignedCheckpoint::create(
        fixture.membership.clone(),
        &fixture.membership_anchor,
        fixture.device_record.device_id.clone(),
        fixture.device.signing_key(),
        &first_anchor,
        orphan_state,
    )
    .unwrap();
    assert_ne!(orphan.hash().unwrap(), second.hash().unwrap());
    let orphan_encrypted = orphan.encrypt(&fixture.membership_anchor).unwrap();
    let mut recovered = kit
        .recover(
            &encrypted_manifest,
            &[
                orphan_encrypted,
                second_encrypted.clone(),
                fixture.encrypted_checkpoint.clone(),
            ],
            None,
            "Recovered device",
        )
        .unwrap();
    assert!(
        !recovered
            .state
            .operations
            .iter()
            .any(|operation| operation.operation_id == "device-one:2"),
        "a valid same-sequence checkpoint orphaned by a lost manifest CAS is not canonical"
    );
    assert_eq!(
        recovered.checkpoint.payload.checkpoint_sequence,
        second.payload.checkpoint_sequence + 1
    );
    let second_anchor =
        CheckpointAnchor::from_trusted(second.payload.checkpoint_sequence, second.hash().unwrap())
            .unwrap();
    let stale_manifest = manual::SignedVaultManifest::create(
        &fixture.state.dataset_id,
        1,
        fixture.membership.clone(),
        &fixture.membership_anchor,
        fixture.device_record.device_id.clone(),
        &fixture.device,
        &fixture.checkpoint,
    )
    .unwrap()
    .encrypt(&fixture.membership_anchor)
    .unwrap();
    assert_eq!(
        kit.recover(
            &stale_manifest,
            std::slice::from_ref(&fixture.encrypted_checkpoint),
            Some(&second_anchor),
            "Stale recovery",
        )
        .err(),
        Some(VaultError::RollbackDetected)
    );
    let verified = recovered
        .membership
        .verify(&MembershipAnchor::RecoveryVerifyingKey(
            kit.recovery_verifying_key().unwrap(),
        ))
        .unwrap();
    assert_eq!(verified.active_devices().count(), 1);
    assert!(
        verified
            .devices
            .get(&fixture.device_record.device_id)
            .unwrap()
            .revoked
    );
    SignedCheckpoint::decrypt_for_device(
        &recovered.encrypted_checkpoint,
        &recovered.device_secrets,
        &MembershipAnchor::RecoveryVerifyingKey(kit.recovery_verifying_key().unwrap()),
    )
    .unwrap();
    assert_eq!(
        SignedCheckpoint::decrypt_for_device(
            &recovered.encrypted_checkpoint,
            &fixture.device,
            &MembershipAnchor::RecoveryVerifyingKey(kit.recovery_verifying_key().unwrap()),
        )
        .err(),
        Some(VaultError::DecryptionFailed)
    );
    assert!(
        recovered
            .membership
            .append_device_action(
                &MembershipAnchor::RecoveryVerifyingKey(kit.recovery_verifying_key().unwrap()),
                &fixture.device_record.device_id,
                fixture.device.signing_key(),
                MembershipAction::AddDevice {
                    device: DeviceRecord {
                        device_id: DeviceId::new("attacker-device").unwrap(),
                        name: "Attacker".to_owned(),
                        revoked: false,
                        public_identity: Some(
                            DeviceSecretMaterial::generate_for("attacker-device")
                                .unwrap()
                                .public_identity()
                        ),
                    },
                },
            )
            .is_err()
    );
}

#[test]
fn membership_rejects_key_aliases_across_recovery_and_device_history() {
    let fixture = bootstrap();
    let mut membership = fixture.membership.clone();
    let alias = DeviceRecord {
        device_id: DeviceId::new("device-alias").unwrap(),
        name: "Alias".to_owned(),
        revoked: false,
        public_identity: Some(fixture.device.public_identity()),
    };
    assert_eq!(
        membership
            .append_device_action(
                &fixture.membership_anchor,
                &fixture.device_record.device_id,
                fixture.device.signing_key(),
                MembershipAction::AddDevice { device: alias },
            )
            .err(),
        Some(VaultError::InvalidDeviceIdentity)
    );

    let kit = RecoveryKit::generate("dataset-alias-root", None).unwrap();
    let initial = DeviceSecretMaterial::generate_for("device-root-alias").unwrap();
    let mut record = DeviceRecord {
        device_id: DeviceId::new(initial.device_id()).unwrap(),
        name: "Root alias".to_owned(),
        revoked: false,
        public_identity: Some(initial.public_identity()),
    };
    record.public_identity.as_mut().unwrap().age_recipient = kit.recovery_recipient();
    assert_eq!(
        SignedMembershipRoot::create(
            "dataset-alias-root",
            kit.recovery_recipient(),
            &kit.signing_key().unwrap(),
            record,
        )
        .err(),
        Some(VaultError::InvalidDeviceIdentity)
    );
}

#[test]
fn file_transport_is_conditional_idempotent_and_ciphertext_only() {
    assert_eq!(
        EncryptedObject::from_bytes(b"plaintext personal state".to_vec()).err(),
        Some(VaultError::InvalidEncryptedObject)
    );
    let fixture = bootstrap();
    let directory = test_directory("transport");
    let transport = FileVaultTransport::create(directory.clone()).unwrap();
    let key =
        ObjectKey::new("yututui/v2/dataset-integration/checkpoints/1/checkpoint.age").unwrap();
    let created = transport
        .put(
            &key,
            &fixture.encrypted_checkpoint,
            ObjectCondition::CreateOnly,
        )
        .unwrap();
    let created_metadata = match created {
        ObjectWriteResult::Created(metadata) => metadata,
        other => panic!("unexpected write result: {other:?}"),
    };
    assert_eq!(
        transport
            .put(
                &key,
                &fixture.encrypted_checkpoint,
                ObjectCondition::CreateOnly
            )
            .err(),
        Some(VaultError::PreconditionFailed)
    );
    // A retry after a lost response resolves the ambiguity with a GET/hash readback.
    assert_eq!(
        transport
            .get(&key, MAX_VAULT_PAYLOAD_BYTES)
            .unwrap()
            .unwrap()
            .1
            .etag,
        created_metadata.etag
    );

    let other = encrypt_json_to_recipients(
        &"different",
        &[fixture.device.public_identity().age_recipient],
    )
    .unwrap();
    assert_eq!(
        transport
            .put(&key, &other, ObjectCondition::Match("0".repeat(64)))
            .err(),
        Some(VaultError::PreconditionFailed)
    );
    let (readback, metadata) = transport
        .get(&key, MAX_VAULT_PAYLOAD_BYTES)
        .unwrap()
        .unwrap();
    assert_eq!(metadata.etag, created_metadata.etag);
    assert_eq!(readback, fixture.encrypted_checkpoint);
    let copied_key =
        ObjectKey::new("yututui/v2/dataset-integration/checkpoints/1/copied.age").unwrap();
    assert_eq!(
        transport
            .put(&copied_key, &readback, ObjectCondition::CreateOnly)
            .err(),
        Some(VaultError::InvalidEncryptedObject)
    );
    SignedCheckpoint::decrypt_for_device(&readback, &fixture.device, &fixture.membership_anchor)
        .unwrap();
    let prefix = ObjectKey::new("yututui/v2/dataset-integration/checkpoints").unwrap();
    assert_eq!(transport.list(&prefix, 10).unwrap().len(), 1);

    let mut tampered = readback.into_bytes();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    crate::util::safe_fs::write_private_atomic(&directory.join(key.as_str()), &tampered).unwrap();
    let (tampered, _) = transport
        .get(&key, MAX_VAULT_PAYLOAD_BYTES)
        .unwrap()
        .unwrap();
    assert!(
        SignedCheckpoint::decrypt_for_device(
            &tampered,
            &fixture.device,
            &fixture.membership_anchor
        )
        .is_err()
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn recovery_export_preserves_user_directory_permissions_and_never_replaces() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = test_directory("recovery-export");
    let destination = root.join("Downloads");
    std::fs::create_dir(&destination).unwrap();
    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = destination.join("recovery.json");
    let kit = RecoveryKit::generate("dataset-recovery-export", None).unwrap();

    kit.export_confirmed(&path).unwrap();

    assert_eq!(
        std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let first = std::fs::read(&path).unwrap();
    assert_eq!(
        kit.export_confirmed(&path).unwrap_err(),
        VaultError::StorageFailed
    );
    assert_eq!(std::fs::read(&path).unwrap(), first);

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn vault_identifiers_and_object_keys_are_portable_and_non_bypassable() {
    assert!(DeviceSecretMaterial::generate_for("device/escape").is_err());
    assert!(DeviceSecretMaterial::generate_for("기기").is_err());
    assert!(RecoveryKit::generate("DatasetUpper", None).is_err());
    assert!(ObjectKey::new("vault/NUL/item.age").is_err());
    assert!(ObjectKey::new("vault/con.txt").is_err());
    assert!(ObjectKey::new("vault/trailing./item.age").is_err());
    assert!(
        serde_json::from_str::<ObjectKey>(r#""vault/../escaped.age""#).is_err(),
        "serde must call the same constructor validation"
    );
}

impl VaultFixture {
    fn membership_anchor_root_hash(&self) -> String {
        match &self.membership_anchor {
            MembershipAnchor::RootHash(hash) => hash.clone(),
            MembershipAnchor::RecoveryVerifyingKey(_) => unreachable!(),
        }
    }
}
