//! Primary-owner completion path for manual encrypted personal-state sync.

use std::sync::Arc;

use super::*;
use crate::remote::proto::RemoteResponse;
use crate::sync::service::{SyncServiceError, rebase_local_operations};

const MAX_STALE_RETRIES: u8 = 3;

#[derive(Clone)]
pub enum PersonalSyncAction {
    SyncNow,
    Revoke(crate::personal_state::DeviceId),
}

/// A single-use remote reply shared by shutdown, the read-only gate, and worker completion.
///
/// Debug is deliberately absent so retained outcome diagnostics cannot accidentally expand to
/// transport internals.
#[derive(Clone)]
pub struct PersonalSyncReply(std::sync::Arc<std::sync::Mutex<Option<crate::remote::RemoteReply>>>);

impl PersonalSyncReply {
    pub(crate) fn new(reply: crate::remote::RemoteReply) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(Some(reply))))
    }

    pub(crate) fn respond(&self, response: crate::remote::proto::RemoteResponse) {
        let mut reply = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reply) = reply.take() {
            let _ = reply.send(response);
        }
    }
}

pub struct PersonalSyncPrepared {
    pub(crate) action: PersonalSyncAction,
    pub(crate) attempt: u8,
    pub(crate) result:
        Result<crate::sync::service::PreparedManualSync, crate::sync::service::SyncServiceError>,
    pub(crate) reply: PersonalSyncReply,
}

#[derive(Clone)]
pub(crate) enum PersonalSyncCommitStage {
    Initial {
        current_state: crate::personal_state::PersonalStateV2,
        candidate: Box<crate::sync::service::PreparedManualSync>,
    },
    Reconcile {
        expected_state: crate::personal_state::PersonalStateV2,
        candidate: Box<crate::personal_state::PersonalStateV2>,
    },
}

/// Owner context retained while the persistence actor confirms one exact sync generation.
#[derive(Clone)]
pub struct PersonalSyncCommit {
    pub(crate) action: PersonalSyncAction,
    pub(crate) attempt: u8,
    pub(crate) observed_local_state: crate::personal_state::PersonalStateV2,
    pub(crate) playlist_revision: u64,
    pub(crate) summary: crate::sync::manual::ManualSyncSummary,
    pub(crate) stage: PersonalSyncCommitStage,
    pub(crate) reply: PersonalSyncReply,
}

pub(crate) enum PersonalSyncPersistOutcome {
    Committed(Box<crate::personal_state::PersonalStateV2>),
    Superseded,
    Failed(SyncServiceError),
}

pub struct PersonalSyncPersisted {
    pub(crate) commit: Box<PersonalSyncCommit>,
    pub(crate) outcome: PersonalSyncPersistOutcome,
}

#[derive(Default)]
pub(crate) struct PersonalSyncState {
    pub(crate) in_progress: bool,
    pub(crate) pending_reply: Option<PersonalSyncReply>,
    shutdown_context: Option<PersonalSyncShutdownContext>,
}

/// Canonical personal-state ledger plus the owner-only runtime needed to synchronize it.
///
/// Keeping these values together prevents the root [`App`] state from accumulating one flat
/// field for every piece of personal-state coordination.
#[derive(Default)]
pub(crate) struct PersonalStateRuntime {
    pub(crate) ledger: crate::personal_state::PersonalStateV2,
    pub(crate) device_id: Option<crate::personal_state::DeviceId>,
    pub(crate) sync: PersonalSyncState,
}

#[derive(Clone)]
struct PersonalSyncShutdownContext {
    observed_state: crate::personal_state::PersonalStateV2,
    candidate: crate::sync::service::PreparedManualSync,
}

