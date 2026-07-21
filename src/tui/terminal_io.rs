//! Bounded terminal output for the interactive TUI.
//!
//! Unix stdout is deliberately reopened through the path reported by `ttyname`. Setting
//! `O_NONBLOCK` on stdout itself (or on `dup(stdout)`) would mutate the shared open-file description
//! and can leave the caller's shell nonblocking after a crash. The reopened descriptor has
//! independent status flags, and its device identity is checked against inherited stdout before it
//! is used.

use std::io::{self, IoSlice, Write};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

#[cfg(unix)]
#[cfg(all(unix, not(target_vendor = "apple")))]
const WAIT_SLICE: Duration = Duration::from_millis(50);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputOperationPhase {
    Preparing,
    Writing,
    Flushing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputOperationSnapshot {
    pub(crate) label: &'static str,
    pub(crate) generation: u64,
    pub(crate) elapsed: Duration,
    pub(crate) phase: OutputOperationPhase,
    pub(crate) expired: bool,
}

#[derive(Clone)]
pub(super) struct TerminalWriter {
    #[cfg(unix)]
    shared: Arc<Shared>,
}

/// Independently owned output for interactive prompts that run before [`crate::tui::init`].
///
/// Unlike [`TerminalWriter::open_stdout`], this does not publish itself as the active AppTerminal
/// writer. Every write gets a fresh operation whose deadline is clamped to both the prompt's
/// overall deadline and the caller-supplied output budget.
pub(crate) struct PreTuiOutput {
    writer: TerminalWriter,
    overall_deadline: Instant,
    operation_budget: Duration,
}

/// Out-of-band cancellation for a pre-TUI writer blocked on a full Unix terminal queue.
#[derive(Clone)]
pub(crate) struct PreTuiOutputCancellation {
    #[cfg(unix)]
    shared: Arc<Shared>,
}

#[cfg(unix)]
struct Shared {
    fd: rustix::fd::OwnedFd,
    operation: Mutex<Option<Operation>>,
    /// Orders one nonblocking kernel write against emergency generation replacement without
    /// hiding operation metadata from the watchdog if the syscall itself misbehaves.
    write_fence: Mutex<()>,
    next_generation: AtomicU64,
    cancelled: AtomicBool,
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
struct Operation {
    generation: u64,
    started_at: Instant,
    deadline: Instant,
    label: &'static str,
    phase: OutputOperationPhase,
}

pub(super) struct OperationGuard {
    #[cfg(unix)]
    shared: Arc<Shared>,
    #[cfg(unix)]
    generation: u64,
}

#[derive(Clone)]
pub(super) struct EmergencyWriter {
    #[cfg(unix)]
    shared: Arc<Shared>,
    #[cfg(unix)]
    label: &'static str,
    #[cfg(unix)]
    generation: Option<u64>,
}

#[cfg(unix)]
struct PhaseGuard {
    shared: Arc<Shared>,
    generation: u64,
    previous: OutputOperationPhase,
}

#[cfg(unix)]
static ACTIVE_OUTPUT: Mutex<Option<Weak<Shared>>> = Mutex::new(None);

impl TerminalWriter {
    pub(super) fn open_stdout() -> io::Result<Self> {
        #[cfg(unix)]
        {
            let writer = Self::open_standalone_stdout()?;
            let shared = Arc::clone(&writer.shared);
            *lock_unpoisoned(&ACTIVE_OUTPUT) = Some(Arc::downgrade(&shared));
            Ok(writer)
        }

        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    fn open_standalone_stdout() -> io::Result<Self> {
        #[cfg(unix)]
        {
            let stdout = io::stdout();
            Ok(Self {
                shared: new_shared(reopen_tty_output(&stdout)?),
            })
        }

        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    pub(super) fn begin_operation(
        &self,
        label: &'static str,
        budget: Duration,
    ) -> io::Result<OperationGuard> {
        let deadline = Instant::now().checked_add(budget).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("terminal output deadline overflow for {label}"),
            )
        })?;
        self.begin_operation_until(label, deadline)
    }

    pub(super) fn begin_operation_until(
        &self,
        label: &'static str,
        deadline: Instant,
    ) -> io::Result<OperationGuard> {
        #[cfg(unix)]
        {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("terminal output had no remaining budget for {label}"),
                ));
            }
            if self.shared.cancelled.load(Ordering::Acquire) {
                return Err(cancelled_error());
            }
            let generation = self
                .shared
                .next_generation
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1);
            let mut operation = lock_unpoisoned(&self.shared.operation);
            if operation.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "terminal output operation is already active",
                ));
            }
            *operation = Some(Operation {
                generation,
                started_at: now,
                deadline,
                label,
                phase: OutputOperationPhase::Preparing,
            });
            drop(operation);
            Ok(OperationGuard {
                shared: Arc::clone(&self.shared),
                generation,
            })
        }

        #[cfg(not(unix))]
        {
            let _ = (label, deadline);
            Ok(OperationGuard {})
        }
    }

    pub(super) fn emergency(&self, _budget: Duration) -> EmergencyWriter {
        EmergencyWriter {
            #[cfg(unix)]
            shared: Arc::clone(&self.shared),
            #[cfg(unix)]
            label: "emergency terminal restore",
            #[cfg(unix)]
            generation: None,
        }
    }

    /// Write one complete control sequence and publish its cleanup obligation before an
    /// emergency writer can emit the inverse sequence.
    #[cfg(unix)]
    pub(super) fn write_all_then(
        &mut self,
        buf: &[u8],
        delivered: impl FnOnce(),
    ) -> io::Result<()> {
        let operation = self.operation()?;
        let _phase = PhaseGuard::enter(
            &self.shared,
            operation.generation,
            OutputOperationPhase::Writing,
        )?;
        write_all_until_then(&self.shared, buf, operation, true, delivered)
    }

    #[cfg(unix)]
    fn operation(&self) -> io::Result<Operation> {
        lock_unpoisoned(&self.shared.operation).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "terminal output write attempted without an active operation",
            )
        })
    }
}

