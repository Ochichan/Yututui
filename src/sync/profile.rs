//! Authenticated, device-local WebDAV profile storage.
//!
//! The endpoint is intentionally absent from portable state, recovery-independent audit output,
//! and user-facing errors. Credentials remain in [`super::PrivateStore`]; this file only binds the
//! canonical endpoint to the dataset and device signing identity so a local file replacement
//! cannot silently redirect those credentials.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::personal_state::DeviceRecord;

use super::crypto::{DeviceSecretMaterial, sign_serializable, verify_serializable};
use super::error::VaultError;

const PROFILE_KIND: &str = "yututui_webdav_profile";
const PROFILE_SCHEMA_VERSION: u32 = 1;
const PROFILE_SIGNATURE_DOMAIN: &[u8] = b"yututui-webdav-profile-signature-v1";
const MAX_PROFILE_BYTES: u64 = 256 * 1024;
const MAX_ENDPOINT_BYTES: usize = 4096;
pub const MAX_CUSTOM_CA_PEM_BYTES: usize = 192 * 1024;

/// Device-local paths used by manual sync.
///
/// This type deliberately has no `Debug` implementation: filesystem paths are excluded from
/// retained sync outcomes and audit messages.
pub struct SyncPaths {
    root: PathBuf,
    profile: PathBuf,
    private_store: PathBuf,
    health: PathBuf,
    audit: PathBuf,
    anchor_transition_manifest: PathBuf,
    anchor_transition_ledger: PathBuf,
    anchor_transition_private: PathBuf,
    anchor_transition_commit: PathBuf,
    anchor_transition_lock: PathBuf,
    pending_join_state: PathBuf,
    pending_join_request: PathBuf,
    pending_join_checkpoint: PathBuf,
    pairing_host_state: PathBuf,
    pairing_host_locator: PathBuf,
    pairing_host_request: PathBuf,
    pairing_host_checkpoint: PathBuf,
    pairing_host_approval: PathBuf,
}

impl SyncPaths {
    pub fn for_data_root(data_root: PathBuf) -> Self {
        let root = data_root.join("sync");
        Self {
            profile: root.join("webdav-profile-v1.json"),
            private_store: root.join("vault-private-v1.json"),
            health: root.join("health-v1.json"),
            audit: root.join("audit-v1.json"),
            anchor_transition_manifest: root.join("anchor-transition-manifest-v1.json"),
            anchor_transition_ledger: root.join("anchor-transition-ledger-v1.json"),
            anchor_transition_private: root.join("anchor-transition-private-v1.json"),
            anchor_transition_commit: root.join("anchor-transition-committed-v1"),
            anchor_transition_lock: root.join("anchor-transition-v1.lock"),
            pending_join_state: root.join("pending-join-state-v1.json"),
            pending_join_request: root.join("pending-join-request-v1.age"),
            pending_join_checkpoint: root.join("pending-join-checkpoint-v1.age"),
            pairing_host_state: root.join("pairing-host-state-v1.json"),
            pairing_host_locator: root.join("pairing-host-locator-v1.age"),
            pairing_host_request: root.join("pairing-host-request-v1.age"),
            pairing_host_checkpoint: root.join("pairing-host-checkpoint-v1.age"),
            pairing_host_approval: root.join("pairing-host-approval-v1.age"),
            root,
        }
    }

