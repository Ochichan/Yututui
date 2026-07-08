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
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionCache {
    pub schema_version: u32,
    pub app_version: String,
    pub last_mode: LastMode,
    pub normal_queue: Option<QueueSnapshot>,
    pub radio_queue: Option<QueueSnapshot>,
    pub local_queue: Option<QueueSnapshot>,
}

impl Default for SessionCache {
    fn default() -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            last_mode: LastMode::Normal,
            normal_queue: None,
            radio_queue: None,
            local_queue: None,
        }
    }
}

impl SessionCache {
    pub fn from_last_mode(last_mode: LastMode) -> Self {
        Self {
            last_mode,
            ..Self::default()
        }
    }

    pub fn from_radio_mode(radio_mode: bool) -> Self {
        Self::from_last_mode(if radio_mode {
            LastMode::Radio
        } else {
            LastMode::Normal
        })
    }

    pub fn was_radio_mode(&self) -> bool {
        self.last_mode == LastMode::Radio
    }

    pub fn was_local_mode(&self) -> bool {
        self.last_mode == LastMode::Local
    }

    pub fn active_queue(&self) -> Option<&QueueSnapshot> {
        match self.last_mode {
            LastMode::Normal => self.normal_queue.as_ref(),
            LastMode::Radio => self.radio_queue.as_ref(),
            LastMode::Local => self.local_queue.as_ref(),
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
    if let Some(dir) = env_cache_dir() {
        return Some(dir.join(SESSION_CACHE_FILE));
    }
    directories::ProjectDirs::from("", "", "ytm-tui")
        .map(|d| d.cache_dir().join(SESSION_CACHE_FILE))
}

fn env_cache_dir() -> Option<PathBuf> {
    let raw = std::env::var("YTM_CACHE_DIR").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;

    fn snapshot(id: &str) -> QueueSnapshot {
        QueueSnapshot {
            songs: vec![Song::remote(id, format!("title-{id}"), "artist", "3:00")],
            order: vec![0],
            cursor: 0,
            shuffle: false,
            repeat: crate::queue::Repeat::Off,
        }
    }

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
    fn from_last_mode_records_local_mode() {
        let cache = SessionCache::from_last_mode(LastMode::Local);
        assert_eq!(cache.last_mode, LastMode::Local);
        assert!(cache.was_local_mode());
        assert!(!cache.was_radio_mode());
    }

    #[test]
    fn active_queue_reads_the_local_queue_for_local_mode() {
        let mut cache = SessionCache::from_last_mode(LastMode::Local);
        cache.normal_queue = Some(snapshot("normal"));
        cache.local_queue = Some(snapshot("local"));

        let active = cache.active_queue().expect("active local queue");

        assert_eq!(active.songs[0].video_id, "local");
    }

    #[test]
    fn old_v2_cache_without_local_queue_still_loads() {
        let path = std::env::temp_dir().join(format!(
            "ytm-tui-session-old-v2-no-local-{}",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":2,"app_version":"{}","last_mode":"normal","normal_queue":null,"radio_queue":null}}"#,
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();

        let cache = SessionCache::load_from_path(&path);

        assert_eq!(cache.last_mode, LastMode::Normal);
        assert!(cache.local_queue.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn old_v2_radio_cache_without_local_queue_still_loads() {
        let path = std::env::temp_dir().join(format!(
            "ytm-tui-session-old-v2-radio-no-local-{}",
            std::process::id()
        ));
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":2,"app_version":"{}","last_mode":"radio","normal_queue":null,"radio_queue":null}}"#,
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();

        let cache = SessionCache::load_from_path(&path);

        assert_eq!(cache.last_mode, LastMode::Radio);
        assert!(cache.local_queue.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn local_session_cache_round_trips() {
        let path = std::env::temp_dir().join(format!(
            "ytm-tui-session-local-round-trip-{}",
            std::process::id()
        ));
        let mut cache = SessionCache::from_last_mode(LastMode::Local);
        cache.normal_queue = Some(snapshot("normal"));
        cache.radio_queue = Some(snapshot("radio"));
        cache.local_queue = Some(snapshot("local"));

        cache.save_to_path(&path).unwrap();
        let loaded = SessionCache::load_from_path(&path);

        assert_eq!(loaded.last_mode, LastMode::Local);
        assert_eq!(
            loaded
                .active_queue()
                .and_then(|snapshot| snapshot.songs.first())
                .map(|song| song.video_id.as_str()),
            Some("local")
        );
        assert_eq!(loaded.normal_queue.as_ref().map(|s| s.songs.len()), Some(1));
        assert_eq!(loaded.radio_queue.as_ref().map(|s| s.songs.len()), Some(1));
        let _ = std::fs::remove_file(&path);
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
