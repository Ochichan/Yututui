use super::{StartupRecoveryError, ensure_startup_recovery_coherent};

/// Every non-config store that participates in the process-wide recovery frontier.
///
/// TUI and daemon owners share this loader so neither can start actors, child cleanup, logging,
/// or player effects after checking only the subset of stores it happens to use immediately.
pub struct StartupStoreSet {
    pub(crate) personal_state: crate::personal_state::PersonalStateV2,
    pub(crate) personal_state_device_id: Option<crate::personal_state::DeviceId>,
    pub(crate) library: crate::library::Library,
    pub(crate) session_cache: crate::session::SessionCache,
    pub(crate) signals: crate::signals::Signals,
    pub(crate) download_store: crate::downloads::DownloadStore,
    pub(crate) playlists: crate::playlists::Playlists,
    pub(crate) playlist_repair: crate::playlists::PlaylistRepairReport,
    pub(crate) station: crate::station::StationStore,
    pub(crate) romanization: crate::romanize::RomanizeCache,
}

fn load_startup_store_set_after_preflight() -> Result<StartupStoreSet, StartupRecoveryError> {
    let library = crate::library::Library::load();
    let session_cache = crate::session::SessionCache::load();
    let signals = crate::signals::Signals::load();
    let download_store = crate::downloads::DownloadStore::load();
    let (playlists, playlist_repair) = crate::playlists::Playlists::load_with_repair_report();
    let station = crate::station::StationStore::load();
    let romanization = crate::romanize::RomanizeCache::load();
    let paths = crate::personal_state::PersonalStatePaths::current()
        .map_err(personal_state_startup_error)?;
    let existing =
        crate::personal_state::load_ledger(&paths).map_err(personal_state_startup_error)?;
    let personal_state = match existing {
        Some(state) => {
            let actual = crate::personal_state::runtime_fingerprint(
                &library, &playlists, &signals, &station,
            )
            .map_err(personal_state_startup_error)?;
            if state.projection_fingerprint.as_deref() != Some(actual.as_str()) {
                return Err(personal_state_startup_error(
                    crate::personal_state::PersonalStateError::ProjectionMismatch,
                ));
            }
            state
        }
        None => {
            let state =
                crate::personal_state::legacy_state(&library, &playlists, &signals, &station)
                    .map_err(personal_state_startup_error)?;
            let commit = crate::personal_state::PersonalStateCommit::prepare(state)
                .map_err(personal_state_startup_error)?;
            if crate::persist::persistence_access().is_read_only() {
                commit.state().clone()
            } else {
                commit
                    .commit(&paths)
                    .map_err(personal_state_startup_error)?
            }
        }
    };
    let sync_private_store = load_sync_private_store_if_present()?;
    let personal_state_device_id =
        active_personal_state_device_id(sync_private_store.as_ref(), &personal_state)?;
    let projection =
        crate::personal_state::project(&personal_state).map_err(personal_state_startup_error)?;
    let (library, playlists, signals, station) = projection.into_runtime();
    Ok(StartupStoreSet {
        personal_state,
        personal_state_device_id,
        library,
        session_cache,
        signals,
        download_store,
        playlists,
        playlist_repair,
        station,
        romanization,
    })
}

/// Inspect the recovery frontier for every persistent store before any ordinary loader is allowed
/// to back up, repair, migrate, or save bytes.
///
/// This phase deliberately never invokes `Config::load` or a store's normal `load`. Each call only
/// acquires that store's intent lock and validates its journal/sidecar/checksum/decoded candidate.
pub fn preflight_all_startup_stores() -> Result<(), StartupRecoveryError> {
    ensure_startup_recovery_coherent()?;
    if let Some(data_root) = crate::paths::data_dir() {
        let paths = crate::personal_state::PersonalStatePaths::for_data_root(data_root);
        crate::personal_state::recover_pending_transactions(&paths)
            .map_err(personal_state_startup_error)?;
        let sync_paths = crate::sync::SyncPaths::for_data_root(paths.data_root.clone());
        crate::sync::service::recover_pending_anchor_transition(&paths, &sync_paths)
            .map_err(sync_private_store_startup_error)?;
    }
    let _ = load_sync_private_store_if_present()?;
    crate::config::Config::preflight_persistence_recovery()?;
    crate::library::Library::preflight_persistence_recovery()?;
    crate::session::SessionCache::preflight_persistence_recovery()?;
    crate::signals::Signals::preflight_persistence_recovery()?;
    crate::downloads::DownloadStore::preflight_persistence_recovery()?;
    crate::playlists::Playlists::preflight_persistence_recovery()?;
    crate::station::StationStore::preflight_persistence_recovery()?;
    crate::romanize::RomanizeCache::preflight_persistence_recovery()?;
    ensure_startup_recovery_coherent()
}

/// Load the non-config startup stores through the mandatory inspect-only phase.
pub fn load_startup_store_set() -> Result<StartupStoreSet, StartupRecoveryError> {
    preflight_all_startup_stores()?;
    let stores = load_startup_store_set_after_preflight()?;
    ensure_startup_recovery_coherent()?;
    Ok(stores)
}

