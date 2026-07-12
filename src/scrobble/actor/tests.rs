use super::*;
use std::sync::Mutex;

use super::super::{
    LastfmApp, LastfmSession, ListenBrainzSession, PendingCommand,
    monitor::{Observation, ObservedTrack},
};

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

    async fn love(&self, _artist: &str, _title: &str, _love: bool) -> Result<(), ScrobbleError> {
        Ok(())
    }
}

fn temp_queue(name: &str) -> (std::path::PathBuf, QueueFile) {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).unwrap();
    let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let dir = std::env::temp_dir().join(format!(
        "yututui-sactor-{name}-{}-{suffix}",
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

fn durable_only_settings() -> ScrobbleSettings {
    ScrobbleSettings {
        // A connected service makes threshold crossings durable, while the absent app
        // credentials deliberately leave no network client for the final test flush.
        lastfm_app: None,
        lastfm: Some(LastfmSession {
            session_key: "unused-test-session".to_owned(),
            love_sync: false,
        }),
        listenbrainz: None,
        local_files: false,
    }
}

fn threshold_observations(key: &str) -> (i64, Vec<Observation>) {
    let track = ObservedTrack {
        key: key.to_owned(),
        title: "Shutdown Track".to_owned(),
        artist: "Artist".to_owned(),
        album: Some("Album".to_owned()),
        duration: Some(40.0),
        is_live: false,
        is_local: false,
        origin_url: None,
        liked: false,
    };
    let started_at = Instant::now();
    let started_unix = crate::signals::unix_now();
    let observations = (0..=4_u64)
        .map(|step| Observation {
            track: Some(track.clone()),
            playing: true,
            stopped: false,
            position: (step * 5) as f64,
            position_epoch: 1,
            rate: 1.0,
            at: started_at + Duration::from_secs(step * 5),
            wall_unix: started_unix + (step * 5) as i64,
        })
        .collect();
    (started_unix, observations)
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
        pending_appends: VecDeque::new(),
        pending_append_lock: None,
        pending_compaction: None,
        append_failure_active: false,
        dropped_total: 0,
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

    let lock = queue.try_lock().expect("test owns queue");
    assert!(
        flush_service(&service, &mut entries, &queue, &lock, &mut health, &emit)
            .await
            .is_none()
    );
    drop(lock);

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

    let lock = queue.try_lock().expect("test owns queue");
    assert!(
        flush_service(&service, &mut entries, &queue, &lock, &mut health, &emit)
            .await
            .is_none()
    );
    drop(lock);

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

    let lock = queue.try_lock().expect("test owns queue");
    assert!(
        flush_service(&service, &mut entries, &queue, &lock, &mut health, &emit)
            .await
            .is_none()
    );
    drop(lock);

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

#[tokio::test]
async fn pre_replace_compaction_failure_retries_before_resubmission() {
    assert_compaction_failure_does_not_resubmit(false).await;
}

#[tokio::test]
async fn post_replace_compaction_failure_retries_before_resubmission() {
    assert_compaction_failure_does_not_resubmit(true).await;
}

async fn assert_compaction_failure_does_not_resubmit(after_replace: bool) {
    let label = if after_replace {
        "compaction-post-replace"
    } else {
        "compaction-pre-replace"
    };
    let (dir, queue) = temp_queue(label);
    let original = vec![entry(0, vec![ServiceKind::Lastfm])];
    queue.rewrite(&original).unwrap();
    if after_replace {
        queue.fail_next_rewrites_after_replace(1);
    } else {
        queue.fail_next_rewrites(1);
    }
    let service = FakeService::new(ServiceKind::Lastfm, Vec::new());
    let mut entries = original;
    let mut health = Health::default();
    let (_events, emit) = captured_events();
    let lock = queue.try_lock().expect("test owns queue");

    let acknowledgements = flush_service(&service, &mut entries, &queue, &lock, &mut health, &emit)
        .await
        .expect("successful network delivery with failed rewrite retains acknowledgement");
    assert_eq!(service.batch_sizes(), vec![1]);
    assert_eq!(acknowledgements.len(), 1);
    let mut pending = PendingCompaction {
        acknowledgements,
        lock,
    };
    let contender = QueueFile::at(queue.path().to_path_buf());
    assert!(
        contender.try_lock().is_none(),
        "another process cannot reload or resubmit during compaction uncertainty"
    );

    retry_pending_compaction(&queue, &pending.lock, &mut pending.acknowledgements)
        .expect("the retained acknowledgement compacts idempotently");
    assert!(pending.acknowledgements.is_empty());
    assert!(queue.load().entries.is_empty());

    drop(pending);
    let contender_lock = contender
        .try_lock()
        .expect("successful recovery releases queue ownership immediately");
    assert!(contender.load().entries.is_empty());

    let mut reloaded = queue.load().entries;
    assert!(
        flush_service(
            &service,
            &mut reloaded,
            &contender,
            &contender_lock,
            &mut health,
            &emit,
        )
        .await
        .is_none()
    );
    assert_eq!(
        service.batch_sizes(),
        vec![1],
        "the same process never resubmits after local compaction uncertainty"
    );
    drop(contender_lock);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn failed_compaction_recovers_before_new_pending_append() {
    let (dir, queue) = temp_queue("compaction-before-new-append");
    let original = vec![entry(0, vec![ServiceKind::Lastfm])];
    queue.rewrite(&original).unwrap();
    queue.fail_next_rewrites(1);
    let service = FakeService::new(ServiceKind::Lastfm, Vec::new());
    let mut entries = original;
    let mut health = Health::default();
    let (_events, emit) = captured_events();
    let lock = queue.try_lock().expect("test owns queue");
    let acknowledgements = flush_service(&service, &mut entries, &queue, &lock, &mut health, &emit)
        .await
        .expect("network acknowledgement survives failed compaction");

    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);
    actor.pending_compaction = Some(PendingCompaction {
        acknowledgements,
        lock,
    });
    let new_ts = crate::signals::unix_now();
    actor.enqueue_scrobble(track(new_ts));
    assert_eq!(actor.pending_appends.len(), 1);
    assert!(actor.pending_append_lock.is_none());
    assert!(actor.pending_compaction.is_some());

    assert_eq!(actor.flush().await, Ok(()));
    assert!(actor.pending_compaction.is_none());
    assert!(actor.pending_appends.is_empty());
    assert!(actor.pending_append_lock.is_none());
    assert_eq!(service.batch_sizes(), vec![1]);
    let loaded = actor.queue.as_ref().unwrap().load();
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].track_key, "track-key");
    assert_eq!(loaded.entries[0].ts, new_ts);

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
fn disk_full_append_is_retained_and_recovers_exactly_once() {
    let (dir, queue) = temp_queue("append-disk-full-recovery");
    let queue_path = queue.path().to_path_buf();
    queue.fail_next_appends(1);
    let (events, emit) = captured_events();
    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);
    let listen = track(1_234);

    let now_playing = actor.record_durable_actions(vec![ScrobbleAction::Scrobble(listen.clone())]);

    assert!(now_playing.is_empty());
    assert_eq!(actor.pending_appends.len(), 1);
    assert!(actor.append_failure_active);
    let retained_id = actor.pending_appends.front().unwrap().id.clone();
    assert!(actor.queue.as_ref().unwrap().load().entries.is_empty());
    {
        let captured = events.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert!(matches!(
            captured[0],
            ScrobbleEvent::QueueStalled { pending: 1 }
        ));
    }

    // Storage is writable again. Retrying reuses the retained id and emits the documented
    // `pending == 0` recovery signal exactly once.
    actor.retry_pending_appends().unwrap();
    actor.retry_pending_appends().unwrap();

    assert!(actor.pending_appends.is_empty());
    assert!(!actor.append_failure_active);
    let loaded = QueueFile::at(queue_path).load();
    assert!(!loaded.read_failed);
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].id, retained_id);
    assert_eq!(loaded.entries[0].track_key, listen.key);
    assert_eq!(loaded.entries[0].ts, listen.started_unix);
    let captured = events.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(matches!(
        captured[1],
        ScrobbleEvent::QueueStalled { pending: 0 }
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn ambiguous_append_failure_blocks_submission_until_durable_retry() {
    let (dir, queue) = temp_queue("append-ambiguous-recovery");
    let queue_path = queue.path().to_path_buf();
    queue.fail_next_appends_after_write(2);
    let (_events, emit) = captured_events();
    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);

    actor.enqueue_scrobble(track(2_345));
    let retained_id = actor.pending_appends.front().unwrap().id.clone();
    assert_eq!(actor.pending_appends.len(), 1);
    assert!(actor.pending_append_lock.is_some());
    assert_eq!(actor.queue.as_ref().unwrap().load().entries.len(), 1);
    let contender = QueueFile::at(queue_path.clone());
    assert!(
        contender.try_lock().is_none(),
        "another process cannot load and submit an ambiguously appended entry"
    );

    assert_eq!(actor.flush().await, Err(DeliveryError::Saturated));

    assert_eq!(actor.pending_appends.len(), 1);
    assert!(actor.pending_append_lock.is_some());
    assert!(actor.next_flush.is_some());
    assert_eq!(
        std::fs::read_to_string(&queue_path)
            .unwrap()
            .lines()
            .count(),
        2,
        "flush must not submit or compact while append acknowledgement is ambiguous"
    );
    assert!(
        contender.try_lock().is_none(),
        "the retained guard spans every failed durability retry"
    );

    actor.retry_pending_appends().unwrap();
    assert!(actor.pending_appends.is_empty());
    assert!(actor.pending_append_lock.is_none());
    let lock = contender
        .try_lock()
        .expect("confirmed append releases queue ownership immediately");
    let loaded = contender.load();
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].id, retained_id);

    let service = FakeService::new(ServiceKind::Lastfm, Vec::new());
    let mut entries = loaded.entries;
    let mut health = Health::default();
    let (_events, emit) = captured_events();
    assert!(
        flush_service(
            &service,
            &mut entries,
            &contender,
            &lock,
            &mut health,
            &emit,
        )
        .await
        .is_none()
    );
    drop(lock);

    assert_eq!(service.batch_sizes(), vec![1]);
    assert!(contender.load().entries.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn retained_durability_guards_release_when_owner_unwinds() {
    let (dir, queue) = temp_queue("durability-lock-unwind");
    let queue_path = queue.path().to_path_buf();
    queue.fail_next_appends_after_write(1);
    let contender = QueueFile::at(queue_path);
    let (_events, emit) = captured_events();

    let append_unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut actor = test_actor(durable_only_settings(), Some(queue), emit);
        actor.enqueue_scrobble(track(3_456));
        assert!(actor.pending_append_lock.is_some());
        assert!(contender.try_lock().is_none());
        panic!("exercise retained append-lock unwind");
    }));
    assert!(append_unwind.is_err());
    let recovered = contender
        .try_lock()
        .expect("dropping an unwinding actor releases its retained append guard");
    drop(recovered);

    let compaction = PendingCompaction {
        acknowledgements: vec![CompactionAck {
            entry_id: "unwind-entry".to_owned(),
            service: ServiceKind::Lastfm,
        }],
        lock: contender.try_lock().expect("compaction guard acquired"),
    };
    let compaction_unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _pending = compaction;
        assert!(contender.try_lock().is_none());
        panic!("exercise retained compaction-lock unwind");
    }));
    assert!(compaction_unwind.is_err());
    assert!(
        contender.try_lock().is_some(),
        "unwinding a pending compaction releases its kernel guard"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn failed_append_retention_is_bounded_and_reports_oldest_loss() {
    let (dir, queue) = temp_queue("append-retention-cap");
    queue.fail_next_appends(1);
    let (events, emit) = captured_events();
    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);
    for index in 0..QUEUE_CAP {
        actor
            .pending_appends
            .push_back(entry(index, vec![ServiceKind::Lastfm]));
    }
    let oldest_id = actor.pending_appends.front().unwrap().id.clone();

    actor.enqueue_scrobble(track(9_999));

    assert_eq!(actor.pending_appends.len(), QUEUE_CAP);
    assert_ne!(actor.pending_appends.front().unwrap().id, oldest_id);
    let captured = events.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(matches!(
        captured[0],
        ScrobbleEvent::QueueDropped { dropped: 1 }
    ));
    assert!(matches!(
        captured[1],
        ScrobbleEvent::QueueStalled { pending: QUEUE_CAP }
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn queue_drop_notifications_are_actor_lifetime_cumulative_totals() {
    let (events, emit) = captured_events();
    let mut actor = test_actor(inactive_settings(), None, emit);

    emit_queue_drop(&mut actor.dropped_total, &actor.emit, 2);
    emit_queue_drop(&mut actor.dropped_total, &actor.emit, 3);

    let captured = events.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(matches!(
        captured[0],
        ScrobbleEvent::QueueDropped { dropped: 2 }
    ));
    assert!(matches!(
        captured[1],
        ScrobbleEvent::QueueDropped { dropped: 5 }
    ));
}

#[test]
fn observation_records_scrobble_before_returning_ephemeral_network_work() {
    let (dir, queue) = temp_queue("durable-before-now-playing");
    let (_events, emit) = captured_events();
    let mut actor = test_actor(active_settings(), Some(queue), emit);
    let now_playing = track(1_000);
    let scrobble = track(1_001);

    let deferred_network = actor.record_durable_actions(vec![
        ScrobbleAction::NowPlaying(now_playing.clone()),
        ScrobbleAction::Scrobble(scrobble.clone()),
    ]);

    assert_eq!(deferred_network, vec![now_playing]);
    let loaded = actor.queue.as_ref().unwrap().load();
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].track_key, scrobble.key);
    assert_eq!(loaded.entries[0].ts, scrobble.started_unix);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn shutdown_queued_before_start_drains_admitted_observations_to_durable_queue() {
    let (dir, queue) = temp_queue("shutdown-drain");
    let queue_path = queue.path().to_path_buf();
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    let (_events, emit) = captured_events();

    assert!(
        tx.try_send(ScrobbleCmd::Reconfigure(Box::new(durable_only_settings())))
            .is_ok()
    );
    let (started_unix, observations) = threshold_observations("shutdown-track");
    for observation in observations {
        assert!(
            tx.try_send(ScrobbleCmd::Observe(Box::new(ObservationBatch::single(
                observation,
            ))))
            .is_ok()
        );
    }

    let (done, acknowledged) = oneshot::channel();
    assert!(shutdown_tx.try_send(ShutdownRequest { done }).is_ok());
    let actor = tokio::spawn(run_actor(
        rx,
        shutdown_rx,
        Arc::new(PendingCommands::default()),
        inactive_settings(),
        emit,
        Some(queue),
    ));

    tokio::time::timeout(
        SHUTDOWN_FLUSH_BUDGET + Duration::from_millis(250),
        acknowledged,
    )
    .await
    .expect("shutdown remains bounded")
    .expect("actor acknowledges shutdown after draining admitted work")
    .expect("drained work reaches a confirmed durability boundary");
    tokio::time::timeout(Duration::from_secs(1), actor)
        .await
        .expect("actor exits after shutdown")
        .expect("actor task joins");

    assert!(matches!(
        tx.try_send(ScrobbleCmd::AuthStart),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
    ));
    let loaded = QueueFile::at(queue_path).load();
    assert!(!loaded.read_failed);
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].track_key, "shutdown-track");
    assert_eq!(loaded.entries[0].ts, started_unix);
    assert_eq!(loaded.entries[0].pending, vec![ServiceKind::Lastfm]);

    let _ = std::fs::remove_dir_all(dir);
}

