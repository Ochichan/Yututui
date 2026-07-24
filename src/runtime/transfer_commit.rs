//! Targeted persistence coordination for transfer-owned playlist patches.

use std::time::Duration;

use super::task_set::RuntimeTaskEmitter;
use super::{RuntimeEvent, RuntimeHandles};
use crate::app::{Msg, TransferPlaylistCommit};
use crate::persist::Snapshot;
use crate::transfer::local_playlist::LocalPlaylistStoreError;

const TARGET_FLUSH_BUDGET: Duration = Duration::from_secs(5);
const RESTORE_RETRY_BASE: Duration = Duration::from_millis(100);
const RESTORE_RETRY_MAX: Duration = Duration::from_secs(5);

impl RuntimeHandles {
    pub(super) fn dispatch_transfer_playlist_commit(
        &mut self,
        app: &crate::app::App,
        commit: Box<TransferPlaylistCommit>,
    ) {
        let prepared = crate::personal_state::reconcile_runtime(
            &app.personal_state,
            &app.library,
            &commit.candidate,
            &app.signals,
            &app.station,
        )
        .and_then(|state| {
            crate::personal_state::PersonalStateCommit::prepare_for_runtime(
                state,
                commit.candidate.revision(),
            )
        });
        let prepared = match prepared {
            Ok(prepared) => Box::new(prepared),
            Err(error) => {
                commit
                    .request
                    .respond(Err(LocalPlaylistStoreError::resumable(format!(
                        "playlist personal-state preparation failed: {error}"
                    ))));
                return;
            }
        };
        let persist = self.persist.clone();
        let emitter = self.background_tasks.emitter(self.worker_tx.clone());
        let rejected_request = commit.request.clone();
        let admitted = self.background_tasks.spawn_cancellable(
            "transfer_playlist_commit",
            persist_transfer_playlist_commit(persist, emitter, commit, prepared),
        );
        if !admitted {
            rejected_request.respond(Err(LocalPlaylistStoreError::resumable(
                "playlist owner is shutting down before targeted confirmation",
            )));
        }
    }
}

