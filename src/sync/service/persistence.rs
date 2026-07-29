//! Persistence-actor payload for a detached manual-sync result.
//!
//! Network workers may populate the remote vault, but only this payload may install their
//! candidate locally. It is retry-safe because the target ledger and private checkpoint anchor
//! are both checked before a repeated write is accepted as complete.

#[path = "persistence/activation.rs"]
mod activation;
#[cfg(test)]
#[path = "persistence/tests.rs"]
mod activation_tests;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::personal_state::{
    DeviceId, Operation, OperationEnvelope, OperationOrigin, PersonalStateCommit,
    PersonalStatePaths, PersonalStateV2, append_operation_as, load_ledger, merge,
};

use super::super::{EnrollmentState, PrivateStore, SyncPaths};
use super::devices::record_revoke_success;
use super::manual::{
    PreparedManualSync, record_sync_failure, record_sync_failure_as, record_sync_started,
    record_sync_success, record_sync_success_as,
};
use super::{
    PreparedPairingApproval, PreparedPairingJoinActivation, PreparedSetup, SyncAttemptKind,
    SyncServiceError, apply_manual_sync, recover_pending_anchor_transition,
};

#[derive(Clone)]
pub enum PersonalSyncApplyKind {
    SyncNow,
    AutomaticSync,
    Revoke(DeviceId),
    PairApprove(Box<PreparedPairingApproval>),
}

/// A clone-cheap, panic-frontier-safe personal-sync write owned by the persistence actor.
#[derive(Clone)]
pub struct PersonalSyncPersistence(Arc<PersonalSyncPersistenceInner>);

struct PersonalSyncPersistenceInner {
    write: PersonalSyncWrite,
    target_state: PersonalStateV2,
    personal_paths: PersonalStatePaths,
    sync_paths: SyncPaths,
    committed: AtomicBool,
}

enum PersonalSyncWrite {
    Initial {
        current_state: PersonalStateV2,
        playlist_revision: u64,
        candidate: Box<PreparedManualSync>,
        action: PersonalSyncApplyKind,
    },
    Reconcile {
        accepted_states: Vec<PersonalStateV2>,
        commit: Box<PersonalStateCommit>,
    },
    Setup {
        current_state: PersonalStateV2,
        playlist_revision: u64,
        prepared: Box<PreparedSetup>,
    },
    PairJoin {
        current_state: PersonalStateV2,
        playlist_revision: u64,
        prepared: Box<PreparedPairingJoinActivation>,
    },
    Shutdown {
        current_state: PersonalStateV2,
        accepted_states: Vec<PersonalStateV2>,
        playlist_revision: u64,
        candidate: Box<PreparedManualSync>,
        commit: Box<PersonalStateCommit>,
        pairing_approval: Option<Box<PreparedPairingApproval>>,
    },
}

