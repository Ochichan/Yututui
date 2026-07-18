use super::*;

pub(super) fn handle_command(
    cmd: Option<PlayerCmd>,
    state: &mut DispatchState,
    validating_load: &mut Option<PendingLoadValidation>,
    pending_load_boundary: &mut Option<PendingLoadBoundary>,
    command_backlog: &mut VecDeque<PlayerCmd>,
    seek_flight: &mut Option<SeekFlight>,
    interactive_burst_gate: &mut InteractiveBurstGate,
) -> Option<ActorExit> {
    let Some(cmd) = cmd else {
        if let Some(validation) = validating_load.take() {
            validation.task.abort();
        }
        return Some(ActorExit::CommandChannelClosed);
    };
    interactive_burst_gate.observe_command(&cmd, Instant::now());
    if !supersede_pending_load_boundary(&cmd, pending_load_boundary) {
        accept_actor_command(state, cmd, validating_load, command_backlog, seek_flight);
    }
    None
}

pub(super) fn handle_validation(
    validation: Result<LoadValidationOutcome, tokio::task::JoinError>,
    emit: &EventSink,
    state: &mut DispatchState,
    validating_load: &mut Option<PendingLoadValidation>,
    pending_load_boundary: &mut Option<PendingLoadBoundary>,
    cache_reset_pending: &mut bool,
) {
    let pending = validating_load
        .take()
        .expect("completed validation remains installed");
    let validated = match validation {
        Ok(result) => result,
        Err(error) => LoadValidationOutcome::Rejected(format!(
            "playback destination validation task failed: {error}"
        )),
    };
    let boundary = finish_load_validation(emit, state, pending, validated);
    if boundary
        .as_ref()
        .is_some_and(PendingLoadBoundary::wait_for_cache_reset)
    {
        *cache_reset_pending = true;
    }
    *pending_load_boundary = boundary;
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_read(
    read: io::Result<crate::util::io::BoundedLine>,
    conn: &Stream,
    cmd_rx: &Receiver<PlayerCmd>,
    emit: &EventSink,
    intentional_close: &Arc<AtomicBool>,
    state: &mut DispatchState,
    request_id: &mut u64,
    seek_flight: &mut Option<SeekFlight>,
    line: &mut Vec<u8>,
) -> Option<ActorExit> {
    match read {
        Ok(crate::util::io::BoundedLine::Eof) => Some(transport_exit_or_shutdown(
            cmd_rx,
            intentional_close,
            ActorExit::Eof,
        )),
        Err(error) => Some(transport_exit_or_shutdown(
            cmd_rx,
            intentional_close,
            ActorExit::Read(error),
        )),
        Ok(crate::util::io::BoundedLine::TooLarge) => Some(transport_exit_or_shutdown(
            cmd_rx,
            intentional_close,
            ActorExit::OversizedLine,
        )),
        Ok(crate::util::io::BoundedLine::Line) => {
            let text = String::from_utf8_lossy(line);
            dispatch_incoming(&text, emit, state);
            if let Some(selection) = state.audio_output.followup.take()
                && let Err(error) =
                    audio_output::perform_followup(conn, emit, state, request_id, selection).await
            {
                return Some(transport_exit_or_shutdown(
                    cmd_rx,
                    intentional_close,
                    ActorExit::Write {
                        operation: "audio device command",
                        error,
                    },
                ));
            }
            if let Some(failure) = state.terminal_failure.take() {
                return Some(transport_exit_or_shutdown(
                    cmd_rx,
                    intentional_close,
                    failure,
                ));
            }
            if let Some(observation) = state.seek_observation.take()
                && let Some(completed) =
                    observe_seek_flight(seek_flight, observation, state.active_file_generation)
            {
                finish_seek_flight(emit, state, completed);
            }
            // Keep a partial frame across cancellation of the read branch by a ready
            // command. Clear only after a complete newline-delimited frame was handled.
            line.clear();
            None
        }
    }
}

pub(super) fn handle_seek_timeout(
    cmd_rx: &Receiver<PlayerCmd>,
    emit: &EventSink,
    intentional_close: &Arc<AtomicBool>,
    state: &mut DispatchState,
    seek_flight: &mut Option<SeekFlight>,
) -> Result<(), ActorExit> {
    if let Some(completed) = take_settled_seek_flight(seek_flight, Instant::now()) {
        finish_seek_flight(emit, state, completed);
        return Ok(());
    }
    let timed_out = seek_flight
        .take()
        .expect("timed out seek flight remains installed");
    tracing::warn!(
        sequence = timed_out.sequence,
        request_id = timed_out.request_id,
        file_generation = timed_out.file_generation,
        timeout_ms = INTERACTIVE_SEEK_TIMEOUT.as_millis() as u64,
        "seek completion became ambiguous; recycling player"
    );
    Err(transport_exit_or_shutdown(
        cmd_rx,
        intentional_close,
        ActorExit::SeekCausalityLost,
    ))
}

pub(super) fn handle_resume_timeout(
    cmd_rx: &Receiver<PlayerCmd>,
    intentional_close: &Arc<AtomicBool>,
    state: &DispatchState,
) -> ActorExit {
    if state.resume.telemetry_is_source_recovery() {
        super::super::diagnostics::source_recovery_outcome(
            super::super::diagnostics::SourceRecoveryOutcome::ResumeRejected,
        );
    }
    tracing::warn!(
        file_generation = state.issued_file_generation,
        timeout_ms = RESUME_POSITION_TERMINAL_TIMEOUT.as_millis() as u64,
        "resume seek completed without a correlated position; recycling player"
    );
    transport_exit_or_shutdown(cmd_rx, intentional_close, ActorExit::SeekCausalityLost)
}

pub(super) fn handle_audio_timeout(state: &mut DispatchState, emit: &EventSink) {
    audio_output::timeout(state, emit);
}

pub(super) fn handle_terminal_timeout(
    pending_terminal_contract: Option<PendingTerminalContract>,
    cmd_rx: &Receiver<PlayerCmd>,
    intentional_close: &Arc<AtomicBool>,
    state: &DispatchState,
) -> ActorExit {
    let contract =
        pending_terminal_contract.expect("expired internal terminal contract remains captured");
    tracing::warn!(
        operation = contract.operation,
        timeout_ms = INTERNAL_COMMAND_REPLY_TIMEOUT.as_millis() as u64,
        "internal recovery command timed out; recycling player"
    );
    let failure = expired_terminal_failure(state, Instant::now())
        .expect("selected internal terminal deadline has expired");
    transport_exit_or_shutdown(cmd_rx, intentional_close, failure)
}

pub(super) async fn handle_cache_watchdog(
    conn: &Stream,
    cmd_rx: &Receiver<PlayerCmd>,
    intentional_close: &Arc<AtomicBool>,
    state: &mut DispatchState,
    request_id: &mut u64,
) -> Option<ActorExit> {
    let (actions, watchdog_needed, rate_sampling_needed) = match state.cache.as_mut() {
        Some(cache) => (
            cache.expire_requests(),
            cache.watchdog_needed(),
            cache.rate_sampling_needed(),
        ),
        None => (Vec::new(), false, false),
    };
    state.cache_actions.extend(actions);
    poll_cache_persistence(state);
    publish_cache_status(state);
    if state.audio_output.is_idle()
        && rate_sampling_needed
        && let Err(error) = dispatch_cache_speed_query(conn, state, request_id).await
    {
        return Some(transport_exit_or_shutdown(
            cmd_rx,
            intentional_close,
            ActorExit::Write {
                operation: "cache rate sample",
                error,
            },
        ));
    }
    if state.audio_output.is_idle()
        && watchdog_needed
        && let Err(error) = dispatch_file_cache_query(conn, state, request_id).await
    {
        tracing::error!(%error, "managed cache watchdog IPC write failed");
        return Some(transport_exit_or_shutdown(
            cmd_rx,
            intentional_close,
            cache_io_failure_exit(
                state,
                crate::player::long_form_seek::CacheReason::PropertyVerificationFailed,
            ),
        ));
    }
    None
}

pub(super) fn handle_debounce_ready() {}

pub(super) fn handle_dispatch_yield() {}
