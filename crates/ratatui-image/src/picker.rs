//! Helper module to build a protocol, and swap protocols at runtime

use std::{
    env,
    io::{self, Read, Write},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crate::{
    FontSize, Resize, Result,
    errors::Errors,
    protocol::{
        Protocol, StatefulProtocol, StatefulProtocolType,
        halfblocks::Halfblocks,
        iterm2::Iterm2,
        kitty::{Kitty, StatefulKitty},
        sixel::Sixel,
    },
};
use cap_parser::{Parser, QueryStdioOptions, Response};
use image::{DynamicImage, Rgba};
use rand::random;
use ratatui::layout::Size;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

pub mod cap_parser;

#[derive(Debug, PartialEq, Clone)]
pub enum Capability {
    /// Reports supporting kitty graphics protocol.
    Kitty,
    /// Reports supporting sixel graphics protocol.
    Sixel,
    /// Reports supporting rectangular ops.
    RectangularOps,
    /// Reports font size in pixels.
    CellSize(Option<(u16, u16)>),
    /// Reports supporting text sizing protocol.
    TextSizingProtocol,
    /// Reports a background color.
    Background(u8, u8, u8),
}

const STDIN_READ_TIMEOUT_MILLIS: u64 = 2000;
// yututui patch: late query cleanup must not turn continuous terminal input into an unbounded
// startup drain.
const MAX_PENDING_INPUT_DRAIN_BYTES: usize = 64 * 1024;
// Reserve a small tail of the caller's one absolute deadline for a nonblocking late-reply drain.
const PENDING_INPUT_DRAIN_RESERVE: Duration = Duration::from_millis(10);
// yututui patch: a cancellable query re-enters its admission fence at this cadence while waiting
// for input. This bounds how long cancellation normally waits behind an in-flight readiness poll.
const CANCELLATION_POLL_SLICE: Duration = Duration::from_millis(10);

fn query_work_deadline(deadline: Instant) -> Instant {
    deadline
        .checked_sub(PENDING_INPUT_DRAIN_RESERVE)
        .unwrap_or(deadline)
}

/// One-way cancellation and I/O-admission fence for a terminal capability query.
///
/// yututui patch: startup runs the synchronous terminal query on a worker. A timeout or signal
/// must be able to prove that the worker cannot subsequently enter raw mode, write another query,
/// or consume shell input before the startup owner restores the inherited terminal state.
///
/// Call [`Self::cancel`] first, then use [`Self::run_cancelled_exclusive_until`] when terminal
/// restoration must be ordered after every operation admitted before cancellation. Once that
/// exclusive closure starts, earlier query I/O has finished and every later admission observes
/// cancellation. Restoring the raw mode captured by an already-started query is intentionally
/// outside this fence: that operation only reapplies the saved original mode and therefore remains
/// safe after the startup owner performs its exact restore.
#[derive(Clone, Debug, Default)]
pub struct QueryCancellation {
    inner: Arc<QueryCancellationInner>,
}

#[derive(Debug, Default)]
struct QueryCancellationInner {
    cancelled: AtomicBool,
    admission: Mutex<()>,
}

impl QueryCancellation {
    /// Create an uncancelled query fence.
    pub fn new() -> Self {
        Self::default()
    }

    /// Irreversibly prevent new query I/O admissions.
    ///
    /// This is nonblocking. Follow it with [`Self::barrier`] before restoring terminal state when
    /// another thread may already be inside an admitted raw-mode or I/O operation.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
    }

    /// Return whether cancellation has been published.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Wait until every operation admitted before cancellation has left the fence.
    ///
    /// The caller must publish [`Self::cancel`] first. Acquiring and releasing this mutex orders
    /// the caller after a raw-mode transition, the complete initial query write, or one bounded
    /// input poll/read/drain admission already in flight. Later attempts observe the cancellation
    /// flag while holding the same mutex and fail before touching the terminal.
    pub fn barrier(&self) {
        drop(self.lock_admission());
    }

    /// Wait for the admission fence only until `deadline`.
    ///
    /// Returns `true` after observing the fence idle. `false` means an already-admitted terminal
    /// operation is still in flight; cleanup must not assume that a later raw-mode or I/O syscall
    /// has been excluded. This method is intended for hard-exit paths which must retain their own
    /// wall-clock bound.
    pub fn barrier_until(&self, deadline: Instant) -> bool {
        loop {
            match self.inner.admission.try_lock() {
                Ok(guard) => {
                    drop(guard);
                    return true;
                }
                Err(std::sync::TryLockError::Poisoned(error)) => {
                    drop(error.into_inner());
                    return true;
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return false;
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(1)));
                }
            }
        }
    }

    /// Cancel the query and run terminal cleanup exclusively after all admitted query I/O.
    ///
    /// The admission fence remains held while `cleanup` runs. This is stronger than observing a
    /// successful [`Self::barrier_until`] and then restoring separately: an already-admitted raw
    /// transition cannot resume between the barrier and the restore. `None` means the fence was
    /// not acquired by `deadline`; the closure is not called, so callers with a hard exit bound do
    /// not perform an unsafe unfenced restore.
    ///
    /// The deadline bounds fence acquisition, not the closure itself. A caller which also needs to
    /// bound cleanup must give the closure its remaining absolute budget or wait for this method on
    /// an independently bounded thread.
    pub fn run_cancelled_exclusive_until<T>(
        &self,
        deadline: Instant,
        cleanup: impl FnOnce() -> T,
    ) -> Option<T> {
        self.cancel();
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            match self.inner.admission.try_lock() {
                Ok(_guard) => return Some(cleanup()),
                Err(std::sync::TryLockError::Poisoned(error)) => {
                    let _guard = error.into_inner();
                    return Some(cleanup());
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(1)));
                }
            }
        }
    }

    fn lock_admission(&self) -> MutexGuard<'_, ()> {
        // A panic drops the worker's guard before poisoning the mutex, so recovering the guard is
        // sufficient to retain the ordering guarantee for emergency cleanup.
        self.inner
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn admit_until(&self, deadline: Instant) -> Result<MutexGuard<'_, ()>> {
        let guard = self.lock_admission();
        self.check_active_until(deadline)?;
        Ok(guard)
    }

    fn check_active_until(&self, deadline: Instant) -> Result<()> {
        if self.is_cancelled() {
            return Err(Errors::Io(io::Error::new(
                io::ErrorKind::Interrupted,
                "terminal capability query was cancelled",
            )));
        }
        if Instant::now() >= deadline {
            return Err(Errors::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal capability query had no remaining startup budget",
            )));
        }
        Ok(())
    }
}

fn with_query_admission<T>(
    cancellation: &QueryCancellation,
    deadline: Instant,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _admission = cancellation.admit_until(deadline)?;
    operation()
}

// yututui patch: allow a conservative, capability-gated Sixel probe on KDE 26.04+.
const KONSOLE_SIXEL_TUI_MIN_VERSION: u32 = 260_400;

#[derive(Clone, Debug)]
pub struct Picker {
    font_size: FontSize,
    protocol_type: ProtocolType,
    background_color: Option<Rgba<u8>>,
    pub(crate) is_tmux: bool,
    capabilities: Vec<Capability>,
}

/// Serde-friendly protocol-type enum for [Picker].
#[derive(PartialEq, Clone, Debug, Copy)]
#[cfg_attr(
    feature = "serde",
    derive(Deserialize, Serialize),
    serde(rename_all = "lowercase")
)]
pub enum ProtocolType {
    Halfblocks,
    Sixel,
    Kitty,
    Iterm2,
}

impl ProtocolType {
    pub fn next(&self) -> ProtocolType {
        match self {
            ProtocolType::Halfblocks => ProtocolType::Sixel,
            ProtocolType::Sixel => ProtocolType::Kitty,
            ProtocolType::Kitty => ProtocolType::Iterm2,
            ProtocolType::Iterm2 => ProtocolType::Halfblocks,
        }
    }
}

