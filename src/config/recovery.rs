use super::*;

pub(super) fn load_from_path(path: &std::path::Path) -> Config {
    let old = old_config_path();
    load_from_path_with_legacy(path, old.as_deref())
}

pub(super) fn load_from_path_with_legacy(
    path: &std::path::Path,
    legacy_path: Option<&std::path::Path>,
) -> Config {
    let recovery = crate::persist::begin_config_recovery(path);
    let base = recovery.read_base(MAX_CONFIG_BYTES);
    load_from_path_with_legacy_after_read(recovery, path, legacy_path, base)
}

#[cfg(test)]
fn load_from_path_with_legacy_using(
    path: &std::path::Path,
    legacy_path: Option<&std::path::Path>,
    read: impl FnOnce(&std::path::Path, u64) -> std::io::Result<Vec<u8>>,
) -> Config {
    let recovery = crate::persist::begin_config_recovery(path);
    let base = recovery.read_base_with(MAX_CONFIG_BYTES, read);
    load_from_path_with_legacy_after_read(recovery, path, legacy_path, base)
}

fn load_from_path_with_legacy_after_read(
    recovery: crate::persist::ConfigRecoveryGuard,
    path: &std::path::Path,
    legacy_path: Option<&std::path::Path>,
    base: std::io::Result<Vec<u8>>,
) -> Config {
    let (can_persist_default, source_was_missing) = match base {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    return recover_value(recovery, value, InstallPolicy::IfChanged);
                }
                (safe_fs::backup_aside_secret(path).is_ok(), false)
            }
            Err(_) => (safe_fs::backup_aside_secret(path).is_ok(), false),
        },
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            (safe_fs::backup_too_large_secret(path).is_ok(), false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (true, true),
        Err(_) => (safe_fs::backup_unreadable_secret(path).is_ok(), false),
    };

    let mut cfg = if source_was_missing {
        config_for_missing_profile(legacy_path)
    } else {
        Config::default()
    };
    if let Some(old) = legacy_path {
        import_old_from(old, &mut cfg);
    }
    let value = match serde_json::to_value(&cfg) {
        Ok(value) => value,
        Err(error) => {
            tracing::error!(
                error = %error,
                "failed to encode recovered config; keeping the in-memory recovery"
            );
            return cfg;
        }
    };
    if can_persist_default {
        cfg = recover_value(recovery, value, InstallPolicy::Always);
    } else {
        cfg = recover_value(recovery, value, InstallPolicy::Forbidden);
        tracing::error!(
            path = %path.display(),
            "refusing to overwrite unreadable/corrupt config because recovery backup failed"
        );
    }
    cfg
}

/// Replay as raw JSON, then migrate before typed recovery. This preserves unknown keys in both the
/// primary config and a pending sidecar. A replay is cleared only after its effective raw object is
/// atomically installed; any failed write leaves the old marker/journal available for retry.
#[derive(Clone, Copy)]
enum InstallPolicy {
    Always,
    IfChanged,
    Forbidden,
}

struct PreparedConfigRecovery {
    transaction: crate::persist::ConfigRecoveryTransaction,
    should_install: bool,
}

impl PreparedConfigRecovery {
    fn complete(self) -> serde_json::Value {
        if self.should_install {
            finish_install(self.transaction.install_and_settle())
        } else {
            self.transaction.into_value()
        }
    }

    #[cfg(test)]
    fn complete_with(
        self,
        write: impl FnOnce(&std::path::Path, &serde_json::Value) -> std::io::Result<()>,
    ) -> serde_json::Value {
        if self.should_install {
            finish_install(self.transaction.install_and_settle_with(write))
        } else {
            self.transaction.into_value()
        }
    }
}

fn finish_install((value, result): (serde_json::Value, std::io::Result<()>)) -> serde_json::Value {
    if let Err(error) = result {
        tracing::warn!(
            error = %error,
            "failed to install recovered config snapshot; will retry"
        );
    }
    value
}

