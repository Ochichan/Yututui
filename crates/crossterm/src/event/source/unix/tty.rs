use std::{
    io::{self, Read},
    os::unix::net::UnixStream,
    os::unix::prelude::AsRawFd,
    time::Duration,
};

use filedescriptor::{poll, pollfd, POLLERR, POLLHUP, POLLIN};
use signal_hook::low_level::pipe;

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{source::EventSource, timeout::PollTimeout, Event, InternalEvent};

use super::input::{DrainBudget, InputFd, Parser, MAX_DRAIN_BYTES, MAX_DRAIN_TIME};

const TTY_BUFFER_SIZE: usize = 4 * 1024;

#[cfg(feature = "event-stream")]
struct WakePipe {
    receiver: UnixStream,
    waker: Waker,
}

#[cfg(feature = "event-stream")]
impl WakePipe {
    fn new() -> io::Result<Self> {
        let (receiver, sender) = nonblocking_unix_pair()?;
        Ok(Self {
            receiver,
            waker: Waker::new(sender),
        })
    }
}

pub(crate) struct UnixInternalEventSource {
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty: InputFd,
    drain_pending: bool,
    winch_signal_receiver: UnixStream,
    #[cfg(feature = "event-stream")]
    wake_pending: bool,
    #[cfg(feature = "event-stream")]
    wake_pipe: WakePipe,
}

fn nonblocking_unix_pair() -> io::Result<(UnixStream, UnixStream)> {
    let (receiver, sender) = UnixStream::pair()?;
    receiver.set_nonblocking(true)?;
    sender.set_nonblocking(true)?;
    Ok((receiver, sender))
}

