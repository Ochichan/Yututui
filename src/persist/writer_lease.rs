use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(not(test))]
use std::sync::{Mutex, OnceLock, PoisonError};

const WRITER_LEASE_FILE: &str = ".ytt-persistence-writer.lock";
const READ_ONLY_PROCESS_REASON: &str =
    "this command is observational and does not acquire a persistence writer lease";

/// Process-wide persistence capability established before any store loader can migrate or repair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistenceAccess {
    Writable,
    ReadOnly { reason: Arc<str> },
}

impl PersistenceAccess {
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }

    pub fn read_only_reason(&self) -> Option<&str> {
        match self {
            Self::Writable => None,
            Self::ReadOnly { reason } => Some(reason),
        }
    }
}

/// Own every independently configurable persistence root for the process lifetime.
///
/// Config, data, and cache may point at the same directory or at three unrelated directories.
/// Locking only one of them permits split-root processes to corrupt the others, so roots are
/// normalized, sorted, deduplicated, and then acquired as one all-or-nothing lease set.
struct PersistenceWriterLease {
    _locks: Vec<crate::util::safe_fs::AdvisoryFileLock>,
}

enum ProcessWriterState {
    Uninitialized,
    Writable(PersistenceWriterLease),
    ReadOnly(Arc<str>),
}

/// A completed acquisition cannot represent the process-only pre-initialization state.
enum InitializedWriterState {
    Writable(PersistenceWriterLease),
    ReadOnly(Arc<str>),
}

impl InitializedWriterState {
    fn access(&self) -> PersistenceAccess {
        match self {
            Self::Writable(_) => PersistenceAccess::Writable,
            Self::ReadOnly(reason) => PersistenceAccess::ReadOnly {
                reason: Arc::clone(reason),
            },
        }
    }

    fn mutation_access(&self) -> Result<(), Arc<str>> {
        match self {
            Self::Writable(_) => Ok(()),
            Self::ReadOnly(reason) => Err(Arc::clone(reason)),
        }
    }

    #[cfg(test)]
    fn ensure_writable(&self) -> std::io::Result<()> {
        match self {
            Self::Writable(lease) => {
                let _held_root_count = lease._locks.len();
                Ok(())
            }
            Self::ReadOnly(reason) => Err(read_only_error(reason)),
        }
    }
}

impl From<InitializedWriterState> for ProcessWriterState {
    fn from(state: InitializedWriterState) -> Self {
        match state {
            InitializedWriterState::Writable(lease) => Self::Writable(lease),
            InitializedWriterState::ReadOnly(reason) => Self::ReadOnly(reason),
        }
    }
}

impl ProcessWriterState {
    fn access(&self) -> PersistenceAccess {
        match self {
            Self::Uninitialized => PersistenceAccess::ReadOnly {
                reason: Arc::from("the process did not initialize its persistence writer lease"),
            },
            Self::Writable(_) => PersistenceAccess::Writable,
            Self::ReadOnly(reason) => PersistenceAccess::ReadOnly {
                reason: Arc::clone(reason),
            },
        }
    }

    fn ensure_writable(&self) -> std::io::Result<()> {
        match self {
            Self::Uninitialized => Err(read_only_error(
                "the process did not initialize its persistence writer lease",
            )),
            Self::Writable(lease) => {
                // Reading the field documents that the RAII guards intentionally live as long as
                // this state. An environment with no resolvable roots legitimately owns zero.
                let _held_root_count = lease._locks.len();
                Ok(())
            }
            Self::ReadOnly(reason) => Err(read_only_error(reason)),
        }
    }
}

#[cfg(not(test))]
static PROCESS_WRITER: OnceLock<Mutex<ProcessWriterState>> = OnceLock::new();

#[cfg(not(test))]
fn process_writer() -> &'static Mutex<ProcessWriterState> {
    PROCESS_WRITER.get_or_init(|| Mutex::new(ProcessWriterState::Uninitialized))
}

fn read_only_error(reason: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!("persistence is read-only: {reason}"),
    )
}

