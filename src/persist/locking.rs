use super::*;

pub(super) fn acquire_intent_lock(
    path: &Path,
) -> std::io::Result<crate::util::safe_fs::AdvisoryFileLock> {
    acquire_intent_lock_with_budget(path, Duration::from_secs(5))
}

pub(super) fn acquire_intent_lock_with_budget(
    path: &Path,
    budget: Duration,
) -> std::io::Result<crate::util::safe_fs::AdvisoryFileLock> {
    let lock_path = intent_lock_path(path)
        .ok_or_else(|| std::io::Error::other("invalid persistence intent lock path"))?;
    acquire_private_lock_with_budget(&lock_path, "persistence journal", budget)
}

pub(super) fn acquire_private_lock(
    lock_path: &Path,
    purpose: &'static str,
) -> std::io::Result<crate::util::safe_fs::AdvisoryFileLock> {
    acquire_private_lock_with_budget(lock_path, purpose, Duration::from_secs(5))
}

fn acquire_private_lock_with_budget(
    lock_path: &Path,
    purpose: &'static str,
    budget: Duration,
) -> std::io::Result<crate::util::safe_fs::AdvisoryFileLock> {
    let deadline = Instant::now() + budget;
    loop {
        match crate::util::safe_fs::try_lock_private_file(lock_path)? {
            Some(lock) => return Ok(lock),
            None if Instant::now() < deadline => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("timed out waiting for the {purpose} lock"),
                ));
            }
        }
    }
}