pub(super) fn preflight_interactive_terminal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let stdout = io::stdout();
        drop(reopen_tty_output(&stdout)?);
    }
    Ok(())
}

impl PreTuiOutput {
    pub(crate) fn open_until(
        overall_deadline: Instant,
        operation_budget: Duration,
    ) -> io::Result<Self> {
        if Instant::now() >= overall_deadline || operation_budget.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "pre-TUI terminal output has no remaining budget",
            ));
        }
        Ok(Self {
            writer: TerminalWriter::open_standalone_stdout()?,
            overall_deadline,
            operation_budget,
        })
    }

    pub(crate) fn cancellation(&self) -> PreTuiOutputCancellation {
        PreTuiOutputCancellation {
            #[cfg(unix)]
            shared: Arc::clone(&self.writer.shared),
        }
    }

    pub(crate) fn write_bytes(&mut self, label: &'static str, bytes: &[u8]) -> io::Result<()> {
        let deadline =
            clamp_operation_deadline(Instant::now(), self.overall_deadline, self.operation_budget);
        let _operation = self.writer.begin_operation_until(label, deadline)?;
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }
}

impl PreTuiOutputCancellation {
    pub(crate) fn cancel(&self) {
        #[cfg(unix)]
        self.shared.cancelled.store(true, Ordering::Release);
    }
}

fn clamp_operation_deadline(now: Instant, overall: Instant, budget: Duration) -> Instant {
    now.checked_add(budget)
        .map_or(overall, |operation| operation.min(overall))
}

impl EmergencyWriter {
    pub(super) fn begin_operation_until(
        &mut self,
        label: &'static str,
        deadline: Instant,
    ) -> io::Result<OperationGuard> {
        #[cfg(unix)]
        {
            let now = Instant::now();
            self.label = label;
            if now >= deadline {
                self.generation = None;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("terminal output had no remaining budget for {label}"),
                ));
            }
            let generation = self
                .shared
                .next_generation
                .fetch_add(1, Ordering::AcqRel)
                .wrapping_add(1);
            self.generation = Some(generation);
            *lock_unpoisoned(&self.shared.operation) = Some(Operation {
                generation,
                started_at: now,
                deadline,
                label,
                phase: OutputOperationPhase::Preparing,
            });
            Ok(OperationGuard {
                shared: Arc::clone(&self.shared),
                generation,
            })
        }

        #[cfg(not(unix))]
        {
            let _ = (label, deadline);
            Ok(OperationGuard {})
        }
    }

    /// Wait until every syscall admitted by the replaced generation has finished. This barrier
    /// must run before restore code observes protocol markers: a successful enable write publishes
    /// its inverse obligation while holding the same fence.
    #[cfg(unix)]
    pub(super) fn takeover_barrier_until(&self, deadline: Instant) -> io::Result<()> {
        let generation = self.generation.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "emergency terminal output has no active restore operation",
            )
        })?;
        let operation = operation_for_generation(&self.shared, generation, self.label)?;
        let (fence, _) = acquire_write_fence(&self.shared, operation, false)?;
        drop(fence);
        if Instant::now() >= deadline {
            return Err(timeout_error(operation));
        }
        Ok(())
    }
}

#[cfg(unix)]
fn reopen_tty_output(source: &impl rustix::fd::AsFd) -> io::Result<rustix::fd::OwnedFd> {
    reopen_tty_output_for(&io::stdin(), source)
}

