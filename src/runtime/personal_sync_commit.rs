//! Exact persistence confirmation for detached personal-sync candidates.

use std::time::Duration;

use super::task_set::RuntimeTaskEmitter;
use super::{RuntimeEvent, RuntimeHandles};
use crate::app::{
    Msg, PersonalSyncCommit, PersonalSyncCommitStage, PersonalSyncPersistOutcome,
    PersonalSyncPersisted,
};
use crate::persist::{Snapshot, TargetFlushOutcome};
use crate::sync::service::{PersonalSyncApplyKind, PersonalSyncPersistence, SyncServiceError};

const TARGET_FLUSH_BUDGET: Duration = Duration::from_secs(5);
const UNCONFIRMED_RETRY_DELAY: Duration = Duration::from_millis(100);

impl RuntimeHandles {
    pub(super) fn dispatch_personal_sync_commit(
        &mut self,
        app: &mut crate::app::App,
        commit: Box<PersonalSyncCommit>,
    ) {
        let persist = self.persist.clone();
        let emitter = self.background_tasks.emitter(self.worker_tx.clone());
        let rejected = commit.clone();
        let admitted = self.background_tasks.spawn_cancellable(
            "personal_sync_commit",
            persist_personal_sync_commit(persist, emitter, commit),
        );
        if !admitted {
            self.reduce_owner_msg(
                app,
                Msg::Data(crate::app::DataMsg::PersonalSyncPersisted(Box::new(
                    PersonalSyncPersisted {
                        commit: rejected,
                        outcome: PersonalSyncPersistOutcome::Failed(SyncServiceError::Storage),
                    },
                ))),
            );
        }
    }
}

async fn persist_personal_sync_commit(
    persist: crate::persist::PersistHandle,
    emitter: RuntimeTaskEmitter,
    commit: Box<PersonalSyncCommit>,
) {
    let writer = match prepare_writer(&commit) {
        Ok(writer) => writer,
        Err(error) => {
            emit_completion(&emitter, commit, PersonalSyncPersistOutcome::Failed(error)).await;
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
                PersonalSyncPersistOutcome::Failed(SyncServiceError::Storage),
            )
            .await;
            return;
        }
    };

    let outcome = match await_target(&persist, target, || writer.committed()).await {
        TargetResolution::Committed => {
            PersonalSyncPersistOutcome::Committed(Box::new(target_state))
        }
        TargetResolution::Superseded => PersonalSyncPersistOutcome::Superseded,
        TargetResolution::InvalidExact => {
            PersonalSyncPersistOutcome::Failed(SyncServiceError::Storage)
        }
    };
    emit_completion(&emitter, commit, outcome).await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetResolution {
    Committed,
    Superseded,
    InvalidExact,
}

async fn await_target(
    persist: &crate::persist::PersistHandle,
    target: crate::persist::PersistTarget,
    committed: impl Fn() -> bool,
) -> TargetResolution {
    loop {
        let flush = persist.flush_target(target, TARGET_FLUSH_BUDGET).await;
        if committed() {
            return TargetResolution::Committed;
        }
        match flush {
            TargetFlushOutcome::Superseded => return TargetResolution::Superseded,
            // The special writer sets its mark only after the full cross-store transaction.
            TargetFlushOutcome::CommittedExact => return TargetResolution::InvalidExact,
            TargetFlushOutcome::Unconfirmed => {
                tokio::time::sleep(UNCONFIRMED_RETRY_DELAY).await;
            }
        }
    }
}

fn prepare_writer(
    commit: &PersonalSyncCommit,
) -> Result<PersonalSyncPersistence, SyncServiceError> {
    let personal_paths =
        crate::personal_state::PersonalStatePaths::current().map_err(SyncServiceError::from)?;
    let sync_paths = crate::sync::SyncPaths::current().map_err(SyncServiceError::from)?;
    match &commit.stage {
        PersonalSyncCommitStage::Initial {
            current_state,
            candidate,
        } => PersonalSyncPersistence::initial(
            current_state.clone(),
            commit.playlist_revision,
            candidate.as_ref().clone(),
            match &commit.action {
                crate::app::PersonalSyncAction::SyncNow => PersonalSyncApplyKind::SyncNow,
                crate::app::PersonalSyncAction::Revoke(device_id) => {
                    PersonalSyncApplyKind::Revoke(device_id.clone())
                }
            },
            personal_paths,
            sync_paths,
        ),
        PersonalSyncCommitStage::Reconcile {
            expected_state,
            candidate,
        } => PersonalSyncPersistence::reconcile(
            expected_state.clone(),
            commit.observed_local_state.clone(),
            candidate.as_ref().clone(),
            commit.playlist_revision,
            personal_paths,
            sync_paths,
        ),
    }
}