    pub fn current() -> Result<Self, VaultError> {
        crate::paths::data_dir()
            .map(Self::for_data_root)
            .ok_or(VaultError::StorageFailed)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn profile(&self) -> &Path {
        &self.profile
    }

    pub fn private_store(&self) -> &Path {
        &self.private_store
    }

    pub fn health(&self) -> &Path {
        &self.health
    }

    pub fn audit(&self) -> &Path {
        &self.audit
    }

    pub(crate) fn anchor_transition_manifest(&self) -> &Path {
        &self.anchor_transition_manifest
    }

    pub(crate) fn anchor_transition_ledger(&self) -> &Path {
        &self.anchor_transition_ledger
    }

    pub(crate) fn anchor_transition_private(&self) -> &Path {
        &self.anchor_transition_private
    }

    pub(crate) fn anchor_transition_commit(&self) -> &Path {
        &self.anchor_transition_commit
    }

    pub(crate) fn anchor_transition_lock(&self) -> &Path {
        &self.anchor_transition_lock
    }

    pub(crate) fn pending_join_request(&self) -> &Path {
        &self.pending_join_request
    }

    pub(crate) fn pending_join_state(&self) -> &Path {
        &self.pending_join_state
    }

    pub(crate) fn pending_join_checkpoint(&self) -> &Path {
        &self.pending_join_checkpoint
    }

    pub(crate) fn pairing_host_state(&self) -> &Path {
        &self.pairing_host_state
    }

    pub(crate) fn pairing_host_locator(&self) -> &Path {
        &self.pairing_host_locator
    }

    pub(crate) fn pairing_host_request(&self) -> &Path {
        &self.pairing_host_request
    }

    pub(crate) fn pairing_host_checkpoint(&self) -> &Path {
        &self.pairing_host_checkpoint
    }

    pub(crate) fn pairing_host_approval(&self) -> &Path {
        &self.pairing_host_approval
    }
}

/// A canonical endpoint authenticated by this device.
///
/// This type deliberately has no `Debug` or serde implementation so callers cannot accidentally
/// retain the endpoint in status snapshots or logs.
pub struct WebDavProfile {
    revision: u64,
    dataset_id: String,
    device_id: String,
    endpoint: String,
    custom_ca_pem: Option<String>,
}

impl WebDavProfile {
    pub fn new(
        dataset_id: impl Into<String>,
        device: &DeviceSecretMaterial,
        endpoint: &str,
    ) -> Result<Self, VaultError> {
        Self::with_custom_ca(dataset_id, device, endpoint, None)
    }