#[cfg(unix)]
fn reopen_tty_output_for(
    input: &impl rustix::fd::AsFd,
    source: &impl rustix::fd::AsFd,
) -> io::Result<rustix::fd::OwnedFd> {
    if !rustix::termios::isatty(source) {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "interactive terminal stdout is not a tty",
        ));
    }
    validate_interactive_tty_pair(input, source)?;
    let tty_path = stdout_tty_path(source)?;
    reopen_tty_output_at(source, tty_path.as_c_str())
}

#[cfg(unix)]
fn validate_interactive_tty_pair(
    input: &impl rustix::fd::AsFd,
    output: &impl rustix::fd::AsFd,
) -> io::Result<()> {
    use rustix::fs::fstat;

    // When stdin is redirected, crossterm reads the controlling TTY. `/dev/tty` itself has a
    // magic device number on Linux, so compare controlling-session IDs instead of `st_rdev`.
    if !rustix::termios::isatty(input) {
        use rustix::fs::{CWD, Mode, OFlags, openat};

        let controlling = openat(
            CWD,
            c"/dev/tty",
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )
        .map_err(os_error)?;
        let controlling_session = rustix::termios::tcgetsid(&controlling).map_err(os_error)?;
        let output_session = rustix::termios::tcgetsid(output).map_err(os_error)?;
        if controlling_session != output_session {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interactive terminal stdout is not the controlling TTY used for input",
            ));
        }
        return Ok(());
    }
    let input = fstat(input).map_err(os_error)?;
    let output = fstat(output).map_err(os_error)?;
    if input.st_rdev != output.st_rdev {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interactive terminal stdin and stdout identify different TTY devices",
        ));
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "fuchsia", target_os = "wasi"))))]
fn stdout_tty_path(source: &impl rustix::fd::AsFd) -> io::Result<std::ffi::CString> {
    rustix::termios::ttyname(source, Vec::new()).map_err(os_error)
}

// rustix cannot expose `ttyname` on these targets. Refuse to open an unverified fallback such as
// `/dev/tty`: its device number identifies the magic alias, not necessarily stdout's terminal.
#[cfg(all(unix, any(target_os = "fuchsia", target_os = "wasi")))]
fn stdout_tty_path(_source: &impl rustix::fd::AsFd) -> io::Result<std::ffi::CString> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this platform cannot resolve the interactive stdout tty path",
    ))
}

#[cfg(unix)]
fn reopen_tty_output_at(
    source: &impl rustix::fd::AsFd,
    tty_path: &std::ffi::CStr,
) -> io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{CWD, FileType, Mode, OFlags, fcntl_getfl, fstat, openat};

    let fd = openat(
        CWD,
        tty_path,
        OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    )
    .map_err(os_error)?;

    if !rustix::termios::isatty(&fd) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reopened terminal output is not a tty",
        ));
    }
    let inherited = fstat(source).map_err(os_error)?;
    let reopened = fstat(&fd).map_err(os_error)?;
    if FileType::from_raw_mode(inherited.st_mode) != FileType::CharacterDevice
        || FileType::from_raw_mode(reopened.st_mode) != FileType::CharacterDevice
        || inherited.st_rdev != reopened.st_rdev
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reopened terminal output does not identify the stdout tty",
        ));
    }
    if !fcntl_getfl(&fd)
        .map_err(os_error)?
        .contains(OFlags::NONBLOCK)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reopened terminal output is unexpectedly blocking",
        ));
    }
    Ok(fd)
}

#[cfg(unix)]
fn new_shared(fd: rustix::fd::OwnedFd) -> Arc<Shared> {
    Arc::new(Shared {
        fd,
        operation: Mutex::new(None),
        write_fence: Mutex::new(()),
        next_generation: AtomicU64::new(0),
        cancelled: AtomicBool::new(false),
    })
}

impl Write for TerminalWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            let operation = self.operation()?;
            let _phase = PhaseGuard::enter(
                &self.shared,
                operation.generation,
                OutputOperationPhase::Writing,
            )?;
            write_until(&self.shared, buf, operation, true)
        }

        #[cfg(not(unix))]
        {
            io::stdout().write(buf)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let operation = self.operation()?;
            let _phase = PhaseGuard::enter(
                &self.shared,
                operation.generation,
                OutputOperationPhase::Flushing,
            )?;
            validate_operation(&self.shared, operation, true)?;
            // A tty has no userspace buffer in this writer. Deliberately avoid `tcdrain`: it can
            // wait indefinitely for the emulator to consume its output queue.
            Ok(())
        }

        #[cfg(not(unix))]
        {
            io::stdout().flush()
        }
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            if let Some(buf) = bufs.iter().find(|buf| !buf.is_empty()) {
                self.write(buf)
            } else {
                Ok(0)
            }
        }

        #[cfg(not(unix))]
        {
            io::stdout().write_vectored(bufs)
        }
    }
}

