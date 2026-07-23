use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalFailureClass {
    DefinitiveLoss,
    AmbiguousExhausted,
    WorkerStall,
    InternalFatal,
}

impl fmt::Display for TerminalFailureClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DefinitiveLoss => "definitive_loss",
            Self::AmbiguousExhausted => "ambiguous_exhausted",
            Self::WorkerStall => "worker_stall",
            Self::InternalFatal => "internal_fatal",
        })
    }
}

// Only `Transport` has a non-Unix producer; the owner-probe domains are constructed by the
// multiplexer probes, which are Unix-only.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EvidenceDomain {
    Transport,
    OwnerEnvironment,
    OwnerTmux,
    OwnerScreen,
    OwnerZellij,
}

impl EvidenceDomain {
    const COUNT: usize = 5;

    const fn index(self) -> usize {
        match self {
            Self::Transport => 0,
            Self::OwnerEnvironment => 1,
            Self::OwnerTmux => 2,
            Self::OwnerScreen => 3,
            Self::OwnerZellij => 4,
        }
    }
}

impl fmt::Display for EvidenceDomain {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Transport => "transport",
            Self::OwnerEnvironment => "owner_environment",
            Self::OwnerTmux => "owner_tmux",
            Self::OwnerScreen => "owner_screen",
            Self::OwnerZellij => "owner_zellij",
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum LivenessStage {
    #[default]
    Idle = 0,
    InputPoll = 1,
    InputRead = 2,
    EventDelivery = 3,
    OwnerProbe = 4,
    OutputGate = 5,
    CprWrite = 6,
    CprWait = 7,
    OwnerRender = 8,
    OwnerFlush = 9,
    Restore = 10,
    Stopping = 11,
}

impl LivenessStage {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::InputPoll,
            2 => Self::InputRead,
            3 => Self::EventDelivery,
            4 => Self::OwnerProbe,
            5 => Self::OutputGate,
            6 => Self::CprWrite,
            7 => Self::CprWait,
            8 => Self::OwnerRender,
            9 => Self::OwnerFlush,
            10 => Self::Restore,
            11 => Self::Stopping,
            _ => Self::Idle,
        }
    }
}

impl fmt::Display for LivenessStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Idle => "idle",
            Self::InputPoll => "input_poll",
            Self::InputRead => "input_read",
            Self::EventDelivery => "event_delivery",
            Self::OwnerProbe => "owner_probe",
            Self::OutputGate => "output_gate",
            Self::CprWrite => "cpr_write",
            Self::CprWait => "cpr_wait",
            Self::OwnerRender => "owner_render",
            Self::OwnerFlush => "owner_flush",
            Self::Restore => "restore",
            Self::Stopping => "stopping",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TerminalFailure {
    pub(super) kind: io::ErrorKind,
    pub(super) class: TerminalFailureClass,
    pub(super) stage: LivenessStage,
    pub(super) domain: Option<EvidenceDomain>,
    pub(super) elapsed_ms: u64,
    pub(super) last_alive_ms: u64,
    pub(super) message: String,
}

impl TerminalFailure {
    pub(super) fn runtime(
        kind: io::ErrorKind,
        class: TerminalFailureClass,
        stage: LivenessStage,
        domain: Option<EvidenceDomain>,
        elapsed: Duration,
        detail: impl fmt::Display,
    ) -> Self {
        let elapsed_ms = duration_ms(elapsed);
        let domain_field = domain
            .map(|value| format!(", domain={value}"))
            .unwrap_or_default();
        Self {
            kind,
            class,
            stage,
            domain,
            elapsed_ms,
            last_alive_ms: 0,
            message: format!(
                "terminal client liveness was lost [class={class}, stage={stage}{domain_field}, elapsed_ms={elapsed_ms}]: {detail}; inspect `ytt doctor terminal --json`"
            ),
        }
    }

    pub(super) fn startup(
        kind: io::ErrorKind,
        class: TerminalFailureClass,
        stage: LivenessStage,
        domain: Option<EvidenceDomain>,
        elapsed: Duration,
        detail: impl fmt::Display,
    ) -> Self {
        let mut value = Self::runtime(kind, class, stage, domain, elapsed, detail);
        value.message = value.message.replacen(
            "terminal client liveness was lost",
            "terminal client liveness could not be established",
            1,
        );
        value
    }

