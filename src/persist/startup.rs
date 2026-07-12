use super::{StartupRecoveryError, ensure_startup_recovery_coherent};

/// Every non-config store that participates in the process-wide recovery frontier.
///
/// TUI and daemon owners share this loader so neither can start actors, child cleanup, logging,
/// or player effects after checking only the subset of stores it happens to use immediately.
pub struct StartupStoreSet {
    pub(crate) library: crate::library::Library,
    pub(crate) session_cache: crate::session::SessionCache,
    pub(crate) signals: crate::signals::Signals,
    pub(crate) download_store: crate::downloads::DownloadStore,
    pub(crate) playlists: crate::playlists::Playlists,
    pub(crate) playlist_repair: crate::playlists::PlaylistRepairReport,
    pub(crate) station: crate::station::StationStore,
    pub(crate) romanization: crate::romanize::RomanizeCache,
}

fn load_startup_store_set_after_preflight() -> StartupStoreSet {
    let library = crate::library::Library::load();
    let session_cache = crate::session::SessionCache::load();
    let signals = crate::signals::Signals::load();
    let download_store = crate::downloads::DownloadStore::load();
    let (playlists, playlist_repair) = crate::playlists::Playlists::load_with_repair_report();
    let station = crate::station::StationStore::load();
    let romanization = crate::romanize::RomanizeCache::load();
    StartupStoreSet {
        library,
        session_cache,
        signals,
        download_store,
        playlists,
        playlist_repair,
        station,
        romanization,
    }
}

/// Inspect the recovery frontier for every persistent store before any ordinary loader is allowed
/// to back up, repair, migrate, or save bytes.
///
/// This phase deliberately never invokes `Config::load` or a store's normal `load`. Each call only
/// acquires that store's intent lock and validates its journal/sidecar/checksum/decoded candidate.
pub fn preflight_all_startup_stores() -> Result<(), StartupRecoveryError> {
    ensure_startup_recovery_coherent()?;
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
    let stores = load_startup_store_set_after_preflight();
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
    let stores = load_startup_store_set_after_preflight();
    ensure_startup_recovery_coherent()?;
    Ok((config, stores))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
}
