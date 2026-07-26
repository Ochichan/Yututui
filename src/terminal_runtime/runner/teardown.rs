use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Result;

use crate::app::App;
use crate::runtime::RuntimeEvent;
use crate::{media, persist, remote, runtime};

use super::buffered_events::BufferedWorkerEvents;
use super::player_startup::PlayerStartup;
use super::{TerminalBackgroundTasks, flush_owner_persistence};

const SECONDARY_OWNER_IO_MARKER: &str = "; secondary owner I/O failure: ";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct OwnerIngressDrain {
    pub(super) remote_requests: usize,
    pub(super) subscribe_requests: usize,
}

impl OwnerIngressDrain {
    pub(super) fn absorb(&mut self, other: Self) {
        self.remote_requests += other.remote_requests;
        self.subscribe_requests += other.subscribe_requests;
    }
}

/// One ordered teardown protocol shared by clean exits and fatal owner-loop errors.
///
/// Keeping the ordering in a small driver makes it impossible for a newly added error branch to
/// return between actor shutdown steps, and lets tests exercise the contract without launching a
/// terminal, mpv, or any background actor.
pub(super) trait OwnerTeardown {
    fn quiesce_remote(&mut self);
    fn seal_playback_observation(&mut self);
    fn retire_player(&mut self);
    fn deactivate_media(&mut self);
    async fn shutdown_scrobble_with_owner_pump(&mut self) -> (OwnerIngressDrain, Result<()>);
    fn pump_open_subsonic(&mut self) -> Result<()>;
    fn close_ingress(&mut self);
    async fn drain_owner_ingress(&mut self) -> OwnerIngressDrain;
    async fn await_remote_reply_flush(&mut self);
    async fn shutdown_remote(&mut self);
    async fn reap_player_startup(&mut self);
    fn close_video(&mut self);
    async fn shutdown_terminal_background(&mut self) -> Option<std::io::Error>;
    async fn shutdown_resolver(&mut self);
    async fn shutdown_runtime_background(&mut self) -> runtime::BackgroundShutdown;
    async fn shutdown_transfer(&mut self);
    async fn shutdown_downloads(&mut self);
    async fn finalize_runtime_background(&mut self);
    async fn flush_persistence(&mut self) -> Result<()>;
}

pub(super) struct LiveOwnerTeardown<'a> {
    pub(super) app: &'a mut App,
    pub(super) handles: &'a mut runtime::RuntimeHandles,
    pub(super) player_startup: &'a mut PlayerStartup,
    pub(super) terminal_background: &'a mut TerminalBackgroundTasks,
    pub(super) media: &'a mut media::MediaSession,
    pub(super) publisher: Option<&'a remote::publish::Publisher>,
    pub(super) remote_guard: Option<&'a mut remote::server::InstanceGuard>,
    pub(super) persist: &'a persist::PersistHandle,
    pub(super) pending_worker_events: &'a mut BufferedWorkerEvents,
    pub(super) pending_shutdown_events: &'a mut VecDeque<RuntimeEvent>,
    pub(super) worker_rx: &'a mut tokio::sync::mpsc::Receiver<RuntimeEvent>,
}

impl LiveOwnerTeardown<'_> {
    fn reduce_runtime_shutdown_event(
        &mut self,
        event: RuntimeEvent,
        drain: &mut OwnerIngressDrain,
    ) {
        match event {
            RuntimeEvent::Remote(
                remote_event @ (remote::server::RemoteEvent::Command(_, _)
                | remote::server::RemoteEvent::SessionCommand { .. }),
            ) => {
                drain.remote_requests += 1;
                self.handles
                    .reduce_shutdown_event(self.app, RuntimeEvent::Remote(remote_event));
            }
            RuntimeEvent::Remote(remote::server::RemoteEvent::SessionSubscribe {
                session,
                frame_id,
                page_id,
                topics: _,
                settlement,
            }) => {
                drain.subscribe_requests += 1;
                if !self.publisher.is_some_and(|publisher| {
                    publisher.reject_subscribe_for_shutdown(
                        &session,
                        page_id.as_deref(),
                        frame_id,
                        settlement,
                    )
                }) {
                    tracing::debug!(
                        frame_id,
                        ?page_id,
                        "retired queued session subscribe during owner shutdown"
                    );
                }
            }
            RuntimeEvent::TelemetryWake => {
                for coalesced in self.handles.drain_background_coalesced() {
                    self.reduce_runtime_shutdown_event(coalesced, drain);
                }
            }
            event => self.handles.reduce_shutdown_event(self.app, event),
        }
    }
}

