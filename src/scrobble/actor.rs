//! The scrobble network actor: owns the monitor, the durable queue, one `reqwest`
//! client, per-service health/backoff, and the Last.fm auth flow. Fed by
//! [`super::ScrobbleHandle::observe`]; talks back only for auth results and rare
//! service-health notices (scrobbling itself is fire-and-forget from the app's view).

use std::collections::HashMap;
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
    /// The offline queue exceeded its retention cap and oldest entries were dropped.
    QueueDropped {
        dropped: usize,
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
    /// In-flight love/unlove sub-tasks keyed by `(artist, title)`. A newer toggle for the same
    /// track aborts the prior one (last-writer-wins) so rapid liking can't settle the wrong way.
    love_tasks: HashMap<(String, String), tokio::task::JoinHandle<()>>,
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
        if loaded.read_failed {
            // Present but unreadable: skip the whole round so we never compact-to-empty and
            // delete a queue we simply couldn't read. Retried on the next natural flush.
            return;
        }
        if loaded.corrupt > 0 {
            tracing::warn!(corrupt = loaded.corrupt, "scrobble queue had corrupt lines");
        }
        let (mut entries, capped) = compact(loaded.entries, crate::signals::unix_now());
        if capped > 0 {
            tracing::warn!(dropped = capped, "scrobble queue over cap; dropped oldest");
            (self.emit)(ScrobbleEvent::QueueDropped { dropped: capped });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use super::super::{LastfmApp, LastfmSession, ListenBrainzSession};

    struct FakeService {
        kind: ServiceKind,
        results: Mutex<Vec<Result<(), ScrobbleError>>>,
        batches: Mutex<Vec<Vec<ScrobbleTrack>>>,
    }

    impl FakeService {
        fn new(kind: ServiceKind, results: Vec<Result<(), ScrobbleError>>) -> Self {
            Self {
                kind,
                results: Mutex::new(results),
                batches: Mutex::new(Vec::new()),
            }
        }

        fn batch_sizes(&self) -> Vec<usize> {
            self.batches.lock().unwrap().iter().map(Vec::len).collect()
        }
    }

    impl ScrobbleService for FakeService {
        fn kind(&self) -> ServiceKind {
            self.kind
        }

        async fn now_playing(&self, _track: &ScrobbleTrack) -> Result<(), ScrobbleError> {
            Ok(())
        }

        async fn scrobble_batch(&self, tracks: &[ScrobbleTrack]) -> Result<(), ScrobbleError> {
            self.batches.lock().unwrap().push(tracks.to_vec());
            self.results.lock().unwrap().pop().unwrap_or(Ok(()))
        }

        async fn love(
            &self,
            _artist: &str,
            _title: &str,
            _love: bool,
        ) -> Result<(), ScrobbleError> {
            Ok(())
        }
    }

    fn temp_queue(name: &str) -> (std::path::PathBuf, QueueFile) {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let dir = std::env::temp_dir().join(format!(
            "ytm-tui-sactor-{name}-{}-{suffix}",
            std::process::id()
        ));
        let queue = QueueFile::at(dir.join("scrobble-queue.jsonl"));
        (dir, queue)
    }

    fn entry(idx: usize, pending: Vec<ServiceKind>) -> QueueEntry {
        QueueEntry {
            id: format!("1000-track-{idx}"),
            track_key: format!("track-{idx}"),
            ts: 1000 + idx as i64,
            artist: format!("artist-{idx}"),
            title: format!("title-{idx}"),
            album: Some(format!("album-{idx}")),
            duration: Some(180 + idx as u32),
            origin_url: Some(format!("https://music.youtube.com/watch?v=track-{idx}")),
            pending,
        }
    }

    fn track(started_unix: i64) -> ScrobbleTrack {
        ScrobbleTrack {
            key: "track-key".to_owned(),
            artist: "artist".to_owned(),
            title: "title".to_owned(),
            album: Some("album".to_owned()),
            duration_secs: Some(240),
            origin_url: Some("https://music.youtube.com/watch?v=track-key".to_owned()),
            started_unix,
        }
    }

    fn inactive_settings() -> ScrobbleSettings {
        ScrobbleSettings {
            lastfm_app: None,
            lastfm: None,
            listenbrainz: None,
            local_files: false,
        }
    }

    fn active_settings() -> ScrobbleSettings {
        ScrobbleSettings {
            lastfm_app: Some(LastfmApp {
                api_key: "api-key".to_owned(),
                api_secret: "api-secret".to_owned(),
            }),
            lastfm: Some(LastfmSession {
                session_key: "session-key".to_owned(),
                love_sync: true,
            }),
            listenbrainz: Some(ListenBrainzSession {
                token: "listen-token".to_owned(),
                api_url: "https://listen.example.test".to_owned(),
            }),
            local_files: true,
        }
    }

    fn test_actor(settings: ScrobbleSettings, queue: Option<QueueFile>, emit: EventSink) -> Actor {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap_or_default();
        Actor {
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
            love_tasks: HashMap::new(),
        }
    }

    fn captured_events() -> (Arc<Mutex<Vec<ScrobbleEvent>>>, EventSink) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink_events = Arc::clone(&events);
        let sink: EventSink = Arc::new(move |event| {
            sink_events.lock().unwrap().push(event);
        });
        (events, sink)
    }

    #[test]
    fn health_auth_latches_once_and_backoff_resets() {
        let now = Instant::now();
        let mut health = Health::default();

        assert!(health.ready(now));
        let first = health.note_error(
            ServiceKind::Lastfm,
            &ScrobbleError::Auth("bad key".to_owned()),
            now,
        );
        assert!(matches!(
            first,
            Some(ScrobbleEvent::SessionInvalid(ServiceKind::Lastfm))
        ));
        assert!(health.auth_latched);
        assert!(!health.ready(now));
        assert!(
            health
                .note_error(
                    ServiceKind::Lastfm,
                    &ScrobbleError::Auth("still bad".to_owned()),
                    now
                )
                .is_none()
        );

        health.reset();
        assert!(health.ready(now));
        health.note_error(
            ServiceKind::ListenBrainz,
            &ScrobbleError::Network("timeout".to_owned()),
            now,
        );
        assert_eq!(health.backoff_step, 1);
        assert!(!health.ready(now));
        assert!(health.ready(now + BACKOFF_STEPS[0] + Duration::from_millis(1)));

        health.note_error(
            ServiceKind::ListenBrainz,
            &ScrobbleError::RateLimited(Some(Duration::MAX)),
            now,
        );
        assert_eq!(health.backoff_until, Some(now + MAX_SANE_BACKOFF));

        health.note_success();
        assert!(health.ready(now));
        assert_eq!(health.backoff_step, 0);
    }

    #[tokio::test]
    async fn flush_service_delivers_chunks_and_compacts_queue() {
        let (dir, queue) = temp_queue("chunks");
        let mut entries: Vec<QueueEntry> = (0..SCROBBLE_BATCH_MAX + 2)
            .map(|idx| entry(idx, vec![ServiceKind::Lastfm]))
            .collect();
        queue.rewrite(&entries).unwrap();
        let service = FakeService::new(ServiceKind::Lastfm, Vec::new());
        let mut health = Health::default();
        let (events, emit) = captured_events();

        flush_service(&service, &mut entries, &queue, &mut health, &emit).await;

        assert!(entries.is_empty());
        assert_eq!(service.batch_sizes(), vec![SCROBBLE_BATCH_MAX, 2]);
        assert!(
            queue.load().entries.is_empty(),
            "successful delivery removes all entries from disk"
        );
        assert!(health.ready(Instant::now()));
        assert!(events.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn flush_service_treats_invalid_content_as_delivered() {
        let (dir, queue) = temp_queue("invalid");
        let mut entries = vec![entry(0, vec![ServiceKind::ListenBrainz])];
        queue.rewrite(&entries).unwrap();
        let service = FakeService::new(
            ServiceKind::ListenBrainz,
            vec![Err(ScrobbleError::Invalid("bad metadata".to_owned()))],
        );
        let mut health = Health::default();
        let (events, emit) = captured_events();

        flush_service(&service, &mut entries, &queue, &mut health, &emit).await;

        assert!(entries.is_empty());
        assert!(queue.load().entries.is_empty());
        assert!(health.ready(Instant::now()));
        assert!(events.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn flush_service_auth_failure_latches_and_leaves_pending() {
        let (dir, queue) = temp_queue("auth");
        let original = vec![entry(0, vec![ServiceKind::Lastfm])];
        let mut entries = original.clone();
        queue.rewrite(&entries).unwrap();
        let service = FakeService::new(
            ServiceKind::Lastfm,
            vec![Err(ScrobbleError::Auth("revoked".to_owned()))],
        );
        let mut health = Health::default();
        let (events, emit) = captured_events();

        flush_service(&service, &mut entries, &queue, &mut health, &emit).await;

        assert_eq!(entries, original);
        assert_eq!(queue.load().entries, original);
        assert!(health.auth_latched);
        let captured = events.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(matches!(
            captured[0],
            ScrobbleEvent::SessionInvalid(ServiceKind::Lastfm)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn enqueue_scrobble_persists_pending_services_and_clamps_future_time() {
        let (dir, queue) = temp_queue("enqueue");
        let (_events, emit) = captured_events();
        let mut actor = test_actor(active_settings(), Some(queue), emit);
        let future = crate::signals::unix_now() + 3600;

        actor.enqueue_scrobble(track(future));

        let queue = actor.queue.as_ref().unwrap();
        let loaded = queue.load();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(
            loaded.entries[0].pending,
            vec![ServiceKind::Lastfm, ServiceKind::ListenBrainz]
        );
        assert!(
            loaded.entries[0].ts <= crate::signals::unix_now(),
            "future timestamps are moved behind now before queueing"
        );
        assert!(actor.next_flush.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn inactive_enqueue_does_not_create_queue_entry() {
        let (dir, queue) = temp_queue("inactive");
        let (_events, emit) = captured_events();
        let mut actor = test_actor(inactive_settings(), Some(queue), emit);

        actor.enqueue_scrobble(track(1000));

        assert!(actor.queue.as_ref().unwrap().load().entries.is_empty());
        assert!(actor.next_flush.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reconfigure_rebuilds_clients_resets_health_and_schedules_flush() {
        let (_events, emit) = captured_events();
        let mut actor = test_actor(inactive_settings(), None, emit);
        actor.lastfm_health.auth_latched = true;
        actor.listenbrainz_health.backoff_until = Some(Instant::now() + Duration::from_secs(60));

        actor.reconfigure(active_settings());

        assert!(actor.lastfm.is_some());
        assert!(actor.listenbrainz.is_some());
        assert!(actor.lastfm_health.ready(Instant::now()));
        assert!(actor.listenbrainz_health.ready(Instant::now()));
        assert!(actor.next_flush.is_some());
    }

    #[test]
    fn start_auth_without_app_emits_failure_without_spawning() {
        let (events, emit) = captured_events();
        let mut actor = test_actor(inactive_settings(), None, emit);

        actor.start_auth();

        assert!(actor.auth_task.is_none());
        let captured = events.lock().unwrap();
        assert_eq!(captured.len(), 1);
        match &captured[0] {
            ScrobbleEvent::AuthFailed(msg) => assert!(msg.contains("app credentials missing")),
            _ => panic!("expected AuthFailed"),
        }
    }

    #[test]
    fn schedule_flush_keeps_earliest_deadline() {
        let (_events, emit) = captured_events();
        let mut actor = test_actor(inactive_settings(), None, emit);

        actor.schedule_flush(Duration::from_secs(30));
        let later = actor.next_flush.unwrap();
        actor.schedule_flush(Duration::from_secs(1));
        let earlier = actor.next_flush.unwrap();

        assert!(earlier < later);
    }
}
