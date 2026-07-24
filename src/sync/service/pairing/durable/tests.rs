use std::sync::atomic::{AtomicU64, Ordering};

use crate::personal_state::DeviceId;
use crate::sync::{DeviceSecretMaterial, PairingInvite, SyncPaths, VaultError};

use super::*;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TempRoot(std::path::PathBuf);

impl TempRoot {
    fn new() -> Self {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "yututui-pairing-host-{}-{sequence}",
            std::process::id()
        ));
        crate::util::safe_fs::ensure_private_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn exact_pairing_artifacts_survive_every_host_restart_boundary() {
    let root = TempRoot::new();
    let paths = SyncPaths::for_data_root(root.0.clone());
    let host = DeviceSecretMaterial::generate_for("device-host").unwrap();
    let joining = DeviceSecretMaterial::generate_for("device-joining").unwrap();
    let invite =
        PairingInvite::create("dataset-pairing", "a".repeat(64), "b".repeat(64), 1_000).unwrap();
    let locator =
        crate::sync::crypto::seal_pairing_json(invite.code(), &("locator", 1_u32)).unwrap();
    let store = HostPairingStore::new(&paths);

    let mut snapshot = store
        .create(&host, &invite, 7, 11, &locator)
        .expect("durably stage invite");
    let restarted = store.load(&host).unwrap().expect("reload invite");
    assert!(snapshot.same_durable_record(&restarted));
    assert_eq!(
        store.locator(&restarted).unwrap().as_bytes(),
        locator.as_bytes()
    );

    let (request, _) =
        PairingInvite::create_request(invite.code(), "Joining device", &joining, 1_001).unwrap();
    let joining_id = DeviceId::new(joining.device_id()).unwrap();
    store
        .bind_request(&host, &mut snapshot, &request.encrypted, &joining_id)
        .expect("durably bind request");
    let restarted = store.load(&host).unwrap().expect("reload request");
    assert!(restarted.has_bound_request());
    assert!(store.request_matches(&restarted, &request.encrypted, &joining_id));
    assert_eq!(
        store.request(&restarted).unwrap().as_bytes(),
        request.encrypted.as_bytes()
    );

    let recipients = vec![host.public_identity().age_recipient];
    let checkpoint =
        crate::sync::crypto::encrypt_json_to_recipients(&("checkpoint", 2_u32), &recipients)
            .unwrap();
    let approval =
        crate::sync::crypto::encrypt_json_to_recipients(&("approval", 3_u32), &recipients).unwrap();
    store
        .prepare_handoff(
            &host,
            &mut snapshot,
            "c".repeat(64),
            "d".repeat(64),
            &checkpoint,
            &approval,
        )
        .expect("durably stage handoff");
    let restarted = store.load(&host).unwrap().expect("reload handoff");
    let handoff = store.load_handoff(&restarted).unwrap();
    assert_eq!(handoff.checkpoint.as_bytes(), checkpoint.as_bytes());
    assert_eq!(handoff.approval.as_bytes(), approval.as_bytes());
}

#[test]
fn signed_host_journal_rejects_replacement_and_request_rebinding() {
    let root = TempRoot::new();
    let paths = SyncPaths::for_data_root(root.0.clone());
    let host = DeviceSecretMaterial::generate_for("device-host").unwrap();
    let joining = DeviceSecretMaterial::generate_for("device-joining").unwrap();
    let other = DeviceSecretMaterial::generate_for("device-other").unwrap();
    let invite =
        PairingInvite::create("dataset-pairing", "a".repeat(64), "b".repeat(64), 1_000).unwrap();
    let locator =
        crate::sync::crypto::seal_pairing_json(invite.code(), &("locator", 1_u32)).unwrap();
    let store = HostPairingStore::new(&paths);
    let mut snapshot = store.create(&host, &invite, 0, 1, &locator).unwrap();
    let (request, _) =
        PairingInvite::create_request(invite.code(), "Joining device", &joining, 1_001).unwrap();
    store
        .bind_request(
            &host,
            &mut snapshot,
            &request.encrypted,
            &DeviceId::new(joining.device_id()).unwrap(),
        )
        .unwrap();
    let (replacement, _) =
        PairingInvite::create_request(invite.code(), "Other device", &other, 1_001).unwrap();
    assert!(matches!(
        store.bind_request(
            &host,
            &mut snapshot,
            &replacement.encrypted,
            &DeviceId::new(other.device_id()).unwrap(),
        ),
        Err(VaultError::PairingConsumed)
    ));

    let bytes = crate::util::safe_fs::read_owner_only_limited(
        paths.pairing_host_state(),
        MAX_HOST_STATE_BYTES,
    )
    .unwrap();
    let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    json["dataset_id"] = serde_json::Value::String("dataset-replaced".to_owned());
    crate::util::safe_fs::write_owner_only_atomic(
        paths.pairing_host_state(),
        &serde_json::to_vec(&json).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        store.load(&host),
        Err(VaultError::InvalidPrivateStore)
    ));
}
