use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Serialize, de::DeserializeOwned};

use super::{
    INTENT_SNAPSHOT_MAX_BYTES, JournalCandidate, JournalOperation, JournalOrder, StoreKind,
    acquire_intent_lock_with_budget, read_journal_state, sha256_hex, sibling_path_from_record,
};

const COHERENT_LOAD_LOCK_TOTAL_BUDGET: Duration = Duration::from_secs(15);
const COHERENT_LOAD_LOCK_ATTEMPT_BUDGET: Duration = Duration::from_secs(5);

/// Coherent ownership for one config load, acquired before the base file is inspected.
///
/// Config recovery can back up an invalid base, import a legacy file, migrate raw JSON, and write
/// the resulting snapshot. Keeping this guard alive across that entire sequence prevents a base
/// value read before a concurrent writer's commit from being installed after that newer commit.
pub(crate) struct ConfigRecoveryGuard {
    state: ConfigRecoveryGuardState,
}

enum ConfigRecoveryGuardState {
    Coherent {
        path: PathBuf,
        lock: crate::util::safe_fs::AdvisoryFileLock,
    },
    ReadOnly {
        path: PathBuf,
    },
}

enum ConfigRecoveryReceipt {
    Ordered {
        order: JournalOrder,
        raw_line: String,
    },
    Legacy {
        raw_line: String,
    },
}

/// Raw config plus the exact journal candidate that produced it, while coherent ownership is
/// still held. Installation consumes the transaction so settlement cannot be performed twice.
pub(crate) struct ConfigRecoveryTransaction {
    state: ConfigRecoveryTransactionState,
}

enum ConfigRecoveryTransactionState {
    Coherent {
        path: PathBuf,
        lock: crate::util::safe_fs::AdvisoryFileLock,
        value: serde_json::Value,
        receipt: Option<ConfigRecoveryReceipt>,
    },
    ReadOnly {
        value: serde_json::Value,
    },
}

#[derive(Debug)]
enum RecoveryLockError {
    ContentionDeadline {
        store: StoreKind,
        attempts: u64,
        budget: Duration,
    },
    Io {
        store: StoreKind,
        error: std::io::Error,
    },
}

impl std::fmt::Display for RecoveryLockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContentionDeadline {
                store,
                attempts,
                budget,
            } => write!(
                formatter,
                "timed out loading {} after {attempts} lock attempts over {budget:?}",
                store.label()
            ),
            Self::Io { error, .. } => error.fmt(formatter),
        }
    }
}

impl std::error::Error for RecoveryLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ContentionDeadline { .. } => None,
            Self::Io { error, .. } => Some(error),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupRecoveryFailure {
    ContentionDeadline {
        attempts: u64,
        budget: Duration,
    },
    LockFailure {
        kind: std::io::ErrorKind,
        error: String,
    },
    ArtifactUnverifiable {
        kind: std::io::ErrorKind,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupRecoveryError {
    pub store: StoreKind,
    pub failure: StartupRecoveryFailure,
}

impl std::fmt::Display for StartupRecoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "startup refused to continue after {} recovery lock failure: ",
            self.store.label()
        )?;
        match &self.failure {
            StartupRecoveryFailure::ContentionDeadline { attempts, budget } => write!(
                formatter,
                "another writer remained active for {budget:?} ({attempts} attempts); refusing to overwrite its newer state"
            ),
            StartupRecoveryFailure::LockFailure { error, .. } => write!(
                formatter,
                "{error}; refusing to run without coherent persistence ownership"
            ),
            StartupRecoveryFailure::ArtifactUnverifiable { error, .. } => write!(
                formatter,
                "{error}; refusing to supersede an unverifiable recovery artifact"
            ),
        }
    }
}

impl std::error::Error for StartupRecoveryError {}

impl RecoveryLockError {
    fn startup_error(&self) -> StartupRecoveryError {
        match self {
            Self::ContentionDeadline {
                store,
                attempts,
                budget,
            } => StartupRecoveryError {
                store: *store,
                failure: StartupRecoveryFailure::ContentionDeadline {
                    attempts: *attempts,
                    budget: *budget,
                },
            },
            Self::Io { store, error } => StartupRecoveryError {
                store: *store,
                failure: StartupRecoveryFailure::LockFailure {
                    kind: error.kind(),
                    error: error.to_string(),
                },
            },
        }
    }
}

impl StartupRecoveryError {
    pub(crate) fn artifact(store: StoreKind, error: std::io::Error) -> Self {
        Self {
            store,
            failure: StartupRecoveryFailure::ArtifactUnverifiable {
                kind: error.kind(),
                error: error.to_string(),
            },
        }
    }
}

#[cfg(not(test))]
static STARTUP_RECOVERY_ERROR: std::sync::OnceLock<std::sync::Mutex<Option<StartupRecoveryError>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
thread_local! {
    static STARTUP_RECOVERY_ERROR: std::cell::RefCell<Option<StartupRecoveryError>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(not(test))]
fn startup_recovery_error() -> Option<StartupRecoveryError> {
    STARTUP_RECOVERY_ERROR
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| {
            tracing::error!("recovering poisoned startup persistence error latch");
            poisoned.into_inner()
        })
        .clone()
}

#[cfg(test)]
fn startup_recovery_error() -> Option<StartupRecoveryError> {
    STARTUP_RECOVERY_ERROR.with(|error| error.borrow().clone())
}