    pub fn with_custom_ca(
        dataset_id: impl Into<String>,
        device: &DeviceSecretMaterial,
        endpoint: &str,
        custom_ca_pem: Option<&[u8]>,
    ) -> Result<Self, VaultError> {
        let dataset_id = dataset_id.into();
        super::crypto::validate_dataset_id(&dataset_id)?;
        let endpoint = canonical_endpoint(endpoint)?;
        let custom_ca_pem = validated_custom_ca(custom_ca_pem)?;
        Ok(Self {
            revision: 0,
            dataset_id,
            device_id: device.device_id().to_owned(),
            endpoint,
            custom_ca_pem,
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn dataset_id(&self) -> &str {
        &self.dataset_id
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Only transport construction should consume this value. Do not include it in errors.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// A device-local trust anchor for transport construction. It is never exported or logged.
    pub fn custom_ca_pem(&self) -> Option<&[u8]> {
        self.custom_ca_pem.as_deref().map(str::as_bytes)
    }
}

impl Drop for WebDavProfile {
    fn drop(&mut self) {
        self.endpoint.zeroize();
        if let Some(custom_ca_pem) = &mut self.custom_ca_pem {
            custom_ca_pem.zeroize();
        }
    }
}

/// Atomic owner of one authenticated WebDAV profile.
pub struct WebDavProfileStore {
    path: PathBuf,
}

impl WebDavProfileStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, VaultError> {
        let path = path.into();
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(VaultError::InvalidPrivateStore);
        }
        Ok(Self { path })
    }

    pub fn create(
        &self,
        profile: &mut WebDavProfile,
        device: &DeviceSecretMaterial,
    ) -> Result<(), VaultError> {
        if profile.revision != 0 {
            return Err(VaultError::RevisionConflict);
        }
        match fs::symlink_metadata(&self.path) {
            Ok(_) => return Err(VaultError::RevisionConflict),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(VaultError::StorageFailed),
        }
        profile.revision = 1;
        let bytes = match encoded_profile(profile, device) {
            Ok(bytes) => bytes,
            Err(error) => {
                profile.revision = 0;
                return Err(error);
            }
        };
        if crate::util::safe_fs::write_owner_only_atomic(&self.path, &bytes).is_err() {
            let visible =
                crate::util::safe_fs::read_owner_only_limited(&self.path, MAX_PROFILE_BYTES)
                    .is_ok_and(|actual| actual == bytes);
            if !visible {
                profile.revision = 0;
                return Err(VaultError::StorageFailed);
            }
        }
        Ok(())
    }

    pub fn load(&self, device: &DeviceSecretMaterial) -> Result<WebDavProfile, VaultError> {
        self.load_with_identity(
            device.device_id(),
            &device.public_identity().ed25519_verifying_key,
        )
    }

    /// Verify an orphaned pending profile through its code-bound pairing request identity.
    pub(crate) fn load_for_pairing_record(
        &self,
        record: &DeviceRecord,
    ) -> Result<WebDavProfile, VaultError> {
        let identity = record
            .public_identity
            .as_ref()
            .ok_or(VaultError::InvalidPrivateStore)?;
        self.load_with_identity(record.device_id.as_str(), &identity.ed25519_verifying_key)
    }

    fn load_with_identity(
        &self,
        device_id: &str,
        verifying_key: &str,
    ) -> Result<WebDavProfile, VaultError> {
        let bytes = crate::util::safe_fs::read_owner_only_limited(&self.path, MAX_PROFILE_BYTES)
            .map_err(|_| VaultError::InvalidPrivateStore)?;
        let mut disk: DiskProfile =
            serde_json::from_slice(&bytes).map_err(|_| VaultError::InvalidPrivateStore)?;
        let result = disk.to_profile(device_id, verifying_key);
        disk.endpoint.zeroize();
        result
    }

    pub fn remove(&self) -> Result<(), VaultError> {
        crate::util::safe_fs::remove_owner_only_file_durable(&self.path)
            .map_err(|_| VaultError::StorageFailed)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskProfile {
    kind: String,
    schema_version: u32,
    revision: u64,
    dataset_id: String,
    device_id: String,
    endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    custom_ca_pem: Option<String>,
    signature: String,
}

#[derive(Serialize)]
struct ProfileBinding<'a> {
    kind: &'a str,
    schema_version: u32,
    revision: u64,
    dataset_id: &'a str,
    device_id: &'a str,
    endpoint: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom_ca_pem: Option<&'a str>,
}

impl DiskProfile {
    fn binding(&self) -> ProfileBinding<'_> {
        ProfileBinding {
            kind: &self.kind,
            schema_version: self.schema_version,
            revision: self.revision,
            dataset_id: &self.dataset_id,
            device_id: &self.device_id,
            endpoint: &self.endpoint,
            custom_ca_pem: self.custom_ca_pem.as_deref(),
        }
    }

    fn to_profile(
        &self,
        device_id: &str,
        verifying_key: &str,
    ) -> Result<WebDavProfile, VaultError> {
        if self.kind != PROFILE_KIND
            || self.schema_version != PROFILE_SCHEMA_VERSION
            || self.revision == 0
            || self.device_id != device_id
        {
            return Err(VaultError::InvalidPrivateStore);
        }
        super::crypto::validate_dataset_id(&self.dataset_id)?;
        let canonical = canonical_endpoint(&self.endpoint)?;
        if canonical != self.endpoint {
            return Err(VaultError::InvalidPrivateStore);
        }
        let custom_ca_pem = validated_custom_ca(self.custom_ca_pem.as_deref().map(str::as_bytes))?;
        if custom_ca_pem != self.custom_ca_pem {
            return Err(VaultError::InvalidPrivateStore);
        }
        verify_serializable(
            PROFILE_SIGNATURE_DOMAIN,
            verifying_key,
            &self.binding(),
            &self.signature,
        )
        .map_err(|_| VaultError::InvalidPrivateStore)?;
        Ok(WebDavProfile {
            revision: self.revision,
            dataset_id: self.dataset_id.clone(),
            device_id: self.device_id.clone(),
            endpoint: canonical,
            custom_ca_pem,
        })
    }
}

impl Drop for DiskProfile {
    fn drop(&mut self) {
        self.endpoint.zeroize();
        if let Some(custom_ca_pem) = &mut self.custom_ca_pem {
            custom_ca_pem.zeroize();
        }
    }
}

fn encoded_profile(
    profile: &WebDavProfile,
    device: &DeviceSecretMaterial,
) -> Result<Vec<u8>, VaultError> {
    if profile.device_id != device.device_id() || profile.revision == 0 {
        return Err(VaultError::InvalidPrivateStore);
    }
    let binding = ProfileBinding {
        kind: PROFILE_KIND,
        schema_version: PROFILE_SCHEMA_VERSION,
        revision: profile.revision,
        dataset_id: &profile.dataset_id,
        device_id: &profile.device_id,
        endpoint: &profile.endpoint,
        custom_ca_pem: profile.custom_ca_pem.as_deref(),
    };
    let signature = sign_serializable(PROFILE_SIGNATURE_DOMAIN, device.signing_key(), &binding)?;
    let disk = DiskProfile {
        kind: PROFILE_KIND.to_owned(),
        schema_version: PROFILE_SCHEMA_VERSION,
        revision: profile.revision,
        dataset_id: profile.dataset_id.clone(),
        device_id: profile.device_id.clone(),
        endpoint: profile.endpoint.clone(),
        custom_ca_pem: profile.custom_ca_pem.clone(),
        signature,
    };
    let bytes = serde_json::to_vec(&disk).map_err(|_| VaultError::SerializationFailed)?;
    if bytes.len() as u64 > MAX_PROFILE_BYTES {
        return Err(VaultError::PayloadTooLarge);
    }
    Ok(bytes)
}

fn validated_custom_ca(custom_ca_pem: Option<&[u8]>) -> Result<Option<String>, VaultError> {
    let Some(custom_ca_pem) = custom_ca_pem else {
        return Ok(None);
    };
    if custom_ca_pem.is_empty() || custom_ca_pem.len() > MAX_CUSTOM_CA_PEM_BYTES {
        return Err(VaultError::InvalidPrivateStore);
    }
    let pem = std::str::from_utf8(custom_ca_pem).map_err(|_| VaultError::InvalidPrivateStore)?;
    let certificates = reqwest::Certificate::from_pem_bundle(custom_ca_pem)
        .map_err(|_| VaultError::InvalidPrivateStore)?;
    if certificates.is_empty() {
        return Err(VaultError::InvalidPrivateStore);
    }
    Ok(Some(pem.to_owned()))
}

fn canonical_endpoint(raw: &str) -> Result<String, VaultError> {
    if raw.is_empty()
        || raw.len() > MAX_ENDPOINT_BYTES
        || raw.chars().any(endpoint_unsafe_character)
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    let mut parsed = reqwest::Url::parse(raw).map_err(|_| VaultError::InvalidPrivateStore)?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(VaultError::InvalidPrivateStore);
    }
    match parsed.scheme() {
        "https" => {}
        "http" if is_loopback_host(&parsed) => {}
        _ => return Err(VaultError::InvalidPrivateStore),
    }
    if parsed.host().is_none() {
        return Err(VaultError::InvalidPrivateStore);
    }
    if !parsed.path().ends_with('/') {
        let path = format!("{}/", parsed.path());
        parsed.set_path(&path);
    }
    Ok(parsed.to_string())
}

fn is_loopback_host(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn endpoint_unsafe_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{200b}'
                | '\u{200c}'
                | '\u{200d}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let suffix = super::super::crypto::random_id_hex::<8>().unwrap();
        let root = std::env::temp_dir().join(format!(
            "yututui-sync-profile-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn profile_round_trips_with_owner_only_storage_and_canonical_endpoint() {
        let root = temp_dir("round-trip");
        let store = WebDavProfileStore::new(root.join("profile.json")).unwrap();
        let device = DeviceSecretMaterial::generate_for("device-a").unwrap();
        let mut profile =
            WebDavProfile::new("dataset-profile", &device, "https://dav.example.test/root")
                .unwrap();

        store.create(&mut profile, &device).unwrap();
        let loaded = store.load(&device).unwrap();

        assert_eq!(loaded.revision(), 1);
        assert_eq!(loaded.dataset_id(), "dataset-profile");
        assert_eq!(loaded.device_id(), "device-a");
        assert_eq!(loaded.endpoint(), "https://dav.example.test/root/");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(root.join("profile.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tampered_endpoint_or_wrong_device_fails_closed() {
        let root = temp_dir("tamper");
        let path = root.join("profile.json");
        let store = WebDavProfileStore::new(path.clone()).unwrap();
        let device = DeviceSecretMaterial::generate_for("device-a").unwrap();
        let other = DeviceSecretMaterial::generate_for("device-b").unwrap();
        let mut profile =
            WebDavProfile::new("dataset-profile", &device, "https://dav.example.test/").unwrap();
        store.create(&mut profile, &device).unwrap();

        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["endpoint"] = serde_json::Value::String("https://evil.example/".to_owned());
        crate::util::safe_fs::write_owner_only_atomic(&path, &serde_json::to_vec(&value).unwrap())
            .unwrap();

        assert!(matches!(
            store.load(&device),
            Err(VaultError::InvalidPrivateStore)
        ));
        assert!(matches!(
            store.load(&other),
            Err(VaultError::InvalidPrivateStore)
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn endpoint_policy_rejects_credentials_fragments_and_non_loopback_http() {
        let device = DeviceSecretMaterial::generate_for("device-a").unwrap();
        for endpoint in [
            "http://dav.example.test/",
            "https://user:secret@dav.example.test/",
            "https://dav.example.test/?token=secret",
            "https://dav.example.test/#fragment",
            "ftp://dav.example.test/",
        ] {
            assert!(
                WebDavProfile::new("dataset-profile", &device, endpoint).is_err(),
                "{endpoint}"
            );
        }
        assert!(
            WebDavProfile::new("dataset-profile", &device, "http://127.0.0.1:8080/dav").is_ok()
        );
        assert!(WebDavProfile::new("dataset-profile", &device, "http://[::1]:8080/dav").is_ok());
        assert!(
            WebDavProfile::new("dataset-profile", &device, "http://localhost:8080/dav").is_err()
        );
    }

    #[test]
    fn custom_ca_is_validated_signed_and_round_trips_in_private_profile() {
        let root = temp_dir("custom-ca");
        let store = WebDavProfileStore::new(root.join("profile.json")).unwrap();
        let device = DeviceSecretMaterial::generate_for("device-a").unwrap();
        let mut profile = WebDavProfile::with_custom_ca(
            "dataset-profile",
            &device,
            "https://dav.example.test/",
            Some(super::super::webdav::tls_tests::TEST_CA_PEM),
        )
        .unwrap();

        store.create(&mut profile, &device).unwrap();
        let loaded = store.load(&device).unwrap();

        assert_eq!(
            loaded.custom_ca_pem(),
            Some(super::super::webdav::tls_tests::TEST_CA_PEM)
        );
        assert!(
            WebDavProfile::with_custom_ca(
                "dataset-profile",
                &device,
                "https://dav.example.test/",
                Some(b"not a certificate"),
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }
}