/// Helper for building widgets
impl Picker {
    /// Query terminal stdio for graphics capabilities and font-size with some escape sequences.
    ///
    /// This writes and reads from stdio momentarily. WARNING: this method should be called after
    /// entering alternate screen but before reading terminal events.
    ///
    /// # Example
    /// ```rust
    /// use ratatui_image::picker::Picker;
    /// let mut picker = Picker::from_query_stdio();
    /// ```
    ///
    pub fn from_query_stdio() -> Result<Self> {
        Picker::from_query_stdio_with_options(QueryStdioOptions::default())
    }

    /// This should ONLY be used if [Capability::TextSizingProtocol] is needed for some external
    /// reason.
    ///
    /// Query for additional capabilities, currently supports querying for [Text Sizing Protocol].
    ///
    /// The result can be checked by searching for [Capability::TextSizingProtocol] in [Picker::capabilities].
    ///
    /// [Text Sizing Protocol] <https://sw.kovidgoyal.net/kitty/text-sizing-protocol//>
    pub fn from_query_stdio_with_options(options: QueryStdioOptions) -> Result<Self> {
        let deadline = Instant::now() + options.timeout;
        let cancellation = QueryCancellation::new();
        Self::from_query_stdio_with_options_until(options, deadline, &cancellation)
    }

    /// Query stdio with one caller-owned absolute deadline and cancellation fence.
    ///
    /// Unlike [`Self::from_query_stdio_with_options`], this method never rebases the remaining
    /// budget to `Instant::now() + options.timeout`. The `timeout` field is intentionally ignored
    /// by this overload; `deadline` is authoritative for opening the output, entering raw mode,
    /// writing, polling, reading, draining, and teardown reservations.
    ///
    /// A startup coordinator may call [`QueryCancellation::run_cancelled_exclusive_until`] from
    /// its cleanup worker to order inherited-termios restoration after every admitted query
    /// operation.
    pub fn from_query_stdio_with_options_until(
        options: QueryStdioOptions,
        deadline: Instant,
        cancellation: &QueryCancellation,
    ) -> Result<Self> {
        cancellation.check_active_until(deadline)?;
        // yututui patch: never put O_NONBLOCK on inherited stdout; the independently reopened
        // query descriptor has its own absolute output deadline. Keep the cleanup tail outside
        // that write budget as well: a partially written query can still produce a late reply.
        #[cfg(not(windows))]
        let mut output = BoundedTtyWriter::open_until(query_work_deadline(deadline), cancellation)?;
        #[cfg(windows)]
        let mut output = io::stdout();

        Self::from_query_stdio_with_options_and_writer_until(
            options,
            &mut output,
            deadline,
            cancellation,
        )
    }

    /// Query stdio while writing requests through a caller-owned output.
    ///
    /// This is useful to applications which already serialize terminal output or impose their
    /// own write deadline. The caller must ensure `output` addresses the same terminal as stdin.
    /// The default [`Self::from_query_stdio_with_options`] uses an independently reopened,
    /// nonblocking TTY on Unix so a stalled emulator cannot block capability detection forever.
    pub fn from_query_stdio_with_options_and_writer(
        options: QueryStdioOptions,
        output: &mut impl Write,
    ) -> Result<Self> {
        let deadline = Instant::now() + options.timeout;
        let cancellation = QueryCancellation::new();
        Self::from_query_stdio_with_options_and_writer_until(
            options,
            output,
            deadline,
            &cancellation,
        )
    }

    /// Writer-taking variant of [`Self::from_query_stdio_with_options_until`].
    ///
    /// The caller owns `output`, while the same absolute deadline and cancellation fence govern
    /// every terminal operation performed by the query.
    pub fn from_query_stdio_with_options_and_writer_until(
        options: QueryStdioOptions,
        output: &mut impl Write,
        deadline: Instant,
        cancellation: &QueryCancellation,
    ) -> Result<Self> {
        // Detect tmux, and only if positive then take some risky guess for iTerm2 support.
        let (is_tmux, tmux_proto) = detect_tmux_and_outer_protocol_from_env();

        static DEFAULT_PICKER: Picker = Picker {
            // This is completely arbitrary. For halfblocks, it doesn't have to be precise
            // since we're not rendering pixels. It should be roughly 1:2 ratio, and some
            // reasonable size.
            font_size: FontSize::new(10, 20),
            background_color: None,
            protocol_type: ProtocolType::Halfblocks,
            is_tmux: false,
            capabilities: Vec::new(),
        };

        let mut options_with_blacklist = options;
        let wezterm_executable = env::var("WEZTERM_EXECUTABLE").ok();
        let konsole_version = env::var("KONSOLE_VERSION").ok();
        let term = env::var("TERM").ok();
        let require_reported_cell_size_for_konsole_sixel = konsole_allows_capability_gated_sixel(
            wezterm_executable.as_deref(),
            konsole_version.as_deref(),
            term.as_deref(),
        );
        for protocol in terminal_protocol_blacklist(
            wezterm_executable.as_deref(),
            konsole_version.as_deref(),
            term.as_deref(),
        ) {
            if !options_with_blacklist
                .blacklist_protocols
                .contains(&protocol)
            {
                options_with_blacklist.blacklist_protocols.push(protocol);
            }
        }

        // Write and read to stdin to query protocol capabilities and font-size.
        match query_until(
            is_tmux,
            options_with_blacklist,
            deadline,
            output,
            cancellation,
        ) {
            Ok((capability_proto, font_size, caps)) => {
                let iterm2_proto = iterm2_from_env();
                let capability_proto = require_reported_cell_size_for_sixel(
                    capability_proto,
                    &caps,
                    require_reported_cell_size_for_konsole_sixel,
                );

                // IO-based detection is authoritative; env-based hints are fallbacks
                // (env vars like KITTY_WINDOW_ID can be stale in tmux sessions).
                let protocol_type = capability_proto
                    .or(tmux_proto)
                    .or(iterm2_proto)
                    .unwrap_or(ProtocolType::Halfblocks);

                if let Some(font_size) = font_size {
                    Ok(Self {
                        font_size,
                        background_color: None,
                        protocol_type,
                        is_tmux,
                        capabilities: caps,
                    })
                } else {
                    let mut p = DEFAULT_PICKER.clone();
                    p.is_tmux = is_tmux;
                    Ok(p)
                }
            }
            Err(Errors::NoCap | Errors::NoStdinResponse | Errors::NoFontSize) => {
                let mut p = DEFAULT_PICKER.clone();
                p.is_tmux = is_tmux;
                Ok(p)
            }
            Err(err) => Err(err),
        }
    }

    /// Create a picker that is guaranteed to only work with Halfblocks.
    ///
    /// # Example
    /// ```rust
    /// use ratatui_image::picker::Picker;
    ///
    /// let mut picker = Picker::halfblocks();
    /// ```
    pub fn halfblocks() -> Self {
        // Detect tmux, ignore iTerm2 as we don't have font-size.
        let (is_tmux, _tmux_proto) = detect_tmux_and_outer_protocol_from_env();

        Self {
            font_size: FontSize::new(10, 20),
            background_color: None,
            protocol_type: ProtocolType::Halfblocks,
            is_tmux,
            capabilities: Vec::new(),
        }
    }

