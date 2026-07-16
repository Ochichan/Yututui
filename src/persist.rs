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

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::{Notify, mpsc, oneshot};

#[path = "persist/durable.rs"]
mod durable;
#[path = "persist/handle.rs"]
mod handle;
#[path = "persist/locking.rs"]
mod locking;
#[path = "persist/ordered_fallback.rs"]
mod ordered_fallback;
#[path = "persist/owned_snapshot.rs"]
mod owned_snapshot;
#[path = "persist/panic_ownership.rs"]
mod panic_ownership;
#[path = "persist/panic_shadow.rs"]
mod panic_shadow;
#[path = "persist/recovery.rs"]
mod recovery;
#[path = "persist/snapshot_state.rs"]
mod snapshot_state;
#[path = "persist/startup.rs"]
mod startup;
#[path = "persist/writer_lease.rs"]
mod writer_lease;
#[cfg(test)]
use durable::allocate_process_epoch_at;
use durable::{AcceptedJournalOrder, JournalGeneration, JournalOrder, JournalOrderSource};
#[cfg(test)]
pub(crate) use locking::with_intent_lock_contention_observer;
use locking::{acquire_intent_lock, acquire_intent_lock_with_budget, acquire_private_lock};
pub use ordered_fallback::PersistenceFallbackError;
use owned_snapshot::OwnedSnapshot;
pub use panic_ownership::install_panic_flush;
use panic_ownership::{
    PanicOperation, PanicOwnedOperation, lock_inflight, remove_inflight_if_order,
    retain_newest_inflight, write_panic_operation,
};
use panic_shadow::{PanicShadow, PanicShadowSealed};
#[cfg(test)]
use recovery::load_with_journal_recovery_then;
#[cfg(test)]
use recovery::replay_journaled_snapshot;
pub(crate) use recovery::{
    ConfigRecoveryGuard, ConfigRecoveryTransaction, begin_config_recovery,
    ensure_persistence_writes_allowed, load_with_journal_recovery, preflight_journal_recovery,
    remove_store_file, write_config_json, write_store_json,
};
pub use recovery::{
    StartupRecoveryError, StartupRecoveryFailure, ensure_startup_recovery_coherent,
};
#[cfg(test)]
pub(crate) use recovery::{
    clear_startup_recovery_error_for_test, latch_startup_recovery_error_for_test,
};
#[cfg(test)]
use snapshot_state::SnapshotPublication;
use snapshot_state::{
    JournalCompletion, PendingAction, PendingOperation, PendingQueue, ShadowCoveredOperation,
    SnapshotAdmission, publish_pending_batch, publish_pending_operation,
};
pub use startup::{
    StartupStoreSet, load_startup_store_set, load_verified_startup_state,
    preflight_all_startup_stores,
};
#[cfg(all(test, feature = "desktop"))]
pub(crate) use writer_lease::with_test_writer_domain;
pub use writer_lease::{
    PersistenceAccess, initialize_persistence_reader, initialize_persistence_writer,
    initialize_persistence_writer_for_roots,
};
pub(crate) use writer_lease::{persistence_access, writer_lease_allows_mutation};

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

/// Immutable snapshot of one store, taken at send time so the actor never reaches back into
/// `App`. The large app-owned stores use shared ownership; app mutations copy on write while a
/// snapshot is live. Writing delegates to the store's own `save()` — same path resolution, same
/// atomic temp-write + fsync + rename.
pub enum Snapshot {
    Library(Arc<crate::library::Library>),
    Signals(Arc<crate::signals::Signals>),
    Downloads(crate::downloads::DownloadStore),
    Config(Box<crate::config::Config>),
    Playlists(Arc<crate::playlists::Playlists>),
    Station(crate::station::StationStore),
    RomanizedTitles(crate::romanize::RomanizeCache),
    Session(crate::session::SessionCache),
    #[cfg(test)]
    Test {
        kind: StoreKind,
        label: &'static str,
        storage_path: Option<PathBuf>,
        writer: Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>,
    },
}

#[cfg(test)]
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

type SharedPending = Arc<Mutex<PendingQueue>>;
type SharedInflight = Arc<Mutex<HashMap<StoreKind, PanicOperation>>>;

const INTENT_JOURNAL_MAX_BYTES: u64 = 1024 * 1024;
const INTENT_SNAPSHOT_MAX_BYTES: u64 = 64 * 1024 * 1024;
const INTENT_ORPHAN_SCAN_LIMIT: usize = 1024;
const INTENT_SIDECAR_MAX_COUNT: usize = 64;

enum JournalIntent {
    Replace {
        order: JournalOrder,
        kind: StoreKind,
        path: PathBuf,
        bytes: Vec<u8>,
    },
    Delete {
        order: JournalOrder,
        kind: StoreKind,
        path: PathBuf,
    },
}

impl JournalIntent {
    fn order(&self) -> JournalOrder {
        match self {
            Self::Replace { order, .. } | Self::Delete { order, .. } => *order,
        }
    }
}

enum PersistMsg {
    Flush(oneshot::Sender<bool>),
    FlushTarget {
        target: PersistTarget,
        ack: oneshot::Sender<TargetFlushOutcome>,
    },
}