/// Two-phase startup shared by the TUI and daemon: validate every frontier first, then and only
/// then run loaders which may perform recoverable backup/migration work.
pub fn load_verified_startup_state()
-> Result<(crate::config::Config, StartupStoreSet), StartupRecoveryError> {
    preflight_all_startup_stores()?;
    let config = crate::config::Config::load();
    ensure_startup_recovery_coherent()?;
    let stores = load_startup_store_set_after_preflight()?;
    ensure_startup_recovery_coherent()?;
    Ok((config, stores))
}

fn personal_state_startup_error(
    error: crate::personal_state::PersonalStateError,
) -> StartupRecoveryError {
    StartupRecoveryError::artifact(
        crate::persist::StoreKind::PersonalState,
        std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()),
    )
}

fn load_sync_private_store_if_present()
-> Result<Option<crate::sync::PrivateStoreSnapshot>, StartupRecoveryError> {
    let Some(data_root) = crate::paths::data_dir() else {
        return Ok(None);
    };
    let paths = crate::sync::SyncPaths::for_data_root(data_root);
    load_sync_private_store_at(paths.private_store())
}

/// Resolve the exact enrolled device which owns new causal operations for `personal_state`.
///
/// The private store is only used to authenticate the binding. Callers receive the portable
/// device identifier, never credentials, endpoint details, or private key material.
pub(crate) fn load_personal_state_device_id(
    personal_state: &crate::personal_state::PersonalStateV2,
) -> Result<Option<crate::personal_state::DeviceId>, StartupRecoveryError> {
    let private_store = load_sync_private_store_if_present()?;
    active_personal_state_device_id(private_store.as_ref(), personal_state)
}

fn load_sync_private_store_at(
    path: &std::path::Path,
) -> Result<Option<crate::sync::PrivateStoreSnapshot>, StartupRecoveryError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(sync_private_store_startup_error(error)),
    }
    let store = crate::sync::PrivateStore::new(path.to_path_buf())
        .map_err(sync_private_store_startup_error)?;
    store
        .load()
        .map(Some)
        .map_err(sync_private_store_startup_error)
}

fn active_personal_state_device_id(
    private_store: Option<&crate::sync::PrivateStoreSnapshot>,
    personal_state: &crate::personal_state::PersonalStateV2,
) -> Result<Option<crate::personal_state::DeviceId>, StartupRecoveryError> {
    let Some(private_store) = private_store else {
        return Ok(None);
    };
    if private_store.enrollment() != crate::sync::EnrollmentState::Active {
        return Ok(None);
    }
    let device_id = crate::personal_state::DeviceId::new(private_store.device_id())
        .map_err(sync_private_store_startup_error)?;
    let valid_binding = private_store.dataset_id() == personal_state.dataset_id
        && personal_state
            .device_registry
            .get(&device_id)
            .is_some_and(|record| private_store.device().matches_personal_record(record));
    if !valid_binding {
        return Err(sync_private_store_startup_error(
            "active sync device does not match the personal-state ledger",
        ));
    }
    Ok(Some(device_id))
}

