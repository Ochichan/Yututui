//! The chooser's `[r]` path: ask the running player to quit, wait until it is truly
//! gone, then take over as the primary.
//!
//! Ordering matters here. The old owner's teardown releases its socket *before* it
//! flushes persistence (`complete_owner_teardown`), and the single-writer lease is only
//! released at process exit — so a socket going quiet is NOT permission to initialize
//! persistence. The sequence therefore waits for the old *process* to exit (when its pid
//! is known) before reporting takeover, and never force-kills: a stuck owner is reported,
//! not displaced.

use std::time::Duration;

use crate::remote;
use crate::remote::proto::{InstanceFile, RemoteCommand};
use crate::t;

pub struct RestartBudget {
    pub quit_ack: Duration,
    pub socket_release: Duration,
    pub process_exit: Duration,
}

impl Default for RestartBudget {
    fn default() -> Self {
        Self {
            quit_ack: Duration::from_secs(3),
            socket_release: Duration::from_secs(10),
            process_exit: Duration::from_secs(15),
        }
    }
}

pub enum RestartResult {
    /// We are the new primary (or run degraded without remote control, like any startup).
    TookOver {
        remote: Option<Box<remote::RemoteServer>>,
    },
    /// Someone else bound the socket between the old owner's exit and our rebind.
    LostRace,
    /// The old owner did not release/exit within budget. Nothing was killed.
    OldOwnerStuck,
}

pub async fn restart_into_primary(old: Option<&InstanceFile>) -> RestartResult {
    restart_with_budget(old, RestartBudget::default()).await
}

async fn restart_with_budget(old: Option<&InstanceFile>, budget: RestartBudget) -> RestartResult {
    println!(
        "{}",
        t!(
            "Asking the running player to quit…",
            "실행 중인 플레이어에 종료를 요청하는 중…"
        )
    );
    match tokio::time::timeout(budget.quit_ack, remote::client::send(RemoteCommand::Quit)).await {
        Ok(Ok(_)) | Ok(Err(remote::client::ClientError::NoRunningInstance)) => {}
        Ok(Err(first_error)) => {
            // One retry after a beat: the owner may have been mid-teardown already.
            tokio::time::sleep(Duration::from_millis(500)).await;
            match tokio::time::timeout(budget.quit_ack, remote::client::send(RemoteCommand::Quit))
                .await
            {
                Ok(Ok(_)) | Ok(Err(remote::client::ClientError::NoRunningInstance)) => {}
                Ok(Err(_)) | Err(_) => {
                    tracing::warn!(error = ?first_error, "restart: quit request failed twice");
                    return RestartResult::OldOwnerStuck;
                }
            }
        }
        Err(_) => return RestartResult::OldOwnerStuck,
    }

    if !remote::await_primary_release(budget.socket_release).await {
        return RestartResult::OldOwnerStuck;
    }

    // The writer-lease guarantee: only process exit proves the old owner finished
    // flushing. Without a pid (foreign/absent descriptor) a fixed grace is the best
    // available approximation.
    match old.map(|instance| instance.app_pid) {
        Some(pid) => {
            if !await_process_exit(pid, budget.process_exit).await {
                return RestartResult::OldOwnerStuck;
            }
        }
        None => tokio::time::sleep(Duration::from_secs(1)).await,
    }

    match remote::bind_or_detect(false).await {
        remote::BindOutcome::Bound(server) => RestartResult::TookOver {
            remote: Some(server),
        },
        remote::BindOutcome::AlreadyRunning => RestartResult::LostRace,
        remote::BindOutcome::Unavailable => RestartResult::TookOver { remote: None },
    }
}

async fn await_process_exit(pid: u32, deadline: Duration) -> bool {
    poll_until(deadline, Duration::from_millis(250), move || {
        !process_is_alive(pid)
    })
    .await
}

fn process_is_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some()
}

/// Poll `probe` every `interval` until it returns true or `deadline` elapses.
/// Checks the probe first so an already-true condition never sleeps.
async fn poll_until(
    deadline: Duration,
    interval: Duration,
    mut probe: impl FnMut() -> bool,
) -> bool {
    let end = tokio::time::Instant::now() + deadline;
    loop {
        if probe() {
            return true;
        }
        if tokio::time::Instant::now() >= end {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn poll_until_returns_immediately_when_already_true() {
        let mut calls = 0;
        let hit = poll_until(Duration::from_secs(5), Duration::from_millis(250), || {
            calls += 1;
            true
        })
        .await;
        assert!(hit);
        assert_eq!(calls, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_until_gives_up_at_the_deadline() {
        let hit = poll_until(Duration::from_secs(2), Duration::from_millis(250), || false).await;
        assert!(!hit);
    }

    #[tokio::test(start_paused = true)]
    async fn poll_until_sees_a_late_flip() {
        let mut calls = 0;
        let hit = poll_until(Duration::from_secs(5), Duration::from_millis(250), || {
            calls += 1;
            calls >= 4
        })
        .await;
        assert!(hit);
        assert_eq!(calls, 4);
    }

    #[test]
    fn own_process_reads_as_alive() {
        assert!(process_is_alive(std::process::id()));
    }
}
