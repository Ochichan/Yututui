//! The scrobble network actor: owns the monitor, the durable queue, one `reqwest`
//! client, per-service health/backoff, and the Last.fm auth flow. Fed by
//! [`super::ScrobbleHandle::observe`]; talks back only for auth results and rare
//! service-health notices (scrobbling itself is fire-and-forget from the app's view).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::Receiver;
#[cfg(test)]
use tokio::sync::oneshot;

use super::lastfm::{LastfmClient, SCROBBLE_BATCH_MAX, SessionPoll};
use super::listenbrainz::ListenBrainzClient;
use super::monitor::{ScrobbleAction, ScrobbleMonitor};
use super::queue::{QUEUE_CAP, QueueEntry, QueueFile, QueueFlushLock, compact};
use super::service::{ScrobbleError, ScrobbleService, ScrobbleTrack, ServiceKind};
use super::{
    ObservationBatch, PendingCommands, ScrobbleHandle, ScrobbleSettings, ShutdownRequest,
    take_terminal_retry, wait_until_pending_drained,
};
use crate::util::backpressure::{SCROBBLE_CONTROL_QUEUE, SCROBBLE_QUEUE, bounded_channel};
use crate::util::delivery::DeliveryError;

/// Debounce between a queue append and the flush that delivers it.
const FLUSH_DEBOUNCE: Duration = Duration::from_secs(2);
/// Poll cadence while entries remain pending (also the backoff floor).
const FLUSH_RETRY: Duration = Duration::from_secs(30);
/// First flush after spawn — delivers leftovers from the previous run.
const FLUSH_ON_START: Duration = Duration::from_secs(10);
/// Network-error backoff ladder (exponential, capped).
const BACKOFF_STEPS: [Duration; 3] = [
    Duration::from_secs(60),
    Duration::from_secs(300),
    Duration::from_secs(900),
];
/// Browser-approval polling: every 5s, give up after 5 minutes.
const AUTH_POLL: Duration = Duration::from_secs(5);
const AUTH_BUDGET: Duration = Duration::from_secs(300);
/// "Queue is stuck" notices are emitted at most this often.
const STALL_NOTICE_EVERY: Duration = Duration::from_secs(3600);
/// Fallback backoff when a `Retry-After` is so large that `Instant + Duration` would overflow.
/// A well-behaved server never asks for this long; a hostile/misconfigured one can't crash us.
const MAX_SANE_BACKOFF: Duration = Duration::from_secs(3600);
/// Leave room inside the run loop's outer 1.5s quit budget for scheduling and acknowledgement.
const SHUTDOWN_FLUSH_BUDGET: Duration = Duration::from_secs(1);

pub enum ScrobbleCmd {
    Observe(Box<ObservationBatch>),
    Reconfigure(Box<ScrobbleSettings>),
    AuthStart,
}

/// Events back to the app. No `Debug`: `AuthDone` carries the session key.
pub enum ScrobbleEvent {
    /// Open this URL for the user (and show it, in case the browser can't launch).
    AuthUrl(String),
    AuthDone {
        username: String,
        session_key: String,
    },
    AuthFailed(String),
    /// The stored session key / token was rejected. Latched: emitted once per
    /// invalidation, cleared by a `Reconfigure` with fresh credentials.
    SessionInvalid(ServiceKind),
    /// Scrobbles are piling up undelivered (emitted at most once an hour), or a durable
    /// append failed. `pending == 0` is the one-shot recovery signal after append storage
    /// becomes writable again; it does not claim that network delivery is complete.
    QueueStalled {
        pending: usize,
    },
    /// The offline queue exceeded its retention cap and oldest entries were dropped.
    /// `dropped` is the actor-lifetime cumulative total, so a latest-value event transport
    /// may coalesce repeated notifications without understating permanent data loss.
    QueueDropped {
        dropped: usize,
    },
}

type EventSink = Arc<dyn Fn(ScrobbleEvent) + Send + Sync>;

