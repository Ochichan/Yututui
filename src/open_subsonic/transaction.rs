//! Crash-consistent installation of the profile, credential, and bridge store set.

use std::fs;
use std::path::Path;

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

use super::bridge_store::{
    MAX_BRIDGE_BYTES, OpenSubsonicBridgeState, decode_bridge, encode_bridge,
};
use super::private_store::{
    MAX_PRIVATE_BYTES, OpenSubsonicPrivateState, decode_private, encode_private,
};
use super::profile::{
    MAX_PROFILE_BYTES, OpenSubsonicPaths, OpenSubsonicProfile, StoreError, decode_profile,
    encode_profile,
};
use crate::util::safe_fs::{
    AdvisoryFileLock, ensure_private_dir_durable, read_owner_only_limited,
    remove_owner_only_file_durable, try_lock_private_file, validate_owner_only_file,
    write_owner_only_atomic,
};

const MANIFEST_KIND: &str = "yututui_open_subsonic_store_set_transaction";
const MANIFEST_SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
const COMMIT_MARKER: &[u8] = b"yututui-open-subsonic-store-set-v1\n";

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_COMMIT_MARKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_after_commit_marker_once_for_test() {
    FAIL_AFTER_COMMIT_MARKER.with(|armed| armed.set(true));
}

/// The three files are always loaded and installed as one identity-bound set.
pub struct OpenSubsonicStoreSet {
    pub profile: OpenSubsonicProfile,
    pub private_state: OpenSubsonicPrivateState,
    pub bridge_state: OpenSubsonicBridgeState,
}

impl OpenSubsonicStoreSet {
    pub fn new(
        profile: OpenSubsonicProfile,
        private_state: OpenSubsonicPrivateState,
        bridge_state: OpenSubsonicBridgeState,
    ) -> Result<Self, StoreError> {
        let store_set = Self {
            profile,
            private_state,
            bridge_state,
        };
        validate_identity(&store_set)?;
        Ok(store_set)
    }

