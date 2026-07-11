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
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
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

    fn storage_path(&self) -> Option<PathBuf> {
        match self {
            Snapshot::Library(_) => crate::library::library_path(),
            Snapshot::Signals(_) => crate::signals::signals_path(),
            Snapshot::Downloads(_) => crate::downloads::store_path(),
            Snapshot::Config(_) => crate::config::config_path(),
            Snapshot::Playlists(_) => crate::playlists::playlists_path(),
            Snapshot::Station(_) => crate::station::station_path(),
            Snapshot::RomanizedTitles(_) => crate::romanize::cache_path(),
            Snapshot::Session(_) => crate::session::session_cache_path(),
            #[cfg(test)]
            Snapshot::Test { .. } => None,
        }
    }

    fn to_json_bytes(&self) -> serde_json::Result<Vec<u8>> {
        match self {
            Snapshot::Library(s) => serde_json::to_vec_pretty(s),
            Snapshot::Signals(s) => serde_json::to_vec_pretty(s),
            Snapshot::Downloads(s) => serde_json::to_vec_pretty(s),
            Snapshot::Config(s) => serde_json::to_vec_pretty(s),
            Snapshot::Playlists(s) => serde_json::to_vec_pretty(s),
            Snapshot::Station(s) => serde_json::to_vec_pretty(s),
            Snapshot::RomanizedTitles(s) => serde_json::to_vec_pretty(s),
            Snapshot::Session(s) => serde_json::to_vec_pretty(s),
            #[cfg(test)]
            Snapshot::Test { .. } => serde_json::to_vec(&serde_json::Value::Null),
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

const INTENT_JOURNAL_MAX_BYTES: u64 = 1024 * 1024;
const INTENT_SNAPSHOT_MAX_BYTES: u64 = 64 * 1024 * 1024;

struct JournalIntent {
    kind: StoreKind,
    path: PathBuf,
    bytes: Vec<u8>,
}

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
                    clear_store_journal_for_kind(StoreKind::RomanizedTitles);
                    let result = crate::util::blocking::spawn_io(
                        crate::romanize::RomanizeCache::delete_saved,
                    )
                    .await;
                    if let Ok(Err(e)) = result {
                        tracing::warn!(error = %e, "failed to delete romanized title cache");
                    }
                }
                Some(PersistMsg::Flush(ack)) => {
                    journal_pending_snapshots(&pending).await;
                    let clean = write_stores(&pending, &mut due, &mut retries, &events, true).await;
                    let _ = ack.send(clean);
                }
                // All senders dropped (quit already flushed; this is a backstop).
                None => {
                    journal_pending_snapshots(&pending).await;
                    write_stores(&pending, &mut due, &mut retries, &events, true).await;
                    break;
                }
            },
            _ = dirty.notified() => {
                journal_pending_snapshots(&pending).await;
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

async fn journal_pending_snapshots(pending: &SharedPending) {
    let intents: Vec<JournalIntent> = {
        let guard = lock(pending);
        guard
            .values()
            .filter_map(|snapshot| {
                let path = snapshot.storage_path()?;
                let bytes = match snapshot.to_json_bytes() {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(
                            store = snapshot.kind().label(),
                            error = %error,
                            "failed to encode persistence intent"
                        );
                        return None;
                    }
                };
                Some(JournalIntent {
                    kind: snapshot.kind(),
                    path,
                    bytes,
                })
            })
            .collect()
    };
    if intents.is_empty() {
        return;
    }
    let result = crate::util::blocking::spawn_io(move || {
        for intent in intents {
            if let Err(error) = write_journal_intent(&intent) {
                tracing::warn!(
                    store = intent.kind.label(),
                    error = %error,
                    "failed to write persistence intent"
                );
            }
        }
    })
    .await;
    if let Err(error) = result {
        tracing::warn!(error = %error, "persistence intent task failed");
    }
}

fn write_journal_intent(intent: &JournalIntent) -> std::io::Result<()> {
    let Some(journal_path) = intent_journal_path(&intent.path) else {
        return Ok(());
    };
    let Some(sidecar_path) = intent_sidecar_path(&intent.path) else {
        return Ok(());
    };
    let sidecar = sidecar_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::other("invalid intent sidecar name"))?;
    let record = journal_record(intent.kind, sidecar, &intent.bytes);
    // Append the durable record before replacing the shared latest sidecar. If the process dies
    // between these two operations, replay skips the new record's checksum mismatch and can still
    // use the previous record with the previous sidecar. Replacing the sidecar first would make
    // every older committed record mismatch during that crash window.
    crate::util::safe_fs::append_private_jsonl_durable(&journal_path, &record.to_string())?;
    crate::util::safe_fs::write_private_atomic(&sidecar_path, &intent.bytes)
}

