use std::cell::RefCell;
use std::convert::Infallible;
use std::io::{self, Write};
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier};
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};

use super::{
    ACTIVE_KEYBOARD_KITTY, ACTIVE_KEYBOARD_NONE, ACTIVE_KEYBOARD_WIN32, DISABLE_WIN32_INPUT,
    ENABLE_WIN32_INPUT, ImeScrubResult, PanicSafeBufWriter, disable_keyboard_input_with,
    draw_frame_after_explicit_clear, draw_frame_inner, draw_frame_with_output,
    scrub_ime_preedit_with_output, scrub_unchanged_terminal, select_keyboard_input_mode,
    write_win32_input_sequence,
};
#[cfg(unix)]
use super::{
    AppTerminal, enable_kitty_keyboard_with_state, restore_terminal_state_with, run_bounded_restore,
};
use crate::terminal_keyboard::{KeyboardInputMode, KeyboardInputPlan};
use crate::zoom::{ZoomBackend, ZoomHandle, ZoomMode};

#[test]
fn keyboard_negotiation_prefers_kitty_and_uses_win32_only_as_fallback() {
    let calls = RefCell::new(Vec::new());
    let mode = select_keyboard_input_mode(
        KeyboardInputPlan::for_test(false, true, true),
        || {
            calls.borrow_mut().push("probe");
            true
        },
        || {
            calls.borrow_mut().push("kitty");
            true
        },
        || {
            calls.borrow_mut().push("win32");
            true
        },
    );
    assert_eq!(mode, KeyboardInputMode::Kitty);
    assert_eq!(*calls.borrow(), ["probe", "kitty"]);

    calls.borrow_mut().clear();
    let mode = select_keyboard_input_mode(
        KeyboardInputPlan::for_test(false, true, true),
        || {
            calls.borrow_mut().push("probe");
            false
        },
        || {
            calls.borrow_mut().push("kitty");
            true
        },
        || {
            calls.borrow_mut().push("win32");
            true
        },
    );
    assert_eq!(mode, KeyboardInputMode::Win32Input);
    assert_eq!(*calls.borrow(), ["probe", "win32"]);

    calls.borrow_mut().clear();
    let mode = select_keyboard_input_mode(
        KeyboardInputPlan::for_test(false, true, true),
        || {
            calls.borrow_mut().push("probe");
            true
        },
        || {
            calls.borrow_mut().push("kitty");
            false
        },
        || {
            calls.borrow_mut().push("win32");
            true
        },
    );
    assert_eq!(mode, KeyboardInputMode::Win32Input);
    assert_eq!(*calls.borrow(), ["probe", "kitty", "win32"]);
}

#[cfg(unix)]
#[test]
fn active_restore_writes_all_terminal_controls_around_raw_mode_restore() {
    let keyboard = AtomicU8::new(ACTIVE_KEYBOARD_KITTY);
    let mut output = Vec::new();
    let mut raw_mode_disabled = false;
    restore_terminal_state_with(&keyboard, &mut output, true, true, || {
        raw_mode_disabled = true;
        Ok(())
    })
    .unwrap();
    assert!(raw_mode_disabled);
    assert_eq!(keyboard.load(std::sync::atomic::Ordering::Acquire), 0);
    let output = String::from_utf8(output).unwrap();
    for sequence in
        "\x1b[<1u|\x1b[?2026l|\x1b[?1004l|\x1b[?2004l|\x1b[?1000l|\x1b[?25h|\x1b[?1049l".split('|')
    {
        assert!(
            output.contains(sequence),
            "missing {sequence:?} in {output:?}"
        );
    }
}

// Asserts the Kitty push/pop bytes directly. On Windows the console, not the writer, decides
// between ANSI bytes and the winapi fallback, and the legacy console reports the pop as
// Unsupported, so those byte assertions only describe Unix behavior.
#[cfg(unix)]
#[test]
fn failed_keyboard_enable_flush_keeps_the_marker_for_restore() {
    struct FlushFailWriter {
        bytes: Vec<u8>,
        fail_flush: bool,
    }
    impl Write for FlushFailWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "synthetic flush deadline",
                ))
            } else {
                Ok(())
            }
        }
    }
    let active = AtomicU8::new(ACTIVE_KEYBOARD_NONE);
    let mut output = FlushFailWriter {
        bytes: Vec::new(),
        fail_flush: true,
    };
    assert!(!enable_kitty_keyboard_with_state(&active, &mut output));
    assert_eq!(
        active.load(std::sync::atomic::Ordering::Acquire),
        ACTIVE_KEYBOARD_KITTY
    );

    output.fail_flush = false;
    disable_keyboard_input_with(&active, &mut output).unwrap();
    let bytes = String::from_utf8(output.bytes).unwrap();
    assert!(bytes.contains("\x1b[>7u"), "missing Kitty push: {bytes:?}");
    assert!(bytes.contains("\x1b[<1u"), "missing Kitty pop: {bytes:?}");
}