    pub fn revisions(&self) -> StoreRevisions {
        StoreRevisions {
            profile: Some(self.profile.revision()),
            private_state: Some(self.private_state.revision()),
            bridge_state: Some(self.bridge_state.revision()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreRevisions {
    pub profile: Option<u64>,
    pub private_state: Option<u64>,
    pub bridge_state: Option<u64>,
}

impl StoreRevisions {
    pub const MISSING: Self = Self {
        profile: None,
        private_state: None,
        bridge_state: None,
    };
}

pub fn load_store_set(
    paths: &OpenSubsonicPaths,
) -> Result<Option<OpenSubsonicStoreSet>, StoreError> {
    if !path_exists(paths.root())? {
        return Ok(None);
    }
    ensure_private_dir_durable(paths.root()).map_err(|_| StoreError::StorageUnavailable)?;
    let _lock = acquire_lock(paths)?;
    recover_locked(paths)?;
    load_locked(paths)
}

/// Read a coherent store snapshot without creating files or rolling a transaction forward.
///
/// Status commands run with the process-wide mutation capability disabled, so they cannot use
/// [`load_store_set`]. A pending transaction is deliberately reported as invalid instead of
/// exposing a possibly mixed old/new snapshot; the primary writer will recover it on its next
/// load.
pub fn load_store_set_read_only(
    paths: &OpenSubsonicPaths,
) -> Result<Option<OpenSubsonicStoreSet>, StoreError> {
    match fs::symlink_metadata(paths.root()) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(StoreError::StorageUnavailable),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(StoreError::InvalidState);
        }
        Ok(_) => {}
    }
    if transaction_artifacts_exist(paths)? {
        return Err(StoreError::InvalidState);
    }
    let first = load_locked(paths)?;
    if transaction_artifacts_exist(paths)? {
        return Err(StoreError::InvalidState);
    }
    let second = load_locked(paths)?;
    if transaction_artifacts_exist(paths)?
        || snapshot_revisions(&first) != snapshot_revisions(&second)
    {
        return Err(StoreError::StorageBusy);
    }
    Ok(second)
}

pub fn commit_store_set(
    paths: &OpenSubsonicPaths,
    expected: StoreRevisions,
    candidate: &mut OpenSubsonicStoreSet,
) -> Result<(), StoreError> {
    ensure_private_dir_durable(paths.root()).map_err(|_| StoreError::StorageUnavailable)?;
    let _lock = acquire_lock(paths)?;
    recover_locked(paths)?;
    let current = load_locked(paths)?;
    let actual = current
        .as_ref()
        .map_or(StoreRevisions::MISSING, OpenSubsonicStoreSet::revisions);
    if actual != expected {
        return Err(StoreError::RevisionConflict);
    }
    validate_identity(candidate)?;
    candidate
        .profile
        .set_revision(next_revision(expected.profile)?);
    candidate
        .private_state
        .set_revision(next_revision(expected.private_state)?);
    candidate
        .bridge_state
        .set_revision(next_revision(expected.bridge_state)?);
    let profile = encode_profile(&candidate.profile)?;
    let private_state = encode_private(&candidate.private_state)?;
    let bridge = encode_bridge(&candidate.bridge_state)?;
    install_transaction(
        paths,
        Some(profile.as_slice()),
        Some(private_state.as_slice()),
        Some(bridge.as_slice()),
    )
}

pub fn remove_store_set(
    paths: &OpenSubsonicPaths,
    expected: StoreRevisions,
) -> Result<(), StoreError> {
    ensure_private_dir_durable(paths.root()).map_err(|_| StoreError::StorageUnavailable)?;
    let _lock = acquire_lock(paths)?;
    recover_locked(paths)?;
    let current = load_locked(paths)?;
    let actual = current
        .as_ref()
        .map_or(StoreRevisions::MISSING, OpenSubsonicStoreSet::revisions);
    if actual != expected {
        return Err(StoreError::RevisionConflict);
    }
    install_transaction(paths, None, None, None)
}

/// Remove the exact OpenSubsonic store artifacts after the user explicitly confirms removal.
///
/// This recovery path deliberately does not decode or roll forward corrupt state. It first
/// validates every existing target as a current-user-only regular file while holding the store
/// lock, then removes only the fixed artifact inventory. The commit marker goes first so a crash
/// cannot resurrect staged credentials on the next startup.
pub fn reset_store_set(paths: &OpenSubsonicPaths) -> Result<(), StoreError> {
    if !path_exists(paths.root())? {
        return Ok(());
    }
    ensure_private_dir_durable(paths.root()).map_err(|_| StoreError::StorageUnavailable)?;
    let _lock = acquire_lock(paths)?;
    let artifacts = reset_artifacts(paths);
    for path in artifacts {
        match fs::symlink_metadata(path) {
            Ok(_) => validate_owner_only_file(path).map_err(|_| StoreError::InvalidState)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(StoreError::StorageUnavailable),
        }
    }
    for path in artifacts {
        remove_owner_only_file_durable(path).map_err(|_| StoreError::StorageUnavailable)?;
    }
    Ok(())
}

pub fn recover_store_set(paths: &OpenSubsonicPaths) -> Result<(), StoreError> {
    if !path_exists(paths.root())? {
        return Ok(());
    }
    ensure_private_dir_durable(paths.root()).map_err(|_| StoreError::StorageUnavailable)?;
    let _lock = acquire_lock(paths)?;
    recover_locked(paths)
}

fn next_revision(current: Option<u64>) -> Result<u64, StoreError> {
    current
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(StoreError::RevisionConflict)
}

fn load_locked(paths: &OpenSubsonicPaths) -> Result<Option<OpenSubsonicStoreSet>, StoreError> {
    let profile = read_optional(paths.profile(), MAX_PROFILE_BYTES)?;
    let private_state = read_optional(paths.private_store(), MAX_PRIVATE_BYTES)?;
    let bridge = read_optional(paths.bridge_store(), MAX_BRIDGE_BYTES)?;
    match (profile, private_state, bridge) {
        (None, None, None) => Ok(None),
        (Some(profile), Some(private_state), Some(bridge)) => OpenSubsonicStoreSet::new(
            decode_profile(&profile)?,
            decode_private(&private_state)?,
            decode_bridge(&bridge)?,
        )
        .map(Some),
        _ => Err(StoreError::InvalidState),
    }
}

fn install_transaction(
    paths: &OpenSubsonicPaths,
    profile: Option<&[u8]>,
    private_state: Option<&[u8]>,
    bridge: Option<&[u8]>,
) -> Result<(), StoreError> {
    cleanup_precommit(paths)?;
    let entries = [
        stage_candidate(paths.transaction_profile(), profile)?,
        stage_candidate(paths.transaction_private(), private_state)?,
        stage_candidate(paths.transaction_bridge(), bridge)?,
    ];
    let manifest = TransactionManifest {
        kind: MANIFEST_KIND.to_owned(),
        schema_version: MANIFEST_SCHEMA_VERSION,
        entries,
    };
    let manifest_bytes =
        serde_json::to_vec(&manifest).map_err(|_| StoreError::SerializationFailed)?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(StoreError::PayloadTooLarge);
    }
    write_owner_only_atomic(paths.transaction_manifest(), &manifest_bytes)
        .map_err(|_| StoreError::StorageUnavailable)?;
    write_owner_only_atomic(paths.transaction_commit(), COMMIT_MARKER)
        .map_err(|_| StoreError::StorageUnavailable)?;
    #[cfg(test)]
    if FAIL_AFTER_COMMIT_MARKER.with(|armed| armed.replace(false)) {
        return Err(StoreError::StorageUnavailable);
    }
    roll_forward(paths, &manifest)
}

fn stage_candidate(path: &Path, bytes: Option<&[u8]>) -> Result<ManifestEntry, StoreError> {
    if let Some(bytes) = bytes {
        write_owner_only_atomic(path, bytes).map_err(|_| StoreError::StorageUnavailable)?;
        Ok(ManifestEntry {
            action: EntryAction::Write,
            sha256: Some(hash(bytes)),
        })
    } else {
        remove_owner_only_file_durable(path).map_err(|_| StoreError::StorageUnavailable)?;
        Ok(ManifestEntry {
            action: EntryAction::Delete,
            sha256: None,
        })
    }
}

fn recover_locked(paths: &OpenSubsonicPaths) -> Result<(), StoreError> {
    if !path_exists(paths.transaction_commit())? {
        return cleanup_precommit(paths);
    }
    let marker = Zeroizing::new(
        read_owner_only_limited(paths.transaction_commit(), COMMIT_MARKER.len() as u64)
            .map_err(|_| StoreError::InvalidState)?,
    );
    if marker.as_slice() != COMMIT_MARKER {
        return Err(StoreError::InvalidState);
    }
    let manifest_bytes = Zeroizing::new(
        read_owner_only_limited(paths.transaction_manifest(), MAX_MANIFEST_BYTES)
            .map_err(|_| StoreError::InvalidState)?,
    );
    let manifest: TransactionManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| StoreError::InvalidState)?;
    validate_manifest(&manifest)?;
    roll_forward(paths, &manifest)
}

