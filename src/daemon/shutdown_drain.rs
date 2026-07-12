//! Final owner-lane drain after daemon producer admission closes.

use std::collections::VecDeque;

use tokio::sync::mpsc;

use super::{DaemonEvent, DaemonEventSender, personal_export::PersonalExport};
use crate::remote::proto::RemoteResponse;
use crate::remote::publish::Publisher;
use crate::remote::server::RemoteEvent;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct DaemonShutdownDrain {
    pub(super) remote_requests: usize,
    pub(super) subscribe_requests: usize,
    pub(super) terminal_events: usize,
    pub(super) personal_export_completions: usize,
    pub(super) coalesced_events: usize,
    pub(super) retired_events: usize,
}

/// Drain every event whose admission linearized before shutdown. The receiver deliberately stays
/// open until the deferred drainer reports idle; producer sender handles can outlive this barrier,
/// so waiting on `recv()` would deadlock if the final send has already completed.
pub(super) async fn drain_daemon_shutdown_ingress(
    event_tx: &DaemonEventSender,
    event_rx: &mut mpsc::Receiver<DaemonEvent>,
    pending_events: &mut VecDeque<DaemonEvent>,
    publisher: &Publisher,
    personal_export: &mut PersonalExport,
) -> DaemonShutdownDrain {
    let mut drain = DaemonShutdownDrain::default();

    while let Some(event) = pending_events.pop_front() {
        settle_event(event_tx, publisher, personal_export, event, &mut drain);
    }

    loop {
        while let Ok(event) = event_rx.try_recv() {
            settle_event(event_tx, publisher, personal_export, event, &mut drain);
        }

        if event_tx.deferred_is_idle() {
            match event_rx.try_recv() {
                Ok(event) => {
                    settle_event(event_tx, publisher, personal_export, event, &mut drain);
                    continue;
                }
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }
        tokio::task::yield_now().await;
    }

    // A wake may have been retired by an actor while the keyed value remained buffered. Producer
    // admission is closed, so this final generation is stable and can be retired exactly once.
    for event in event_tx.drain_coalesced() {
        drain.coalesced_events += 1;
        settle_non_wake(publisher, personal_export, event, &mut drain);
    }
    event_rx.close();
    drain
}

fn settle_event(
    event_tx: &DaemonEventSender,
    publisher: &Publisher,
    personal_export: &mut PersonalExport,
    event: DaemonEvent,
    drain: &mut DaemonShutdownDrain,
) {
    if matches!(event, DaemonEvent::TelemetryWake) {
        for event in event_tx.drain_coalesced() {
            drain.coalesced_events += 1;
            settle_non_wake(publisher, personal_export, event, drain);
        }
    } else {
        settle_non_wake(publisher, personal_export, event, drain);
    }
}

fn settle_non_wake(
    publisher: &Publisher,
    personal_export: &mut PersonalExport,
    event: DaemonEvent,
    drain: &mut DaemonShutdownDrain,
) {
    let kind = event.kind();
    let policy = event.policy();
    match event {
        DaemonEvent::Remote(RemoteEvent::Command(_, reply))
        | DaemonEvent::Remote(RemoteEvent::SessionCommand { reply, .. }) => {
            drain.remote_requests += 1;
            let _ = reply.send(RemoteResponse::err("shutting_down"));
        }
        DaemonEvent::Remote(RemoteEvent::SessionSubscribe {
            session,
            frame_id,
            page_id,
            topics: _,
            settlement,
        }) => {
            drain.subscribe_requests += 1;
            if !publisher.reject_subscribe_for_shutdown(
                &session,
                page_id.as_deref(),
                frame_id,
                settlement,
            ) {
                tracing::debug!(
                    frame_id,
                    ?page_id,
                    "retired superseded or closed session subscribe during daemon shutdown"
                );
            }
        }
        DaemonEvent::Player(event)
            if matches!(
                event.unscoped(),
                crate::player::PlayerEvent::Eof
                    | crate::player::PlayerEvent::Error(_)
                    | crate::player::PlayerEvent::TransportClosed(_)
            ) =>
        {
            drain.terminal_events += 1;
            tracing::debug!(
                event_kind = kind,
                event_policy = policy.name(),
                shutdown_disposition = "retired_terminal",
                "retired daemon terminal event after transport recovery was suppressed"
            );
        }
        DaemonEvent::PersonalExportFinished(finished) => {
            drain.personal_export_completions += 1;
            personal_export.finish(finished);
        }
        event => {
            drain.retired_events += 1;
            tracing::debug!(
                event_kind = event.kind(),
                event_policy = event.policy().name(),
                shutdown_disposition = "retired",
                "retired accepted daemon event during owner shutdown"
            );
        }
    }
}