/// Exact persistence acceptance identity used by owner-mediated transfer commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PersistTarget {
    kind: StoreKind,
    order: JournalOrder,
}

/// Whether this exact target committed, was replaced by a newer same-store operation, or could
/// not be confirmed. Supersession is deliberately not success: the replacement may not contain
/// an owner patch which was never installed into live state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetFlushOutcome {
    CommittedExact,
    Superseded,
    Unconfirmed,
}

#[derive(Clone)]
pub struct PersistHandle {
    tx: mpsc::Sender<PersistMsg>,
    pending: SharedPending,
    inflight: SharedInflight,
    dirty: Arc<Notify>,
    events: EventSinkSlot,
    order_source: Arc<JournalOrderSource>,
    panic_shadow: Arc<PanicShadow>,
}

/// Opaque handle used by the panic hook without exposing the actor's pending-operation map.
#[derive(Clone)]
pub struct PanicPending {
    #[cfg(test)]
    inner: SharedPending,
    shadow: Arc<PanicShadow>,
}

pub fn spawn() -> PersistHandle {
    // Reserve the process epoch before this handle can accept snapshots. This one-time startup
    // fsync establishes predecessor/successor order without adding disk I/O to any user action.
    let order_source = Arc::new(match persistence_access() {
        PersistenceAccess::Writable => JournalOrderSource::allocate(),
        PersistenceAccess::ReadOnly { reason } => JournalOrderSource::unavailable(reason),
    });
    let (tx, rx) = crate::util::backpressure::bounded_channel(
        crate::util::backpressure::PERSIST_CONTROL_QUEUE,
    );
    let pending: SharedPending = Arc::new(Mutex::new(PendingQueue::new()));
    let inflight: SharedInflight = Arc::new(Mutex::new(HashMap::new()));
    let dirty = Arc::new(Notify::new());
    let events: EventSinkSlot = Arc::new(Mutex::new(None));
    let panic_shadow = Arc::new(PanicShadow::new());
    tokio::spawn(run_actor(
        rx,
        Arc::clone(&pending),
        Arc::clone(&inflight),
        Arc::clone(&dirty),
        Arc::clone(&events),
        Arc::clone(&panic_shadow),
    ));
    PersistHandle {
        tx,
        pending,
        inflight,
        dirty,
        events,
        order_source,
        panic_shadow,
    }
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
    inflight: SharedInflight,
    dirty: Arc<Notify>,
    events: EventSinkSlot,
    panic_shadow: Arc<PanicShadow>,
) {
    let mut due: HashMap<StoreKind, tokio::time::Instant> = HashMap::new();
    let mut retries: RetryMap = HashMap::new();
    let mut completions = CompletionLedger::default();
    loop {
        let next_due = due.values().min().copied();
        tokio::select! {
            // Flush requests carry caller deadlines. When both control and a coalesced dirty
            // wake are ready, service the explicit target before ordinary background journaling.
            biased;
            msg = rx.recv() => match msg {
                Some(PersistMsg::Flush(ack)) => {
                    journal_pending_operations(&pending, WriteSelection::All).await;
                    let clean = write_stores_with_inflight_tracking(
                        &pending, &inflight, &panic_shadow, &mut due, &mut retries, &events,
                        WriteSelection::All, &mut completions,
                    ).await;
                    let _ = ack.send(clean);
                }
                Some(PersistMsg::FlushTarget { target, ack }) => {
                    journal_pending_operations(&pending, WriteSelection::Store(target.kind)).await;
                    write_stores_with_inflight_tracking(
                        &pending, &inflight, &panic_shadow, &mut due, &mut retries, &events,
                        WriteSelection::Store(target.kind), &mut completions,
                    ).await;
                    let latest_accepted = lock(&pending).latest_accepted(&target.kind);
                    let _ = ack.send(completions.outcome(target, latest_accepted));
                }
                // All senders dropped (quit already flushed; this is a backstop).
                None => {
                    journal_pending_operations(&pending, WriteSelection::All).await;
                    write_stores_with_inflight_tracking(
                        &pending, &inflight, &panic_shadow, &mut due, &mut retries, &events,
                        WriteSelection::All, &mut completions,
                    ).await;
                    break;
                }
            },
            _ = dirty.notified() => {
                journal_pending_operations(&pending, WriteSelection::All).await;
                arm_due_for_pending(&pending, &mut due);
            },
            _ = tokio::time::sleep_until(next_due.unwrap_or_else(tokio::time::Instant::now)),
                if next_due.is_some() =>
            {
                write_stores_with_inflight_tracking(
                    &pending, &inflight, &panic_shadow, &mut due, &mut retries, &events,
                    WriteSelection::Due, &mut completions,
                ).await;
            }
        }
    }
}

const COMPLETION_HISTORY_PER_STORE: usize = 256;

#[derive(Default)]
struct CompletionLedger {
    frontier: HashMap<StoreKind, JournalOrder>,
    exact: HashMap<StoreKind, VecDeque<JournalOrder>>,
}