impl App {
    pub(in crate::app) fn start_personal_sync(
        &mut self,
        action: PersonalSyncAction,
        reply: crate::remote::RemoteReply,
    ) -> Vec<Cmd> {
        let reply = PersonalSyncReply::new(reply);
        if self.personal_state.sync.in_progress {
            reply.respond(RemoteResponse::err_with_message(
                "sync_busy",
                "personal sync is already running".to_owned(),
            ));
            return Vec::new();
        }
        if self.personal_state.device_id.is_none() {
            reply.respond(sync_error_response(SyncServiceError::NotConfigured));
            return Vec::new();
        }
        self.personal_state.sync.in_progress = true;
        self.personal_state.sync.pending_reply = Some(reply.clone());
        self.status.text = crate::t!(
            "Syncing personal state…",
            "개인 상태 동기화 중…",
            "個人状態を同期中…"
        )
        .to_owned();
        self.status.kind = StatusKind::Info;
        self.dirty = true;
        vec![Cmd::Data(DataCmd::PersonalSync {
            action,
            attempt: 1,
            personal_state: Box::new(self.personal_state.ledger.clone()),
            reply,
        })]
    }

    pub(in crate::app) fn finish_personal_sync(
        &mut self,
        prepared: PersonalSyncPrepared,
    ) -> Vec<Cmd> {
        if !self.personal_state.sync.in_progress {
            return Vec::new();
        }
        let current_state = match self.reconcile_personal_state(&self.playlists) {
            Ok(state) => state,
            Err(_) => {
                self.finish_personal_sync_error(&prepared.reply, SyncServiceError::Storage);
                return Vec::new();
            }
        };
        match prepared.result {
            Ok(candidate) if candidate.expected_local_revision != current_state.revision => self
                .retry_personal_sync(
                    prepared.action,
                    prepared.attempt,
                    current_state,
                    prepared.reply,
                ),
            Ok(candidate) => {
                let summary = candidate.summary.clone();
                self.personal_state.sync.shutdown_context = Some(PersonalSyncShutdownContext {
                    observed_state: current_state.clone(),
                    candidate: candidate.clone(),
                });
                vec![Cmd::Persist(PersistCmd::PersonalSyncCommit(Box::new(
                    PersonalSyncCommit {
                        action: prepared.action,
                        attempt: prepared.attempt,
                        observed_local_state: current_state.clone(),
                        playlist_revision: self.playlists.revision(),
                        summary,
                        stage: PersonalSyncCommitStage::Initial {
                            current_state,
                            candidate: Box::new(candidate),
                        },
                        reply: prepared.reply,
                    },
                )))]
            }
            Err(error) => {
                self.finish_personal_sync_error(&prepared.reply, error);
                Vec::new()
            }
        }
    }

    pub(in crate::app) fn finish_personal_sync_persistence(
        &mut self,
        persisted: PersonalSyncPersisted,
    ) -> Vec<Cmd> {
        if !self.personal_state.sync.in_progress {
            return Vec::new();
        }
        let PersonalSyncPersisted {
            mut commit,
            outcome,
        } = persisted;
        match outcome {
            PersonalSyncPersistOutcome::Failed(error) => {
                self.finish_personal_sync_error(&commit.reply, error);
                Vec::new()
            }
            PersonalSyncPersistOutcome::Superseded => {
                let current = match self.reconcile_personal_state(&self.playlists) {
                    Ok(state) => state,
                    Err(_) => {
                        self.finish_personal_sync_error(&commit.reply, SyncServiceError::Storage);
                        return Vec::new();
                    }
                };
                self.retry_personal_sync(commit.action, commit.attempt, current, commit.reply)
            }
            PersonalSyncPersistOutcome::Committed(durable_state) => {
                let current = match self.reconcile_personal_state(&self.playlists) {
                    Ok(state) => state,
                    Err(_) => {
                        self.finish_personal_sync_error(&commit.reply, SyncServiceError::Storage);
                        return Vec::new();
                    }
                };
                if current != commit.observed_local_state {
                    let local_device = match &self.personal_state.device_id {
                        Some(device) => device,
                        None => {
                            self.finish_personal_sync_error(
                                &commit.reply,
                                SyncServiceError::NotConfigured,
                            );
                            return Vec::new();
                        }
                    };
                    let rebased = match rebase_local_operations(
                        &durable_state,
                        &commit.observed_local_state,
                        &current,
                        local_device,
                    ) {
                        Ok(state) => state,
                        Err(error) => {
                            self.finish_personal_sync_error(&commit.reply, error);
                            return Vec::new();
                        }
                    };
                    commit.observed_local_state = current;
                    commit.playlist_revision = self.playlists.revision();
                    commit.stage = PersonalSyncCommitStage::Reconcile {
                        expected_state: *durable_state,
                        candidate: Box::new(rebased),
                    };
                    return vec![Cmd::Persist(PersistCmd::PersonalSyncCommit(commit))];
                }
                if let Err(error) = self.install_personal_sync_runtime(*durable_state) {
                    self.finish_personal_sync_error(&commit.reply, error);
                    return Vec::new();
                }
                self.personal_state.sync.in_progress = false;
                self.personal_state.sync.pending_reply = None;
                self.personal_state.sync.shutdown_context = None;
                let message = sync_summary(&commit.action, &commit.summary);
                commit.reply.respond(RemoteResponse::ok(message.clone()));
                self.status.text = message;
                self.status.kind = StatusKind::Info;
                self.dirty = true;
                Vec::new()
            }
        }
    }