/// Spawn the actor; returns the run loop's handle. The default queue path is used —
/// tests drive [`Actor`] pieces directly instead.
pub fn spawn(
    mut settings: ScrobbleSettings,
    emit: impl Fn(ScrobbleEvent) + Send + Sync + 'static,
) -> ScrobbleHandle {
    let (tx, rx) = bounded_channel(SCROBBLE_QUEUE);
    let (shutdown_tx, shutdown_rx) = bounded_channel(SCROBBLE_CONTROL_QUEUE);
    let pending = Arc::new(PendingCommands::default());
    let queue = if crate::persist::persistence_access().is_read_only() {
        // Keep a live inert owner so shutdown remains coherent, but do not read, deliver, append,
        // compact, authenticate, or refresh any durable scrobble state in a secondary process.
        settings.lastfm_app = None;
        settings.lastfm = None;
        settings.listenbrainz = None;
        settings.local_files = false;
        None
    } else {
        QueueFile::default_path().map(QueueFile::at)
    };
    let actor_thread = match spawn_actor_thread(
        rx,
        shutdown_rx,
        Arc::clone(&pending),
        settings,
        Arc::new(emit),
        queue,
    ) {
        Ok(actor_thread) => Some(actor_thread),
        Err(error) => {
            tracing::error!(error = %error, "failed to spawn isolated scrobble actor thread");
            None
        }
    };
    ScrobbleHandle::with_pending(tx, shutdown_tx, pending, actor_thread)
}

/// Keep durable append/fsync and queue compaction off the application's Tokio workers. A stuck
/// filesystem cannot prevent the main runtime's signal timers from running. The returned join
/// handle stays owned by [`ScrobbleHandle`], so a diagnostic shutdown deadline never turns this
/// isolated worker into a detached process-exit race.
fn spawn_actor_thread(
    rx: Receiver<ScrobbleCmd>,
    shutdown_rx: Receiver<ShutdownRequest>,
    pending: Arc<PendingCommands>,
    settings: ScrobbleSettings,
    emit: EventSink,
    queue: Option<QueueFile>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("ytt-scrobble-actor".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(error = %error, "failed to build scrobble actor runtime");
                    return;
                }
            };
            runtime.block_on(run_actor(rx, shutdown_rx, pending, settings, emit, queue));
        })
}

/// Per-service delivery health: auth failures latch (no retry until reconfigured);
/// network trouble backs off exponentially; success resets everything.
#[derive(Default)]
struct Health {
    auth_latched: bool,
    backoff_until: Option<Instant>,
    backoff_step: usize,
    invalid_notified: bool,
}

impl Health {
    fn ready(&self, now: Instant) -> bool {
        !self.auth_latched && self.backoff_until.is_none_or(|until| now >= until)
    }

    fn note_success(&mut self) {
        self.backoff_until = None;
        self.backoff_step = 0;
    }

