//! Pure policy and lifecycle state for the managed current-media packet cache.
//!
//! The mpv actor owns one [`LongFormCacheController`] and translates [`CacheAction`] values into
//! correlated JSON IPC operations.  Keeping time, filesystem, and IPC outside this module makes
//! every threshold and fail-closed transition deterministic in unit tests.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::LongFormSeekOptimization;

pub const LONG_FORM_MIN_DURATION_SECS: f64 = 30.0 * 60.0;
pub const AUTO_MIN_JUMP_SECS: f64 = 5.0 * 60.0;
pub const RANGE_ENDPOINT_EPSILON_SECS: f64 = 0.050;
pub const AUTO_PROBE_INTERVAL_MS: u64 = 5_000;
pub const CACHE_SOFT_TARGET_BYTES: u64 = 256 * 1024 * 1024;
pub const FREE_SPACE_RESERVE_BYTES: u64 = 1024 * 1024 * 1024;
pub const SESSION_WRITE_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;
pub const ROLLING_WRITE_BUDGET_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const IN_FLIGHT_BYTES: u64 = 32 * 1024 * 1024;
pub const MIN_OVERSHOOT_BYTES: u64 = 8 * 1024 * 1024;
pub const ENABLE_DEADLINE_MS: u64 = 3_000;
pub const DISABLE_DEADLINE_MS: u64 = 5_000;
pub const DISABLE_STABLE_WINDOW_MS: u64 = 2_000;
pub const DISABLE_STABLE_SAMPLES: u8 = 3;
/// Provisional pre-activation multiplier for mpv `cache-speed`. The performance harness must
/// validate source-rate/cache-speed ratios before Auto can become the default; release default Off
/// keeps this opt-in while the cross-platform measurement gate remains open.
const CACHE_SPEED_SAFETY_FACTOR: u64 = 2;

/// Public effective-state vocabulary. These IDs are shared by logs and the remote v8 read model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheEffectiveState {
    #[default]
    NoMedia,
    RamOnly,
    Probing,
    EnablePending,
    DiskActive,
    DisablePending,
    LatchedUntilClose,
    EmergencyClosePending,
    Overridden,
    Unavailable,
}