fn prepare_recovery(
    recovery: crate::persist::ConfigRecoveryGuard,
    base_value: serde_json::Value,
    policy: InstallPolicy,
    migrate: impl FnOnce(&mut serde_json::Value) -> bool,
) -> PreparedConfigRecovery {
    let mut recovery = recovery.replay(base_value, MAX_CONFIG_BYTES);
    // The persistence layer validates that a replay is an object before returning it, retaining
    // a rejected scalar/array intent for a future compatible reader.
    let replayed = recovery.has_replayed_candidate();
    let migrated = migrate(recovery.value_mut());
    let should_install = match policy {
        InstallPolicy::Always => true,
        InstallPolicy::IfChanged => replayed || migrated,
        InstallPolicy::Forbidden => false,
    };
    PreparedConfigRecovery {
        transaction: recovery,
        should_install,
    }
}

fn recover_value(
    recovery: crate::persist::ConfigRecoveryGuard,
    value: serde_json::Value,
    policy: InstallPolicy,
) -> Config {
    let value = prepare_recovery(
        recovery,
        value,
        policy,
        super::audio::migrate_cache_defaults_json,
    )
    .complete();

    decode_config(value)
}

#[cfg(test)]
fn recover_value_with_writer(
    recovery: crate::persist::ConfigRecoveryGuard,
    base_value: serde_json::Value,
    policy: InstallPolicy,
    write: impl FnOnce(&std::path::Path, &serde_json::Value) -> std::io::Result<()>,
) -> Config {
    recover_value_with_migration_and_writer(
        recovery,
        base_value,
        policy,
        super::audio::migrate_cache_defaults_json,
        write,
    )
}

#[cfg(test)]
fn recover_value_with_migration_and_writer(
    recovery: crate::persist::ConfigRecoveryGuard,
    base_value: serde_json::Value,
    policy: InstallPolicy,
    migrate: impl FnOnce(&mut serde_json::Value) -> bool,
    write: impl FnOnce(&std::path::Path, &serde_json::Value) -> std::io::Result<()>,
) -> Config {
    let value = prepare_recovery(recovery, base_value, policy, migrate).complete_with(write);
    decode_config(value)
}