    /// Classify a delivery error; returns an event to emit (once) for auth latches.
    fn note_error(
        &mut self,
        kind: ServiceKind,
        err: &ScrobbleError,
        now: Instant,
    ) -> Option<ScrobbleEvent> {
        match err {
            ScrobbleError::Auth(_) => {
                self.auth_latched = true;
                if !self.invalid_notified {
                    self.invalid_notified = true;
                    return Some(ScrobbleEvent::SessionInvalid(kind));
                }
                None
            }
            ScrobbleError::RateLimited(after) => {
                // A hostile/misconfigured `Retry-After` can be enormous; `Instant + Duration`
                // panics on overflow, so add checked and fall back to a bounded max instead of
                // crashing the actor. The normal (representable) delay is used unchanged.
                let delay = after.unwrap_or(FLUSH_RETRY).max(Duration::from_secs(10));
                self.backoff_until = Some(
                    now.checked_add(delay)
                        .unwrap_or_else(|| now + MAX_SANE_BACKOFF),
                );
                None
            }
            ScrobbleError::Network(_) => {
                let step = BACKOFF_STEPS[self.backoff_step.min(BACKOFF_STEPS.len() - 1)];
                self.backoff_step = (self.backoff_step + 1).min(BACKOFF_STEPS.len() - 1);
                self.backoff_until = Some(now + step);
                None
            }
            // Content rejections are "delivered" — the flusher already treats them so.
            ScrobbleError::Invalid(_) => None,
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, PartialEq, Eq)]
struct CompactionAck {
    entry_id: String,
    service: ServiceKind,
}

struct PendingCompaction {
    acknowledgements: Vec<CompactionAck>,
    /// The acknowledgement and its queue ownership are one recovery unit. Keeping the kernel
    /// lock across rewrite uncertainty prevents another process from reloading and resubmitting
    /// the acknowledged entry before this actor establishes the durable replacement.
    lock: QueueFlushLock,
}

struct Actor {
    settings: ScrobbleSettings,
    emit: EventSink,
    monitor: ScrobbleMonitor,
    queue: Option<QueueFile>,
    http: reqwest::Client,
    lastfm: Option<LastfmClient>,
    lastfm_health: Health,
    listenbrainz: Option<ListenBrainzClient>,
    listenbrainz_health: Health,
    auth_task: Option<tokio::task::JoinHandle<()>>,
    next_flush: Option<Instant>,
    last_stall_notice: Option<Instant>,
    /// Threshold crossings whose durable append failed. The bound matches the on-disk retention
    /// cap; overflow follows the same explicit oldest-first loss policy as queue compaction.
    pending_appends: VecDeque<QueueEntry>,
    /// Queue ownership retained across an ambiguous append result. A retry uses `append_locked`
    /// with this exact guard, avoiding self-deadlock and excluding cross-process resubmission.
    pending_append_lock: Option<QueueFlushLock>,
    /// Network acknowledgements whose atomic queue rewrite did not reach a confirmed durability
    /// boundary. The guard stays attached until the replacement is durably confirmed.
    pending_compaction: Option<PendingCompaction>,
    /// Latches append-health notifications so a full disk produces one failure and one recovery
    /// event instead of a toast on every observation/retry.
    append_failure_active: bool,
    /// Actor-lifetime cumulative permanent loss. Events publish this total rather than a delta,
    /// which makes replacement/coalescing safe under event-channel pressure.
    dropped_total: usize,
    /// In-flight love/unlove sub-tasks keyed by `(artist, title)`. A newer toggle for the same
    /// track aborts the prior one (last-writer-wins) so rapid liking can't settle the wrong way.
    love_tasks: HashMap<(String, String), tokio::task::JoinHandle<()>>,
}

async fn run_actor(
    mut rx: Receiver<ScrobbleCmd>,
    mut shutdown_rx: Receiver<ShutdownRequest>,
    pending: Arc<PendingCommands>,
    settings: ScrobbleSettings,
    emit: EventSink,
    queue: Option<QueueFile>,
) {
    let http = reqwest::Client::builder()
        .user_agent(format!("yututui/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_default();
    let mut actor = Actor {
        lastfm: lastfm_client(&http, &settings),
        listenbrainz: listenbrainz_client(&http, &settings),
        settings,
        emit,
        monitor: ScrobbleMonitor::new(),
        queue,
        http,
        lastfm_health: Health::default(),
        listenbrainz_health: Health::default(),
        auth_task: None,
        next_flush: None,
        last_stall_notice: None,
        pending_appends: VecDeque::new(),
        pending_append_lock: None,
        pending_compaction: None,
        append_failure_active: false,
        dropped_total: 0,
        love_tasks: HashMap::new(),
    };
    // Leftovers from the previous run (crash, offline quit) get an early delivery try —
    // but only when something could receive them.
    if actor.settings.any_active() {
        actor.schedule_flush(FLUSH_ON_START);
    }

    loop {
        let deadline = actor.next_flush;
        tokio::select! {
            biased;
            shutdown = shutdown_rx.recv() => match shutdown {
                Some(done) => {
                    finish_shutdown(&mut actor, &mut rx, &pending, Some(done)).await;
                    break;
                }
                None => {
                    finish_shutdown(&mut actor, &mut rx, &pending, None).await;
                    break;
                }
            },
            cmd = rx.recv() => match cmd {
                None => break,
                Some(ScrobbleCmd::Observe(obs)) => {
                    // Commit durable actions before polling any ephemeral network work. If
                    // shutdown becomes ready below, this observation has already crossed the
                    // persistence boundary and only its NowPlaying request is cancelled.
                    let now_playing = actor.record_observation_batch(*obs);
                    let mut interrupted = None;
                    for track in now_playing {
                        interrupted = tokio::select! {
                            biased;
                            shutdown = shutdown_rx.recv() => Some(shutdown),
                            () = actor.send_now_playing(track) => None,
                        };
                        if interrupted.is_some() {
                            break;
                        }
                    }
                    match interrupted {
                        Some(Some(done)) => {
                            finish_shutdown(&mut actor, &mut rx, &pending, Some(done)).await;
                            break;
                        }
                        Some(None) => {
                            finish_shutdown(&mut actor, &mut rx, &pending, None).await;
                            break;
                        }
                        None => {}
                    }
                }
                Some(ScrobbleCmd::Reconfigure(settings)) => actor.reconfigure(*settings),
                Some(ScrobbleCmd::AuthStart) => actor.start_auth(),
            },
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now).into()),
                if deadline.is_some() => {
                    let interrupted = tokio::select! {
                        biased;
                        shutdown = shutdown_rx.recv() => Some(shutdown),
                        result = actor.flush() => {
                            if let Err(error) = result {
                                tracing::debug!(
                                    delivery_outcome = error.reason(),
                                    "scrobble queue flush remains pending"
                                );
                            }
                            None
                        },
                    };
                    match interrupted {
                        Some(Some(done)) => {
                            finish_shutdown(&mut actor, &mut rx, &pending, Some(done)).await;
                            break;
                        }
                        Some(None) => {
                            finish_shutdown(&mut actor, &mut rx, &pending, None).await;
                            break;
                        }
                        None => {}
                    }
                },
        }
    }
}

async fn finish_shutdown(
    actor: &mut Actor,
    rx: &mut Receiver<ScrobbleCmd>,
    pending: &PendingCommands,
    request: Option<ShutdownRequest>,
) {
    // Refuse new work and drain everything that crossed the bounded inbox boundary before the
    // final network flush. Durable file appends are synchronous and deliberately non-cancellable:
    // a slow fsync may extend the nominal budget, but accepted threshold observations are never
    // abandoned merely to make the timer fire.
    let shutdown_started = Instant::now();
    let done = request.map(|request| request.done);
    loop {
        tokio::select! {
            biased;
            result = wait_until_pending_drained(pending) => {
                if let Err(error) = result {
                    tracing::warn!(%error, "scrobble shutdown admission barrier failed");
                }
                break;
            }
            command = rx.recv() => match command {
                Some(command) => record_shutdown_command(actor, command),
                None => break,
            },
        }
    }
    rx.close();
    while let Some(cmd) = rx.recv().await {
        record_shutdown_command(actor, cmd);
    }
    if let Some(observation) = take_terminal_retry(pending) {
        // This snapshot was explicitly rejected with `Busy`, so it must not overtake admitted
        // FIFO work. Shutdown is its sole actor-side owner after sealing admission above: apply
        // the latest terminal tail only after the inbox is closed and fully drained.
        record_shutdown_command(actor, ScrobbleCmd::Observe(observation));
    }

    // Spend only the remainder of the internal quit budget. `flush` distinguishes queue-lock
    // contention from an I/O durability failure, and neither is acknowledged as a clean exit.
    let remaining = SHUTDOWN_FLUSH_BUDGET.saturating_sub(shutdown_started.elapsed());
    let mut outcome = if remaining.is_zero() {
        Err(DeliveryError::Busy)
    } else {
        match tokio::time::timeout(remaining, actor.flush()).await {
            Ok(result) => result,
            Err(_) => Err(DeliveryError::Busy),
        }
    };
    if outcome.is_ok() && actor.has_pending_durability() {
        outcome = Err(DeliveryError::Saturated);
    }

    let elapsed = shutdown_started.elapsed();
    if let Err(error) = &outcome {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis(),
            delivery_outcome = error.reason(),
            pending_appends = actor.pending_appends.len(),
            append_lock_held = actor.pending_append_lock.is_some(),
            compaction_lock_held = actor.pending_compaction.is_some(),
            "scrobble shutdown could not confirm all queue durability; in-memory appends may be lost on exit"
        );
    } else if elapsed > SHUTDOWN_FLUSH_BUDGET {
        tracing::warn!(
            elapsed_ms = elapsed.as_millis(),
            "scrobble shutdown confirmed durability after its internal budget"
        );
    }
    if let Some(done) = done {
        let _ = done.send(outcome);
    }
}

