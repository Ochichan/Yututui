//! Crash-safe commit boundary between personal-state projections and the private rollback anchor.
//!
//! The personal-state coordinator remains the sole installer of the ledger and its four runtime
//! projections. This journal adds one outer decision marker and an owner-only staged next
//! private-store revision. Once the marker is durable, recovery always rolls both stores forward;
//! without it, staged bytes are discarded.

use std::io;

use serde::{Deserialize, Serialize};

use crate::personal_state::{
    PersonalStateCommit, PersonalStatePaths, PersonalStateV2, load_ledger,
    recover_pending_transactions,
};
use crate::util::safe_fs::{
    AdvisoryFileLock, ensure_private_dir_durable, read_owner_only_limited,
    remove_owner_only_file_durable, try_lock_private_file, write_owner_only_atomic,
};

use super::super::crypto::{
    sha256_domain_hex, sign_serializable, validate_dataset_id, verify_serializable,
};
use super::super::{EnrollmentState, PrivateStore, PrivateStoreSnapshot, SyncPaths};
use super::SyncServiceError;

const TRANSITION_KIND: &str = "yututui_sync_anchor_transition";
const TRANSITION_SCHEMA_VERSION: u32 = 1;
const TRANSITION_SIGNATURE_DOMAIN: &[u8] = b"yututui-sync-anchor-transition-signature-v1";
const LEDGER_PAYLOAD_HASH_DOMAIN: &[u8] = b"yututui-sync-anchor-ledger-payload-v1";
const ACTIVATION_HASH_DOMAIN: &[u8] = b"yututui-sync-anchor-activation-v1";
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const COMMIT_MARKER_BYTES: &[u8] = b"committed\n";

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum AnchorActivationKind {
    ManualSync,
    Setup,
    PairJoin,
}

impl AnchorActivationKind {
    fn label(self) -> &'static [u8] {
        match self {
            Self::ManualSync => b"manual_sync",
            Self::Setup => b"setup",
            Self::PairJoin => b"pair_join",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionManifest {
    binding: TransitionBinding,
    signature: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionBinding {
    kind: String,
    schema_version: u32,
    dataset_id: String,
    device_id: String,
    activation_kind: AnchorActivationKind,
    expected_personal_revision: u64,
    expected_personal_hash: String,
    candidate_personal_revision: u64,
    candidate_personal_hash: String,
    playlist_revision: u64,
    expected_private_revision: u64,
    target_private_revision: u64,
    target_private_hash: String,
    checkpoint_sequence: u64,
    checkpoint_hash: String,
    activation_hash: String,
}

struct TransitionStore<'a> {
    paths: &'a SyncPaths,
}

impl<'a> TransitionStore<'a> {
    fn new(paths: &'a SyncPaths) -> Self {
        Self { paths }
    }

    fn acquire_lock(&self) -> Result<AdvisoryFileLock, SyncServiceError> {
        ensure_private_dir_durable(self.paths.root()).map_err(|_| SyncServiceError::Storage)?;
        match try_lock_private_file(self.paths.anchor_transition_lock()) {
            Ok(Some(lock)) => Ok(lock),
            Ok(None) | Err(_) => Err(SyncServiceError::Storage),
        }
    }

    fn has_artifacts(&self) -> Result<bool, SyncServiceError> {
        for path in [
            self.paths.anchor_transition_manifest(),
            self.paths.anchor_transition_ledger(),
            self.paths.anchor_transition_private(),
            self.paths.anchor_transition_commit(),
        ] {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    return Ok(true);
                }
                Ok(_) => return Err(SyncServiceError::Storage),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err(SyncServiceError::Storage),
            }
        }
        Ok(false)
    }

    fn marker_exists(&self) -> Result<bool, SyncServiceError> {
        match read_owner_only_limited(self.paths.anchor_transition_commit(), 32) {
            Ok(bytes) if bytes == COMMIT_MARKER_BYTES => Ok(true),
            Ok(_) => Err(SyncServiceError::Storage),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(SyncServiceError::Storage),
        }
    }