    pub(super) fn into_error(self) -> io::Error {
        io::Error::new(self.kind, self.message)
    }

    pub(super) fn with_last_alive(mut self, elapsed: Duration) -> Self {
        self.last_alive_ms = duration_ms(elapsed);
        self
    }
}

pub(super) struct FailureStore {
    primary: OnceLock<TerminalFailure>,
    secondary: Mutex<Vec<TerminalFailure>>,
}

impl Default for FailureStore {
    fn default() -> Self {
        Self {
            primary: OnceLock::new(),
            secondary: Mutex::new(Vec::new()),
        }
    }
}

impl FailureStore {
    pub(super) fn record_primary(&self, failure: TerminalFailure) -> bool {
        self.primary.set(failure).is_ok()
    }

    pub(super) fn record_secondary(&self, failure: TerminalFailure) -> bool {
        let mut secondary = match self.secondary.try_lock() {
            Ok(secondary) => secondary,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return false,
        };
        secondary.push(failure);
        true
    }

    pub(super) fn primary_class(&self) -> Option<TerminalFailureClass> {
        self.primary.get().map(|failure| failure.class)
    }

    pub(super) fn has_primary(&self) -> bool {
        self.primary.get().is_some()
    }

    /// Produce a non-consuming snapshot. Teardown may add a join diagnostic after the owner first
    /// observes the primary liveness failure, so callers are deliberately allowed to read again.
    /// Failure reporting must never wait behind a wedged secondary diagnostic producer.
    pub(super) fn error_snapshot(&self) -> Option<io::Error> {
        let primary = self.primary.get()?.clone();
        let secondary = match self.secondary.try_lock() {
            Ok(secondary) => secondary,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Some(primary.into_error()),
        };
        if secondary.is_empty() {
            return Some(primary.into_error());
        }
        let details = secondary
            .iter()
            .map(|failure| failure.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        Some(io::Error::new(
            primary.kind,
            format!(
                "{}; secondary terminal shutdown failures: {details}",
                primary.message
            ),
        ))
    }
}

#[derive(Debug)]
pub(super) enum ProbeOutcome {
    Alive(EvidenceDomain),
    PendingInput,
    RecentInput,
    OwnerOutputBusy,
    NoEvidence,
    DefinitiveLoss {
        domain: EvidenceDomain,
        stage: LivenessStage,
        error: io::Error,
    },
    Ambiguous {
        domain: EvidenceDomain,
        stage: LivenessStage,
        evidence_id: u64,
        error: io::Error,
    },
    InternalFatal {
        domain: Option<EvidenceDomain>,
        stage: LivenessStage,
        error: io::Error,
    },
}

impl ProbeOutcome {
    pub(super) fn transport(error: io::Error, evidence_id: u64, stage: LivenessStage) -> Self {
        if is_definitive_terminal_loss(&error) {
            Self::DefinitiveLoss {
                domain: EvidenceDomain::Transport,
                stage,
                error,
            }
        } else if !matches!(
            error.kind(),
            io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput
        ) {
            Self::Ambiguous {
                domain: EvidenceDomain::Transport,
                stage,
                evidence_id,
                error,
            }
        } else {
            Self::InternalFatal {
                domain: Some(EvidenceDomain::Transport),
                stage,
                error,
            }
        }
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub(super) fn owner(domain: EvidenceDomain, error: io::Error, evidence_id: u64) -> Self {
        if is_definitive_terminal_loss(&error) {
            Self::DefinitiveLoss {
                domain,
                stage: LivenessStage::OwnerProbe,
                error,
            }
        } else {
            // A missing/old CLI, command timeout, malformed response, or incomplete inherited
            // environment is evidence about the probe, not proof that the PTY client vanished.
            Self::Ambiguous {
                domain,
                stage: LivenessStage::OwnerProbe,
                evidence_id,
                error,
            }
        }
    }
}

pub(super) fn is_definitive_terminal_loss(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
    ) {
        return true;
    }
    #[cfg(unix)]
    if matches!(error.raw_os_error(), Some(libc::EIO | libc::ENXIO)) {
        return true;
    }
    false
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Suspicion {
    evidence_id: u64,
    confirmations: u8,
    started_at: Instant,
}

pub(super) struct SuspicionTracker {
    domains: [Option<Suspicion>; EvidenceDomain::COUNT],
}

impl Default for SuspicionTracker {
    fn default() -> Self {
        Self {
            domains: [None; EvidenceDomain::COUNT],
        }
    }
}

pub(super) struct AmbiguityObservation {
    pub(super) confirmations: u8,
    pub(super) elapsed: Duration,
}

impl SuspicionTracker {
    pub(super) fn observe(
        &mut self,
        domain: EvidenceDomain,
        evidence_id: u64,
        now: Instant,
    ) -> AmbiguityObservation {
        let slot = &mut self.domains[domain.index()];
        let suspicion = slot.get_or_insert(Suspicion {
            evidence_id,
            confirmations: 1,
            started_at: now,
        });
        if suspicion.evidence_id != evidence_id {
            suspicion.evidence_id = evidence_id;
            suspicion.confirmations = suspicion.confirmations.saturating_add(1);
        }
        AmbiguityObservation {
            confirmations: suspicion.confirmations,
            elapsed: now.saturating_duration_since(suspicion.started_at),
        }
    }

