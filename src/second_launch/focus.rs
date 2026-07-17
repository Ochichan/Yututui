//! Best-effort "bring the running player's terminal to the front" ladder.
//!
//! Planning is pure ([`focus_plan`]) so unit tests pin the exact commands per OS without
//! spawning anything; [`run_ladder`] executes steps in order and stops at the first
//! success. Every step is a hint that may be stale (a recorded `$WINDOWID` can outlive
//! its window), so executors validate liveness where the platform allows and callers
//! always fall through to a notification when the ladder reports failure.
//!
//! Focus-stealing reality check: only the process the user just launched (us) holds a
//! focus grant, and even then Wayland compositors and the Windows foreground lock may
//! degrade activation to a taskbar/attention flash. That degraded outcome is accepted —
//! never fought with hacks.

use std::time::Duration;

use crate::remote::proto::HostTerminalHint;
use crate::util::process::{ProcessProfile, std_command};

/// How long a single ladder step may run. A hung `qdbus` must not stall the chooser.
const STEP_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusStep {
    /// Spawn `program args…`; exit status 0 counts as success.
    Exec {
        program: &'static str,
        args: Vec<String>,
    },
    /// Query `program args…`; success only if it exits 0 AND prints `expect_stdout`
    /// (trimmed). Used to detect "already visible" dropdown states without toggling.
    ExecExpect {
        program: &'static str,
        args: Vec<String>,
        expect_stdout: &'static str,
    },
    /// Win32 SetForegroundWindow resolution (see `focus_windows`).
    #[cfg(windows)]
    Win32Foreground {
        console_hwnd: Option<u64>,
        terminal_host_pid: Option<u32>,
        app_pid: Option<u32>,
    },
}