impl CompletionLedger {
    fn record(&mut self, kind: StoreKind, order: JournalOrder) {
        self.frontier
            .entry(kind)
            .and_modify(|frontier| *frontier = (*frontier).max(order))
            .or_insert(order);
        let exact = self.exact.entry(kind).or_default();
        exact.push_back(order);
        while exact.len() > COMPLETION_HISTORY_PER_STORE {
            exact.pop_front();
        }
    }

    fn outcome(
        &self,
        target: PersistTarget,
        latest_accepted: Option<JournalOrder>,
    ) -> TargetFlushOutcome {
        let frontier = self.frontier.get(&target.kind).copied();
        if latest_accepted.is_some_and(|latest| {
            latest > target.order && frontier.is_none_or(|frontier| latest > frontier)
        }) {
            TargetFlushOutcome::Unconfirmed
        } else if frontier.is_some_and(|frontier| frontier > target.order) {
            TargetFlushOutcome::Superseded
        } else if latest_accepted == Some(target.order)
            && frontier == Some(target.order)
            && self
                .exact
                .get(&target.kind)
                .is_some_and(|orders| orders.contains(&target.order))
        {
            TargetFlushOutcome::CommittedExact
        } else {
            TargetFlushOutcome::Unconfirmed
        }
    }
}

fn lock(pending: &SharedPending) -> std::sync::MutexGuard<'_, PendingQueue> {
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
    lock(pending).insert(
        kind,
        PendingOperation::save(snapshot, JournalOrderSource::for_test(1).accept()),
    );
    due.entry(kind)
        .or_insert_with(|| tokio::time::Instant::now() + debounce(kind));
}

