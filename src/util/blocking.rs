//! Shared budget for `spawn_blocking` work.
//!
//! Tokio's blocking pool can grow under pressure. The app already keeps the async runtime small,
//! so CPU-heavy image work and IO-heavy scans/saves get explicit process-wide budgets too.

use std::sync::{Arc, LazyLock};

use tokio::sync::Semaphore;
use tokio::task::JoinError;

static CPU_BLOCKING: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));
static IO_BLOCKING: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));

pub async fn spawn_cpu<F, R>(work: F) -> Result<R, JoinError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    spawn_with_budget(Arc::clone(&CPU_BLOCKING), work).await
}

pub async fn spawn_io<F, R>(work: F) -> Result<R, JoinError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    spawn_with_budget(Arc::clone(&IO_BLOCKING), work).await
}

async fn spawn_with_budget<F, R>(budget: Arc<Semaphore>, work: F) -> Result<R, JoinError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let permit = budget
        .acquire_owned()
        .await
        .expect("blocking budget semaphore is never closed");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
}
