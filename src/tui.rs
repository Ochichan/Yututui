//! Terminal setup/teardown. Unix uses an independently reopened, nonblocking TTY; Windows keeps
//! ratatui's native console setup and restoration.
//! Mouse capture is opt-in (config `mouse`, default on) and drives buttons + seekbar.
//! `player::lifetime` wraps the panic hook to kill mpv before restoring the terminal.

use std::io::{self, Write};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
#[cfg(unix)]
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableFocusChange,
    EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
#[cfg(unix)]
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::{Frame, Terminal};

use crate::terminal_keyboard::{KeyboardInputMode, KeyboardInputPlan};
#[cfg(unix)]
use crate::terminal_policy::{
    CPR_TOTAL_TIMEOUT as KEYBOARD_QUERY_MAX, EMERGENCY_RESTORE_TIMEOUT as EMERGENCY_RESTORE_BUDGET,
    NORMAL_RESTORE_TIMEOUT as NORMAL_RESTORE_BUDGET,
    STARTUP_OUTPUT_TIMEOUT as STARTUP_OUTPUT_BUDGET,
};
use crate::terminal_policy::{
    CPR_WRITE_TIMEOUT as CPR_WRITE_BUDGET, OWNER_OUTPUT_TIMEOUT as OWNER_OUTPUT_BUDGET,
};
use crate::zoom::{ZoomBackend, ZoomHandle};

mod terminal_io;
pub(crate) use terminal_io::{OutputOperationPhase, OutputOperationSnapshot, PreTuiOutput};

/// Reusable terminal output buffer that never leaks a partial frame after terminal restoration.
///
/// An unfenced normal drop retains `BufWriter`'s best-effort flush. [`AppTerminal`] arms its
/// instance fence before Ratatui runs its cursor-restoring destructor; panic unwinding is fenced
/// implicitly. In both cases pending bytes are detached and discarded instead of being emitted
/// after the bounded restore owner has taken over.
pub struct PanicSafeBufWriter<W: Write> {
    inner: Option<io::BufWriter<W>>,
    drop_fence: Arc<AtomicBool>,
}

impl<W: Write> PanicSafeBufWriter<W> {
    #[cfg(test)]
    fn new(writer: W) -> Self {
        Self::with_drop_fence(writer, Arc::new(AtomicBool::new(false)))
    }

    fn with_drop_fence(writer: W, drop_fence: Arc<AtomicBool>) -> Self {
        Self {
            inner: Some(io::BufWriter::new(writer)),
            drop_fence,
        }
    }

    fn output_is_fenced(&self) -> bool {
        std::thread::panicking() || self.drop_fence.load(Ordering::Acquire)
    }

    fn inner_mut(&mut self) -> &mut io::BufWriter<W> {
        self.inner
            .as_mut()
            .expect("buffered writer is present until drop")
    }
}

impl<W: Write> Write for PanicSafeBufWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.output_is_fenced() {
            return Ok(buf.len());
        }
        self.inner_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.output_is_fenced() {
            return Ok(());
        }
        self.inner_mut().flush()
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        if self.output_is_fenced() {
            return Ok(bufs.iter().map(|buf| buf.len()).sum());
        }
        self.inner_mut().write_vectored(bufs)
    }
}

impl<W: Write> Drop for PanicSafeBufWriter<W> {
    fn drop(&mut self) {
        if self.output_is_fenced()
            && let Some(buffered) = self.inner.take()
        {
            let (_writer, pending) = buffered.into_parts();
            drop(pending);
        }
    }
}

/// The inner terminal type owned by [`AppTerminal`].
#[doc(hidden)]
pub type AppTerminalInner =
    Terminal<ZoomBackend<PanicSafeBufWriter<Box<dyn Write + Send + 'static>>>>;

/// The app's terminal: a buffered [`CrosstermBackend`] wrapped in the OSC 66 text-zoom layer.
/// Ratatui's terminal flush remains the frame boundary while the reusable buffer coalesces the
/// backend's many small writes. At zoom 1 the zoom wrapper is a transparent pass-through.
///
/// Dropping this owner first fences its buffered backend. Ratatui's destructor may then update its
/// internal cursor state, but that update and any failed-frame remainder are acknowledged without
/// terminal I/O. [`restore`] is the sole physical teardown writer and retains its bounded,
/// cancellation-insensitive output generation.
pub struct AppTerminal {
    inner: AppTerminalInner,
    drop_fence: Arc<AtomicBool>,
}

