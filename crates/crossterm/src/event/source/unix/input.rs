use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

use rustix::{
    fd::{AsFd, AsRawFd, OwnedFd, RawFd},
    fs::{fcntl_getfl, fstat, open, Mode, OFlags},
    termios::{isatty, tcgetattr, ttyname, LocalModes, SpecialCodeIndex},
};

use crate::event::{sys::unix::parse::parse_event, Event, InternalEvent, KeyCode};

const GENERIC_PENDING_IDLE: Duration = Duration::from_secs(1);
// A lone ESC is both a complete key and the prefix of every legacy terminal control sequence.
// Keep a short ambiguity window so a syscall boundary immediately after ESC does not split
// CSI/focus/CPR input, without imposing the full generic-prefix timeout on the Esc key.
const ESC_PENDING_IDLE: Duration = Duration::from_millis(100);
#[cfg(feature = "bracketed-paste")]
const PASTE_PENDING_IDLE: Duration = Duration::from_secs(3);
#[cfg(feature = "bracketed-paste")]
const PASTE_START: &[u8] = b"\x1b[200~";
#[cfg(feature = "bracketed-paste")]
const PASTE_END: &[u8] = b"\x1b[201~";
#[cfg(feature = "bracketed-paste")]
const MAX_PASTE_BYTES: usize = 16 * 1024 * 1024;

/// An independently opened, non-blocking descriptor for terminal event input.
///
/// `dup` is deliberately not used: file status flags such as `O_NONBLOCK` live on the
/// open-file-description and would otherwise leak back to the invoking shell after a crash.
/// yututui patch: make Mio's drain-to-WouldBlock contract safe without mutating inherited stdin.
#[derive(Debug)]
pub(super) struct InputFd {
    fd: OwnedFd,
}

impl InputFd {
    pub(super) fn open() -> io::Result<Self> {
        let stdin = rustix::stdio::stdin();
        if isatty(stdin) {
            Self::reopen(stdin)
        } else {
            Self::open_path("/dev/tty")
        }
    }

    fn reopen(fd: impl AsFd) -> io::Result<Self> {
        let expected = fstat(fd.as_fd())?;
        let path = ttyname(fd.as_fd(), Vec::new())?;
        let reopened = Self::open_path(path.as_c_str())?;
        if fstat(&reopened.fd)?.st_rdev != expected.st_rdev {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reopened terminal event input refers to a different device",
            ));
        }
        Ok(reopened)
    }

    fn open_path<P: rustix::path::Arg>(path: P) -> io::Result<Self> {
        let fd = open(
            path,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )?;

        if !isatty(&fd) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal event input is not a TTY",
            ));
        }

        if !fcntl_getfl(&fd)?.contains(OFlags::NONBLOCK) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "terminal event input could not be opened non-blocking",
            ));
        }

        Ok(Self { fd })
    }

    #[cfg(test)]
    pub(super) fn reopen_for_test(fd: impl AsFd) -> io::Result<Self> {
        Self::reopen(fd)
    }

    #[cfg(test)]
    pub(super) fn from_owned_for_test(fd: OwnedFd) -> Self {
        Self { fd }
    }

    pub(super) fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        rustix::io::read(&self.fd, buffer).map_err(Into::into)
    }

    pub(super) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub(super) fn read_zero_is_eof(&self, close_hint: bool) -> bool {
        if close_hint {
            return true;
        }

        tcgetattr(&self.fd).map_or(false, |termios| {
            !termios.local_modes.contains(LocalModes::ICANON)
                && termios.special_codes[SpecialCodeIndex::VMIN] > 0
        })
    }
}

impl AsRawFd for InputFd {
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd()
    }
}

#[derive(Debug)]
pub(super) struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
    last_input_at: Option<Instant>,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            buffer: Vec::with_capacity(256),
            internal_events: VecDeque::with_capacity(128),
            last_input_at: None,
        }
    }
}

impl Parser {
    pub(super) fn advance(&mut self, bytes: &[u8], more: bool) -> io::Result<()> {
        self.advance_at(bytes, more, Instant::now())
    }

