//! Reusable remote-control wrapper for desktop companion surfaces.

use std::fmt;

use crate::daemon::{self, DaemonError, StartOptions};
use crate::desktop::launch;
use crate::remote::client::{self, ClientError};
use crate::remote::proto::{RemoteCommand, RemoteResponse, StatusSnapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    NotRunning,
    StaleInstance,
    Rejected(String),
    MissingStatus,
    Transport(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::NotRunning => write!(f, "ytm-tui is not running"),
            ControlError::StaleInstance => write!(f, "the saved ytm-tui instance is stale"),
            ControlError::Rejected(reason) => write!(f, "command rejected: {reason}"),
            ControlError::MissingStatus => write!(f, "ytt returned success without a status body"),
            ControlError::Transport(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<ClientError> for ControlError {
    fn from(value: ClientError) -> Self {
        match value {
            ClientError::NoRunningInstance => ControlError::NotRunning,
            ClientError::ConnectFailed | ClientError::NoResponse => ControlError::StaleInstance,
            other => ControlError::Transport(other.human_message()),
        }
    }
}

impl From<DaemonError> for ControlError {
    fn from(value: DaemonError) -> Self {
        match value {
            DaemonError::StandaloneOwner => ControlError::Rejected(
                "ytm-tui is already running in standalone TUI mode".to_string(),
            ),
            DaemonError::NotRunning(message) => ControlError::Transport(message),
            DaemonError::ResumeRejected(reason) => ControlError::Rejected(reason),
            DaemonError::StopRejected(reason) => ControlError::Rejected(reason),
            other => ControlError::Transport(other.to_string()),
        }
    }
}

pub async fn send_remote(command: RemoteCommand) -> Result<RemoteResponse, ControlError> {
    let resp = client::send(command).await.map_err(ControlError::from)?;
    response_to_result(resp)
}

pub fn response_to_result(resp: RemoteResponse) -> Result<RemoteResponse, ControlError> {
    if resp.ok {
        Ok(resp)
    } else {
        Err(ControlError::Rejected(
            resp.reason.unwrap_or_else(|| "rejected".to_string()),
        ))
    }
}

pub async fn status() -> Result<StatusSnapshot, ControlError> {
    let resp = send_remote(RemoteCommand::Status).await?;
    resp.status.ok_or(ControlError::MissingStatus)
}

pub async fn start_daemon(resume: bool) -> Result<(), ControlError> {
    daemon::start_daemon(StartOptions {
        resume,
        from_tray: true,
        executable: Some(launch::resolve_ytt_path()),
    })
    .await
    .map(|_| ())
    .map_err(ControlError::from)
}

pub async fn stop_daemon() -> Result<(), ControlError> {
    daemon::stop_daemon().await.map_err(ControlError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_errors_map_to_user_facing_control_errors() {
        assert_eq!(
            ControlError::from(ClientError::NoRunningInstance),
            ControlError::NotRunning
        );
        assert_eq!(
            ControlError::from(ClientError::ConnectFailed),
            ControlError::StaleInstance
        );
        assert!(matches!(
            ControlError::from(ClientError::MalformedEndpoint),
            ControlError::Transport(_)
        ));
    }

    #[test]
    fn rejected_response_maps_to_control_error() {
        let err = response_to_result(RemoteResponse::err("queue_empty")).unwrap_err();
        assert_eq!(err, ControlError::Rejected("queue_empty".to_string()));
    }

    #[test]
    fn daemon_errors_map_to_user_facing_control_errors() {
        assert_eq!(
            ControlError::from(DaemonError::StandaloneOwner),
            ControlError::Rejected("ytm-tui is already running in standalone TUI mode".to_string())
        );
        assert_eq!(
            ControlError::from(DaemonError::StopRejected("busy".to_string())),
            ControlError::Rejected("busy".to_string())
        );
    }
}