fn record_shutdown_command(actor: &mut Actor, command: ScrobbleCmd) {
    match command {
        ScrobbleCmd::Observe(obs) => {
            let _now_playing = actor.record_observation_batch(*obs);
        }
        ScrobbleCmd::Reconfigure(settings) => actor.reconfigure(*settings),
        ScrobbleCmd::AuthStart => {}
    }
}

fn lastfm_client(http: &reqwest::Client, settings: &ScrobbleSettings) -> Option<LastfmClient> {
    let app = settings.lastfm_app.as_ref()?;
    let session = settings.lastfm.as_ref()?;
    Some(LastfmClient::new(
        http.clone(),
        app.api_key.clone(),
        app.api_secret.clone(),
        Some(session.session_key.clone()),
    ))
}

fn listenbrainz_client(
    http: &reqwest::Client,
    settings: &ScrobbleSettings,
) -> Option<ListenBrainzClient> {
    let session = settings.listenbrainz.as_ref()?;
    Some(ListenBrainzClient::new(
        http.clone(),
        session.token.clone(),
        session.api_url.clone(),
    ))
}

fn emit_queue_drop(dropped_total: &mut usize, emit: &EventSink, dropped: usize) {
    *dropped_total = dropped_total.saturating_add(dropped);
    emit(ScrobbleEvent::QueueDropped {
        dropped: *dropped_total,
    });
}