fn latch_startup_recovery_error(error: StartupRecoveryError) {
    tracing::error!(%error, "persistence startup preflight will abort");
    let mutation_revoke_reason = std::sync::Arc::from(error.to_string());
    #[cfg(not(test))]
    {
        let mut latched = STARTUP_RECOVERY_ERROR
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap_or_else(|poisoned| {
                tracing::error!("recovering poisoned startup persistence error latch");
                poisoned.into_inner()
            });
        if latched.is_none() {
            *latched = Some(error);
        }
    }
    #[cfg(test)]
    STARTUP_RECOVERY_ERROR.with(|latched| {
        let mut latched = latched.borrow_mut();
        if latched.is_none() {
            *latched = Some(error);
        }
    });
    crate::util::safe_fs::revoke_process_mutations(mutation_revoke_reason);
}

pub fn ensure_startup_recovery_coherent() -> Result<(), StartupRecoveryError> {
    match startup_recovery_error() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Refuse every persistence mutation after coherent recovery ownership was lost.
///
/// Some callers (daemon and maintenance CLI paths) do not have the TUI startup preflight, and a
/// runtime store reload can fail after actors are already running. Keeping this guard process-wide
/// ensures those paths can continue read-only without ever overwriting the writer whose lock timed
/// out. The typed startup error remains available through [`ensure_startup_recovery_coherent`];
/// write APIs surface the same failure through their existing `io::Result` contracts.
pub(crate) fn ensure_persistence_writes_allowed() -> std::io::Result<()> {
    super::writer_lease_allows_mutation()?;
    ensure_startup_recovery_coherent().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!("persistence writes are disabled: {error}"),
        )
    })
}

pub(crate) fn write_store_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    ensure_persistence_writes_allowed()?;
    crate::util::safe_fs::write_private_atomic_json(path, value)
}

/// Serialize a direct config save with the same intent lock used by journaled actor writes and
/// config recovery. Actor-owned snapshots call [`write_store_json`] only after their outer
/// journal transaction has already acquired this lock.
pub(crate) fn write_config_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    ensure_persistence_writes_allowed()?;
    let _lock = super::acquire_intent_lock(path)?;
    write_store_json(path, value)
}

pub(crate) fn remove_store_file(path: &Path) -> std::io::Result<bool> {
    ensure_persistence_writes_allowed()?;
    let existed = match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    // A committed delete journal record is a durability claim. Sync the parent after unlink (and
    // after an already-absent retry) before the caller is allowed to advance that commit frontier.
    crate::util::safe_fs::remove_private_file_durable(path)?;
    Ok(existed)
}

/// Inspect one store's recovery frontier without invoking its ordinary base loader or migration
/// finalizer. Startup calls this for *every* store before any loader is allowed to back up,
/// repair, migrate, or save bytes.
pub(crate) fn preflight_journal_recovery<T>(
    kind: StoreKind,
    path: &Path,
    max_bytes: u64,
) -> Result<(), StartupRecoveryError>
where
    T: DeserializeOwned + Serialize + Default,
{
    ensure_startup_recovery_coherent()?;
    if super::persistence_access().is_read_only() {
        // Deliberate secondary/reader processes never replay or repair recovery artifacts. Their
        // later loaders use the strict byte-for-byte read-only path as well.
        return Ok(());
    }
    let _lock = acquire_coherent_load_lock(kind, path).map_err(|error| {
        let error = error.startup_error();
        latch_startup_recovery_error(error.clone());
        error
    })?;
    replay_locked::<T>(kind, path, max_bytes)
        .map(|_| ())
        .map_err(|error| {
            let error = StartupRecoveryError::artifact(kind, error);
            latch_startup_recovery_error(error.clone());
            error
        })
}

#[cfg(test)]
pub(crate) fn clear_startup_recovery_error_for_test() {
    STARTUP_RECOVERY_ERROR.with(|error| *error.borrow_mut() = None);
    crate::util::safe_fs::clear_process_mutation_revoke_for_test();
}

#[cfg(test)]
pub(crate) fn latch_startup_recovery_error_for_test(store: StoreKind) {
    clear_startup_recovery_error_for_test();
    latch_startup_recovery_error(StartupRecoveryError {
        store,
        failure: StartupRecoveryFailure::ArtifactUnverifiable {
            kind: std::io::ErrorKind::InvalidData,
            error: "injected unverifiable recovery artifact".to_owned(),
        },
    });
}

/// Load the base store and its recovery journal while holding the same advisory lock used by
/// writers. A committed/no-candidate frontier is therefore paired with the post-commit store,
/// never with a base value read before the writer acquired the lock.
pub(crate) fn load_with_journal_recovery<T, F>(
    kind: StoreKind,
    path: &Path,
    max_bytes: u64,
    load_current: F,
) -> T
where
    T: DeserializeOwned + Serialize + Default,
    F: FnOnce() -> T,
{
    load_with_journal_recovery_then(kind, path, max_bytes, load_current, |_| {})
}

/// Config migration uses the finalizer to persist a recovered default before releasing the
/// journal lock. Other stores should use [`load_with_journal_recovery`].
pub(crate) fn load_with_journal_recovery_then<T, F, G>(
    kind: StoreKind,
    path: &Path,
    max_bytes: u64,
    load_current: F,
    finish: G,
) -> T
where
    T: DeserializeOwned + Serialize + Default,
    F: FnOnce() -> T,
    G: FnOnce(&T),
{
    load_with_journal_recovery_then_using(kind, path, max_bytes, load_current, finish, || {
        acquire_coherent_load_lock(kind, path)
    })
}

