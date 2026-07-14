//! Terminal setup/teardown. Built on ratatui 0.30's `try_init`/`restore`, which
//! handle raw mode, the alternate screen, and a terminal-restoring panic hook.
//! Mouse capture is opt-in (config `mouse`, default on) and drives buttons + seekbar.
//! The panic hook is additionally wrapped in `player::lifetime` to kill mpv on a
//! crash before the terminal is restored.

use std::io::{self, Write};
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

/// Reusable terminal output buffer that never leaks a partial frame after panic restoration.
///
/// Normal drop retains `BufWriter`'s best-effort flush. During unwinding, ratatui's panic hook has
/// already restored the terminal before locals are dropped, so the pending bytes are detached and
/// discarded instead of being emitted onto the restored screen.
pub struct PanicSafeBufWriter<W: Write> {
    inner: Option<io::BufWriter<W>>,
}

impl<W: Write> PanicSafeBufWriter<W> {
    fn new(writer: W) -> Self {
        Self {
            inner: Some(io::BufWriter::new(writer)),
        }
    }

    fn inner_mut(&mut self) -> &mut io::BufWriter<W> {
        self.inner
            .as_mut()
            .expect("buffered writer is present until drop")
    }
}

impl<W: Write> Write for PanicSafeBufWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner_mut().flush()
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.inner_mut().write_vectored(bufs)
    }
}

impl<W: Write> Drop for PanicSafeBufWriter<W> {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(buffered) = self.inner.take()
        {
            let (_writer, pending) = buffered.into_parts();
            drop(pending);
        }
    }
}

/// The app's terminal: a buffered [`CrosstermBackend`] wrapped in the OSC 66 text-zoom layer.
/// Ratatui's terminal flush remains the frame boundary while the reusable buffer coalesces the
/// backend's many small writes. At zoom 1 the zoom wrapper is a transparent pass-through.
pub type AppTerminal = Terminal<ZoomBackend<PanicSafeBufWriter<io::Stdout>>>;

/// Outcome of the low-cost terminal-owned IME preedit scrub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImeScrubResult {
    /// The unchanged terminal apply sequence completed successfully.
    Fast,
    /// The backend size differs from ratatui's current frame; render a full frame immediately.
    Resized,
}

static KEYBOARD_ENHANCEMENT_ENABLED: AtomicBool = AtomicBool::new(false);

/// Initialise the terminal. When `mouse` is true, mouse events are captured.
///
/// Also detects the terminal's text-zoom mechanism into `zoom` — done here because the
/// probes must run after the alternate screen is entered (so probe glyphs land on a
/// throwaway screen) and before the event loop's `EventStream` starts reading stdin
/// (the probes read their own cursor-position replies).
pub fn init(mouse: bool, zoom: ZoomHandle) -> io::Result<AppTerminal> {
    // `try_init` = panic hook + raw mode + alternate screen + a `DefaultTerminal` we
    // don't want. Drop the terminal (it has no teardown Drop) and rebuild on the zoom
    // backend, keeping ratatui's hook/raw-mode/alt-screen setup — and `ratatui::restore`
    // in `restore()` — exactly as they were.
    drop(ratatui::try_init()?);
    let terminal = Terminal::new(ZoomBackend::new(
        CrosstermBackend::new(PanicSafeBufWriter::new(io::stdout())),
        zoom.clone(),
    ))?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    // Ask the terminal to report focus in/out (DECSET ?1004) so the reducer can park animations
    // while we're hidden. Independent of mouse capture; a no-op on terminals that don't support
    // it (they simply never send the events, and `App.focused` stays `true`). Safe to enable
    // before the input flush below — focus is reported only on *transitions*, not as a backlog.
    let _ = execute!(io::stdout(), EnableFocusChange);
    enable_keyboard_enhancement();
    zoom.set_mode(crate::zoom::detect_mode());
    // Discard any input already queued by terminal setup — chiefly leftover bytes from the
    // graphics/keyboard capability probes (DA1 `\e[?...c`, cell-size `\e[...t`, kitty APC) that
    // would otherwise be mis-parsed as key/mouse events the moment the event loop starts.
    // Runs after the zoom-mode probes so a late/partial CPR reply can't ghost into the
    // event loop either.
    flush_pending_input();
    Ok(terminal)
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
    with_synchronized_update(&mut io::stdout(), |_| draw_frame_inner(terminal, render))
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
    draw_frame_with_output(
        terminal,
        &mut io::stdout(),
        synchronized,
        clear_before,
        render,
    )
}

