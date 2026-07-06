//! Small last-session cache: UI mode and queue snapshots restored on the next launch.
//!
//! Track/station history still lives in the library. This cache stores the session-shaped
//! state that history cannot reconstruct: the active mode plus the queue order/cursor for
//! normal and dedicated Radio mode. Because it is transient playback state, an app/schema
//! version mismatch discards it instead of trying to reinterpret stale queue entries.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::queue::QueueSnapshot;
use crate::util::safe_fs;

const SESSION_CACHE_FILE: &str = "session.json";
const SESSION_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastMode {
    #[default]
    Normal,
    Radio,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionCache {
    pub schema_version: u32,
    pub app_version: String,
    pub last_mode: LastMode,
    pub normal_queue: Option<QueueSnapshot>,
    pub radio_queue: Option<QueueSnapshot>,
}

impl Default for SessionCache {
    fn default() -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            last_mode: LastMode::Normal,
            normal_queue: None,
            radio_queue: None,
        }
    }
}

impl SessionCache {
    pub fn from_radio_mode(radio_mode: bool) -> Self {
        Self {
            last_mode: if radio_mode {
                LastMode::Radio
            } else {
                LastMode::Normal
            },
            ..Self::default()
        }
    }

    pub fn was_radio_mode(&self) -> bool {
        self.last_mode == LastMode::Radio
    }

    pub fn active_queue(&self) -> Option<&QueueSnapshot> {
        match self.last_mode {
            LastMode::Normal => self.normal_queue.as_ref(),
            LastMode::Radio => self.radio_queue.as_ref(),
        }
        .filter(|snapshot| !snapshot.is_empty())
    }

    pub fn load() -> Self {
        let Some(path) = session_cache_path() else {
            return Self::default();
        };
        Self::load_from_path(&path)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = session_cache_path() else {
            return Ok(());
        };
        self.save_to_path(&path)
    }

    pub fn clear() -> std::io::Result<bool> {
        let Some(path) = session_cache_path() else {
            return Ok(false);
        };
        match std::fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn load_from_path(path: &Path) -> Self {
        // A corrupt/duplicated/synced multi-GB session.json is set aside (never slurped into
        // memory at startup), mirroring the size-capped config load; the queue snapshot it
        // holds is itself capped on restore.
        const MAX_BYTES: u64 = 32 * 1024 * 1024;
        let cache = safe_fs::load_json_or_default_limited::<SessionCache>(path, MAX_BYTES);
        if cache.schema_version != SESSION_SCHEMA_VERSION
            || cache.app_version != env!("CARGO_PKG_VERSION")
        {
            tracing::info!(
                schema = cache.schema_version,
                app_version = %cache.app_version,
                "discarding stale session cache"
            );
            return Self::default();
        }
        cache
    }

    fn save_to_path(&self, path: &Path) -> std::io::Result<()> {
        safe_fs::write_private_atomic_json(path, self)
    }
}

fn session_cache_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui")
        .map(|d| d.cache_dir().join(SESSION_CACHE_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_radio_mode_records_last_mode() {
        assert_eq!(
            SessionCache::from_radio_mode(false).last_mode,
            LastMode::Normal
        );
        assert_eq!(
            SessionCache::from_radio_mode(true).last_mode,
            LastMode::Radio
        );
        assert!(SessionCache::from_radio_mode(true).was_radio_mode());
    }

    #[test]
    fn missing_or_invalid_cache_defaults_to_normal_mode() {
        let path =
            std::env::temp_dir().join(format!("ytm-tui-session-missing-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            SessionCache::load_from_path(&path).last_mode,
            LastMode::Normal
        );
    }

    #[test]
    fn stale_schema_or_app_version_discards_transient_cache() {
        let path =
            std::env::temp_dir().join(format!("ytm-tui-session-stale-{}", std::process::id()));
        std::fs::write(
            &path,
            r#"{"schema_version":1,"app_version":"0.0.0","last_mode":"radio"}"#,
        )
        .unwrap();

        assert_eq!(
            SessionCache::load_from_path(&path).last_mode,
            LastMode::Normal
        );
        let _ = std::fs::remove_file(&path);
    }
}
