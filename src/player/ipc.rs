//! The mpv IPC client: connect to the cross-platform local socket, then run the actor loop.
//!
//! mpv creates the endpoint after spawn, so [`connect_retry`] polls briefly. The actor reads
//! events and writes commands over one shared `interprocess` connection.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::BufReader;
use tokio::sync::{mpsc::Receiver, watch};
use tokio::time::{Duration, sleep};

use super::proto::{self, MpvIncoming};
use super::{EventSink, PlayerCmd, PlayerEvent};

mod audio_output;
mod wire;

use audio_output::evict_oldest_unprotected_pending;
pub(super) use wire::write_json;

#[cfg(test)]
use audio_output::{
    Selection as PendingAudioDeviceSelection, SelectionPhase as AudioDeviceSelectionPhase,
    fail_pending_commands, remember_refresh as remember_pending_audio_device_refresh,
    remember_selection as remember_pending_audio_device_selection,
    selection_command as audio_device_selection_command, timeout as timeout_audio_device_selection,
};

/// Upper bound on a single mpv IPC line. mpv's JSON events are well under a kilobyte; the
/// cap only guards against a broken/hostile endpoint growing one line without limit.
pub(crate) const MPV_IPC_MAX_LINE: usize = 1024 * 1024;

enum ActorExit {
    CommandChannelClosed,
    Eof,
    Read(io::Error),
    OversizedLine,
    Write {
        operation: &'static str,
        error: io::Error,
    },
}

impl ActorExit {
    fn barrier_reason(&self) -> String {
        match self {
            Self::CommandChannelClosed => {
                "mpv command channel closed before acknowledgement".to_owned()
            }
            Self::Eof => "mpv IPC closed before command acknowledgement".to_owned(),
            Self::Read(error) => format!("mpv IPC read failed before acknowledgement: {error}"),
            Self::OversizedLine => {
                "mpv IPC protocol failed before command acknowledgement".to_owned()
            }
            Self::Write { operation, error } => {
                format!("mpv IPC {operation} write failed before acknowledgement: {error}")
            }
        }
    }

    fn transport_reason(self) -> Option<String> {
        let reason = match self {
            Self::CommandChannelClosed => return None,
            Self::Eof => "mpv IPC closed unexpectedly".to_owned(),
            Self::Read(error) => format!("mpv IPC read failed: {error}"),
            Self::OversizedLine => {
                format!("mpv IPC message exceeded the {MPV_IPC_MAX_LINE}-byte safety limit")
            }
            Self::Write { operation, error } => {
                format!("mpv IPC {operation} write failed: {error}")
            }
        };
        Some(crate::util::sanitize::sanitize_error_text(reason))
    }
}

fn finish_actor(exit: ActorExit, emit: &EventSink) {
    if let Some(reason) = exit.transport_reason() {
        tracing::warn!(%reason, "mpv IPC transport closed");
        emit(PlayerEvent::TransportClosed(reason));
    }
}

fn transport_exit_or_shutdown(
    cmd_rx: &Receiver<PlayerCmd>,
    intentional_close: &AtomicBool,
    failure: ActorExit,
) -> ActorExit {
    if intentional_close.load(Ordering::Acquire) || cmd_rx.is_closed() {
        ActorExit::CommandChannelClosed
    } else {
        failure
    }
}

