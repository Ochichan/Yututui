//! Read-only terminal capability and liveness diagnostics.
//!
//! This module deliberately inspects only environment variables and existing file descriptors.
//! It never enters raw mode, writes a terminal query, launches a multiplexer CLI, or loads user
//! configuration.

use serde::Serialize;
use std::io::IsTerminal;

use crate::terminal_policy;

const KONSOLE_SIXEL_TUI_MIN_VERSION: u32 = 260_400;

#[derive(Serialize)]
struct TerminalLivenessPolicy {
    heartbeat_interval_ms: u64,
    owner_probe_interval_ms: u64,
    owner_probe_timeout_ms: u64,
    liveness_output_gate_timeout_ms: u64,
    cpr_total_timeout_ms: u64,
    cpr_write_timeout_ms: u64,
    ambiguous_confirmations: u8,
    retry_delay_ms: u64,
    hard_watchdog_ms: u64,
    startup_liveness_report_timeout_ms: u64,
    owner_output_deadline_ms: u64,
    startup_output_deadline_ms: u64,
    normal_restore_deadline_ms: u64,
    emergency_restore_deadline_ms: u64,
    hard_exit_query_quiesce_deadline_ms: u64,
    hard_exit_total_deadline_ms: u64,
    notification_output_deadline_ms: u64,
    generic_pending_input_idle_ms: u64,
    escape_pending_input_idle_ms: u64,
    paste_pending_input_idle_ms: u64,
    paste_max_bytes: usize,
}

impl Default for TerminalLivenessPolicy {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: duration_ms(terminal_policy::HEARTBEAT_INTERVAL),
            owner_probe_interval_ms: duration_ms(terminal_policy::OWNER_PROBE_INTERVAL),
            owner_probe_timeout_ms: duration_ms(terminal_policy::OWNER_PROBE_TIMEOUT),
            liveness_output_gate_timeout_ms: duration_ms(
                terminal_policy::LIVENESS_OUTPUT_GATE_TIMEOUT,
            ),
            cpr_total_timeout_ms: duration_ms(terminal_policy::CPR_TOTAL_TIMEOUT),
            cpr_write_timeout_ms: duration_ms(terminal_policy::CPR_WRITE_TIMEOUT),
            ambiguous_confirmations: terminal_policy::AMBIGUOUS_CONFIRMATIONS,
            retry_delay_ms: duration_ms(terminal_policy::AMBIGUOUS_RETRY_INTERVAL),
            hard_watchdog_ms: duration_ms(terminal_policy::HARD_WATCHDOG_TIMEOUT),
            startup_liveness_report_timeout_ms: duration_ms(
                terminal_policy::STARTUP_LIVENESS_REPORT_TIMEOUT,
            ),
            owner_output_deadline_ms: duration_ms(terminal_policy::OWNER_OUTPUT_TIMEOUT),
            startup_output_deadline_ms: duration_ms(terminal_policy::STARTUP_OUTPUT_TIMEOUT),
            normal_restore_deadline_ms: duration_ms(terminal_policy::NORMAL_RESTORE_TIMEOUT),
            emergency_restore_deadline_ms: duration_ms(terminal_policy::EMERGENCY_RESTORE_TIMEOUT),
            hard_exit_query_quiesce_deadline_ms: duration_ms(
                terminal_policy::HARD_EXIT_QUERY_QUIESCE_TIMEOUT,
            ),
            hard_exit_total_deadline_ms: duration_ms(terminal_policy::HARD_EXIT_TOTAL_TIMEOUT),
            notification_output_deadline_ms: duration_ms(
                terminal_policy::NOTIFICATION_OUTPUT_TIMEOUT,
            ),
            generic_pending_input_idle_ms: duration_ms(terminal_policy::GENERIC_PENDING_INPUT_IDLE),
            escape_pending_input_idle_ms: duration_ms(terminal_policy::ESCAPE_PENDING_INPUT_IDLE),
            paste_pending_input_idle_ms: duration_ms(terminal_policy::PASTE_PENDING_INPUT_IDLE),
            paste_max_bytes: terminal_policy::PASTE_MAX_BYTES,
        }
    }
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Serialize)]
struct TerminalDoctor {
    term: Option<String>,
    term_program: Option<String>,
    wt_session: bool,
    terminal_family: &'static str,
    konsole_version: Option<String>,
    terminal_input_backend: &'static str,
    terminal_output_backend: &'static str,
    terminal_owner_layers: Vec<&'static str>,
    terminal_liveness_mode: &'static str,
    terminal_liveness_policy: TerminalLivenessPolicy,
    stdin_stdout_same_tty: Option<bool>,
    image_protocol: &'static str,
    image_protocol_source: &'static str,
    native_image_hint: bool,
    image_probe_timeout_ms: u64,
    image_protocol_override: Option<String>,
    image_protocol_override_supported: Option<bool>,
    image_protocol_override_suggestions: Vec<&'static str>,
    zoom_mode: &'static str,
    zoom_mode_source: &'static str,
    keyboard_enhancement_supported: Option<bool>,
    keyboard_enhancement_source: &'static str,
    mouse_capture_configured: Option<bool>,
    mouse_capture_source: &'static str,
    stdout_is_tty: bool,
    stdin_is_tty: bool,
    warnings: Vec<String>,
}

