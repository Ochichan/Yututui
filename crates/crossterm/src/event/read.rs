use std::{
    collections::vec_deque::VecDeque,
    io::{self, Write},
    time::Duration,
};

#[cfg(unix)]
use crate::event::source::unix::UnixInternalEventSource;
#[cfg(windows)]
use crate::event::source::windows::WindowsEventSource;
#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{filter::Filter, source::EventSource, timeout::PollTimeout, InternalEvent};

enum StoredInitError {
    Os(i32),
    Message(io::ErrorKind, String),
}

struct FailedEventSource(StoredInitError);

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CursorPositionQuery {
    Position(u16, u16),
    DeferredForPendingInput,
    DeferredForRecentInput,
}

impl FailedEventSource {
    fn new(error: io::Error) -> Self {
        match error.raw_os_error() {
            Some(errno) => Self(StoredInitError::Os(errno)),
            None => Self(StoredInitError::Message(error.kind(), error.to_string())),
        }
    }

    fn error(&self) -> io::Error {
        match &self.0 {
            StoredInitError::Os(errno) => io::Error::from_raw_os_error(*errno),
            StoredInitError::Message(kind, message) => io::Error::new(*kind, message.clone()),
        }
    }
}

impl EventSource for FailedEventSource {
    fn try_read(&mut self, _timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        Err(self.error())
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        panic!("an event reader that failed to initialize has no waker")
    }
}

/// Can be used to read `InternalEvent`s.
pub(crate) struct InternalEventReader {
    events: VecDeque<InternalEvent>,
    source: Option<Box<dyn EventSource>>,
    skipped_events: Vec<InternalEvent>,
}

impl Default for InternalEventReader {
    fn default() -> Self {
        #[cfg(windows)]
        let source = WindowsEventSource::new();
        #[cfg(unix)]
        let source = UnixInternalEventSource::new();

        // yututui patch: do not erase the kind/raw errno from event-source initialization.
        let source = Some(match source {
            Ok(source) => Box::new(source) as Box<dyn EventSource>,
            Err(error) => Box::new(FailedEventSource::new(error)) as Box<dyn EventSource>,
        });

        InternalEventReader {
            source,
            events: VecDeque::with_capacity(32),
            skipped_events: Vec::with_capacity(32),
        }
    }
}

