//! Debounced background persistence for the on-disk stores.
//!
//! `runtime::dispatch` used to run every `Cmd::Persist` write inline on the main loop task:
//! pretty-JSON serialize + temp-file write + fsync + rename. An fsync can stall for
//! tens of milliseconds, and the library/signals writes fire on every track load and
//! like/skip — a visible frame hitch. This actor takes an owned snapshot instead,
//! coalesces latest-wins per store, debounces briefly, and does the identical atomic
//! write (each variant delegates to the store's own `save()`) on the blocking pool.
//!
//! Durability contract:
//! - Clean quit (incl. SIGINT/SIGTERM/SIGHUP → `Msg::Quit`): the quit path sends fresh
//!   snapshots and awaits [`PersistHandle::flush`] — zero loss.
//! - Panic (incl. release `panic = "abort"`): [`install_panic_flush`] writes everything
//!   still pending before the inherited hook kills mpv and restores the terminal.
//! - SIGKILL / power loss: at most the per-store debounce below is lost. That window is
//!   the price of not fsyncing on the UI task; every store here is preference/cache
//!   data, not payment ledgers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Notify, mpsc, oneshot};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StoreKind {
    Library,
    Signals,
    Downloads,
    Config,
    Playlists,
    Station,
    RomanizedTitles,
    Session,
}

impl StoreKind {
    pub fn label(self) -> &'static str {
        match self {
            StoreKind::Library => "library",
            StoreKind::Signals => "signals",
            StoreKind::Downloads => "downloads manifest",
            StoreKind::Config => "config",
            StoreKind::Playlists => "playlists",
            StoreKind::Station => "station profile",
            StoreKind::RomanizedTitles => "romanized title cache",
            StoreKind::Session => "session cache",
        }
    }

    fn user_visible_failure(self) -> bool {
        !matches!(self, StoreKind::RomanizedTitles | StoreKind::Session)
    }
}

#[derive(Debug, Clone)]
pub enum PersistEvent {
    WriteFailed { store: StoreKind, error: String },
}

type EventSink = Arc<dyn Fn(PersistEvent) + Send + Sync + 'static>;
type EventSinkSlot = Arc<Mutex<Option<EventSink>>>;

/// Owned copy of one store, taken at send time so the actor never reaches back into
/// `App`. Writing delegates to the store's own `save()` — same path resolution, same
/// atomic temp-write + fsync + rename.
pub enum Snapshot {
    Library(crate::library::Library),
    Signals(crate::signals::Signals),
    Downloads(crate::downloads::DownloadStore),
    Config(Box<crate::config::Config>),
    Playlists(crate::playlists::Playlists),
    Station(crate::station::StationStore),
    RomanizedTitles(crate::romanize::RomanizeCache),
    Session(crate::session::SessionCache),
    #[cfg(test)]
    Test {
        kind: StoreKind,
        label: &'static str,
        writer: Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>,
    },
}

impl Snapshot {
    fn kind(&self) -> StoreKind {
        match self {
            Snapshot::Library(_) => StoreKind::Library,
            Snapshot::Signals(_) => StoreKind::Signals,
            Snapshot::Downloads(_) => StoreKind::Downloads,
            Snapshot::Config(_) => StoreKind::Config,
            Snapshot::Playlists(_) => StoreKind::Playlists,
            Snapshot::Station(_) => StoreKind::Station,
            Snapshot::RomanizedTitles(_) => StoreKind::RomanizedTitles,
            Snapshot::Session(_) => StoreKind::Session,
            #[cfg(test)]
            Snapshot::Test { kind, .. } => *kind,
        }
    }

    fn write(&self) -> std::io::Result<()> {
        match self {
            Snapshot::Library(s) => s.save(),
            Snapshot::Signals(s) => s.save(),
            Snapshot::Downloads(s) => s.save(),
            Snapshot::Config(s) => s.save(),
            Snapshot::Playlists(s) => s.save(),
            Snapshot::Station(s) => s.save(),
            Snapshot::RomanizedTitles(s) => s.save(),
            Snapshot::Session(s) => s.save(),
            #[cfg(test)]
            Snapshot::Test { writer, .. } => writer(),
        }
    }

