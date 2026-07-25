//! Exact persistence confirmation for setup and pairing activation.

use std::time::Duration;

use super::task_set::RuntimeTaskEmitter;
use super::{RuntimeEvent, RuntimeHandles};
use crate::app::{
    Msg, SyncActivationCommit, SyncActivationCommitStage, SyncActivationPersistOutcome,
    SyncActivationPersisted,
};
use crate::persist::{Snapshot, TargetFlushOutcome};
use crate::sync::service::{PersonalSyncPersistence, SyncServiceError};

const TARGET_FLUSH_BUDGET: Duration = Duration::from_secs(5);
const UNCONFIRMED_RETRY_DELAY: Duration = Duration::from_millis(100);

impl RuntimeHandles {
    pub(super) fn dispatch_sync_activation_commit(
        &mut self,
        app: &mut crate::app::App,
        commit: Box<SyncActivationCommit>,
    ) {
        let persist = self.persist.clone();
        let emitter = self.background_tasks.emitter(self.worker_tx.clone());
        let rejected = commit.clone();
        let admitted = self.background_tasks.spawn_cancellable(
            "sync_activation_commit",
            persist_sync_activation(persist, emitter, commit),
        );
        if !admitted {
            self.reduce_owner_msg(
                app,
                Msg::Data(crate::app::DataMsg::SyncActivationPersisted(Box::new(
                    SyncActivationPersisted {
                        commit: rejected,
                        outcome: SyncActivationPersistOutcome::Failed(SyncServiceError::Storage),
                    },
                ))),
            );
        }
    }
}

async fn persist_sync_activation(
    persist: crate::persist::PersistHandle,
    emitter: RuntimeTaskEmitter,
    commit: Box<SyncActivationCommit>,
) {
    let writer = match prepare_writer(&commit) {
        Ok(writer) => writer,
        Err(error) => {
            emit_completion(
                &emitter,
                commit,
                SyncActivationPersistOutcome::Failed(error),
            )
            .await;
            return;
        }
    };
    let target_state = writer.state().clone();
    let target = match persist.save_tracked(Snapshot::PersonalSync(writer.clone())) {
        Ok(target) => target,
        Err(_) => {
            emit_completion(
                &emitter,
                commit,
                SyncActivationPersistOutcome::Failed(SyncServiceError::Storage),
            )
            .await;
            return;
        }
    };

    let outcome = loop {
        let flush = persist.flush_target(target, TARGET_FLUSH_BUDGET).await;
        if writer.committed() {
            break SyncActivationPersistOutcome::Committed(Box::new(target_state));
        }
        match flush {
            TargetFlushOutcome::Superseded => {
                break SyncActivationPersistOutcome::Superseded;
            }
            TargetFlushOutcome::CommittedExact => {
                break SyncActivationPersistOutcome::Failed(SyncServiceError::Storage);
            }
            TargetFlushOutcome::Unconfirmed => {
                tokio::time::sleep(UNCONFIRMED_RETRY_DELAY).await;
            }
        }
    };
    emit_completion(&emitter, commit, outcome).await;
}

fn prepare_writer(
    commit: &SyncActivationCommit,
) -> Result<PersonalSyncPersistence, SyncServiceError> {
    let personal_paths =
        crate::personal_state::PersonalStatePaths::current().map_err(SyncServiceError::from)?;
    let sync_paths = crate::sync::SyncPaths::current().map_err(SyncServiceError::from)?;
    match &commit.stage {
        SyncActivationCommitStage::Reconcile {
            expected_state,
            candidate,
        } => PersonalSyncPersistence::reconcile(
            expected_state.as_ref().clone(),
            commit.observed_local_state.clone(),
            candidate.as_ref().clone(),
            commit.playlist_revision,
            personal_paths,
            sync_paths,
        ),
        SyncActivationCommitStage::Initial => match &commit.kind {
            crate::app::SyncActivationKind::Setup(prepared) => {
                PersonalSyncPersistence::setup_activation(
                    commit.observed_local_state.clone(),
                    commit.playlist_revision,
                    prepared.clone(),
                    personal_paths,
                    sync_paths,
                )
            }
            crate::app::SyncActivationKind::PairJoin(prepared) => {
                PersonalSyncPersistence::pairing_join_activation(
                    commit.observed_local_state.clone(),
                    commit.playlist_revision,
                    prepared.clone(),
                    personal_paths,
                    sync_paths,
                )
            }
            crate::app::SyncActivationKind::PairApprove {
                prepared,
                network_observed_state,
            } => PersonalSyncPersistence::pairing_approval_activation(
                network_observed_state.clone(),
                commit.observed_local_state.clone(),
                commit.playlist_revision,
                prepared.clone(),
                personal_paths,
                sync_paths,
            ),
        },
    }
}

async fn emit_completion(
    emitter: &RuntimeTaskEmitter,
    commit: Box<SyncActivationCommit>,
    outcome: SyncActivationPersistOutcome,
) {
    emitter
        .emit_terminal(RuntimeEvent::App(Msg::Data(
            crate::app::DataMsg::SyncActivationPersisted(Box::new(SyncActivationPersisted {
                commit,
                outcome,
            })),
        )))
        .await;
}