    /// Create a picker from a given terminal [FontSize].
    #[deprecated(
        since = "9.0.0",
        note = "use `from_query_stdio` or `halfblocks` instead"
    )]
    pub fn from_fontsize(font_size: FontSize) -> Self {
        // Detect tmux, and if positive then take some risky guess for iTerm2 support.
        let (is_tmux, tmux_proto) = detect_tmux_and_outer_protocol_from_env();

        // Disregard protocol-from-capabilities if some env var says that we could try iTerm2.
        let iterm2_proto = iterm2_from_env();

        let protocol_type = tmux_proto
            .or(iterm2_proto)
            .unwrap_or(ProtocolType::Halfblocks);

        Self {
            font_size,
            background_color: None,
            protocol_type,
            is_tmux,
            capabilities: Vec::new(),
        }
    }

    /// Returns the current protocol type.
    pub fn protocol_type(&self) -> ProtocolType {
        self.protocol_type
    }

    /// Force a protocol type.
    pub fn set_protocol_type(&mut self, protocol_type: ProtocolType) {
        self.protocol_type = protocol_type;
    }

    /// Returns the [FontSize] detected by [Picker::from_query_stdio].
    pub fn font_size(&self) -> FontSize {
        self.font_size
    }

    /// Change the default background color (transparent black).
    pub fn set_background_color<T: Into<Rgba<u8>>>(&mut self, background_color: Option<T>) {
        self.background_color = background_color.map(Into::into);
    }

    /// Returns the capabilities detected by [Picker::from_query_stdio].
    pub fn capabilities(&self) -> &Vec<Capability> {
        &self.capabilities
    }

    /// Returns a new protocol.
    ///
    /// The image must match the given area at the terminal's current font size.
    pub(crate) fn new_protocol_raw(&self, image: DynamicImage, size: Size) -> Result<Protocol> {
        match self.protocol_type {
            ProtocolType::Halfblocks => Ok(Protocol::Halfblocks(Halfblocks::new(image, size)?)),
            ProtocolType::Sixel => Ok(Protocol::Sixel(Sixel::new(image, size, self.is_tmux)?)),
            ProtocolType::Kitty => Ok(Protocol::Kitty(Kitty::new(
                image,
                size,
                rand::random(),
                self.is_tmux,
            )?)),
            ProtocolType::Iterm2 => Ok(Protocol::ITerm2(Iterm2::new(image, size, self.is_tmux)?)),
        }
    }

    /// Returns a new protocol for [`crate::Image`] widgets that fits into the given size.
    pub fn new_protocol(
        &self,
        image: DynamicImage,
        size: Size,
        resize: Resize,
    ) -> Result<Protocol> {
        let desired =
            Resize::round_pixel_size_to_cells(image.width(), image.height(), self.font_size);
        let (image, area) =
            match resize.needs_resize(&image, Some(desired), self.font_size, None, size, false) {
                Some(area) => {
                    let image = resize.resize(&image, self.font_size, area, self.background_color);
                    (image, area)
                }
                None => (image, desired),
            };

        self.new_protocol_raw(image, area)
    }

    /// Returns a new *stateful* protocol for [`crate::StatefulImage`] widgets.
    pub fn new_resize_protocol(&self, image: DynamicImage) -> StatefulProtocol {
        self.new_resize_protocol_shared(Arc::new(image))
    }

    /// Returns a new stateful protocol sharing its immutable decoded source pixels.
    pub fn new_resize_protocol_shared(&self, image: Arc<DynamicImage>) -> StatefulProtocol {
        self.new_resize_protocol_shared_with_kitty_z_index(image, None)
    }

    /// Returns a new *stateful* protocol, overriding Kitty's z-index when Kitty is selected.
    ///
    /// yututui uses album art as a background layer with a very low Kitty z-index, but small
    /// foreground graphics inside opaque popups need the normal text layer instead.
    pub fn new_resize_protocol_with_kitty_z_index(
        &self,
        image: DynamicImage,
        kitty_z_index: Option<i32>,
    ) -> StatefulProtocol {
        self.new_resize_protocol_shared_with_kitty_z_index(Arc::new(image), kitty_z_index)
    }

    /// Shared-source variant of [`Self::new_resize_protocol_with_kitty_z_index`].
    pub fn new_resize_protocol_shared_with_kitty_z_index(
        &self,
        image: Arc<DynamicImage>,
        kitty_z_index: Option<i32>,
    ) -> StatefulProtocol {
        let protocol_type = match self.protocol_type {
            ProtocolType::Halfblocks => StatefulProtocolType::Halfblocks(Halfblocks::default()),
            ProtocolType::Sixel => StatefulProtocolType::Sixel(Sixel {
                is_tmux: self.is_tmux,
                ..Sixel::default()
            }),
            ProtocolType::Kitty => StatefulProtocolType::Kitty(match kitty_z_index {
                Some(z_index) => StatefulKitty::new_with_z_index(random(), self.is_tmux, z_index),
                None => StatefulKitty::new(random(), self.is_tmux),
            }),
            ProtocolType::Iterm2 => StatefulProtocolType::ITerm2(Iterm2 {
                is_tmux: self.is_tmux,
                ..Iterm2::default()
            }),
        };
        StatefulProtocol::new_shared(image, self.font_size, self.background_color, protocol_type)
    }
}

// yututui patch: keep older/unknown Konsole versions conservative, and capability-gate 26.04+.
fn terminal_protocol_blacklist(
    wezterm_executable: Option<&str>,
    konsole_version: Option<&str>,
    term: Option<&str>,
) -> Vec<ProtocolType> {
    let is_wezterm = wezterm_executable.is_some_and(|value| !value.is_empty());
    if is_wezterm {
        // WezTerm could use Sixel, but iTerm2 (detected later) is better. It also does not
        // implement the placeholder part of Kitty correctly.
        return vec![ProtocolType::Kitty, ProtocolType::Sixel];
    }

    let is_konsole = konsole_version.is_some_and(|value| !value.is_empty())
        || term.is_some_and(|value| value.to_ascii_lowercase().contains("konsole"));
    if !is_konsole {
        return Vec::new();
    }

    if konsole_allows_capability_gated_sixel(wezterm_executable, konsole_version, term) {
        // Konsole's Kitty implementation still lacks Unicode placeholders. Sixel remains subject
        // to the normal DA1 capability and cell-size response checks below.
        vec![ProtocolType::Kitty]
    } else {
        vec![ProtocolType::Kitty, ProtocolType::Sixel]
    }
}

fn konsole_allows_capability_gated_sixel(
    wezterm_executable: Option<&str>,
    konsole_version: Option<&str>,
    term: Option<&str>,
) -> bool {
    let is_wezterm = wezterm_executable.is_some_and(|value| !value.is_empty());
    let is_konsole = konsole_version.is_some_and(|value| !value.is_empty())
        || term.is_some_and(|value| value.to_ascii_lowercase().contains("konsole"));

    !is_wezterm
        && is_konsole
        && konsole_version
            .and_then(|version| version.trim().parse::<u32>().ok())
            .is_some_and(|version| version >= KONSOLE_SIXEL_TUI_MIN_VERSION)
}

fn require_reported_cell_size_for_sixel(
    protocol_type: Option<ProtocolType>,
    capabilities: &[Capability],
    required: bool,
) -> Option<ProtocolType> {
    if required
        && protocol_type == Some(ProtocolType::Sixel)
        && !capabilities
            .iter()
            .any(|capability| matches!(capability, Capability::CellSize(Some(_))))
    {
        None
    } else {
        protocol_type
    }
}

fn detect_tmux_and_outer_protocol_from_env() -> (bool, Option<ProtocolType>) {
    // Check if we're inside tmux.
    if !env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        && !env::var("TERM_PROGRAM").is_ok_and(|term_program| term_program == "tmux")
    {
        return (false, None);
    }

    // yututui patch: capability detection is read-only and bounded. Do not launch a `tmux set`
    // subprocess here: waiting for the server could hang startup, and detection should not mutate
    // the pane's configuration. Passthrough probes work when the user's tmux configuration allows
    // them and otherwise fall back conservatively.

    // Crude guess based on the *existence* of some magic program specific env vars.
    // Note: kitty is detected via io query (which works through tmux passthrough),
    // not env vars, since KITTY_WINDOW_ID is often stale in tmux sessions.
    const OUTER_TERM_HINTS: [(&str, ProtocolType); 2] = [
        ("ITERM_SESSION_ID", ProtocolType::Iterm2),
        ("WEZTERM_EXECUTABLE", ProtocolType::Iterm2),
    ];
    for (hint, proto) in OUTER_TERM_HINTS {
        if env::var(hint).is_ok_and(|s| !s.is_empty()) {
            return (true, Some(proto));
        }
    }
    (true, None)
}

fn iterm2_from_env() -> Option<ProtocolType> {
    if env::var("TERM_PROGRAM").is_ok_and(|term_program| {
        term_program.contains("iTerm")
            || term_program.contains("WezTerm")
            || term_program.contains("mintty")
            || term_program.contains("vscode")
            || term_program.contains("Tabby")
            || term_program.contains("Hyper")
            || term_program.contains("rio")
            || term_program.contains("Bobcat")
            || term_program.contains("WarpTerminal")
    }) {
        return Some(ProtocolType::Iterm2);
    }
    if env::var("LC_TERMINAL").is_ok_and(|lc_term| lc_term.contains("iTerm")) {
        return Some(ProtocolType::Iterm2);
    }
    None
}