impl AppTerminal {
    fn new(inner: AppTerminalInner, drop_fence: Arc<AtomicBool>) -> Self {
        Self { inner, drop_fence }
    }
}

impl Deref for AppTerminal {
    type Target = AppTerminalInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for AppTerminal {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for AppTerminal {
    fn drop(&mut self) {
        // This runs before Rust drops `inner`, so Ratatui's cursor-restoring Drop and the
        // BufWriter destructor cannot bypass the bounded restore owner or flush a failed frame.
        self.drop_fence.store(true, Ordering::Release);
    }
}

/// Outcome of the low-cost terminal-owned IME preedit scrub.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImeScrubResult {
    /// The unchanged terminal apply sequence completed successfully.
    Fast,
    /// The backend size differs from ratatui's current frame; render a full frame immediately.
    Resized,
}

const ACTIVE_KEYBOARD_NONE: u8 = 0;
const ACTIVE_KEYBOARD_KITTY: u8 = 1;
const ACTIVE_KEYBOARD_WIN32: u8 = 2;
const ACTIVE_KEYBOARD_KITTY_CLEANING: u8 = 3;
const ACTIVE_KEYBOARD_WIN32_CLEANING: u8 = 4;
static ACTIVE_KEYBOARD_PROTOCOL: AtomicU8 = AtomicU8::new(ACTIVE_KEYBOARD_NONE);
#[cfg(unix)]
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
static RAW_MODE_TRANSITION: Mutex<()> = Mutex::new(());
#[cfg(unix)]
static RAW_MODE_INHIBITED: AtomicBool = AtomicBool::new(false);

const ENABLE_WIN32_INPUT: &[u8] = b"\x1b[?9001h";
const DISABLE_WIN32_INPUT: &[u8] = b"\x1b[?9001l";

/// Initialise the terminal. When `mouse` is true, mouse events are captured.
///
/// Also detects the terminal's text-zoom mechanism into `zoom` — done here because the
/// probes must run after the alternate screen is entered (so probe glyphs land on a
/// throwaway screen) and before the exclusive terminal event worker starts reading stdin
/// (the probes read their own cursor-position replies).
pub fn init(mouse: bool, zoom: ZoomHandle) -> io::Result<(AppTerminal, KeyboardInputMode)> {
    #[cfg(unix)]
    return init_until(mouse, zoom, Instant::now() + STARTUP_OUTPUT_BUDGET);

    #[cfg(not(unix))]
    init_platform(mouse, zoom)
}

/// Share one Unix startup deadline; Windows retains its existing console initialization.
pub fn init_until(
    mouse: bool,
    zoom: ZoomHandle,
    deadline: Instant,
) -> io::Result<(AppTerminal, KeyboardInputMode)> {
    #[cfg(unix)]
    return init_unix(mouse, zoom, deadline);

    #[cfg(not(unix))]
    {
        let _ = deadline;
        init_platform(mouse, zoom)
    }
}

/// Validate the interactive input/output TTY pair before any probe changes termios or emits bytes.
pub fn preflight_interactive_terminal() -> io::Result<()> {
    terminal_io::preflight_interactive_terminal()
}

/// Enter raw mode only while a second-signal hard exit has not permanently inhibited new
/// terminal activation. Holding the transition mutex across the ioctl lets hard-exit restoration
/// run strictly after an already-admitted activation, never just before it.
pub(crate) fn enable_interactive_raw_mode() -> io::Result<()> {
    #[cfg(unix)]
    {
        let _transition = lock_raw_mode_transition();
        if RAW_MODE_INHIBITED.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "terminal raw-mode activation was inhibited by process shutdown",
            ));
        }
        crossterm::terminal::enable_raw_mode()
    }

    #[cfg(not(unix))]
    crossterm::terminal::enable_raw_mode()
}

#[cfg(unix)]
pub(crate) fn inhibit_raw_mode_transitions() {
    RAW_MODE_INHIBITED.store(true, Ordering::Release);
}

