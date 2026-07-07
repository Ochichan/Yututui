//! Central resolution of the per-user **config**, **data**, and **cache** directories.
//!
//! Every persistent store (config, library, playlists, signals, station, session,
//! downloads, …) resolves its base directory here so that **tests never touch the real
//! user directories**. Two override layers sit in front of the platform default:
//!
//! 1. An env override (`YTM_CONFIG_DIR` / `YTM_DATA_DIR` / `YTM_CACHE_DIR`) — used by the
//!    CLI smoke test and the `verify` skill to run against a throwaway directory, and by
//!    anyone who wants to relocate their stores.
//! 2. Under `#[cfg(test)]`, a process-unique temp directory. In-crate tests drive real
//!    `save()` calls (the daemon parity/engine tests persist the library and config while
//!    exercising playback and settings commands); without this redirect those writes land
//!    in the developer's real `~/Library/Application Support/yututui`, injecting fixture
//!    tracks into their library and resetting their settings on every `cargo test`. The
//!    redirect is keyed on the process id and initialized once, so parallel test threads
//!    share one sandbox and none of them can escape to the real directories — and it needs
//!    no per-test `set_var`, which would be a data race under the parallel test runner.

use std::path::PathBuf;

/// A non-empty directory read from env var `name`: surrounding whitespace trimmed and a
/// leading `~` / `~/` expanded to the home directory (matching the old `config_path`'s
/// `YTM_CONFIG_DIR` handling, so a literal `~` never creates a directory named `~`).
/// Returns `None` when the variable is unset or blank.
fn env_dir(name: &str) -> Option<PathBuf> {
    let raw = std::env::var(name).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "~" {
        return directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf());
    }
    if let Some(rest) = trimmed.strip_prefix("~/")
        && let Some(base) = directories::BaseDirs::new()
    {
        return Some(base.home_dir().join(rest));
    }
    Some(PathBuf::from(trimmed))
}

/// The process-unique sandbox root used to isolate every store under `#[cfg(test)]`.
#[cfg(test)]
fn test_base() -> PathBuf {
    use std::sync::OnceLock;
    static BASE: OnceLock<PathBuf> = OnceLock::new();
    BASE.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("yututui-test-{}", std::process::id()));
        // A recycled pid could inherit a prior test run's files; start clean so no round-trip
        // test can read another run's leftovers.
        let _ = std::fs::remove_dir_all(&base);
        base
    })
    .clone()
}

/// The per-user **config** directory (holds `config.json`). `None` when the platform has no
/// resolvable config dir.
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = env_dir("YTM_CONFIG_DIR") {
        return Some(dir);
    }
    #[cfg(test)]
    {
        Some(test_base().join("config"))
    }
    #[cfg(not(test))]
    {
        directories::ProjectDirs::from("", "", "yututui").map(|d| d.config_dir().to_path_buf())
    }
}

/// The per-user **data** directory (library, playlists, signals, station, downloads,
/// romanized-title cache, AI usage log, transfers, managed tools). `None` when unresolvable.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = env_dir("YTM_DATA_DIR") {
        return Some(dir);
    }
    #[cfg(test)]
    {
        Some(test_base().join("data"))
    }
    #[cfg(not(test))]
    {
        directories::ProjectDirs::from("", "", "yututui").map(|d| d.data_dir().to_path_buf())
    }
}

/// The per-user **cache** directory (session resume snapshot, media artwork, art picker).
/// `None` when unresolvable.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = env_dir("YTM_CACHE_DIR") {
        return Some(dir);
    }
    #[cfg(test)]
    {
        Some(test_base().join("cache"))
    }
    #[cfg(not(test))]
    {
        directories::ProjectDirs::from("", "", "yututui").map(|d| d.cache_dir().to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dirs_stay_inside_the_process_sandbox() {
        // Under cfg(test) with no env override, every base dir must resolve inside the
        // per-process temp sandbox — never the real user directories.
        let base = test_base();
        for dir in [config_dir(), data_dir(), cache_dir()] {
            let dir = dir.expect("test dirs are always Some");
            assert!(
                dir.starts_with(&base),
                "{dir:?} escaped the test sandbox {base:?}"
            );
        }
    }

    #[test]
    fn config_data_cache_are_distinct_subdirs() {
        assert_ne!(config_dir(), data_dir());
        assert_ne!(data_dir(), cache_dir());
    }
}
