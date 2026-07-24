use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use age::secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::personal_state::{
    CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
};

use super::crypto::{base64url_encode, derive_recovery_signing_key, sha256_domain_hex};
use super::error::VaultError;
use super::membership::{MembershipAnchor, RecoveryCutoff};

const RECOVERY_KIT_KIND: &str = "yututui_recovery_kit";
const RECOVERY_KIT_SCHEMA_VERSION: u32 = 1;
const RECOVERY_KIT_MAX_BYTES: u64 = 64 * 1024;
const CHECKSUM_DOMAIN: &[u8] = b"yututui-recovery-kit-checksum-v1";
const MAX_RECOVERY_CHECKPOINT_CANDIDATES: usize = 16;

/// The offline recovery authority for one encrypted dataset.
///
/// It intentionally contains neither a WebDAV password nor a device key. Debug output and errors
/// are redacted; JSON is only produced by the explicit export method.
pub struct RecoveryKit {
    dataset_id: String,
    recovery_identity: age::x25519::Identity,
    checksum: String,
    endpoint: Option<String>,
}

impl fmt::Debug for RecoveryKit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoveryKit")
            .field("dataset_id", &"[redacted]")
            .field("recovery_identity", &"[redacted]")
            .field("checksum", &"[redacted]")
            .field("endpoint", &self.endpoint.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

/// Result of replacing every lost device with one recovery-authorized device.
///
/// The secret material is intentionally not `Debug` or serializable.
pub struct RecoveryResult {
    pub device_secrets: super::crypto::DeviceSecretMaterial,
    pub membership: super::membership::MembershipChain,
    pub state: crate::personal_state::PersonalStateV2,
    pub checkpoint: super::checkpoint::SignedCheckpoint,
    pub encrypted_checkpoint: super::crypto::EncryptedObject,
}

impl RecoveryKit {
    pub fn generate(
        dataset_id: impl Into<String>,
        endpoint: Option<String>,
    ) -> Result<Self, VaultError> {
        let dataset_id = dataset_id.into();
        validate_dataset_id(&dataset_id)?;
        validate_endpoint(endpoint.as_deref())?;
        let recovery_identity = age::x25519::Identity::generate();
        Self::from_identity(dataset_id, recovery_identity, endpoint)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, VaultError> {
        if bytes.len() as u64 > RECOVERY_KIT_MAX_BYTES {
            return Err(VaultError::PayloadTooLarge);
        }
        let mut disk: RecoveryKitDisk =
            serde_json::from_slice(bytes).map_err(|_| VaultError::RecoveryKitInvalid)?;
        validate_dataset_id(&disk.dataset_id)?;
        validate_endpoint(disk.endpoint.as_deref())?;
        if disk.kind != RECOVERY_KIT_KIND
            || disk.schema_version != RECOVERY_KIT_SCHEMA_VERSION
            || !valid_checksum_encoding(&disk.checksum)
        {
            disk.recovery_age_identity.zeroize();
            return Err(VaultError::RecoveryKitInvalid);
        }
        let identity = age::x25519::Identity::from_str(&disk.recovery_age_identity)
            .map_err(|_| VaultError::RecoveryKitInvalid);
        disk.recovery_age_identity.zeroize();
        let identity = identity?;
        let kit = Self {
            dataset_id: std::mem::take(&mut disk.dataset_id),
            recovery_identity: identity,
            checksum: std::mem::take(&mut disk.checksum),
            endpoint: disk.endpoint.take(),
        };
        kit.validate_checksum()?;
        Ok(kit)
    }

    pub fn to_json(&self) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        self.validate_checksum()?;
        let secret = self.recovery_identity.to_string();
        let disk = RecoveryKitDiskRef {
            kind: RECOVERY_KIT_KIND,
            schema_version: RECOVERY_KIT_SCHEMA_VERSION,
            dataset_id: &self.dataset_id,
            recovery_age_identity: secret.expose_secret(),
            checksum: &self.checksum,
            endpoint: self.endpoint.as_deref(),
        };
        let bytes =
            serde_json::to_vec_pretty(&disk).map_err(|_| VaultError::SerializationFailed)?;
        if bytes.len() as u64 > RECOVERY_KIT_MAX_BYTES {
            return Err(VaultError::PayloadTooLarge);
        }
        Ok(Zeroizing::new(bytes))
    }

    /// Export with current-user-only permissions and verify the exact bytes by reading them back.
    ///
    /// A successful return is the setup flow's required "recovery kit saved" confirmation.
    pub fn export_confirmed(&self, path: &Path) -> Result<String, VaultError> {
        let bytes = self.to_json()?;
        crate::util::safe_fs::write_owner_only_atomic(path, &bytes)
            .map_err(|_| VaultError::StorageFailed)?;
        let readback = Zeroizing::new(
            crate::util::safe_fs::read_owner_only_limited(path, RECOVERY_KIT_MAX_BYTES)
                .map_err(|_| VaultError::StorageFailed)?,
        );
        let restored = Self::from_json(&readback)?;
        if restored.checksum != self.checksum || restored.dataset_id != self.dataset_id {
            return Err(VaultError::RecoveryKitInvalid);
        }
        Ok(self.checksum.clone())
    }

    pub fn dataset_id(&self) -> &str {
        &self.dataset_id
    }

    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    pub fn recovery_recipient(&self) -> String {
        self.recovery_identity.to_public().to_string()
    }

    pub fn recovery_verifying_key(&self) -> Result<String, VaultError> {
        let key = derive_recovery_signing_key(&self.recovery_identity, &self.dataset_id)?;
        Ok(base64url_encode(key.verifying_key().as_bytes()))
    }

    /// Recover the highest contiguous authenticated checkpoint supplied by the vault listing.
    ///
    /// Every previously active device is explicitly revoked at its covered sequence, the
    /// membership epoch rotates, and the replacement checkpoint excludes all old recipients.
    /// A retained local anchor, when available, makes a stale or forked listing fail closed.
    /// With only the static recovery kit, callers must provide every object returned by the
    /// bounded checkpoint listing; no static kit can detect a server that hides its newest object.
    pub fn recover(
        &self,
        encrypted_checkpoints: &[super::crypto::EncryptedObject],
        retained_anchor: Option<&super::checkpoint::CheckpointAnchor>,
        new_device_name: impl Into<String>,
    ) -> Result<RecoveryResult, VaultError> {
        let recovery_verifying_key = self.recovery_verifying_key()?;
        let anchor = MembershipAnchor::RecoveryVerifyingKey(recovery_verifying_key);
        let previous =
            self.select_latest_checkpoint(encrypted_checkpoints, retained_anchor, &anchor)?;
        let previous_hash = previous.hash()?;
        let previous_membership = previous.payload.membership.verify(&anchor)?;
        let device_id = DeviceId::new(format!("dev-{}", super::crypto::random_id_hex::<16>()?))
            .map_err(|_| VaultError::InvalidDeviceId)?;
        let device_secrets = super::crypto::DeviceSecretMaterial::generate_for(device_id.as_str())?;
        let new_device = DeviceRecord {
            device_id: device_id.clone(),
            name: new_device_name.into(),
            revoked: false,
            public_identity: Some(device_secrets.public_identity()),
        };
        validate_device_name(&new_device.name)?;

        let mut cutoffs = previous_membership
            .active_devices()
            .map(|device| RecoveryCutoff {
                device_id: device.device_id.clone(),
                last_accepted_sequence: previous
                    .payload
                    .state
                    .version_vector
                    .observed(&device.device_id),
            })
            .collect::<Vec<_>>();
        cutoffs.sort_by(|left, right| left.device_id.cmp(&right.device_id));

        let mut membership = previous.payload.membership.clone();
        let recovery_signing_key = self.signing_key()?;
        let verified = membership.append_recovery(
            &anchor,
            &recovery_signing_key,
            new_device.clone(),
            cutoffs,
        )?;
        let state = recovered_state(
            previous.payload.state,
            &device_id,
            new_device,
            &previous_membership,
        )?;
        if state.device_registry != verified.devices {
            return Err(VaultError::RegistryMismatch);
        }

        let checkpoint_anchor = super::checkpoint::CheckpointAnchor::from_trusted(
            previous.payload.checkpoint_sequence,
            previous_hash,
        )?;
        let checkpoint = super::checkpoint::SignedCheckpoint::create(
            membership.clone(),
            &anchor,
            device_id,
            device_secrets.signing_key(),
            &checkpoint_anchor,
            state.clone(),
        )?;
        let encrypted_checkpoint = checkpoint.encrypt(&anchor)?;
        Ok(RecoveryResult {
            device_secrets,
            membership,
            state,
            checkpoint,
            encrypted_checkpoint,
        })
    }

    fn select_latest_checkpoint(
        &self,
        encrypted_checkpoints: &[super::crypto::EncryptedObject],
        retained_anchor: Option<&super::checkpoint::CheckpointAnchor>,
        membership_anchor: &MembershipAnchor,
    ) -> Result<super::checkpoint::SignedCheckpoint, VaultError> {
        if encrypted_checkpoints.is_empty()
            || encrypted_checkpoints.len() > MAX_RECOVERY_CHECKPOINT_CANDIDATES
        {
            return Err(VaultError::ResourceLimitExceeded);
        }
        let mut candidates = BTreeMap::<u64, (String, super::checkpoint::SignedCheckpoint)>::new();
        for encrypted in encrypted_checkpoints {
            let checkpoint = super::checkpoint::SignedCheckpoint::decrypt(
                encrypted,
                &self.recovery_identity,
                membership_anchor,
            )?;
            if checkpoint.payload.dataset_id != self.dataset_id {
                return Err(VaultError::RecoveryKitInvalid);
            }
            let hash = checkpoint.hash()?;
            match candidates.get(&checkpoint.payload.checkpoint_sequence) {
                Some((existing_hash, _)) if existing_hash == &hash => continue,
                Some(_) => return Err(VaultError::RollbackDetected),
                None => {
                    candidates.insert(checkpoint.payload.checkpoint_sequence, (hash, checkpoint));
                }
            }
        }

        let (mut previous, start_sequence) = match retained_anchor {
            Some(anchor) => {
                let hash = anchor
                    .checkpoint_hash
                    .as_deref()
                    .ok_or(VaultError::RollbackDetected)?;
                (
                    Some((anchor.checkpoint_sequence, hash)),
                    anchor.checkpoint_sequence,
                )
            }
            None => (None, 0),
        };
        let mut saw_candidate_at_or_after_anchor = retained_anchor.is_none();
        for (sequence, (hash, checkpoint)) in candidates.range(start_sequence..) {
            saw_candidate_at_or_after_anchor = true;
            if let Some((previous_sequence, previous_hash)) = previous {
                if *sequence == previous_sequence {
                    if hash != previous_hash {
                        return Err(VaultError::RollbackDetected);
                    }
                    previous = Some((*sequence, hash));
                    continue;
                }
                if *sequence != previous_sequence.saturating_add(1)
                    || checkpoint.payload.previous_checkpoint_hash.as_deref() != Some(previous_hash)
                {
                    return Err(VaultError::SequenceGap);
                }
            }
            previous = Some((*sequence, hash));
        }
        if !saw_candidate_at_or_after_anchor {
            return Err(VaultError::RollbackDetected);
        }
        candidates
            .pop_last()
            .map(|(_, (_, checkpoint))| checkpoint)
            .ok_or(VaultError::RecoveryKitInvalid)
    }

    pub(crate) fn signing_key(&self) -> Result<ed25519_dalek::SigningKey, VaultError> {
        derive_recovery_signing_key(&self.recovery_identity, &self.dataset_id)
    }

    fn from_identity(
        dataset_id: String,
        recovery_identity: age::x25519::Identity,
        endpoint: Option<String>,
    ) -> Result<Self, VaultError> {
        let secret = recovery_identity.to_string();
        let checksum = checksum(&dataset_id, secret.expose_secret(), endpoint.as_deref());
        Ok(Self {
            dataset_id,
            recovery_identity,
            checksum,
            endpoint,
        })
    }

    fn validate_checksum(&self) -> Result<(), VaultError> {
        let secret = self.recovery_identity.to_string();
        let expected = checksum(
            &self.dataset_id,
            secret.expose_secret(),
            self.endpoint.as_deref(),
        );
        if expected == self.checksum {
            Ok(())
        } else {
            Err(VaultError::RecoveryKitInvalid)
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryKitDisk {
    kind: String,
    schema_version: u32,
    dataset_id: String,
    recovery_age_identity: String,
    checksum: String,
    #[serde(default)]
    endpoint: Option<String>,
}

impl Drop for RecoveryKitDisk {
    fn drop(&mut self) {
        self.recovery_age_identity.zeroize();
    }
}

#[derive(Serialize)]
struct RecoveryKitDiskRef<'a> {
    kind: &'a str,
    schema_version: u32,
    dataset_id: &'a str,
    recovery_age_identity: &'a str,
    checksum: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<&'a str>,
}

fn checksum(dataset_id: &str, identity: &str, endpoint: Option<&str>) -> String {
    let schema = RECOVERY_KIT_SCHEMA_VERSION.to_be_bytes();
    sha256_domain_hex(
        CHECKSUM_DOMAIN,
        &[
            &schema,
            dataset_id.as_bytes(),
            identity.as_bytes(),
            endpoint.unwrap_or_default().as_bytes(),
        ],
    )
}

fn validate_dataset_id(dataset_id: &str) -> Result<(), VaultError> {
    super::crypto::validate_dataset_id(dataset_id)
}

fn validate_endpoint(endpoint: Option<&str>) -> Result<(), VaultError> {
    let Some(endpoint) = endpoint else {
        return Ok(());
    };
    if endpoint.len() > 2_048 || endpoint.chars().any(char::is_control) {
        return Err(VaultError::RecoveryKitInvalid);
    }
    let parsed = reqwest::Url::parse(endpoint).map_err(|_| VaultError::RecoveryKitInvalid)?;
    if !matches!(parsed.scheme(), "https" | "http")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(VaultError::RecoveryKitInvalid);
    }
    Ok(())
}

fn valid_checksum_encoding(checksum: &str) -> bool {
    checksum.len() == 64
        && checksum
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn recovered_state(
    mut state: crate::personal_state::PersonalStateV2,
    new_device_id: &DeviceId,
    new_device: DeviceRecord,
    previous_membership: &super::membership::VerifiedMembership,
) -> Result<crate::personal_state::PersonalStateV2, VaultError> {
    let mut observed = state.version_vector.clone();
    let mut sequence = 0_u64;
    let mut append = |operation: Operation| -> Result<(), VaultError> {
        sequence = sequence.checked_add(1).ok_or(VaultError::SequenceGap)?;
        let dot = Dot {
            device_id: new_device_id.clone(),
            sequence,
        };
        state.operations.push(OperationEnvelope {
            operation_id: format!("recovery-{}-{sequence}", new_device_id.as_str()),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: observed.clone(),
                recorded_at_unix: crate::signals::unix_now(),
            },
            origin: OperationOrigin::Imported,
            operation,
        });
        observed.observe(&dot);
        state.version_vector.observe(&dot);
        Ok(())
    };
    append(Operation::AddDevice { device: new_device })?;
    let mut active = previous_membership
        .active_devices()
        .map(|device| device.device_id.clone())
        .collect::<Vec<_>>();
    active.sort();
    for device_id in active {
        append(Operation::RevokeDevice { device_id })?;
    }
    state.projection_fingerprint = None;
    crate::personal_state::refresh_device_registry(&mut state)
        .map_err(|_| VaultError::InvalidMembership)?;
    state
        .normalize()
        .map_err(|_| VaultError::InvalidMembership)?;
    Ok(state)
}

fn validate_device_name(name: &str) -> Result<(), VaultError> {
    if name.chars().count() > 1_024 || name.chars().any(char::is_control) {
        Err(VaultError::InvalidDeviceIdentity)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_kit_round_trip_and_tamper_detection() {
        let kit = RecoveryKit::generate(
            "dataset-recovery",
            Some("https://dav.example.test/vault".to_owned()),
        )
        .unwrap();
        let bytes = kit.to_json().unwrap();
        let restored = RecoveryKit::from_json(&bytes).unwrap();
        assert_eq!(restored.dataset_id(), kit.dataset_id());
        assert_eq!(restored.recovery_recipient(), kit.recovery_recipient());
        assert_eq!(
            restored.recovery_verifying_key().unwrap(),
            kit.recovery_verifying_key().unwrap()
        );

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["dataset_id"] = serde_json::Value::String("other-dataset".to_owned());
        assert!(RecoveryKit::from_json(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn recovery_kit_rejects_credentials_in_endpoint() {
        assert!(
            RecoveryKit::generate(
                "dataset-recovery",
                Some("https://user:secret@example.test/vault".to_owned())
            )
            .is_err()
        );
    }

    #[test]
    fn debug_is_redacted() {
        let kit = RecoveryKit::generate("dataset-recovery", None).unwrap();
        let debug = format!("{kit:?}");
        assert!(!debug.contains(kit.dataset_id()));
        assert!(!debug.contains(&kit.recovery_recipient()));
        assert!(!debug.contains(kit.checksum()));
    }
}