fn load_with_journal_recovery_then_using<T, F, G, L, H>(
    kind: StoreKind,
    path: &Path,
    max_bytes: u64,
    load_current: F,
    finish: G,
    acquire: L,
) -> T
where
    T: DeserializeOwned + Serialize + Default,
    F: FnOnce() -> T,
    G: FnOnce(&T),
    L: FnOnce() -> Result<H, RecoveryLockError>,
{
    if let super::PersistenceAccess::ReadOnly { reason } = super::persistence_access() {
        tracing::warn!(
            store = kind.label(),
            %reason,
            "loading store through the strict read-only persistence path"
        );
        // Writer-lease losers may run only as explicit secondary read-only instances. Do not call
        // ordinary loaders: they can back up corrupt files, import legacy config, repair counts,
        // or run a migration finalizer before the first frame explains the read-only mode.
        return load_read_only(path, max_bytes);
    }
    if let Err(error) = ensure_startup_recovery_coherent() {
        tracing::warn!(
            store = kind.label(),
            blocked_by = error.store.label(),
            "loading store read-only after an earlier persistence startup failure"
        );
        return load_read_only(path, max_bytes);
    }
    match acquire() {
        Ok(_lock) => {
            let recovered = match replay_locked(kind, path, max_bytes) {
                Ok(Some(recovered)) => recovered,
                Ok(None) => load_current(),
                Err(error) => {
                    let startup_error = StartupRecoveryError::artifact(kind, error);
                    latch_startup_recovery_error(startup_error.clone());
                    tracing::warn!(
                        store = kind.label(),
                        error = %startup_error,
                        "loading store read-only after unverifiable persistence recovery"
                    );
                    // The normal loader can move corrupt state aside and the finalizer can save
                    // migrations. An authoritative intent that cannot be verified must latch the
                    // process read-only before either closure gets a chance to mutate the base.
                    return load_read_only(path, max_bytes);
                }
            };
            finish(&recovered);
            recovered
        }
        Err(error) => {
            latch_startup_recovery_error(error.startup_error());
            tracing::warn!(
                store = kind.label(),
                error = %error,
                "loading store read-only after persistence intent-lock failure"
            );
            // Neither supplied closure is safe without ownership: normal base loaders may move
            // corrupt files aside, while `finish` may persist migrated/default state. Use a
            // strictly read-only decoder so failed lock setup cannot trigger an uncoordinated
            // mutation. Lock contention itself is retried through the finite coherent-load
            // deadline above; after that, read-only fallback is the explicit recoverable outcome.
            load_read_only(path, max_bytes)
        }
    }
}

fn acquire_coherent_load_lock(
    kind: StoreKind,
    path: &Path,
) -> Result<crate::util::safe_fs::AdvisoryFileLock, RecoveryLockError> {
    acquire_coherent_load_lock_with(
        kind,
        path,
        COHERENT_LOAD_LOCK_TOTAL_BUDGET,
        COHERENT_LOAD_LOCK_ATTEMPT_BUDGET,
        acquire_intent_lock_with_budget,
        Instant::now,
    )
}

fn acquire_coherent_load_lock_with<T, A, N>(
    kind: StoreKind,
    path: &Path,
    total_budget: Duration,
    attempt_budget: Duration,
    mut acquire: A,
    now: N,
) -> Result<T, RecoveryLockError>
where
    A: FnMut(&Path, Duration) -> std::io::Result<T>,
    N: Fn() -> Instant,
{
    let started = now();
    let deadline = started + total_budget;
    let mut attempts = 0_u64;
    loop {
        let remaining = deadline.saturating_duration_since(now());
        if remaining.is_zero() {
            return Err(RecoveryLockError::ContentionDeadline {
                store: kind,
                attempts,
                budget: total_budget,
            });
        }
        let slice = remaining.min(attempt_budget);
        match acquire(path, slice) {
            Ok(lock) => return Ok(lock),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                attempts = attempts.saturating_add(1);
                if now() >= deadline {
                    return Err(RecoveryLockError::ContentionDeadline {
                        store: kind,
                        attempts,
                        budget: total_budget,
                    });
                }
                tracing::warn!(
                    store = kind.label(),
                    attempts,
                    remaining_ms = deadline.saturating_duration_since(now()).as_millis(),
                    "waiting for persistence writer before loading a coherent snapshot"
                );
            }
            Err(error) => return Err(RecoveryLockError::Io { store: kind, error }),
        }
    }
}

fn load_read_only<T>(path: &Path, max_bytes: u64) -> T
where
    T: DeserializeOwned + Serialize + Default,
{
    let Ok(bytes) = crate::util::safe_fs::read_no_symlink_limited(path, max_bytes) else {
        return T::default();
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return T::default();
    };
    if let Ok(current) = serde_json::from_str::<T>(&text) {
        return current;
    }
    serde_json::from_str::<serde_json::Value>(&text)
        .map(crate::util::safe_fs::recover_lenient::<T>)
        .unwrap_or_default()
}

/// Replay a pending snapshot while holding the same coherent advisory lock used by PR40's
/// startup loaders. `serde_json::Value` callers retain unknown config keys and can migrate the
/// raw object before typed recovery; the status remains false on any lock/artifact failure.
#[cfg(test)]
pub(crate) fn replay_journaled_snapshot_with_status<T>(
    kind: StoreKind,
    path: &Path,
    current: T,
    max_bytes: u64,
) -> (T, bool)
where
    T: DeserializeOwned + Serialize + Default,
{
    if super::persistence_access().is_read_only() {
        return (current, false);
    }
    if ensure_startup_recovery_coherent().is_err() {
        return (current, false);
    }
    let _lock = match acquire_coherent_load_lock(kind, path) {
        Ok(lock) => lock,
        Err(error) => {
            latch_startup_recovery_error(error.startup_error());
            return (current, false);
        }
    };
    match replay_locked(kind, path, max_bytes) {
        Ok(Some(replayed)) => (replayed, true),
        Ok(None) => (current, false),
        Err(error) => {
            latch_startup_recovery_error(StartupRecoveryError::artifact(kind, error));
            (current, false)
        }
    }
}