impl OwnerTeardown for LiveOwnerTeardown<'_> {
    fn quiesce_remote(&mut self) {
        if let Some(guard) = self.remote_guard.as_deref_mut() {
            guard.quiesce_owner_admission();
        } else if let Some(publisher) = self.publisher {
            publisher.quiesce_owner_admission();
        }
    }

    fn seal_playback_observation(&mut self) {
        let snapshot = self.app.media_snapshot();
        if let Err(error) = self.handles.scrobble_observe_shutdown(&snapshot) {
            // Busy terminal observations are retained by ScrobbleHandle's shutdown retry slot.
            // Other outcomes are diagnosed by the correlated actor shutdown receipt below.
            tracing::debug!(
                delivery_outcome = error.reason(),
                "final scrobble observation will settle at the shutdown frontier"
            );
        }
    }

    fn retire_player(&mut self) {
        self.handles.begin_player_shutdown(self.app);
    }

    fn deactivate_media(&mut self) {
        let _ = self.media.set_enabled(false);
    }

    async fn shutdown_scrobble_with_owner_pump(&mut self) -> (OwnerIngressDrain, Result<()>) {
        let mut drain = OwnerIngressDrain::default();
        while let Some(event) = self.pending_worker_events.pop_front() {
            self.reduce_runtime_shutdown_event(event, &mut drain);
        }
        while let Some(event) = self.pending_shutdown_events.pop_front() {
            self.reduce_runtime_shutdown_event(event, &mut drain);
        }

        let Some(mut scrobble) = self.handles.take_scrobble_for_shutdown() else {
            return (drain, Ok(()));
        };
        let shutdown = scrobble.shutdown_and_join(Duration::from_millis(1500));
        tokio::pin!(shutdown);
        let mut ingress_open = true;
        loop {
            tokio::select! {
                biased;
                outcome = shutdown.as_mut() => {
                    let outcome = outcome.map_err(|error| {
                        anyhow::anyhow!("scrobble shutdown durability failed: {error}")
                    });
                    return (drain, outcome);
                }
                event = self.worker_rx.recv(), if ingress_open => match event {
                    Some(event) => self.reduce_runtime_shutdown_event(event, &mut drain),
                    None => ingress_open = false,
                },
            }
        }
    }

    fn pump_open_subsonic(&mut self) -> Result<()> {
        let result = self
            .handles
            .pump_open_subsonic_for_shutdown(self.app)
            .map_err(|error| {
                anyhow::anyhow!("music server playback report durability failed: {error}")
            });
        // The credential owner remains live through the final receipt pump, but no loopback route
        // or secret-bearing task may outlive the completed playback durability frontier.
        self.handles.retire_open_subsonic_runtime();
        result
    }

    fn close_ingress(&mut self) {
        // Reject new producers without closing the receiver: callback retries are released, while
        // already accepted main/deferred/coalesced events remain available to the final drain.
        self.handles.close_event_ingress();
    }

    async fn drain_owner_ingress(&mut self) -> OwnerIngressDrain {
        let mut drain = OwnerIngressDrain::default();
        while let Some(event) = self.pending_worker_events.pop_front() {
            self.reduce_runtime_shutdown_event(event, &mut drain);
        }
        while let Some(event) = self.pending_shutdown_events.pop_front() {
            self.reduce_runtime_shutdown_event(event, &mut drain);
        }
        loop {
            while let Ok(event) = self.worker_rx.try_recv() {
                self.reduce_runtime_shutdown_event(event, &mut drain);
            }
            if self.handles.background_ingress_is_idle() {
                match self.worker_rx.try_recv() {
                    Ok(event) => {
                        self.reduce_runtime_shutdown_event(event, &mut drain);
                        continue;
                    }
                    Err(
                        tokio::sync::mpsc::error::TryRecvError::Empty
                        | tokio::sync::mpsc::error::TryRecvError::Disconnected,
                    ) => break,
                }
            }
            // The drainer may publish after the empty try-receive or may have completed its final
            // send without retiring in-flight accounting yet. Yield and recheck both predicates;
            // never wait on a receiver whose sender handles deliberately remain alive.
            tokio::task::yield_now().await;
        }
        for coalesced in self.handles.drain_background_coalesced() {
            self.reduce_runtime_shutdown_event(coalesced, &mut drain);
        }
        self.worker_rx.close();
        drain
    }

    async fn await_remote_reply_flush(&mut self) {
        if let Some(publisher) = self.publisher
            && !publisher.wait_for_wire_settlements().await
        {
            // The structural barrier owns the normal path. This tiny final window is only a
            // scheduler fallback after the bounded writer budget was exhausted and logged.
            remote::await_shutdown_reply_grace().await;
        }
    }

    async fn shutdown_remote(&mut self) {
        // The endpoint must be unlinked while this guard still owns the listener. If the hub were
        // latched first, the listener could drop and a successor could rebind before our late
        // path cleanup, letting the old process delete the new socket.
        if let Some(guard) = self.remote_guard.as_deref_mut() {
            guard.release_endpoint();
        }
        if let Some(publisher) = self.publisher {
            publisher.shutting_down();
        }
        if let Some(guard) = self.remote_guard.as_deref_mut() {
            guard.shutdown().await;
        }
    }

    async fn reap_player_startup(&mut self) {
        self.player_startup.cancel_and_join().await;
    }

    fn close_video(&mut self) {
        self.app.close_video();
    }

    async fn shutdown_terminal_background(&mut self) -> Option<std::io::Error> {
        self.terminal_background.shutdown().await
    }

    async fn shutdown_resolver(&mut self) {
        self.handles
            .resolver_shutdown(Duration::from_millis(3500))
            .await;
    }

    async fn shutdown_runtime_background(&mut self) -> runtime::BackgroundShutdown {
        // Runtime-local jobs close admission together with the player barrier. Cancellable work
        // is aborted; real blocking work gets a bounded join window and reports exact leftovers.
        // The teardown driver preserves a timeout and invokes this once more after transfer and
        // download actors stop, before persistence flush.
        self.handles
            .background_shutdown(Duration::from_millis(3500))
            .await
    }

    async fn shutdown_transfer(&mut self) {
        // Transfer owns auth, playlist, and import child tasks. Stop it while the runtime ingress
        // still exists so the actor can interrupt reliable retries, then reap every child.
        self.handles
            .transfer_shutdown(Duration::from_millis(3500))
            .await;
    }

    async fn shutdown_downloads(&mut self) {
        // Stop yt-dlp/ffmpeg process groups before slower persistence/scrobble work.
        self.handles
            .download_shutdown(Duration::from_millis(3500))
            .await;
    }

    async fn finalize_runtime_background(&mut self) {
        let fallback = self.handles.finalize_background().await;
        // Main/deferred/coalesced work was settled before the remote/session owner closed. Only
        // exact terminal completions which crossed the closed-ingress boundary remain here.
        for event in fallback {
            self.handles.reduce_shutdown_event(self.app, event);
        }
    }

    async fn flush_persistence(&mut self) -> Result<()> {
        // Publish all authoritative quit snapshots, then drain the actor. A timeout retries every
        // still-owned journal frontier and reports any operation whose durability is unconfirmed.
        flush_owner_persistence(self.app, self.persist).await
    }
}

