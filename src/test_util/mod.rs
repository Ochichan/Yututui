pub mod env;

/// Runs `f` with `YTM_DATA_DIR` pointed at a directory no other test uses.
///
/// Under `#[cfg(test)]` every store resolves inside one process-wide sandbox, so each test that
/// writes through a real store shares the same files — `<data dir>/playlists.json` in particular.
/// Two tests interleaving a load-modify-save there lose one of the writes, which surfaces as an
/// assertion that a playlist the test had just created is missing. It only reproduces under load,
/// which is why it looked like a platform flake on CI rather than a shared-state bug.
///
/// The override is thread-scoped by `paths::env_dir`, so it isolates the calling test without
/// changing what parallel readers on other threads see.
#[cfg(test)]
pub fn with_isolated_data_dir<T>(label: &str, f: impl FnOnce() -> T) -> T {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = crate::paths::test_base().join(format!("isolated-data-{label}-{sequence}"));
    std::fs::create_dir_all(&dir).expect("create the isolated test data directory");
    env::with_var("YTM_DATA_DIR", Some(&dir.to_string_lossy()), f)
}

/// Drives `future` to completion with an isolated data directory installed.
///
/// The env override and the thread-scoped flag that exposes it both live on one thread, so the
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

    #[test]
    fn block_on_isolated_applies_the_override_inside_the_future() {
        let shared = crate::paths::data_dir().expect("sandboxed data dir");
        let inside = block_on_isolated("selftest-async", async {
            crate::paths::data_dir().expect("isolated data dir")
        });
        assert_ne!(inside, shared, "the future ran outside the override scope");
    }
}
