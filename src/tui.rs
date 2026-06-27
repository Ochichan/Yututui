//! Terminal setup/teardown. Built on ratatui 0.30's `try_init`/`restore`, which
//! handle raw mode, the alternate screen, and a terminal-restoring panic hook.
//! Mouse capture is opt-in (config `mouse`, default on) and drives buttons + seekbar.
//! The panic hook is additionally wrapped in `player::lifetime` to kill mpv on a
//! crash before the terminal is restored.

use std::io;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use ratatui::DefaultTerminal;

/// Initialise the terminal. When `mouse` is true, mouse events are captured.
pub fn init(mouse: bool) -> io::Result<DefaultTerminal> {
    let terminal = ratatui::try_init()?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    Ok(terminal)
}

/// Restore the terminal to its original state. Safe to call more than once.
pub fn restore(mouse: bool) {
    if mouse {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
}