#[cfg(unix)]
pub(crate) fn with_raw_mode_transition<T>(operation: impl FnOnce() -> T) -> T {
    let _transition = lock_raw_mode_transition();
    operation()
}

#[cfg(unix)]
fn lock_raw_mode_transition() -> MutexGuard<'static, ()> {
    RAW_MODE_TRANSITION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(unix)]
fn init_unix(
    mouse: bool,
    zoom: ZoomHandle,
    deadline: Instant,
) -> io::Result<(AppTerminal, KeyboardInputMode)> {
    let writer = terminal_io::TerminalWriter::open_stdout()?;
    terminal_io::reset_active_output();
    let emergency = writer.emergency(EMERGENCY_RESTORE_BUDGET);
    let startup_budget = deadline.saturating_duration_since(Instant::now());
    // An expired negotiation budget must fail before raw mode or alternate-screen output.
    let _operation = writer.begin_operation("terminal startup", startup_budget)?;
    let mut control = writer.clone();

    enable_interactive_raw_mode()?;
    // Mark active first so a partial alternate-screen write still runs bounded restoration.
    TERMINAL_ACTIVE.store(true, Ordering::Release);
    if let Err(error) = execute!(control, EnterAlternateScreen) {
        let restored = bounded_restore_with(
            writer.emergency(NORMAL_RESTORE_BUDGET),
            mouse,
            "partial startup terminal restore",
            NORMAL_RESTORE_BUDGET,
        );
        return Err(merge_restore_error(error, restored));
    }
    install_terminal_panic_hook(mouse, emergency);

    let mut initialized: io::Result<(AppTerminal, KeyboardInputMode)> = (|| {
        let drop_fence = Arc::new(AtomicBool::new(false));
        let sink: Box<dyn Write + Send> = Box::new(writer.clone());
        let terminal = AppTerminal::new(
            Terminal::new(ZoomBackend::new(
                CrosstermBackend::new(PanicSafeBufWriter::with_drop_fence(
                    sink,
                    Arc::clone(&drop_fence),
                )),
                zoom.clone(),
            ))?,
            drop_fence,
        );
        if mouse {
            execute!(control, EnableMouseCapture)?;
        }
        execute!(control, DisableBracketedPaste)?;
        // Focus reports let the reducer park hidden animations; unsupported terminals ignore it.
        let _ = execute!(control, EnableFocusChange);
        let keyboard_mode = enable_keyboard_input_with(&mut control, deadline);
        zoom.set_mode(crate::zoom::detect_mode_with_output_until(
            &mut control,
            deadline,
        ));
        flush_pending_input_until(Some(deadline));
        // `TerminalWriter::flush` is also a no-syscall deadline check. Keep it immediately before
        // success so serial setup work cannot silently overrun the shared startup budget.
        control.flush()?;
        Ok((terminal, keyboard_mode))
    })();

    if initialized.is_err() {
        let restored = bounded_restore_with(
            writer.emergency(NORMAL_RESTORE_BUDGET),
            mouse,
            "startup terminal restore",
            NORMAL_RESTORE_BUDGET,
        );
        if let (Err(error), Err(restore_error)) = (&mut initialized, restored) {
            *error = merge_restore_error(
                io::Error::new(error.kind(), error.to_string()),
                Err(restore_error),
            );
        }
    }
    initialized
}