impl PersonalSyncPersistence {
    /// Validate and own an initial detached network candidate without writing any local file.
    pub fn initial(
        current_state: PersonalStateV2,
        playlist_revision: u64,
        mut candidate: PreparedManualSync,
        action: PersonalSyncApplyKind,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        if candidate.expected_local_revision != current_state.revision {
            return Err(SyncServiceError::LocalStateChanged);
        }
        if let PersonalSyncApplyKind::PairApprove(prepared) = &action
            && !same_manual_sync_candidate(&candidate, prepared.candidate())
        {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        let prepared_current =
            PersonalStateCommit::prepare_for_runtime(current_state.clone(), playlist_revision)?;
        if prepared_current.state() != &current_state {
            return Err(SyncServiceError::LocalStateChanged);
        }
        let prepared =
            PersonalStateCommit::prepare_for_runtime(candidate.state, playlist_revision)?;
        candidate.state = prepared.state().clone();
        if !verified_state_extension(&current_state, &candidate.state)? {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        let target_state = candidate.state.clone();
        Ok(Self(Arc::new(PersonalSyncPersistenceInner {
            write: PersonalSyncWrite::Initial {
                current_state,
                playlist_revision,
                candidate: Box::new(candidate),
                action,
            },
            target_state,
            personal_paths,
            sync_paths,
            committed: AtomicBool::new(false),
        })))
    }

    /// Validate and own a host approval whose in-flight local suffix was deterministically
    /// re-authored after the authenticated remote checkpoint.
    ///
    /// Unlike an ordinary manual-sync candidate, the target cannot be a byte-for-byte extension
    /// of `current_state`: local operations which raced the remote membership write may need fresh
    /// dots. Recompute that target here from the sealed approval plus both owner snapshots, then
    /// require it to retain the exact network-observed prefix before admitting the writer.
    #[allow(clippy::too_many_arguments)]
    pub fn pairing_approval_activation(
        observed_state: PersonalStateV2,
        current_state: PersonalStateV2,
        playlist_revision: u64,
        prepared: PreparedPairingApproval,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        let prepared = prepared.retarget(&observed_state, &current_state)?;
        let mut candidate = prepared.candidate().clone();
        if candidate.expected_local_revision != current_state.revision {
            return Err(SyncServiceError::LocalStateChanged);
        }
        let prepared_current =
            PersonalStateCommit::prepare_for_runtime(current_state.clone(), playlist_revision)?;
        if prepared_current.state() != &current_state {
            return Err(SyncServiceError::LocalStateChanged);
        }
        let target =
            PersonalStateCommit::prepare_for_runtime(candidate.state.clone(), playlist_revision)?;
        candidate.state = target.state().clone();
        if !verified_state_extension(&observed_state, &candidate.state)? {
            return Err(SyncServiceError::InvalidRemoteData);
        }
        let target_state = candidate.state.clone();
        Ok(Self(Arc::new(PersonalSyncPersistenceInner {
            write: PersonalSyncWrite::Initial {
                current_state,
                playlist_revision,
                candidate: Box::new(candidate),
                action: PersonalSyncApplyKind::PairApprove(Box::new(prepared)),
            },
            target_state,
            personal_paths,
            sync_paths,
            committed: AtomicBool::new(false),
        })))
    }

    /// Prepare a higher-order local reconciliation after the initial candidate became durable.
    pub fn reconcile(
        expected_state: PersonalStateV2,
        alternate_expected_state: PersonalStateV2,
        candidate: PersonalStateV2,
        playlist_revision: u64,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        Self::reconcile_accepted(
            vec![expected_state, alternate_expected_state],
            candidate,
            playlist_revision,
            personal_paths,
            sync_paths,
        )
    }

    fn reconcile_accepted(
        mut accepted_states: Vec<PersonalStateV2>,
        candidate: PersonalStateV2,
        playlist_revision: u64,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        if accepted_states.is_empty() {
            return Err(SyncServiceError::LocalStateChanged);
        }
        accepted_states.dedup();
        let commit = PersonalStateCommit::prepare_for_runtime(candidate, playlist_revision)?;
        let target_state = commit.state().clone();
        let mut extends_accepted_state = false;
        for accepted in &accepted_states {
            if verified_state_extension(accepted, &target_state)? {
                extends_accepted_state = true;
                break;
            }
        }
        if !extends_accepted_state {
            return Err(SyncServiceError::LocalStateChanged);
        }
        Ok(Self(Arc::new(PersonalSyncPersistenceInner {
            write: PersonalSyncWrite::Reconcile {
                accepted_states,
                commit: Box::new(commit),
            },
            target_state,
            personal_paths,
            sync_paths,
            committed: AtomicBool::new(false),
        })))
    }

    /// Prepare the final owner transaction when shutdown races an accepted sync candidate.
    ///
    /// The detached candidate and every local operation observed since its network snapshot are
    /// folded into one target. The writer accepts either side of the persistence race on disk and
    /// is therefore safe whether the initial sync transaction did or did not reach durability.
    pub fn shutdown(
        observed_state: PersonalStateV2,
        current_state: PersonalStateV2,
        playlist_revision: u64,
        candidate: PreparedManualSync,
        local_device: &DeviceId,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        Self::shutdown_with_context(
            observed_state,
            current_state,
            playlist_revision,
            candidate,
            local_device,
            Vec::new(),
            None,
            personal_paths,
            sync_paths,
        )
    }

    /// Preserve a host pairing approval when its activation completion is retired at shutdown.
    ///
    /// The initial activation target and the last reconcile target are accepted in addition to the
    /// ordinary manual-sync race states. This covers a reconcile writer which became durable just
    /// before owner ingress closed, while still applying the original checkpoint transition when
    /// it did not.
    #[allow(clippy::too_many_arguments)]
    pub fn pairing_approval_activation_shutdown(
        observed_state: PersonalStateV2,
        current_state: PersonalStateV2,
        playlist_revision: u64,
        committed_activation_state: PersonalStateV2,
        possible_reconcile_state: Option<PersonalStateV2>,
        prepared: PreparedPairingApproval,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        let mut accepted_states = vec![committed_activation_state];
        accepted_states.extend(possible_reconcile_state);
        let local_device = prepared.candidate().local_device_id.clone();
        Self::shutdown_with_context(
            observed_state,
            current_state,
            playlist_revision,
            prepared.candidate().clone(),
            &local_device,
            accepted_states,
            Some(prepared),
            personal_paths,
            sync_paths,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn shutdown_with_context(
        observed_state: PersonalStateV2,
        current_state: PersonalStateV2,
        playlist_revision: u64,
        mut candidate: PreparedManualSync,
        local_device: &DeviceId,
        additional_accepted_states: Vec<PersonalStateV2>,
        pairing_approval: Option<PreparedPairingApproval>,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        if candidate.expected_local_revision != observed_state.revision
            || candidate.local_device_id != *local_device
        {
            return Err(SyncServiceError::LocalStateChanged);
        }
        let prepared_sync =
            PersonalStateCommit::prepare_for_runtime(candidate.state, playlist_revision)?;
        candidate.state = prepared_sync.state().clone();
        let target = rebase_local_operations(
            &candidate.state,
            &observed_state,
            &current_state,
            local_device,
        )?;
        let commit = PersonalStateCommit::prepare_for_runtime(target, playlist_revision)?;
        let target_state = commit.state().clone();

        let prepared_observed =
            PersonalStateCommit::prepare_for_runtime(observed_state.clone(), playlist_revision)?
                .state()
                .clone();
        let prepared_current =
            PersonalStateCommit::prepare_for_runtime(current_state.clone(), playlist_revision)?
                .state()
                .clone();
        let mut accepted_states = vec![
            observed_state,
            prepared_observed,
            current_state,
            prepared_current.clone(),
            candidate.state.clone(),
        ];
        accepted_states.extend(additional_accepted_states);
        accepted_states.dedup();

        Ok(Self(Arc::new(PersonalSyncPersistenceInner {
            write: PersonalSyncWrite::Shutdown {
                current_state: prepared_current,
                accepted_states,
                playlist_revision,
                candidate: Box::new(candidate),
                commit: Box::new(commit),
                pairing_approval: pairing_approval.map(Box::new),
            },
            target_state,
            personal_paths,
            sync_paths,
            committed: AtomicBool::new(false),
        })))
    }

    pub fn state(&self) -> &PersonalStateV2 {
        &self.0.target_state
    }

    /// True only after this exact payload completed its authoritative local transaction.
    pub fn committed(&self) -> bool {
        self.0.committed.load(Ordering::Acquire)
    }

    pub(crate) fn write(&self) -> Result<(), SyncServiceError> {
        if self.committed() {
            return Ok(());
        }
        let installed = match &self.0.write {
            PersonalSyncWrite::Initial {
                current_state,
                playlist_revision,
                candidate,
                action,
            } => self.write_initial(current_state, *playlist_revision, candidate, action)?,
            PersonalSyncWrite::Reconcile {
                accepted_states,
                commit,
            } => self.write_reconciliation(accepted_states, commit)?,
            PersonalSyncWrite::Setup {
                current_state,
                playlist_revision,
                prepared,
            } => self.write_setup(current_state, *playlist_revision, prepared)?,
            PersonalSyncWrite::PairJoin {
                current_state,
                playlist_revision,
                prepared,
            } => self.write_pair_join(current_state, *playlist_revision, prepared)?,
            PersonalSyncWrite::Shutdown {
                current_state,
                accepted_states,
                playlist_revision,
                candidate,
                commit,
                pairing_approval,
            } => {
                let installed = self.write_shutdown(
                    current_state,
                    accepted_states,
                    *playlist_revision,
                    candidate,
                    commit,
                )?;
                if let Some(prepared) = pairing_approval {
                    super::finalize_prepared_pairing_approval(
                        &installed,
                        &self.0.sync_paths,
                        prepared,
                    )?;
                }
                installed
            }
        };
        if installed != self.0.target_state {
            return Err(SyncServiceError::LocalStateChanged);
        }
        self.0.committed.store(true, Ordering::Release);
        Ok(())
    }

    /// Record the terminal health and audit outcome of a successful attempt.
    fn record_terminal_success(
        &self,
        action: &PersonalSyncApplyKind,
        summary: &crate::sync::manual::ManualSyncSummary,
    ) {
        self.record_terminal_success_at(action, crate::signals::unix_now(), summary);
    }

    fn record_terminal_success_at(
        &self,
        action: &PersonalSyncApplyKind,
        now: i64,
        summary: &crate::sync::manual::ManualSyncSummary,
    ) {
        match action {
            PersonalSyncApplyKind::SyncNow => {
                let _ = record_sync_success(&self.0.sync_paths, now, summary);
            }
            PersonalSyncApplyKind::AutomaticSync => {
                let _ = record_sync_success_as(
                    &self.0.sync_paths,
                    now,
                    summary,
                    SyncAttemptKind::Automatic,
                );
            }
            PersonalSyncApplyKind::Revoke(device_id) => {
                let _ = record_revoke_success(&self.0.sync_paths, now, device_id, summary);
            }
            PersonalSyncApplyKind::PairApprove(_) => {}
        }
    }

    fn write_initial(
        &self,
        current_state: &PersonalStateV2,
        playlist_revision: u64,
        candidate: &PreparedManualSync,
        action: &PersonalSyncApplyKind,
    ) -> Result<PersonalStateV2, SyncServiceError> {
        self.write_initial_using(current_state, playlist_revision, candidate, action, || {
            Ok(())
        })
    }

    fn write_initial_using(
        &self,
        current_state: &PersonalStateV2,
        playlist_revision: u64,
        candidate: &PreparedManualSync,
        action: &PersonalSyncApplyKind,
        after_current_durable: impl FnOnce() -> Result<(), SyncServiceError>,
    ) -> Result<PersonalStateV2, SyncServiceError> {
        if let Some(installed) =
            recover_pending_anchor_transition(&self.0.personal_paths, &self.0.sync_paths)?
            && installed != *current_state
            && installed != self.0.target_state
        {
            return Err(SyncServiceError::LocalStateChanged);
        }
        if self.initial_target_is_already_durable(candidate)? {
            if let PersonalSyncApplyKind::PairApprove(prepared) = action {
                super::finalize_prepared_pairing_approval(
                    &self.0.target_state,
                    &self.0.sync_paths,
                    prepared,
                )?;
            }
            // The attempt still talked to the server, and preparing it marked health `Syncing`.
            // A no-op result must record its terminal outcome too, or the status stays stuck on
            // "Syncing" — which `status` reports as Needs attention / local changes arrived — and
            // no audit row ever explains the attempt.
            self.record_terminal_success(action, &candidate.summary);
            return Ok(self.0.target_state.clone());
        }

        let now = crate::signals::unix_now();
        let _ = record_sync_started(&self.0.sync_paths, now);
        let result = (|| {
            self.ensure_current_state_durable(current_state, playlist_revision)?;
            after_current_durable()?;
            let installed = apply_manual_sync(
                current_state,
                playlist_revision,
                candidate.clone(),
                &self.0.personal_paths,
                &self.0.sync_paths,
            )?;
            if let PersonalSyncApplyKind::PairApprove(prepared) = action {
                super::finalize_prepared_pairing_approval(
                    &installed,
                    &self.0.sync_paths,
                    prepared,
                )?;
            }
            Ok(installed)
        })();
        match &result {
            Ok(_) => self.record_terminal_success_at(action, now, &candidate.summary),
            Err(error) => match action {
                PersonalSyncApplyKind::AutomaticSync => {
                    let _ = record_sync_failure_as(
                        &self.0.sync_paths,
                        now,
                        *error,
                        SyncAttemptKind::Automatic,
                    );
                }
                _ => {
                    let _ = record_sync_failure(&self.0.sync_paths, now, *error);
                }
            },
        }
        result
    }

    fn ensure_current_state_durable(
        &self,
        current_state: &PersonalStateV2,
        playlist_revision: u64,
    ) -> Result<PersonalStateV2, SyncServiceError> {
        let installed =
            load_ledger(&self.0.personal_paths)?.ok_or(SyncServiceError::LocalStateChanged)?;
        roll_forward_verified_extension(
            installed,
            current_state,
            playlist_revision,
            &self.0.personal_paths,
        )
    }

    fn initial_target_is_already_durable(
        &self,
        candidate: &PreparedManualSync,
    ) -> Result<bool, SyncServiceError> {
        if load_ledger(&self.0.personal_paths)?.as_ref() != Some(&self.0.target_state) {
            return Ok(false);
        }
        let private = PrivateStore::new(self.0.sync_paths.private_store())?.load()?;
        let target_hash = candidate
            .checkpoint_anchor
            .checkpoint_hash
            .as_deref()
            .ok_or(SyncServiceError::InvalidRemoteData)?;
        let anchor_matches = private.checkpoint_sequence()
            == Some(candidate.checkpoint_anchor.checkpoint_sequence)
            && private.checkpoint_hash() == Some(target_hash);
        let revision_matches = private.revision() == candidate.expected_private_revision
            || candidate.expected_private_revision.checked_add(1) == Some(private.revision());
        Ok(anchor_matches
            && revision_matches
            && private.dataset_id() == self.0.target_state.dataset_id
            && private.device_id() == candidate.local_device_id.as_str()
            && private.enrollment() == EnrollmentState::Active)
    }

    fn write_reconciliation(
        &self,
        accepted_states: &[PersonalStateV2],
        commit: &PersonalStateCommit,
    ) -> Result<PersonalStateV2, SyncServiceError> {
        let _ = recover_pending_anchor_transition(&self.0.personal_paths, &self.0.sync_paths)?;
        let installed =
            load_ledger(&self.0.personal_paths)?.ok_or(SyncServiceError::LocalStateChanged)?;
        if installed == self.0.target_state {
            return Ok(installed);
        }
        if !accepted_states.iter().any(|state| state == &installed) {
            return Err(SyncServiceError::LocalStateChanged);
        }
        let installed = commit.commit(&self.0.personal_paths)?;
        if installed == self.0.target_state {
            Ok(installed)
        } else {
            Err(SyncServiceError::LocalStateChanged)
        }
    }

    fn write_shutdown(
        &self,
        current_state: &PersonalStateV2,
        accepted_states: &[PersonalStateV2],
        playlist_revision: u64,
        candidate: &PreparedManualSync,
        commit: &PersonalStateCommit,
    ) -> Result<PersonalStateV2, SyncServiceError> {
        let _ = recover_pending_anchor_transition(&self.0.personal_paths, &self.0.sync_paths)?;
        let mut installed =
            load_ledger(&self.0.personal_paths)?.ok_or(SyncServiceError::LocalStateChanged)?;
        let private = self.validate_shutdown_private(candidate)?;
        let anchor_matches = shutdown_anchor_matches(&private, candidate)?;
        if installed != self.0.target_state
            && !accepted_states.iter().any(|state| state == &installed)
        {
            // A reconciliation may have committed a strict prefix of the final local suffix before
            // its owner completion was retired by shutdown. Plainly extend that prefix only after
            // the candidate's private checkpoint anchor is active; an old anchor must still cross
            // the ledger/private decision marker below as one transaction.
            installed =
                if anchor_matches && verified_state_extension(&installed, &self.0.target_state)? {
                    let installed = commit.commit(&self.0.personal_paths)?;
                    if installed != self.0.target_state {
                        return Err(SyncServiceError::LocalStateChanged);
                    }
                    installed
                } else {
                    roll_forward_verified_extension(
                        installed,
                        current_state,
                        playlist_revision,
                        &self.0.personal_paths,
                    )?
                };
        }
        if installed != self.0.target_state
            && !accepted_states.iter().any(|state| state == &installed)
        {
            return Err(SyncServiceError::LocalStateChanged);
        }

        if anchor_matches {
            if installed == self.0.target_state {
                return Ok(installed);
            }
            let installed = commit.commit(&self.0.personal_paths)?;
            return if installed == self.0.target_state {
                Ok(installed)
            } else {
                Err(SyncServiceError::LocalStateChanged)
            };
        }
        if private.revision() != candidate.expected_private_revision {
            return Err(SyncServiceError::LocalStateChanged);
        }

        let mut final_candidate = candidate.clone();
        final_candidate.expected_local_revision = installed.revision;
        final_candidate.state = self.0.target_state.clone();
        let installed = apply_manual_sync(
            &installed,
            playlist_revision,
            final_candidate,
            &self.0.personal_paths,
            &self.0.sync_paths,
        )?;
        if installed == self.0.target_state {
            Ok(installed)
        } else {
            Err(SyncServiceError::LocalStateChanged)
        }
    }

    fn validate_shutdown_private(
        &self,
        candidate: &PreparedManualSync,
    ) -> Result<crate::sync::PrivateStoreSnapshot, SyncServiceError> {
        let private = PrivateStore::new(self.0.sync_paths.private_store())?.load()?;
        if private.dataset_id() != self.0.target_state.dataset_id
            || private.device_id() != candidate.local_device_id.as_str()
            || private.enrollment() != EnrollmentState::Active
        {
            return Err(SyncServiceError::LocalStateChanged);
        }
        Ok(private)
    }
}

fn same_manual_sync_candidate(left: &PreparedManualSync, right: &PreparedManualSync) -> bool {
    left.state == right.state
        && left.membership == right.membership
        && left.checkpoint_anchor == right.checkpoint_anchor
        && left.expected_local_revision == right.expected_local_revision
        && left.expected_private_revision == right.expected_private_revision
        && left.local_device_id == right.local_device_id
        && left.summary == right.summary
}

fn roll_forward_verified_extension(
    installed: PersonalStateV2,
    current: &PersonalStateV2,
    playlist_revision: u64,
    personal_paths: &PersonalStatePaths,
) -> Result<PersonalStateV2, SyncServiceError> {
    if installed == *current {
        return Ok(installed);
    }
    if !verified_state_extension(&installed, current)? {
        return Err(SyncServiceError::LocalStateChanged);
    }
    let commit = PersonalStateCommit::prepare_for_runtime(current.clone(), playlist_revision)?;
    if commit.state() != current {
        return Err(SyncServiceError::LocalStateChanged);
    }
    let installed = commit.commit(personal_paths)?;
    if installed == *current {
        Ok(installed)
    } else {
        Err(SyncServiceError::LocalStateChanged)
    }
}

pub(crate) fn verify_activation_extension(
    base: &PersonalStateV2,
    extension: &PersonalStateV2,
) -> Result<(), SyncServiceError> {
    if verified_state_extension(base, extension)? {
        Ok(())
    } else {
        Err(SyncServiceError::LocalStateChanged)
    }
}

fn verified_state_extension(
    base: &PersonalStateV2,
    extension: &PersonalStateV2,
) -> Result<bool, SyncServiceError> {
    base.validate()?;
    extension.validate()?;
    if base == extension {
        return Ok(true);
    }
    if extension.revision <= base.revision {
        return Ok(false);
    }
    merge_preserves_extension(base, extension)
}

fn merge_preserves_extension(
    base: &PersonalStateV2,
    extension: &PersonalStateV2,
) -> Result<bool, SyncServiceError> {
    base.validate()?;
    extension.validate()?;
    if extension.dataset_id != base.dataset_id
        || extension.metadata != base.metadata
        || base
            .version_vector
            .0
            .iter()
            .any(|(device, sequence)| extension.version_vector.observed(device) < *sequence)
    {
        return Ok(false);
    }
    // Personal-state merge owns the trust rules for compaction checkpoint ancestry, covered
    // engagement pruning, and anti-resurrection. If joining the prior state into the candidate
    // produces the candidate itself, it is either a plain operation extension or a legitimate
    // compaction descendant. Arbitrary operation deletion cannot satisfy this fixed-point check.
    let Ok((mut joined, _)) = merge(base, extension) else {
        return Ok(false);
    };
    // This fingerprint is a local runtime projection cache, not part of causal merge identity.
    joined.projection_fingerprint = extension.projection_fingerprint.clone();
    Ok(joined == *extension)
}

fn shutdown_anchor_matches(
    private: &crate::sync::PrivateStoreSnapshot,
    candidate: &PreparedManualSync,
) -> Result<bool, SyncServiceError> {
    let target_hash = candidate
        .checkpoint_anchor
        .checkpoint_hash
        .as_deref()
        .ok_or(SyncServiceError::InvalidRemoteData)?;
    Ok(
        private.checkpoint_sequence() == Some(candidate.checkpoint_anchor.checkpoint_sequence)
            && private.checkpoint_hash() == Some(target_hash),
    )
}

/// Re-author local operations that appeared while a detached sync candidate was in flight.
///
/// A remote operation from the same device can legitimately occupy the dot the live runtime used
/// meanwhile. Re-appending only the observed local suffix against the durable candidate gives
/// every operation a fresh dot while retaining its payload and display timestamp.
pub(crate) fn rebase_local_operations(
    durable: &PersonalStateV2,
    observed: &PersonalStateV2,
    current: &PersonalStateV2,
    local_device: &DeviceId,
) -> Result<PersonalStateV2, SyncServiceError> {
    durable.validate()?;
    observed.validate()?;
    current.validate()?;
    if durable.dataset_id != observed.dataset_id
        || current.dataset_id != observed.dataset_id
        || durable.metadata != observed.metadata
        || current.metadata != observed.metadata
        || current.compaction_checkpoint != observed.compaction_checkpoint
    {
        return Err(SyncServiceError::LocalStateChanged);
    }
    // Detached manual-sync candidates deliberately keep the observed local revision until the
    // persistence owner prepares them. Their causal content must still be a merge fixed point.
    if !merge_preserves_extension(observed, durable)? {
        return Err(SyncServiceError::LocalStateChanged);
    }

    let observed_by_id: BTreeMap<&str, &OperationEnvelope> = observed
        .operations
        .iter()
        .map(|operation| (operation.operation_id.as_str(), operation))
        .collect();
    let current_by_id: BTreeMap<&str, &OperationEnvelope> = current
        .operations
        .iter()
        .map(|operation| (operation.operation_id.as_str(), operation))
        .collect();
    if observed_by_id
        .iter()
        .any(|(id, operation)| current_by_id.get(id).copied() != Some(*operation))
    {
        return Err(SyncServiceError::LocalStateChanged);
    }

    let durable_by_id: BTreeMap<&str, &OperationEnvelope> = durable
        .operations
        .iter()
        .map(|operation| (operation.operation_id.as_str(), operation))
        .collect();
    let mut additions: Vec<&OperationEnvelope> = current
        .operations
        .iter()
        .filter(|operation| !observed_by_id.contains_key(operation.operation_id.as_str()))
        .collect();
    additions.sort_by(|left, right| {
        left.stamp
            .dot
            .cmp(&right.stamp.dot)
            .then(left.operation_id.cmp(&right.operation_id))
    });

    let mut expected_vector = observed.version_vector.clone();
    for addition in &additions {
        let expected_sequence = expected_vector
            .observed(local_device)
            .checked_add(1)
            .ok_or(SyncServiceError::LocalStateChanged)?;
        if addition.origin != OperationOrigin::Local
            || addition.stamp.dot.device_id != *local_device
            || addition.stamp.dot.sequence != expected_sequence
            || addition.stamp.observed != expected_vector
            || addition.operation_id != format!("{}:{expected_sequence}", local_device.as_str())
            || matches!(
                addition.operation,
                Operation::AddDevice { .. } | Operation::RevokeDevice { .. }
            )
        {
            return Err(SyncServiceError::LocalStateChanged);
        }
        expected_vector.observe(&addition.stamp.dot);
    }
    if current.version_vector != expected_vector {
        return Err(SyncServiceError::LocalStateChanged);
    }

    let mut rebased = durable.clone();
    let mut appended = false;
    for addition in additions {
        if durable_by_id.get(addition.operation_id.as_str()).copied() == Some(addition) {
            continue;
        }
        rebased = append_operation_as(
            &rebased,
            local_device,
            addition.operation.clone(),
            addition.stamp.recorded_at_unix,
        )?;
        appended = true;
    }
    if appended {
        rebased.revision = PersonalStateV2::revision_after(durable.revision.max(current.revision))?;
        rebased.projection_fingerprint = None;
        rebased.normalize()?;
    }
    Ok(rebased)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::personal_state::{
        CausalStamp, DeviceRecord, Dot, Operation, OperationOrigin, PortableTrack,
        PortableTrackKey, Rating, VersionVector,
    };
    use crate::sync::{
        CheckpointAnchor, DeviceSecretMaterial, MembershipAnchor, MembershipChain,
        PrivateStoreSnapshot, RecoveryKit, SignedCheckpoint, SignedMembershipRoot,
    };

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    pub(super) struct ShutdownFixture {
        pub(super) root: std::path::PathBuf,
        pub(super) personal_paths: PersonalStatePaths,
        pub(super) initial: PersonalStateV2,
        pub(super) local: PersonalStateV2,
        pub(super) candidate: PreparedManualSync,
        pub(super) device_id: DeviceId,
        pub(super) checkpoint_sequence: u64,
    }

    impl Drop for ShutdownFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    pub(super) fn shutdown_fixture() -> ShutdownFixture {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "yututui-sync-shutdown-{}-{sequence}",
            std::process::id()
        ));
        crate::util::safe_fs::ensure_private_dir(&root).unwrap();
        let personal_paths = PersonalStatePaths::for_data_root(root.clone());
        let sync_paths = SyncPaths::for_data_root(root.clone());
        crate::util::safe_fs::ensure_private_dir(sync_paths.root()).unwrap();

        let dataset_id = "shutdown-union".to_owned();
        let recovery = RecoveryKit::generate(dataset_id.clone(), None).unwrap();
        let device = DeviceSecretMaterial::generate_for("shutdown-device").unwrap();
        let device_id = DeviceId::new(device.device_id()).unwrap();
        let device_record = DeviceRecord {
            device_id: device_id.clone(),
            name: "Shutdown device".to_owned(),
            revoked: false,
            public_identity: Some(device.public_identity()),
        };
        let membership_root = SignedMembershipRoot::create(
            dataset_id.clone(),
            recovery.recovery_recipient(),
            &recovery.signing_key().unwrap(),
            device_record.clone(),
        )
        .unwrap();
        let membership_anchor = MembershipAnchor::RootHash(membership_root.hash().unwrap());
        let membership = MembershipChain::new(membership_root);
        let dot = Dot {
            device_id: device_id.clone(),
            sequence: 1,
        };
        let mut state = PersonalStateV2::empty(dataset_id).unwrap();
        state.operations.push(OperationEnvelope {
            operation_id: "shutdown-device:1".to_owned(),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: VersionVector::default(),
                recorded_at_unix: 1,
            },
            origin: OperationOrigin::Local,
            operation: Operation::AddDevice {
                device: device_record,
            },
        });
        state.version_vector.observe(&dot);
        crate::personal_state::refresh_device_registry(&mut state).unwrap();
        state.normalize().unwrap();
        let initial_commit = PersonalStateCommit::prepare_for_runtime(state, 7).unwrap();
        let initial = initial_commit.state().clone();
        initial_commit.commit(&personal_paths).unwrap();

        let checkpoint = SignedCheckpoint::create(
            membership.clone(),
            &membership_anchor,
            device_id.clone(),
            device.signing_key(),
            &CheckpointAnchor::default(),
            initial.clone(),
        )
        .unwrap();
        let checkpoint_sequence = checkpoint.payload.checkpoint_sequence;
        let mut private = PrivateStoreSnapshot::pending_ledger_commit(
            device,
            recovery.recovery_recipient(),
            recovery.recovery_verifying_key().unwrap(),
            match membership_anchor {
                MembershipAnchor::RootHash(hash) => hash,
                MembershipAnchor::RecoveryVerifyingKey(_) => unreachable!(),
            },
            &checkpoint,
        )
        .unwrap();
        private.mark_active(&checkpoint, &initial).unwrap();
        PrivateStore::new(sync_paths.private_store())
            .unwrap()
            .create(&mut private)
            .unwrap();

        let remote = append_operation_as(
            &initial,
            &device_id,
            Operation::SetRating {
                track: PortableTrack {
                    key: PortableTrackKey::Catalog {
                        provider: "youtube".to_owned(),
                        exact_catalog_id: "shutdown-track".to_owned(),
                    },
                    title: "Shutdown track".to_owned(),
                    artist: "Remote artist".to_owned(),
                    album: None,
                    duration_secs: None,
                    isrc: None,
                },
                rating: Rating::Liked,
            },
            2,
        )
        .unwrap();
        let local = append_operation_as(
            &initial,
            &device_id,
            Operation::SetAvoidArtist {
                artist_key: "local-artist".to_owned(),
                avoid: true,
            },
            3,
        )
        .unwrap();
        let candidate = PreparedManualSync {
            state: remote,
            membership,
            checkpoint_anchor: CheckpointAnchor::from_trusted(
                checkpoint_sequence + 1,
                "b".repeat(64),
            )
            .unwrap(),
            expected_local_revision: initial.revision,
            expected_private_revision: private.revision(),
            local_device_id: device_id.clone(),
            summary: crate::sync::manual::ManualSyncSummary::default(),
        };
        ShutdownFixture {
            root,
            personal_paths,
            initial,
            local,
            candidate,
            device_id,
            checkpoint_sequence,
        }
    }

    fn pending_local_commit_and_remote_candidate(
        fixture: &ShutdownFixture,
    ) -> (PersonalStateCommit, PreparedManualSync) {
        let current_commit =
            PersonalStateCommit::prepare_for_runtime(fixture.local.clone(), 7).unwrap();
        let current = current_commit.state().clone();
        let remote = append_operation_as(
            &current,
            &fixture.device_id,
            Operation::SetRating {
                track: PortableTrack {
                    key: PortableTrackKey::Catalog {
                        provider: "youtube".to_owned(),
                        exact_catalog_id: "coalesced-remote-track".to_owned(),
                    },
                    title: "Coalesced remote track".to_owned(),
                    artist: "Remote artist".to_owned(),
                    album: None,
                    duration_secs: None,
                    isrc: None,
                },
                rating: Rating::Liked,
            },
            4,
        )
        .unwrap();
        let mut candidate = fixture.candidate.clone();
        candidate.expected_local_revision = current.revision;
        candidate.state = remote;
        (current_commit, candidate)
    }

    #[test]
    fn reconciliation_accepts_newer_observed_local_branch_after_sync_commit() {
        let durable =
            PersonalStateV2::empty("persistence-durable".to_owned()).expect("durable state");
        let mut local = durable.clone();
        local.revision = 9;
        let unrelated =
            PersonalStateV2::empty("persistence-unrelated".to_owned()).expect("other state");

        let accepted = [durable.clone(), local.clone()];
        assert!(accepted.contains(&durable));
        assert!(accepted.contains(&local));
        assert!(!accepted.contains(&unrelated));
    }

    #[test]
    fn initial_retry_after_local_roll_forward_preserves_both_sides() {
        let fixture = shutdown_fixture();
        let (current_commit, candidate) = pending_local_commit_and_remote_candidate(&fixture);
        let current = current_commit.state().clone();
        assert!(
            !verified_state_extension(&fixture.initial, &fixture.local).unwrap(),
            "an operation extension without a monotonic revision must not be accepted"
        );
        assert!(verified_state_extension(&fixture.initial, &current).unwrap());

        let sync_paths = SyncPaths::for_data_root(fixture.root.clone());
        let writer = PersonalSyncPersistence::initial(
            current.clone(),
            7,
            candidate,
            PersonalSyncApplyKind::SyncNow,
            fixture.personal_paths.clone(),
            SyncPaths::for_data_root(fixture.root.clone()),
        )
        .unwrap();
        let PersonalSyncWrite::Initial {
            current_state,
            playlist_revision,
            candidate,
            action,
        } = &writer.0.write
        else {
            unreachable!("initial writer")
        };
        assert_eq!(
            writer.write_initial_using(
                current_state,
                *playlist_revision,
                candidate,
                action,
                || Err(SyncServiceError::Storage),
            ),
            Err(SyncServiceError::Storage)
        );
        assert!(!writer.committed());
        assert_eq!(load_ledger(&fixture.personal_paths).unwrap(), Some(current));
        let private = PrivateStore::new(sync_paths.private_store())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            private.checkpoint_sequence(),
            Some(fixture.checkpoint_sequence)
        );
        assert_ne!(private.checkpoint_hash(), Some("b".repeat(64).as_str()));
        assert!(!sync_paths.anchor_transition_manifest().exists());
        assert!(!sync_paths.anchor_transition_ledger().exists());
        assert!(!sync_paths.anchor_transition_private().exists());
        assert!(!sync_paths.anchor_transition_commit().exists());

        writer.write().unwrap();
        assert!(writer.committed());
        assert_eq!(
            load_ledger(&fixture.personal_paths).unwrap(),
            Some(writer.state().clone())
        );
        let private = PrivateStore::new(sync_paths.private_store())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            private.checkpoint_sequence(),
            Some(fixture.checkpoint_sequence + 1)
        );
        assert_eq!(private.checkpoint_hash(), Some("b".repeat(64).as_str()));
        assert!(
            recover_pending_anchor_transition(&fixture.personal_paths, &sync_paths)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn actor_coalescing_rolls_forward_evicted_local_snapshot_before_sync() {
        let fixture = shutdown_fixture();
        let (current_commit, candidate) = pending_local_commit_and_remote_candidate(&fixture);
        let expected_current = current_commit.state().clone();
        let sync_paths = SyncPaths::for_data_root(fixture.root.clone());
        let writer = PersonalSyncPersistence::initial(
            expected_current.clone(),
            7,
            candidate,
            PersonalSyncApplyKind::SyncNow,
            fixture.personal_paths.clone(),
            SyncPaths::for_data_root(fixture.root.clone()),
        )
        .unwrap();
        let expected_target = writer.state().clone();
        let override_value = fixture.root.to_string_lossy().into_owned();

        crate::test_util::env::with_var("YTM_DATA_DIR", Some(&override_value), || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let handle = crate::persist::spawn();
                    assert_eq!(
                        handle
                            .save(crate::persist::Snapshot::PersonalState(Box::new(
                                current_commit,
                            )))
                            .unwrap(),
                        crate::util::delivery::DeliveryReceipt::Enqueued
                    );
                    let target = handle
                        .save_tracked(crate::persist::Snapshot::PersonalSync(writer.clone()))
                        .unwrap();
                    assert!(handle.has_pending_for_test(crate::persist::StoreKind::PersonalState));
                    assert_eq!(
                        handle
                            .flush_target(target, std::time::Duration::from_secs(2))
                            .await,
                        crate::persist::TargetFlushOutcome::CommittedExact
                    );
                    assert!(writer.committed());
                    handle.seal_with_snapshots([]).unwrap();
                    assert!(handle.flush(std::time::Duration::from_secs(2)).await);
                });
        });

        let installed = load_ledger(&fixture.personal_paths).unwrap().unwrap();
        assert_eq!(installed, expected_target);
        assert!(
            installed
                .operations
                .iter()
                .any(|operation| operation.operation_id
                    == expected_current.operations.last().unwrap().operation_id)
        );
        assert!(installed.operations.iter().any(|operation| matches!(
            operation.operation,
            Operation::SetRating {
                rating: Rating::Liked,
                ..
            }
        )));
        let private = PrivateStore::new(sync_paths.private_store())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            private.checkpoint_sequence(),
            Some(fixture.checkpoint_sequence + 1)
        );
    }

    #[test]
    fn shutdown_rolls_forward_an_evicted_observed_snapshot() {
        let fixture = shutdown_fixture();
        let (current_commit, candidate) = pending_local_commit_and_remote_candidate(&fixture);
        let current = current_commit.state().clone();
        let writer = PersonalSyncPersistence::shutdown(
            current.clone(),
            current,
            7,
            candidate,
            &fixture.device_id,
            fixture.personal_paths.clone(),
            SyncPaths::for_data_root(fixture.root.clone()),
        )
        .unwrap();

        writer.write().unwrap();

        assert!(writer.committed());
        assert_eq!(
            load_ledger(&fixture.personal_paths).unwrap(),
            Some(writer.state().clone())
        );
    }

    #[test]
    fn shutdown_after_initial_sync_commit_persists_union_for_restart() {
        let fixture = shutdown_fixture();
        assert_eq!(
            fixture.candidate.state.operations.last().unwrap().stamp.dot,
            fixture.local.operations.last().unwrap().stamp.dot,
            "the test must exercise the same-revision, same-dot shutdown race"
        );

        let initial_writer = PersonalSyncPersistence::initial(
            fixture.initial.clone(),
            7,
            fixture.candidate.clone(),
            PersonalSyncApplyKind::SyncNow,
            fixture.personal_paths.clone(),
            SyncPaths::for_data_root(fixture.root.clone()),
        )
        .unwrap();
        initial_writer.write().unwrap();
        assert_eq!(
            load_ledger(&fixture.personal_paths).unwrap(),
            Some(initial_writer.state().clone())
        );

        let shutdown_writer = PersonalSyncPersistence::shutdown(
            fixture.initial.clone(),
            fixture.local.clone(),
            7,
            fixture.candidate.clone(),
            &fixture.device_id,
            fixture.personal_paths.clone(),
            SyncPaths::for_data_root(fixture.root.clone()),
        )
        .unwrap();
        let expected = shutdown_writer.state().clone();
        shutdown_writer.write().unwrap();
        assert!(shutdown_writer.committed());

        let restarted = load_ledger(&fixture.personal_paths).unwrap().unwrap();
        assert_eq!(restarted, expected);
        assert!(restarted.operations.iter().any(|operation| matches!(
            operation.operation,
            Operation::SetRating {
                rating: Rating::Liked,
                ..
            }
        )));
        assert!(restarted.operations.iter().any(|operation| matches!(
            operation.operation,
            Operation::SetAvoidArtist {
                ref artist_key,
                avoid: true
            } if artist_key == "local-artist"
        )));
        let dots: BTreeSet<_> = restarted
            .operations
            .iter()
            .map(|operation| operation.stamp.dot.clone())
            .collect();
        assert_eq!(dots.len(), restarted.operations.len());

        let sync_paths = SyncPaths::for_data_root(fixture.root.clone());
        let private = PrivateStore::new(sync_paths.private_store())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            private.checkpoint_sequence(),
            Some(fixture.checkpoint_sequence + 1)
        );
        assert_eq!(private.checkpoint_hash(), Some("b".repeat(64).as_str()));
        assert!(
            recover_pending_anchor_transition(&fixture.personal_paths, &sync_paths)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            PersonalStateCommit::prepare_for_runtime(restarted, 7)
                .unwrap()
                .state(),
            &expected
        );
    }

    #[test]
    fn shutdown_rolls_a_partial_reconciliation_forward_after_anchor_commit() {
        let fixture = shutdown_fixture();
        let sync_paths = SyncPaths::for_data_root(fixture.root.clone());
        let initial_writer = PersonalSyncPersistence::initial(
            fixture.initial.clone(),
            7,
            fixture.candidate.clone(),
            PersonalSyncApplyKind::SyncNow,
            fixture.personal_paths.clone(),
            SyncPaths::for_data_root(fixture.root.clone()),
        )
        .unwrap();
        initial_writer.write().unwrap();

        let first_current_commit =
            PersonalStateCommit::prepare_for_runtime(fixture.local.clone(), 7).unwrap();
        let first_current = first_current_commit.state().clone();
        let partial = rebase_local_operations(
            initial_writer.state(),
            &fixture.initial,
            &first_current,
            &fixture.device_id,
        )
        .unwrap();
        let partial_commit = PersonalStateCommit::prepare_for_runtime(partial, 7).unwrap();
        let partial = partial_commit.state().clone();
        assert_eq!(
            partial_commit.commit(&fixture.personal_paths).unwrap(),
            partial
        );

        let later_local = append_operation_as(
            &first_current,
            &fixture.device_id,
            Operation::SetAvoidArtist {
                artist_key: "later-local-artist".to_owned(),
                avoid: true,
            },
            5,
        )
        .unwrap();
        let current = PersonalStateCommit::prepare_for_runtime(later_local, 7)
            .unwrap()
            .state()
            .clone();
        let shutdown_writer = PersonalSyncPersistence::shutdown(
            fixture.initial.clone(),
            current,
            7,
            fixture.candidate.clone(),
            &fixture.device_id,
            fixture.personal_paths.clone(),
            SyncPaths::for_data_root(fixture.root.clone()),
        )
        .unwrap();
        assert!(verified_state_extension(&partial, shutdown_writer.state()).unwrap());

        shutdown_writer.write().unwrap();

        assert!(shutdown_writer.committed());
        assert_eq!(
            load_ledger(&fixture.personal_paths).unwrap(),
            Some(shutdown_writer.state().clone())
        );
        assert!(
            shutdown_writer
                .state()
                .operations
                .iter()
                .any(|operation| matches!(
                    operation.operation,
                    Operation::SetAvoidArtist {
                        ref artist_key,
                        avoid: true
                    } if artist_key == "later-local-artist"
                ))
        );
        let private = PrivateStore::new(sync_paths.private_store())
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            private.checkpoint_sequence(),
            Some(fixture.checkpoint_sequence + 1)
        );
        assert_eq!(private.checkpoint_hash(), Some("b".repeat(64).as_str()));
    }
}
