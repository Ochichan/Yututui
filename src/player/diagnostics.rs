//! Bounded, process-local diagnostics for long-form seeking and cache lifecycle.
//!
//! The production surface is deliberately counters, durations, and finite reason codes. It never
//! retains media identifiers, URLs, request IDs, or property samples. High-rate observations only
//! update counters; one cumulative summary is emitted at most once per minute (plus actor close).

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::long_form_seek::{CacheEffectiveState, CacheReason, CacheStatus};
use crate::config::LongFormSeekOptimization;

const SUMMARY_INTERVAL_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SourceRecoveryOutcome {
    AdmissionAccepted,
    AdmissionRejected,
    ValidationRejected,
    Superseded,
    LoadRejected,
    ResumeRejected,
    ResumeDispatched,
}

impl SourceRecoveryOutcome {
    const fn id(self) -> &'static str {
        match self {
            Self::AdmissionAccepted => "admission_accepted",
            Self::AdmissionRejected => "admission_rejected",
            Self::ValidationRejected => "validation_rejected",
            Self::Superseded => "superseded",
            Self::LoadRejected => "load_rejected",
            Self::ResumeRejected => "resume_rejected",
            Self::ResumeDispatched => "resume_dispatched",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Snapshot {
    scrub_started: u64,
    scrub_cancelled: u64,
    scrub_committed: u64,
    interactive_admitted: u64,
    interactive_coalesced: u64,
    interactive_dispatched: u64,
    final_restart_count: u64,
    final_restart_total_ms: u64,
    final_restart_max_ms: u64,
    stale_restarts: u64,
    paused_for_cache_count: u64,
    paused_for_cache_total_ms: u64,
    auto_candidates: u64,
    auto_rejections: BTreeMap<&'static str, u64>,
    disk_activation_attempts: u64,
    disk_activation_successes: u64,
    disk_activation_failures: u64,
    file_cache_current_bytes: u64,
    file_cache_peak_bytes: u64,
    cache_latches: BTreeMap<&'static str, u64>,
    cleanup_count: u64,
    cleanup_total_ms: u64,
    cleanup_max_ms: u64,
    source_recovery_attempts: u64,
    source_recovery_causes: BTreeMap<&'static str, u64>,
    source_recovery_outcomes: BTreeMap<&'static str, u64>,
}

#[derive(Default)]
struct State {
    snapshot: Snapshot,
    dirty: bool,
    last_summary_ms: u64,
    paused_for_cache: Option<(Option<u64>, u64)>,
    last_cache_status: Option<CacheStatus>,
    auto_generation: Option<u64>,
    auto_candidate_generation: Option<u64>,
    auto_rejection: Option<(u64, CacheReason)>,
    activation_pending_generation: Option<u64>,
    last_latch: Option<(u64, CacheReason)>,
    cleanup_started: Option<(u64, u64)>,
}

pub(crate) struct ProductionDiagnostics {
    started: Instant,
    state: Mutex<State>,
}

impl Default for ProductionDiagnostics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            state: Mutex::new(State::default()),
        }
    }
}

impl ProductionDiagnostics {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn update(&self, update: impl FnOnce(&mut State, u64)) {
        self.update_at(self.now_ms(), update);
    }