    fn advance_at(&mut self, bytes: &[u8], more: bool, now: Instant) -> io::Result<()> {
        for (index, byte) in bytes.iter().copied().enumerate() {
            let following_input_available = index + 1 < bytes.len() || more;

            if self.buffer.as_slice() == b"\x1b" && byte == b'\x1b' {
                // yututui patch: the first ESC is a complete key while the second ESC can begin a
                // new CSI/focus/CPR sequence. Emit the old ambiguity and retain the current byte
                // as the next prefix instead of letting `parse_event([ESC, ESC])` consume both.
                self.internal_events
                    .push_back(InternalEvent::Event(Event::Key(KeyCode::Esc.into())));
                self.clear_pending();
            }

            let had_prefix = !self.buffer.is_empty();
            self.buffer.push(byte);
            self.last_input_at = Some(now);

            #[cfg(feature = "bracketed-paste")]
            if self.buffer.starts_with(PASTE_START)
                && self.buffer.len() > PASTE_START.len() + MAX_PASTE_BYTES
            {
                let possible_end = &self.buffer[PASTE_START.len() + MAX_PASTE_BYTES..];
                if !PASTE_END.starts_with(possible_end) {
                    self.buffer.clear();
                    self.last_input_at = None;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bracketed paste exceeded the 16 MiB input limit",
                    ));
                }
            }

