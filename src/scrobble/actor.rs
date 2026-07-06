//! The scrobble network actor: owns the monitor, the durable queue, one `reqwest`
//! client, per-service health/backoff, and the Last.fm auth flow. Fed by
//! [`super::ScrobbleHandle::observe`]; talks back only for auth results and rare
//! service-health notices (scrobbling itself is fire-and-forget from the app's view).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, Receiver};
use tokio::sync::oneshot;

use super::lastfm::{LastfmClient, SCROBBLE_BATCH_MAX, SessionPoll};
use super::listenbrainz::ListenBrainzClient;
use super::monitor::{Observation, ScrobbleAction, ScrobbleMonitor};
use super::queue::{QueueEntry, QueueFile, compact};
use super::service::{ScrobbleError, ScrobbleService, ScrobbleTrack, ServiceKind};
use super::{ScrobbleHandle, ScrobbleSettings};

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

pub enum ScrobbleCmd {
    Observe(Box<Observation>),
    Reconfigure(Box<ScrobbleSettings>),
    AuthStart,
    Shutdown { done: oneshot::Sender<()> },
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
    /// Scrobbles are piling up undelivered (emitted at most once an hour).
    QueueStalled {
        pending: usize,
    },
}

type EventSink = Arc<dyn Fn(ScrobbleEvent) + Send + Sync>;

/// Spawn the actor; returns the run loop's handle. The default queue path is used —
/// tests drive [`Actor`] pieces directly instead.
pub fn spawn(
    settings: ScrobbleSettings,
    emit: impl Fn(ScrobbleEvent) + Send + Sync + 'static,
) -> ScrobbleHandle {
    // Bounded with a generous cap; ~1 Hz Observe messages never fill it in normal use, but a
    // long network stall drops new messages (`try_send` at the send sites) rather than letting
    // the inbox grow without bound.
    let (tx, rx) = mpsc::channel(512);
    let queue = QueueFile::default_path().map(QueueFile::at);
    tokio::spawn(run_actor(rx, settings, Arc::new(emit), queue));
    ScrobbleHandle::new(tx)
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
}

async fn run_actor(
    mut rx: Receiver<ScrobbleCmd>,
    settings: ScrobbleSettings,
    emit: EventSink,
    queue: Option<QueueFile>,
) {
    let http = reqwest::Client::builder()
        .user_agent(format!("ytm-tui/{}", env!("CARGO_PKG_VERSION")))
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
    };
    // Leftovers from the previous run (crash, offline quit) get an early delivery try —
    // but only when something could receive them.
    if actor.settings.any_active() {
        actor.schedule_flush(FLUSH_ON_START);
    }

    loop {
        let deadline = actor.next_flush;
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break,
                Some(ScrobbleCmd::Observe(obs)) => actor.handle_observe(*obs).await,
                Some(ScrobbleCmd::Reconfigure(settings)) => actor.reconfigure(*settings),
                Some(ScrobbleCmd::AuthStart) => actor.start_auth(),
                Some(ScrobbleCmd::Shutdown { done }) => {
                    // Best effort within the caller's timeout; sending the ack is the
                    // caller's cue either way.
                    actor.flush().await;
                    let _ = done.send(());
                    break;
                }
            },
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now).into()),
                if deadline.is_some() => actor.flush().await,
        }
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

