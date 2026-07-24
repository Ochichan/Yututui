use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize};

use super::crypto::{EncryptedObject, sha256_domain_hex};
use super::error::VaultError;

const MAX_OBJECT_KEY_BYTES: usize = 1_024;
const MAX_OBJECT_SEGMENTS: usize = 16;
const MAX_OBJECT_SEGMENT_BYTES: usize = 128;
const MAX_LIST_RESOURCES: usize = 10_000;
const LOCK_WAIT: Duration = Duration::from_secs(5);
const CIPHERTEXT_OVERHEAD_LIMIT: usize = 2 * 1024 * 1024;

/// A relative, traversal-free key in the encrypted vault.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ObjectKey(String);

impl<'de> Deserialize<'de> for ObjectKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl ObjectKey {
    pub fn new(value: impl Into<String>) -> Result<Self, VaultError> {
        let value = value.into();
        validate_key(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn as_relative_path(&self) -> PathBuf {
        self.0.split('/').collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectCondition {
    CreateOnly,
    Match(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMetadata {
    pub key: ObjectKey,
    pub etag: String,
    pub content_length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectWriteResult {
    Created(ObjectMetadata),
    Updated(ObjectMetadata),
    AlreadyPresent(ObjectMetadata),
}

pub trait VaultTransport {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError>;

    fn put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError>;

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError>;
}

/// A deterministic encrypted-object store used by vault and lifecycle conformance tests.
///
/// The directory is private because tests also use it to exercise response-loss and tamper
/// recovery. Contents remain age ciphertext; the transport has no plaintext write API.
pub struct FileVaultTransport {
    root: PathBuf,
    lock_path: PathBuf,
}

impl FileVaultTransport {
    pub fn create(root: impl Into<PathBuf>) -> Result<Self, VaultError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(VaultError::InvalidObjectKey);
        }
        crate::util::safe_fs::ensure_private_dir(&root).map_err(|_| VaultError::StorageFailed)?;
        validate_directory(&root)?;
        Ok(Self {
            lock_path: root.join(".vault.lock"),
            root,
        })
    }

    pub fn open(root: impl Into<PathBuf>) -> Result<Self, VaultError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(VaultError::InvalidObjectKey);
        }
        validate_directory(&root)?;
        Ok(Self {
            lock_path: root.join(".vault.lock"),
            root,
        })
    }

    fn path_for(&self, key: &ObjectKey) -> Result<PathBuf, VaultError> {
        validate_directory(&self.root)?;
        validate_key(key.as_str())?;
        let relative = key.as_relative_path();
        if relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(VaultError::InvalidObjectKey);
        }
        Ok(self.root.join(relative))
    }

    fn acquire_lock(&self) -> Result<crate::util::safe_fs::AdvisoryFileLock, VaultError> {
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match crate::util::safe_fs::try_lock_private_file(&self.lock_path) {
                Ok(Some(lock)) => return Ok(lock),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                Ok(None) => return Err(VaultError::StorageBusy),
                Err(_) => return Err(VaultError::StorageFailed),
            }
        }
    }

    fn read_locked(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        let path = self.path_for(key)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(VaultError::StorageFailed),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(VaultError::StorageFailed);
        }
        let hard_limit = max_bytes
            .saturating_add(CIPHERTEXT_OVERHEAD_LIMIT)
            .min(super::crypto::MAX_ENCRYPTED_OBJECT_BYTES);
        let bytes = crate::util::safe_fs::read_private_file_limited(&path, hard_limit as u64)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    VaultError::PayloadTooLarge
                } else {
                    VaultError::StorageFailed
                }
            })?;
        let object = EncryptedObject::from_bytes(bytes.clone())
            .map_err(|_| VaultError::InvalidEncryptedObject)?;
        let metadata = metadata_for(key, &bytes)?;
        Ok(Some((object, metadata)))
    }
}

impl VaultTransport for FileVaultTransport {
    fn get(
        &self,
        key: &ObjectKey,
        max_bytes: usize,
    ) -> Result<Option<(EncryptedObject, ObjectMetadata)>, VaultError> {
        let _lock = self.acquire_lock()?;
        self.read_locked(key, max_bytes)
    }

    fn put(
        &self,
        key: &ObjectKey,
        object: &EncryptedObject,
        condition: ObjectCondition,
    ) -> Result<ObjectWriteResult, VaultError> {
        if !object.is_locally_produced() {
            return Err(VaultError::InvalidEncryptedObject);
        }
        let bytes = object.as_bytes();
        if bytes.len() > super::MAX_VAULT_PAYLOAD_BYTES.saturating_add(CIPHERTEXT_OVERHEAD_LIMIT) {
            return Err(VaultError::PayloadTooLarge);
        }

        let _lock = self.acquire_lock()?;
        let path = self.path_for(key)?;
        let current = self.read_locked(key, super::MAX_VAULT_PAYLOAD_BYTES)?;
        match (&condition, &current) {
            (ObjectCondition::CreateOnly, Some(_)) => {
                return Err(VaultError::PreconditionFailed);
            }
            (ObjectCondition::Match(expected), Some((_, metadata)))
                if &metadata.etag != expected =>
            {
                return Err(VaultError::PreconditionFailed);
            }
            (ObjectCondition::Match(_), None) => return Err(VaultError::PreconditionFailed),
            _ => {}
        }
        if let Some((_, metadata)) = &current
            && metadata.etag == metadata_for(key, bytes)?.etag
        {
            return Ok(ObjectWriteResult::AlreadyPresent(metadata.clone()));
        }

        let parent = path.parent().ok_or(VaultError::InvalidObjectKey)?;
        ensure_private_path(&self.root, parent)?;
        crate::util::safe_fs::write_private_atomic(&path, bytes)
            .map_err(|_| VaultError::StorageFailed)?;
        let metadata = metadata_for(key, bytes)?;
        match current {
            Some(_) => Ok(ObjectWriteResult::Updated(metadata)),
            None => Ok(ObjectWriteResult::Created(metadata)),
        }
    }

