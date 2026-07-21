use std::sync::{Condvar, Mutex};

use super::*;

fn noop_kill_media() -> KillMedia {
    Arc::new(|| {})
}

struct StartupTerminalLoss;

impl InputBackend for StartupTerminalLoss {
    fn check_owner_attached(&mut self) -> Vec<ProbeOutcome> {
        vec![ProbeOutcome::DefinitiveLoss {
            domain: EvidenceDomain::OwnerEnvironment,
            stage: LivenessStage::OwnerProbe,
            error: io::Error::new(io::ErrorKind::BrokenPipe, "startup terminal owner detached"),
        }]
    }

    fn heartbeat(&mut self) -> ProbeOutcome {
        unreachable!("the definitive owner loss ends startup before CPR")
    }

    fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
        unreachable!("startup failure never enters the event loop")
    }

    fn read(&mut self) -> io::Result<Event> {
        unreachable!("startup failure never enters the event loop")
    }
}

#[test]
fn startup_terminal_failure_claims_shutdown_before_it_is_reported() {
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let shutdown = ShutdownLatch::new();
    let failure = Arc::new(FailureStore::default());
    let killed = Arc::new(AtomicBool::new(false));
    let kill_observation = Arc::clone(&killed);

    run_input_loop(
        StartupTerminalLoss,
        event_tx,
        ready_tx,
        InputLoopControl {
            stop: Arc::new(AtomicBool::new(false)),
            shutdown: shutdown.clone(),
            failure: Arc::clone(&failure),
            progress: Arc::new(WatchdogProgress::default()),
            kill_media: Arc::new(move || kill_observation.store(true, Ordering::Release)),
            output_lock: TerminalOutputLock::new(shutdown.clone()),
            heartbeat_interval: Duration::from_secs(1),
            owner_probe_interval: Duration::from_secs(1),
        },
    );

    assert!(ready_rx.recv().unwrap().is_err());
    assert!(shutdown.is_triggered());
    assert!(!shutdown.was_triggered_by_signal());
    assert!(killed.load(Ordering::Acquire));
    assert!(
        failure
            .error_snapshot()
            .unwrap()
            .to_string()
            .contains("startup terminal owner detached")
    );
}

struct BlockingHeartbeat {
    calls: usize,
    entered: Option<std::sync::mpsc::SyncSender<()>>,
    release: Arc<(Mutex<bool>, Condvar)>,
    fail_after_release: bool,
}

impl InputBackend for BlockingHeartbeat {
    fn check_owner_attached(&mut self) -> Vec<ProbeOutcome> {
        vec![ProbeOutcome::NoEvidence]
    }

    fn heartbeat(&mut self) -> ProbeOutcome {
        self.calls += 1;
        if self.calls == 1 {
            return ProbeOutcome::Alive(EvidenceDomain::Transport);
        }
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        let (lock, changed) = &*self.release;
        let mut released = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*released {
            released = changed
                .wait(released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if self.fail_after_release {
            ProbeOutcome::transport(
                io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "heartbeat cancelled during ordinary shutdown",
                ),
                2,
                LivenessStage::CprWait,
            )
        } else {
            ProbeOutcome::Alive(EvidenceDomain::Transport)
        }
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        std::thread::sleep(timeout);
        Ok(false)
    }

    fn read(&mut self) -> io::Result<Event> {
        unreachable!("blocking fixture never reports an event")
    }
}

#[test]
fn independent_watchdog_kills_media_when_backend_call_never_returns() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = ShutdownLatch::new();
    let failure = Arc::new(FailureStore::default());
    let progress = Arc::new(WatchdogProgress::default());
    let killed_after_latch = Arc::new(AtomicBool::new(false));
    let kill_observation = Arc::clone(&killed_after_latch);
    let kill_shutdown = shutdown.clone();
    let kill_media: KillMedia = Arc::new(move || {
        kill_observation.store(kill_shutdown.is_triggered(), Ordering::Release);
    });

    let input_stop = Arc::clone(&stop);
    let input_shutdown = shutdown.clone();
    let input_failure = Arc::clone(&failure);
    let input_progress = Arc::clone(&progress);
    let input_kill = Arc::clone(&kill_media);
    let input_output = TerminalOutputLock::new(input_shutdown.clone());
    let input_release = Arc::clone(&release);
    let input = std::thread::spawn(move || {
        run_input_loop(
            BlockingHeartbeat {
                calls: 0,
                entered: Some(entered_tx),
                release: input_release,
                fail_after_release: false,
            },
            tx,
            ready_tx,
            InputLoopControl {
                stop: input_stop,
                shutdown: input_shutdown,
                failure: input_failure,
                progress: input_progress,
                kill_media: input_kill,
                output_lock: input_output,
                heartbeat_interval: Duration::from_millis(5),
                owner_probe_interval: Duration::from_secs(1),
            },
        );
    });
    assert_eq!(ready_rx.recv().unwrap(), Ok(()));
    let armed_at = Instant::now();
    progress.arm();

