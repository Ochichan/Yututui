//! Pure owner-lane scheduling for automatic personal-state synchronization.
//!
//! This module performs no I/O and owns no worker. The TUI and daemon each embed one scheduler
//! beside their existing primary-owner sync coordinator, feed it monotonic time, and execute the
//! generation-tagged attempts it returns. Keeping policy here gives both owners identical debounce,
//! fallback, coalescing, and retry behavior without creating a second state writer.

use std::time::{Duration, Instant};

/// Wait for a quiet period after the latest local personal-state mutation.
pub const LOCAL_MUTATION_DEBOUNCE: Duration = Duration::from_secs(30);
/// Poll the encrypted vault even when no local mutation or reconnect was observed.
pub const FALLBACK_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// First retry delay after an offline attempt, before bounded jitter.
pub const BACKOFF_MIN: Duration = Duration::from_secs(2);
/// Longest client-selected exponential retry delay, including jitter.
///
/// A server-provided `Retry-After` is an independent lower bound and may be longer.
pub const BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);

/// The automatic cause which won when one coalesced sync attempt started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticSyncTrigger {
    Startup,
    LocalMutation,
    Fallback,
    Retry,
    NetworkReconnect,
}

/// Whether an attempt was automatic or explicitly requested by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAttemptKind {
    Automatic(AutomaticSyncTrigger),
    Manual,
}

/// An opaque generation token which must accompany a completion.
///
/// A token is intentionally created only by [`AutomaticSyncScheduler`]. Comparing the complete
/// value prevents an old detached worker from settling a newer attempt after disable/re-enable,
/// retry, or owner restart boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncAttemptToken {
    generation: u64,
    kind: SyncAttemptKind,
}

impl SyncAttemptToken {
    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn kind(self) -> SyncAttemptKind {
        self.kind
    }
}

/// Scheduling-relevant terminal result of one owner-accepted sync attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncAttemptOutcome {
    Succeeded,
    /// A transport failure which should retry automatically.
    ///
    /// `retry_after` is redacted protocol metadata, never an endpoint or response body. The
    /// scheduler treats it as an independent lower bound; client-selected backoff remains capped.
    Offline {
        retry_after: Option<Duration>,
    },
    /// A non-retryable failure. The 15-minute fallback remains armed while automatic sync is on,
    /// allowing corrected credentials or local state to recover without a process restart.
    Failed,
}

/// Result of settling one generation-tagged attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFinish {
    Stale,
    Accepted {
        /// A coalesced cause which was already due when the accepted attempt finished.
        next: Option<SyncAttemptToken>,
    },
}

/// An explicit request cannot start while either owner mode already has a network attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStartError {
    AlreadyInFlight,
}

impl std::fmt::Display for SyncStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("personal sync is already running")
    }
}

impl std::error::Error for SyncStartError {}

/// Stable, injectable jitter for retry scheduling.
///
/// Runtime owners may derive the seed from device-local identity. Tests use either [`Self::none`]
/// for the exact exponential ladder or a fixed seed to prove stable bounded jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffJitter {
    seed: u64,
    enabled: bool,
}

impl BackoffJitter {
    pub const fn none() -> Self {
        Self {
            seed: 0,
            enabled: false,
        }
    }

    pub const fn stable(seed: u64) -> Self {
        Self {
            seed,
            enabled: true,
        }
    }

    /// Derive a non-secret, stable spread from device-local identity.
    pub fn stable_for(device_id: &str) -> Self {
        let seed = device_id
            .as_bytes()
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            });
        Self::stable(seed)
    }

    fn apply(self, base: Duration, generation: u64, step: u8) -> Duration {
        if !self.enabled || base >= BACKOFF_MAX {
            return base.min(BACKOFF_MAX);
        }
        let base_millis = duration_millis(base);
        let max_millis = duration_millis(BACKOFF_MAX);
        let room = max_millis.saturating_sub(base_millis);
        let window = (base_millis / 4).min(room);
        if window == 0 {
            return base;
        }
        let mixed = mix64(self.seed ^ generation.rotate_left(17) ^ u64::from(step).rotate_left(41));
        Duration::from_millis(base_millis + mixed % (window + 1))
    }
}