pub(super) async fn complete_owner_teardown<T: OwnerTeardown>(
    teardown: &mut T,
    owner_error: Option<anyhow::Error>,
) -> Result<()> {
    // Remote token admission must close before player retirement closes the shared worker lane.
    // Both boundaries are synchronous; this total order proves no untracked remote event can
    // cross between owner-ingress close and the final drain.
    teardown.quiesce_remote();
    // Capture a fresh monotonic observation while the player projection is still available.
    // Converting it to a terminal observation credits the final wall-clock interval exactly once.
    teardown.seal_playback_observation();
    teardown.retire_player();
    // Remove the OS media surface before slower durability work so a fast successor can become the
    // active system media target.
    teardown.deactivate_media();
    // The scrobble actor can discover one final OpenSubsonic threshold while draining observations
    // accepted before player retirement. Keep the owner ingress open and actively reduce those
    // events until the actor joins.
    let (mut ingress_drain, scrobble_result) = teardown.shutdown_scrobble_with_owner_pump().await;
    // Every producer which can create a playback report is now joined. Close the remaining
    // producer-facing ingress before other actor joins to break callback saturation cycles.
    teardown.close_ingress();
    // Settle every request/event accepted before the ingress boundary while the remote session
    // writers and ordinary reducer dependencies are still live. Post-boundary task completions
    // are retained separately and applied by the final background barrier below.
    ingress_drain.absorb(teardown.drain_owner_ingress().await);
    // A threshold emitted immediately before the scrobble actor joined can be part of this final
    // drain. Pump already-ready bridge receipts after the complete accepted set is visible; an
    // unresolved Submission stays in the scrobble journal for restart replay.
    let open_subsonic_result = teardown.pump_open_subsonic();
    tracing::debug!(
        remote_requests = ingress_drain.remote_requests,
        subscribe_requests = ingress_drain.subscribe_requests,
        "terminal shutdown ingress drained"
    );
    // Requests handled on the final normal reducer turn (not just events found by this drain) may
    // still own wire tokens. The tracker returns immediately when none exist.
    teardown.await_remote_reply_flush().await;
    // Release the advertised endpoint and stop accepting before any potentially slow actor or
    // blocking-job wait. This also makes a fast successor safe while the old owner drains state.
    teardown.shutdown_remote().await;
    teardown.reap_player_startup().await;
    teardown.close_video();
    let terminal_background_error = teardown.shutdown_terminal_background().await;
    teardown.shutdown_resolver().await;
    let first_background_shutdown = teardown.shutdown_runtime_background().await;
    teardown.shutdown_transfer().await;
    teardown.shutdown_downloads().await;
    // A started `spawn_blocking` closure cannot be aborted. Reap it again only after the actor
    // shutdown windows have elapsed, but still before the final App snapshots are flushed. This
    // both gives real work a second bounded completion window and prevents a timed-out deletion or
    // recorder job from racing a clean-looking persistence barrier unnoticed.
    if !first_background_shutdown.is_drained() {
        let _retry_diagnostic = teardown.shutdown_runtime_background().await;
    }
    // Diagnostic deadlines may expire, but the persistence frontier may not cross a live direct
    // mutator. Recover every non-abortable join and synchronously apply all retained completions.
    teardown.finalize_runtime_background().await;
    let persistence_error = teardown.flush_persistence().await.err();
    let scrobble_error = scrobble_result.err();
    let open_subsonic_error = open_subsonic_result.err();

    let mut terminal_error = merge_terminal_shutdown_error(owner_error, terminal_background_error);
    if let Some(persistence_error) = persistence_error {
        terminal_error = Some(match terminal_error {
            Some(error) => error.context(format!(
                "persistence shutdown also failed: {persistence_error:#}"
            )),
            None => persistence_error,
        });
    }
    if let Some(scrobble_error) = scrobble_error {
        terminal_error = Some(match terminal_error {
            Some(error) => {
                error.context(format!("scrobble shutdown also failed: {scrobble_error:#}"))
            }
            None => scrobble_error,
        });
    }
    if let Some(open_subsonic_error) = open_subsonic_error {
        terminal_error = Some(match terminal_error {
            Some(error) => error.context(format!(
                "music server bridge shutdown also failed: {open_subsonic_error:#}"
            )),
            None => open_subsonic_error,
        });
    }
    terminal_error.map_or(Ok(()), Err)
}