#[cfg(unix)]
#[test]
fn keyboard_cleanup_marker_tracks_command_delivery_not_the_final_flush() {
    struct ControlledWriter {
        bytes: Vec<u8>,
        fail_write: bool,
        fail_flush: bool,
    }

    impl Write for ControlledWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.fail_write {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "restore generation was replaced before the write",
                ));
            }
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "restore generation was replaced after the write",
                ))
            } else {
                Ok(())
            }
        }
    }

    let not_delivered = AtomicU8::new(ACTIVE_KEYBOARD_KITTY);
    let mut failed_write = ControlledWriter {
        bytes: Vec::new(),
        fail_write: true,
        fail_flush: false,
    };
    disable_keyboard_input_with(&not_delivered, &mut failed_write).unwrap_err();
    assert_eq!(
        not_delivered.load(std::sync::atomic::Ordering::Acquire),
        ACTIVE_KEYBOARD_KITTY,
        "a takeover must be able to retry a command that was never delivered"
    );

    let delivered = AtomicU8::new(ACTIVE_KEYBOARD_KITTY);
    let mut failed_flush = ControlledWriter {
        bytes: Vec::new(),
        fail_write: false,
        fail_flush: true,
    };
    disable_keyboard_input_with(&delivered, &mut failed_flush).unwrap_err();
    assert_eq!(failed_flush.bytes, b"\x1b[<1u");
    assert_eq!(
        delivered.load(std::sync::atomic::Ordering::Acquire),
        ACTIVE_KEYBOARD_NONE,
        "a repeat restore must not pop the Kitty stack after the bytes were delivered"
    );
}

#[cfg(unix)]
#[test]
fn concurrent_keyboard_cleanup_claim_prevents_a_duplicate_kitty_pop() {
    struct BlockingWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        entered: Option<std::sync::mpsc::SyncSender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            self.bytes.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let keyboard = Arc::new(AtomicU8::new(ACTIVE_KEYBOARD_KITTY));
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let worker_keyboard = Arc::clone(&keyboard);
    let worker_bytes = Arc::clone(&bytes);
    let worker = std::thread::spawn(move || {
        disable_keyboard_input_with(
            worker_keyboard.as_ref(),
            &mut BlockingWriter {
                bytes: worker_bytes,
                entered: Some(entered_tx),
                release: release_rx,
            },
        )
    });

    entered_rx.recv().unwrap();
    let mut takeover_output = Vec::new();
    let takeover = disable_keyboard_input_with(keyboard.as_ref(), &mut takeover_output)
        .expect_err("a takeover must wait for the claimed cleanup command");
    assert_eq!(takeover.kind(), io::ErrorKind::WouldBlock);
    assert!(takeover_output.is_empty());

    release_tx.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert_eq!(*bytes.lock().unwrap(), b"\x1b[<1u");
    assert_eq!(
        keyboard.load(std::sync::atomic::Ordering::Acquire),
        ACTIVE_KEYBOARD_NONE
    );
}

#[cfg(unix)]
#[test]
fn restore_reports_output_failure_but_still_attempts_raw_mode_and_later_controls() {
    struct FailingWriter {
        attempts: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            self.attempts += 1;
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "synthetic bounded restore timeout",
            ))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let keyboard = AtomicU8::new(ACTIVE_KEYBOARD_KITTY);
    let mut output = FailingWriter { attempts: 0 };
    let mut raw_mode_attempted = false;
    let error = restore_terminal_state_with(&keyboard, &mut output, true, true, || {
        raw_mode_attempted = true;
        Ok(())
    })
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(raw_mode_attempted);
    assert_eq!(
        keyboard.load(std::sync::atomic::Ordering::Acquire),
        ACTIVE_KEYBOARD_KITTY,
        "a failed control write must preserve the cleanup obligation"
    );
    assert!(
        output.attempts >= 6,
        "restore stopped after its first failed control write"
    );
}

#[cfg(unix)]
#[test]
fn raw_mode_restore_cannot_hold_the_caller_past_its_wall_clock_budget() {
    let started = std::time::Instant::now();
    let error = run_bounded_restore(
        "synthetic blocked raw-mode restore",
        std::time::Duration::from_millis(25),
        || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(())
        },
    )
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(
        started.elapsed() < std::time::Duration::from_millis(150),
        "bounded restore waited for its blocked worker"
    );
}

#[test]
fn native_and_legacy_plans_do_not_run_unnecessary_protocol_operations() {
    let calls = RefCell::new(0);
    let native = select_keyboard_input_mode(
        KeyboardInputPlan::for_test(true, true, true),
        || {
            *calls.borrow_mut() += 1;
            true
        },
        || {
            *calls.borrow_mut() += 1;
            true
        },
        || {
            *calls.borrow_mut() += 1;
            true
        },
    );
    assert_eq!(native, KeyboardInputMode::Native);
    assert_eq!(*calls.borrow(), 0);

    let legacy = select_keyboard_input_mode(
        KeyboardInputPlan::for_test(false, false, false),
        || {
            *calls.borrow_mut() += 1;
            true
        },
        || {
            *calls.borrow_mut() += 1;
            true
        },
        || {
            *calls.borrow_mut() += 1;
            true
        },
    );
    assert_eq!(legacy, KeyboardInputMode::Legacy);
    assert_eq!(*calls.borrow(), 0);
}

