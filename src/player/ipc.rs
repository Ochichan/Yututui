//! The mpv IPC client: connect to the socket/pipe, then run the actor loop.
//!
//! `interprocess` gives us one async `LocalSocket` API over Unix sockets and Windows
//! named pipes. mpv creates the endpoint a moment after spawn, so [`connect_retry`]
//! polls briefly. The actor holds a single connection and reads events while writing
//! commands concurrently by sharing `&conn` (the documented `interprocess` pattern).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Instant;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::BufReader;
use tokio::sync::{mpsc::Receiver, watch};
use tokio::time::{Duration, sleep};

use super::proto::{self, MpvIncoming};
use super::{
    EventSink, MediaSourceContext, PlayerCmd, PlayerEvent, SharedLongFormSeekStatus,
    cache_runtime::CacheRuntime, pending,
};
use crate::player::long_form_seek::{CacheAction, CacheEffectiveState};

mod actor_exit;
mod actor_handlers;
mod audio_output;
mod resume;
mod wire;

pub(super) use wire::write_json;

use actor_exit::{ActorExit, finish_actor, transport_exit_or_shutdown};
use resume::ResumeCoordinator;

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

struct DispatchState {
    last_sent_time_sec: Option<i64>,
    last_sent_cache_sec: Option<i64>,
    /// Whether a real (numeric) duration was forwarded for the current file — the latch
    /// that turns the FIRST `duration: null` into a `Duration(None)` loss signal while
    /// keeping repeated nulls (a stream that never had one) silent.
    duration_known: bool,
    pending: HashMap<u64, PendingCommand>,
    /// FIFO identity consumed by every accepted Load/Stop, including a validation later
    /// cancelled before dispatch. Kept separate so readiness follows only an issued boundary.
    admitted_file_generation: u64,
    /// Latest physical load/alias/stop boundary published by this actor.
    issued_file_generation: u64,
    active_file_generation: Option<u64>,
    active_playlist_entry_id: Option<u64>,
    /// Correlated load lifecycle boundaries. A seek waits through both so the load's own
    /// `playback-restart` can never become that seek's completion proof.
    file_loaded_generation: Option<u64>,
    playback_ready_generation: Option<u64>,
    pending_load_restart_generation: Option<u64>,
    /// Load lifecycle events can race ahead of the reply which supplies generation identity.
    uncorrelated_file_loaded: bool,
    uncorrelated_load_restart: bool,
    /// mpv returns the stable playlist entry ID in each successful `loadfile` reply. This map
    /// is the authoritative correlation boundary; start-file ordering alone is ambiguous when
    /// a redirect or a rapid replacement creates additional entries.
    entry_generations: HashMap<u64, u64>,
    /// Owner-provided semantics keyed by the exact admitted file generation. Redirect children
    /// inherit the generation, while rapid replacements can never inherit the previous source.
    media_source_contexts: HashMap<u64, MediaSourceContext>,
    /// Redirect replacement ranges may be announced before the originating load reply reaches
    /// this client. Retain the relation until the old entry's generation becomes known.
    pending_redirects: HashMap<u64, (u64, u64)>,
    failed_load_generations: HashSet<u64>,
    playlist_identity_mode: PlaylistIdentityMode,
    /// Legacy mpv 0.32 predates playlist-entry IDs in events, command results, and playlist
    /// properties. Keep its accepted direct loads ordered, and explicitly inherit a redirect's
    /// generation into the next start-file event. Newer mpv never consults this fallback.
    legacy_loads: VecDeque<LegacyLoad>,
    legacy_redirect_generation: Option<u64>,
    legacy_latest_playlist_filename: Option<String>,
    legacy_start_waiting: bool,
    /// `--keep-open` makes the observable `eof-reached` property the primary natural-end
    /// signal on every supported mpv; explicit newer end-file reasons remain a fallback.
    eof_emitted: bool,
    /// A reasonless legacy end waits for the ordered idle/start/eof boundary: idle means a
    /// playback failure, start means a redirect, and eof is handled by the property latch.
    legacy_pending_end_generation: Option<u64>,
    /// Property notifications can race ahead of the `loadfile` reply. Hold a small bounded set
    /// until the start-file entry is correlated instead of tagging them as the newest track.
    quarantined_file_events: VecDeque<PlayerEvent>,
    /// Facts delivered after `start-file` but before load-reply correlation. Each field keeps
    /// the latest observation, including an explicit mpv `null`.
    quarantined_cache_facts: QuarantinedCacheFacts,
    /// Raw mpv numeric-property traffic for the opt-in `YTM_PERF` trace. Kept behind an
    /// `Option` so normal runs pay only one predictable branch per incoming IPC line.
    numeric_perf: Option<NumericPerfWindow>,
    audio_output: audio_output::State,
    /// One actor-local seek completion observation produced by the latest parsed IPC line.
    seek_observation: Option<SeekObservation>,
    /// Managed packet-cache policy. Unit parser tests leave this absent.
    cache: Option<CacheRuntime>,
    cache_actions: VecDeque<CacheAction>,
    cache_status: Option<Arc<std::sync::Mutex<SharedLongFormSeekStatus>>>,
    last_confirmed_time: f64,
    paused: bool,
    media_seekable: Option<bool>,
    resume: ResumeCoordinator,
    terminal_failure: Option<ActorExit>,
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct QuarantinedCacheFacts {
    duration_secs: Option<Option<f64>>,
    via_network: Option<Option<bool>>,
    seekable: Option<Option<bool>>,
    partially_seekable: Option<Option<bool>>,
}

const FILE_EVENT_QUARANTINE_CAPACITY: usize = 32;
const REDIRECT_ENTRY_RANGE_CAPACITY: u64 = 1024;
const ENTRY_GENERATION_CAPACITY: usize = 2048;
const PENDING_REDIRECT_CAPACITY: usize = 128;
const LEGACY_LOAD_CAPACITY: usize = 128;
const LOAD_VALIDATION_BACKLOG_CAPACITY: usize = 256;
const INTERNAL_COMMAND_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const INTERACTIVE_SEEK_TIMEOUT: Duration = Duration::from_secs(3);
include!("ipc/seek.rs");

include!("ipc/command_queue.rs");

impl Default for DispatchState {
    fn default() -> Self {
        Self {
            last_sent_time_sec: None,
            last_sent_cache_sec: None,
            duration_known: false,
            pending: HashMap::new(),
            admitted_file_generation: 0,
            issued_file_generation: 0,
            // Property observation echoes before the first app load belong to the initial
            // generation and are harmless; subsequent starts always replace this value by ID.
            active_file_generation: Some(0),
            active_playlist_entry_id: None,
            file_loaded_generation: None,
            playback_ready_generation: None,
            pending_load_restart_generation: None,
            uncorrelated_file_loaded: false,
            uncorrelated_load_restart: false,
            entry_generations: HashMap::new(),
            media_source_contexts: HashMap::new(),
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
            quarantined_cache_facts: QuarantinedCacheFacts::default(),
            numeric_perf: None,
            audio_output: audio_output::State::default(),
            seek_observation: None,
            cache: None,
            cache_actions: VecDeque::new(),
            cache_status: None,
            last_confirmed_time: 0.0,
            paused: true,
            media_seekable: None,
            resume: ResumeCoordinator::default(),
            terminal_failure: None,
        }
    }
}

fn queue_cache_action(state: &mut DispatchState, action: Option<CacheAction>) {
    if let Some(action) = action {
        state.cache_actions.push_back(action);
    }
    publish_cache_status(state);
}

fn command_ready_for_dispatch(state: &DispatchState, command: &PlayerCmd) -> bool {
    let waiting_for_resume_position = matches!(
        command,
        PlayerCmd::SetProperty { name, .. } if name == "pause"
    ) && state.resume.pause_waits_for_position();
    !waiting_for_resume_position
        && (SeekPurpose::for_command(command).is_none()
            || (state.active_file_generation == Some(state.issued_file_generation)
                && state.file_loaded_generation == Some(state.issued_file_generation)
                && state.playback_ready_generation == Some(state.issued_file_generation)))
}

fn publish_cache_status(state: &mut DispatchState) {
    let status = state
        .cache
        .as_mut()
        .and_then(CacheRuntime::take_changed_status);
    if let (Some(status), Some(shared)) = (status, state.cache_status.as_ref()) {
        shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .update(status);
        if super::diagnostics::cache_status(status) {
            tracing::info!(
                requested = status.requested.id(),
                effective = status.effective.id(),
                reason = status.reason.id(),
                file_generation = ?status.file_generation,
                "long-form seek cache state changed"
            );
        }
    }
}

struct PendingCommand {
    label: String,
    file_generation: Option<u64>,
    acknowledgement: Option<crate::util::command_barrier::CommandBarrierSignal>,
    audio_output: Option<audio_output::Pending>,
    terminal_contract: Option<PendingTerminalContract>,
}

#[derive(Clone, Copy, Debug)]
struct PendingTerminalContract {
    operation: &'static str,
    deadline: Instant,
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

include!("ipc/incoming.rs");

const MPV_IPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MPV_IPC_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(15);

/// Connect to the mpv IPC endpoint while mpv finishes starting up.
///
/// Cold Intel Macs can take longer than three seconds to initialize a Homebrew mpv after a
/// resource-heavy build. Keep the retry bounded, but leave enough headroom for that first launch.
pub async fn connect_retry(path: &str) -> io::Result<Stream> {
    let deadline = Instant::now() + MPV_IPC_CONNECT_TIMEOUT;
    loop {
        let name = path.to_fs_name::<GenericFilePath>()?;
        match Stream::connect(name).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                sleep(MPV_IPC_CONNECT_RETRY_DELAY).await;
            }
        }
    }
}