/// Begin a config load before inspecting its base file.
///
/// Reader processes and processes that already lost coherent recovery ownership retain the
/// existing strict read-only behavior and therefore carry no lock. A fresh lock failure is
/// latched before the caller can run a mutating backup or migration path.
pub(crate) fn begin_config_recovery(path: &Path) -> ConfigRecoveryGuard {
    if super::persistence_access().is_read_only() || ensure_startup_recovery_coherent().is_err() {
        return ConfigRecoveryGuard {
            state: ConfigRecoveryGuardState::ReadOnly {
                path: path.to_owned(),
            },
        };
    }
    match acquire_coherent_load_lock(StoreKind::Config, path) {
        Ok(lock) => ConfigRecoveryGuard {
            state: ConfigRecoveryGuardState::Coherent {
                path: path.to_owned(),
                lock,
            },
        },
        Err(error) => {
            latch_startup_recovery_error(error.startup_error());
            ConfigRecoveryGuard {
                state: ConfigRecoveryGuardState::ReadOnly {
                    path: path.to_owned(),
                },
            }
        }
    }
}

impl ConfigRecoveryGuard {
    fn path(&self) -> &Path {
        match &self.state {
            ConfigRecoveryGuardState::Coherent { path, .. }
            | ConfigRecoveryGuardState::ReadOnly { path } => path,
        }
    }

    /// Read the config base through the guard so production callers cannot inspect a stale base
    /// before coherent ownership has been resolved.
    pub(crate) fn read_base(&self, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        crate::util::safe_fs::read_no_symlink_limited(self.path(), max_bytes)
    }

    #[cfg(test)]
    pub(crate) fn read_base_with(
        &self,
        max_bytes: u64,
        read: impl FnOnce(&Path, u64) -> std::io::Result<Vec<u8>>,
    ) -> std::io::Result<Vec<u8>> {
        read(self.path(), max_bytes)
    }

    /// Replay a pending raw config object without releasing coherent ownership. Invalid
    /// scalar/array roots remain pending for a future compatible reader and never replace the
    /// valid base object supplied by the caller.
    pub(crate) fn replay(
        self,
        current: serde_json::Value,
        max_bytes: u64,
    ) -> ConfigRecoveryTransaction {
        match self.state {
            ConfigRecoveryGuardState::ReadOnly { .. } => ConfigRecoveryTransaction {
                state: ConfigRecoveryTransactionState::ReadOnly { value: current },
            },
            ConfigRecoveryGuardState::Coherent { path, lock } => {
                let base = current.clone();
                match replay_config_locked(&path, max_bytes) {
                    Ok(Some((candidate, _receipt))) if !candidate.is_object() => {
                        tracing::warn!("refusing non-object pending config snapshot");
                        ConfigRecoveryTransaction {
                            state: ConfigRecoveryTransactionState::Coherent {
                                path,
                                lock,
                                value: base,
                                receipt: None,
                            },
                        }
                    }
                    Ok(Some((value, receipt))) => ConfigRecoveryTransaction {
                        state: ConfigRecoveryTransactionState::Coherent {
                            path,
                            lock,
                            value,
                            receipt: Some(receipt),
                        },
                    },
                    Ok(None) => ConfigRecoveryTransaction {
                        state: ConfigRecoveryTransactionState::Coherent {
                            path,
                            lock,
                            value: current,
                            receipt: None,
                        },
                    },
                    Err(error) => {
                        latch_startup_recovery_error(StartupRecoveryError::artifact(
                            StoreKind::Config,
                            error,
                        ));
                        drop(lock);
                        ConfigRecoveryTransaction {
                            state: ConfigRecoveryTransactionState::ReadOnly { value: current },
                        }
                    }
                }
            }
        }
    }
}

impl ConfigRecoveryTransaction {
    pub(crate) fn value_mut(&mut self) -> &mut serde_json::Value {
        match &mut self.state {
            ConfigRecoveryTransactionState::Coherent { value, .. }
            | ConfigRecoveryTransactionState::ReadOnly { value } => value,
        }
    }

    pub(crate) fn has_replayed_candidate(&self) -> bool {
        matches!(
            &self.state,
            ConfigRecoveryTransactionState::Coherent {
                receipt: Some(_),
                ..
            }
        )
    }

    pub(crate) fn into_value(self) -> serde_json::Value {
        match self.state {
            ConfigRecoveryTransactionState::Coherent { lock, value, .. } => {
                drop(lock);
                value
            }
            ConfigRecoveryTransactionState::ReadOnly { value } => value,
        }
    }

    /// Atomically install the raw config and settle only the exact candidate that was replayed.
    /// Any write or settlement error drops coherent ownership without advancing the journal, so
    /// a later load can retry from the same durable receipt.
    pub(crate) fn install_and_settle(self) -> (serde_json::Value, std::io::Result<()>) {
        self.install_and_settle_using(|path, value| {
            crate::util::safe_fs::write_private_atomic_json(path, value)
        })
    }

    #[cfg(test)]
    pub(crate) fn install_and_settle_with(
        self,
        write: impl FnOnce(&Path, &serde_json::Value) -> std::io::Result<()>,
    ) -> (serde_json::Value, std::io::Result<()>) {
        self.install_and_settle_using(write)
    }

    fn install_and_settle_using(
        self,
        write: impl FnOnce(&Path, &serde_json::Value) -> std::io::Result<()>,
    ) -> (serde_json::Value, std::io::Result<()>) {
        match self.state {
            ConfigRecoveryTransactionState::Coherent {
                path,
                lock,
                value,
                receipt,
            } => {
                let result = ensure_persistence_writes_allowed()
                    .and_then(|()| write(&path, &value))
                    .and_then(|()| match receipt {
                        Some(receipt) => settle_config_recovery_locked(&path, &receipt),
                        None => Ok(()),
                    });
                drop(lock);
                (value, result)
            }
            ConfigRecoveryTransactionState::ReadOnly { value } => {
                let result = ensure_persistence_writes_allowed().and_then(|()| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "config recovery has no coherent writer ownership",
                    ))
                });
                (value, result)
            }
        }
    }
}