fn decode_config(value: serde_json::Value) -> Config {
    // Fast path when the schema is current; otherwise keep every field that still fits instead of
    // resetting the whole config. The typed fallback mirrors the raw rule defensively for a value
    // whose marker shape could not be interpreted above.
    let mut cfg = serde_json::from_value::<Config>(value.clone())
        .unwrap_or_else(|_| safe_fs::recover_lenient::<Config>(value));
    cfg.audio.mpv.migrate_cache_defaults();
    cfg
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use sha2::{Digest, Sha256};

    use super::*;

    const TEST_SYNC_TIMEOUT: Duration = Duration::from_secs(10);

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        std::env::temp_dir()
            .join(format!("yututui-config-recovery-{name}-{suffix}"))
            .join("config.json")
    }

    fn write_pending(path: &std::path::Path, value: &serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::persist::write_test_journaled_snapshot(
            crate::persist::StoreKind::Config,
            path.to_owned(),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }

    fn sibling(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
        let name = path.file_name().unwrap().to_str().unwrap();
        path.with_file_name(format!("{name}{suffix}"))
    }

    fn journal_path(path: &std::path::Path) -> std::path::PathBuf {
        sibling(path, ".intent.jsonl")
    }

    fn journal_records(path: &std::path::Path) -> Vec<serde_json::Value> {
        let Ok(text) = std::fs::read_to_string(journal_path(path)) else {
            return Vec::new();
        };
        text.lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                serde_json::from_str(line).unwrap_or_else(|error| {
                    panic!("invalid journal record at line {}: {error}", index + 1)
                })
            })
            .collect()
    }

    fn record_order(record: &serde_json::Value) -> (String, String, String) {
        (
            record["generation"].as_str().unwrap().to_owned(),
            record["process_epoch"].as_str().unwrap().to_owned(),
            record["sequence"].as_str().unwrap().to_owned(),
        )
    }

    fn record_sidecar(path: &std::path::Path, record: &serde_json::Value) -> std::path::PathBuf {
        path.parent()
            .unwrap()
            .join(record["sidecar"].as_str().unwrap())
    }

    fn intent_lock_is_held(path: &std::path::Path) -> bool {
        match safe_fs::try_lock_private_file(&sibling(path, ".intent.lock")).unwrap() {
            Some(lock) => {
                drop(lock);
                false
            }
            None => true,
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn migrate_selected_cache(value: &mut serde_json::Value) -> bool {
        super::super::audio::migrate_cache_defaults_json_to_for_test(value, "16MiB", "4MiB", 1)
    }

    fn assert_failed_replay_install_is_retryable(name: &str, visible_before_error: bool) {
        let path = temp_path(name);
        let mut base = serde_json::to_value(Config::default()).unwrap();
        base["volume"] = serde_json::json!(17);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        safe_fs::write_private_atomic_json(&path, &base).unwrap();

        let mut pending = base.clone();
        pending["volume"] = serde_json::json!(73);
        pending["unknown_pending"] = serde_json::json!({"keep": true});
        write_pending(&path, &pending);
        let original_journal = std::fs::read(journal_path(&path)).unwrap();
        let original_records = journal_records(&path);
        assert_eq!(original_records.len(), 1);
        assert_eq!(original_records[0]["op"], "replace");
        let original_order = record_order(&original_records[0]);
        let original_sidecar = record_sidecar(&path, &original_records[0]);
        let original_sidecar_bytes = std::fs::read(&original_sidecar).unwrap();

        let recovered = recover_value_with_writer(
            crate::persist::begin_config_recovery(&path),
            base.clone(),
            InstallPolicy::IfChanged,
            |path, value| {
                if visible_before_error {
                    safe_fs::write_private_atomic_json(path, value)?;
                }
                Err(std::io::Error::other("injected install failure"))
            },
        );
        assert_eq!(recovered.volume, 73, "valid pending state remains usable");
        assert!(crate::persist::test_journal_exists(&path));
        assert_eq!(
            std::fs::read(journal_path(&path)).unwrap(),
            original_journal
        );
        assert_eq!(
            std::fs::read(&original_sidecar).unwrap(),
            original_sidecar_bytes
        );
        assert!(
            !intent_lock_is_held(&path),
            "failed installation must release coherent ownership"
        );
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            disk["volume"],
            if visible_before_error { 73 } else { 17 },
            "the journal must remain retryable whether the failed write became visible or not"
        );

        let recovered = recover_value(
            crate::persist::begin_config_recovery(&path),
            disk,
            InstallPolicy::IfChanged,
        );
        assert_eq!(recovered.volume, 73);
        assert!(!crate::persist::test_journal_exists(&path));
        let installed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed["volume"], 73);
        assert_eq!(
            installed["unknown_pending"],
            serde_json::json!({"keep": true})
        );
        let settled_records = journal_records(&path);
        assert_eq!(settled_records.len(), 1);
        assert_eq!(settled_records[0]["op"], "commit");
        assert_eq!(record_order(&settled_records[0]), original_order);
        assert!(!original_sidecar.exists());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_pre_visible_replay_install_retains_exact_intent_for_retry() {
        assert_failed_replay_install_is_retryable("pre-visible-write-failure", false);
    }

    #[test]
    fn failed_post_visible_replay_install_retains_exact_intent_for_retry() {
        assert_failed_replay_install_is_retryable("post-visible-write-failure", true);
    }

    #[test]
    fn changed_candidate_receipt_is_not_settled_after_install() {
        let path = temp_path("receipt-changed-before-settle");
        let mut base = serde_json::to_value(Config::default()).unwrap();
        base["volume"] = serde_json::json!(17);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        safe_fs::write_private_atomic_json(&path, &base).unwrap();

        let mut pending = base.clone();
        pending["volume"] = serde_json::json!(73);
        pending["unknown_pending"] = serde_json::json!({"keep": true});
        write_pending(&path, &pending);
        let original_record = journal_records(&path).pop().unwrap();
        let original_order = record_order(&original_record);
        let sidecar = record_sidecar(&path, &original_record);
        let mut changed_record = original_record;
        changed_record["unexpected_interference"] = serde_json::json!(true);
        let changed_journal = format!("{changed_record}\n").into_bytes();
        let journal = journal_path(&path);

        let recovered = recover_value_with_writer(
            crate::persist::begin_config_recovery(&path),
            base,
            InstallPolicy::IfChanged,
            move |path, value| {
                safe_fs::write_private_atomic_json(path, value)?;
                safe_fs::write_private_atomic(&journal, &changed_journal)
            },
        );
        assert_eq!(recovered.volume, 73);
        assert!(crate::persist::test_journal_exists(&path));
        let records = journal_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["op"], "replace");
        assert_eq!(records[0]["unexpected_interference"], true);
        assert!(sidecar.exists());

        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let recovered = recover_value(
            crate::persist::begin_config_recovery(&path),
            disk,
            InstallPolicy::IfChanged,
        );
        assert_eq!(recovered.volume, 73);
        let records = journal_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["op"], "commit");
        assert_eq!(record_order(&records[0]), original_order);
        assert!(!sidecar.exists());

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn config_base_is_read_while_coherent_recovery_lock_is_held() {
        crate::persist::clear_startup_recovery_error_for_test();
        let path = temp_path("base-read-lock");
        let mut base = serde_json::to_value(Config::default()).unwrap();
        base["volume"] = serde_json::json!(31);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        safe_fs::write_private_atomic_json(&path, &base).unwrap();

        let observed_lock = Arc::new(AtomicBool::new(false));
        let observed_in_reader = Arc::clone(&observed_lock);
        let loaded = load_from_path_with_legacy_using(&path, None, move |path, max_bytes| {
            observed_in_reader.store(intent_lock_is_held(path), Ordering::SeqCst);
            safe_fs::read_no_symlink_limited(path, max_bytes)
        });

        assert!(
            observed_lock.load(Ordering::SeqCst),
            "the base must not be exposed before coherent ownership is acquired"
        );
        assert_eq!(loaded.volume, 31);
        assert!(!intent_lock_is_held(&path));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn direct_config_save_after_missing_base_read_wins_after_recovery_install() {
        crate::persist::clear_startup_recovery_error_for_test();
        let path = temp_path("direct-save-after-read");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        let newer = Config {
            volume: 88,
            ..Config::default()
        };
        let (start_tx, start_rx) = mpsc::channel();
        let (contended_tx, contended_rx) = mpsc::channel();
        let writer_path = path.clone();
        let observed_lock = Arc::new(AtomicBool::new(false));
        let read_observation = Arc::clone(&observed_lock);
        let loaded = std::thread::scope(|scope| {
            let writer = scope.spawn(move || {
                start_rx.recv().unwrap();
                crate::persist::with_intent_lock_contention_observer(contended_tx, || {
                    newer.save_to(&writer_path).unwrap();
                });
            });
            let loaded = load_from_path_with_legacy_using(&path, None, move |_, _| {
                start_tx.send(()).unwrap();
                contended_rx.recv_timeout(TEST_SYNC_TIMEOUT).unwrap();
                read_observation.store(true, Ordering::SeqCst);
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "injected missing-base cutoff",
                ))
            });
            writer.join().unwrap();
            loaded
        });

        assert!(observed_lock.load(Ordering::SeqCst));
        assert_eq!(loaded.volume, Config::fresh_install().volume);
        let installed: Config = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed.volume, 88, "the later direct save must win");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn newer_config_intent_waits_for_replayed_candidate_to_settle() {
        crate::persist::clear_startup_recovery_error_for_test();
        let path = temp_path("newer-intent-waits");
        let mut base = serde_json::to_value(Config::default()).unwrap();
        base["volume"] = serde_json::json!(17);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        safe_fs::write_private_atomic_json(&path, &base).unwrap();

        let mut replayed = base.clone();
        replayed["volume"] = serde_json::json!(73);
        replayed["unknown_replayed"] = serde_json::json!({"keep": "x"});
        write_pending(&path, &replayed);
        let original_record = journal_records(&path).pop().unwrap();
        let replayed_order = record_order(&original_record);
        let replayed_sidecar = record_sidecar(&path, &original_record);

        let mut newer = base.clone();
        newer["volume"] = serde_json::json!(91);
        newer["unknown_newer"] = serde_json::json!({"keep": "y"});
        let newer_bytes = serde_json::to_vec_pretty(&newer).unwrap();

        let (start_tx, start_rx) = mpsc::channel();
        let (contended_tx, contended_rx) = mpsc::channel();
        let writer_path = path.clone();
        let writer_value = newer.clone();
        let lock_seen_during_migration = Arc::new(AtomicBool::new(false));
        let migration_observation = Arc::clone(&lock_seen_during_migration);
        let lock_seen_during_install = Arc::new(AtomicBool::new(false));
        let install_observation = Arc::clone(&lock_seen_during_install);
        let recovered = std::thread::scope(|scope| {
            let newer_writer = scope.spawn(move || {
                start_rx.recv().unwrap();
                crate::persist::with_intent_lock_contention_observer(contended_tx, || {
                    write_pending(&writer_path, &writer_value);
                });
            });
            let recovered = recover_value_with_migration_and_writer(
                crate::persist::begin_config_recovery(&path),
                base,
                InstallPolicy::IfChanged,
                move |_| {
                    start_tx.send(()).unwrap();
                    contended_rx.recv_timeout(TEST_SYNC_TIMEOUT).unwrap();
                    migration_observation.store(true, Ordering::SeqCst);
                    false
                },
                move |path, value| {
                    install_observation.store(intent_lock_is_held(path), Ordering::SeqCst);
                    safe_fs::write_private_atomic_json(path, value)
                },
            );
            newer_writer.join().unwrap();
            recovered
        });

        assert!(lock_seen_during_migration.load(Ordering::SeqCst));
        assert!(lock_seen_during_install.load(Ordering::SeqCst));
        assert_eq!(recovered.volume, 73);
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk, replayed);

        let records = journal_records(&path);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["op"], "commit");
        assert_eq!(record_order(&records[0]), replayed_order);
        assert_eq!(records[1]["op"], "replace");
        let newer_order = record_order(&records[1]);
        assert_ne!(newer_order, replayed_order);
        let newer_sidecar = record_sidecar(&path, &records[1]);
        assert!(!replayed_sidecar.exists());
        assert_eq!(std::fs::read(&newer_sidecar).unwrap(), newer_bytes);
        assert!(crate::persist::test_journal_exists(&path));

        let recovered = recover_value(
            crate::persist::begin_config_recovery(&path),
            disk,
            InstallPolicy::IfChanged,
        );
        assert_eq!(recovered.volume, 91);
        let installed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed, newer);
        let records = journal_records(&path);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["op"], "commit");
        assert_eq!(record_order(&records[0]), newer_order);
        assert!(!newer_sidecar.exists());
        assert!(!crate::persist::test_journal_exists(&path));

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_legacy_replay_keeps_exact_record_until_successful_install() {
        let path = temp_path("legacy-retry");
        let mut base = serde_json::to_value(Config::default()).unwrap();
        base["volume"] = serde_json::json!(11);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        safe_fs::write_private_atomic_json(&path, &base).unwrap();

        let mut pending = base.clone();
        pending["volume"] = serde_json::json!(67);
        pending["unknown_legacy"] = serde_json::json!({"keep": true});
        let pending_bytes = serde_json::to_vec_pretty(&pending).unwrap();
        let sidecar = sibling(&path, ".intent.latest.json");
        safe_fs::write_private_atomic(&sidecar, &pending_bytes).unwrap();
        let record = serde_json::json!({
            "v": 1,
            "op": "replace",
            "kind": "config",
            "sidecar": sidecar.file_name().unwrap().to_str().unwrap(),
            "sha256": sha256_hex(&pending_bytes),
        });
        let journal = format!("{record}\n").into_bytes();
        safe_fs::write_private_atomic(&journal_path(&path), &journal).unwrap();

        let recovered = recover_value_with_writer(
            crate::persist::begin_config_recovery(&path),
            base.clone(),
            InstallPolicy::IfChanged,
            |_, _| Err(std::io::Error::other("injected legacy install failure")),
        );
        assert_eq!(recovered.volume, 67);
        assert_eq!(std::fs::read(journal_path(&path)).unwrap(), journal);
        assert_eq!(std::fs::read(&sidecar).unwrap(), pending_bytes);

        let recovered = recover_value(
            crate::persist::begin_config_recovery(&path),
            base,
            InstallPolicy::IfChanged,
        );
        assert_eq!(recovered.volume, 67);
        let installed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed, pending);
        assert!(!journal_path(&path).exists());
        #[cfg(unix)]
        assert!(!sidecar.exists());
        #[cfg(not(unix))]
        assert_eq!(std::fs::read(&sidecar).unwrap(), pending_bytes);

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_simulated_future_migration_leaves_the_unmarked_source_retryable() {
        let path = temp_path("migration-write-failure");
        let mut base = serde_json::to_value(Config::default()).unwrap();
        base["audio"]["mpv"]
            .as_object_mut()
            .unwrap()
            .remove("_cache_defaults_revision");
        base["unknown_primary"] = serde_json::json!({"keep": [1, 2, 3]});
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        safe_fs::write_private_atomic_json(&path, &base).unwrap();

        let recovered = recover_value_with_migration_and_writer(
            crate::persist::begin_config_recovery(&path),
            base.clone(),
            InstallPolicy::IfChanged,
            migrate_selected_cache,
            |_, _| Err(std::io::Error::other("injected migration failure")),
        );
        assert_eq!(recovered.audio.mpv.cache_defaults_revision, 1);
        assert_eq!(recovered.audio.mpv.cache_forward, "16MiB");
        assert_eq!(recovered.audio.mpv.cache_back, "4MiB");
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            disk["audio"]["mpv"]
                .get("_cache_defaults_revision")
                .is_none()
        );

        assert_eq!(disk["audio"]["mpv"]["cache_forward"], "32MiB");
        assert_eq!(disk["audio"]["mpv"]["cache_back"], "8MiB");

        let recovered = recover_value_with_migration_and_writer(
            crate::persist::begin_config_recovery(&path),
            disk,
            InstallPolicy::IfChanged,
            migrate_selected_cache,
            safe_fs::write_private_atomic_json,
        );
        assert_eq!(recovered.audio.mpv.cache_defaults_revision, 1);
        assert_eq!(recovered.audio.mpv.cache_forward, "16MiB");
        assert_eq!(recovered.audio.mpv.cache_back, "4MiB");
        let installed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed["audio"]["mpv"]["_cache_defaults_revision"], 1);
        assert_eq!(installed["audio"]["mpv"]["cache_forward"], "16MiB");
        assert_eq!(installed["audio"]["mpv"]["cache_back"], "4MiB");
        assert_eq!(
            installed["unknown_primary"],
            serde_json::json!({"keep": [1, 2, 3]})
        );

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn non_object_replay_is_neither_installed_nor_cleared() {
        let path = temp_path("invalid-root");
        let mut base = serde_json::to_value(Config::default()).unwrap();
        base["volume"] = serde_json::json!(29);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        safe_fs::write_private_atomic_json(&path, &base).unwrap();
        write_pending(&path, &serde_json::json!(["not", "a", "config"]));

        let recovered = recover_value(
            crate::persist::begin_config_recovery(&path),
            base.clone(),
            InstallPolicy::IfChanged,
        );

        assert_eq!(recovered.volume, 29);
        assert!(crate::persist::test_journal_exists(&path));
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk, base);

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