#[cfg(not(unix))]
fn init_platform(mouse: bool, zoom: ZoomHandle) -> io::Result<(AppTerminal, KeyboardInputMode)> {
    // `try_init` = panic hook + raw mode + alternate screen + a `DefaultTerminal` we
    // don't want. Drop the terminal (it has no teardown Drop) and rebuild on the zoom
    // backend, keeping ratatui's hook/raw-mode/alt-screen setup — and `ratatui::restore`
    // in `restore()` — exactly as they were.
    let default_terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            // `try_init` installs its panic hook before entering raw/alternate mode, but an I/O
            // error is returned rather than panicked. Best-effort restoration closes that gap.
            let _ = ratatui::try_restore();
            return Err(error);
        }
    };
    drop(default_terminal);
    install_keyboard_protocol_panic_hook();

    let writer = terminal_io::TerminalWriter::open_stdout()?;
    let sink: Box<dyn Write + Send> = Box::new(writer);

    let initialized = (|| {
        let drop_fence = Arc::new(AtomicBool::new(false));
        let terminal = AppTerminal::new(
            Terminal::new(ZoomBackend::new(
                CrosstermBackend::new(PanicSafeBufWriter::with_drop_fence(
                    sink,
                    Arc::clone(&drop_fence),
                )),
                zoom.clone(),
            ))?,
            drop_fence,
        );
        if mouse {
            execute!(io::stdout(), EnableMouseCapture)?;
        }
        // Ask the terminal to report focus in/out (DECSET ?1004) so the reducer can park
        // animations while we're hidden. Independent of mouse capture; a no-op on terminals that
        // don't support it (they simply never send the events, and `App.focused` stays `true`).
        let _ = execute!(io::stdout(), EnableFocusChange);
        let keyboard_mode = enable_keyboard_input();
        zoom.set_mode(crate::zoom::detect_mode());
        // Discard input buffered by setup probes before the exclusive event worker starts. This
        // also drains DA1 replies left by a terminal that declined the Kitty query.
        flush_pending_input_until(None);
        Ok((terminal, keyboard_mode))
    })();

    if initialized.is_err() {
        // `try_init` has already entered raw mode and the alternate screen. The caller cannot run
        // its ordinary teardown when this function returns Err, so restore here as well.
        let _ = restore(mouse);
    }
    initialized
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
    let mut output = terminal_io::active_writer()?;
    let _operation = output.begin_operation("synchronized frame", OWNER_OUTPUT_BUDGET)?;
    with_synchronized_update(&mut output, |_| draw_frame_inner(terminal, render))
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
    draw_frame_until(
        terminal,
        synchronized,
        clear_before,
        Instant::now() + OWNER_OUTPUT_BUDGET,
        render,
    )
}

pub(crate) fn draw_frame_until<F>(
    terminal: &mut AppTerminal,
    synchronized: bool,
    clear_before: bool,
    deadline: Instant,
    render: F,
) -> io::Result<()>
where
    F: FnOnce(&mut Frame),
{
    let mut output = terminal_io::active_writer()?;
    let _operation = output.begin_operation_until("terminal frame", deadline)?;
    draw_frame_with_output(terminal, &mut output, synchronized, clear_before, render)
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
    execute!(output, BeginSynchronizedUpdate)?;
    let result = operation(output);
    let ended = execute!(output, EndSynchronizedUpdate);
    match result {
        Err(error) => Err(error),
        Ok(value) => ended.map(|_| value),
    }
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
    scrub_ime_preedit_until(terminal, synchronized, Instant::now() + OWNER_OUTPUT_BUDGET)
}

