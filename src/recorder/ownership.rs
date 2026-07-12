//! Stable recorder storage layout and per-process namespace leases.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::util::safe_fs;

pub(super) const STATE_DIR_NAME: &str = ".ytt-recorder-state";
pub(super) const LEGACY_REGISTRY_DIR_NAME: &str = ".ytt-recorder-registry";
const OWNERS_DIR_NAME: &str = "owners";
const PENDING_DIR_NAME: &str = "pending";
const LEASE_FILE_NAME: &str = "lease.lock";
const PUBLICATION_LOCK_NAME: &str = "publication.lock";
const LOCK_WAIT: Duration = Duration::from_secs(5);

pub(super) struct OwnerNamespace {
    token: String,
    lease: Mutex<Option<OwnerLease>>,
}

struct OwnerLease {
    temp_root: PathBuf,
    namespace: PathBuf,
    _namespace: safe_fs::PinnedDir,
    _lock: safe_fs::AdvisoryFileLock,
}

/// A lock-proven stale namespace held by both its lease file and directory object. No caller may
/// turn this into pathname-recursive deletion; the claim exists so inventory can stay fenced
/// while preserving an uncertain generation for manual recovery.
pub(super) struct StaleOwnerClaim {
    namespace: PathBuf,
    pinned: safe_fs::PinnedDir,
    _lock: safe_fs::AdvisoryFileLock,
}

impl StaleOwnerClaim {
    pub(super) fn path(&self) -> &Path {
        &self.namespace
    }

    pub(super) fn verify(&self) -> io::Result<()> {
        self.pinned.verify()
    }
}

static ACTIVE_OWNERS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn active_owners() -> &'static Mutex<HashSet<PathBuf>> {
    ACTIVE_OWNERS.get_or_init(|| Mutex::new(HashSet::new()))
}

impl Drop for OwnerLease {
    fn drop(&mut self) {
        active_owners()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.namespace);
    }
}

impl Default for OwnerNamespace {
    fn default() -> Self {
        Self {
            token: random_token(),
            lease: Mutex::new(None),
        }
    }
}

impl OwnerNamespace {
    pub(super) fn ensure_active(&self, temp_root: &Path) -> io::Result<PathBuf> {
        let mut lease = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(active) = lease.as_ref() {
            if active.temp_root == temp_root {
                return Ok(owner_dir(temp_root, &self.token));
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording temp root changed while its owner lease was active",
            ));
        }

        let _publication = acquire_publication_lock(temp_root)?;
        let namespace = owner_dir(temp_root, &self.token);
        safe_fs::ensure_private_dir_durable(&namespace)?;
        let pinned = safe_fs::PinnedDir::open_existing(&namespace, Path::new(""))?;
        let lock = pinned
            .try_lock_child(std::ffi::OsStr::new(LEASE_FILE_NAME), None, true)?
            .map(|(lock, _)| lock)
            .ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "owner token collision"))?;
        active_owners()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(namespace.clone());
        *lease = Some(OwnerLease {
            temp_root: temp_root.to_path_buf(),
            namespace: namespace.clone(),
            _namespace: pinned,
            _lock: lock,
        });
        Ok(namespace)
    }

    pub(super) fn path(&self, temp_root: &Path, id: u64, ext: &str) -> PathBuf {
        owner_dir(temp_root, &self.token).join(format!("rec-{id}.{ext}"))
    }
}

pub(super) fn stable_root(temp_root: &Path) -> PathBuf {
    temp_root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(STATE_DIR_NAME)
}

pub(super) fn pending_dir(temp_root: &Path) -> PathBuf {
    stable_root(temp_root).join(PENDING_DIR_NAME)
}

#[cfg(test)]
pub(crate) fn pending_dir_for_test(temp_root: &Path) -> PathBuf {
    pending_dir(temp_root)
}

pub(super) fn legacy_pending_dir(temp_root: &Path) -> PathBuf {
    temp_root
        .join(LEGACY_REGISTRY_DIR_NAME)
        .join(PENDING_DIR_NAME)
}

pub(super) fn owners_dir(temp_root: &Path) -> PathBuf {
    stable_root(temp_root).join(OWNERS_DIR_NAME)
}

pub(super) fn acquire_publication_lock(temp_root: &Path) -> io::Result<safe_fs::AdvisoryFileLock> {
    let root = stable_root(temp_root);
    safe_fs::ensure_private_dir_durable(&root)?;
    let path = root.join(PUBLICATION_LOCK_NAME);
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match safe_fs::try_lock_private_file(&path)? {
            Some(lock) => return Ok(lock),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "timed out waiting for the recorder registry lock",
                ));
            }
        }
    }
}

pub(super) fn namespace_for_source(temp_root: &Path, source: &Path) -> Option<PathBuf> {
    let relative = source.strip_prefix(owners_dir(temp_root)).ok()?;
    let mut components = relative.components();
    let token = match components.next()? {
        std::path::Component::Normal(token) => token,
        _ => return None,
    };
    let file = match components.next()? {
        std::path::Component::Normal(file) => file,
        _ => return None,
    };
    if components.next().is_some()
        || file.is_empty()
        || token.to_string_lossy().len() != 32
        || !token
            .to_string_lossy()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    Some(owners_dir(temp_root).join(token))
}

pub(super) fn owner_namespaces(temp_root: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(owners_dir(temp_root)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    paths.sort();
    Ok(paths)
}

/// `None` means a live process still holds the namespace. `Some` proves it stale for as long as
/// the returned lease remains held.
pub(super) fn try_claim_stale_owner(namespace: &Path) -> io::Result<Option<StaleOwnerClaim>> {
    if active_owners()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(namespace)
    {
        return Ok(None);
    }
    let pinned = safe_fs::PinnedDir::open_existing(namespace, Path::new(""))?;
    let Some((lock, _)) =
        pinned.try_lock_child(std::ffi::OsStr::new(LEASE_FILE_NAME), None, false)?
    else {
        return Ok(None);
    };
    pinned.verify()?;
    Ok(Some(StaleOwnerClaim {
        namespace: namespace.to_path_buf(),
        pinned,
        _lock: lock,
    }))
}

fn owner_dir(temp_root: &Path, token: &str) -> PathBuf {
    owners_dir(temp_root).join(token)
}

fn random_token() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        bytes[..8].copy_from_slice(&(nanos as u64).to_le_bytes());
        bytes[8..].copy_from_slice(&(sequence ^ u64::from(std::process::id())).to_le_bytes());
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
