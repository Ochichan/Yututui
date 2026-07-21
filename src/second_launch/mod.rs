//! What a second interactive `ytt` launch does instead of silently bowing out.
//!
//! The single-instance guard is the remote socket (`remote::bind_or_detect`). Historically
//! a second launch printed [`ALREADY_RUNNING_NOTICE`] and exited — which, from a desktop
//! icon, looks like the app refusing to start (the launcher terminal flashes and closes;
//! the macOS `.app` runs with no terminal at all, so the notice lands nowhere). This module
//! branches on how the process was launched:
//!
//! - both stdin and stdout are ttys → an interactive chooser in that terminal
//!   ([`chooser`]): focus the running player, restart in place, open a read-only second
//!   player, or quit;
//! - no stdio is a tty (Finder/`.app`/GUI launches) → best-effort focus of the hosting
//!   terminal ([`focus`]) plus a desktop notification ([`notify_fallback`]);
//! - anything piped/mixed → exactly the old one-line notice.
//!
//! Everything here runs BEFORE persistence initialization and terminal setup — the same
//! ordering contract as the guard itself (`src/main.rs`), so nothing may write config or
//! touch the alternate screen.

pub mod chooser;
pub mod focus;
#[cfg(windows)]
pub mod focus_windows;
pub mod notify_fallback;
pub mod restart;

use crate::remote;
use crate::remote::proto::InstanceFile;
use crate::t;

pub const ALREADY_RUNNING_NOTICE: &str = "ytt is already running.\n  \
                                          Control it:  ytt -r <command>   (e.g. `ytt -r pp`, `ytt -r next`)\n  \
                                          Stop it:     ytt -r quit";

/// How a second launch should present itself, decided purely from stdio shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondLaunchUx {
    /// A human is looking at this terminal: render the chooser in it.
    InteractiveChooser,
    /// No terminal anywhere (Finder/.app/GUI launch): focus + desktop notification.
    FocusAndNotify,
    /// Piped or redirected: keep the historical plain notice on stderr.
    NoticeOnly,
}

pub fn classify(stdin_tty: bool, stdout_tty: bool, stderr_tty: bool) -> SecondLaunchUx {
    match (stdin_tty, stdout_tty, stderr_tty) {
        (true, true, _) => SecondLaunchUx::InteractiveChooser,
        (false, false, false) => SecondLaunchUx::FocusAndNotify,
        _ => SecondLaunchUx::NoticeOnly,
    }
}

/// What `async_main` does after the guard said `AlreadyRunning`.
pub enum Outcome {
    Exit,
    /// Continue into normal startup with this (possibly absent) remote server.
    /// `read_only` mirrors `--new-instance` persistence semantics.
    Continue {
        remote: Option<Box<remote::RemoteServer>>,
        read_only: bool,
    },
}

/// Entry point for the `AlreadyRunning` branch of the interactive TUI launch.
///
/// Daemon, one-shot, and `-r` verbs never reach this: they are dispatched in `run()`
/// before `async_main`.
pub async fn handle_already_running(shutdown: &crate::player::lifetime::ShutdownLatch) -> Outcome {
    // Strict reader: a missing/foreign descriptor only downgrades the focus ladder and
    // the notification location text, never the flow itself.
    let instance = remote::endpoint::read_current_instance().ok();
    // The chooser renders before Config::load may run, so peek the saved language with a
    // read-only probe; failures fall back to English.
    crate::i18n::set_language(crate::config::peek_saved_language());

    use std::io::IsTerminal;
    let ux = classify(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    );
    match ux {
        SecondLaunchUx::NoticeOnly => {
            eprintln!("{ALREADY_RUNNING_NOTICE}");
            Outcome::Exit
        }
        SecondLaunchUx::FocusAndNotify => {
            // Fully-redirected launches are also (false, false, false) — a cron job or
            // systemd unit must keep the old inert contract, not steal focus and toast.
            // A GUI launcher always runs inside a graphical session; on unix that means
            // DISPLAY/WAYLAND_DISPLAY is set (macOS aside, where Aqua needs no env).
            if has_gui_session() {
                let focused = run_focus(instance.as_ref());
                // Always notify: even a successful silent focus should tell the user why
                // no new player appeared. Keep the stderr notice too — it may be a log.
                notify_fallback::notify_already_running(instance.as_ref(), focused);
            }
            eprintln!("{ALREADY_RUNNING_NOTICE}");
            Outcome::Exit
        }
        SecondLaunchUx::InteractiveChooser => run_chooser(instance.as_ref(), shutdown).await,
    }
}

fn has_gui_session() -> bool {
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        set("DISPLAY") || set("WAYLAND_DISPLAY")
    }
    #[cfg(any(not(unix), target_os = "macos"))]
    {
        true
    }
}