#[cfg(not(windows))]
fn enable_raw_mode() -> Result<impl FnOnce() -> Result<()>> {
    use rustix::termios::{self, LocalModes, OptionalActions};

    let stdin = io::stdin();
    let mut termios = termios::tcgetattr(&stdin)?;
    let termios_original = termios.clone();

    // Disable canonical mode to read without waiting for Enter, disable echoing.
    termios.local_modes &= !LocalModes::ICANON;
    termios.local_modes &= !LocalModes::ECHO;
    // yututui patch: `Drain` can wait forever behind a stopped terminal emulator. Input mode
    // changes do not require output-drain ordering, so apply them immediately.
    termios::tcsetattr(&stdin, OptionalActions::Now, &termios)?;

    Ok(move || {
        Ok(termios::tcsetattr(
            io::stdin(),
            OptionalActions::Now,
            &termios_original,
        )?)
    })
}

#[cfg(windows)]
fn enable_raw_mode() -> Result<impl FnOnce() -> Result<()>> {
    use windows::{
        Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE},
            Storage::FileSystem::{
                self, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            },
            System::Console::{
                self, CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
            },
        },
        core::PCWSTR,
    };

    let utf16: Vec<u16> = "CONIN$\0".encode_utf16().collect();
    let utf16_ptr: *const u16 = utf16.as_ptr();

    // SAFETY: `utf16_ptr` points to a NUL-terminated "CONIN$" buffer that lives for
    // the call; CreateFileW returns a Result-wrapped console input handle.
    let in_handle = unsafe {
        FileSystem::CreateFileW(
            PCWSTR(utf16_ptr),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )
    }?;

    let mut original_in_mode = CONSOLE_MODE::default();
    // SAFETY: `in_handle` is the console input handle returned by CreateFileW and
    // `original_in_mode` is valid output storage.
    unsafe { Console::GetConsoleMode(in_handle, &mut original_in_mode) }?;

    let requested_in_modes = !ENABLE_ECHO_INPUT & !ENABLE_LINE_INPUT & !ENABLE_PROCESSED_INPUT;
    let in_mode = original_in_mode & requested_in_modes;
    // SAFETY: `in_handle` is a console input handle and `in_mode` is derived from the
    // current mode by clearing documented raw-input flags.
    unsafe { Console::SetConsoleMode(in_handle, in_mode) }?;

    Ok(move || {
        // SAFETY: restores the saved mode on the same console input handle; failure is
        // returned to the caller.
        unsafe { Console::SetConsoleMode(in_handle, original_in_mode) }?;
        Ok(())
    })
}

#[cfg(not(windows))]
fn font_size_fallback() -> Option<FontSize> {
    use rustix::termios::{self, Winsize};

    let winsize = termios::tcgetwinsize(io::stdout()).ok()?;
    let Winsize {
        ws_xpixel: x,
        ws_ypixel: y,
        ws_col: cols,
        ws_row: rows,
    } = winsize;
    if x == 0 || y == 0 || cols == 0 || rows == 0 {
        return None;
    }

    Some(FontSize::new(x / cols, y / rows))
}

#[cfg(windows)]
fn font_size_fallback() -> Option<FontSize> {
    None
}

/// yututui patch: query-only output which cannot mutate stdout's shared
/// open-file-description flags or wait forever for a stalled terminal emulator.
#[cfg(not(windows))]
struct BoundedTtyWriter {
    fd: rustix::fd::OwnedFd,
    deadline: Instant,
    cancellation: QueryCancellation,
}

#[cfg(not(windows))]
impl BoundedTtyWriter {
    fn open_until(deadline: Instant, cancellation: &QueryCancellation) -> Result<Self> {
        let stdout = io::stdout();
        Self::open_tty_until(&stdout, deadline, cancellation)
    }

    fn open_tty_until(
        source: &impl rustix::fd::AsFd,
        deadline: Instant,
        cancellation: &QueryCancellation,
    ) -> Result<Self> {
        if !rustix::termios::isatty(source) {
            return Err(Errors::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "terminal query stdout is not a tty",
            )));
        }
        let tty_path = terminal_path(source)?;
        Self::open_tty_path_until(source, tty_path.as_c_str(), deadline, cancellation)
    }

    fn open_tty_path_until(
        source: &impl rustix::fd::AsFd,
        tty_path: &std::ffi::CStr,
        deadline: Instant,
        cancellation: &QueryCancellation,
    ) -> Result<Self> {
        use rustix::fs::{CWD, FileType, Mode, OFlags, fcntl_getfl, fstat, openat};

        let fd = openat(
            CWD,
            tty_path,
            OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )?;
        if !rustix::termios::isatty(&fd) {
            return Err(Errors::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "reopened terminal query output is not a tty",
            )));
        }

        let inherited = fstat(source)?;
        let reopened = fstat(&fd)?;
        if FileType::from_raw_mode(inherited.st_mode) != FileType::CharacterDevice
            || FileType::from_raw_mode(reopened.st_mode) != FileType::CharacterDevice
            || inherited.st_rdev != reopened.st_rdev
        {
            return Err(Errors::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "reopened terminal query output does not identify the stdout tty",
            )));
        }
        if !fcntl_getfl(&fd)?.contains(OFlags::NONBLOCK) {
            return Err(Errors::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "reopened terminal query output is unexpectedly blocking",
            )));
        }

        Ok(Self {
            fd,
            deadline,
            cancellation: cancellation.clone(),
        })
    }

    fn timeout_error() -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal capability query output timed out",
        )
    }

    fn wait_writable(&self) -> io::Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "terminal capability query was cancelled",
            ));
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Self::timeout_error());
        }

        #[cfg(not(target_vendor = "apple"))]
        {
            use rustix::event::{PollFd, PollFlags, poll};

            let millis: i32 = remaining
                .min(CANCELLATION_POLL_SLICE)
                .as_millis()
                .try_into()
                .unwrap_or(50);
            let mut fds = [PollFd::new(&self.fd, PollFlags::OUT)];
            match poll(&mut fds, millis) {
                Ok(0) => Ok(()),
                Ok(_) => {
                    let events = fds[0].revents();
                    if events.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!(
                                "terminal capability query output closed (poll revents: {events:?})"
                            ),
                        ))
                    } else {
                        Ok(())
                    }
                }
                Err(rustix::io::Errno::INTR) => Ok(()),
                Err(error) => Err(io::Error::from(error)),
            }
        }

        // Terminal-output `poll(2)` behavior is not consistent across Apple terminal devices.
        // Nonblocking retries remain bounded by the same absolute deadline.
        #[cfg(target_vendor = "apple")]
        {
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
            Ok(())
        }
    }
}

#[cfg(not(any(windows, target_os = "fuchsia", target_os = "wasi")))]
fn terminal_path(source: &impl rustix::fd::AsFd) -> Result<std::ffi::CString> {
    rustix::termios::ttyname(source, Vec::new()).map_err(Into::into)
}

// rustix cannot expose `ttyname` on these targets. Refuse to use `/dev/tty` as an unverified
// fallback: its device number identifies the magic alias rather than stdout's terminal device.
#[cfg(all(not(windows), any(target_os = "fuchsia", target_os = "wasi")))]
fn terminal_path(_source: &impl rustix::fd::AsFd) -> Result<std::ffi::CString> {
    Err(Errors::Io(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform cannot resolve the terminal query stdout path",
    )))
}

