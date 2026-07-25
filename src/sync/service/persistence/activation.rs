//! Persistence-owner writers for setup and pairing-join activation.
//!
//! Network workers may prepare either transition, but the ledger and private enrollment anchor
//! become active only from the PersonalState persistence lane.

use super::*;

impl PersonalSyncPersistence {
    pub fn setup_activation(
        current_state: PersonalStateV2,
        playlist_revision: u64,
        prepared: PreparedSetup,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        let current =
            PersonalStateCommit::prepare_for_runtime(current_state.clone(), playlist_revision)?;
        if current.state() != &current_state {
            return Err(SyncServiceError::LocalStateChanged);
        }
        let target_state = prepared.target_state(&current_state)?;
        let target =
            PersonalStateCommit::prepare_for_runtime(target_state.clone(), playlist_revision)?;
        if target.state() != &target_state {
            return Err(SyncServiceError::LocalStateChanged);
        }
        Ok(Self(Arc::new(PersonalSyncPersistenceInner {
            write: PersonalSyncWrite::Setup {
                current_state,
                playlist_revision,
                prepared: Box::new(prepared),
            },
            target_state,
            personal_paths,
            sync_paths,
            committed: AtomicBool::new(false),
        })))
    }

    pub fn pairing_join_activation(
        current_state: PersonalStateV2,
        playlist_revision: u64,
        prepared: PreparedPairingJoinActivation,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        let prepared = prepared.retarget(&current_state)?;
        let current =
            PersonalStateCommit::prepare_for_runtime(current_state.clone(), playlist_revision)?;
        if current.state() != &current_state {
            return Err(SyncServiceError::LocalStateChanged);
        }
        let target = super::super::pairing::prepare_pairing_join_commit(
            &current_state,
            playlist_revision,
            prepared.target_state().clone(),
        )?;
        let target_state = target.state().clone();
        Ok(Self(Arc::new(PersonalSyncPersistenceInner {
            write: PersonalSyncWrite::PairJoin {
                current_state,
                playlist_revision,
                prepared: Box::new(prepared),
            },
            target_state,
            personal_paths,
            sync_paths,
            committed: AtomicBool::new(false),
        })))
    }

    /// Build the final setup writer after the owner has stopped accepting activation completions.
    ///
    /// Background activation work is joined before this constructor is called. The private store
    /// therefore tells us whether the initial cross-store transition committed: a pending setup is
    /// activated against the latest local state, while an active setup only needs a ledger
    /// reconciliation which retains every possible superseding snapshot.
    #[allow(clippy::too_many_arguments)]
    pub fn setup_activation_shutdown(
        current_state: PersonalStateV2,
        playlist_revision: u64,
        committed_activation_state: PersonalStateV2,
        possible_reconcile_state: Option<PersonalStateV2>,
        prepared: PreparedSetup,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        let _ = recover_pending_anchor_transition(&personal_paths, &sync_paths)?;
        let installed = load_ledger(&personal_paths)?.ok_or(SyncServiceError::LocalStateChanged)?;
        let private = PrivateStore::new(sync_paths.private_store())?.load()?;
        match private.enrollment() {
            EnrollmentState::PendingLedgerCommit
                if private.revision() == prepared.expected_private_revision()
                    && private.device_id() == prepared.device_id().as_str() =>
            {
                Self::setup_activation(
                    current_state,
                    playlist_revision,
                    prepared,
                    personal_paths,
                    sync_paths,
                )
            }
            EnrollmentState::Active
                if active_private_matches(
                    &private,
                    &committed_activation_state,
                    prepared.device_id().as_str(),
                ) =>
            {
                let candidate = prepared.target_state(&current_state)?;
                verify_activation_extension(&committed_activation_state, &candidate)?;
                if let Some(possible) = possible_reconcile_state.as_ref()
                    && installed == *possible
                {
                    verify_activation_extension(possible, &candidate)?;
                }
                let accepted_states = shutdown_accepted_states(
                    installed,
                    current_state,
                    committed_activation_state,
                    possible_reconcile_state,
                );
                Self::reconcile_accepted(
                    accepted_states,
                    candidate,
                    playlist_revision,
                    personal_paths,
                    sync_paths,
                )
            }
            _ => Err(SyncServiceError::LocalStateChanged),
        }
    }