pub(super) fn run_json() -> i32 {
    // No config load, cookie read, playback init, mpv spawn, terminal raw mode, commands, or
    // terminal control/query writes are permitted in this diagnostic.
    let term = std::env::var("TERM").ok();
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let konsole_version = std::env::var("KONSOLE_VERSION").ok();
    let wt_session = env_nonempty("WT_SESSION");
    let image_protocol_override = std::env::var("YTM_TUI_IMAGE_PROTOCOL").ok();
    let image_protocol_override_supported = image_protocol_override
        .as_deref()
        .map(image_protocol_override_supported);
    let image_protocol = terminal_image_protocol(
        term.as_deref(),
        term_program.as_deref(),
        wt_session,
        konsole_version.as_deref(),
    );
    let native_image_hint =
        terminal_native_image_hint(term.as_deref(), term_program.as_deref(), wt_session);
    let image_protocol_override_suggestions =
        terminal_image_override_suggestions(term.as_deref(), term_program.as_deref(), wt_session);
    let stdout_is_tty = std::io::stdout().is_terminal();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let stdin_stdout_same_tty = same_terminal_device(stdin_is_tty, stdout_is_tty);
    let (terminal_owner_layers, terminal_liveness_mode, mut warnings) =
        owner_context(term.as_deref());

    if !stdout_is_tty {
        warnings.push(
            "stdout is not a TTY; YuTuTui! cannot open its bounded terminal output".to_owned(),
        );
    }
    if !stdin_is_tty {
        warnings.push("stdin is not a TTY; interactive terminal probes may not answer".to_owned());
    }
    if stdin_stdout_same_tty == Some(false) {
        warnings.push(
            "stdin and stdout refer to different terminal devices; interactive startup will fail closed"
                .to_owned(),
        );
    }
    if image_protocol_override_supported == Some(false) {
        warnings.push(
            "unsupported YTM_TUI_IMAGE_PROTOCOL; accepted values are halfblocks, sixel, kitty, iterm2"
                .to_owned(),
        );
    }
    if native_image_hint
        && matches!(image_protocol, "unknown" | "halfblocks_or_retro")
        && !image_protocol_override_suggestions.is_empty()
    {
        warnings.push(format!(
            "native image hint detected; if album art falls back to halfblocks, try {}",
            image_protocol_override_suggestions.join(", ")
        ));
    }

    let report = TerminalDoctor {
        terminal_family: terminal_family(
            term.as_deref(),
            term_program.as_deref(),
            wt_session,
            konsole_version.as_deref(),
        ),
        konsole_version,
        terminal_input_backend: terminal_input_backend(),
        terminal_output_backend: terminal_output_backend(),
        terminal_owner_layers,
        terminal_liveness_mode,
        terminal_liveness_policy: TerminalLivenessPolicy::default(),
        stdin_stdout_same_tty,
        image_protocol,
        image_protocol_source: "environment",
        native_image_hint,
        image_probe_timeout_ms: terminal_image_probe_timeout_ms(native_image_hint),
        image_protocol_override,
        image_protocol_override_supported,
        image_protocol_override_suggestions,
        zoom_mode: terminal_zoom_mode(term.as_deref(), term_program.as_deref(), wt_session),
        zoom_mode_source: "environment",
        keyboard_enhancement_supported: terminal_keyboard_hint(),
        keyboard_enhancement_source: "environment_hint",
        // This command intentionally does not load user configuration. `null` is different from
        // claiming the default is the user's saved effective value.
        mouse_capture_configured: None,
        mouse_capture_source: "not_loaded_by_read_only_diagnostic",
        term,
        term_program,
        wt_session,
        stdout_is_tty,
        stdin_is_tty,
        warnings,
    };

    match serde_json::to_string_pretty(&report) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(error) => {
            eprintln!("ytt doctor: could not encode terminal report: {error}");
            1
        }
    }
}

