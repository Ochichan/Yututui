//! Terminal keyboard capability classification.
//!
//! This module is deliberately environment-only. Terminal queries and mode activation live in
//! [`crate::tui`], before the exclusive terminal event worker starts reading input.

use std::ffi::OsString;

const KONSOLE_WIN32_INPUT_MIN_VERSION: u32 = 260_400;

/// How faithfully the active terminal path reports otherwise-ambiguous control keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyboardInputMode {
    /// The platform input API already reports physical keys distinctly (the Windows console).
    #[default]
    Native,
    /// Kitty's progressive keyboard protocol is active.
    Kitty,
    /// DEC private mode 9001 (Win32-input escape sequences) is active.
    Win32Input,
    /// The terminal can collapse physical Ctrl+Backspace and Ctrl+H to the same byte.
    Legacy,
}

impl KeyboardInputMode {
    pub const fn is_legacy(self) -> bool {
        matches!(self, Self::Legacy)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProtocolOverride {
    #[default]
    Auto,
    On,
    Off,
}

impl ProtocolOverride {
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("1" | "true" | "on") => Self::On,
            Some("0" | "false" | "off") => Self::Off,
            _ => Self::Auto,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct KeyboardEnvironment {
    term: String,
    term_program: String,
    kitty_window: bool,
    wezterm: bool,
    konsole: bool,
    konsole_version: Option<u32>,
    windows_terminal: bool,
    indirect: bool,
    kitty_override: ProtocolOverride,
    win32_override: ProtocolOverride,
}

impl KeyboardEnvironment {
    fn capture() -> Self {
        Self::from_lookup(|key| std::env::var_os(key))
    }

    fn from_lookup(mut get: impl FnMut(&str) -> Option<OsString>) -> Self {
        let string = |value: Option<OsString>| {
            value
                .map(|value| value.to_string_lossy().trim().to_ascii_lowercase())
                .unwrap_or_default()
        };
        let nonempty = |value: Option<OsString>| value.is_some_and(|value| !value.is_empty());

        let term = string(get("TERM"));
        let term_program = string(get("TERM_PROGRAM"));
        let konsole_value = string(get("KONSOLE_VERSION"));
        let konsole_version = konsole_value.parse::<u32>().ok();
        let konsole = !konsole_value.is_empty() || term.contains("konsole");
        let kitty_override = string(get("YTM_TUI_KEYBOARD_ENHANCEMENT"));
        let win32_override = string(get("YTM_TUI_WIN32_INPUT"));

        Self {
            term,
            term_program,
            kitty_window: nonempty(get("KITTY_WINDOW_ID")),
            wezterm: nonempty(get("WEZTERM_EXECUTABLE")),
            konsole,
            konsole_version,
            windows_terminal: nonempty(get("WT_SESSION")),
            indirect: [
                "TMUX",
                "STY",
                "ZELLIJ",
                "ZELLIJ_SESSION_NAME",
                "SSH_CONNECTION",
                "SSH_CLIENT",
                "SSH_TTY",
            ]
            .into_iter()
            .any(|key| nonempty(get(key))),
            kitty_override: ProtocolOverride::parse(Some(&kitty_override)),
            win32_override: ProtocolOverride::parse(Some(&win32_override)),
        }
    }

    fn known_kitty_terminal(&self) -> bool {
        ["kitty", "ghostty", "foot", "alacritty", "wezterm"]
            .into_iter()
            .any(|name| self.term.contains(name) || self.term_program.contains(name))
            || self.term_program.contains("iterm")
            || self.kitty_window
            || self.wezterm
    }

    fn kitty_probe_candidate(&self) -> bool {
        self.known_kitty_terminal() || self.windows_terminal || self.konsole
    }

    fn win32_input_candidate(&self) -> bool {
        self.windows_terminal
            || self
                .konsole_version
                .is_some_and(|version| version >= KONSOLE_WIN32_INPUT_MIN_VERSION)
    }

    fn known_terminal(&self) -> bool {
        self.kitty_probe_candidate() || self.win32_input_candidate()
    }
}

/// Environment-derived work to perform while terminal setup still owns stdin exclusively.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KeyboardInputPlan {
    native: bool,
    probe_kitty: bool,
    win32_fallback: bool,
    exact_hint: Option<bool>,
}

impl KeyboardInputPlan {
    pub(crate) fn detect() -> Self {
        Self::from_environment(cfg!(windows), &KeyboardEnvironment::capture())
    }

    fn from_environment(native_windows: bool, env: &KeyboardEnvironment) -> Self {
        if native_windows {
            return Self {
                native: true,
                probe_kitty: false,
                win32_fallback: false,
                exact_hint: Some(true),
            };
        }

        let probe_kitty = match env.kitty_override {
            ProtocolOverride::On => true,
            ProtocolOverride::Off => false,
            ProtocolOverride::Auto => !env.indirect && env.kitty_probe_candidate(),
        };
        let win32_fallback = match env.win32_override {
            ProtocolOverride::On => true,
            ProtocolOverride::Off => false,
            ProtocolOverride::Auto => !env.indirect && env.win32_input_candidate(),
        };

        let expected_kitty = env.known_kitty_terminal()
            && !matches!(env.kitty_override, ProtocolOverride::Off)
            && (!env.indirect || matches!(env.kitty_override, ProtocolOverride::On));
        let expected_win32 = env.win32_input_candidate()
            && !matches!(env.win32_override, ProtocolOverride::Off)
            && (!env.indirect || matches!(env.win32_override, ProtocolOverride::On));
        let forced = matches!(env.kitty_override, ProtocolOverride::On)
            || matches!(env.win32_override, ProtocolOverride::On);
        let exact_hint = if expected_kitty || expected_win32 || forced {
            Some(true)
        } else if env.known_terminal() {
            Some(false)
        } else {
            None
        };

        Self {
            native: false,
            probe_kitty,
            win32_fallback,
            exact_hint,
        }
    }

    pub(crate) const fn native(self) -> bool {
        self.native
    }

    pub(crate) const fn probe_kitty(self) -> bool {
        self.probe_kitty
    }

    pub(crate) const fn win32_fallback(self) -> bool {
        self.win32_fallback
    }

    pub(crate) const fn exact_hint(self) -> Option<bool> {
        self.exact_hint
    }

    #[cfg(test)]
    pub(crate) const fn for_test(native: bool, probe_kitty: bool, win32_fallback: bool) -> Self {
        Self {
            native,
            probe_kitty,
            win32_fallback,
            exact_hint: None,
        }
    }
}

/// Environment-only diagnostic; it never performs a terminal query or changes terminal state.
pub(crate) fn keyboard_input_hint() -> Option<bool> {
    KeyboardInputPlan::detect().exact_hint()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn environment(values: &[(&str, &str)]) -> KeyboardEnvironment {
        let values: HashMap<&str, &str> = values.iter().copied().collect();
        KeyboardEnvironment::from_lookup(|key| values.get(key).map(OsString::from))
    }

    fn plan(values: &[(&str, &str)]) -> KeyboardInputPlan {
        KeyboardInputPlan::from_environment(false, &environment(values))
    }

    #[test]
    fn native_windows_never_negotiates_an_escape_protocol() {
        let plan = KeyboardInputPlan::from_environment(
            true,
            &environment(&[("TERM", "xterm-kitty"), ("YTM_TUI_WIN32_INPUT", "1")]),
        );
        assert!(plan.native());
        assert!(!plan.probe_kitty());
        assert!(!plan.win32_fallback());
        assert_eq!(plan.exact_hint(), Some(true));
    }

    #[test]
    fn modern_direct_terminals_are_kitty_probe_candidates() {
        for values in [
            [("TERM", "xterm-kitty")],
            [("TERM_PROGRAM", "Ghostty")],
            [("TERM", "foot")],
            [("TERM", "alacritty")],
            [("TERM_PROGRAM", "WezTerm")],
            [("TERM_PROGRAM", "iTerm.app")],
        ] {
            let plan = plan(&values);
            assert!(plan.probe_kitty(), "fixture: {values:?}");
            assert_eq!(plan.exact_hint(), Some(true));
        }
    }

    #[test]
    fn windows_terminal_and_konsole_use_win32_only_as_a_fallback() {
        let windows_terminal = plan(&[("WT_SESSION", "session")]);
        assert!(windows_terminal.probe_kitty());
        assert!(windows_terminal.win32_fallback());

        let before = plan(&[("KONSOLE_VERSION", "260399")]);
        assert!(before.probe_kitty());
        assert!(!before.win32_fallback());
        assert_eq!(before.exact_hint(), Some(false));

        let supported = plan(&[("KONSOLE_VERSION", "260400")]);
        assert!(supported.probe_kitty());
        assert!(supported.win32_fallback());
        assert_eq!(supported.exact_hint(), Some(true));
    }

    #[test]
    fn indirect_sessions_default_to_the_safe_legacy_path() {
        for marker in [
            "TMUX",
            "STY",
            "ZELLIJ",
            "ZELLIJ_SESSION_NAME",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
        ] {
            let fixture = [("TERM_PROGRAM", "Ghostty"), (marker, "present")];
            let plan = plan(&fixture);
            assert!(!plan.probe_kitty(), "marker: {marker}");
            assert!(!plan.win32_fallback(), "marker: {marker}");
            assert_eq!(plan.exact_hint(), Some(false));
        }
    }

    #[test]
    fn explicit_overrides_bypass_auto_suppression_but_keep_kitty_priority() {
        let plan = plan(&[
            ("TERM_PROGRAM", "Ghostty"),
            ("WT_SESSION", "session"),
            ("TMUX", "present"),
            ("YTM_TUI_KEYBOARD_ENHANCEMENT", "ON"),
            ("YTM_TUI_WIN32_INPUT", "true"),
        ]);
        assert!(plan.probe_kitty());
        assert!(plan.win32_fallback());
        assert_eq!(plan.exact_hint(), Some(true));
    }

    #[test]
    fn disabling_protocols_and_invalid_overrides_are_deterministic() {
        let disabled = plan(&[
            ("TERM_PROGRAM", "Ghostty"),
            ("YTM_TUI_KEYBOARD_ENHANCEMENT", "false"),
            ("YTM_TUI_WIN32_INPUT", "0"),
        ]);
        assert!(!disabled.probe_kitty());
        assert!(!disabled.win32_fallback());
        assert_eq!(disabled.exact_hint(), Some(false));

        let invalid = plan(&[
            ("TERM_PROGRAM", "Ghostty"),
            ("YTM_TUI_KEYBOARD_ENHANCEMENT", "maybe"),
        ]);
        assert!(invalid.probe_kitty());

        let unknown = plan(&[("TERM", "dumb")]);
        assert!(!unknown.probe_kitty());
        assert!(!unknown.win32_fallback());
        assert_eq!(unknown.exact_hint(), None);
    }
}
