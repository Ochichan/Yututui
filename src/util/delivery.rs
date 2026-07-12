//! Shared bounded delivery for owner-loop events.
//!
//! Both the terminal runtime and the headless daemon feed a bounded owner queue.  This
//! adapter keeps their saturation semantics in one place: telemetry is coalesced by a
//! semantic key, must-deliver events use one bounded spill queue and one drainer, and all
//! other full-queue outcomes are returned to the producer instead of being hidden.

use std::collections::VecDeque;
use std::hash::Hash;
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::{self, error::TrySendError};

use super::event_policy::{EventLane, EventPolicy, LatestEventBuffer, LatestEventClass};

mod admission;

pub(crate) use admission::CallbackCancellation;
use admission::IngressAdmission;

pub const DEFAULT_DEFERRED_CAPACITY: usize = 1024;
const DEFAULT_CONTROL_RESERVE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "delivery receipts must be observed or explicitly discarded"]
pub enum DeliveryReceipt {
    Enqueued,
    Deferred,
    Coalesced {
        replaced_existing: bool,
        evicted_oldest: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryError {
    Busy,
    StaleOrFull,
    BestEffortDropped,
    Saturated,
    Closed,
}

impl DeliveryError {
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::StaleOrFull => "stale_or_full",
            Self::BestEffortDropped => "dropped_best_effort",
            Self::Saturated => "must_deliver_saturated",
            Self::Closed => "closed",
        }
    }
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for DeliveryError {}

pub type DeliveryResult = Result<DeliveryReceipt, DeliveryError>;

/// Domain contract required by [`OwnerEventIngress`].
pub(crate) trait OwnerEvent: Send + 'static {
    type CoalesceKey: Eq + Hash + Clone + Send + 'static;

    fn policy(&self) -> EventPolicy;
    fn kind(&self) -> &'static str;
    fn coalesce_key(&self) -> Option<Self::CoalesceKey>;
    fn wake_event() -> Self;
}

pub(crate) struct OwnerEventIngress<E: OwnerEvent> {
    owner: &'static str,
    tx: mpsc::Sender<E>,
    admission: Arc<IngressAdmission>,
    coalesced: Arc<Mutex<LatestEventBuffer<E::CoalesceKey, E>>>,
    deferred: Arc<DeferredQueue<E>>,
}

impl<E: OwnerEvent> Clone for OwnerEventIngress<E> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner,
            tx: self.tx.clone(),
            admission: Arc::clone(&self.admission),
            coalesced: Arc::clone(&self.coalesced),
            deferred: Arc::clone(&self.deferred),
        }
    }
}

impl<E: OwnerEvent> OwnerEventIngress<E> {
    pub(crate) fn new(owner: &'static str, tx: mpsc::Sender<E>, coalesced_capacity: usize) -> Self {
        Self::with_deferred_capacity(owner, tx, coalesced_capacity, DEFAULT_DEFERRED_CAPACITY)
    }

    pub(crate) fn with_deferred_capacity(
        owner: &'static str,
        tx: mpsc::Sender<E>,
        coalesced_capacity: usize,
        deferred_capacity: usize,
    ) -> Self {
        Self {
            owner,
            tx,
            admission: Arc::new(IngressAdmission::new()),
            coalesced: Arc::new(Mutex::new(LatestEventBuffer::new(coalesced_capacity))),
            deferred: Arc::new(DeferredQueue::new(owner, deferred_capacity)),
        }
    }

    pub(crate) fn emit(&self, event: E) -> DeliveryResult {
        let policy = event.policy();
        let event_kind = event.kind();
        let Some(_admission) = self.admission.while_open() else {
            log_rejected(self.owner, event_kind, policy, DeliveryError::Closed);
            return Err(DeliveryError::Closed);
        };
        match policy {
            EventPolicy::CoalesceLatest { .. } => self.emit_coalesced(event, policy),
            EventPolicy::DropIfStale { .. } => self.emit_stale(event, policy),
            _ => self.emit_direct(event, policy),
        }
    }