struct DispatchState {
    last_sent_time_sec: Option<i64>,
    last_sent_cache_sec: Option<i64>,
    /// Whether a real (numeric) duration was forwarded for the current file — the latch
    /// that turns the FIRST `duration: null` into a `Duration(None)` loss signal while
    /// keeping repeated nulls (a stream that never had one) silent.
    duration_known: bool,
    pending: HashMap<u64, PendingCommand>,
    /// Monotonic across every accepted Load/Stop, matching [`PlayerHandle`]'s admission
    /// counter. A Load becomes active only at mpv's ordered `start-file` boundary.
    issued_file_generation: u64,
    active_file_generation: Option<u64>,
    active_playlist_entry_id: Option<u64>,
    /// mpv returns the stable playlist entry ID in each successful `loadfile` reply. This map
    /// is the authoritative correlation boundary; start-file ordering alone is ambiguous when
    /// a redirect or a rapid replacement creates additional entries.
    entry_generations: HashMap<u64, u64>,
    /// Redirect replacement ranges may be announced before the originating load reply reaches
    /// this client. Retain the relation until the old entry's generation becomes known.
    pending_redirects: HashMap<u64, (u64, u64)>,
    failed_load_generations: HashSet<u64>,
    playlist_identity_mode: PlaylistIdentityMode,
    /// mpv 0.32 predates playlist-entry IDs in events, command results, and playlist
    /// properties. Keep its accepted direct loads ordered, and explicitly inherit a redirect's
    /// generation into the next start-file event. Newer mpv never consults this fallback.
    legacy_loads: VecDeque<LegacyLoad>,
    legacy_redirect_generation: Option<u64>,
    legacy_latest_playlist_filename: Option<String>,
    legacy_start_waiting: bool,
    /// `--keep-open` makes the observable `eof-reached` property the primary natural-end
    /// signal on every supported mpv; explicit newer end-file reasons remain a fallback.
    eof_emitted: bool,
    /// A reasonless 0.32 end waits for the ordered idle/start/eof boundary: idle means a
    /// playback failure, start means a redirect, and eof is handled by the property latch.
    legacy_pending_end_generation: Option<u64>,
    /// Property notifications can race ahead of the `loadfile` reply. Hold a small bounded set
    /// until the start-file entry is correlated instead of tagging them as the newest track.
    quarantined_file_events: VecDeque<PlayerEvent>,
    /// Raw mpv numeric-property traffic for the opt-in `YTM_PERF` trace. Kept behind an
    /// `Option` so normal runs pay only one predictable branch per incoming IPC line.
    numeric_perf: Option<NumericPerfWindow>,
    audio_output: audio_output::State,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PlaylistIdentityMode {
    #[default]
    Unknown,
    EntryIds,
    Legacy,
}

struct LegacyLoad {
    generation: u64,
    url: String,
    replied: bool,
}

const FILE_EVENT_QUARANTINE_CAPACITY: usize = 32;
const REDIRECT_ENTRY_RANGE_CAPACITY: u64 = 1024;
const ENTRY_GENERATION_CAPACITY: usize = 2048;
const PENDING_REDIRECT_CAPACITY: usize = 128;
const LEGACY_LOAD_CAPACITY: usize = 128;
const LOAD_VALIDATION_BACKLOG_CAPACITY: usize = 256;

struct PendingLoadValidation {
    request_id: u64,
    file_generation: u64,
    task: tokio::task::JoinHandle<LoadValidationOutcome>,
}

enum LoadValidationOutcome {
    Validated(String),
    Rejected(String),
    Superseded,
}

fn queue_during_load_validation(
    cmd: PlayerCmd,
    validating_load: &mut Option<PendingLoadValidation>,
    backlog: &mut VecDeque<PlayerCmd>,
) {
    let invalidates_pending_load = matches!(cmd, PlayerCmd::Load(_) | PlayerCmd::Stop);
    backlog.push_back(cmd);
    if invalidates_pending_load && let Some(validation) = validating_load.take() {
        validation.task.abort();
        tracing::debug!(
            file_generation = validation.file_generation,
            "cancelled superseded playback destination validation"
        );
    }
}

impl Default for DispatchState {
    fn default() -> Self {
        Self {
            last_sent_time_sec: None,
            last_sent_cache_sec: None,
            duration_known: false,
            pending: HashMap::new(),
            issued_file_generation: 0,
            // Property observation echoes before the first app load belong to the initial
            // generation and are harmless; subsequent starts always replace this value by ID.
            active_file_generation: Some(0),
            active_playlist_entry_id: None,
            entry_generations: HashMap::new(),
            pending_redirects: HashMap::new(),
            failed_load_generations: HashSet::new(),
            playlist_identity_mode: PlaylistIdentityMode::Unknown,
            legacy_loads: VecDeque::new(),
            legacy_redirect_generation: None,
            legacy_latest_playlist_filename: None,
            legacy_start_waiting: false,
            eof_emitted: false,
            legacy_pending_end_generation: None,
            quarantined_file_events: VecDeque::new(),
            numeric_perf: None,
            audio_output: audio_output::State::default(),
        }
    }
}

struct PendingCommand {
    label: String,
    file_generation: Option<u64>,
    acknowledgement: Option<crate::util::command_barrier::CommandBarrierSignal>,
    audio_output: Option<audio_output::Pending>,
}

struct NumericPerfWindow {
    started: Instant,
    raw_time_pos: u64,
    raw_cache_time: u64,
    borrowed_fast_path: u64,
    generic_fallback: u64,
    forwarded_time_pos: u64,
    forwarded_cache_time: u64,
}

impl NumericPerfWindow {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            raw_time_pos: 0,
            raw_cache_time: 0,
            borrowed_fast_path: 0,
            generic_fallback: 0,
            forwarded_time_pos: 0,
            forwarded_cache_time: 0,
        }
    }

    fn reset(&mut self, now: Instant) {
        self.started = now;
        self.raw_time_pos = 0;
        self.raw_cache_time = 0;
        self.borrowed_fast_path = 0;
        self.generic_fallback = 0;
        self.forwarded_time_pos = 0;
        self.forwarded_cache_time = 0;
    }
}

const NUMERIC_PERF_WINDOW: Duration = Duration::from_secs(5);

fn record_numeric_input(state: &mut DispatchState, name: &str, borrowed: bool) {
    let Some(perf) = state.numeric_perf.as_mut() else {
        return;
    };
    match name {
        "time-pos" => perf.raw_time_pos += 1,
        "demuxer-cache-time" => perf.raw_cache_time += 1,
        _ => return,
    }
    if borrowed {
        perf.borrowed_fast_path += 1;
    } else {
        perf.generic_fallback += 1;
    }
}

fn record_numeric_forward(state: &mut DispatchState, name: &str) {
    let Some(perf) = state.numeric_perf.as_mut() else {
        return;
    };
    match name {
        "time-pos" => perf.forwarded_time_pos += 1,
        "demuxer-cache-time" => perf.forwarded_cache_time += 1,
        _ => {}
    }
}

fn log_numeric_perf(state: &mut DispatchState, force: bool) {
    let Some(perf) = state.numeric_perf.as_mut() else {
        return;
    };
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(perf.started);
    if !force && elapsed < NUMERIC_PERF_WINDOW {
        return;
    }
    let raw_total = perf.raw_time_pos + perf.raw_cache_time;
    if raw_total > 0 {
        let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
        tracing::info!(
            target: "ytt::perf",
            window_ms = elapsed.as_millis() as u64,
            raw_time_pos_lines = perf.raw_time_pos,
            raw_cache_time_lines = perf.raw_cache_time,
            raw_numeric_hz = raw_total as f64 / seconds,
            borrowed_numeric_lines = perf.borrowed_fast_path,
            generic_numeric_lines = perf.generic_fallback,
            forwarded_time_pos = perf.forwarded_time_pos,
            forwarded_cache_time = perf.forwarded_cache_time,
            "mpv numeric IPC window"
        );
    }
    perf.reset(now);
}

/// Clear the per-file dedup/latch state when a file ends for ANY reason, so the next
/// load's first `time-pos`/cache second and duration are never dedup-suppressed.
fn reset_file_state(state: &mut DispatchState) {
    state.last_sent_time_sec = None;
    state.last_sent_cache_sec = None;
    state.duration_known = false;
    state.eof_emitted = false;
    state.legacy_pending_end_generation = None;
}

fn emit_file_event(emit: &EventSink, state: &mut DispatchState, event: PlayerEvent) {
    if let Some(generation) = state.active_file_generation {
        emit(PlayerEvent::file_scoped(generation, event));
        return;
    }
    if state.quarantined_file_events.len() == FILE_EVENT_QUARANTINE_CAPACITY {
        state.quarantined_file_events.pop_front();
        tracing::warn!(
            capacity = FILE_EVENT_QUARANTINE_CAPACITY,
            "mpv file event quarantine full; dropped oldest uncorrelated property"
        );
    }
    state.quarantined_file_events.push_back(event);
}

