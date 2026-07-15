use std::time::Duration;

use anyhow::Result;

use crate::player::PlayerHandle;
use crate::runtime::RuntimeEvent;
use crate::{config, player, runtime};

type PlayerReadyResult = Result<(PlayerHandle, player::Mpv), String>;
const PLAYER_START_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct PlayerStartup {
    pub(super) ready_rx: Option<tokio::sync::oneshot::Receiver<PlayerReadyResult>>,
    pub(super) task: Option<tokio::task::JoinHandle<()>>,
}

impl PlayerStartup {
    pub(super) async fn recv(
        &mut self,
    ) -> std::result::Result<PlayerReadyResult, tokio::sync::oneshot::error::RecvError> {
        let result = self
            .ready_rx
            .as_mut()
            .expect("player startup receiver is consumed only once")
            .await;
        self.ready_rx = None;
        result
    }

    pub(super) async fn join_finished(&mut self) {
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "player startup task failed unexpectedly");
        }
    }

    /// Close the result slot first, then abort and reap the producer. A startup which has already
    /// created mpv cannot leave its `(handle, guard)` buffered in an unpolled oneshot during the
    /// slower actor and persistence shutdown which follows owner-loop exit.
    pub(super) async fn cancel_and_join(&mut self) {
        if let Some(mut ready_rx) = self.ready_rx.take() {
            ready_rx.close();
            drop(ready_rx);
        }
        if let Some(task) = self.task.take() {
            task.abort();
            match task.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => tracing::warn!(%error, "player startup task failed during shutdown"),
            }
        }
    }
}

impl Drop for PlayerStartup {
    fn drop(&mut self) {
        if let Some(ready_rx) = self.ready_rx.as_mut() {
            ready_rx.close();
        }
        if let Some(task) = self.task.as_ref() {
            task.abort();
        }
    }
}

pub(super) fn spawn_player_startup<F>(future: F) -> PlayerStartup
where
    F: std::future::Future<Output = PlayerReadyResult> + Send + 'static,
{
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let result = future.await;
        if ready_tx.send(result).is_err() {
            // Closing/replacing the result slot is the cancellation boundary for obsolete work.
            // Dropping the rejected value also retires its handle/guard in the safe order.
            tracing::debug!("player startup completed after its readiness slot was closed");
        }
    });
    PlayerStartup {
        ready_rx: Some(ready_rx),
        task: Some(task),
    }
}

pub(super) fn spawn_audio_player(
    worker_tx: runtime::RuntimeSender,
    data_dir: Option<std::path::PathBuf>,
    cfg: &config::PlayerRuntimeConfig,
    shutdown: player::lifetime::ShutdownLatch,
) -> PlayerStartup {
    let cookies_file = cfg.cookies_file.clone();
    let gapless = cfg.gapless;
    let audio = cfg.audio.clone();
    spawn_player_startup(async move {
        if shutdown.is_triggered() {
            return Err(
                "player startup suppressed because terminal shutdown already won".to_owned(),
            );
        }
        match tokio::time::timeout(
            PLAYER_START_TIMEOUT,
            player::spawn(
                runtime::sink(worker_tx, RuntimeEvent::Player),
                data_dir,
                cookies_file,
                gapless,
                audio,
            ),
        )
        .await
        {
            Ok(result) => result.map_err(|error| format!("{error:#}")),
            Err(_) => Err(format!(
                "player startup timed out after {} seconds",
                PLAYER_START_TIMEOUT.as_secs()
            )),
        }
    })
}
