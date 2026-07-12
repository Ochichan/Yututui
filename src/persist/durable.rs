use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use super::{
    INTENT_JOURNAL_MAX_BYTES, acquire_private_lock, parse_journal_order, remove_file_if_exists,
    sibling_with_suffix,
};

const PROCESS_EPOCH_MAX_BYTES: u64 = 1024;
const PROCESS_EPOCH_SCAN_LIMIT: usize = 4096;

#[derive(Clone)]
pub(super) struct AcceptedJournalOrder {
    pub(super) order: JournalOrder,
    pub(super) error: Option<Arc<str>>,
}

pub(super) struct JournalOrderSource {
    pub(super) process_epoch: u64,
    pub(super) allocation_error: Option<Arc<str>>,
    pub(super) next_sequence: Mutex<u128>,
}

impl JournalOrderSource {
    pub(super) fn unavailable(reason: Arc<str>) -> Self {
        Self {
            process_epoch: 0,
            allocation_error: Some(reason),
            next_sequence: Mutex::new(0),
        }
    }

    pub(super) fn allocate() -> Self {
        #[cfg(test)]
        {
            static NEXT_TEST_EPOCH: AtomicU64 = AtomicU64::new(1);
            Self::for_test(NEXT_TEST_EPOCH.fetch_add(1, Ordering::Relaxed))
        }
        #[cfg(not(test))]
        match allocate_process_epoch() {
            Ok(process_epoch) => Self {
                process_epoch,
                allocation_error: None,
                next_sequence: Mutex::new(0),
            },
            Err(error) => {
                let error: Arc<str> = Arc::from(format!(
                    "failed to reserve persistence process epoch: {error}"
                ));
                tracing::error!(error = %error, "persistence ordering is unavailable");
                Self {
                    process_epoch: 0,
                    allocation_error: Some(error),
                    next_sequence: Mutex::new(0),
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(process_epoch: u64) -> Self {
        Self {
            process_epoch,
            allocation_error: None,
            next_sequence: Mutex::new(0),
        }
    }

    pub(super) fn accept(&self) -> AcceptedJournalOrder {
        let mut sequence = self
            .next_sequence
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(next_sequence) = sequence.checked_add(1) else {
            return AcceptedJournalOrder {
                order: JournalOrder {
                    process_epoch: self.process_epoch,
                    sequence: u128::MAX,
                    generation: JournalGeneration::for_order(self.process_epoch, u128::MAX),
                },
                error: Some(Arc::from("persistence acceptance sequence exhausted")),
            };
        };
        *sequence = next_sequence;
        AcceptedJournalOrder {
            order: JournalOrder {
                process_epoch: self.process_epoch,
                sequence: *sequence,
                generation: JournalGeneration::for_order(self.process_epoch, *sequence),
            },
            error: self.allocation_error.clone(),
        }
    }
}

#[cfg_attr(test, allow(dead_code))]
fn allocate_process_epoch() -> std::io::Result<u64> {
    let config_path = crate::config::config_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "configuration directory is unavailable for persistence ordering",
        )
    })?;
    let parent = config_path
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid persistence ordering directory"))?;
    allocate_process_epoch_at(&parent.join(".ytt-persist-order.json"))
}

pub(super) fn allocate_process_epoch_at(path: &Path) -> std::io::Result<u64> {
    super::ensure_persistence_writes_allowed()?;
    let lock_path = sibling_with_suffix(path, ".lock")
        .ok_or_else(|| std::io::Error::other("invalid persistence epoch lock path"))?;
    let _lock = acquire_private_lock(&lock_path, "persistence process epoch")?;
    let (counter_epoch, counter_corrupt) =
        match crate::util::safe_fs::read_no_symlink_limited(path, PROCESS_EPOCH_MAX_BYTES) {
            Ok(bytes) => {
                let parsed = serde_json::from_slice::<serde_json::Value>(&bytes)
                    .ok()
                    .filter(|record| record.get("v").and_then(|value| value.as_u64()) == Some(1))
                    .and_then(|record| {
                        let value = record.get("last_epoch")?;
                        value
                            .as_str()
                            .and_then(|value| value.parse().ok())
                            .or_else(|| value.as_u64())
                    });
                (parsed.unwrap_or(0), parsed.is_none())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (0, false),
            Err(error) => return Err(error),
        };
    let observed_epoch = observed_process_epoch(path)?;
    if counter_corrupt && observed_epoch == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "corrupt persistence process epoch has no safe recovery frontier",
        ));
    }
    let process_epoch = counter_epoch
        .max(observed_epoch)
        .checked_add(1)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "persistence process epoch exhausted",
            )
        })?;
    let marker_path = process_epoch_marker_path(path, process_epoch)?;
    write_epoch_marker(&marker_path, process_epoch)?;
    crate::util::safe_fs::write_private_atomic(
        path,
        serde_json::json!({
            "v": 1,
            "last_epoch": process_epoch.to_string(),
        })
        .to_string()
        .as_bytes(),
    )?;
    cleanup_epoch_markers(path, process_epoch)?;
    Ok(process_epoch)
}

