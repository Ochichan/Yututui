use anyhow::Result;

use crate::runtime;

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
    async fn shutdown_terminal_background(&mut self);
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
    teardown.shutdown_terminal_background().await;
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

    let mut terminal_error = owner_error;
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