fn arm_due_for_pending(
    pending: &SharedPending,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
) {
    let now = tokio::time::Instant::now();
    let operations: Vec<(StoreKind, Duration)> = lock(pending)
        .iter()
        .map(|(kind, operation)| (*kind, operation.debounce()))
        .collect();
    for (kind, delay) in operations {
        due.entry(kind).or_insert_with(|| now + delay);
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

async fn journal_pending_operations(pending: &SharedPending, selection: WriteSelection) {
    let intents: Vec<JournalIntent> = {
        let guard = lock(pending);
        guard
            .values()
            .filter(|operation| match selection {
                WriteSelection::Store(kind) => operation.kind() == kind,
                WriteSelection::All | WriteSelection::Due => true,
            })
            .filter(|operation| operation.publication().needs_journal())
            .filter_map(|operation| operation.journal_intent())
            .collect()
    };
    if intents.is_empty() {
        return;
    }
    let io_pending = Arc::clone(pending);
    let result = crate::util::blocking::spawn_io(move || {
        let mut written = Vec::new();
        for intent in intents {
            match write_journal_intent_if_current(&intent, &io_pending) {
                Ok(JournalAppend::Written(completion) | JournalAppend::Superseded(completion)) => {
                    written.push(completion);
                }
                Ok(JournalAppend::Stale) => {}
                Err(error) => {
                    tracing::warn!(
                        store = intent.kind().label(),
                        error = %error,
                        "failed to write persistence intent"
                    );
                }
            }
        }
        written
    })
    .await;
    match result {
        Ok(written) => {
            let mut guard = lock(pending);
            for completion in written {
                guard.resolve_journal(completion);
            }
        }
        Err(error) => tracing::warn!(error = %error, "persistence intent task failed"),
    }
}

impl JournalIntent {
    fn kind(&self) -> StoreKind {
        match self {
            Self::Replace { kind, .. } | Self::Delete { kind, .. } => *kind,
        }
    }
}

enum JournalAppend {
    Written(JournalCompletion),
    Superseded(JournalCompletion),
    Stale,
}

#[derive(Clone)]
enum JournalOperation {
    Replace { sidecar: String, sha256: String },
    Delete,
}

#[derive(Clone)]
struct JournalCandidate {
    order: Option<JournalOrder>,
    operation: JournalOperation,
    raw_line: String,
}

#[derive(Default)]
struct JournalState {
    committed_through: Option<JournalOrder>,
    candidate: Option<JournalCandidate>,
}

struct PreparedJournalRecord {
    value: serde_json::Value,
    created_sidecar: Option<PathBuf>,
}

impl PreparedJournalRecord {
    fn without_sidecar(value: serde_json::Value) -> Self {
        Self {
            value,
            created_sidecar: None,
        }
    }

    fn remove_created_sidecar(&self) {
        if let Some(path) = &self.created_sidecar {
            remove_file_if_exists(path, "failed to remove uncommitted persistence sidecar");
        }
    }
}

#[cfg(test)]
fn write_journal_intent(intent: &JournalIntent) -> std::io::Result<()> {
    ensure_persistence_writes_allowed()?;
    let (kind, path) = intent_kind_path(intent);
    let _lock = acquire_intent_lock(path)?;
    let record = prepare_journal_record(intent)?;
    let state = replace_journal_with_record_locked(kind, path, &record)?;
    verify_intent_state(&state, intent.order()).map(|_| ())
}

fn write_journal_intent_if_current(
    intent: &JournalIntent,
    pending: &SharedPending,
) -> std::io::Result<JournalAppend> {
    ensure_persistence_writes_allowed()?;
    let (kind, path) = intent_kind_path(intent);
    let _lock = acquire_intent_lock(path)?;
    let record = prepare_journal_record(intent)?;
    let current = lock(pending)
        .get(&kind)
        .is_some_and(|operation| operation.order == intent.order());
    if !current {
        record.remove_created_sidecar();
        return Ok(JournalAppend::Stale);
    }
    let state = replace_journal_with_record_locked(kind, path, &record)?;
    match verify_intent_state(&state, intent.order())? {
        IntentState::Current => Ok(JournalAppend::Written(JournalCompletion::confirmed(
            kind,
            intent.order(),
        ))),
        IntentState::Superseded => Ok(JournalAppend::Superseded(JournalCompletion::confirmed(
            kind,
            intent.order(),
        ))),
    }
}

fn intent_kind_path(intent: &JournalIntent) -> (StoreKind, &Path) {
    match intent {
        JournalIntent::Replace { kind, path, .. } | JournalIntent::Delete { kind, path, .. } => {
            (*kind, path)
        }
    }
}

fn prepare_journal_record(intent: &JournalIntent) -> std::io::Result<PreparedJournalRecord> {
    ensure_persistence_writes_allowed()?;
    let (kind, path) = intent_kind_path(intent);
    let order = intent.order();
    let generation = order.generation.to_hex();
    let process_epoch = order.process_epoch.to_string();
    let sequence = order.sequence.to_string();
    // Keep the original v1/op/kind/sidecar/sha256 shape and add ordering fields. A rolled-back
    // reader can replay immutable sidecars and simply ignores `commit` records. Concurrent old
    // and new binaries are not a supported coordination mode because the old binary does not
    // take this advisory lock. A complete generation-less record on an otherwise clean segment
    // after the ordered frontier is treated as a sequential rollback boundary.
    match intent {
        JournalIntent::Replace { bytes, .. } => {
            let sidecar_path = unique_intent_sidecar_path(path, order)
                .ok_or_else(|| std::io::Error::other("invalid persistence intent path"))?;
            let sidecar = sidecar_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| std::io::Error::other("invalid intent sidecar name"))?
                .to_owned();
            let created = write_immutable_sidecar(&sidecar_path, bytes)?;
            Ok(PreparedJournalRecord {
                value: serde_json::json!({
                    "v": 1,
                    "op": "replace",
                    "kind": kind.label(),
                    "sidecar": sidecar,
                    "sha256": sha256_hex(bytes),
                    "generation": generation,
                    "process_epoch": process_epoch,
                    "sequence": sequence,
                }),
                created_sidecar: created.then_some(sidecar_path),
            })
        }
        JournalIntent::Delete { .. } => {
            Ok(PreparedJournalRecord::without_sidecar(serde_json::json!({
                "v": 1,
                "op": "delete",
                "kind": kind.label(),
                "generation": generation,
                "process_epoch": process_epoch,
                "sequence": sequence,
            })))
        }
    }
}

fn write_immutable_sidecar(path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    if bytes.len() as u64 > INTENT_SNAPSHOT_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "persistence intent snapshot exceeds the recovery limit",
        ));
    }
    match crate::util::safe_fs::read_no_symlink_limited(path, INTENT_SNAPSHOT_MAX_BYTES) {
        Ok(existing) if existing == bytes => Ok(false),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "refusing to overwrite an immutable persistence sidecar",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure_sidecar_artifact_capacity(path)?;
            crate::util::safe_fs::write_private_atomic(path, bytes)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

fn ensure_sidecar_artifact_capacity(path: &Path) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid persistence sidecar directory"))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::other("invalid persistence sidecar name"))?;
    let (base_name, _) = file_name
        .rsplit_once(".intent.")
        .ok_or_else(|| std::io::Error::other("invalid persistence sidecar name"))?;
    let prefix = format!("{base_name}.intent.");
    let mut artifacts = 0_usize;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".json"))
        {
            artifacts += 1;
            if artifacts >= INTENT_SIDECAR_MAX_COUNT {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "persistence sidecar artifact limit reached",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn append_journal_record(path: &Path, record: &serde_json::Value) -> std::io::Result<()> {
    let Some(journal_path) = intent_journal_path(path) else {
        return Ok(());
    };
    crate::util::safe_fs::append_private_jsonl_durable(&journal_path, &record.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntentState {
    Current,
    Superseded,
}

fn verify_intent_state(state: &JournalState, order: JournalOrder) -> std::io::Result<IntentState> {
    if state
        .candidate
        .as_ref()
        .is_some_and(|candidate| candidate.order == Some(order))
    {
        return Ok(IntentState::Current);
    }
    if state
        .committed_through
        .is_some_and(|frontier| frontier >= order)
        || state.candidate.as_ref().is_some_and(|candidate| {
            candidate
                .order
                .is_none_or(|candidate_order| candidate_order > order)
        })
    {
        return Ok(IntentState::Superseded);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "persistence journal did not retain or supersede the appended generation",
    ))
}

fn replace_journal_with_record_locked(
    kind: StoreKind,
    path: &Path,
    record: &PreparedJournalRecord,
) -> std::io::Result<JournalState> {
    ensure_persistence_writes_allowed()?;
    replace_journal_with_record_locked_by(kind, path, record, |journal_path, bytes| {
        crate::util::safe_fs::write_private_atomic(journal_path, bytes)
    })
}

fn replace_journal_with_record_locked_by<F>(
    kind: StoreKind,
    path: &Path,
    record: &PreparedJournalRecord,
    writer: F,
) -> std::io::Result<JournalState>
where
    F: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
{
    let Some(journal_path) = intent_journal_path(path) else {
        return Ok(JournalState::default());
    };
    let current = match read_journal_state(kind, path) {
        Ok(current) => current,
        Err(error) => {
            // No journal mutation has been attempted yet, so the newly prepared sidecar cannot
            // be referenced and is always safe to remove.
            record.remove_created_sidecar();
            return Err(error);
        }
    };
    let mut proposed = compacted_journal_text(kind, &current);
    proposed.push_str(&record.value.to_string());
    proposed.push('\n');
    let desired_state = parse_journal_state(kind, &proposed);
    let desired = compacted_journal_text(kind, &desired_state);
    if desired.len() as u64 > INTENT_JOURNAL_MAX_BYTES {
        cleanup_journal_sidecars_locked(path, &current);
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "compacted persistence journal exceeds its size limit",
        ));
    }

    let write_result = writer(&journal_path, desired.as_bytes());
    let observed = read_journal_state(kind, path);
    if let Ok(observed_state) = &observed {
        if write_result.is_ok() {
            cleanup_journal_sidecars_locked(path, observed_state);
        } else {
            // A failed atomic write may be pre-rename, or the rename may be visible while its
            // directory sync remains uncertain. Preserve the payloads referenced by both the old
            // durable possibility and the newly observed state until a later confirmed write.
            cleanup_journal_sidecars_after_ambiguous_write_locked(path, &current, observed_state);
        }
    }
    write_result?;
    observed
}

fn compacted_journal_text(kind: StoreKind, state: &JournalState) -> String {
    let mut compacted = String::new();
    if let Some(frontier) = state.committed_through {
        compacted.push_str(&commit_record(kind, frontier).to_string());
        compacted.push('\n');
    }
    if let Some(candidate) = &state.candidate {
        compacted.push_str(&candidate.raw_line);
        compacted.push('\n');
    }
    compacted
}

fn cleanup_journal_sidecars_locked(path: &Path, state: &JournalState) {
    let retained = candidate_sidecar(state);
    cleanup_orphan_sidecars_locked(path, retained);
}

fn cleanup_journal_sidecars_after_ambiguous_write_locked(
    path: &Path,
    previous: &JournalState,
    observed: &JournalState,
) {
    match (candidate_sidecar(previous), candidate_sidecar(observed)) {
        (Some(previous), Some(observed)) if previous != observed => {
            cleanup_orphan_sidecars_locked_retaining(path, &[previous, observed]);
        }
        (Some(retained), _) | (_, Some(retained)) => {
            cleanup_orphan_sidecars_locked(path, Some(retained));
        }
        (None, None) => cleanup_orphan_sidecars_locked(path, None),
    }
}

fn candidate_sidecar(state: &JournalState) -> Option<&str> {
    state.candidate.as_ref().and_then(|candidate| {
        if let JournalOperation::Replace { sidecar, .. } = &candidate.operation {
            Some(sidecar.as_str())
        } else {
            None
        }
    })
}

#[cfg(test)]
fn commit_journal_generation(
    kind: StoreKind,
    path: &Path,
    order: JournalOrder,
) -> std::io::Result<()> {
    let _lock = acquire_intent_lock(path)?;
    commit_journal_generation_locked(kind, path, order)
}

fn commit_journal_generation_locked(
    kind: StoreKind,
    path: &Path,
    order: JournalOrder,
) -> std::io::Result<()> {
    ensure_persistence_writes_allowed()?;
    let record = PreparedJournalRecord::without_sidecar(commit_record(kind, order));
    let state = replace_journal_with_record_locked(kind, path, &record)?;
    if state
        .committed_through
        .is_some_and(|frontier| frontier >= order)
    {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "persistence journal did not retain the committed generation frontier",
        ))
    }
}

