//! Daemon ownership for the OpenSubsonic actor and loopback playback proxy.

use std::time::Duration;

use super::{DaemonEngine, data_dir};

const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(15 * 60);

fn next_retry(delay: Duration) -> Duration {
    delay.saturating_mul(2).min(RETRY_MAX)
}

pub(super) struct OpenSubsonicOwner {
    routes: crate::playback_target::PlaybackRouteProviderSlot,
    loader: crate::util::background_task::BackgroundTask,
}

impl Default for OpenSubsonicOwner {
    fn default() -> Self {
        Self {
            routes: Default::default(),
            loader: crate::util::background_task::BackgroundTask::disabled(
                "daemon music server loader",
            ),
        }
    }
}

impl OpenSubsonicOwner {
    fn start(&mut self, paths: Option<crate::open_subsonic::OpenSubsonicPaths>) {
        self.retire();
        let Some(paths) = paths else {
            return;
        };
        let routes = self.routes.clone();
        self.loader = crate::util::background_task::BackgroundTask::spawn(
            "daemon music server loader",
            async move {
                let mut retry = RETRY_MIN;
                loop {
                    // The loader already runs off the daemon owner path, and every DNS/API step
                    // inside `load_actor` has its own bound. Do not impose a shorter aggregate
                    // deadline that can permanently reject a valid but slower server.
                    match crate::open_subsonic::load_actor(&paths).await {
                        Ok(Some(runtime)) => {
                            runtime.activate();
                            routes.install(runtime.route_provider());
                            // This task owns the actor and proxy until daemon shutdown.
                            std::future::pending::<()>().await;
                            drop(runtime);
                        }
                        Ok(None) => return,
                        Err(error) => {
                            tracing::warn!(
                                reason = %error,
                                retry_seconds = retry.as_secs(),
                                "daemon music server runtime is unavailable; retrying"
                            );
                            tokio::time::sleep(retry).await;
                            retry = next_retry(retry);
                        }
                    }
                }
            },
        );
    }

    pub(super) fn route_provider(&self) -> crate::playback_target::PlaybackRouteProviderHandle {
        self.routes.handle()
    }

    pub(super) fn retire(&mut self) {
        self.routes.disable();
        self.loader =
            crate::util::background_task::BackgroundTask::disabled("daemon music server loader");
    }

    pub(super) async fn shutdown(&mut self) {
        self.routes.disable();
        self.loader.shutdown().await;
    }
}

pub(super) fn initialize(engine: &mut DaemonEngine) {
    let paths = data_dir().map(crate::open_subsonic::OpenSubsonicPaths::for_data_root);
    engine.open_subsonic.start(paths);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(next_retry(RETRY_MIN), Duration::from_secs(4));
        assert_eq!(next_retry(RETRY_MAX), RETRY_MAX);
    }
}
