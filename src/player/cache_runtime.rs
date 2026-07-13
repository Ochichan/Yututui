//! mpv-actor adapter for the pure long-form cache policy.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

use serde_json::Value;

use crate::config::LongFormSeekOptimization;

use super::MediaSourceContext;
use super::cache_budget::{
    PendingReservation, ReservationCompletion, ReservationPlan, RollingWriteBudget,
};
use super::cache_support::CacheSpawnSupport;
use super::long_form_seek::{
    ActiveStorageGuard, CACHE_SOFT_TARGET_BYTES, CacheAction, CacheEffectiveState, CacheReason,
    CacheStatus, DISABLE_DEADLINE_MS, ENABLE_DEADLINE_MS, FREE_SPACE_RESERVE_BYTES,
    LongFormCacheController, MIN_OVERSHOOT_BYTES, MediaFacts, PolicyToken, RangeProbe,
    RateEvidence, StorageBudget,
};

const POLICY_REQUEST_TIMEOUT_MS: u64 = ENABLE_DEADLINE_MS;
const RATE_SAMPLE_CAPACITY: usize = 16;
const WATCHDOG_POLL_PHASE_MS: u64 = 1_000;
const CONSERVATIVE_CONTROL_LATENCY_MS: u64 =
    WATCHDOG_POLL_PHASE_MS.saturating_add(DISABLE_DEADLINE_MS);
const VOLUME_SPACE_REFRESH_MS: u64 = 5_000;
const VOLUME_SPACE_NEAR_RESERVE_REFRESH_MS: u64 = 1_000;

struct OwnerSessionAccounting {
    written_bytes: AtomicU64,
    worst_control_latency_ms: AtomicU64,
}

impl OwnerSessionAccounting {
    fn new() -> Self {
        Self {
            written_bytes: AtomicU64::new(0),
            // Until a longer real loop is observed, reserve for one full poll phase plus the
            // controller's declared disable deadline. A guessed one-second loop is not safe.
            worst_control_latency_ms: AtomicU64::new(CONSERVATIVE_CONTROL_LATENCY_MS),
        }
    }

    fn add_written(&self, delta: u64) -> u64 {
        self.written_bytes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.saturating_add(delta))
            })
            .unwrap_or_else(|current| current)
            .saturating_add(delta)
    }

    fn written(&self) -> u64 {
        self.written_bytes.load(Ordering::SeqCst)
    }

    fn observe_control_latency(&self, latency_ms: u64) {
        self.worst_control_latency_ms
            .fetch_max(latency_ms, Ordering::SeqCst);
    }

    fn control_latency_ms(&self) -> u64 {
        self.worst_control_latency_ms.load(Ordering::SeqCst)
    }
}