fn journal_record(kind: StoreKind, sidecar: &str, bytes: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "v": 1,
        "op": "replace",
        "kind": kind.label(),
        "sidecar": sidecar,
        "sha256": sha256_hex(bytes),
    })
}

fn clear_store_journal_for_kind(kind: StoreKind) {
    let path = match kind {
        StoreKind::Library => crate::library::library_path(),
        StoreKind::Signals => crate::signals::signals_path(),
        StoreKind::Downloads => crate::downloads::store_path(),
        StoreKind::Config => crate::config::config_path(),
        StoreKind::Playlists => crate::playlists::playlists_path(),
        StoreKind::Station => crate::station::station_path(),
        StoreKind::RomanizedTitles => crate::romanize::cache_path(),
        StoreKind::Session => crate::session::session_cache_path(),
    };
    if let Some(path) = path {
        clear_store_journal(&path);
    }
}

fn clear_store_journal(path: &Path) {
    for path in [intent_journal_path(path), intent_sidecar_path(path)]
        .into_iter()
        .flatten()
    {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "failed to remove persistence intent"
                );
            }
        }
    }
}

pub(crate) fn replay_journaled_snapshot<T>(
    kind: StoreKind,
    path: &Path,
    current: T,
    max_bytes: u64,
) -> T
where
    T: DeserializeOwned,
{
    replay_journaled_snapshot_with_status(kind, path, current, max_bytes).0
}

/// Replay the newest valid intent and report whether the returned value came from its sidecar.
/// Config recovery uses the status to atomically install a migrated replay before clearing the
/// journal; all other stores keep the existing value-only interface above.
pub(crate) fn replay_journaled_snapshot_with_status<T>(
    kind: StoreKind,
    path: &Path,
    current: T,
    max_bytes: u64,
) -> (T, bool)
where
    T: DeserializeOwned,
{
    let Some(journal_path) = intent_journal_path(path) else {
        return (current, false);
    };
    let Ok(bytes) =
        crate::util::safe_fs::read_no_symlink_limited(&journal_path, INTENT_JOURNAL_MAX_BYTES)
    else {
        return (current, false);
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return (current, false);
    };
    for line in text.lines().rev() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if record.get("v").and_then(|v| v.as_u64()) != Some(1)
            || record.get("op").and_then(|v| v.as_str()) != Some("replace")
            || record.get("kind").and_then(|v| v.as_str()) != Some(kind.label())
        {
            continue;
        }
        let Some(sidecar_name) = record.get("sidecar").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(expected_hash) = record.get("sha256").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(sidecar_path) = sibling_path_from_record(path, sidecar_name) else {
            continue;
        };
        let cap = max_bytes.min(INTENT_SNAPSHOT_MAX_BYTES);
        let Ok(snapshot_bytes) = crate::util::safe_fs::read_no_symlink_limited(&sidecar_path, cap)
        else {
            continue;
        };
        if sha256_hex(&snapshot_bytes) != expected_hash {
            tracing::warn!(
                store = kind.label(),
                "discarding persistence intent with checksum mismatch"
            );
            continue;
        }
        match serde_json::from_slice::<T>(&snapshot_bytes) {
            Ok(snapshot) => {
                tracing::info!(store = kind.label(), "replayed pending persistence intent");
                return (snapshot, true);
            }
            Err(error) => {
                tracing::warn!(
                    store = kind.label(),
                    error = %error,
                    "discarding invalid persistence intent"
                );
            }
        }
    }
    (current, false)
}

/// Remove a replayed intent only after its snapshot has been successfully installed at `path`.
pub(crate) fn clear_journaled_snapshot(path: &Path) {
    clear_store_journal(path);
}

#[cfg(test)]
pub(crate) fn write_test_journaled_snapshot(
    kind: StoreKind,
    path: PathBuf,
    bytes: Vec<u8>,
) -> std::io::Result<()> {
    write_journal_intent(&JournalIntent { kind, path, bytes })
}

