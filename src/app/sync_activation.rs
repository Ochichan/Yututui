//! Owner-lane completion for setup and device-pairing activation.
//!
//! Network workers only prepare transitions. This module sends the exact candidate through the
//! PersonalState persistence lane, rebasing any local operations that race the durable commit.

use super::sync_ui::localized_sync_error;
use super::*;
use crate::sync::service::{
    PreparedPairingApproval, PreparedPairingJoinActivation, PreparedSetup, SyncServiceError,
};

const MAX_ACTIVATION_RETRIES: u8 = 3;

fn activation_retry_limit_reached(
    stage: &SyncActivationCommitStage,
    attempt: u8,
    count_initial_attempt: bool,
) -> bool {
    count_initial_attempt
        && matches!(stage, SyncActivationCommitStage::Initial)
        && attempt >= MAX_ACTIVATION_RETRIES
}

#[derive(Clone)]
pub(crate) enum SyncActivationKind {
    Setup(PreparedSetup),
    PairJoin(PreparedPairingJoinActivation),
    PairApprove {
        prepared: PreparedPairingApproval,
        network_observed_state: crate::personal_state::PersonalStateV2,
    },
}

impl SyncActivationKind {
    pub(crate) fn device_id(
        &self,
    ) -> Result<Option<crate::personal_state::DeviceId>, SyncServiceError> {
        match self {
            Self::Setup(prepared) => Ok(Some(prepared.device_id().clone())),
            Self::PairJoin(prepared) => crate::personal_state::DeviceId::new(prepared.device_id())
                .map(Some)
                .map_err(|_| SyncServiceError::InvalidRemoteData),
            Self::PairApprove { .. } => Ok(None),
        }
    }

    pub(crate) fn target_state_for(
        &self,
        current: &crate::personal_state::PersonalStateV2,
    ) -> Result<crate::personal_state::PersonalStateV2, SyncServiceError> {
        match self {
            Self::Setup(prepared) => prepared.target_state(current),
            Self::PairJoin(prepared) => prepared.target_state_for(current),
            Self::PairApprove {
                prepared,
                network_observed_state,
            } => Ok(prepared
                .retarget(network_observed_state, current)?
                .candidate()
                .state
                .clone()),
        }
    }

    /// Extend an activation which has already crossed its private/ledger commit boundary.
    ///
    /// A joining device is special: its first deletion-free import is now part of the
    /// authenticated dataset, so every later local snapshot must extend that durable baseline
    /// instead of rebuilding a replacement baseline from the original checkpoint.
    fn target_state_after_commit(
        &self,
        durable: &crate::personal_state::PersonalStateV2,
        current: &crate::personal_state::PersonalStateV2,
    ) -> Result<crate::personal_state::PersonalStateV2, SyncServiceError> {
        match self {
            Self::PairJoin(prepared) => {
                let device_id = crate::personal_state::DeviceId::new(prepared.device_id())
                    .map_err(|_| SyncServiceError::InvalidRemoteData)?;
                let mut candidate =
                    crate::personal_state::plan_join_import(durable, current, &device_id)?
                        .candidate;
                if candidate != *current && candidate.revision <= current.revision {
                    candidate.revision = current.next_revision()?;
                    candidate.projection_fingerprint = None;
                    candidate.normalize()?;
                }
                Ok(candidate)
            }
            Self::Setup(prepared) => {
                let candidate = prepared.target_state(current)?;
                crate::sync::service::verify_activation_extension(durable, &candidate)?;
                Ok(candidate)
            }
            Self::PairApprove {
                prepared,
                network_observed_state,
            } => {
                let candidate = prepared
                    .retarget(network_observed_state, current)?
                    .candidate()
                    .state
                    .clone();
                crate::sync::service::verify_activation_extension(durable, &candidate)?;
                Ok(candidate)
            }
        }
    }
}

#[derive(Clone)]
pub(crate) enum SyncActivationCommitStage {
    Initial,
    Reconcile {
        expected_state: Box<crate::personal_state::PersonalStateV2>,
        candidate: Box<crate::personal_state::PersonalStateV2>,
    },
}

#[derive(Clone)]
pub struct SyncActivationCommit {
    pub(crate) flow_id: u64,
    pub(crate) attempt: u8,
    pub(crate) kind: SyncActivationKind,
    pub(crate) observed_local_state: crate::personal_state::PersonalStateV2,
    pub(crate) playlist_revision: u64,
    pub(crate) stage: SyncActivationCommitStage,
}

pub(crate) enum SyncActivationPersistOutcome {
    Committed(Box<crate::personal_state::PersonalStateV2>),
    Superseded,
    Failed(SyncServiceError),
}

