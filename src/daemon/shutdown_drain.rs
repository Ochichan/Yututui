//! Final owner-lane drain after daemon producer admission closes.

use std::collections::VecDeque;

use tokio::sync::mpsc;

use super::{
    DaemonEvent, DaemonEventSender, engine::DaemonEngine, personal_export::PersonalExport,
};
use crate::remote::proto::RemoteResponse;
use crate::remote::publish::Publisher;
use crate::remote::server::RemoteEvent;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct DaemonShutdownDrain {
    pub(super) remote_requests: usize,
    pub(super) subscribe_requests: usize,
    pub(super) terminal_events: usize,
    pub(super) personal_export_completions: usize,
    pub(super) open_subsonic_events: usize,
    pub(super) coalesced_events: usize,
    pub(super) retired_events: usize,
}

impl DaemonShutdownDrain {
    pub(super) fn absorb(&mut self, other: Self) {
        self.remote_requests += other.remote_requests;
        self.subscribe_requests += other.subscribe_requests;
        self.terminal_events += other.terminal_events;
        self.personal_export_completions += other.personal_export_completions;
        self.open_subsonic_events += other.open_subsonic_events;
        self.coalesced_events += other.coalesced_events;
        self.retired_events += other.retired_events;
    }

    pub(super) fn log_summary(self) {
        tracing::debug!(
            remote_requests = self.remote_requests,
            subscribe_requests = self.subscribe_requests,
            terminal_events = self.terminal_events,
            personal_export_completions = self.personal_export_completions,
            open_subsonic_events = self.open_subsonic_events,
            coalesced_events = self.coalesced_events,
            retired_events = self.retired_events,
            "daemon shutdown ingress drained"
        );
    }
}

pub(super) fn playback_report_frontier_succeeded(
    scrobble: Result<(), crate::util::delivery::DeliveryError>,
    open_subsonic: Result<(), crate::open_subsonic::ServiceError>,
) -> bool {
    match (scrobble, open_subsonic) {
        (Ok(()), Ok(())) => true,
        (Err(error), _) => {
            tracing::warn!(%error, "scrobble shutdown durability was not confirmed");
            false
        }
        (Ok(()), Err(error)) => {
            tracing::warn!(%error, "music server playback report durability was not confirmed");
            false
        }
    }
}

/// Capture one fresh monotonic tail before retiring audio ownership. A saturated terminal lane
/// retains this observation in the scrobble handle's shutdown-only retry slot.
pub(super) fn seal_final_playback_observation(
    scrobble: &mut crate::scrobble::ScrobbleHandle,
    engine: &DaemonEngine,
) {
    if let Err(error) = scrobble.observe_shutdown(&engine.media_snapshot()) {
        tracing::debug!(
            delivery_outcome = error.reason(),
            "final daemon scrobble observation will settle at the shutdown frontier"
        );
    }
}

/// Join the final playback-threshold producer while its owner lane remains live, then close
/// admission and pump already-ready credential-owner receipts. Unresolved submissions stay in
/// the scrobble journal instead of extending shutdown behind a network request.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drain_playback_report_frontier(
    scrobble: &mut crate::scrobble::ScrobbleHandle,
    event_tx: &DaemonEventSender,
    event_rx: &mut mpsc::Receiver<DaemonEvent>,
    pending_events: &mut VecDeque<DaemonEvent>,
    publisher: &Publisher,
    personal_export: &mut PersonalExport,
    engine: &mut DaemonEngine,
) -> (
    Result<(), crate::util::delivery::DeliveryError>,
    Result<(), crate::open_subsonic::ServiceError>,
    DaemonShutdownDrain,
) {
    let (scrobble_outcome, mut drain) = shutdown_scrobble_with_owner_pump(
        scrobble,
        event_tx,
        event_rx,
        pending_events,
        publisher,
        personal_export,
        engine,
    )
    .await;
    event_tx.close_admission();
    drain.absorb(
        drain_daemon_shutdown_ingress(
            event_tx,
            event_rx,
            pending_events,
            publisher,
            personal_export,
            engine,
        )
        .await,
    );
    let open_subsonic_outcome = engine.pump_open_subsonic_scrobbles_for_shutdown();
    (scrobble_outcome, open_subsonic_outcome, drain)
}

