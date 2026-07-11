//! Status polling for desktop companion surfaces.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;

use super::control::{self, ControlError};
use super::menu_model::TrayState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollConfig {
    pub connected_interval: Duration,
    pub disconnected_interval: Duration,
}

impl Default for PollConfig {
    fn default() -> Self {
        Self {
            connected_interval: Duration::from_secs(2),
            disconnected_interval: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollUpdate {
    pub state: TrayState,
    pub error: Option<ControlError>,
}

impl PollUpdate {
    pub fn connected(status: crate::remote::proto::StatusSnapshot) -> Self {
        Self {
            state: TrayState::Connected(status),
            error: None,
        }
    }

    pub fn disconnected(error: ControlError) -> Self {
        Self::disconnected_with_resume(error, false)
    }

    pub fn disconnected_with_resume(error: ControlError, resume_available: bool) -> Self {
        Self {
            state: TrayState::disconnected(resume_available),
            error: Some(error),
        }
    }
}

pub async fn poll_once() -> PollUpdate {
    match control::status().await {
        Ok(status) => PollUpdate::connected(status),
        Err(error) => {
            PollUpdate::disconnected_with_resume(error, crate::session::resume_available())
        }
    }
}

/// Process-local serialization gate for the legacy v7 request/response socket. A status request
/// is a one-shot connection, but callers from the periodic fallback and lifecycle command lane
/// must still not overlap and race their projections.
#[derive(Clone)]
pub struct PollLane(Arc<tokio::sync::Mutex<()>>);

impl Default for PollLane {
    fn default() -> Self {
        static PROCESS_LANE: std::sync::OnceLock<Arc<tokio::sync::Mutex<()>>> =
            std::sync::OnceLock::new();
        Self(Arc::clone(
            PROCESS_LANE.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))),
        ))
    }
}

impl PollLane {
    pub async fn poll_once(&self) -> PollUpdate {
        let _guard = self.0.lock().await;
        poll_once().await
    }
}

pub async fn poll_once_exclusive() -> PollUpdate {
    PollLane::default().poll_once().await
}

pub fn next_delay(config: PollConfig, update: &PollUpdate) -> Duration {
    if matches!(update.state, TrayState::Disconnected { .. }) {
        config.disconnected_interval
    } else {
        config.connected_interval
    }
}

pub async fn run_until_shutdown<F, S>(config: PollConfig, mut emit: F, shutdown: S)
where
    F: FnMut(PollUpdate),
    S: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    loop {
        let update = tokio::select! {
            update = poll_once() => update,
            _ = &mut shutdown => break,
        };
        let delay = next_delay(config, &update);
        emit(update);
        tokio::select! {
            _ = sleep(delay) => {}
            _ = &mut shutdown => break,
        }
    }
}

pub async fn run_until_shutdown_with_lane<F, S>(
    config: PollConfig,
    lane: PollLane,
    mut emit: F,
    shutdown: S,
) where
    F: FnMut(PollUpdate),
    S: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    loop {
        let update = tokio::select! {
            update = lane.poll_once() => update,
            _ = &mut shutdown => break,
        };
        let delay = next_delay(config, &update);
        emit(update);
        tokio::select! {
            _ = sleep(delay) => {}
            _ = &mut shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_updates_use_slow_interval() {
        let config = PollConfig {
            connected_interval: Duration::from_millis(10),
            disconnected_interval: Duration::from_millis(50),
        };
        let update = PollUpdate::disconnected(ControlError::NotRunning);
        assert_eq!(next_delay(config, &update), Duration::from_millis(50));
    }

    #[test]
    fn connected_updates_use_fast_interval() {
        let config = PollConfig {
            connected_interval: Duration::from_millis(10),
            disconnected_interval: Duration::from_millis(50),
        };
        let update = PollUpdate::connected(crate::remote::proto::StatusSnapshot {
            title: Some("Song".to_string()),
            artist: None,
            paused: false,
            volume: 50,
            position: 1,
            total: 1,
            streaming: false,
            owner_mode: crate::remote::proto::InstanceMode::StandaloneTui,
            settings: Default::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
        });
        assert_eq!(next_delay(config, &update), Duration::from_millis(10));
    }

    #[tokio::test]
    async fn shutdown_cancels_a_poll_waiting_for_the_exclusive_lane() {
        let lane = PollLane(Arc::new(tokio::sync::Mutex::new(())));
        let _held = lane.0.lock().await;
        let runner_lane = lane.clone();
        tokio::time::timeout(
            Duration::from_millis(50),
            run_until_shutdown_with_lane(
                PollConfig::default(),
                runner_lane,
                |_| panic!("a blocked poll must not emit"),
                async {},
            ),
        )
        .await
        .expect("shutdown should cancel the lane wait immediately");
    }
}