fn emit_file_event_for_generation(
    emit: &EventSink,
    state: &mut DispatchState,
    playlist_entry_id: Option<u64>,
    generation: Option<u64>,
    event: PlayerEvent,
) {
    if let Some(generation) = generation {
        emit(PlayerEvent::file_scoped(generation, event));
    } else if playlist_entry_id.is_some() && playlist_entry_id != state.active_playlist_entry_id {
        tracing::debug!(
            ?playlist_entry_id,
            active_playlist_entry_id = ?state.active_playlist_entry_id,
            "ignored terminal event for an uncorrelated inactive playlist entry"
        );
    } else {
        // Old mpv does not include playlist IDs; an explicitly active but not-yet-correlated
        // entry can also end before its load reply. Quarantine only those plausible current
        // terminals. Never relabel a known-different entry with the current generation.
        emit_file_event(emit, state, event);
    }
}

fn associate_entry_generation(
    emit: &EventSink,
    state: &mut DispatchState,
    entry_id: u64,
    generation: u64,
) {
    insert_entry_generation(state, entry_id, generation);
    if let Some((first, count)) = state.pending_redirects.remove(&entry_id) {
        associate_redirect_range(emit, state, first, count, generation);
    }
    if state.active_playlist_entry_id == Some(entry_id) {
        activate_correlated_entry(emit, state, generation);
    }
}

fn associate_redirect_range(
    emit: &EventSink,
    state: &mut DispatchState,
    first_entry_id: u64,
    count: u64,
    generation: u64,
) {
    for offset in 0..count.min(REDIRECT_ENTRY_RANGE_CAPACITY) {
        if let Some(entry_id) = first_entry_id.checked_add(offset) {
            insert_entry_generation(state, entry_id, generation);
            if state.active_playlist_entry_id == Some(entry_id) {
                activate_correlated_entry(emit, state, generation);
            }
        }
    }
}

fn insert_entry_generation(state: &mut DispatchState, entry_id: u64, generation: u64) {
    if !state.entry_generations.contains_key(&entry_id)
        && state.entry_generations.len() >= ENTRY_GENERATION_CAPACITY
        && let Some(victim) = state
            .entry_generations
            .keys()
            .filter(|candidate| Some(**candidate) != state.active_playlist_entry_id)
            .min()
            .copied()
    {
        state.entry_generations.remove(&victim);
        state.pending_redirects.remove(&victim);
        tracing::debug!(victim, "evicted stale mpv playlist correlation");
    }
    if state.entry_generations.len() < ENTRY_GENERATION_CAPACITY
        || state.entry_generations.contains_key(&entry_id)
    {
        state.entry_generations.insert(entry_id, generation);
    }
}

fn remember_pending_redirect(
    state: &mut DispatchState,
    old_entry_id: u64,
    first_entry_id: u64,
    count: u64,
) {
    if !state.pending_redirects.contains_key(&old_entry_id)
        && state.pending_redirects.len() >= PENDING_REDIRECT_CAPACITY
        && let Some(victim) = state
            .pending_redirects
            .keys()
            .filter(|candidate| Some(**candidate) != state.active_playlist_entry_id)
            .min()
            .copied()
    {
        state.pending_redirects.remove(&victim);
        tracing::debug!(victim, "evicted stale pending mpv redirect correlation");
    }
    if state.pending_redirects.len() < PENDING_REDIRECT_CAPACITY
        || state.pending_redirects.contains_key(&old_entry_id)
    {
        state
            .pending_redirects
            .insert(old_entry_id, (first_entry_id, count));
    }
}

fn activate_correlated_entry(emit: &EventSink, state: &mut DispatchState, generation: u64) {
    state.active_file_generation = Some(generation);
    while let Some(event) = state.quarantined_file_events.pop_front() {
        emit(PlayerEvent::file_scoped(generation, event));
    }
}

fn use_entry_id_protocol(state: &mut DispatchState) {
    state.playlist_identity_mode = PlaylistIdentityMode::EntryIds;
    state.legacy_loads.clear();
    state.legacy_redirect_generation = None;
    state.legacy_latest_playlist_filename = None;
    state.legacy_start_waiting = false;
    state.eof_emitted = false;
    state.legacy_pending_end_generation = None;
}

fn remove_legacy_load_generation(state: &mut DispatchState, generation: u64) {
    if let Some(index) = state
        .legacy_loads
        .iter()
        .position(|pending| pending.generation == generation)
    {
        state.legacy_loads.remove(index);
    }
}

fn mark_legacy_load_replied(state: &mut DispatchState, generation: u64) {
    state.playlist_identity_mode = PlaylistIdentityMode::Legacy;
    while state
        .legacy_loads
        .front()
        .is_some_and(|pending| pending.generation < generation)
    {
        state.legacy_loads.pop_front();
    }
    if let Some(pending) = state
        .legacy_loads
        .iter_mut()
        .find(|pending| pending.generation == generation)
    {
        pending.replied = true;
    }
}

struct PlaylistSelection<'a> {
    entry_id: Option<u64>,
    filename: Option<&'a str>,
}

fn playlist_selection(data: Option<&serde_json::Value>) -> Option<PlaylistSelection<'_>> {
    let entries = data?.as_array()?;
    let selected = entries
        .iter()
        .find(|entry| {
            entry.get("current").and_then(serde_json::Value::as_bool) == Some(true)
                || entry.get("playing").and_then(serde_json::Value::as_bool) == Some(true)
        })
        .or_else(|| (entries.len() == 1).then(|| &entries[0]))?;
    Some(PlaylistSelection {
        entry_id: selected.get("id").and_then(serde_json::Value::as_u64),
        filename: selected.get("filename").and_then(serde_json::Value::as_str),
    })
}

