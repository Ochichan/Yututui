//! Bounded blocking operations for the Settings Sync wizard.

use crate::app::{SyncFormError, SyncUiCommand, SyncUiEvent};
use crate::sync::service::SyncServiceError;

pub(super) fn run(command: SyncUiCommand) -> SyncUiEvent {
    match command {
        SyncUiCommand::Refresh {
            flow_id,
            request_id,
            state,
            in_progress,
        } => {
            let result = sync_paths().and_then(|paths| {
                let read_only = crate::persist::persistence_access().is_read_only();
                let state = if read_only {
                    let personal_paths = crate::personal_state::PersonalStatePaths::current()
                        .map_err(SyncServiceError::from)?;
                    crate::sync::service::load_personal_state_read_only(&personal_paths)?
                        .unwrap_or(*state)
                } else {
                    *state
                };
                crate::sync::service::read_overview(
                    &paths,
                    &state,
                    in_progress,
                    crate::signals::unix_now(),
                )
            });
            SyncUiEvent::Refreshed {
                flow_id,
                request_id,
                result: Box::new(result),
            }
        }
        SyncUiCommand::RecoveryExport {
            flow_id,
            state,
            source,
            destination,
        } => {
            let result = sync_paths().and_then(|paths| {
                crate::sync::service::export_recovery_kit(&state, &paths, &source, &destination)
            });
            SyncUiEvent::RecoveryExported {
                flow_id,
                result: Box::new(result),
            }
        }
        SyncUiCommand::SetupPrepare {
            flow_id,
            state,
            playlist_revision,
            input,
        } => {
            let result = input
                .into_setup_request()
                .map_err(map_form_error)
                .and_then(|request| {
                    sync_paths().and_then(|paths| {
                        crate::sync::service::prepare_setup(
                            &state,
                            playlist_revision,
                            &paths,
                            request,
                        )
                    })
                });
            SyncUiEvent::SetupPrepared {
                flow_id,
                result: Box::new(result),
            }
        }
        SyncUiCommand::SetupResume {
            flow_id,
            state,
            playlist_revision,
        } => {
            let result = sync_paths().and_then(|paths| {
                crate::sync::service::resume_prepared_setup(&state, playlist_revision, &paths)
            });
            SyncUiEvent::SetupPrepared {
                flow_id,
                result: Box::new(result),
            }
        }
        SyncUiCommand::HostCreate { flow_id, state } => {
            let result = sync_paths().and_then(|paths| {
                crate::sync::service::create_pairing_invite(
                    &state,
                    &paths,
                    crate::signals::unix_now(),
                )
            });
            SyncUiEvent::HostCreated {
                flow_id,
                result: Box::new(result),
            }
        }
        SyncUiCommand::HostPoll {
            flow_id,
            state,
            mut host,
        } => {
            let result = sync_paths().and_then(|paths| {
                crate::sync::service::poll_pairing_request(
                    &state,
                    &paths,
                    &mut host,
                    crate::signals::unix_now(),
                )
            });
            SyncUiEvent::HostPolled {
                flow_id,
                host,
                result: Box::new(result),
            }
        }
        SyncUiCommand::HostApprove {
            flow_id,
            state,
            mut host,
            review,
        } => {
            let observed_state = state.clone();
            let result = sync_paths().and_then(|paths| {
                crate::sync::service::prepare_pairing_approval(
                    &state,
                    &paths,
                    &mut host,
                    *review,
                    crate::signals::unix_now(),
                )
            });
            SyncUiEvent::HostApprovalPrepared {
                flow_id,
                host,
                observed_state,
                result: Box::new(result),
            }
        }
        SyncUiCommand::HostCancel {
            flow_id,
            state,
            host,
        } => {
            let result = sync_paths().and_then(|paths| {
                crate::sync::service::cancel_pairing_invite(&state, &paths, &host)
            });
            SyncUiEvent::HostCancelled {
                flow_id,
                host,
                result,
            }
        }
        SyncUiCommand::JoinStart { flow_id, input } => {
            let result = input.into_join_request().map_err(map_form_error).and_then(
                |input: crate::app::SyncJoinRequest| {
                    sync_paths().and_then(|paths| {
                        crate::sync::service::start_pairing_join(
                            &paths,
                            input.endpoint,
                            input.custom_ca_pem,
                            input.credential,
                            input.pairing_code.as_str(),
                            input.device_name,
                            crate::signals::unix_now(),
                        )
                    })
                },
            );
            SyncUiEvent::JoinStarted {
                flow_id,
                result: Box::new(result),
            }
        }
        SyncUiCommand::JoinPoll { flow_id, state } => {
            let result = sync_paths().and_then(|paths| {
                crate::sync::service::poll_pairing_join(&state, &paths, crate::signals::unix_now())
            });
            SyncUiEvent::JoinPolled {
                flow_id,
                result: Box::new(result),
            }
        }
        SyncUiCommand::JoinResume { flow_id, state } => {
            let result = sync_paths()
                .and_then(|paths| crate::sync::service::resume_pairing_join(&state, &paths));
            SyncUiEvent::JoinResumed {
                flow_id,
                result: Box::new(result),
            }
        }
        SyncUiCommand::DiscardJoin { flow_id } => {
            let result = sync_paths().and_then(|paths| discard_unfinished_join_at(&paths));
            SyncUiEvent::JoinDiscarded { flow_id, result }
        }
        SyncUiCommand::JoinPrepareActivation {
            flow_id,
            state,
            preview,
        } => {
            let result = sync_paths().and_then(|paths| {
                crate::sync::service::prepare_pairing_join_activation(&state, &paths, *preview)
            });
            SyncUiEvent::JoinActivationPrepared {
                flow_id,
                result: Box::new(result),
            }
        }
    }
}