            let input_available = following_input_available || self.buffer.as_slice() == b"\x1b";
            match parse_event(&self.buffer, input_available) {
                Ok(Some(event)) => {
                    self.internal_events.push_back(event);
                    self.clear_pending();
                }
                Ok(None) => {}
                Err(_) if had_prefix => {
                    // yututui patch: a stale partial sequence must not consume the first byte of
                    // the next event (notably a cursor-position response). Retry it once as a new
                    // sequence; a second parse failure simply discards the byte.
                    self.buffer.clear();
                    self.buffer.push(byte);
                    let input_available =
                        following_input_available || self.buffer.as_slice() == b"\x1b";
                    match parse_event(&self.buffer, input_available) {
                        Ok(Some(event)) => {
                            self.internal_events.push_back(event);
                            self.clear_pending();
                        }
                        Ok(None) => {}
                        Err(_) => self.clear_pending(),
                    }
                }
                Err(_) => self.clear_pending(),
            }
        }
        Ok(())
    }

    pub(super) fn next(&mut self) -> Option<InternalEvent> {
        self.internal_events.pop_front()
    }

    pub(super) fn push(&mut self, event: InternalEvent) {
        self.internal_events.push_back(event);
    }

    pub(super) fn expire_stale(&mut self) {
        self.expire_stale_at(Instant::now());
    }

    fn expire_stale_at(&mut self, now: Instant) {
        let last_input_at = match self.last_input_at {
            Some(last_input_at) => last_input_at,
            None => return,
        };
        if self.buffer.is_empty()
            || now.saturating_duration_since(last_input_at) < self.idle_limit()
        {
            return;
        }

        if self.buffer.as_slice() == b"\x1b" {
            self.internal_events
                .push_back(InternalEvent::Event(Event::Key(KeyCode::Esc.into())));
        } else {
            #[cfg(feature = "bracketed-paste")]
            if self.buffer.starts_with(PASTE_START) {
                let payload =
                    String::from_utf8_lossy(&self.buffer[PASTE_START.len()..]).into_owned();
                self.internal_events
                    .push_back(InternalEvent::Event(Event::Paste(payload)));
            }
        }
        self.clear_pending();
    }

    pub(super) fn pending_input_is_recent(&mut self) -> bool {
        self.expire_stale();
        !self.buffer.is_empty()
    }

    pub(super) fn pending_wait(&self) -> Option<Duration> {
        let last_input_at = self.last_input_at?;
        if self.buffer.is_empty() {
            return None;
        }
        Some(
            self.idle_limit()
                .saturating_sub(Instant::now().saturating_duration_since(last_input_at)),
        )
    }

    fn idle_limit(&self) -> Duration {
        if self.buffer.as_slice() == b"\x1b" {
            return ESC_PENDING_IDLE;
        }
        #[cfg(feature = "bracketed-paste")]
        if self.buffer.starts_with(PASTE_START) {
            return PASTE_PENDING_IDLE;
        }
        GENERIC_PENDING_IDLE
    }

    fn clear_pending(&mut self) {
        self.buffer.clear();
        self.last_input_at = None;
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        Parser::next(self)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::event::{KeyEvent, KeyModifiers};
    use rustix::{
        fs::{fcntl_getfl, open, Mode, OFlags},
        pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags},
        termios::{tcgetattr, tcsetattr, OptionalActions},
    };

    pub(crate) fn raw_pty() -> (OwnedFd, OwnedFd) {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let path = ptsname(&master, Vec::new()).unwrap();
        let slave = open(
            path.as_c_str(),
            OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )
        .unwrap();
        let mut termios = tcgetattr(&slave).unwrap();
        termios.make_raw();
        tcsetattr(&slave, OptionalActions::Now, &termios).unwrap();
        (master, slave)
    }

    #[test]
    fn reopening_a_tty_does_not_change_the_original_file_status_flags() {
        let (master, nonblocking_slave) = raw_pty();
        let path = ptsname(&master, Vec::new()).unwrap();
        let original = open(
            path.as_c_str(),
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )
        .unwrap();
        assert!(!fcntl_getfl(&original).unwrap().contains(OFlags::NONBLOCK));

        let reopened = InputFd::reopen_for_test(&original).unwrap();

        assert!(fcntl_getfl(&reopened.fd)
            .unwrap()
            .contains(OFlags::NONBLOCK));
        assert!(!fcntl_getfl(&original).unwrap().contains(OFlags::NONBLOCK));
        drop(nonblocking_slave);
    }

    #[test]
    fn read_zero_is_only_eof_for_closed_or_blocking_raw_input() {
        let (_master, slave) = raw_pty();
        let input = InputFd::from_owned_for_test(slave);
        assert!(input.read_zero_is_eof(false));

        let mut termios = tcgetattr(&input.fd).unwrap();
        termios.special_codes[SpecialCodeIndex::VMIN] = 0;
        tcsetattr(&input.fd, OptionalActions::Now, &termios).unwrap();
        assert!(!input.read_zero_is_eof(false));
        assert!(input.read_zero_is_eof(true));

        termios.local_modes.insert(LocalModes::ICANON);
        termios.special_codes[SpecialCodeIndex::VMIN] = 1;
        tcsetattr(&input.fd, OptionalActions::Now, &termios).unwrap();
        assert!(!input.read_zero_is_eof(false));
    }

    #[test]
    fn stale_utf8_prefix_retries_the_current_byte_as_a_new_event() {
        let mut parser = Parser::default();
        parser.advance_at(&[0xe2], false, Instant::now()).unwrap();
        parser.advance_at(b"x", false, Instant::now()).unwrap();
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE
            ))))
        );
    }

    #[test]
    fn stale_utf8_prefix_does_not_consume_a_cursor_response() {
        let mut parser = Parser::default();
        parser.advance_at(&[0xe2], false, Instant::now()).unwrap();
        parser
            .advance_at(b"\x1b[20;10R", false, Instant::now())
            .unwrap();
        assert_eq!(parser.next(), Some(InternalEvent::CursorPosition(9, 19)));
        assert_eq!(parser.next(), None);
    }

    #[test]
    fn stale_csi_prefix_retries_the_current_byte_as_a_new_event() {
        let mut parser = Parser::default();
        parser.advance_at(b"\x1b[", false, Instant::now()).unwrap();
        parser.advance_at(b"x", false, Instant::now()).unwrap();
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::NONE
            ))))
        );
    }

    #[test]
    fn fragmented_focus_sequence_is_preserved() {
        let now = Instant::now();
        let sequence = b"\x1b[I";
        for split in 1..sequence.len() {
            let mut parser = Parser::default();
            parser.advance_at(&sequence[..split], false, now).unwrap();
            assert_eq!(parser.next(), None, "split={split}");
            parser.advance_at(&sequence[split..], false, now).unwrap();
            assert_eq!(
                parser.next(),
                Some(InternalEvent::Event(Event::FocusGained)),
                "split={split}"
            );
            assert_eq!(parser.next(), None, "split={split}");
        }
    }

    #[test]
    fn fragmented_cursor_response_is_preserved_at_every_byte_boundary() {
        let now = Instant::now();
        let sequence = b"\x1b[20;10R";
        for split in 1..sequence.len() {
            let mut parser = Parser::default();
            parser.advance_at(&sequence[..split], false, now).unwrap();
            assert_eq!(parser.next(), None, "split={split}");
            parser.advance_at(&sequence[split..], false, now).unwrap();
            assert_eq!(
                parser.next(),
                Some(InternalEvent::CursorPosition(9, 19)),
                "split={split}"
            );
            assert_eq!(parser.next(), None, "split={split}");
        }
    }

    #[test]
    fn lone_escape_is_emitted_after_its_short_ambiguity_window() {
        let start = Instant::now();
        let mut parser = Parser::default();
        parser.advance_at(b"\x1b", false, start).unwrap();
        assert_eq!(parser.next(), None);
        parser.expire_stale_at(start + ESC_PENDING_IDLE - Duration::from_millis(1));
        assert_eq!(parser.next(), None);
        parser.expire_stale_at(start + ESC_PENDING_IDLE);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
    }

    #[test]
    fn escape_followed_by_a_focus_sequence_preserves_both_events() {
        let start = Instant::now();
        let mut parser = Parser::default();
        parser.advance_at(b"\x1b", false, start).unwrap();
        parser
            .advance_at(b"\x1b[I", false, start + Duration::from_millis(10))
            .unwrap();

        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::FocusGained))
        );
        assert_eq!(parser.next(), None);
    }

    #[test]
    fn two_fast_escape_keys_preserve_the_second_ambiguity() {
        let start = Instant::now();
        let second_at = start + Duration::from_millis(10);
        let mut parser = Parser::default();
        parser.advance_at(b"\x1b", false, start).unwrap();
        parser.advance_at(b"\x1b", false, second_at).unwrap();

        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
        assert_eq!(parser.next(), None);
        parser.expire_stale_at(second_at + ESC_PENDING_IDLE);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
        assert_eq!(parser.next(), None);
    }

    #[test]
    fn generic_pending_input_expires_after_one_second() {
        let start = Instant::now();
        let mut parser = Parser::default();
        parser.advance_at(&[0xe2], false, start).unwrap();
        parser.expire_stale_at(start + GENERIC_PENDING_IDLE - Duration::from_millis(1));
        assert!(parser.pending_input_is_recent());
        parser.expire_stale_at(start + GENERIC_PENDING_IDLE);
        assert!(parser.buffer.is_empty());
    }

    #[cfg(feature = "bracketed-paste")]
    #[test]
    fn abandoned_paste_is_emitted_after_three_idle_seconds() {
        let start = Instant::now();
        let mut parser = Parser::default();
        parser.advance_at(b"\x1b[200~hello", false, start).unwrap();
        parser.expire_stale_at(start + PASTE_PENDING_IDLE - Duration::from_millis(1));
        assert_eq!(parser.next(), None);
        parser.expire_stale_at(start + PASTE_PENDING_IDLE);
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Paste("hello".to_owned())))
        );
    }

    #[cfg(feature = "bracketed-paste")]
    #[test]
    fn actively_arriving_paste_has_no_total_duration_limit() {
        let start = Instant::now();
        let mut parser = Parser::default();
        parser.advance_at(PASTE_START, false, start).unwrap();
        let mut now = start;
        for _ in 0..4 {
            now += Duration::from_secs(2);
            parser.expire_stale_at(now);
            assert_eq!(parser.next(), None);
            now += Duration::from_millis(500);
            parser.advance_at(b"x", false, now).unwrap();
        }
        parser
            .advance_at(b"\x1b[201~", false, now + Duration::from_millis(100))
            .unwrap();
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Paste("xxxx".to_owned())))
        );
    }

    #[cfg(feature = "bracketed-paste")]
    #[test]
    fn paste_limit_is_bounded() {
        let mut parser = Parser::default();
        parser.buffer.extend_from_slice(PASTE_START);
        parser.buffer.resize(MAX_PASTE_BYTES + 12, b'x');
        parser.last_input_at = Some(Instant::now());
        let error = parser.advance(b"x", false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(parser.buffer.is_empty());
    }
}