fn roll_forward(
    paths: &OpenSubsonicPaths,
    manifest: &TransactionManifest,
) -> Result<(), StoreError> {
    let targets = [
        (
            paths.transaction_profile(),
            paths.profile(),
            MAX_PROFILE_BYTES,
        ),
        (
            paths.transaction_private(),
            paths.private_store(),
            MAX_PRIVATE_BYTES,
        ),
        (
            paths.transaction_bridge(),
            paths.bridge_store(),
            MAX_BRIDGE_BYTES,
        ),
    ];
    for (entry, (stage, target, max_bytes)) in manifest.entries.iter().zip(targets) {
        match entry.action {
            EntryAction::Write => {
                let expected = entry.sha256.as_deref().ok_or(StoreError::InvalidState)?;
                if target_has_hash(target, max_bytes, expected) {
                    continue;
                }
                let bytes = Zeroizing::new(
                    read_owner_only_limited(stage, max_bytes)
                        .map_err(|_| StoreError::InvalidState)?,
                );
                if hash(&bytes) != expected {
                    return Err(StoreError::InvalidState);
                }
                write_owner_only_atomic(target, &bytes)
                    .map_err(|_| StoreError::StorageUnavailable)?;
                if !target_has_hash(target, max_bytes, expected) {
                    return Err(StoreError::StorageUnavailable);
                }
            }
            EntryAction::Delete => {
                remove_owner_only_file_durable(target)
                    .map_err(|_| StoreError::StorageUnavailable)?;
            }
        }
    }
    remove_owner_only_file_durable(paths.transaction_commit())
        .map_err(|_| StoreError::StorageUnavailable)?;
    cleanup_precommit(paths)
}

