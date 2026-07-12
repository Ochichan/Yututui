//! Two long-lived, ordered async lanes for desktop companion actions.
//!
//! Native event loops cannot own a Tokio runtime and WebViews are `!Send`, but remote-control
//! futures do not need a fresh OS thread/runtime per click. This actor runs the ordered media
//! command lane and the serialized daemon/startup/terminal lifecycle lane independently on one
//! dedicated runtime thread, preserving order within each lane with separate bounded backpressure.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

type LocalFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;
type Job = Box<dyn FnOnce() -> LocalFuture + Send + 'static>;

const COMMAND_CAPACITY: usize = 64;
const LIFECYCLE_CAPACITY: usize = 16;
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopLane {
    Command,
    Lifecycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    Full,
    Closed,
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("desktop executor lane is full"),
            Self::Closed => f.write_str("desktop executor lane is closed"),
        }
    }
}

impl std::error::Error for SubmitError {}

pub struct DesktopCommandExecutor {
    command_sender: Option<tokio::sync::mpsc::Sender<Job>>,
    lifecycle_sender: Option<tokio::sync::mpsc::Sender<Job>>,
    shutdown: Option<tokio::sync::watch::Sender<bool>>,
    done: std::sync::mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DesktopCommandExecutor {
    pub fn spawn(thread_name: &str) -> std::io::Result<Self> {
        let (command_sender, command_receiver) =
            tokio::sync::mpsc::channel::<Job>(COMMAND_CAPACITY);
        let (lifecycle_sender, lifecycle_receiver) =
            tokio::sync::mpsc::channel::<Job>(LIFECYCLE_CAPACITY);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let Ok(runtime) = runtime else {
                    let _ = ready_tx.send(false);
                    return;
                };
                let _ = ready_tx.send(true);
                runtime.block_on(async move {
                    tokio::join!(
                        run_lane(command_receiver, shutdown_rx.clone()),
                        run_lane(lifecycle_receiver, shutdown_rx),
                    );
                });
                let _ = done_tx.send(());
            })?;
        if ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .ok()
            != Some(true)
        {
            return Err(std::io::Error::other(
                "desktop command executor failed to start",
            ));
        }
        Ok(Self {
            command_sender: Some(command_sender),
            lifecycle_sender: Some(lifecycle_sender),
            shutdown: Some(shutdown_tx),
            done: done_rx,
            thread: Some(thread),
        })
    }

    pub fn submit<F, Fut>(&self, make_future: F) -> Result<(), SubmitError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.submit_on(DesktopLane::Command, make_future)
    }

    pub fn submit_lifecycle<F, Fut>(&self, make_future: F) -> Result<(), SubmitError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.submit_on(DesktopLane::Lifecycle, make_future)
    }

    pub fn submit_on<F, Fut>(&self, lane: DesktopLane, make_future: F) -> Result<(), SubmitError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let sender = match lane {
            DesktopLane::Command => self.command_sender.as_ref(),
            DesktopLane::Lifecycle => self.lifecycle_sender.as_ref(),
        };
        let Some(sender) = sender else {
            return Err(SubmitError::Closed);
        };
        sender
            .try_send(Box::new(move || Box::pin(make_future())))
            .map_err(|error| match error {
                tokio::sync::mpsc::error::TrySendError::Full(_) => SubmitError::Full,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => SubmitError::Closed,
            })
    }

    /// Submit work whose projection generation must only advance after the bounded lane has
    /// accepted it. Allocating inside the queued job prevents a rejected Full/Closed submission
    /// from invalidating the still-running fallback poll generation.
    pub fn submit_with_generation<F, Fut>(
        &self,
        generation: Arc<AtomicU64>,
        make_future: F,
    ) -> Result<(), SubmitError>
    where
        F: FnOnce(u64) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.submit(move || {
            let generation = generation.fetch_add(1, Ordering::AcqRel) + 1;
            make_future(generation)
        })
    }

    pub fn submit_lifecycle_with_generation<F, Fut>(
        &self,
        generation: Arc<AtomicU64>,
        make_future: F,
    ) -> Result<(), SubmitError>
    where
        F: FnOnce(u64) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        self.submit_lifecycle(move || {
            let generation = generation.fetch_add(1, Ordering::AcqRel) + 1;
            make_future(generation)
        })
    }
}

async fn run_lane(
    mut receiver: tokio::sync::mpsc::Receiver<Job>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        let job = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
            job = receiver.recv() => match job {
                Some(job) => job,
                None => break,
            },
        };
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = job() => {},
        }
    }
}