pub(crate) fn scrub_ime_preedit_until(
    terminal: &mut AppTerminal,
    synchronized: bool,
    deadline: Instant,
) -> io::Result<ImeScrubResult> {
    // `init` always constructs this AppTerminal with ratatui's fullscreen viewport.
    let mut output = terminal_io::active_writer()?;
    let _operation = output.begin_operation_until("IME preedit scrub", deadline)?;
    scrub_ime_preedit_with_output(terminal, &mut output, synchronized, true)
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
fn flush_pending_input_until(deadline: Option<Instant>) {
    use std::time::Duration;
    for _ in 0..1024 {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
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
pub fn restore(mouse: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        if let Ok(writer) = terminal_io::active_writer() {
            return bounded_restore_with(
                writer.emergency(NORMAL_RESTORE_BUDGET),
                mouse,
                "normal terminal restore",
                NORMAL_RESTORE_BUDGET,
            );
        }
        bounded_raw_mode_restore("normal terminal restore", NORMAL_RESTORE_BUDGET)
    }

    #[cfg(not(unix))]
    {
        let mut first_error = disable_keyboard_input().err();
        remember_restore_error(
            &mut first_error,
            execute!(io::stdout(), DisableFocusChange, DisableBracketedPaste),
        );
        if mouse {
            remember_restore_error(
                &mut first_error,
                execute!(io::stdout(), DisableMouseCapture),
            );
        }
        ratatui::restore();
        first_error.map_or(Ok(()), Err)
    }
}

/// Synchronous second-signal restore: 150 ms on Unix, existing best-effort elsewhere.
pub(crate) fn emergency_restore(mouse: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        if let Ok(writer) = terminal_io::active_writer() {
            return bounded_restore_with(
                writer.emergency(EMERGENCY_RESTORE_BUDGET),
                mouse,
                "signal emergency restore",
                EMERGENCY_RESTORE_BUDGET,
            );
        }
        bounded_raw_mode_restore(
            "signal emergency terminal restore",
            EMERGENCY_RESTORE_BUDGET,
        )
    }

    #[cfg(not(unix))]
    restore(mouse)
}

#[cfg(not(unix))]
fn enable_keyboard_input() -> KeyboardInputMode {
    let plan = KeyboardInputPlan::detect();
    select_keyboard_input_mode(
        plan,
        || {
            matches!(
                crossterm::terminal::supports_keyboard_enhancement(),
                Ok(true)
            )
        },
        enable_kitty_keyboard,
        enable_win32_keyboard,
    )
}

#[cfg(unix)]
fn enable_keyboard_input_with(
    output: &mut terminal_io::TerminalWriter,
    deadline: Instant,
) -> KeyboardInputMode {
    let plan = KeyboardInputPlan::detect();
    if plan.native() {
        return KeyboardInputMode::Native;
    }
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .min(KEYBOARD_QUERY_MAX);
    if plan.probe_kitty()
        && !remaining.is_zero()
        && matches!(
            crossterm::terminal::supports_keyboard_enhancement_with_timeout(output, remaining),
            Ok(true)
        )
    {
        return if enable_kitty_keyboard_unix(output) {
            KeyboardInputMode::Kitty
        } else {
            // The push may have reached the terminal even when its final flush failed. Preserve
            // the Kitty marker for teardown instead of overwriting it with a Win32 fallback.
            KeyboardInputMode::Legacy
        };
    }
    if plan.win32_fallback() && enable_win32_keyboard_unix(output) {
        return KeyboardInputMode::Win32Input;
    }
    KeyboardInputMode::Legacy
}

#[cfg(any(test, not(unix)))]
fn select_keyboard_input_mode(
    plan: KeyboardInputPlan,
    mut kitty_supported: impl FnMut() -> bool,
    mut enable_kitty: impl FnMut() -> bool,
    mut enable_win32: impl FnMut() -> bool,
) -> KeyboardInputMode {
    if plan.native() {
        return KeyboardInputMode::Native;
    }
    if plan.probe_kitty() && kitty_supported() && enable_kitty() {
        return KeyboardInputMode::Kitty;
    }
    if plan.win32_fallback() && enable_win32() {
        return KeyboardInputMode::Win32Input;
    }
    KeyboardInputMode::Legacy
}

#[cfg(not(unix))]
fn enable_kitty_keyboard() -> bool {
    enable_kitty_keyboard_with(&mut io::stdout())
}

#[cfg(not(unix))]
fn enable_kitty_keyboard_with(output: &mut impl Write) -> bool {
    enable_kitty_keyboard_with_state(&ACTIVE_KEYBOARD_PROTOCOL, output)
}

#[cfg(any(test, not(unix)))]
fn enable_kitty_keyboard_with_state(active: &AtomicU8, output: &mut impl Write) -> bool {
    // Deliberately *without* REPORT_ALL_KEYS_AS_ESCAPE_CODES: under that flag kitty (and other
    // strict implementers) route every keystroke — including plain text — as an escape code and
    // turn off the terminal's IME, so Hangul/CJK jamo never compose into syllables in the search
    // and DJ Gem input boxes. (ghostty was lenient enough to keep composing, which is why this only
    // broke in kitty.) The remaining flags disambiguate modified keys without touching text input.
    // Ctrl+Shift character chords work when the terminal reports distinct enhanced key events;
    // legacy encodings may still collapse them to the matching Ctrl+key before we see them.
    let flags = kitty_keyboard_flags();
    // Non-Unix writers can buffer, so retain the cleanup marker before `execute!`: the control
    // may have been delivered even when the writer reports a later flush failure.
    active.store(ACTIVE_KEYBOARD_KITTY, Ordering::Release);
    if execute!(output, PushKeyboardEnhancementFlags(flags)).is_err() {
        return false;
    }
    true
}

fn kitty_keyboard_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
}

