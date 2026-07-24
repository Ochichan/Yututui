use super::delivery::DURABILITY_UNCONFIRMED_REASON;
use super::{DaemonEngine, EngineEffect, EngineError};
use crate::persist::{StartupRecoveryError, StoreKind};
use crate::remote::proto::{RemoteCommand, RemoteResponse};

/// Only operations that do not change durable daemon state may continue after coherent
/// persistence ownership has been lost. Unknown future commands default to rejection.
fn may_continue_read_only(command: &RemoteCommand) -> bool {
    matches!(
        command,
        RemoteCommand::Status
            | RemoteCommand::RunSearch { .. }
            | RemoteCommand::Quit
            // v8 pure reads: paging/drill-down/provenance never touch durable state.
            | RemoteCommand::FetchLibraryPage { .. }
            | RemoteCommand::FetchPlaylistDetail { .. }
            | RemoteCommand::FetchWhyGem { .. }
    )
}

pub(super) fn current_recovery_status() -> Result<(), StartupRecoveryError> {
    #[cfg(test)]
    if let Some(error) = TEST_RECOVERY_ERROR.with(|slot| slot.borrow().clone()) {
        return Err(error);
    }
    crate::persist::ensure_startup_recovery_coherent()
}

#[cfg(test)]
fn save_failure_injected(kind: StoreKind) -> bool {
    TEST_SAVE_FAILURE.with(|failure| failure.get() == Some(kind))
}

#[cfg(not(test))]
fn save_failure_injected(_kind: StoreKind) -> bool {
    false
}

fn save_store<T>(kind: StoreKind, save: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    if save_failure_injected(kind) {
        return Err(std::io::Error::other(format!(
            "injected {} save failure",
            kind.label()
        )));
    }
    save()
}

impl DaemonEngine {
    fn reconcile_personal_state(
        &self,
        playlists: &crate::playlists::Playlists,
    ) -> Result<crate::personal_state::PersonalStateV2, crate::personal_state::PersonalStateError>
    {
        match &self.personal_state_device_id {
            Some(device_id) => crate::personal_state::reconcile_runtime_as(
                &self.personal_state,
                device_id,
                &self.library,
                playlists,
                &self.signals,
                &self.station,
            ),
            None => crate::personal_state::reconcile_runtime(
                &self.personal_state,
                &self.library,
                playlists,
                &self.signals,
                &self.station,
            ),
        }
    }

    pub(super) fn personal_state_paths(
        &self,
    ) -> Result<crate::personal_state::PersonalStatePaths, crate::personal_state::PersonalStateError>
    {
        #[cfg(test)]
        {
            Ok(self.personal_state_paths.clone())
        }
        #[cfg(not(test))]
        {
            crate::personal_state::PersonalStatePaths::current()
        }
    }

    pub(super) fn reject_remote_recovery(&mut self, error: StartupRecoveryError) -> RemoteResponse {
        let error = EngineError::from(error);
        let message = error.to_string();
        tracing::warn!(%error, "daemon persistence became unavailable during a command");
        self.remote_persistence_error = Some(message);
        self.remote_persistence_read_only = true;
        RemoteResponse::err(error.reason())
    }

    pub(super) fn preflight_remote_persistence(
        &mut self,
        command: &RemoteCommand,
    ) -> Option<RemoteResponse> {
        self.remote_persistence_write_failed = false;
        self.remote_persistence_command_active = false;
        self.remote_persistence_read_only = false;
        match current_recovery_status() {
            Ok(()) => {
                self.remote_persistence_error = None;
                self.remote_persistence_command_active = true;
                None
            }
            Err(error) => {
                let error = EngineError::from(error);
                let message = error.to_string();
                if self.remote_persistence_error.as_deref() != Some(message.as_str()) {
                    tracing::warn!(%error, "daemon persistence preflight failed");
                }
                self.remote_persistence_error = Some(message.clone());
                self.remote_persistence_read_only = true;
                if may_continue_read_only(command) {
                    self.remote_persistence_command_active = true;
                    None
                } else {
                    Some(RemoteResponse::err(error.reason()))
                }
            }
        }
    }

