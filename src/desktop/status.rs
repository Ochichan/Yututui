//! Status polling for desktop companion surfaces.

use std::future::Future;
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
        Self {
            state: TrayState::Disconnected,
            error: Some(error),
        }
    }
}

pub async fn poll_once() -> PollUpdate {
    match control::status().await {
        Ok(status) => PollUpdate::connected(status),
        Err(error) => PollUpdate::disconnected(error),
    }
}

pub fn next_delay(config: PollConfig, update: &PollUpdate) -> Duration {
    if matches!(update.state, TrayState::Disconnected) {
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
        let update = poll_once().await;
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
            artwork: None,
        });
        assert_eq!(next_delay(config, &update), Duration::from_millis(10));
    }
}