#[cfg(unix)]
fn enable_kitty_keyboard_unix(output: &mut terminal_io::TerminalWriter) -> bool {
    let mut command = Vec::new();
    if crossterm::queue!(
        command,
        PushKeyboardEnhancementFlags(kitty_keyboard_flags())
    )
    .is_err()
    {
        return false;
    }
    // Publish the cleanup marker under the same per-syscall fence as the final command byte.
    // Emergency generation replacement can therefore observe either no push or a retryable pop,
    // never a push that arrives after cleanup.
    if output
        .write_all_then(&command, || {
            ACTIVE_KEYBOARD_PROTOCOL.store(ACTIVE_KEYBOARD_KITTY, Ordering::Release);
        })
        .is_err()
    {
        return false;
    }
    output.flush().is_ok()
}

#[cfg(not(unix))]
fn enable_win32_keyboard() -> bool {
    enable_win32_keyboard_with(&mut io::stdout())
}

#[cfg(not(unix))]
fn enable_win32_keyboard_with(output: &mut impl Write) -> bool {
    // As above, preserve enough state to undo a control sequence whose final flush failed.
    ACTIVE_KEYBOARD_PROTOCOL.store(ACTIVE_KEYBOARD_WIN32, Ordering::Release);
    if write_win32_input_sequence(output, ENABLE_WIN32_INPUT).is_err() {
        return false;
    }
    true
}

#[cfg(unix)]
fn enable_win32_keyboard_unix(output: &mut terminal_io::TerminalWriter) -> bool {
    if output
        .write_all_then(ENABLE_WIN32_INPUT, || {
            ACTIVE_KEYBOARD_PROTOCOL.store(ACTIVE_KEYBOARD_WIN32, Ordering::Release);
        })
        .is_err()
    {
        return false;
    }
    output.flush().is_ok()
}

#[cfg(any(test, not(unix)))]
fn write_win32_input_sequence(output: &mut impl Write, sequence: &[u8]) -> io::Result<()> {
    output.write_all(sequence)?;
    output.flush()
}

#[cfg(not(unix))]
fn disable_keyboard_input() -> io::Result<()> {
    disable_keyboard_input_with(&ACTIVE_KEYBOARD_PROTOCOL, &mut io::stdout())
}

fn disable_keyboard_input_with(active: &AtomicU8, output: &mut impl Write) -> io::Result<()> {
    #[cfg(not(unix))]
    return match active.swap(ACTIVE_KEYBOARD_NONE, Ordering::AcqRel) {
        ACTIVE_KEYBOARD_KITTY => execute!(output, PopKeyboardEnhancementFlags).map(|_| ()),
        ACTIVE_KEYBOARD_WIN32 => write_win32_input_sequence(output, DISABLE_WIN32_INPUT),
        _ => Ok(()),
    };

    #[cfg(unix)]
    {
        let (protocol, cleaning) = loop {
            let protocol = active.load(Ordering::Acquire);
            let cleaning = match protocol {
                ACTIVE_KEYBOARD_KITTY => ACTIVE_KEYBOARD_KITTY_CLEANING,
                ACTIVE_KEYBOARD_WIN32 => ACTIVE_KEYBOARD_WIN32_CLEANING,
                ACTIVE_KEYBOARD_KITTY_CLEANING | ACTIVE_KEYBOARD_WIN32_CLEANING => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "terminal keyboard protocol cleanup is already in flight",
                    ));
                }
                _ => return Ok(()),
            };
            if active
                .compare_exchange(protocol, cleaning, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break (protocol, cleaning);
            }
        };
        let queued = match protocol {
            // `queue!` deliberately omits the final flush. Once all command bytes have been accepted
            // by the writer, a later replacement at the no-op TTY flush must not make a repeat signal
            // pop Kitty's stack twice. Conversely, a failed write retains the marker for takeover.
            ACTIVE_KEYBOARD_KITTY => crossterm::queue!(output, PopKeyboardEnhancementFlags),
            ACTIVE_KEYBOARD_WIN32 => output.write_all(DISABLE_WIN32_INPUT),
            _ => unreachable!("only active keyboard protocols can be claimed"),
        };
        if let Err(error) = queued {
            let _ =
                active.compare_exchange(cleaning, protocol, Ordering::AcqRel, Ordering::Acquire);
            return Err(error);
        }
        let _ = active.compare_exchange(
            cleaning,
            ACTIVE_KEYBOARD_NONE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        output.flush()
    }
}

