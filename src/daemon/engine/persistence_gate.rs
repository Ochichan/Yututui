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

fn save_store(kind: StoreKind, save: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
    if save_failure_injected(kind) {
        return Err(std::io::Error::other(format!(
            "injected {} save failure",
            kind.label()
        )));
    }
    save()
}

impl DaemonEngine {
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
        if self.should_skip_remote_save() {
            return;
        }
        if let Err(error) = save_store(StoreKind::Library, || self.library.save()) {
            self.record_persistence_failure(context, error);
        }
    }

    pub(super) fn save_playlists(&mut self, context: &str) {
        if self.should_skip_remote_save() {
            return;
        }
        if let Err(error) = save_store(StoreKind::Playlists, || self.playlists.save()) {
            self.record_persistence_failure(context, error);
        }
    }

    pub(super) fn save_signals(&mut self, context: &str) {
        if self.should_skip_remote_save() {
            return;
        }
        if let Err(error) = save_store(StoreKind::Signals, || self.signals.save()) {
            self.record_persistence_failure(context, error);
        }
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
