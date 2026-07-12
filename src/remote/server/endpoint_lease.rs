use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

use super::PublishedInstance;
#[cfg(unix)]
use super::{SocketFileIdentity, remove_socket_file_if_matches};
use crate::remote::endpoint;
use crate::remote::proto::InstanceFile;

/// Shared ownership of the published descriptor and bound socket name.
///
/// The accept task starts before descriptor publication. If it exhausts its accept-error budget
/// in that window, `release_requested` remembers the failure and publication immediately runs the
/// same exact-identity cleanup before `RemoteServer::start` returns.
pub(super) struct EndpointLease {
    #[cfg_attr(windows, allow(dead_code))]
    socket: String,
    #[cfg(unix)]
    socket_identity: Option<SocketFileIdentity>,
    state: Mutex<LeaseState>,
    accept_owner_failed: AtomicBool,
}

#[derive(Default)]
struct LeaseState {
    advertisement: Option<PublishedInstance>,
    publication_complete: bool,
    release_requested: bool,
    released: bool,
}

struct CleanupPlan {
    advertisement: Option<PublishedInstance>,
}

impl EndpointLease {
    pub(super) fn new(
        socket: String,
        #[cfg(unix)] socket_identity: Option<SocketFileIdentity>,
    ) -> Self {
        Self {
            socket,
            #[cfg(unix)]
            socket_identity,
            state: Mutex::new(LeaseState::default()),
            accept_owner_failed: AtomicBool::new(false),
        }
    }

    pub(super) fn socket(&self) -> &str {
        &self.socket
    }

    /// Publish only while the listener owner is still healthy. Publication and failure marking
    /// share `state`, while the accept task retains the listener until marking completes, so a
    /// failed old owner cannot overwrite a successor descriptor during this transition.
    pub(super) fn publish_instance(&self, identity: Option<InstanceFile>) {
        self.publish_with(identity, |identity| {
            let path = endpoint::instance_path()?;
            endpoint::write_instance(identity)?;
            Ok(path)
        });
    }

    #[cfg(all(test, unix))]
    pub(super) fn publish_instance_at(&self, path: std::path::PathBuf, identity: InstanceFile) {
        self.publish_with(Some(identity), move |identity| {
            crate::util::safe_fs::write_private_atomic_json(&path, identity)?;
            Ok(path)
        });
    }

    fn publish_with<F>(&self, identity: Option<InstanceFile>, write: F)
    where
        F: FnOnce(&InstanceFile) -> std::io::Result<std::path::PathBuf>,
    {
        let cleanup = {
            let mut state = self.lock_state();
            let skip = state.release_requested || self.accept_owner_failed();
            state.advertisement = if skip {
                None
            } else {
                identity.and_then(|identity| match write(&identity) {
                    Ok(path) => Some(PublishedInstance { path, identity }),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "remote: could not write instance descriptor; no remote control"
                        );
                        None
                    }
                })
            };
            state.publication_complete = true;
            Self::take_cleanup_plan(&mut state)
        };
        self.execute_cleanup(cleanup);
    }

    /// Complete the post-spawn publication phase and honor an accept failure that raced it.
    #[cfg(test)]
    pub(super) fn finish_publication(&self, advertisement: Option<PublishedInstance>) {
        let cleanup = {
            let mut state = self.lock_state();
            state.advertisement = advertisement;
            state.publication_complete = true;
            Self::take_cleanup_plan(&mut state)
        };
        self.execute_cleanup(cleanup);
    }

    /// Revoke this exact endpoint after the accept owner gives up permanently.
    pub(super) fn mark_accept_owner_failed(&self) {
        self.accept_owner_failed.store(true, Ordering::Release);
        // The accept task still owns the listener while this runs, so no successor can have
        // legitimately rebound the pathname unless another actor first removed it. Reclaim the
        // exact socket now and never retry from delayed publication/guard cleanup: a late retry
        // would have an unavoidable ABA window on filesystems that immediately reuse inodes.
        #[cfg(unix)]
        let current_identity = super::socket_file_identity(&self.socket).ok();
        match remove_socket_file_if_matches(&self.socket, current_identity) {
            Ok(_) => {}
            Err(error) => tracing::warn!(
                %error,
                "remote: failed to revoke unusable socket endpoint"
            ),
        }
        self.release();
    }

    pub(super) fn accept_owner_failed(&self) -> bool {
        self.accept_owner_failed.load(Ordering::Acquire)
    }

    /// Idempotently relinquish the exact descriptor and socket identity owned by this lease.
    pub(super) fn release(&self) {
        let cleanup = {
            let mut state = self.lock_state();
            state.release_requested = true;
            Self::take_cleanup_plan(&mut state)
        };
        self.execute_cleanup(cleanup);
    }

    fn take_cleanup_plan(state: &mut LeaseState) -> Option<CleanupPlan> {
        if !state.publication_complete || !state.release_requested || state.released {
            return None;
        }
        state.released = true;
        Some(CleanupPlan {
            advertisement: state.advertisement.take(),
        })
    }

    fn execute_cleanup(&self, cleanup: Option<CleanupPlan>) {
        let Some(cleanup) = cleanup else {
            return;
        };
        if let Some(advertisement) = cleanup.advertisement
            && let Err(error) = endpoint::remove_instance_file_if_matches(
                &advertisement.path,
                &advertisement.identity,
            )
        {
            tracing::warn!(%error, "remote: failed to remove owned instance descriptor");
        }
        #[cfg(unix)]
        if !self.accept_owner_failed() {
            match remove_socket_file_if_matches(&self.socket, self.socket_identity) {
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "remote: failed to release owned socket endpoint")
                }
            }
        }
        if self.accept_owner_failed() {
            tracing::warn!(
                "remote: accept owner stopped after repeated failures; owned endpoint revoked"
            );
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, LeaseState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("remote: recovering poisoned endpoint lease state");
            poisoned.into_inner()
        })
    }
}