    /// Attempt one must-deliver admission while returning ownership on rejection.
    ///
    /// Terminal background completions use this to retry saturation without cloning or losing
    /// payloads. Other policies deliberately keep their ordinary coalesce/drop/reply semantics.
    pub(crate) fn emit_must_deliver_owned(
        &self,
        event: E,
    ) -> Result<DeliveryReceipt, (DeliveryError, Box<E>)> {
        let policy = event.policy();
        let event_kind = event.kind();
        let Some(_admission) = self.admission.while_open() else {
            log_rejected(self.owner, event_kind, policy, DeliveryError::Closed);
            return Err((DeliveryError::Closed, Box::new(event)));
        };
        if !matches!(policy, EventPolicy::MustDeliver { .. }) {
            let error = direct_rejection(policy);
            log_rejected(self.owner, event_kind, policy, error);
            return Err((error, Box::new(event)));
        }
        let result = self
            .deferred
            .admit_owned(self.tx.clone(), event, event_kind, policy)
            .map_err(|(error, event)| (error, Box::new(event)));
        match &result {
            Ok(DeliveryReceipt::Deferred) => tracing::warn!(
                owner = self.owner,
                event_policy = policy.name(),
                event_lane = policy.lane().map(EventLane::name).unwrap_or("none"),
                event_kind,
                delivery_outcome = "deferred",
                "owner event queue full; deferred must-deliver event"
            ),
            Err((error, _)) => log_rejected(self.owner, event_kind, policy, *error),
            Ok(_) => {}
        }
        result
    }

    /// Apply bounded backpressure for callback APIs which cannot return an owned rejection.
    ///
    /// Ordinary callers should use [`Self::emit`] or [`Self::emit_must_deliver_owned`] and surface
    /// a busy result. A callback has no such reply path, so consuming a saturated terminal event
    /// would turn an explicit rejection into silent loss. Keep that one payload on the producer's
    /// stack and retry until the bounded owner/spill lanes accept it or the owner closes.
    pub(crate) fn emit_callback_blocking(&self, event: E) -> DeliveryResult {
        let cancellation = CallbackCancellation::new();
        self.emit_callback_blocking_until(event, &cancellation)
    }

    /// Apply callback-owned backpressure until this producer generation is cancelled.
    ///
    /// Lifecycle cancellation is reported as [`DeliveryError::Closed`] even while the global
    /// owner queue remains open. The owned variant below is the source of truth so cancellation
    /// and admission can never both consume the same event.
    pub(crate) fn emit_callback_blocking_until(
        &self,
        event: E,
        cancellation: &CallbackCancellation,
    ) -> DeliveryResult {
        if cancellation.is_cancelled() {
            return Err(DeliveryError::Closed);
        }
        if !matches!(event.policy(), EventPolicy::MustDeliver { .. }) {
            return self.emit(event);
        }
        self.emit_callback_owned_until(event, cancellation)
            .map_err(|(error, _event)| error)
    }

    /// Retain the exact must-deliver callback payload until admission, owner close, or producer
    /// generation cancellation. On rejection ownership is returned to the caller.
    pub(crate) fn emit_callback_owned_until(
        &self,
        event: E,
        cancellation: &CallbackCancellation,
    ) -> Result<DeliveryReceipt, (DeliveryError, Box<E>)> {
        let policy = event.policy();
        if !matches!(policy, EventPolicy::MustDeliver { .. }) {
            let error = direct_rejection(policy);
            log_rejected(self.owner, event.kind(), policy, error);
            return Err((error, Box::new(event)));
        }

        // Callback producers can be Tokio actors. Tell a multi-thread runtime that this worker
        // may block so it can keep scheduling the owner which releases channel capacity. The
        // daemon and terminal owners both use multi-thread runtimes; native callback threads
        // simply execute the bounded wait directly.
        if let Ok(handle) = tokio::runtime::Handle::try_current()
            && handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        {
            return tokio::task::block_in_place(|| {
                self.wait_for_callback_capacity(event, policy, cancellation)
            });
        }

        self.wait_for_callback_capacity(event, policy, cancellation)
    }