async fn emit_completion(
    emitter: &RuntimeTaskEmitter,
    commit: Box<PersonalSyncCommit>,
    outcome: PersonalSyncPersistOutcome,
) {
    emitter
        .emit_terminal(RuntimeEvent::App(Msg::Data(
            crate::app::DataMsg::PersonalSyncPersisted(Box::new(PersonalSyncPersisted {
                commit,
                outcome,
            })),
        )))
        .await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    fn test_snapshot(label: &'static str, committed: Arc<AtomicBool>) -> crate::persist::Snapshot {
        crate::persist::Snapshot::Test {
            kind: crate::persist::StoreKind::PersonalState,
            label,
            storage_path: None,
            writer: Arc::new(move || {
                committed.store(true, Ordering::Release);
                Ok(())
            }),
        }
    }

    #[tokio::test]
    async fn exact_special_generation_is_confirmed() {
        let persist = crate::persist::spawn();
        let committed = Arc::new(AtomicBool::new(false));
        let target = persist
            .save_tracked(test_snapshot("sync exact", Arc::clone(&committed)))
            .unwrap();

        assert_eq!(
            await_target(&persist, target, || committed.load(Ordering::Acquire)).await,
            TargetResolution::Committed
        );
    }

    #[tokio::test]
    async fn newer_local_generation_supersedes_unwritten_sync_candidate() {
        let persist = crate::persist::spawn();
        let older_committed = Arc::new(AtomicBool::new(false));
        let target = persist
            .save_tracked(test_snapshot(
                "sync candidate",
                Arc::clone(&older_committed),
            ))
            .unwrap();
        let newer_committed = Arc::new(AtomicBool::new(false));
        let _ = persist
            .save(test_snapshot(
                "local mutation",
                Arc::clone(&newer_committed),
            ))
            .unwrap();

        assert_eq!(
            await_target(&persist, target, || older_committed.load(Ordering::Acquire)).await,
            TargetResolution::Superseded
        );
        assert!(!older_committed.load(Ordering::Acquire));
        assert!(newer_committed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn committed_sync_wins_outcome_even_when_newer_local_write_finishes() {
        let persist = crate::persist::spawn();
        let sync_committed = Arc::new(AtomicBool::new(false));
        let target = persist
            .save_tracked(test_snapshot("committed sync", Arc::clone(&sync_committed)))
            .unwrap();
        assert_eq!(
            persist.flush_target(target, TARGET_FLUSH_BUDGET).await,
            TargetFlushOutcome::CommittedExact
        );
        let local_committed = Arc::new(AtomicBool::new(false));
        let _ = persist
            .save(test_snapshot("newer local", Arc::clone(&local_committed)))
            .unwrap();

        assert_eq!(
            await_target(&persist, target, || sync_committed.load(Ordering::Acquire)).await,
            TargetResolution::Committed
        );
    }

    #[tokio::test]
    async fn sealed_actor_rejects_sync_admission() {
        let persist = crate::persist::spawn();
        persist.seal_with_snapshots([]).unwrap();
        let committed = Arc::new(AtomicBool::new(false));

        assert!(
            persist
                .save_tracked(test_snapshot("rejected sync", Arc::clone(&committed)))
                .is_err()
        );
        assert!(!committed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn transient_unconfirmed_write_surfaces_failure_then_recovers() {
        let persist = crate::persist::spawn();
        let failures = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&failures);
        persist.set_event_sink(move |event| {
            captured.lock().unwrap().push(event);
        });
        let attempts = Arc::new(AtomicUsize::new(0));
        let committed = Arc::new(AtomicBool::new(false));
        let writer_attempts = Arc::clone(&attempts);
        let writer_committed = Arc::clone(&committed);
        let target = persist
            .save_tracked(crate::persist::Snapshot::Test {
                kind: crate::persist::StoreKind::PersonalState,
                label: "recovering personal sync",
                storage_path: None,
                writer: Arc::new(move || {
                    if writer_attempts.fetch_add(1, Ordering::AcqRel) == 0 {
                        return Err(std::io::Error::other("temporary storage failure"));
                    }
                    writer_committed.store(true, Ordering::Release);
                    Ok(())
                }),
            })
            .unwrap();

        let resolution = tokio::time::timeout(
            Duration::from_secs(2),
            await_target(&persist, target, || committed.load(Ordering::Acquire)),
        )
        .await
        .expect("retained persistence retry must settle");
        assert_eq!(resolution, TargetResolution::Committed);
        assert!(attempts.load(Ordering::Acquire) >= 2);
        let failures = failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        let crate::persist::PersistEvent::WriteFailed { store, error } = &failures[0];
        assert_eq!(*store, crate::persist::StoreKind::PersonalState);
        assert!(error.contains("temporary storage failure"));
    }
}