#[test]
fn keyboard_protocol_cleanup_emits_the_matching_inverse_once() {
    // The legacy Windows console has no progressive-enhancement support, so popping the Kitty
    // flags is reported as Unsupported there. The cleanup contract under test is the state
    // transition and the WIN32 inverse below, both of which still have to hold.
    fn expect_cleanup(result: io::Result<()>) {
        match result {
            Ok(()) => {}
            Err(error) if cfg!(windows) && error.kind() == io::ErrorKind::Unsupported => {}
            Err(error) => panic!("unexpected keyboard cleanup failure: {error}"),
        }
    }

    let kitty = AtomicU8::new(ACTIVE_KEYBOARD_KITTY);
    let mut kitty_output = Vec::new();
    expect_cleanup(disable_keyboard_input_with(&kitty, &mut kitty_output));
    expect_cleanup(disable_keyboard_input_with(&kitty, &mut kitty_output));
    assert_eq!(
        kitty.load(std::sync::atomic::Ordering::Relaxed),
        ACTIVE_KEYBOARD_NONE
    );
    // On Windows the console — not the writer — decides between ANSI bytes and the winapi
    // fallback (`Command::is_ansi_code_supported`), so a headless runner captures nothing.
    #[cfg(not(windows))]
    assert_eq!(kitty_output, b"\x1b[<1u");
    #[cfg(windows)]
    assert!(kitty_output.is_empty() || kitty_output == b"\x1b[<1u");

    let win32 = AtomicU8::new(ACTIVE_KEYBOARD_WIN32);
    let mut win32_output = Vec::new();
    disable_keyboard_input_with(&win32, &mut win32_output).unwrap();
    disable_keyboard_input_with(&win32, &mut win32_output).unwrap();
    assert_eq!(
        win32.load(std::sync::atomic::Ordering::Relaxed),
        ACTIVE_KEYBOARD_NONE
    );
    assert_eq!(win32_output, DISABLE_WIN32_INPUT);
}

#[test]
fn win32_input_mode_uses_dec_private_mode_9001() {
    let mut output = Vec::new();
    write_win32_input_sequence(&mut output, ENABLE_WIN32_INPUT).unwrap();
    write_win32_input_sequence(&mut output, DISABLE_WIN32_INPUT).unwrap();
    assert_eq!(output, b"\x1b[?9001h\x1b[?9001l");
}

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

struct CursorWriteOnDrop<W: Write>(PanicSafeBufWriter<W>);

impl<W: Write> Drop for CursorWriteOnDrop<W> {
    fn drop(&mut self) {
        self.0
            .write_all(b"ratatui cursor restore during unwind")
            .unwrap();
        self.0.flush().unwrap();
    }
}

#[test]
fn panic_safe_buffered_writer_absorbs_destructor_output_during_unwind() {
    let sink = CountingWriter::default();
    let panic_sink = sink.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut writer = PanicSafeBufWriter::new(panic_sink);
        writer.write_all(b"failed frame remainder").unwrap();
        let _cursor_drop = CursorWriteOnDrop(writer);
        panic!("exercise unwind-time cursor output");
    }));

    assert!(result.is_err());
    assert_eq!(sink.snapshot(), WriteStats::default());
}

#[test]
#[cfg(unix)]
fn app_terminal_drop_silences_ratatui_cursor_output_and_discards_pending_frame() {
    let sink = CountingWriter::default();
    let drop_fence = Arc::new(AtomicBool::new(false));
    let output: Box<dyn Write + Send> = Box::new(sink.clone());
    let backend = normal_scale_backend(PanicSafeBufWriter::with_drop_fence(
        output,
        Arc::clone(&drop_fence),
    ));
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 8, 1)),
        },
    )
    .unwrap();
    let mut terminal = AppTerminal::new(terminal, drop_fence);

    terminal.hide_cursor().unwrap();
    let before_failed_frame = sink.snapshot();
    assert!(
        before_failed_frame
            .bytes
            .windows(6)
            .any(|bytes| bytes == b"\x1b[?25l")
    );

    let mut cell = Cell::default();
    cell.set_symbol("pending");
    terminal
        .backend_mut()
        .draw(std::iter::once((0, 0, &cell)))
        .unwrap();
    assert_eq!(sink.snapshot(), before_failed_frame);

    drop(terminal);
    assert_eq!(sink.snapshot(), before_failed_frame);
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
        for (terminal, output) in [(&mut fast, &mut fast_output), (&mut full, &mut full_output)] {
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

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
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
