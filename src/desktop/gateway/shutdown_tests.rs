use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{mpsc, oneshot, watch};

use super::{GatewayHandle, OutEnvelope, SubscriptionState};

#[cfg(unix)]
fn socket_endpoint(label: &str) -> String {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir()
        .join(format!(
            "ytt-gw-{label}-{}-{nonce}.sock",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

fn handle_with_worker(
    shutdown: oneshot::Sender<()>,
    worker: std::thread::JoinHandle<()>,
) -> GatewayHandle {
    let (commands, _command_rx) = mpsc::channel::<OutEnvelope>(1);
    let (subscriptions, _subscription_rx) = watch::channel(SubscriptionState::default());
    GatewayHandle {
        shutdown: Some(shutdown),
        worker: Some(worker),
        commands,
        subscriptions,
        online: Arc::new(AtomicBool::new(true)),
    }
}

#[test]
fn stop_signals_and_reaps_the_worker() {
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let finished = Arc::new(AtomicBool::new(false));
    let worker_finished = Arc::clone(&finished);
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let _ = shutdown_rx.await;
        });
        worker_finished.store(true, Ordering::Release);
    });

    handle_with_worker(shutdown_tx, worker).stop();

    assert!(
        finished.load(Ordering::Acquire),
        "stop must return only after the worker exits"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn latched_shutdown_beats_ready_session_work() {
    use std::time::Duration;

    use interprocess::local_socket::tokio::Stream;
    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::{GenericFilePath, ListenerOptions};
    use tokio::io::{AsyncBufReadExt, BufReader};

    let endpoint = socket_endpoint("priority");
    let name = endpoint.as_str().to_fs_name::<GenericFilePath>().unwrap();
    let listener = ListenerOptions::new().name(name).create_tokio().unwrap();
    let name = endpoint.as_str().to_fs_name::<GenericFilePath>().unwrap();
    let conn = Stream::connect(name).await.unwrap();
    let peer = listener.accept().await.unwrap();

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    shutdown_tx.send(()).unwrap();
    let (command_tx, mut command_rx) = mpsc::channel(1);
    command_tx
        .try_send(OutEnvelope {
            v: 1,
            id: Some(7),
            page_id: None,
            request_id: None,
            kind: super::OutKind::Cmd,
            name: "next".to_string(),
            payload: serde_json::Value::Null,
        })
        .unwrap();
    let (_subscription_tx, mut subscription_rx) = watch::channel(SubscriptionState::default());
    let online = AtomicBool::new(true);
    let reason = super::run_session(
        conn,
        &mut shutdown_rx,
        &mut command_rx,
        &mut subscription_rx,
        &|_| {},
        &online,
        "shutdown-test",
    )
    .await;

    assert_eq!(reason, "shutdown");
    assert!(!online.load(Ordering::Acquire));
    assert_eq!(command_rx.try_recv().unwrap().id, Some(7));
    let mut reader = BufReader::new(&peer);
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_millis(100), reader.read_line(&mut line))
        .await
        .expect("shutdown must close the session")
        .unwrap();
    assert_eq!(read, 0, "shutdown must not write a frame: {line:?}");
    let _ = std::fs::remove_file(endpoint);
}

#[cfg(unix)]
#[tokio::test]
async fn drop_reaps_a_writer_stalled_by_a_non_reading_peer() {
    use std::io;
    use std::time::{Duration, Instant};

    use interprocess::local_socket::tokio::Listener;
    use interprocess::local_socket::tokio::Stream;
    use interprocess::local_socket::tokio::prelude::*;
    use interprocess::local_socket::{GenericFilePath, ListenerOptions};

    let endpoint = socket_endpoint("stalled");
    let _ = std::fs::remove_file(&endpoint);
    let name = endpoint.as_str().to_fs_name::<GenericFilePath>().unwrap();
    let listener: Listener = ListenerOptions::new().name(name).create_tokio().unwrap();

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
    let finished = Arc::new(AtomicBool::new(false));
    let worker_finished = Arc::clone(&finished);
    let worker_endpoint = endpoint.clone();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let name = worker_endpoint
                .as_str()
                .to_fs_name::<GenericFilePath>()
                .unwrap();
            let conn = Stream::connect(name).await.unwrap();
            let payload = vec![b'x'; 8 * 1024 * 1024];
            let _shutdown_rx = shutdown_rx;
            ready_tx.send(()).unwrap();
            let outcome = super::write_bytes_with_timeout(&conn, &payload, super::WRITE_TIMEOUT)
                .await
                .map_err(|error| error.kind());
            outcome_tx.send(outcome).unwrap();
        });
        worker_finished.store(true, Ordering::Release);
    });
    let handle = handle_with_worker(shutdown_tx, worker);

    let _peer = tokio::time::timeout(Duration::from_secs(1), listener.accept())
        .await
        .expect("worker must connect")
        .unwrap();
    ready_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("worker must enter the bounded write");
    let started = Instant::now();
    drop(handle);
    let elapsed = started.elapsed();

    assert_eq!(
        outcome_rx.recv_timeout(Duration::from_millis(100)).unwrap(),
        Err(io::ErrorKind::TimedOut),
        "the non-reading peer must hit the write deadline"
    );
    assert!(
        finished.load(Ordering::Acquire),
        "drop must join rather than detach the stalled worker"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "bounded writer shutdown took {elapsed:?}"
    );
    let _ = std::fs::remove_file(endpoint);
}