fn replay_config_locked(
    path: &Path,
    max_bytes: u64,
) -> std::io::Result<Option<(serde_json::Value, ConfigRecoveryReceipt)>> {
    let state = read_journal_state(StoreKind::Config, path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("could not read config recovery journal: {error}"),
        )
    })?;
    let Some(candidate) = state.candidate.as_ref() else {
        return Ok(None);
    };
    let receipt = match candidate.order {
        Some(order) => ConfigRecoveryReceipt::Ordered {
            order,
            raw_line: candidate.raw_line.clone(),
        },
        None => ConfigRecoveryReceipt::Legacy {
            raw_line: candidate.raw_line.clone(),
        },
    };
    replay_candidate(StoreKind::Config, path, max_bytes, Some(candidate))
        .map(|value| value.map(|value| (value, receipt)))
}

fn settle_config_recovery_locked(
    path: &Path,
    receipt: &ConfigRecoveryReceipt,
) -> std::io::Result<()> {
    settle_config_recovery_locked_using(
        path,
        receipt,
        |path, order| super::commit_journal_generation_locked(StoreKind::Config, path, order),
        clear_legacy_config_recovery_locked,
    )
}

fn settle_config_recovery_locked_using(
    path: &Path,
    receipt: &ConfigRecoveryReceipt,
    commit_ordered: impl FnOnce(&Path, JournalOrder) -> std::io::Result<()>,
    clear_legacy: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    ensure_persistence_writes_allowed()?;
    let state = read_journal_state(StoreKind::Config, path)?;
    let Some(candidate) = state.candidate.as_ref() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "installed config recovery candidate is no longer current",
        ));
    };
    match receipt {
        ConfigRecoveryReceipt::Ordered { order, raw_line }
            if candidate.order == Some(*order) && candidate.raw_line == *raw_line =>
        {
            commit_ordered(path, *order)
        }
        ConfigRecoveryReceipt::Legacy { raw_line }
            if candidate.order.is_none() && candidate.raw_line == *raw_line =>
        {
            clear_legacy(path)
        }
        ConfigRecoveryReceipt::Ordered { .. } | ConfigRecoveryReceipt::Legacy { .. } => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "installed config recovery receipt no longer matches the journal candidate",
            ))
        }
    }
}

fn clear_legacy_config_recovery_locked(path: &Path) -> std::io::Result<()> {
    clear_legacy_config_recovery_locked_with(path, |journal_path| {
        crate::util::safe_fs::remove_private_file_durable(journal_path)
    })
}

fn clear_legacy_config_recovery_locked_with(
    path: &Path,
    remove_journal: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let journal_path = super::intent_journal_path(path)
        .ok_or_else(|| std::io::Error::other("invalid config recovery journal path"))?;
    // The sidecar is the only replay payload for a generation-less record. Remove and sync the
    // journal name first; on any unlink/sync failure the sidecar must remain available for retry.
    remove_journal(&journal_path)?;
    // Unix establishes a durable name-removal boundary with the parent-directory sync above.
    // Windows has no documented directory-fsync API for an unlink, so retain the one bounded
    // legacy sidecar there; the next confirmed ordered journal write will reclaim it safely.
    #[cfg(unix)]
    super::cleanup_orphan_sidecars_locked(path, None);
    Ok(())
}

#[cfg(test)]
pub(super) fn replay_journaled_snapshot<T>(
    kind: StoreKind,
    path: &Path,
    current: T,
    max_bytes: u64,
) -> T
where
    T: DeserializeOwned + Serialize + Default,
{
    replay_journaled_snapshot_with_status(kind, path, current, max_bytes).0
}

fn replay_locked<T>(kind: StoreKind, path: &Path, max_bytes: u64) -> std::io::Result<Option<T>>
where
    T: DeserializeOwned + Serialize + Default,
{
    let state = read_journal_state(kind, path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("could not read {} recovery journal: {error}", kind.label()),
        )
    })?;
    replay_candidate(kind, path, max_bytes, state.candidate.as_ref())
}