fn correlate_legacy_playlist(emit: &EventSink, state: &mut DispatchState, filename: Option<&str>) {
    if let Some(filename) = filename {
        state.legacy_latest_playlist_filename = Some(filename.to_owned());
    }
    if !state.legacy_start_waiting {
        return;
    }

    let selected_filename = state.legacy_latest_playlist_filename.as_deref();
    let selected_matches_pending = selected_filename.is_some_and(|filename| {
        state
            .legacy_loads
            .iter()
            .any(|pending| pending.url == filename)
    });
    let matched = selected_filename.and_then(|filename| {
        state
            .legacy_loads
            .iter()
            .rposition(|pending| pending.replied && pending.url == filename)
    });
    if let Some(index) = matched {
        let generation = state.legacy_loads[index].generation;
        let newer_direct_load_is_pending = state.legacy_loads.len() > index + 1;
        state.legacy_loads.drain(..=index);
        if newer_direct_load_is_pending {
            // The owner has already admitted a newer replacement. Even if this older file
            // reaches start-file, its quarantined properties are stale and must not be flushed
            // into the newer reducer generation. Keep waiting for the newest direct start.
            state.active_file_generation = None;
            state.quarantined_file_events.clear();
            state.legacy_redirect_generation = None;
            return;
        }
        state.legacy_start_waiting = false;
        state.legacy_redirect_generation = None;
        activate_correlated_entry(emit, state, generation);
    } else if selected_filename.is_some()
        && !selected_matches_pending
        && state.legacy_loads.is_empty()
        && let Some(generation) = state.legacy_redirect_generation.take()
    {
        state.legacy_start_waiting = false;
        activate_correlated_entry(emit, state, generation);
    } else if selected_filename.is_none()
        && let Some(index) = state
            .legacy_loads
            .iter()
            .position(|pending| pending.replied)
    {
        let generation = state.legacy_loads[index].generation;
        state.legacy_loads.drain(..=index);
        state.legacy_start_waiting = false;
        state.legacy_redirect_generation = None;
        activate_correlated_entry(emit, state, generation);
    } else if state.legacy_loads.is_empty()
        && let Some(generation) = state.legacy_redirect_generation.take()
    {
        state.legacy_start_waiting = false;
        activate_correlated_entry(emit, state, generation);
    }
}

fn remember_pending_command(state: &mut DispatchState, request_id: u64, label: impl Into<String>) {
    if state.pending.len() >= 128 && !evict_oldest_unprotected_pending(state) {
        // Command diagnostics are expendable; load identity is not. If every slot protects a
        // file-generation correlation, leave this ordinary reply untracked.
        return;
    }
    state.pending.insert(
        request_id,
        PendingCommand {
            label: label.into(),
            file_generation: None,
            acknowledgement: None,
            audio_output: None,
        },
    );
}

fn remember_pending_load(
    state: &mut DispatchState,
    request_id: u64,
    generation: u64,
    label: impl Into<String>,
) -> bool {
    if state.pending.len() >= 128 && !evict_oldest_unprotected_pending(state) {
        return false;
    }
    state.pending.insert(
        request_id,
        PendingCommand {
            label: label.into(),
            file_generation: Some(generation),
            acknowledgement: None,
            audio_output: None,
        },
    );
    true
}

fn remember_pending_tracked(
    state: &mut DispatchState,
    request_id: u64,
    label: String,
    acknowledgement: crate::util::command_barrier::CommandBarrierSignal,
) -> io::Result<()> {
    if state.pending.len() >= 128 && !evict_oldest_unprotected_pending(state) {
        acknowledgement.fail("mpv acknowledgement queue saturated");
        return Err(io::Error::other("mpv acknowledgement queue saturated"));
    }
    state.pending.insert(
        request_id,
        PendingCommand {
            label,
            file_generation: None,
            acknowledgement: Some(acknowledgement),
            audio_output: None,
        },
    );
    Ok(())
}

/// Connect to the mpv IPC endpoint, retrying for ~3s while mpv finishes starting up.
pub async fn connect_retry(path: &str) -> io::Result<Stream> {
    let mut last_err: Option<io::Error> = None;
    for _ in 0..200 {
        let name = path.to_fs_name::<GenericFilePath>()?;
        match Stream::connect(name).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                last_err = Some(e);
                sleep(Duration::from_millis(15)).await;
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "mpv IPC connect timed out")))
}