impl Drop for DesktopCommandExecutor {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(true);
        }
        self.command_sender.take();
        self.lifecycle_sender.take();
        if self.done.recv_timeout(SHUTDOWN_TIMEOUT).is_ok() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        } else {
            tracing::warn!(target: "ytt_desktop", "desktop command executor did not stop before deadline");
            // Rust cannot force-stop an OS thread safely. Detaching is bounded and
            // the closed channels prevent it from publishing late UI work.
            self.thread.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_complete_in_submission_order() {
        let executor = DesktopCommandExecutor::spawn("ytt-command-order-test").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        for value in 0..5 {
            let tx = tx.clone();
            executor
                .submit(move || async move {
                    if value == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    tx.send(value).unwrap();
                })
                .unwrap();
        }
        drop(tx);
        let values = (0..5)
            .map(|_| rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap())
            .collect::<Vec<_>>();
        drop(executor);
        assert_eq!(values, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn lifecycle_lane_is_serial_and_independent_from_a_blocked_command() {
        let executor = DesktopCommandExecutor::spawn("ytt-two-lane-test").unwrap();
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let command_tx = events_tx.clone();
        executor
            .submit(move || async move {
                command_tx.send("command-started").unwrap();
                let _ = release_rx.await;
                command_tx.send("command-1").unwrap();
            })
            .unwrap();
        let command_tx = events_tx.clone();
        executor
            .submit(move || async move {
                command_tx.send("command-2").unwrap();
            })
            .unwrap();
        assert_eq!(
            events_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            "command-started"
        );

        let lifecycle_tx = events_tx.clone();
        executor
            .submit_lifecycle(move || async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                lifecycle_tx.send("lifecycle-1").unwrap();
            })
            .unwrap();
        executor
            .submit_lifecycle(move || async move {
                events_tx.send("lifecycle-2").unwrap();
            })
            .unwrap();
        assert_eq!(
            events_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            "lifecycle-1"
        );
        assert_eq!(
            events_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            "lifecycle-2"
        );

        release_tx.send(()).unwrap();
        assert_eq!(
            events_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            "command-1"
        );
        assert_eq!(
            events_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            "command-2"
        );
    }

    #[test]
    fn drop_cancels_a_running_job_without_waiting_for_its_timeout() {
        let executor = DesktopCommandExecutor::spawn("ytt-command-cancel-test").unwrap();
        executor
            .submit(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            })
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let started = std::time::Instant::now();
        drop(executor);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn rejected_generation_jobs_do_not_advance_the_projection_epoch() {
        let mut executor = DesktopCommandExecutor::spawn("ytt-command-generation-test").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        executor
            .submit(move || async move {
                started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            })
            .unwrap();
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();

        // Keep the single consumer occupied and fill every bounded queue slot.
        for _ in 0..COMMAND_CAPACITY {
            executor.submit(|| async {}).unwrap();
        }
        let generation = Arc::new(AtomicU64::new(7));
        assert_eq!(
            executor.submit_with_generation(Arc::clone(&generation), |_| async {}),
            Err(SubmitError::Full)
        );
        assert_eq!(generation.load(Ordering::Acquire), 7);

        // Closed rejection has the same guarantee.
        executor.command_sender.take();
        assert_eq!(
            executor.submit_with_generation(Arc::clone(&generation), |_| async {}),
            Err(SubmitError::Closed)
        );
        assert_eq!(generation.load(Ordering::Acquire), 7);
    }

    #[test]
    fn rejected_lifecycle_jobs_do_not_advance_the_projection_epoch() {
        let mut executor = DesktopCommandExecutor::spawn("ytt-lifecycle-generation-test").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        executor
            .submit_lifecycle(move || async move {
                started_tx.send(()).unwrap();
                std::future::pending::<()>().await;
            })
            .unwrap();
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        for _ in 0..LIFECYCLE_CAPACITY {
            executor.submit_lifecycle(|| async {}).unwrap();
        }
        let generation = Arc::new(AtomicU64::new(11));
        assert_eq!(
            executor.submit_lifecycle_with_generation(Arc::clone(&generation), |_| async {}),
            Err(SubmitError::Full)
        );
        assert_eq!(generation.load(Ordering::Acquire), 11);

        executor.lifecycle_sender.take();
        assert_eq!(
            executor.submit_lifecycle_with_generation(Arc::clone(&generation), |_| async {}),
            Err(SubmitError::Closed)
        );
        assert_eq!(generation.load(Ordering::Acquire), 11);
    }
}
