//! Ownership handshake for the Windows SMTC initialization worker.
//!
//! Session initialization can block inside a platform API beyond the facade's
//! deadline. The worker therefore cannot assume that a successfully-created
//! session still has an owner: it publishes readiness, waits for an explicit
//! claim, and observes cancellation before entering the permanent message pump.

use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;

/// One process-wide Windows SMTC worker may be live or retiring at a time.
///
/// A platform initialization call can outlive the facade's timeout and cannot be
/// interrupted safely. The lease therefore belongs to the worker closure itself,
/// not the constructor: a timed-out worker keeps the slot until it actually exits.
struct WorkerSlot {
    occupied: Arc<AtomicBool>,
}

pub(super) struct WorkerLease {
    occupied: Arc<AtomicBool>,
}

/// One coalesced Windows thread-message wake. Producers may replace the latest snapshot while
/// this token is claimed, but only the first producer posts `WM_APP_UPDATE`.
#[derive(Default)]
#[cfg(test)]
pub(super) struct WorkerWake {
    posted: AtomicBool,
}

#[cfg(test)]
impl WorkerWake {
    pub(super) fn claim(&self) -> bool {
        self.posted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn clear(&self) {
        self.posted.store(false, Ordering::Release);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum WorkerStartError<E> {
    Occupied,
    Spawn(E),
}

impl WorkerSlot {
    fn new() -> Self {
        Self {
            occupied: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start_with<T, E>(
        &self,
        start: impl FnOnce(WorkerLease) -> Result<T, E>,
    ) -> Result<T, WorkerStartError<E>> {
        if self
            .occupied
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(WorkerStartError::Occupied);
        }

        start(WorkerLease {
            occupied: Arc::clone(&self.occupied),
        })
        .map_err(WorkerStartError::Spawn)
    }
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        self.occupied.store(false, Ordering::Release);
    }
}

#[cfg(windows)]
pub(super) fn start_process_worker<T, E>(
    start: impl FnOnce(WorkerLease) -> Result<T, E>,
) -> Result<T, WorkerStartError<E>> {
    static SLOT: std::sync::OnceLock<WorkerSlot> = std::sync::OnceLock::new();
    SLOT.get_or_init(WorkerSlot::new).start_with(start)
}

pub(super) struct StartupOwner {
    cancelled: Arc<AtomicBool>,
    claim_tx: Option<mpsc::SyncSender<()>>,
}

pub(super) struct WorkerStartup {
    cancelled: Arc<AtomicBool>,
    claim_rx: mpsc::Receiver<()>,
}

pub(super) fn startup_pair() -> (StartupOwner, WorkerStartup) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (claim_tx, claim_rx) = mpsc::sync_channel(0);
    (
        StartupOwner {
            cancelled: Arc::clone(&cancelled),
            claim_tx: Some(claim_tx),
        },
        WorkerStartup {
            cancelled,
            claim_rx,
        },
    )
}

impl StartupOwner {
    /// Confirm that the caller has installed this owner in its final backend.
    /// The rendezvous send ensures the worker has reached its ownership gate.
    pub(super) fn claim(&mut self) -> Result<(), ()> {
        let Some(claim_tx) = self.claim_tx.take() else {
            return Err(());
        };
        claim_tx.send(()).map_err(|_| ())
    }

    pub(super) fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.claim_tx.take();
    }
}

impl Drop for StartupOwner {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl WorkerStartup {
    /// Publish the initialized worker's identifier and wait until the caller
    /// explicitly owns it. `false` means the session must be torn down locally.
    pub(super) fn hand_off<T>(
        &self,
        ready_tx: &mpsc::SyncSender<Result<T, String>>,
        ready: T,
    ) -> bool {
        if self.is_cancelled() || ready_tx.send(Ok(ready)).is_err() {
            return false;
        }
        self.claim_rx.recv().is_ok() && !self.is_cancelled()
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Join a late initialization worker without extending the caller's timeout.
/// The returned handle is only needed by deterministic tests; production may
/// detach the reaper because it owns and joins the actual worker handle.
pub(super) fn spawn_reaper(worker: JoinHandle<()>) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("smtc-worker-reaper".to_owned())
        .spawn(move || {
            if worker.join().is_err() {
                tracing::warn!("late SMTC initialization worker panicked");
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn worker_wake_allows_only_one_post_until_the_worker_clears_it() {
        let wake = Arc::new(WorkerWake::default());
        let mut claimers = Vec::new();
        for _ in 0..16 {
            let wake = Arc::clone(&wake);
            claimers.push(std::thread::spawn(move || usize::from(wake.claim())));
        }
        let claims: usize = claimers
            .into_iter()
            .map(|claimer| claimer.join().expect("join wake claimer"))
            .sum();
        assert_eq!(claims, 1);

        wake.clear();
        assert!(wake.claim());
        assert!(!wake.claim());
    }

    #[derive(Default)]
    struct Counters {
        pumps: AtomicUsize,
        teardowns: AtomicUsize,
    }

    fn spawn_fake_worker(
        startup: WorkerStartup,
        ready_tx: mpsc::SyncSender<Result<u32, String>>,
        initialize_rx: mpsc::Receiver<()>,
        shutdown_rx: mpsc::Receiver<()>,
        pump_tx: mpsc::SyncSender<()>,
        counters: Arc<Counters>,
        worker_lease: Option<WorkerLease>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let _worker_lease = worker_lease;
            initialize_rx.recv().expect("release fake initialization");
            if startup.hand_off(&ready_tx, 41) {
                counters.pumps.fetch_add(1, Ordering::SeqCst);
                pump_tx.send(()).expect("announce fake pump");
                let _ = shutdown_rx.recv();
                assert!(startup.is_cancelled());
            }
            counters.teardowns.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn late_success_after_timeout_is_torn_down_and_reaped() {
        let slot = WorkerSlot::new();
        let (mut owner, startup) = startup_pair();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (initialize_tx, initialize_rx) = mpsc::sync_channel(0);
        let (_shutdown_tx, shutdown_rx) = mpsc::sync_channel(0);
        let (pump_tx, pump_rx) = mpsc::sync_channel(0);
        let counters = Arc::new(Counters::default());
        let worker = slot
            .start_with(|lease| {
                Ok::<_, ()>(spawn_fake_worker(
                    startup,
                    ready_tx,
                    initialize_rx,
                    shutdown_rx,
                    pump_tx,
                    Arc::clone(&counters),
                    Some(lease),
                ))
            })
            .expect("reserve fake worker slot");

        owner.cancel();
        drop(ready_rx);
        let reaper = spawn_reaper(worker).expect("spawn fake reaper");
        assert!(
            matches!(
                slot.start_with(Ok::<_, ()>),
                Err(WorkerStartError::Occupied)
            ),
            "a timed-out worker must retain the slot until it actually exits"
        );
        initialize_tx.send(()).expect("finish late initialization");
        reaper.join().expect("reap fake worker");

        assert_eq!(counters.pumps.load(Ordering::SeqCst), 0);
        assert_eq!(counters.teardowns.load(Ordering::SeqCst), 1);
        assert!(pump_rx.try_recv().is_err());

        let replacement = slot
            .start_with(Ok::<_, ()>)
            .expect("late worker exit releases slot");
        drop(replacement);
    }

    #[test]
    fn cancellation_after_ready_prevents_pump_entry() {
        let (mut owner, startup) = startup_pair();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (initialize_tx, initialize_rx) = mpsc::sync_channel(0);
        let (_shutdown_tx, shutdown_rx) = mpsc::sync_channel(0);
        let (pump_tx, pump_rx) = mpsc::sync_channel(0);
        let counters = Arc::new(Counters::default());
        let worker = spawn_fake_worker(
            startup,
            ready_tx,
            initialize_rx,
            shutdown_rx,
            pump_tx,
            Arc::clone(&counters),
            None,
        );

        initialize_tx.send(()).expect("finish initialization");
        assert_eq!(ready_rx.recv_timeout(Duration::from_secs(1)), Ok(Ok(41)));
        owner.cancel();
        worker.join().expect("join cancelled worker");

        assert_eq!(counters.pumps.load(Ordering::SeqCst), 0);
        assert_eq!(counters.teardowns.load(Ordering::SeqCst), 1);
        assert!(pump_rx.try_recv().is_err());
    }

    #[test]
    fn claimed_worker_enters_pump_and_shuts_down_normally() {
        let (owner, startup) = startup_pair();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (initialize_tx, initialize_rx) = mpsc::sync_channel(0);
        let (shutdown_tx, shutdown_rx) = mpsc::sync_channel(0);
        let (pump_tx, pump_rx) = mpsc::sync_channel(0);
        let counters = Arc::new(Counters::default());
        let worker = spawn_fake_worker(
            startup,
            ready_tx,
            initialize_rx,
            shutdown_rx,
            pump_tx,
            Arc::clone(&counters),
            None,
        );

        initialize_tx.send(()).expect("finish initialization");
        assert_eq!(ready_rx.recv_timeout(Duration::from_secs(1)), Ok(Ok(41)));
        let mut owner = owner;
        owner.claim().expect("claim fake worker");
        pump_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker entered pump");
        owner.cancel();
        shutdown_tx.send(()).expect("wake fake pump for shutdown");
        worker.join().expect("join owned worker");

        assert_eq!(counters.pumps.load(Ordering::SeqCst), 1);
        assert_eq!(counters.teardowns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn single_flight_rejects_a_second_worker_until_the_first_exits() {
        let slot = WorkerSlot::new();
        let first = slot
            .start_with(Ok::<_, ()>)
            .expect("reserve first worker slot");

        assert!(matches!(
            slot.start_with(Ok::<_, ()>),
            Err(WorkerStartError::Occupied)
        ));

        drop(first);
        let replacement = slot
            .start_with(Ok::<_, ()>)
            .expect("worker exit releases slot");
        drop(replacement);
    }

    #[test]
    fn spawn_failure_and_worker_panic_release_the_single_flight_slot() {
        let slot = WorkerSlot::new();

        assert_eq!(
            slot.start_with(|_lease| Err::<(), _>(io::ErrorKind::Other)),
            Err(WorkerStartError::Spawn(io::ErrorKind::Other))
        );
        let after_spawn_failure = slot
            .start_with(Ok::<_, ()>)
            .expect("spawn failure releases slot");
        drop(after_spawn_failure);

        let panicking_worker = slot
            .start_with(|lease| {
                std::thread::Builder::new()
                    .name("panicking-fake-smtc-worker".to_owned())
                    .spawn(move || {
                        let _worker_lease = lease;
                        panic!("fake SMTC worker panic");
                    })
            })
            .expect("spawn panicking fake worker");
        assert!(panicking_worker.join().is_err());

        let after_panic = slot
            .start_with(Ok::<_, ()>)
            .expect("worker panic releases slot");
        drop(after_panic);
    }
}