    let watchdog_progress = Arc::clone(&progress);
    let watchdog_stop = Arc::clone(&stop);
    let watchdog_shutdown = shutdown.clone();
    let watchdog_failure = Arc::clone(&failure);
    let watchdog_kill = Arc::clone(&kill_media);
    let watchdog_output = TerminalOutputLock::new(shutdown.clone());
    let watchdog = std::thread::spawn(move || {
        run_watchdog(
            watchdog_progress,
            watchdog_stop,
            watchdog_shutdown,
            watchdog_failure,
            watchdog_kill,
            watchdog_output,
            (Duration::from_millis(100), None),
        );
    });

    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("fixture must enter its permanently blocked heartbeat");
    let deadline = Instant::now() + Duration::from_secs(1);
    while !shutdown.is_triggered() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(shutdown.is_triggered());
    assert!(
        armed_at.elapsed() < Duration::from_millis(500),
        "scaled watchdog exceeded its bounded detection window"
    );
    watchdog.join().unwrap();
    assert!(killed_after_latch.load(Ordering::Acquire));
    assert!(
        failure
            .error_snapshot()
            .unwrap()
            .to_string()
            .contains("hard deadline")
    );

    stop.store(true, Ordering::Release);
    let (released, changed) = &*release;
    *released.lock().unwrap() = true;
    changed.notify_all();
    input.join().unwrap();
}

#[derive(Clone, Copy)]
enum BlockingIoStage {
    Poll,
    Read,
}

struct BlockingIoRace {
    stage: BlockingIoStage,
    entered: Option<std::sync::mpsc::SyncSender<()>>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl BlockingIoRace {
    fn block_and_fail(&mut self) -> io::Result<bool> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
        }
        let (lock, changed) = &*self.release;
        let mut released = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*released {
            released = changed
                .wait(released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "terminal I/O cancelled by ordinary shutdown",
        ))
    }
}

impl InputBackend for BlockingIoRace {
    fn check_owner_attached(&mut self) -> Vec<ProbeOutcome> {
        vec![ProbeOutcome::NoEvidence]
    }

    fn heartbeat(&mut self) -> ProbeOutcome {
        ProbeOutcome::Alive(EvidenceDomain::Transport)
    }

    fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
        match self.stage {
            BlockingIoStage::Poll => self.block_and_fail(),
            BlockingIoStage::Read => Ok(true),
        }
    }

    fn read(&mut self) -> io::Result<Event> {
        debug_assert!(matches!(self.stage, BlockingIoStage::Read));
        self.block_and_fail().map(|_| unreachable!())
    }
}

fn assert_in_flight_io_cancellation_is_not_terminal(stage: BlockingIoStage) {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = ShutdownLatch::new();
    let failure = Arc::new(FailureStore::default());
    let thread_stop = Arc::clone(&stop);
    let thread_shutdown = shutdown.clone();
    let thread_failure = Arc::clone(&failure);
    let thread_release = Arc::clone(&release);
    let worker = std::thread::spawn(move || {
        run_input_loop(
            BlockingIoRace {
                stage,
                entered: Some(entered_tx),
                release: thread_release,
            },
            tx,
            ready_tx,
            InputLoopControl {
                stop: thread_stop,
                shutdown: thread_shutdown.clone(),
                failure: thread_failure,
                progress: Arc::new(WatchdogProgress::default()),
                kill_media: noop_kill_media(),
                output_lock: TerminalOutputLock::new(thread_shutdown),
                heartbeat_interval: Duration::from_secs(1),
                owner_probe_interval: Duration::from_secs(1),
            },
        );
    });
    assert_eq!(ready_rx.recv().unwrap(), Ok(()));
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    stop.store(true, Ordering::Release);
    let (released, changed) = &*release;
    *released.lock().unwrap() = true;
    changed.notify_all();
    worker.join().unwrap();

    assert!(!shutdown.is_triggered());
    assert!(failure.error_snapshot().is_none());
}

#[test]
fn ordinary_shutdown_discards_in_flight_poll_and_read_cancellation() {
    assert_in_flight_io_cancellation_is_not_terminal(BlockingIoStage::Poll);
    assert_in_flight_io_cancellation_is_not_terminal(BlockingIoStage::Read);
}