fn terminal_input_backend() -> &'static str {
    if cfg!(unix) {
        "unix_mio_nonblocking_tty"
    } else if cfg!(windows) {
        "windows_console"
    } else {
        "unsupported"
    }
}

fn terminal_output_backend() -> &'static str {
    if cfg!(unix) {
        "unix_nonblocking_tty"
    } else if cfg!(windows) {
        "windows_console"
    } else {
        "unsupported"
    }
}

fn terminal_family(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
    konsole_version: Option<&str>,
) -> &'static str {
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();
    if konsole_version.is_some_and(|value| !value.is_empty()) || term.contains("konsole") {
        // Yakuake embeds KonsolePart and intentionally reports the same family.
        "konsole"
    } else if wt_session {
        "windows_terminal"
    } else if env_nonempty("KITTY_WINDOW_ID") || term.contains("kitty") {
        "kitty"
    } else if env_nonempty("WEZTERM_EXECUTABLE") || term_program.contains("wezterm") {
        "wezterm"
    } else if term_program == "iterm.app" {
        "iterm2"
    } else if term_program.contains("ghostty") || term.contains("ghostty") {
        "ghostty"
    } else if term.contains("foot") {
        "foot"
    } else if term.contains("mintty") {
        "mintty"
    } else if term.contains("mlterm") {
        "mlterm"
    } else if term == "linux" {
        "linux_console"
    } else {
        "unknown"
    }
}

fn owner_context(term: Option<&str>) -> (Vec<&'static str>, &'static str, Vec<String>) {
    let mut layers = Vec::with_capacity(3);
    let mut warnings = Vec::new();
    let mut incomplete = false;

    if std::env::var_os("ZELLIJ").is_some() || std::env::var_os("ZELLIJ_SESSION_NAME").is_some() {
        layers.push("zellij");
        if !env_nonempty("ZELLIJ_SESSION_NAME") {
            incomplete = true;
            warnings.push(
                "Zellij was detected without ZELLIJ_SESSION_NAME; owner liveness cannot be established"
                    .to_owned(),
            );
        }
    }
    if std::env::var_os("TMUX").is_some() || std::env::var_os("TMUX_PANE").is_some() {
        layers.push("tmux");
        if !env_nonempty("TMUX_PANE") {
            incomplete = true;
            warnings.push(
                "tmux was detected without TMUX_PANE; owner liveness cannot be established"
                    .to_owned(),
            );
        }
    }
    if std::env::var_os("STY").is_some() {
        layers.push("screen");
        if !env_nonempty("STY") {
            incomplete = true;
            warnings.push(
                "GNU screen was detected without a session identifier; owner liveness cannot be established"
                    .to_owned(),
            );
        }
    }

    let term = term.unwrap_or_default().to_ascii_lowercase();
    if layers.is_empty() && (term.starts_with("tmux") || term.starts_with("screen")) {
        incomplete = true;
        let layer = if term.starts_with("tmux") {
            "tmux"
        } else {
            "screen"
        };
        layers.push(layer);
        warnings.push(format!(
            "{layer} was inferred from TERM without its owner identifier; owner liveness cannot be established"
        ));
    }

    let mode = if incomplete {
        "owner_probe_unavailable"
    } else if layers.is_empty() {
        "cpr_only"
    } else {
        "cpr_and_owner_probe"
    };
    (layers, mode, warnings)
}

#[cfg(unix)]
fn same_terminal_device(stdin_is_tty: bool, stdout_is_tty: bool) -> Option<bool> {
    if !stdin_is_tty || !stdout_is_tty {
        return None;
    }
    let stdin_stat = rustix::fs::fstat(std::io::stdin()).ok()?;
    let stdout_stat = rustix::fs::fstat(std::io::stdout()).ok()?;
    Some(stdin_stat.st_rdev == stdout_stat.st_rdev)
}

#[cfg(not(unix))]
fn same_terminal_device(_stdin_is_tty: bool, _stdout_is_tty: bool) -> Option<bool> {
    None
}