/// Preserve an already-observed owner failure while adding terminal-worker teardown diagnostics.
/// A later FailureStore snapshot can be the same liveness failure with join context appended; in
/// that case use the enriched snapshot instead of repeating the primary message twice.
pub(super) fn merge_terminal_shutdown_error(
    primary: Option<anyhow::Error>,
    terminal: Option<std::io::Error>,
) -> Option<anyhow::Error> {
    match (primary, terminal) {
        (None, None) => None,
        (Some(primary), None) => Some(primary),
        (None, Some(terminal)) => Some(terminal.into()),
        (Some(primary), Some(terminal)) => {
            let primary_message = primary.to_string();
            let terminal_message = terminal.to_string();
            if terminal_message == primary_message {
                Some(primary)
            } else if terminal_message.starts_with(&format!(
                "{primary_message}; secondary terminal shutdown failures:"
            )) {
                Some(terminal.into())
            } else if let Some((terminal_primary, owner_secondary)) =
                primary_message.split_once(SECONDARY_OWNER_IO_MARKER)
                && (terminal_message == terminal_primary
                    || terminal_message.starts_with(&format!(
                        "{terminal_primary}; secondary terminal shutdown failures:"
                    )))
            {
                Some(
                    std::io::Error::new(
                        terminal.kind(),
                        format!("{terminal_message}{SECONDARY_OWNER_IO_MARKER}{owner_secondary}"),
                    )
                    .into(),
                )
            } else {
                Some(primary.context(format!(
                    "terminal worker shutdown also failed: {terminal_message}"
                )))
            }
        }
    }
}

/// Prefer a terminal cause already published by FailureStore over the owner write error caused by
/// its output cancellation. Preserve the derived owner failure as bounded string context while
/// retaining the terminal I/O kind and terminal message as the top-level error.
pub(super) fn prefer_terminal_failure(
    derived_owner: Option<anyhow::Error>,
    terminal: Option<std::io::Error>,
) -> Option<anyhow::Error> {
    match (derived_owner, terminal) {
        (None, None) => None,
        (Some(owner), None) => Some(owner),
        (None, Some(terminal)) => Some(terminal.into()),
        (Some(owner), Some(terminal)) => {
            let owner_message = format!("{owner:#}");
            let terminal_message = terminal.to_string();
            if owner.to_string() == terminal_message {
                Some(terminal.into())
            } else {
                Some(
                    std::io::Error::new(
                        terminal.kind(),
                        format!("{terminal_message}{SECONDARY_OWNER_IO_MARKER}{owner_message}"),
                    )
                    .into(),
                )
            }
        }
    }
}