    fn wait_for_callback_capacity(
        &self,
        event: E,
        policy: EventPolicy,
        cancellation: &CallbackCancellation,
    ) -> Result<DeliveryReceipt, (DeliveryError, Box<E>)> {
        debug_assert!(matches!(policy, EventPolicy::MustDeliver { .. }));

        let event_kind = event.kind();
        let mut event = event;
        let mut reported_wait = false;
        loop {
            if cancellation.is_cancelled() {
                tracing::debug!(
                    owner = self.owner,
                    event_policy = policy.name(),
                    event_lane = policy.lane().map(EventLane::name).unwrap_or("none"),
                    event_kind,
                    delivery_outcome = "callback_generation_cancelled",
                    "callback terminal event released during producer teardown"
                );
                return Err((DeliveryError::Closed, Box::new(event)));
            }
            let Some(admission) = self.admission.while_open() else {
                log_rejected(self.owner, event_kind, policy, DeliveryError::Closed);
                return Err((DeliveryError::Closed, Box::new(event)));
            };
            let attempt = self
                .deferred
                .admit_owned(self.tx.clone(), event, event_kind, policy);
            drop(admission);
            match attempt {
                Ok(receipt) => {
                    if reported_wait {
                        tracing::debug!(
                            owner = self.owner,
                            event_policy = policy.name(),
                            event_lane = policy.lane().map(EventLane::name).unwrap_or("none"),
                            event_kind,
                            delivery_outcome = "callback_backpressure_admitted",
                            "callback terminal event admitted after bounded backpressure"
                        );
                    }
                    return Ok(receipt);
                }
                Err((DeliveryError::Saturated, rejected)) => {
                    event = rejected;
                    if !reported_wait {
                        reported_wait = true;
                        tracing::warn!(
                            owner = self.owner,
                            event_policy = policy.name(),
                            event_lane = policy.lane().map(EventLane::name).unwrap_or("none"),
                            event_kind,
                            delivery_outcome = "callback_backpressure_wait",
                            "callback terminal event is waiting for bounded owner capacity"
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err((error, rejected)) => {
                    log_rejected(self.owner, event_kind, policy, error);
                    return Err((error, Box::new(rejected)));
                }
            }
        }
    }

    pub(crate) fn drain_coalesced(&self) -> Vec<E> {
        self.coalesced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
    }

    /// Monotonically reject new producers without closing the receiver. Already accepted main,
    /// deferred, and coalesced events remain owned and can be drained by the shutdown owner.
    pub(crate) fn close_admission(&self) -> bool {
        self.admission.close()
    }

    /// True once the single deferred drainer owns no queued or in-flight event. Callers must
    /// close admission first; otherwise a new producer could invalidate an observed idle state.
    pub(crate) fn deferred_is_idle(&self) -> bool {
        self.deferred.is_idle()
    }

    fn emit_coalesced(&self, event: E, policy: EventPolicy) -> DeliveryResult {
        let event_kind = event.kind();
        let Some(key) = event.coalesce_key() else {
            return self.emit_direct(event, policy);
        };
        let mut coalesced = self
            .coalesced
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.tx.is_closed() {
            coalesced.clear();
            self.deferred.mark_closed();
            return Err(DeliveryError::Closed);
        }
        let insert = coalesced.insert_prioritized(
            key,
            event,
            match policy {
                EventPolicy::CoalesceLatest {
                    lane: EventLane::Telemetry,
                    ..
                } => LatestEventClass::Telemetry,
                EventPolicy::CoalesceLatest { .. } => LatestEventClass::Protected,
                EventPolicy::DropIfStale { .. } => LatestEventClass::StaleResult,
                _ => LatestEventClass::Telemetry,
            },
        );
        if !insert.accepted {
            let error = direct_rejection(policy);
            log_rejected(self.owner, event_kind, policy, error);
            return Err(error);
        }
        if insert.replaced_existing || insert.evicted_oldest {
            tracing::trace!(
                owner = self.owner,
                event_policy = policy.name(),
                event_lane = policy.lane().map(EventLane::name).unwrap_or("none"),
                event_kind,
                coalesce_key = policy.key().map(|key| key.name()).unwrap_or("dynamic"),
                delivery_outcome = if insert.evicted_oldest {
                    "coalesced_evicted_oldest"
                } else {
                    "coalesced"
                },
                "owner event coalesced"
            );
        }
        if insert.should_wake {
            let wake = E::wake_event();
            let wake_policy = wake.policy();
            if let Err(error) = self.emit_direct(wake, wake_policy) {
                if error == DeliveryError::Closed {
                    coalesced.clear();
                    return Err(error);
                } else if self
                    .deferred
                    .latch_wake(
                        self.tx.clone(),
                        E::wake_event(),
                        E::wake_event().kind(),
                        wake_policy,
                    )
                    .is_ok()
                {
                    // Keep LatestEventBuffer::wake_pending armed: the single bounded latch is
                    // now ordered behind every already-deferred owner event and will wake this
                    // exact buffered generation without requiring another producer.
                } else {
                    coalesced.rearm_wake();
                    return Err(error);
                }
            }
        }
        Ok(DeliveryReceipt::Coalesced {
            replaced_existing: insert.replaced_existing,
            evicted_oldest: insert.evicted_oldest,
        })
    }

    /// Keep stale-able work results behind their semantic key even while the owner
    /// lane has room. Mixing buffered and direct delivery would allow a newer direct
    /// result to overtake the deferred wake and then be overwritten by the older
    /// buffered value when that wake finally runs.
    fn emit_stale(&self, event: E, policy: EventPolicy) -> DeliveryResult {
        if event.coalesce_key().is_some() {
            self.emit_coalesced(event, policy)
        } else {
            self.emit_direct(event, policy)
        }
    }

    fn emit_direct(&self, event: E, policy: EventPolicy) -> DeliveryResult {
        let event_kind = event.kind();
        if matches!(policy, EventPolicy::MustDeliver { .. }) {
            let result = self
                .deferred
                .admit(self.tx.clone(), event, event_kind, policy);
            match result {
                Ok(DeliveryReceipt::Deferred) => tracing::warn!(
                    owner = self.owner,
                    event_policy = policy.name(),
                    event_lane = policy.lane().map(EventLane::name).unwrap_or("none"),
                    event_kind,
                    delivery_outcome = "deferred",
                    "owner event queue full; deferred must-deliver event"
                ),
                Err(error) => log_rejected(self.owner, event_kind, policy, error),
                Ok(_) => {}
            }
            return result;
        }

        let result = self.deferred.try_direct(&self.tx, event, policy);
        if let Err(error) = result {
            log_rejected(self.owner, event_kind, policy, error);
        }
        result
    }
}

struct DeferredEvent<E> {
    event: E,
    event_kind: &'static str,
    policy: EventPolicy,
}

struct DeferredQueue<E> {
    owner: &'static str,
    capacity: usize,
    control_reserve: usize,
    state: Mutex<DeferredState<E>>,
    #[cfg(test)]
    fail_next_drainer_spawn: AtomicBool,
}

struct DeferredState<E> {
    queue: VecDeque<DeferredEvent<E>>,
    /// One wake is sufficient for every keyed latest-value buffered behind it. Keeping this
    /// outside the ordinary capacity prevents a full spill queue from stranding a one-shot
    /// completion forever, while remaining strictly bounded and FIFO-after-current-work.
    latched_wake: Option<LatchedWake<E>>,
    /// The OS drainer removes one ordinary event before `blocking_send` can complete. Keep
    /// that accepted event in both total-capacity and lane-reserve accounting until delivery.
    in_flight_lane: Option<EventLane>,
    non_control_len: usize,
    drainer_running: bool,
    closed: bool,
}

struct LatchedWake<E> {
    event: DeferredEvent<E>,
    /// Number of ordinary events that were already queued when this wake was latched. New
    /// arrivals append after the sequence point and cannot starve the wake.
    remaining_before: usize,
}

impl<E: Send + 'static> DeferredQueue<E> {
    fn new(owner: &'static str, capacity: usize) -> Self {
        let control_reserve = match capacity {
            0 => 0,
            1..=31 => 1,
            _ => DEFAULT_CONTROL_RESERVE.min(capacity),
        };
        Self {
            owner,
            capacity,
            control_reserve,
            state: Mutex::new(DeferredState {
                queue: VecDeque::new(),
                latched_wake: None,
                in_flight_lane: None,
                non_control_len: 0,
                drainer_running: false,
                closed: false,
            }),
            #[cfg(test)]
            fail_next_drainer_spawn: AtomicBool::new(false),
        }
    }

    /// Admit must-deliver events under one lock so a newly available owner slot
    /// cannot let a later event overtake an earlier deferred event.
    fn admit(
        self: &Arc<Self>,
        tx: mpsc::Sender<E>,
        event: E,
        event_kind: &'static str,
        policy: EventPolicy,
    ) -> DeliveryResult {
        self.admit_owned(tx, event, event_kind, policy)
            .map_err(|(error, _event)| error)
    }

    fn admit_owned(
        self: &Arc<Self>,
        tx: mpsc::Sender<E>,
        event: E,
        event_kind: &'static str,
        policy: EventPolicy,
    ) -> Result<DeliveryReceipt, (DeliveryError, E)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if state.closed || tx.is_closed() {
            state.mark_closed();
            return Err((DeliveryError::Closed, event));
        }

        if !state.drainer_running && state.queue.is_empty() {
            match tx.try_send(event) {
                Ok(()) => return Ok(DeliveryReceipt::Enqueued),
                Err(TrySendError::Closed(event)) => {
                    state.mark_closed();
                    return Err((DeliveryError::Closed, event));
                }
                Err(TrySendError::Full(event)) => {
                    if !self.has_capacity(&state, policy) {
                        return Err((DeliveryError::Saturated, event));
                    }
                    state.push(DeferredEvent {
                        event,
                        event_kind,
                        policy,
                    });
                }
            }
        } else {
            if !self.has_capacity(&state, policy) {
                return Err((DeliveryError::Saturated, event));
            }
            state.push(DeferredEvent {
                event,
                event_kind,
                policy,
            });
        }

        if !state.drainer_running {
            state.drainer_running = true;
            // Keep the state lock across spawn publication. A second producer must not receive
            // `Deferred` until there is a real drainer that owns every queued event.
            if !self.spawn_drainer(tx) {
                let rejected = state
                    .queue
                    .pop_back()
                    .expect("current must-deliver event was queued before drainer spawn");
                state.clear_open();
                return Err((DeliveryError::Saturated, rejected.event));
            }
        }
        Ok(DeliveryReceipt::Deferred)
    }

    /// Direct/reply-or-busy work shares the deferred-state lock so it cannot jump into a newly
    /// freed owner slot ahead of an earlier must-deliver event whose drainer has not polled yet.
    fn try_direct(&self, tx: &mpsc::Sender<E>, event: E, policy: EventPolicy) -> DeliveryResult {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || tx.is_closed() {
            state.mark_closed();
            return Err(DeliveryError::Closed);
        }
        if state.drainer_running || !state.queue.is_empty() || state.latched_wake.is_some() {
            return Err(direct_rejection(policy));
        }
        match tx.try_send(event) {
            Ok(()) => Ok(DeliveryReceipt::Enqueued),
            Err(TrySendError::Full(_)) => Err(direct_rejection(policy)),
            Err(TrySendError::Closed(_)) => {
                state.mark_closed();
                Err(DeliveryError::Closed)
            }
        }
    }

    fn has_capacity(&self, state: &DeferredState<E>, policy: EventPolicy) -> bool {
        let ordinary_len = state.queue.len() + usize::from(state.in_flight_lane.is_some());
        if ordinary_len >= self.capacity {
            return false;
        }
        if policy.lane() == Some(EventLane::Control) {
            return true;
        }
        state.non_control_len < self.capacity.saturating_sub(self.control_reserve)
    }

    fn latch_wake(
        self: &Arc<Self>,
        tx: mpsc::Sender<E>,
        event: E,
        event_kind: &'static str,
        policy: EventPolicy,
    ) -> DeliveryResult {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || tx.is_closed() {
            state.mark_closed();
            return Err(DeliveryError::Closed);
        }
        if state.latched_wake.is_some() {
            return Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            });
        }
        state.latched_wake = Some(LatchedWake {
            event: DeferredEvent {
                event,
                event_kind,
                policy,
            },
            remaining_before: state.queue.len(),
        });
        if !state.drainer_running {
            state.drainer_running = true;
            if !self.spawn_drainer(tx) {
                state.clear_open();
                return Err(DeliveryError::Saturated);
            }
        }
        Ok(DeliveryReceipt::Deferred)
    }

    fn pop_or_stop(&self) -> Option<DeferredEvent<E>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            state.mark_closed();
            return None;
        }
        debug_assert!(state.in_flight_lane.is_none());
        if state
            .latched_wake
            .as_ref()
            .is_some_and(|wake| wake.remaining_before == 0)
        {
            return state.latched_wake.take().map(|wake| wake.event);
        }
        match state.pop_front() {
            Some(item) => {
                if let Some(wake) = state.latched_wake.as_mut() {
                    wake.remaining_before = wake.remaining_before.saturating_sub(1);
                }
                state.in_flight_lane = item.policy.lane();
                Some(item)
            }
            None => {
                debug_assert!(state.latched_wake.is_none());
                state.drainer_running = false;
                None
            }
        }
    }

    fn complete_in_flight(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .in_flight_lane
            .take()
            .is_some_and(|lane| lane != EventLane::Control)
        {
            state.non_control_len = state.non_control_len.saturating_sub(1);
        }
    }

    fn spawn_drainer(self: &Arc<Self>, tx: mpsc::Sender<E>) -> bool {
        #[cfg(test)]
        if self.fail_next_drainer_spawn.swap(false, Ordering::SeqCst) {
            return false;
        }

        // This drainer must outlive whatever Tokio runtime happened to admit the event. A
        // detached async task can be cancelled while owning an item when that runtime drops,
        // turning a successful `Deferred` receipt into silent loss. One short-lived OS thread
        // per saturated owner is runtime-independent and the `drainer_running` latch keeps the
        // count bounded to one.
        let queue = Arc::clone(self);
        let spawn = std::thread::Builder::new()
            .name(format!("ytt-{}-delivery", self.owner))
            .spawn(move || {
                while let Some(item) = queue.pop_or_stop() {
                    let send_failed = tx.blocking_send(item.event).is_err();
                    queue.complete_in_flight();
                    if send_failed {
                        queue.mark_closed();
                        log_rejected(
                            queue.owner,
                            item.event_kind,
                            item.policy,
                            DeliveryError::Closed,
                        );
                        break;
                    }
                }
            });
        if let Err(error) = spawn {
            tracing::error!(
                owner = self.owner,
                %error,
                delivery_outcome = "drainer_spawn_failed",
                "could not spawn owner event delivery drainer"
            );
            false
        } else {
            true
        }
    }

    #[cfg(test)]
    fn fail_next_drainer_spawn(&self) {
        self.fail_next_drainer_spawn.store(true, Ordering::SeqCst);
    }

    fn mark_closed(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.mark_closed();
    }

    fn is_idle(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.drainer_running
            && state.queue.is_empty()
            && state.latched_wake.is_none()
            && state.in_flight_lane.is_none()
    }
}