#[cfg(not(unix))]
fn install_keyboard_protocol_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Installed outside ratatui's hook, so protocol state is popped while the terminal is
        // still raw/alternate. The later player hook wraps this one and kills media first.
        let _ = disable_keyboard_input();
        previous(info);
    }));
}

#[cfg(unix)]
fn emergency_restore_with(
    mut output: terminal_io::EmergencyWriter,
    mouse: bool,
    label: &'static str,
    deadline: Instant,
) -> io::Result<()> {
    terminal_io::cancel_active_output();
    let (_restore_operation, mut first_error) = match output.begin_operation_until(label, deadline)
    {
        Ok(operation) => {
            // Generation replacement prevents not-yet-started old writes. The fence additionally
            // waits for a syscall already admitted by the old generation and its protocol-marker
            // callback, so inverse controls follow the activation they undo.
            let barrier_error = output.takeover_barrier_until(deadline).err();
            (Some(operation), barrier_error)
        }
        // A delayed worker must not replace a newer restore generation after its original
        // absolute deadline. Raw-mode cleanup below remains best effort and idempotent.
        Err(error) => (None, Some(error)),
    };
    // Do not replace an older restore between its successful keyboard-pop write and marker clear.
    // A claimed command either completes exactly once or restores the retryable marker first.
    while matches!(
        ACTIVE_KEYBOARD_PROTOCOL.load(Ordering::Acquire),
        ACTIVE_KEYBOARD_KITTY_CLEANING | ACTIVE_KEYBOARD_WIN32_CLEANING
    ) && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    // Observe the cleanup obligation after the fenced takeover. If a second signal replaces an
    // ordinary restore, both callers retain `true`; the fresh emergency writer can therefore emit
    // the complete leave-alt/focus/mouse sequence after the older generation fails.
    let was_active = TERMINAL_ACTIVE.load(Ordering::Acquire);
    let restored = restore_terminal_state_with(
        &ACTIVE_KEYBOARD_PROTOCOL,
        &mut output,
        mouse,
        was_active,
        crossterm::terminal::disable_raw_mode,
    );
    remember_restore_error(&mut first_error, restored);
    let restored = first_error.map_or(Ok(()), Err);
    if was_active && restored.is_ok() {
        TERMINAL_ACTIVE.store(false, Ordering::Release);
    }
    restored
}

#[cfg(unix)]
fn bounded_restore_with(
    output: terminal_io::EmergencyWriter,
    mouse: bool,
    label: &'static str,
    budget: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + budget;
    run_bounded_restore_until(label, deadline, budget, move || {
        emergency_restore_with(output, mouse, label, deadline)
    })
}

#[cfg(unix)]
pub(crate) fn bounded_raw_mode_restore(label: &'static str, budget: Duration) -> io::Result<()> {
    let terminal_was_active = TERMINAL_ACTIVE.load(Ordering::Acquire);
    let restored = run_bounded_restore(label, budget, crossterm::terminal::disable_raw_mode);
    if terminal_was_active {
        return Err(merge_restore_error(
            io::Error::new(
                io::ErrorKind::NotConnected,
                "terminal controls could not be restored because the active output was unavailable",
            ),
            restored,
        ));
    }
    restored
}

#[cfg(unix)]
pub(crate) fn bounded_termios_restore_until(
    label: &'static str,
    deadline: Instant,
    budget: Duration,
    input: rustix::fd::OwnedFd,
    termios: rustix::termios::Termios,
) -> io::Result<()> {
    run_bounded_restore_until(label, deadline, budget, move || {
        rustix::termios::tcsetattr(input, rustix::termios::OptionalActions::Now, &termios)
            .map_err(io::Error::from)
    })
}