fn process_session_accounting() -> Arc<OwnerSessionAccounting> {
    static ACCOUNTING: OnceLock<Arc<OwnerSessionAccounting>> = OnceLock::new();
    Arc::clone(ACCOUNTING.get_or_init(|| Arc::new(OwnerSessionAccounting::new())))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PendingKind {
    Ranges(RangeProbe),
    Set { token: PolicyToken, enabled: bool },
    ReadBack { token: PolicyToken, expected: bool },
    CacheState,
    CacheSpeed,
}

impl PendingKind {
    fn rebase_file_generation(&mut self, previous: u64, generation: u64) {
        match self {
            Self::Ranges(probe) => probe.token.rebase_file_generation(previous, generation),
            Self::Set { token, .. } | Self::ReadBack { token, .. } => {
                token.rebase_file_generation(previous, generation);
            }
            Self::CacheState | Self::CacheSpeed => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PendingRequest {
    kind: PendingKind,
    deadline_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RateSample {
    at_ms: u64,
    bytes_per_sec: u64,
}

/// Single-process runtime facts and correlated policy requests. The actor remains the only owner.
pub(crate) struct CacheRuntime {
    started: Instant,
    controller: LongFormCacheController,
    cache_dir: Option<PathBuf>,
    facts: MediaFacts,
    pending: HashMap<u64, PendingRequest>,
    cache_speed: VecDeque<RateSample>,
    raw_input_rate: VecDeque<RateSample>,
    file_rate: VecDeque<RateSample>,
    last_file_sample: Option<(u64, u64)>,
    fixture_max_bytes_per_sec: Option<u64>,
    session: Arc<OwnerSessionAccounting>,
    disable_control_started_ms: Option<u64>,
    rolling_written_bytes: u64,
    rolling_budget: RollingWriteBudget,
    deferred_cache_enable: Option<CacheAction>,
    persistence_results: Arc<Mutex<VecDeque<ReservationCompletion>>>,
    last_emitted_status: Option<CacheStatus>,
    volume_space_cache: Cell<Option<(u64, Option<crate::util::safe_fs::VolumeSpace>)>>,
    #[cfg(test)]
    volume_space_override: Option<crate::util::safe_fs::VolumeSpace>,
    #[cfg(test)]
    volume_space_probe_count: Cell<u64>,
}

impl CacheRuntime {
    /// Production cache actors in one owner process share cumulative write and actuation facts,
    /// including across a safety recycle. A recycle must not reset the one-session budget.
    pub(crate) fn for_owner_process(
        support: CacheSpawnSupport,
        requested: LongFormSeekOptimization,
    ) -> Self {
        Self::with_session(
            support,
            requested,
            process_session_accounting(),
            benchmark_fixture_rate_bound(),
        )
    }

    #[cfg(test)]
    pub(crate) fn new(support: CacheSpawnSupport, requested: LongFormSeekOptimization) -> Self {
        Self::with_session(
            support,
            requested,
            Arc::new(OwnerSessionAccounting::new()),
            None,
        )
    }

    fn with_session(
        support: CacheSpawnSupport,
        requested: LongFormSeekOptimization,
        session: Arc<OwnerSessionAccounting>,
        fixture_max_bytes_per_sec: Option<u64>,
    ) -> Self {
        let rolling_budget = RollingWriteBudget::open(support.cache_dir.as_deref());
        let rolling_written_bytes = rolling_budget.bytes();
        Self {
            started: Instant::now(),
            controller: LongFormCacheController::new(support.capability, requested),
            cache_dir: support.cache_dir,
            facts: MediaFacts::default(),
            pending: HashMap::new(),
            cache_speed: VecDeque::new(),
            raw_input_rate: VecDeque::new(),
            file_rate: VecDeque::new(),
            last_file_sample: None,
            fixture_max_bytes_per_sec,
            session,
            disable_control_started_ms: None,
            rolling_written_bytes,
            rolling_budget,
            deferred_cache_enable: None,
            persistence_results: Arc::new(Mutex::new(VecDeque::new())),
            last_emitted_status: None,
            volume_space_cache: Cell::new(None),
            #[cfg(test)]
            volume_space_override: None,
            #[cfg(test)]
            volume_space_probe_count: Cell::new(0),
        }
    }

    pub(crate) fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn status(&self) -> CacheStatus {
        self.controller.status()
    }

    /// Re-identify the still-open physical media after a same-item recovery load is cancelled
    /// before replacement preparation begins. Process-global cache work remains ordered, so every
    /// queued and in-flight policy token must cross the alias boundary together.
    pub(crate) fn rebase_file_generation(&mut self, previous: u64, generation: u64) {
        if !self.controller.rebase_file_generation(previous, generation) {
            return;
        }
        for pending in self.pending.values_mut() {
            pending.kind.rebase_file_generation(previous, generation);
        }
        if let Some(action) = self.deferred_cache_enable.as_mut() {
            action.rebase_file_generation(previous, generation);
        }
        self.last_emitted_status = None;
    }

    #[cfg(test)]
    pub(crate) fn test_force_disk_active(&mut self, generation: u64) {
        let facts = MediaFacts {
            duration_secs: Some(super::long_form_seek::LONG_FORM_MIN_DURATION_SECS),
            via_network: Some(true),
            seekable: Some(true),
            partially_seekable: Some(false),
            live: false,
        };
        let budget = StorageBudget {
            available_bytes: 8 * 1024 * 1024 * 1024,
            volume_bytes: 100 * 1024 * 1024 * 1024,
            session_written_bytes: 0,
            rolling_written_bytes: 0,
            control_latency_ms: 1_000,
            rate: RateEvidence {
                fixture_max_bytes_per_sec: Some(1024 * 1024),
                ..RateEvidence::default()
            },
        };
        let CacheAction::SetCacheOnDisk { token, .. } = self
            .controller
            .begin_media(generation, facts, 0, Some(budget))
            .expect("eligible test media begins cache activation")
        else {
            panic!("test cache activation must issue a true write")
        };
        assert!(matches!(
            self.controller.set_reply(token, true, true, 1),
            Some(CacheAction::ReadBackCacheOnDisk { .. })
        ));
        assert!(
            self.controller
                .readback_reply(token, true, Some(true), 2)
                .is_none()
        );
        let admission = budget.admission().expect("test storage budget is safe");
        assert!(
            self.controller
                .file_cache_sample(
                    1,
                    3,
                    Ok(ActiveStorageGuard {
                        available_bytes: budget.available_bytes,
                        admission,
                    }),
                    1,
                    1,
                )
                .is_none()
        );
        self.facts = facts;
        self.last_emitted_status = None;
        assert_eq!(
            self.controller.status().effective,
            CacheEffectiveState::DiskActive
        );
    }

    pub(crate) fn force_next_media_ram_only(&mut self) {
        self.controller.force_next_media_ram_only();
    }

    pub(crate) fn take_changed_status(&mut self) -> Option<CacheStatus> {
        let status = self.controller.status();
        if self.last_emitted_status == Some(status) {
            None
        } else {
            self.last_emitted_status = Some(status);
            Some(status)
        }
    }

    pub(crate) fn begin_media(
        &mut self,
        generation: u64,
        source_context: MediaSourceContext,
    ) -> Option<CacheAction> {
        self.pending.clear();
        self.invalidate_volume_space();
        self.facts = MediaFacts {
            live: source_context.is_live(),
            ..MediaFacts::default()
        };
        self.cache_speed.clear();
        self.raw_input_rate.clear();
        self.file_rate.clear();
        self.last_file_sample = None;
        let now = self.now_ms();
        let budget = self.storage_budget();
        self.controller
            .begin_media(generation, self.facts, now, budget)
    }

    pub(crate) fn close_media(&mut self) {
        self.pending.clear();
        self.invalidate_volume_space();
        self.controller.close_media();
        self.finish_disable_measurement();
        self.facts = MediaFacts::default();
        self.last_file_sample = None;
    }

    pub(crate) fn observe_duration(&mut self, duration: Option<f64>) -> Option<CacheAction> {
        self.facts.duration_secs = duration;
        self.update_facts()
    }

    pub(crate) fn observe_network(&mut self, via_network: Option<bool>) -> Option<CacheAction> {
        self.facts.via_network = via_network;
        self.update_facts()
    }

    pub(crate) fn observe_seekable(&mut self, seekable: Option<bool>) -> Option<CacheAction> {
        self.facts.seekable = seekable;
        self.update_facts()
    }

    pub(crate) fn observe_partially_seekable(
        &mut self,
        partially_seekable: Option<bool>,
    ) -> Option<CacheAction> {
        self.facts.partially_seekable = partially_seekable;
        self.update_facts()
    }

    pub(crate) fn observe_transport(&mut self, position_secs: f64, paused: bool) {
        self.controller.observe_transport(position_secs, paused);
    }

    pub(crate) fn before_interactive_seek(
        &mut self,
        from_secs: f64,
        target_secs: f64,
    ) -> Option<CacheAction> {
        let action =
            self.controller
                .committed_interactive_seek(from_secs, target_secs, self.now_ms());
        if matches!(action, Some(CacheAction::QueryRanges(_))) {
            // A possible Auto admission must not use a normal-cadence snapshot from before the
            // user committed this seek. The later correlated range reply performs a fresh probe.
            self.invalidate_volume_space();
        }
        action
    }

    pub(crate) fn interactive_seek_succeeded(&mut self, target_secs: f64) -> Option<CacheAction> {
        let now = self.now_ms();
        let budget = self
            .controller
            .off_cache_seek_completion_needs_budget(target_secs)
            .then(|| self.storage_budget())
            .flatten();
        self.controller
            .mark_off_cache_seek_succeeded(target_secs, budget, now)
    }

    pub(crate) fn update_requested(
        &mut self,
        requested: LongFormSeekOptimization,
    ) -> Option<CacheAction> {
        self.invalidate_volume_space();
        let now = self.now_ms();
        let budget = self.storage_budget();
        self.controller.update_requested(requested, now, budget)
    }

    pub(crate) fn prepare_replacement(&mut self) -> Option<CacheAction> {
        self.controller.prepare_replacement(self.now_ms())
    }

    pub(crate) fn observe_cache_speed(
        &mut self,
        bytes_per_sec: Option<u64>,
    ) -> Option<CacheAction> {
        let now_ms = self.now_ms();
        self.observe_cache_speed_at(bytes_per_sec, now_ms)
    }

    fn observe_cache_speed_at(
        &mut self,
        bytes_per_sec: Option<u64>,
        now_ms: u64,
    ) -> Option<CacheAction> {
        if let Some(rate) = bytes_per_sec {
            push_rate(&mut self.cache_speed, now_ms, rate);
        }
        self.update_facts()
    }

    pub(crate) fn observe_raw_input_rate(
        &mut self,
        bytes_per_sec: Option<u64>,
    ) -> Option<CacheAction> {
        if let Some(rate) = bytes_per_sec {
            let now = self.now_ms();
            push_rate(&mut self.raw_input_rate, now, rate);
        }
        self.update_facts()
    }

    pub(crate) fn observe_file_cache_bytes(&mut self, bytes: u64) -> Option<CacheAction> {
        let now = self.now_ms();
        let managed_lifecycle = matches!(
            self.controller.status().effective,
            CacheEffectiveState::EnablePending
                | CacheEffectiveState::DiskActive
                | CacheEffectiveState::DisablePending
                | CacheEffectiveState::LatchedUntilClose
        );
        let delta = self
            .last_file_sample
            .map_or(bytes, |(_, last_bytes)| bytes.saturating_sub(last_bytes));
        if managed_lifecycle && delta > 0 {
            self.session.add_written(delta);
            self.rolling_written_bytes = self.rolling_budget.record_observed(delta);
        }
        if managed_lifecycle
            && let Some((last_at, last_bytes)) = self.last_file_sample
            && now > last_at
            && bytes > last_bytes
        {
            let rate = (u128::from(delta) * 1_000) / u128::from(now - last_at);
            push_rate(
                &mut self.file_rate,
                now,
                u64::try_from(rate).unwrap_or(u64::MAX),
            );
        }
        self.last_file_sample = Some((now, bytes));
        let storage = self.active_storage_guard();
        let action = self.controller.file_cache_sample(
            bytes,
            now,
            storage,
            self.session.written(),
            self.rolling_written_bytes,
        );
        self.finish_disable_measurement();
        action
    }

    pub(crate) fn register(&mut self, actor_request_id: u64, action: CacheAction) {
        let kind = match action {
            CacheAction::QueryRanges(probe) => PendingKind::Ranges(probe),
            CacheAction::SetCacheOnDisk { token, enabled } => PendingKind::Set { token, enabled },
            CacheAction::ReadBackCacheOnDisk { token, expected } => {
                PendingKind::ReadBack { token, expected }
            }
            CacheAction::EmergencyCloseAndResume { .. } => return,
        };
        if matches!(kind, PendingKind::Set { enabled: false, .. })
            && self.disable_control_started_ms.is_none()
        {
            self.disable_control_started_ms = Some(self.now_ms());
        }
        self.pending.insert(
            actor_request_id,
            PendingRequest {
                kind,
                deadline_ms: self.now_ms().saturating_add(POLICY_REQUEST_TIMEOUT_MS),
            },
        );
    }

    /// Gate the process-global true write on a durable, whole-lifetime reservation. Preparing the
    /// immutable snapshot is CPU-only; the atomic write runs on Tokio's blocking pool and the
    /// actor observes its ACK through [`Self::poll_persistence`].
    pub(crate) fn prepare_cache_action(&mut self, action: CacheAction) -> Option<CacheAction> {
        let (ready, persistence) = self.prepare_cache_action_inner(action);
        if let Some(persistence) = persistence {
            let results = Arc::clone(&self.persistence_results);
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    let _task = handle.spawn_blocking(move || {
                        let completion = persistence.persist();
                        results
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push_back(completion);
                    });
                }
                Err(_) => {
                    let completion = persistence.completion_without_runtime();
                    self.persistence_results
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push_back(completion);
                }
            }
        }
        ready
    }

    fn prepare_cache_action_inner(
        &mut self,
        action: CacheAction,
    ) -> (Option<CacheAction>, Option<PendingReservation>) {
        if !matches!(action, CacheAction::SetCacheOnDisk { enabled: true, .. }) {
            return (Some(action), None);
        }
        let required = self
            .storage_budget()
            .and_then(|budget| budget.admission().ok())
            .map(|admission| CACHE_SOFT_TARGET_BYTES.saturating_add(admission.overshoot_bytes));
        let Some(required) = required else {
            return (self.persistence_failure_action(), None);
        };
        match self.rolling_budget.prepare_reservation(required) {
            Ok(ReservationPlan::Covered) => (Some(action), None),
            Ok(ReservationPlan::Persist(persistence)) => {
                self.rolling_written_bytes = self.rolling_budget.bytes();
                self.deferred_cache_enable = Some(action);
                (None, Some(persistence))
            }
            Err(_) => (self.persistence_failure_action(), None),
        }
    }

    pub(crate) fn poll_persistence(&mut self) -> Option<CacheAction> {
        let completion = self
            .persistence_results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()?;
        self.complete_persistence(completion)
    }

    fn complete_persistence(&mut self, completion: ReservationCompletion) -> Option<CacheAction> {
        let persisted = self.rolling_budget.complete_reservation(completion);
        self.rolling_written_bytes = self.rolling_budget.bytes();
        let deferred = self.deferred_cache_enable.take();
        if !persisted {
            return self.persistence_failure_action();
        }
        deferred.filter(|action| match action {
            CacheAction::SetCacheOnDisk {
                token,
                enabled: true,
            } => {
                let status = self.controller.status();
                status.effective == CacheEffectiveState::EnablePending
                    && status.file_generation == Some(token.file_generation)
                    && status.policy_revision == token.policy_revision
            }
            _ => false,
        })
    }

    fn persistence_failure_action(&mut self) -> Option<CacheAction> {
        self.deferred_cache_enable = None;
        self.rolling_written_bytes = super::long_form_seek::ROLLING_WRITE_BUDGET_BYTES;
        self.controller
            .active_watchdog_failure(self.now_ms(), CacheReason::WriteBudgetExhausted)
    }

    pub(crate) fn register_file_bytes_query(&mut self, actor_request_id: u64) -> bool {
        if self
            .pending
            .values()
            .any(|pending| pending.kind == PendingKind::CacheState)
        {
            return false;
        }
        self.pending.insert(
            actor_request_id,
            PendingRequest {
                kind: PendingKind::CacheState,
                deadline_ms: self.now_ms().saturating_add(POLICY_REQUEST_TIMEOUT_MS),
            },
        );
        true
    }

    pub(crate) fn register_cache_speed_query(&mut self, actor_request_id: u64) -> bool {
        if self
            .pending
            .values()
            .any(|pending| pending.kind == PendingKind::CacheSpeed)
        {
            return false;
        }
        self.pending.insert(
            actor_request_id,
            PendingRequest {
                kind: PendingKind::CacheSpeed,
                deadline_ms: self.now_ms().saturating_add(POLICY_REQUEST_TIMEOUT_MS),
            },
        );
        true
    }

    pub(crate) fn handle_reply(
        &mut self,
        actor_request_id: u64,
        accepted: bool,
        data: Option<&Value>,
    ) -> Option<CacheAction> {
        let pending = self.pending.remove(&actor_request_id)?;
        let now = self.now_ms();
        let action = match pending.kind {
            PendingKind::Ranges(probe) => {
                if let Some(state) = accepted.then_some(data).flatten() {
                    self.observe_cache_state_rate(state);
                }
                self.controller.range_reply(
                    probe,
                    accepted.then_some(data).flatten().ok_or(()),
                    self.storage_budget(),
                    now,
                )
            }
            PendingKind::Set { token, enabled } => {
                self.controller.set_reply(token, enabled, accepted, now)
            }
            PendingKind::ReadBack { token, expected } => self.controller.readback_reply(
                token,
                expected,
                accepted.then_some(data).flatten().and_then(Value::as_bool),
                now,
            ),
            PendingKind::CacheState => match accepted.then_some(data).flatten() {
                Some(state) => {
                    self.observe_cache_state_rate(state);
                    match node_u64(state, "file-cache-bytes") {
                        Some(bytes) => self.observe_file_cache_bytes(bytes),
                        None => self
                            .controller
                            .active_watchdog_failure(now, CacheReason::PropertyVerificationFailed),
                    }
                }
                None => self
                    .controller
                    .active_watchdog_failure(now, CacheReason::PropertyVerificationFailed),
            },
            PendingKind::CacheSpeed => accepted
                .then_some(data)
                .flatten()
                .and_then(value_u64)
                .and_then(|rate| self.observe_cache_speed(Some(rate))),
        };
        self.finish_disable_measurement();
        action
    }

    pub(crate) fn expire_requests(&mut self) -> Vec<CacheAction> {
        let now = self.now_ms();
        let expired = self
            .pending
            .iter()
            .filter_map(|(request_id, pending)| {
                (now >= pending.deadline_ms).then_some((*request_id, pending.kind))
            })
            .collect::<Vec<_>>();
        let mut actions = Vec::new();
        for (request_id, kind) in expired {
            self.pending.remove(&request_id);
            let action = match kind {
                PendingKind::Ranges(probe) => {
                    self.controller
                        .range_reply(probe, Err(()), self.storage_budget(), now)
                }
                PendingKind::Set { token, enabled }
                | PendingKind::ReadBack {
                    token,
                    expected: enabled,
                } => self.controller.property_timeout(token, enabled, now),
                PendingKind::CacheState => self
                    .controller
                    .active_watchdog_failure(now, CacheReason::PropertyTimeout),
                PendingKind::CacheSpeed => None,
            };
            if let Some(action) = action {
                actions.push(action);
            }
        }
        if let Some(action) = self.controller.tick(now) {
            actions.push(action);
        }
        self.finish_disable_measurement();
        actions
    }

    pub(crate) fn watchdog_needed(&self) -> bool {
        matches!(
            self.controller.status().effective,
            CacheEffectiveState::EnablePending
                | CacheEffectiveState::DiskActive
                | CacheEffectiveState::DisablePending
        )
    }

    pub(crate) fn rate_sampling_needed(&self) -> bool {
        let status = self.controller.status();
        status.file_generation.is_some()
            && status.requested != LongFormSeekOptimization::Off
            && matches!(
                status.effective,
                CacheEffectiveState::RamOnly | CacheEffectiveState::Probing
            )
    }

    fn update_facts(&mut self) -> Option<CacheAction> {
        let now = self.now_ms();
        if matches!(
            self.controller.status().effective,
            CacheEffectiveState::EnablePending | CacheEffectiveState::DiskActive
        ) && let Err(reason) = self.active_storage_guard()
        {
            return self.controller.active_watchdog_failure(now, reason);
        }
        let budget = self.storage_budget();
        self.controller.update_media_facts(self.facts, now, budget)
    }

    fn observe_cache_state_rate(&mut self, state: &Value) {
        if let Some(rate) = node_u64(state, "raw-input-rate") {
            let now = self.now_ms();
            push_rate(&mut self.raw_input_rate, now, rate);
        }
    }

    fn storage_budget(&self) -> Option<StorageBudget> {
        let space = self.volume_space()?;
        Some(StorageBudget {
            available_bytes: space.available_bytes,
            volume_bytes: space.total_bytes,
            session_written_bytes: self.session.written(),
            rolling_written_bytes: self.rolling_written_bytes,
            control_latency_ms: self.session.control_latency_ms(),
            rate: RateEvidence {
                fixture_max_bytes_per_sec: self.fixture_max_bytes_per_sec,
                measured_file_delta_bytes_per_sec: proven_rate(&self.file_rate),
                cache_speed_bytes_per_sec: proven_rate(&self.cache_speed),
                raw_input_rate_bytes_per_sec: proven_rate(&self.raw_input_rate),
            },
        })
    }

    fn active_storage_guard(&self) -> Result<ActiveStorageGuard, CacheReason> {
        let budget = self
            .storage_budget()
            .ok_or(CacheReason::CacheRootUnavailable)?;
        budget.active_guard()
    }

    fn volume_space(&self) -> Option<crate::util::safe_fs::VolumeSpace> {
        self.volume_space_at(self.now_ms())
    }

    fn volume_space_at(&self, now_ms: u64) -> Option<crate::util::safe_fs::VolumeSpace> {
        if let Some((sampled_ms, cached)) = self.volume_space_cache.get() {
            let refresh_ms = cached.map_or(VOLUME_SPACE_NEAR_RESERVE_REFRESH_MS, |space| {
                let reserve =
                    FREE_SPACE_RESERVE_BYTES.max(space.total_bytes.saturating_add(19) / 20);
                let near_reserve = reserve
                    .saturating_add(CACHE_SOFT_TARGET_BYTES)
                    .saturating_add(MIN_OVERSHOOT_BYTES.saturating_mul(2));
                if space.available_bytes <= near_reserve {
                    VOLUME_SPACE_NEAR_RESERVE_REFRESH_MS
                } else {
                    VOLUME_SPACE_REFRESH_MS
                }
            });
            if now_ms.saturating_sub(sampled_ms) < refresh_ms {
                return cached;
            }
        }
        #[cfg(test)]
        self.volume_space_probe_count
            .set(self.volume_space_probe_count.get().saturating_add(1));
        #[cfg(test)]
        let sampled = self.volume_space_override.or_else(|| {
            self.cache_dir
                .as_deref()
                .and_then(|path| crate::util::safe_fs::volume_space(path).ok())
        });
        #[cfg(not(test))]
        let sampled = self
            .cache_dir
            .as_deref()
            .and_then(|path| crate::util::safe_fs::volume_space(path).ok());
        self.volume_space_cache.set(Some((now_ms, sampled)));
        sampled
    }

    fn invalidate_volume_space(&self) {
        self.volume_space_cache.set(None);
    }

    fn finish_disable_measurement(&mut self) {
        if self.controller.status().effective == CacheEffectiveState::DisablePending {
            return;
        }
        if let Some(started_ms) = self.disable_control_started_ms.take() {
            self.session
                .observe_control_latency(self.now_ms().saturating_sub(started_ms));
        }
    }
}

fn node_u64(state: &Value, name: &str) -> Option<u64> {
    value_u64(state.get(name)?)
}

fn value_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let number = value.as_f64()?;
        (number.is_finite() && number >= 0.0 && number <= u64::MAX as f64)
            .then_some(number.round() as u64)
    })
}