impl Actor {
    fn record_observation_batch(&mut self, batch: ObservationBatch) -> Vec<ScrobbleTrack> {
        let (first, tail) = batch.into_parts();
        let mut actions = self.monitor.observe(&first, self.settings.local_files);
        if let Some((latest, preserved_credit)) = tail {
            actions.extend(self.monitor.observe_with_preserved_credit(
                &latest,
                self.settings.local_files,
                preserved_credit,
            ));
        }
        self.record_durable_actions(actions)
    }

    fn record_durable_actions(&mut self, actions: Vec<ScrobbleAction>) -> Vec<ScrobbleTrack> {
        let mut now_playing = Vec::new();
        for action in actions {
            match action {
                // Persist threshold-crossing scrobbles before awaiting any ephemeral
                // now-playing request. Shutdown is allowed to cancel those network
                // awaits, but it must never cancel the durable queue append that was
                // produced by the same observation.
                ScrobbleAction::NowPlaying(track) => now_playing.push(track),
                ScrobbleAction::Scrobble(track) => self.enqueue_scrobble(track),
                ScrobbleAction::Love {
                    artist,
                    title,
                    love,
                } => self.send_love(artist, title, love),
            }
        }
        now_playing
    }

    /// Now-playing is ephemeral: one attempt per service, never queued.
    async fn send_now_playing(&mut self, track: ScrobbleTrack) {
        let now = Instant::now();
        if let Some(client) = &self.lastfm
            && self.lastfm_health.ready(now)
            && let Err(e) = client.now_playing(&track).await
        {
            tracing::debug!(error = %e, "last.fm now-playing failed");
            if let Some(event) = self.lastfm_health.note_error(ServiceKind::Lastfm, &e, now) {
                (self.emit)(event);
            }
        }
        if let Some(client) = &self.listenbrainz
            && self.listenbrainz_health.ready(now)
            && let Err(e) = client.now_playing(&track).await
        {
            tracing::debug!(error = %e, "listenbrainz playing-now failed");
            if let Some(event) =
                self.listenbrainz_health
                    .note_error(ServiceKind::ListenBrainz, &e, now)
            {
                (self.emit)(event);
            }
        }
    }

    /// Durability first: the entry hits the JSONL queue before any network attempt. A failed
    /// append retains this exact entry (and therefore its dedupe id) for bounded retry.
    fn enqueue_scrobble(&mut self, mut track: ScrobbleTrack) {
        let mut pending = Vec::new();
        if self.settings.lastfm.is_some() {
            pending.push(ServiceKind::Lastfm);
        }
        if self.settings.listenbrainz.is_some() {
            pending.push(ServiceKind::ListenBrainz);
        }
        let Some(_queue) = &self.queue else { return };
        if pending.is_empty() {
            return;
        }
        // Clock-skew guard: Last.fm rejects future timestamps.
        let now_unix = crate::signals::unix_now();
        if track.started_unix > now_unix + 60 {
            track.started_unix = now_unix - 1;
        }
        let entry = QueueEntry::from_track(&track, pending);
        if self.pending_appends.len() == QUEUE_CAP {
            self.pending_appends.pop_front();
            tracing::warn!(
                capacity = QUEUE_CAP,
                "pending scrobble append queue full; dropped oldest entry"
            );
            emit_queue_drop(&mut self.dropped_total, &self.emit, 1);
        }
        self.pending_appends.push_back(entry);
        let _ = self.retry_pending_appends();
    }

