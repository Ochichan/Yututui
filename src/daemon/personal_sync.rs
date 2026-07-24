//! Detached WebDAV preparation with primary-owner installation and bounded stale retry.

use super::DaemonEventSender;
use super::engine::DaemonEngine;
use super::engine::personal_sync::PersonalSyncAction;
use crate::player::lifetime::ShutdownLatch;
use crate::remote::{RemoteReply, proto::RemoteResponse};
use crate::sync::service::{PreparedManualSync, SyncServiceError};

const MAX_STALE_RETRIES: u8 = 3;

#[derive(Default)]
pub(super) struct PersonalSync {
    next_generation: u64,
    pending: Option<Pending>,
}

struct Pending {
    generation: u64,
    attempt: u8,
    action: PersonalSyncAction,
    reply: RemoteReply,
}

pub(super) struct Finished {
    pub(super) generation: u64,
    pub(super) result: Result<PreparedManualSync, SyncServiceError>,
}

impl PersonalSync {
    pub(super) fn start(
        &mut self,
        action: PersonalSyncAction,
        reply: RemoteReply,
        engine: &mut DaemonEngine,
        event_tx: &DaemonEventSender,
        shutdown: &ShutdownLatch,
    ) {
        if self.pending.is_some() {
            let _ = reply.send(RemoteResponse::err_with_message(
                "sync_busy",
                "personal sync is already running".to_owned(),
            ));
            return;
        }
        if crate::persist::ensure_persistence_writes_allowed().is_err() {
            let _ = reply.send(persistence_unavailable());
            return;
        }
        let (state, paths) = match engine.personal_sync_source() {
            Ok(source) => source,
            Err(error) => {
                let _ = reply.send(error_response(error));
                return;
            }
        };
        let generation = self.next_generation();
        self.pending = Some(Pending {
            generation,
            attempt: 1,
            action: action.clone(),
            reply,
        });
        engine.set_personal_sync_in_progress(true);
        if !schedule_prepare(event_tx, shutdown, generation, action, state, paths) {
            engine.set_personal_sync_in_progress(false);
            self.settle_generation(generation, RemoteResponse::err("shutting_down"));
        }
    }

    pub(super) fn start_command(
        &mut self,
        command: crate::remote::proto::RemoteCommand,
        reply: RemoteReply,
        engine: &mut DaemonEngine,
        event_tx: &DaemonEventSender,
        shutdown: &ShutdownLatch,
    ) {
        let action = match command {
            crate::remote::proto::RemoteCommand::SyncNow => PersonalSyncAction::SyncNow,
            crate::remote::proto::RemoteCommand::SyncRevokeDevice { device_id } => {
                let Ok(device_id) = crate::personal_state::DeviceId::new(device_id) else {
                    let _ = reply.send(RemoteResponse::err("bad_device_id"));
                    return;
                };
                PersonalSyncAction::Revoke(device_id)
            }
            _ => unreachable!("only personal-sync commands reach this owner host"),
        };
        self.start(action, reply, engine, event_tx, shutdown);
    }

    pub(super) fn finish(
        &mut self,
        finished: Finished,
        engine: &mut DaemonEngine,
        event_tx: &DaemonEventSender,
        shutdown: &ShutdownLatch,
    ) {
        let Some(pending) = self
            .pending
            .as_ref()
            .filter(|pending| pending.generation == finished.generation)
        else {
            tracing::debug!(
                generation = finished.generation,
                "retired stale personal-sync completion"
            );
            return;
        };
        let action = pending.action.clone();
        let response = match finished.result {
            Ok(candidate) => {
                match engine.personal_sync_source_is_current(candidate.expected_local_revision) {
                    Ok(false) => return self.retry_stale(engine, event_tx, shutdown),
                    Err(error) => error_response(error),
                    Ok(true) => {
                        match engine.apply_personal_sync_candidate(action.clone(), candidate) {
                            Ok(applied) => {
                                RemoteResponse::ok(sync_summary(&action, &applied.summary))
                            }
                            Err(SyncServiceError::LocalStateChanged) => {
                                return self.retry_stale(engine, event_tx, shutdown);
                            }
                            Err(error) => error_response(error),
                        }
                    }
                }
            }
            Err(error) => error_response(error),
        };
        engine.set_personal_sync_in_progress(false);
        self.settle_generation(finished.generation, response);
    }

