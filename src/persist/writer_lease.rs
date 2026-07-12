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
    Writable(PersistenceWriterLease),
    ReadOnly(Arc<str>),
}

impl ProcessWriterState {
    fn access(&self) -> PersistenceAccess {
        match self {
            Self::Writable(_) => PersistenceAccess::Writable,
            Self::ReadOnly(reason) => PersistenceAccess::ReadOnly {
                reason: Arc::clone(reason),
            },
        }
    }

    fn ensure_writable(&self) -> std::io::Result<()> {
        match self {
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
static PROCESS_WRITER: OnceLock<Mutex<Option<ProcessWriterState>>> = OnceLock::new();

#[cfg(not(test))]
fn process_writer() -> &'static Mutex<Option<ProcessWriterState>> {
    PROCESS_WRITER.get_or_init(|| Mutex::new(None))
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
) -> std::io::Result<ProcessWriterState>
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
            return Ok(ProcessWriterState::ReadOnly(Arc::from(format!(
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
                    Ok(ProcessWriterState::ReadOnly(reason))
                } else {
                    Err(read_only_error(&reason))
                };
            }
            Err(error) if allow_read_only => {
                return Ok(ProcessWriterState::ReadOnly(Arc::from(format!(
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
            return Ok(ProcessWriterState::ReadOnly(Arc::from(format!(
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
            Ok(ProcessWriterState::ReadOnly(Arc::from(format!(
                "{error}; changes are not saved"
            ))))
        } else {
            Err(error)
        };
    }
    Ok(ProcessWriterState::Writable(PersistenceWriterLease {
        _locks: locks,
    }))
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
        if let Some(existing) = state.as_ref() {
            if !allow_read_only {
                existing.ensure_writable()?;
            }
            return Ok(existing.access());
        }
        let acquired = acquire_state_for_roots(roots, allow_read_only)?;
        let access = acquired.access();
        let mutation_access = match &access {
            PersistenceAccess::Writable => Ok(()),
            PersistenceAccess::ReadOnly { reason } => Err(Arc::clone(reason)),
        };
        crate::util::safe_fs::configure_process_mutations(mutation_access)?;
        *state = Some(acquired);
        Ok(access)
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
        if let Some(existing) = state.as_ref() {
            return match existing {
                ProcessWriterState::ReadOnly(_) => Ok(existing.access()),
                ProcessWriterState::Writable(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "cannot downgrade an initialized persistence writer to a reader",
                )),
            };
        }
        crate::util::safe_fs::configure_process_mutations(Err(Arc::clone(&reason)))?;
        *state = Some(ProcessWriterState::ReadOnly(Arc::clone(&reason)));
        Ok(PersistenceAccess::ReadOnly { reason })
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
            .as_ref()
            .map(ProcessWriterState::access)
            .unwrap_or_else(|| PersistenceAccess::ReadOnly {
                reason: Arc::from("the process did not initialize its persistence writer lease"),
            })
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
            .as_ref()
            .ok_or_else(|| {
                read_only_error("the process did not initialize its persistence writer lease")
            })?
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
mod tests {
    use super::*;

    fn test_dir(label: &str) -> PathBuf {
        let mut suffix = [0_u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let suffix = suffix
            .into_iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        std::env::temp_dir().join(format!(
            "yututui-writer-lease-{}-{label}-{suffix}",
            std::process::id()
        ))
    }

    fn assert_read_only(state: &ProcessWriterState) {
        assert!(matches!(state.access(), PersistenceAccess::ReadOnly { .. }));
        assert_eq!(
            state.ensure_writable().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn configured_roots_include_the_local_index_storage_domain() {
        let roots = configured_roots();
        let local_index_root = crate::paths::local_index_data_dir().unwrap();

        assert!(roots.contains(&local_index_root));
        assert_eq!(
            roots.len(),
            4,
            "all four configured storage domains remain leased"
        );
    }

    #[test]
    fn shared_root_is_deduplicated_and_only_one_writer_wins() {
        let root = test_dir("dedupe");
        let primary = acquire_state_for_roots([&root, &root, &root], false).unwrap();
        primary.ensure_writable().unwrap();
        let secondary = acquire_state_for_roots([&root], true).unwrap();
        assert_read_only(&secondary);
        drop(primary);
        acquire_state_for_roots([&root], false)
            .unwrap()
            .ensure_writable()
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shared_config_with_split_data_and_cache_blocks_second_writer() {
        let shared = test_dir("shared-config");
        let a_data = test_dir("a-data");
        let a_cache = test_dir("a-cache");
        let b_data = test_dir("b-data");
        let b_cache = test_dir("b-cache");
        let primary = acquire_state_for_roots([&shared, &a_data, &a_cache], false).unwrap();
        let secondary = acquire_state_for_roots([&shared, &b_data, &b_cache], true).unwrap();
        assert_read_only(&secondary);
        drop(primary);
        for path in [shared, a_data, a_cache, b_data, b_cache] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn shared_cache_with_split_config_and_data_blocks_second_writer() {
        let shared = test_dir("shared-cache");
        let a_config = test_dir("a-config");
        let a_data = test_dir("a-data");
        let b_config = test_dir("b-config");
        let b_data = test_dir("b-data");
        let primary = acquire_state_for_roots([&a_config, &a_data, &shared], false).unwrap();
        let secondary = acquire_state_for_roots([&b_config, &b_data, &shared], true).unwrap();
        assert_read_only(&secondary);
        drop(primary);
        for path in [shared, a_config, a_data, b_config, b_data] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn fully_split_roots_allow_independent_writers() {
        let a = [
            test_dir("a-config"),
            test_dir("a-data"),
            test_dir("a-cache"),
        ];
        let b = [
            test_dir("b-config"),
            test_dir("b-data"),
            test_dir("b-cache"),
        ];
        let first = acquire_state_for_roots(&a, false).unwrap();
        let second = acquire_state_for_roots(&b, false).unwrap();
        first.ensure_writable().unwrap();
        second.ensure_writable().unwrap();
        drop((first, second));
        for path in a.into_iter().chain(b) {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn partial_acquisition_failure_releases_all_earlier_roots() {
        let earlier = test_dir("a-earlier");
        let later = test_dir("z-later");
        let held_later = crate::util::safe_fs::try_lock_private_file(
            &normalize_root(&later).unwrap().join(WRITER_LEASE_FILE),
        )
        .unwrap()
        .unwrap();

        let failed = acquire_state_for_roots([&earlier, &later], true).unwrap();
        assert_read_only(&failed);
        let earlier_lock = crate::util::safe_fs::try_lock_private_file(
            &normalize_root(&earlier).unwrap().join(WRITER_LEASE_FILE),
        )
        .unwrap();
        assert!(
            earlier_lock.is_some(),
            "partial lease stranded the first root"
        );

        drop((earlier_lock, held_later));
        let _ = std::fs::remove_dir_all(earlier);
        let _ = std::fs::remove_dir_all(later);
    }

    #[test]
    fn primary_remains_writable_until_drop_then_successor_acquires_every_root() {
        let roots = [test_dir("config"), test_dir("data"), test_dir("cache")];
        let primary = acquire_state_for_roots(&roots, false).unwrap();
        primary.ensure_writable().unwrap();
        assert_read_only(&acquire_state_for_roots(&roots, true).unwrap());

        drop(primary);
        let successor = acquire_state_for_roots(&roots, false).unwrap();
        successor.ensure_writable().unwrap();
        drop(successor);
        for path in roots {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}
