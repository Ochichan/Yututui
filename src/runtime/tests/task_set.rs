use std::future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use tokio::sync::oneshot;

use crate::app::Msg;
use crate::runtime::task_set::RuntimeTaskSet;
use crate::runtime::{BackgroundShutdown, RuntimeEvent, RuntimeSender};

const WAIT: Duration = Duration::from_secs(2);

fn terminal_event(video_id: &str) -> RuntimeEvent {
    RuntimeEvent::App(Msg::YtdlpHealResult {
        video_id: video_id.to_owned(),
        updated: false,
    })
}

struct ReleaseOnDrop(Option<mpsc::Sender<()>>);

impl ReleaseOnDrop {
    fn new(tx: mpsc::Sender<()>) -> Self {
        Self(Some(tx))
    }

    fn release(mut self) {
        if let Some(tx) = self.0.take() {
            tx.send(()).expect("background task still owns receiver");
        }
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

struct NotifyOnDrop(Option<oneshot::Sender<()>>);

impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_admission_is_monotonic_and_rejects_new_tasks() {
    let mut tasks = RuntimeTaskSet::new();
    assert!(tasks.close_admission());
    assert!(!tasks.close_admission());

    let blocking_ran = Arc::new(AtomicBool::new(false));
    let blocking_flag = Arc::clone(&blocking_ran);
    assert!(!tasks.spawn_blocking("rejected_blocking", move || {
        blocking_flag.store(true, Ordering::SeqCst);
    }));

    let async_ran = Arc::new(AtomicBool::new(false));
    let async_flag = Arc::clone(&async_ran);
    assert!(!tasks.spawn_cancellable("rejected_async", async move {
        async_flag.store(true, Ordering::SeqCst);
    }));

    tokio::task::yield_now().await;
    assert!(!blocking_ran.load(Ordering::SeqCst));
    assert!(!async_ran.load(Ordering::SeqCst));
    assert_eq!(tasks.pending_counts(), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_reap_removes_all_completed_task_handles() {
    let mut tasks = RuntimeTaskSet::new();
    let (blocking_done_tx, blocking_done_rx) = mpsc::channel();
    let (async_done_tx, async_done_rx) = oneshot::channel();

    assert!(tasks.spawn_blocking("completed_blocking", move || {
        blocking_done_tx
            .send(())
            .expect("test receiver remains open");
    }));
    assert!(tasks.spawn_cancellable("completed_async", async move {
        async_done_tx.send(()).expect("test receiver remains open");
    }));

    blocking_done_rx
        .recv_timeout(WAIT)
        .expect("blocking task completed");
    async_done_rx.await.expect("async task completed");
    tokio::time::timeout(WAIT, async {
        while tasks.pending_counts() != (0, 0) {
            tasks.reap_finished();
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completed joins become reapable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_aborts_async_work_and_waits_for_blocking_work() {
    let mut tasks = RuntimeTaskSet::new();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (async_started_tx, async_started_rx) = oneshot::channel();
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let blocking_finished = Arc::new(AtomicBool::new(false));
    let finished = Arc::clone(&blocking_finished);

    assert!(tasks.spawn_blocking("waited_blocking", move || {
        started_tx.send(()).expect("test receiver remains open");
        release_rx.recv().expect("test releases blocking task");
        finished.store(true, Ordering::SeqCst);
    }));
    assert!(tasks.spawn_cancellable("aborted_async", async move {
        let _notify = NotifyOnDrop(Some(cancelled_tx));
        async_started_tx
            .send(())
            .expect("test waits for cancellable task startup");
        future::pending::<()>().await;
    }));
    started_rx
        .recv_timeout(WAIT)
        .expect("blocking task started");
    tokio::time::timeout(WAIT, async_started_rx)
        .await
        .expect("cancellable task startup stayed within the test budget")
        .expect("cancellable task reached its cancellation guard");

    let releaser = tokio::spawn(async move {
        tokio::task::yield_now().await;
        release_tx.send(()).expect("blocking task still running");
    });
    assert_eq!(tasks.shutdown(WAIT).await, BackgroundShutdown::Drained);
    releaser.await.expect("releaser task joined");
    cancelled_rx
        .await
        .expect("async task was dropped before join");
    assert!(blocking_finished.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_timeout_is_reported_and_the_real_work_remains_tracked() {
    let mut tasks = RuntimeTaskSet::new();
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release = ReleaseOnDrop::new(release_tx);

    assert!(tasks.spawn_blocking("timed_out_blocking", move || {
        started_tx.send(()).expect("test receiver remains open");
        release_rx.recv().expect("test releases blocking task");
    }));
    started_rx
        .recv_timeout(WAIT)
        .expect("blocking task started");

    assert_eq!(
        tasks.shutdown(Duration::ZERO).await,
        BackgroundShutdown::TimedOut {
            blocking_remaining: 1,
            cancellable_remaining: 0,
        }
    );
    assert_eq!(tasks.pending_counts(), (1, 0));

    release.release();
    assert_eq!(tasks.shutdown(WAIT).await, BackgroundShutdown::Drained);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_admission_prevents_a_late_emit_after_receiver_close() {
    let mut tasks = RuntimeTaskSet::new();
    let (runtime_tx, runtime_rx) =
        crate::runtime::channel(crate::util::backpressure::OWNER_EVENT_QUEUE);
    let emitter = tasks.emitter(runtime_tx);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (emit_result_tx, emit_result_rx) = mpsc::channel();
    let release = ReleaseOnDrop::new(release_tx);

    assert!(tasks.spawn_blocking("late_emitter", move || {
        started_tx.send(()).expect("test receiver remains open");
        release_rx.recv().expect("test releases blocking task");
        let emitted = emitter.emit(RuntimeEvent::App(Msg::Noop));
        emit_result_tx
            .send(emitted)
            .expect("test receiver remains open");
    }));
    started_rx
        .recv_timeout(WAIT)
        .expect("background task started");

    assert!(tasks.close_admission());
    drop(runtime_rx);
    release.release();
    assert_eq!(tasks.shutdown(WAIT).await, BackgroundShutdown::Drained);
    assert!(
        !emit_result_rx
            .recv_timeout(WAIT)
            .expect("background task reported admission result")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocking_terminal_completion_retries_saturation_without_losing_payload() {
    let mut tasks = RuntimeTaskSet::new();
    let (raw_tx, mut runtime_rx) = tokio::sync::mpsc::channel(1);
    raw_tx.try_send(terminal_event("filler")).unwrap();
    let emitter = tasks.emitter(RuntimeSender::with_deferred_capacity(raw_tx, 0));
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();

    assert!(tasks.spawn_blocking("terminal_retry", move || {
        started_tx.send(()).unwrap();
        let admitted = emitter.emit_terminal_blocking(terminal_event("kept"));
        done_tx.send(admitted).unwrap();
    }));
    started_rx.recv_timeout(WAIT).unwrap();
    assert!(
        done_rx.recv_timeout(Duration::from_millis(30)).is_err(),
        "completion must remain owned while both bounded lanes are full"
    );

    assert!(matches!(
        runtime_rx.recv().await,
        Some(RuntimeEvent::App(_))
    ));
    assert!(done_rx.recv_timeout(WAIT).unwrap());
    assert!(matches!(
        runtime_rx.recv().await,
        Some(RuntimeEvent::App(Msg::YtdlpHealResult { video_id, .. })) if video_id == "kept"
    ));
    assert_eq!(tasks.shutdown(WAIT).await, BackgroundShutdown::Drained);
}

#[tokio::test(flavor = "current_thread")]
async fn async_terminal_completion_retries_without_blocking_owner_drain() {
    let mut tasks = RuntimeTaskSet::new();
    let (raw_tx, mut runtime_rx) = tokio::sync::mpsc::channel(1);
    raw_tx.try_send(terminal_event("filler")).unwrap();
    let emitter = tasks.emitter(RuntimeSender::with_deferred_capacity(raw_tx, 0));
    let (started_tx, started_rx) = oneshot::channel();
    let (done_tx, done_rx) = oneshot::channel();

    assert!(tasks.spawn_cancellable("async_terminal_retry", async move {
        started_tx.send(()).unwrap();
        let admitted = emitter.emit_terminal(terminal_event("kept-async")).await;
        done_tx.send(admitted).unwrap();
    }));
    started_rx.await.unwrap();
    tokio::task::yield_now().await;

    assert!(matches!(
        runtime_rx.recv().await,
        Some(RuntimeEvent::App(_))
    ));
    assert!(done_rx.await.unwrap());
    assert!(matches!(
        runtime_rx.recv().await,
        Some(RuntimeEvent::App(Msg::YtdlpHealResult { video_id, .. })) if video_id == "kept-async"
    ));
    assert_eq!(tasks.shutdown(WAIT).await, BackgroundShutdown::Drained);
}

#[tokio::test(flavor = "current_thread")]
async fn owner_ingress_closes_before_task_fallback_and_prevents_late_overtaking() {
    let mut tasks = RuntimeTaskSet::new();
    let (raw_tx, mut runtime_rx) = tokio::sync::mpsc::channel(4);
    let runtime_tx = RuntimeSender::with_deferred_capacity(raw_tx, 2);
    let emitter = tasks.emitter(runtime_tx.clone());

    assert!(crate::runtime::emit(&runtime_tx, RuntimeEvent::App(Msg::Noop)).is_ok());

    // The real owner boundary closes worker ingress first, then lets already-running tasks retain
    // terminal completions, and only then closes task admission. A producer which arrives after
    // the fallback therefore cannot enter the earlier main-queue drain.
    assert!(runtime_tx.close_admission());
    assert!(emitter.emit_terminal_blocking(terminal_event("fallback")));
    assert_eq!(
        crate::runtime::emit(&runtime_tx, RuntimeEvent::App(Msg::Noop)),
        Err(crate::util::delivery::DeliveryError::Closed)
    );
    assert!(tasks.close_admission());

    let fallback = tasks.finalize().await;
    assert!(matches!(
        runtime_rx.try_recv(),
        Ok(RuntimeEvent::App(Msg::Noop))
    ));
    assert!(matches!(
        runtime_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(matches!(
        fallback.as_slice(),
        [RuntimeEvent::App(Msg::YtdlpHealResult { video_id, .. })]
            if video_id == "fallback"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_admission_cannot_orphan_an_accepted_recorder_failure() {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random).unwrap();
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let root = std::env::temp_dir().join(format!(
        "yututui-recorder-closed-admission-{}-{suffix}",
        std::process::id()
    ));
    let temp_dir = root.join("temp");
    let final_dir = root.join("final");
    let source = temp_dir.join("accepted.mkv");
    let parked = temp_dir.join("temporarily-unavailable.mkv");
    std::fs::create_dir_all(&temp_dir).unwrap();
    std::fs::write(&source, b"accepted audio survives shutdown").unwrap();

    let accepted = crate::recorder::job::accept_save(crate::recorder::job::RecorderJob::Save {
        id: 41,
        temp: source.clone(),
        temp_dir: temp_dir.clone(),
        final_dir: final_dir.clone(),
        filename: "Recovered".to_owned(),
        ext: "mkv",
        title: None,
        artist: None,
        station: None,
        close_barrier: None,
        automatic: false,
        bypass_limits: false,
    })
    .unwrap();
    std::fs::rename(&source, &parked).unwrap();
    let event = crate::recorder::job::run_accepted(accepted);
    assert!(matches!(
        &event,
        crate::recorder::job::RecorderEvent::SaveDeferred { id: 41, .. }
    ));
    std::fs::rename(&parked, &source).unwrap();

    let mut tasks = RuntimeTaskSet::new();
    let (runtime_tx, _runtime_rx) =
        crate::runtime::channel(crate::util::backpressure::OWNER_EVENT_QUEUE);
    let emitter = tasks.emitter(runtime_tx);
    assert!(tasks.close_admission());
    assert!(emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Recorder(event))));
    let retained = tasks.finalize().await;
    assert!(matches!(
        retained.as_slice(),
        [RuntimeEvent::App(Msg::Recorder(
            crate::recorder::job::RecorderEvent::SaveDeferred { id: 41, .. }
        ))]
    ));
    assert!(
        source.exists(),
        "the accepted source remains recovery-owned"
    );
    assert_eq!(
        std::fs::read_dir(crate::recorder::ownership::pending_dir_for_test(&temp_dir))
            .unwrap()
            .count(),
        1,
        "terminal delivery rejection must not cancel the journal"
    );

    let report = crate::recorder::job::recover_pending(&temp_dir, &final_dir);
    assert_eq!(report.recovered, 1, "warnings: {:?}", report.warnings);
    assert_eq!(
        std::fs::read(final_dir.join("Recovered.mkv")).unwrap(),
        b"accepted audio survives shutdown"
    );
    assert!(
        !source.exists(),
        "startup cleanup runs after successful recovery"
    );
    assert_eq!(tasks.shutdown(WAIT).await, BackgroundShutdown::Drained);
    let _ = std::fs::remove_dir_all(root);
}