    fn discard_uncommitted_locked(&self) -> Result<(), SyncServiceError> {
        for path in [
            self.paths.anchor_transition_manifest(),
            self.paths.anchor_transition_ledger(),
            self.paths.anchor_transition_private(),
        ] {
            remove_owner_only_file_durable(path).map_err(|_| SyncServiceError::Storage)?;
        }
        Ok(())
    }

    fn complete_locked(&self) -> Result<(), SyncServiceError> {
        // Removing the decision marker first means an interrupted cleanup is observed only as
        // harmless orphaned staging on the next recovery pass.
        remove_owner_only_file_durable(self.paths.anchor_transition_commit())
            .map_err(|_| SyncServiceError::Storage)?;
        self.discard_uncommitted_locked()
    }

    fn load_manifest_locked(&self) -> Result<TransitionManifest, SyncServiceError> {
        let bytes =
            read_owner_only_limited(self.paths.anchor_transition_manifest(), MAX_MANIFEST_BYTES)
                .map_err(|_| SyncServiceError::Storage)?;
        serde_json::from_slice(&bytes).map_err(|_| SyncServiceError::InvalidRemoteData)
    }

    fn recover_locked(
        &self,
        personal_paths: &PersonalStatePaths,
        mut checkpoint: impl FnMut(AnchorCommitPoint) -> io::Result<()>,
    ) -> Result<Option<PersonalStateV2>, SyncServiceError> {
        recover_pending_transactions(personal_paths)?;
        if !self.marker_exists()? {
            self.discard_uncommitted_locked()?;
            return Ok(None);
        }

        let manifest = self.load_manifest_locked()?;
        let binding = &manifest.binding;
        validate_binding(binding)?;
        let private_store = PrivateStore::new(self.paths.private_store())?;
        let private = private_store.load()?;
        if private.dataset_id() != binding.dataset_id
            || private.device_id() != binding.device_id
            || !matches!(
                private.revision(),
                revision if revision == binding.expected_private_revision
                    || revision == binding.target_private_revision
            )
        {
            return Err(SyncServiceError::LocalStateChanged);
        }
        verify_serializable(
            TRANSITION_SIGNATURE_DOMAIN,
            &private.device().public_identity().ed25519_verifying_key,
            binding,
            &manifest.signature,
        )
        .map_err(|_| SyncServiceError::InvalidRemoteData)?;

        let candidate_bytes = read_owner_only_limited(
            self.paths.anchor_transition_ledger(),
            crate::data_export::EXPORT_MAX_BYTES,
        )
        .map_err(|_| SyncServiceError::Storage)?;
        if ledger_payload_hash(&candidate_bytes) != binding.candidate_personal_hash {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        let mut candidate: PersonalStateV2 = serde_json::from_slice(&candidate_bytes)
            .map_err(|_| SyncServiceError::InvalidRemoteData)?;
        candidate.normalize()?;
        if candidate.dataset_id != binding.dataset_id
            || candidate.revision != binding.candidate_personal_revision
            || state_hash(&candidate)? != binding.candidate_personal_hash
        {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        let prepared =
            PersonalStateCommit::prepare_for_runtime(candidate.clone(), binding.playlist_revision)?;
        if prepared.state() != &candidate {
            return Err(SyncServiceError::InvalidRemoteData);
        }

        let installed = load_ledger(personal_paths)?.ok_or(SyncServiceError::LocalStateChanged)?;
        let installed_hash = state_hash(&installed)?;
        let installed = if installed.revision == candidate.revision
            && installed_hash == binding.candidate_personal_hash
            && installed == candidate
        {
            installed
        } else if installed.revision == binding.expected_personal_revision
            && installed_hash == binding.expected_personal_hash
        {
            let installed = prepared.commit(personal_paths)?;
            if installed != candidate {
                return Err(SyncServiceError::LocalStateChanged);
            }
            installed
        } else {
            // In particular, never install an older candidate over a newer visible ledger.
            return Err(SyncServiceError::LocalStateChanged);
        };
        checkpoint(AnchorCommitPoint::LedgerInstalled).map_err(|_| SyncServiceError::Storage)?;

        private_store.install_transition_update(
            self.paths.anchor_transition_private(),
            binding.expected_private_revision,
            binding.target_private_revision,
            &binding.target_private_hash,
        )?;
        checkpoint(AnchorCommitPoint::PrivateInstalled).map_err(|_| SyncServiceError::Storage)?;
        let completed_pair_join = binding.activation_kind == AnchorActivationKind::PairJoin;
        self.complete_locked()?;
        if completed_pair_join {
            let _ = remove_owner_only_file_durable(self.paths.pending_join_checkpoint());
            let _ = remove_owner_only_file_durable(self.paths.pending_join_state());
            let _ = remove_owner_only_file_durable(self.paths.pending_join_request());
        }
        Ok(Some(installed))
    }
}

/// Recover a committed cross-store transition without creating sync state when none exists.
pub(crate) fn recover_pending_anchor_transition(
    personal_paths: &PersonalStatePaths,
    sync_paths: &SyncPaths,
) -> Result<Option<PersonalStateV2>, SyncServiceError> {
    let store = TransitionStore::new(sync_paths);
    if !store.has_artifacts()? {
        return Ok(None);
    }
    let _lock = store.acquire_lock()?;
    store.recover_locked(personal_paths, |_| Ok(()))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn commit_with_anchor_transition(
    current_state: &PersonalStateV2,
    commit: &PersonalStateCommit,
    personal_paths: &PersonalStatePaths,
    sync_paths: &SyncPaths,
    target_private: &mut PrivateStoreSnapshot,
    activation_kind: AnchorActivationKind,
    checkpoint_sequence: u64,
    checkpoint_hash: &str,
    playlist_revision: u64,
) -> Result<PersonalStateV2, SyncServiceError> {
    commit_with_anchor_transition_using(
        current_state,
        commit,
        personal_paths,
        sync_paths,
        target_private,
        activation_kind,
        checkpoint_sequence,
        checkpoint_hash,
        playlist_revision,
        |_| Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
fn commit_with_anchor_transition_using(
    current_state: &PersonalStateV2,
    commit: &PersonalStateCommit,
    personal_paths: &PersonalStatePaths,
    sync_paths: &SyncPaths,
    target_private: &mut PrivateStoreSnapshot,
    activation_kind: AnchorActivationKind,
    checkpoint_sequence: u64,
    checkpoint_hash: &str,
    playlist_revision: u64,
    mut checkpoint: impl FnMut(AnchorCommitPoint) -> io::Result<()>,
) -> Result<PersonalStateV2, SyncServiceError> {
    if checkpoint_sequence == 0 || !valid_hash(checkpoint_hash) {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    if target_private.enrollment() != EnrollmentState::Active
        || target_private.checkpoint_sequence() != Some(checkpoint_sequence)
        || target_private.checkpoint_hash() != Some(checkpoint_hash)
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    let store = TransitionStore::new(sync_paths);
    let _lock = store.acquire_lock()?;
    if let Some(recovered) = store.recover_locked(personal_paths, |_| Ok(()))? {
        if recovered != *current_state {
            return Err(SyncServiceError::LocalStateChanged);
        }
        return Ok(recovered);
    }

    let installed = load_ledger(personal_paths)?.ok_or(SyncServiceError::LocalStateChanged)?;
    if installed != *current_state {
        return Err(SyncServiceError::LocalStateChanged);
    }
    if commit.state().dataset_id != current_state.dataset_id {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    let candidate_bytes =
        serde_json::to_vec(commit.state()).map_err(|_| SyncServiceError::Storage)?;
    if candidate_bytes.len() as u64 > crate::data_export::EXPORT_MAX_BYTES {
        return Err(SyncServiceError::Storage);
    }
    write_owner_only_atomic(sync_paths.anchor_transition_ledger(), &candidate_bytes)
        .map_err(|_| SyncServiceError::Storage)?;

    let private_store = PrivateStore::new(sync_paths.private_store())?;
    let staged = private_store
        .stage_transition_update(target_private, sync_paths.anchor_transition_private())?;
    let expected_personal_hash = state_hash(current_state)?;
    let candidate_personal_hash = ledger_payload_hash(&candidate_bytes);
    let activation_hash = activation_hash(
        activation_kind,
        &candidate_personal_hash,
        staged.payload_hash(),
        checkpoint_sequence,
        checkpoint_hash,
    );
    let binding = TransitionBinding {
        kind: TRANSITION_KIND.to_owned(),
        schema_version: TRANSITION_SCHEMA_VERSION,
        dataset_id: current_state.dataset_id.clone(),
        device_id: target_private.device_id().to_owned(),
        activation_kind,
        expected_personal_revision: current_state.revision,
        expected_personal_hash,
        candidate_personal_revision: commit.state().revision,
        candidate_personal_hash,
        playlist_revision,
        expected_private_revision: staged.expected_revision(),
        target_private_revision: staged.target_revision(),
        target_private_hash: staged.payload_hash().to_owned(),
        checkpoint_sequence,
        checkpoint_hash: checkpoint_hash.to_owned(),
        activation_hash,
    };
    validate_binding(&binding)?;
    let signature = sign_serializable(
        TRANSITION_SIGNATURE_DOMAIN,
        target_private.device().signing_key(),
        &binding,
    )?;
    let manifest = TransitionManifest { binding, signature };
    let manifest_bytes = serde_json::to_vec(&manifest).map_err(|_| SyncServiceError::Storage)?;
    write_owner_only_atomic(sync_paths.anchor_transition_manifest(), &manifest_bytes)
        .map_err(|_| SyncServiceError::Storage)?;
    checkpoint(AnchorCommitPoint::Staged).map_err(|_| SyncServiceError::Storage)?;

    write_owner_only_atomic(sync_paths.anchor_transition_commit(), COMMIT_MARKER_BYTES)
        .map_err(|_| SyncServiceError::Storage)?;
    checkpoint(AnchorCommitPoint::Committed).map_err(|_| SyncServiceError::Storage)?;
    store
        .recover_locked(personal_paths, &mut checkpoint)?
        .ok_or(SyncServiceError::Storage)
}

fn validate_binding(binding: &TransitionBinding) -> Result<(), SyncServiceError> {
    validate_dataset_id(&binding.dataset_id).map_err(|_| SyncServiceError::InvalidRemoteData)?;
    if binding.kind != TRANSITION_KIND
        || binding.schema_version != TRANSITION_SCHEMA_VERSION
        || binding.device_id.is_empty()
        || binding.expected_private_revision == 0
        || binding.target_private_revision != binding.expected_private_revision.saturating_add(1)
        || binding.checkpoint_sequence == 0
        || !valid_hash(&binding.expected_personal_hash)
        || !valid_hash(&binding.candidate_personal_hash)
        || !valid_hash(&binding.target_private_hash)
        || !valid_hash(&binding.checkpoint_hash)
        || binding.activation_hash
            != activation_hash(
                binding.activation_kind,
                &binding.candidate_personal_hash,
                &binding.target_private_hash,
                binding.checkpoint_sequence,
                &binding.checkpoint_hash,
            )
    {
        return Err(SyncServiceError::InvalidRemoteData);
    }
    Ok(())
}

fn state_hash(state: &PersonalStateV2) -> Result<String, SyncServiceError> {
    let bytes = serde_json::to_vec(state).map_err(|_| SyncServiceError::Storage)?;
    Ok(ledger_payload_hash(&bytes))
}

fn ledger_payload_hash(bytes: &[u8]) -> String {
    sha256_domain_hex(LEDGER_PAYLOAD_HASH_DOMAIN, &[bytes])
}

fn activation_hash(
    kind: AnchorActivationKind,
    candidate_personal_hash: &str,
    target_private_hash: &str,
    checkpoint_sequence: u64,
    checkpoint_hash: &str,
) -> String {
    let sequence = checkpoint_sequence.to_be_bytes();
    sha256_domain_hex(
        ACTIVATION_HASH_DOMAIN,
        &[
            kind.label(),
            candidate_personal_hash.as_bytes(),
            target_private_hash.as_bytes(),
            &sequence,
            checkpoint_hash.as_bytes(),
        ],
    )
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AnchorCommitPoint {
    Staged,
    Committed,
    LedgerInstalled,
    PrivateInstalled,
}

#[cfg(test)]
mod tests;