fn commit_record(kind: StoreKind, order: JournalOrder) -> serde_json::Value {
    serde_json::json!({
        "v": 1,
        "op": "commit",
        "kind": kind.label(),
        "generation": order.generation.to_hex(),
        "process_epoch": order.process_epoch.to_string(),
        "sequence": order.sequence.to_string(),
    })
}

#[cfg(test)]
fn clear_store_journal(path: &Path) {
    let Ok(_lock) = acquire_intent_lock(path) else {
        return;
    };
    clear_store_journal_locked(path);
}

#[cfg(test)]
fn clear_store_journal_locked(path: &Path) {
    if let Some(journal_path) = intent_journal_path(path) {
        remove_file_if_exists(&journal_path, "failed to remove persistence intent");
    }
    cleanup_orphan_sidecars_locked(path, None);
}

#[cfg(test)]
pub(crate) fn write_test_journaled_snapshot(
    kind: StoreKind,
    path: PathBuf,
    bytes: Vec<u8>,
) -> std::io::Result<()> {
    let accepted = JournalOrderSource::allocate().accept();
    if let Some(error) = accepted.error {
        return Err(std::io::Error::other(error.to_string()));
    }
    write_journal_intent(&JournalIntent::Replace {
        order: accepted.order,
        kind,
        path,
        bytes,
    })
}

