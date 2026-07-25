//! Owner-only OpenSubsonic profile metadata and storage paths.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::model::{AccountScopeId, BackendId};
use super::origin::{ConfiguredPrivateOrigin, custom_ca_fingerprint};

pub(crate) const PROFILE_KIND: &str = "yututui_open_subsonic_profile";
pub(crate) const PROFILE_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_PROFILE_BYTES: u64 = 256 * 1024;
const MAX_DISPLAY_NAME_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    StorageUnavailable,
    StorageBusy,
    InvalidState,
    RevisionConflict,
    PayloadTooLarge,
    SerializationFailed,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::StorageUnavailable => "music server storage is unavailable",
            Self::StorageBusy => "music server storage is busy",
            Self::InvalidState => "music server storage needs attention",
            Self::RevisionConflict => "music server settings changed; retry",
            Self::PayloadTooLarge => "music server settings are too large",
            Self::SerializationFailed => "music server settings could not be encoded",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for StoreError {}

/// Fixed paths for the OpenSubsonic owner-only store set. Deliberately not `Debug`.
pub struct OpenSubsonicPaths {
    root: PathBuf,
    profile: PathBuf,
    private_store: PathBuf,
    bridge_store: PathBuf,
    transaction_manifest: PathBuf,
    transaction_profile: PathBuf,
    transaction_private: PathBuf,
    transaction_bridge: PathBuf,
    transaction_commit: PathBuf,
    transaction_lock: PathBuf,
}

impl OpenSubsonicPaths {
    pub fn for_data_root(data_root: PathBuf) -> Self {
        let root = data_root.join("open-subsonic");
        Self {
            profile: root.join("open-subsonic-profile-v1.json"),
            private_store: root.join("open-subsonic-private-v1.json"),
            bridge_store: root.join("open-subsonic-bridge-v1.json"),
            transaction_manifest: root.join("store-set-transaction-v1.json"),
            transaction_profile: root.join("store-set-profile-v1.stage"),
            transaction_private: root.join("store-set-private-v1.stage"),
            transaction_bridge: root.join("store-set-bridge-v1.stage"),
            transaction_commit: root.join("store-set-committed-v1"),
            transaction_lock: root.join("store-set-v1.lock"),
            root,
        }
    }

    pub fn current() -> Result<Self, StoreError> {
        crate::paths::data_dir()
            .map(Self::for_data_root)
            .ok_or(StoreError::StorageUnavailable)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn profile(&self) -> &Path {
        &self.profile
    }

    pub(crate) fn private_store(&self) -> &Path {
        &self.private_store
    }

    pub(crate) fn bridge_store(&self) -> &Path {
        &self.bridge_store
    }

    pub(crate) fn transaction_manifest(&self) -> &Path {
        &self.transaction_manifest
    }

    pub(crate) fn transaction_profile(&self) -> &Path {
        &self.transaction_profile
    }

    pub(crate) fn transaction_private(&self) -> &Path {
        &self.transaction_private
    }

    pub(crate) fn transaction_bridge(&self) -> &Path {
        &self.transaction_bridge
    }

    pub(crate) fn transaction_commit(&self) -> &Path {
        &self.transaction_commit
    }

    pub(crate) fn transaction_lock(&self) -> &Path {
        &self.transaction_lock
    }
}

/// A local profile deliberately lacking `Debug` and serde implementations.
pub struct OpenSubsonicProfile {
    revision: u64,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
    display_name: String,
    origin: ConfiguredPrivateOrigin,
    custom_ca_pem: Option<Vec<u8>>,
    custom_ca_fingerprint: Option<String>,
}

impl Clone for OpenSubsonicProfile {
    fn clone(&self) -> Self {
        Self {
            revision: self.revision,
            backend_id: self.backend_id.clone(),
            account_scope_id: self.account_scope_id.clone(),
            display_name: self.display_name.clone(),
            origin: self.origin.clone(),
            custom_ca_pem: self.custom_ca_pem.clone(),
            custom_ca_fingerprint: self.custom_ca_fingerprint.clone(),
        }
    }
}

impl Drop for OpenSubsonicProfile {
    fn drop(&mut self) {
        if let Some(pem) = &mut self.custom_ca_pem {
            pem.zeroize();
        }
    }
}

impl OpenSubsonicProfile {
    pub fn new(
        display_name: &str,
        origin: ConfiguredPrivateOrigin,
        custom_ca_pem: Option<Vec<u8>>,
    ) -> Result<Self, StoreError> {
        Self::with_ids(
            0,
            BackendId::random().map_err(|_| StoreError::InvalidState)?,
            AccountScopeId::random().map_err(|_| StoreError::InvalidState)?,
            display_name,
            origin,
            custom_ca_pem,
        )
    }