    fn label(&self) -> &'static str {
        #[cfg(test)]
        if let Snapshot::Test { label, .. } = self {
            return label;
        }
        self.kind().label()
    }
}

/// How long a store may sit dirty before its write lands. The deadline is armed by the
/// *first* dirty event and not pushed back by later ones, so a continuous stream of
/// saves still hits disk once per window (bounded staleness, no starvation).
fn debounce(kind: StoreKind) -> Duration {
    match kind {
        // Ratings/favorites/likes: flush fast — this is the data a user would miss.
        StoreKind::Library | StoreKind::Signals => Duration::from_millis(300),
        StoreKind::Downloads | StoreKind::Config | StoreKind::Playlists | StoreKind::Station => {
            Duration::from_millis(500)
        }
        // Pure display cache, fully rebuildable — lazy is fine.
        StoreKind::RomanizedTitles => Duration::from_secs(3),
        // Only sent at quit, where flush() drains immediately anyway.
        StoreKind::Session => Duration::ZERO,
    }
}

type SharedPending = Arc<Mutex<HashMap<StoreKind, Snapshot>>>;

enum PersistMsg {
    DeleteRomanizedTitles,
    Flush(oneshot::Sender<bool>),
}

#[derive(Clone)]
pub struct PersistHandle {
    tx: mpsc::Sender<PersistMsg>,
    pending: SharedPending,
    dirty: Arc<Notify>,
    events: EventSinkSlot,
}

impl PersistHandle {
    pub fn save(&self, snapshot: Snapshot) {
        let kind = snapshot.kind();
        lock(&self.pending).insert(kind, snapshot);
        self.dirty.notify_one();
    }

    /// Delete the romanize cache file. Routed through the actor so channel order makes
    /// it impossible for an older pending save to resurrect the file afterwards.
    pub fn delete_romanized_titles(&self) {
        lock(&self.pending).remove(&StoreKind::RomanizedTitles);
        match self.tx.try_send(PersistMsg::DeleteRomanizedTitles) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(msg)) => {
                let tx = self.tx.clone();
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(async move {
                        let _ = tx.send(msg).await;
                    });
                } else {
                    tracing::warn!("persist control queue full; romanized cache delete deferred");
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("persist actor stopped before romanized cache delete");
            }
        }
    }

    /// Drain every pending write, bounded by `budget`. Returns `false` on timeout or when a
    /// failed write remains dirty for retry.
    pub async fn flush(&self, budget: Duration) -> bool {
        let (ack_tx, ack_rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + budget;
        match tokio::time::timeout_at(deadline, self.tx.send(PersistMsg::Flush(ack_tx))).await {
            Ok(Ok(())) => {}
            _ => return false,
        }
        match tokio::time::timeout_at(deadline, ack_rx).await {
            Ok(Ok(clean)) => clean,
            _ => false,
        }
    }

    /// The shared dirty-snapshot map, for [`install_panic_flush`].
    pub fn pending(&self) -> SharedPending {
        Arc::clone(&self.pending)
    }

    pub fn set_event_sink<F>(&self, emit: F)
    where
        F: Fn(PersistEvent) + Send + Sync + 'static,
    {
        *lock_event_sink(&self.events) = Some(Arc::new(emit));
    }
}