pub struct SyncActivationPersisted {
    pub(crate) commit: Box<SyncActivationCommit>,
    pub(crate) outcome: SyncActivationPersistOutcome,
}

impl App {
    pub(in crate::app) fn start_sync_activation(
        &mut self,
        flow_id: u64,
        kind: SyncActivationKind,
    ) -> Vec<Cmd> {
        if !self.personal_state.sync_ui.is_current(flow_id) {
            return Vec::new();
        }
        if self.personal_state.sync.in_progress {
            self.finish_sync_activation_error(flow_id, SyncServiceError::LocalStateChanged);
            return Vec::new();
        }
        let current = match self.reconcile_personal_state(&self.playlists) {
            Ok(state) => state,
            Err(_) => {
                self.finish_sync_activation_error(flow_id, SyncServiceError::Storage);
                return Vec::new();
            }
        };
        if let Err(error) = kind.target_state_for(&current) {
            self.finish_sync_activation_error(flow_id, error);
            return Vec::new();
        }
        self.personal_state.sync.in_progress = true;
        self.dirty = true;
        self.schedule_sync_activation_commit(Box::new(SyncActivationCommit {
            flow_id,
            attempt: 1,
            kind,
            observed_local_state: current,
            playlist_revision: self.playlists.revision(),
            stage: SyncActivationCommitStage::Initial,
        }))
    }

    pub(in crate::app) fn finish_sync_activation_persistence(
        &mut self,
        persisted: SyncActivationPersisted,
    ) -> Vec<Cmd> {
        let SyncActivationPersisted {
            mut commit,
            outcome,
        } = persisted;
        if !self.personal_state.sync_ui.is_current(commit.flow_id)
            || !self.personal_state.sync.in_progress
        {
            return Vec::new();
        }
        match outcome {
            SyncActivationPersistOutcome::Failed(error) => {
                self.finish_sync_activation_error(commit.flow_id, error);
                Vec::new()
            }
            SyncActivationPersistOutcome::Superseded => {
                if matches!(commit.stage, SyncActivationCommitStage::Initial) {
                    return self.retry_sync_activation(commit, true);
                }
                let current = match self.reconcile_personal_state(&self.playlists) {
                    Ok(state) => state,
                    Err(_) => {
                        self.finish_sync_activation_error(
                            commit.flow_id,
                            SyncServiceError::Storage,
                        );
                        return Vec::new();
                    }
                };
                let SyncActivationCommitStage::Reconcile { expected_state, .. } = &commit.stage
                else {
                    unreachable!("initial activation was handled above");
                };
                let candidate = match commit
                    .kind
                    .target_state_after_commit(expected_state, &current)
                {
                    Ok(candidate) => candidate,
                    Err(error) => {
                        self.finish_sync_activation_error(commit.flow_id, error);
                        return Vec::new();
                    }
                };
                commit.observed_local_state = current;
                commit.playlist_revision = self.playlists.revision();
                commit.stage = SyncActivationCommitStage::Reconcile {
                    expected_state: expected_state.clone(),
                    candidate: Box::new(candidate),
                };
                self.retry_sync_activation(commit, false)
            }
            SyncActivationPersistOutcome::Committed(durable_state) => {
                let current = match self.reconcile_personal_state(&self.playlists) {
                    Ok(state) => state,
                    Err(_) => {
                        self.finish_sync_activation_error(
                            commit.flow_id,
                            SyncServiceError::Storage,
                        );
                        return Vec::new();
                    }
                };
                if current != commit.observed_local_state {
                    let candidate = match commit
                        .kind
                        .target_state_after_commit(&durable_state, &current)
                    {
                        Ok(candidate) => candidate,
                        Err(error) => {
                            self.finish_sync_activation_error(commit.flow_id, error);
                            return Vec::new();
                        }
                    };
                    commit.observed_local_state = current;
                    commit.playlist_revision = self.playlists.revision();
                    commit.stage = SyncActivationCommitStage::Reconcile {
                        expected_state: durable_state,
                        candidate: Box::new(candidate),
                    };
                    return self.retry_sync_activation(commit, false);
                }
                let device_id = match commit.kind.device_id() {
                    Ok(device_id) => device_id,
                    Err(error) => {
                        self.finish_sync_activation_error(commit.flow_id, error);
                        return Vec::new();
                    }
                };
                if let Err(error) = self.install_personal_sync_runtime(*durable_state) {
                    self.finish_sync_activation_error(commit.flow_id, error);
                    return Vec::new();
                }
                if let Some(device_id) = device_id {
                    self.personal_state.device_id = Some(device_id);
                }
                self.personal_state.sync.in_progress = false;
                self.clear_sync_activation_shutdown_context();
                self.personal_state.sync_ui.busy = None;
                self.queue_sync_ui_refresh();
                self.personal_state
                    .sync_ui
                    .finish_activation_success(&commit.kind);
                self.dirty = true;
                if self.personal_state.device_id.is_some() {
                    self.enable_automatic_sync()
                } else {
                    Vec::new()
                }
            }
        }
    }