impl Write for EmergencyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            let generation = self.generation.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "emergency terminal output has no active restore operation",
                )
            })?;
            let operation = operation_for_generation(&self.shared, generation, self.label)?;
            let _phase =
                PhaseGuard::enter(&self.shared, generation, OutputOperationPhase::Writing)?;
            write_until(&self.shared, buf, operation, false)
        }

        #[cfg(not(unix))]
        {
            io::stdout().write(buf)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let generation = self.generation.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "emergency terminal output has no active restore operation",
                )
            })?;
            let operation = operation_for_generation(&self.shared, generation, self.label)?;
            let _phase =
                PhaseGuard::enter(&self.shared, generation, OutputOperationPhase::Flushing)?;
            validate_operation(&self.shared, operation, false)?;
            Ok(())
        }

        #[cfg(not(unix))]
        {
            io::stdout().flush()
        }
    }
}

#[cfg(unix)]
impl PhaseGuard {
    fn enter(
        shared: &Arc<Shared>,
        generation: u64,
        phase: OutputOperationPhase,
    ) -> io::Result<Self> {
        let mut active = lock_unpoisoned(&shared.operation);
        let operation = active.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "terminal output operation ended before its write phase",
            )
        })?;
        if operation.generation != generation {
            return Err(replaced_error(operation.label));
        }
        let previous = operation.phase;
        operation.phase = phase;
        drop(active);
        Ok(Self {
            shared: Arc::clone(shared),
            generation,
            previous,
        })
    }
}

#[cfg(unix)]
impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let mut active = lock_unpoisoned(&self.shared.operation);
        if let Some(operation) = active.as_mut()
            && operation.generation == self.generation
        {
            operation.phase = self.previous;
        }
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let mut operation = lock_unpoisoned(&self.shared.operation);
            if operation.is_some_and(|active| active.generation == self.generation) {
                *operation = None;
            }
        }
    }
}

pub(super) fn active_writer() -> io::Result<TerminalWriter> {
    #[cfg(unix)]
    {
        let shared = lock_unpoisoned(&ACTIVE_OUTPUT)
            .as_ref()
            .and_then(Weak::upgrade)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "terminal output is not initialized",
                )
            })?;
        Ok(TerminalWriter { shared })
    }

    #[cfg(not(unix))]
    {
        Ok(TerminalWriter {})
    }
}

pub(super) fn cancel_active_output() {
    #[cfg(unix)]
    if let Some(shared) = lock_unpoisoned(&ACTIVE_OUTPUT)
        .as_ref()
        .and_then(Weak::upgrade)
    {
        shared.cancelled.store(true, Ordering::Release);
    }
}

pub(super) fn reset_active_output() {
    #[cfg(unix)]
    if let Some(shared) = lock_unpoisoned(&ACTIVE_OUTPUT)
        .as_ref()
        .and_then(Weak::upgrade)
    {
        shared.cancelled.store(false, Ordering::Release);
    }
}

#[cfg_attr(test, allow(dead_code))]
pub(super) fn active_operation_snapshot() -> Option<OutputOperationSnapshot> {
    #[cfg(unix)]
    {
        let shared = lock_unpoisoned(&ACTIVE_OUTPUT)
            .as_ref()
            .and_then(Weak::upgrade)?;
        operation_snapshot(&shared)
    }

    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
fn write_until(
    shared: &Shared,
    buf: &[u8],
    mut operation: Operation,
    respect_cancel: bool,
) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    loop {
        let attempt = {
            let (fence, current) = acquire_write_fence(shared, operation, respect_cancel)?;
            operation = current;
            let attempt = rustix::io::write(&shared.fd, buf);
            drop(fence);
            attempt
        };
        match attempt {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!(
                        "terminal output wrote zero bytes during {}",
                        operation.label
                    ),
                ));
            }
            Ok(written) => return Ok(written),
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => {
                wait_writable(shared, operation, respect_cancel)?;
            }
            Err(error) => return Err(terminal_io_error(error, operation.label)),
        }
    }
}

#[cfg(unix)]
fn write_all_until_then(
    shared: &Shared,
    buf: &[u8],
    mut operation: Operation,
    respect_cancel: bool,
    delivered: impl FnOnce(),
) -> io::Result<()> {
    let mut delivered = Some(delivered);
    let mut offset = 0;
    while offset < buf.len() {
        let attempt = {
            let (fence, current) = acquire_write_fence(shared, operation, respect_cancel)?;
            operation = current;
            let attempt = rustix::io::write(&shared.fd, &buf[offset..]);
            if let Ok(written) = attempt
                && written > 0
                && offset + written == buf.len()
            {
                delivered.take().expect("delivery callback runs once")();
            }
            drop(fence);
            attempt
        };
        match attempt {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "terminal control sequence wrote zero bytes",
                ));
            }
            Ok(written) => offset += written,
            Err(rustix::io::Errno::INTR) => continue,
            Err(rustix::io::Errno::AGAIN) => wait_writable(shared, operation, respect_cancel)?,
            Err(error) => return Err(terminal_io_error(error, operation.label)),
        }
    }
    if let Some(delivered) = delivered {
        delivered();
    }
    Ok(())
}