    /// Retry retained appends in listen order. An append error can be ambiguous (the line may
    /// have reached the file before a later sync failed), so the same `QueueEntry::id` is reused;
    /// `QueueFile::load` deduplicates that id and exposes the listen exactly once. Queue ownership
    /// stays held from the first uncertain result through its successful retry so another process
    /// cannot observe, submit, and remove that id between attempts.
    fn retry_pending_appends(&mut self) -> Result<(), DeliveryError> {
        let mut appended = 0usize;
        while let Some(entry) = self.pending_appends.front().cloned() {
            if self.pending_append_lock.is_none() {
                let lock = match self
                    .queue
                    .as_ref()
                    .expect("pending appends require a durable queue")
                    .try_lock_result()
                {
                    Ok(Some(lock)) => lock,
                    Ok(None) => {
                        self.note_append_failure(None);
                        return Err(DeliveryError::Busy);
                    }
                    Err(error) => {
                        self.note_append_failure(Some(&error));
                        return Err(DeliveryError::Saturated);
                    }
                };
                self.pending_append_lock = Some(lock);
            }
            let result = self
                .queue
                .as_ref()
                .expect("pending appends require a durable queue")
                .append_locked(
                    &entry,
                    self.pending_append_lock
                        .as_ref()
                        .expect("append retry owns queue lock"),
                );
            match result {
                Ok(()) => {
                    self.pending_appends.pop_front();
                    // A confirmed append ends this ownership interval. A later pending entry
                    // obtains a fresh guard so other processes are not excluded unnecessarily.
                    self.pending_append_lock = None;
                    appended += 1;
                }
                Err(error) => {
                    self.note_append_failure(Some(&error));
                    return Err(DeliveryError::Saturated);
                }
            }
        }

        self.pending_append_lock = None;
        if appended > 0 {
            self.schedule_flush(FLUSH_DEBOUNCE);
        }
        if self.append_failure_active {
            self.append_failure_active = false;
            (self.emit)(ScrobbleEvent::QueueStalled { pending: 0 });
        }
        Ok(())
    }

    fn note_append_failure(&mut self, error: Option<&std::io::Error>) {
        if let Some(error) = error {
            tracing::warn!(
                error = %error,
                pending = self.pending_appends.len(),
                lock_held = self.pending_append_lock.is_some(),
                "failed to append scrobble queue entry; retained for retry"
            );
        } else {
            tracing::debug!(
                pending = self.pending_appends.len(),
                "scrobble append waits for queue ownership"
            );
        }
        if !self.append_failure_active {
            self.append_failure_active = true;
            (self.emit)(ScrobbleEvent::QueueStalled {
                pending: self.pending_appends.len(),
            });
        }
        self.schedule_flush(FLUSH_RETRY);
    }

    /// Love/unlove is ephemeral like now-playing, but worth a couple of retries; runs in
    /// a sub-task so a slow call never delays queue flushes or observations.
    fn send_love(&mut self, artist: String, title: String, love: bool) {
        let Some(session) = &self.settings.lastfm else {
            return;
        };
        if !session.love_sync {
            return;
        }
        let Some(client) = self.lastfm.clone() else {
            return;
        };
        // Last-writer-wins: abort any in-flight love for this same track so a slow older toggle
        // can't land after a newer one. Prune finished handles first so the map can't grow
        // without bound across a long session.
        self.love_tasks.retain(|_, h| !h.is_finished());
        let key = (artist.clone(), title.clone());
        if let Some(prev) = self.love_tasks.remove(&key) {
            prev.abort();
        }
        let handle = tokio::spawn(async move {
            for attempt in 0..3u32 {
                match client.love(&artist, &title, love).await {
                    Ok(()) => return,
                    Err(ScrobbleError::Network(_) | ScrobbleError::RateLimited(_))
                        if attempt < 2 =>
                    {
                        tokio::time::sleep(Duration::from_secs(5 << attempt)).await;
                    }
                    Err(e) => {
                        tracing::info!(error = %e, love, "last.fm love sync failed");
                        return;
                    }
                }
            }
        });
        self.love_tasks.insert(key, handle);
    }

    fn reconfigure(&mut self, settings: ScrobbleSettings) {
        self.settings = settings;
        self.lastfm = lastfm_client(&self.http, &self.settings);
        self.listenbrainz = listenbrainz_client(&self.http, &self.settings);
        // Fresh credentials mean a fresh start: clear latches and retry soon.
        self.lastfm_health.reset();
        self.listenbrainz_health.reset();
        if self.settings.any_active() || !self.pending_appends.is_empty() {
            self.schedule_flush(FLUSH_DEBOUNCE);
        }
    }