pub fn spawn() -> PersistHandle {
    let (tx, rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
    let dirty = Arc::new(Notify::new());
    let events: EventSinkSlot = Arc::new(Mutex::new(None));
    tokio::spawn(run_actor(
        rx,
        Arc::clone(&pending),
        Arc::clone(&dirty),
        Arc::clone(&events),
    ));
    PersistHandle {
        tx,
        pending,
        dirty,
        events,
    }
}

/// Wrap the current panic hook so pending snapshots hit disk before the inherited chain
/// (mpv kill, terminal restore) runs. Must be installed *after*
/// `player::lifetime::install_panic_hook` — last installed runs first.
///
/// `try_lock` (never block): if the persist actor itself panicked while holding the
/// lock, skipping beats deadlocking. The lock is only ever held for a map insert/remove,
/// so in practice this always succeeds. A snapshot the actor already removed and was
/// mid-writing when another thread panicked is not re-written here; the temp+rename
/// write keeps the old file intact (same exposure as a SIGKILL mid-write today).
pub fn install_panic_flush(pending: SharedPending) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(map) = pending.try_lock() {
            for snapshot in map.values() {
                let _ = snapshot.write();
            }
        }
        previous(info);
    }));
}

const RETRY_INITIAL: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Default)]
struct RetryState {
    retry_count: u32,
    retry_not_before: Option<tokio::time::Instant>,
    last_error: Option<String>,
    last_failed_at: Option<tokio::time::Instant>,
}

type RetryMap = HashMap<StoreKind, RetryState>;

async fn run_actor(
    mut rx: mpsc::Receiver<PersistMsg>,
    pending: SharedPending,
    dirty: Arc<Notify>,
    events: EventSinkSlot,
) {
    let mut due: HashMap<StoreKind, tokio::time::Instant> = HashMap::new();
    let mut retries: RetryMap = HashMap::new();
    loop {
        let next_due = due.values().min().copied();
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(PersistMsg::DeleteRomanizedTitles) => {
                    lock(&pending).remove(&StoreKind::RomanizedTitles);
                    due.remove(&StoreKind::RomanizedTitles);
                    retries.remove(&StoreKind::RomanizedTitles);
                    let result = tokio::task::spawn_blocking(
                        crate::romanize::RomanizeCache::delete_saved,
                    )
                    .await;
                    if let Ok(Err(e)) = result {
                        tracing::warn!(error = %e, "failed to delete romanized title cache");
                    }
                }
                Some(PersistMsg::Flush(ack)) => {
                    let clean = write_stores(&pending, &mut due, &mut retries, &events, true).await;
                    let _ = ack.send(clean);
                }
                // All senders dropped (quit already flushed; this is a backstop).
                None => {
                    write_stores(&pending, &mut due, &mut retries, &events, true).await;
                    break;
                }
            },
            _ = dirty.notified() => {
                arm_due_for_pending(&pending, &mut due);
            },
            _ = tokio::time::sleep_until(next_due.unwrap_or_else(tokio::time::Instant::now)),
                if next_due.is_some() =>
            {
                write_stores(&pending, &mut due, &mut retries, &events, false).await;
            }
        }
    }
}

fn lock(pending: &SharedPending) -> std::sync::MutexGuard<'_, HashMap<StoreKind, Snapshot>> {
    // A panicking writer can't leave the map half-mutated in a harmful way (it's a
    // plain insert/remove), so recover from poisoning instead of propagating it.
    pending.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock_event_sink(events: &EventSinkSlot) -> std::sync::MutexGuard<'_, Option<EventSink>> {
    events.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
fn queue_pending_save(
    pending: &SharedPending,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    snapshot: Snapshot,
) {
    let kind = snapshot.kind();
    lock(pending).insert(kind, snapshot);
    due.entry(kind)
        .or_insert_with(|| tokio::time::Instant::now() + debounce(kind));
}

fn arm_due_for_pending(
    pending: &SharedPending,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
) {
    let now = tokio::time::Instant::now();
    let kinds: Vec<StoreKind> = lock(pending).keys().copied().collect();
    for kind in kinds {
        due.entry(kind).or_insert_with(|| now + debounce(kind));
    }
}

fn retry_delay(retry_count: u32) -> Duration {
    let shift = retry_count.saturating_sub(1).min(16);
    RETRY_INITIAL
        .checked_mul(1 << shift)
        .unwrap_or(RETRY_MAX)
        .min(RETRY_MAX)
}

