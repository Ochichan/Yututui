//! Terminal setup/teardown. Built on ratatui 0.30's `try_init`/`restore`, which
//! handle raw mode, the alternate screen, and a terminal-restoring panic hook.
//! Mouse capture is opt-in (config `mouse`, default on) and drives buttons + seekbar.
//! The panic hook is additionally wrapped in `player::lifetime` to kill mpv on a
//! crash before the terminal is restored.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use ratatui::DefaultTerminal;

static KEYBOARD_ENHANCEMENT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Initialise the terminal. When `mouse` is true, mouse events are captured.
pub fn init(mouse: bool) -> io::Result<DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    enable_keyboard_enhancement();
    // Discard any input already queued by terminal setup — chiefly leftover bytes from the
    // graphics/keyboard capability probes (DA1 `\e[?...c`, cell-size `\e[...t`, kitty APC) that
    // would otherwise be mis-parsed as key/mouse events the moment the event loop starts.
    flush_pending_input();
    Ok(terminal)
}

/// Drain and discard any events already buffered before the main event loop begins. Bounded so a
/// user holding a key at launch can't make this spin.
fn flush_pending_input() {
    use std::time::Duration;
    for _ in 0..1024 {
        match crossterm::event::poll(Duration::ZERO) {
            Ok(true) => {
                if crossterm::event::read().is_err() {
                    break;
                }
            }
            _ => break,
        }
    }
}

/// Restore the terminal to its original state. Safe to call more than once.
pub fn restore(mouse: bool) {
    disable_keyboard_enhancement();
    if mouse {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
}

fn enable_keyboard_enhancement() {
    if !should_probe_keyboard_enhancement() {
        return;
    }
    if !matches!(
        crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    ) {
        return;
    }
    // Deliberately *without* REPORT_ALL_KEYS_AS_ESCAPE_CODES: under that flag kitty (and other
    // strict implementers) route every keystroke — including plain text — as an escape code and
    // turn off the terminal's IME, so Hangul/CJK jamo never compose into syllables in the search
    // and AI input boxes. (ghostty was lenient enough to keep composing, which is why this only
    // broke in kitty.) The remaining flags disambiguate special keys without touching text input,
    // and every keymap chord is a plain Ctrl/Alt/Shift+key the legacy encoding already reports.
    let flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
    if execute!(io::stdout(), PushKeyboardEnhancementFlags(flags)).is_ok() {
        KEYBOARD_ENHANCEMENT_ENABLED.store(true, Ordering::Relaxed);
    }
}

fn disable_keyboard_enhancement() {
    if KEYBOARD_ENHANCEMENT_ENABLED.swap(false, Ordering::Relaxed) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
}

fn should_probe_keyboard_enhancement() -> bool {
    match std::env::var("YTM_TUI_KEYBOARD_ENHANCEMENT")
        .ok()
        .as_deref()
    {
        Some("0" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF") => return false,
        Some("1" | "true" | "True" | "TRUE" | "on" | "On" | "ON") => return true,
        _ => {}
    }

    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    term.contains("kitty")
        || term.contains("wezterm")
        || term.contains("foot")
        || term.contains("alacritty")
        || term_program.contains("wezterm")
}