    pub(crate) fn settle_personal_sync_shutdown(&mut self) {
        self.personal_state.sync.in_progress = false;
        if let Some(reply) = self.personal_state.sync.pending_reply.take() {
            reply.respond(RemoteResponse::err(
                crate::remote::proto::CONFIRMATION_LOST_REASON,
            ));
        }
    }

    /// Build the final cross-store transaction for an accepted candidate still owned at quit.
    pub(crate) fn personal_sync_shutdown_persistence(
        &self,
        personal_paths: crate::personal_state::PersonalStatePaths,
        sync_paths: crate::sync::SyncPaths,
    ) -> Result<Option<crate::sync::service::PersonalSyncPersistence>, SyncServiceError> {
        let Some(context) = &self.personal_state.sync.shutdown_context else {
            return Ok(None);
        };
        let current = self
            .reconcile_personal_state(&self.playlists)
            .map_err(SyncServiceError::from)?;
        let local_device = self
            .personal_state
            .device_id
            .as_ref()
            .ok_or(SyncServiceError::NotConfigured)?;
        crate::sync::service::PersonalSyncPersistence::shutdown(
            context.observed_state.clone(),
            current,
            self.playlists.revision(),
            context.candidate.clone(),
            local_device,
            personal_paths,
            sync_paths,
        )
        .map(Some)
    }

    fn retry_personal_sync(
        &mut self,
        action: PersonalSyncAction,
        attempt: u8,
        current_state: crate::personal_state::PersonalStateV2,
        reply: PersonalSyncReply,
    ) -> Vec<Cmd> {
        if attempt >= MAX_STALE_RETRIES {
            self.finish_personal_sync_error(&reply, SyncServiceError::LocalStateChanged);
            return Vec::new();
        }
        vec![Cmd::Data(DataCmd::PersonalSync {
            action,
            attempt: attempt.saturating_add(1),
            personal_state: Box::new(current_state),
            reply,
        })]
    }

    fn finish_personal_sync_error(&mut self, reply: &PersonalSyncReply, error: SyncServiceError) {
        self.personal_state.sync.in_progress = false;
        self.personal_state.sync.pending_reply = None;
        reply.respond(sync_error_response(error));
        self.set_status_error(error.to_string());
    }

    fn install_personal_sync_runtime(
        &mut self,
        state: crate::personal_state::PersonalStateV2,
    ) -> Result<(), SyncServiceError> {
        let prepared = crate::personal_state::PersonalStateCommit::prepare_for_runtime(
            state,
            self.playlists.revision(),
        )
        .map_err(SyncServiceError::from)?;
        let (library, mut playlists, signals, station) = prepared.runtime_stores();
        playlists.inherit_revision_from(&self.playlists);
        self.personal_state.ledger = prepared.state().clone();
        self.library = Arc::new(library);
        self.playlists = Arc::new(playlists);
        self.signals = Arc::new(signals);
        self.station = station;
        self.library_rows_cache.borrow_mut().take();
        self.all_count_cache.set(None);
        self.clamp_library_selection();
        Ok(())
    }
}

pub(crate) fn sync_error_response(error: SyncServiceError) -> RemoteResponse {
    RemoteResponse::err_with_message(error.reason(), error.to_string())
}

