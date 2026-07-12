use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Serialize, de::DeserializeOwned};

use super::{
    INTENT_SNAPSHOT_MAX_BYTES, JournalOperation, StoreKind, acquire_intent_lock_with_budget,
    read_journal_state, sha256_hex, sibling_path_from_record,
};

const COHERENT_LOAD_LOCK_TOTAL_BUDGET: Duration = Duration::from_secs(15);
const COHERENT_LOAD_LOCK_ATTEMPT_BUDGET: Duration = Duration::from_secs(5);

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
    fn artifact(store: StoreKind, error: std::io::Error) -> Self {
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

/// Config-specific raw replay. Invalid scalar/array roots are left pending for a future reader and
/// never replace a valid base object. The returned `replayed` flag may be used to settle the
/// ordered intent with [`super::clear_journaled_snapshot`] only after an atomic migrated install
/// succeeds.
pub(crate) fn replay_config_journaled_value_with_status(
    path: &Path,
    current: serde_json::Value,
    max_bytes: u64,
) -> (serde_json::Value, bool) {
    let base = current.clone();
    let (candidate, replayed) =
        replay_journaled_snapshot_with_status(StoreKind::Config, path, current, max_bytes);
    if replayed && !candidate.is_object() {
        tracing::warn!("refusing non-object pending config snapshot");
        (base, false)
    } else {
        (candidate, replayed)
    }
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
    let Some(candidate) = state.candidate else {
        return Ok(None);
    };
    match candidate.operation {
        JournalOperation::Delete if kind == StoreKind::RomanizedTitles => {
            tracing::info!(store = kind.label(), "replayed pending deletion intent");
            Ok(Some(T::default()))
        }
        JournalOperation::Replace { sidecar, sha256 } => {
            let sidecar_path = sibling_path_from_record(path, &sidecar).ok_or_else(|| {
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
            if sha256_hex(&snapshot_bytes) != sha256 {
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