    fn update_at(&self, now_ms: u64, update: impl FnOnce(&mut State, u64)) {
        let summary = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            update(&mut state, now_ms);
            take_summary(&mut state, now_ms, false)
        };
        if let Some(summary) = summary {
            log_summary(&summary);
        }
    }

    fn scrub_started(&self) {
        self.update(|state, _| {
            bump(&mut state.snapshot.scrub_started, 1);
            state.dirty = true;
        });
    }

    fn scrub_cancelled(&self) {
        self.update(|state, _| {
            bump(&mut state.snapshot.scrub_cancelled, 1);
            state.dirty = true;
        });
    }

    fn scrub_committed(&self) {
        self.update(|state, _| {
            bump(&mut state.snapshot.scrub_committed, 1);
            state.dirty = true;
        });
    }

    fn interactive_admitted(&self, count: u64, coalesced: u64) {
        if count == 0 {
            return;
        }
        self.update(|state, _| {
            bump(&mut state.snapshot.interactive_admitted, count);
            bump(
                &mut state.snapshot.interactive_coalesced,
                coalesced.min(count),
            );
            state.dirty = true;
        });
    }

    fn interactive_coalesced(&self) {
        self.update(|state, _| {
            bump(&mut state.snapshot.interactive_coalesced, 1);
            state.dirty = true;
        });
    }

    fn interactive_dispatched(&self) {
        self.update(|state, _| {
            bump(&mut state.snapshot.interactive_dispatched, 1);
            state.dirty = true;
        });
    }

    fn seek_restart(&self, dispatch_started_ms: u64, stale: bool) {
        self.update(|state, now_ms| {
            if stale {
                bump(&mut state.snapshot.stale_restarts, 1);
            } else {
                let latency_ms = now_ms.saturating_sub(dispatch_started_ms);
                bump(&mut state.snapshot.final_restart_count, 1);
                bump(&mut state.snapshot.final_restart_total_ms, latency_ms);
                state.snapshot.final_restart_max_ms =
                    state.snapshot.final_restart_max_ms.max(latency_ms);
            }
            state.dirty = true;
        });
    }

    fn paused_for_cache(&self, generation: Option<u64>, paused: bool) {
        self.update(|state, now_ms| {
            record_paused_for_cache(state, generation, paused, now_ms);
        });
    }

    fn cache_status(&self, status: CacheStatus) -> bool {
        let mut transition = false;
        self.update(|state, now_ms| {
            transition = record_cache_status(state, status, now_ms);
        });
        transition
    }

    fn source_recovery_attempt(&self, cause: &'static str) {
        self.update(|state, _| {
            bump(&mut state.snapshot.source_recovery_attempts, 1);
            bump_map(&mut state.snapshot.source_recovery_causes, cause);
            state.dirty = true;
        });
        tracing::info!(cause, "position-preserving source recovery attempted");
    }

    fn source_recovery_outcome(&self, outcome: SourceRecoveryOutcome) {
        self.update(|state, _| {
            bump_map(&mut state.snapshot.source_recovery_outcomes, outcome.id());
            state.dirty = true;
        });
        tracing::info!(
            outcome = outcome.id(),
            "position-preserving source recovery changed"
        );
    }

    fn actor_closed(&self) {
        let now_ms = self.now_ms();
        let summary = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            record_paused_for_cache(&mut state, None, false, now_ms);
            if state.activation_pending_generation.take().is_some() {
                bump(&mut state.snapshot.disk_activation_failures, 1);
                state.dirty = true;
            }
            state.dirty |= state.snapshot.file_cache_current_bytes != 0;
            state.snapshot.file_cache_current_bytes = 0;
            state.last_cache_status = None;
            state.cleanup_started = None;
            take_summary(&mut state, now_ms, true)
        };
        if let Some(summary) = summary {
            log_summary(&summary);
        }
    }
}

fn global() -> &'static ProductionDiagnostics {
    static DIAGNOSTICS: OnceLock<ProductionDiagnostics> = OnceLock::new();
    DIAGNOSTICS.get_or_init(ProductionDiagnostics::default)
}

pub(crate) fn scrub_started() {
    global().scrub_started();
}

pub(crate) fn scrub_cancelled() {
    global().scrub_cancelled();
}

pub(crate) fn scrub_committed() {
    global().scrub_committed();
}

pub(crate) fn interactive_admitted(count: u64, coalesced: u64) {
    global().interactive_admitted(count, coalesced);
}

pub(crate) fn interactive_coalesced() {
    global().interactive_coalesced();
}

pub(crate) fn interactive_dispatched() -> u64 {
    let diagnostics = global();
    diagnostics.interactive_dispatched();
    diagnostics.now_ms()
}

pub(crate) fn seek_restart(dispatch_started_ms: u64, stale: bool) {
    global().seek_restart(dispatch_started_ms, stale);
}

pub(crate) fn paused_for_cache(generation: Option<u64>, paused: bool) {
    global().paused_for_cache(generation, paused);
}