/// Drive one mpv connection: subscribe to progress properties, then loop forwarding
/// mpv events to the runtime (`emit`) and writing player commands (`cmd_rx`) to mpv.
/// Returns when mpv closes the connection or all command senders drop.
pub async fn run_actor(
    conn: Stream,
    mut cmd_rx: Receiver<PlayerCmd>,
    emit: EventSink,
    intentional_close: Arc<AtomicBool>,
    file_generation_rx: watch::Receiver<u64>,
) {
    let mut state = DispatchState::default();
    if std::env::var_os("YTM_PERF").is_some() {
        state.numeric_perf = Some(NumericPerfWindow::new());
    }

    // Subscribe to the properties the player view needs. IDs are arbitrary but stable.
    for (id, prop) in [
        (1u64, "time-pos"),
        (2, "duration"),
        (3, "pause"),
        (4, "volume"),
        (5, "metadata"),
        (6, "demuxer-cache-time"),
        // For the radio recorder: pick the passthrough container from the stream codec.
        (7, "audio-codec-name"),
        (8, "file-format"),
        // mpv 0.32 has no playlist entry IDs, but its playlist property still exposes the
        // selected filename. That is the legacy correlation key for rapid replacements.
        (9, "playlist"),
        // mpv 0.32 also omits end-file reasons from JSON IPC. This observed property is its
        // reliable natural-EOF signal; newer mpv remains covered by explicit end-file data.
        (10, "eof-reached"),
        (11, "audio-device-list"),
        (12, "audio-device"),
        (13, "current-ao"),
    ] {
        remember_pending_command(&mut state, id, format!("observe {prop}"));
        if let Err(error) = write_json(&conn, &proto::cmd_observe(id, prop)).await {
            let exit = transport_exit_or_shutdown(
                &cmd_rx,
                &intentional_close,
                ActorExit::Write {
                    operation: "property subscription",
                    error,
                },
            );
            finish_actor(exit, &emit);
            return;
        }
    }

    let mut reader = BufReader::new(&conn);
    let mut line: Vec<u8> = Vec::new();
    let mut request_id: u64 = 13;
    let mut validating_load: Option<PendingLoadValidation> = None;
    let mut validation_backlog = VecDeque::new();
    let exit = loop {
        let audio_device_selection_deadline = state.audio_output.tokio_deadline();
        // Commands accepted after a load whose destination check was slow remain ordered here.
        // A newer Load/Stop cancels that obsolete validation, then this backlog is replayed in
        // FIFO order before fresh channel work.
        if state.audio_output.is_idle()
            && validating_load.is_none()
            && let Some(cmd) = validation_backlog.pop_front()
        {
            match begin_or_dispatch_command(
                &conn,
                &emit,
                &mut state,
                &mut request_id,
                &file_generation_rx,
                cmd,
            )
            .await
            {
                Ok(validation) => validating_load = validation,
                Err(error) => {
                    break transport_exit_or_shutdown(
                        &cmd_rx,
                        &intentional_close,
                        ActorExit::Write {
                            operation: "command",
                            error,
                        },
                    );
                }
            }
        }

        tokio::select! {
            cmd = cmd_rx.recv(), if state.audio_output.is_idle()
                && ((validating_load.is_some()
                && validation_backlog.len() < LOAD_VALIDATION_BACKLOG_CAPACITY)
                || (validating_load.is_none() && validation_backlog.is_empty())) => match cmd {
                None => {
                    if let Some(validation) = validating_load.take() {
                        validation.task.abort();
                    }
                    break ActorExit::CommandChannelClosed;
                }
                Some(cmd) if validating_load.is_some() => {
                    queue_during_load_validation(
                        cmd,
                        &mut validating_load,
                        &mut validation_backlog,
                    );
                }
                Some(cmd) => {
                    match begin_or_dispatch_command(
                        &conn,
                        &emit,
                        &mut state,
                        &mut request_id,
                        &file_generation_rx,
                        cmd,
                    ).await {
                        Ok(validation) => validating_load = validation,
                        Err(error) => break transport_exit_or_shutdown(
                            &cmd_rx,
                            &intentional_close,
                            ActorExit::Write {
                                operation: "command",
                                error,
                            },
                        ),
                    }
                }
            },
            validation = async {
                let task = &mut validating_load
                    .as_mut()
                    .expect("validation branch is guarded")
                    .task;
                task.await
            }, if validating_load.is_some() => {
                let pending = validating_load
                    .take()
                    .expect("completed validation remains installed");
                let validated = match validation {
                    Ok(result) => result,
                    Err(error) => LoadValidationOutcome::Rejected(format!(
                        "playback destination validation task failed: {error}"
                    )),
                };
                if let Err(error) = finish_load_validation(
                    &conn,
                    &emit,
                    &mut state,
                    &mut request_id,
                    pending,
                    validated,
                ).await {
                    break transport_exit_or_shutdown(
                        &cmd_rx,
                        &intentional_close,
                        ActorExit::Write {
                            operation: "load command",
                            error,
                        },
                    );
                }
            },
            // Bounded read (shared with the remote protocol): a well-behaved mpv sends tiny
            // JSON lines, so a line past the cap means a broken/hostile endpoint — tear down
            // rather than let one line grow memory without limit.
            read = crate::util::io::read_bounded_line(&mut reader, &mut line, MPV_IPC_MAX_LINE) => match read {
                Ok(crate::util::io::BoundedLine::Eof) => {
                    break transport_exit_or_shutdown(
                        &cmd_rx,
                        &intentional_close,
                        ActorExit::Eof,
                    );
                }
                Err(error) => {
                    break transport_exit_or_shutdown(
                        &cmd_rx,
                        &intentional_close,
                        ActorExit::Read(error),
                    );
                }
                Ok(crate::util::io::BoundedLine::TooLarge) => {
                    break transport_exit_or_shutdown(
                        &cmd_rx,
                        &intentional_close,
                        ActorExit::OversizedLine,
                    );
                }
                Ok(crate::util::io::BoundedLine::Line) => {
                    let text = String::from_utf8_lossy(&line);
                    dispatch_incoming(&text, &emit, &mut state);
                    // Keep a partial frame across cancellation of the read branch by a ready
                    // command. Clear only after a complete newline-delimited frame was handled.
                    line.clear();
                    if let Some(selection) = state.audio_output.followup.take()
                        && let Err(error) = audio_output::perform_followup(
                            &conn,
                            &emit,
                            &mut state,
                            &mut request_id,
                            selection,
                        ).await
                    {
                        break transport_exit_or_shutdown(
                            &cmd_rx,
                            &intentional_close,
                            ActorExit::Write {
                                operation: "audio device command",
                                error,
                            },
                        );
                    }
                }
            },
            // Replay one queued command, then give the read side a scheduling point before
            // writing another. This keeps a 256-command validation backlog from starving mpv
            // replies/events while preserving strict FIFO ahead of fresh channel commands.
            _ = tokio::task::yield_now(), if validating_load.is_none()
                && state.audio_output.is_idle()
                && !validation_backlog.is_empty() => {},
            _ = async {
                tokio::time::sleep_until(
                    audio_device_selection_deadline.expect("timeout branch is guarded")
                ).await;
            }, if audio_device_selection_deadline.is_some() => {
                audio_output::timeout(&mut state, &emit);
            },
        }
    };

    if let Some(validation) = validating_load.take() {
        validation.task.abort();
    }
    log_numeric_perf(&mut state, true);

    let barrier_error = exit.barrier_reason();
    audio_output::fail_pending_commands(&state, &emit, &barrier_error);

    // This is deliberately the actor's final action. The owner may start a replacement only
    // after processing this event, so no event from the old actor can follow the terminal one.
    finish_actor(exit, &emit);
}

async fn begin_or_dispatch_command(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    request_id: &mut u64,
    file_generation_rx: &watch::Receiver<u64>,
    cmd: PlayerCmd,
) -> io::Result<Option<PendingLoadValidation>> {
    if let PlayerCmd::Load(url) = cmd {
        state.issued_file_generation = state.issued_file_generation.wrapping_add(1);
        *request_id = request_id.wrapping_add(1);
        let file_generation = state.issued_file_generation;
        let load_request_id = *request_id;
        let task = tokio::spawn(validate_load_until_superseded(
            url,
            file_generation,
            file_generation_rx.clone(),
        ));
        return Ok(Some(PendingLoadValidation {
            request_id: load_request_id,
            file_generation,
            task,
        }));
    }
    dispatch_command(conn, emit, state, request_id, cmd).await?;
    Ok(None)
}

async fn validate_load_until_superseded(
    url: String,
    file_generation: u64,
    mut file_generation_rx: watch::Receiver<u64>,
) -> LoadValidationOutcome {
    let validation = crate::playback_target::validate_playback_target_for_handoff(&url);
    tokio::pin!(validation);

    loop {
        if *file_generation_rx.borrow_and_update() > file_generation {
            return LoadValidationOutcome::Superseded;
        }
        tokio::select! {
            changed = file_generation_rx.changed() => {
                if changed.is_err() {
                    return LoadValidationOutcome::Superseded;
                }
            }
            result = &mut validation => {
                // An admission can race the validation future becoming ready. Re-read the
                // watch value at the commit boundary so unbiased select fairness cannot revive
                // a superseded URL.
                if *file_generation_rx.borrow_and_update() > file_generation {
                    return LoadValidationOutcome::Superseded;
                }
                return match result {
                    Ok(url) => LoadValidationOutcome::Validated(url),
                    Err(error) => LoadValidationOutcome::Rejected(error.to_string()),
                };
            }
        }
    }
}