fn emit_persist_event(events: &EventSinkSlot, event: PersistEvent) {
    let sink = lock_event_sink(events).clone();
    if let Some(emit) = sink {
        emit(event);
    }
}

fn requeue_failed_snapshot(
    pending: &SharedPending,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    retries: &mut RetryMap,
    events: &EventSinkSlot,
    snapshot: Snapshot,
    error: String,
) {
    let kind = snapshot.kind();
    let label = snapshot.label();
    let sanitized = crate::util::sanitize::sanitize_error_text(error);
    let now = tokio::time::Instant::now();
    let state = retries.entry(kind).or_default();
    let first_failure = state.retry_count == 0;
    state.retry_count = state.retry_count.saturating_add(1);
    let delay = retry_delay(state.retry_count);
    let retry_at = now + delay;
    state.retry_not_before = Some(retry_at);
    state.last_error = Some(sanitized.clone());
    state.last_failed_at = Some(now);
    let retry_in_ms = state
        .retry_not_before
        .map(|deadline| deadline.saturating_duration_since(now).as_millis())
        .unwrap_or(0);
    let retry_count = state.retry_count;
    let last_error = state
        .last_error
        .clone()
        .unwrap_or_else(|| sanitized.clone());
    let failure_age_ms = state
        .last_failed_at
        .map(|failed_at| now.saturating_duration_since(failed_at).as_millis())
        .unwrap_or(0);

    let mut guard = lock(pending);
    match guard.entry(kind) {
        std::collections::hash_map::Entry::Occupied(_) => {
            tracing::warn!(
                error = %last_error,
                retry_count,
                retry_in_ms,
                failure_age_ms,
                "failed to save {label}; newer snapshot is already pending"
            );
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(snapshot);
            due.insert(kind, retry_at);
            tracing::warn!(
                error = %last_error,
                retry_count,
                retry_in_ms,
                failure_age_ms,
                "failed to save {label}; retry scheduled"
            );
        }
    }
    drop(guard);

    if first_failure && kind.user_visible_failure() {
        emit_persist_event(
            events,
            PersistEvent::WriteFailed {
                store: kind,
                error: sanitized,
            },
        );
    }
}