/// Drive one mpv connection: subscribe to progress properties, then loop forwarding
/// mpv events to the runtime (`emit`) and writing player commands (`cmd_rx`) to mpv.
/// Returns when mpv closes the connection or all command senders drop.
pub(crate) async fn run_actor(
    conn: Stream,
    mut cmd_rx: Receiver<PlayerCmd>,
    emit: EventSink,
    intentional_close: Arc<AtomicBool>,
    file_generation_rx: watch::Receiver<u64>,
    cache_runtime: CacheRuntime,
    cache_status: Arc<std::sync::Mutex<SharedLongFormSeekStatus>>,
) {
    let mut state = DispatchState {
        cache: Some(cache_runtime),
        cache_status: Some(cache_status),
        ..DispatchState::default()
    };
    publish_cache_status(&mut state);
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
        // Legacy mpv 0.32 has no playlist entry IDs, but its playlist property still exposes the
        // selected filename. That is the legacy correlation key for rapid replacements.
        (9, "playlist"),
        // Legacy mpv 0.32 also omits end-file reasons from JSON IPC. This observed property is its
        // reliable natural-EOF signal; newer mpv remains covered by explicit end-file data.
        (10, "eof-reached"),
        // Interactive seeks stay in flight until mpv reports a stable seeking transition or a
        // playback restart. This property exists as far back as the legacy mpv 0.32 line.
        (11, "seeking"),
        // Narrow long-form policy facts. Unsupported properties fail only their observation;
        // the player keeps running and the policy remains fail-closed in RAM.
        (12, "demuxer-via-network"),
        (13, "seekable"),
        (14, "partially-seekable"),
        (15, "cache-speed"),
        (16, "paused-for-cache"),
        // Audio-output observation IDs follow the seek/cache range. Dynamic commands start
        // above 19 so replies can never collide with a property subscription.
        (17, "audio-device-list"),
        (18, "audio-device"),
        (19, "current-ao"),
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
    let mut request_id: u64 = 19;
    let mut validating_load: Option<PendingLoadValidation> = None;
    let mut pending_load_boundary: Option<PendingLoadBoundary> = None;
    // This barrier is process-global rather than load-owned. A newer Load/Stop may supersede
    // the validated destination, but it must not erase uncertainty about cache-on-disk still
    // being true in the existing mpv process.
    let mut cache_reset_pending = false;
    let mut command_backlog = VecDeque::new();
    let mut seek_flight: Option<SeekFlight> = None;
    let mut seek_sequence = 0u64;
    let mut interactive_burst_gate = InteractiveBurstGate::default();
    let mut cache_watchdog = tokio::time::interval(Duration::from_secs(1));
    cache_watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let exit = loop {
        if state.audio_output.is_idle()
            && let Some(action) = state.cache_actions.pop_front()
        {
            let Some(action) = prepare_cache_action(&mut state, action) else {
                continue;
            };
            if let CacheAction::EmergencyCloseAndResume {
                file_generation,
                position_secs,
                paused,
                reason,
            } = action
            {
                break ActorExit::CacheEmergency {
                    file_generation,
                    position_secs,
                    paused,
                    reason,
                };
            }
            if let Err(error) =
                dispatch_cache_action(&conn, &emit, &mut state, &mut request_id, action).await
            {
                tracing::error!(%error, "managed cache policy IPC write failed");
                break transport_exit_or_shutdown(
                    &cmd_rx,
                    &intentional_close,
                    cache_io_failure_exit(
                        &state,
                        crate::player::long_form_seek::CacheReason::PropertyVerificationFailed,
                    ),
                );
            }
        }
        if cache_reset_pending
            && state.cache.as_ref().is_some_and(|cache| {
                cache.status().effective == CacheEffectiveState::LatchedUntilClose
            })
        {
            if let Some(cache) = state.cache.as_mut() {
                cache.close_media();
            }
            cache_reset_pending = false;
            publish_cache_status(&mut state);
        }
        if pending_load_boundary
            .as_ref()
            .and_then(PendingLoadBoundary::supersedable_generation)
            .is_some_and(|generation| *file_generation_rx.borrow() > generation)
        {
            let superseded = pending_load_boundary
                .take()
                .expect("superseded validated load remains installed");
            superseded.record_resume_superseded();
            tracing::debug!(
                file_generation = superseded.file_generation(),
                "discarded validated load after a newer file admission"
            );
        }
        // Pull every command already waiting in the bounded transport into the semantic queue
        // before selecting a dispatch target. A ready synthetic burst therefore collapses at
        // the actor boundary instead of leaking an arbitrary direct-channel prefix to mpv.
        if validating_load.is_none() && seek_flight.is_none() {
            while command_backlog.len() < LOAD_VALIDATION_BACKLOG_CAPACITY {
                match cmd_rx.try_recv() {
                    Ok(cmd) => {
                        interactive_burst_gate.observe_command(&cmd, Instant::now());
                        if !supersede_pending_load_boundary(&cmd, &mut pending_load_boundary) {
                            accept_actor_command(
                                &mut state,
                                cmd,
                                &mut validating_load,
                                &mut command_backlog,
                                &mut seek_flight,
                            );
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                    | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
        }
        // A command admitted while cache cleanup completed must get the first chance to strip a
        // recovery resume intent. Only then may the newly-ready physical load boundary dispatch.
        let pending_load_boundary_ready = pending_load_boundary
            .as_ref()
            .is_some_and(|boundary| !boundary.wait_for_cache_reset() || !cache_reset_pending);
        if pending_load_boundary_ready {
            let boundary = pending_load_boundary
                .take()
                .expect("ready load boundary remains installed");
            let result = match boundary {
                PendingLoadBoundary::Validated(load) => {
                    dispatch_validated_load(&conn, &mut state, &mut request_id, load).await
                }
                PendingLoadBoundary::RejectedStop {
                    file_generation, ..
                } => {
                    dispatch_rejected_load_stop(
                        &conn,
                        &emit,
                        &mut state,
                        &mut request_id,
                        file_generation,
                    )
                    .await
                }
            };
            if let Err(error) = result {
                break transport_exit_or_shutdown(
                    &cmd_rx,
                    &intentional_close,
                    ActorExit::Write {
                        operation: "load command",
                        error,
                    },
                );
            }
        }
        // Every command passes through this semantic queue. While a seek owns mpv's lifecycle
        // evidence, later targets collapse where allowed and ordering barriers wait.
        let dispatch_check_at = Instant::now();
        let next_command_ready = state
            .resume
            .front_command()
            .or_else(|| command_backlog.front())
            .is_none_or(|command| {
                command_ready_for_dispatch(&state, command)
                    && interactive_burst_gate.command_ready(command, dispatch_check_at)
            });
        if validating_load.is_none()
            && pending_load_boundary.is_none()
            && !cache_reset_pending
            && seek_flight.is_none()
            && !state.resume.is_awaiting_boundary()
            && state.audio_output.is_idle()
            && next_command_ready
            && let Some(cmd) = pop_next_actor_command(&mut state, &mut command_backlog)
        {
            let seek_purpose = SeekPurpose::for_command(&cmd);
            interactive_burst_gate.dispatched(&cmd, Instant::now());
            if matches!(cmd, PlayerCmd::Stop)
                && let Some(action) = state
                    .cache
                    .as_mut()
                    .and_then(CacheRuntime::prepare_replacement)
            {
                queue_cache_action(&mut state, Some(action));
                cache_reset_pending = true;
                command_backlog.push_front(cmd);
                publish_cache_status(&mut state);
                continue;
            }
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
                Ok(validation) => {
                    validating_load = validation;
                    if let Some(purpose) = seek_purpose {
                        seek_sequence = seek_sequence.wrapping_add(1);
                        let flight_generation = state.issued_file_generation;
                        begin_resume_seek_observation(&mut state, flight_generation, request_id);
                        seek_flight = Some(SeekFlight::new(
                            seek_sequence,
                            request_id,
                            flight_generation,
                            purpose,
                            state.media_seekable == Some(false),
                        ));
                        tracing::trace!(
                            sequence = seek_sequence,
                            request_id,
                            file_generation = flight_generation,
                            "seek completion flight dispatched"
                        );
                    }
                }
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

        let resume_terminal_deadline = resume_position_terminal_deadline(&state);
        let pending_terminal_contract = earliest_terminal_contract(&state);
        let audio_device_selection_deadline = state.audio_output.tokio_deadline();
        tokio::select! {
            cmd = cmd_rx.recv(), if command_backlog.len() < LOAD_VALIDATION_BACKLOG_CAPACITY => {
                if let Some(exit) = actor_handlers::handle_command(
                    cmd,
                    &mut state,
                    &mut validating_load,
                    &mut pending_load_boundary,
                    &mut command_backlog,
                    &mut seek_flight,
                    &mut interactive_burst_gate,
                ) {
                    break exit;
                }
            },
            validation = async {
                let task = &mut validating_load
                    .as_mut()
                    .expect("validation branch is guarded")
                    .task;
                task.await
            }, if validating_load.is_some() => actor_handlers::handle_validation(
                validation,
                &emit,
                &mut state,
                &mut validating_load,
                &mut pending_load_boundary,
                &mut cache_reset_pending,
            ),
            // Bounded read (shared with the remote protocol): a well-behaved mpv sends tiny
            // JSON lines, so a line past the cap means a broken/hostile endpoint — tear down
            // rather than let one line grow memory without limit.
            read = crate::util::io::read_bounded_line(&mut reader, &mut line, MPV_IPC_MAX_LINE) => {
                if let Some(exit) = actor_handlers::handle_read(
                    read,
                    &conn,
                    &cmd_rx,
                    &emit,
                    &intentional_close,
                    &mut state,
                    &mut request_id,
                    &mut seek_flight,
                    &mut line,
                ).await {
                    break exit;
                }
            },
            _ = async {
                let deadline = seek_flight
                    .as_ref()
                    .expect("timeout branch is guarded")
                    .wake_deadline();
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            }, if seek_flight.is_some() => match actor_handlers::handle_seek_timeout(
                &cmd_rx,
                &emit,
                &intentional_close,
                &mut state,
                &mut seek_flight,
            ) {
                Ok(()) => continue,
                Err(exit) => break exit,
            },
            _ = async {
                let deadline = resume_terminal_deadline
                    .expect("resume terminal branch is guarded");
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            }, if resume_terminal_deadline.is_some() => break actor_handlers::handle_resume_timeout(
                    &cmd_rx,
                    &intentional_close,
                    &state,
                ),
            _ = async {
                let deadline = audio_device_selection_deadline
                    .expect("audio device timeout branch is guarded");
                tokio::time::sleep_until(deadline).await;
            }, if audio_device_selection_deadline.is_some() => actor_handlers::handle_audio_timeout(
                &mut state,
                &emit,
            ),
            _ = async {
                let contract = pending_terminal_contract
                    .expect("internal terminal branch is guarded");
                tokio::time::sleep_until(
                    tokio::time::Instant::from_std(contract.deadline),
                ).await;
            }, if pending_terminal_contract.is_some() => break actor_handlers::handle_terminal_timeout(
                pending_terminal_contract,
                &cmd_rx,
                &intentional_close,
                &state,
            ),
            _ = cache_watchdog.tick() => {
                if let Some(exit) = actor_handlers::handle_cache_watchdog(
                    &conn,
                    &cmd_rx,
                    &intentional_close,
                    &mut state,
                    &mut request_id,
                ).await {
                    break exit;
                }
            },
            _ = async {
                let deadline = interactive_burst_gate
                    .wake_deadline()
                    .expect("debounce branch is guarded");
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            }, if state.audio_output.is_idle()
                && seek_flight.is_none()
                && !next_command_ready
                && interactive_burst_gate
                    .wake_deadline()
                    .is_some_and(|deadline| Instant::now() < deadline)
                && state
                    .resume
                    .front_command()
                    .or_else(|| command_backlog.front())
                    .is_some_and(PlayerCmd::is_interactive_seek) => actor_handlers::handle_debounce_ready(),
            // Give the read side a scheduling point between queued command writes so replies and
            // lifecycle events cannot be starved by a full actor backlog.
            _ = tokio::task::yield_now(), if validating_load.is_none()
                && pending_load_boundary.is_none()
                && !cache_reset_pending
                && seek_flight.is_none()
                && state.audio_output.is_idle()
                && next_command_ready
                && !command_backlog.is_empty() => actor_handlers::handle_dispatch_yield(),
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
    if let PlayerCmd::SetLongFormSeekOptimization(requested) = &cmd {
        let action = state
            .cache
            .as_mut()
            .and_then(|cache| cache.update_requested(*requested));
        queue_cache_action(state, action);
        return Ok(None);
    }
    let load = match cmd {
        PlayerCmd::Load(load) => Some((load.url().to_owned(), load.source_context(), None)),
        PlayerCmd::LoadWithResume(resume) => {
            Some((resume.url.clone(), resume.source_context, Some(resume)))
        }
        cmd => {
            if matches!(cmd, PlayerCmd::Stop) {
                state.issued_file_generation = reserve_file_generation(state);
            }
            dispatch_command(conn, emit, state, request_id, cmd, None).await?;
            None
        }
    };
    if let Some((url, source_context, resume)) = load {
        *request_id = request_id.wrapping_add(1);
        // The public handle has already admitted this generation, but the actor does not publish
        // it as issued until validation has succeeded and `loadfile` is actually dispatched.
        // A seek/pause that supersedes recovery validation can therefore keep using the ready
        // current generation instead of waiting forever for a file that was never sent to mpv.
        let file_generation = reserve_file_generation(state);
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
            resume: resume::ResumeLoad::from_request(resume),
            source_context,
        }));
    }
    Ok(None)
}

fn reserve_file_generation(state: &mut DispatchState) -> u64 {
    state.admitted_file_generation = state
        .admitted_file_generation
        .max(state.issued_file_generation)
        .wrapping_add(1);
    state.admitted_file_generation
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
                    Err(error) => {
                        LoadValidationOutcome::Rejected(error.handoff_reason().to_owned())
                    }
                };
            }
        }
    }
}