pub(super) fn terminal_native_image_hint(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
) -> bool {
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();
    wt_session
        || env_nonempty("KITTY_WINDOW_ID")
        || env_nonempty("WEZTERM_EXECUTABLE")
        || env_nonempty("KONSOLE_VERSION")
        || term_program == "iterm.app"
        || term_program.contains("wezterm")
        || term_program.contains("ghostty")
        || [
            "kitty", "ghostty", "wezterm", "foot", "konsole", "mlterm", "mintty", "rio", "contour",
        ]
        .iter()
        .any(|hint| term.contains(hint))
}

fn env_nonempty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

pub(super) fn terminal_image_probe_timeout_ms(native_image_hint: bool) -> u64 {
    if native_image_hint { 700 } else { 250 }
}

pub(super) fn image_protocol_override_supported(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "halfblocks" | "halfblock" | "blocks" | "block" | "sixel" | "kitty" | "iterm2" | "iterm"
    )
}

pub(super) fn terminal_image_override_suggestions(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
) -> Vec<&'static str> {
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();

    if env_nonempty("KITTY_WINDOW_ID")
        || term.contains("kitty")
        || term.contains("ghostty")
        || term_program.contains("ghostty")
    {
        return vec!["YTM_TUI_IMAGE_PROTOCOL=kitty"];
    }
    if term_program == "iterm.app" {
        return vec!["YTM_TUI_IMAGE_PROTOCOL=iterm2"];
    }
    if env_nonempty("WEZTERM_EXECUTABLE")
        || term_program.contains("wezterm")
        || term.contains("wezterm")
    {
        return vec![
            "YTM_TUI_IMAGE_PROTOCOL=iterm2",
            "YTM_TUI_IMAGE_PROTOCOL=kitty",
            "YTM_TUI_IMAGE_PROTOCOL=sixel",
        ];
    }
    if env_nonempty("KONSOLE_VERSION") || term.contains("konsole") {
        return vec!["YTM_TUI_IMAGE_PROTOCOL=sixel"];
    }
    if wt_session || term.contains("foot") || term.contains("mintty") || term.contains("mlterm") {
        return vec!["YTM_TUI_IMAGE_PROTOCOL=sixel"];
    }
    if terminal_native_image_hint(Some(&term), Some(&term_program), wt_session) {
        return vec![
            "YTM_TUI_IMAGE_PROTOCOL=kitty",
            "YTM_TUI_IMAGE_PROTOCOL=iterm2",
            "YTM_TUI_IMAGE_PROTOCOL=sixel",
        ];
    }
    Vec::new()
}

pub(super) fn terminal_image_protocol(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
    konsole_version: Option<&str>,
) -> &'static str {
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();
    if term.contains("kitty") {
        "kitty"
    } else if term_program == "iterm.app" {
        "iterm2"
    } else if term_program.contains("wezterm") {
        "iterm2_or_kitty_or_sixel"
    } else if wt_session {
        "sixel_versioned"
    } else if term.contains("foot") || term.contains("mintty") || term.contains("mlterm") {
        "sixel"
    } else if term.contains("konsole") || konsole_version.is_some_and(|version| !version.is_empty())
    {
        if konsole_version
            .and_then(|version| version.trim().parse::<u32>().ok())
            .is_some_and(|version| version >= KONSOLE_SIXEL_TUI_MIN_VERSION)
        {
            "sixel_versioned"
        } else {
            "halfblocks"
        }
    } else if term_program.contains("ghostty") || term.contains("ghostty") {
        "kitty"
    } else if term.contains("linux") {
        "halfblocks_or_retro"
    } else {
        "unknown"
    }
}

pub(super) fn terminal_zoom_mode(
    term: Option<&str>,
    term_program: Option<&str>,
    wt_session: bool,
) -> &'static str {
    if let Ok(value) = std::env::var("YTM_TUI_TEXT_SIZING") {
        return match value.as_str() {
            "0" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => "none_forced",
            "dhl" | "DHL" | "decdhl" => "decdhl_forced",
            _ => "probe_requested",
        };
    }
    let term = term.unwrap_or_default().to_ascii_lowercase();
    let term_program = term_program.unwrap_or_default().to_ascii_lowercase();
    if term.contains("kitty") {
        "osc66_versioned"
    } else if wt_session {
        "decdhl_expected"
    } else if term_program.contains("wezterm") || term_program.contains("ghostty") {
        "unknown_probe_required"
    } else {
        "unknown"
    }
}

