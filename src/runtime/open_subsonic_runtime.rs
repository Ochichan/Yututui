//! Owner-lane lifetime for the credential-owning OpenSubsonic actor and playback proxy.

use super::{RuntimeEvent, RuntimeHandles};

impl RuntimeHandles {
    /// Mark a reload generation before its network work starts.
    ///
    /// Only the owner calls this method. A later request makes every older completion inert,
    /// including the process-start load racing a settings save.
    pub(crate) fn begin_open_subsonic_reload(&mut self, generation: u64) {
        self.open_subsonic_reload_generation = generation;
    }

    /// Load the configured profile without blocking first-frame/player startup.
    pub(crate) fn spawn_open_subsonic_reload(
        &mut self,
        generation: u64,
        paths: crate::open_subsonic::OpenSubsonicPaths,
    ) -> bool {
        self.begin_open_subsonic_reload(generation);
        let read_only = self.persistence_read_only.is_some();
        let emitter = self.background_tasks.emitter(self.worker_tx.clone());
        self.background_tasks
            .spawn_cancellable("open_subsonic_startup", async move {
                let result = if read_only {
                    crate::open_subsonic::load_actor_read_only(&paths).await
                } else {
                    crate::open_subsonic::load_actor(&paths).await
                };
                emitter
                    .emit_terminal(RuntimeEvent::OpenSubsonicReloaded { generation, result })
                    .await;
            })
    }

    /// Install one owner-approved load result. Returns `false` for a stale generation.
    pub(crate) fn install_open_subsonic_runtime(
        &mut self,
        generation: u64,
        result: Result<
            Option<crate::open_subsonic::OpenSubsonicRuntime>,
            crate::open_subsonic::ServiceError,
        >,
    ) -> bool {
        if generation != self.open_subsonic_reload_generation {
            drop(result);
            return false;
        }

        match result {
            Ok(Some(runtime)) => {
                // Activation closes the previously published proxy before this stable forwarding
                // slot can expose the new one. Dropping the old guard cannot clear the newer
                // generation because the actor registry compares exact generations.
                runtime.activate();
                self.open_subsonic_routes.install(runtime.route_provider());
                self.open_subsonic_runtime = Some(runtime);
            }
            Ok(None) => {
                self.retire_open_subsonic_runtime();
            }
            Err(error) => {
                self.retire_open_subsonic_runtime();
                tracing::warn!(reason = %error, "music server runtime is unavailable");
            }
        }
        true
    }

    /// Disable new routes before dropping the actor/proxy guard that revokes existing routes.
    pub(crate) fn retire_open_subsonic_runtime(&mut self) {
        self.open_subsonic_routes.disable();
        self.open_subsonic_runtime = None;
    }
}