async fn shutdown_handle_result(actor: &mut Actor) -> Result<(), DeliveryError> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);
    let handle = ScrobbleHandle::new(tx, shutdown_tx);
    let pending = Arc::clone(&handle.pending);
    let flush = tokio::spawn(async move { handle.shutdown_flush().await });
    let request = shutdown_rx
        .recv()
        .await
        .expect("handle publishes shutdown request");

    finish_shutdown(actor, &mut rx, &pending, Some(request)).await;
    flush.await.expect("shutdown handle task joins")
}

#[tokio::test(flavor = "current_thread")]
async fn cancelling_shutdown_caller_cannot_revoke_locally_accepted_backlog() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);
    let handle = ScrobbleHandle::new(tx, shutdown_tx);
    let pending = Arc::clone(&handle.pending);
    assert_eq!(
        handle.auth_start(),
        Ok(crate::util::delivery::DeliveryReceipt::Enqueued)
    );
    let mut latest = inactive_settings();
    latest.local_files = true;
    assert_eq!(
        handle.reconfigure(latest),
        Ok(crate::util::delivery::DeliveryReceipt::Deferred)
    );

    let flush = tokio::spawn(async move { handle.shutdown_flush().await });
    let request = shutdown_rx.recv().await.expect("shutdown request queued");
    flush.abort();
    assert!(flush.await.unwrap_err().is_cancelled());

    let (_events, emit) = captured_events();
    let mut actor = test_actor(inactive_settings(), None, emit);
    finish_shutdown(&mut actor, &mut rx, &pending, Some(request)).await;
    assert!(
        actor.settings.local_files,
        "caller timeout must not revoke a configuration already retained by the local drainer"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn immediate_shutdown_applies_latest_terminal_observation_rejected_as_busy() {
    let (dir, queue) = temp_queue("shutdown-terminal-retry");
    let queue_path = queue.path().to_path_buf();
    let (_events, emit) = captured_events();
    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);

    // Accumulate 20 seconds against a long-duration snapshot. The accepted terminal backlog
    // below keeps that listen paused but ineligible; only the rejected latest metadata snapshot
    // lowers the threshold enough to produce the durable scrobble asserted at the end.
    let started_at = Instant::now()
        .checked_sub(Duration::from_secs(20))
        .expect("test monotonic clock can represent a short prior listen");
    let started_unix = crate::signals::unix_now() - 20;
    let long_track = ObservedTrack {
        key: "shutdown-terminal-retry".to_owned(),
        title: "Shutdown Terminal Retry".to_owned(),
        artist: "Artist".to_owned(),
        album: Some("Album".to_owned()),
        duration: Some(100.0),
        is_live: false,
        is_local: false,
        origin_url: None,
        liked: false,
    };
    for step in 0..=4_u64 {
        let _now_playing = actor.record_observation_batch(ObservationBatch::single(Observation {
            track: Some(long_track.clone()),
            playing: true,
            stopped: false,
            position: (step * 5) as f64,
            position_epoch: 1,
            rate: 1.0,
            at: started_at + Duration::from_secs(step * 5),
            wall_unix: started_unix + (step * 5) as i64,
        }));
    }
    assert!(QueueFile::at(queue_path.clone()).load().entries.is_empty());

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);
    let mut handle = ScrobbleHandle::new(tx, shutdown_tx);
    let pending = Arc::clone(&handle.pending);
    assert_eq!(
        handle.auth_start(),
        Ok(crate::util::delivery::DeliveryReceipt::Enqueued)
    );
    {
        let mut state = super::super::lock_pending(&pending.state);
        // Hold the synthetic drainer flag while filling the terminal lane so `observe` reaches
        // its deterministic full-FIFO rejection before a real delivery thread can make room.
        state.drainer_running = true;
        for epoch in 10..10 + super::super::PENDING_TERMINAL_CAPACITY as u64 {
            state
                .terminal_observations
                .push_back(Box::new(ObservationBatch::single(Observation {
                    track: Some(long_track.clone()),
                    playing: false,
                    stopped: false,
                    position: 20.0,
                    position_epoch: epoch,
                    rate: 1.0,
                    at: started_at + Duration::from_secs(20),
                    wall_unix: started_unix + 20,
                })));
        }
    }

    let mut latest = crate::media::MediaSnapshot::idle();
    latest.status = crate::media::MediaPlaybackStatus::Paused;
    latest.position = 20.0;
    latest.position_epoch = 1_000;
    latest.track = Some(crate::media::MediaTrack {
        key: long_track.key.clone(),
        title: long_track.title.clone(),
        artist: long_track.artist.clone(),
        album: long_track.album.clone(),
        duration: Some(40.0),
        is_live: false,
        url: None,
        art_remote_url: None,
        art_file: None,
        art_query: None,
        liked: false,
        disliked: false,
    });
    assert_eq!(handle.observe(&latest), Err(DeliveryError::Busy));
    assert_eq!(
        super::super::lock_pending(&pending.state)
            .terminal_retry
            .as_ref()
            .and_then(|batch| batch.latest().track.as_ref())
            .and_then(|track| track.duration),
        Some(40.0)
    );

    assert!(super::super::spawn_pending_drainer(
        handle.tx.clone(),
        Arc::clone(&pending),
    ));
    let flush = tokio::spawn(async move { handle.shutdown_flush().await });
    let request = shutdown_rx
        .recv()
        .await
        .expect("immediate shutdown request is queued");
    finish_shutdown(&mut actor, &mut rx, &pending, Some(request)).await;
    assert_eq!(
        tokio::time::timeout(SHUTDOWN_FLUSH_BUDGET + Duration::from_millis(250), flush)
            .await
            .expect("immediate shutdown remains bounded")
            .expect("shutdown handle task joins"),
        Ok(())
    );
    assert!(
        super::super::lock_pending(&pending.state)
            .terminal_retry
            .is_none(),
        "shutdown consumes the sealed retry tail"
    );

    let loaded = QueueFile::at(queue_path).load();
    assert!(!loaded.read_failed);
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].track_key, "shutdown-terminal-retry");
    assert_eq!(loaded.entries[0].duration, Some(40));
    assert_eq!(loaded.entries[0].ts, started_unix);
    assert_eq!(loaded.entries[0].pending, vec![ServiceKind::Lastfm]);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn shutdown_reports_saturated_when_pending_append_cannot_reach_disk() {
    let (dir, queue) = temp_queue("shutdown-disk-full");
    let queue_path = queue.path().to_path_buf();
    queue.fail_next_appends(1);
    let (events, emit) = captured_events();
    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);
    actor
        .pending_appends
        .push_back(entry(0, vec![ServiceKind::Lastfm]));

    assert_eq!(
        shutdown_handle_result(&mut actor).await,
        Err(DeliveryError::Saturated),
        "shutdown must not acknowledge an ENOSPC-retained listen as durable"
    );
    assert_eq!(actor.pending_appends.len(), 1);
    assert!(actor.pending_append_lock.is_some());
    assert!(matches!(
        events.lock().unwrap().as_slice(),
        [ScrobbleEvent::QueueStalled { pending: 1 }]
    ));
    let contender = QueueFile::at(queue_path);
    assert!(contender.try_lock().is_none());

    drop(actor);
    assert!(
        contender.try_lock().is_some(),
        "actor drop releases the failed append guard after reporting shutdown failure"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn shutdown_reports_saturated_while_acknowledged_compaction_is_unconfirmed() {
    let (dir, queue) = temp_queue("shutdown-compaction-failure");
    let queue_path = queue.path().to_path_buf();
    let original = entry(0, vec![ServiceKind::Lastfm]);
    queue.rewrite(std::slice::from_ref(&original)).unwrap();
    let lock = queue.try_lock().expect("actor owns failed compaction");
    queue.fail_next_rewrites(1);
    let (_events, emit) = captured_events();
    let mut actor = test_actor(durable_only_settings(), Some(queue), emit);
    actor.pending_compaction = Some(PendingCompaction {
        acknowledgements: vec![CompactionAck {
            entry_id: original.id,
            service: ServiceKind::Lastfm,
        }],
        lock,
    });

    assert_eq!(
        shutdown_handle_result(&mut actor).await,
        Err(DeliveryError::Saturated)
    );
    assert!(actor.pending_compaction.is_some());
    let contender = QueueFile::at(queue_path);
    assert!(
        contender.try_lock().is_none(),
        "failed shutdown retains compaction ownership until actor teardown"
    );

    drop(actor);
    assert!(contender.try_lock().is_some());
    assert_eq!(contender.load().entries.len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn shutdown_reports_busy_while_another_process_owns_queue() {
    let (dir, owner) = temp_queue("shutdown-competing-lock");
    let queue_path = owner.path().to_path_buf();
    let competing_lock = owner.try_lock().expect("competing process owns queue");
    let (_events, emit) = captured_events();
    let mut actor = test_actor(
        durable_only_settings(),
        Some(QueueFile::at(queue_path.clone())),
        emit,
    );

    assert_eq!(
        shutdown_handle_result(&mut actor).await,
        Err(DeliveryError::Busy),
        "queue-lock contention must be visible to the shutdown caller"
    );
    assert!(!actor.has_pending_durability());

    drop(competing_lock);
    assert_eq!(actor.flush().await, Ok(()));
    assert!(QueueFile::at(queue_path).try_lock().is_some());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_durable_append_keeps_runtime_responsive_and_drop_joins_owner() {
    let (dir, mut queue) = temp_queue("blocked-io-isolated");
    let queue_path = queue.path().to_path_buf();
    let append_block = queue.block_next_append();
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    let (_events, emit) = captured_events();
    let pending = Arc::new(PendingCommands::default());
    let actor_thread = spawn_actor_thread(
        rx,
        shutdown_rx,
        Arc::clone(&pending),
        durable_only_settings(),
        emit,
        Some(queue),
    )
    .expect("isolated actor thread starts");
    let handle = ScrobbleHandle::with_pending(tx, shutdown_tx, pending, Some(actor_thread));

    let (_started_unix, observations) = threshold_observations("blocked-io-track");
    for observation in observations {
        handle
            .tx
            .try_send(ScrobbleCmd::Observe(Box::new(ObservationBatch::single(
                observation,
            ))))
            .expect("threshold observations fit the actor inbox");
    }
    tokio::time::timeout(Duration::from_secs(1), append_block.wait_until_blocked())
        .await
        .expect("append reaches deterministic blocking point");

    // The durable owner is blocked on another OS thread. The application's current-thread
    // runtime can still run signal/quit timers and publish its shutdown request.
    tokio::time::timeout(
        Duration::from_millis(100),
        tokio::time::sleep(Duration::from_millis(10)),
    )
    .await
    .expect("main current-thread timer remains responsive during blocked fsync path");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), handle.shutdown_flush())
            .await
            .is_err(),
        "the caller's shutdown budget fires instead of waiting on blocked durable I/O"
    );

    // Cancelling that receipt must not detach the actor. Drop is the final process-lifetime
    // owner and therefore waits until a separately released fsync finishes and the actor exits.
    let delayed_release = append_block.clone();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        delayed_release.release();
    });
    let drop_started = Instant::now();
    drop(handle);
    assert!(
        drop_started.elapsed() >= Duration::from_millis(20),
        "handle drop must join rather than detach the blocked durability owner"
    );
    release
        .join()
        .expect("fault-injection release thread joins");
    let loaded = QueueFile::at(queue_path.clone()).load();
    assert!(!loaded.read_failed);
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].track_key, "blocked-io-track");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_interrupt_applies_latest_config_and_post_config_observation() {
    let (dir, queue) = temp_queue("shutdown-config-tail");
    let queue_path = queue.path().to_path_buf();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);
    let handle = ScrobbleHandle::new(tx, shutdown_tx);
    let pending = Arc::clone(&handle.pending);
    let (_events, emit) = captured_events();

    assert_eq!(
        handle.auth_start(),
        Ok(crate::util::delivery::DeliveryReceipt::Enqueued)
    );
    assert_eq!(
        handle.reconfigure(inactive_settings()),
        Ok(crate::util::delivery::DeliveryReceipt::Deferred)
    );
    let mut latest = durable_only_settings();
    latest.local_files = true;
    assert_eq!(
        handle.reconfigure(latest),
        Ok(crate::util::delivery::DeliveryReceipt::Coalesced {
            replaced_existing: true,
            evicted_oldest: false,
        })
    );
    let (started_unix, observations) = threshold_observations("local:shutdown-config-tail");
    for mut observation in observations {
        observation
            .track
            .as_mut()
            .expect("threshold observation has a track")
            .is_local = true;
        assert!(
            handle
                .admit_pending(PendingCommand::Observe(Box::new(ObservationBatch::single(
                    observation
                ),)))
                .is_ok()
        );
    }
    let flush = tokio::spawn(async move { handle.shutdown_flush().await });
    tokio::task::yield_now().await;

    let mut actor = test_actor(inactive_settings(), Some(queue), emit);
    let request = shutdown_rx.recv().await.expect("shutdown request queued");
    finish_shutdown(&mut actor, &mut rx, &pending, Some(request)).await;
    assert_eq!(
        tokio::time::timeout(SHUTDOWN_FLUSH_BUDGET + Duration::from_millis(250), flush)
            .await
            .expect("handle shutdown remains bounded")
            .expect("handle shutdown task joins"),
        Ok(())
    );
    assert!(actor.settings.local_files);
    assert!(actor.settings.lastfm.is_some());
    let loaded = QueueFile::at(queue_path).load();
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].track_key, "local:shutdown-config-tail");
    assert_eq!(loaded.entries[0].ts, started_unix);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn dropped_shutdown_sender_still_drains_already_admitted_observations() {
    let (dir, queue) = temp_queue("shutdown-sender-drop");
    let queue_path = queue.path().to_path_buf();
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    let (_events, emit) = captured_events();
    let (started_unix, observations) = threshold_observations("shutdown-drop-track");
    for observation in observations {
        assert!(
            tx.try_send(ScrobbleCmd::Observe(Box::new(ObservationBatch::single(
                observation,
            ))))
            .is_ok()
        );
    }
    drop(shutdown_tx);

    tokio::time::timeout(
        SHUTDOWN_FLUSH_BUDGET + Duration::from_millis(250),
        run_actor(
            rx,
            shutdown_rx,
            Arc::new(PendingCommands::default()),
            durable_only_settings(),
            emit,
            Some(queue),
        ),
    )
    .await
    .expect("sender-drop shutdown remains bounded");

    assert!(matches!(
        tx.try_send(ScrobbleCmd::AuthStart),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
    ));
    let loaded = QueueFile::at(queue_path).load();
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].track_key, "shutdown-drop-track");
    assert_eq!(loaded.entries[0].ts, started_unix);
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