    fn list(
        &self,
        prefix: &ObjectKey,
        max_resources: usize,
    ) -> Result<Vec<ObjectMetadata>, VaultError> {
        let _lock = self.acquire_lock()?;
        let limit = max_resources.min(MAX_LIST_RESOURCES);
        if max_resources == 0 || max_resources > MAX_LIST_RESOURCES {
            return Err(VaultError::ResourceLimitExceeded);
        }
        let start = self.path_for(prefix)?;
        let start_metadata = match fs::symlink_metadata(&start) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(VaultError::StorageFailed),
        };
        if start_metadata.file_type().is_symlink() {
            return Err(VaultError::StorageFailed);
        }
        let mut pending = vec![start];
        let mut result = Vec::new();
        let mut scanned_resources = 0_usize;
        let mut scanned_bytes = 0_u64;
        while let Some(path) = pending.pop() {
            scanned_resources = scanned_resources.saturating_add(1);
            if scanned_resources > MAX_LIST_RESOURCES {
                return Err(VaultError::ResourceLimitExceeded);
            }
            let metadata = fs::symlink_metadata(&path).map_err(|_| VaultError::StorageFailed)?;
            if metadata.file_type().is_symlink() {
                return Err(VaultError::StorageFailed);
            }
            if metadata.is_dir() {
                for entry in fs::read_dir(&path).map_err(|_| VaultError::StorageFailed)? {
                    let entry = entry.map_err(|_| VaultError::StorageFailed)?;
                    if entry.file_name() == OsStr::new(".vault.lock") {
                        continue;
                    }
                    if pending.len().saturating_add(scanned_resources) >= MAX_LIST_RESOURCES {
                        return Err(VaultError::ResourceLimitExceeded);
                    }
                    pending.push(entry.path());
                }
                continue;
            }
            if !metadata.is_file() {
                return Err(VaultError::StorageFailed);
            }
            if result.len() == limit {
                return Err(VaultError::ResourceLimitExceeded);
            }
            scanned_bytes = scanned_bytes
                .checked_add(metadata.len())
                .ok_or(VaultError::ResourceLimitExceeded)?;
            if scanned_bytes > (super::MAX_VAULT_PAYLOAD_BYTES + CIPHERTEXT_OVERHEAD_LIMIT) as u64 {
                return Err(VaultError::ResourceLimitExceeded);
            }
            let relative = path
                .strip_prefix(&self.root)
                .map_err(|_| VaultError::StorageFailed)?;
            let key = ObjectKey::new(
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/"),
            )?;
            let bytes = crate::util::safe_fs::read_private_file_limited(
                &path,
                (super::MAX_VAULT_PAYLOAD_BYTES + CIPHERTEXT_OVERHEAD_LIMIT) as u64,
            )
            .map_err(|_| VaultError::StorageFailed)?;
            result.push(metadata_for(&key, &bytes)?);
        }
        result.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(result)
    }
}

fn validate_key(value: &str) -> Result<(), VaultError> {
    if value.is_empty()
        || value.len() > MAX_OBJECT_KEY_BYTES
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        return Err(VaultError::InvalidObjectKey);
    }
    let segments = value.split('/').collect::<Vec<_>>();
    if segments.len() > MAX_OBJECT_SEGMENTS
        || segments.iter().any(|segment| {
            segment.is_empty()
                || *segment == "."
                || *segment == ".."
                || segment.starts_with('.')
                || segment.ends_with('.')
                || segment.len() > MAX_OBJECT_SEGMENT_BYTES
                || windows_reserved_name(segment)
                || !segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_' | b'.')
                })
        })
    {
        return Err(VaultError::InvalidObjectKey);
    }
    Ok(())
}

fn windows_reserved_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    matches!(stem, "con" | "prn" | "aux" | "nul")
        || stem.strip_prefix("com").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || stem.strip_prefix("lpt").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

fn validate_directory(path: &Path) -> Result<(), VaultError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| VaultError::StorageFailed)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(VaultError::StorageFailed);
    }
    Ok(())
}

fn ensure_private_path(root: &Path, target: &Path) -> Result<(), VaultError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| VaultError::InvalidObjectKey)?;
    let mut current = root.to_path_buf();
    validate_directory(&current)?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(VaultError::InvalidObjectKey);
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(VaultError::StorageFailed);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::util::safe_fs::ensure_private_dir(&current)
                    .map_err(|_| VaultError::StorageFailed)?;
            }
            Err(_) => return Err(VaultError::StorageFailed),
        }
    }
    Ok(())
}

fn metadata_for(key: &ObjectKey, bytes: &[u8]) -> Result<ObjectMetadata, VaultError> {
    Ok(ObjectMetadata {
        key: key.clone(),
        etag: sha256_domain_hex(b"yututui-vault-etag-v1", &[bytes]),
        content_length: bytes
            .len()
            .try_into()
            .map_err(|_| VaultError::PayloadTooLarge)?,
    })
}