fn install_resume_state(
    state: &mut DispatchState,
    file_generation: u64,
    request: super::recovery::LoadWithResume,
) {
    state.last_confirmed_time = request.position_secs;
    state.resume.install(file_generation, request);
}

async fn dispatch_validated_load(
    conn: &Stream,
    state: &mut DispatchState,
    request_id: &mut u64,
    load: ValidatedLoad,
) -> io::Result<()> {
    if load
        .resume
        .request()
        .is_some_and(super::recovery::LoadWithResume::forces_ram_only)
        && let Some(cache) = state.cache.as_mut()
    {
        cache.force_next_media_ram_only();
    }
    if state.media_source_contexts.len() >= ENTRY_GENERATION_CAPACITY {
        let oldest = state
            .media_source_contexts
            .keys()
            .filter(|generation| **generation != load.file_generation)
            .min()
            .copied();
        if let Some(oldest) = oldest {
            state.media_source_contexts.remove(&oldest);
        }
    }
    state
        .media_source_contexts
        .insert(load.file_generation, load.source_context);
    reset_file_state(state);
    if state.playlist_identity_mode != PlaylistIdentityMode::EntryIds
        && state.legacy_loads.len() >= LEGACY_LOAD_CAPACITY
    {
        return Err(io::Error::other(
            "legacy mpv load correlation queue saturated",
        ));
    }
    let needs_identity_query = state.playlist_identity_mode != PlaylistIdentityMode::EntryIds;
    let load_reserved =
        remember_pending_load(state, load.request_id, load.file_generation, "loadfile");
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
            load.file_generation,
            "loadfile identity",
        )
    });
    if !load_reserved || !identity_reserved {
        state.pending.remove(&load.request_id);
        if let Some(identity_request_id) = identity_request_id {
            state.pending.remove(&identity_request_id);
        }
        return Err(io::Error::other(
            "mpv load identity correlation queue saturated",
        ));
    }
    if load.resume.is_some() {
        *request_id = request_id.wrapping_add(1);
        let pause_request_id = *request_id;
        remember_pending_command(state, pause_request_id, "recovery pre-load pause");
        write_json(
            conn,
            &proto::cmd_set_property("pause", &serde_json::Value::Bool(true), pause_request_id),
        )
        .await?;
    }
    state.issued_file_generation = load.file_generation;
    write_json(
        conn,
        &proto::cmd_loadfile(&load.url, "replace", load.request_id),
    )
    .await?;
    if let Some(request) = load.resume.into_request() {
        install_resume_state(state, load.file_generation, request);
    }
    state.legacy_latest_playlist_filename = None;
    state.legacy_loads.push_back(LegacyLoad {
        generation: load.file_generation,
        url: load.url,
        replied: false,
    });

    // mpv 0.33+ exposes stable IDs. Legacy mpv 0.32 exposes only the selected filename, so its
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