#[cfg(test)]
pub(crate) fn test_journal_exists(path: &Path) -> bool {
    intent_journal_path(path).is_some_and(|journal| journal.exists())
        || intent_sidecar_path(path).is_some_and(|sidecar| sidecar.exists())
}

fn intent_journal_path(path: &Path) -> Option<PathBuf> {
    sibling_with_suffix(path, ".intent.jsonl")
}

fn intent_sidecar_path(path: &Path) -> Option<PathBuf> {
    sibling_with_suffix(path, ".intent.latest.json")
}

fn sibling_with_suffix(path: &Path, suffix: &str) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    Some(path.with_file_name(format!("{name}{suffix}")))
}

fn sibling_path_from_record(path: &Path, name: &str) -> Option<PathBuf> {
    let recorded = Path::new(name);
    if recorded.file_name().and_then(|file| file.to_str()) != Some(name) {
        return None;
    }
    path.parent().map(|parent| parent.join(name))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
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
        let result = crate::util::blocking::spawn_io(move || {
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
                if let Some(path) = snapshot.storage_path() {
                    clear_store_journal(&path);
                }
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

    use serde::{Deserialize, Serialize};

    fn temp_dir(name: &str) -> PathBuf {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        let suffix = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        std::env::temp_dir().join(format!(
            "yututui-persist-{name}-{}-{suffix}",
            std::process::id()
        ))
    }

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

    #[test]
    fn journaled_snapshot_replays_and_clears() {
        #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
        struct Tiny {
            value: u8,
        }

        let dir = temp_dir("intent");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.json");
        let bytes = serde_json::to_vec_pretty(&Tiny { value: 7 }).unwrap();

        write_journal_intent(&JournalIntent {
            kind: StoreKind::Config,
            path: path.clone(),
            bytes,
        })
        .unwrap();

        let (replayed, from_journal) = replay_journaled_snapshot_with_status(
            StoreKind::Config,
            &path,
            Tiny { value: 1 },
            1024,
        );
        assert_eq!(replayed, Tiny { value: 7 });
        assert!(from_journal);
        assert!(test_journal_exists(&path));

        clear_journaled_snapshot(&path);
        let (replayed, from_journal) = replay_journaled_snapshot_with_status(
            StoreKind::Config,
            &path,
            Tiny { value: 1 },
            1024,
        );
        assert_eq!(replayed, Tiny { value: 1 });
        assert!(!from_journal);
        assert!(!test_journal_exists(&path));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sidecar_replacement_crash_windows_keep_the_last_committed_intent() {
        #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
        struct Tiny {
            value: u8,
        }

        let dir = temp_dir("intent-replace-order");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.json");
        let old_bytes = serde_json::to_vec_pretty(&Tiny { value: 7 }).unwrap();
        write_journal_intent(&JournalIntent {
            kind: StoreKind::Config,
            path: path.clone(),
            bytes: old_bytes,
        })
        .unwrap();

        let journal_path = intent_journal_path(&path).unwrap();
        let sidecar_path = intent_sidecar_path(&path).unwrap();
        let sidecar_name = sidecar_path.file_name().unwrap().to_str().unwrap();
        let new_bytes = serde_json::to_vec_pretty(&Tiny { value: 9 }).unwrap();
        let new_record = journal_record(StoreKind::Config, sidecar_name, &new_bytes);

        // Crash after the new record is durable but before its sidecar replacement: the newest
        // checksum mismatches, so replay must fall back to the older committed pair.
        crate::util::safe_fs::append_private_jsonl_durable(&journal_path, &new_record.to_string())
            .unwrap();
        let (replayed, from_journal) = replay_journaled_snapshot_with_status(
            StoreKind::Config,
            &path,
            Tiny { value: 1 },
            1024,
        );
        assert!(from_journal);
        assert_eq!(replayed, Tiny { value: 7 });

        // Once the atomic sidecar replacement lands, the same durable record becomes the newest
        // valid pair. Replaying it repeatedly is safe until the installed store is cleared.
        crate::util::safe_fs::write_private_atomic(&sidecar_path, &new_bytes).unwrap();
        for _ in 0..2 {
            let (replayed, from_journal) = replay_journaled_snapshot_with_status(
                StoreKind::Config,
                &path,
                Tiny { value: 1 },
                1024,
            );
            assert!(from_journal);
            assert_eq!(replayed, Tiny { value: 9 });
        }

        clear_journaled_snapshot(&path);
        let _ = std::fs::remove_dir_all(dir);
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