#[test]
fn ordinary_shutdown_discards_in_flight_heartbeat_cancellation() {
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = ShutdownLatch::new();
    let failure = Arc::new(FailureStore::default());
    let thread_stop = Arc::clone(&stop);
    let thread_shutdown = shutdown.clone();
    let thread_failure = Arc::clone(&failure);
    let thread_release = Arc::clone(&release);
    let worker = std::thread::spawn(move || {
        run_input_loop(
            BlockingHeartbeat {
                calls: 0,
                entered: Some(entered_tx),
                release: thread_release,
                fail_after_release: true,
            },
            tx,
            ready_tx,
            InputLoopControl {
                stop: thread_stop,
                shutdown: thread_shutdown.clone(),
                failure: thread_failure,
                progress: Arc::new(WatchdogProgress::default()),
                kill_media: noop_kill_media(),
                output_lock: TerminalOutputLock::new(thread_shutdown),
                heartbeat_interval: Duration::from_millis(5),
                owner_probe_interval: Duration::from_secs(1),
            },
        );
    });
    assert_eq!(ready_rx.recv().unwrap(), Ok(()));
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    stop.store(true, Ordering::Release);
    let (released, changed) = &*release;
    *released.lock().unwrap() = true;
    changed.notify_all();
    worker.join().unwrap();

    assert!(!shutdown.is_triggered());
    assert!(failure.error_snapshot().is_none());
}

#[test]
fn ordinary_shutdown_wins_after_watchdog_detection_but_before_publication() {
    let progress = Arc::new(WatchdogProgress::default());
    progress.enter(LivenessStage::CprWait);
    progress.arm();
    let stop = Arc::new(AtomicBool::new(false));
    let shutdown = ShutdownLatch::new();
    let failure = Arc::new(FailureStore::default());
    let killed = Arc::new(AtomicBool::new(false));
    let kill_observation = Arc::clone(&killed);
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let hook_release = Arc::clone(&release);
    let hook: KillMedia = Arc::new(move || {
        let _ = entered_tx.send(());
        let (lock, changed) = &*hook_release;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = changed.wait(released).unwrap();
        }
    });

    let thread_stop = Arc::clone(&stop);
    let thread_shutdown = shutdown.clone();
    let thread_failure = Arc::clone(&failure);
    let watchdog = std::thread::spawn(move || {
        run_watchdog(
            progress,
            thread_stop,
            thread_shutdown.clone(),
            thread_failure,
            Arc::new(move || kill_observation.store(true, Ordering::Release)),
            TerminalOutputLock::new(thread_shutdown),
            (Duration::ZERO, Some(hook)),
        );
    });
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    shutdown.trigger();
    stop.store(true, Ordering::Release);
    let (released, changed) = &*release;
    *released.lock().unwrap() = true;
    changed.notify_all();
    watchdog.join().unwrap();

    assert!(shutdown.is_triggered());
    assert!(failure.error_snapshot().is_none());
    assert!(!killed.load(Ordering::Acquire));
}

#[test]
fn owner_output_contention_neither_confirms_nor_clears_transport_suspicion() {
    let progress = WatchdogProgress::default();
    let mut suspicions = SuspicionTracker::default();
    let now = Instant::now();
    assert_eq!(
        evaluate_probe_outcomes(
            [ProbeOutcome::transport(
                io::Error::new(io::ErrorKind::TimedOut, "first CPR timeout"),
                1,
                LivenessStage::CprWait,
            )],
            &mut suspicions,
            &progress,
            false,
            now,
        )
        .unwrap(),
        ProbeDisposition::Retry
    );
    assert_eq!(
        evaluate_probe_outcomes(
            [ProbeOutcome::OwnerOutputBusy],
            &mut suspicions,
            &progress,
            false,
            now + Duration::from_secs(1),
        )
        .unwrap(),
        ProbeDisposition::Complete
    );
    let failure = evaluate_probe_outcomes(
        [ProbeOutcome::transport(
            io::Error::new(io::ErrorKind::TimedOut, "later CPR timeout"),
            2,
            LivenessStage::CprWait,
        )],
        &mut suspicions,
        &progress,
        false,
        now + Duration::from_secs(2),
    )
    .unwrap_err();
    assert_eq!(failure.class, TerminalFailureClass::AmbiguousExhausted);
}

#[test]
fn worker_return_after_publishing_shutdown_is_not_reported_as_unexpected() {
    let stop = AtomicBool::new(false);
    let shutdown = ShutdownLatch::new();
    let failure = Arc::new(FailureStore::default());
    assert!(failure.record_primary(TerminalFailure::runtime(
        io::ErrorKind::BrokenPipe,
        TerminalFailureClass::DefinitiveLoss,
        LivenessStage::InputRead,
        Some(EvidenceDomain::Transport),
        Duration::ZERO,
        "authoritative terminal failure",
    )));
    shutdown.trigger();

    signal_abnormal_worker_exit(
        "terminal input worker",
        Ok(()),
        &stop,
        &shutdown,
        &failure,
        &|| panic!("expected return must not repeat emergency teardown"),
        &TerminalOutputLock::new(shutdown.clone()),
    );

    let snapshot = failure.error_snapshot().unwrap().to_string();
    assert!(snapshot.contains("authoritative terminal failure"));
    assert!(!snapshot.contains("returned unexpectedly"));
}