fn sync_private_store_startup_error(error: impl std::fmt::Display) -> StartupRecoveryError {
    StartupRecoveryError::artifact(
        crate::persist::StoreKind::PersonalState,
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("sync private store: {error}"),
        ),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{active_personal_state_device_id, load_sync_private_store_at};
    use crate::personal_state::{
        CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
        PersonalStateV2, VersionVector,
    };
    use crate::sync::{
        CheckpointAnchor, MembershipAnchor, MembershipChain, PrivateStore, PrivateStoreSnapshot,
        RecoveryKit, SignedCheckpoint, SignedMembershipRoot, SyncPaths,
    };

    struct RecoveryLatchReset;

    impl Drop for RecoveryLatchReset {
        fn drop(&mut self) {
            crate::persist::clear_startup_recovery_error_for_test();
        }
    }

    fn test_dir() -> std::path::PathBuf {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let suffix = random
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let directory = std::env::temp_dir().join(format!(
            "yututui-two-phase-preflight-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn snapshot_without_lock_files(directory: &Path) -> Vec<(String, Vec<u8>)> {
        let mut snapshot = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                (name, entry.path())
            })
            .filter(|(name, _)| !name.ends_with(".intent.lock"))
            .map(|(name, path)| (name, std::fs::read(path).unwrap()))
            .collect::<Vec<_>>();
        snapshot.sort_by(|left, right| left.0.cmp(&right.0));
        snapshot
    }

    fn install_active_sync_private_store(
        data_root: &Path,
    ) -> (PersonalStateV2, DeviceId, std::path::PathBuf) {
        let kit = RecoveryKit::generate("startup-sync-test", None).unwrap();
        let device = crate::sync::DeviceSecretMaterial::generate_for("startup-device").unwrap();
        let device_id = DeviceId::new(device.device_id()).unwrap();
        let device_record = DeviceRecord {
            device_id: device_id.clone(),
            name: "Startup device".to_owned(),
            revoked: false,
            public_identity: Some(device.public_identity()),
        };
        let recovery_signer = kit.signing_key().unwrap();
        let root = SignedMembershipRoot::create(
            "startup-sync-test",
            kit.recovery_recipient(),
            &recovery_signer,
            device_record.clone(),
        )
        .unwrap();
        let root_hash = root.hash().unwrap();
        let membership = MembershipChain::new(root);
        let dot = Dot {
            device_id: device_id.clone(),
            sequence: 1,
        };
        let mut state = PersonalStateV2::empty("startup-sync-test".to_owned()).unwrap();
        state.operations.push(OperationEnvelope {
            operation_id: "startup-device-enrollment".to_owned(),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: VersionVector::default(),
                recorded_at_unix: 0,
            },
            origin: OperationOrigin::Local,
            operation: Operation::AddDevice {
                device: device_record,
            },
        });
        state.version_vector.observe(&dot);
        crate::personal_state::refresh_device_registry(&mut state).unwrap();
        state.normalize().unwrap();
        let membership_anchor = MembershipAnchor::RootHash(root_hash.clone());
        let checkpoint = SignedCheckpoint::create(
            membership,
            &membership_anchor,
            device_id.clone(),
            device.signing_key(),
            &CheckpointAnchor::default(),
            state.clone(),
        )
        .unwrap();
        let mut private = PrivateStoreSnapshot::pending_ledger_commit(
            device,
            kit.recovery_recipient(),
            kit.recovery_verifying_key().unwrap(),
            root_hash,
            &checkpoint,
        )
        .unwrap();
        private.mark_active(&checkpoint, &state).unwrap();

        let paths = SyncPaths::for_data_root(data_root.to_path_buf());
        crate::util::safe_fs::ensure_private_dir(paths.root()).unwrap();
        let private_path = paths.private_store().to_path_buf();
        PrivateStore::new(private_path.clone())
            .unwrap()
            .create(&mut private)
            .unwrap();
        (state, device_id, private_path)
    }

    #[test]
    fn later_unverifiable_frontier_cannot_trigger_earlier_config_backup_or_save() {
        let _reset = RecoveryLatchReset;
        crate::persist::clear_startup_recovery_error_for_test();
        let directory = test_dir();
        let config = directory.join("config.json");
        let station = directory.join("station.json");
        let corrupt_config = b"{not-json-with-secret-sentinel";
        std::fs::write(&config, corrupt_config).unwrap();
        std::fs::write(&station, br#"{"active":null}"#).unwrap();
        let journal = crate::persist::intent_journal_path(&station).unwrap();
        let missing_sidecar = "station.json.intent.missing.json";
        let record = serde_json::json!({
            "v": 1,
            "op": "replace",
            "kind": crate::persist::StoreKind::Station.label(),
            "sidecar": missing_sidecar,
            "sha256": "00"
        });
        std::fs::write(&journal, format!("{record}\n")).unwrap();
        let before = snapshot_without_lock_files(&directory);

        crate::persist::preflight_journal_recovery::<crate::config::Config>(
            crate::persist::StoreKind::Config,
            &config,
            1024 * 1024,
        )
        .expect("a recoverable corrupt base is inspected without running its loader");
        let error = crate::persist::preflight_journal_recovery::<crate::station::StationStore>(
            crate::persist::StoreKind::Station,
            &station,
            16 * 1024 * 1024,
        )
        .expect_err("the later missing authoritative sidecar must abort phase one");

        assert_eq!(error.store, crate::persist::StoreKind::Station);
        assert_eq!(std::fs::read(&config).unwrap(), corrupt_config);
        assert_eq!(snapshot_without_lock_files(&directory), before);
        assert!(
            crate::util::safe_fs::recovery_backups(&config)
                .unwrap()
                .is_empty()
        );
        assert!(!directory.join(missing_sidecar).exists());

        crate::persist::clear_startup_recovery_error_for_test();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn active_sync_private_store_binds_the_exact_ledger_device() {
        let directory = test_dir();
        let (state, expected_device, private_path) = install_active_sync_private_store(&directory);

        let private = load_sync_private_store_at(&private_path)
            .unwrap()
            .expect("active private store");
        assert_eq!(
            active_personal_state_device_id(Some(&private), &state).unwrap(),
            Some(expected_device)
        );

        let mut wrong_dataset = state;
        wrong_dataset.dataset_id = "other-dataset".to_owned();
        assert!(
            active_personal_state_device_id(Some(&private), &wrong_dataset).is_err(),
            "an active private identity must not bind to another ledger"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_private_store_is_legacy_but_existing_corruption_fails_closed() {
        let directory = test_dir();
        let private_path = directory.join("vault-private-v1.json");
        assert!(load_sync_private_store_at(&private_path).unwrap().is_none());

        crate::util::safe_fs::write_owner_only_atomic(&private_path, b"{not-valid-json").unwrap();
        let error = load_sync_private_store_at(&private_path)
            .err()
            .expect("an existing private store must be validated");
        assert_eq!(error.store, crate::persist::StoreKind::PersonalState);
        let _ = std::fs::remove_dir_all(directory);
    }
}