pub(crate) fn sync_summary(
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
                    name: "This device".to_owned(),
                    revoked: false,
                    public_identity: Some(secrets.public_identity()),
                },
            },
            0,
        )
        .unwrap();
        (state, device_id)
    }

    fn detached_candidate(
        state: crate::personal_state::PersonalStateV2,
        device_id: crate::personal_state::DeviceId,
    ) -> crate::sync::service::PreparedManualSync {
        let recovery = crate::sync::RecoveryKit::generate(state.dataset_id.clone(), None).unwrap();
        let root = crate::sync::SignedMembershipRoot::create(
            state.dataset_id.clone(),
            recovery.recovery_recipient(),
            &recovery.signing_key().unwrap(),
            state.device_registry[&device_id].clone(),
        )
        .unwrap();
        crate::sync::service::PreparedManualSync {
            expected_local_revision: state.revision,
            expected_private_revision: 1,
            local_device_id: device_id,
            state,
            membership: crate::sync::MembershipChain::new(root),
            checkpoint_anchor: crate::sync::CheckpointAnchor::default(),
            summary: crate::sync::manual::ManualSyncSummary::default(),
        }
    }

    #[test]
    fn unconfigured_owner_rejects_without_starting_network_work() {
        let mut app = App::new(50);
        let (reply, mut response) = oneshot::channel();

        let commands = app.update(Msg::Remote(
            crate::remote::proto::RemoteCommand::SyncNow,
            reply.into(),
        ));

        assert!(commands.is_empty());
        assert_eq!(
            response.try_recv().unwrap().reason.as_deref(),
            Some("sync_not_configured")
        );
        assert!(!app.personal_state.sync.in_progress);
    }

    #[test]
    fn owner_manual_sync_is_single_flight() {
        let mut app = App::new(50);
        app.personal_state.device_id =
            Some(crate::personal_state::DeviceId::new("device-a").unwrap());
        let (first_reply, _first_response) = oneshot::channel();
        let commands = app.update(Msg::Remote(
            crate::remote::proto::RemoteCommand::SyncNow,
            first_reply.into(),
        ));
        assert!(matches!(
            commands.as_slice(),
            [Cmd::Data(DataCmd::PersonalSync { attempt: 1, .. })]
        ));

        let (second_reply, mut second_response) = oneshot::channel();
        let duplicate = app.update(Msg::Remote(
            crate::remote::proto::RemoteCommand::SyncNow,
            second_reply.into(),
        ));
        assert!(duplicate.is_empty());
        assert_eq!(
            second_response.try_recv().unwrap().reason.as_deref(),
            Some("sync_busy")
        );
    }

    #[test]
    fn shutdown_settles_pending_sync_as_confirmation_lost() {
        let mut app = App::new(50);
        app.personal_state.device_id =
            Some(crate::personal_state::DeviceId::new("device-a").unwrap());
        let (reply, mut response) = oneshot::channel();
        let commands = app.update(Msg::Remote(
            crate::remote::proto::RemoteCommand::SyncNow,
            reply.into(),
        ));
        assert_eq!(commands.len(), 1);

        app.settle_personal_sync_shutdown();

        assert_eq!(
            response.try_recv().unwrap().reason.as_deref(),
            Some(crate::remote::proto::CONFIRMATION_LOST_REASON)
        );
        assert!(!app.personal_state.sync.in_progress);
        assert!(app.personal_state.sync.pending_reply.is_none());
    }

    #[test]
    fn runtime_install_invalidates_library_filter_and_count_caches() {
        let mut app = App::new(50);
        let matching = crate::api::Song::remote("old-track", "matches filter", "Artist", "3:00");
        app.library_mut().favorites.push(matching);
        app.library_ui.filter_query = "matches".to_owned();
        assert_eq!(app.library_rows_len(), 1);
        assert!(app.library_rows_cache.borrow().is_some());
        assert_eq!(app.library_count_for(LibraryTab::All), 1);
        assert!(app.all_count_cache.get().is_some());

        let mut replacement = crate::library::Library::default();
        replacement.favorites.push(crate::api::Song::remote(
            "new-track",
            "different title",
            "Artist",
            "3:00",
        ));
        let state = crate::personal_state::legacy_state(
            &replacement,
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        app.install_personal_sync_runtime(state).unwrap();

        assert!(app.all_count_cache.get().is_none());
        assert_eq!(app.library_rows_len(), 0);
        assert_eq!(app.library_count_for(LibraryTab::All), 1);
    }

    #[test]
    fn runtime_install_bumps_playlist_revision_only_for_playlist_content() {
        let mut app = App::new(50);
        let initial_revision = app.playlists.revision();
        let mut playlists = crate::playlists::Playlists::default();
        let playlist_id = playlists.create("Remote mix").unwrap();
        playlists.add(
            &playlist_id,
            crate::api::Song::remote("remote-track", "Remote track", "Artist", "3:00"),
        );
        let playlist_state = crate::personal_state::legacy_state(
            &crate::library::Library::default(),
            &playlists,
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();

        app.install_personal_sync_runtime(playlist_state.clone())
            .unwrap();
        let playlist_revision = initial_revision.wrapping_add(1);
        assert_eq!(app.playlists.revision(), playlist_revision);
        assert_eq!(app.playlists.list().len(), 1);

        app.install_personal_sync_runtime(playlist_state).unwrap();
        assert_eq!(
            app.playlists.revision(),
            playlist_revision,
            "reinstalling identical playlist content is a no-op"
        );

        let mut liked_library = crate::library::Library::default();
        liked_library.favorites.push(crate::api::Song::remote(
            "liked-track",
            "Liked track",
            "Artist",
            "3:00",
        ));
        let rating_only_state = crate::personal_state::legacy_state(
            &liked_library,
            &playlists,
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        app.install_personal_sync_runtime(rating_only_state)
            .unwrap();

        assert_eq!(
            app.playlists.revision(),
            playlist_revision,
            "rating-only projection must not invalidate playlist caches"
        );
        assert_eq!(app.library.favorites[0].video_id, "liked-track");
    }

    #[test]
    fn revoke_summary_names_the_completed_action() {
        let action =
            PersonalSyncAction::Revoke(crate::personal_state::DeviceId::new("old-device").unwrap());
        let message = sync_summary(&action, &crate::sync::manual::ManualSyncSummary::default());
        assert!(message.starts_with("Device removed."));
        assert!(message.contains("WebDAV password"));
    }

    #[test]
    fn exact_persistence_completion_is_the_live_publication_boundary() {
        let (initial, device_id) = keyed_state();
        let mut app = App::new(50);
        app.personal_state.device_id = Some(device_id.clone());
        app.install_personal_sync_runtime(initial).unwrap();
        let observed = app.personal_state.ledger.clone();
        let durable = crate::personal_state::append_operation_as(
            &observed,
            &device_id,
            crate::personal_state::Operation::SetStationProfile {
                query: Some("durable remote mix".to_owned()),
                explore: crate::station::Explore::default(),
            },
            1,
        )
        .unwrap();
        app.personal_state.sync.in_progress = true;
        let (reply, mut response) = oneshot::channel();
        let reply = PersonalSyncReply::new(reply.into());
        app.personal_state.sync.pending_reply = Some(reply.clone());

        let commands = app.finish_personal_sync_persistence(PersonalSyncPersisted {
            commit: Box::new(PersonalSyncCommit {
                action: PersonalSyncAction::SyncNow,
                attempt: 1,
                observed_local_state: observed,
                playlist_revision: app.playlists.revision(),
                summary: crate::sync::manual::ManualSyncSummary::default(),
                stage: PersonalSyncCommitStage::Reconcile {
                    expected_state: durable.clone(),
                    candidate: Box::new(durable.clone()),
                },
                reply,
            }),
            outcome: PersonalSyncPersistOutcome::Committed(Box::new(durable.clone())),
        });

        assert!(commands.is_empty());
        assert_eq!(app.personal_state.ledger.operations, durable.operations);
        assert!(response.try_recv().unwrap().ok);
        assert!(!app.personal_state.sync.in_progress);
    }

    #[test]
    fn network_completion_only_requests_actor_persistence() {
        let (state, device_id) = keyed_state();
        let mut app = App::new(50);
        app.personal_state.device_id = Some(device_id.clone());
        app.install_personal_sync_runtime(state.clone()).unwrap();
        let state_before = app.personal_state.ledger.clone();
        let candidate = detached_candidate(state_before.clone(), device_id);
        app.personal_state.sync.in_progress = true;
        let (reply, _response) = oneshot::channel();
        let reply = PersonalSyncReply::new(reply.into());
        app.personal_state.sync.pending_reply = Some(reply.clone());

        let commands = app.finish_personal_sync(PersonalSyncPrepared {
            action: PersonalSyncAction::SyncNow,
            attempt: 1,
            result: Ok(candidate),
            reply,
        });

        assert!(matches!(
            commands.as_slice(),
            [Cmd::Persist(PersistCmd::PersonalSyncCommit(_))]
        ));
        assert_eq!(app.personal_state.ledger, state_before);
        assert!(app.personal_state.sync.in_progress);
        assert!(app.personal_state.sync.shutdown_context.is_some());
    }

    #[test]
    fn stale_network_candidate_retries_from_the_current_owner_revision() {
        let (stale_state, device_id) = keyed_state();
        let candidate = detached_candidate(stale_state.clone(), device_id.clone());
        let mut app = App::new(50);
        app.personal_state.device_id = Some(device_id);
        app.install_personal_sync_runtime(stale_state).unwrap();
        assert_ne!(
            candidate.expected_local_revision, app.personal_state.ledger.revision,
            "runtime preparation must make the detached candidate stale"
        );
        app.personal_state.sync.in_progress = true;
        let (reply, mut response) = oneshot::channel();
        let reply = PersonalSyncReply::new(reply.into());
        app.personal_state.sync.pending_reply = Some(reply.clone());

        let commands = app.finish_personal_sync(PersonalSyncPrepared {
            action: PersonalSyncAction::SyncNow,
            attempt: 1,
            result: Ok(candidate),
            reply,
        });

        let [
            Cmd::Data(DataCmd::PersonalSync {
                attempt,
                personal_state,
                ..
            }),
        ] = commands.as_slice()
        else {
            panic!("a stale candidate must schedule one detached retry");
        };
        assert_eq!(*attempt, 2);
        assert_eq!(personal_state.revision, app.personal_state.ledger.revision);
        assert!(response.try_recv().is_err());
        assert!(app.personal_state.sync.in_progress);
        assert!(app.personal_state.sync.shutdown_context.is_none());
    }

    #[test]
    fn local_operation_is_reauthored_after_conflicting_sync_dot() {
        let (observed, device_id) = keyed_state();
        let durable = crate::personal_state::append_operation_as(
            &observed,
            &device_id,
            crate::personal_state::Operation::SetStationProfile {
                query: Some("remote mix".to_owned()),
                explore: crate::station::Explore::default(),
            },
            10,
        )
        .unwrap();
        let current = crate::personal_state::append_operation_as(
            &observed,
            &device_id,
            crate::personal_state::Operation::SetAvoidArtist {
                artist_key: "local-artist".to_owned(),
                avoid: true,
            },
            11,
        )
        .unwrap();
        assert_eq!(
            durable.operations.last().unwrap().stamp.dot,
            current.operations.last().unwrap().stamp.dot,
            "the persistence-window race intentionally shares one original local dot"
        );

        let rebased = rebase_local_operations(&durable, &observed, &current, &device_id).unwrap();
        assert_eq!(rebased.operations.len(), durable.operations.len() + 1);
        assert!(rebased.operations.iter().any(|operation| matches!(
            &operation.operation,
            crate::personal_state::Operation::SetStationProfile {
                query: Some(query),
                ..
            } if query == "remote mix"
        )));
        let local = rebased
            .operations
            .iter()
            .find(|operation| {
                matches!(
                    &operation.operation,
                    crate::personal_state::Operation::SetAvoidArtist {
                        artist_key,
                        avoid: true,
                    } if artist_key == "local-artist"
                )
            })
            .unwrap();
        assert!(local.stamp.dot.sequence > durable.operations.last().unwrap().stamp.dot.sequence);
    }
}