    fn should_skip_remote_save(&self) -> bool {
        self.remote_persistence_command_active
            && (self.remote_persistence_write_failed || self.remote_persistence_read_only)
    }

    fn record_persistence_failure(&mut self, context: &str, error: std::io::Error) {
        let error = EngineError::PersistenceWrite(format!("failed to save {context}: {error}"));
        tracing::warn!(%error, "daemon persistence failed");
        let message = error.to_string();
        self.remote_persistence_error = Some(message);
        if self.remote_persistence_command_active {
            self.remote_persistence_write_failed = true;
        }
    }

    pub(super) fn save_config(&mut self, context: &str) {
        if self.should_skip_remote_save() {
            return;
        }
        if let Err(error) = save_store(StoreKind::Config, || self.config.save()) {
            self.record_persistence_failure(context, error);
        }
    }

    pub(super) fn save_library(&mut self, context: &str) {
        self.save_personal_state(context, StoreKind::Library);
    }

    pub(super) fn save_playlists(&mut self, context: &str) {
        self.save_personal_state(context, StoreKind::Playlists);
    }

    fn save_personal_state(&mut self, context: &str, failure_kind: StoreKind) {
        if self.should_skip_remote_save() {
            return;
        }
        if let Err(error) = self.commit_live_personal_state(failure_kind) {
            self.record_persistence_failure(context, error);
        }
    }

    /// Make every mutation currently visible in the daemon projections durable before a
    /// detached personal-sync worker observes it.
    ///
    /// The exact enrolled device authors any synthesized operation. The live ledger and
    /// projections are swapped only after the multi-store transaction is confirmed, so a
    /// transient write failure leaves the user's in-memory mutation available for a later retry.
    pub(super) fn persist_live_personal_state_for_sync(
        &mut self,
    ) -> Result<crate::personal_state::PersonalStateV2, crate::sync::service::SyncServiceError>
    {
        if let Err(error) = current_recovery_status() {
            self.record_persistence_failure(
                "daemon personal sync preflight",
                std::io::Error::other(error.to_string()),
            );
            return Err(crate::sync::service::SyncServiceError::Storage);
        }
        if self.should_skip_remote_save() {
            return Err(crate::sync::service::SyncServiceError::Storage);
        }
        match self.commit_live_personal_state(StoreKind::PersonalState) {
            Ok(state) => Ok(state),
            Err(error) => {
                self.record_persistence_failure("daemon personal sync source", error);
                Err(crate::sync::service::SyncServiceError::Storage)
            }
        }
    }

    fn commit_live_personal_state(
        &mut self,
        failure_kind: StoreKind,
    ) -> Result<crate::personal_state::PersonalStateV2, std::io::Error> {
        let commit = self
            .reconcile_personal_state(&self.playlists)
            .and_then(|state| {
                crate::personal_state::PersonalStateCommit::prepare_for_runtime(
                    state,
                    self.playlists.revision(),
                )
            });
        let commit = match commit {
            Ok(commit) => commit,
            Err(error) => return Err(std::io::Error::other(error.to_string())),
        };
        let paths = match self.personal_state_paths() {
            Ok(paths) => paths,
            Err(error) => return Err(std::io::Error::other(error.to_string())),
        };
        let installed = save_store(failure_kind, || {
            commit.commit(&paths).map_err(std::io::Error::other)
        })?;
        if installed != *commit.state() {
            return Err(std::io::Error::other(
                "personal state changed while the daemon transaction was installing",
            ));
        }
        let (library, mut playlists, signals, station) = commit.runtime_stores();
        let playlists_changed = playlists.inherit_revision_from(&self.playlists);
        self.personal_state = installed.clone();
        self.library = library;
        self.playlists = playlists;
        self.signals = signals;
        self.station = station;
        if playlists_changed {
            self.bump_playlists_rev();
        }
        Ok(installed)
    }

