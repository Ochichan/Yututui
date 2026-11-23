//! Disk-backed probe cache for `yt-dlp --version` and `--js-runtimes` support.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{UpdateLock, file_stamp, load_state, save_state, tools_dir};

fn try_lock_state() -> Option<UpdateLock> {
    let dir = tools_dir()?;
    let _ = crate::util::safe_fs::ensure_private_dir(&dir);
    UpdateLock::try_acquire(&dir)
}

/// Upper bound on probe-cache entries (override + managed + system is 3; a few
/// spares for paths that moved).
const PROBE_CACHE_MAX: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProbeEntry {
    pub path: String,
    pub mtime_unix: u64,
    pub len: u64,
    pub version: String,
    #[serde(default)]
    pub supports_js_runtimes: Option<bool>,
}

/// Probe `<path> --version`, reusing the `(path, mtime, len)` cache in `ytdlp.json`.
pub(crate) async fn cached_probe(path: &Path) -> Option<String> {
    let path_str = path.to_string_lossy().into_owned();
    let stamp = file_stamp(path);

    if let Some((mtime, len)) = stamp {
        let state = load_state();
        if let Some(hit) = state
            .probe_cache
            .iter()
            .find(|e| e.path == path_str && e.mtime_unix == mtime && e.len == len)
        {
            return Some(hit.version.clone());
        }
    }

    let version = super::super::probe_version(&path_str).await?;

    if let Some((mtime, len)) = stamp
        && !crate::persist::persistence_access().is_read_only()
        && let Some(_lock) = try_lock_state()
    {
        let mut state = load_state();
        let existing_js = state
            .probe_cache
            .iter()
            .find(|e| e.path == path_str && e.mtime_unix == mtime && e.len == len)
            .and_then(|e| e.supports_js_runtimes);
        state.probe_cache.retain(|e| e.path != path_str);
        state.probe_cache.push(ProbeEntry {
            path: path_str,
            mtime_unix: mtime,
            len,
            version: version.clone(),
            supports_js_runtimes: existing_js,
        });
        if state.probe_cache.len() > PROBE_CACHE_MAX {
            let excess = state.probe_cache.len() - PROBE_CACHE_MAX;
            state.probe_cache.drain(..excess);
        }
        save_state(&state);
    }
    Some(version)
}

/// Probe `<path> --help` for `--js-runtimes`, reusing the `(path, mtime, len)` probe cache so
/// warm startups spawn zero yt-dlp subprocesses. Failed/timed-out probes are treated as
/// unsupported for this run without persisting a negative cache entry.
pub(crate) async fn cached_js_runtimes_support(path: &Path) -> bool {
    let path_str = path.to_string_lossy().into_owned();
    let stamp = file_stamp(path);

    if let Some((mtime, len)) = stamp {
        let state = load_state();
        if let Some(supported) = state
            .probe_cache
            .iter()
            .find(|e| e.path == path_str && e.mtime_unix == mtime && e.len == len)
            .and_then(|e| e.supports_js_runtimes)
        {
            return supported;
        }
    }

    let Some(supported) = super::super::probe_js_runtimes_option(&path_str).await else {
        return false;
    };

    if let Some((mtime, len)) = stamp
        && !crate::persist::persistence_access().is_read_only()
        && let Some(_lock) = try_lock_state()
    {
        let mut state = load_state();
        if let Some(entry) = state
            .probe_cache
            .iter_mut()
            .find(|e| e.path == path_str && e.mtime_unix == mtime && e.len == len)
            && entry.supports_js_runtimes != Some(supported)
        {
            entry.supports_js_runtimes = Some(supported);
            save_state(&state);
        }
    }
    supported
}