fn process_epoch_marker_path(counter_path: &Path, epoch: u64) -> std::io::Result<PathBuf> {
    let parent = counter_path
        .parent()
        .ok_or_else(|| std::io::Error::other("invalid persistence epoch marker directory"))?;
    Ok(parent.join(format!(".ytt-persist-order.{epoch}.marker")))
}

fn write_epoch_marker(path: &Path, epoch: u64) -> std::io::Result<()> {
    let bytes = serde_json::json!({ "v": 1, "process_epoch": epoch.to_string() })
        .to_string()
        .into_bytes();
    match crate::util::safe_fs::read_no_symlink_limited(path, PROCESS_EPOCH_MAX_BYTES) {
        Ok(existing) if existing == bytes => Ok(()),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "persistence epoch marker collision",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::util::safe_fs::write_private_atomic(path, &bytes)
        }
        Err(error) => Err(error),
    }
}

fn observed_process_epoch(counter_path: &Path) -> std::io::Result<u64> {
    let mut directories = Vec::new();
    if let Some(parent) = counter_path.parent() {
        directories.push(parent.to_path_buf());
    }
    #[cfg(not(test))]
    {
        for path in [
            crate::config::config_path(),
            crate::library::library_path(),
            crate::session::session_cache_path(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(parent) = path.parent()
                && !directories.iter().any(|existing| existing == parent)
            {
                directories.push(parent.to_path_buf());
            }
        }
    }
    let mut maximum = 0_u64;
    for directory in directories {
        maximum = maximum.max(observed_process_epoch_in_dir(&directory)?);
    }
    Ok(maximum)
}

fn observed_process_epoch_in_dir(directory: &Path) -> std::io::Result<u64> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut relevant = 0_usize;
    let mut maximum = 0_u64;
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if let Some(epoch) =
            epoch_from_marker_name(file_name).or_else(|| epoch_from_sidecar_name(file_name))
        {
            relevant += 1;
            maximum = maximum.max(epoch);
        } else if file_name.ends_with(".intent.jsonl") {
            relevant += 1;
            let bytes = crate::util::safe_fs::read_no_symlink_limited(
                &entry.path(),
                INTENT_JOURNAL_MAX_BYTES,
            )?;
            let text = String::from_utf8_lossy(&bytes);
            for line in text.lines() {
                if let Ok(record) = serde_json::from_str::<serde_json::Value>(line)
                    && let Some(order) = parse_journal_order(&record)
                {
                    maximum = maximum.max(order.process_epoch);
                }
            }
        }
        if relevant > PROCESS_EPOCH_SCAN_LIMIT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "too many persistence ordering artifacts to reserve a safe epoch",
            ));
        }
    }
    Ok(maximum)
}

fn epoch_from_marker_name(name: &str) -> Option<u64> {
    name.strip_prefix(".ytt-persist-order.")?
        .strip_suffix(".marker")?
        .parse()
        .ok()
}

fn epoch_from_sidecar_name(name: &str) -> Option<u64> {
    let (_, suffix) = name.rsplit_once(".intent.")?;
    let mut parts = suffix.split('.');
    let epoch = parts.next()?.parse().ok()?;
    let _sequence = parts.next()?.parse::<u128>().ok()?;
    let generation = parts.next()?;
    if JournalGeneration::from_hex(generation).is_none()
        || parts.next()? != "json"
        || parts.next().is_some()
    {
        return None;
    }
    Some(epoch)
}

fn cleanup_epoch_markers(counter_path: &Path, retained_epoch: u64) -> std::io::Result<()> {
    let Some(parent) = counter_path.parent() else {
        return Ok(());
    };
    let mut inspected = 0_usize;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(epoch) = epoch_from_marker_name(file_name) else {
            continue;
        };
        inspected += 1;
        if inspected > PROCESS_EPOCH_SCAN_LIMIT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "too many persistence epoch markers to clean safely",
            ));
        }
        if epoch != retained_epoch {
            remove_file_if_exists(
                &entry.path(),
                "failed to remove old persistence epoch marker",
            );
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct JournalGeneration(pub(super) [u8; 16]);

impl JournalGeneration {
    fn for_order(process_epoch: u64, sequence: u128) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"ytt-persist-generation-v1");
        hasher.update(process_epoch.to_be_bytes());
        hasher.update(sequence.to_be_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        Self(bytes)
    }

    pub(super) fn to_hex(self) -> String {
        let mut out = String::with_capacity(32);
        for byte in self.0 {
            let _ = write!(&mut out, "{byte:02x}");
        }
        out
    }

    pub(super) fn from_hex(value: &str) -> Option<Self> {
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0_u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct JournalOrder {
    pub(super) process_epoch: u64,
    pub(super) sequence: u128,
    pub(super) generation: JournalGeneration,
}
