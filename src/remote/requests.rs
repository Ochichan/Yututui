//! Bounded idempotency registry for remote mutations.
//!
//! A control-socket timeout only means the caller stopped waiting; the owner loop may still
//! admit and finish the command.  Retrying such a command as fresh work can therefore apply a
//! mutation twice.  This registry keeps the owner reply receiver alive beyond an individual
//! caller deadline and lets a retry with the same request identity join the original execution.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Notify, oneshot};

use super::proto::{
    CONFIRMATION_LOST_REASON, MAX_ONESHOT_REPLY_BYTES, RemoteCommand, RemoteResponse,
    RequestRetryClass,
};

/// Request identities are deliberately small: they cross a local trust boundary and become
/// cache keys. The generated form is 32 hex characters; the extra room supports namespaced
/// clients.
pub(crate) const MAX_REQUEST_ID_BYTES: usize = 128;
pub(crate) const MAX_PAGE_ID_BYTES: usize = 64;

/// Owner work may remain pending indefinitely, so it has an independent hard count and
/// actual-retained-byte budget. Saturation is reported before owner admission.
const MAX_PENDING_ENTRIES: usize = 64;
const MAX_PENDING_BYTES: usize = 16 * 1024 * 1024;

/// Compact replies are the normal control path. This tier retains more than 68 replies/second
/// for the full 60-second retry horizon, while both count and retained bytes stay bounded.
const MAX_COMPACT_COMPLETED_ENTRIES: usize = 4_096;
const MAX_COMPACT_COMPLETED_BYTES: usize = 24 * 1024 * 1024;
const COMPACT_RESPONSE_MAX_BYTES: usize = 4 * 1024;

/// Unexpectedly large mutation replies cannot consume the compact-control budget. Read-only
/// status snapshots bypass retention altogether, so compacting this tier never weakens the
/// `status` response contract. Once the tier is protected and full, a completed mutation keeps
/// the same success/rejection outcome with oversized diagnostic detail bounded, so a same-ID
/// retry remains truthful and replay-safe.
const MAX_LARGE_COMPLETED_ENTRIES: usize = 64;
const MAX_LARGE_COMPLETED_BYTES: usize = 16 * 1024 * 1024;
const MAX_COMPACT_MESSAGE_BYTES: usize = 2 * 1024;

/// Conservative allocator/container slack included in every accounted entry in addition to its
/// exact boxed fingerprint, request-id, and serialized-response lengths.
const ENTRY_ACCOUNTING_SLACK_BYTES: usize = 128;

/// Keep a completed identity long enough for the current client to finish its original exchange
/// and its one direct same-ID retry. The longest client attempt is 22.5 seconds (500 ms connect,
/// 2 s request write, and a 20 s playback reply), so two complete attempts fit inside 45 seconds.
/// Sixty seconds leaves scheduler margin while keeping the retained payload bound unchanged.
const MIN_COMPLETED_RETENTION: Duration = Duration::from_secs(60);

type MonotonicNow = Arc<dyn Fn() -> Duration + Send + Sync>;

#[derive(Clone, Copy)]
struct CacheLimits {
    pending_entries: usize,
    pending_bytes: usize,
    compact_completed_entries: usize,
    compact_completed_bytes: usize,
    compact_response_bytes: usize,
    large_completed_entries: usize,
    large_completed_bytes: usize,
    completed_retention: Duration,
}