    /// Build the final pairing-join writer after activation completion delivery was retired.
    ///
    /// Once the private store is active, the already imported baseline is immutable. Replanning
    /// from that durable state appends a new deterministic baseline for later local changes rather
    /// than replacing the first import operation.
    #[allow(clippy::too_many_arguments)]
    pub fn pairing_join_activation_shutdown(
        current_state: PersonalStateV2,
        playlist_revision: u64,
        committed_activation_state: PersonalStateV2,
        possible_reconcile_state: Option<PersonalStateV2>,
        prepared: PreparedPairingJoinActivation,
        personal_paths: PersonalStatePaths,
        sync_paths: SyncPaths,
    ) -> Result<Self, SyncServiceError> {
        let _ = recover_pending_anchor_transition(&personal_paths, &sync_paths)?;
        let installed = load_ledger(&personal_paths)?.ok_or(SyncServiceError::LocalStateChanged)?;
        let private = PrivateStore::new(sync_paths.private_store())?.load()?;
        match private.enrollment() {
            EnrollmentState::PendingLedgerCommit
                if private.revision() == prepared.expected_private_revision()
                    && private.device_id() == prepared.device_id() =>
            {
                Self::pairing_join_activation(
                    current_state,
                    playlist_revision,
                    prepared,
                    personal_paths,
                    sync_paths,
                )
            }
            EnrollmentState::Active
                if active_private_matches(
                    &private,
                    &committed_activation_state,
                    prepared.device_id(),
                ) =>
            {
                let durable = if possible_reconcile_state
                    .as_ref()
                    .is_some_and(|candidate| candidate == &installed)
                    || (installed.dataset_id == committed_activation_state.dataset_id
                        && verified_state_extension(&committed_activation_state, &installed)?)
                {
                    installed.clone()
                } else {
                    committed_activation_state.clone()
                };
                let device_id = DeviceId::new(prepared.device_id())
                    .map_err(|_| SyncServiceError::InvalidRemoteData)?;
                let planned =
                    crate::personal_state::plan_join_import(&durable, &current_state, &device_id)?;
                let candidate = super::super::pairing::prepare_pairing_join_commit(
                    &current_state,
                    playlist_revision,
                    planned.candidate,
                )?
                .state()
                .clone();
                verify_activation_extension(&durable, &candidate)?;
                let accepted_states = shutdown_accepted_states(
                    installed,
                    current_state,
                    committed_activation_state,
                    possible_reconcile_state,
                );
                Self::reconcile_accepted(
                    accepted_states,
                    candidate,
                    playlist_revision,
                    personal_paths,
                    sync_paths,
                )
            }
            _ => Err(SyncServiceError::LocalStateChanged),
        }
    }

    pub(super) fn write_setup(
        &self,
        current_state: &PersonalStateV2,
        playlist_revision: u64,
        prepared: &PreparedSetup,
    ) -> Result<PersonalStateV2, SyncServiceError> {
        let recovered =
            recover_pending_anchor_transition(&self.0.personal_paths, &self.0.sync_paths)?;
        if recovered.as_ref() == Some(&self.0.target_state)
            || self.activation_target_is_already_durable(prepared.device_id().as_str())?
        {
            return Ok(self.0.target_state.clone());
        }
        if recovered.is_some_and(|installed| installed != *current_state) {
            return Err(SyncServiceError::LocalStateChanged);
        }
        self.ensure_current_state_durable(current_state, playlist_revision)?;
        super::super::apply_prepared_setup(
            current_state,
            playlist_revision,
            &self.0.personal_paths,
            &self.0.sync_paths,
            prepared.clone(),
        )
        .map(|result| result.state)
    }

    pub(super) fn write_pair_join(
        &self,
        current_state: &PersonalStateV2,
        playlist_revision: u64,
        prepared: &PreparedPairingJoinActivation,
    ) -> Result<PersonalStateV2, SyncServiceError> {
        let recovered =
            recover_pending_anchor_transition(&self.0.personal_paths, &self.0.sync_paths)?;
        if recovered.as_ref() == Some(&self.0.target_state)
            || self.activation_target_is_already_durable(prepared.device_id())?
        {
            return Ok(self.0.target_state.clone());
        }
        if recovered.is_some_and(|installed| installed != *current_state) {
            return Err(SyncServiceError::LocalStateChanged);
        }
        self.ensure_current_state_durable(current_state, playlist_revision)?;
        super::super::apply_prepared_pairing_join(
            current_state,
            playlist_revision,
            &self.0.personal_paths,
            &self.0.sync_paths,
            prepared.clone(),
        )
    }

    fn activation_target_is_already_durable(
        &self,
        device_id: &str,
    ) -> Result<bool, SyncServiceError> {
        if load_ledger(&self.0.personal_paths)?.as_ref() != Some(&self.0.target_state) {
            return Ok(false);
        }
        let private = PrivateStore::new(self.0.sync_paths.private_store())?.load()?;
        let device = self
            .0
            .target_state
            .device_registry
            .values()
            .find(|device| device.device_id.as_str() == device_id);
        Ok(private.enrollment() == EnrollmentState::Active
            && private.dataset_id() == self.0.target_state.dataset_id
            && private.device_id() == device_id
            && device.is_some_and(|record| {
                !record.revoked && private.device().matches_personal_record(record)
            }))
    }
}

fn active_private_matches(
    private: &crate::sync::PrivateStoreSnapshot,
    state: &PersonalStateV2,
    device_id: &str,
) -> bool {
    private.dataset_id() == state.dataset_id
        && private.device_id() == device_id
        && state
            .device_registry
            .values()
            .find(|device| device.device_id.as_str() == device_id)
            .is_some_and(|record| {
                !record.revoked && private.device().matches_personal_record(record)
            })
}

fn shutdown_accepted_states(
    installed: PersonalStateV2,
    current: PersonalStateV2,
    committed: PersonalStateV2,
    possible_reconcile: Option<PersonalStateV2>,
) -> Vec<PersonalStateV2> {
    let mut states = vec![installed, current, committed];
    states.extend(possible_reconcile);
    states
}