pub(crate) fn cache_status(status: CacheStatus) -> bool {
    global().cache_status(status)
}

pub(crate) fn source_recovery_attempt(cause: &'static str) {
    global().source_recovery_attempt(cause);
}

pub(crate) fn source_recovery_outcome(outcome: SourceRecoveryOutcome) {
    global().source_recovery_outcome(outcome);
}

pub(crate) fn actor_closed() {
    global().actor_closed();
}

fn record_paused_for_cache(state: &mut State, generation: Option<u64>, paused: bool, now_ms: u64) {
    if paused {
        if state
            .paused_for_cache
            .is_some_and(|(active_generation, _)| active_generation == generation)
        {
            return;
        }
        close_paused_for_cache(state, now_ms);
        state.paused_for_cache = Some((generation, now_ms));
        bump(&mut state.snapshot.paused_for_cache_count, 1);
        state.dirty = true;
    } else {
        close_paused_for_cache(state, now_ms);
    }
}

fn close_paused_for_cache(state: &mut State, now_ms: u64) {
    if let Some((_, started_ms)) = state.paused_for_cache.take() {
        bump(
            &mut state.snapshot.paused_for_cache_total_ms,
            now_ms.saturating_sub(started_ms),
        );
        state.dirty = true;
    }
}

fn record_cache_status(state: &mut State, status: CacheStatus, now_ms: u64) -> bool {
    let previous_current = state.snapshot.file_cache_current_bytes;
    let previous_peak = state.snapshot.file_cache_peak_bytes;
    state.snapshot.file_cache_current_bytes = status.file_cache_bytes;
    state.snapshot.file_cache_peak_bytes = state
        .snapshot
        .file_cache_peak_bytes
        .max(status.peak_file_cache_bytes)
        .max(status.file_cache_bytes);
    state.dirty |= previous_current != state.snapshot.file_cache_current_bytes
        || previous_peak != state.snapshot.file_cache_peak_bytes;

    let transition = state
        .last_cache_status
        .is_none_or(|previous| cache_control_changed(previous, status));
    state.last_cache_status = Some(status);
    if !transition {
        return false;
    }
    state.dirty = true;

    let generation = status.file_generation.unwrap_or(0);
    if status.file_generation != state.auto_generation {
        state.auto_generation = status.file_generation;
        state.auto_rejection = None;
    }
    if status.requested == LongFormSeekOptimization::Auto {
        if matches!(
            status.effective,
            CacheEffectiveState::Probing | CacheEffectiveState::EnablePending
        ) && state.auto_candidate_generation != status.file_generation
        {
            bump(&mut state.snapshot.auto_candidates, 1);
            state.auto_candidate_generation = status.file_generation;
        }
        if stable_auto_rejection(status.effective, status.reason)
            && state.auto_rejection != Some((generation, status.reason))
        {
            bump_map(&mut state.snapshot.auto_rejections, status.reason.id());
            state.auto_rejection = Some((generation, status.reason));
            tracing::info!(
                reason = status.reason.id(),
                "long-form Auto candidate rejected"
            );
        }
    }

    if status.effective == CacheEffectiveState::EnablePending
        && state.activation_pending_generation != Some(generation)
    {
        bump(&mut state.snapshot.disk_activation_attempts, 1);
        state.activation_pending_generation = Some(generation);
    } else if status.effective == CacheEffectiveState::DiskActive
        && state.activation_pending_generation == Some(generation)
    {
        bump(&mut state.snapshot.disk_activation_successes, 1);
        state.activation_pending_generation = None;
    } else if state.activation_pending_generation == Some(generation)
        && status.effective != CacheEffectiveState::EnablePending
    {
        if activation_failure(status.reason) {
            bump(&mut state.snapshot.disk_activation_failures, 1);
        }
        state.activation_pending_generation = None;
    }

    if matches!(
        status.effective,
        CacheEffectiveState::DisablePending
            | CacheEffectiveState::LatchedUntilClose
            | CacheEffectiveState::EmergencyClosePending
    ) && latch_reason(status.reason)
        && state.last_latch != Some((generation, status.reason))
    {
        bump_map(&mut state.snapshot.cache_latches, status.reason.id());
        state.last_latch = Some((generation, status.reason));
    }

    if status.effective == CacheEffectiveState::DisablePending
        && status.reason == CacheReason::MediaClosed
        && state.cleanup_started.is_none()
    {
        state.cleanup_started = Some((generation, now_ms));
    }
    if let Some((cleanup_generation, started_ms)) = state.cleanup_started
        && status.file_generation != Some(cleanup_generation)
    {
        let elapsed_ms = now_ms.saturating_sub(started_ms);
        bump(&mut state.snapshot.cleanup_count, 1);
        bump(&mut state.snapshot.cleanup_total_ms, elapsed_ms);
        state.snapshot.cleanup_max_ms = state.snapshot.cleanup_max_ms.max(elapsed_ms);
        state.cleanup_started = None;
    }
    true
}