    fn start_auth(&mut self) {
        if self.auth_task.as_ref().is_some_and(|t| !t.is_finished()) {
            return; // one flow at a time; the browser tab is already open
        }
        let Some(app) = self.settings.lastfm_app.clone() else {
            (self.emit)(ScrobbleEvent::AuthFailed(
                "app credentials missing (scrobble.lastfm.api_key/api_secret)".to_owned(),
            ));
            return;
        };
        let client = LastfmClient::new(self.http.clone(), app.api_key, app.api_secret, None);
        let emit = Arc::clone(&self.emit);
        self.auth_task = Some(tokio::spawn(async move {
            let token = match client.get_token().await {
                Ok(t) => t,
                Err(e) => {
                    emit(ScrobbleEvent::AuthFailed(e.to_string()));
                    return;
                }
            };
            emit(ScrobbleEvent::AuthUrl(client.auth_url(&token)));
            let deadline = Instant::now() + AUTH_BUDGET;
            loop {
                tokio::time::sleep(AUTH_POLL).await;
                if Instant::now() >= deadline {
                    emit(ScrobbleEvent::AuthFailed(
                        "authorization timed out".to_owned(),
                    ));
                    return;
                }
                match client.get_session(&token).await {
                    Ok(SessionPoll::Pending) => {}
                    Ok(SessionPoll::Granted { key, username }) => {
                        emit(ScrobbleEvent::AuthDone {
                            username,
                            session_key: key,
                        });
                        return;
                    }
                    // Transient trouble mid-poll: keep waiting out the budget.
                    Err(ScrobbleError::Network(_) | ScrobbleError::RateLimited(_)) => {}
                    Err(e) => {
                        emit(ScrobbleEvent::AuthFailed(e.to_string()));
                        return;
                    }
                }
            }
        }));
    }

    fn schedule_flush(&mut self, after: Duration) {
        let at = Instant::now() + after;
        self.next_flush = Some(self.next_flush.map_or(at, |cur| cur.min(at)));
    }

    fn has_pending_durability(&self) -> bool {
        !self.pending_appends.is_empty()
            || self.pending_append_lock.is_some()
            || self.pending_compaction.is_some()
    }

    /// Drain the queue: compact, deliver per healthy service in ≤50 chunks (compacting
    /// after every successful chunk), and reschedule while anything stays pending.
    async fn flush(&mut self) -> Result<(), DeliveryError> {
        self.next_flush = None;
        if self.pending_compaction.is_some() {
            let retry = {
                let queue = self
                    .queue
                    .as_ref()
                    .expect("pending compaction requires a durable queue");
                let pending = self
                    .pending_compaction
                    .as_mut()
                    .expect("checked pending compaction");
                retry_pending_compaction(queue, &pending.lock, &mut pending.acknowledgements)
            };
            if let Err(error) = retry {
                tracing::warn!(error = %error, "failed to retry acknowledged scrobble compaction");
                self.schedule_flush(FLUSH_RETRY);
                return Err(DeliveryError::Saturated);
            }
            // Release the recovery guard as soon as the acknowledged replacement is durable.
            self.pending_compaction = None;
        }

        // A retained compaction owns the same kernel lock that new appends need. Recover and
        // release it first; otherwise a newly queued listen can make this actor contend with its
        // own guard forever.
        self.retry_pending_appends()?;
        debug_assert!(self.pending_appends.is_empty());
        debug_assert!(self.pending_append_lock.is_none());
        let Some(queue) = &self.queue else {
            return if self.has_pending_durability() {
                Err(DeliveryError::Saturated)
            } else {
                Ok(())
            };
        };

        let lock = match queue.try_lock_result() {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                self.schedule_flush(FLUSH_RETRY);
                return Err(DeliveryError::Busy);
            }
            Err(error) => {
                tracing::warn!(error = %error, "failed to acquire scrobble queue lock");
                self.schedule_flush(FLUSH_RETRY);
                return Err(DeliveryError::Saturated);
            }
        };
        let loaded = queue.load();
        if loaded.read_failed {
            // Present but unreadable: skip the whole round so we never compact-to-empty and
            // delete a queue we simply couldn't read. Retried on the next natural flush.
            self.schedule_flush(FLUSH_RETRY);
            return Err(DeliveryError::Saturated);
        }
        if loaded.corrupt > 0 {
            tracing::warn!(corrupt = loaded.corrupt, "scrobble queue had corrupt lines");
        }
        let (mut entries, capped) = compact(loaded.entries, crate::signals::unix_now());
        if capped > 0 {
            tracing::warn!(dropped = capped, "scrobble queue over cap; dropped oldest");
            emit_queue_drop(&mut self.dropped_total, &self.emit, capped);
        }

        if let Some(client) = self.lastfm.clone()
            && self.lastfm_health.ready(Instant::now())
            && let Some(pending) = flush_service(
                &client,
                &mut entries,
                queue,
                &lock,
                &mut self.lastfm_health,
                &self.emit,
            )
            .await
        {
            self.pending_compaction = Some(PendingCompaction {
                acknowledgements: pending,
                lock,
            });
            self.schedule_flush(FLUSH_RETRY);
            return Err(DeliveryError::Saturated);
        }
        if let Some(client) = self.listenbrainz.clone()
            && self.listenbrainz_health.ready(Instant::now())
            && let Some(pending) = flush_service(
                &client,
                &mut entries,
                queue,
                &lock,
                &mut self.listenbrainz_health,
                &self.emit,
            )
            .await
        {
            self.pending_compaction = Some(PendingCompaction {
                acknowledgements: pending,
                lock,
            });
            self.schedule_flush(FLUSH_RETRY);
            return Err(DeliveryError::Saturated);
        }