impl CacheEffectiveState {
    pub const fn id(self) -> &'static str {
        match self {
            Self::NoMedia => "no_media",
            Self::RamOnly => "ram_only",
            Self::Probing => "probing",
            Self::EnablePending => "enable_pending",
            Self::DiskActive => "disk_active",
            Self::DisablePending => "disable_pending",
            Self::LatchedUntilClose => "latched_until_close",
            Self::EmergencyClosePending => "emergency_close_pending",
            Self::Overridden => "overridden",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Stable reason vocabulary. Additions are backward compatible; existing spellings are frozen.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheReason {
    #[default]
    NoMedia,
    RequestedOff,
    AwaitingMediaFacts,
    ShortMedia,
    SequentialPlayback,
    SeekBelowThreshold,
    SeekWithinCachedRange,
    LocalSource,
    LiveSource,
    UnseekableSource,
    PartiallySeekableUnproven,
    ReadOnlyInstance,
    UnsupportedMpv,
    CustomMpvOverride,
    CacheRootUnavailable,
    InsufficientFreeSpace,
    UnsafeRateBound,
    InvalidRangeState,
    ProbeFailed,
    AutoUncachedSeek,
    OnEligibleMedia,
    UserRequestedOff,
    SoftCapReached,
    FreeSpaceFloor,
    WriteBudgetExhausted,
    PropertyRejected,
    PropertyTimeout,
    PropertyVerificationFailed,
    DisableFailed,
    MediaClosed,
}

impl CacheReason {
    pub const fn id(self) -> &'static str {
        match self {
            Self::RequestedOff => "requested_off",
            Self::NoMedia => "no_media",
            Self::AwaitingMediaFacts => "awaiting_media_facts",
            Self::ShortMedia => "short_media",
            Self::SequentialPlayback => "sequential_playback",
            Self::SeekBelowThreshold => "seek_below_threshold",
            Self::SeekWithinCachedRange => "seek_within_cached_range",
            Self::LocalSource => "local_source",
            Self::LiveSource => "live_source",
            Self::UnseekableSource => "unseekable_source",
            Self::PartiallySeekableUnproven => "partially_seekable_unproven",
            Self::ReadOnlyInstance => "read_only_instance",
            Self::UnsupportedMpv => "unsupported_mpv",
            Self::CustomMpvOverride => "custom_mpv_override",
            Self::CacheRootUnavailable => "cache_root_unavailable",
            Self::InsufficientFreeSpace => "insufficient_free_space",
            Self::UnsafeRateBound => "unsafe_rate_bound",
            Self::InvalidRangeState => "invalid_range_state",
            Self::ProbeFailed => "probe_failed",
            Self::AutoUncachedSeek => "auto_uncached_seek",
            Self::OnEligibleMedia => "on_eligible_media",
            Self::UserRequestedOff => "user_requested_off",
            Self::SoftCapReached => "soft_cap_reached",
            Self::FreeSpaceFloor => "free_space_floor",
            Self::WriteBudgetExhausted => "write_budget_exhausted",
            Self::PropertyRejected => "property_rejected",
            Self::PropertyTimeout => "property_timeout",
            Self::PropertyVerificationFailed => "property_verification_failed",
            Self::DisableFailed => "disable_failed",
            Self::MediaClosed => "media_closed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheOptionFamily {
    Modern,
    Legacy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerCapability {
    Available(CacheOptionFamily),
    Unavailable(CacheReason),
    Overridden,
}

impl ControllerCapability {
    fn status(self) -> Option<(CacheEffectiveState, CacheReason)> {
        match self {
            Self::Available(_) => None,
            Self::Unavailable(reason) => Some((CacheEffectiveState::Unavailable, reason)),
            Self::Overridden => Some((
                CacheEffectiveState::Overridden,
                CacheReason::CustomMpvOverride,
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MediaFacts {
    pub duration_secs: Option<f64>,
    pub via_network: Option<bool>,
    pub seekable: Option<bool>,
    pub partially_seekable: Option<bool>,
    pub live: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheStatus {
    pub requested: LongFormSeekOptimization,
    pub effective: CacheEffectiveState,
    pub reason: CacheReason,
    pub file_generation: Option<u64>,
    pub policy_revision: u64,
    pub file_cache_bytes: u64,
    pub peak_file_cache_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PolicyToken {
    pub request_id: u64,
    pub file_generation: u64,
    pub policy_revision: u64,
}

impl PolicyToken {
    pub(crate) fn rebase_file_generation(&mut self, previous: u64, generation: u64) {
        if self.file_generation == previous {
            self.file_generation = generation;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RangeProbe {
    pub token: PolicyToken,
    pub target_secs: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisableCause {
    UserOff,
    SoftCap,
    FreeSpace,
    WriteBudget,
    Failure(CacheReason),
    Replacement,
}

impl DisableCause {
    const fn reason(self) -> CacheReason {
        match self {
            Self::UserOff => CacheReason::UserRequestedOff,
            Self::SoftCap => CacheReason::SoftCapReached,
            Self::FreeSpace => CacheReason::FreeSpaceFloor,
            Self::WriteBudget => CacheReason::WriteBudgetExhausted,
            Self::Failure(reason) => reason,
            Self::Replacement => CacheReason::MediaClosed,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CacheAction {
    QueryRanges(RangeProbe),
    SetCacheOnDisk {
        token: PolicyToken,
        enabled: bool,
    },
    ReadBackCacheOnDisk {
        token: PolicyToken,
        expected: bool,
    },
    EmergencyCloseAndResume {
        file_generation: u64,
        position_secs: f64,
        paused: bool,
        reason: CacheReason,
    },
}

impl CacheAction {
    pub(crate) fn rebase_file_generation(&mut self, previous: u64, generation: u64) {
        match self {
            Self::QueryRanges(probe) => {
                probe.token.rebase_file_generation(previous, generation);
            }
            Self::SetCacheOnDisk { token, .. } | Self::ReadBackCacheOnDisk { token, .. } => {
                token.rebase_file_generation(previous, generation);
            }
            Self::EmergencyCloseAndResume {
                file_generation, ..
            } => {
                if *file_generation == previous {
                    *file_generation = generation;
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RateEvidence {
    pub fixture_max_bytes_per_sec: Option<u64>,
    pub measured_file_delta_bytes_per_sec: Option<u64>,
    pub cache_speed_bytes_per_sec: Option<u64>,
    pub raw_input_rate_bytes_per_sec: Option<u64>,
}

impl RateEvidence {
    /// Conservative maximum across the safe evidence currently available. `raw-input-rate` can
    /// corroborate and raise another bound, but cannot establish admission by itself because it is
    /// missing on legacy mpv 0.32 and can under-report segmented input.
    pub fn conservative_bound(self) -> Option<u64> {
        let corroborated = [
            self.fixture_max_bytes_per_sec,
            self.measured_file_delta_bytes_per_sec,
            self.cache_speed_bytes_per_sec
                .map(|value| value.saturating_mul(CACHE_SPEED_SAFETY_FACTOR)),
        ]
        .into_iter()
        .flatten()
        .max()?;
        Some(
            self.raw_input_rate_bytes_per_sec
                .map(|value| value.saturating_mul(CACHE_SPEED_SAFETY_FACTOR))
                .map_or(corroborated, |raw| corroborated.max(raw)),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageBudget {
    pub available_bytes: u64,
    pub volume_bytes: u64,
    pub session_written_bytes: u64,
    pub rolling_written_bytes: u64,
    /// Measured worst-case poll + actor + IPC actuation time.
    pub control_latency_ms: u64,
    pub rate: RateEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Admission {
    pub reserve_bytes: u64,
    pub overshoot_bytes: u64,
    pub required_available_bytes: u64,
    pub rate_bound_bytes_per_sec: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveStorageGuard {
    pub available_bytes: u64,
    pub admission: Admission,
}

impl StorageBudget {
    pub fn admission(self) -> Result<Admission, CacheReason> {
        let admission = self.guard_terms()?;
        if self.available_bytes < admission.required_available_bytes {
            return Err(CacheReason::InsufficientFreeSpace);
        }
        Ok(admission)
    }

    /// Recompute the active writer's reserve and next-loop guard without requiring the original
    /// 256 MiB admission headroom to remain untouched. Per-media bytes and cumulative budgets are
    /// checked separately while active; consuming admitted headroom must not immediately disable.
    pub fn active_guard(self) -> Result<ActiveStorageGuard, CacheReason> {
        Ok(ActiveStorageGuard {
            available_bytes: self.available_bytes,
            admission: self.guard_terms()?,
        })
    }

    fn guard_terms(self) -> Result<Admission, CacheReason> {
        if self.session_written_bytes >= SESSION_WRITE_BUDGET_BYTES
            || self.rolling_written_bytes >= ROLLING_WRITE_BUDGET_BYTES
        {
            return Err(CacheReason::WriteBudgetExhausted);
        }
        let rate = self
            .rate
            .conservative_bound()
            .ok_or(CacheReason::UnsafeRateBound)?;
        let reserve = FREE_SPACE_RESERVE_BYTES.max(div_ceil(self.volume_bytes, 20));
        let timed = u128::from(rate)
            .saturating_mul(u128::from(self.control_latency_ms))
            .saturating_mul(3)
            / 2_000;
        let timed = u64::try_from(timed).unwrap_or(u64::MAX);
        let overshoot = MIN_OVERSHOOT_BYTES.max(timed.saturating_add(IN_FLIGHT_BYTES));
        let required = reserve
            .saturating_add(CACHE_SOFT_TARGET_BYTES)
            .saturating_add(overshoot);
        Ok(Admission {
            reserve_bytes: reserve,
            overshoot_bytes: overshoot,
            required_available_bytes: required,
            rate_bound_bytes_per_sec: rate,
        })
    }
}

const fn div_ceil(value: u64, denominator: u64) -> u64 {
    value / denominator + ((!value.is_multiple_of(denominator)) as u64)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CachedRange {
    pub start: f64,
    pub end: f64,
}

/// Parse and normalize mpv's arbitrary, overlapping `seekable-ranges` array.
///
/// One malformed member invalidates the whole snapshot: Auto must never turn missing evidence
/// into a claim that a target is outside cache.
pub fn normalize_seekable_ranges(state: &Value) -> Result<Vec<CachedRange>, CacheReason> {
    let ranges = state
        .get("seekable-ranges")
        .and_then(Value::as_array)
        .ok_or(CacheReason::InvalidRangeState)?;
    let mut normalized = Vec::with_capacity(ranges.len());
    for value in ranges {
        let start = value
            .get("start")
            .and_then(Value::as_f64)
            .ok_or(CacheReason::InvalidRangeState)?;
        let end = value
            .get("end")
            .and_then(Value::as_f64)
            .ok_or(CacheReason::InvalidRangeState)?;
        if !start.is_finite() || !end.is_finite() || start < 0.0 || end < start {
            return Err(CacheReason::InvalidRangeState);
        }
        normalized.push(CachedRange { start, end });
    }
    normalized.sort_by(|left, right| left.start.total_cmp(&right.start));
    let mut merged: Vec<CachedRange> = Vec::with_capacity(normalized.len());
    for range in normalized {
        if let Some(last) = merged.last_mut()
            && range.start <= last.end + RANGE_ENDPOINT_EPSILON_SECS
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    Ok(merged)
}

pub fn target_is_cached(target_secs: f64, ranges: &[CachedRange]) -> bool {
    target_secs.is_finite()
        && target_secs >= 0.0
        && ranges.iter().any(|range| {
            target_secs >= range.start - RANGE_ENDPOINT_EPSILON_SECS
                && target_secs <= range.end + RANGE_ENDPOINT_EPSILON_SECS
        })
}

#[derive(Clone, Copy, Debug)]
struct PendingEnable {
    token: PolicyToken,
    readback_true: bool,
    baseline_bytes: u64,
    started_ms: u64,
}

#[derive(Clone, Copy, Debug)]
struct DisableProof {
    token: PolicyToken,
    cause: DisableCause,
    started_ms: u64,
    reply_ok: bool,
    readback_false: bool,
    stable_value: Option<u64>,
    stable_started_ms: u64,
    stable_samples: u8,
}

impl DisableProof {
    fn proven(self, now_ms: u64) -> bool {
        self.reply_ok
            && self.readback_false
            && self.stable_samples >= DISABLE_STABLE_SAMPLES
            && now_ms.saturating_sub(self.stable_started_ms) >= DISABLE_STABLE_WINDOW_MS
    }
}

/// Deterministic per-player/per-media controller. All returned actions must retain their token.
pub struct LongFormCacheController {
    capability: ControllerCapability,
    requested: LongFormSeekOptimization,
    effective: CacheEffectiveState,
    reason: CacheReason,
    file_generation: Option<u64>,
    policy_revision: u64,
    next_request_id: u64,
    facts: MediaFacts,
    partial_seek_proven: bool,
    activation_attempted: bool,
    last_probe_ms: Option<u64>,
    pending_probe: Option<RangeProbe>,
    pending_partial_outside: Option<RangeProbe>,
    successful_interactive_target: Option<f64>,
    pending_enable: Option<PendingEnable>,
    disable: Option<DisableProof>,
    file_cache_bytes: u64,
    peak_file_cache_bytes: u64,
    last_position_secs: f64,
    paused: bool,
    force_next_media_ram_only: bool,
    media_forced_ram_only: bool,
}

impl LongFormCacheController {
    pub fn new(capability: ControllerCapability, requested: LongFormSeekOptimization) -> Self {
        let (effective, reason) = capability
            .status()
            .unwrap_or((CacheEffectiveState::NoMedia, CacheReason::NoMedia));
        Self {
            capability,
            requested,
            effective,
            reason,
            file_generation: None,
            policy_revision: 0,
            next_request_id: 0,
            facts: MediaFacts::default(),
            partial_seek_proven: false,
            activation_attempted: false,
            last_probe_ms: None,
            pending_probe: None,
            pending_partial_outside: None,
            successful_interactive_target: None,
            pending_enable: None,
            disable: None,
            file_cache_bytes: 0,
            peak_file_cache_bytes: 0,
            last_position_secs: 0.0,
            paused: true,
            force_next_media_ram_only: false,
            media_forced_ram_only: false,
        }
    }

    /// Force exactly the next admitted media to stay RAM-only after an emergency player recycle.
    ///
    /// This is deliberately independent of the requested preference: a later normal media may
    /// apply the still-requested policy after the forced media closes.
    pub fn force_next_media_ram_only(&mut self) {
        self.force_next_media_ram_only = true;
    }

    pub fn status(&self) -> CacheStatus {
        CacheStatus {
            requested: self.requested,
            effective: self.effective,
            reason: self.reason,
            file_generation: self.file_generation,
            policy_revision: self.policy_revision,
            file_cache_bytes: self.file_cache_bytes,
            peak_file_cache_bytes: self.peak_file_cache_bytes,
        }
    }

    pub(crate) fn rebase_file_generation(&mut self, previous: u64, generation: u64) -> bool {
        if self.file_generation != Some(previous) {
            return false;
        }
        self.file_generation = Some(generation);
        for probe in [
            self.pending_probe.as_mut(),
            self.pending_partial_outside.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            probe.token.rebase_file_generation(previous, generation);
        }
        if let Some(pending) = self.pending_enable.as_mut() {
            pending.token.rebase_file_generation(previous, generation);
        }
        if let Some(disable) = self.disable.as_mut() {
            disable.token.rebase_file_generation(previous, generation);
        }
        true
    }

    pub fn option_family(&self) -> Option<CacheOptionFamily> {
        match self.capability {
            ControllerCapability::Available(family) => Some(family),
            ControllerCapability::Unavailable(_) | ControllerCapability::Overridden => None,
        }
    }

    pub fn begin_media(
        &mut self,
        file_generation: u64,
        facts: MediaFacts,
        now_ms: u64,
        budget: Option<StorageBudget>,
    ) -> Option<CacheAction> {
        self.policy_revision = self.policy_revision.wrapping_add(1);
        self.file_generation = Some(file_generation);
        self.facts = facts;
        self.partial_seek_proven = false;
        self.activation_attempted = false;
        self.last_probe_ms = None;
        self.pending_probe = None;
        self.pending_partial_outside = None;
        self.successful_interactive_target = None;
        self.pending_enable = None;
        self.disable = None;
        self.file_cache_bytes = 0;
        self.peak_file_cache_bytes = 0;
        self.last_position_secs = 0.0;
        self.paused = true;
        self.media_forced_ram_only = std::mem::take(&mut self.force_next_media_ram_only);
        if self.media_forced_ram_only {
            self.effective = CacheEffectiveState::RamOnly;
            self.reason = CacheReason::DisableFailed;
            return None;
        }
        if let Some(status) = self.capability.status() {
            (self.effective, self.reason) = status;
            return None;
        }
        self.effective = CacheEffectiveState::RamOnly;
        if self.requested == LongFormSeekOptimization::Off {
            self.reason = CacheReason::RequestedOff;
            return None;
        }
        if self.requested == LongFormSeekOptimization::Auto {
            self.reason = self
                .eligibility_reason()
                .unwrap_or(CacheReason::SequentialPlayback);
            return None;
        }
        self.try_on_activation(now_ms, budget)
    }

    pub fn update_media_facts(
        &mut self,
        facts: MediaFacts,
        now_ms: u64,
        budget: Option<StorageBudget>,
    ) -> Option<CacheAction> {
        self.facts = facts;
        if self.file_generation.is_none() {
            (self.effective, self.reason) = self
                .capability
                .status()
                .unwrap_or((CacheEffectiveState::NoMedia, CacheReason::NoMedia));
            return None;
        }
        if self.media_forced_ram_only {
            self.effective = CacheEffectiveState::RamOnly;
            self.reason = CacheReason::DisableFailed;
            return None;
        }
        if matches!(
            self.effective,
            CacheEffectiveState::EnablePending | CacheEffectiveState::DiskActive
        ) && let Some(reason) = self.eligibility_reason()
        {
            // Eligibility facts arrive asynchronously and may also be revoked mid-media. Never
            // leave an already-issued process-global true write (or an active writer) running
            // after the current generation stops satisfying the fail-closed policy.
            return self.start_disable(now_ms, DisableCause::Failure(reason));
        }
        if self.effective == CacheEffectiveState::Probing
            && let Some(reason) = self.eligibility_reason()
            && reason != CacheReason::PartiallySeekableUnproven
        {
            // A range snapshot cannot outlive the facts that made its activation candidate
            // eligible. Clearing the exact pending token makes any already-arriving reply stale.
            self.fallback(reason);
            return None;
        }
        if self.requested == LongFormSeekOptimization::On
            && self.effective == CacheEffectiveState::RamOnly
        {
            return self.try_on_activation(now_ms, budget);
        }
        if self.effective == CacheEffectiveState::RamOnly {
            self.reason = self.eligibility_reason().unwrap_or(match self.requested {
                LongFormSeekOptimization::Auto => CacheReason::SequentialPlayback,
                LongFormSeekOptimization::Off => CacheReason::RequestedOff,
                LongFormSeekOptimization::On => CacheReason::OnEligibleMedia,
            });
        }
        None
    }

    pub fn observe_transport(&mut self, position_secs: f64, paused: bool) {
        if position_secs.is_finite() && position_secs >= 0.0 {
            self.last_position_secs = position_secs;
        }
        self.paused = paused;
    }

    pub fn mark_off_cache_seek_succeeded(
        &mut self,
        target_secs: f64,
        budget: Option<StorageBudget>,
        now_ms: u64,
    ) -> Option<CacheAction> {
        if self.media_forced_ram_only {
            return None;
        }
        if !target_secs.is_finite() || target_secs < 0.0 {
            return None;
        }
        self.successful_interactive_target = Some(target_secs);
        let probe = self.pending_partial_outside?;
        if (probe.target_secs - target_secs).abs() > RANGE_ENDPOINT_EPSILON_SECS
            || !self.token_current(probe.token)
        {
            return None;
        }
        self.pending_partial_outside = None;
        self.partial_seek_proven = true;
        self.start_enable(CacheReason::AutoUncachedSeek, budget, now_ms)
    }

    pub fn off_cache_seek_completion_needs_budget(&self, target_secs: f64) -> bool {
        self.pending_partial_outside.is_some_and(|probe| {
            (probe.target_secs - target_secs).abs() <= RANGE_ENDPOINT_EPSILON_SECS
                && self.token_current(probe.token)
        })
    }

    pub fn committed_interactive_seek(
        &mut self,
        from_secs: f64,
        target_secs: f64,
        now_ms: u64,
    ) -> Option<CacheAction> {
        if self.media_forced_ram_only {
            return None;
        }
        let proving_partial = self.requested != LongFormSeekOptimization::Off
            && self.facts.partially_seekable == Some(true)
            && !self.partial_seek_proven;
        if (self.requested != LongFormSeekOptimization::Auto && !proving_partial)
            || self.effective != CacheEffectiveState::RamOnly
            || self.activation_attempted
            || self.pending_probe.is_some()
        {
            return None;
        }
        if let Some(reason) = self.eligibility_reason()
            && reason != CacheReason::PartiallySeekableUnproven
        {
            self.reason = reason;
            return None;
        }
        if !from_secs.is_finite()
            || !target_secs.is_finite()
            || from_secs < 0.0
            || target_secs < 0.0
            || (target_secs - from_secs).abs() < AUTO_MIN_JUMP_SECS
        {
            self.reason = CacheReason::SeekBelowThreshold;
            return None;
        }
        if self
            .last_probe_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < AUTO_PROBE_INTERVAL_MS)
        {
            return None;
        }
        let token = self.next_token()?;
        let probe = RangeProbe { token, target_secs };
        self.last_probe_ms = Some(now_ms);
        self.pending_probe = Some(probe);
        self.effective = CacheEffectiveState::Probing;
        self.reason = CacheReason::AutoUncachedSeek;
        Some(CacheAction::QueryRanges(probe))
    }

    pub fn range_reply(
        &mut self,
        probe: RangeProbe,
        result: Result<&Value, ()>,
        budget: Option<StorageBudget>,
        now_ms: u64,
    ) -> Option<CacheAction> {
        if self.media_forced_ram_only {
            return None;
        }
        if self.pending_probe != Some(probe) || !self.token_current(probe.token) {
            return None;
        }
        self.pending_probe = None;
        if let Some(reason) = self.eligibility_reason()
            && reason != CacheReason::PartiallySeekableUnproven
        {
            self.fallback(reason);
            return None;
        }
        let state = match result {
            Ok(state) => state,
            Err(()) => {
                self.fallback(CacheReason::ProbeFailed);
                return None;
            }
        };
        let ranges = match normalize_seekable_ranges(state) {
            Ok(ranges) => ranges,
            Err(reason) => {
                self.fallback(reason);
                return None;
            }
        };
        if target_is_cached(probe.target_secs, &ranges) {
            self.fallback(CacheReason::SeekWithinCachedRange);
            return None;
        }
        if self.facts.partially_seekable == Some(true) && !self.partial_seek_proven {
            if self.successful_interactive_target.is_some_and(|target| {
                (target - probe.target_secs).abs() <= RANGE_ENDPOINT_EPSILON_SECS
            }) {
                self.partial_seek_proven = true;
                return self.start_enable(CacheReason::AutoUncachedSeek, budget, now_ms);
            }
            self.pending_partial_outside = Some(probe);
            self.effective = CacheEffectiveState::Probing;
            self.reason = CacheReason::PartiallySeekableUnproven;
            return None;
        }
        self.start_enable(CacheReason::AutoUncachedSeek, budget, now_ms)
    }

    pub fn set_reply(
        &mut self,
        token: PolicyToken,
        enabled: bool,
        accepted: bool,
        now_ms: u64,
    ) -> Option<CacheAction> {
        if enabled {
            if !self.token_current(token) {
                return None;
            }
            let pending = self.pending_enable.as_ref()?;
            if pending.token != token {
                return None;
            }
            if now_ms.saturating_sub(pending.started_ms) >= ENABLE_DEADLINE_MS {
                return self.enable_failure(now_ms, CacheReason::PropertyTimeout);
            }
            if !accepted {
                return self.enable_failure(now_ms, CacheReason::PropertyRejected);
            }
            Some(CacheAction::ReadBackCacheOnDisk {
                token,
                expected: true,
            })
        } else {
            let disable = self.disable.as_mut()?;
            if disable.token != token {
                return None;
            }
            if !accepted {
                return self.emergency(CacheReason::DisableFailed);
            }
            disable.reply_ok = true;
            Some(CacheAction::ReadBackCacheOnDisk {
                token,
                expected: false,
            })
        }
    }

    pub fn readback_reply(
        &mut self,
        token: PolicyToken,
        expected: bool,
        value: Option<bool>,
        now_ms: u64,
    ) -> Option<CacheAction> {
        if expected {
            if !self.token_current(token) {
                return None;
            }
            let pending = self.pending_enable.as_mut()?;
            if pending.token != token {
                return None;
            }
            if now_ms.saturating_sub(pending.started_ms) >= ENABLE_DEADLINE_MS {
                return self.enable_failure(now_ms, CacheReason::PropertyTimeout);
            }
            if value != Some(true) {
                return self.enable_failure(now_ms, CacheReason::PropertyVerificationFailed);
            }
            pending.readback_true = true;
        } else {
            let disable = self.disable.as_mut()?;
            if disable.token != token {
                return None;
            }
            if value != Some(false) {
                return self.emergency(CacheReason::PropertyVerificationFailed);
            }
            if !disable.readback_false {
                // Stability evidence is meaningful only after effective false is proven. Samples
                // gathered while the process-global option might still be true cannot shorten
                // the post-disable settle window.
                disable.readback_false = true;
                disable.stable_value = None;
                disable.stable_started_ms = now_ms;
                disable.stable_samples = 0;
            }
        }
        None
    }

    pub fn property_timeout(
        &mut self,
        token: PolicyToken,
        enabled: bool,
        now_ms: u64,
    ) -> Option<CacheAction> {
        if enabled {
            if !self.token_current(token)
                || !self
                    .pending_enable
                    .is_some_and(|pending| pending.token == token)
            {
                return None;
            }
            return self.enable_failure(now_ms, CacheReason::PropertyTimeout);
        }
        if self.disable.is_some_and(|disable| disable.token == token) {
            return self.emergency(CacheReason::DisableFailed);
        }
        None
    }

    pub fn active_watchdog_failure(
        &mut self,
        now_ms: u64,
        reason: CacheReason,
    ) -> Option<CacheAction> {
        if self.effective == CacheEffectiveState::DisablePending {
            return self.emergency(reason);
        }
        self.enable_failure(now_ms, reason)
    }

    pub fn file_cache_sample(
        &mut self,
        bytes: u64,
        now_ms: u64,
        storage: Result<ActiveStorageGuard, CacheReason>,
        session_written_bytes: u64,
        rolling_written_bytes: u64,
    ) -> Option<CacheAction> {
        let previous_bytes = self.file_cache_bytes;
        self.file_cache_bytes = bytes;
        self.peak_file_cache_bytes = self.peak_file_cache_bytes.max(bytes);
        if self.effective == CacheEffectiveState::LatchedUntilClose && bytes > previous_bytes {
            return self.emergency(CacheReason::DisableFailed);
        }
        if self.effective == CacheEffectiveState::EnablePending {
            let pending = self.pending_enable?;
            if now_ms.saturating_sub(pending.started_ms) >= ENABLE_DEADLINE_MS {
                let reason = if pending.readback_true {
                    CacheReason::PropertyVerificationFailed
                } else {
                    CacheReason::PropertyTimeout
                };
                return self.enable_failure(now_ms, reason);
            }
        }
        if matches!(
            self.effective,
            CacheEffectiveState::EnablePending | CacheEffectiveState::DiskActive
        ) {
            let guard = match storage {
                Ok(guard) => guard,
                Err(reason) => return self.enable_failure(now_ms, reason),
            };
            if bytes >= CACHE_SOFT_TARGET_BYTES {
                return self.start_disable(now_ms, DisableCause::SoftCap);
            }
            if session_written_bytes >= SESSION_WRITE_BUDGET_BYTES
                || rolling_written_bytes >= ROLLING_WRITE_BUDGET_BYTES
            {
                return self.start_disable(now_ms, DisableCause::WriteBudget);
            }
            if guard.available_bytes
                <= guard
                    .admission
                    .reserve_bytes
                    .saturating_add(guard.admission.overshoot_bytes)
            {
                return self.start_disable(now_ms, DisableCause::FreeSpace);
            }
        }
        if self.effective == CacheEffectiveState::DisablePending {
            match storage {
                Err(CacheReason::WriteBudgetExhausted) => {}
                Err(reason) => return self.emergency(reason),
                Ok(guard) if guard.available_bytes <= guard.admission.reserve_bytes => {
                    return self.emergency(CacheReason::FreeSpaceFloor);
                }
                Ok(_) => {}
            }
        }
        if self.effective == CacheEffectiveState::EnablePending
            && let Some(pending) = self.pending_enable
            && pending.readback_true
            && bytes > pending.baseline_bytes
        {
            self.pending_enable = None;
            self.effective = CacheEffectiveState::DiskActive;
        }
        if self.effective == CacheEffectiveState::DisablePending {
            let disable = self.disable.as_mut()?;
            if disable.stable_value == Some(bytes) {
                disable.stable_samples = disable.stable_samples.saturating_add(1);
            } else {
                disable.stable_value = Some(bytes);
                disable.stable_started_ms = now_ms;
                disable.stable_samples = 1;
            }
            if disable.proven(now_ms) {
                self.effective = CacheEffectiveState::LatchedUntilClose;
                self.reason = disable.cause.reason();
                self.disable = None;
            } else if now_ms.saturating_sub(disable.started_ms) >= DISABLE_DEADLINE_MS {
                return self.emergency(CacheReason::DisableFailed);
            }
        }
        None
    }

    pub fn tick(&mut self, now_ms: u64) -> Option<CacheAction> {
        if let Some(pending) = self.pending_enable
            && now_ms.saturating_sub(pending.started_ms) >= ENABLE_DEADLINE_MS
        {
            let reason = if pending.readback_true {
                CacheReason::PropertyVerificationFailed
            } else {
                CacheReason::PropertyTimeout
            };
            return self.enable_failure(now_ms, reason);
        }
        if let Some(disable) = self.disable
            && now_ms.saturating_sub(disable.started_ms) >= DISABLE_DEADLINE_MS
        {
            return self.emergency(CacheReason::DisableFailed);
        }
        None
    }

    pub fn update_requested(
        &mut self,
        requested: LongFormSeekOptimization,
        now_ms: u64,
        budget: Option<StorageBudget>,
    ) -> Option<CacheAction> {
        if self.requested == requested {
            return None;
        }
        let previous = self.requested;
        self.requested = requested;
        self.policy_revision = self.policy_revision.wrapping_add(1);
        self.pending_probe = None;
        self.pending_partial_outside = None;
        self.successful_interactive_target = None;
        if let Some((effective, reason)) = self.capability.status() {
            self.pending_enable = None;
            self.disable = None;
            self.effective = effective;
            self.reason = reason;
            return None;
        }
        if self.file_generation.is_none() {
            self.pending_enable = None;
            self.disable = None;
            self.effective = CacheEffectiveState::NoMedia;
            self.reason = CacheReason::NoMedia;
            return None;
        }
        if self.media_forced_ram_only {
            self.effective = CacheEffectiveState::RamOnly;
            self.reason = CacheReason::DisableFailed;
            return None;
        }
        if requested == LongFormSeekOptimization::Off {
            if matches!(
                self.effective,
                CacheEffectiveState::EnablePending | CacheEffectiveState::DiskActive
            ) {
                return self.start_disable(now_ms, DisableCause::UserOff);
            }
            if matches!(
                self.effective,
                CacheEffectiveState::DisablePending
                    | CacheEffectiveState::EmergencyClosePending
                    | CacheEffectiveState::LatchedUntilClose
            ) {
                return None;
            }
            self.pending_enable = None;
            self.effective = CacheEffectiveState::RamOnly;
            self.reason = CacheReason::UserRequestedOff;
            return None;
        }
        if self.effective == CacheEffectiveState::EnablePending {
            // Every policy revision invalidates a state-promoting reply. The old true write is
            // therefore ambiguous even when the new mode is more permissive; compensate and
            // prove false instead of dropping watchdog ownership.
            return self.start_disable(
                now_ms,
                DisableCause::Failure(CacheReason::PropertyVerificationFailed),
            );
        }
        if self.effective == CacheEffectiveState::DiskActive {
            if previous == LongFormSeekOptimization::On
                && requested == LongFormSeekOptimization::Auto
            {
                // Auto promises no writes until a qualifying miss. A cache activated eagerly by
                // On cannot be grandfathered into that stricter policy for the current media.
                return self.start_disable(
                    now_ms,
                    DisableCause::Failure(CacheReason::SequentialPlayback),
                );
            }
            // Auto -> On is more permissive and the writer is already fully proven. Keep active
            // under the new revision and continue the same watchdog.
            return None;
        }
        if self.effective == CacheEffectiveState::LatchedUntilClose
            || self.effective == CacheEffectiveState::DisablePending
            || self.effective == CacheEffectiveState::EmergencyClosePending
        {
            return None;
        }
        self.activation_attempted = false;
        if requested == LongFormSeekOptimization::On {
            return self.try_on_activation(now_ms, budget);
        }
        self.effective = CacheEffectiveState::RamOnly;
        self.reason = self
            .eligibility_reason()
            .unwrap_or(CacheReason::SequentialPlayback);
        None
    }

    pub fn prepare_replacement(&mut self, now_ms: u64) -> Option<CacheAction> {
        if self.effective == CacheEffectiveState::EmergencyClosePending {
            return self.file_generation.map(|file_generation| {
                CacheAction::EmergencyCloseAndResume {
                    file_generation,
                    position_secs: self.last_position_secs,
                    paused: self.paused,
                    reason: self.reason,
                }
            });
        }
        if matches!(
            self.effective,
            CacheEffectiveState::EnablePending
                | CacheEffectiveState::DiskActive
                | CacheEffectiveState::DisablePending
        ) {
            return self.start_disable(now_ms, DisableCause::Replacement);
        }
        self.close_media();
        None
    }

    pub fn close_media(&mut self) {
        self.policy_revision = self.policy_revision.wrapping_add(1);
        self.file_generation = None;
        self.pending_probe = None;
        self.pending_partial_outside = None;
        self.successful_interactive_target = None;
        self.pending_enable = None;
        self.disable = None;
        self.file_cache_bytes = 0;
        self.media_forced_ram_only = false;
        self.effective = self
            .capability
            .status()
            .map_or(CacheEffectiveState::NoMedia, |status| status.0);
        self.reason = self
            .capability
            .status()
            .map_or(CacheReason::MediaClosed, |status| status.1);
    }

    fn try_on_activation(
        &mut self,
        now_ms: u64,
        budget: Option<StorageBudget>,
    ) -> Option<CacheAction> {
        if let Some((effective, reason)) = self.capability.status() {
            self.effective = effective;
            self.reason = reason;
            return None;
        }
        if self.media_forced_ram_only {
            self.effective = CacheEffectiveState::RamOnly;
            self.reason = CacheReason::DisableFailed;
            return None;
        }
        if let Some(reason) = self.eligibility_reason() {
            self.effective = CacheEffectiveState::RamOnly;
            self.reason = reason;
            return None;
        }
        self.start_enable(CacheReason::OnEligibleMedia, budget, now_ms)
    }

    fn eligibility_reason(&self) -> Option<CacheReason> {
        if self.facts.live {
            return Some(CacheReason::LiveSource);
        }
        match self.facts.via_network {
            Some(false) => return Some(CacheReason::LocalSource),
            None => return Some(CacheReason::AwaitingMediaFacts),
            Some(true) => {}
        }
        match self.facts.duration_secs {
            Some(duration) if duration.is_finite() && duration >= LONG_FORM_MIN_DURATION_SECS => {}
            Some(duration) if duration.is_finite() && duration >= 0.0 => {
                return Some(CacheReason::ShortMedia);
            }
            _ => return Some(CacheReason::AwaitingMediaFacts),
        }
        match self.facts.seekable {
            Some(false) => return Some(CacheReason::UnseekableSource),
            None => return Some(CacheReason::AwaitingMediaFacts),
            Some(true) => {}
        }
        match self.facts.partially_seekable {
            None => return Some(CacheReason::AwaitingMediaFacts),
            Some(true) if !self.partial_seek_proven => {
                return Some(CacheReason::PartiallySeekableUnproven);
            }
            Some(false) | Some(true) => {}
        }
        None
    }

    fn start_enable(
        &mut self,
        reason: CacheReason,
        budget: Option<StorageBudget>,
        now_ms: u64,
    ) -> Option<CacheAction> {
        if self.activation_attempted || self.media_forced_ram_only {
            return None;
        }
        let budget = match budget {
            Some(budget) => budget,
            None => {
                self.fallback(CacheReason::UnsafeRateBound);
                return None;
            }
        };
        if let Err(reason) = budget.admission() {
            self.fallback(reason);
            return None;
        }
        let token = self.next_token()?;
        self.activation_attempted = true;
        self.pending_enable = Some(PendingEnable {
            token,
            readback_true: false,
            baseline_bytes: self.file_cache_bytes,
            started_ms: now_ms,
        });
        self.effective = CacheEffectiveState::EnablePending;
        self.reason = reason;
        Some(CacheAction::SetCacheOnDisk {
            token,
            enabled: true,
        })
    }

    fn start_disable(&mut self, now_ms: u64, cause: DisableCause) -> Option<CacheAction> {
        if self.effective == CacheEffectiveState::EmergencyClosePending {
            return None;
        }
        self.policy_revision = self.policy_revision.wrapping_add(1);
        self.pending_probe = None;
        self.pending_partial_outside = None;
        self.pending_enable = None;
        let token = self.next_token()?;
        self.disable = Some(DisableProof {
            token,
            cause,
            started_ms: now_ms,
            reply_ok: false,
            readback_false: false,
            stable_value: None,
            stable_started_ms: now_ms,
            stable_samples: 0,
        });
        self.effective = CacheEffectiveState::DisablePending;
        self.reason = cause.reason();
        Some(CacheAction::SetCacheOnDisk {
            token,
            enabled: false,
        })
    }

    fn enable_failure(&mut self, now_ms: u64, reason: CacheReason) -> Option<CacheAction> {
        if !matches!(
            self.effective,
            CacheEffectiveState::EnablePending | CacheEffectiveState::DiskActive
        ) {
            return None;
        }
        self.start_disable(now_ms, DisableCause::Failure(reason))
    }

    fn emergency(&mut self, reason: CacheReason) -> Option<CacheAction> {
        let generation = self.file_generation?;
        self.pending_probe = None;
        self.pending_partial_outside = None;
        self.successful_interactive_target = None;
        self.pending_enable = None;
        self.disable = None;
        self.effective = CacheEffectiveState::EmergencyClosePending;
        self.reason = reason;
        Some(CacheAction::EmergencyCloseAndResume {
            file_generation: generation,
            position_secs: self.last_position_secs,
            paused: self.paused,
            reason,
        })
    }

    fn fallback(&mut self, reason: CacheReason) {
        self.pending_probe = None;
        self.pending_partial_outside = None;
        self.successful_interactive_target = None;
        self.pending_enable = None;
        self.disable = None;
        self.effective = CacheEffectiveState::RamOnly;
        self.reason = reason;
    }

    fn next_token(&mut self) -> Option<PolicyToken> {
        let generation = self.file_generation?;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        Some(PolicyToken {
            request_id: self.next_request_id,
            file_generation: generation,
            policy_revision: self.policy_revision,
        })
    }

    fn token_current(&self, token: PolicyToken) -> bool {
        self.file_generation == Some(token.file_generation)
            && self.policy_revision == token.policy_revision
    }
}

#[cfg(test)]
#[path = "long_form_seek/tests.rs"]
mod tests;