fn sync_paths() -> Result<crate::sync::SyncPaths, SyncServiceError> {
    crate::sync::SyncPaths::current().map_err(SyncServiceError::from)
}

fn discard_unfinished_join_at(paths: &crate::sync::SyncPaths) -> Result<(), SyncServiceError> {
    if crate::sync::service::read_lifecycle(paths)?
        != crate::sync::service::SyncLifecycleState::NeedsCleanup
    {
        return Err(SyncServiceError::LocalStateChanged);
    }
    crate::sync::service::cancel_pairing_join(paths)
}

fn map_form_error(error: SyncFormError) -> SyncServiceError {
    match error {
        SyncFormError::CustomCa => SyncServiceError::Certificate,
        SyncFormError::EndpointRequired
        | SyncFormError::SecretRequired
        | SyncFormError::InvalidDeviceName
        | SyncFormError::PairingCodeRequired
        | SyncFormError::RecoveryFileRequired => SyncServiceError::InvalidRemoteData,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(std::path::PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "yututui-sync-ui-worker-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn discard_scope_guard_rejects_a_non_cleanup_lifecycle() {
        let root = TempRoot::new();
        let paths = crate::sync::SyncPaths::for_data_root(root.0.clone());

        assert_eq!(
            discard_unfinished_join_at(&paths),
            Err(SyncServiceError::LocalStateChanged)
        );
    }

    #[test]
    fn discard_keeps_unverifiable_cleanup_artifacts() {
        let root = TempRoot::new();
        let paths = crate::sync::SyncPaths::for_data_root(root.0.clone());
        std::fs::create_dir_all(paths.root()).unwrap();
        std::fs::write(paths.profile(), b"not an authenticated profile").unwrap();

        assert_eq!(
            crate::sync::service::read_lifecycle(&paths).unwrap(),
            crate::sync::service::SyncLifecycleState::NeedsCleanup
        );
        assert_eq!(
            discard_unfinished_join_at(&paths),
            Err(SyncServiceError::InvalidRemoteData)
        );
        assert!(
            paths.profile().exists(),
            "unverifiable artifacts must not be deleted"
        );
    }
}
