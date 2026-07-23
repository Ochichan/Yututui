//! Process-signal ownership acquired before any interactive terminal mode change.

use crate::player::lifetime::{self, ShutdownLatch};
use crate::util::background_task::BackgroundTask;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
struct InitialTerminalSnapshot {
    input: rustix::fd::OwnedFd,
    termios: rustix::termios::Termios,
}
#[cfg(unix)]
type InitialTerminalState = Option<InitialTerminalSnapshot>;
#[cfg(not(unix))]
type InitialTerminalState = ();

/// Registered termination streams plus the out-of-band shutdown latch they drive.
///
/// Construction happens before the graphics probe or TUI enters raw mode. Dropping the value
/// aborts the owned task, while [`Self::shutdown`] also joins that cancellation on orderly exits.
pub struct InteractiveSignals {
    shutdown: ShutdownLatch,
    handlers: BackgroundTask,
    mouse: Arc<AtomicBool>,
    query_cancellation: ratatui_image::picker::QueryCancellation,
}

impl InteractiveSignals {
    // `InitialTerminalState` is `()` off Unix, so the captured state is a unit binding there.
    #[cfg_attr(not(unix), allow(clippy::let_unit_value))]
    pub fn install() -> std::io::Result<Self> {
        let shutdown = ShutdownLatch::new();
        let mouse = Arc::new(AtomicBool::new(false));
        let hard_exit_mouse = Arc::clone(&mouse);
        let query_cancellation = ratatui_image::picker::QueryCancellation::new();
        let hard_exit_query_cancellation = query_cancellation.clone();
        let initial_terminal_state = capture_initial_terminal_state();
        let handlers = lifetime::spawn_signal_handlers(
            shutdown.clone(),
            |_| {},
            move |code| {
                hard_exit_after_second_signal(
                    hard_exit_mouse.load(Ordering::Acquire),
                    code,
                    initial_terminal_state,
                    hard_exit_query_cancellation,
                )
            },
        )?;
        Ok(Self {
            shutdown,
            handlers,
            mouse,
            query_cancellation,
        })
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.is_triggered()
    }

    pub fn set_mouse(&self, mouse: bool) {
        self.mouse.store(mouse, Ordering::Release);
    }

    pub fn shutdown_latch(&self) -> ShutdownLatch {
        self.shutdown.clone()
    }

    pub fn query_cancellation(&self) -> ratatui_image::picker::QueryCancellation {
        self.query_cancellation.clone()
    }

    pub async fn shutdown(mut self) {
        self.handlers.shutdown().await;
    }
}

#[cfg(unix)]
fn capture_initial_terminal_state() -> InitialTerminalState {
    use std::os::fd::AsFd as _;

    let stdin = std::io::stdin();
    let input = if rustix::termios::isatty(&stdin) {
        stdin.as_fd().try_clone_to_owned().ok()?
    } else {
        rustix::fs::openat(
            rustix::fs::CWD,
            c"/dev/tty",
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOCTTY,
            rustix::fs::Mode::empty(),
        )
        .ok()?
    };
    let termios = rustix::termios::tcgetattr(&input).ok()?;
    Some(InitialTerminalSnapshot { input, termios })
}

#[cfg(not(unix))]
fn capture_initial_terminal_state() -> InitialTerminalState {}

#[cfg_attr(not(unix), allow(clippy::let_unit_value))]
fn hard_exit_after_second_signal(
    mouse: bool,
    code: i32,
    initial_terminal_state: InitialTerminalState,
    query_cancellation: ratatui_image::picker::QueryCancellation,
) {
    let hard_exit_started = std::time::Instant::now();
    let hard_exit_deadline = hard_exit_started
        .checked_add(crate::terminal_policy::HARD_EXIT_TOTAL_TIMEOUT)
        .unwrap_or(hard_exit_started);
    let query_fence_deadline = hard_exit_deadline
        .checked_sub(crate::terminal_policy::EMERGENCY_RESTORE_TIMEOUT)
        .unwrap_or(hard_exit_deadline);

    #[cfg(unix)]
    crate::tui::inhibit_raw_mode_transitions();
    query_cancellation.cancel();
    lifetime::kill_mpv_now();

    #[cfg(unix)]
    {
        // Keep exact restoration under the query admission fence. A worker paused immediately
        // before raw tcsetattr therefore finishes first, or the hard deadline exits without an
        // unsafe unfenced restore which that worker could overwrite afterwards.
        let _ = run_hard_exit_restore_until(hard_exit_deadline, move || {
            let _ =
                query_cancellation.run_cancelled_exclusive_until(query_fence_deadline, move || {
                    crate::tui::with_raw_mode_transition(move || {
                        if let Some(initial) = initial_terminal_state {
                            // Pre-TUI scopes do not publish a TUI writer. Restore the exact
                            // inherited mode first; the outer deadline still forces process exit
                            // if this ioctl wedges.
                            let _ = rustix::termios::tcsetattr(
                                &initial.input,
                                rustix::termios::OptionalActions::Now,
                                &initial.termios,
                            );
                        }
                        let _ = crate::tui::emergency_restore(mouse);
                    });
                });
        });
    }

    #[cfg(not(unix))]
    {
        let _ = initial_terminal_state;
        // Native console restore is ordered by the same query fence and waited under the same
        // single absolute deadline. The signal task never blocks on an unbounded console call.
        let _ = run_hard_exit_restore_until(hard_exit_deadline, move || {
            let _ =
                query_cancellation.run_cancelled_exclusive_until(query_fence_deadline, move || {
                    let _ = crate::tui::restore(mouse);
                });
        });
    }

    std::process::exit(code);
}

fn run_hard_exit_restore_until(
    deadline: std::time::Instant,
    restore: impl FnOnce() + Send + 'static,
) -> bool {
    if std::time::Instant::now() >= deadline {
        return false;
    }
    let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("signal hard-exit terminal restore".to_owned())
        .spawn(move || {
            restore();
            let _ = finished_tx.send(());
        })
        .is_ok()
    {
        return finished_rx
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            .is_ok();
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::run_hard_exit_restore_until;
    use std::time::{Duration, Instant};

    #[test]
    fn hard_exit_restore_wait_is_bounded_even_when_termios_never_returns_in_time() {
        let started = Instant::now();
        let completed = run_hard_exit_restore_until(started + Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_millis(500))
        });
        assert!(!completed);
        assert!(
            started.elapsed() < Duration::from_millis(350),
            "repeat-signal restore exceeded its hard-exit budget"
        );
    }
}
