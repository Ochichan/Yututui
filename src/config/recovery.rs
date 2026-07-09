use super::*;

pub(super) fn load_from_path(path: &std::path::Path) -> Config {
    let can_persist_default = match safe_fs::read_no_symlink_limited(path, MAX_CONFIG_BYTES) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => {
                // Fast path: schema unchanged since this file was written.
                if let Ok(cfg) = serde_json::from_str::<Config>(&text) {
                    return crate::persist::replay_journaled_snapshot(
                        crate::persist::StoreKind::Config,
                        path,
                        cfg,
                        MAX_CONFIG_BYTES,
                    );
                }
                // Schema drifted: keep every field that still fits instead of resetting.
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    let cfg = safe_fs::recover_lenient::<Config>(value);
                    return crate::persist::replay_journaled_snapshot(
                        crate::persist::StoreKind::Config,
                        path,
                        cfg,
                        MAX_CONFIG_BYTES,
                    );
                }
                safe_fs::backup_aside_secret(path).is_ok()
            }
            Err(_) => safe_fs::backup_aside_secret(path).is_ok(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            safe_fs::backup_too_large_secret(path).is_ok()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => safe_fs::backup_unreadable_secret(path).is_ok(),
    };

    let mut cfg = Config::default();
    if let Some(old) = old_config_path() {
        import_old_from(&old, &mut cfg);
    }
    cfg = crate::persist::replay_journaled_snapshot(
        crate::persist::StoreKind::Config,
        path,
        cfg,
        MAX_CONFIG_BYTES,
    );
    if can_persist_default {
        let _ = cfg.save_to(path);
    } else {
        tracing::error!(
            path = %path.display(),
            "refusing to overwrite unreadable/corrupt config because recovery backup failed"
        );
    }
    cfg
}