/// Build the per-OS ladder. Pure: no environment reads, no process spawns.
pub fn focus_plan(hint: Option<&HostTerminalHint>, app_pid: Option<u32>) -> Vec<FocusStep> {
    let Some(hint) = hint else {
        #[cfg(windows)]
        {
            // Old descriptors carry no hint, but the app pid alone can still find a window.
            return app_pid
                .map(|pid| {
                    vec![FocusStep::Win32Foreground {
                        console_hwnd: None,
                        terminal_host_pid: None,
                        app_pid: Some(pid),
                    }]
                })
                .unwrap_or_default();
        }
        #[cfg(not(windows))]
        {
            let _ = app_pid;
            return Vec::new();
        }
    };
    if hint.ssh == Some(true) {
        // The hosting terminal is on another machine; nothing local can focus it.
        return Vec::new();
    }
    platform_plan(hint, app_pid)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_plan(hint: &HostTerminalHint, _app_pid: Option<u32>) -> Vec<FocusStep> {
    let mut steps = Vec::new();
    if hint.konsole_dbus_service.as_deref() == Some("org.kde.yakuake") {
        // Yakuake only exposes a *toggle*. Probe visibility first: `true` means the
        // dropdown is already on screen — report success without hiding it. If the probe
        // itself fails (API drift), accept toggle semantics as documented best-effort.
        steps.push(FocusStep::ExecExpect {
            program: "qdbus",
            args: vec![
                "org.kde.yakuake".into(),
                "/yakuake/MainWindow_1".into(),
                "org.qtproject.Qt.QWidget.visible".into(),
            ],
            expect_stdout: "true",
        });
        steps.push(FocusStep::Exec {
            program: "qdbus",
            args: vec![
                "org.kde.yakuake".into(),
                "/yakuake/window".into(),
                "org.kde.yakuake.toggleWindowState".into(),
            ],
        });
    }
    if hint.guake_tab_uuid.is_some() {
        // `--show`, never toggle: repeated activations must not hide an open Guake.
        steps.push(FocusStep::Exec {
            program: "guake",
            args: vec!["--show".into()],
        });
    }
    if hint.x11 == Some(true)
        && let Some(windowid) = hint.windowid.as_deref()
    {
        steps.push(FocusStep::Exec {
            program: "wmctrl",
            args: vec!["-i".into(), "-a".into(), windowid.to_string()],
        });
        steps.push(FocusStep::Exec {
            program: "xdotool",
            args: vec!["windowactivate".into(), windowid.to_string()],
        });
    }
    // Wayland without a dropdown match: activation needs an xdg-activation token tied to
    // recent input in OUR window — which a bare launcher process cannot mint for another
    // surface. Fall through to the notification instead of fighting the compositor.
    steps
}

#[cfg(target_os = "macos")]
fn platform_plan(hint: &HostTerminalHint, _app_pid: Option<u32>) -> Vec<FocusStep> {
    let mut steps = Vec::new();
    if let Some(bundle) = hint
        .term_program
        .as_deref()
        .and_then(|program| terminal_bundle_id(program, hint.term.as_deref()))
    {
        steps.push(FocusStep::Exec {
            program: "open",
            args: vec!["-b".into(), bundle.to_string()],
        });
    }
    if hint.term_program.as_deref() == Some("iTerm.app") {
        // Hotkey-window reveal via AppleScript is version-fragile; plain activation plus
        // the notification is the reliable floor.
        steps.push(FocusStep::Exec {
            program: "osascript",
            args: vec![
                "-e".into(),
                r#"tell application "iTerm2" to activate"#.into(),
            ],
        });
    }
    steps
}

#[cfg(windows)]
fn platform_plan(hint: &HostTerminalHint, app_pid: Option<u32>) -> Vec<FocusStep> {
    // `wt -w _quake` is deliberately absent: it summons the quake window whether or not
    // the TUI lives there. The recorded HWND / host-pid resolution below focuses the
    // actual hosting window, quake-hosted sessions included.
    vec![FocusStep::Win32Foreground {
        console_hwnd: hint.console_hwnd,
        terminal_host_pid: hint.terminal_host_pid,
        app_pid,
    }]
}

/// Map `$TERM_PROGRAM` (plus `$TERM` for kitty, which sets no TERM_PROGRAM) to the
/// LaunchServices bundle id `open -b` activates.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn terminal_bundle_id(term_program: &str, term: Option<&str>) -> Option<&'static str> {
    match term_program {
        "Apple_Terminal" => Some("com.apple.Terminal"),
        "iTerm.app" => Some("com.googlecode.iterm2"),
        "ghostty" | "Ghostty" => Some("com.mitchellh.ghostty"),
        "WezTerm" => Some("com.github.wez.wezterm"),
        "WarpTerminal" => Some("dev.warp.Warp-Stable"),
        "kitty" => Some("net.kovidgoyal.kitty"),
        _ if term.is_some_and(|t| t.contains("kitty")) => Some("net.kovidgoyal.kitty"),
        _ => None,
    }
}

/// Execute steps in order; the first success wins.
pub fn run_ladder(steps: Vec<FocusStep>) -> bool {
    steps.into_iter().any(run_step)
}

fn run_step(step: FocusStep) -> bool {
    match step {
        FocusStep::Exec { program, args } => {
            run_bounded(program, &args).is_some_and(|(status_ok, _)| status_ok)
        }
        FocusStep::ExecExpect {
            program,
            args,
            expect_stdout,
        } => run_bounded(program, &args)
            .is_some_and(|(status_ok, stdout)| status_ok && stdout.trim() == expect_stdout),
        #[cfg(windows)]
        FocusStep::Win32Foreground {
            console_hwnd,
            terminal_host_pid,
            app_pid,
        } => super::focus_windows::bring_to_foreground(console_hwnd, terminal_host_pid, app_pid),
    }
}