async fn finish_load_validation(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    request_id: &mut u64,
    pending: PendingLoadValidation,
    validated: LoadValidationOutcome,
) -> io::Result<()> {
    let url = match validated {
        LoadValidationOutcome::Validated(url) => url,
        LoadValidationOutcome::Superseded => return Ok(()),
        LoadValidationOutcome::Rejected(error) => {
            tracing::warn!(%error, "blocked unsafe playback URL");
            emit(PlayerEvent::file_scoped(
                pending.file_generation,
                PlayerEvent::Error(format!("blocked playback URL: {error}")),
            ));
            return Ok(());
        }
    };
    reset_file_state(state);
    if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds
        && state.legacy_loads.len() >= LEGACY_LOAD_CAPACITY
    {
        return Err(io::Error::other(
            "legacy mpv load correlation queue saturated",
        ));
    }
    let needs_identity_query = state.playlist_identity_mode != PlaylistIdentityMode::EntryIds;
    let load_reserved = remember_pending_load(
        state,
        pending.request_id,
        pending.file_generation,
        "loadfile",
    );
    let identity_request_id = if needs_identity_query {
        *request_id = request_id.wrapping_add(1);
        Some(*request_id)
    } else {
        None
    };
    let identity_reserved = identity_request_id.is_none_or(|identity_request_id| {
        remember_pending_load(
            state,
            identity_request_id,
            pending.file_generation,
            "loadfile identity",
        )
    });
    if !load_reserved || !identity_reserved {
        state.pending.remove(&pending.request_id);
        if let Some(identity_request_id) = identity_request_id {
            state.pending.remove(&identity_request_id);
        }
        return Err(io::Error::other(
            "mpv load identity correlation queue saturated",
        ));
    }
    write_json(
        conn,
        &proto::cmd_loadfile(&url, "replace", pending.request_id),
    )
    .await?;
    state.legacy_latest_playlist_filename = None;
    state.legacy_loads.push_back(LegacyLoad {
        generation: pending.file_generation,
        url,
        replied: false,
    });

    // mpv 0.33+ exposes stable IDs. mpv 0.32 exposes only the selected filename, so its
    // ordered snapshot remains the barrier that distinguishes a direct rapid replacement from
    // a redirect child. Once stable event IDs are proven, the redundant query is omitted.
    if let Some(identity_request_id) = identity_request_id {
        write_json(
            conn,
            &proto::cmd_get_property("playlist", identity_request_id),
        )
        .await?;
    }
    Ok(())
}

async fn dispatch_command(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    request_id: &mut u64,
    cmd: PlayerCmd,
) -> io::Result<()> {
    *request_id = request_id.wrapping_add(1);
    let command_request_id = *request_id;
    if audio_output::is_command(&cmd) {
        return audio_output::dispatch_command(conn, emit, state, command_request_id, cmd).await;
    }
    let (json, label, acknowledgement) = match cmd {
        PlayerCmd::Load(_) => unreachable!("loads start in begin_or_dispatch_command"),
        PlayerCmd::Stop => {
            state.issued_file_generation = state.issued_file_generation.wrapping_add(1);
            state.legacy_loads.clear();
            state.legacy_redirect_generation = None;
            state.legacy_latest_playlist_filename = None;
            state.legacy_start_waiting = false;
            state.last_sent_time_sec = None;
            state.last_sent_cache_sec = None;
            state.duration_known = false;
            state.eof_emitted = false;
            state.legacy_pending_end_generation = None;
            (proto::cmd_stop(command_request_id), "stop".to_owned(), None)
        }
        PlayerCmd::CyclePause => (
            proto::cmd_cycle("pause", command_request_id),
            "cycle pause".to_owned(),
            None,
        ),
        PlayerCmd::SeekRelative(secs) => (
            proto::cmd_seek_relative(secs, command_request_id),
            "seek".to_owned(),
            None,
        ),
        PlayerCmd::SeekAbsolute(secs) => (
            proto::cmd_seek_absolute(secs, command_request_id),
            "seek".to_owned(),
            None,
        ),
        PlayerCmd::SetVolume(vol) => (
            proto::cmd_set_volume(vol, command_request_id),
            "set volume".to_owned(),
            None,
        ),
        PlayerCmd::SetAudioFilter(af) => (
            proto::cmd_set_property("af", &serde_json::Value::from(af), command_request_id),
            "set af".to_owned(),
            None,
        ),
        PlayerCmd::AfCommand {
            label,
            param,
            value,
        } => (
            proto::cmd_af_command(&label, &param, &value, command_request_id),
            "af-command".to_owned(),
            None,
        ),
        PlayerCmd::SetProperty { name, value } => {
            let label = format!("set_property {name}");
            (
                proto::cmd_set_property(&name, &value, command_request_id),
                label,
                None,
            )
        }
        PlayerCmd::RefreshAudioDevices => {
            unreachable!("audio-device refreshes are handled before ordinary commands")
        }
        PlayerCmd::SelectAudioDevice { .. } => {
            unreachable!("audio-device selections are handled before ordinary commands")
        }
        PlayerCmd::TrackedProperty(tracked) => {
            let label = format!("set_property {}", tracked.name);
            (
                proto::cmd_set_property(&tracked.name, &tracked.value, command_request_id),
                label,
                Some(tracked.acknowledgement),
            )
        }
    };
    if let Some(acknowledgement) = acknowledgement {
        remember_pending_tracked(state, command_request_id, label, acknowledgement)?;
    } else {
        remember_pending_command(state, command_request_id, label);
    }
    write_json(conn, &json).await
}

/// Zero-copy view of the high-rate numeric properties, avoiding a `serde_json::Value` tree.
/// `None` covers both JSON null and a missing `data` field.
#[derive(serde::Deserialize)]
struct NumericPropertyLine<'a> {
    event: &'a str,
    name: &'a str,
    data: Option<f64>,
}