/// Lexically normalize an absolute path. The lease roots are never permitted to retain `.` or
/// `..`, because two spellings of the same missing directory must still deduplicate.
fn lexical_normalize_absolute(path: &Path) -> PathBuf {
    debug_assert!(path.is_absolute());
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never pop a platform prefix or root.
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

/// Canonicalize the deepest existing ancestor, then append the missing suffix. This makes aliases
/// through an existing symlink converge even when the configured leaf has not been created yet.
fn normalize_root(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let absolute = lexical_normalize_absolute(&absolute);
    let mut cursor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(cursor) {
            Ok(existing) => {
                let mut normalized = existing;
                for component in missing.iter().rev() {
                    normalized.push(component);
                }
                return Ok(lexical_normalize_absolute(&normalized));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor.file_name().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "persistence root has no canonical ancestor: {}",
                            path.display()
                        ),
                    )
                })?;
                missing.push(name.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("persistence root has no parent: {}", path.display()),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn normalized_roots<I, P>(roots: I) -> std::io::Result<Vec<PathBuf>>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut roots = roots
        .into_iter()
        .map(|root| normalize_root(root.as_ref()))
        .collect::<std::io::Result<Vec<_>>>()?;
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn configured_roots() -> Vec<PathBuf> {
    [
        crate::paths::config_dir(),
        crate::paths::data_dir(),
        crate::paths::cache_dir(),
        crate::paths::local_index_data_dir(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn acquire_state_for_roots<I, P>(
    roots: I,
    allow_read_only: bool,
) -> std::io::Result<InitializedWriterState>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let configured: Vec<PathBuf> = roots
        .into_iter()
        .map(|root| root.as_ref().to_path_buf())
        .collect();
    let roots = match normalized_roots(&configured) {
        Ok(roots) => roots,
        Err(error) if allow_read_only => {
            return Ok(InitializedWriterState::ReadOnly(Arc::from(format!(
                "the persistence roots could not be verified ({error}); changes are not saved"
            ))));
        }
        Err(error) => {
            return Err(std::io::Error::new(
                error.kind(),
                format!("could not verify persistence roots: {error}"),
            ));
        }
    };

    let mut locks = Vec::with_capacity(roots.len());
    for root in &roots {
        let lock_path = root.join(WRITER_LEASE_FILE);
        match crate::util::safe_fs::try_lock_private_file(&lock_path) {
            Ok(Some(lock)) => locks.push(lock),
            Ok(None) => {
                // Returning drops every earlier guard. A process either owns the complete root set
                // or none of it; partial ownership must never strand another writer.
                let reason: Arc<str> = Arc::from(format!(
                    "another live YuTuTui! process owns the persistence writer lease at {}; changes are not saved",
                    root.display()
                ));
                return if allow_read_only {
                    Ok(InitializedWriterState::ReadOnly(reason))
                } else {
                    Err(read_only_error(&reason))
                };
            }
            Err(error) if allow_read_only => {
                return Ok(InitializedWriterState::ReadOnly(Arc::from(format!(
                    "the persistence writer lease at {} could not be verified ({error}); changes are not saved",
                    root.display()
                ))));
            }
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!(
                        "could not acquire persistence writer lease at {}: {error}",
                        root.display()
                    ),
                ));
            }
        }
    }

    // Creating the lock files may create previously absent roots. Re-resolve every original path
    // while all guards are held, both to converge the new directories and to reject a symlink
    // replacement between normalization and acquisition. Returning here drops the complete set.
    let verified_roots = match normalized_roots(&configured) {
        Ok(roots) => roots,
        Err(error) if allow_read_only => {
            return Ok(InitializedWriterState::ReadOnly(Arc::from(format!(
                "persistence roots changed or became unreadable while acquiring the writer lease ({error}); changes are not saved"
            ))));
        }
        Err(error) => return Err(error),
    };
    if verified_roots != roots {
        let error = std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "persistence roots changed while acquiring the writer lease",
        );
        return if allow_read_only {
            Ok(InitializedWriterState::ReadOnly(Arc::from(format!(
                "{error}; changes are not saved"
            ))))
        } else {
            Err(error)
        };
    }
    Ok(InitializedWriterState::Writable(PersistenceWriterLease {
        _locks: locks,
    }))
}

fn initialize_writer_state<I, P>(
    state: &mut ProcessWriterState,
    roots: I,
    allow_read_only: bool,
    acquire: impl FnOnce(I, bool) -> std::io::Result<InitializedWriterState>,
    configure: impl FnOnce(Result<(), Arc<str>>) -> std::io::Result<()>,
) -> std::io::Result<PersistenceAccess>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    match state {
        ProcessWriterState::Uninitialized => {}
        existing => {
            if !allow_read_only {
                existing.ensure_writable()?;
            }
            return Ok(existing.access());
        }
    }

    let acquired = acquire(roots, allow_read_only)?;
    let access = acquired.access();
    configure(acquired.mutation_access())?;
    *state = acquired.into();
    Ok(access)
}