#[cfg(not(windows))]
impl Write for BoundedTtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.cancellation.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "terminal capability query was cancelled",
                ));
            }
            if Instant::now() >= self.deadline {
                return Err(Self::timeout_error());
            }
            match rustix::io::write(&self.fd, buf) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "terminal capability query wrote zero bytes",
                    ));
                }
                Ok(written) => return Ok(written),
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::AGAIN) => self.wait_writable()?,
                Err(error) => {
                    if matches!(
                        error,
                        rustix::io::Errno::PIPE | rustix::io::Errno::IO | rustix::io::Errno::NXIO
                    ) {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!(
                                "terminal capability query output disconnected: {error} (os error {})",
                                error.raw_os_error()
                            ),
                        ));
                    }
                    return Err(io::Error::from(error));
                }
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Query the terminal by writing capability/font-size escape sequences to stdout and reading the
/// responses from stdin, bounded by `timeout`.
///
/// The terminal must already be in "raw mode" (no echo, no canonical/line buffering) so the
/// responses can be read byte-by-byte; the caller restores the previous mode afterwards.
///
/// Several control sequences are sent at once:
/// - `_Gi=...`: Kitty graphics support.
/// - `[c`: Capabilities including sixels.
/// - `[16t`: Cell-size.
/// - `[5n`: Device Status Report, implemented by all terminals. Its `[0n` answer is the
///   terminator we stop on, so a cooperating terminal never makes us read forever.
///
/// We also stop once `timeout` elapses with no further input — so a terminal that drops a response
/// cannot hang us. This runs entirely on the calling thread: there is no background reader that
/// could outlive the call and steal input from the event loop later.
fn query_stdio_capabilities(
    is_tmux: bool,
    options: QueryStdioOptions,
    deadline: Instant,
    output: &mut impl Write,
    cancellation: &QueryCancellation,
) -> Result<(Option<ProtocolType>, Option<FontSize>, Vec<Capability>)> {
    let query = Parser::query(is_tmux, options);
    // yututui patch: keep the complete write_all/flush transaction inside one admission. A
    // cancellation barrier therefore cannot return between a short write and a later retry.
    with_query_admission(cancellation, query_work_deadline(deadline), || {
        output.write_all(query.as_bytes())?;
        output.flush()?;
        Ok(())
    })?;

    let response_deadline = query_work_deadline(deadline);
    let mut parser = Parser::new();
    let mut responses = vec![];
    let mut stdin = io::stdin();
    'out: loop {
        let remaining = response_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // Timed out waiting for (more) response. Drain anything already buffered so stray
            // bytes don't leak into the subsequent event loop, then give up on the rest.
            drain_pending_input_until(deadline, cancellation);
            break 'out;
        }

        let mut charbuf: [u8; 50] = [0; 50];
        // yututui patch: readiness waits use short slices, and poll + read share one admission.
        // Cancellation published during the poll is rechecked before read; cancellation published
        // after that check is ordered before cleanup by the coordinator's barrier.
        let read = poll_and_read_input_until(
            &mut stdin,
            &mut charbuf,
            response_deadline,
            cancellation,
            wait_readable,
        )?;
        let Some(read) = read else {
            continue;
        };
        if read == 0 {
            break 'out; // EOF on stdin
        }

        for ch in charbuf.iter().take(read) {
            let mut more_caps = parser.push(char::from(*ch));
            match more_caps[..] {
                [Response::Status] => {
                    // DSR terminator: the full response has arrived, nothing trails it.
                    break 'out;
                }
                _ => responses.append(&mut more_caps),
            }
        }
    }

    interpret_parser_responses(responses)
}

fn poll_and_read_input_until(
    reader: &mut impl Read,
    buf: &mut [u8],
    deadline: Instant,
    cancellation: &QueryCancellation,
    wait: impl FnOnce(Duration) -> Result<bool>,
) -> Result<Option<usize>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(None);
    }
    with_query_admission(cancellation, deadline, || {
        if !wait(remaining.min(CANCELLATION_POLL_SLICE))? {
            return Ok(None);
        }
        // Cancellation can be published without taking the admission mutex. Recheck after the
        // bounded poll and before entering the read syscall. If it races this check, the guard
        // still keeps the coordinator's barrier behind the read.
        if cancellation.is_cancelled() {
            return Err(Errors::Io(io::Error::new(
                io::ErrorKind::Interrupted,
                "terminal capability query was cancelled",
            )));
        }
        reader.read(buf).map(Some).map_err(Into::into)
    })
}

/// Block until stdin is readable or `timeout` elapses; returns whether stdin became readable.
#[cfg(not(windows))]
fn wait_readable(timeout: Duration) -> Result<bool> {
    use rustix::event::{PollFd, PollFlags, poll};

    let stdin = rustix::stdio::stdin();
    let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
    let millis: i32 = timeout.as_millis().try_into().unwrap_or(i32::MAX);
    poll(&mut fds, millis)?;
    Ok(fds[0].revents().contains(PollFlags::IN))
}

/// Block until stdin is readable or `timeout` elapses; returns whether stdin became readable.
#[cfg(windows)]
fn wait_readable(timeout: Duration) -> Result<bool> {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    use windows::Win32::System::Threading::WaitForSingleObject;

    // SAFETY: STD_INPUT_HANDLE is the documented selector and the Result captures
    // invalid-handle failures.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }?;
    let millis: u32 = timeout.as_millis().try_into().unwrap_or(u32::MAX);
    // SAFETY: waiting on the console input handle is a bounded read readiness probe;
    // timeout conversion saturates and the result is compared to WAIT_OBJECT_0.
    let result = unsafe { WaitForSingleObject(handle, millis) };
    Ok(result == WAIT_OBJECT_0)
}

/// Best-effort, non-blocking drain of any bytes still buffered on stdin (e.g. a late or partial
/// capability response) so they are not re-interpreted as input events by the event loop.
fn drain_pending_input_until(deadline: Instant, cancellation: &QueryCancellation) {
    let mut stdin = io::stdin();
    let _ = drain_available_input_until_cancellable(&mut stdin, deadline, cancellation, || {
        wait_readable(Duration::ZERO)
    });
}

fn drain_available_input_until_cancellable(
    reader: &mut impl Read,
    deadline: Instant,
    cancellation: &QueryCancellation,
    mut readable: impl FnMut() -> Result<bool>,
) -> usize {
    let mut drained = 0;
    let mut buf = [0u8; 256];
    while drained < MAX_PENDING_INPUT_DRAIN_BYTES && Instant::now() < deadline {
        // yututui patch: drain readiness and consumption are fenced just like the response path.
        // Once cancel + barrier completes, the detached query worker cannot steal shell input.
        let result = with_query_admission(cancellation, deadline, || {
            if !readable()? || cancellation.is_cancelled() {
                return Ok(None);
            }
            let remaining = MAX_PENDING_INPUT_DRAIN_BYTES - drained;
            let limit = remaining.min(buf.len());
            reader.read(&mut buf[..limit]).map(Some).map_err(Into::into)
        });
        match result {
            Ok(Some(read)) if read > 0 => drained += read,
            _ => break,
        }
    }
    drained
}

#[cfg(all(test, not(windows)))]
fn drain_available_input_until(
    reader: &mut impl Read,
    deadline: Instant,
    mut readable: impl FnMut() -> bool,
) -> usize {
    let mut drained = 0;
    let mut buf = [0u8; 256];
    while drained < MAX_PENDING_INPUT_DRAIN_BYTES && Instant::now() < deadline && readable() {
        let remaining = MAX_PENDING_INPUT_DRAIN_BYTES - drained;
        let limit = remaining.min(buf.len());
        match reader.read(&mut buf[..limit]) {
            Ok(read) if read > 0 => drained += read,
            _ => break,
        }
    }
    drained
}

