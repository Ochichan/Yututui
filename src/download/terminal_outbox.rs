use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Notify, watch};

use super::ownership::OutstandingDownloads;
use super::{
    DownloadCancellation, EventSink, PendingDownloadCommands, emit_terminal,
    lock_pending_download_commands,
};

pub(super) struct TerminalSupervisor {
    pub(super) actor: tokio::task::JoinHandle<()>,
    pub(super) pending: Arc<std::sync::Mutex<PendingDownloadCommands>>,
    pub(super) outstanding: Arc<OutstandingDownloads>,
    pub(super) done_tx: watch::Sender<bool>,
    pub(super) outbox: Arc<TerminalOutbox>,
    pub(super) emit: EventSink,
    pub(super) cancel: watch::Receiver<bool>,
}

/// Durable wake-up edge for terminal downloads.
///
/// The payload remains in [`OutstandingDownloads`]; this object only tells the notifier to scan
/// that bounded owner state.  A notifier panic therefore cannot consume or lose a completion.
#[derive(Default)]
pub(super) struct TerminalOutbox {
    wake: Notify,
    closed: AtomicBool,
}

impl TerminalOutbox {
    pub(super) fn notify(&self) {
        self.wake.notify_one();
    }

    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

pub(super) async fn run(
    outbox: Arc<TerminalOutbox>,
    emit: EventSink,
    outstanding: Arc<OutstandingDownloads>,
    cancel: DownloadCancellation,
) {
    loop {
        let pending = outstanding.terminal_pending_ids();
        if pending.is_empty() {
            if outbox.is_closed() || cancel.is_cancelled() {
                return;
            }
            tokio::select! {
                _ = outbox.wake.notified() => {}
                _ = cancel.cancelled() => return,
            }
            continue;
        }

        for admission_id in pending {
            let Some(event) = outstanding.terminal_event(admission_id) else {
                continue;
            };
            // Saturation is retried inside `emit_terminal`. Every return means the terminal
            // owner either accepted the event, closed, or cancelled; all three retire it.
            let _ = emit_terminal(&emit, event, &cancel).await;
            outstanding.retire(admission_id);
        }
    }
}

pub(super) async fn supervise(supervisor: TerminalSupervisor) {
    let TerminalSupervisor {
        mut actor,
        pending,
        outstanding,
        done_tx,
        outbox,
        emit,
        cancel,
    } = supervisor;
    let mut notifier = spawn_notifier(
        Arc::clone(&outbox),
        Arc::clone(&emit),
        Arc::clone(&outstanding),
        cancel.clone(),
    );
    let mut notifier_joined = false;
    let outcome = loop {
        tokio::select! {
            outcome = &mut actor => break outcome,
            notifier_outcome = &mut notifier => {
                if outstanding.is_intentional_shutdown() {
                    notifier_joined = true;
                    break actor.await;
                }
                match notifier_outcome {
                    Ok(()) => tracing::warn!("download terminal notifier stopped unexpectedly"),
                    Err(error) => tracing::error!(%error, "download terminal notifier panicked or was cancelled"),
                }
                notifier = spawn_notifier(
                    Arc::clone(&outbox),
                    Arc::clone(&emit),
                    Arc::clone(&outstanding),
                    cancel.clone(),
                );
                outbox.notify();
            }
        }
    };
    if outstanding.is_intentional_shutdown() {
        outbox.close();
        if !notifier_joined {
            join_notifier(
                notifier,
                Arc::clone(&outbox),
                Arc::clone(&emit),
                Arc::clone(&outstanding),
                cancel,
            )
            .await;
        }
        outstanding.clear();
        let _ = done_tx.send(true);
        return;
    }

    match outcome {
        Ok(()) => tracing::warn!("download actor stopped unexpectedly"),
        Err(error) => tracing::error!(%error, "download actor panicked or was cancelled"),
    }
    let downloads = {
        let _pending = lock_pending_download_commands(&pending);
        outstanding.running_snapshot()
    };
    for (admission_id, owner) in downloads {
        if outstanding.is_intentional_shutdown() {
            break;
        }
        let event = owner.unexpected_error(
            "download worker stopped unexpectedly; retry the download".to_owned(),
        );
        if outstanding.mark_terminal_pending(admission_id, event) {
            outbox.notify();
        }
    }
    outbox.close();
    join_notifier(notifier, outbox, emit, Arc::clone(&outstanding), cancel).await;
    if outstanding.is_intentional_shutdown() {
        outstanding.clear();
    }
    let _ = done_tx.send(true);
}

fn spawn_notifier(
    outbox: Arc<TerminalOutbox>,
    emit: EventSink,
    outstanding: Arc<OutstandingDownloads>,
    cancel: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(
        outbox,
        emit,
        outstanding,
        DownloadCancellation::watched(cancel),
    ))
}

async fn join_notifier(
    mut notifier: tokio::task::JoinHandle<()>,
    outbox: Arc<TerminalOutbox>,
    emit: EventSink,
    outstanding: Arc<OutstandingDownloads>,
    cancel: watch::Receiver<bool>,
) {
    loop {
        match notifier.await {
            Ok(()) => return,
            Err(error) => {
                tracing::error!(%error, "download terminal notifier failed during drain; restarting");
                if outstanding.is_intentional_shutdown()
                    && outstanding.terminal_pending_ids().is_empty()
                {
                    return;
                }
                notifier = spawn_notifier(
                    Arc::clone(&outbox),
                    Arc::clone(&emit),
                    Arc::clone(&outstanding),
                    cancel.clone(),
                );
                outbox.notify();
            }
        }
    }
}