    pub(super) fn clear(&mut self, domain: EvidenceDomain) {
        self.domains[domain.index()] = None;
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct WatchdogSnapshot {
    pub(super) stage: LivenessStage,
    pub(super) stage_elapsed: Duration,
    pub(super) suspect_elapsed: Option<Duration>,
    pub(super) suspect_stage: LivenessStage,
    pub(super) last_alive_elapsed: Duration,
}

pub(super) struct WatchdogProgress {
    origin: Instant,
    writer: Mutex<()>,
    sequence: AtomicU64,
    stage: AtomicU8,
    stage_started_ms: AtomicU64,
    last_alive_ms: AtomicU64,
    transport_suspect_ms: AtomicU64,
    transport_suspect_stage: AtomicU8,
    armed: AtomicBool,
    force_probe: AtomicBool,
}

impl Default for WatchdogProgress {
    fn default() -> Self {
        let origin = Instant::now();
        Self {
            origin,
            writer: Mutex::new(()),
            sequence: AtomicU64::new(0),
            stage: AtomicU8::new(LivenessStage::Idle as u8),
            stage_started_ms: AtomicU64::new(1),
            last_alive_ms: AtomicU64::new(1),
            transport_suspect_ms: AtomicU64::new(0),
            transport_suspect_stage: AtomicU8::new(LivenessStage::Idle as u8),
            armed: AtomicBool::new(false),
            force_probe: AtomicBool::new(false),
        }
    }
}

impl WatchdogProgress {
    pub(super) fn enter(&self, stage: LivenessStage) {
        self.write_snapshot(stage, self.stamp(Instant::now()));
    }

    pub(super) fn idle(&self) {
        self.write_snapshot(LivenessStage::Idle, self.stamp(Instant::now()));
    }

    pub(super) fn alive(&self) {
        let stamp = self.stamp(Instant::now());
        self.publish(|| {
            self.last_alive_ms.store(stamp, Ordering::Release);
            self.transport_suspect_ms.store(0, Ordering::Release);
            self.transport_suspect_stage
                .store(LivenessStage::Idle as u8, Ordering::Release);
            self.store_stage(LivenessStage::Idle, stamp);
        });
    }

    pub(super) fn transport_suspect(&self, stage: LivenessStage) {
        let stamp = self.stamp(Instant::now());
        self.publish(|| {
            if self.transport_suspect_ms.load(Ordering::Acquire) == 0 {
                self.transport_suspect_ms.store(stamp, Ordering::Release);
            }
            self.transport_suspect_stage
                .store(stage as u8, Ordering::Release);
            self.store_stage(LivenessStage::Idle, stamp);
        });
    }

    pub(super) fn arm(&self) {
        self.armed.store(true, Ordering::Release);
    }

    pub(super) fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }

    pub(super) fn request_probe(&self) {
        self.force_probe.store(true, Ordering::Release);
    }

    pub(super) fn take_probe_request(&self) -> bool {
        self.force_probe.swap(false, Ordering::AcqRel)
    }

    pub(super) fn snapshot(&self, now: Instant) -> WatchdogSnapshot {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let stage = LivenessStage::from_u8(self.stage.load(Ordering::Acquire));
            let stage_started = self.stage_started_ms.load(Ordering::Acquire);
            let last_alive = self.last_alive_ms.load(Ordering::Acquire);
            let suspect = self.transport_suspect_ms.load(Ordering::Acquire);
            let suspect_stage =
                LivenessStage::from_u8(self.transport_suspect_stage.load(Ordering::Acquire));
            let after = self.sequence.load(Ordering::Acquire);
            if before == after {
                let now_ms = self.elapsed_ms(now);
                return WatchdogSnapshot {
                    stage,
                    stage_elapsed: millis_since(now_ms, stage_started),
                    suspect_elapsed: (suspect != 0).then(|| millis_since(now_ms, suspect)),
                    suspect_stage,
                    last_alive_elapsed: millis_since(now_ms, last_alive),
                };
            }
        }
    }