fn install_rejected_load_stop_boundary(state: &mut DispatchState, file_generation: u64) {
    reset_file_state(state);
    state.issued_file_generation = file_generation;
    state.admitted_file_generation = state.admitted_file_generation.max(file_generation);
    // The old physical file may emit a final stop event, but no later uncorrelated property may
    // be presented as the rejected owner's media while the internal Stop is in flight.
    state.active_file_generation = None;
}

async fn dispatch_rejected_load_stop(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    request_id: &mut u64,
    file_generation: u64,
) -> io::Result<()> {
    install_rejected_load_stop_boundary(state, file_generation);
    dispatch_command(
        conn,
        emit,
        state,
        request_id,
        PlayerCmd::Stop,
        Some("rejected-load stop"),
    )
    .await
}

async fn dispatch_cache_action(
    conn: &Stream,
    _emit: &EventSink,
    state: &mut DispatchState,
    request_id: &mut u64,
    action: CacheAction,
) -> io::Result<()> {
    if let CacheAction::EmergencyCloseAndResume {
        file_generation,
        position_secs,
        paused,
        reason,
    } = action
    {
        tracing::error!(
            file_generation,
            position_secs,
            paused,
            reason = reason.id(),
            "managed cache safety boundary requires player recycle"
        );
        return Err(io::Error::other(format!(
            "managed cache safety recycle required ({})",
            reason.id()
        )));
    }

    *request_id = request_id.wrapping_add(1);
    let actor_request_id = *request_id;
    let json = match action {
        CacheAction::QueryRanges(_) => {
            proto::cmd_get_property("demuxer-cache-state", actor_request_id)
        }
        CacheAction::SetCacheOnDisk { enabled, .. } => proto::cmd_set_property(
            "cache-on-disk",
            &serde_json::Value::Bool(enabled),
            actor_request_id,
        ),
        CacheAction::ReadBackCacheOnDisk { .. } => {
            proto::cmd_get_property("cache-on-disk", actor_request_id)
        }
        CacheAction::EmergencyCloseAndResume { .. } => unreachable!("handled above"),
    };
    if let Some(cache) = state.cache.as_mut() {
        cache.register(actor_request_id, action);
    }
    write_json(conn, &json).await
}

