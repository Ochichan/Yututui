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
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::{Frame, Terminal};

use crate::zoom::{ZoomBackend, ZoomHandle};

/// The app's terminal: a [`CrosstermBackend`] wrapped in the OSC 66 text-zoom layer.
/// At zoom 1 (always, on terminals without the text sizing protocol) the wrapper is a
/// transparent pass-through of ratatui's `DefaultTerminal`.
pub type AppTerminal = Terminal<ZoomBackend<io::Stdout>>;

static KEYBOARD_ENHANCEMENT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Initialise the terminal. When `mouse` is true, mouse events are captured.
///
/// Returns the terminal plus whether the text sizing protocol (OSC 66 zoom) is
/// available — probed here because the handshake must happen after the alternate
/// screen is entered (so the probe glyph lands on a throwaway screen) and before the
/// event loop's `EventStream` starts reading stdin (the probe reads its own replies).
pub fn init(mouse: bool, zoom: ZoomHandle) -> io::Result<(AppTerminal, bool)> {
    // `try_init` = panic hook + raw mode + alternate screen + a `DefaultTerminal` we
    // don't want. Drop the terminal (it has no teardown Drop) and rebuild on the zoom
    // backend, keeping ratatui's hook/raw-mode/alt-screen setup — and `ratatui::restore`
    // in `restore()` — exactly as they were.
    drop(ratatui::try_init()?);
    let terminal = Terminal::new(ZoomBackend::new(CrosstermBackend::new(io::stdout()), zoom))?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    // Ask the terminal to report focus in/out (DECSET ?1004) so the reducer can park animations
    // while we're hidden. Independent of mouse capture; a no-op on terminals that don't support
    // it (they simply never send the events, and `App.focused` stays `true`). Safe to enable
    // before the input flush below — focus is reported only on *transitions*, not as a backlog.
    let _ = execute!(io::stdout(), EnableFocusChange);
    enable_keyboard_enhancement();
    let text_sizing = crate::zoom::should_probe() && crate::zoom::probe_support();
    // Discard any input already queued by terminal setup — chiefly leftover bytes from the
    // graphics/keyboard capability probes (DA1 `\e[?...c`, cell-size `\e[...t`, kitty APC) that
    // would otherwise be mis-parsed as key/mouse events the moment the event loop starts.
    // Runs after the text-sizing probe so a late/partial CPR reply can't ghost into the
    // event loop either.
    flush_pending_input();
    Ok((terminal, text_sizing))
}

/// Draw one frame wrapped in a synchronized update (DECSET ?2026), so the terminal swaps the
/// whole frame atomically instead of revealing it mid-paint. This removes tearing on the
/// full-screen canvas effects (matrix rain / donut / visualizer), which touch most of the screen
/// each frame. `Begin`/`End` are unsupported-terminal-safe — a terminal that doesn't grok the
/// private mode simply ignores both, leaving behaviour identical to a bare `draw`. `End` is always
/// emitted, even if `draw` errors, so a failed frame can't leave the terminal stuck mid-update.
pub fn draw_synced<F>(terminal: &mut AppTerminal, render: F) -> io::Result<()>
where
    F: FnOnce(&mut Frame),
{
    let _ = execute!(io::stdout(), BeginSynchronizedUpdate);
    let res = terminal.draw(render);
    let _ = execute!(io::stdout(), EndSynchronizedUpdate);
    res.map(|_| ())
}

/// Draw one frame, using synchronized update only when the caller expects large image/canvas
/// damage. This keeps ordinary one-line redraws from emitting DECSET ?2026 wrappers.
pub fn draw_frame<F>(
    terminal: &mut AppTerminal,
    synchronized: bool,
    clear_before: bool,
    render: F,
) -> io::Result<()>
where
    F: FnOnce(&mut Frame),
{
    if synchronized {
        if !clear_before {
            return draw_synced(terminal, render);
        }
        let _ = execute!(io::stdout(), BeginSynchronizedUpdate);
        let res = (|| {
            write_vt_clear_for_native_images()?;
            draw_frame_after_explicit_clear(terminal, render)
        })();
        let _ = execute!(io::stdout(), EndSynchronizedUpdate);
        return res;
    }
    if clear_before {
        write_vt_clear_for_native_images()?;
        return draw_frame_after_explicit_clear(terminal, render);
    }
    draw_frame_inner(terminal, render)
}

fn write_vt_clear_for_native_images() -> io::Result<()> {
    use io::Write;

    // Clear native terminal graphics directly. We deliberately do not use `Terminal::clear()` for
    // this path: ratatui preserves the cursor by querying crossterm's cursor position, which can
    // race the event stream on Unix and fail after a 2s ESC[6n timeout during image-heavy redraws.
    let mut stdout = io::stdout().lock();
    stdout.write_all(b"\x1b[2J\x1b[H")?;
    stdout.flush()
}

fn draw_frame_inner<B, F>(terminal: &mut Terminal<B>, render: F) -> Result<(), B::Error>
where
    B: Backend,
    F: FnOnce(&mut Frame),
{
    terminal.draw(render).map(|_| ())
}

fn draw_frame_after_explicit_clear<B, F>(
    terminal: &mut Terminal<B>,
    render: F,
) -> Result<(), B::Error>
where
    B: Backend,
    F: FnOnce(&mut Frame),
{
    terminal.autoresize()?;
    // After the explicit VT clear, reset ratatui's previous-screen buffer without calling
    // `Terminal::clear()`. The next flush then treats the screen as empty and re-emits the full
    // frame, including native-image anchor cells.
    terminal.swap_buffers();
    {
        let mut frame = terminal.get_frame();
        render(&mut frame);
    }
    terminal.apply_buffer_with_cursor(None).map(|_| ())
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
    // and DJ Gem input boxes. (ghostty was lenient enough to keep composing, which is why this only
    // broke in kitty.) The remaining flags disambiguate modified keys without touching text input.
    // Ctrl+Shift character chords work when the terminal reports distinct enhanced key events;
    // legacy encodings may still collapse them to the matching Ctrl+key before we see them.
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

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Size};
    use ratatui::widgets::Paragraph;

    use super::{draw_frame_after_explicit_clear, draw_frame_inner};

    #[test]
    fn clear_before_draw_forces_unchanged_cells_to_redraw() {
        let backend = TestBackend::new(5, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        draw_frame_inner(&mut terminal, |frame| {
            frame.render_widget(Paragraph::new("abc"), frame.area());
        })
        .unwrap();
        terminal.backend().assert_buffer_lines(["abc  "]);

        draw_frame_after_explicit_clear(&mut terminal, |frame| {
            frame.render_widget(Paragraph::new("abc"), frame.area());
        })
        .unwrap();
        terminal.backend().assert_buffer_lines(["abc  "]);
    }

    struct CursorQueryPanicBackend(TestBackend);

    impl Backend for CursorQueryPanicBackend {
        type Error = Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.0.draw(content)
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.0.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.0.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
            panic!("cursor position must not be queried")
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.0.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.0.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
            self.0.clear_region(clear_type)
        }

        fn size(&self) -> Result<Size, Self::Error> {
            self.0.size()
        }

        fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
            self.0.window_size()
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            self.0.flush()
        }
    }

    #[test]
    fn explicit_clear_draw_does_not_query_cursor_position() {
        let backend = CursorQueryPanicBackend(TestBackend::new(5, 1));
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        draw_frame_after_explicit_clear(&mut terminal, |frame| {
            frame.render_widget(Paragraph::new("abc"), frame.area());
        })
        .unwrap();

        terminal.backend().0.assert_buffer_lines(["abc  "]);
    }
}