/// Shared automatic-sync policy state. It must live on the primary owner's lane.
#[derive(Debug)]
pub struct AutomaticSyncScheduler {
    enabled: bool,
    next_generation: u64,
    in_flight: Option<SyncAttemptToken>,
    immediate: Option<AutomaticSyncTrigger>,
    local_due: Option<Instant>,
    retry_due: Option<Instant>,
    retry_not_before: Option<Instant>,
    fallback_due: Option<Instant>,
    backoff_step: u8,
    offline_latched: bool,
    jitter: BackoffJitter,
}

impl AutomaticSyncScheduler {
    pub fn new(jitter: BackoffJitter) -> Self {
        Self {
            enabled: false,
            next_generation: 0,
            in_flight: None,
            immediate: None,
            local_due: None,
            retry_due: None,
            retry_not_before: None,
            fallback_due: None,
            backoff_step: 0,
            offline_latched: false,
            jitter,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn in_flight(&self) -> Option<SyncAttemptToken> {
        self.in_flight
    }

    /// Whether the owner should listen for a positive local connectivity transition.
    pub fn waiting_for_connectivity(&self) -> bool {
        self.enabled
            && self.in_flight.is_none()
            && self.offline_latched
            && self.immediate != Some(AutomaticSyncTrigger::NetworkReconnect)
    }

    /// Configure retry spread before this owner enables or starts synchronization.
    pub fn configure_jitter(&mut self, jitter: BackoffJitter) {
        if !self.enabled && self.in_flight.is_none() {
            self.jitter = jitter;
        }
    }

    /// Enable automatic sync and start the startup attempt immediately when the owner is idle.
    ///
    /// Enabling while a manual or previous automatic attempt is running coalesces Startup behind
    /// that attempt instead of creating a second worker.
    pub fn enable(&mut self, now: Instant) -> Option<SyncAttemptToken> {
        if !self.enabled {
            self.enabled = true;
            self.immediate.get_or_insert(AutomaticSyncTrigger::Startup);
        }
        self.poll(now)
    }

    /// Disable future automatic work without cancelling a worker which may already have written
    /// immutable remote objects. Its exact token may still settle safely; no follow-up is armed.
    pub fn disable(&mut self) {
        self.enabled = false;
        self.clear_automatic_work();
        self.backoff_step = 0;
        self.offline_latched = false;
    }

    /// Record one newly durable local personal-state mutation.
    pub fn local_mutation(&mut self, now: Instant) -> Option<SyncAttemptToken> {
        if !self.enabled {
            return None;
        }
        self.local_due = Some(deadline_after(now, LOCAL_MUTATION_DEBOUNCE));
        self.poll(now)
    }

    /// Positive connectivity evidence bypasses a pending transport backoff.
    ///
    /// Owners call this only after this scheduler latched an offline WebDAV result. Arbitrary
    /// successful UI events therefore cannot produce extra network traffic. A server-provided
    /// retry lower bound remains authoritative even after connectivity returns.
    pub fn network_reconnected(&mut self, now: Instant) -> Option<SyncAttemptToken> {
        if !self.enabled || !self.offline_latched {
            return None;
        }
        self.immediate = Some(AutomaticSyncTrigger::NetworkReconnect);
        if self
            .retry_not_before
            .is_some_and(|not_before| now < not_before)
        {
            return None;
        }
        self.offline_latched = false;
        self.retry_due = None;
        self.poll(now)
    }

    /// Start an explicit sync through the same single-flight gate.
    ///
    /// A manual pass consumes every cause observed before it starts because its owner snapshot
    /// covers that state. Causes received after it starts remain pending for a follow-up.
    pub fn begin_manual(&mut self, _now: Instant) -> Result<SyncAttemptToken, SyncStartError> {
        if self.in_flight.is_some() {
            return Err(SyncStartError::AlreadyInFlight);
        }
        Ok(self.start(SyncAttemptKind::Manual))
    }

    /// Start automatic work whose deadline has arrived, if the owner lane is idle.
    pub fn poll(&mut self, now: Instant) -> Option<SyncAttemptToken> {
        if !self.enabled || self.in_flight.is_some() {
            return None;
        }
        if self
            .retry_not_before
            .is_some_and(|not_before| now < not_before)
        {
            return None;
        }
        let trigger = if self.offline_latched {
            self.immediate
                .filter(|trigger| *trigger == AutomaticSyncTrigger::NetworkReconnect)
                .or_else(|| due(self.retry_due, now).then_some(AutomaticSyncTrigger::Retry))
        } else {
            self.immediate.or_else(|| {
                due(self.retry_due, now)
                    .then_some(AutomaticSyncTrigger::Retry)
                    .or_else(|| {
                        due(self.local_due, now).then_some(AutomaticSyncTrigger::LocalMutation)
                    })
                    .or_else(|| {
                        due(self.fallback_due, now).then_some(AutomaticSyncTrigger::Fallback)
                    })
            })
        }?;
        Some(self.start(SyncAttemptKind::Automatic(trigger)))
    }

    /// Earliest timer the owner needs to park in its event loop.
    ///
    /// There is no timer while a worker is active: causes continue to coalesce, and its completion
    /// immediately evaluates anything which became due in the meantime.
    pub fn next_deadline(&self) -> Option<Instant> {
        if !self.enabled || self.in_flight.is_some() {
            return None;
        }
        if self.offline_latched {
            if self.immediate == Some(AutomaticSyncTrigger::NetworkReconnect) {
                return self.retry_not_before.or(self.retry_due);
            }
            return self.retry_due;
        }
        let pending = [self.retry_due, self.local_due, self.fallback_due]
            .into_iter()
            .flatten()
            .min();
        match (pending, self.retry_not_before) {
            (Some(pending), Some(not_before)) => Some(pending.max(not_before)),
            (pending, None) => pending,
            (None, Some(_)) => None,
        }
    }

    /// Settle one exact worker generation and return an already-due coalesced follow-up.
    pub fn finish(
        &mut self,
        token: SyncAttemptToken,
        outcome: SyncAttemptOutcome,
        now: Instant,
    ) -> SyncFinish {
        if self.in_flight != Some(token) {
            return SyncFinish::Stale;
        }
        self.in_flight = None;
        if !self.enabled {
            self.clear_automatic_work();
            self.backoff_step = 0;
            self.offline_latched = false;
            return SyncFinish::Accepted { next: None };
        }
        match outcome {
            SyncAttemptOutcome::Succeeded => {
                self.clear_reconnect_signal();
                self.backoff_step = 0;
                self.offline_latched = false;
                self.retry_due = None;
                self.retry_not_before = None;
                self.arm_fallback(now);
            }
            SyncAttemptOutcome::Offline { retry_after } => {
                // A connectivity edge observed before this newer failed request is stale. Keeping
                // it would immediately bypass the backoff which this failure is about to arm.
                self.clear_reconnect_signal();
                self.offline_latched = true;
                self.arm_fallback(now);
                if self.enabled {
                    let base = backoff_base(self.backoff_step);
                    let delay = self.jitter.apply(base, token.generation, self.backoff_step);
                    self.backoff_step = self.backoff_step.saturating_add(1);
                    let existing_embargo =
                        self.retry_not_before.filter(|not_before| *not_before > now);
                    let hinted_embargo =
                        retry_after.map(|hint| deadline_after_saturating(now, hint));
                    self.retry_not_before = match (existing_embargo, hinted_embargo) {
                        (Some(existing), Some(hinted)) => Some(existing.max(hinted)),
                        (existing, hinted) => existing.or(hinted),
                    };
                    let retry_due = deadline_after(now, delay);
                    self.retry_due = Some(
                        self.retry_not_before
                            .map_or(retry_due, |not_before| retry_due.max(not_before)),
                    );
                }
            }
            SyncAttemptOutcome::Failed => {
                self.clear_reconnect_signal();
                self.backoff_step = 0;
                self.offline_latched = false;
                self.retry_due = None;
                self.retry_not_before =
                    self.retry_not_before.filter(|not_before| *not_before > now);
                self.arm_fallback(now);
            }
        }
        SyncFinish::Accepted {
            next: self.poll(now),
        }
    }

    fn start(&mut self, kind: SyncAttemptKind) -> SyncAttemptToken {
        debug_assert!(self.in_flight.is_none());
        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == 0 {
            self.next_generation = 1;
        }
        let token = SyncAttemptToken {
            generation: self.next_generation,
            kind,
        };
        self.in_flight = Some(token);
        self.immediate = None;
        self.local_due = None;
        self.retry_due = None;
        self.fallback_due = None;
        token
    }

    fn clear_automatic_work(&mut self) {
        self.immediate = None;
        self.local_due = None;
        self.retry_due = None;
        self.retry_not_before = None;
        self.fallback_due = None;
    }

    fn arm_fallback(&mut self, now: Instant) {
        self.fallback_due = self.enabled.then(|| deadline_after(now, FALLBACK_INTERVAL));
    }

    fn clear_reconnect_signal(&mut self) {
        if self.immediate == Some(AutomaticSyncTrigger::NetworkReconnect) {
            self.immediate = None;
        }
    }
}

impl Default for AutomaticSyncScheduler {
    fn default() -> Self {
        Self::new(BackoffJitter::none())
    }
}

fn due(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|deadline| deadline <= now)
}

fn deadline_after(now: Instant, duration: Duration) -> Instant {
    now.checked_add(duration).unwrap_or(now)
}

fn deadline_after_saturating(now: Instant, duration: Duration) -> Instant {
    if let Some(deadline) = now.checked_add(duration) {
        return deadline;
    }
    let mut lower = 0_u64;
    let mut upper = duration.as_secs();
    while lower < upper {
        let midpoint = lower + (upper - lower).div_ceil(2);
        if now.checked_add(Duration::from_secs(midpoint)).is_some() {
            lower = midpoint;
        } else {
            upper = midpoint - 1;
        }
    }
    now.checked_add(Duration::from_secs(lower)).unwrap_or(now)
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn backoff_base(step: u8) -> Duration {
    let multiplier = 1_u64.checked_shl(u32::from(step)).unwrap_or(u64::MAX);
    Duration::from_secs(
        BACKOFF_MIN
            .as_secs()
            .saturating_mul(multiplier)
            .min(BACKOFF_MAX.as_secs()),
    )
}

fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted_next(result: SyncFinish) -> Option<SyncAttemptToken> {
        match result {
            SyncFinish::Accepted { next } => next,
            SyncFinish::Stale => panic!("the current token must be accepted"),
        }
    }

    fn automatic_trigger(token: SyncAttemptToken) -> AutomaticSyncTrigger {
        match token.kind() {
            SyncAttemptKind::Automatic(trigger) => trigger,
            SyncAttemptKind::Manual => panic!("expected an automatic token"),
        }
    }

    #[test]
    fn enabled_startup_is_immediate_and_success_arms_the_fallback() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::default();

        let startup = scheduler.enable(now).expect("startup attempt");
        assert_eq!(startup.generation(), 1);
        assert_eq!(automatic_trigger(startup), AutomaticSyncTrigger::Startup);
        assert_eq!(scheduler.in_flight(), Some(startup));
        assert_eq!(scheduler.next_deadline(), None);

        assert_eq!(
            accepted_next(scheduler.finish(startup, SyncAttemptOutcome::Succeeded, now)),
            None
        );
        assert_eq!(scheduler.next_deadline(), Some(now + FALLBACK_INTERVAL));
    }

