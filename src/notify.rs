//! Desktop notifications for recording events.
//!
//! Tiered on purpose. For a TUI the best transport is an **OSC 9 / OSC 777 escape sequence**:
//! the terminal emulator posts the OS notification, so it's zero-dependency, works on all three
//! OSes, is attributed to the terminal (not "Script Editor"/"Finder"), already has the terminal's
//! notification permission, and supports click-to-focus. When the terminal doesn't advertise OSC
//! support we fall back to a native [`notify-rust`] toast on a background thread (`show()` blocks).
//! The in-app status toast (set in the recorder reducer) is the final fallback and always shows.
//!
//! Detection is env-based and done once at startup; emission is best-effort and never surfaces an
//! error to the caller.

use std::io::Write;

/// Which OSC notification form the running terminal understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Osc {
    /// `ESC ] 9 ; <text> BEL` — iTerm2, Ghostty, WezTerm, Warp, Apple Terminal, Windows Terminal.
    Nine,
    /// `ESC ] 777 ; notify ; <title> ; <body> BEL` — kitty, WezTerm, VTE (GNOME Terminal, …).
    SevenSevenSeven,
    /// No known OSC notification support → use the native fallback.
    None,
}

/// Terminal notification capability, detected once at startup and cheap to copy.
#[derive(Clone, Copy, Debug)]
pub struct Notifier {
    osc: Osc,
    /// Inside tmux/screen: OSC must be wrapped in the DCS passthrough (and needs
    /// `set -g allow-passthrough on`) or it never reaches the host terminal.
    tmux: bool,
}

impl Notifier {
    /// Detect terminal notification support from environment variables only (no I/O, no probing).
    pub fn detect() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let term = env("TERM").unwrap_or_default();
        let term_program = env("TERM_PROGRAM").unwrap_or_default();
        let tmux = env("TMUX").is_some() || term.starts_with("screen") || term.starts_with("tmux");

        // kitty, WezTerm, and VTE terminals (GNOME Terminal, Tilix, …) speak OSC 777; iTerm2,
        // Ghostty, Warp, Apple Terminal, and Windows Terminal speak OSC 9.
        let osc = if env("KITTY_WINDOW_ID").is_some()
            || term.contains("kitty")
            || term_program == "WezTerm"
            || env("WEZTERM_PANE").is_some()
            || env("VTE_VERSION").is_some()
        {
            Osc::SevenSevenSeven
        } else if env("WT_SESSION").is_some()
            || matches!(
                term_program.as_str(),
                "iTerm.app" | "ghostty" | "Ghostty" | "WarpTerminal" | "Apple_Terminal"
            )
        {
            Osc::Nine
        } else {
            Osc::None
        };
        Self { osc, tmux }
    }

    /// Fire a desktop notification for `(title, body)`. Best-effort. The OSC path writes a handful
    /// of bytes to the shared stdout, so call this **between frames** (the main loop's command
    /// dispatch, never mid-draw); the native fallback runs on its own thread and never blocks.
    pub fn emit(&self, title: &str, body: &str) {
        match self.osc {
            Osc::Nine | Osc::SevenSevenSeven => self.emit_osc(title, body),
            Osc::None => emit_native(title.to_owned(), body.to_owned()),
        }
    }

    fn emit_osc(&self, title: &str, body: &str) {
        let title = sanitize(title);
        let body = sanitize(body);
        let seq = match self.osc {
            // OSC 9 carries a single string; fold the title in so it isn't lost.
            Osc::Nine if title.is_empty() => format!("\x1b]9;{body}\x07"),
            Osc::Nine => format!("\x1b]9;{title}: {body}\x07"),
            Osc::SevenSevenSeven => format!("\x1b]777;notify;{title};{body}\x07"),
            Osc::None => return,
        };
        let seq = if self.tmux {
            // tmux passthrough: wrap in DCS and double every ESC in the payload.
            format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
        } else {
            seq
        };
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(seq.as_bytes());
        let _ = out.flush();
    }
}

/// Drop control bytes (BEL/ESC/other C0) that would prematurely terminate an OSC string.
fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Native OS toast via `notify-rust`, off the caller's thread (`show()` blocks on every OS) and
/// inside `catch_unwind` (the macOS `NSUserNotification` backend can panic on some OS versions —
/// contain it so a failed notification can never take down the TUI). Best-effort.
fn emit_native(title: String, body: String) {
    std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut n = notify_rust::Notification::new();
            n.appname("YuTuTui!").summary(&title).body(&body);
            #[cfg(target_os = "windows")]
            {
                // Attribute the toast to us (matches the SMTC registration) instead of PowerShell.
                n.app_id(crate::media::identity::APP_USER_MODEL_ID);
            }
            let _ = n.show();
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct EnvRestore(Vec<(String, Option<OsString>)>);

    impl EnvRestore {
        fn capture(names: &[&str]) -> Self {
            Self(
                names
                    .iter()
                    .map(|name| ((*name).to_owned(), std::env::var_os(name)))
                    .collect(),
            )
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    fn clear_env(names: &[&str]) {
        for name in names {
            unsafe { std::env::remove_var(name) };
        }
    }

    #[test]
    fn detect_maps_terminal_environment_to_osc_capability() {
        let _guard = env_lock();
        const NAMES: &[&str] = &[
            "TERM",
            "TERM_PROGRAM",
            "TMUX",
            "KITTY_WINDOW_ID",
            "WEZTERM_PANE",
            "VTE_VERSION",
            "WT_SESSION",
        ];
        let _restore = EnvRestore::capture(NAMES);
        clear_env(NAMES);

        let none = Notifier::detect();
        assert_eq!(none.osc, Osc::None);
        assert!(!none.tmux);

        unsafe { std::env::set_var("KITTY_WINDOW_ID", "1") };
        assert_eq!(Notifier::detect().osc, Osc::SevenSevenSeven);
        unsafe { std::env::remove_var("KITTY_WINDOW_ID") };

        unsafe { std::env::set_var("TERM_PROGRAM", "WezTerm") };
        assert_eq!(Notifier::detect().osc, Osc::SevenSevenSeven);
        unsafe { std::env::set_var("TERM_PROGRAM", "Apple_Terminal") };
        assert_eq!(Notifier::detect().osc, Osc::Nine);

        unsafe { std::env::remove_var("TERM_PROGRAM") };
        unsafe { std::env::set_var("WT_SESSION", "abc") };
        assert_eq!(Notifier::detect().osc, Osc::Nine);

        unsafe { std::env::remove_var("WT_SESSION") };
        unsafe { std::env::set_var("TERM", "screen-256color") };
        let tmux = Notifier::detect();
        assert_eq!(tmux.osc, Osc::None);
        assert!(tmux.tmux);
    }

    #[test]
    fn sanitize_strips_control_bytes_but_keeps_text() {
        assert_eq!(sanitize("Title\x1b]9;evil\x07\nBody"), "Title]9;evilBody");
        assert_eq!(sanitize("plain text"), "plain text");
    }
}
