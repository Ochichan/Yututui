#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SeekObservation {
    CommandReply { request_id: u64, accepted: bool },
    Seeking(bool),
    PlaybackRestart,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SeekPurpose {
    CompletionBarrier,
    Interactive { target_secs: f64 },
}

impl SeekPurpose {
    fn for_command(command: &PlayerCmd) -> Option<Self> {
        match command {
            PlayerCmd::SeekRelative(_) => Some(Self::CompletionBarrier),
            PlayerCmd::SeekAbsolute {
                seconds,
                precision: super::SeekPrecision::InteractiveFast,
            } => Some(Self::Interactive {
                target_secs: *seconds,
            }),
            PlayerCmd::SeekAbsolute {
                precision: super::SeekPrecision::Exact,
                ..
            } => Some(Self::CompletionBarrier),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct SeekFlight {
    sequence: u64,
    request_id: u64,
    file_generation: u64,
    purpose: SeekPurpose,
    deadline: Instant,
    reply_accepted: bool,
    restart_seen: bool,
    seeking_started: bool,
    seeking_finished: bool,
    known_unseekable: bool,
    superseded: bool,
    dispatch_started_ms: Option<u64>,
    restart_recorded: bool,
    settle_deadline: Option<Instant>,
}

impl SeekFlight {
    fn new(
        sequence: u64,
        request_id: u64,
        file_generation: u64,
        purpose: SeekPurpose,
        known_unseekable: bool,
    ) -> Self {
        let dispatch_started_ms = matches!(purpose, SeekPurpose::Interactive { .. })
            .then(super::diagnostics::interactive_dispatched);
        Self {
            sequence,
            request_id,
            file_generation,
            purpose,
            deadline: Instant::now() + INTERACTIVE_SEEK_TIMEOUT,
            reply_accepted: false,
            restart_seen: false,
            seeking_started: false,
            seeking_finished: false,
            known_unseekable,
            superseded: false,
            dispatch_started_ms,
            restart_recorded: false,
            settle_deadline: None,
        }
    }

    /// The exact command reply and one same-generation lifecycle boundary jointly complete this
    /// sequence. mpv may emit the lifecycle event before its JSON command reply, so retain either
    /// ordering but never complete until the exact request is acknowledged. The actor permits only
    /// one seek flight at a time and generation changes reject load/resync lifecycle evidence.
    /// When `seeking` completes first, a short drain grace keeps that flight installed so its
    /// trailing restart cannot be credited to the next sequence.
    fn observe(&mut self, observation: SeekObservation, active_generation: Option<u64>) -> bool {
        match observation {
            SeekObservation::CommandReply {
                request_id,
                accepted,
            } if request_id == self.request_id => {
                if !accepted {
                    return true;
                }
                self.reply_accepted = true;
                if self.known_unseekable {
                    return true;
                }
            }
            SeekObservation::Seeking(seeking)
                if active_generation == Some(self.file_generation) =>
            {
                if seeking {
                    self.seeking_started = true;
                } else if self.seeking_started {
                    self.seeking_finished = true;
                }
            }
            SeekObservation::PlaybackRestart if active_generation == Some(self.file_generation) => {
                self.restart_seen = true;
            }
            _ => {}
        }
        if self.reply_accepted && self.restart_seen {
            self.record_restart();
            return true;
        }
        if self.reply_accepted && self.seeking_finished && self.settle_deadline.is_none() {
            self.settle_deadline = Some(Instant::now() + SEEK_LIFECYCLE_DRAIN_GRACE);
        }
        false
    }

    fn record_restart(&mut self) {
        if self.restart_recorded {
            return;
        }
        if let Some(started_ms) = self.dispatch_started_ms {
            super::diagnostics::seek_restart(started_ms, self.superseded);
        }
        self.restart_recorded = true;
    }

    fn wake_deadline(&self) -> Instant {
        self.settle_deadline
            .map_or(self.deadline, |settle| settle.min(self.deadline))
    }

    fn settled(&self, now: Instant) -> bool {
        self.reply_accepted
            && self.seeking_finished
            && self.settle_deadline.is_some_and(|deadline| now >= deadline)
    }

    fn mark_superseded(&mut self) {
        self.superseded = true;
    }

    fn interactive_target(&self) -> Option<f64> {
        match self.purpose {
            SeekPurpose::CompletionBarrier => None,
            SeekPurpose::Interactive { target_secs } => Some(target_secs),
        }
    }

    fn cache_success_proven(&self) -> bool {
        self.reply_accepted
            && !self.known_unseekable
            && (self.restart_seen || self.seeking_finished)
    }
}

fn mark_seek_superseded(flight: &mut Option<SeekFlight>, command: &PlayerCmd) {
    if SeekPurpose::for_command(command).is_some()
        && let Some(flight) = flight.as_mut()
    {
        flight.mark_superseded();
    }
}

fn observe_seek_flight(
    flight: &mut Option<SeekFlight>,
    observation: SeekObservation,
    active_generation: Option<u64>,
) -> Option<SeekFlight> {
    flight
        .as_mut()
        .is_some_and(|flight| flight.observe(observation, active_generation))
        .then(|| {
            flight
                .take()
                .expect("completed seek flight remains installed")
        })
}

fn take_settled_seek_flight(flight: &mut Option<SeekFlight>, now: Instant) -> Option<SeekFlight> {
    flight
        .as_ref()
        .is_some_and(|flight| flight.settled(now))
        .then(|| {
            flight
                .take()
                .expect("settled seek flight remains installed")
        })
}

fn finish_seek_flight(emit: &EventSink, state: &mut DispatchState, completed: SeekFlight) {
    if completed.reply_accepted {
        complete_resume_telemetry(emit, state, completed.file_generation, completed.request_id);
    } else if let Some(purpose) = state
        .resume
        .take_telemetry_for_generation(completed.file_generation)
    {
        state
            .resume
            .clear_dispatch_for_generation(completed.file_generation);
        if purpose.is_source_recovery() {
            super::diagnostics::source_recovery_outcome(
                super::diagnostics::SourceRecoveryOutcome::ResumeRejected,
            );
        }
    }
    if let Some(target_secs) = completed.interactive_target()
        && completed.cache_success_proven()
    {
        let action = state
            .cache
            .as_mut()
            .and_then(|cache| cache.interactive_seek_succeeded(target_secs));
        queue_cache_action(state, action);
    }
    tracing::trace!(
        sequence = completed.sequence,
        request_id = completed.request_id,
        "seek completion flight released"
    );
}
const SEEK_LIFECYCLE_DRAIN_GRACE: Duration = Duration::from_millis(20);
const INTERACTIVE_SEEK_TRAILING_DEBOUNCE: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
struct InteractiveBurstGate {
    quiet_deadline: Option<Instant>,
}

impl InteractiveBurstGate {
    fn observe_command(&mut self, command: &PlayerCmd, now: Instant) {
        if command.is_coalescing_barrier() {
            self.quiet_deadline = None;
            return;
        }
        if !command.is_interactive_seek() {
            return;
        }
        match self.quiet_deadline {
            Some(deadline) if now < deadline => {
                self.quiet_deadline = Some(now + INTERACTIVE_SEEK_TRAILING_DEBOUNCE);
            }
            Some(_) => self.quiet_deadline = None,
            None => {}
        }
    }

    fn command_ready(&self, command: &PlayerCmd, now: Instant) -> bool {
        !command.is_interactive_seek() || self.quiet_deadline.is_none_or(|deadline| now >= deadline)
    }

    fn dispatched(&mut self, command: &PlayerCmd, now: Instant) {
        if command.is_interactive_seek() {
            self.quiet_deadline = Some(now + INTERACTIVE_SEEK_TRAILING_DEBOUNCE);
        }
    }

    fn wake_deadline(&self) -> Option<Instant> {
        self.quiet_deadline
    }
}
