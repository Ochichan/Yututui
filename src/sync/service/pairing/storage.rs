use super::{
    EncryptedObject, LOCATOR_KIND, MAX_PAIRING_WAIT_SECONDS, MAX_VAULT_PAYLOAD_BYTES,
    PairingLocator, PrivateStoreSnapshot, SyncPaths, SyncServiceError, VaultError, VaultTransport,
    WebDavProfile, open_saved_webdav_transport,
};
use crate::sync::{ObjectCondition, ObjectKey};

pub(super) fn validate_locator(
    locator: &PairingLocator,
    invite_id: &str,
    now_unix: i64,
) -> Result<(), SyncServiceError> {
    if locator.kind != LOCATOR_KIND
        || locator.schema_version != crate::sync::VAULT_SCHEMA_VERSION
        || locator.invite_id != invite_id
        || locator.expires_at_unix > now_unix.saturating_add(MAX_PAIRING_WAIT_SECONDS)
        || crate::personal_state::PersonalStateV2::empty(locator.dataset_id.clone()).is_err()
    {
        return Err(SyncServiceError::PairingRejected);
    }
    if locator.expires_at_unix < now_unix {
        return Err(SyncServiceError::PairingExpired);
    }
    Ok(())
}

pub(super) fn put_immutable_or_verify<T: VaultTransport + ?Sized>(
    transport: &T,
    key: &ObjectKey,
    object: &EncryptedObject,
) -> Result<(), SyncServiceError> {
    match transport.put(key, object, ObjectCondition::CreateOnly) {
        Ok(_) => Ok(()),
        Err(VaultError::PreconditionFailed) => {
            let (existing, _) = transport
                .get(key, MAX_VAULT_PAYLOAD_BYTES)?
                .ok_or(SyncServiceError::InvalidRemoteData)?;
            if existing.as_bytes() == object.as_bytes() {
                Ok(())
            } else {
                Err(SyncServiceError::InvalidRemoteData)
            }
        }
        Err(error) => match transport.get(key, MAX_VAULT_PAYLOAD_BYTES) {
            Ok(Some((existing, _))) if existing.as_bytes() == object.as_bytes() => Ok(()),
            Ok(Some(_)) => Err(SyncServiceError::InvalidRemoteData),
            Ok(None) | Err(_) => Err(error.into()),
        },
    }
}

pub(super) fn global_pairing_key(
    invite_id: &str,
    file: &str,
) -> Result<ObjectKey, SyncServiceError> {
    pairing_key(None, invite_id, file)
}

pub(super) fn dataset_pairing_key(
    dataset_id: &str,
    invite_id: &str,
    file: &str,
) -> Result<ObjectKey, SyncServiceError> {
    pairing_key(Some(dataset_id), invite_id, file)
}

pub(super) fn checkpoint_key(
    dataset_id: &str,
    epoch: u64,
    checkpoint_hash: &str,
) -> Result<ObjectKey, SyncServiceError> {
    crate::sync::manual::checkpoint_key(dataset_id, epoch, checkpoint_hash).map_err(Into::into)
}

fn pairing_key(
    dataset_id: Option<&str>,
    invite_id: &str,
    file: &str,
) -> Result<ObjectKey, SyncServiceError> {
    let key = match dataset_id {
        Some(dataset_id) => {
            format!("yututui/v2/{dataset_id}/pairing/{invite_id}/{file}")
        }
        None => format!("yututui/v2/pairing/{invite_id}/{file}"),
    };
    ObjectKey::new(key).map_err(Into::into)
}

pub(super) fn regular_file_exists(path: &std::path::Path) -> Result<bool, SyncServiceError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(SyncServiceError::Storage),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(SyncServiceError::Storage),
    }
}

pub(super) fn persist_join_checkpoint(
    paths: &SyncPaths,
    checkpoint: &EncryptedObject,
) -> Result<(), SyncServiceError> {
    if regular_file_exists(paths.pending_join_checkpoint())? {
        let current = read_join_checkpoint(paths)?;
        if current.as_bytes() == checkpoint.as_bytes() {
            return Ok(());
        }
        return Err(SyncServiceError::InvalidRemoteData);
    }
    crate::util::safe_fs::write_owner_only_atomic(
        paths.pending_join_checkpoint(),
        checkpoint.as_bytes(),
    )
    .map_err(|_| SyncServiceError::Storage)
}

fn read_join_checkpoint(paths: &SyncPaths) -> Result<EncryptedObject, SyncServiceError> {
    let bytes = crate::util::safe_fs::read_owner_only_limited(
        paths.pending_join_checkpoint(),
        crate::sync::crypto::MAX_ENCRYPTED_OBJECT_BYTES as u64,
    )
    .map_err(|_| SyncServiceError::Storage)?;
    EncryptedObject::from_bytes(bytes).map_err(Into::into)
}

pub(super) fn load_or_fetch_join_checkpoint(
    paths: &SyncPaths,
    private: &PrivateStoreSnapshot,
    profile: &WebDavProfile,
    invite_id: &str,
) -> Result<(EncryptedObject, bool), SyncServiceError> {
    if regular_file_exists(paths.pending_join_checkpoint())? {
        return read_join_checkpoint(paths).map(|checkpoint| (checkpoint, false));
    }
    let credential = private
        .credential()
        .ok_or(SyncServiceError::MissingCredential)?;
    let transport = open_saved_webdav_transport(profile, credential)?;
    fetch_join_checkpoint(&transport, private.dataset_id(), invite_id)
        .map(|checkpoint| (checkpoint, true))
}

pub(super) fn fetch_join_checkpoint<T: VaultTransport + ?Sized>(
    transport: &T,
    dataset_id: &str,
    invite_id: &str,
) -> Result<EncryptedObject, SyncServiceError> {
    let key = dataset_pairing_key(dataset_id, invite_id, "checkpoint.age")?;
    transport
        .get(&key, MAX_VAULT_PAYLOAD_BYTES)?
        .map(|(checkpoint, _)| checkpoint)
        .ok_or(SyncServiceError::PendingApproval)
}
