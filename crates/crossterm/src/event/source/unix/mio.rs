use std::{io, time::Duration};

use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use signal_hook_mio::v1_0::Signals;

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{source::EventSource, timeout::PollTimeout, Event, InternalEvent};

use super::input::{InputFd, Parser};

const TTY_TOKEN: Token = Token(0);
const SIGNAL_TOKEN: Token = Token(1);
#[cfg(feature = "event-stream")]
const WAKE_TOKEN: Token = Token(2);
const TTY_BUFFER_SIZE: usize = 4 * 1024;
const MAX_DRAIN_BYTES: usize = 64 * 1024;
const MAX_DRAIN_TIME: Duration = Duration::from_millis(50);

pub(crate) struct UnixInternalEventSource {
    poll: Poll,
    events: Events,
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty: InputFd,
    drain_pending: bool,
    signals: Signals,
    #[cfg(feature = "event-stream")]
    wake_pending: bool,
    #[cfg(feature = "event-stream")]
    waker: Waker,
}

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        Self::from_input_fd(InputFd::open()?)
    }

    fn from_input_fd(input_fd: InputFd) -> io::Result<Self> {
        let poll = Poll::new()?;
        let registry = poll.registry();

        let tty_raw_fd = input_fd.raw_fd();
        registry.register(&mut SourceFd(&tty_raw_fd), TTY_TOKEN, Interest::READABLE)?;

        let mut signals = Signals::new([signal_hook::consts::SIGWINCH])?;
        registry.register(&mut signals, SIGNAL_TOKEN, Interest::READABLE)?;

        #[cfg(feature = "event-stream")]
        let waker = Waker::new(registry, WAKE_TOKEN)?;

        Ok(Self {
            poll,
            events: Events::with_capacity(3),
            parser: Parser::default(),
            tty_buffer: [0; TTY_BUFFER_SIZE],
            tty: input_fd,
            drain_pending: false,
            signals,
            #[cfg(feature = "event-stream")]
            wake_pending: false,
            #[cfg(feature = "event-stream")]
            waker,
        })
    }

    fn drain_tty(&mut self, close_hint: bool, timeout: &PollTimeout) -> io::Result<()> {
        let mut drained = 0;
        let drain_timeout = PollTimeout::new(Some(MAX_DRAIN_TIME));
        let mut attempted_once = false;
        loop {
            // Preserve poll(0)'s readiness check by allowing one nonblocking read. Every retry,
            // including an EINTR storm, remains inside both the caller's absolute timeout and a
            // short per-drain scheduling slice.
            if attempted_once && (timeout.elapsed() || drain_timeout.elapsed()) {
                self.drain_pending = true;
                return Ok(());
            }
            attempted_once = true;
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
                    drained += read_count;
                    self.parser.advance(
                        &self.tty_buffer[..read_count],
                        read_count == TTY_BUFFER_SIZE,
                    )?;
                    if drained >= MAX_DRAIN_BYTES {
                        // Do not return to edge-triggered poll until a later call has completed
                        // the drain. Buffered parsed events give callers a fair scheduling point.
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

    fn wait_duration(timeout: &PollTimeout, parser: &Parser) -> Option<Duration> {
        match (timeout.leftover(), parser.pending_wait()) {
            (Some(timeout), Some(pending)) => Some(timeout.min(pending)),
            (Some(timeout), None) => Some(timeout),
            (None, Some(pending)) => Some(pending),
            (None, None) => None,
        }
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
                self.drain_tty(false, &timeout)?;
                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
                if self.drain_pending {
                    // yututui patch: one event-source call drains at most 64 KiB when no
                    // complete event exists. Returning control here lets the application mark
                    // watchdog progress during a long, still-incomplete bracketed paste.
                    return Ok(None);
                }
                continue;
            }

            if polled_once && timeout.elapsed() {
                return Ok(None);
            }
            polled_once = true;

            match self.poll.poll(
                &mut self.events,
                Self::wait_duration(&timeout, &self.parser),
            ) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
                Ok(()) => {}
            }

            if self.events.is_empty() {
                self.parser.expire_stale();
                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
                if timeout.elapsed() {
                    return Ok(None);
                }
                continue;
            }

            // Collect every readiness flag before doing any work. Returning an input event must
            // not lose a simultaneous SIGWINCH or async wake edge.
            let mut tty_ready = false;
            let mut tty_closed = false;
            let mut signal_ready = false;
            #[cfg(feature = "event-stream")]
            let mut wake_ready = false;
            for event in self.events.iter() {
                match event.token() {
                    TTY_TOKEN => {
                        tty_ready |= event.is_readable();
                        tty_closed |= event.is_read_closed() || event.is_error();
                    }
                    SIGNAL_TOKEN => signal_ready = true,
                    #[cfg(feature = "event-stream")]
                    WAKE_TOKEN => wake_ready = true,
                    _ => unreachable!("event token registration and handling diverged"),
                }
            }

            if tty_ready || tty_closed {
                self.drain_tty(tty_closed, &timeout)?;
            }
            let mut saw_winch = false;
            if signal_ready {
                // Drain every pending signal; `Iterator::any` would leave later readiness behind.
                for signal in self.signals.pending() {
                    saw_winch |= signal == signal_hook::consts::SIGWINCH;
                }
            }
            if saw_winch {
                self.queue_resize()?;
            }
            #[cfg(feature = "event-stream")]
            if wake_ready {
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
        }
    }

    fn pending_input_is_recent(&mut self) -> bool {
        self.parser.pending_input_is_recent()
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.waker.clone()
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