/// Write every store whose deadline has passed (`all: false`) or everything pending
/// (`all: true`). Snapshots are removed from the map before writing — short lock, and
/// the panic-flush hook then can't double-write a file the actor is mid-writing.
/// Writes run sequentially, which is what guarantees per-file ordering.
async fn write_stores(
    pending: &SharedPending,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    retries: &mut RetryMap,
    events: &EventSinkSlot,
    all: bool,
) -> bool {
    let now = tokio::time::Instant::now();
    let kinds: Vec<StoreKind> = if all {
        // Everything dirty, whether or not its deadline armed (flush/backstop path).
        lock(pending).keys().copied().collect()
    } else {
        due.iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(kind, _)| *kind)
            .collect()
    };
    for kind in kinds {
        due.remove(&kind);
        let Some(snapshot) = lock(pending).remove(&kind) else {
            continue;
        };
        let result = tokio::task::spawn_blocking(move || {
            let outcome = snapshot.write();
            (outcome, snapshot)
        })
        .await;
        match result {
            Ok((Err(e), snapshot)) => {
                requeue_failed_snapshot(pending, due, retries, events, snapshot, e.to_string());
            }
            Err(e) => tracing::warn!(error = %e, "persist write task failed"),
            Ok((Ok(()), snapshot)) => {
                retries.remove(&snapshot.kind());
            }
        }
    }
    lock(pending).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::AssertUnwindSafe;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn debounce_windows_match_store_durability_policy() {
        assert_eq!(debounce(StoreKind::Library), Duration::from_millis(300));
        assert_eq!(debounce(StoreKind::Signals), Duration::from_millis(300));
        assert_eq!(debounce(StoreKind::Downloads), Duration::from_millis(500));
        assert_eq!(debounce(StoreKind::Config), Duration::from_millis(500));
        assert_eq!(debounce(StoreKind::Playlists), Duration::from_millis(500));
        assert_eq!(debounce(StoreKind::Station), Duration::from_millis(500));
        assert_eq!(debounce(StoreKind::RomanizedTitles), Duration::from_secs(3));
        assert_eq!(debounce(StoreKind::Session), Duration::ZERO);
    }

    #[test]
    fn pending_lock_recovers_from_poisoned_mutex() {
        let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));

        let _ = std::panic::catch_unwind(AssertUnwindSafe({
            let pending = Arc::clone(&pending);
            move || {
                let _guard = pending.lock().unwrap();
                panic!("poison pending map");
            }
        }));

        let guard = lock(&pending);
        assert!(guard.is_empty());
    }

    #[tokio::test]
    async fn write_stores_clears_stale_due_entries_when_snapshot_is_missing() {
        let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
        let mut due = HashMap::from([(StoreKind::Library, tokio::time::Instant::now())]);
        let mut retries = HashMap::new();
        let events = Arc::new(Mutex::new(None));

        write_stores(&pending, &mut due, &mut retries, &events, false).await;

        assert!(due.is_empty());
        assert!(lock(&pending).is_empty());
    }

    #[test]
    fn queued_saves_are_latest_wins_without_extending_deadline() {
        let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
        let mut due = HashMap::new();
        let mut created = crate::playlists::Playlists::default();
        created.create("Focus").expect("playlist created");
        let mut added = created.clone();
        assert_eq!(
            added.add(
                "Focus",
                crate::api::Song::remote("id0", "Track", "Artist", "3:00")
            ),
            crate::playlists::AddResult::Added
        );

        queue_pending_save(&pending, &mut due, Snapshot::Playlists(created));
        let first_due = due[&StoreKind::Playlists];
        queue_pending_save(&pending, &mut due, Snapshot::Playlists(added));

        assert_eq!(due[&StoreKind::Playlists], first_due);
        let guard = lock(&pending);
        let Snapshot::Playlists(playlists) = guard.get(&StoreKind::Playlists).unwrap() else {
            panic!("expected playlists snapshot");
        };
        let focus = playlists.find("Focus").expect("focus playlist");
        assert_eq!(focus.songs.len(), 1);
        assert_eq!(focus.songs[0].video_id, "id0");
    }

    #[test]
    fn save_replaces_pending_without_queueing_payloads() {
        let (tx, mut rx) = crate::util::backpressure::bounded_channel(
            crate::util::backpressure::PERSIST_CONTROL_QUEUE,
        );
        let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
        let handle = PersistHandle {
            tx,
            pending: Arc::clone(&pending),
            dirty: Arc::new(Notify::new()),
            events: Arc::new(Mutex::new(None)),
        };
        let mut created = crate::playlists::Playlists::default();
        created.create("Focus").expect("playlist created");
        let mut added = created.clone();
        assert_eq!(
            added.add(
                "Focus",
                crate::api::Song::remote("id0", "Track", "Artist", "3:00")
            ),
            crate::playlists::AddResult::Added
        );

        handle.save(Snapshot::Playlists(created));
        handle.save(Snapshot::Playlists(added));

        assert!(rx.try_recv().is_err(), "save must not enqueue snapshots");
        let guard = lock(&pending);
        let Snapshot::Playlists(playlists) = guard.get(&StoreKind::Playlists).unwrap() else {
            panic!("expected playlists snapshot");
        };
        let focus = playlists.find("Focus").expect("focus playlist");
        assert_eq!(focus.songs.len(), 1);
    }

    #[tokio::test]
    async fn write_stores_requeues_failed_snapshot_and_retries_until_success() {
        let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
        let mut due = HashMap::from([(StoreKind::Config, tokio::time::Instant::now())]);
        let mut retries = HashMap::new();
        let events = Arc::new(Mutex::new(None));
        let attempts = Arc::new(AtomicUsize::new(0));
        let writer_attempts = Arc::clone(&attempts);
        lock(&pending).insert(
            StoreKind::Config,
            Snapshot::Test {
                kind: StoreKind::Config,
                label: "config",
                writer: Arc::new(move || {
                    if writer_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(std::io::Error::other("disk full"))
                    } else {
                        Ok(())
                    }
                }),
            },
        );

        let clean = write_stores(&pending, &mut due, &mut retries, &events, false).await;

        assert!(!clean);
        assert!(lock(&pending).contains_key(&StoreKind::Config));
        assert!(due.contains_key(&StoreKind::Config));
        assert_eq!(retries[&StoreKind::Config].retry_count, 1);

        let clean = write_stores(&pending, &mut due, &mut retries, &events, true).await;

        assert!(clean);
        assert!(lock(&pending).is_empty());
        assert!(!retries.contains_key(&StoreKind::Config));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_snapshot_does_not_overwrite_newer_pending_snapshot() {
        let pending: SharedPending = Arc::new(Mutex::new(HashMap::new()));
        let mut due = HashMap::new();
        let mut retries = HashMap::new();
        let events = Arc::new(Mutex::new(None));
        lock(&pending).insert(
            StoreKind::Config,
            Snapshot::Test {
                kind: StoreKind::Config,
                label: "newer",
                writer: Arc::new(|| Ok(())),
            },
        );

        requeue_failed_snapshot(
            &pending,
            &mut due,
            &mut retries,
            &events,
            Snapshot::Test {
                kind: StoreKind::Config,
                label: "older",
                writer: Arc::new(|| Ok(())),
            },
            "transient".to_owned(),
        );

        let guard = lock(&pending);
        let Snapshot::Test { label, .. } = guard.get(&StoreKind::Config).unwrap() else {
            panic!("expected test snapshot");
        };
        assert_eq!(*label, "newer");
    }

    #[tokio::test]
    async fn flush_returns_false_when_write_keeps_failing() {
        let handle = spawn();
        handle.save(Snapshot::Test {
            kind: StoreKind::Config,
            label: "config",
            writer: Arc::new(|| Err(std::io::Error::other("still full"))),
        });

        assert!(!handle.flush(Duration::from_secs(1)).await);
        assert!(lock(&handle.pending()).contains_key(&StoreKind::Config));
    }

    #[tokio::test]
    async fn first_high_value_failure_emits_one_status_event() {
        let handle = spawn();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        handle.set_event_sink(move |event| {
            captured.lock().unwrap().push(event);
        });
        handle.save(Snapshot::Test {
            kind: StoreKind::Library,
            label: "library",
            writer: Arc::new(|| Err(std::io::Error::other("permission denied"))),
        });

        assert!(!handle.flush(Duration::from_secs(1)).await);
        assert!(!handle.flush(Duration::from_secs(1)).await);

        let guard = events.lock().unwrap();
        assert_eq!(guard.len(), 1);
        let PersistEvent::WriteFailed { store, error } = &guard[0];
        assert_eq!(*store, StoreKind::Library);
        assert!(error.contains("permission denied"));
    }

    #[tokio::test]
    async fn delete_romanized_titles_removes_older_pending_save() {
        let handle = spawn();
        handle.save(Snapshot::Test {
            kind: StoreKind::RomanizedTitles,
            label: "romanized title cache",
            writer: Arc::new(|| Ok(())),
        });

        handle.delete_romanized_titles();

        assert!(
            !lock(&handle.pending()).contains_key(&StoreKind::RomanizedTitles),
            "delete must drop an older pending cache save before actor deletion"
        );
    }

    #[tokio::test]
    async fn flush_acknowledges_when_there_is_no_pending_work() {
        let handle = spawn();

        assert!(handle.flush(Duration::from_secs(1)).await);
        assert!(lock(&handle.pending()).is_empty());
    }
}
