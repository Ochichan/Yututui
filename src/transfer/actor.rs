//! The TUI-side transfer actor. Owns its own clients (deliberately NOT routed through
//! the interactive API actor, so a ten-minute import never queues behind — or starves —
//! search), runs one job at a time, and throttles progress events to ~1/s.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinHandle, JoinSet};

use super::checkpoint::TransferReport;
use super::engine::JobCtx;
use super::{
    JobSpec, TransferDest, TransferProgress, TransferSource, new_job_id, run_job,
    write_reviewed_local_job,
};
use crate::config::Config;
use crate::spotify::auth;
use crate::spotify::client::SpotifyClient;
use crate::util::backpressure::{TRANSFER_CONTROL_QUEUE, TRANSFER_QUEUE, bounded_channel};
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};

pub enum TransferCmd {
    /// Start the PKCE flow with the (possibly unsaved) draft Client ID.
    AuthStart {
        client_id: String,
        port: u16,
    },
    Disconnect,
    ListSpotifyPlaylists,
    StartJob(Box<JobSpec>),
    WriteReviewedLocal {
        job_id: String,
    },
    CancelJob,
}

/// Events back to the reducer. No secrets anywhere here.
#[derive(Clone)]
pub enum TransferEvent {
    AuthUrl(String),
    AuthDone {
        display_name: String,
    },
    AuthError(String),
    Disconnected,
    SpotifyPlaylists(Result<Vec<PickerPlaylist>, String>),
    Progress(TransferProgress),
    JobDone(Arc<TransferReport>),
    /// A newly requested job was not admitted because another job still owns the actor.
    /// Unlike a terminal failure, this must not clear the active job's UI guard.
    JobRejected {
        job_id: String,
        error: String,
    },
    JobFailed {
        job_id: String,
        error: String,
        resumable: bool,
    },
}

/// What the picker popup needs per row.
#[derive(Clone)]
pub struct PickerPlaylist {
    pub source: TransferSource,
    pub label: String,
    pub total: u32,
}

type EventSink = Arc<dyn Fn(TransferEvent) -> DeliveryResult + Send + Sync>;

pub struct TransferHandle {
    tx: mpsc::Sender<SequencedCmd>,
    cancel_tx: mpsc::Sender<()>,
    shutdown_tx: watch::Sender<bool>,
    done_rx: Option<oneshot::Receiver<()>>,
    next_sequence: AtomicU64,
    cancel_through: Arc<AtomicU64>,
    list_active: Arc<AtomicBool>,
}

struct SequencedCmd {
    sequence: u64,
    command: TransferCmd,
}

struct ActorChannels {
    commands: mpsc::Receiver<SequencedCmd>,
    cancel: mpsc::Receiver<()>,
    shutdown: watch::Receiver<bool>,
    done: oneshot::Sender<()>,
    cancel_through: Arc<AtomicU64>,
    list_active: Arc<AtomicBool>,
}

struct RunningJob {
    sequence: u64,
    job_id: String,
    task: JoinHandle<TransferEvent>,
}

enum ActorInput {
    Shutdown,
    Cancel,
    AuthUrl(String),
    JobFinished(Result<TransferEvent, JoinError>),
    AuthFinished(Result<TransferEvent, JoinError>),
    ListFinished(Result<TransferEvent, JoinError>),
    Command(SequencedCmd),
}

impl TransferHandle {
    pub fn send(&self, cmd: TransferCmd) -> DeliveryResult {
        if *self.shutdown_tx.borrow() {
            return Err(DeliveryError::Closed);
        }
        if matches!(cmd, TransferCmd::CancelJob) {
            let cancel_through = self.next_sequence.load(Ordering::Acquire);
            self.cancel_through
                .fetch_max(cancel_through, Ordering::AcqRel);
            return match self.cancel_tx.try_send(()) {
                Ok(()) => Ok(DeliveryReceipt::Enqueued),
                Err(mpsc::error::TrySendError::Full(())) => Ok(DeliveryReceipt::Coalesced {
                    replaced_existing: true,
                    evicted_oldest: false,
                }),
                Err(mpsc::error::TrySendError::Closed(())) => Err(DeliveryError::Closed),
            };
        }
        let sequence = self.next_sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let is_list = matches!(&cmd, TransferCmd::ListSpotifyPlaylists);
        if is_list {
            if self.tx.is_closed() {
                return Err(DeliveryError::Closed);
            }
            if self.list_active.swap(true, Ordering::AcqRel) {
                return Ok(DeliveryReceipt::Coalesced {
                    replaced_existing: true,
                    evicted_oldest: false,
                });
            }
        }
        match self.tx.try_send(SequencedCmd {
            sequence,
            command: cmd,
        }) {
            Ok(()) => Ok(DeliveryReceipt::Enqueued),
            Err(mpsc::error::TrySendError::Full(_)) => {
                if is_list {
                    self.list_active.store(false, Ordering::Release);
                }
                Err(DeliveryError::Busy)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if is_list {
                    self.list_active.store(false, Ordering::Release);
                }
                Err(DeliveryError::Closed)
            }
        }
    }

