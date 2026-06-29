//! Terminal setup/teardown. Built on ratatui 0.30's `try_init`/`restore`, which
//! handle raw mode, the alternate screen, and a terminal-restoring panic hook.
//! Mouse capture is opt-in (config `mouse`, default on) and drives buttons + seekbar.
//! The panic hook is additionally wrapped in `player::lifetime` to kill mpv on a
//! crash before the terminal is restored.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::{DefaultTerminal, Frame};

static KEYBOARD_ENHANCEMENT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Initialise the terminal. When `mouse` is true, mouse events are captured.
pub fn init(mouse: bool) -> io::Result<DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    // Ask the terminal to report focus in/out (DECSET ?1004) so the reducer can park animations
    // while we're hidden. Independent of mouse capture; a no-op on terminals that don't support
    // it (they simply never send the events, and `App.focused` stays `true`). Safe to enable
    // before the input flush below — focus is reported only on *transitions*, not as a backlog.
    let _ = execute!(io::stdout(), EnableFocusChange);
    enable_keyboard_enhancement();
    // Discard any input already queued by terminal setup — chiefly leftover bytes from the
    // graphics/keyboard capability probes (DA1 `\e[?...c`, cell-size `\e[...t`, kitty APC) that
    // would otherwise be mis-parsed as key/mouse events the moment the event loop starts.
    flush_pending_input();
    Ok(terminal)
}

/// Draw one frame wrapped in a synchronized update (DECSET ?2026), so the terminal swaps the
/// whole frame atomically instead of revealing it mid-paint. This removes tearing on the
/// full-screen canvas effects (matrix rain / donut / visualizer), which touch most of the screen
/// each frame. `Begin`/`End` are unsupported-terminal-safe — a terminal that doesn't grok the
/// private mode simply ignores both, leaving behaviour identical to a bare `draw`. `End` is always
/// emitted, even if `draw` errors, so a failed frame can't leave the terminal stuck mid-update.
pub fn draw_synced<F>(terminal: &mut DefaultTerminal, render: F) -> io::Result<()>
where
    F: FnOnce(&mut Frame),
{
    let _ = execute!(io::stdout(), BeginSynchronizedUpdate);
    let res = terminal.draw(render);
    let _ = execute!(io::stdout(), EndSynchronizedUpdate);
    res.map(|_| ())
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
    let _ = execute!(io::stdout(), DisableFocusChange);
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