fn cache_control_changed(previous: CacheStatus, current: CacheStatus) -> bool {
    previous.requested != current.requested
        || previous.effective != current.effective
        || previous.reason != current.reason
        || previous.file_generation != current.file_generation
        || previous.policy_revision != current.policy_revision
}

fn stable_auto_rejection(effective: CacheEffectiveState, reason: CacheReason) -> bool {
    matches!(
        effective,
        CacheEffectiveState::RamOnly
            | CacheEffectiveState::Overridden
            | CacheEffectiveState::Unavailable
    ) && !matches!(
        reason,
        CacheReason::NoMedia
            | CacheReason::RequestedOff
            | CacheReason::AwaitingMediaFacts
            | CacheReason::AutoUncachedSeek
            | CacheReason::OnEligibleMedia
            | CacheReason::MediaClosed
    )
}

fn activation_failure(reason: CacheReason) -> bool {
    !matches!(
        reason,
        CacheReason::UserRequestedOff | CacheReason::MediaClosed | CacheReason::RequestedOff
    )
}

fn latch_reason(reason: CacheReason) -> bool {
    matches!(
        reason,
        CacheReason::SoftCapReached
            | CacheReason::FreeSpaceFloor
            | CacheReason::WriteBudgetExhausted
    )
}

fn take_summary(state: &mut State, now_ms: u64, force: bool) -> Option<Snapshot> {
    if !state.dirty
        || (!force && now_ms.saturating_sub(state.last_summary_ms) < SUMMARY_INTERVAL_MS)
    {
        return None;
    }
    state.last_summary_ms = now_ms;
    state.dirty = false;
    Some(state.snapshot.clone())
}

fn log_summary(snapshot: &Snapshot) {
    tracing::info!(
        scrub_started = snapshot.scrub_started,
        scrub_cancelled = snapshot.scrub_cancelled,
        scrub_committed = snapshot.scrub_committed,
        interactive_admitted = snapshot.interactive_admitted,
        interactive_coalesced = snapshot.interactive_coalesced,
        interactive_dispatched = snapshot.interactive_dispatched,
        final_restart_count = snapshot.final_restart_count,
        final_restart_total_ms = snapshot.final_restart_total_ms,
        final_restart_max_ms = snapshot.final_restart_max_ms,
        stale_restarts = snapshot.stale_restarts,
        paused_for_cache_count = snapshot.paused_for_cache_count,
        paused_for_cache_total_ms = snapshot.paused_for_cache_total_ms,
        auto_candidates = snapshot.auto_candidates,
        auto_rejections = ?snapshot.auto_rejections,
        disk_activation_attempts = snapshot.disk_activation_attempts,
        disk_activation_successes = snapshot.disk_activation_successes,
        disk_activation_failures = snapshot.disk_activation_failures,
        file_cache_current_bytes = snapshot.file_cache_current_bytes,
        file_cache_peak_bytes = snapshot.file_cache_peak_bytes,
        cache_latches = ?snapshot.cache_latches,
        cleanup_count = snapshot.cleanup_count,
        cleanup_total_ms = snapshot.cleanup_total_ms,
        cleanup_max_ms = snapshot.cleanup_max_ms,
        source_recovery_attempts = snapshot.source_recovery_attempts,
        source_recovery_causes = ?snapshot.source_recovery_causes,
        source_recovery_outcomes = ?snapshot.source_recovery_outcomes,
        "long-form seek diagnostic summary"
    );
}