    #[test]
    fn local_mutations_use_a_trailing_thirty_second_debounce() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::default();
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(startup, SyncAttemptOutcome::Succeeded, now);

        assert_eq!(scheduler.local_mutation(now + Duration::from_secs(3)), None);
        assert_eq!(
            scheduler.next_deadline(),
            Some(now + Duration::from_secs(33))
        );
        assert_eq!(
            scheduler.local_mutation(now + Duration::from_secs(20)),
            None
        );
        let due = now + Duration::from_secs(50);
        assert_eq!(scheduler.next_deadline(), Some(due));
        assert_eq!(scheduler.poll(due - Duration::from_nanos(1)), None);

        let local = scheduler.poll(due).expect("debounced attempt");
        assert_eq!(
            automatic_trigger(local),
            AutomaticSyncTrigger::LocalMutation
        );
    }

    #[test]
    fn fallback_runs_every_fifteen_minutes_without_other_causes() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::default();
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(startup, SyncAttemptOutcome::Succeeded, now);

        assert_eq!(
            scheduler.poll(now + FALLBACK_INTERVAL - Duration::from_nanos(1)),
            None
        );
        let fallback = scheduler
            .poll(now + FALLBACK_INTERVAL)
            .expect("fallback attempt");
        assert_eq!(automatic_trigger(fallback), AutomaticSyncTrigger::Fallback);
    }

    #[test]
    fn causes_coalesce_while_one_attempt_is_in_flight() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::default();
        let startup = scheduler.enable(now).unwrap();

        assert_eq!(scheduler.local_mutation(now + Duration::from_secs(1)), None);
        assert_eq!(
            scheduler.local_mutation(now + Duration::from_secs(10)),
            None
        );
        let next = accepted_next(scheduler.finish(
            startup,
            SyncAttemptOutcome::Succeeded,
            now + Duration::from_secs(45),
        ))
        .expect("the quiet period elapsed while startup was running");
        assert_eq!(automatic_trigger(next), AutomaticSyncTrigger::LocalMutation);
        assert_eq!(next.generation(), startup.generation() + 1);
        assert_eq!(scheduler.poll(now + Duration::from_secs(45)), None);
    }

    #[test]
    fn offline_backoff_doubles_from_two_seconds_to_fifteen_minutes() {
        let mut now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::new(BackoffJitter::none());
        let mut token = scheduler.enable(now).unwrap();
        let expected = [2, 4, 8, 16, 32, 64, 128, 256, 512, 900, 900];

        for seconds in expected {
            assert_eq!(
                accepted_next(scheduler.finish(
                    token,
                    SyncAttemptOutcome::Offline { retry_after: None },
                    now,
                )),
                None
            );
            let due = now + Duration::from_secs(seconds);
            assert_eq!(scheduler.next_deadline(), Some(due));
            token = scheduler.poll(due).expect("retry attempt");
            assert_eq!(automatic_trigger(token), AutomaticSyncTrigger::Retry);
            now = due;
        }
    }

    #[test]
    fn retry_after_is_an_unclamped_lower_bound() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::new(BackoffJitter::none());
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(
            startup,
            SyncAttemptOutcome::Offline {
                retry_after: Some(Duration::from_secs(75)),
            },
            now,
        );
        assert_eq!(
            scheduler.next_deadline(),
            Some(now + Duration::from_secs(75))
        );

        let retry = scheduler.poll(now + Duration::from_secs(75)).unwrap();
        let _ = scheduler.finish(
            retry,
            SyncAttemptOutcome::Offline {
                retry_after: Some(BACKOFF_MAX + Duration::from_secs(3_600)),
            },
            now + Duration::from_secs(75),
        );
        assert_eq!(
            scheduler.next_deadline(),
            Some(now + Duration::from_secs(75) + BACKOFF_MAX + Duration::from_secs(3_600))
        );
    }

    #[test]
    fn manual_failure_cannot_shorten_an_existing_server_embargo() {
        let now = Instant::now();
        let embargo = now + Duration::from_secs(600);
        let mut scheduler = AutomaticSyncScheduler::new(BackoffJitter::none());
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(
            startup,
            SyncAttemptOutcome::Offline {
                retry_after: Some(Duration::from_secs(600)),
            },
            now,
        );

        let manual = scheduler
            .begin_manual(now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(manual.kind(), SyncAttemptKind::Manual);
        let _ = scheduler.finish(
            manual,
            SyncAttemptOutcome::Offline { retry_after: None },
            now + Duration::from_secs(2),
        );

        assert_eq!(scheduler.next_deadline(), Some(embargo));
        assert_eq!(scheduler.poll(embargo - Duration::from_nanos(1)), None);
        assert_eq!(
            automatic_trigger(scheduler.poll(embargo).expect("embargo elapsed")),
            AutomaticSyncTrigger::Retry
        );
    }

    #[test]
    fn a_successful_manual_override_clears_the_server_embargo() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::new(BackoffJitter::none());
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(
            startup,
            SyncAttemptOutcome::Offline {
                retry_after: Some(Duration::from_secs(600)),
            },
            now,
        );
        let manual = scheduler
            .begin_manual(now + Duration::from_secs(1))
            .unwrap();

        let _ = scheduler.finish(
            manual,
            SyncAttemptOutcome::Succeeded,
            now + Duration::from_secs(2),
        );

        assert_eq!(
            scheduler.next_deadline(),
            Some(now + Duration::from_secs(2) + FALLBACK_INTERVAL)
        );
    }

    #[test]
    fn oversized_retry_after_saturates_to_a_distant_deadline() {
        let now = Instant::now();
        let deadline = deadline_after_saturating(now, Duration::MAX);

        assert!(deadline > now + BACKOFF_MAX);
        assert_eq!(
            deadline_after_saturating(now, Duration::from_secs(75)),
            now + Duration::from_secs(75)
        );
    }

    #[test]
    fn local_mutations_cannot_bypass_an_offline_retry_hint() {
        let now = Instant::now();
        let server_hint = BACKOFF_MAX + Duration::from_secs(3_600);
        let mut scheduler = AutomaticSyncScheduler::new(BackoffJitter::none());
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(
            startup,
            SyncAttemptOutcome::Offline {
                retry_after: Some(server_hint),
            },
            now,
        );
        let retry_due = now + server_hint;

        assert_eq!(scheduler.local_mutation(now + Duration::from_secs(1)), None);
        assert_eq!(scheduler.next_deadline(), Some(retry_due));
        assert_eq!(
            scheduler.poll(now + LOCAL_MUTATION_DEBOUNCE + Duration::from_secs(1)),
            None
        );
        let retry = scheduler.poll(retry_due).expect("bounded retry");
        assert_eq!(automatic_trigger(retry), AutomaticSyncTrigger::Retry);
    }

    #[test]
    fn reconnect_cannot_bypass_a_server_retry_hint() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::new(BackoffJitter::none());
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(
            startup,
            SyncAttemptOutcome::Offline {
                retry_after: Some(Duration::from_secs(75)),
            },
            now,
        );
        let retry_due = now + Duration::from_secs(75);
        assert!(scheduler.waiting_for_connectivity());

        assert_eq!(
            scheduler.network_reconnected(now + Duration::from_secs(1)),
            None
        );
        assert!(!scheduler.waiting_for_connectivity());
        assert_eq!(scheduler.next_deadline(), Some(retry_due));
        assert_eq!(
            scheduler.poll(now + Duration::from_secs(74)),
            None,
            "connectivity evidence must not override Retry-After"
        );
        let reconnect = scheduler
            .poll(retry_due)
            .expect("queued reconnect starts after the retry lower bound");
        assert_eq!(
            automatic_trigger(reconnect),
            AutomaticSyncTrigger::NetworkReconnect
        );
    }

    #[test]
    fn reconnect_bypasses_only_the_transport_backoff_after_a_short_server_hint() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::new(BackoffJitter::none());
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(
            startup,
            SyncAttemptOutcome::Offline {
                retry_after: Some(Duration::from_secs(1)),
            },
            now,
        );
        assert_eq!(
            scheduler.next_deadline(),
            Some(now + BACKOFF_MIN),
            "without connectivity evidence, exponential backoff remains active"
        );

        assert_eq!(
            scheduler.network_reconnected(now + Duration::from_millis(100)),
            None
        );
        let retry_not_before = now + Duration::from_secs(1);
        assert_eq!(scheduler.next_deadline(), Some(retry_not_before));
        let reconnect = scheduler
            .poll(retry_not_before)
            .expect("connectivity resumes work after the server lower bound");
        assert_eq!(
            automatic_trigger(reconnect),
            AutomaticSyncTrigger::NetworkReconnect
        );
    }

    #[test]
    fn stable_jitter_is_repeatable_bounded_and_never_exceeds_the_cap() {
        let now = Instant::now();
        let device_jitter = BackoffJitter::stable_for("device-a");
        assert_eq!(device_jitter, BackoffJitter::stable_for("device-a"));
        assert_ne!(device_jitter, BackoffJitter::stable_for("device-b"));
        let mut left = AutomaticSyncScheduler::new(device_jitter);
        let mut right = AutomaticSyncScheduler::new(device_jitter);
        let left_start = left.enable(now).unwrap();
        let right_start = right.enable(now).unwrap();
        let _ = left.finish(
            left_start,
            SyncAttemptOutcome::Offline { retry_after: None },
            now,
        );
        let _ = right.finish(
            right_start,
            SyncAttemptOutcome::Offline { retry_after: None },
            now,
        );

        let left_delay = left.next_deadline().unwrap().duration_since(now);
        let right_delay = right.next_deadline().unwrap().duration_since(now);
        assert_eq!(left_delay, right_delay);
        assert!(left_delay >= BACKOFF_MIN);
        assert!(left_delay <= BACKOFF_MIN + BACKOFF_MIN / 4);

        let capped = BackoffJitter::stable(9).apply(BACKOFF_MAX, 99, u8::MAX);
        assert_eq!(capped, BACKOFF_MAX);
    }

    #[test]
    fn reconnect_is_absorbed_but_a_newer_local_mutation_remains_due() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::default();
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(
            startup,
            SyncAttemptOutcome::Offline { retry_after: None },
            now,
        );
        let manual = scheduler
            .begin_manual(now + Duration::from_secs(1))
            .unwrap();

        assert_eq!(
            scheduler.network_reconnected(now + Duration::from_secs(1)),
            None
        );
        assert_eq!(scheduler.local_mutation(now + Duration::from_secs(1)), None);
        let next = accepted_next(scheduler.finish(
            manual,
            SyncAttemptOutcome::Succeeded,
            now + Duration::from_secs(1),
        ));
        assert_eq!(next, None);
        assert_eq!(
            scheduler.next_deadline(),
            Some(now + Duration::from_secs(1) + LOCAL_MUTATION_DEBOUNCE)
        );
    }

    #[test]
    fn failed_in_flight_attempt_supersedes_an_older_reconnect_edge() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::default();
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(
            startup,
            SyncAttemptOutcome::Offline { retry_after: None },
            now,
        );
        let manual = scheduler
            .begin_manual(now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            scheduler.network_reconnected(now + Duration::from_secs(1)),
            None
        );

        assert_eq!(
            accepted_next(scheduler.finish(
                manual,
                SyncAttemptOutcome::Offline {
                    retry_after: Some(Duration::from_secs(75)),
                },
                now + Duration::from_secs(2),
            )),
            None
        );
        assert_eq!(
            scheduler.next_deadline(),
            Some(now + Duration::from_secs(77))
        );
    }

    #[test]
    fn manual_attempts_share_single_flight_and_cover_existing_causes() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::default();
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(startup, SyncAttemptOutcome::Succeeded, now);
        let _ = scheduler.local_mutation(now + Duration::from_secs(1));

        let manual = scheduler
            .begin_manual(now + Duration::from_secs(2))
            .unwrap();
        assert_eq!(manual.kind(), SyncAttemptKind::Manual);
        assert_eq!(
            scheduler.begin_manual(now + Duration::from_secs(2)),
            Err(SyncStartError::AlreadyInFlight)
        );
        assert_eq!(scheduler.next_deadline(), None);

        let _ = scheduler.local_mutation(now + Duration::from_secs(3));
        assert_eq!(
            accepted_next(scheduler.finish(
                manual,
                SyncAttemptOutcome::Succeeded,
                now + Duration::from_secs(10),
            )),
            None
        );
        assert_eq!(
            scheduler.next_deadline(),
            Some(now + Duration::from_secs(33))
        );
    }

    #[test]
    fn stale_completion_cannot_settle_the_current_generation() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::default();
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.finish(startup, SyncAttemptOutcome::Succeeded, now);
        let fallback = scheduler.poll(now + FALLBACK_INTERVAL).unwrap();

        assert_eq!(
            scheduler.finish(
                startup,
                SyncAttemptOutcome::Offline { retry_after: None },
                now + FALLBACK_INTERVAL,
            ),
            SyncFinish::Stale
        );
        assert_eq!(scheduler.in_flight(), Some(fallback));
        assert_eq!(scheduler.next_deadline(), None);
    }

    #[test]
    fn disable_drops_automatic_deadlines_but_allows_safe_in_flight_settlement() {
        let now = Instant::now();
        let mut scheduler = AutomaticSyncScheduler::default();
        let startup = scheduler.enable(now).unwrap();
        let _ = scheduler.local_mutation(now + Duration::from_secs(1));

        scheduler.disable();
        assert!(!scheduler.enabled());
        assert_eq!(scheduler.next_deadline(), None);
        assert_eq!(scheduler.poll(now + Duration::from_secs(60)), None);
        assert_eq!(
            scheduler.finish(
                startup,
                SyncAttemptOutcome::Offline {
                    retry_after: Some(Duration::from_secs(75)),
                },
                now,
            ),
            SyncFinish::Accepted { next: None }
        );
        assert_eq!(scheduler.next_deadline(), None);

        let restarted = scheduler.enable(now).expect("fresh startup after enable");
        assert_eq!(automatic_trigger(restarted), AutomaticSyncTrigger::Startup);
        assert!(restarted.generation() > startup.generation());
    }
}