pub(super) fn terminal_keyboard_hint() -> Option<bool> {
    crate::terminal_keyboard::keyboard_input_hint()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::env::{with_var, with_vars};

    const TERMINAL_ENV: &[(&str, Option<&str>)] = &[
        ("KITTY_WINDOW_ID", None),
        ("WEZTERM_EXECUTABLE", None),
        ("KONSOLE_VERSION", None),
        ("WT_SESSION", None),
        ("TERM", None),
        ("TERM_PROGRAM", None),
        ("TMUX", None),
        ("TMUX_PANE", None),
        ("STY", None),
        ("ZELLIJ", None),
        ("ZELLIJ_SESSION_NAME", None),
        ("SSH_CONNECTION", None),
        ("SSH_CLIENT", None),
        ("SSH_TTY", None),
        ("YTM_TUI_KEYBOARD_ENHANCEMENT", None),
        ("YTM_TUI_WIN32_INPUT", None),
    ];

    fn with_terminal_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let mut scoped = TERMINAL_ENV.to_vec();
        scoped.extend_from_slice(vars);
        with_vars(&scoped, f)
    }

    #[test]
    fn detects_protocol_zoom_and_keyboard_hints_without_probing() {
        with_var("YTM_TUI_TEXT_SIZING", None, || {
            assert_eq!(
                terminal_image_protocol(Some("xterm-kitty"), None, false, None),
                "kitty"
            );
            assert_eq!(
                terminal_image_protocol(Some("xterm-256color"), Some("WezTerm"), false, None),
                "iterm2_or_kitty_or_sixel"
            );
            assert_eq!(
                terminal_image_protocol(Some("konsole"), None, false, Some("260400")),
                "sixel_versioned"
            );
            assert_eq!(
                terminal_zoom_mode(Some("xterm-kitty"), None, false),
                "osc66_versioned"
            );
        });
        with_terminal_env(&[("TERM", Some("foot"))], || {
            assert_eq!(terminal_keyboard_hint(), Some(true));
        });
    }

    #[test]
    fn reports_native_hint_timeout_and_override_guidance() {
        with_terminal_env(&[("KONSOLE_VERSION", Some("260400"))], || {
            assert!(terminal_native_image_hint(None, None, false));
            assert_eq!(terminal_image_probe_timeout_ms(true), 700);
            assert_eq!(
                terminal_image_override_suggestions(None, None, false),
                vec!["YTM_TUI_IMAGE_PROTOCOL=sixel"]
            );
        });
        assert!(image_protocol_override_supported("  SIXEL  "));
        assert!(!image_protocol_override_supported("bad"));
    }

    #[test]
    fn owner_context_contains_only_layer_types() {
        with_terminal_env(
            &[
                ("TERM", Some("screen-256color")),
                ("TMUX", Some("/private/path,123,0")),
                ("TMUX_PANE", Some("%42")),
            ],
            || {
                let (layers, mode, warnings) = owner_context(Some("screen-256color"));
                assert_eq!(layers, vec!["tmux"]);
                assert_eq!(mode, "cpr_and_owner_probe");
                assert!(warnings.is_empty());
                assert!(!format!("{layers:?}").contains("%42"));
            },
        );
    }

    #[test]
    fn incomplete_owner_context_is_explicit() {
        with_terminal_env(&[("TERM", Some("tmux-256color"))], || {
            let (layers, mode, warnings) = owner_context(Some("tmux-256color"));
            assert_eq!(layers, vec!["tmux"]);
            assert_eq!(mode, "owner_probe_unavailable");
            assert_eq!(warnings.len(), 1);
        });
    }

    #[test]
    fn terminal_family_treats_yakuake_as_konsolepart() {
        with_terminal_env(&[("KONSOLE_VERSION", Some("260403"))], || {
            assert_eq!(
                terminal_family(Some("xterm-256color"), None, false, Some("260403")),
                "konsole"
            );
        });
    }

    #[test]
    fn empty_terminal_hint_variables_are_not_treated_as_present() {
        with_terminal_env(
            &[
                ("WT_SESSION", Some("")),
                ("KITTY_WINDOW_ID", Some("")),
                ("WEZTERM_EXECUTABLE", Some("")),
                ("KONSOLE_VERSION", Some("")),
            ],
            || {
                assert!(!env_nonempty("WT_SESSION"));
                assert_eq!(
                    terminal_family(Some("xterm"), None, false, Some("")),
                    "unknown"
                );
                assert!(!terminal_native_image_hint(
                    Some("xterm"),
                    Some("plain-terminal"),
                    false
                ));
            },
        );
    }
}
