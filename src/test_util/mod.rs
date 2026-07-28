pub mod env;

/// Runs `f` with `YTM_DATA_DIR` and `YTM_CACHE_DIR` pointed at directories no other test uses.
///
/// Under `#[cfg(test)]` every store resolves inside one process-wide sandbox, so each test that
/// writes through a real store shares the same files — `<data dir>/playlists.json` and
/// `<cache dir>/session.json` in particular. Two tests interleaving a load-modify-save there lose
/// one of the writes, which surfaces as an assertion that a playlist the test had just created is
/// missing. It only reproduces under load, which is why it looked like a platform flake on CI
/// rather than a shared-state bug.
///
/// The cache half matters for a second reason. On Windows a concurrent atomic replace of the same
/// file can fail outright, and a daemon command whose save fails does not merely lose a write: the
/// engine replaces its success-shaped response with `durability_unconfirmed`, while the App owner
/// has no such gate and still reports success. In the App/daemon parity tests that reads as the
/// two owners disagreeing about a queue edit — a projection divergence that never happened.
///
/// The overrides are thread-scoped by `paths::env_dir`, so they isolate the calling test without
/// changing what parallel readers on other threads see.
#[cfg(test)]
pub fn with_isolated_data_dir<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let (data, cache) = isolated_dir_pair(label);
    env::with_vars(
        &[
            ("YTM_DATA_DIR", Some(&*data.to_string_lossy())),
            ("YTM_CACHE_DIR", Some(&*cache.to_string_lossy())),
        ],
        f,
    )
}

#[cfg(test)]
fn isolated_dir_pair(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let base = crate::paths::test_base();
    let data = base.join(format!("isolated-data-{label}-{sequence}"));
    let cache = base.join(format!("isolated-cache-{label}-{sequence}"));
    std::fs::create_dir_all(&data).expect("create the isolated test data directory");
    std::fs::create_dir_all(&cache).expect("create the isolated test cache directory");
    (data, cache)
}

/// Drives `future` to completion with isolated data and cache directories installed.
///
/// The env overrides and the thread-scoped flag that exposes them both live on one thread, so the
/// future has to run there too: a current-thread runtime built inside the scope. Tests needing
/// this cannot stay `#[tokio::test]`, whose runtime is created outside any scope we control.
#[cfg(test)]
pub fn block_on_isolated<F: std::future::Future>(label: &str, future: F) -> F::Output {
    with_isolated_data_dir(label, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the isolated test runtime")
            .block_on(future)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An override that silently failed to apply would leave every caller sharing the sandbox
    /// again while still passing, so assert the redirection rather than assuming it.
    #[test]
    fn isolated_data_dir_redirects_and_differs_per_call() {
        let shared = crate::paths::data_dir().expect("sandboxed data dir");

        let first = with_isolated_data_dir("selftest", || {
            let dir = crate::paths::data_dir().expect("isolated data dir");
            assert_ne!(dir, shared, "the override did not take effect");
            assert_eq!(
                crate::playlists::playlists_path().expect("playlists path"),
                dir.join("playlists.json"),
                "playlists.json must follow the override"
            );
            dir
        });

        let second = with_isolated_data_dir("selftest", || {
            crate::paths::data_dir().expect("isolated dir")
        });
        assert_ne!(first, second, "two calls must not share a directory");
        assert_eq!(
            crate::paths::data_dir(),
            Some(shared),
            "the override must not outlive its scope"
        );
    }

    /// `session.json` lives under the cache dir, not the data dir. Isolating only the data half
    /// left every parity test writing one shared file, and on Windows a failed write there turns
    /// an engine command into `durability_unconfirmed` while the App still reports success.
    #[test]
    fn isolated_scope_moves_the_session_cache_too() {
        let shared = crate::session::session_cache_path().expect("sandboxed session path");

        let first = with_isolated_data_dir("selftest-cache", || {
            let cache = crate::paths::cache_dir().expect("isolated cache dir");
            let path = crate::session::session_cache_path().expect("isolated session path");
            assert_ne!(path, shared, "session.json did not follow the override");
            assert_eq!(
                path,
                cache.join("session.json"),
                "session.json must resolve inside the isolated cache dir"
            );
            path
        });

        let second = with_isolated_data_dir("selftest-cache", || {
            crate::session::session_cache_path().expect("isolated session path")
        });
        assert_ne!(first, second, "two calls must not share a session file");
        assert_eq!(
            crate::session::session_cache_path(),
            Some(shared),
            "the override must not outlive its scope"
        );
    }

    #[test]
    fn block_on_isolated_applies_the_override_inside_the_future() {
        let shared = crate::paths::data_dir().expect("sandboxed data dir");
        let inside = block_on_isolated("selftest-async", async {
            crate::paths::data_dir().expect("isolated data dir")
        });
        assert_ne!(inside, shared, "the future ran outside the override scope");
    }
}