async fn persist_transfer_playlist_commit(
    persist: crate::persist::PersistHandle,
    emitter: RuntimeTaskEmitter,
    commit: Box<TransferPlaylistCommit>,
    prepared: Box<crate::personal_state::PersonalStateCommit>,
) {
    let restore_attempt = match &commit.kind {
        crate::app::TransferPlaylistCommitKind::RestoreThenFail { retry_attempt, .. } => {
            Some(*retry_attempt)
        }
        crate::app::TransferPlaylistCommitKind::Apply { .. } => None,
    };
    if let Some(attempt) = restore_attempt {
        tokio::time::sleep(restore_retry_delay(attempt)).await;
    }
    let restore = restore_attempt.is_some();
    let personal_state = prepared.state().clone();
    let target = loop {
        match persist.save_tracked(Snapshot::PersonalState(prepared.clone())) {
            Ok(target) => break target,
            Err(error) if restore => {
                // A prior candidate may already be durable. Never release the checkpoint waiter
                // while its compensating owner snapshot has no accepted order; shutdown aborts
                // this task only before the final owner snapshot barrier.
                tracing::error!(%error, "transfer playlist restore admission blocked");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(error) => {
                commit
                    .request
                    .respond(Err(LocalPlaylistStoreError::resumable(format!(
                        "playlist persistence admission failed: {error}"
                    ))));
                return;
            }
        }
    };
    let persistence = persist.flush_target(target, TARGET_FLUSH_BUDGET).await;
    emitter
        .emit_terminal(RuntimeEvent::App(Msg::Data(
            crate::app::DataMsg::TransferPlaylistPersisted(Box::new(
                crate::app::TransferPlaylistPersistence {
                    commit,
                    persistence,
                    personal_state,
                },
            )),
        )))
        .await;
}

fn restore_retry_delay(attempt: u8) -> Duration {
    let multiplier = 1_u32
        .checked_shl(u32::from(attempt).min(16))
        .unwrap_or(u32::MAX);
    RESTORE_RETRY_BASE
        .checked_mul(multiplier)
        .unwrap_or(RESTORE_RETRY_MAX)
        .min(RESTORE_RETRY_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(
        playlists: &crate::playlists::Playlists,
    ) -> Box<crate::personal_state::PersonalStateCommit> {
        let state = crate::personal_state::legacy_state(
            &crate::library::Library::default(),
            playlists,
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        Box::new(
            crate::personal_state::PersonalStateCommit::prepare_for_runtime(
                state,
                playlists.revision(),
            )
            .unwrap(),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persistence_admission_starts_inside_the_spawned_task() {
        let persist = crate::persist::PersistHandle::detached_for_runtime_test();
        let (runtime_tx, _runtime_rx) =
            crate::runtime::channel(crate::util::backpressure::OWNER_EVENT_QUEUE);
        let mut tasks = crate::runtime::task_set::RuntimeTaskSet::new();
        let emitter = tasks.emitter(runtime_tx);
        let (request, _reply) = crate::transfer::actor::LocalPlaylistRequest::for_test(
            1,
            crate::transfer::local_playlist::LocalPlaylistOwnerRequest::Snapshot,
        );
        let patch = crate::transfer::local_playlist::LocalPlaylistPatch {
            observed_revision: 0,
            destination_id: None,
            destination_name: "Transfer".to_owned(),
            rows: Vec::new(),
        };
        let commit = Box::new(TransferPlaylistCommit {
            request,
            owner_base_revision: 0,
            candidate: crate::playlists::Playlists::default(),
            kind: crate::app::TransferPlaylistCommitKind::Apply {
                patch,
                outcome: crate::transfer::local_playlist::LocalPlaylistWriteOutcome {
                    destination_id: "transfer".to_owned(),
                    rows: Vec::new(),
                    rebased: false,
                },
            },
        });
        let prepared = prepared(&commit.candidate);

        assert!(!persist.has_pending_for_test(crate::persist::StoreKind::PersonalState));
        assert!(tasks.spawn_cancellable(
            "transfer_playlist_commit_test",
            persist_transfer_playlist_commit(persist.clone(), emitter, commit, prepared),
        ));
        assert!(
            !persist.has_pending_for_test(crate::persist::StoreKind::PersonalState),
            "constructing and admitting the task must not synchronously queue persistence"
        );

        tokio::task::yield_now().await;
        assert!(
            persist.has_pending_for_test(crate::persist::StoreKind::PersonalState),
            "the first task poll owns persistence admission"
        );
        let _ = tasks.shutdown(Duration::from_secs(1)).await;
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn restore_retry_waits_with_bounded_backoff_before_new_admission() {
        let persist = crate::persist::PersistHandle::detached_for_runtime_test();
        let (runtime_tx, _runtime_rx) =
            crate::runtime::channel(crate::util::backpressure::OWNER_EVENT_QUEUE);
        let mut tasks = crate::runtime::task_set::RuntimeTaskSet::new();
        let emitter = tasks.emitter(runtime_tx);
        let (request, _reply) = crate::transfer::actor::LocalPlaylistRequest::for_test(
            2,
            crate::transfer::local_playlist::LocalPlaylistOwnerRequest::Snapshot,
        );
        let commit = Box::new(TransferPlaylistCommit {
            request,
            owner_base_revision: 0,
            candidate: crate::playlists::Playlists::default(),
            kind: crate::app::TransferPlaylistCommitKind::RestoreThenFail {
                error: LocalPlaylistStoreError::resumable("test restore"),
                retry_attempt: 3,
            },
        });
        let prepared = prepared(&commit.candidate);

        assert!(tasks.spawn_cancellable(
            "transfer_playlist_restore_backoff_test",
            persist_transfer_playlist_commit(persist.clone(), emitter, commit, prepared),
        ));
        tokio::task::yield_now().await;
        assert!(!persist.has_pending_for_test(crate::persist::StoreKind::PersonalState));

        tokio::time::advance(Duration::from_millis(799)).await;
        tokio::task::yield_now().await;
        assert!(!persist.has_pending_for_test(crate::persist::StoreKind::PersonalState));

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(persist.has_pending_for_test(crate::persist::StoreKind::PersonalState));
        assert_eq!(restore_retry_delay(u8::MAX), RESTORE_RETRY_MAX);
        let _ = tasks.shutdown(Duration::from_secs(1)).await;
    }
}