#[cfg(unix)]
fn run_bounded_restore(
    label: &'static str,
    budget: Duration,
    restore: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    run_bounded_restore_until(label, Instant::now() + budget, budget, restore)
}

#[cfg(unix)]
fn run_bounded_restore_until(
    label: &'static str,
    deadline: Instant,
    budget: Duration,
    restore: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
    let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("ytt-terminal-restore".to_owned())
        .spawn(move || {
            let _ = finished_tx.send(restore());
        })
        .map_err(|error| {
            io::Error::new(error.kind(), format!("could not start {label}: {error}"))
        })?;
    match finished_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "{label} exceeded its {} ms wall-clock deadline",
                budget.as_millis()
            ),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(format!(
            "{label} worker stopped without reporting a result"
        ))),
    }
}

#[cfg(unix)]
fn restore_terminal_state_with(
    keyboard_protocol: &AtomicU8,
    output: &mut impl Write,
    mouse: bool,
    was_active: bool,
    disable_raw_mode: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let mut first_error = None;
    if was_active {
        remember_restore_error(
            &mut first_error,
            disable_keyboard_input_with(keyboard_protocol, output),
        );
        remember_restore_error(&mut first_error, execute!(output, EndSynchronizedUpdate));
        remember_restore_error(
            &mut first_error,
            execute!(output, DisableFocusChange, DisableBracketedPaste),
        );
        if mouse {
            remember_restore_error(&mut first_error, execute!(output, DisableMouseCapture));
        }
        remember_restore_error(&mut first_error, execute!(output, crossterm::cursor::Show));
    }
    // Termios restoration is an ioctl on stdin and must not be skipped merely because the
    // terminal emulator stopped draining output.
    remember_restore_error(&mut first_error, disable_raw_mode());
    if was_active {
        remember_restore_error(&mut first_error, execute!(output, LeaveAlternateScreen));
    }
    first_error.map_or(Ok(()), Err)
}

fn remember_restore_error<T>(first: &mut Option<io::Error>, result: io::Result<T>) {
    if let Err(error) = result
        && first.is_none()
    {
        *first = Some(error);
    }
}

fn merge_restore_error(primary: io::Error, restored: io::Result<()>) -> io::Error {
    match restored {
        Ok(()) => primary,
        Err(restore_error) => io::Error::new(
            primary.kind(),
            format!("{primary}; terminal restore also failed: {restore_error}"),
        ),
    }
}

#[cfg(unix)]
fn install_terminal_panic_hook(mouse: bool, emergency: terminal_io::EmergencyWriter) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = bounded_restore_with(
            emergency.clone(),
            mouse,
            "panic emergency terminal restore",
            EMERGENCY_RESTORE_BUDGET,
        );
        previous(info);
    }));
}

/// Wake bounded writes promptly once runtime shutdown has become authoritative. The liveness
/// coordinator calls this before media teardown; emergency terminal restoration ignores it and
/// retains its own short absolute deadline.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn cancel_output() {
    terminal_io::cancel_active_output();
}

/// Return the active terminal-output operation without coupling the writer to liveness states.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn output_operation_snapshot() -> Option<OutputOperationSnapshot> {
    terminal_io::active_operation_snapshot()
}

pub(crate) fn write_control_until<T>(
    label: &'static str,
    deadline: Instant,
    operation: impl FnOnce(&mut dyn Write) -> io::Result<T>,
) -> io::Result<T> {
    let mut output = terminal_io::active_writer()?;
    let _guard = output.begin_operation_until(label, deadline)?;
    operation(&mut output)
}

/// Run a CPR on the exclusive event worker while routing its request through the independently
/// reopened output descriptor. The request write has a short budget; crossterm applies
/// `total_timeout` as one absolute response deadline.
pub(crate) fn probe_cursor_position(
    total_timeout: Duration,
) -> io::Result<crossterm::cursor::CursorPositionProbe> {
    let mut output = terminal_io::active_writer()?;
    let _guard = output.begin_operation("cursor-position probe", CPR_WRITE_BUDGET)?;
    crossterm::cursor::probe_position_with(&mut output, total_timeout)
}

#[cfg(test)]
mod tests;