        if let Err(e) = queue.rewrite_locked(&entries, &lock) {
            tracing::warn!(error = %e, "failed to rewrite scrobble queue");
            self.schedule_flush(FLUSH_RETRY);
            return Err(DeliveryError::Saturated);
        }
        let pending = entries.len();
        if pending > 0 && self.settings.any_active() {
            self.schedule_flush(FLUSH_RETRY);
            let notice_due = self
                .last_stall_notice
                .is_none_or(|t| t.elapsed() >= STALL_NOTICE_EVERY);
            // Only nag when nothing can move (auth latch), not while a routine retry is
            // seconds away from clearing the queue.
            let all_latched = (self.lastfm.is_none() || self.lastfm_health.auth_latched)
                && (self.listenbrainz.is_none() || self.listenbrainz_health.auth_latched);
            if notice_due && all_latched {
                self.last_stall_notice = Some(Instant::now());
                (self.emit)(ScrobbleEvent::QueueStalled { pending });
            }
        }
        Ok(())
    }
}

/// Deliver everything `kind` still owes, oldest first, ≤50 per call; strip the marker
/// (and rewrite the file) after each successful chunk so a crash re-sends at most one
/// chunk. Content rejections count as delivered.
async fn flush_service<S: ScrobbleService>(
    service: &S,
    entries: &mut Vec<QueueEntry>,
    queue: &QueueFile,
    lock: &QueueFlushLock,
    health: &mut Health,
    emit: &EventSink,
) -> Option<Vec<CompactionAck>> {
    let kind = service.kind();
    loop {
        let mut chunk_ids = Vec::new();
        let mut tracks = Vec::new();
        for e in entries.iter() {
            if e.pending.contains(&kind) {
                chunk_ids.push(e.id.clone());
                tracks.push(e.to_track());
                if tracks.len() == SCROBBLE_BATCH_MAX {
                    break;
                }
            }
        }
        if tracks.is_empty() {
            health.note_success();
            return None;
        }
        let delivered = match service.scrobble_batch(&tracks).await {
            Ok(()) => true,
            Err(ScrobbleError::Invalid(msg)) => {
                tracing::info!(error = %msg, service = kind.label(), "scrobbles rejected as invalid; dropping");
                true
            }
            Err(e) => {
                tracing::info!(error = %e, service = kind.label(), "scrobble delivery failed; will retry");
                if let Some(event) = health.note_error(kind, &e, Instant::now()) {
                    emit(event);
                }
                false
            }
        };
        if !delivered {
            return None;
        }
        let acknowledgements: Vec<CompactionAck> = chunk_ids
            .iter()
            .map(|entry_id| CompactionAck {
                entry_id: entry_id.clone(),
                service: kind,
            })
            .collect();
        for e in entries.iter_mut() {
            if chunk_ids.contains(&e.id) {
                e.pending.retain(|s| *s != kind);
            }
        }
        entries.retain(|e| !e.pending.is_empty());
        if let Err(e) = queue.rewrite_locked(entries, lock) {
            tracing::warn!(error = %e, "failed to compact scrobble queue after chunk");
            return Some(acknowledgements);
        }
    }
}

fn apply_compaction_acks(entries: &mut Vec<QueueEntry>, acknowledgements: &[CompactionAck]) {
    for entry in entries.iter_mut() {
        for acknowledgement in acknowledgements {
            if entry.id == acknowledgement.entry_id {
                entry
                    .pending
                    .retain(|service| *service != acknowledgement.service);
            }
        }
    }
    entries.retain(|entry| !entry.pending.is_empty());
}

fn retry_pending_compaction(
    queue: &QueueFile,
    lock: &QueueFlushLock,
    acknowledgements: &mut Vec<CompactionAck>,
) -> std::io::Result<()> {
    let loaded = queue.load();
    if loaded.read_failed {
        return Err(std::io::Error::other(
            "scrobble queue unreadable during acknowledged compaction retry",
        ));
    }
    let mut entries = loaded.entries;
    apply_compaction_acks(&mut entries, acknowledgements);
    queue.rewrite_locked(&entries, lock)?;
    acknowledgements.clear();
    Ok(())
}

#[cfg(test)]
mod tests;