fn replay_candidate<T>(
    kind: StoreKind,
    path: &Path,
    max_bytes: u64,
    candidate: Option<&JournalCandidate>,
) -> std::io::Result<Option<T>>
where
    T: DeserializeOwned + Serialize + Default,
{
    let Some(candidate) = candidate else {
        return Ok(None);
    };
    match &candidate.operation {
        JournalOperation::Delete if kind == StoreKind::RomanizedTitles => {
            tracing::info!(store = kind.label(), "replayed pending deletion intent");
            Ok(Some(T::default()))
        }
        JournalOperation::Replace { sidecar, sha256 } => {
            let sidecar_path = sibling_path_from_record(path, sidecar).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} recovery journal names an invalid sidecar", kind.label()),
                )
            })?;
            let cap = max_bytes.min(INTENT_SNAPSHOT_MAX_BYTES);
            let snapshot_bytes = crate::util::safe_fs::read_no_symlink_limited(&sidecar_path, cap)
                .map_err(|error| {
                    std::io::Error::new(
                        error.kind(),
                        format!(
                            "could not verify {} recovery sidecar {}: {error}",
                            kind.label(),
                            sidecar_path.display()
                        ),
                    )
                })?;
            if sha256_hex(&snapshot_bytes) != *sha256 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{} recovery sidecar checksum mismatch", kind.label()),
                ));
            }
            match serde_json::from_slice::<T>(&snapshot_bytes) {
                Ok(snapshot) => {
                    tracing::info!(store = kind.label(), "replayed pending persistence intent");
                    Ok(Some(snapshot))
                }
                Err(strict_error) => {
                    let value = serde_json::from_slice::<serde_json::Value>(&snapshot_bytes)
                        .map_err(|json_error| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!(
                                    "{} recovery sidecar is not valid JSON after strict decode failed ({strict_error}): {json_error}",
                                    kind.label()
                                ),
                            )
                        })?;
                    tracing::warn!(
                        store = kind.label(),
                        error = %strict_error,
                        "recovering schema-drifted persistence intent field-by-field"
                    );
                    Ok(Some(crate::util::safe_fs::recover_lenient::<T>(value)))
                }
            }
        }
        JournalOperation::Delete => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} recovery journal contains an unsupported delete intent",
                kind.label()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    use serde::{Deserialize, Serialize};

    use super::super::{
        JournalIntent, JournalOrder, JournalOrderSource, PendingOperation, Snapshot,
        clear_store_journal, intent_journal_path, read_journal_state, unique_intent_sidecar_path,
        write_journal_intent, write_panic_operation,
    };
    use super::*;

    #[test]
    fn transient_contention_retries_after_the_first_five_second_slice() {
        let base = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let attempts = Cell::new(0_u64);
        let budgets = RefCell::new(Vec::new());

        let acquired = acquire_coherent_load_lock_with(
            StoreKind::Config,
            Path::new("config.json"),
            Duration::from_secs(15),
            Duration::from_secs(5),
            |_, budget| {
                budgets.borrow_mut().push(budget);
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                if attempt == 1 {
                    elapsed.set(Duration::from_secs(5));
                    Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "first lock slice still contended",
                    ))
                } else {
                    elapsed.set(Duration::from_secs(6));
                    Ok("coherent lock")
                }
            },
            || base + elapsed.get(),
        )
        .unwrap();

        assert_eq!(acquired, "coherent lock");
        assert_eq!(attempts.get(), 2);
        assert_eq!(elapsed.get(), Duration::from_secs(6));
        assert_eq!(
            *budgets.borrow(),
            [Duration::from_secs(5), Duration::from_secs(5)]
        );
    }

    #[test]
    fn permanent_contention_returns_a_typed_error_at_the_total_deadline() {
        let base = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let budgets = RefCell::new(Vec::new());

        let error = acquire_coherent_load_lock_with::<(), _, _>(
            StoreKind::Library,
            Path::new("library.json"),
            Duration::from_millis(12),
            Duration::from_millis(5),
            |_, budget| {
                budgets.borrow_mut().push(budget);
                elapsed.set(elapsed.get() + budget);
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "lock remains held",
                ))
            },
            || base + elapsed.get(),
        )
        .unwrap_err();

        assert!(matches!(
            error,
            RecoveryLockError::ContentionDeadline {
                store: StoreKind::Library,
                attempts: 3,
                budget
            } if budget == Duration::from_millis(12)
        ));
        assert_eq!(
            *budgets.borrow(),
            [
                Duration::from_millis(5),
                Duration::from_millis(5),
                Duration::from_millis(2),
            ]
        );
        assert_eq!(elapsed.get(), Duration::from_millis(12));
    }

    #[test]
    fn timeout_latches_startup_abort_without_running_loaders_finalizers_or_new_writes() {
        #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        struct Tiny {
            value: u8,
        }

        clear_startup_recovery_error_for_test();
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let suffix = suffix
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let directory = std::env::temp_dir().join(format!(
            "yututui-recovery-startup-abort-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("library.json");
        crate::util::safe_fs::write_private_atomic(
            &path,
            serde_json::to_vec(&Tiny { value: 1 }).unwrap().as_slice(),
        )
        .unwrap();

        let load_calls = Cell::new(0_u32);
        let finalizer_calls = Cell::new(0_u32);
        let loaded = load_with_journal_recovery_then_using(
            StoreKind::Library,
            &path,
            1024,
            || {
                load_calls.set(load_calls.get() + 1);
                Tiny { value: 2 }
            },
            |_| finalizer_calls.set(finalizer_calls.get() + 1),
            || {
                Err::<(), _>(RecoveryLockError::ContentionDeadline {
                    store: StoreKind::Library,
                    attempts: 3,
                    budget: Duration::from_secs(15),
                })
            },
        );

        assert_eq!(loaded, Tiny { value: 1 });
        assert_eq!(load_calls.get(), 0);
        assert_eq!(finalizer_calls.get(), 0);
        assert!(matches!(
            ensure_startup_recovery_coherent(),
            Err(StartupRecoveryError {
                store: StoreKind::Library,
                failure: StartupRecoveryFailure::ContentionDeadline {
                    attempts: 3,
                    budget
                }
            }) if budget == Duration::from_secs(15)
        ));
        assert_eq!(
            crate::util::safe_fs::load_json_or_default_limited::<Tiny>(&path, 1024),
            Tiny { value: 1 },
            "read-only timeout handling must not mutate the store"
        );

        // Simulate the old lock owner committing after our deadline. Non-TUI callers can miss the
        // startup preflight, so the process-wide write gate itself must preserve this completion.
        std::fs::write(
            &path,
            serde_json::to_vec(&Tiny { value: 9 }).unwrap().as_slice(),
        )
        .unwrap();

        let non_store_path = directory.join("transfer-checkpoint.json");
        let non_store_error =
            crate::util::safe_fs::write_private_atomic(&non_store_path, b"checkpoint").unwrap_err();
        assert_eq!(non_store_error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(!non_store_path.exists());

        let direct_error = write_store_json(&path, &Tiny { value: 2 }).unwrap_err();
        assert_eq!(direct_error.kind(), std::io::ErrorKind::WouldBlock);

        let accepted = JournalOrderSource::for_test(41).accept();
        let journal_error = write_journal_intent(&JournalIntent::Replace {
            order: accepted.order,
            kind: StoreKind::Library,
            path: path.clone(),
            bytes: serde_json::to_vec(&Tiny { value: 3 }).unwrap(),
        })
        .unwrap_err();
        assert_eq!(journal_error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(!intent_journal_path(&path).unwrap().exists());

        let panic_path = path.clone();
        let panic_operation = PendingOperation::save(
            Snapshot::Test {
                kind: StoreKind::Library,
                label: "blocked non-TUI panic writer",
                storage_path: Some(path.clone()),
                writer: Arc::new(move || {
                    crate::util::safe_fs::write_private_atomic(&panic_path, b"panic")
                }),
            },
            JournalOrderSource::for_test(42).accept(),
        )
        .panic_operation()
        .unwrap();
        let panic_error = write_panic_operation(&panic_operation).unwrap_err();
        assert_eq!(panic_error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(!intent_journal_path(&path).unwrap().exists());

        assert_eq!(
            crate::util::safe_fs::load_json_or_default_limited::<Tiny>(&path, 1024),
            Tiny { value: 9 },
            "direct, journal, and panic writes must not overwrite the old writer's completion"
        );

        let later_acquire_calls = Cell::new(0_u32);
        let later_load_calls = Cell::new(0_u32);
        let later_finish_calls = Cell::new(0_u32);
        let later = load_with_journal_recovery_then_using(
            StoreKind::Signals,
            &path,
            1024,
            || {
                later_load_calls.set(later_load_calls.get() + 1);
                Tiny { value: 3 }
            },
            |_| later_finish_calls.set(later_finish_calls.get() + 1),
            || {
                later_acquire_calls.set(later_acquire_calls.get() + 1);
                Ok(())
            },
        );
        assert_eq!(later, Tiny { value: 9 });
        assert_eq!(later_acquire_calls.get(), 0);
        assert_eq!(later_load_calls.get(), 0);
        assert_eq!(later_finish_calls.get(), 0);

        clear_startup_recovery_error_for_test();
        let _ = std::fs::remove_dir_all(directory);
    }

    #[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(default)]
    struct ArtifactState {
        preserved: u8,
        drifted: u8,
    }

    fn artifact_test_dir(label: &str) -> std::path::PathBuf {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let suffix = suffix
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let directory = std::env::temp_dir().join(format!(
            "yututui-recovery-{label}-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn failed_legacy_journal_removal_keeps_the_only_replay_sidecar() {
        let directory = artifact_test_dir("legacy-clear-failure");
        let path = directory.join("config.json");
        let journal = intent_journal_path(&path).unwrap();
        let sidecar = path.with_file_name("config.json.intent.latest.json");
        crate::util::safe_fs::write_private_atomic(&journal, b"legacy record\n").unwrap();
        crate::util::safe_fs::write_private_atomic(&sidecar, b"replay payload").unwrap();

        let error = clear_legacy_config_recovery_locked_with(&path, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected journal unlink failure",
            ))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&journal).unwrap(), b"legacy record\n");
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"replay payload");

        let error = clear_legacy_config_recovery_locked_with(&path, |journal| {
            crate::util::safe_fs::remove_private_file_durable(journal)?;
            Err(std::io::Error::other(
                "injected failure after durable journal unlink",
            ))
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(!journal.exists());
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"replay payload");

        clear_legacy_config_recovery_locked(&path).unwrap();
        #[cfg(unix)]
        assert!(!sidecar.exists());
        #[cfg(not(unix))]
        assert_eq!(std::fs::read(&sidecar).unwrap(), b"replay payload");
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn seed_artifact_intent(
        path: &Path,
        bytes: Vec<u8>,
        epoch: u64,
    ) -> (JournalOrder, std::path::PathBuf) {
        let accepted = JournalOrderSource::for_test(epoch).accept();
        write_journal_intent(&JournalIntent::Replace {
            order: accepted.order,
            kind: StoreKind::Config,
            path: path.to_path_buf(),
            bytes,
        })
        .unwrap();
        let sidecar = unique_intent_sidecar_path(path, accepted.order).unwrap();
        (accepted.order, sidecar)
    }

    fn seed_ordered_config_receipt(
        label: &str,
        epoch: u64,
    ) -> (
        std::path::PathBuf,
        std::path::PathBuf,
        JournalOrder,
        std::path::PathBuf,
        ConfigRecoveryReceipt,
    ) {
        let directory = artifact_test_dir(label);
        let path = directory.join("config.json");
        let bytes = serde_json::to_vec(&ArtifactState {
            preserved: 7,
            drifted: 9,
        })
        .unwrap();
        let (order, sidecar) = seed_artifact_intent(&path, bytes, epoch);
        let (_, receipt) = replay_config_locked(&path, 1024).unwrap().unwrap();
        (directory, path, order, sidecar, receipt)
    }

    #[test]
    fn ordered_settlement_faults_preserve_retry_or_recognize_commit() {
        clear_startup_recovery_error_for_test();
        let (directory, path, order, sidecar, receipt) =
            seed_ordered_config_receipt("ordered-commit-pre-visible", 81);
        let journal = intent_journal_path(&path).unwrap();
        let original_journal = std::fs::read(&journal).unwrap();
        let original_sidecar = std::fs::read(&sidecar).unwrap();

        let error = settle_config_recovery_locked_using(
            &path,
            &receipt,
            |_, _| Err(std::io::Error::other("injected pre-visible commit failure")),
            |_| unreachable!("ordered receipt must not take the legacy settlement path"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(std::fs::read(&journal).unwrap(), original_journal);
        assert_eq!(std::fs::read(&sidecar).unwrap(), original_sidecar);

        settle_config_recovery_locked(&path, &receipt).unwrap();
        let state = read_journal_state(StoreKind::Config, &path).unwrap();
        assert_eq!(state.committed_through, Some(order));
        assert!(state.candidate.is_none());
        assert!(!sidecar.exists());
        std::fs::remove_dir_all(directory).unwrap();

        let (directory, path, order, sidecar, receipt) =
            seed_ordered_config_receipt("ordered-commit-post-visible", 82);
        let error = settle_config_recovery_locked_using(
            &path,
            &receipt,
            |path, order| {
                super::super::commit_journal_generation_locked(StoreKind::Config, path, order)?;
                Err(std::io::Error::other(
                    "injected failure after visible commit",
                ))
            },
            |_| unreachable!("ordered receipt must not take the legacy settlement path"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        let state = read_journal_state(StoreKind::Config, &path).unwrap();
        assert_eq!(state.committed_through, Some(order));
        assert!(state.candidate.is_none());
        assert!(!sidecar.exists());
        assert!(replay_config_locked(&path, 1024).unwrap().is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn assert_unverifiable_artifact_latches_without_supersession<F>(
        label: &str,
        bytes: Vec<u8>,
        mutate: F,
    ) where
        F: FnOnce(&Path),
    {
        clear_startup_recovery_error_for_test();
        let directory = artifact_test_dir(label);
        let path = directory.join("config.json");
        crate::util::safe_fs::write_private_atomic_json(
            &path,
            &ArtifactState {
                preserved: 1,
                drifted: 2,
            },
        )
        .unwrap();
        let (original_order, sidecar) = seed_artifact_intent(&path, bytes, 71);
        mutate(&sidecar);

        let load_calls = Cell::new(0_u32);
        let finalizer_calls = Cell::new(0_u32);
        let loaded = load_with_journal_recovery_then_using(
            StoreKind::Config,
            &path,
            1024,
            || {
                load_calls.set(load_calls.get() + 1);
                ArtifactState {
                    preserved: 9,
                    drifted: 9,
                }
            },
            |_| finalizer_calls.set(finalizer_calls.get() + 1),
            || Ok(()),
        );

        assert_eq!(
            loaded,
            ArtifactState {
                preserved: 1,
                drifted: 2,
            }
        );
        assert_eq!(load_calls.get(), 0, "{label}: base loader must not run");
        assert_eq!(finalizer_calls.get(), 0, "{label}: finalizer must not run");
        assert!(matches!(
            ensure_startup_recovery_coherent(),
            Err(StartupRecoveryError {
                store: StoreKind::Config,
                failure: StartupRecoveryFailure::ArtifactUnverifiable { .. }
            })
        ));

        let replacement = JournalOrderSource::for_test(72).accept();
        let error = write_journal_intent(&JournalIntent::Replace {
            order: replacement.order,
            kind: StoreKind::Config,
            path: path.clone(),
            bytes: serde_json::to_vec(&ArtifactState::default()).unwrap(),
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(
            read_journal_state(StoreKind::Config, &path)
                .unwrap()
                .candidate
                .and_then(|candidate| candidate.order),
            Some(original_order),
            "{label}: failed recovery must not be superseded later in the process"
        );

        clear_startup_recovery_error_for_test();
        if sidecar.is_dir() {
            std::fs::remove_dir(&sidecar).unwrap();
        }
        clear_store_journal(&path);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn missing_authoritative_sidecar_latches_read_only_before_base_loader() {
        assert_unverifiable_artifact_latches_without_supersession(
            "missing-sidecar",
            serde_json::to_vec(&ArtifactState::default()).unwrap(),
            |sidecar| std::fs::remove_file(sidecar).unwrap(),
        );
    }

    #[test]
    fn unreadable_authoritative_sidecar_latches_read_only_before_base_loader() {
        assert_unverifiable_artifact_latches_without_supersession(
            "unreadable-sidecar",
            serde_json::to_vec(&ArtifactState::default()).unwrap(),
            |sidecar| {
                std::fs::remove_file(sidecar).unwrap();
                std::fs::create_dir(sidecar).unwrap();
            },
        );
    }

    #[test]
    fn checksum_mismatch_latches_read_only_before_base_loader() {
        assert_unverifiable_artifact_latches_without_supersession(
            "checksum-mismatch",
            serde_json::to_vec(&ArtifactState::default()).unwrap(),
            |sidecar| std::fs::write(sidecar, br#"{"preserved":99}"#).unwrap(),
        );
    }

    #[test]
    fn non_json_sidecar_latches_read_only_after_strict_decode_failure() {
        assert_unverifiable_artifact_latches_without_supersession(
            "strict-decode",
            b"{not-json".to_vec(),
            |_| {},
        );
    }

    #[test]
    fn valid_json_schema_drift_uses_lenient_recovery_without_latching() {
        clear_startup_recovery_error_for_test();
        let directory = artifact_test_dir("schema-drift");
        let path = directory.join("config.json");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "preserved": 7,
            "drifted": "old-enum-variant"
        }))
        .unwrap();
        seed_artifact_intent(&path, bytes, 81);
        let load_calls = Cell::new(0_u32);
        let finish_calls = Cell::new(0_u32);

        let loaded = load_with_journal_recovery_then_using(
            StoreKind::Config,
            &path,
            1024,
            || {
                load_calls.set(load_calls.get() + 1);
                ArtifactState {
                    preserved: 1,
                    drifted: 2,
                }
            },
            |_| finish_calls.set(finish_calls.get() + 1),
            || Ok(()),
        );

        assert_eq!(
            loaded,
            ArtifactState {
                preserved: 7,
                drifted: 0,
            }
        );
        assert_eq!(load_calls.get(), 0);
        assert_eq!(finish_calls.get(), 1);
        assert!(ensure_startup_recovery_coherent().is_ok());

        clear_store_journal(&path);
        let _ = std::fs::remove_dir_all(directory);
    }
}