fn bump(value: &mut u64, delta: u64) {
    *value = value.saturating_add(delta);
}

fn bump_map(values: &mut BTreeMap<&'static str, u64>, key: &'static str) {
    bump(values.entry(key).or_default(), 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        requested: LongFormSeekOptimization,
        effective: CacheEffectiveState,
        reason: CacheReason,
        generation: Option<u64>,
        current: u64,
        peak: u64,
    ) -> CacheStatus {
        CacheStatus {
            requested,
            effective,
            reason,
            file_generation: generation,
            policy_revision: 1,
            file_cache_bytes: current,
            peak_file_cache_bytes: peak,
        }
    }

    fn snapshot(diagnostics: &ProductionDiagnostics) -> Snapshot {
        diagnostics
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    #[test]
    fn gesture_seek_restart_and_pause_counters_are_deterministic() {
        let diagnostics = ProductionDiagnostics::default();
        diagnostics.update_at(10, |state, _| {
            bump(&mut state.snapshot.scrub_started, 1);
            bump(&mut state.snapshot.scrub_cancelled, 1);
            bump(&mut state.snapshot.scrub_committed, 1);
            bump(&mut state.snapshot.interactive_admitted, 3);
            bump(&mut state.snapshot.interactive_coalesced, 1);
            bump(&mut state.snapshot.interactive_dispatched, 2);
            state.dirty = true;
        });
        diagnostics.update_at(125, |state, now| {
            record_paused_for_cache(state, Some(7), true, now);
        });
        diagnostics.update_at(200, |state, now| {
            record_paused_for_cache(state, Some(7), true, now);
        });
        diagnostics.update_at(425, |state, now| {
            record_paused_for_cache(state, Some(7), false, now);
            bump(&mut state.snapshot.final_restart_count, 1);
            bump(&mut state.snapshot.final_restart_total_ms, 300);
            state.snapshot.final_restart_max_ms = 300;
            bump(&mut state.snapshot.stale_restarts, 1);
        });

        let snapshot = snapshot(&diagnostics);
        assert_eq!(snapshot.scrub_started, 1);
        assert_eq!(snapshot.scrub_cancelled, 1);
        assert_eq!(snapshot.scrub_committed, 1);
        assert_eq!(snapshot.interactive_admitted, 3);
        assert_eq!(snapshot.interactive_coalesced, 1);
        assert_eq!(snapshot.interactive_dispatched, 2);
        assert_eq!(snapshot.final_restart_count, 1);
        assert_eq!(snapshot.final_restart_total_ms, 300);
        assert_eq!(snapshot.final_restart_max_ms, 300);
        assert_eq!(snapshot.stale_restarts, 1);
        assert_eq!(snapshot.paused_for_cache_count, 1);
        assert_eq!(snapshot.paused_for_cache_total_ms, 300);
    }

    #[test]
    fn cache_transitions_count_once_while_byte_samples_only_update_gauges() {
        let diagnostics = ProductionDiagnostics::default();
        diagnostics.update_at(0, |state, now| {
            record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::Auto,
                    CacheEffectiveState::RamOnly,
                    CacheReason::SequentialPlayback,
                    Some(4),
                    0,
                    0,
                ),
                now,
            );
        });
        diagnostics.update_at(10, |state, now| {
            record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::Auto,
                    CacheEffectiveState::Probing,
                    CacheReason::AutoUncachedSeek,
                    Some(4),
                    0,
                    0,
                ),
                now,
            );
        });
        diagnostics.update_at(20, |state, now| {
            record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::Auto,
                    CacheEffectiveState::EnablePending,
                    CacheReason::AutoUncachedSeek,
                    Some(4),
                    0,
                    0,
                ),
                now,
            );
        });
        diagnostics.update_at(30, |state, now| {
            record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::Auto,
                    CacheEffectiveState::DiskActive,
                    CacheReason::AutoUncachedSeek,
                    Some(4),
                    8,
                    8,
                ),
                now,
            );
        });
        diagnostics.update_at(40, |state, now| {
            assert!(!record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::Auto,
                    CacheEffectiveState::DiskActive,
                    CacheReason::AutoUncachedSeek,
                    Some(4),
                    16,
                    16,
                ),
                now,
            ));
        });
        diagnostics.update_at(50, |state, now| {
            record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::Auto,
                    CacheEffectiveState::DisablePending,
                    CacheReason::SoftCapReached,
                    Some(4),
                    16,
                    16,
                ),
                now,
            );
        });

        let snapshot = snapshot(&diagnostics);
        assert_eq!(snapshot.auto_candidates, 1);
        assert_eq!(snapshot.auto_rejections["sequential_playback"], 1);
        assert_eq!(snapshot.disk_activation_attempts, 1);
        assert_eq!(snapshot.disk_activation_successes, 1);
        assert_eq!(snapshot.disk_activation_failures, 0);
        assert_eq!(snapshot.file_cache_current_bytes, 16);
        assert_eq!(snapshot.file_cache_peak_bytes, 16);
        assert_eq!(snapshot.cache_latches["soft_cap_reached"], 1);
    }

    #[test]
    fn activation_failure_and_managed_cleanup_have_bounded_outcomes() {
        let diagnostics = ProductionDiagnostics::default();
        diagnostics.update_at(100, |state, now| {
            record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::On,
                    CacheEffectiveState::EnablePending,
                    CacheReason::OnEligibleMedia,
                    Some(9),
                    0,
                    0,
                ),
                now,
            );
        });
        diagnostics.update_at(120, |state, now| {
            record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::On,
                    CacheEffectiveState::DisablePending,
                    CacheReason::PropertyRejected,
                    Some(9),
                    0,
                    0,
                ),
                now,
            );
        });
        diagnostics.update_at(200, |state, now| {
            record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::On,
                    CacheEffectiveState::DisablePending,
                    CacheReason::MediaClosed,
                    Some(10),
                    64,
                    64,
                ),
                now,
            );
        });
        diagnostics.update_at(550, |state, now| {
            record_cache_status(
                state,
                status(
                    LongFormSeekOptimization::On,
                    CacheEffectiveState::NoMedia,
                    CacheReason::MediaClosed,
                    None,
                    0,
                    0,
                ),
                now,
            );
        });

        let snapshot = snapshot(&diagnostics);
        assert_eq!(snapshot.disk_activation_attempts, 1);
        assert_eq!(snapshot.disk_activation_failures, 1);
        assert_eq!(snapshot.cleanup_count, 1);
        assert_eq!(snapshot.cleanup_total_ms, 350);
        assert_eq!(snapshot.cleanup_max_ms, 350);
    }

    #[test]
    fn summaries_are_rate_bounded_and_reason_maps_are_finite_vocabularies() {
        let diagnostics = ProductionDiagnostics::default();
        let mut state = State::default();
        state.snapshot.scrub_started = 1;
        state.dirty = true;
        assert!(take_summary(&mut state, SUMMARY_INTERVAL_MS - 1, false).is_none());
        assert!(take_summary(&mut state, SUMMARY_INTERVAL_MS, false).is_some());
        state.dirty = true;
        assert!(take_summary(&mut state, SUMMARY_INTERVAL_MS + 1, false).is_none());
        assert!(take_summary(&mut state, SUMMARY_INTERVAL_MS + 1, true).is_some());

        diagnostics.update_at(1, |state, _| {
            bump_map(&mut state.snapshot.source_recovery_causes, "http_forbidden");
            bump_map(
                &mut state.snapshot.source_recovery_outcomes,
                SourceRecoveryOutcome::ResumeDispatched.id(),
            );
        });
        let snapshot = snapshot(&diagnostics);
        assert_eq!(snapshot.source_recovery_causes.len(), 1);
        assert_eq!(snapshot.source_recovery_outcomes.len(), 1);
    }
}