#[cfg(test)]
pub(crate) fn test_journal_exists(path: &Path) -> bool {
    match read_journal_state(StoreKind::Config, path) {
        Ok(state) => state.candidate.is_some(),
        // An unreadable journal is still an unsettled recovery artifact.
        Err(_) => intent_journal_path(path).is_some_and(|journal| journal.exists()),
    }
}

fn intent_journal_path(path: &Path) -> Option<PathBuf> {
    sibling_with_suffix(path, ".intent.jsonl")
}

#[cfg(test)]
fn intent_sidecar_path(path: &Path) -> Option<PathBuf> {
    sibling_with_suffix(path, ".intent.latest.json")
}

fn unique_intent_sidecar_path(path: &Path, order: JournalOrder) -> Option<PathBuf> {
    sibling_with_suffix(
        path,
        &format!(
            ".intent.{}.{}.{}.json",
            order.process_epoch,
            order.sequence,
            order.generation.to_hex()
        ),
    )
}

fn intent_lock_path(path: &Path) -> Option<PathBuf> {
    sibling_with_suffix(path, ".intent.lock")
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

fn parse_journal_order(record: &serde_json::Value) -> Option<JournalOrder> {
    let generation = JournalGeneration::from_hex(record.get("generation")?.as_str()?)?;
    if let (Some(process_epoch), Some(sequence)) =
        (record.get("process_epoch"), record.get("sequence"))
    {
        let process_epoch = process_epoch
            .as_str()
            .and_then(|value| value.parse().ok())
            .or_else(|| process_epoch.as_u64())?;
        let sequence = sequence
            .as_str()
            .and_then(|value| value.parse().ok())
            .or_else(|| sequence.as_u64().map(u128::from))?;
        return Some(JournalOrder {
            process_epoch,
            sequence,
            generation,
        });
    }
    // Transitional development records used wall-clock nanos. Keep them replayable below every
    // durable process epoch; no released reader depended on this ordering field.
    let accepted = record.get("accepted_unix_nanos")?;
    let sequence = accepted
        .as_str()
        .and_then(|value| value.parse().ok())
        .or_else(|| accepted.as_u64().map(u128::from))?;
    Some(JournalOrder {
        process_epoch: 0,
        sequence,
        generation,
    })
}

enum ParsedJournalEvent {
    Commit(JournalOrder),
    Candidate(JournalCandidate),
}

fn parse_journal_event(kind: StoreKind, line: &str) -> Option<ParsedJournalEvent> {
    let record = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if record.get("v").and_then(|value| value.as_u64()) != Some(1)
        || record.get("kind").and_then(|value| value.as_str()) != Some(kind.label())
    {
        return None;
    }
    let order = parse_journal_order(&record);
    let declares_order = [
        "generation",
        "process_epoch",
        "sequence",
        "accepted_unix_nanos",
    ]
    .iter()
    .any(|field| record.get(field).is_some());
    if declares_order && order.is_none() {
        return None;
    }
    let operation = match record.get("op").and_then(|value| value.as_str()) {
        Some("commit") => return order.map(ParsedJournalEvent::Commit),
        Some("replace") => {
            let sidecar = record.get("sidecar")?.as_str()?.to_owned();
            let sha256 = record.get("sha256")?.as_str()?.to_owned();
            JournalOperation::Replace { sidecar, sha256 }
        }
        Some("delete") => JournalOperation::Delete,
        _ => return None,
    };
    Some(ParsedJournalEvent::Candidate(JournalCandidate {
        order,
        operation,
        raw_line: line.to_owned(),
    }))
}

fn parse_journal_state(kind: StoreKind, text: &str) -> JournalState {
    let mut committed_through = None;
    let mut latest_ordered: Option<JournalCandidate> = None;
    let mut latest_legacy: Option<(usize, JournalCandidate)> = None;
    let mut latest_ordered_line = None;
    let mut invalid_after_latest_ordered = false;
    for (line_index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let Some(event) = parse_journal_event(kind, line) else {
            if latest_ordered_line.is_some() {
                invalid_after_latest_ordered = true;
            }
            continue;
        };
        match event {
            ParsedJournalEvent::Commit(order) => {
                latest_ordered_line = Some(line_index);
                invalid_after_latest_ordered = false;
                committed_through = Some(
                    committed_through.map_or(order, |current: JournalOrder| current.max(order)),
                );
            }
            ParsedJournalEvent::Candidate(candidate) => {
                if let Some(order) = candidate.order {
                    latest_ordered_line = Some(line_index);
                    invalid_after_latest_ordered = false;
                    if latest_ordered
                        .as_ref()
                        .and_then(|current| current.order)
                        .is_none_or(|current| order > current)
                    {
                        latest_ordered = Some(candidate);
                    }
                } else {
                    latest_legacy = Some((line_index, candidate));
                }
            }
        }
    }
    let ordered_candidate = latest_ordered.filter(|candidate| {
        committed_through.is_none_or(|frontier| candidate.order.is_some_and(|o| o > frontier))
    });
    // Generation-less v1 records cannot participate in epoch ordering. A *complete* legacy line
    // physically after the last ordered event is nevertheless a machine-checkable sequential
    // rollback boundary: the old binary ran after the new writer stopped. Concurrent old/new
    // writers remain unsupported because old binaries do not acquire the journal lock.
    let later_legacy = latest_legacy.and_then(|(line_index, candidate)| {
        (latest_ordered_line.is_none_or(|ordered_line| line_index > ordered_line)
            && !invalid_after_latest_ordered)
            .then_some(candidate)
    });
    let candidate = later_legacy.or(ordered_candidate);
    JournalState {
        committed_through,
        candidate,
    }
}

fn read_journal_state(kind: StoreKind, path: &Path) -> std::io::Result<JournalState> {
    let Some(journal_path) = intent_journal_path(path) else {
        return Ok(JournalState::default());
    };
    let bytes = match crate::util::safe_fs::read_no_symlink_limited(
        &journal_path,
        INTENT_JOURNAL_MAX_BYTES,
    ) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalState::default());
        }
        Err(error) => return Err(error),
    };
    let text = String::from_utf8_lossy(&bytes);
    Ok(parse_journal_state(kind, &text))
}