    fn write_snapshot(&self, stage: LivenessStage, started_ms: u64) {
        self.publish(|| self.store_stage(stage, started_ms));
    }

    fn store_stage(&self, stage: LivenessStage, started_ms: u64) {
        self.stage.store(stage as u8, Ordering::Release);
        self.stage_started_ms.store(started_ms, Ordering::Release);
    }

    fn publish(&self, operation: impl FnOnce()) {
        let _writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.sequence.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(self.sequence.load(Ordering::Relaxed) & 1, 1);
        operation();
        self.sequence.fetch_add(1, Ordering::Release);
    }

    fn stamp(&self, now: Instant) -> u64 {
        self.elapsed_ms(now).saturating_add(1)
    }

    fn elapsed_ms(&self, now: Instant) -> u64 {
        duration_ms(now.saturating_duration_since(self.origin))
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn millis_since(now_ms: u64, stamped_ms: u64) -> Duration {
    Duration::from_millis(now_ms.saturating_add(1).saturating_sub(stamped_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_gap_probe_request_does_not_extend_hard_deadlines() {
        let now = Instant::now();
        let progress = WatchdogProgress {
            origin: now - Duration::from_secs(20),
            ..WatchdogProgress::default()
        };
        let now_stamp = progress.stamp(now);
        let stalled_at = now_stamp.saturating_sub(8_900);
        progress.stage.store(
            LivenessStage::CprWait as u8,
            std::sync::atomic::Ordering::Release,
        );
        progress
            .stage_started_ms
            .store(stalled_at, std::sync::atomic::Ordering::Release);
        progress
            .last_alive_ms
            .store(stalled_at, std::sync::atomic::Ordering::Release);
        progress
            .transport_suspect_ms
            .store(stalled_at, std::sync::atomic::Ordering::Release);
        progress.transport_suspect_stage.store(
            LivenessStage::CprWait as u8,
            std::sync::atomic::Ordering::Release,
        );

        let before = progress.snapshot(now);
        progress.request_probe();
        progress.request_probe();
        let snapshot = progress.snapshot(Instant::now());

        assert!(snapshot.stage_elapsed >= before.stage_elapsed);
        assert!(snapshot.suspect_elapsed.unwrap() >= before.suspect_elapsed.unwrap());
        assert!(snapshot.last_alive_elapsed >= before.last_alive_elapsed);
        assert!(progress.take_probe_request());
    }

    #[test]
    fn alive_publication_cannot_expose_new_liveness_with_a_stale_expired_stage() {
        let progress = std::sync::Arc::new(WatchdogProgress::default());
        progress.transport_suspect(LivenessStage::CprWait);
        progress.enter(LivenessStage::CprWait);

        let writer = progress.writer.lock().unwrap();
        progress.sequence.fetch_add(1, Ordering::AcqRel);
        let stamp = progress.stamp(Instant::now());
        progress.last_alive_ms.store(stamp, Ordering::Release);
        progress.transport_suspect_ms.store(0, Ordering::Release);

        let snapshot_progress = std::sync::Arc::clone(&progress);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (snapshot_tx, snapshot_rx) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let _ = started_tx.send(());
            let _ = snapshot_tx.send(snapshot_progress.snapshot(Instant::now()));
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            snapshot_rx.recv_timeout(Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        progress.store_stage(LivenessStage::Idle, stamp);
        progress.sequence.fetch_add(1, Ordering::Release);
        drop(writer);
        let snapshot = snapshot_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        reader.join().unwrap();
        assert_eq!(snapshot.stage, LivenessStage::Idle);
        assert!(snapshot.suspect_elapsed.is_none());
    }

    #[test]
    fn ambiguity_requires_two_distinct_evidence_ids_in_one_domain() {
        let start = Instant::now();
        let mut tracker = SuspicionTracker::default();
        assert_eq!(
            tracker
                .observe(EvidenceDomain::Transport, 7, start)
                .confirmations,
            1
        );
        assert_eq!(
            tracker
                .observe(EvidenceDomain::Transport, 7, start + Duration::from_secs(1))
                .confirmations,
            1,
            "the same continuously-held output permit is one observation"
        );
        assert_eq!(
            tracker
                .observe(EvidenceDomain::Transport, 8, start + Duration::from_secs(2))
                .confirmations,
            2
        );
    }

    #[test]
    fn suspicion_is_scoped_and_success_clears_only_its_domain() {
        let start = Instant::now();
        let mut tracker = SuspicionTracker::default();
        tracker.observe(EvidenceDomain::Transport, 1, start);
        tracker.observe(EvidenceDomain::OwnerTmux, 1, start);
        tracker.clear(EvidenceDomain::Transport);
        assert_eq!(
            tracker
                .observe(EvidenceDomain::Transport, 2, start)
                .confirmations,
            1
        );
        assert_eq!(
            tracker
                .observe(EvidenceDomain::OwnerTmux, 2, start)
                .confirmations,
            2
        );
    }

    #[test]
    fn definitive_transport_errors_are_not_ambiguous() {
        for kind in [
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::NotConnected,
        ] {
            assert!(matches!(
                ProbeOutcome::transport(
                    io::Error::new(kind, "closed"),
                    1,
                    LivenessStage::InputRead
                ),
                ProbeOutcome::DefinitiveLoss { .. }
            ));
        }
        assert!(matches!(
            ProbeOutcome::transport(
                io::Error::new(io::ErrorKind::TimedOut, "late"),
                1,
                LivenessStage::CprWait
            ),
            ProbeOutcome::Ambiguous { .. }
        ));
        assert!(matches!(
            ProbeOutcome::transport(
                io::Error::other("temporary backend failure"),
                2,
                LivenessStage::InputPoll
            ),
            ProbeOutcome::Ambiguous { .. }
        ));
        assert!(matches!(
            ProbeOutcome::transport(
                io::Error::new(io::ErrorKind::PermissionDenied, "unknown terminal denial"),
                3,
                LivenessStage::CprWait
            ),
            ProbeOutcome::Ambiguous { .. }
        ));
        assert!(matches!(
            ProbeOutcome::transport(
                io::Error::new(io::ErrorKind::InvalidData, "parser invariant"),
                4,
                LivenessStage::CprWait
            ),
            ProbeOutcome::InternalFatal { .. }
        ));
    }

    #[test]
    fn first_failure_is_immutable_and_secondary_context_is_retained() {
        let store = FailureStore::default();
        let first = TerminalFailure::runtime(
            io::ErrorKind::BrokenPipe,
            TerminalFailureClass::DefinitiveLoss,
            LivenessStage::InputRead,
            Some(EvidenceDomain::Transport),
            Duration::ZERO,
            "first",
        );
        let later = TerminalFailure::runtime(
            io::ErrorKind::TimedOut,
            TerminalFailureClass::WorkerStall,
            LivenessStage::Stopping,
            None,
            Duration::from_secs(1),
            "join later",
        );
        assert!(store.record_primary(first));
        assert!(!store.record_primary(later.clone()));
        store.record_secondary(later);
        let message = store.error_snapshot().unwrap().to_string();
        assert!(message.contains("first"));
        assert!(message.contains("join later"));
    }

    #[test]
    fn failure_snapshot_falls_back_to_primary_while_secondary_store_is_busy() {
        let store = FailureStore::default();
        assert!(store.record_primary(TerminalFailure::runtime(
            io::ErrorKind::BrokenPipe,
            TerminalFailureClass::DefinitiveLoss,
            LivenessStage::InputRead,
            Some(EvidenceDomain::Transport),
            Duration::ZERO,
            "immutable primary",
        )));
        let _held_secondary = store.secondary.lock().unwrap();
        assert!(!store.record_secondary(TerminalFailure::runtime(
            io::ErrorKind::TimedOut,
            TerminalFailureClass::WorkerStall,
            LivenessStage::Stopping,
            None,
            Duration::ZERO,
            "best-effort secondary",
        )));

        let snapshot = store
            .error_snapshot()
            .expect("the immutable primary remains immediately reportable");
        assert!(snapshot.to_string().contains("immutable primary"));
    }
}