fn cleanup_precommit(paths: &OpenSubsonicPaths) -> Result<(), StoreError> {
    for path in [
        paths.transaction_profile(),
        paths.transaction_private(),
        paths.transaction_bridge(),
        paths.transaction_manifest(),
    ] {
        remove_owner_only_file_durable(path).map_err(|_| StoreError::StorageUnavailable)?;
    }
    Ok(())
}

fn read_optional(path: &Path, max_bytes: u64) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_owner_only_limited(path, max_bytes)
            .map(Zeroizing::new)
            .map(Some)
            .map_err(|_| StoreError::InvalidState),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(StoreError::StorageUnavailable),
    }
}

fn path_exists(path: &Path) -> Result<bool, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(StoreError::StorageUnavailable),
    }
}

fn transaction_artifacts_exist(paths: &OpenSubsonicPaths) -> Result<bool, StoreError> {
    for path in [
        paths.transaction_profile(),
        paths.transaction_private(),
        paths.transaction_bridge(),
        paths.transaction_manifest(),
        paths.transaction_commit(),
    ] {
        if path_exists(path)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn reset_artifacts(paths: &OpenSubsonicPaths) -> [&Path; 8] {
    [
        paths.transaction_commit(),
        paths.transaction_profile(),
        paths.transaction_private(),
        paths.transaction_bridge(),
        paths.transaction_manifest(),
        paths.profile(),
        paths.private_store(),
        paths.bridge_store(),
    ]
}

fn snapshot_revisions(store_set: &Option<OpenSubsonicStoreSet>) -> StoreRevisions {
    store_set
        .as_ref()
        .map_or(StoreRevisions::MISSING, OpenSubsonicStoreSet::revisions)
}

fn target_has_hash(path: &Path, max_bytes: u64, expected: &str) -> bool {
    read_owner_only_limited(path, max_bytes).is_ok_and(|bytes| {
        let bytes = Zeroizing::new(bytes);
        hash(&bytes) == expected
    })
}

fn hash(bytes: &[u8]) -> String {
    HEXLOWER.encode(&Sha256::digest(bytes))
}

fn acquire_lock(paths: &OpenSubsonicPaths) -> Result<AdvisoryFileLock, StoreError> {
    match try_lock_private_file(paths.transaction_lock()) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(StoreError::StorageBusy),
        Err(_) => Err(StoreError::StorageUnavailable),
    }
}

fn validate_identity(store_set: &OpenSubsonicStoreSet) -> Result<(), StoreError> {
    let backend = store_set.profile.backend_id();
    let account = store_set.profile.account_scope_id();
    if store_set.private_state.backend_id() != backend
        || store_set.private_state.account_scope_id() != account
        || store_set.bridge_state.backend_id() != backend
        || store_set.bridge_state.account_scope_id() != account
        || store_set.private_state.revision() != store_set.profile.revision()
        || store_set.bridge_state.revision() != store_set.profile.revision()
    {
        return Err(StoreError::InvalidState);
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionManifest {
    kind: String,
    schema_version: u32,
    entries: [ManifestEntry; 3],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntry {
    action: EntryAction,
    sha256: Option<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryAction {
    Write,
    Delete,
}

fn validate_manifest(manifest: &TransactionManifest) -> Result<(), StoreError> {
    if manifest.kind != MANIFEST_KIND || manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(StoreError::InvalidState);
    }
    for entry in &manifest.entries {
        match (&entry.action, &entry.sha256) {
            (EntryAction::Write, Some(hash))
                if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) => {}
            (EntryAction::Delete, None) => {}
            _ => return Err(StoreError::InvalidState),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use age::secrecy::SecretString;

    use super::*;
    use crate::open_subsonic::{
        AccountScopeId, ConfiguredPrivateOrigin, OpenSubsonicProfile, ServerCredential,
    };

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: std::path::PathBuf,
        paths: OpenSubsonicPaths,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture() -> Fixture {
        let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "yututui-open-subsonic-store-set-{}-{id}",
            std::process::id()
        ));
        crate::util::safe_fs::ensure_private_dir(&root).unwrap();
        let paths = OpenSubsonicPaths::for_data_root(root.clone());
        Fixture { root, paths }
    }

    fn candidate() -> OpenSubsonicStoreSet {
        let profile = OpenSubsonicProfile::new(
            "Server",
            ConfiguredPrivateOrigin::new("https://music.example.test/", false).unwrap(),
            None,
        )
        .unwrap();
        let private_state = OpenSubsonicPrivateState::new(
            profile.backend_id().clone(),
            profile.account_scope_id().clone(),
            ServerCredential::api_key(SecretString::from("secret".to_owned())).unwrap(),
        );
        let bridge_state = OpenSubsonicBridgeState::new(
            profile.backend_id().clone(),
            profile.account_scope_id().clone(),
        );
        OpenSubsonicStoreSet::new(profile, private_state, bridge_state).unwrap()
    }

    #[test]
    fn commits_loads_and_removes_one_identity_bound_set() {
        let fixture = fixture();
        let mut candidate = candidate();
        commit_store_set(&fixture.paths, StoreRevisions::MISSING, &mut candidate).unwrap();
        let loaded = load_store_set(&fixture.paths).unwrap().unwrap();
        assert_eq!(loaded.revisions().profile, Some(1));
        assert_eq!(loaded.profile.backend_id(), candidate.profile.backend_id());
        remove_store_set(&fixture.paths, loaded.revisions()).unwrap();
        assert!(load_store_set(&fixture.paths).unwrap().is_none());
    }

    #[test]
    fn read_only_load_never_creates_storage_or_requires_mutation_capability() {
        struct MutationRevokeGuard;
        impl Drop for MutationRevokeGuard {
            fn drop(&mut self) {
                crate::util::safe_fs::clear_process_mutation_revoke_for_test();
            }
        }

        let empty = fixture();
        assert!(load_store_set(&empty.paths).unwrap().is_none());
        assert!(!empty.paths.root().exists());
        assert!(load_store_set_read_only(&empty.paths).unwrap().is_none());
        assert!(!empty.paths.root().exists());

        let fixture = fixture();
        let mut candidate = candidate();
        commit_store_set(&fixture.paths, StoreRevisions::MISSING, &mut candidate).unwrap();
        let _guard = MutationRevokeGuard;
        crate::util::safe_fs::revoke_process_mutations(std::sync::Arc::from("test"));
        let loaded = load_store_set_read_only(&fixture.paths).unwrap().unwrap();
        assert_eq!(loaded.revisions().profile, Some(1));
    }

    #[test]
    fn read_only_load_never_publishes_a_pending_commit() {
        let fixture = fixture();
        let mut candidate = candidate();
        candidate.profile.set_revision(1);
        candidate.private_state.set_revision(1);
        candidate.bridge_state.set_revision(1);
        let entries = [
            stage_candidate(
                fixture.paths.transaction_profile(),
                Some(&encode_profile(&candidate.profile).unwrap()),
            )
            .unwrap(),
            stage_candidate(
                fixture.paths.transaction_private(),
                Some(&encode_private(&candidate.private_state).unwrap()),
            )
            .unwrap(),
            stage_candidate(
                fixture.paths.transaction_bridge(),
                Some(&encode_bridge(&candidate.bridge_state).unwrap()),
            )
            .unwrap(),
        ];
        let manifest = TransactionManifest {
            kind: MANIFEST_KIND.to_owned(),
            schema_version: MANIFEST_SCHEMA_VERSION,
            entries,
        };
        write_owner_only_atomic(
            fixture.paths.transaction_manifest(),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        write_owner_only_atomic(fixture.paths.transaction_commit(), COMMIT_MARKER).unwrap();

        assert_eq!(
            load_store_set_read_only(&fixture.paths)
                .err()
                .expect("pending commit must fail closed"),
            StoreError::InvalidState
        );
        assert!(fixture.paths.transaction_commit().exists());
        assert!(!fixture.paths.profile().exists());
    }

    #[test]
    fn stale_revision_cannot_replace_a_store_set() {
        let fixture = fixture();
        let mut first = candidate();
        commit_store_set(&fixture.paths, StoreRevisions::MISSING, &mut first).unwrap();
        let mut second = candidate();
        assert_eq!(
            commit_store_set(&fixture.paths, StoreRevisions::MISSING, &mut second),
            Err(StoreError::RevisionConflict)
        );
    }

    #[test]
    fn committed_transaction_is_rolled_forward_on_next_load() {
        let fixture = fixture();
        let mut candidate = candidate();
        candidate.profile.set_revision(1);
        candidate.private_state.set_revision(1);
        candidate.bridge_state.set_revision(1);
        let profile = encode_profile(&candidate.profile).unwrap();
        let private_state = encode_private(&candidate.private_state).unwrap();
        let bridge = encode_bridge(&candidate.bridge_state).unwrap();
        let entries = [
            stage_candidate(fixture.paths.transaction_profile(), Some(&profile)).unwrap(),
            stage_candidate(fixture.paths.transaction_private(), Some(&private_state)).unwrap(),
            stage_candidate(fixture.paths.transaction_bridge(), Some(&bridge)).unwrap(),
        ];
        let manifest = TransactionManifest {
            kind: MANIFEST_KIND.to_owned(),
            schema_version: MANIFEST_SCHEMA_VERSION,
            entries,
        };
        write_owner_only_atomic(
            fixture.paths.transaction_manifest(),
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        write_owner_only_atomic(fixture.paths.transaction_commit(), COMMIT_MARKER).unwrap();

        assert!(load_store_set(&fixture.paths).unwrap().is_some());
        assert!(!fixture.paths.transaction_commit().exists());
    }

    #[test]
    fn uncommitted_staging_is_discarded_without_publishing() {
        let fixture = fixture();
        let candidate = candidate();
        let profile = encode_profile(&candidate.profile).unwrap();
        stage_candidate(fixture.paths.transaction_profile(), Some(&profile)).unwrap();
        write_owner_only_atomic(
            fixture.paths.transaction_manifest(),
            br#"{"kind":"incomplete"}"#,
        )
        .unwrap();

        assert!(load_store_set(&fixture.paths).unwrap().is_none());
        assert!(!fixture.paths.transaction_profile().exists());
        assert!(!fixture.paths.transaction_manifest().exists());
    }

    #[test]
    fn mismatched_account_binding_is_rejected_before_staging() {
        let fixture = fixture();
        let profile = OpenSubsonicProfile::new(
            "Server",
            ConfiguredPrivateOrigin::new("https://music.example.test/", false).unwrap(),
            None,
        )
        .unwrap();
        let private_state = OpenSubsonicPrivateState::new(
            profile.backend_id().clone(),
            AccountScopeId::new("different-account").unwrap(),
            ServerCredential::api_key(SecretString::from("secret".to_owned())).unwrap(),
        );
        let bridge_state = OpenSubsonicBridgeState::new(
            profile.backend_id().clone(),
            profile.account_scope_id().clone(),
        );
        assert!(OpenSubsonicStoreSet::new(profile, private_state, bridge_state).is_err());
        assert!(load_store_set(&fixture.paths).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn installed_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = fixture();
        let mut candidate = candidate();
        commit_store_set(&fixture.paths, StoreRevisions::MISSING, &mut candidate).unwrap();
        for path in [
            fixture.paths.profile(),
            fixture.paths.private_store(),
            fixture.paths.bridge_store(),
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