fn draw_frame_with_output<B, W, F>(
    terminal: &mut Terminal<B>,
    output: &mut W,
    synchronized: bool,
    clear_before: bool,
    render: F,
) -> io::Result<()>
where
    B: Backend<Error = io::Error>,
    W: Write,
    F: FnOnce(&mut Frame),
{
    if synchronized {
        if !clear_before {
            return with_synchronized_update(output, |_| draw_frame_inner(terminal, render));
        }
        return with_synchronized_update(output, |output| {
            write_vt_clear_for_native_images(output)?;
            draw_frame_after_explicit_clear(terminal, render)
        });
    }
    if clear_before {
        write_vt_clear_for_native_images(output)?;
        return draw_frame_after_explicit_clear(terminal, render);
    }
    draw_frame_inner(terminal, render)
}

fn write_vt_clear_for_native_images(output: &mut impl Write) -> io::Result<()> {
    // Clear native terminal graphics directly. We deliberately do not use `Terminal::clear()` for
    // this path: ratatui preserves the cursor by querying crossterm's cursor position, which can
    // race the event stream on Unix and fail after a 2s ESC[6n timeout during image-heavy redraws.
    output.write_all(b"\x1b[2J\x1b[H")?;
    output.flush()
}

fn with_synchronized_update<W, T>(
    output: &mut W,
    operation: impl FnOnce(&mut W) -> io::Result<T>,
) -> io::Result<T>
where
    W: Write,
{
    let _ = execute!(output, BeginSynchronizedUpdate);
    let result = operation(output);
    let _ = execute!(output, EndSynchronizedUpdate);
    result
}

/// Re-emit ratatui's exact successful unchanged-frame terminal sequence without invoking the UI
/// render callback or swapping its buffers. In particular, this deliberately does not call
/// `Terminal::flush()`: after a successful full draw ratatui's current buffer is the reset/blank
/// one, so diffing it against the displayed previous buffer can emit cells that erase the UI.
fn scrub_unchanged_terminal<B>(terminal: &mut Terminal<B>) -> Result<(), B::Error>
where
    B: Backend,
{
    terminal.backend_mut().draw(std::iter::empty())?;
    terminal.hide_cursor()?;
    terminal.backend_mut().flush()
}

fn fullscreen_size_changed<B>(terminal: &mut Terminal<B>) -> Result<bool, B::Error>
where
    B: Backend,
{
    Ok(terminal.size()? != terminal.get_frame().area().as_size())
}

fn scrub_ime_preedit_with_output<B, W>(
    terminal: &mut Terminal<B>,
    output: &mut W,
    synchronized: bool,
    fullscreen: bool,
) -> io::Result<ImeScrubResult>
where
    B: Backend<Error = io::Error>,
    W: Write,
{
    // `Terminal::autoresize()` can clear and flush the visible surface. Merely observe the
    // backend here; the immediately-following normal full draw performs autoresize inside its
    // synchronized-update wrapper. Fixed viewports deliberately opt out, matching ratatui.
    if fullscreen && fullscreen_size_changed(terminal)? {
        return Ok(ImeScrubResult::Resized);
    }
    if synchronized {
        with_synchronized_update(output, |_| scrub_unchanged_terminal(terminal))?;
    } else {
        scrub_unchanged_terminal(terminal)?;
    }
    Ok(ImeScrubResult::Fast)
}

