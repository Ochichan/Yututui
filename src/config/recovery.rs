use super::*;

pub(super) fn load_from_path(path: &std::path::Path) -> Config {
    let old = old_config_path();
    load_from_path_with_legacy(path, old.as_deref())
}

pub(super) fn load_from_path_with_legacy(
    path: &std::path::Path,
    legacy_path: Option<&std::path::Path>,
) -> Config {
    let (can_persist_default, source_was_missing) =
        match safe_fs::read_no_symlink_limited(path, MAX_CONFIG_BYTES) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        return recover_value(path, value, false, true);
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
        cfg = recover_value(path, value, true, true);
    } else {
        cfg = recover_value(path, value, false, false);
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
fn recover_value(
    path: &std::path::Path,
    value: serde_json::Value,
    install_base: bool,
    allow_persist: bool,
) -> Config {
    recover_value_with_writer(
        path,
        value,
        install_base,
        allow_persist,
        safe_fs::write_private_atomic_json,
    )
}

fn recover_value_with_writer(
    path: &std::path::Path,
    base_value: serde_json::Value,
    install_base: bool,
    allow_persist: bool,
    write: impl Fn(&std::path::Path, &serde_json::Value) -> std::io::Result<()>,
) -> Config {
    recover_value_with_migration_and_writer(
        path,
        base_value,
        install_base,
        allow_persist,
        super::audio::migrate_cache_defaults_json,
        write,
    )
}

fn recover_value_with_migration_and_writer(
    path: &std::path::Path,
    base_value: serde_json::Value,
    install_base: bool,
    allow_persist: bool,
    migrate: impl Fn(&mut serde_json::Value) -> bool,
    write: impl Fn(&std::path::Path, &serde_json::Value) -> std::io::Result<()>,
) -> Config {
    let (replayed_value, replayed) = crate::persist::replay_config_journaled_value_with_status(
        path,
        base_value,
        MAX_CONFIG_BYTES,
    );
    // The persistence layer validates that a replay is an object before returning it, retaining
    // a rejected scalar/array intent for a future compatible reader.
    let mut value = replayed_value;
    let migrated = migrate(&mut value);

    if allow_persist && (replayed || install_base || migrated) {
        match write(path, &value) {
            Ok(()) => {
                if replayed {
                    crate::persist::clear_journaled_snapshot(path);
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "failed to install recovered config snapshot; will retry"
                );
            }
        }
    }

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
    use super::*;

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

    fn migrate_selected_cache(value: &mut serde_json::Value) -> bool {
        super::super::audio::migrate_cache_defaults_json_to_for_test(value, "16MiB", "4MiB", 1)
    }

    #[test]
    fn failed_replay_install_retains_intent_then_retries_and_clears() {
        let path = temp_path("write-failure");
        let mut base = serde_json::to_value(Config::default()).unwrap();
        base["volume"] = serde_json::json!(17);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        safe_fs::write_private_atomic_json(&path, &base).unwrap();

        let mut pending = base.clone();
        pending["volume"] = serde_json::json!(73);
        pending["unknown_pending"] = serde_json::json!({"keep": true});
        write_pending(&path, &pending);

        let recovered = recover_value_with_writer(&path, base.clone(), false, true, |_, _| {
            Err(std::io::Error::other("injected install failure"))
        });
        assert_eq!(recovered.volume, 73, "valid pending state remains usable");
        assert!(crate::persist::test_journal_exists(&path));
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["volume"], 17, "failed install cannot replace the base");

        let recovered = recover_value(&path, disk, false, true);
        assert_eq!(recovered.volume, 73);
        assert!(!crate::persist::test_journal_exists(&path));
        let installed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(installed["volume"], 73);
        assert_eq!(
            installed["unknown_pending"],
            serde_json::json!({"keep": true})
        );

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
            &path,
            base.clone(),
            false,
            true,
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
            &path,
            disk,
            false,
            true,
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

        let recovered = recover_value(&path, base.clone(), false, true);

        assert_eq!(recovered.volume, 29);
        assert!(crate::persist::test_journal_exists(&path));
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk, base);

        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