fn push_rate(samples: &mut VecDeque<RateSample>, at_ms: u64, bytes_per_sec: u64) {
    if samples.len() == RATE_SAMPLE_CAPACITY {
        samples.pop_front();
    }
    samples.push_back(RateSample {
        at_ms,
        bytes_per_sec,
    });
}

/// The performance harness owns a loopback server with an enforced delivery ceiling. Ordinary
/// playback has no such hard source-rate bound and therefore stays fail-closed for disk-cache
/// admission until a measured production bound or an actual limiter is implemented.
#[cfg(feature = "perf-harness")]
fn benchmark_fixture_rate_bound() -> Option<u64> {
    let scenario = std::env::var("TUI_PERF_SCENARIO_SHA256").ok();
    let rate = std::env::var("YTM_PERF_SOURCE_RATE_BOUND_BPS").ok();
    benchmark_fixture_rate_bound_from(scenario.as_deref(), rate.as_deref())
}

#[cfg(not(feature = "perf-harness"))]
fn benchmark_fixture_rate_bound() -> Option<u64> {
    None
}

#[cfg(feature = "perf-harness")]
fn benchmark_fixture_rate_bound_from(scenario: Option<&str>, rate: Option<&str>) -> Option<u64> {
    let scenario = scenario?;
    if scenario.len() != 64 || !scenario.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    rate?.parse::<u64>().ok().filter(|value| *value > 0)
}