    pub(crate) fn with_ids(
        revision: u64,
        backend_id: BackendId,
        account_scope_id: AccountScopeId,
        display_name: &str,
        origin: ConfiguredPrivateOrigin,
        custom_ca_pem: Option<Vec<u8>>,
    ) -> Result<Self, StoreError> {
        let display_name = sanitize_display_name(display_name)?;
        let custom_ca_fingerprint = custom_ca_pem
            .as_deref()
            .map(custom_ca_fingerprint)
            .transpose()
            .map_err(|_| StoreError::InvalidState)?;
        Ok(Self {
            revision,
            backend_id,
            account_scope_id,
            display_name,
            origin,
            custom_ca_pem,
            custom_ca_fingerprint,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn backend_id(&self) -> &BackendId {
        &self.backend_id
    }

    pub fn account_scope_id(&self) -> &AccountScopeId {
        &self.account_scope_id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn uses_lan_http(&self) -> bool {
        self.origin.uses_lan_http()
    }

    pub fn custom_ca_fingerprint(&self) -> Option<&str> {
        self.custom_ca_fingerprint.as_deref()
    }

    pub(crate) fn origin(&self) -> &ConfiguredPrivateOrigin {
        &self.origin
    }

    pub(crate) fn custom_ca_pem(&self) -> Option<&[u8]> {
        self.custom_ca_pem.as_deref()
    }

    pub(crate) fn set_revision(&mut self, revision: u64) {
        self.revision = revision;
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskProfile {
    kind: String,
    schema_version: u32,
    revision: u64,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
    display_name: String,
    origin: String,
    allow_lan_http: bool,
    custom_ca_pem: Option<String>,
    custom_ca_fingerprint: Option<String>,
}

impl Drop for DiskProfile {
    fn drop(&mut self) {
        self.origin.zeroize();
        if let Some(pem) = &mut self.custom_ca_pem {
            pem.zeroize();
        }
    }
}

pub(crate) fn encode_profile(
    profile: &OpenSubsonicProfile,
) -> Result<Zeroizing<Vec<u8>>, StoreError> {
    let disk = DiskProfile {
        kind: PROFILE_KIND.to_owned(),
        schema_version: PROFILE_SCHEMA_VERSION,
        revision: profile.revision,
        backend_id: profile.backend_id.clone(),
        account_scope_id: profile.account_scope_id.clone(),
        display_name: profile.display_name.clone(),
        origin: profile.origin.canonical().to_owned(),
        allow_lan_http: profile.origin.uses_lan_http(),
        custom_ca_pem: profile
            .custom_ca_pem
            .as_deref()
            .map(|pem| {
                std::str::from_utf8(pem)
                    .map(str::to_owned)
                    .map_err(|_| StoreError::SerializationFailed)
            })
            .transpose()?,
        custom_ca_fingerprint: profile.custom_ca_fingerprint.clone(),
    };
    let bytes =
        Zeroizing::new(serde_json::to_vec(&disk).map_err(|_| StoreError::SerializationFailed)?);
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err(StoreError::PayloadTooLarge);
    }
    Ok(bytes)
}

pub(crate) fn decode_profile(bytes: &[u8]) -> Result<OpenSubsonicProfile, StoreError> {
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err(StoreError::PayloadTooLarge);
    }
    let mut disk: DiskProfile =
        serde_json::from_slice(bytes).map_err(|_| StoreError::InvalidState)?;
    if disk.kind != PROFILE_KIND || disk.schema_version != PROFILE_SCHEMA_VERSION {
        return Err(StoreError::InvalidState);
    }
    let origin = ConfiguredPrivateOrigin::new(&disk.origin, disk.allow_lan_http)
        .map_err(|_| StoreError::InvalidState)?;
    let profile = OpenSubsonicProfile::with_ids(
        disk.revision,
        disk.backend_id.clone(),
        disk.account_scope_id.clone(),
        &disk.display_name,
        origin,
        disk.custom_ca_pem.take().map(String::into_bytes),
    )?;
    if profile.custom_ca_fingerprint != disk.custom_ca_fingerprint {
        return Err(StoreError::InvalidState);
    }
    Ok(profile)
}

fn sanitize_display_name(raw: &str) -> Result<String, StoreError> {
    let value = crate::api::sanitize_metadata_text(raw, MAX_DISPLAY_NAME_CHARS);
    if value.is_empty() {
        return Err(StoreError::InvalidState);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_round_trip_preserves_stable_identity() {
        let origin =
            ConfiguredPrivateOrigin::new("https://music.example.test/base", false).unwrap();
        let profile = OpenSubsonicProfile::new("My server", origin, None).unwrap();
        let bytes = encode_profile(&profile).unwrap();
        let decoded = decode_profile(&bytes).unwrap();
        assert_eq!(decoded.backend_id(), profile.backend_id());
        assert_eq!(decoded.account_scope_id(), profile.account_scope_id());
        assert_eq!(decoded.display_name(), "My server");
    }

    #[test]
    fn unknown_or_oversized_profiles_fail_closed() {
        assert!(decode_profile(br#"{"kind":"unknown"}"#).is_err());
        assert!(decode_profile(&vec![b'x'; MAX_PROFILE_BYTES as usize + 1]).is_err());
    }

    #[test]
    fn maximum_accepted_ca_bundle_fits_the_profile_store() {
        const MAX_CA_BYTES: usize = 192 * 1024;
        let certificate = crate::sync::webdav::tls_tests::TEST_CA_PEM;
        let mut bundle = Vec::with_capacity(MAX_CA_BYTES);
        while bundle.len().saturating_add(certificate.len()) <= MAX_CA_BYTES {
            bundle.extend_from_slice(certificate);
        }
        assert!(bundle.len() > MAX_CA_BYTES - certificate.len());

        let origin =
            ConfiguredPrivateOrigin::new("https://music.example.test/base", false).unwrap();
        let profile = OpenSubsonicProfile::new("Large CA server", origin, Some(bundle)).unwrap();
        let encoded = encode_profile(&profile).unwrap();
        assert!(encoded.len() as u64 <= MAX_PROFILE_BYTES);
        let decoded = decode_profile(&encoded).unwrap();
        assert_eq!(
            decoded.custom_ca_fingerprint(),
            profile.custom_ca_fingerprint()
        );
    }
}