/// Translate one mpv line into a player event for the runtime.
fn dispatch_incoming(line: &str, emit: &EventSink, state: &mut DispatchState) {
    // Flush the previous complete measurement window before accounting this line, so forwarded
    // counts and raw counts always describe the same set of fully-dispatched events.
    log_numeric_perf(state, false);
    // Fast path: normalize and dedup the two high-rate progress properties before allocating.
    // Other property shapes fail or fall through to the general parser unchanged.
    if let Ok(property) = serde_json::from_str::<NumericPropertyLine>(line.trim())
        && property.event == "property-change"
    {
        match property.name {
            "time-pos" => {
                record_numeric_input(state, property.name, true);
                if let Some(t) = property.data {
                    let t = crate::playback_policy::norm_position(t);
                    let sec = t as i64;
                    if state.last_sent_time_sec != Some(sec) {
                        state.last_sent_time_sec = Some(sec);
                        emit_file_event(emit, state, PlayerEvent::TimePos(t));
                        record_numeric_forward(state, property.name);
                    }
                }
                return;
            }
            "demuxer-cache-time" => {
                record_numeric_input(state, property.name, true);
                match property.data {
                    Some(t) => {
                        let t = crate::playback_policy::norm_position(t);
                        let sec = t as i64;
                        if state.last_sent_cache_sec != Some(sec) {
                            state.last_sent_cache_sec = Some(sec);
                            emit_file_event(emit, state, PlayerEvent::CacheTime(Some(t)));
                            record_numeric_forward(state, property.name);
                        }
                    }
                    None => {
                        if state.last_sent_cache_sec.take().is_some() {
                            emit_file_event(emit, state, PlayerEvent::CacheTime(None));
                            record_numeric_forward(state, property.name);
                        }
                    }
                }
                return;
            }
            _ => {}
        }
    }
    let Some(incoming) = proto::parse_line(line) else {
        return;
    };
    match incoming {
        MpvIncoming::PropertyChange { name, value, .. } => match name.as_str() {
            "time-pos" => {
                record_numeric_input(state, &name, false);
                if let Some(t) = value.as_f64() {
                    let t = crate::playback_policy::norm_position(t);
                    let sec = t as i64;
                    if state.last_sent_time_sec != Some(sec) {
                        state.last_sent_time_sec = Some(sec);
                        emit_file_event(emit, state, PlayerEvent::TimePos(t));
                        record_numeric_forward(state, &name);
                    }
                }
            }
            "duration" => match value.as_f64() {
                Some(d) => {
                    let d = crate::playback_policy::norm_duration(d);
                    state.duration_known = d > 0.0;
                    emit_file_event(emit, state, PlayerEvent::Duration(Some(d)));
                }
                // Null after a real value = the property became unavailable (live edge,
                // mid-file teardown) — forward the loss ONCE so a reducer that missed the
                // load-time clear (late event from the old file) can't keep a stale length.
                None => {
                    if std::mem::take(&mut state.duration_known) {
                        emit_file_event(emit, state, PlayerEvent::Duration(None));
                    }
                }
            },
            "pause" => {
                if let Some(p) = value.as_bool() {
                    emit_file_event(emit, state, PlayerEvent::Paused(p));
                }
            }
            "volume" => {
                if let Some(v) = value.as_f64() {
                    emit(PlayerEvent::Volume(v));
                }
            }
            "metadata" => {
                emit_file_event(emit, state, PlayerEvent::Metadata(value));
            }
            "audio-codec-name" => {
                emit_file_event(
                    emit,
                    state,
                    PlayerEvent::AudioCodec(value.as_str().map(str::to_owned)),
                );
            }
            "file-format" => {
                emit_file_event(
                    emit,
                    state,
                    PlayerEvent::FileFormat(value.as_str().map(str::to_owned)),
                );
            }
            audio_property if audio_output::dispatch_property(audio_property, &value, emit) => {}
            "playlist" => {
                if let Some(selection) = playlist_selection(Some(&value)) {
                    if selection.entry_id.is_some() {
                        use_entry_id_protocol(state);
                    } else if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds {
                        state.playlist_identity_mode = PlaylistIdentityMode::Legacy;
                        correlate_legacy_playlist(emit, state, selection.filename);
                    }
                }
            }
            "eof-reached" => {
                if value.as_bool() == Some(true) && !state.eof_emitted {
                    state.eof_emitted = true;
                    if let Some(generation) = state.legacy_pending_end_generation.take() {
                        emit(PlayerEvent::file_scoped(generation, PlayerEvent::Eof));
                    } else {
                        emit_file_event(emit, state, PlayerEvent::Eof);
                    }
                }
            }
            "demuxer-cache-time" => match value.as_f64() {
                // High-rate like time-pos → dedup to whole seconds.
                Some(t) => {
                    record_numeric_input(state, &name, false);
                    let t = crate::playback_policy::norm_position(t);
                    let sec = t as i64;
                    if state.last_sent_cache_sec != Some(sec) {
                        state.last_sent_cache_sec = Some(sec);
                        emit_file_event(emit, state, PlayerEvent::CacheTime(Some(t)));
                        record_numeric_forward(state, &name);
                    }
                }
                // Unlike time-pos, a null here is a signal the reducer needs: the
                // property became unavailable (stream teardown, cache-less demuxer).
                None => {
                    record_numeric_input(state, &name, false);
                    if state.last_sent_cache_sec.take().is_some() {
                        emit_file_event(emit, state, PlayerEvent::CacheTime(None));
                        record_numeric_forward(state, &name);
                    }
                }
            },
            _ => {}
        },
        MpvIncoming::StartFile { playlist_entry_id } => {
            reset_file_state(state);
            state.quarantined_file_events.clear();
            let previous_generation = state.active_file_generation;
            state.active_playlist_entry_id = playlist_entry_id;
            state.active_file_generation = None;
            if let Some(entry_id) = playlist_entry_id {
                use_entry_id_protocol(state);
                state.active_file_generation = state.entry_generations.get(&entry_id).copied();
            } else if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds {
                state.playlist_identity_mode = PlaylistIdentityMode::Legacy;
                state.legacy_start_waiting = true;
                state.legacy_redirect_generation = previous_generation;
                correlate_legacy_playlist(emit, state, None);
            }
            if state.active_file_generation.is_none() {
                tracing::debug!(
                    ?playlist_entry_id,
                    "quarantining mpv file events until playlist entry is correlated"
                );
            }
        }
        MpvIncoming::EndFile {
            reason,
            file_error,
            playlist_entry_id,
            playlist_insert_id,
            playlist_insert_num_entries,
        } => {
            let eof_was_emitted = state.eof_emitted;
            let generation = playlist_entry_id
                .and_then(|entry_id| state.entry_generations.get(&entry_id).copied())
                .or_else(|| {
                    (playlist_entry_id.is_none()
                        || playlist_entry_id == state.active_playlist_entry_id)
                        .then_some(state.active_file_generation)
                        .flatten()
                });
            let legacy_reasonless_error_candidate = reason.is_empty()
                && playlist_entry_id.is_none()
                && state.playlist_identity_mode == PlaylistIdentityMode::Legacy
                && !eof_was_emitted
                && generation.is_some_and(|generation| generation == state.issued_file_generation);
            if playlist_entry_id.is_none()
                && state.playlist_identity_mode == PlaylistIdentityMode::Legacy
            {
                state.legacy_redirect_generation = generation;
            }
            if reason == "redirect"
                && let (Some(first), Some(count)) =
                    (playlist_insert_id, playlist_insert_num_entries)
                && count > 0
            {
                if let Some(generation) = generation {
                    associate_redirect_range(emit, state, first, count, generation);
                } else if let Some(old_entry_id) = playlist_entry_id {
                    remember_pending_redirect(state, old_entry_id, first, count);
                }
            }
            reset_file_state(state);
            match reason.as_str() {
                "eof" => {
                    if !eof_was_emitted {
                        emit_file_event_for_generation(
                            emit,
                            state,
                            playlist_entry_id,
                            generation,
                            PlayerEvent::Eof,
                        );
                    }
                    // Property changes and lifecycle events are delivered asynchronously.
                    // Keep the latch armed if the explicit fallback wins that race.
                    state.eof_emitted = true;
                }
                "error" => {
                    // Surface mpv's own reason when it gives one — it's the closest thing to a
                    // "why" (HTTP 403, unsupported format, …); otherwise a generic message.
                    let msg = match file_error {
                        Some(fe) if !fe.is_empty() => {
                            format!("mpv could not play this track ({fe})")
                        }
                        _ => "mpv could not play this track".to_owned(),
                    };
                    emit_file_event_for_generation(
                        emit,
                        state,
                        playlist_entry_id,
                        generation,
                        PlayerEvent::Error(msg),
                    );
                }
                "" if legacy_reasonless_error_candidate => {
                    state.legacy_pending_end_generation = generation;
                    tracing::debug!(
                        ?generation,
                        "waiting for mpv 0.32 idle/start/eof terminal boundary"
                    );
                }
                // `stop` (our own Stop/replace), `quit`, `redirect`, and any future reason:
                // no event (the reducers own those transitions), but the per-file dedup
                // state must not survive into the next load's first second.
                reason => tracing::debug!(reason, "mpv end-file"),
            }
            if let Some(entry_id) = playlist_entry_id {
                state.entry_generations.remove(&entry_id);
            }
        }
        MpvIncoming::Idle => {
            if state.playlist_identity_mode == PlaylistIdentityMode::Legacy
                && let Some(generation) = state.legacy_pending_end_generation.take()
                && generation == state.issued_file_generation
            {
                emit(PlayerEvent::file_scoped(
                    generation,
                    PlayerEvent::Error("mpv could not play this track".to_owned()),
                ));
            }
        }
        MpvIncoming::CommandReply {
            request_id,
            error,
            data,
        } => {
            let Some(mut pending) = state.pending.remove(&request_id) else {
                return;
            };
            if audio_output::dispatch_reply(state, &mut pending, &error, data.as_ref(), emit) {
                return;
            }
            if let Some(acknowledgement) = &pending.acknowledgement {
                if error == "success" || error.is_empty() {
                    acknowledgement.succeed();
                } else {
                    acknowledgement.fail(format!("mpv rejected {} ({error})", pending.label));
                }
            }
            if pending.label == "loadfile" {
                let file_generation = pending
                    .file_generation
                    .unwrap_or(state.issued_file_generation);
                if error == "success" || error.is_empty() {
                    if let Some(entry_id) = data
                        .as_ref()
                        .and_then(|value| value.get("playlist_entry_id"))
                        .and_then(serde_json::Value::as_u64)
                    {
                        use_entry_id_protocol(state);
                        associate_entry_generation(emit, state, entry_id, file_generation);
                    } else if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds {
                        mark_legacy_load_replied(state, file_generation);
                        if state.legacy_latest_playlist_filename.is_some() {
                            correlate_legacy_playlist(emit, state, None);
                        }
                    }
                } else {
                    state.failed_load_generations.insert(file_generation);
                    remove_legacy_load_generation(state, file_generation);
                    emit(PlayerEvent::file_scoped(
                        file_generation,
                        PlayerEvent::Error(format!("mpv rejected loadfile ({error})")),
                    ));
                }
            } else if pending.label == "loadfile identity" {
                let file_generation = pending
                    .file_generation
                    .unwrap_or(state.issued_file_generation);
                if state.failed_load_generations.remove(&file_generation) {
                    return;
                }
                let selection = (error == "success" || error.is_empty())
                    .then(|| playlist_selection(data.as_ref()))
                    .flatten();
                if let Some(entry_id) = selection.as_ref().and_then(|value| value.entry_id) {
                    use_entry_id_protocol(state);
                    associate_entry_generation(emit, state, entry_id, file_generation);
                } else if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds {
                    mark_legacy_load_replied(state, file_generation);
                    correlate_legacy_playlist(
                        emit,
                        state,
                        selection.and_then(|value| value.filename),
                    );
                } else {
                    emit(PlayerEvent::file_scoped(
                        file_generation,
                        PlayerEvent::Error(
                            "mpv could not correlate loadfile with a playlist entry".to_owned(),
                        ),
                    ));
                }
            } else if error != "success" && !error.is_empty() {
                tracing::warn!(command = %pending.label, error = %error, "mpv command failed");
            }
        }
        // Script messages are a video-overlay concern ([`super::video`]); the audio
        // engine has no keys to press.
        MpvIncoming::ClientMessage { .. } | MpvIncoming::Other => {}
    }
}

#[cfg(test)]
mod tests;