const PRODUCTION_LIMITS: CacheLimits = CacheLimits {
    pending_entries: MAX_PENDING_ENTRIES,
    pending_bytes: MAX_PENDING_BYTES,
    compact_completed_entries: MAX_COMPACT_COMPLETED_ENTRIES,
    compact_completed_bytes: MAX_COMPACT_COMPLETED_BYTES,
    compact_response_bytes: COMPACT_RESPONSE_MAX_BYTES,
    large_completed_entries: MAX_LARGE_COMPLETED_ENTRIES,
    large_completed_bytes: MAX_LARGE_COMPLETED_BYTES,
    completed_retention: MIN_COMPLETED_RETENTION,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RequestKey {
    /// Stable across connections when a client supplies an explicit request identity.
    Stable(String),
    /// Backward-compatible session identity for v8 clients that only send `ClientFrame::id`.
    Session { session_id: u64, frame_id: u64 },
}

impl RequestKey {
    pub(crate) fn stable(value: String) -> Option<Self> {
        valid_request_id(&value).then_some(Self::Stable(value))
    }

    fn dynamic_bytes(&self) -> usize {
        match self {
            Self::Stable(value) => value.capacity(),
            Self::Session { .. } => 0,
        }
    }
}

pub(crate) fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_REQUEST_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

pub(crate) fn valid_page_id(value: &str) -> bool {
    value.len() <= MAX_PAGE_ID_BYTES && valid_request_id(value)
}

/// Mint a process-local request namespace without adding a UUID dependency. OS randomness is the
/// normal path; the fallback only needs collision resistance (the control token is still the
/// authentication boundary), so time + pid + an atomic counter is sufficient when entropy fails.
pub(crate) fn fresh_request_id() -> String {
    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_ok() {
        return hex_id(bytes);
    }

    static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = u128::from(FALLBACK_COUNTER.fetch_add(1, Ordering::Relaxed));
    let pid = u128::from(std::process::id());
    format!("{:032x}", now ^ (pid << 64) ^ counter)
}

fn hex_id(bytes: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

struct RequestEntry {
    command_fingerprint: Box<[u8]>,
    pending_bytes: usize,
    completed_base_bytes: usize,
    compact_reservation_bytes: usize,
    completion: Mutex<RequestCompletion>,
    ready: Notify,
}

struct RequestCompletion {
    response_json: Option<Box<[u8]>>,
}

impl RequestEntry {
    fn new(key: &RequestKey, command_fingerprint: Box<[u8]>, limits: CacheLimits) -> Option<Self> {
        let pending_bytes = std::mem::size_of::<(RequestKey, Arc<RequestEntry>)>()
            .checked_add(std::mem::size_of::<RequestEntry>())?
            .checked_add(key.dynamic_bytes())?
            .checked_add(command_fingerprint.len())?
            .checked_add(ENTRY_ACCOUNTING_SLACK_BYTES)?;
        let completed_base_bytes = pending_bytes
            .checked_add(std::mem::size_of::<CompletedRecord>())?
            .checked_add(key.dynamic_bytes())?
            .checked_add(ENTRY_ACCOUNTING_SLACK_BYTES)?;
        let compact_reservation_bytes =
            completed_base_bytes.checked_add(limits.compact_response_bytes)?;
        Some(Self {
            command_fingerprint,
            pending_bytes,
            completed_base_bytes,
            compact_reservation_bytes,
            completion: Mutex::new(RequestCompletion {
                response_json: None,
            }),
            ready: Notify::new(),
        })
    }

    fn complete(&self, response_json: Box<[u8]>) {
        let mut completion = self
            .completion
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if completion.response_json.is_none() {
            completion.response_json = Some(response_json);
            drop(completion);
            self.ready.notify_waiters();
        }
    }

    async fn response(&self) -> RemoteResponse {
        loop {
            // `notify_waiters` does not store a permit for a future that has only been created.
            // Enable the waiter before checking the slot so completion between the check and
            // first poll cannot strand a duplicate request forever.
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let response = {
                let completion = self
                    .completion
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                completion.response_json.as_deref().map(|encoded| {
                    serde_json::from_slice(encoded)
                        .unwrap_or_else(|_| RemoteResponse::err("timeout"))
                })
            };
            if let Some(response) = response {
                return response;
            }
            notified.as_mut().await;
        }
    }
}

#[derive(Clone, Copy)]
enum CompletedTier {
    Compact,
    Large,
}

struct CompletedRecord {
    key: RequestKey,
    completed_at: Duration,
    tier: CompletedTier,
    retained_bytes: usize,
}

struct RequestState {
    entries: HashMap<RequestKey, Arc<RequestEntry>>,
    completed_order: VecDeque<CompletedRecord>,
    pending_entries: usize,
    pending_bytes: usize,
    compact_reserved_bytes: usize,
    compact_completed_entries: usize,
    compact_completed_bytes: usize,
    large_completed_entries: usize,
    large_completed_bytes: usize,
}

fn encoded_error(reason: &str) -> Box<[u8]> {
    serde_json::to_vec(&RemoteResponse::err(reason))
        .expect("RemoteResponse error serialization is infallible")
        .into_boxed_slice()
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_owned()
}

fn compact_semantic_response(response: &RemoteResponse) -> RemoteResponse {
    RemoteResponse {
        ok: response.ok,
        // Machine reasons are stable protocol codes and therefore take priority over the human
        // message. They stay byte-for-byte intact unless a malformed oversized reason alone
        // cannot fit the compact wire budget.
        reason: response.reason.clone(),
        message: response
            .message
            .as_deref()
            .map(|message| truncate_utf8(message, MAX_COMPACT_MESSAGE_BYTES)),
        status: None,
    }
}

fn compact_semantic_json(response: &RemoteResponse, max_bytes: usize) -> Box<[u8]> {
    let mut compact = compact_semantic_response(response);
    loop {
        let encoded = serde_json::to_vec(&compact)
            .expect("compacted RemoteResponse serialization is infallible");
        if encoded.len() <= max_bytes {
            return encoded.into_boxed_slice();
        }
        if let Some(message) = compact.message.as_deref()
            && !message.is_empty()
        {
            let reduced = truncate_utf8(message, message.len() / 2);
            compact.message = (!reduced.is_empty()).then_some(reduced);
            continue;
        }
        if let Some(reason) = compact.reason.as_deref()
            && !reason.is_empty()
        {
            let reduced = truncate_utf8(reason, reason.len() / 2);
            compact.reason = (!reduced.is_empty()).then_some(reduced);
            continue;
        }
        // `CacheLimits` rejects configurations too small for this field-minimal shape.
        return encoded.into_boxed_slice();
    }
}

fn bounded_response_json(response: &RemoteResponse, compact_max_bytes: usize) -> Box<[u8]> {
    match serde_json::to_vec(response) {
        Ok(encoded) if encoded.len() <= MAX_ONESHOT_REPLY_BYTES => encoded.into_boxed_slice(),
        _ => compact_semantic_json(response, compact_max_bytes),
    }
}

fn exceeds_budget(used: usize, additional: usize, limit: usize) -> bool {
    used.checked_add(additional).is_none_or(|next| next > limit)
}

fn purge_expired(state: &mut RequestState, now: Duration, retention: Duration) {
    while state
        .completed_order
        .front()
        .is_some_and(|record| now.saturating_sub(record.completed_at) >= retention)
    {
        let record = state
            .completed_order
            .pop_front()
            .expect("front was present");
        state.entries.remove(&record.key);
        match record.tier {
            CompletedTier::Compact => {
                state.compact_completed_entries -= 1;
                state.compact_completed_bytes -= record.retained_bytes;
            }
            CompletedTier::Large => {
                state.large_completed_entries -= 1;
                state.large_completed_bytes -= record.retained_bytes;
            }
        }
    }
}

fn complete_retained(
    state: &Mutex<RequestState>,
    limits: CacheLimits,
    now: &MonotonicNow,
    key: RequestKey,
    entry: Arc<RequestEntry>,
    response: RemoteResponse,
) {
    let mut response_json = bounded_response_json(&response, limits.compact_response_bytes);
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !state
        .entries
        .get(&key)
        .is_some_and(|current| Arc::ptr_eq(current, &entry))
    {
        drop(state);
        entry.complete(encoded_error("server_busy"));
        return;
    }

    let mut retained_bytes = entry
        .completed_base_bytes
        .checked_add(response_json.len())
        .expect("admitted response accounting cannot overflow");
    let tier = if response_json.len() <= limits.compact_response_bytes {
        CompletedTier::Compact
    } else if state.large_completed_entries < limits.large_completed_entries
        && !exceeds_budget(
            state.large_completed_bytes,
            retained_bytes,
            limits.large_completed_bytes,
        )
    {
        CompletedTier::Large
    } else {
        // The owner has already executed, so never erase this identity or invite a duplicate
        // execution. Retain one stable compact result for the original caller and every retry.
        response_json = compact_semantic_json(&response, limits.compact_response_bytes);
        retained_bytes = entry
            .completed_base_bytes
            .checked_add(response_json.len())
            .expect("compact fallback accounting cannot overflow");
        CompletedTier::Compact
    };

    state.pending_entries -= 1;
    state.pending_bytes -= entry.pending_bytes;
    state.compact_reserved_bytes -= entry.compact_reservation_bytes;
    match tier {
        CompletedTier::Compact => {
            debug_assert!(retained_bytes <= entry.compact_reservation_bytes);
            debug_assert!(
                !exceeds_budget(
                    state.compact_completed_bytes,
                    retained_bytes,
                    limits.compact_completed_bytes,
                ),
                "compact completion exceeded its admission reservation"
            );
            state.compact_completed_entries += 1;
            state.compact_completed_bytes += retained_bytes;
        }
        CompletedTier::Large => {
            state.large_completed_entries += 1;
            state.large_completed_bytes += retained_bytes;
        }
    }
    state.completed_order.push_back(CompletedRecord {
        key,
        completed_at: now(),
        tier,
        retained_bytes,
    });
    drop(state);
    entry.complete(response_json);
}

enum BeginRequest {
    First(Arc<RequestEntry>),
    Retry(Arc<RequestEntry>),
    Conflict,
    Full,
    Invalid,
}

/// Internal result metadata for a one-shot caller that must distinguish a retained same-ID
/// outcome from a fresh response issued by whichever process currently owns the socket.
pub(crate) struct RetainedRequestOutcome {
    pub(crate) response: RemoteResponse,
    pub(crate) retained_replay: bool,
}

pub(crate) struct CommandDeduper {
    state: Arc<Mutex<RequestState>>,
    limits: CacheLimits,
    now: MonotonicNow,
}

impl Default for CommandDeduper {
    fn default() -> Self {
        let epoch = Instant::now();
        Self::with_clock(PRODUCTION_LIMITS, Arc::new(move || epoch.elapsed()))
    }
}

impl CommandDeduper {
    #[cfg(test)]
    fn new(capacity: usize) -> Self {
        let epoch = Instant::now();
        let limits = CacheLimits {
            pending_entries: capacity,
            compact_completed_entries: capacity,
            large_completed_entries: capacity,
            ..PRODUCTION_LIMITS
        };
        Self::with_clock(limits, Arc::new(move || epoch.elapsed()))
    }

    fn with_clock(limits: CacheLimits, now: MonotonicNow) -> Self {
        debug_assert!(limits.pending_entries > 0);
        debug_assert!(limits.compact_completed_entries > 0);
        debug_assert!(
            limits.compact_response_bytes
                >= serde_json::to_vec(&RemoteResponse {
                    ok: false,
                    reason: None,
                    message: None,
                    status: None,
                })
                .expect("minimal RemoteResponse serialization is infallible")
                .len()
        );
        Self {
            state: Arc::new(Mutex::new(RequestState {
                entries: HashMap::new(),
                completed_order: VecDeque::new(),
                pending_entries: 0,
                pending_bytes: 0,
                compact_reserved_bytes: 0,
                compact_completed_entries: 0,
                compact_completed_bytes: 0,
                large_completed_entries: 0,
                large_completed_bytes: 0,
            })),
            limits,
            now,
        }
    }

    #[cfg(test)]
    pub(crate) fn fill_with_completed_for_test(&self) {
        for index in 0..self.limits.compact_completed_entries {
            let key = RequestKey::Stable(format!("retained-test-{index}"));
            let BeginRequest::First(entry) = self.begin(key.clone(), &RemoteCommand::Status) else {
                panic!("test cache must begin empty");
            };
            complete_retained(
                &self.state,
                self.limits,
                &self.now,
                key,
                entry,
                RemoteResponse::ok("cached".to_owned()),
            );
        }
    }

    fn begin(&self, key: RequestKey, command: &RemoteCommand) -> BeginRequest {
        let Ok(fingerprint) = serde_json::to_vec(command).map(Vec::into_boxed_slice) else {
            return BeginRequest::Invalid;
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        purge_expired(&mut state, (self.now)(), self.limits.completed_retention);

        if let Some(entry) = state.entries.get(&key) {
            return if entry.command_fingerprint.as_ref() == fingerprint.as_ref() {
                BeginRequest::Retry(Arc::clone(entry))
            } else {
                BeginRequest::Conflict
            };
        }

        let Some(entry) = RequestEntry::new(&key, fingerprint, self.limits) else {
            return BeginRequest::Full;
        };
        if state.pending_entries >= self.limits.pending_entries
            || exceeds_budget(
                state.pending_bytes,
                entry.pending_bytes,
                self.limits.pending_bytes,
            )
            || state
                .compact_completed_entries
                .saturating_add(state.pending_entries)
                >= self.limits.compact_completed_entries
            || exceeds_budget(
                state
                    .compact_completed_bytes
                    .saturating_add(state.compact_reserved_bytes),
                entry.compact_reservation_bytes,
                self.limits.compact_completed_bytes,
            )
        {
            return BeginRequest::Full;
        }

        let entry = Arc::new(entry);
        state.pending_entries += 1;
        state.pending_bytes += entry.pending_bytes;
        state.compact_reserved_bytes += entry.compact_reservation_bytes;
        state.entries.insert(key, Arc::clone(&entry));
        BeginRequest::First(entry)
    }

    fn forget_unadmitted(&self, key: &RequestKey, entry: &Arc<RequestEntry>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .entries
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            state.entries.remove(key);
            state.pending_entries -= 1;
            state.pending_bytes -= entry.pending_bytes;
            state.compact_reserved_bytes -= entry.compact_reservation_bytes;
        }
    }

    /// Execute once for `key`, while allowing each caller to have its own response deadline.
    /// The proxy reply receiver lives in a detached task, so a timed-out caller does not cancel
    /// the owner response or erase the result a retry needs.
    #[cfg(test)]
    pub(crate) async fn execute<F>(
        &self,
        key: RequestKey,
        command: &RemoteCommand,
        reply_timeout: Duration,
        emit: F,
    ) -> RemoteResponse
    where
        F: FnOnce(oneshot::Sender<RemoteResponse>) -> bool,
    {
        self.execute_with_replay_proof(key, command, reply_timeout, emit)
            .await
            .response
    }

    /// Execute with transport proof indicating whether this caller observed the retained result
    /// of an earlier same-ID mutation admission. A timeout while merely waiting on that entry is
    /// not proof; the marker is true only after the retained response itself is available.
    pub(crate) async fn execute_with_replay_proof<F>(
        &self,
        key: RequestKey,
        command: &RemoteCommand,
        reply_timeout: Duration,
        emit: F,
    ) -> RetainedRequestOutcome
    where
        F: FnOnce(oneshot::Sender<RemoteResponse>) -> bool,
    {
        // Status is a read-only snapshot. RunSearch completes through a separately correlated
        // push. Retaining either immediate acknowledgement in the idempotency cache would let
        // large snapshots consume the mutation retry budget or replay an incomplete response.
        // Execute them independently, while still classifying a lost RunSearch acknowledgement as
        // ambiguous because its dispatch may already have happened.
        if command.request_retry_class() == RequestRetryClass::ReexecuteReadOnly {
            let (reply_tx, reply_rx) = oneshot::channel();
            if !emit(reply_tx) {
                return RetainedRequestOutcome {
                    response: RemoteResponse::err("server_busy"),
                    retained_replay: false,
                };
            }
            let response = match tokio::time::timeout(reply_timeout, reply_rx).await {
                Ok(Ok(response)) => response,
                _ => RemoteResponse::err(unconfirmed_reason(command)),
            };
            return RetainedRequestOutcome {
                response,
                retained_replay: false,
            };
        }

        let cache_key = key.clone();
        let (entry, first) = match self.begin(key, command) {
            BeginRequest::First(entry) => (entry, true),
            BeginRequest::Retry(entry) => (entry, false),
            BeginRequest::Conflict => {
                return RetainedRequestOutcome {
                    response: RemoteResponse::err("request_id_conflict"),
                    retained_replay: false,
                };
            }
            BeginRequest::Full => {
                return RetainedRequestOutcome {
                    response: RemoteResponse::err("server_busy"),
                    retained_replay: false,
                };
            }
            BeginRequest::Invalid => {
                return RetainedRequestOutcome {
                    response: RemoteResponse::err("bad_request"),
                    retained_replay: false,
                };
            }
        };

        if first {
            let (reply_tx, reply_rx) = oneshot::channel();
            if emit(reply_tx) {
                let completion = Arc::clone(&entry);
                let state = Arc::clone(&self.state);
                let limits = self.limits;
                let now = Arc::clone(&self.now);
                let completion_key = cache_key.clone();
                let unconfirmed_reason = unconfirmed_reason(command);
                tokio::spawn(async move {
                    let response = reply_rx
                        .await
                        .unwrap_or_else(|_| RemoteResponse::err(unconfirmed_reason));
                    complete_retained(&state, limits, &now, completion_key, completion, response);
                });
            } else {
                entry.complete(encoded_error("server_busy"));
                // Nothing reached the owner, so the same identity may safely retry admission.
                // Keep the completed Arc alive for callers that already joined this attempt,
                // while removing it from future cache lookups.
                self.forget_unadmitted(&cache_key, &entry);
            }
        }

        match tokio::time::timeout(reply_timeout, entry.response()).await {
            Ok(response) => RetainedRequestOutcome {
                response,
                retained_replay: !first,
            },
            Err(_) => RetainedRequestOutcome {
                response: RemoteResponse::err(unconfirmed_reason(command)),
                retained_replay: false,
            },
        }
    }
}

fn unconfirmed_reason(command: &RemoteCommand) -> &'static str {
    if command.requires_confirmation() {
        CONFIRMATION_LOST_REASON
    } else {
        "timeout"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct TestClock {
        nanos: AtomicU64,
    }

    impl TestClock {
        fn now(&self) -> Duration {
            Duration::from_nanos(self.nanos.load(Ordering::Relaxed))
        }

        fn advance(&self, duration: Duration) {
            let nanos = u64::try_from(duration.as_nanos()).expect("test duration fits in u64");
            self.nanos.fetch_add(nanos, Ordering::Relaxed);
        }
    }

    fn cache_with_limits(limits: CacheLimits) -> (CommandDeduper, Arc<TestClock>) {
        let clock = Arc::new(TestClock::default());
        let now_clock = Arc::clone(&clock);
        let cache = CommandDeduper::with_clock(limits, Arc::new(move || now_clock.now()));
        (cache, clock)
    }

    fn cache_with_clock(capacity: usize) -> (CommandDeduper, Arc<TestClock>) {
        cache_with_limits(CacheLimits {
            pending_entries: capacity,
            compact_completed_entries: capacity,
            large_completed_entries: capacity,
            completed_retention: MIN_COMPLETED_RETENTION,
            ..PRODUCTION_LIMITS
        })
    }

    #[derive(Debug)]
    struct StateSnapshot {
        pending_entries: usize,
        pending_bytes: usize,
        compact_completed_entries: usize,
        compact_completed_bytes: usize,
        large_completed_entries: usize,
        large_completed_bytes: usize,
    }

    fn snapshot(cache: &CommandDeduper) -> StateSnapshot {
        let state = cache
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        StateSnapshot {
            pending_entries: state.pending_entries,
            pending_bytes: state.pending_bytes,
            compact_completed_entries: state.compact_completed_entries,
            compact_completed_bytes: state.compact_completed_bytes,
            large_completed_entries: state.large_completed_entries,
            large_completed_bytes: state.large_completed_bytes,
        }
    }

    #[tokio::test]
    async fn timeout_retry_joins_one_execution_and_observes_late_completion() {
        let cache = CommandDeduper::new(4);
        let emits = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(Mutex::new(None));
        let held_for_emit = Arc::clone(&held);
        let emits_for_first = Arc::clone(&emits);

        let first = cache
            .execute(
                RequestKey::Stable("same-request".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::ZERO,
                move |reply| {
                    emits_for_first.fetch_add(1, Ordering::Relaxed);
                    *held_for_emit
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reply);
                    true
                },
            )
            .await;
        assert_eq!(first.reason.as_deref(), Some(CONFIRMATION_LOST_REASON));

        let retry = cache.execute_with_replay_proof(
            RequestKey::Stable("same-request".to_owned()),
            &RemoteCommand::TogglePause,
            Duration::from_secs(1),
            |_| panic!("a retry with the same identity must not reach the owner twice"),
        );
        tokio::pin!(retry);
        assert!(matches!(
            futures::poll!(&mut retry),
            std::task::Poll::Pending
        ));

        held.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("first execution retained its reply sender")
            .send(RemoteResponse::ok("late success".to_owned()))
            .expect("dedupe task keeps the late receiver alive");

        let outcome = retry.await;
        assert!(
            outcome.retained_replay,
            "only the joined retained outcome may carry replay proof"
        );
        let response = outcome.response;
        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("late success"));
        assert_eq!(emits.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn pending_entries_are_never_evicted_when_cache_is_full() {
        let (cache, clock) = cache_with_clock(1);
        let held = Arc::new(Mutex::new(None));
        let held_for_emit = Arc::clone(&held);

        let first = cache
            .execute(
                RequestKey::Stable("pending".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::ZERO,
                move |reply| {
                    *held_for_emit
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reply);
                    true
                },
            )
            .await;
        assert_eq!(first.reason.as_deref(), Some(CONFIRMATION_LOST_REASON));

        clock.advance(MIN_COMPLETED_RETENTION + Duration::from_secs(1));

        let second = cache
            .execute(
                RequestKey::Stable("different".to_owned()),
                &RemoteCommand::Next,
                Duration::from_secs(1),
                |_| panic!("full cache must fail closed before owner admission"),
            )
            .await;
        assert_eq!(second.reason.as_deref(), Some("server_busy"));

        held.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("pending sender retained")
            .send(RemoteResponse::ok("done".to_owned()))
            .expect("completion receiver remains alive");
    }

    #[tokio::test]
    async fn production_limits_cover_minute_key_repeat_and_a_separate_pending_burst() {
        const REALISTIC_REPEAT_HZ: usize = 60;
        const RETENTION_SECONDS: usize = 60;
        let (cache, _clock) = cache_with_limits(PRODUCTION_LIMITS);

        for index in 0..REALISTIC_REPEAT_HZ * RETENTION_SECONDS {
            let response = cache
                .execute(
                    RequestKey::Stable(format!("key-repeat-{index}")),
                    &RemoteCommand::TogglePause,
                    Duration::from_secs(1),
                    |reply| {
                        reply
                            .send(RemoteResponse::ok("toggled".to_owned()))
                            .expect("dedupe receiver is live");
                        true
                    },
                )
                .await;
            assert!(response.ok, "compact completion {index} must be retained");
        }

        let after_repeat = snapshot(&cache);
        assert_eq!(
            after_repeat.compact_completed_entries,
            REALISTIC_REPEAT_HZ * RETENTION_SECONDS
        );
        assert_eq!(after_repeat.large_completed_entries, 0);
        assert_eq!(after_repeat.pending_entries, 0);
        assert!(after_repeat.compact_completed_bytes <= MAX_COMPACT_COMPLETED_BYTES);

        let held = Arc::new(Mutex::new(Vec::new()));
        for index in 0..MAX_PENDING_ENTRIES {
            let held_for_emit = Arc::clone(&held);
            let response = cache
                .execute(
                    RequestKey::Stable(format!("pending-burst-{index}")),
                    &RemoteCommand::TogglePause,
                    Duration::ZERO,
                    move |reply| {
                        held_for_emit
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(reply);
                        true
                    },
                )
                .await;
            assert_eq!(response.reason.as_deref(), Some(CONFIRMATION_LOST_REASON));
        }
        let pending = snapshot(&cache);
        assert_eq!(pending.pending_entries, MAX_PENDING_ENTRIES);
        assert!(pending.pending_bytes <= MAX_PENDING_BYTES);
        assert_eq!(
            pending.compact_completed_entries,
            REALISTIC_REPEAT_HZ * RETENTION_SECONDS,
            "pending capacity is independent from completed count"
        );

        let overflow = cache
            .execute(
                RequestKey::Stable("pending-burst-overflow".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                |_| panic!("pending saturation must reject before owner admission"),
            )
            .await;
        assert_eq!(overflow.reason.as_deref(), Some("server_busy"));

        let replies = std::mem::take(
            &mut *held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for reply in replies {
            let _ = reply.send(RemoteResponse::ok("finished".to_owned()));
        }
        let replay = cache
            .execute(
                RequestKey::Stable("pending-burst-0".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                |_| panic!("pending identity must join instead of re-executing"),
            )
            .await;
        assert!(replay.ok);
    }

    #[tokio::test]
    async fn production_status_queries_bypass_large_retention_without_losing_the_snapshot() {
        use crate::remote::proto::{QueueItemSnapshot, SettingsSnapshot, StatusSnapshot};

        let (cache, _clock) = cache_with_limits(PRODUCTION_LIMITS);
        let executions = AtomicUsize::new(0);

        for index in 0..=MAX_LARGE_COMPLETED_ENTRIES {
            let title = format!("status-{index}");
            let status = StatusSnapshot {
                title: Some(title.clone()),
                artist: Some("artist".to_owned()),
                paused: false,
                volume: 50,
                position: 1,
                total: 96,
                streaming: false,
                owner_mode: Default::default(),
                settings: SettingsSnapshot::default(),
                queue: (0..96)
                    .map(|row| QueueItemSnapshot {
                        title: format!("{row}-{}", "t".repeat(64)),
                        artist: "a".repeat(32),
                        duration: "3:00".to_owned(),
                        current: row == 0,
                    })
                    .collect(),
                shuffle: false,
                repeat: Default::default(),
                elapsed_ms: Some(1_000),
                duration_ms: Some(180_000),
                is_live: false,
                queue_rev: None,
                track_id: None,
                position_epoch: 0,
                artwork: None,
            };

            let response = cache
                .execute(
                    RequestKey::Stable(format!("large-status-{index}")),
                    &RemoteCommand::Status,
                    Duration::from_secs(1),
                    |reply| {
                        executions.fetch_add(1, Ordering::Relaxed);
                        reply
                            .send(RemoteResponse::status(status))
                            .expect("status query receiver is live");
                        true
                    },
                )
                .await;

            assert_eq!(
                response
                    .status
                    .as_ref()
                    .and_then(|status| status.title.as_ref()),
                Some(&title),
                "query {index} must retain its complete status contract"
            );
            assert!(
                serde_json::to_vec(&response).unwrap().len() > COMPACT_RESPONSE_MAX_BYTES,
                "fixture must exercise the former large-response tier"
            );
        }

        let retained = snapshot(&cache);
        assert_eq!(retained.pending_entries, 0);
        assert_eq!(retained.compact_completed_entries, 0);
        assert_eq!(retained.large_completed_entries, 0);
        assert_eq!(
            executions.load(Ordering::Relaxed),
            MAX_LARGE_COMPLETED_ENTRIES + 1
        );
    }

    #[tokio::test]
    async fn durability_unconfirmed_identity_replays_without_duplicate_execution_until_expiry() {
        let (cache, clock) = cache_with_clock(1);
        let executions = Arc::new(AtomicUsize::new(0));
        let executions_for_a = Arc::clone(&executions);
        let a = cache
            .execute(
                RequestKey::Stable("completed-a".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                move |reply| {
                    executions_for_a.fetch_add(1, Ordering::Relaxed);
                    reply
                        .send(RemoteResponse::err("durability_unconfirmed"))
                        .expect("dedupe receiver is live");
                    true
                },
            )
            .await;
        assert!(!a.ok);
        assert_eq!(a.reason.as_deref(), Some("durability_unconfirmed"));

        clock.advance(MIN_COMPLETED_RETENTION - Duration::from_nanos(1));
        let protected_b = cache
            .execute(
                RequestKey::Stable("new-b".to_owned()),
                &RemoteCommand::Next,
                Duration::from_secs(1),
                |_| panic!("protected completed identity must not be evicted"),
            )
            .await;
        assert_eq!(protected_b.reason.as_deref(), Some("server_busy"));

        let replayed_a = cache
            .execute(
                RequestKey::Stable("completed-a".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                |_| panic!("same-ID retry must replay without owner execution"),
            )
            .await;
        assert!(!replayed_a.ok);
        assert_eq!(replayed_a.reason.as_deref(), Some("durability_unconfirmed"));
        assert_eq!(executions.load(Ordering::Relaxed), 1);

        clock.advance(Duration::from_nanos(1));
        let executions_for_b = Arc::clone(&executions);
        let admitted_b = cache
            .execute(
                RequestKey::Stable("new-b".to_owned()),
                &RemoteCommand::Next,
                Duration::from_secs(1),
                move |reply| {
                    executions_for_b.fetch_add(1, Ordering::Relaxed);
                    reply
                        .send(RemoteResponse::ok("b".to_owned()))
                        .expect("dedupe receiver is live");
                    true
                },
            )
            .await;
        assert!(admitted_b.ok);
        assert_eq!(admitted_b.message.as_deref(), Some("b"));
        assert_eq!(executions.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn large_byte_pressure_keeps_success_truthful_replay_safe_and_expires() {
        let limits = CacheLimits {
            large_completed_entries: 8,
            large_completed_bytes: 14 * 1024,
            ..PRODUCTION_LIMITS
        };
        let (cache, clock) = cache_with_limits(limits);
        let large_message = "L".repeat(8 * 1024);

        let first = cache
            .execute(
                RequestKey::Stable("large-a".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                {
                    let large_message = large_message.clone();
                    move |reply| {
                        reply
                            .send(RemoteResponse::ok(large_message))
                            .expect("dedupe receiver is live");
                        true
                    }
                },
            )
            .await;
        assert_eq!(first.message.as_deref().map(str::len), Some(8 * 1024));

        let executions = Arc::new(AtomicUsize::new(0));
        let executions_for_emit = Arc::clone(&executions);
        let second = cache
            .execute(
                RequestKey::Stable("large-b".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                {
                    let large_message = large_message.clone();
                    move |reply| {
                        executions_for_emit.fetch_add(1, Ordering::Relaxed);
                        reply
                            .send(RemoteResponse {
                                ok: true,
                                reason: Some("applied".to_owned()),
                                message: Some(large_message),
                                status: None,
                            })
                            .expect("dedupe receiver is live");
                        true
                    }
                },
            )
            .await;
        assert!(second.ok, "executed mutation must remain a success");
        assert_eq!(second.reason.as_deref(), Some("applied"));
        assert_eq!(second.message.as_deref().map(str::len), Some(2 * 1024));

        let pressured = snapshot(&cache);
        assert_eq!(pressured.large_completed_entries, 1);
        assert!(pressured.large_completed_bytes > 8 * 1024);
        assert!(pressured.large_completed_bytes <= limits.large_completed_bytes);
        assert_eq!(pressured.compact_completed_entries, 1);
        assert!(pressured.compact_completed_bytes > 2 * 1024);

        let replay = cache
            .execute(
                RequestKey::Stable("large-b".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                |_| panic!("large-tier fallback must replay without owner execution"),
            )
            .await;
        assert_eq!(replay, second);
        assert_eq!(executions.load(Ordering::Relaxed), 1);

        let compact = cache
            .execute(
                RequestKey::Stable("compact-c".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                |reply| {
                    reply
                        .send(RemoteResponse::ok("compact".to_owned()))
                        .expect("dedupe receiver is live");
                    true
                },
            )
            .await;
        assert!(compact.ok, "large pressure must not block compact controls");

        clock.advance(MIN_COMPLETED_RETENTION + Duration::from_nanos(1));
        let after_expiry = cache
            .execute(
                RequestKey::Stable("large-after-expiry".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                {
                    let large_message = large_message.clone();
                    move |reply| {
                        reply
                            .send(RemoteResponse::ok(large_message))
                            .expect("dedupe receiver is live");
                        true
                    }
                },
            )
            .await;
        assert_eq!(
            after_expiry.message.as_deref().map(str::len),
            Some(8 * 1024),
            "expired large bytes must be reusable"
        );
        let expired = snapshot(&cache);
        assert_eq!(expired.large_completed_entries, 1);
        assert_eq!(expired.compact_completed_entries, 0);
    }

    #[tokio::test]
    async fn oversized_success_is_semantically_compacted_and_replayed() {
        let cache = CommandDeduper::new(1);
        let executions = Arc::new(AtomicUsize::new(0));
        let executions_for_emit = Arc::clone(&executions);
        let response = cache
            .execute(
                RequestKey::Stable("oversized-response".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                move |reply| {
                    executions_for_emit.fetch_add(1, Ordering::Relaxed);
                    reply
                        .send(RemoteResponse::ok("x".repeat(MAX_ONESHOT_REPLY_BYTES + 1)))
                        .expect("dedupe receiver is live");
                    true
                },
            )
            .await;

        assert!(response.ok);
        assert_eq!(response.reason.as_deref(), Some("ok"));
        assert_eq!(response.message.as_deref().map(str::len), Some(2_048));

        let replay = cache
            .execute(
                RequestKey::Stable("oversized-response".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                |_| panic!("compacted same-ID result must not execute twice"),
            )
            .await;
        assert_eq!(replay, response);
        assert_eq!(executions.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn owner_admission_rejection_does_not_poison_the_request_identity() {
        let cache = CommandDeduper::new(1);
        let first = cache
            .execute(
                RequestKey::Stable("retry-after-busy".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                |_| false,
            )
            .await;
        assert_eq!(first.reason.as_deref(), Some("server_busy"));

        let second = cache
            .execute(
                RequestKey::Stable("retry-after-busy".to_owned()),
                &RemoteCommand::TogglePause,
                Duration::from_secs(1),
                |reply| {
                    reply
                        .send(RemoteResponse::ok("admitted".to_owned()))
                        .expect("dedupe receiver is live");
                    true
                },
            )
            .await;
        assert!(second.ok);
        assert_eq!(second.message.as_deref(), Some("admitted"));
    }

    #[tokio::test]
    async fn gui_search_same_id_reexecutes_instead_of_replaying_dead_session_dispatch_ack() {
        let cache = CommandDeduper::new(1);
        let command = RemoteCommand::RunSearch {
            ticket: 1,
            query: "same query".to_owned(),
            source: crate::search_source::SearchSource::All,
        };
        let executions = Arc::new(AtomicUsize::new(0));
        for message in ["session-a", "session-b"] {
            let executions = Arc::clone(&executions);
            let response = cache
                .execute(
                    RequestKey::Stable("same-reconnect-id".to_owned()),
                    &command,
                    Duration::from_secs(1),
                    move |reply| {
                        executions.fetch_add(1, Ordering::Relaxed);
                        reply.send(RemoteResponse::ok(message.to_owned())).is_ok()
                    },
                )
                .await;
            assert_eq!(response.message.as_deref(), Some(message));
        }
        assert_eq!(
            executions.load(Ordering::Relaxed),
            2,
            "ReexecuteReadOnly is the typed exception to retained same-ID outcomes"
        );
    }

    #[test]
    fn generated_and_namespaced_request_ids_are_valid() {
        assert!(valid_request_id(&fresh_request_id()));
        assert!(valid_request_id("desktop:abcd:42"));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id("spaces are not allowed"));
        assert!(!valid_request_id(&"x".repeat(MAX_REQUEST_ID_BYTES + 1)));
    }
}