fn interpret_parser_responses(
    responses: Vec<Response>,
) -> Result<(Option<ProtocolType>, Option<FontSize>, Vec<Capability>)> {
    if responses.is_empty() {
        return Err(Errors::NoCap);
    }

    let mut capabilities = Vec::new();

    let mut proto = None;
    let mut font_size = None;

    let mut cursor_position_reports = vec![];
    for response in &responses {
        if let Some(capability) = match response {
            Response::Kitty => {
                proto = Some(ProtocolType::Kitty);
                Some(Capability::Kitty)
            }
            Response::Sixel => {
                if proto.is_none() {
                    // Only if kitty is not supported.
                    proto = Some(ProtocolType::Sixel);
                }
                Some(Capability::Sixel)
            }
            Response::RectangularOps => Some(Capability::RectangularOps),
            Response::CellSize(cell_size) => {
                if let Some((w, h)) = cell_size {
                    font_size = Some((*w, *h).into());
                }
                Some(Capability::CellSize(*cell_size))
            }
            Response::CursorPositionReport(x, y) => {
                cursor_position_reports.push((x, y));
                None
            }
            Response::Background(r, g, b) => Some(Capability::Background(*r, *g, *b)),
            Response::Status => None,
        } {
            capabilities.push(capability);
        }
    }

    // In case some terminal didn't support the cell-size query.
    font_size = font_size.or_else(font_size_fallback);

    if let [(x1, _y1), (x2, _y2), (x3, _y3)] = cursor_position_reports[..] {
        // Test if the cursor advanced exactly two columns (instead of one) on both the width and
        // scaling queries of the protocol.
        // The documentation is a bit ambiguous, as it only says the cursor positions "need to be
        // different from each other".
        // However from my testing on Kitty and other terminals that do not support the feature,
        // the cursor always advances at least one column since it is printing a space, so the CPRs
        // will always be different from each other (unless we would move the cursor to a known
        // position or something like that - and this also begs the question of needing to do this
        // anyway, for the edge case of the cursor being at the very end of a line).
        // My interpretation is that the cursor should advance 2 columns, instead of one, with both
        // queries, and only then can we interpret it as supported.
        // The Foot terminal notably reports a 2 column movement but fortunately only for the `w=2`
        // query.
        //
        // The row part can be ignored.
        if *x2 == x1 + 2 && *x3 == x2 + 2 {
            capabilities.push(Capability::TextSizingProtocol);
        }
    }

    Ok((proto, font_size, capabilities))
}

fn query_until(
    is_tmux: bool,
    options: QueryStdioOptions,
    deadline: Instant,
    output: &mut impl Write,
    cancellation: &QueryCancellation,
) -> Result<(Option<ProtocolType>, Option<FontSize>, Vec<Capability>)> {
    cancellation.check_active_until(deadline)?;
    // Put the tty in raw mode so the query responses aren't echoed or line-buffered, run the
    // query, then restore the previous mode BEFORE returning. Doing this synchronously — with no
    // background thread — is what fixes the kitty startup corruption: the old design signalled
    // completion over a channel and only restored the terminal mode afterwards, so the caller
    // (crossterm's `enable_raw_mode`) could observe, and save as its restore target, a termios
    // that was still raw. That left the app running in cooked mode (every keystroke echoed and
    // line-buffered) and the user's shell in raw mode after exit. A detached reader could also
    // outlive a timeout and keep stealing bytes from the event loop; there is none now.
    // yututui patch: raw-mode activation is an admitted operation. If cancellation wins first it
    // never starts; if activation wins, cancel + barrier waits for tcsetattr to return before the
    // coordinator performs its exact inherited-termios restore.
    let disable_raw_mode = with_query_admission(cancellation, deadline, enable_raw_mode)?;
    let result = query_stdio_capabilities(is_tmux, options, deadline, output, cancellation);
    if result.is_err() {
        // A partial request can still provoke a delayed terminal reply. Drain while the input is
        // noncanonical and while the caller's reserved cleanup slice remains.
        drain_pending_input_until(deadline, cancellation);
    }
    finish_query_with_restore(result, disable_raw_mode())
}

fn finish_query_with_restore<T>(query: Result<T>, restored: Result<()>) -> Result<T> {
    match restored {
        Ok(()) => query,
        Err(error) => Err(Errors::TerminalRestore(Box::new(error))),
    }
}

#[cfg(test)]
mod tests {
    use std::assert_eq;
    use std::io;
    #[cfg(not(windows))]
    use std::io::{Cursor, Write as _};
    use std::time::{Duration, Instant};

    #[cfg(not(windows))]
    use rustix::fd::OwnedFd;
    #[cfg(not(windows))]
    use rustix::fs::{CWD, Mode, OFlags, openat};
    #[cfg(not(windows))]
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

    use crate::errors::Errors;
    use crate::picker::{Capability, Picker, ProtocolType};

    #[cfg(not(windows))]
    use super::{
        BoundedTtyWriter, MAX_PENDING_INPUT_DRAIN_BYTES, drain_available_input_until,
        drain_available_input_until_cancellable, query_stdio_capabilities,
    };
    use super::{
        PENDING_INPUT_DRAIN_RESERVE, QueryCancellation,
        cap_parser::{Parser, QueryStdioOptions, Response},
        interpret_parser_responses, poll_and_read_input_until, query_until, query_work_deadline,
        require_reported_cell_size_for_sixel, terminal_protocol_blacklist, with_query_admission,
    };