fn cleanup_orphan_sidecars_locked(path: &Path, retained: Option<&str>) {
    match retained {
        Some(retained) => cleanup_orphan_sidecars_locked_retaining(path, &[retained]),
        None => cleanup_orphan_sidecars_locked_retaining(path, &[]),
    }
}

fn cleanup_orphan_sidecars_locked_retaining(path: &Path, retained: &[&str]) {
    let (Some(parent), Some(name)) = (
        path.parent(),
        path.file_name().and_then(|value| value.to_str()),
    ) else {
        return;
    };
    let prefix = format!("{name}.intent.");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let mut inspected = 0_usize;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if !file_name.starts_with(&prefix) || !file_name.ends_with(".json") {
            continue;
        }
        inspected += 1;
        if inspected > INTENT_ORPHAN_SCAN_LIMIT {
            break;
        }
        if retained.contains(&file_name) {
            continue;
        }
        remove_file_if_exists(
            &entry.path(),
            "failed to remove obsolete persistence sidecar",
        );
    }
}

fn remove_file_if_exists(path: &Path, message: &'static str) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), error = %error, message),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn write_operation_durable(operation: &PendingOperation) -> std::io::Result<()> {
    write_operation_durable_using(operation, || operation.write())
}

fn write_operation_durable_using(
    operation: &PendingOperation,
    write: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    ensure_persistence_writes_allowed()?;
    let Some(path) = operation.storage_path() else {
        return write();
    };
    operation.publication().ensure_ordering()?;
    let kind = operation.kind();
    let _lock = acquire_intent_lock(&path)?;
    if operation.publication().needs_journal() {
        let intent = operation
            .journal_intent()
            .ok_or_else(|| std::io::Error::other("failed to prepare persistence journal intent"))?;
        let record = prepare_journal_record(&intent)?;
        let state = replace_journal_with_record_locked(kind, &path, &record)?;
        if verify_intent_state(&state, operation.order)? == IntentState::Superseded {
            return Ok(());
        }
    }
    let state = read_journal_state(kind, &path)?;
    if verify_intent_state(&state, operation.order)? == IntentState::Superseded {
        // A newer accepted operation (possibly from another process) superseded this one.
        // Treat it as settled without touching the store; its unique sidecar was discarded
        // by compaction only after the newer record became durable.
        return Ok(());
    }
    write()?;
    commit_journal_generation_locked(kind, &path, operation.order)?;
    Ok(())
}

/// Run the durable write with the writer's panics downgraded to `io::Error` so the persist actor
/// survives them and keeps serving later saves. Containment exists only in unwind builds
/// (dev/test); release builds use `panic = "abort"`, where a writer panic aborts the process
/// before reaching the `unwrap_or_else` below — there it documents intent, not a guarantee.
fn write_operation_caught(operation: &PendingOperation) -> std::io::Result<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        write_operation_durable(operation)
    }))
    .unwrap_or_else(|payload| {
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown panic");
        Err(std::io::Error::other(format!(
            "persistence writer panicked: {message}"
        )))
    })
}

