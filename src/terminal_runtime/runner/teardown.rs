use anyhow::Result;

use crate::runtime;

const SECONDARY_OWNER_IO_MARKER: &str = "; secondary owner I/O failure: ";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct OwnerIngressDrain {
    pub(super) remote_requests: usize,
    pub(super) subscribe_requests: usize,
}

/// One ordered teardown protocol shared by clean exits and fatal owner-loop errors.
///
/// Keeping the ordering in a small driver makes it impossible for a newly added error branch to
/// return between actor shutdown steps, and lets tests exercise the contract without launching a
/// terminal, mpv, or any background actor.
pub(super) trait OwnerTeardown {
    fn quiesce_remote(&mut self);
    fn retire_player(&mut self);
    fn close_ingress(&mut self);
    fn deactivate_media(&mut self);
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
    async fn shutdown_scrobble(&mut self) -> Result<()>;
}

pub(super) async fn complete_owner_teardown<T: OwnerTeardown>(
    teardown: &mut T,
    owner_error: Option<anyhow::Error>,
) -> Result<()> {
    // Remote token admission must close before player retirement closes the shared worker lane.
    // Both boundaries are synchronous; this total order proves no untracked remote event can
    // cross between owner-ingress close and the final drain.
    teardown.quiesce_remote();
    teardown.retire_player();
    // Close every producer-facing ingress before awaiting actor joins. A callback that started
    // just before shutdown can otherwise block on a saturated must-deliver lane while the owner
    // waits for the callback's actor, forming a shutdown cycle.
    teardown.close_ingress();
    // Remove the OS media surface while its callbacks can still observe the now-closed ingress.
    // Leaving it registered through the slower actor barrier advertises controls which can only
    // fail and can keep a fast successor from becoming the active system media target.
    teardown.deactivate_media();
    // Settle every request/event accepted before the ingress boundary while the remote session
    // writers and ordinary reducer dependencies are still live. Post-boundary task completions
    // are retained separately and applied by the final background barrier below.
    let ingress_drain = teardown.drain_owner_ingress().await;
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
    let scrobble_error = teardown.shutdown_scrobble().await.err();

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