    /// Signal actor shutdown and wait until every accepted child task has been aborted and joined.
    pub async fn shutdown(&mut self) -> bool {
        self.shutdown_tx.send_replace(true);
        let stopped = match self.done_rx.as_mut() {
            Some(done) => done.await.is_ok(),
            None => true,
        };
        if stopped {
            self.done_rx = None;
        }
        stopped
    }
}

impl Drop for TransferHandle {
    fn drop(&mut self) {
        self.shutdown_tx.send_replace(true);
    }
}

pub fn spawn(
    emit: impl Fn(TransferEvent) -> DeliveryResult + Send + Sync + 'static,
) -> TransferHandle {
    let (handle, channels) = actor_channel();
    tokio::spawn(run_actor(channels, Arc::new(emit)));
    handle
}

fn actor_channel() -> (TransferHandle, ActorChannels) {
    let (tx, rx) = bounded_channel(TRANSFER_QUEUE);
    let (cancel_tx, cancel_rx) = bounded_channel(TRANSFER_CONTROL_QUEUE);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (done_tx, done_rx) = oneshot::channel();
    let cancel_through = Arc::new(AtomicU64::new(0));
    let list_active = Arc::new(AtomicBool::new(false));
    (
        TransferHandle {
            tx,
            cancel_tx,
            shutdown_tx,
            done_rx: Some(done_rx),
            next_sequence: AtomicU64::new(0),
            cancel_through: Arc::clone(&cancel_through),
            list_active: Arc::clone(&list_active),
        },
        ActorChannels {
            commands: rx,
            cancel: cancel_rx,
            shutdown: shutdown_rx,
            done: done_tx,
            cancel_through,
            list_active,
        },
    )
}

struct ListActivityGuard(Arc<AtomicBool>);

impl Drop for ListActivityGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn run_actor(mut channels: ActorChannels, emit: EventSink) {
    // PKCE invokes its synchronous callback exactly once. A watch slot gives that callback a
    // bounded, non-blocking actor-owned handoff without spawning an untracked retry task or
    // dropping the URL when the owner queue is temporarily saturated.
    let (auth_url_tx, mut auth_url_rx) = watch::channel(None::<String>);
    let mut auth_task: Option<JoinHandle<TransferEvent>> = None;
    let mut job_task: Option<RunningJob> = None;
    let mut list_tasks = JoinSet::new();

    loop {
        let input = tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut channels.shutdown) => ActorInput::Shutdown,
            cancel = channels.cancel.recv() => match cancel {
                Some(()) => ActorInput::Cancel,
                None => ActorInput::Shutdown,
            },
            url = receive_auth_url(&mut auth_url_rx) => match url {
                Some(url) => ActorInput::AuthUrl(url),
                None => ActorInput::Shutdown,
            },
            result = join_running_job(&mut job_task) => ActorInput::JobFinished(result),
            result = join_optional_task(&mut auth_task) => ActorInput::AuthFinished(result),
            result = join_list_task(&mut list_tasks) => ActorInput::ListFinished(result),
            command = channels.commands.recv() => match command {
                Some(command) => ActorInput::Command(command),
                None => ActorInput::Shutdown,
            },
        };

        let event = match input {
            ActorInput::Shutdown => break,
            ActorInput::Cancel => {
                cancel_running_job(
                    &mut job_task,
                    channels.cancel_through.load(Ordering::Acquire),
                )
                .await
            }
            ActorInput::AuthUrl(url) => Some(TransferEvent::AuthUrl(url)),
            ActorInput::JobFinished(result) => {
                let running = job_task
                    .take()
                    .expect("a job completion requires an owned task");
                Some(job_join_event(running.job_id, result))
            }
            ActorInput::AuthFinished(result) => {
                auth_task = None;
                Some(auth_join_event(result))
            }
            ActorInput::ListFinished(result) => Some(list_join_event(result)),
            ActorInput::Command(SequencedCmd { sequence, command }) => match command {
                TransferCmd::AuthStart { client_id, port } => {
                    if auth_task.is_some() {
                        None // flow already open in the browser
                    } else {
                        auth_task =
                            Some(tokio::spawn(run_auth(client_id, port, auth_url_tx.clone())));
                        None
                    }
                }
                TransferCmd::Disconnect => Some(disconnect_event(
                    crate::spotify::auth::SpotifyToken::delete_saved(),
                )),
                TransferCmd::ListSpotifyPlaylists => {
                    // Construct the guard before spawning so abort-before-first-poll still clears
                    // the admission latch.
                    let activity = ListActivityGuard(Arc::clone(&channels.list_active));
                    list_tasks.spawn(async move {
                        let _activity = activity;
                        TransferEvent::SpotifyPlaylists(list_playlists().await)
                    });
                    None
                }
                TransferCmd::StartJob(spec) => {
                    let job_id = new_job_id(match spec.dest {
                        TransferDest::SpotifyNewPlaylist { .. }
                        | TransferDest::SpotifyMirrorPlaylist { .. } => "yt2sp",
                        _ => "sp2yt",
                    });
                    if sequence <= channels.cancel_through.load(Ordering::Acquire) {
                        Some(cancelled_event(job_id))
                    } else if job_task.is_some() {
                        Some(already_running_event(job_id))
                    } else {
                        let emit = Arc::clone(&emit);
                        let task_job_id = job_id.clone();
                        job_task = Some(RunningJob {
                            sequence,
                            job_id,
                            task: tokio::spawn(async move {
                                run_one_job(task_job_id, *spec, emit).await
                            }),
                        });
                        None
                    }
                }
                TransferCmd::WriteReviewedLocal { job_id } => {
                    if sequence <= channels.cancel_through.load(Ordering::Acquire) {
                        Some(cancelled_event(job_id))
                    } else if job_task.is_some() {
                        Some(already_running_event(job_id))
                    } else {
                        let emit = Arc::clone(&emit);
                        let task_job_id = job_id.clone();
                        job_task = Some(RunningJob {
                            sequence,
                            job_id,
                            task: tokio::spawn(async move {
                                run_reviewed_local_write(task_job_id, emit).await
                            }),
                        });
                        None
                    }
                }
                TransferCmd::CancelJob => {
                    unreachable!("cancellation uses the reserved control lane")
                }
            },
        };

        if let Some(event) = event
            && !emit_reliably(&emit, event, &mut channels.shutdown).await
        {
            break;
        }
    }

    cleanup_actor_tasks(&mut auth_task, &mut job_task, &mut list_tasks).await;
    let _ = channels.done.send(());
}

