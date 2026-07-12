use std::collections::VecDeque;

use super::teardown::{OwnerIngressDrain, OwnerTeardown, complete_owner_teardown};
use super::*;

#[derive(Default)]
struct RecordingTeardown {
    steps: Vec<&'static str>,
    background_outcomes: VecDeque<runtime::BackgroundShutdown>,
    ingress_drain: OwnerIngressDrain,
    fail_scrobble: bool,
}

impl OwnerTeardown for RecordingTeardown {
    fn quiesce_remote(&mut self) {
        self.steps.push("remote_quiesce");
    }

    fn retire_player(&mut self) {
        self.steps.push("player");
    }

    fn close_ingress(&mut self) {
        self.steps.push("ingress");
    }

    fn deactivate_media(&mut self) {
        self.steps.push("media");
    }

    async fn drain_owner_ingress(&mut self) -> OwnerIngressDrain {
        self.steps.push("ingress_drain");
        self.ingress_drain
    }

    async fn await_remote_reply_flush(&mut self) {
        self.steps.push("remote_reply_flush");
    }

    async fn shutdown_remote(&mut self) {
        self.steps.push("remote");
    }

    async fn reap_player_startup(&mut self) {
        self.steps.push("startup");
    }

    fn close_video(&mut self) {
        self.steps.push("video");
    }

    async fn shutdown_terminal_background(&mut self) {
        self.steps.push("terminal_background");
    }

    async fn shutdown_resolver(&mut self) {
        self.steps.push("resolver");
    }

    async fn shutdown_runtime_background(&mut self) -> runtime::BackgroundShutdown {
        self.steps.push("runtime_background");
        self.background_outcomes
            .pop_front()
            .unwrap_or(runtime::BackgroundShutdown::Drained)
    }

    async fn shutdown_transfer(&mut self) {
        self.steps.push("transfer");
    }

    async fn shutdown_downloads(&mut self) {
        self.steps.push("downloads");
    }

    async fn finalize_runtime_background(&mut self) {
        self.steps.push("runtime_finalize");
    }

    async fn flush_persistence(&mut self) -> Result<()> {
        self.steps.push("persistence");
        Ok(())
    }

    async fn shutdown_scrobble(&mut self) -> Result<()> {
        self.steps.push("scrobble");
        if self.fail_scrobble {
            anyhow::bail!("injected scrobble durability failure");
        }
        Ok(())
    }
}

#[tokio::test]
async fn accepted_remote_requests_flush_before_the_hub_is_shut_down() {
    let mut teardown = RecordingTeardown {
        ingress_drain: OwnerIngressDrain {
            remote_requests: 1,
            subscribe_requests: 1,
        },
        ..Default::default()
    };

    complete_owner_teardown(&mut teardown, None)
        .await
        .expect("remote settlement flush is part of normal teardown");

    let quiesce = teardown
        .steps
        .iter()
        .position(|step| *step == "remote_quiesce")
        .unwrap();
    let player = teardown
        .steps
        .iter()
        .position(|step| *step == "player")
        .unwrap();
    let drain = teardown
        .steps
        .iter()
        .position(|step| *step == "ingress_drain")
        .unwrap();
    let flush = teardown
        .steps
        .iter()
        .position(|step| *step == "remote_reply_flush")
        .unwrap();
    let remote = teardown
        .steps
        .iter()
        .position(|step| *step == "remote")
        .unwrap();
    assert!(quiesce < player && player < drain && drain < flush && flush < remote);
}

#[tokio::test]
async fn owner_draw_error_runs_the_full_barrier_before_returning_the_original_error() {
    let mut owner_error = None;
    let injected = std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "injected owner-loop draw failure",
    );

    assert!(capture_owner_io_result(Err::<bool, _>(injected), &mut owner_error).is_none());
    let mut teardown = RecordingTeardown::default();
    let returned = complete_owner_teardown(&mut teardown, owner_error)
        .await
        .expect_err("injected owner-loop error must be returned after teardown");

    assert_eq!(
        teardown.steps,
        [
            "remote_quiesce",
            "player",
            "ingress",
            "media",
            "ingress_drain",
            "remote_reply_flush",
            "remote",
            "startup",
            "video",
            "terminal_background",
            "resolver",
            "runtime_background",
            "transfer",
            "downloads",
            "runtime_finalize",
            "persistence",
            "scrobble",
        ]
    );
    let returned_io = returned
        .downcast_ref::<std::io::Error>()
        .expect("the original I/O error type is preserved");
    assert_eq!(returned_io.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(returned_io.to_string(), "injected owner-loop draw failure");
}

#[tokio::test]
async fn background_timeout_is_reaped_again_before_persistence_flush() {
    let first = runtime::BackgroundShutdown::TimedOut {
        blocking_remaining: 1,
        cancellable_remaining: 0,
    };
    let mut teardown = RecordingTeardown {
        background_outcomes: VecDeque::from([first, runtime::BackgroundShutdown::Drained]),
        ..Default::default()
    };

    complete_owner_teardown(&mut teardown, None)
        .await
        .expect("the second wait drained the tracked blocking job");

    assert_eq!(
        teardown.steps,
        [
            "remote_quiesce",
            "player",
            "ingress",
            "media",
            "ingress_drain",
            "remote_reply_flush",
            "remote",
            "startup",
            "video",
            "terminal_background",
            "resolver",
            "runtime_background",
            "transfer",
            "downloads",
            "runtime_background",
            "runtime_finalize",
            "persistence",
            "scrobble",
        ]
    );
}

#[tokio::test]
async fn repeated_background_timeout_still_joins_without_a_final_deadline_before_flush() {
    let first = runtime::BackgroundShutdown::TimedOut {
        blocking_remaining: 2,
        cancellable_remaining: 1,
    };
    let retry = runtime::BackgroundShutdown::TimedOut {
        blocking_remaining: 1,
        cancellable_remaining: 0,
    };
    let mut teardown = RecordingTeardown {
        background_outcomes: VecDeque::from([first, retry]),
        ..Default::default()
    };

    complete_owner_teardown(&mut teardown, None)
        .await
        .expect("the final ownership barrier is not bounded by either diagnostic timeout");
    assert_eq!(
        teardown.steps,
        [
            "remote_quiesce",
            "player",
            "ingress",
            "media",
            "ingress_drain",
            "remote_reply_flush",
            "remote",
            "startup",
            "video",
            "terminal_background",
            "resolver",
            "runtime_background",
            "transfer",
            "downloads",
            "runtime_background",
            "runtime_finalize",
            "persistence",
            "scrobble",
        ],
        "persistence cannot start before the unbounded final ownership barrier"
    );
}

#[tokio::test]
async fn scrobble_durability_failure_is_returned_after_the_full_barrier() {
    let mut teardown = RecordingTeardown {
        fail_scrobble: true,
        ..Default::default()
    };

    let error = complete_owner_teardown(&mut teardown, None)
        .await
        .expect_err("an unconfirmed scrobble frontier cannot look like a clean terminal exit");
    assert!(
        error
            .to_string()
            .contains("injected scrobble durability failure")
    );
    assert_eq!(teardown.steps.last(), Some(&"scrobble"));
}