    pub(super) fn shutdown(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let _ = pending.reply.send(RemoteResponse::err(
            crate::remote::proto::CONFIRMATION_LOST_REASON,
        ));
    }

    fn retry_stale(
        &mut self,
        engine: &mut DaemonEngine,
        event_tx: &DaemonEventSender,
        shutdown: &ShutdownLatch,
    ) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        if pending.attempt >= MAX_STALE_RETRIES {
            let generation = pending.generation;
            engine.set_personal_sync_in_progress(false);
            self.settle_generation(
                generation,
                error_response(SyncServiceError::LocalStateChanged),
            );
            return;
        }
        if crate::persist::ensure_persistence_writes_allowed().is_err() {
            let generation = pending.generation;
            engine.set_personal_sync_in_progress(false);
            self.settle_generation(generation, persistence_unavailable());
            return;
        }
        let (state, paths) = match engine.personal_sync_source() {
            Ok(source) => source,
            Err(error) => {
                let generation = pending.generation;
                engine.set_personal_sync_in_progress(false);
                self.settle_generation(generation, error_response(error));
                return;
            }
        };
        let action = pending.action.clone();
        let attempt = pending.attempt.saturating_add(1);
        let generation = self.next_generation();
        if let Some(pending) = self.pending.as_mut() {
            pending.generation = generation;
            pending.attempt = attempt;
        }
        if !schedule_prepare(event_tx, shutdown, generation, action, state, paths) {
            engine.set_personal_sync_in_progress(false);
            self.settle_generation(generation, RemoteResponse::err("shutting_down"));
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == 0 {
            self.next_generation = 1;
        }
        self.next_generation
    }

    fn settle_generation(&mut self, generation: u64, response: RemoteResponse) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.generation == generation)
            && let Some(pending) = self.pending.take()
        {
            let _ = pending.reply.send(response);
        }
    }
}

fn schedule_prepare(
    event_tx: &DaemonEventSender,
    shutdown: &ShutdownLatch,
    generation: u64,
    action: PersonalSyncAction,
    state: crate::personal_state::PersonalStateV2,
    paths: crate::sync::SyncPaths,
) -> bool {
    if shutdown.is_triggered() {
        return false;
    }
    let tx = event_tx.clone();
    let completion_shutdown = shutdown.clone();
    crate::sync::spawn_detached_prepare(
        move || prepare(action, state, paths),
        move |result| {
            let _ = super::effects::deliver_terminal_blocking(
                &tx,
                &completion_shutdown,
                super::DaemonEvent::PersonalSyncFinished(Box::new(Finished { generation, result })),
            );
        },
    )
}

fn prepare(
    action: PersonalSyncAction,
    state: crate::personal_state::PersonalStateV2,
    paths: crate::sync::SyncPaths,
) -> Result<PreparedManualSync, SyncServiceError> {
    let revoke_target = match &action {
        PersonalSyncAction::SyncNow => None,
        PersonalSyncAction::Revoke(device_id) => Some(device_id),
    };
    crate::sync::service::prepare_owner_sync(&state, revoke_target, &paths)
}

fn error_response(error: SyncServiceError) -> RemoteResponse {
    RemoteResponse::err_with_message(error.reason(), error.to_string())
}

fn persistence_unavailable() -> RemoteResponse {
    RemoteResponse::err_with_message(
        "persistence_unavailable",
        "the primary persistence owner is unavailable".to_owned(),
    )
}