/// Spawn with null stdin, captured stdout, and a hard kill at [`STEP_TIMEOUT`].
fn run_bounded(program: &str, args: &[String]) -> Option<(bool, String)> {
    let mut child = std_command(program, ProcessProfile::DesktopOpen)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + STEP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_string(&mut stdout);
                }
                return Some((status.success(), stdout));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hint() -> HostTerminalHint {
        HostTerminalHint::default()
    }

    #[test]
    fn ssh_sessions_and_missing_hints_produce_an_empty_plan_on_unix() {
        let ssh = HostTerminalHint {
            ssh: Some(true),
            windowid: Some("0x1".into()),
            x11: Some(true),
            ..hint()
        };
        assert!(focus_plan(Some(&ssh), Some(1)).is_empty());
        #[cfg(not(windows))]
        assert!(focus_plan(None, Some(1)).is_empty());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn yakuake_probe_precedes_toggle_and_windowid_steps() {
        let yakuake = HostTerminalHint {
            konsole_dbus_service: Some("org.kde.yakuake".into()),
            windowid: Some("0x2a".into()),
            x11: Some(true),
            ..hint()
        };
        let plan = focus_plan(Some(&yakuake), None);
        assert_eq!(plan.len(), 4);
        assert!(matches!(
            &plan[0],
            FocusStep::ExecExpect {
                program: "qdbus",
                expect_stdout: "true",
                ..
            }
        ));
        assert!(
            matches!(&plan[1], FocusStep::Exec { program: "qdbus", args }
            if args.iter().any(|a| a.contains("toggleWindowState")))
        );
        assert!(
            matches!(&plan[2], FocusStep::Exec { program: "wmctrl", args }
            if args == &vec!["-i".to_string(), "-a".to_string(), "0x2a".to_string()])
        );
        assert!(matches!(
            &plan[3],
            FocusStep::Exec {
                program: "xdotool",
                ..
            }
        ));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn guake_uses_show_never_toggle_and_wayland_skips_x11_tools() {
        let guake_on_wayland = HostTerminalHint {
            guake_tab_uuid: Some("uuid".into()),
            windowid: Some("0x2a".into()),
            wayland: Some(true),
            ..hint()
        };
        let plan = focus_plan(Some(&guake_on_wayland), None);
        assert_eq!(
            plan,
            vec![FocusStep::Exec {
                program: "guake",
                args: vec!["--show".into()],
            }],
            "wayland without x11 must not plan wmctrl/xdotool; guake must use --show"
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn plain_konsole_relies_on_windowid_not_bespoke_dbus() {
        let konsole = HostTerminalHint {
            konsole_dbus_service: Some("org.kde.konsole-1234".into()),
            windowid: Some("0x99".into()),
            x11: Some(true),
            ..hint()
        };
        let plan = focus_plan(Some(&konsole), None);
        assert!(
            plan.iter().all(|step| !matches!(
                step,
                FocusStep::Exec {
                    program: "qdbus",
                    ..
                }
            )),
            "only yakuake gets a qdbus step"
        );
        assert_eq!(plan.len(), 2);
    }

    #[test]
    fn macos_bundle_map_covers_known_terminals() {
        assert_eq!(
            terminal_bundle_id("Apple_Terminal", None),
            Some("com.apple.Terminal")
        );
        assert_eq!(
            terminal_bundle_id("iTerm.app", None),
            Some("com.googlecode.iterm2")
        );
        assert_eq!(
            terminal_bundle_id("WezTerm", None),
            Some("com.github.wez.wezterm")
        );
        assert_eq!(
            terminal_bundle_id("", Some("xterm-kitty")),
            Some("net.kovidgoyal.kitty")
        );
        assert_eq!(terminal_bundle_id("SomethingNew", None), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_plan_is_a_single_resolution_step_even_without_a_hint() {
        let with_hint = HostTerminalHint {
            console_hwnd: Some(42),
            terminal_host_pid: Some(7),
            ..hint()
        };
        assert_eq!(
            focus_plan(Some(&with_hint), Some(99)),
            vec![FocusStep::Win32Foreground {
                console_hwnd: Some(42),
                terminal_host_pid: Some(7),
                app_pid: Some(99),
            }]
        );
        assert_eq!(
            focus_plan(None, Some(99)),
            vec![FocusStep::Win32Foreground {
                console_hwnd: None,
                terminal_host_pid: None,
                app_pid: Some(99),
            }]
        );
    }
}