    fn retry_sync_activation(
        &mut self,
        mut commit: Box<SyncActivationCommit>,
        count_initial_attempt: bool,
    ) -> Vec<Cmd> {
        if activation_retry_limit_reached(&commit.stage, commit.attempt, count_initial_attempt) {
            self.finish_sync_activation_error(commit.flow_id, SyncServiceError::LocalStateChanged);
            return Vec::new();
        }
        if count_initial_attempt && matches!(commit.stage, SyncActivationCommitStage::Initial) {
            commit.attempt = commit.attempt.saturating_add(1);
        }
        if matches!(commit.stage, SyncActivationCommitStage::Initial) {
            let current = match self.reconcile_personal_state(&self.playlists) {
                Ok(state) => state,
                Err(_) => {
                    self.finish_sync_activation_error(commit.flow_id, SyncServiceError::Storage);
                    return Vec::new();
                }
            };
            commit.observed_local_state = current;
            commit.playlist_revision = self.playlists.revision();
        }
        self.schedule_sync_activation_commit(commit)
    }

    fn schedule_sync_activation_commit(&mut self, commit: Box<SyncActivationCommit>) -> Vec<Cmd> {
        self.retain_sync_activation_shutdown_context(&commit);
        vec![Cmd::Persist(PersistCmd::SyncActivationCommit(commit))]
    }

    fn finish_sync_activation_error(&mut self, flow_id: u64, error: SyncServiceError) {
        self.personal_state.sync.in_progress = false;
        if self.personal_state.sync_ui.is_current(flow_id) {
            self.personal_state.sync_ui.busy = None;
            self.queue_sync_ui_refresh();
            self.personal_state.sync_ui.wizard = Some(SyncWizard::Result {
                success: false,
                message: localized_sync_error(error),
            });
            self.dirty = true;
        }
    }
}

impl SyncActivationCommit {
    pub(in crate::app) fn shutdown_persistence(
        &self,
        current: crate::personal_state::PersonalStateV2,
        playlist_revision: u64,
        personal_paths: crate::personal_state::PersonalStatePaths,
        sync_paths: crate::sync::SyncPaths,
    ) -> Result<crate::sync::service::PersonalSyncPersistence, SyncServiceError> {
        let (durable_state, possible_reconcile_state) = match &self.stage {
            SyncActivationCommitStage::Initial => (
                self.kind.target_state_for(&self.observed_local_state)?,
                None,
            ),
            SyncActivationCommitStage::Reconcile {
                expected_state,
                candidate,
            } => (
                expected_state.as_ref().clone(),
                Some(candidate.as_ref().clone()),
            ),
        };
        match &self.kind {
            SyncActivationKind::Setup(prepared) => {
                crate::sync::service::PersonalSyncPersistence::setup_activation_shutdown(
                    current,
                    playlist_revision,
                    durable_state,
                    possible_reconcile_state,
                    prepared.clone(),
                    personal_paths,
                    sync_paths,
                )
            }
            SyncActivationKind::PairJoin(prepared) => {
                crate::sync::service::PersonalSyncPersistence::pairing_join_activation_shutdown(
                    current,
                    playlist_revision,
                    durable_state,
                    possible_reconcile_state,
                    prepared.clone(),
                    personal_paths,
                    sync_paths,
                )
            }
            SyncActivationKind::PairApprove {
                prepared,
                network_observed_state,
            } => {
                crate::sync::service::PersonalSyncPersistence::pairing_approval_activation_shutdown(
                    network_observed_state.clone(),
                    current,
                    playlist_revision,
                    durable_state,
                    possible_reconcile_state,
                    prepared.clone(),
                    personal_paths,
                    sync_paths,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_reconciliation_never_uses_the_initial_retry_limit() {
        let state =
            crate::personal_state::PersonalStateV2::empty("activation-retry".to_owned()).unwrap();
        let stage = SyncActivationCommitStage::Reconcile {
            expected_state: Box::new(state.clone()),
            candidate: Box::new(state),
        };

        assert!(!activation_retry_limit_reached(&stage, u8::MAX, true));
        assert!(activation_retry_limit_reached(
            &SyncActivationCommitStage::Initial,
            MAX_ACTIVATION_RETRIES,
            true
        ));
    }
}