impl InternalEventReader {
    #[cfg(unix)]
    pub(crate) fn probe_cursor_position<W: Write>(
        &mut self,
        writer: &mut W,
        timeout: Duration,
    ) -> io::Result<CursorPositionQuery> {
        use crate::event::filter::CursorPositionFilter;

        let source = self.source.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "Failed to initialize input reader")
        })?;
        let deadline = PollTimeout::new(Some(timeout));

        // Pump bytes already readable before writing a new DSR and discard queued replies from
        // older queries. Non-cursor events stay ordered in the public event queue. DSR carries no
        // request identifier, so the first reply parsed after the successful write is the only
        // protocol-level tie-breaker available for a reply racing this purge.
        let mut recent_input = self
            .events
            .iter()
            .chain(self.skipped_events.iter())
            .any(|event| !matches!(event, InternalEvent::CursorPosition(_, _)));
        loop {
            if deadline.elapsed() {
                if recent_input {
                    break;
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "pre-query terminal input pump exceeded the cursor-position deadline",
                ));
            }
            match source.try_read(Some(Duration::ZERO)) {
                Ok(Some(event)) => {
                    recent_input |= !matches!(event, InternalEvent::CursorPosition(_, _));
                    self.events.push_back(event);
                }
                Ok(None) => break,
                Err(error) => return Err(error),
            }
        }
        self.events
            .retain(|event| !matches!(event, InternalEvent::CursorPosition(_, _)));
        self.skipped_events
            .retain(|event| !matches!(event, InternalEvent::CursorPosition(_, _)));

        if recent_input {
            return Ok(CursorPositionQuery::DeferredForRecentInput);
        }
        if source.pending_input_is_recent() {
            return Ok(CursorPositionQuery::DeferredForPendingInput);
        }

        writer.write_all(b"\x1b[6n")?;
        writer.flush()?;
        if deadline.elapsed() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "cursor-position query write exceeded its deadline",
            ));
        }

        if !self.poll(deadline.leftover(), &CursorPositionFilter)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "cursor position was not received before the query deadline",
            ));
        }
        match self.read(&CursorPositionFilter)? {
            InternalEvent::CursorPosition(column, row) => {
                Ok(CursorPositionQuery::Position(column, row))
            }
            _ => unreachable!("cursor-position filter admitted another event kind"),
        }
    }

    /// Returns a `Waker` allowing to wake/force the `poll` method to return `Ok(false)`.
    #[cfg(feature = "event-stream")]
    pub(crate) fn waker(&self) -> Waker {
        self.source.as_ref().expect("reader source not set").waker()
    }

    pub(crate) fn poll<F>(&mut self, timeout: Option<Duration>, filter: &F) -> io::Result<bool>
    where
        F: Filter,
    {
        for event in &self.events {
            if filter.eval(event) {
                return Ok(true);
            }
        }

        let event_source = match self.source.as_mut() {
            Some(source) => source,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Failed to initialize input reader",
                ))
            }
        };

        let poll_timeout = PollTimeout::new(timeout);

        loop {
            let maybe_event = match event_source.try_read(poll_timeout.leftover()) {
                Ok(None) => None,
                Ok(Some(event)) => {
                    if filter.eval(&event) {
                        Some(event)
                    } else {
                        self.skipped_events.push(event);
                        None
                    }
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        return Ok(false);
                    }

                    return Err(e);
                }
            };

            if poll_timeout.elapsed() || maybe_event.is_some() {
                self.events.extend(self.skipped_events.drain(..));

                if let Some(event) = maybe_event {
                    self.events.push_front(event);
                    return Ok(true);
                }

                return Ok(false);
            }
        }
    }

    pub(crate) fn read<F>(&mut self, filter: &F) -> io::Result<InternalEvent>
    where
        F: Filter,
    {
        let mut skipped_events = VecDeque::new();

        loop {
            while let Some(event) = self.events.pop_front() {
                if filter.eval(&event) {
                    while let Some(event) = skipped_events.pop_front() {
                        self.events.push_back(event);
                    }

                    return Ok(event);
                } else {
                    // We can not directly write events back to `self.events`.
                    // If we did, we would put our self's into an endless loop
                    // that would enqueue -> dequeue -> enqueue etc.
                    // This happens because `poll` in this function will always return true if there are events in it's.
                    // And because we just put the non-fulfilling event there this is going to be the case.
                    // Instead we can store them into the temporary buffer,
                    // and then when the filter is fulfilled write all events back in order.
                    skipped_events.push_back(event);
                }
            }

            let _ = self.poll(None, filter)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::{collections::VecDeque, time::Duration};

    #[cfg(unix)]
    use super::super::filter::CursorPositionFilter;
    #[cfg(unix)]
    use super::CursorPositionQuery;
    use super::{
        super::Event, EventSource, FailedEventSource, Filter, InternalEvent, InternalEventReader,
    };

    #[derive(Debug, Clone)]
    pub(crate) struct InternalEventFilter;

    impl Filter for InternalEventFilter {
        fn eval(&self, _: &InternalEvent) -> bool {
            true
        }
    }

    #[test]
    fn test_poll_fails_without_event_source() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(10)), &InternalEventFilter)
            .is_err());
    }

    #[test]
    fn test_poll_returns_true_for_matching_event_in_queue_at_front() {
        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10))].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn test_poll_returns_true_for_matching_event_in_queue_at_back() {
        let mut reader = InternalEventReader {
            events: vec![
                InternalEvent::Event(Event::Resize(10, 10)),
                InternalEvent::CursorPosition(10, 20),
            ]
            .into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &CursorPositionFilter).unwrap());
    }

    #[test]
    fn test_read_returns_matching_event_in_queue_at_front() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let mut reader = InternalEventReader {
            events: vec![EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    #[cfg(unix)]
    fn test_read_returns_matching_event_in_queue_at_back() {
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![InternalEvent::Event(Event::Resize(10, 10)), CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
    }

    #[test]
    #[cfg(unix)]
    fn test_read_does_not_consume_skipped_event() {
        const SKIPPED_EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));
        const CURSOR_EVENT: InternalEvent = InternalEvent::CursorPosition(10, 20);

        let mut reader = InternalEventReader {
            events: vec![SKIPPED_EVENT, CURSOR_EVENT].into(),
            source: None,
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&CursorPositionFilter).unwrap(), CURSOR_EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), SKIPPED_EVENT);
    }

    #[test]
    fn test_poll_timeouts_if_source_has_no_events() {
        let source = FakeSource::default();

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert!(!reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_poll_returns_true_if_source_has_at_least_one_event() {
        let source = FakeSource::with_events(&[InternalEvent::Event(Event::Resize(10, 10))]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert!(reader.poll(None, &InternalEventFilter).unwrap());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_reads_returns_event_if_source_has_at_least_one_event() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_read_returns_events_if_source_has_events() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn test_poll_returns_false_after_all_source_events_are_consumed() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::with_events(&[EVENT, EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(!reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_poll_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
                .err()
                // yututui patch: keep the vendored crate warning-free on the repository toolchain.
                .map(|e| format!("{:?}", e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_read_propagates_error() {
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(FakeSource::new(&[]))),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(
            reader
                .read(&InternalEventFilter)
                .err()
                // yututui patch: keep the vendored crate warning-free on the repository toolchain.
                .map(|e| format!("{:?}", e.kind())),
            Some(format!("{:?}", io::ErrorKind::Other))
        );
    }

    #[test]
    fn test_poll_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert!(reader
            .poll(Some(Duration::from_secs(0)), &InternalEventFilter)
            .unwrap());
    }

    #[test]
    fn test_read_continues_after_error() {
        const EVENT: InternalEvent = InternalEvent::Event(Event::Resize(10, 10));

        let source = FakeSource::new(&[EVENT, EVENT]);

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::with_capacity(32),
        };

        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
        assert!(reader.read(&InternalEventFilter).is_err());
        assert_eq!(reader.read(&InternalEventFilter).unwrap(), EVENT);
    }

    #[test]
    fn failed_source_preserves_the_initialization_errno() {
        let mut source = FailedEventSource::new(io::Error::from_raw_os_error(5));
        let error = source.try_read(Some(Duration::ZERO)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
    }

    #[test]
    #[cfg(unix)]
    fn cursor_probe_defers_without_writing_when_input_is_pending() {
        let source = FakeSource {
            pending_input: true,
            ..FakeSource::default()
        };
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::new(),
        };
        let mut writer = Vec::new();

        assert_eq!(
            reader
                .probe_cursor_position(&mut writer, Duration::from_secs(1))
                .unwrap(),
            CursorPositionQuery::DeferredForPendingInput
        );
        assert!(writer.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn cursor_probe_discards_a_stale_queued_response() {
        let source = FakeSource {
            pending_input: true,
            ..FakeSource::default()
        };
        let mut reader = InternalEventReader {
            events: vec![InternalEvent::CursorPosition(1, 2)].into(),
            source: Some(Box::new(source)),
            skipped_events: vec![InternalEvent::CursorPosition(3, 4)],
        };
        let mut writer = Vec::new();

        assert_eq!(
            reader
                .probe_cursor_position(&mut writer, Duration::from_secs(1))
                .unwrap(),
            CursorPositionQuery::DeferredForPendingInput
        );
        assert!(reader.events.is_empty());
        assert!(reader.skipped_events.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn cursor_probe_preserves_complete_input_and_defers_without_writing() {
        let key = InternalEvent::Event(Event::Key(crate::event::KeyCode::Char('x').into()));
        let source = FakeSource {
            events: vec![key.clone()].into(),
            ..FakeSource::default()
        };
        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(source)),
            skipped_events: Vec::new(),
        };
        let mut writer = Vec::new();

        assert_eq!(
            reader
                .probe_cursor_position(&mut writer, Duration::from_secs(1))
                .unwrap(),
            CursorPositionQuery::DeferredForRecentInput
        );
        assert!(writer.is_empty());
        assert_eq!(reader.events.pop_front(), Some(key));
    }

    #[test]
    #[cfg(unix)]
    fn cursor_probe_deadline_with_recent_input_defers_instead_of_timing_out() {
        struct DeadlineCrossingSource;

        impl EventSource for DeadlineCrossingSource {
            fn try_read(
                &mut self,
                _timeout: Option<Duration>,
            ) -> io::Result<Option<InternalEvent>> {
                std::thread::sleep(Duration::from_millis(2));
                Ok(Some(InternalEvent::Event(Event::Key(
                    crate::event::KeyCode::Char('x').into(),
                ))))
            }

            #[cfg(feature = "event-stream")]
            fn waker(&self) -> super::super::sys::Waker {
                unimplemented!()
            }
        }

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(DeadlineCrossingSource)),
            skipped_events: Vec::new(),
        };
        let mut writer = Vec::new();

        assert_eq!(
            reader
                .probe_cursor_position(&mut writer, Duration::from_millis(1))
                .unwrap(),
            CursorPositionQuery::DeferredForRecentInput
        );
        assert!(writer.is_empty());
        assert!(matches!(
            reader.events.front(),
            Some(InternalEvent::Event(Event::Key(_)))
        ));
    }

    #[test]
    #[cfg(unix)]
    fn cursor_probe_purges_pre_query_reply_and_accepts_the_first_post_query_reply() {
        struct QuerySource {
            pre_query_reply_sent: bool,
            post_query_reply_sent: bool,
        }

        impl EventSource for QuerySource {
            fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
                if timeout == Some(Duration::ZERO) {
                    if !self.pre_query_reply_sent {
                        self.pre_query_reply_sent = true;
                        return Ok(Some(InternalEvent::CursorPosition(1, 2)));
                    }
                    return Ok(None);
                }
                if !self.post_query_reply_sent {
                    self.post_query_reply_sent = true;
                    return Ok(Some(InternalEvent::CursorPosition(30, 40)));
                }
                Ok(None)
            }

            #[cfg(feature = "event-stream")]
            fn waker(&self) -> super::super::sys::Waker {
                unimplemented!()
            }
        }

        let mut reader = InternalEventReader {
            events: VecDeque::new(),
            source: Some(Box::new(QuerySource {
                pre_query_reply_sent: false,
                post_query_reply_sent: false,
            })),
            skipped_events: Vec::new(),
        };
        let mut writer = Vec::new();

        assert_eq!(
            reader
                .probe_cursor_position(&mut writer, Duration::from_secs(1))
                .unwrap(),
            CursorPositionQuery::Position(30, 40)
        );
        assert!(!writer.is_empty());
    }

    #[derive(Default)]
    struct FakeSource {
        events: VecDeque<InternalEvent>,
        error: Option<io::Error>,
        pending_input: bool,
    }

    impl FakeSource {
        fn new(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: Some(io::Error::new(io::ErrorKind::Other, "")),
                pending_input: false,
            }
        }

        fn with_events(events: &[InternalEvent]) -> FakeSource {
            FakeSource {
                events: events.to_vec().into(),
                error: None,
                pending_input: false,
            }
        }
    }

    impl EventSource for FakeSource {
        fn try_read(&mut self, _timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
            // Return error if set in case there's just one remaining event
            if self.events.len() == 1 {
                if let Some(error) = self.error.take() {
                    return Err(error);
                }
            }

            // Return all events from the queue
            if let Some(event) = self.events.pop_front() {
                return Ok(Some(event));
            }

            // Return error if there're no more events
            if let Some(error) = self.error.take() {
                return Err(error);
            }

            // Timeout
            Ok(None)
        }

        #[cfg(unix)]
        fn pending_input_is_recent(&mut self) -> bool {
            self.pending_input
        }

        #[cfg(feature = "event-stream")]
        fn waker(&self) -> super::super::sys::Waker {
            unimplemented!();
        }
    }
}