impl<E> DeferredState<E> {
    fn push(&mut self, item: DeferredEvent<E>) {
        if item.policy.lane() != Some(EventLane::Control) {
            self.non_control_len += 1;
        }
        self.queue.push_back(item);
    }

    fn pop_front(&mut self) -> Option<DeferredEvent<E>> {
        self.queue.pop_front()
    }

    fn mark_closed(&mut self) {
        self.clear_open();
        self.closed = true;
    }

    fn clear_open(&mut self) {
        self.queue.clear();
        self.latched_wake = None;
        self.in_flight_lane = None;
        self.non_control_len = 0;
        self.drainer_running = false;
    }
}

const fn direct_rejection(policy: EventPolicy) -> DeliveryError {
    match policy {
        EventPolicy::MustReplyOrBusy { .. } => DeliveryError::Busy,
        EventPolicy::DropIfStale { .. } => DeliveryError::StaleOrFull,
        EventPolicy::BestEffort { .. } => DeliveryError::BestEffortDropped,
        EventPolicy::CoalesceLatest { .. } | EventPolicy::MustDeliver { .. } => {
            DeliveryError::Saturated
        }
    }
}

fn log_rejected(
    owner: &'static str,
    event_kind: &'static str,
    policy: EventPolicy,
    error: DeliveryError,
) {
    tracing::warn!(
        owner,
        event_policy = policy.name(),
        event_lane = policy.lane().map(EventLane::name).unwrap_or("none"),
        event_kind,
        coalesce_key = policy.key().map(|key| key.name()).unwrap_or("none"),
        delivery_outcome = error.reason(),
        "owner event was not accepted"
    );
}

#[cfg(test)]
mod tests;