fn drain_stream(stream: &mut UnixStream, timeout: &PollTimeout) -> io::Result<()> {
    let mut buffer = [0; 1024];
    let mut drained = 0;
    let drain_timeout = PollTimeout::new(Some(MAX_DRAIN_TIME));
    let mut attempted_once = false;
    loop {
        if attempted_once && (timeout.elapsed() || drain_timeout.elapsed()) {
            return Ok(());
        }
        attempted_once = true;
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => {
                drained += read;
                if drained >= MAX_DRAIN_BYTES {
                    return Ok(());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        Self::from_input_fd(InputFd::open()?)
    }

    fn from_input_fd(input_fd: InputFd) -> io::Result<Self> {
        Ok(Self {
            parser: Parser::default(),
            tty_buffer: [0; TTY_BUFFER_SIZE],
            tty: input_fd,
            drain_pending: false,
            winch_signal_receiver: {
                let (receiver, sender) = nonblocking_unix_pair()?;
                // EventSource is process-global, so explicit unregistering is unnecessary.
                #[cfg(feature = "libc")]
                pipe::register(libc::SIGWINCH, sender)?;
                #[cfg(not(feature = "libc"))]
                pipe::register(rustix::process::Signal::WINCH.as_raw(), sender)?;
                receiver
            },
            #[cfg(feature = "event-stream")]
            wake_pending: false,
            #[cfg(feature = "event-stream")]
            wake_pipe: WakePipe::new()?,
        })
    }

    fn drain_tty(
        &mut self,
        close_hint: bool,
        timeout: &PollTimeout,
        budget: &mut DrainBudget,
    ) -> io::Result<()> {
        loop {
            // Preserve poll(0)'s readiness check by allowing one nonblocking read. Every retry,
            // including an EINTR storm, remains inside both the caller's absolute timeout and the
            // scheduling slice shared by this entire event-source call.
            let first_attempt = budget.start();
            if !first_attempt && (timeout.elapsed() || budget.exhausted()) {
                self.drain_pending = true;
                return Ok(());
            }
            match self.tty.read(&mut self.tty_buffer) {
                Ok(0) if self.tty.read_zero_is_eof(close_hint) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal event input closed",
                    ));
                }
                Ok(0) => {
                    self.drain_pending = false;
                    return Ok(());
                }
                Ok(read_count) => {
                    budget.record(read_count);
                    self.parser.advance(
                        &self.tty_buffer[..read_count],
                        read_count == TTY_BUFFER_SIZE,
                    )?;
                    if budget.exhausted() {
                        self.drain_pending = true;
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.drain_pending = false;
                    if close_hint {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "terminal event input hung up",
                        ));
                    }
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn wait_duration(
        timeout: &PollTimeout,
        parser: &Parser,
        budget: &DrainBudget,
    ) -> Option<Duration> {
        [
            timeout.leftover(),
            parser.pending_wait(),
            budget.time_left(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn queue_resize(&mut self) -> io::Result<()> {
        let size = crate::terminal::sys::window_size()?;
        self.parser
            .push(InternalEvent::Event(Event::Resize(size.columns, size.rows)));
        Ok(())
    }
}

impl EventSource for UnixInternalEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        let timeout = PollTimeout::new(timeout);
        let mut drain_budget = DrainBudget::default();
        let mut polled_once = false;

        loop {
            self.parser.expire_stale();
            #[cfg(feature = "event-stream")]
            if self.wake_pending {
                self.wake_pending = false;
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Poll operation was woken up by `Waker::wake`",
                ));
            }
            if let Some(event) = self.parser.next() {
                return Ok(Some(event));
            }

            if self.drain_pending {
                self.drain_tty(false, &timeout, &mut drain_budget)?;
                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
                if self.drain_pending {
                    // yututui patch: keep a single source call bounded even while an incomplete
                    // paste remains continuously readable, so the owner can report progress.
                    return Ok(None);
                }
                continue;
            }

            if drain_budget.exhausted() {
                return Ok(None);
            }
            if polled_once && timeout.elapsed() {
                return Ok(None);
            }
            polled_once = true;

            fn poll_fd<F: AsRawFd + ?Sized>(fd: &F) -> pollfd {
                pollfd {
                    fd: fd.as_raw_fd(),
                    events: POLLIN,
                    revents: 0,
                }
            }

            #[cfg(not(feature = "event-stream"))]
            let mut fds = [poll_fd(&self.tty), poll_fd(&self.winch_signal_receiver)];
            #[cfg(feature = "event-stream")]
            let mut fds = [
                poll_fd(&self.tty),
                poll_fd(&self.winch_signal_receiver),
                poll_fd(&self.wake_pipe.receiver),
            ];

            match poll(
                &mut fds,
                Self::wait_duration(&timeout, &self.parser, &drain_budget),
            ) {
                Err(filedescriptor::Error::Poll(error)) | Err(filedescriptor::Error::Io(error))
                    if error.kind() == io::ErrorKind::Interrupted =>
                {
                    continue
                }
                Err(filedescriptor::Error::Poll(error)) | Err(filedescriptor::Error::Io(error)) => {
                    return Err(error)
                }
                Err(error) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("unexpected terminal poll error: {error:?}"),
                    ))
                }
                Ok(_) => {}
            }

            if fds.iter().all(|fd| fd.revents == 0)
                && (timeout.elapsed() || drain_budget.exhausted())
            {
                return Ok(None);
            }

            let tty_ready = fds[0].revents & POLLIN != 0;
            let tty_closed = fds[0].revents & (POLLHUP | POLLERR) != 0;
            let signal_ready = fds[1].revents & POLLIN != 0;
            #[cfg(feature = "event-stream")]
            let wake_ready = fds[2].revents & POLLIN != 0;

            if tty_ready || tty_closed {
                self.drain_tty(tty_closed, &timeout, &mut drain_budget)?;
            }
            if signal_ready {
                drain_stream(&mut self.winch_signal_receiver, &timeout)?;
                self.queue_resize()?;
            }
            #[cfg(feature = "event-stream")]
            if wake_ready {
                drain_stream(&mut self.wake_pipe.receiver, &timeout)?;
                self.wake_pending = true;
            }

            if self.drain_pending {
                #[cfg(feature = "event-stream")]
                if self.wake_pending {
                    self.wake_pending = false;
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "Poll operation was woken up by `Waker::wake`",
                    ));
                }
                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
                return Ok(None);
            }

            self.parser.expire_stale();
        }
    }

    fn pending_input_is_recent(&mut self) -> bool {
        self.parser.pending_input_is_recent()
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.wake_pipe.waker.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use rustix::io::write;

    use super::*;
    use crate::event::{source::unix::input::tests::raw_pty, KeyCode, KeyEvent, KeyModifiers};

    fn bounded_read(
        mut source: UnixInternalEventSource,
        timeout: Duration,
    ) -> (UnixInternalEventSource, io::Result<Option<InternalEvent>>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result = source.try_read(Some(timeout));
            let _ = sender.send((source, result));
        });
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("terminal event read exceeded the parent wall-clock timeout")
    }

    #[test]
    fn partial_utf8_cannot_block_a_bounded_poll() {
        let (master, slave) = raw_pty();
        let source =
            UnixInternalEventSource::from_input_fd(InputFd::from_owned_for_test(slave)).unwrap();
        write(&master, &[0xe2]).unwrap();

        let started = Instant::now();
        let (source, first) = bounded_read(source, Duration::from_millis(40));
        assert_eq!(first.unwrap(), None);
        assert!(started.elapsed() < Duration::from_secs(1));

        write(&master, &[0x82, 0xac]).unwrap();
        let (_, second) = bounded_read(source, Duration::from_secs(1));
        assert_eq!(
            second.unwrap(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('€'),
                KeyModifiers::NONE
            ))))
        );
    }

    #[test]
    fn fragmented_focus_sequence_survives_separate_reads() {
        let (master, slave) = raw_pty();
        let source =
            UnixInternalEventSource::from_input_fd(InputFd::from_owned_for_test(slave)).unwrap();
        write(&master, b"\x1b").unwrap();

        let (source, first) = bounded_read(source, Duration::from_millis(40));
        assert_eq!(first.unwrap(), None);
        write(&master, b"[I").unwrap();
        let (_, second) = bounded_read(source, Duration::from_secs(1));
        assert_eq!(
            second.unwrap(),
            Some(InternalEvent::Event(Event::FocusGained))
        );
    }

    #[test]
    fn closing_the_pty_master_returns_instead_of_spinning() {
        let (master, slave) = raw_pty();
        let source =
            UnixInternalEventSource::from_input_fd(InputFd::from_owned_for_test(slave)).unwrap();
        drop(master);

        let started = Instant::now();
        let (_, result) = bounded_read(source, Duration::from_secs(1));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn drain_budget_survives_would_block_between_fragments() {
        let (master, slave) = raw_pty();
        let mut source =
            UnixInternalEventSource::from_input_fd(InputFd::from_owned_for_test(slave)).unwrap();
        let timeout = PollTimeout::new(Some(Duration::from_secs(1)));
        let mut budget = DrainBudget::with_test_limits(5, Duration::from_secs(1));

        write(&master, b"abc").unwrap();
        source.drain_tty(false, &timeout, &mut budget).unwrap();
        assert!(!budget.exhausted());
        assert!(!source.drain_pending);

        write(&master, b"def").unwrap();
        source.drain_tty(false, &timeout, &mut budget).unwrap();
        assert!(budget.exhausted());
        assert!(source.drain_pending);
    }

    #[cfg(feature = "bracketed-paste")]
    #[test]
    fn incomplete_paste_yields_after_one_drain_budget() {
        let (master, slave) = raw_pty();
        let source =
            UnixInternalEventSource::from_input_fd(InputFd::from_owned_for_test(slave)).unwrap();
        let mut paste = Vec::with_capacity(MAX_DRAIN_BYTES);
        paste.extend_from_slice(b"\x1b[200~");
        paste.resize(MAX_DRAIN_BYTES, b'x');
        let writer = std::thread::spawn(move || {
            let mut written = 0;
            while written < paste.len() {
                match write(&master, &paste[written..]) {
                    Ok(0) => panic!("PTY writer made no progress"),
                    Ok(count) => written += count,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(error) => panic!("PTY write failed: {error}"),
                }
            }
            master
        });

        let started = Instant::now();
        let (source, result) = bounded_read(source, Duration::from_secs(1));

        assert_eq!(result.unwrap(), None);
        assert!(source.drain_pending);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer.join().unwrap());
    }
}