#[cfg(unix)]
fn acquire_write_fence(
    shared: &Shared,
    mut operation: Operation,
    respect_cancel: bool,
) -> io::Result<(MutexGuard<'_, ()>, Operation)> {
    loop {
        let fence = match shared.write_fence.try_lock() {
            Ok(fence) => Some(fence),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        if let Some(fence) = fence {
            operation = validate_operation(shared, operation, respect_cancel)?;
            return Ok((fence, operation));
        }
        operation = validate_operation(shared, operation, respect_cancel)?;
        let remaining = operation.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(timeout_error(operation));
        }
        std::thread::sleep(remaining.min(Duration::from_millis(1)));
    }
}

#[cfg(unix)]
fn operation_for_generation(
    shared: &Shared,
    generation: u64,
    label: &'static str,
) -> io::Result<Operation> {
    let active = lock_unpoisoned(&shared.operation);
    match *active {
        Some(operation) if operation.generation == generation => Ok(operation),
        _ => Err(replaced_error(label)),
    }
}

#[cfg(unix)]
fn validate_operation(
    shared: &Shared,
    mut operation: Operation,
    respect_cancel: bool,
) -> io::Result<Operation> {
    if respect_cancel && shared.cancelled.load(Ordering::Acquire) {
        return Err(cancelled_error());
    }

    let now = Instant::now();
    let mut active = lock_unpoisoned(&shared.operation);
    let current = active
        .as_mut()
        .filter(|current| current.generation == operation.generation)
        .ok_or_else(|| replaced_error(operation.label))?;
    operation = *current;
    drop(active);

    if now >= operation.deadline {
        return Err(timeout_error(operation));
    }
    Ok(operation)
}

#[cfg(unix)]
fn operation_snapshot(shared: &Shared) -> Option<OutputOperationSnapshot> {
    let operation = (*lock_unpoisoned(&shared.operation))?;
    let now = Instant::now();
    Some(OutputOperationSnapshot {
        label: operation.label,
        generation: operation.generation,
        elapsed: now.saturating_duration_since(operation.started_at),
        phase: operation.phase,
        expired: now >= operation.deadline,
    })
}

#[cfg(all(unix, not(target_vendor = "apple")))]
fn wait_writable(shared: &Shared, operation: Operation, respect_cancel: bool) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags, poll};
    use rustix::time::Timespec;

    if respect_cancel && shared.cancelled.load(Ordering::Acquire) {
        return Err(cancelled_error());
    }
    let remaining = operation.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timeout_error(operation));
    }
    let timeout = Timespec::try_from(remaining.min(WAIT_SLICE)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "terminal poll timeout overflow",
        )
    })?;
    let mut fds = [PollFd::new(&shared.fd, PollFlags::OUT)];
    match poll(&mut fds, Some(&timeout)) {
        Ok(0) => Ok(()),
        Ok(_) => {
            let events = fds[0].revents();
            if events.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!(
                        "terminal output closed during {} (poll revents: {events:?})",
                        operation.label
                    ),
                ));
            }
            Ok(())
        }
        Err(rustix::io::Errno::INTR) => Ok(()),
        Err(error) => Err(os_error(error)),
    }
}

// Terminal-output `poll(2)` behavior is not consistent across Apple terminal devices. The
// descriptor is nonblocking, so a short bounded retry is safer there and still preserves the
// absolute deadline.
#[cfg(all(unix, target_vendor = "apple"))]
fn wait_writable(shared: &Shared, operation: Operation, respect_cancel: bool) -> io::Result<()> {
    if respect_cancel && shared.cancelled.load(Ordering::Acquire) {
        return Err(cancelled_error());
    }
    let remaining = operation.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timeout_error(operation));
    }
    std::thread::sleep(remaining.min(Duration::from_millis(10)));
    Ok(())
}

#[cfg(unix)]
fn os_error(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

#[cfg(unix)]
fn terminal_io_error(error: rustix::io::Errno, label: &'static str) -> io::Error {
    if matches!(
        error,
        rustix::io::Errno::PIPE | rustix::io::Errno::IO | rustix::io::Errno::NXIO
    ) {
        return io::Error::new(
            io::ErrorKind::BrokenPipe,
            format!(
                "terminal output disconnected during {label}: {error} (os error {})",
                error.raw_os_error()
            ),
        );
    }
    os_error(error)
}

#[cfg(unix)]
fn timeout_error(operation: Operation) -> io::Error {
    let elapsed_ms = Instant::now()
        .saturating_duration_since(operation.started_at)
        .as_millis();
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "terminal output timed out during {} (phase={:?}, generation={}, elapsed_ms={elapsed_ms})",
            operation.label, operation.phase, operation.generation
        ),
    )
}