async fn run_chooser(
    instance: Option<&InstanceFile>,
    shutdown: &crate::player::lifetime::ShutdownLatch,
) -> Outcome {
    let owner = match instance.map(|instance| instance.mode) {
        Some(crate::remote::proto::InstanceMode::Daemon) => chooser::OwnerKind::Daemon,
        // A TUI owner or an unreadable descriptor: offer the full menu (focus degrades
        // gracefully to the printed notice when there is nothing to focus).
        _ => chooser::OwnerKind::Tui,
    };
    let prompt_shutdown = shutdown.clone();
    let choice = tokio::task::spawn_blocking(move || {
        chooser::prompt(chooser::CHOOSER_TIMEOUT, owner, &prompt_shutdown)
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or(chooser::Choice::Quit);
    if shutdown.is_triggered() {
        return Outcome::Exit;
    }
    match choice {
        chooser::Choice::Focus => {
            if run_focus(instance) {
                let _ = chooser::write_status(
                    t!(
                        "Switched to the running player.",
                        "실행 중인 플레이어로 전환했습니다.",
                        "実行中のプレイヤーに切り替えました。"
                    )
                    .to_owned(),
                    shutdown,
                )
                .await;
            } else {
                let _ = chooser::write_status(
                    format!(
                        "{}\n{ALREADY_RUNNING_NOTICE}",
                        t!(
                            "Could not switch to the running player.",
                            "실행 중인 플레이어로 전환하지 못했습니다.",
                            "実行中のプレイヤーに切り替えられませんでした。"
                        )
                    ),
                    shutdown,
                )
                .await;
            }
            Outcome::Exit
        }
        chooser::Choice::Restart => {
            let _ = chooser::write_status(
                t!(
                    "Asking the running player to quit…",
                    "실행 중인 플레이어에 종료를 요청하는 중…",
                    "実行中のプレイヤーに終了を要求しています…"
                )
                .to_owned(),
                shutdown,
            )
            .await;
            if shutdown.is_triggered() {
                return Outcome::Exit;
            }
            match tokio::select! {
                _ = shutdown.wait() => return Outcome::Exit,
                result = restart::restart_into_primary(instance) => result,
            } {
                restart::RestartResult::TookOver { remote } => Outcome::Continue {
                    remote,
                    read_only: false,
                },
                restart::RestartResult::LostRace => {
                    let _ = chooser::write_status(
                        t!(
                            "Another player took over first; leaving it in control.",
                            "다른 플레이어가 먼저 시작되어 그쪽에 제어를 넘깁니다.",
                            "別のプレイヤーが先に引き継いだため、そちらに制御を任せます。"
                        )
                        .to_owned(),
                        shutdown,
                    )
                    .await;
                    Outcome::Exit
                }
                restart::RestartResult::OldOwnerStuck => {
                    let _ = chooser::write_status(
                        t!(
                            "The running player did not exit. Close it manually and run ytt again.",
                            "실행 중인 플레이어가 종료되지 않았습니다. 직접 종료한 뒤 ytt를 다시 실행하세요.",
                            "実行中のプレイヤーが終了しませんでした。手動で終了してから、yttを再度実行してください。"
                        )
                        .to_owned(),
                        shutdown,
                    )
                    .await;
                    Outcome::Exit
                }
            }
        }
        chooser::Choice::NewInstance => {
            // The explicit `--new-instance` path: bind a private pid-qualified endpoint and
            // stay a strict read-only observer (never promoted to the writer).
            let outcome = tokio::select! {
                _ = shutdown.wait() => return Outcome::Exit,
                outcome = remote::bind_or_detect(true) => outcome,
            };
            let remote = match outcome {
                remote::BindOutcome::Bound(server) => Some(server),
                _ => None,
            };
            Outcome::Continue {
                remote,
                read_only: true,
            }
        }
        chooser::Choice::Quit => Outcome::Exit,
    }
}

fn run_focus(instance: Option<&InstanceFile>) -> bool {
    let Some(instance) = instance else {
        return false;
    };
    let plan = focus::focus_plan(instance.host_terminal.as_ref(), Some(instance.app_pid));
    focus::run_ladder(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_running_notice_keeps_controls_without_advertising_new_instance() {
        assert!(ALREADY_RUNNING_NOTICE.contains("Control it:"));
        assert!(ALREADY_RUNNING_NOTICE.contains("ytt -r <command>"));
        assert!(ALREADY_RUNNING_NOTICE.contains("Stop it:"));
        assert!(ALREADY_RUNNING_NOTICE.contains("ytt -r quit"));
        assert!(!ALREADY_RUNNING_NOTICE.contains("--new-instance"));
    }

    #[test]
    fn classification_covers_every_stdio_shape() {
        // A human-visible terminal needs both a readable stdin and a writable stdout.
        assert_eq!(
            classify(true, true, true),
            SecondLaunchUx::InteractiveChooser
        );
        assert_eq!(
            classify(true, true, false),
            SecondLaunchUx::InteractiveChooser
        );
        // GUI launches have no terminal anywhere.
        assert_eq!(
            classify(false, false, false),
            SecondLaunchUx::FocusAndNotify
        );
        // Every piped/mixed shape keeps the historical notice.
        assert_eq!(classify(true, false, true), SecondLaunchUx::NoticeOnly);
        assert_eq!(classify(true, false, false), SecondLaunchUx::NoticeOnly);
        assert_eq!(classify(false, true, true), SecondLaunchUx::NoticeOnly);
        assert_eq!(classify(false, true, false), SecondLaunchUx::NoticeOnly);
        assert_eq!(classify(false, false, true), SecondLaunchUx::NoticeOnly);
    }
}