fn disconnect_event(result: std::io::Result<()>) -> TransferEvent {
    match result {
        Ok(()) => TransferEvent::Disconnected,
        Err(error) => TransferEvent::AuthError(format!("could not remove the token: {error}")),
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

async fn join_running_job(job_task: &mut Option<RunningJob>) -> Result<TransferEvent, JoinError> {
    match job_task.as_mut() {
        Some(running) => (&mut running.task).await,
        None => std::future::pending().await,
    }
}

async fn receive_auth_url(auth_url: &mut watch::Receiver<Option<String>>) -> Option<String> {
    loop {
        if auth_url.changed().await.is_err() {
            return None;
        }
        if let Some(url) = auth_url.borrow_and_update().clone() {
            return Some(url);
        }
    }
}

async fn join_optional_task(
    task: &mut Option<JoinHandle<TransferEvent>>,
) -> Result<TransferEvent, JoinError> {
    match task.as_mut() {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

async fn join_list_task(tasks: &mut JoinSet<TransferEvent>) -> Result<TransferEvent, JoinError> {
    match tasks.join_next().await {
        Some(result) => result,
        None => std::future::pending().await,
    }
}

async fn cancel_running_job(
    job_task: &mut Option<RunningJob>,
    cancel_through: u64,
) -> Option<TransferEvent> {
    let running = job_task.take()?;
    if running.sequence > cancel_through {
        *job_task = Some(running);
        return None;
    }

    // Aborting between awaits is safe: the checkpoint flushes at every chunk boundary and resume
    // reconciles against the destination. Awaiting the handle decides which side actually won:
    // only a cancelled JoinError may manufacture a cancelled terminal.
    running.task.abort();
    let job_id = running.job_id;
    Some(match running.task.await {
        Ok(event) => event,
        Err(error) if error.is_cancelled() => cancelled_event(job_id),
        Err(error) => job_join_error(job_id, error),
    })
}

fn cancelled_event(job_id: String) -> TransferEvent {
    TransferEvent::JobFailed {
        job_id,
        error: "cancelled".to_owned(),
        resumable: true,
    }
}

fn already_running_event(job_id: String) -> TransferEvent {
    TransferEvent::JobRejected {
        job_id,
        error: "a transfer is already running".to_owned(),
    }
}

fn job_join_event(job_id: String, result: Result<TransferEvent, JoinError>) -> TransferEvent {
    match result {
        Ok(event) => event,
        Err(error) => job_join_error(job_id, error),
    }
}

fn job_join_error(job_id: String, error: JoinError) -> TransferEvent {
    tracing::error!(%error, %job_id, "transfer job task failed unexpectedly");
    TransferEvent::JobFailed {
        job_id,
        error: "transfer task failed unexpectedly".to_owned(),
        resumable: true,
    }
}

fn auth_join_event(result: Result<TransferEvent, JoinError>) -> TransferEvent {
    match result {
        Ok(event) => event,
        Err(error) => {
            tracing::error!(%error, "Spotify authorization task failed unexpectedly");
            TransferEvent::AuthError("Spotify authorization task failed unexpectedly".to_owned())
        }
    }
}

fn list_join_event(result: Result<TransferEvent, JoinError>) -> TransferEvent {
    match result {
        Ok(event) => event,
        Err(error) => {
            tracing::error!(%error, "Spotify playlist task failed unexpectedly");
            TransferEvent::SpotifyPlaylists(Err(
                "Spotify playlist request failed unexpectedly".to_owned()
            ))
        }
    }
}

async fn cleanup_actor_tasks(
    auth_task: &mut Option<JoinHandle<TransferEvent>>,
    job_task: &mut Option<RunningJob>,
    list_tasks: &mut JoinSet<TransferEvent>,
) {
    if let Some(task) = auth_task.take() {
        task.abort();
        log_cleanup_join("authorization", task.await);
    }
    if let Some(running) = job_task.take() {
        running.task.abort();
        log_cleanup_join("job", running.task.await);
    }
    list_tasks.abort_all();
    while let Some(result) = list_tasks.join_next().await {
        log_cleanup_join("playlist", result);
    }
}

fn log_cleanup_join(component: &'static str, result: Result<TransferEvent, JoinError>) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        tracing::warn!(%error, component, "transfer child failed during actor cleanup");
    }
}

/// Terminal and response events own reducer state, so a temporary full owner spill queue must
/// delay their producer instead of making the UI guard permanent. Actor shutdown interrupts the
/// retry, and owner closure returns synchronously without leaving a detached retry task behind.
async fn emit_reliably(
    emit: &EventSink,
    event: TransferEvent,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        if *shutdown.borrow() {
            return false;
        }
        match emit(event.clone()) {
            Ok(_) => return true,
            Err(DeliveryError::Closed) => return false,
            Err(
                DeliveryError::Busy
                | DeliveryError::StaleOrFull
                | DeliveryError::BestEffortDropped
                | DeliveryError::Saturated,
            ) => {
                tokio::select! {
                    biased;
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return false;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
        }
    }
}

async fn run_reviewed_local_write(job_id: String, emit: EventSink) -> TransferEvent {
    let mut last_beat: Option<Instant> = None;
    let emit_progress = Arc::clone(&emit);
    let mut progress = move |p: TransferProgress| {
        let due = last_beat.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if due || p.done == p.total {
            last_beat = Some(Instant::now());
            let _ = emit_progress(TransferEvent::Progress(p));
        }
    };
    match write_reviewed_local_job(&job_id, &mut progress).await {
        Ok(report) => TransferEvent::JobDone(Arc::new(report)),
        Err(error) => TransferEvent::JobFailed {
            job_id,
            error: format!("{:#}", error.error),
            resumable: error.resumable,
        },
    }
}

async fn run_auth(
    client_id: String,
    port: u16,
    auth_url_tx: watch::Sender<Option<String>>,
) -> TransferEvent {
    let http = reqwest::Client::builder()
        .user_agent(format!("yututui/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default();
    let flow = auth::run_pkce_flow(&http, &client_id, port, &mut move |url| {
        auth_url_tx.send_replace(Some(url));
    })
    .await;
    match flow {
        Ok(token) => {
            let mut client = SpotifyClient::with_token(token);
            match client.me().await {
                Ok(user) => TransferEvent::AuthDone {
                    display_name: user.label().to_owned(),
                },
                Err(error) => TransferEvent::AuthError(error.to_string()),
            }
        }
        Err(error) => TransferEvent::AuthError(crate::util::sanitize::sanitize_error_text(
            format!("{error:#}"),
        )),
    }
}

async fn list_playlists() -> Result<Vec<PickerPlaylist>, String> {
    let cfg = Config::load();
    let mut client =
        SpotifyClient::from_saved(cfg.spotify.client_id.as_deref()).map_err(|e| e.to_string())?;
    let playlists = client.my_playlists().await.map_err(|e| e.to_string())?;
    let mut items = vec![PickerPlaylist {
        source: TransferSource::SpotifyLiked,
        label: "Liked Songs".to_owned(),
        total: 0,
    }];
    items.extend(playlists.into_iter().map(|p| PickerPlaylist {
        source: TransferSource::SpotifyPlaylist { id: p.id },
        label: p.name,
        total: p.total,
    }));
    Ok(items)
}

async fn run_one_job(job_id: String, spec: JobSpec, emit: EventSink) -> TransferEvent {
    // Fresh config: picks up the cookie/market as they are *now*, not at spawn time.
    let cfg = Config::load();
    let mut ctx = match build_ctx(&spec, &cfg).await {
        Ok(ctx) => ctx,
        Err(error) => {
            return TransferEvent::JobFailed {
                job_id,
                error,
                resumable: false,
            };
        }
    };
    // Throttle the per-track beats to ~1/s for the status line.
    let mut last_beat: Option<Instant> = None;
    let emit_progress = Arc::clone(&emit);
    let mut progress = move |p: TransferProgress| {
        let due = last_beat.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if due || p.done == p.total {
            last_beat = Some(Instant::now());
            let _ = emit_progress(TransferEvent::Progress(p));
        }
    };
    match run_job(job_id.clone(), spec, None, &mut ctx, &mut progress).await {
        Ok(report) => TransferEvent::JobDone(Arc::new(report)),
        Err(error) => TransferEvent::JobFailed {
            job_id,
            error: format!("{:#}", error.error),
            resumable: error.resumable,
        },
    }
}

async fn build_ctx(spec: &JobSpec, cfg: &Config) -> Result<JobCtx, String> {
    let needs_spotify = matches!(
        spec.source,
        TransferSource::SpotifyPlaylist { .. } | TransferSource::SpotifyLiked
    ) || matches!(
        spec.dest,
        TransferDest::SpotifyNewPlaylist { .. } | TransferDest::SpotifyMirrorPlaylist { .. }
    );
    // LocalPlaylist writes locally but still *matches* against YouTube Music.
    let needs_ytm = matches!(spec.source, TransferSource::YtmPlaylist { .. })
        || matches!(
            spec.dest,
            TransferDest::YtmNewPlaylist { .. }
                | TransferDest::YtmExistingPlaylist { .. }
                | TransferDest::YtmLikes
                | TransferDest::LocalPlaylist { .. }
        );
    let spotify = if needs_spotify {
        Some(
            SpotifyClient::from_saved(cfg.spotify.client_id.as_deref())
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let ytm = if needs_ytm {
        match cfg.effective_cookie() {
            Some(cookie) => Some(
                crate::api::ytmusic::YtMusicApi::from_cookie(&cookie)
                    .await
                    .map_err(|e| {
                        crate::util::sanitize::sanitize_error_text(format!(
                            "YouTube Music auth failed: {e:#}"
                        ))
                    })?,
            ),
            // No cookie is fine when YTM is only needed to *search* for matches (a LocalPlaylist
            // dest) — anonymous search falls back to yt-dlp, so a Spotify→Library import still
            // works. Reading a YTM playlist or writing to the account (new/existing playlist,
            // likes) genuinely needs the cookie, so those still fail with a clear message.
            None => {
                let account_op = matches!(spec.source, TransferSource::YtmPlaylist { .. })
                    || matches!(
                        spec.dest,
                        TransferDest::YtmNewPlaylist { .. }
                            | TransferDest::YtmExistingPlaylist { .. }
                            | TransferDest::YtmLikes
                    );
                if account_op {
                    return Err(
                        "this needs a YouTube Music cookie — set one in Settings › General"
                            .to_owned(),
                    );
                }
                Some(crate::api::ytmusic::YtMusicApi::Anonymous)
            }
        }
    } else {
        None
    };
    Ok(JobCtx {
        ytm,
        spotify,
        search_config: cfg.effective_search(),
        market: cfg.spotify.market.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::FileFormat;

    struct RecoveryLatchReset;

    impl Drop for RecoveryLatchReset {
        fn drop(&mut self) {
            crate::persist::clear_startup_recovery_error_for_test();
        }
    }

    #[test]
    fn disconnect_after_late_recovery_reports_auth_error_without_deleting_token() {
        let path = std::env::temp_dir().join(format!(
            "yututui-disconnect-recovery-{}-{}",
            std::process::id(),
            crate::signals::unix_now()
        ));
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, b"token-before-latch").unwrap();

        let reset = RecoveryLatchReset;
        crate::persist::latch_startup_recovery_error_for_test(crate::persist::StoreKind::Config);
        let event = disconnect_event(crate::spotify::auth::remove_auth_file(&path));

        assert!(
            matches!(event, TransferEvent::AuthError(error) if error.contains("revoked after recovery failure"))
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"token-before-latch");

        drop(reset);
        let _ = std::fs::remove_file(path);
    }

    struct DropSignal(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn file_spec(dest: TransferDest) -> JobSpec {
        JobSpec {
            source: TransferSource::File {
                path: "input.csv".into(),
            },
            dest,
            media_kind: crate::transfer::ImportMediaKind::Track,
            dry_run: false,
            min_score: 0.80,
            take_best: false,
            auto_accept_ambiguous_min_score: None,
            match_policy: crate::transfer::MatchPolicy::Strict,
            allow_user_videos: false,
            cache_mode: crate::transfer::TransferCacheMode::Use,
            rematch: false,
        }
    }

    fn config_without_cookie() -> Config {
        Config {
            cookie: None,
            cookies_file: Some(
                std::env::temp_dir().join(format!("yututui-missing-cookie-{}", std::process::id())),
            ),
            ..Config::default()
        }
    }

    #[test]
    fn transfer_handle_forwards_commands_to_actor_channel() {
        let (handle, mut channels) = actor_channel();

        assert_eq!(
            handle.send(TransferCmd::AuthStart {
                client_id: "cid".to_owned(),
                port: 9271,
            }),
            Ok(DeliveryReceipt::Enqueued)
        );
        match channels.commands.try_recv().unwrap().command {
            TransferCmd::AuthStart { client_id, port } => {
                assert_eq!(client_id, "cid");
                assert_eq!(port, 9271);
            }
            _ => panic!("expected auth start"),
        }

        assert_eq!(
            handle.send(TransferCmd::StartJob(Box::new(file_spec(
                TransferDest::File {
                    path: "out.json".into(),
                    format: FileFormat::Json,
                },
            )))),
            Ok(DeliveryReceipt::Enqueued)
        );
        match channels.commands.try_recv().unwrap().command {
            TransferCmd::StartJob(spec) => {
                assert!(matches!(spec.source, TransferSource::File { .. }));
                assert!(matches!(spec.dest, TransferDest::File { .. }));
            }
            _ => panic!("expected start job"),
        }

        assert_eq!(
            handle.send(TransferCmd::CancelJob),
            Ok(DeliveryReceipt::Enqueued)
        );
        assert_eq!(channels.cancel.try_recv(), Ok(()));
    }

    #[test]
    fn cancellation_has_reserved_capacity_when_work_inbox_is_full() {
        let (handle, channels) = actor_channel();
        let ActorChannels {
            commands,
            mut cancel,
            ..
        } = channels;
        let capacity = TRANSFER_QUEUE
            .capacity()
            .expect("transfer queue is bounded");

        for _ in 0..capacity {
            assert_eq!(
                handle.send(TransferCmd::Disconnect),
                Ok(DeliveryReceipt::Enqueued)
            );
        }
        assert_eq!(
            handle.send(TransferCmd::Disconnect),
            Err(DeliveryError::Busy)
        );

        assert_eq!(
            handle.send(TransferCmd::CancelJob),
            Ok(DeliveryReceipt::Enqueued)
        );
        assert_eq!(
            handle.send(TransferCmd::CancelJob),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        );
        assert_eq!(cancel.try_recv(), Ok(()));

        drop(commands);
        assert_eq!(
            handle.send(TransferCmd::Disconnect),
            Err(DeliveryError::Closed)
        );
        drop(cancel);
        assert_eq!(
            handle.send(TransferCmd::CancelJob),
            Err(DeliveryError::Closed)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_after_an_admitted_start_cannot_overtake_and_disappear() {
        let (handle, channels) = actor_channel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let emit: EventSink = Arc::new(move |event| {
            let _ = event_tx.send(event);
            Ok(DeliveryReceipt::Enqueued)
        });

        assert_eq!(
            handle.send(TransferCmd::StartJob(Box::new(file_spec(
                TransferDest::File {
                    path: "out.json".into(),
                    format: FileFormat::Json,
                },
            )))),
            Ok(DeliveryReceipt::Enqueued)
        );
        assert_eq!(
            handle.send(TransferCmd::CancelJob),
            Ok(DeliveryReceipt::Enqueued)
        );

        let actor = tokio::spawn(run_actor(channels, emit));
        let event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("queued cancellation is processed")
            .expect("actor emits cancellation");
        assert!(matches!(
            event,
            TransferEvent::JobFailed {
                error,
                resumable: true,
                ..
            } if error == "cancelled"
        ));

        drop(handle);
        actor.await.expect("actor exits after its senders close");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_event_retries_hard_saturation_without_losing_payload() {
        let attempts = Arc::new(AtomicU64::new(0));
        let sink_attempts = Arc::clone(&attempts);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let emit: EventSink = Arc::new(move |event| {
            if sink_attempts.fetch_add(1, Ordering::AcqRel) < 2 {
                return Err(DeliveryError::Saturated);
            }
            let _ = event_tx.send(event);
            Ok(DeliveryReceipt::Deferred)
        });
        let (_shutdown_tx, mut shutdown) = watch::channel(false);

        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                emit_reliably(
                    &emit,
                    TransferEvent::JobFailed {
                        job_id: "job-7".to_owned(),
                        error: "offline".to_owned(),
                        resumable: true,
                    },
                    &mut shutdown,
                ),
            )
            .await
            .expect("bounded saturation clears")
        );

        assert_eq!(attempts.load(Ordering::Acquire), 3);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TransferEvent::JobFailed {
                job_id,
                error,
                resumable: true,
            }) if job_id == "job-7" && error == "offline"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owner_closed_stops_reliable_delivery_without_a_background_retry() {
        let attempts = Arc::new(AtomicU64::new(0));
        let sink_attempts = Arc::clone(&attempts);
        let emit: EventSink = Arc::new(move |_| {
            sink_attempts.fetch_add(1, Ordering::AcqRel);
            Err(DeliveryError::Closed)
        });
        let (_shutdown_tx, mut shutdown) = watch::channel(false);

        assert!(
            !emit_reliably(
                &emit,
                TransferEvent::AuthError("owner closed".to_owned()),
                &mut shutdown,
            )
            .await
        );
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::Acquire), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owner_closed_stops_the_actor_after_one_terminal_attempt() {
        let (handle, channels) = actor_channel();
        let attempts = Arc::new(AtomicU64::new(0));
        let sink_attempts = Arc::clone(&attempts);
        let emit: EventSink = Arc::new(move |_| {
            sink_attempts.fetch_add(1, Ordering::AcqRel);
            Err(DeliveryError::Closed)
        });
        assert!(
            handle
                .send(TransferCmd::StartJob(Box::new(file_spec(
                    TransferDest::File {
                        path: "out.json".into(),
                        format: FileFormat::Json,
                    },
                ))))
                .is_ok()
        );
        assert!(handle.send(TransferCmd::CancelJob).is_ok());

        let actor = tokio::spawn(run_actor(channels, emit));
        tokio::time::timeout(Duration::from_millis(100), actor)
            .await
            .expect("closed owner stops the actor")
            .expect("actor cleanup does not panic");

        assert_eq!(attempts.load(Ordering::Acquire), 1);
        assert_eq!(
            handle.send(TransferCmd::Disconnect),
            Err(DeliveryError::Closed)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn auth_url_uses_the_actor_owned_bounded_handoff() {
        let (auth_url_tx, mut auth_url_rx) = watch::channel(None);
        auth_url_tx.send_replace(Some("https://accounts.example/authorize".to_owned()));

        assert_eq!(
            receive_auth_url(&mut auth_url_rx).await.as_deref(),
            Some("https://accounts.example/authorize")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_interrupts_a_saturated_reliable_delivery() {
        let attempts = Arc::new(AtomicU64::new(0));
        let sink_attempts = Arc::clone(&attempts);
        let emit: EventSink = Arc::new(move |_| {
            sink_attempts.fetch_add(1, Ordering::AcqRel);
            Err(DeliveryError::Saturated)
        });
        let (shutdown_tx, mut shutdown) = watch::channel(false);
        let delivery = tokio::spawn(async move {
            emit_reliably(
                &emit,
                TransferEvent::AuthError("waiting".to_owned()),
                &mut shutdown,
            )
            .await
        });

        tokio::task::yield_now().await;
        shutdown_tx.send_replace(true);

        assert!(
            !tokio::time::timeout(Duration::from_millis(100), delivery)
                .await
                .expect("shutdown interrupts the retry delay")
                .expect("delivery task does not panic")
        );
        let attempts_after_shutdown = attempts.load(Ordering::Acquire);
        tokio::task::yield_now().await;
        assert_eq!(attempts.load(Ordering::Acquire), attempts_after_shutdown);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn actor_cleanup_aborts_and_joins_auth_job_and_playlist_tasks() {
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let auth_guard = DropSignal(Arc::clone(&drops));
        let job_guard = DropSignal(Arc::clone(&drops));
        let list_guard = DropSignal(Arc::clone(&drops));
        let list_active = Arc::new(AtomicBool::new(true));
        let activity = ListActivityGuard(Arc::clone(&list_active));

        let mut auth_task = Some(tokio::spawn(async move {
            let _guard = auth_guard;
            std::future::pending::<TransferEvent>().await
        }));
        let mut job_task = Some(RunningJob {
            sequence: 1,
            job_id: "job-cleanup".to_owned(),
            task: tokio::spawn(async move {
                let _guard = job_guard;
                std::future::pending::<TransferEvent>().await
            }),
        });
        let mut list_tasks = JoinSet::new();
        list_tasks.spawn(async move {
            let _guard = list_guard;
            let _activity = activity;
            std::future::pending::<TransferEvent>().await
        });

        cleanup_actor_tasks(&mut auth_task, &mut job_task, &mut list_tasks).await;

        assert!(auth_task.is_none());
        assert!(job_task.is_none());
        assert!(list_tasks.is_empty());
        assert_eq!(drops.load(Ordering::Acquire), 3);
        assert!(!list_active.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn child_panics_map_to_one_terminal_response_per_task_kind() {
        let job_result = tokio::spawn(async {
            panic!("injected job panic");
        })
        .await;
        let auth_result = tokio::spawn(async {
            panic!("injected auth panic");
        })
        .await;
        let mut lists = JoinSet::new();
        lists.spawn(async {
            panic!("injected list panic");
        });
        let list_result = lists.join_next().await.expect("list task is registered");

        let terminals = [
            job_join_event("job-panic".to_owned(), job_result),
            auth_join_event(auth_result),
            list_join_event(list_result),
        ];
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let emit: EventSink = Arc::new(move |event| {
            let _ = event_tx.send(event);
            Ok(DeliveryReceipt::Enqueued)
        });
        let (_shutdown_tx, mut shutdown) = watch::channel(false);
        for terminal in terminals {
            assert!(emit_reliably(&emit, terminal, &mut shutdown).await);
        }
        let emitted = [
            event_rx.try_recv().expect("job panic terminal"),
            event_rx.try_recv().expect("auth panic response"),
            event_rx.try_recv().expect("playlist panic response"),
        ];
        assert!(
            event_rx.try_recv().is_err(),
            "each failed task emits exactly one response"
        );

        assert!(matches!(
            &emitted[0],
            TransferEvent::JobFailed {
                job_id,
                error,
                resumable: true,
            } if job_id == "job-panic" && error == "transfer task failed unexpectedly"
        ));
        assert!(matches!(
            &emitted[1],
            TransferEvent::AuthError(error)
                if error == "Spotify authorization task failed unexpectedly"
        ));
        assert!(matches!(
            &emitted[2],
            TransferEvent::SpotifyPlaylists(Err(error))
                if error == "Spotify playlist request failed unexpectedly"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_vs_complete_returns_exactly_one_terminal() {
        let completed = TransferEvent::JobDone(Arc::new(TransferReport {
            job_id: "job-complete".to_owned(),
            ..TransferReport::default()
        }));
        let completed_task = tokio::spawn(async move { completed });
        while !completed_task.is_finished() {
            tokio::task::yield_now().await;
        }
        let mut completed_job = Some(RunningJob {
            sequence: 7,
            job_id: "job-complete".to_owned(),
            task: completed_task,
        });

        let completion_won = cancel_running_job(&mut completed_job, 7)
            .await
            .expect("a matching cancellation resolves the owned task");
        assert!(matches!(
            &completion_won,
            TransferEvent::JobDone(report) if report.job_id == "job-complete"
        ));
        assert!(completed_job.is_none());

        let mut pending_job = Some(RunningJob {
            sequence: 8,
            job_id: "job-cancel".to_owned(),
            task: tokio::spawn(std::future::pending::<TransferEvent>()),
        });
        let cancellation_won = cancel_running_job(&mut pending_job, 8)
            .await
            .expect("a matching cancellation resolves the owned task");
        assert!(matches!(
            &cancellation_won,
            TransferEvent::JobFailed {
                job_id,
                error,
                resumable: true,
            } if job_id == "job-cancel" && error == "cancelled"
        ));
        assert!(pending_job.is_none());

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let emit: EventSink = Arc::new(move |event| {
            let _ = event_tx.send(event);
            Ok(DeliveryReceipt::Enqueued)
        });
        let (_shutdown_tx, mut shutdown) = watch::channel(false);
        assert!(emit_reliably(&emit, completion_won, &mut shutdown).await);
        assert!(emit_reliably(&emit, cancellation_won, &mut shutdown).await);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TransferEvent::JobDone(report)) if report.job_id == "job-complete"
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TransferEvent::JobFailed { job_id, error, .. })
                if job_id == "job-cancel" && error == "cancelled"
        ));
        assert!(
            event_rx.try_recv().is_err(),
            "each cancel/complete race emits one terminal"
        );
    }

    #[test]
    fn already_running_response_is_not_a_job_terminal() {
        assert!(matches!(
            already_running_event("rejected-job".to_owned()),
            TransferEvent::JobRejected { job_id, error }
                if job_id == "rejected-job" && error == "a transfer is already running"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transfer_handle_shutdown_waits_for_actor_cleanup() {
        let (mut handle, channels) = actor_channel();
        let emit: EventSink = Arc::new(|_| Ok(DeliveryReceipt::Enqueued));
        let actor = tokio::spawn(run_actor(channels, emit));

        assert!(
            tokio::time::timeout(Duration::from_millis(100), handle.shutdown())
                .await
                .expect("actor acknowledges shutdown")
        );
        actor.await.expect("actor exits cleanly");
        assert_eq!(
            handle.send(TransferCmd::CancelJob),
            Err(DeliveryError::Closed)
        );
    }

    #[test]
    fn repeated_playlist_refreshes_coalesce_until_the_active_request_finishes() {
        let (handle, mut channels) = actor_channel();

        assert_eq!(
            handle.send(TransferCmd::ListSpotifyPlaylists),
            Ok(DeliveryReceipt::Enqueued)
        );
        assert_eq!(
            handle.send(TransferCmd::ListSpotifyPlaylists),
            Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            })
        );
        assert!(matches!(
            channels.commands.try_recv().map(|request| request.command),
            Ok(TransferCmd::ListSpotifyPlaylists)
        ));
        assert!(
            channels.commands.try_recv().is_err(),
            "only one refresh is queued"
        );

        drop(ListActivityGuard(Arc::clone(&handle.list_active)));
        assert_eq!(
            handle.send(TransferCmd::ListSpotifyPlaylists),
            Ok(DeliveryReceipt::Enqueued)
        );
    }

    #[tokio::test]
    async fn build_ctx_avoids_clients_for_plain_file_export() {
        let spec = file_spec(TransferDest::File {
            path: "out.csv".into(),
            format: FileFormat::Csv,
        });
        let cfg = config_without_cookie();

        let ctx = build_ctx(&spec, &cfg).await.unwrap();

        assert!(ctx.ytm.is_none());
        assert!(ctx.spotify.is_none());
        assert_eq!(ctx.search_config, cfg.effective_search());
    }

    #[tokio::test]
    async fn build_ctx_uses_anonymous_ytm_for_local_playlist_matching_without_cookie() {
        let spec = file_spec(TransferDest::LocalPlaylist {
            name: Some("Imported".to_owned()),
        });
        let cfg = config_without_cookie();

        let ctx = build_ctx(&spec, &cfg).await.unwrap();

        assert!(matches!(
            ctx.ytm,
            Some(crate::api::ytmusic::YtMusicApi::Anonymous)
        ));
        assert!(ctx.spotify.is_none());
    }

    #[tokio::test]
    async fn build_ctx_requires_cookie_for_account_writes() {
        let spec = file_spec(TransferDest::YtmLikes);
        let cfg = config_without_cookie();

        let err = match build_ctx(&spec, &cfg).await {
            Ok(_) => panic!("account write without a cookie should fail"),
            Err(err) => err,
        };

        assert!(err.contains("YouTube Music cookie"));
    }
}