#[cfg(all(test, not(feature = "perf-harness")))]
fn benchmark_fixture_rate_bound_from(_scenario: Option<&str>, _rate: Option<&str>) -> Option<u64> {
    None
}

/// A property becomes admission evidence only after three positive samples spanning at least one
/// second. Zero means "no rate observed yet", not a proven zero-cost upper bound.
fn proven_rate(samples: &VecDeque<RateSample>) -> Option<u64> {
    let mut positive = samples.iter().filter(|sample| sample.bytes_per_sec > 0);
    let first = positive.next()?;
    let mut count = 1usize;
    let mut last_at = first.at_ms;
    let mut maximum = first.bytes_per_sec;
    for sample in positive {
        count += 1;
        last_at = sample.at_ms;
        maximum = maximum.max(sample.bytes_per_sec);
    }
    if count < 3 || last_at.saturating_sub(first.at_ms) < 1_000 {
        return None;
    }
    Some(maximum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_CACHE_DIR: AtomicU64 = AtomicU64::new(0);

    fn support_without_root() -> CacheSpawnSupport {
        CacheSpawnSupport {
            capability: super::super::long_form_seek::ControllerCapability::Available(
                super::super::long_form_seek::CacheOptionFamily::Modern,
            ),
            option_family: Some(super::super::long_form_seek::CacheOptionFamily::Modern),
            override_source: None,
            cache_dir: None,
            spawn_args: Vec::new(),
        }
    }

    fn support_with_root(label: &str) -> (CacheSpawnSupport, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "ytt-cache-runtime-{label}-{}-{}",
            std::process::id(),
            NEXT_CACHE_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        crate::util::safe_fs::ensure_private_dir(&path).unwrap();
        (
            CacheSpawnSupport {
                capability: super::super::long_form_seek::ControllerCapability::Available(
                    super::super::long_form_seek::CacheOptionFamily::Modern,
                ),
                option_family: Some(super::super::long_form_seek::CacheOptionFamily::Modern),
                override_source: None,
                cache_dir: Some(path.clone()),
                spawn_args: Vec::new(),
            },
            path,
        )
    }

    fn eligible_runtime_with_root(label: &str) -> (CacheRuntime, PathBuf, CacheAction) {
        let (support, path) = support_with_root(label);
        let mut runtime = CacheRuntime::with_session(
            support,
            LongFormSeekOptimization::On,
            Arc::new(OwnerSessionAccounting::new()),
            Some(1024 * 1024),
        );
        runtime.volume_space_override = Some(crate::util::safe_fs::VolumeSpace {
            available_bytes: 8 * 1024 * 1024 * 1024,
            total_bytes: 100 * 1024 * 1024 * 1024,
        });
        assert!(
            runtime
                .begin_media(1, MediaSourceContext::OnDemand)
                .is_none()
        );
        assert!(runtime.observe_duration(Some(3_600.0)).is_none());
        assert!(runtime.observe_network(Some(true)).is_none());
        assert!(runtime.observe_seekable(Some(true)).is_none());
        let action = runtime
            .observe_partially_seekable(Some(false))
            .expect("eligible On media requests activation");
        (runtime, path, action)
    }

    fn eligible_facts() -> MediaFacts {
        MediaFacts {
            duration_secs: Some(super::super::long_form_seek::LONG_FORM_MIN_DURATION_SECS),
            via_network: Some(true),
            seekable: Some(true),
            partially_seekable: Some(false),
            live: false,
        }
    }

    fn budget() -> StorageBudget {
        StorageBudget {
            available_bytes: 8 * 1024 * 1024 * 1024,
            volume_bytes: 100 * 1024 * 1024 * 1024,
            session_written_bytes: 0,
            rolling_written_bytes: 0,
            control_latency_ms: 1_000,
            rate: RateEvidence {
                fixture_max_bytes_per_sec: Some(1024 * 1024),
                ..RateEvidence::default()
            },
        }
    }

    fn production_rate_runtime(requested: LongFormSeekOptimization) -> CacheRuntime {
        let mut runtime = CacheRuntime::new(support_without_root(), requested);
        runtime.rolling_written_bytes = 0;
        runtime.volume_space_override = Some(crate::util::safe_fs::VolumeSpace {
            available_bytes: 8 * 1024 * 1024 * 1024,
            total_bytes: 100 * 1024 * 1024 * 1024,
        });
        assert!(
            runtime
                .begin_media(1, MediaSourceContext::OnDemand)
                .is_none()
        );
        assert!(runtime.observe_duration(Some(3_600.0)).is_none());
        assert!(runtime.observe_network(Some(true)).is_none());
        assert!(runtime.observe_seekable(Some(true)).is_none());
        assert!(runtime.observe_partially_seekable(Some(false)).is_none());
        runtime
    }

    #[test]
    fn rate_evidence_requires_three_samples_over_one_second() {
        let mut samples = VecDeque::new();
        push_rate(&mut samples, 0, 10);
        push_rate(&mut samples, 500, 20);
        assert_eq!(proven_rate(&samples), None);
        push_rate(&mut samples, 999, 30);
        assert_eq!(proven_rate(&samples), None);
        push_rate(&mut samples, 1_000, 25);
        assert_eq!(proven_rate(&samples), Some(30));
    }

    #[test]
    fn pre_activation_cache_speed_query_is_bounded_and_collects_numeric_evidence() {
        let mut runtime = CacheRuntime::new(support_without_root(), LongFormSeekOptimization::On);
        runtime.controller.begin_media(1, eligible_facts(), 0, None);
        assert!(runtime.rate_sampling_needed());
        assert!(runtime.register_cache_speed_query(91));
        assert!(!runtime.register_cache_speed_query(92));
        assert!(
            runtime
                .handle_reply(91, true, Some(&serde_json::json!(24_000.0)))
                .is_none()
        );
        assert_eq!(
            runtime
                .cache_speed
                .back()
                .map(|sample| sample.bytes_per_sec),
            Some(24_000)
        );
        assert!(runtime.register_cache_speed_query(93));
        assert!(runtime.handle_reply(93, false, None).is_none());
        assert!(runtime.rate_sampling_needed());
    }

    #[test]
    fn default_feature_on_activates_only_after_proven_runtime_cache_speed() {
        let mut runtime = production_rate_runtime(LongFormSeekOptimization::On);
        assert_eq!(runtime.status().reason, CacheReason::UnsafeRateBound);
        assert!(runtime.observe_cache_speed_at(Some(2_000_000), 0).is_none());
        assert!(
            runtime
                .observe_cache_speed_at(Some(2_500_000), 500)
                .is_none()
        );
        assert_eq!(runtime.status().reason, CacheReason::UnsafeRateBound);
        assert!(matches!(
            runtime.observe_cache_speed_at(Some(2_250_000), 1_000),
            Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
        ));
    }

    #[test]
    fn default_feature_auto_probe_uses_proven_runtime_cache_speed() {
        let mut runtime = production_rate_runtime(LongFormSeekOptimization::Auto);
        for (at_ms, rate) in [(0, 2_000_000), (500, 2_500_000), (1_000, 2_250_000)] {
            assert!(runtime.observe_cache_speed_at(Some(rate), at_ms).is_none());
        }
        let probe = runtime
            .before_interactive_seek(0.0, 900.0)
            .expect("eligible large Auto seek probes ranges");
        assert!(matches!(probe, CacheAction::QueryRanges(_)));
        runtime.register(77, probe);
        assert!(matches!(
            runtime.handle_reply(77, true, Some(&serde_json::json!({"seekable-ranges": []})),),
            Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
        ));
    }

    #[cfg(feature = "perf-harness")]
    #[test]
    fn perf_harness_build_accepts_only_a_bound_tied_to_a_scenario_digest() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            benchmark_fixture_rate_bound_from(Some(digest), Some("24000")),
            Some(24_000)
        );
        assert_eq!(
            benchmark_fixture_rate_bound_from(Some("not-a-digest"), Some("24000")),
            None
        );
        assert_eq!(
            benchmark_fixture_rate_bound_from(Some(digest), Some("0")),
            None
        );
    }

    #[cfg(not(feature = "perf-harness"))]
    #[test]
    fn default_build_ignores_even_well_formed_harness_escape_inputs() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            benchmark_fixture_rate_bound_from(Some(digest), Some("24000")),
            None
        );
        assert_eq!(benchmark_fixture_rate_bound(), None);
    }

    #[test]
    fn zero_samples_do_not_prove_a_safe_rate_before_a_later_burst() {
        let mut samples = VecDeque::new();
        push_rate(&mut samples, 0, 0);
        push_rate(&mut samples, 500, 0);
        push_rate(&mut samples, 1_000, 0);
        assert_eq!(proven_rate(&samples), None);

        push_rate(&mut samples, 1_001, 8 * 1024 * 1024);
        push_rate(&mut samples, 1_501, 16 * 1024 * 1024);
        assert_eq!(proven_rate(&samples), None);
        push_rate(&mut samples, 2_001, 12 * 1024 * 1024);
        assert_eq!(proven_rate(&samples), Some(16 * 1024 * 1024));
    }

    #[test]
    fn first_positive_sample_and_recycled_actor_share_the_session_budget() {
        let session = Arc::new(OwnerSessionAccounting::new());
        let mut first = CacheRuntime::with_session(
            support_without_root(),
            LongFormSeekOptimization::On,
            Arc::clone(&session),
            None,
        );
        assert!(matches!(
            first
                .controller
                .begin_media(1, eligible_facts(), 0, Some(budget())),
            Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
        ));
        first.observe_file_cache_bytes(4_096);
        assert_eq!(session.written(), 4_096);

        let mut recycled = CacheRuntime::with_session(
            support_without_root(),
            LongFormSeekOptimization::On,
            Arc::clone(&session),
            None,
        );
        assert!(matches!(
            recycled
                .controller
                .begin_media(2, eligible_facts(), 0, Some(budget())),
            Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
        ));
        recycled.observe_file_cache_bytes(2_048);
        assert_eq!(session.written(), 6_144);
    }

    #[test]
    fn generation_alias_rewrites_controller_pending_and_deferred_policy_tokens() {
        let mut runtime = CacheRuntime::new(support_without_root(), LongFormSeekOptimization::On);
        let action = runtime
            .controller
            .begin_media(7, eligible_facts(), 0, Some(budget()))
            .expect("eligible media begins activation");
        runtime.register(44, action);
        runtime.deferred_cache_enable = Some(action);

        runtime.rebase_file_generation(7, 8);

        assert_eq!(runtime.status().file_generation, Some(8));
        assert!(matches!(
            runtime.pending.get(&44).map(|pending| pending.kind),
            Some(PendingKind::Set {
                token: PolicyToken {
                    file_generation: 8,
                    ..
                },
                enabled: true,
            })
        ));
        assert!(matches!(
            runtime.deferred_cache_enable,
            Some(CacheAction::SetCacheOnDisk {
                token: PolicyToken {
                    file_generation: 8,
                    ..
                },
                enabled: true,
            })
        ));
    }

    #[test]
    fn actor_observation_consumes_reservation_without_durable_io() {
        let (mut runtime, cache_dir, action) = eligible_runtime_with_root("no-observe-io");
        let (ready, persistence) = runtime.prepare_cache_action_inner(action);
        assert!(ready.is_none());
        let persistence = persistence.expect("activation waits for durable reservation");
        let completion = persistence.persist();
        let ready = runtime
            .complete_persistence(completion)
            .expect("durable ACK releases true write");
        assert!(matches!(
            ready,
            CacheAction::SetCacheOnDisk { enabled: true, .. }
        ));
        let ledger = cache_dir.join(super::super::cache_budget::LEDGER_FILE);
        let persisted = std::fs::read(&ledger).unwrap();
        let persist_count = runtime.rolling_budget.persistence_count();

        assert!(runtime.observe_file_cache_bytes(4_096).is_none());
        assert_eq!(runtime.rolling_budget.persistence_count(), persist_count);
        assert_eq!(std::fs::read(&ledger).unwrap(), persisted);
        let _ = std::fs::remove_dir_all(cache_dir);
    }

    #[test]
    fn failed_reservation_compensates_and_latches_future_activation() {
        let (mut runtime, cache_dir, action) = eligible_runtime_with_root("persist-failure");
        let (ready, persistence) = runtime.prepare_cache_action_inner(action);
        assert!(ready.is_none());
        let persistence = persistence.expect("activation waits for durable reservation");
        let failure = persistence
            .completion_for_test(Err(std::io::Error::other("injected persistence failure")));
        assert!(matches!(
            runtime.complete_persistence(failure),
            Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
        ));
        assert_eq!(
            runtime.status().effective,
            CacheEffectiveState::DisablePending
        );
        assert_eq!(runtime.status().reason, CacheReason::WriteBudgetExhausted);
        assert_eq!(
            runtime.rolling_written_bytes,
            super::super::long_form_seek::ROLLING_WRITE_BUDGET_BYTES
        );

        runtime.close_media();
        assert!(
            runtime
                .begin_media(2, MediaSourceContext::OnDemand)
                .is_none()
        );
        assert!(runtime.observe_duration(Some(3_600.0)).is_none());
        assert!(runtime.observe_network(Some(true)).is_none());
        assert!(runtime.observe_seekable(Some(true)).is_none());
        assert!(runtime.observe_partially_seekable(Some(false)).is_none());
        assert_eq!(runtime.status().effective, CacheEffectiveState::RamOnly);
        assert_eq!(runtime.status().reason, CacheReason::WriteBudgetExhausted);
        let _ = std::fs::remove_dir_all(cache_dir);
    }

    #[test]
    fn unmanaged_or_ram_only_file_bytes_do_not_consume_the_managed_budget() {
        let session = Arc::new(OwnerSessionAccounting::new());
        let mut runtime = CacheRuntime::with_session(
            support_without_root(),
            LongFormSeekOptimization::Off,
            Arc::clone(&session),
            None,
        );
        runtime.observe_file_cache_bytes(64 * 1024 * 1024);
        assert_eq!(session.written(), 0);
    }

    #[test]
    fn control_latency_starts_fail_closed_and_retains_the_session_worst_case() {
        let session = OwnerSessionAccounting::new();
        assert_eq!(
            session.control_latency_ms(),
            WATCHDOG_POLL_PHASE_MS + DISABLE_DEADLINE_MS
        );
        session.observe_control_latency(7_500);
        session.observe_control_latency(100);
        assert_eq!(session.control_latency_ms(), 7_500);
    }

    #[test]
    fn volume_space_probe_uses_normal_and_near_reserve_cadence() {
        let mut runtime = CacheRuntime::new(support_without_root(), LongFormSeekOptimization::On);
        runtime.volume_space_override = Some(crate::util::safe_fs::VolumeSpace {
            available_bytes: 8 * 1024 * 1024 * 1024,
            total_bytes: 100 * 1024 * 1024 * 1024,
        });
        assert!(runtime.volume_space_at(0).is_some());
        assert!(runtime.before_interactive_seek(0.0, 900.0).is_none());
        assert!(runtime.interactive_seek_succeeded(900.0).is_none());
        assert!(runtime.volume_space_at(1).is_some());
        assert!(
            runtime
                .volume_space_at(VOLUME_SPACE_REFRESH_MS - 1)
                .is_some()
        );
        assert_eq!(runtime.volume_space_probe_count.get(), 1);
        assert!(runtime.volume_space_at(VOLUME_SPACE_REFRESH_MS).is_some());
        assert_eq!(runtime.volume_space_probe_count.get(), 2);

        runtime.volume_space_override = Some(crate::util::safe_fs::VolumeSpace {
            available_bytes: FREE_SPACE_RESERVE_BYTES + CACHE_SOFT_TARGET_BYTES,
            total_bytes: 10 * 1024 * 1024 * 1024,
        });
        runtime.invalidate_volume_space();
        assert!(runtime.volume_space_at(10_000).is_some());
        assert!(
            runtime
                .volume_space_at(10_000 + VOLUME_SPACE_NEAR_RESERVE_REFRESH_MS - 1)
                .is_some()
        );
        assert_eq!(runtime.volume_space_probe_count.get(), 3);
        assert!(
            runtime
                .volume_space_at(10_000 + VOLUME_SPACE_NEAR_RESERVE_REFRESH_MS)
                .is_some()
        );
        assert_eq!(runtime.volume_space_probe_count.get(), 4);
    }

    #[test]
    fn rate_sample_queue_is_bounded() {
        let mut samples = VecDeque::new();
        for value in 0..100 {
            push_rate(&mut samples, value, value);
        }
        assert_eq!(samples.len(), RATE_SAMPLE_CAPACITY);
        assert_eq!(samples.front().map(|sample| sample.bytes_per_sec), Some(84));
    }

    #[test]
    fn missing_active_volume_stat_is_not_ignored() {
        let mut runtime = CacheRuntime::new(support_without_root(), LongFormSeekOptimization::On);
        assert!(matches!(
            runtime
                .controller
                .begin_media(1, eligible_facts(), 0, Some(budget())),
            Some(CacheAction::SetCacheOnDisk { enabled: true, .. })
        ));
        assert!(matches!(
            runtime.observe_file_cache_bytes(0),
            Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
        ));
        assert_eq!(runtime.status().reason, CacheReason::CacheRootUnavailable);
    }

    #[test]
    fn rejected_file_evidence_query_compensates_enable() {
        let mut runtime = CacheRuntime::new(support_without_root(), LongFormSeekOptimization::On);
        runtime
            .controller
            .begin_media(1, eligible_facts(), 0, Some(budget()));
        assert!(runtime.register_file_bytes_query(77));
        assert!(matches!(
            runtime.handle_reply(77, false, None),
            Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
        ));
        assert_eq!(
            runtime.status().reason,
            CacheReason::PropertyVerificationFailed
        );
    }

    #[test]
    fn watchdog_extracts_file_bytes_from_actual_cache_state_node() {
        let mut runtime = CacheRuntime::new(support_without_root(), LongFormSeekOptimization::On);
        runtime
            .controller
            .begin_media(1, eligible_facts(), 0, Some(budget()));
        assert!(runtime.register_file_bytes_query(77));
        let state = serde_json::json!({
            "file-cache-bytes": 4_096,
            "raw-input-rate": 2_048.0,
            "seekable-ranges": [],
        });
        assert!(matches!(
            runtime.handle_reply(77, true, Some(&state)),
            Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
        ));
        assert_eq!(runtime.status().file_cache_bytes, 4_096);
        assert_eq!(
            runtime
                .raw_input_rate
                .back()
                .map(|sample| sample.bytes_per_sec),
            Some(2_048)
        );
    }

    #[test]
    fn cache_state_without_nested_file_bytes_fails_closed() {
        let mut runtime = CacheRuntime::new(support_without_root(), LongFormSeekOptimization::On);
        runtime
            .controller
            .begin_media(1, eligible_facts(), 0, Some(budget()));
        assert!(runtime.register_file_bytes_query(78));
        let state = serde_json::json!({"seekable-ranges": []});
        assert!(matches!(
            runtime.handle_reply(78, true, Some(&state)),
            Some(CacheAction::SetCacheOnDisk { enabled: false, .. })
        ));
        assert_eq!(
            runtime.status().reason,
            CacheReason::PropertyVerificationFailed
        );
    }

    #[test]
    fn runtime_forwards_one_shot_forced_ram_only_policy() {
        let mut runtime = CacheRuntime::new(support_without_root(), LongFormSeekOptimization::On);
        runtime.force_next_media_ram_only();
        assert!(
            runtime
                .begin_media(1, MediaSourceContext::OnDemand)
                .is_none()
        );
        assert_eq!(runtime.status().requested, LongFormSeekOptimization::On);
        assert_eq!(runtime.status().effective, CacheEffectiveState::RamOnly);
        assert_eq!(runtime.status().reason, CacheReason::DisableFailed);
    }
}