impl Actor {
    async fn handle_observe(&mut self, obs: Observation) {
        for action in self.monitor.observe(&obs, self.settings.local_files) {
            match action {
                ScrobbleAction::NowPlaying(track) => self.send_now_playing(track).await,
                ScrobbleAction::Scrobble(track) => self.enqueue_scrobble(track),
                ScrobbleAction::Love {
                    artist,
                    title,
                    love,
                } => self.send_love(artist, title, love),
            }
        }
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

    /// Durability first: the entry hits the JSONL queue before any network attempt.
    fn enqueue_scrobble(&mut self, mut track: ScrobbleTrack) {
        let mut pending = Vec::new();
        if self.settings.lastfm.is_some() {
            pending.push(ServiceKind::Lastfm);
        }
        if self.settings.listenbrainz.is_some() {
            pending.push(ServiceKind::ListenBrainz);
        }
        let Some(queue) = &self.queue else { return };
        if pending.is_empty() {
            return;
        }
        // Clock-skew guard: Last.fm rejects future timestamps.
        let now_unix = crate::signals::unix_now();
        if track.started_unix > now_unix + 60 {
            track.started_unix = now_unix - 1;
        }
        let entry = QueueEntry::from_track(&track, pending);
        if let Err(e) = queue.append(&entry) {
            tracing::warn!(error = %e, "failed to append scrobble queue entry");
            return;
        }
        self.schedule_flush(FLUSH_DEBOUNCE);
    }

    /// Love/unlove is ephemeral like now-playing, but worth a couple of retries; runs in
    /// a sub-task so a slow call never delays queue flushes or observations.
    fn send_love(&self, artist: String, title: String, love: bool) {
        let Some(session) = &self.settings.lastfm else {
            return;
        };
        if !session.love_sync {
            return;
        }
        let Some(client) = self.lastfm.clone() else {
            return;
        };
        tokio::spawn(async move {
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
    }

    fn reconfigure(&mut self, settings: ScrobbleSettings) {
        self.settings = settings;
        self.lastfm = lastfm_client(&self.http, &self.settings);
        self.listenbrainz = listenbrainz_client(&self.http, &self.settings);
        // Fresh credentials mean a fresh start: clear latches and retry soon.
        self.lastfm_health.reset();
        self.listenbrainz_health.reset();
        if self.settings.any_active() {
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

    /// Drain the queue: compact, deliver per healthy service in ≤50 chunks (compacting
    /// after every successful chunk), and reschedule while anything stays pending.
    async fn flush(&mut self) {
        self.next_flush = None;
        let Some(queue) = &self.queue else { return };
        let Some(_lock) = queue.try_lock() else {
            // Another process is flushing; check back later.
            self.schedule_flush(FLUSH_RETRY);
            return;
        };
        let loaded = queue.load();
        if loaded.corrupt > 0 {
            tracing::warn!(corrupt = loaded.corrupt, "scrobble queue had corrupt lines");
        }
        let (mut entries, capped) = compact(loaded.entries, crate::signals::unix_now());
        if capped > 0 {
            tracing::warn!(dropped = capped, "scrobble queue over cap; dropped oldest");
        }

        if let Some(client) = self.lastfm.clone()
            && self.lastfm_health.ready(Instant::now())
        {
            flush_service(
                &client,
                &mut entries,
                queue,
                &mut self.lastfm_health,
                &self.emit,
            )
            .await;
        }
        if let Some(client) = self.listenbrainz.clone()
            && self.listenbrainz_health.ready(Instant::now())
        {
            flush_service(
                &client,
                &mut entries,
                queue,
                &mut self.listenbrainz_health,
                &self.emit,
            )
            .await;
        }

        if let Err(e) = queue.rewrite(&entries) {
            tracing::warn!(error = %e, "failed to rewrite scrobble queue");
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
    }
}

/// Deliver everything `kind` still owes, oldest first, ≤50 per call; strip the marker
/// (and rewrite the file) after each successful chunk so a crash re-sends at most one
/// chunk. Content rejections count as delivered.
async fn flush_service<S: ScrobbleService>(
    service: &S,
    entries: &mut Vec<QueueEntry>,
    queue: &QueueFile,
    health: &mut Health,
    emit: &EventSink,
) {
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
            return;
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
            return;
        }
        for e in entries.iter_mut() {
            if chunk_ids.contains(&e.id) {
                e.pending.retain(|s| *s != kind);
            }
        }
        entries.retain(|e| !e.pending.is_empty());
        if let Err(e) = queue.rewrite(entries) {
            tracing::warn!(error = %e, "failed to compact scrobble queue after chunk");
        }
    }
}