fn sync_summary(
    action: &PersonalSyncAction,
    summary: &crate::sync::manual::ManualSyncSummary,
) -> String {
    if matches!(action, PersonalSyncAction::Revoke(_)) {
        return "Device removed. Change the shared WebDAV password if that device knew it."
            .to_owned();
    }
    if summary.downloaded_operations == 0 && summary.uploaded_operations == 0 {
        "Personal state is up to date.".to_owned()
    } else {
        format!(
            "Personal state synced: {} sent, {} received.",
            summary.uploaded_operations, summary.downloaded_operations
        )
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    fn keyed_state() -> (
        crate::personal_state::PersonalStateV2,
        crate::personal_state::DeviceId,
    ) {
        let mut state = crate::personal_state::legacy_state(
            &crate::library::Library::default(),
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        let device_id = state
            .device_registry
            .values()
            .find(|device| device.device_id.as_str() != "legacy")
            .unwrap()
            .device_id
            .clone();
        let secrets = crate::sync::DeviceSecretMaterial::generate_for(device_id.as_str()).unwrap();
        state = crate::personal_state::append_operation_as(
            &state,
            &device_id,
            crate::personal_state::Operation::AddDevice {
                device: crate::personal_state::DeviceRecord {
                    device_id: device_id.clone(),
                    name: "Daemon".to_owned(),
                    revoked: false,
                    public_identity: Some(secrets.public_identity()),
                },
            },
            0,
        )
        .unwrap();
        (state, device_id)
    }

    fn event_channel() -> (
        DaemonEventSender,
        tokio::sync::mpsc::Receiver<super::super::DaemonEvent>,
    ) {
        let (raw, rx) = crate::util::backpressure::bounded_channel(
            crate::util::backpressure::DAEMON_EVENT_QUEUE,
        );
        (DaemonEventSender::new(raw), rx)
    }

    #[tokio::test]
    async fn duplicate_request_is_rejected_without_replacing_owner_reply() {
        let (events, _rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        let (first_reply, mut first_response) = oneshot::channel();
        let mut host = PersonalSync {
            next_generation: 1,
            pending: Some(Pending {
                generation: 1,
                attempt: 1,
                action: PersonalSyncAction::SyncNow,
                reply: first_reply.into(),
            }),
        };
        let mut engine = super::super::engine::tests::engine_with_queue(&[]);
        let (reply, mut response) = oneshot::channel();

        host.start(
            PersonalSyncAction::SyncNow,
            reply.into(),
            &mut engine,
            &events,
            &shutdown,
        );

        assert_eq!(
            response.try_recv().unwrap().reason.as_deref(),
            Some("sync_busy")
        );
        assert!(first_response.try_recv().is_err());
        host.shutdown();
        assert_eq!(
            first_response.try_recv().unwrap().reason.as_deref(),
            Some(crate::remote::proto::CONFIRMATION_LOST_REASON)
        );
    }

    #[tokio::test]
    async fn unconfigured_owner_is_rejected_before_worker_start() {
        let (events, mut rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        let mut host = PersonalSync::default();
        let mut engine = super::super::engine::tests::engine_with_queue(&[]);
        let (reply, mut response) = oneshot::channel();

        host.start(
            PersonalSyncAction::SyncNow,
            reply.into(),
            &mut engine,
            &events,
            &shutdown,
        );

        assert_eq!(
            response.try_recv().unwrap().reason.as_deref(),
            Some("sync_not_configured")
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn in_progress_covers_detached_prepare_and_clears_on_terminal_settlement() {
        let (events, mut rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        let mut host = PersonalSync::default();
        let mut engine = super::super::engine::tests::engine_with_queue(&[]);
        let (state, device_id) = keyed_state();
        engine.configure_personal_sync_for_test(state, device_id);
        let (reply, mut response) = oneshot::channel();

        host.start(
            PersonalSyncAction::SyncNow,
            reply.into(),
            &mut engine,
            &events,
            &shutdown,
        );

        assert!(engine.personal_sync_in_progress_for_test());
        let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(super::super::DaemonEvent::PersonalSyncFinished(finished)) =
                    rx.recv().await
                {
                    break finished;
                }
            }
        })
        .await
        .expect("detached prepare must report one terminal result");
        host.finish(*finished, &mut engine, &events, &shutdown);

        assert!(!engine.personal_sync_in_progress_for_test());
        assert!(!response.try_recv().unwrap().ok);
    }

    #[tokio::test]
    async fn retired_generation_cannot_settle_the_current_owner_request() {
        let (events, _rx) = event_channel();
        let shutdown = ShutdownLatch::new();
        let (current_reply, mut current_response) = oneshot::channel();
        let mut host = PersonalSync {
            next_generation: 2,
            pending: Some(Pending {
                generation: 2,
                attempt: 2,
                action: PersonalSyncAction::SyncNow,
                reply: current_reply.into(),
            }),
        };
        let mut engine = super::super::engine::tests::engine_with_queue(&[]);

        host.finish(
            Finished {
                generation: 1,
                result: Err(SyncServiceError::Offline),
            },
            &mut engine,
            &events,
            &shutdown,
        );

        assert!(current_response.try_recv().is_err());
        assert!(
            host.pending
                .as_ref()
                .is_some_and(|pending| { pending.generation == 2 && pending.attempt == 2 })
        );
        host.shutdown();
        assert_eq!(
            current_response.try_recv().unwrap().reason.as_deref(),
            Some(crate::remote::proto::CONFIRMATION_LOST_REASON)
        );
    }
}