    /// Persist an owner candidate without touching the live daemon store. The caller may swap it
    /// only after this gate returns success.
    pub(super) fn persist_transfer_playlists_candidate(
        &mut self,
        candidate: &crate::playlists::Playlists,
    ) -> Result<(), crate::transfer::local_playlist::LocalPlaylistStoreError> {
        if let Err(error) = current_recovery_status() {
            return Err(
                crate::transfer::local_playlist::LocalPlaylistStoreError::resumable(format!(
                    "daemon recovery preflight rejected transfer playlist persistence: {error}"
                )),
            );
        }
        if self.should_skip_remote_save() {
            return Err(
                crate::transfer::local_playlist::LocalPlaylistStoreError::resumable(
                    "daemon persistence gate is read-only for this owner turn",
                ),
            );
        }
        let commit = self
            .reconcile_personal_state(candidate)
            .and_then(|state| {
                crate::personal_state::PersonalStateCommit::prepare_for_runtime(
                    state,
                    candidate.revision(),
                )
            })
            .map_err(|error| {
                crate::transfer::local_playlist::LocalPlaylistStoreError::resumable(format!(
                    "failed to prepare daemon transfer playlists: {error}"
                ))
            })?;
        let paths = self.personal_state_paths().map_err(|error| {
            crate::transfer::local_playlist::LocalPlaylistStoreError::resumable(format!(
                "failed to resolve personal-state storage: {error}"
            ))
        })?;
        match save_store(StoreKind::Playlists, || {
            commit
                .commit(&paths)
                .map(|_| ())
                .map_err(std::io::Error::other)
        }) {
            Ok(()) => {
                self.personal_state = commit.state().clone();
                Ok(())
            }
            Err(error) => {
                let message = format!("failed to save daemon transfer playlists: {error}");
                self.record_persistence_failure("daemon transfer playlists", error);
                Err(crate::transfer::local_playlist::LocalPlaylistStoreError::resumable(message))
            }
        }
    }

    pub(super) fn save_signals(&mut self, context: &str) {
        self.save_personal_state(context, StoreKind::Signals);
    }

    pub(super) fn save_session(&mut self) {
        if self.should_skip_remote_save() {
            return;
        }
        let snapshot = self.session_cache_snapshot();
        if let Err(error) = save_store(StoreKind::Session, || snapshot.save()) {
            self.record_persistence_failure("daemon session", error);
        }
    }

    pub(super) fn finish_remote_persistence(
        &mut self,
        response: RemoteResponse,
        shutdown: bool,
        effects: Vec<EngineEffect>,
    ) -> (RemoteResponse, bool, Vec<EngineEffect>) {
        self.remote_persistence_command_active = false;
        if self.remote_persistence_write_failed {
            (
                // Player commands and actor effects may already be visible. Preserve the
                // applied owner projection and return a distinct result so clients know not
                // to retry under a new request identity; stable request-id replay returns this
                // same terminal response without executing the command again.
                RemoteResponse::err(DURABILITY_UNCONFIRMED_REASON),
                shutdown,
                effects,
            )
        } else {
            (response, shutdown, effects)
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_RECOVERY_ERROR: std::cell::RefCell<Option<StartupRecoveryError>> = const {
        std::cell::RefCell::new(None)
    };
    static TEST_SAVE_FAILURE: std::cell::Cell<Option<StoreKind>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(super) struct TestRecoveryFailureGuard {
    previous: Option<StartupRecoveryError>,
}

#[cfg(test)]
impl Drop for TestRecoveryFailureGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        TEST_RECOVERY_ERROR.with(|slot| *slot.borrow_mut() = previous);
    }
}

#[cfg(test)]
pub(super) fn fail_recovery_for_test(error: StartupRecoveryError) -> TestRecoveryFailureGuard {
    let previous = TEST_RECOVERY_ERROR.with(|slot| slot.borrow_mut().replace(error));
    TestRecoveryFailureGuard { previous }
}

#[cfg(test)]
pub(super) struct TestSaveFailureGuard {
    previous: Option<StoreKind>,
}

#[cfg(test)]
impl Drop for TestSaveFailureGuard {
    fn drop(&mut self) {
        TEST_SAVE_FAILURE.with(|failure| failure.set(self.previous));
    }
}

#[cfg(test)]
pub(super) fn fail_store_saves_for_test(kind: StoreKind) -> TestSaveFailureGuard {
    let previous = TEST_SAVE_FAILURE.with(|failure| failure.replace(Some(kind)));
    TestSaveFailureGuard { previous }
}