#[cfg(unix)]
fn cancelled_error() -> io::Error {
    // `Write::write_all` retries `Interrupted` forever. Runtime cancellation is authoritative,
    // so surface a non-retriable kind and let the owner enter bounded restoration immediately.
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "terminal output was cancelled by runtime shutdown",
    )
}

#[cfg(unix)]
fn replaced_error(label: &'static str) -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        format!("terminal output operation was replaced during {label}"),
    )
}

#[cfg(unix)]
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use rustix::fd::OwnedFd;
    use rustix::fs::{CWD, Mode, OFlags, fcntl_getfl, fcntl_setfl, openat};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

    use super::{
        OutputOperationPhase, PhaseGuard, PreTuiOutputCancellation, TerminalWriter,
        clamp_operation_deadline, new_shared, operation_snapshot, reopen_tty_output_at,
        reopen_tty_output_for, validate_interactive_tty_pair, validate_operation,
    };

    fn pty_pair() -> (OwnedFd, OwnedFd) {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let master_flags = fcntl_getfl(&master).unwrap();
        fcntl_setfl(&master, master_flags | OFlags::NONBLOCK).unwrap();
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

    fn writer_for(fd: OwnedFd) -> TerminalWriter {
        let flags = fcntl_getfl(&fd).unwrap();
        fcntl_setfl(&fd, flags | OFlags::NONBLOCK).unwrap();
        TerminalWriter {
            shared: new_shared(fd),
        }
    }

    fn read_pty_bytes(master: &OwnedFd, expected: usize) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut bytes = Vec::with_capacity(expected);
        let mut chunk = [0_u8; 32];
        while bytes.len() < expected {
            match rustix::io::read(master, &mut chunk) {
                Ok(read) if read > 0 => bytes.extend_from_slice(&chunk[..read]),
                Err(rustix::io::Errno::AGAIN) if Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                outcome => panic!("PTY did not produce {expected} bytes: {outcome:?}"),
            }
        }
        bytes
    }

    #[test]
    fn kernel_reported_path_reopens_the_same_tty_without_mutating_inherited_flags() {
        let (_master, slave) = pty_pair();
        let before = fcntl_getfl(&slave).unwrap();
        assert!(!before.contains(OFlags::NONBLOCK));

        let reopened = reopen_tty_output_for(&slave, &slave).unwrap();

        assert_eq!(fcntl_getfl(&slave).unwrap(), before);
        assert!(fcntl_getfl(&reopened).unwrap().contains(OFlags::NONBLOCK));
        assert_eq!(
            rustix::fs::fstat(&reopened).unwrap().st_rdev,
            rustix::fs::fstat(&slave).unwrap().st_rdev
        );
        drop(reopened);
        assert_eq!(fcntl_getfl(&slave).unwrap(), before);
    }

    #[test]
    fn reopening_a_different_tty_is_rejected_by_device_identity() {
        let (_expected_master, expected_slave) = pty_pair();
        let (other_master, _other_slave) = pty_pair();
        let other_path = ptsname(&other_master, Vec::new()).unwrap();

        let error = reopen_tty_output_at(&expected_slave, other_path.as_c_str()).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("does not identify the stdout tty")
        );
    }

    #[test]
    fn interactive_input_and_output_must_use_the_same_tty_when_both_are_terminals() {
        let (_first_master, first_slave) = pty_pair();
        let (_second_master, second_slave) = pty_pair();

        validate_interactive_tty_pair(&first_slave, &first_slave).unwrap();
        let error = validate_interactive_tty_pair(&first_slave, &second_slave).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("different TTY devices"));
    }

    #[test]
    fn ordinary_writer_rejects_unscoped_output() {
        let (_master, slave) = pty_pair();
        let mut writer = writer_for(slave);

        let error = writer.write_all(b"must be scoped").unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("without an active operation"));
    }

    #[test]
    fn zero_budget_operation_is_rejected_without_becoming_active() {
        let (_master, slave) = pty_pair();
        let writer = writer_for(slave);

        let error = writer
            .begin_operation("expired startup", Duration::ZERO)
            .err()
            .expect("zero budget must be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(operation_snapshot(&writer.shared).is_none());
    }

    #[test]
    fn pre_tui_operation_deadline_is_clamped_by_both_bounds() {
        let now = Instant::now();
        let overall = now + Duration::from_secs(30);
        assert_eq!(
            clamp_operation_deadline(now, overall, Duration::from_secs(3)),
            now + Duration::from_secs(3)
        );

        let near_overall = now + Duration::from_millis(200);
        assert_eq!(
            clamp_operation_deadline(now, near_overall, Duration::from_secs(3)),
            near_overall
        );
    }

    #[test]
    fn final_flush_rejects_an_operation_that_expired_without_more_output() {
        let (_master, slave) = pty_pair();
        let mut writer = writer_for(slave);
        let _operation = writer
            .begin_operation("terminal startup", Duration::from_millis(5))
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));

        let error = writer.flush().unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let message = error.to_string();
        assert!(message.contains("terminal startup"));
        assert!(message.contains("phase=Flushing"));
    }

    #[test]
    fn operation_snapshot_tracks_generation_elapsed_and_phase() {
        let (_master, slave) = pty_pair();
        let writer = writer_for(slave);
        let operation = writer
            .begin_operation("snapshot test", Duration::from_secs(1))
            .unwrap();

        let preparing = operation_snapshot(&writer.shared).unwrap();
        assert_eq!(preparing.label, "snapshot test");
        assert_eq!(preparing.phase, OutputOperationPhase::Preparing);
        assert!(preparing.elapsed < Duration::from_secs(1));

        {
            let _phase = PhaseGuard::enter(
                &writer.shared,
                preparing.generation,
                OutputOperationPhase::Writing,
            )
            .unwrap();
            assert_eq!(
                operation_snapshot(&writer.shared).unwrap().phase,
                OutputOperationPhase::Writing
            );
        }
        assert_eq!(
            operation_snapshot(&writer.shared).unwrap().phase,
            OutputOperationPhase::Preparing
        );
        drop(operation);
        assert!(operation_snapshot(&writer.shared).is_none());
    }

    #[test]
    fn idle_between_writes_does_not_extend_the_operation_deadline() {
        let (_master, slave) = pty_pair();
        let writer = writer_for(slave);
        let _guard = writer
            .begin_operation("scheduled idle test", Duration::from_millis(100))
            .unwrap();
        let now = Instant::now();
        let operation = {
            let mut active = super::lock_unpoisoned(&writer.shared.operation);
            let operation = active.as_mut().unwrap();
            operation.started_at = now - Duration::from_secs(2);
            operation.deadline = now - Duration::from_millis(500);
            *operation
        };

        let error = validate_operation(&writer.shared, operation, true).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn delayed_reader_preserves_every_byte_in_order() {
        let (master, slave) = pty_pair();
        let mut writer = writer_for(slave);
        // Printable bytes avoid the slave's output post-processing changing the assertion data.
        let expected: Vec<u8> = (0..256 * 1024)
            .map(|index| b'A' + (index % 26) as u8)
            .collect();
        let sent = expected.clone();

        let writer_thread = std::thread::spawn(move || {
            let _operation = writer
                .begin_operation("delayed reader test", Duration::from_secs(3))
                .unwrap();
            writer.write_all(&sent)
        });
        std::thread::sleep(Duration::from_millis(75));

        let mut received = Vec::with_capacity(expected.len());
        let mut chunk = [0u8; 8192];
        let read_deadline = Instant::now() + Duration::from_secs(4);
        while received.len() < expected.len() {
            match rustix::io::read(&master, &mut chunk) {
                Ok(read) if read > 0 => received.extend_from_slice(&chunk[..read]),
                Err(rustix::io::Errno::AGAIN) if Instant::now() < read_deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                outcome => panic!("PTY reader did not complete before its bound: {outcome:?}"),
            }
        }

        writer_thread.join().unwrap().unwrap();
        assert_eq!(received, expected);
    }

    #[test]
    fn blocked_output_honors_the_absolute_operation_deadline() {
        let (_master, slave) = pty_pair();
        let mut writer = writer_for(slave);
        let _operation = writer
            .begin_operation("blocked output test", Duration::from_millis(100))
            .unwrap();
        let payload = vec![b'x'; 2 * 1024 * 1024];
        let started = Instant::now();

        let error = writer.write_all(&payload).unwrap_err();
        let elapsed = started.elapsed();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let message = error.to_string();
        assert!(message.contains("blocked output test"));
        assert!(message.contains("phase=Writing"));
        assert!(message.contains("generation=1"));
        assert!(message.contains("elapsed_ms="));
        assert!(elapsed >= Duration::from_millis(75), "elapsed={elapsed:?}");
        assert!(elapsed < Duration::from_secs(1), "elapsed={elapsed:?}");
    }

    #[test]
    fn emergency_restore_uses_its_fresh_budget_and_diagnostic_label() {
        let (_master, slave) = pty_pair();
        let writer = writer_for(slave);
        let mut output = writer.emergency(Duration::from_millis(75));
        let _operation = output
            .begin_operation_until(
                "panic emergency terminal restore",
                Instant::now() + Duration::from_millis(75),
            )
            .unwrap();
        let started = Instant::now();

        let error = output.write_all(&vec![b'x'; 2 * 1024 * 1024]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            error
                .to_string()
                .contains("panic emergency terminal restore")
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn delayed_emergency_worker_cannot_write_after_its_original_deadline() {
        let (master, slave) = pty_pair();
        let writer = writer_for(slave);
        let mut output = writer.emergency(Duration::from_millis(10));
        let error = output
            .begin_operation_until(
                "delayed emergency restore",
                Instant::now() - Duration::from_millis(1),
            )
            .err()
            .expect("expired emergency operation must be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        let mut received = [0_u8; 32];
        assert_eq!(
            rustix::io::read(&master, &mut received),
            Err(rustix::io::Errno::AGAIN)
        );
    }

    #[test]
    fn emergency_replacement_prevents_a_waiting_old_control_write_and_publish() {
        let (master, slave) = pty_pair();
        let writer = writer_for(slave);
        let original = writer
            .begin_operation("startup control", Duration::from_secs(1))
            .unwrap();
        let fence = super::lock_unpoisoned(&writer.shared.write_fence);
        let published = Arc::new(AtomicBool::new(false));
        let thread_published = Arc::clone(&published);
        let mut old_writer = writer.clone();
        let old = std::thread::spawn(move || {
            old_writer.write_all_then(b"old-enable", || {
                thread_published.store(true, Ordering::Release);
            })
        });

        let phase_deadline = Instant::now() + Duration::from_secs(1);
        while operation_snapshot(&writer.shared)
            .is_none_or(|snapshot| snapshot.phase != OutputOperationPhase::Writing)
        {
            assert!(
                Instant::now() < phase_deadline,
                "old writer did not reach its fenced write"
            );
            std::thread::yield_now();
        }

        let mut emergency = writer.emergency(Duration::from_secs(1));
        let takeover = emergency
            .begin_operation_until("emergency inverse", Instant::now() + Duration::from_secs(1))
            .unwrap();
        drop(fence);
        let old_error = old.join().unwrap().unwrap_err();
        assert_eq!(old_error.kind(), std::io::ErrorKind::ConnectionAborted);
        assert!(!published.load(Ordering::Acquire));

        emergency.write_all(b"inverse").unwrap();
        assert_eq!(read_pty_bytes(&master, b"inverse".len()), b"inverse");
        drop(takeover);
        drop(original);
    }

    #[test]
    fn takeover_barrier_waits_for_an_admitted_control_write_and_its_marker() {
        let (master, slave) = pty_pair();
        let writer = writer_for(slave);
        let original = writer
            .begin_operation("startup activation", Duration::from_secs(1))
            .unwrap();
        let published = Arc::new(AtomicBool::new(false));
        let thread_published = Arc::clone(&published);
        let (callback_tx, callback_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let mut old_writer = writer.clone();
        let old = std::thread::spawn(move || {
            old_writer.write_all_then(b"push", || {
                callback_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                thread_published.store(true, Ordering::Release);
            })
        });
        callback_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let mut emergency = writer.emergency(Duration::from_secs(1));
        let takeover = emergency
            .begin_operation_until("emergency cleanup", Instant::now() + Duration::from_secs(1))
            .unwrap();
        let (barrier_tx, barrier_rx) = std::sync::mpsc::sync_channel(1);
        let barrier = std::thread::spawn(move || {
            let result = emergency.takeover_barrier_until(Instant::now() + Duration::from_secs(1));
            barrier_tx.send(()).unwrap();
            (emergency, result)
        });
        assert!(
            barrier_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "takeover passed the fence before the activation marker was published"
        );

        release_tx.send(()).unwrap();
        old.join().unwrap().unwrap();
        barrier_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let (mut emergency, barrier_result) = barrier.join().unwrap();
        barrier_result.unwrap();
        assert!(published.load(Ordering::Acquire));
        emergency.write_all(b"pop").unwrap();

        assert_eq!(read_pty_bytes(&master, b"pushpop".len()), b"pushpop");
        drop(takeover);
        drop(original);
    }

    #[test]
    fn cancellation_wakes_a_blocked_writer_before_its_deadline() {
        let (_master, slave) = pty_pair();
        let mut writer = writer_for(slave);
        let cancellation = PreTuiOutputCancellation {
            shared: writer.shared.clone(),
        };
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let started = Instant::now();
        let writer_thread = std::thread::spawn(move || {
            let _operation = writer
                .begin_operation("cancel test", Duration::from_secs(5))
                .unwrap();
            started_tx.send(()).unwrap();
            writer.write_all(&vec![b'x'; 2 * 1024 * 1024])
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        cancellation.cancel();
        let error = writer_thread.join().unwrap().unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionAborted);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn closed_pty_maps_to_definitive_broken_pipe() {
        let (master, slave) = pty_pair();
        let mut writer = writer_for(slave);
        let _operation = writer
            .begin_operation("closed PTY test", Duration::from_secs(1))
            .unwrap();
        drop(master);

        let error = writer.write_all(&vec![b'x'; 2 * 1024 * 1024]).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        let message = error.to_string();
        assert!(
            message.contains("os error") || message.contains("poll revents"),
            "message={message}"
        );
    }
}