async fn dispatch_file_cache_query(
    conn: &Stream,
    state: &mut DispatchState,
    request_id: &mut u64,
) -> io::Result<()> {
    let Some(cache) = state.cache.as_mut() else {
        return Ok(());
    };
    let candidate = request_id.wrapping_add(1);
    if !cache.register_file_bytes_query(candidate) {
        return Ok(());
    }
    *request_id = candidate;
    write_json(
        conn,
        &proto::cmd_get_property("demuxer-cache-state", candidate),
    )
    .await
}

async fn dispatch_cache_speed_query(
    conn: &Stream,
    state: &mut DispatchState,
    request_id: &mut u64,
) -> io::Result<()> {
    let Some(cache) = state.cache.as_mut() else {
        return Ok(());
    };
    let candidate = request_id.wrapping_add(1);
    if !cache.register_cache_speed_query(candidate) {
        return Ok(());
    }
    *request_id = candidate;
    write_json(conn, &proto::cmd_get_property("cache-speed", candidate)).await
}

async fn dispatch_command(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    request_id: &mut u64,
    cmd: PlayerCmd,
    terminal_operation: Option<&'static str>,
) -> io::Result<()> {
    let recovery_terminal_operation = recovery_terminal_operation(state, &cmd);
    let recovery_pause_restore = recovery_terminal_operation == Some("recovery pause restore");
    let terminal_operation = command_terminal_operation(state, &cmd, terminal_operation);
    if let PlayerCmd::SeekAbsolute {
        seconds,
        precision: super::SeekPrecision::InteractiveFast,
    } = &cmd
    {
        let action = state
            .cache
            .as_mut()
            .and_then(|cache| cache.before_interactive_seek(state.last_confirmed_time, *seconds));
        if let Some(action) = action {
            // The immutable range snapshot must be ordered before the user seek, but its reply
            // never delays that seek.
            dispatch_cache_action(conn, emit, state, request_id, action).await?;
            publish_cache_status(state);
        }
    }
    *request_id = request_id.wrapping_add(1);
    let command_request_id = *request_id;
    if audio_output::is_command(&cmd) {
        return audio_output::dispatch_command(conn, emit, state, command_request_id, cmd).await;
    }
    let (json, label, acknowledgement) = match cmd {
        PlayerCmd::Load(_) | PlayerCmd::LoadWithResume(_) => {
            unreachable!("loads start in begin_or_dispatch_command")
        }
        PlayerCmd::SetLongFormSeekOptimization(_) => {
            unreachable!("policy updates are actor-local commands")
        }
        PlayerCmd::RefreshAudioDevices | PlayerCmd::SelectAudioDevice { .. } => {
            unreachable!("audio-output commands are routed above")
        }
        PlayerCmd::Stop => {
            state.legacy_loads.clear();
            state.legacy_redirect_generation = None;
            state.legacy_latest_playlist_filename = None;
            state.legacy_start_waiting = false;
            state.last_sent_time_sec = None;
            state.last_sent_cache_sec = None;
            state.duration_known = false;
            state.eof_emitted = false;
            state.legacy_pending_end_generation = None;
            state.file_loaded_generation = None;
            state.playback_ready_generation = None;
            state.pending_load_restart_generation = None;
            state.uncorrelated_file_loaded = false;
            state.uncorrelated_load_restart = false;
            state.media_source_contexts.clear();
            state.quarantined_cache_facts = QuarantinedCacheFacts::default();
            if let Some(cache) = state.cache.as_mut() {
                cache.close_media();
            }
            publish_cache_status(state);
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
        PlayerCmd::SeekAbsolute { seconds, precision } => (
            proto::cmd_seek_absolute(seconds, precision, command_request_id),
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
        PlayerCmd::TrackedProperty(tracked) => {
            let label = format!("set_property {}", tracked.name);
            (
                proto::cmd_set_property(&tracked.name, &tracked.value, command_request_id),
                label,
                Some(tracked.acknowledgement),
            )
        }
    };
    if let Some(operation) = terminal_operation {
        remember_pending_terminal(
            state,
            command_request_id,
            state.issued_file_generation,
            label,
            operation,
        )?;
        if recovery_pause_restore {
            state.resume.finish_dispatch();
        }
    } else if let Some(acknowledgement) = acknowledgement {
        remember_pending_tracked(state, command_request_id, label, acknowledgement)?;
    } else {
        remember_pending_command(state, command_request_id, label);
    }
    write_json(conn, &json).await
}

fn recovery_terminal_operation(state: &DispatchState, command: &PlayerCmd) -> Option<&'static str> {
    state.resume.dispatch_generation()?;
    match command {
        PlayerCmd::SeekAbsolute {
            precision: super::SeekPrecision::Exact,
            ..
        } => Some("recovery exact seek"),
        PlayerCmd::SetProperty { name, .. } if name == "pause" => Some("recovery pause restore"),
        _ => None,
    }
}

fn command_terminal_operation(
    state: &DispatchState,
    command: &PlayerCmd,
    explicit: Option<&'static str>,
) -> Option<&'static str> {
    explicit
        .or_else(|| recovery_terminal_operation(state, command))
        .or_else(|| SeekPurpose::for_command(command).map(|_| "seek"))
}

#[cfg(test)]
mod tests;