fn requeue_failed_operation(
    pending: &SharedPending,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    retries: &mut RetryMap,
    events: &EventSinkSlot,
    operation: ShadowCoveredOperation,
    error: String,
) {
    let kind = operation.kind();
    let label = operation.label();
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
    if guard.contains_key(&kind) {
        tracing::warn!(
            error = %last_error,
            retry_count,
            retry_in_ms,
            failure_age_ms,
            "failed to save {label}; newer snapshot is already pending"
        );
    } else {
        guard.insert_owned(operation);
        due.insert(kind, retry_at);
        tracing::warn!(
            error = %last_error,
            retry_count,
            retry_in_ms,
            failure_age_ms,
            "failed to save {label}; retry scheduled"
        );
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

fn schedule_retained_failure(
    kind: StoreKind,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    retries: &mut RetryMap,
    events: &EventSinkSlot,
    error: String,
) {
    let sanitized = crate::util::sanitize::sanitize_error_text(error);
    let state = retries.entry(kind).or_default();
    let first_failure = state.retry_count == 0;
    state.retry_count = state.retry_count.saturating_add(1);
    let retry_at = tokio::time::Instant::now() + retry_delay(state.retry_count);
    state.retry_not_before = Some(retry_at);
    state.last_error = Some(sanitized.clone());
    state.last_failed_at = Some(tokio::time::Instant::now());
    due.insert(kind, retry_at);
    tracing::warn!(store = kind.label(), error = %sanitized, "persistence ownership remains pending");
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

#[cfg(test)]
async fn write_stores(
    pending: &SharedPending,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    retries: &mut RetryMap,
    events: &EventSinkSlot,
    all: bool,
) -> bool {
    let inflight = Arc::new(Mutex::new(HashMap::new()));
    let panic_shadow = PanicShadow::new();
    write_stores_with_inflight(pending, &inflight, &panic_shadow, due, retries, events, all).await
}

/// Apply every due operation. Before ownership leaves `pending`, an immutable equivalent is
/// published to `inflight`; the panic hook can therefore recover it during blocking-pool waits.
#[cfg(test)]
async fn write_stores_with_inflight(
    pending: &SharedPending,
    inflight: &SharedInflight,
    panic_shadow: &PanicShadow,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    retries: &mut RetryMap,
    events: &EventSinkSlot,
    all: bool,
) -> bool {
    let mut completions = CompletionLedger::default();
    write_stores_with_inflight_tracking(
        pending,
        inflight,
        panic_shadow,
        due,
        retries,
        events,
        if all {
            WriteSelection::All
        } else {
            WriteSelection::Due
        },
        &mut completions,
    )
    .await
}

#[derive(Clone, Copy)]
enum WriteSelection {
    All,
    Due,
    Store(StoreKind),
}

#[allow(clippy::too_many_arguments)]
async fn write_stores_with_inflight_tracking(
    pending: &SharedPending,
    inflight: &SharedInflight,
    panic_shadow: &PanicShadow,
    due: &mut HashMap<StoreKind, tokio::time::Instant>,
    retries: &mut RetryMap,
    events: &EventSinkSlot,
    selection: WriteSelection,
    completions: &mut CompletionLedger,
) -> bool {
    let now = tokio::time::Instant::now();
    let kinds: Vec<StoreKind> = match selection {
        WriteSelection::All => {
            // Everything dirty, whether or not its deadline armed (flush/backstop path).
            lock(pending).keys().copied().collect()
        }
        WriteSelection::Due => due
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(kind, _)| *kind)
            .collect(),
        WriteSelection::Store(kind) => lock(pending)
            .contains_key(&kind)
            .then_some(kind)
            .into_iter()
            .collect(),
    };
    for kind in kinds {
        due.remove(&kind);
        let prepared = {
            let guard = lock(pending);
            let Some(operation) = guard.get(&kind) else {
                continue;
            };
            match operation.panic_operation() {
                Ok(prepared) => prepared,
                Err(error) => {
                    drop(guard);
                    schedule_retained_failure(
                        kind,
                        due,
                        retries,
                        events,
                        format!("failed to prepare panic-safe persistence ownership: {error}"),
                    );
                    continue;
                }
            }
        };
        let order = prepared.order;
        match panic_shadow.publish(PanicOwnedOperation::Prepared(Arc::new(prepared.clone()))) {
            Ok(()) => {}
            Err(PanicShadowSealed) => {
                // Admission published the Pending form before the panic boundary, so the
                // hook's sealed snapshot still owns this operation while the actor continues.
            }
        }
        retain_newest_inflight(inflight, prepared);
        let operation = {
            let mut guard = lock(pending);
            if guard
                .get(&kind)
                .is_none_or(|operation| operation.order != order)
            {
                remove_inflight_if_order(inflight, kind, order);
                continue;
            }
            guard
                .remove(&kind)
                .expect("persistence order was rechecked")
        };
        let result = crate::util::blocking::spawn_io(move || {
            let outcome = write_operation_caught(&operation);
            (outcome, operation)
        })
        .await;
        match result {
            Ok((Err(error), operation)) => {
                requeue_failed_operation(
                    pending,
                    due,
                    retries,
                    events,
                    operation,
                    error.to_string(),
                );
                remove_inflight_if_order(inflight, kind, order);
            }
            Err(error) => {
                tracing::warn!(
                    store = kind.label(),
                    error = %error,
                    "persist write task failed; panic recovery retains ownership"
                );
            }
            Ok((Ok(()), operation)) => {
                remove_inflight_if_order(inflight, kind, order);
                panic_shadow.clear_through(kind, order);
                retries.remove(&operation.kind());
                completions.record(kind, order);
            }
        }
    }
    lock(pending).is_empty() && lock_inflight(inflight).is_empty()
}

#[cfg(test)]
#[path = "persist/tests.rs"]
mod tests;