    #[test]
    fn raw_mode_restore_failure_is_never_downgraded_to_a_capability_failure() {
        let query: crate::Result<()> = Err(Errors::NoCap);
        let restore = Err(Errors::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            "synthetic termios restore timeout",
        )));

        let error = super::finish_query_with_restore(query, restore).unwrap_err();
        assert!(matches!(error, Errors::TerminalRestore(_)));
        assert!(error.to_string().contains("termios restore timeout"));
    }

    #[test]
    fn cancellation_barrier_orders_pause_after_check_before_raw_activation() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        };

        let cancellation = QueryCancellation::new();
        let worker_cancellation = cancellation.clone();
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let order = Arc::new(AtomicUsize::new(0));
        let raw_order = Arc::new(AtomicUsize::new(usize::MAX));
        let worker_order = Arc::clone(&order);
        let worker_raw_order = Arc::clone(&raw_order);
        let worker = std::thread::spawn(move || {
            with_query_admission(
                &worker_cancellation,
                Instant::now() + Duration::from_secs(1),
                || {
                    // Deterministic seam: admission already checked cancellation/deadline, but the
                    // synthetic raw tcsetattr has not started yet.
                    admitted_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    worker_raw_order.store(
                        worker_order.fetch_add(1, Ordering::SeqCst),
                        Ordering::SeqCst,
                    );
                    Ok(())
                },
            )
        });

        admitted_rx.recv().unwrap();
        cancellation.cancel();
        assert!(
            !cancellation.barrier_until(Instant::now() + Duration::from_millis(20)),
            "bounded barrier must report an admission which outlives its deadline"
        );
        let barrier_cancellation = cancellation.clone();
        let barrier_order_source = Arc::clone(&order);
        let (barrier_tx, barrier_rx) = mpsc::channel();
        let barrier = std::thread::spawn(move || {
            barrier_cancellation.barrier();
            let sequence = barrier_order_source.fetch_add(1, Ordering::SeqCst);
            barrier_tx.send(sequence).unwrap();
        });

        assert!(
            matches!(
                barrier_rx.recv_timeout(Duration::from_millis(20)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "barrier passed a raw-mode admission paused before tcsetattr"
        );
        release_tx.send(()).unwrap();
        let barrier_order = barrier_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap().unwrap();
        barrier.join().unwrap();
        assert!(raw_order.load(Ordering::SeqCst) < barrier_order);
        assert!(cancellation.barrier_until(Instant::now()));

        let touched_after_barrier = AtomicBool::new(false);
        let error = with_query_admission(
            &cancellation,
            Instant::now() + Duration::from_secs(1),
            || {
                touched_after_barrier.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            matches!(error, Errors::Io(ref error) if error.kind() == io::ErrorKind::Interrupted)
        );
        assert!(!touched_after_barrier.load(Ordering::SeqCst));
    }

    #[test]
    fn cancelled_exclusive_cleanup_cannot_run_before_paused_raw_activation() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        };

        let cancellation = QueryCancellation::new();
        let worker_cancellation = cancellation.clone();
        let (admitted_tx, admitted_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let order = Arc::new(AtomicUsize::new(0));
        let raw_order = Arc::new(AtomicUsize::new(usize::MAX));
        let worker_order = Arc::clone(&order);
        let worker_raw_order = Arc::clone(&raw_order);
        let worker = std::thread::spawn(move || {
            with_query_admission(
                &worker_cancellation,
                Instant::now() + Duration::from_secs(1),
                || {
                    admitted_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    worker_raw_order.store(
                        worker_order.fetch_add(1, Ordering::SeqCst),
                        Ordering::SeqCst,
                    );
                    Ok(())
                },
            )
        });

        admitted_rx.recv().unwrap();
        let touched_before_deadline = AtomicBool::new(false);
        assert_eq!(
            cancellation
                .run_cancelled_exclusive_until(Instant::now() + Duration::from_millis(5), || {
                    touched_before_deadline.store(true, Ordering::SeqCst)
                },),
            None,
        );
        assert!(!touched_before_deadline.load(Ordering::SeqCst));

        let cleanup_cancellation = cancellation.clone();
        let cleanup_order_source = Arc::clone(&order);
        let (cleanup_tx, cleanup_rx) = mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            let result = cleanup_cancellation
                .run_cancelled_exclusive_until(Instant::now() + Duration::from_secs(1), || {
                    cleanup_order_source.fetch_add(1, Ordering::SeqCst)
                });
            cleanup_tx.send(result).unwrap();
        });
        assert!(
            matches!(
                cleanup_rx.recv_timeout(Duration::from_millis(20)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "exclusive cleanup ran before the admitted raw transition finished",
        );

        release_tx.send(()).unwrap();
        let cleanup_order = cleanup_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap().unwrap();
        cleanup.join().unwrap();
        assert_eq!(cleanup_order, Some(1));
        assert_eq!(raw_order.load(Ordering::SeqCst), 0);
    }

    #[cfg(not(windows))]
    #[test]
    fn cancellation_barrier_waits_for_the_complete_query_output_admission() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };

        struct PausingWriter {
            write_started: mpsc::Sender<()>,
            release_write: mpsc::Receiver<()>,
            flushed: Arc<AtomicBool>,
        }

        impl std::io::Write for PausingWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.write_started.send(()).unwrap();
                self.release_write.recv().unwrap();
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                self.flushed.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let cancellation = QueryCancellation::new();
        let worker_cancellation = cancellation.clone();
        let (write_started_tx, write_started_rx) = mpsc::channel();
        let (release_write_tx, release_write_rx) = mpsc::channel();
        let flushed = Arc::new(AtomicBool::new(false));
        let worker_flushed = Arc::clone(&flushed);
        let worker = std::thread::spawn(move || {
            let mut writer = PausingWriter {
                write_started: write_started_tx,
                release_write: release_write_rx,
                flushed: worker_flushed,
            };
            query_stdio_capabilities(
                false,
                QueryStdioOptions::default(),
                Instant::now() + Duration::from_secs(1),
                &mut writer,
                &worker_cancellation,
            )
        });

        write_started_rx.recv().unwrap();
        cancellation.cancel();
        let barrier_cancellation = cancellation.clone();
        let (barrier_tx, barrier_rx) = mpsc::channel();
        let barrier = std::thread::spawn(move || {
            barrier_cancellation.barrier();
            barrier_tx.send(()).unwrap();
        });
        assert!(
            matches!(
                barrier_rx.recv_timeout(Duration::from_millis(20)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "barrier passed while write_all/flush was still admitted"
        );

        release_write_tx.send(()).unwrap();
        barrier_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(flushed.load(Ordering::SeqCst));
        barrier.join().unwrap();
        let error = worker.join().unwrap().unwrap_err();
        assert!(
            matches!(error, Errors::Io(ref error) if error.kind() == io::ErrorKind::Interrupted)
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn cancellation_during_poll_prevents_read_and_later_drain_admissions() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };

        struct CountingReader(Arc<AtomicUsize>);
        impl std::io::Read for CountingReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(0)
            }
        }

        let cancellation = QueryCancellation::new();
        let worker_cancellation = cancellation.clone();
        let reads = Arc::new(AtomicUsize::new(0));
        let worker_reads = Arc::clone(&reads);
        let (poll_started_tx, poll_started_rx) = mpsc::channel();
        let (release_poll_tx, release_poll_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut reader = CountingReader(worker_reads);
            let mut buf = [0u8; 8];
            poll_and_read_input_until(
                &mut reader,
                &mut buf,
                Instant::now() + Duration::from_secs(1),
                &worker_cancellation,
                |_| {
                    poll_started_tx.send(()).unwrap();
                    release_poll_rx.recv().unwrap();
                    Ok(true)
                },
            )
        });

        poll_started_rx.recv().unwrap();
        cancellation.cancel();
        let barrier_cancellation = cancellation.clone();
        let (barrier_tx, barrier_rx) = mpsc::channel();
        let barrier = std::thread::spawn(move || {
            barrier_cancellation.barrier();
            barrier_tx.send(()).unwrap();
        });
        assert!(matches!(
            barrier_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release_poll_tx.send(()).unwrap();
        barrier_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        barrier.join().unwrap();

        let error = worker.join().unwrap().unwrap_err();
        assert!(
            matches!(error, Errors::Io(ref error) if error.kind() == io::ErrorKind::Interrupted)
        );
        assert_eq!(reads.load(Ordering::SeqCst), 0);

        let mut reader = CountingReader(Arc::clone(&reads));
        let readable_calls = AtomicUsize::new(0);
        let drained = drain_available_input_until_cancellable(
            &mut reader,
            Instant::now() + Duration::from_secs(1),
            &cancellation,
            || {
                readable_calls.fetch_add(1, Ordering::SeqCst);
                Ok(true)
            },
        );
        assert_eq!(drained, 0);
        assert_eq!(readable_calls.load(Ordering::SeqCst), 0);
        assert_eq!(reads.load(Ordering::SeqCst), 0);
    }

    #[cfg(not(windows))]
    fn pty_pair() -> (OwnedFd, OwnedFd) {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let path = ptsname(&master, Vec::new()).unwrap();
        let slave = openat(
            CWD,
            path.as_c_str(),
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )
        .unwrap();
        (master, slave)
    }

    #[cfg(not(windows))]
    #[test]
    fn bounded_query_writer_reopens_kernel_reported_tty_without_mutating_source_flags() {
        use rustix::fs::{fcntl_getfl, fstat};

        let (_master, slave) = pty_pair();
        let before = fcntl_getfl(&slave).unwrap();

        let output = BoundedTtyWriter::open_tty_until(
            &slave,
            Instant::now() + Duration::from_secs(1),
            &QueryCancellation::new(),
        )
        .unwrap();

        assert_eq!(fcntl_getfl(&slave).unwrap(), before);
        assert!(fcntl_getfl(&output.fd).unwrap().contains(OFlags::NONBLOCK));
        assert_eq!(
            fstat(&output.fd).unwrap().st_rdev,
            fstat(&slave).unwrap().st_rdev
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn bounded_query_writer_rejects_a_different_tty_path() {
        let (_expected_master, expected_slave) = pty_pair();
        let (other_master, _other_slave) = pty_pair();
        let other_path = ptsname(&other_master, Vec::new()).unwrap();

        let error = BoundedTtyWriter::open_tty_path_until(
            &expected_slave,
            other_path.as_c_str(),
            Instant::now() + Duration::from_secs(1),
            &QueryCancellation::new(),
        )
        .err()
        .expect("a different PTY path must fail identity validation");

        assert!(
            matches!(error, Errors::Io(ref error) if error.kind() == io::ErrorKind::InvalidData)
        );
        assert!(
            error
                .to_string()
                .contains("does not identify the stdout tty")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn bounded_query_writer_times_out_when_pty_reader_stalls() {
        use rustix::fs::{fcntl_getfl, fcntl_setfl};

        let (master, slave) = pty_pair();
        let flags = fcntl_getfl(&slave).unwrap();
        fcntl_setfl(&slave, flags | OFlags::NONBLOCK).unwrap();
        let mut output = BoundedTtyWriter {
            fd: slave,
            deadline: Instant::now() + Duration::from_millis(75),
            cancellation: QueryCancellation::new(),
        };
        let started = Instant::now();

        let error = output.write_all(&vec![b'x'; 2 * 1024 * 1024]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(master);
    }

    #[cfg(not(windows))]
    #[test]
    fn query_write_and_response_share_one_absolute_deadline() {
        struct DelayedWriter;
        impl std::io::Write for DelayedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(Duration::from_millis(30));
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let started = Instant::now();
        let deadline = started + Duration::from_millis(10);
        let result = query_stdio_capabilities(
            false,
            QueryStdioOptions::default(),
            deadline,
            &mut DelayedWriter,
            &QueryCancellation::new(),
        );

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[cfg(not(windows))]
    #[test]
    fn pending_input_drain_stops_at_its_byte_cap() {
        let input = vec![b'x'; MAX_PENDING_INPUT_DRAIN_BYTES * 2];
        let mut reader = Cursor::new(input);
        let deadline = Instant::now() + Duration::from_secs(1);

        let drained = drain_available_input_until(&mut reader, deadline, || true);

        assert_eq!(drained, MAX_PENDING_INPUT_DRAIN_BYTES);
        assert_eq!(reader.position(), MAX_PENDING_INPUT_DRAIN_BYTES as u64);
        assert!(Instant::now() < deadline);
    }

    #[test]
    fn response_wait_reserves_cleanup_time_inside_the_absolute_deadline() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let response_deadline = query_work_deadline(deadline);

        assert_eq!(
            deadline.duration_since(response_deadline),
            PENDING_INPUT_DRAIN_RESERVE
        );
    }

    #[test]
    fn expired_query_is_rejected_before_raw_mode_setup() {
        let mut output = Vec::new();
        let error = query_until(
            false,
            QueryStdioOptions::default(),
            Instant::now(),
            &mut output,
            &QueryCancellation::new(),
        )
        .unwrap_err();

        match error {
            crate::errors::Errors::Io(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            }
            other => panic!("unexpected expired-query error: {other}"),
        }
        assert!(output.is_empty());
    }

    #[test]
    fn caller_absolute_deadline_is_not_rebased_from_the_options_timeout() {
        let mut output = Vec::new();
        let options = QueryStdioOptions {
            timeout: Duration::from_secs(60),
            ..QueryStdioOptions::default()
        };
        let error = Picker::from_query_stdio_with_options_and_writer_until(
            options,
            &mut output,
            Instant::now(),
            &QueryCancellation::new(),
        )
        .unwrap_err();

        assert!(matches!(error, Errors::Io(ref error) if error.kind() == io::ErrorKind::TimedOut));
        assert!(output.is_empty());
    }

    #[test]
    fn test_cycle_protocol() {
        let mut proto = ProtocolType::Halfblocks;
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Sixel);
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Kitty);
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Iterm2);
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Halfblocks);
    }

    #[test]
    fn test_from_query_stdio_no_hang() {
        let _ = Picker::from_query_stdio();
    }

    #[test]
    fn test_terminal_protocol_blacklist() {
        struct Case {
            name: &'static str,
            wezterm_executable: Option<&'static str>,
            konsole_version: Option<&'static str>,
            term: Option<&'static str>,
            expected: Vec<ProtocolType>,
        }

        let cases = [
            Case {
                name: "unrelated terminal",
                wezterm_executable: None,
                konsole_version: None,
                term: Some("xterm-256color"),
                expected: vec![],
            },
            Case {
                name: "WezTerm keeps Kitty and Sixel blacklisted",
                wezterm_executable: Some("/Applications/WezTerm.app/Contents/MacOS/wezterm-gui"),
                konsole_version: None,
                term: Some("xterm-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
            Case {
                name: "Konsole detected from TERM without a version",
                wezterm_executable: None,
                konsole_version: None,
                term: Some("konsole-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
            Case {
                name: "empty Konsole version is not a hint",
                wezterm_executable: None,
                konsole_version: Some(""),
                term: Some("xterm-256color"),
                expected: vec![],
            },
            Case {
                name: "invalid Konsole version stays conservative",
                wezterm_executable: None,
                konsole_version: Some("26.04"),
                term: Some("xterm-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
            Case {
                name: "Konsole before 26.04 stays conservative",
                wezterm_executable: None,
                konsole_version: Some("260399"),
                term: Some("xterm-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
            Case {
                name: "Konsole 26.04 allows Sixel capability queries",
                wezterm_executable: None,
                konsole_version: Some("260400"),
                term: Some("konsole-256color"),
                expected: vec![ProtocolType::Kitty],
            },
            Case {
                name: "Yakuake KonsolePart hint works with a generic TERM",
                wezterm_executable: None,
                konsole_version: Some("260401"),
                term: Some("xterm-256color"),
                expected: vec![ProtocolType::Kitty],
            },
            Case {
                name: "WezTerm policy wins over a new Konsole hint",
                wezterm_executable: Some("wezterm-gui"),
                konsole_version: Some("260400"),
                term: Some("konsole-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
        ];

        for case in cases {
            assert_eq!(
                terminal_protocol_blacklist(
                    case.wezterm_executable,
                    case.konsole_version,
                    case.term,
                ),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn test_konsole_sixel_query_is_version_gated() {
        let query_for = |konsole_version| {
            let options = QueryStdioOptions {
                blacklist_protocols: terminal_protocol_blacklist(
                    None,
                    Some(konsole_version),
                    Some("konsole-256color"),
                ),
                ..QueryStdioOptions::default()
            };
            Parser::query(false, options)
        };

        let old_konsole_query = query_for("260399");
        assert!(!old_konsole_query.contains("_Gi="));
        assert!(!old_konsole_query.contains("\x1b[c"));

        let new_konsole_query = query_for("260400");
        assert!(!new_konsole_query.contains("_Gi="));
        assert!(new_konsole_query.contains("\x1b[c"));
    }

    #[test]
    fn test_terminal_blacklist_preserves_caller_entries() {
        let mut blacklist = vec![ProtocolType::Iterm2];
        for protocol in terminal_protocol_blacklist(None, Some("260399"), None) {
            if !blacklist.contains(&protocol) {
                blacklist.push(protocol);
            }
        }

        assert_eq!(
            blacklist,
            vec![
                ProtocolType::Iterm2,
                ProtocolType::Kitty,
                ProtocolType::Sixel
            ]
        );
    }

    #[test]
    fn test_konsole_sixel_requires_reported_cell_size() {
        let (sixel_without_cell_size, _, capabilities) =
            interpret_parser_responses(vec![Response::Sixel]).unwrap();

        assert_eq!(
            require_reported_cell_size_for_sixel(sixel_without_cell_size, &capabilities, true),
            None
        );

        let (sixel_without_valid_cell_size, _, capabilities) =
            interpret_parser_responses(vec![Response::Sixel, Response::CellSize(None)]).unwrap();
        assert_eq!(
            require_reported_cell_size_for_sixel(
                sixel_without_valid_cell_size,
                &capabilities,
                true
            ),
            None
        );

        let (sixel_with_cell_size, _, capabilities) =
            interpret_parser_responses(vec![Response::Sixel, Response::CellSize(Some((10, 20)))])
                .unwrap();
        assert_eq!(
            require_reported_cell_size_for_sixel(sixel_with_cell_size, &capabilities, true),
            Some(ProtocolType::Sixel)
        );
        assert_eq!(
            require_reported_cell_size_for_sixel(sixel_without_cell_size, &[], false),
            Some(ProtocolType::Sixel)
        );
        assert_eq!(
            require_reported_cell_size_for_sixel(Some(ProtocolType::Kitty), &[], true),
            Some(ProtocolType::Kitty)
        );
    }

    #[test]
    fn test_interpret_parser_responses_text_sizing_protocol() {
        let (_, _, caps) = interpret_parser_responses(vec![
            // Example response from Kitty.
            Response::CursorPositionReport(1, 1),
            Response::CursorPositionReport(3, 1),
            Response::CursorPositionReport(5, 1),
        ])
        .unwrap();
        assert!(caps.contains(&Capability::TextSizingProtocol));
    }

    #[test]
    fn test_interpret_parser_responses_text_sizing_protocol_incomplete() {
        let (_, _, caps) = interpret_parser_responses(vec![
            // Example response from Foot, notably moves 2 columns only on `w=2` query, but not
            // `s=2`.
            Response::CursorPositionReport(1, 22),
            Response::CursorPositionReport(3, 22),
            Response::CursorPositionReport(4, 22),
        ])
        .unwrap();
        assert!(!caps.contains(&Capability::TextSizingProtocol));
    }
}
