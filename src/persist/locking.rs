use super::*;

#[cfg(test)]
thread_local! {
    static INTENT_LOCK_CONTENTION_OBSERVER:
        std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const {
            std::cell::RefCell::new(None)
        };
}

/// Run one ordinary persistence action while observing its first real contended intent-lock
/// attempt. Keeping the hook thread-local prevents unrelated parallel tests from consuming it.
#[cfg(test)]
pub(crate) fn with_intent_lock_contention_observer<T>(
    observer: std::sync::mpsc::Sender<()>,
    action: impl FnOnce() -> T,
) -> T {
    struct ResetObserver;

    impl Drop for ResetObserver {
        fn drop(&mut self) {
            INTENT_LOCK_CONTENTION_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    INTENT_LOCK_CONTENTION_OBSERVER.with(|slot| {
        assert!(
            slot.borrow_mut().replace(observer).is_none(),
            "intent-lock contention observer already installed on this test thread"
        );
    });
    let reset = ResetObserver;
    let result = action();
    drop(reset);
    result
}

fn observe_intent_lock_contention(observe: bool) {
    #[cfg(test)]
    if observe {
        INTENT_LOCK_CONTENTION_OBSERVER.with(|slot| {
            if let Some(observer) = slot.borrow_mut().take() {
                let _ = observer.send(());
            }
        });
    }
    #[cfg(not(test))]
    let _ = observe;
}

pub(super) fn acquire_intent_lock(
    path: &Path,
) -> std::io::Result<crate::util::safe_fs::AdvisoryFileLock> {
    acquire_intent_lock_with_budget(path, Duration::from_secs(5))
}

pub(crate) fn with_store_intent_lock<T>(
    path: &Path,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let _lock = acquire_intent_lock(path)?;
    operation()
}

pub(super) fn acquire_intent_lock_with_budget(
    path: &Path,
    budget: Duration,
) -> std::io::Result<crate::util::safe_fs::AdvisoryFileLock> {
    let lock_path = intent_lock_path(path)
        .ok_or_else(|| std::io::Error::other("invalid persistence intent lock path"))?;
    acquire_private_lock_with_budget(&lock_path, "persistence journal", budget, true)
}

pub(super) fn acquire_private_lock(
    lock_path: &Path,
    purpose: &'static str,
) -> std::io::Result<crate::util::safe_fs::AdvisoryFileLock> {
    acquire_private_lock_with_budget(lock_path, purpose, Duration::from_secs(5), false)
}

fn acquire_private_lock_with_budget(
    lock_path: &Path,
    purpose: &'static str,
    budget: Duration,
    observe_intent_contention: bool,
) -> std::io::Result<crate::util::safe_fs::AdvisoryFileLock> {
    let deadline = Instant::now() + budget;
    loop {
        match crate::util::safe_fs::try_lock_private_file(lock_path)? {
            Some(lock) => return Ok(lock),
            None if Instant::now() < deadline => {
                observe_intent_lock_contention(observe_intent_contention);
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            None => {
                observe_intent_lock_contention(observe_intent_contention);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("timed out waiting for the {purpose} lock"),
                ));
            }
        }
    }
}