#[allow(clippy::too_many_arguments)]
async fn shutdown_scrobble_with_owner_pump(
    scrobble: &mut crate::scrobble::ScrobbleHandle,
    event_tx: &DaemonEventSender,
    event_rx: &mut mpsc::Receiver<DaemonEvent>,
    pending_events: &mut VecDeque<DaemonEvent>,
    publisher: &Publisher,
    personal_export: &mut PersonalExport,
    engine: &mut DaemonEngine,
) -> (
    Result<(), crate::util::delivery::DeliveryError>,
    DaemonShutdownDrain,
) {
    let mut drain = DaemonShutdownDrain::default();
    while let Some(event) = pending_events.pop_front() {
        settle_daemon_shutdown_event(
            event_tx,
            publisher,
            personal_export,
            engine,
            event,
            &mut drain,
        );
    }

    let shutdown = scrobble.shutdown_and_join(std::time::Duration::from_millis(1500));
    tokio::pin!(shutdown);
    let mut ingress_open = true;
    loop {
        tokio::select! {
            biased;
            outcome = shutdown.as_mut() => return (outcome, drain),
            event = event_rx.recv(), if ingress_open => match event {
                Some(event) => settle_daemon_shutdown_event(
                    event_tx,
                    publisher,
                    personal_export,
                    engine,
                    event,
                    &mut drain,
                ),
                None => ingress_open = false,
            },
        }
    }
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
    engine: &mut DaemonEngine,
) -> DaemonShutdownDrain {
    let mut drain = DaemonShutdownDrain::default();

    while let Some(event) = pending_events.pop_front() {
        settle_daemon_shutdown_event(
            event_tx,
            publisher,
            personal_export,
            engine,
            event,
            &mut drain,
        );
    }

    loop {
        while let Ok(event) = event_rx.try_recv() {
            settle_daemon_shutdown_event(
                event_tx,
                publisher,
                personal_export,
                engine,
                event,
                &mut drain,
            );
        }

        if event_tx.deferred_is_idle() {
            match event_rx.try_recv() {
                Ok(event) => {
                    settle_daemon_shutdown_event(
                        event_tx,
                        publisher,
                        personal_export,
                        engine,
                        event,
                        &mut drain,
                    );
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
        settle_non_wake(publisher, personal_export, engine, event, &mut drain);
    }
    event_rx.close();
    drain
}

pub(super) fn settle_daemon_shutdown_event(
    event_tx: &DaemonEventSender,
    publisher: &Publisher,
    personal_export: &mut PersonalExport,
    engine: &mut DaemonEngine,
    event: DaemonEvent,
    drain: &mut DaemonShutdownDrain,
) {
    if matches!(event, DaemonEvent::TelemetryWake) {
        for event in event_tx.drain_coalesced() {
            drain.coalesced_events += 1;
            settle_non_wake(publisher, personal_export, engine, event, drain);
        }
    } else {
        settle_non_wake(publisher, personal_export, engine, event, drain);
    }
}

fn settle_non_wake(
    publisher: &Publisher,
    personal_export: &mut PersonalExport,
    engine: &mut DaemonEngine,
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
        DaemonEvent::OpenSubsonicBridge(import) => {
            drain.open_subsonic_events += 1;
            engine.accept_open_subsonic_bridge_import(&import);
        }
        DaemonEvent::OpenSubsonicReady => {
            drain.open_subsonic_events += 1;
            engine.maintain_open_subsonic_bridge();
        }
        DaemonEvent::Scrobble(crate::scrobble::ScrobbleEvent::OpenSubsonic {
            event_id,
            kind,
            track,
            confirmation,
        }) => {
            drain.open_subsonic_events += 1;
            engine.queue_open_subsonic_scrobble(event_id, kind, track, confirmation);
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