/// Scrub terminal-owned IME preedit while preserving the exact output stream of an unchanged full
/// draw. A detected resize is reported before any fast-path output so the caller can immediately
/// perform the normal full render.
pub fn scrub_ime_preedit(
    terminal: &mut AppTerminal,
    synchronized: bool,
) -> io::Result<ImeScrubResult> {
    // `init` always constructs this AppTerminal with ratatui's fullscreen viewport.
    scrub_ime_preedit_with_output(terminal, &mut io::stdout(), synchronized, true)
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
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use ratatui::backend::{Backend, ClearType, CrosstermBackend, TestBackend, WindowSize};
    use ratatui::buffer::Cell;
    use ratatui::layout::{Position, Rect, Size};
    use ratatui::style::{Color, Modifier};
    use ratatui::widgets::Paragraph;
    use ratatui::{Terminal, TerminalOptions, Viewport};

    use super::{
        ImeScrubResult, PanicSafeBufWriter, draw_frame_after_explicit_clear, draw_frame_inner,
        draw_frame_with_output, scrub_ime_preedit_with_output, scrub_unchanged_terminal,
    };
    use crate::zoom::{ZoomBackend, ZoomHandle, ZoomMode};

    /// Shared byte sink: the terminal backend and synchronized-update writer use clones so tests
    /// observe their real interleaving in one stream.
    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl CaptureWriter {
        fn clear(&self) {
            self.0.lock().unwrap().clear();
        }

        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct WriteStats {
        bytes: Vec<u8>,
        write_calls: usize,
        flush_calls: usize,
    }

    /// Models the writer directly underneath `BufWriter` and records only non-empty writes. In
    /// production these are entries into stdout's shared writer; any further syscall batching is
    /// an implementation detail of `Stdout` rather than an invariant asserted by these tests.
    #[derive(Clone, Default)]
    struct CountingWriter(Arc<Mutex<WriteStats>>);

    impl CountingWriter {
        fn snapshot(&self) -> WriteStats {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for CountingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !buf.is_empty() {
                let mut stats = self.0.lock().unwrap();
                stats.bytes.extend_from_slice(buf);
                stats.write_calls += 1;
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.lock().unwrap().flush_calls += 1;
            Ok(())
        }
    }

    #[test]
    fn panic_safe_buffered_writer_flushes_pending_bytes_on_normal_drop() {
        let sink = CountingWriter::default();
        {
            let mut writer = PanicSafeBufWriter::new(sink.clone());
            writer.write_all(b"normal remainder").unwrap();
            assert_eq!(sink.snapshot(), WriteStats::default());
        }

        let stats = sink.snapshot();
        assert_eq!(stats.bytes, b"normal remainder");
        assert_eq!(stats.write_calls, 1);
    }

    #[test]
    fn panic_safe_buffered_writer_discards_pending_bytes_during_unwind() {
        let sink = CountingWriter::default();
        let panic_sink = sink.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut writer = PanicSafeBufWriter::new(panic_sink.clone());
            writer.write_all(b"panic remainder").unwrap();
            assert_eq!(panic_sink.snapshot(), WriteStats::default());
            panic!("exercise panic-safe writer drop");
        }));

        assert!(result.is_err());
        assert_eq!(sink.snapshot(), WriteStats::default());
    }

    fn normal_scale_backend<W: Write>(writer: W) -> ZoomBackend<W> {
        ZoomBackend::new(CrosstermBackend::new(writer), ZoomHandle::default())
    }

    fn representative_cells() -> Vec<(u16, u16, Cell)> {
        let symbols = [
            "A",
            "한",
            "b",
            "界",
            "\x1b_Gi=7,a=q\x1b\\",
            "c",
            "🙂",
            "d",
            "é",
            "Z",
        ];
        symbols
            .into_iter()
            .enumerate()
            .map(|(index, symbol)| {
                let mut cell = Cell::default();
                cell.set_symbol(symbol);
                cell.fg = if index % 2 == 0 {
                    Color::Rgb(220, 80, 120)
                } else {
                    Color::Cyan
                };
                cell.bg = if index % 3 == 0 {
                    Color::Rgb(10, 20, 30)
                } else {
                    Color::Reset
                };
                cell.underline_color = if index % 4 == 0 {
                    Color::Yellow
                } else {
                    Color::Reset
                };
                cell.modifier = match index % 4 {
                    0 => Modifier::BOLD,
                    1 => Modifier::ITALIC,
                    2 => Modifier::UNDERLINED | Modifier::REVERSED,
                    _ => Modifier::empty(),
                };
                (index as u16 * 2, (index / 5) as u16, cell)
            })
            .collect()
    }

    fn draw_representative_cells<W: Write>(backend: &mut ZoomBackend<W>) {
        let cells = representative_cells();
        backend
            .draw(cells.iter().map(|(x, y, cell)| (*x, *y, cell)))
            .unwrap();
    }

    #[test]
    fn buffered_normal_scale_output_matches_unbuffered_bytes() {
        let direct_sink = CountingWriter::default();
        let mut direct = normal_scale_backend(direct_sink.clone());
        draw_representative_cells(&mut direct);
        direct.flush().unwrap();

        let buffered_sink = CountingWriter::default();
        let mut buffered = normal_scale_backend(PanicSafeBufWriter::new(buffered_sink.clone()));
        draw_representative_cells(&mut buffered);
        buffered.flush().unwrap();

        let direct = direct_sink.snapshot();
        let buffered = buffered_sink.snapshot();
        assert_eq!(buffered.bytes, direct.bytes);
        let output = String::from_utf8(buffered.bytes).unwrap();
        assert!(output.contains("한"), "wide glyph must reach the writer");
        assert!(
            output.contains("\x1b_Gi=7,a=q\x1b\\"),
            "raw image escape must reach the writer verbatim"
        );
        assert!(
            output.contains("\x1b["),
            "styled cells must emit terminal style escapes"
        );
    }

    #[test]
    fn buffered_normal_scale_coalesces_writes_until_backend_flush() {
        let direct_sink = CountingWriter::default();
        let mut direct = normal_scale_backend(direct_sink.clone());
        draw_representative_cells(&mut direct);
        direct.flush().unwrap();
        let direct = direct_sink.snapshot();

        let buffered_sink = CountingWriter::default();
        let mut buffered = normal_scale_backend(PanicSafeBufWriter::new(buffered_sink.clone()));
        draw_representative_cells(&mut buffered);
        assert_eq!(
            buffered_sink.snapshot(),
            WriteStats::default(),
            "a sub-capacity frame must remain pending until ratatui's backend flush boundary"
        );
        buffered.flush().unwrap();
        let buffered = buffered_sink.snapshot();

        assert_eq!(buffered.bytes, direct.bytes);
        assert_eq!(buffered.flush_calls, 1);
        assert_eq!(direct.flush_calls, 1);
        assert_eq!(
            buffered.write_calls, 1,
            "the representative frame fits in one BufWriter batch"
        );
        assert!(
            buffered.write_calls * 4 <= direct.write_calls,
            "expected at least a 4x reduction: buffered={}, direct={}",
            buffered.write_calls,
            direct.write_calls
        );
    }

    #[test]
    fn terminal_draw_drains_the_reusable_buffer_at_each_frame_boundary() {
        let sink = CountingWriter::default();
        let backend = normal_scale_backend(PanicSafeBufWriter::new(sink.clone()));
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 8, 1)),
            },
        )
        .unwrap();

        let initialized = sink.snapshot();
        terminal.draw(|frame| render_text(frame, "first")).unwrap();
        let first = sink.snapshot();
        assert!(first.flush_calls > initialized.flush_calls);
        assert_eq!(first.write_calls, initialized.write_calls + 1);
        assert!(first.bytes.len() > initialized.bytes.len());

        terminal.draw(|frame| render_text(frame, "second")).unwrap();
        let second = sink.snapshot();
        assert!(second.flush_calls > first.flush_calls);
        assert_eq!(second.write_calls, first.write_calls + 1);
        assert!(second.bytes.len() > first.bytes.len());
    }

    struct IoTestBackend {
        inner: TestBackend,
        draw_calls: usize,
        clear_calls: usize,
        flush_calls: usize,
    }

    impl IoTestBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: TestBackend::new(width, height),
                draw_calls: 0,
                clear_calls: 0,
                flush_calls: 0,
            }
        }

        fn resize(&mut self, width: u16, height: u16) {
            self.inner.resize(width, height);
        }

        fn reset_operations(&mut self) {
            self.draw_calls = 0;
            self.clear_calls = 0;
            self.flush_calls = 0;
        }

        fn into_io<T>(result: Result<T, Infallible>) -> io::Result<T> {
            match result {
                Ok(value) => Ok(value),
                Err(error) => match error {},
            }
        }
    }

    impl Backend for IoTestBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.draw_calls += 1;
            Self::into_io(self.inner.draw(content))
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            Self::into_io(self.inner.hide_cursor())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            Self::into_io(self.inner.show_cursor())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Self::into_io(self.inner.get_cursor_position())
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            Self::into_io(self.inner.set_cursor_position(position))
        }

        fn clear(&mut self) -> io::Result<()> {
            self.clear_calls += 1;
            Self::into_io(self.inner.clear())
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            self.clear_calls += 1;
            Self::into_io(self.inner.clear_region(clear_type))
        }

        fn size(&self) -> io::Result<Size> {
            Self::into_io(self.inner.size())
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Self::into_io(self.inner.window_size())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_calls += 1;
            Self::into_io(self.inner.flush())
        }
    }

    fn capture_terminal(percent: u16) -> (Terminal<ZoomBackend<CaptureWriter>>, CaptureWriter) {
        let sink = CaptureWriter::default();
        let zoom = ZoomHandle::default();
        zoom.set_mode(ZoomMode::Osc66);
        zoom.set(percent);
        let backend = ZoomBackend::new(CrosstermBackend::new(sink.clone()), zoom);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 5, 1)),
            },
        )
        .unwrap();
        (terminal, sink)
    }

    fn render_text(frame: &mut ratatui::Frame, text: &'static str) {
        frame.render_widget(Paragraph::new(text), frame.area());
    }

    #[test]
    fn ime_fast_path_matches_unchanged_full_draw_bytes_with_and_without_sync() {
        for (percent, synchronized) in [100, 200]
            .into_iter()
            .flat_map(|percent| [false, true].map(move |synchronized| (percent, synchronized)))
        {
            let (mut full, full_sink) = capture_terminal(percent);
            let mut full_output = full_sink.clone();
            draw_frame_with_output(&mut full, &mut full_output, false, false, |frame| {
                render_text(frame, "abc");
            })
            .unwrap();
            full_sink.clear();
            draw_frame_with_output(&mut full, &mut full_output, synchronized, false, |frame| {
                render_text(frame, "abc")
            })
            .unwrap();
            let expected = full_sink.bytes();

            let (mut fast, fast_sink) = capture_terminal(percent);
            let mut fast_output = fast_sink.clone();
            draw_frame_with_output(&mut fast, &mut fast_output, false, false, |frame| {
                render_text(frame, "abc");
            })
            .unwrap();
            fast_sink.clear();
            assert_eq!(
                scrub_ime_preedit_with_output(&mut fast, &mut fast_output, synchronized, false,)
                    .unwrap(),
                ImeScrubResult::Fast
            );
            let actual = fast_sink.bytes();

            assert!(!expected.is_empty());
            assert_eq!(
                actual, expected,
                "percent={percent}, synchronized={synchronized}"
            );
        }
    }

    #[test]
    fn fast_scrubs_preserve_the_next_changed_frame_buffer_result() {
        let mut fast = Terminal::new(TestBackend::new(5, 1)).unwrap();
        let mut full = Terminal::new(TestBackend::new(5, 1)).unwrap();
        for terminal in [&mut fast, &mut full] {
            draw_frame_inner(terminal, |frame| render_text(frame, "A")).unwrap();
        }

        for _ in 0..4 {
            scrub_unchanged_terminal(&mut fast).unwrap();
            draw_frame_inner(&mut full, |frame| render_text(frame, "A")).unwrap();
        }
        draw_frame_inner(&mut fast, |frame| render_text(frame, "B")).unwrap();
        draw_frame_inner(&mut full, |frame| render_text(frame, "B")).unwrap();

        assert_eq!(fast.backend().buffer(), full.backend().buffer());
        fast.backend().assert_buffer_lines(["B    "]);
    }

    #[test]
    fn fast_scrubs_preserve_the_next_changed_frame_byte_stream() {
        for percent in [100, 200] {
            let (mut fast, fast_sink) = capture_terminal(percent);
            let (mut full, full_sink) = capture_terminal(percent);
            let mut fast_output = fast_sink.clone();
            let mut full_output = full_sink.clone();
            for (terminal, output) in [(&mut fast, &mut fast_output), (&mut full, &mut full_output)]
            {
                draw_frame_with_output(terminal, output, false, false, |frame| {
                    render_text(frame, "A")
                })
                .unwrap();
            }

            for _ in 0..4 {
                scrub_ime_preedit_with_output(&mut fast, &mut fast_output, false, false).unwrap();
                draw_frame_with_output(&mut full, &mut full_output, false, false, |frame| {
                    render_text(frame, "A")
                })
                .unwrap();
            }

            fast_sink.clear();
            full_sink.clear();
            draw_frame_with_output(&mut fast, &mut fast_output, false, false, |frame| {
                render_text(frame, "B")
            })
            .unwrap();
            draw_frame_with_output(&mut full, &mut full_output, false, false, |frame| {
                render_text(frame, "B")
            })
            .unwrap();

            assert_eq!(fast_sink.bytes(), full_sink.bytes(), "percent={percent}");
        }
    }

    #[test]
    fn resize_is_reported_without_output_before_synced_full_draw_autoresizes() {
        let mut terminal = Terminal::new(IoTestBackend::new(5, 1)).unwrap();
        draw_frame_inner(&mut terminal, |frame| render_text(frame, "abc")).unwrap();
        terminal.backend_mut().resize(7, 2);
        terminal.backend_mut().reset_operations();
        let area_before = terminal.get_frame().area();
        let mut output = Vec::new();

        assert_eq!(
            scrub_ime_preedit_with_output(&mut terminal, &mut output, true, true).unwrap(),
            ImeScrubResult::Resized
        );
        assert!(output.is_empty());
        assert_eq!(terminal.get_frame().area(), area_before);
        assert_eq!(terminal.backend().draw_calls, 0);
        assert_eq!(terminal.backend().clear_calls, 0);
        assert_eq!(terminal.backend().flush_calls, 0);

        draw_frame_with_output(&mut terminal, &mut output, true, false, |frame| {
            render_text(frame, "changed")
        })
        .unwrap();
        assert_eq!(terminal.get_frame().area(), Rect::new(0, 0, 7, 2));
        assert_eq!(terminal.backend().clear_calls, 1);
        assert!(terminal.backend().draw_calls > 0);
        assert!(terminal.backend().flush_calls > 0);
        assert_eq!(output, b"\x1b[?2026h\x1b[?2026l");
    }

    #[test]
    fn fixed_viewport_fast_scrub_ignores_backend_size_changes() {
        let fixed = Rect::new(1, 0, 3, 1);
        let mut terminal = Terminal::with_options(
            IoTestBackend::new(5, 1),
            TerminalOptions {
                viewport: Viewport::Fixed(fixed),
            },
        )
        .unwrap();
        draw_frame_inner(&mut terminal, |frame| render_text(frame, "abc")).unwrap();
        terminal.backend_mut().resize(7, 2);
        terminal.backend_mut().reset_operations();

        assert_eq!(
            scrub_ime_preedit_with_output(&mut terminal, &mut Vec::new(), false, false).unwrap(),
            ImeScrubResult::Fast
        );
        assert_eq!(terminal.get_frame().area(), fixed);
        assert_eq!(terminal.backend().clear_calls, 0);
        assert_eq!(terminal.backend().draw_calls, 1);
        assert_eq!(terminal.backend().flush_calls, 1);
    }

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
