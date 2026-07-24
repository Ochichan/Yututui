//! Detached, preparation-only worker for manual network sync.
//!
//! A WebDAV request is bounded but synchronous in PR 3. Keeping this worker outside owner task
//! sets lets TUI and daemon shutdown stop waiting for network work. The closure may only prepare a
//! detached candidate; durable local installation remains on the primary owner lane.

use super::service::{PreparedManualSync, SyncServiceError};

pub(crate) fn spawn_detached_prepare<F, C>(prepare: F, complete: C) -> bool
where
    F: FnOnce() -> Result<PreparedManualSync, SyncServiceError> + Send + 'static,
    C: FnOnce(Result<PreparedManualSync, SyncServiceError>) + Send + 'static,
{
    std::thread::Builder::new()
        .name("ytt-personal-sync".to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(prepare))
                .unwrap_or(Err(SyncServiceError::Storage));
            complete(result);
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn worker_contains_panics_and_reports_storage() {
        let (tx, rx) = mpsc::channel();
        assert!(spawn_detached_prepare(
            || panic!("injected prepare panic"),
            move |result| {
                tx.send(result.err()).unwrap();
            }
        ));
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
            Some(SyncServiceError::Storage)
        );
    }
}