fn initialize_reader_state(
    state: &mut ProcessWriterState,
    reason: Arc<str>,
    configure: impl FnOnce(Result<(), Arc<str>>) -> std::io::Result<()>,
) -> std::io::Result<PersistenceAccess> {
    match state {
        ProcessWriterState::Uninitialized => {
            configure(Err(Arc::clone(&reason)))?;
            *state = ProcessWriterState::ReadOnly(Arc::clone(&reason));
            Ok(PersistenceAccess::ReadOnly { reason })
        }
        ProcessWriterState::ReadOnly(_) => Ok(state.access()),
        ProcessWriterState::Writable(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "cannot downgrade an initialized persistence writer to a reader",
        )),
    }
}

/// Establish the process-wide writer lease before `Config::load` or any other mutating loader.
///
/// Ordinary owners and mutating one-shot commands pass `false` and fail startup on contention.
/// Callers that pass `true` still acquire the writer when it is available and only fall back to
/// read-only on failure. An explicitly observational process (including `--new-instance`) must
/// call [`initialize_persistence_reader`] instead so its capability cannot depend on timing.
pub fn initialize_persistence_writer(allow_read_only: bool) -> std::io::Result<PersistenceAccess> {
    initialize_persistence_writer_for_roots(configured_roots(), allow_read_only)
}

/// Establish a writer domain over caller-owned roots. The desktop companion uses its own
/// config/cache domain so it never competes with the playback owner's store roots.
pub fn initialize_persistence_writer_for_roots<I, P>(
    roots: I,
    allow_read_only: bool,
) -> std::io::Result<PersistenceAccess>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    #[cfg(test)]
    {
        let _ = roots;
        let _ = allow_read_only;
        Ok(PersistenceAccess::Writable)
    }
    #[cfg(not(test))]
    {
        let mut state = process_writer().lock().unwrap_or_else(|poisoned| {
            tracing::error!("recovering poisoned persistence writer-lease state");
            poisoned.into_inner()
        });
        initialize_writer_state(
            &mut state,
            roots,
            allow_read_only,
            acquire_state_for_roots,
            crate::util::safe_fs::configure_process_mutations,
        )
    }
}

/// Establish an intentionally read-only process without resolving, creating, or locking any
/// persistence root. Observational CLI commands use this path so they can run beside an owner
/// without briefly becoming a writer when the owner is absent.
pub fn initialize_persistence_reader() -> std::io::Result<PersistenceAccess> {
    let reason: Arc<str> = Arc::from(READ_ONLY_PROCESS_REASON);
    #[cfg(test)]
    {
        Ok(PersistenceAccess::ReadOnly { reason })
    }
    #[cfg(not(test))]
    {
        let mut state = process_writer()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        initialize_reader_state(
            &mut state,
            reason,
            crate::util::safe_fs::configure_process_mutations,
        )
    }
}

pub(crate) fn persistence_access() -> PersistenceAccess {
    #[cfg(test)]
    {
        PersistenceAccess::Writable
    }
    #[cfg(not(test))]
    {
        process_writer()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .access()
    }
}

pub(crate) fn writer_lease_allows_mutation() -> std::io::Result<()> {
    #[cfg(test)]
    {
        Ok(())
    }
    #[cfg(not(test))]
    {
        process_writer()
            .lock()
            .unwrap_or_else(|poisoned| {
                tracing::error!("recovering poisoned persistence writer-lease state");
                poisoned.into_inner()
            })
            .ensure_writable()
    }
}

/// Hold the real multi-root lease implementation around one isolated unit-test operation without
/// consuming the production process-wide `OnceLock`.
#[cfg(all(test, feature = "desktop"))]
pub(crate) fn with_test_writer_domain<I, P, T>(
    roots: I,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let state = acquire_state_for_roots(roots, false)?;
    state.ensure_writable()?;
    let result = operation();
    drop(state);
    result
}

#[cfg(test)]
#[path = "writer_lease/tests.rs"]
mod tests;
