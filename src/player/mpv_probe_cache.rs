//! Disk-backed capability probe cache for `mpv`.
//!
//! Startup probes `mpv --no-config <flag> --version` and the guardian lifeline check to verify
//! optional flags without parsing version strings. Caching definitive answers under
//! `<cache_dir>/mpv-probe-cache.json` keyed by `(canonical_path, mtime_unix, len)` eliminates
//! repeated subprocess spawns on warm starts while invalidating automatically whenever `mpv` is
//! upgraded or swapped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::util::safe_fs;

const CACHE_FILE_NAME: &str = "mpv-probe-cache.json";
const MAX_ENTRIES: usize = 8;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct MpvProbeCache {
    entries: Vec<MpvProbeEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
struct MpvProbeEntry {
    path: String,
    mtime_unix: u64,
    len: u64,
    flags: HashMap<String, bool>,
    lifeline: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryStamp {
    path: String,
    mtime_unix: u64,
    len: u64,
}

fn cache_path() -> Option<PathBuf> {
    crate::paths::cache_dir().map(|dir| dir.join(CACHE_FILE_NAME))
}

fn resolve_binary_stamp(program: &str) -> Option<BinaryStamp> {
    let raw = Path::new(program);
    let resolved = if raw.is_absolute() || raw.components().count() > 1 {
        raw.to_path_buf()
    } else {
        crate::deps::resolve_on_path(program)?
    };
    let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
    let meta = std::fs::metadata(&canonical).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime_unix = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(BinaryStamp {
        path: canonical.to_string_lossy().into_owned(),
        mtime_unix,
        len: meta.len(),
    })
}

fn load_at(path: &Path) -> MpvProbeCache {
    safe_fs::load_json_or_default(path)
}

fn save_at(path: &Path, cache: &MpvProbeCache) {
    if let Some(dir) = path.parent() {
        let _ = safe_fs::ensure_private_dir(dir);
    }
    if let Err(error) = safe_fs::write_private_atomic_json(path, cache) {
        tracing::debug!(error = %error, "failed to persist mpv probe cache");
    }
}

fn entry_matches(entry: &MpvProbeEntry, stamp: &BinaryStamp) -> bool {
    entry.path == stamp.path && entry.mtime_unix == stamp.mtime_unix && entry.len == stamp.len
}

fn lookup_flag_at(cache_file: &Path, stamp: &BinaryStamp, flag: &str) -> Option<bool> {
    let cache = load_at(cache_file);
    cache
        .entries
        .iter()
        .find(|entry| entry_matches(entry, stamp))
        .and_then(|entry| entry.flags.get(flag).copied())
}

fn store_flag_at(cache_file: &Path, stamp: BinaryStamp, flag: &str, supported: bool) {
    let mut cache = load_at(cache_file);
    if let Some(entry) = cache
        .entries
        .iter_mut()
        .find(|entry| entry_matches(entry, &stamp))
    {
        if entry.flags.get(flag).copied() == Some(supported) {
            return;
        }
        entry.flags.insert(flag.to_owned(), supported);
    } else {
        cache.entries.retain(|entry| entry.path != stamp.path);
        let mut flags = HashMap::new();
        flags.insert(flag.to_owned(), supported);
        cache.entries.push(MpvProbeEntry {
            path: stamp.path,
            mtime_unix: stamp.mtime_unix,
            len: stamp.len,
            flags,
            lifeline: None,
        });
        if cache.entries.len() > MAX_ENTRIES {
            let excess = cache.entries.len() - MAX_ENTRIES;
            cache.entries.drain(..excess);
        }
    }
    save_at(cache_file, &cache);
}

fn lookup_lifeline_at(cache_file: &Path, stamp: &BinaryStamp) -> Option<bool> {
    let cache = load_at(cache_file);
    cache
        .entries
        .iter()
        .find(|entry| entry_matches(entry, stamp))
        .and_then(|entry| entry.lifeline)
}

fn store_lifeline_at(cache_file: &Path, stamp: BinaryStamp, supported: bool) {
    let mut cache = load_at(cache_file);
    if let Some(entry) = cache
        .entries
        .iter_mut()
        .find(|entry| entry_matches(entry, &stamp))
    {
        if entry.lifeline == Some(supported) {
            return;
        }
        entry.lifeline = Some(supported);
    } else {
        cache.entries.retain(|entry| entry.path != stamp.path);
        cache.entries.push(MpvProbeEntry {
            path: stamp.path,
            mtime_unix: stamp.mtime_unix,
            len: stamp.len,
            flags: HashMap::new(),
            lifeline: Some(supported),
        });
        if cache.entries.len() > MAX_ENTRIES {
            let excess = cache.entries.len() - MAX_ENTRIES;
            cache.entries.drain(..excess);
        }
    }
    save_at(cache_file, &cache);
}

pub(super) fn lookup_flag(program: &str, flag: &str) -> Option<bool> {
    let cache_file = cache_path()?;
    let stamp = resolve_binary_stamp(program)?;
    lookup_flag_at(&cache_file, &stamp, flag)
}

pub(super) fn store_flag(program: &str, flag: &str, supported: bool) {
    if crate::persist::persistence_access().is_read_only() {
        return;
    }
    let Some(cache_file) = cache_path() else {
        return;
    };
    let Some(stamp) = resolve_binary_stamp(program) else {
        return;
    };
    store_flag_at(&cache_file, stamp, flag, supported);
}

pub(super) fn lookup_lifeline(program: &str) -> Option<bool> {
    let cache_file = cache_path()?;
    let stamp = resolve_binary_stamp(program)?;
    lookup_lifeline_at(&cache_file, &stamp)
}

pub(super) fn store_lifeline(program: &str, supported: bool) {
    if crate::persist::persistence_access().is_read_only() {
        return;
    }
    let Some(cache_file) = cache_path() else {
        return;
    };
    let Some(stamp) = resolve_binary_stamp(program) else {
        return;
    };
    store_lifeline_at(&cache_file, stamp, supported);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache_path(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "yututui-mpv-probe-cache-{label}-{}-{seq}",
                std::process::id()
            ))
            .join(CACHE_FILE_NAME)
    }

    #[test]
    fn flag_and_lifeline_round_trip_and_invalidate_on_stamp_change() {
        let cache_file = temp_cache_path("round-trip");
        let dir = cache_file.parent().unwrap().to_path_buf();
        let stamp_v1 = BinaryStamp {
            path: "/usr/bin/mpv".to_owned(),
            mtime_unix: 100,
            len: 4096,
        };
        let stamp_v2 = BinaryStamp {
            path: "/usr/bin/mpv".to_owned(),
            mtime_unix: 200,
            len: 4096,
        };

        assert_eq!(
            lookup_flag_at(&cache_file, &stamp_v1, "--stream-record="),
            None
        );
        assert_eq!(lookup_lifeline_at(&cache_file, &stamp_v1), None);

        store_flag_at(&cache_file, stamp_v1.clone(), "--stream-record=", true);
        store_flag_at(&cache_file, stamp_v1.clone(), "--media-controls=no", false);
        store_lifeline_at(&cache_file, stamp_v1.clone(), true);

        assert_eq!(
            lookup_flag_at(&cache_file, &stamp_v1, "--stream-record="),
            Some(true)
        );
        assert_eq!(
            lookup_flag_at(&cache_file, &stamp_v1, "--media-controls=no"),
            Some(false)
        );
        assert_eq!(lookup_lifeline_at(&cache_file, &stamp_v1), Some(true));

        // Upgrading the binary (different mtime/len) invalidates cached answers.
        assert_eq!(
            lookup_flag_at(&cache_file, &stamp_v2, "--stream-record="),
            None
        );
        assert_eq!(lookup_lifeline_at(&cache_file, &stamp_v2), None);

        let _ = std::fs::remove_dir_all(dir);
    }
}
