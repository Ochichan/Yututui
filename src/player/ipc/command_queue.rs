struct PendingLoadValidation {
    request_id: u64,
    file_generation: u64,
    task: tokio::task::JoinHandle<LoadValidationOutcome>,
    resume: Option<super::recovery::LoadWithResume>,
    /// True until the first user transport intent is merged into the pending physical load.
    restore_transport: bool,
    source_context: MediaSourceContext,
}

struct ValidatedLoad {
    request_id: u64,
    file_generation: u64,
    url: String,
    resume: Option<super::recovery::LoadWithResume>,
    restore_transport: bool,
    source_context: MediaSourceContext,
    wait_for_cache_reset: bool,
}

enum PendingLoadBoundary {
    Validated(ValidatedLoad),
    RejectedStop {
        file_generation: u64,
        wait_for_cache_reset: bool,
    },
}

impl PendingLoadBoundary {
    fn file_generation(&self) -> u64 {
        match self {
            Self::Validated(load) => load.file_generation,
            Self::RejectedStop {
                file_generation, ..
            } => *file_generation,
        }
    }

    fn wait_for_cache_reset(&self) -> bool {
        match self {
            Self::Validated(load) => load.wait_for_cache_reset,
            Self::RejectedStop {
                wait_for_cache_reset,
                ..
            } => *wait_for_cache_reset,
        }
    }

    fn supersedable_generation(&self) -> Option<u64> {
        match self {
            Self::Validated(load) => Some(load.file_generation),
            Self::RejectedStop { .. } => None,
        }
    }

    fn record_resume_superseded(&self) {
        if let Self::Validated(load) = self
            && load.restore_transport
        {
            record_resume_outcome(
                load.resume.as_ref(),
                super::diagnostics::SourceRecoveryOutcome::Superseded,
            );
        }
    }
}

#[derive(Clone, Debug)]
struct PendingResume {
    file_generation: u64,
    request: super::recovery::LoadWithResume,
    file_loaded: bool,
}

enum LoadValidationOutcome {
    Validated(String),
    Rejected(String),
    Superseded,
}

#[cfg(test)]
fn stage_actor_command(
    cmd: PlayerCmd,
    validating_load: &mut Option<PendingLoadValidation>,
    backlog: &mut VecDeque<PlayerCmd>,
) {
    let interactive = cmd.is_interactive_seek();
    if let Some(validation) = take_superseded_validation(&cmd, validating_load) {
        validation.task.abort();
        tracing::debug!(
            file_generation = validation.file_generation,
            "cancelled superseded playback destination validation"
        );
    }
    stage_actor_command_without_validation(cmd, backlog, interactive);
}

fn take_superseded_validation(
    cmd: &PlayerCmd,
    validating_load: &mut Option<PendingLoadValidation>,
) -> Option<PendingLoadValidation> {
    let invalidates_pending_load = matches!(
        cmd,
        PlayerCmd::Load(_) | PlayerCmd::LoadWithResume(_) | PlayerCmd::Stop
    );
    invalidates_pending_load
        .then(|| validating_load.take())
        .flatten()
}

fn stage_actor_command_without_validation(
    cmd: PlayerCmd,
    backlog: &mut VecDeque<PlayerCmd>,
    interactive: bool,
) {
    let coalesced = pending::push_pending_command(
        backlog,
        cmd,
        LOAD_VALIDATION_BACKLOG_CAPACITY,
        crate::util::delivery::DeliveryError::Busy,
    )
    .expect("actor backlog receive guard reserves capacity");
    if interactive && coalesced {
        super::diagnostics::interactive_coalesced();
    }
}

fn accept_actor_command(
    state: &mut DispatchState,
    cmd: PlayerCmd,
    validating_load: &mut Option<PendingLoadValidation>,
    backlog: &mut VecDeque<PlayerCmd>,
    seek_flight: &mut Option<SeekFlight>,
) {
    if merge_issued_pending_resume_command(state, &cmd) {
        return;
    }
    if merge_post_load_resume_command(state, &cmd) {
        return;
    }
    cancel_resume_post_load_if_superseded(state, &cmd);
    let mut consumed_by_pending_resume = false;
    mark_seek_superseded(seek_flight, &cmd);
    if cmd.invalidates_file_generation() {
        if let Some(in_flight) = seek_flight.take() {
            tracing::debug!(
                sequence = in_flight.sequence,
                "invalidated seek for replaced media"
            );
        }
        backlog.retain(|pending| !pending.is_interactive_seek());
    }
    if let Some(validation) = take_superseded_validation(&cmd, validating_load) {
        record_resume_outcome(
            validation
                .restore_transport
                .then_some(validation.resume.as_ref())
                .flatten(),
            super::diagnostics::SourceRecoveryOutcome::Superseded,
        );
        validation.task.abort();
        tracing::debug!(
            file_generation = validation.file_generation,
            "cancelled superseded playback destination validation"
        );
    } else if let Some(validation) = validating_load.as_mut()
        && validation.resume.is_some()
        && supersedes_pending_resume(&cmd)
    {
        let can_alias = validation.restore_transport
            && validation
                .resume
                .as_ref()
                .is_some_and(|resume| resume.episode_id.get() != 0)
            && state.active_file_generation == Some(state.issued_file_generation)
            && state.file_loaded_generation == Some(state.issued_file_generation)
            && state.playback_ready_generation == Some(state.issued_file_generation);
        if can_alias {
            let validation = validating_load
                .take()
                .expect("aliasable recovery validation remains installed");
            rebase_cancelled_recovery(state, validation.file_generation, validation.source_context);
            record_resume_outcome(
                validation.resume.as_ref(),
                super::diagnostics::SourceRecoveryOutcome::Superseded,
            );
            validation.task.abort();
        } else if let Some(resume) = validation.resume.as_mut()
            && merge_pending_resume_command(resume, &cmd)
        {
            if validation.restore_transport {
                record_resume_outcome(
                    Some(resume),
                    super::diagnostics::SourceRecoveryOutcome::Superseded,
                );
                validation.restore_transport = false;
            }
            consumed_by_pending_resume = true;
        }
    }
    if !consumed_by_pending_resume {
        let interactive = cmd.is_interactive_seek();
        stage_actor_command_without_validation(cmd, backlog, interactive);
    }
}

fn rebase_cancelled_recovery(
    state: &mut DispatchState,
    generation: u64,
    source_context: MediaSourceContext,
) {
    let previous = state.issued_file_generation;
    if generation == previous {
        return;
    }
    state.issued_file_generation = generation;
    state.admitted_file_generation = state.admitted_file_generation.max(generation);
    if state.active_file_generation == Some(previous) {
        state.active_file_generation = Some(generation);
    }
    if state.file_loaded_generation == Some(previous) {
        state.file_loaded_generation = Some(generation);
    }
    if state.playback_ready_generation == Some(previous) {
        state.playback_ready_generation = Some(generation);
    }
    if state.pending_load_restart_generation == Some(previous) {
        state.pending_load_restart_generation = Some(generation);
    }
    for mapped in state.entry_generations.values_mut() {
        if *mapped == previous {
            *mapped = generation;
        }
    }
    for load in &mut state.legacy_loads {
        if load.generation == previous {
            load.generation = generation;
        }
    }
    if state.legacy_redirect_generation == Some(previous) {
        state.legacy_redirect_generation = Some(generation);
    }
    if state.legacy_pending_end_generation == Some(previous) {
        state.legacy_pending_end_generation = Some(generation);
    }
    for pending in state.pending.values_mut() {
        if pending.file_generation == Some(previous) {
            pending.file_generation = Some(generation);
        }
    }
    if state.failed_load_generations.remove(&previous) {
        state.failed_load_generations.insert(generation);
    }
    if let Some(pending) = state
        .pending_resume
        .as_mut()
        .filter(|pending| pending.file_generation == previous)
    {
        pending.file_generation = generation;
    }
    if let Some(gate) = state
        .resume_telemetry
        .as_mut()
        .filter(|gate| gate.file_generation == previous)
    {
        gate.file_generation = generation;
    }
    if state.resume_post_load_generation == Some(previous) {
        state.resume_post_load_generation = Some(generation);
    }
    state.media_source_contexts.remove(&previous);
    state
        .media_source_contexts
        .insert(generation, source_context);
    if let Some(cache) = state.cache.as_mut() {
        cache.rebase_file_generation(previous, generation);
    }
    for action in &mut state.cache_actions {
        action.rebase_file_generation(previous, generation);
    }
    publish_cache_status(state);
}

fn supersedes_pending_resume(cmd: &PlayerCmd) -> bool {
    matches!(
        cmd,
        PlayerCmd::Load(_)
            | PlayerCmd::LoadWithResume(_)
            | PlayerCmd::Stop
            | PlayerCmd::CyclePause
            | PlayerCmd::SeekRelative(_)
            | PlayerCmd::SeekAbsolute { .. }
    ) || matches!(cmd, PlayerCmd::SetProperty { name, .. } if name == "pause")
}

fn merge_pending_resume_command(
    resume: &mut super::recovery::LoadWithResume,
    command: &PlayerCmd,
) -> bool {
    match command {
        PlayerCmd::SeekAbsolute { seconds, .. } => {
            resume.position_secs = crate::playback_policy::norm_position(*seconds);
            true
        }
        PlayerCmd::SeekRelative(delta) => {
            resume.position_secs =
                crate::playback_policy::norm_position(resume.position_secs + delta);
            true
        }
        PlayerCmd::CyclePause => {
            resume.paused = !resume.paused;
            true
        }
        PlayerCmd::SetProperty { name, value } if name == "pause" => {
            value.as_bool().is_some_and(|paused| {
                resume.paused = paused;
                true
            })
        }
        _ => false,
    }
}

fn merge_issued_pending_resume_command(state: &mut DispatchState, command: &PlayerCmd) -> bool {
    state
        .pending_resume
        .as_mut()
        .is_some_and(|pending| merge_pending_resume_command(&mut pending.request, command))
}

fn merge_post_load_resume_command(state: &mut DispatchState, command: &PlayerCmd) -> bool {
    let Some(file_generation) = state.resume_post_load_generation else {
        return false;
    };
    match command {
        PlayerCmd::CyclePause | PlayerCmd::SetProperty { .. } => {
            let explicit = match command {
                PlayerCmd::CyclePause => None,
                PlayerCmd::SetProperty { name, value } if name == "pause" => {
                    let Some(paused) = value.as_bool() else {
                        return false;
                    };
                    Some(paused)
                }
                _ => return false,
            };
            state.post_load_commands.iter_mut().any(|queued| {
                let PlayerCmd::SetProperty { name, value } = queued else {
                    return false;
                };
                if name != "pause" {
                    return false;
                }
                let Some(paused) = value.as_bool() else {
                    return false;
                };
                *value = serde_json::Value::Bool(explicit.unwrap_or(!paused));
                true
            })
        }
        PlayerCmd::SeekAbsolute { .. } | PlayerCmd::SeekRelative(_) => {
            let queued_target = state.post_load_commands.iter().find_map(|queued| match queued {
                PlayerCmd::SeekAbsolute { seconds, .. } => Some(*seconds),
                _ => None,
            });
            let current_target = queued_target
                .or_else(|| {
                    state
                        .resume_telemetry
                        .as_ref()
                        .filter(|gate| gate.file_generation == file_generation)
                        .map(|gate| gate.target_secs)
                })
                .unwrap_or(state.last_confirmed_time);
            let target = match command {
                PlayerCmd::SeekAbsolute { seconds, .. } => {
                    crate::playback_policy::norm_position(*seconds)
                }
                PlayerCmd::SeekRelative(delta) => {
                    crate::playback_policy::norm_position(current_target + delta)
                }
                _ => unreachable!("post-load seek match is exhaustive"),
            };
            let mut replaced_queued = false;
            for queued in &mut state.post_load_commands {
                if let PlayerCmd::SeekAbsolute { seconds, .. } = queued {
                    *seconds = target;
                    replaced_queued = true;
                    break;
                }
            }
            if !replaced_queued {
                let before_pause = state
                    .post_load_commands
                    .iter()
                    .position(|queued| {
                        matches!(queued, PlayerCmd::SetProperty { name, .. } if name == "pause")
                    })
                    .unwrap_or(state.post_load_commands.len());
                state
                    .post_load_commands
                    .insert(before_pause, PlayerCmd::exact_seek(target));
            }
            if let Some(gate) = state
                .resume_telemetry
                .as_mut()
                .filter(|gate| gate.file_generation == file_generation)
            {
                gate.target_secs = target;
                gate.expected_request_id = None;
                gate.exact_completed = false;
                gate.buffered_near_target = None;
                gate.latest_seek_position = None;
                gate.terminal_deadline = None;
            } else {
                state.resume_telemetry = Some(ResumeTelemetryGate {
                    file_generation,
                    target_secs: target,
                    expected_request_id: None,
                    exact_completed: false,
                    buffered_near_target: None,
                    latest_seek_position: None,
                    terminal_deadline: None,
                    source_recovery: false,
                });
            }
            true
        }
        _ => false,
    }
}

fn supersede_pending_load_boundary(
    cmd: &PlayerCmd,
    boundary: &mut Option<PendingLoadBoundary>,
) -> bool {
    let Some(PendingLoadBoundary::Validated(load)) = boundary.as_mut() else {
        // A rejected load has already committed its internal Stop boundary. A newer Load/Stop
        // must stay behind that close instead of reviving the old physical media.
        return false;
    };
    if cmd.invalidates_file_generation() {
        let cancelled = boundary
            .take()
            .expect("superseded validated load remains installed");
        cancelled.record_resume_superseded();
        tracing::debug!(
            file_generation = cancelled.file_generation(),
            "cancelled validated load for a newer file boundary"
        );
        return false;
    }
    if load.resume.is_some() && supersedes_pending_resume(cmd) {
        let merged = load
            .resume
            .as_mut()
            .is_some_and(|resume| merge_pending_resume_command(resume, cmd));
        if !merged {
            return false;
        }
        if load.restore_transport {
            record_resume_outcome(
                load.resume.as_ref(),
                super::diagnostics::SourceRecoveryOutcome::Superseded,
            );
            load.restore_transport = false;
        }
        tracing::debug!(
            file_generation = load.file_generation,
            "merged user transport into retained validated load boundary"
        );
        return true;
    }
    false
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
            terminal_contract: None,
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
            terminal_contract: None,
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
            terminal_contract: None,
        },
    );
    Ok(())
}

fn remember_pending_terminal(
    state: &mut DispatchState,
    request_id: u64,
    generation: u64,
    label: String,
    operation: &'static str,
) -> io::Result<()> {
    if state.pending.len() >= 128 && !evict_oldest_unprotected_pending(state) {
        return Err(io::Error::other("mpv acknowledgement queue saturated"));
    }
    state.pending.insert(
        request_id,
        PendingCommand {
            label,
            file_generation: Some(generation),
            acknowledgement: None,
            terminal_contract: Some(PendingTerminalContract {
                operation,
                deadline: Instant::now() + INTERNAL_COMMAND_REPLY_TIMEOUT,
            }),
        },
    );
    Ok(())
}

fn earliest_terminal_contract(state: &DispatchState) -> Option<PendingTerminalContract> {
    state
        .pending
        .values()
        .filter_map(|pending| pending.terminal_contract)
        .min_by_key(|contract| contract.deadline)
}

fn expired_terminal_failure(state: &DispatchState, now: Instant) -> Option<ActorExit> {
    earliest_terminal_contract(state)
        .filter(|contract| now >= contract.deadline)
        .map(|contract| ActorExit::InternalCommandFailed {
            operation: contract.operation,
            rejected: false,
        })
}

fn fail_pending_barriers(state: &DispatchState, error: &str) {
    for pending in state.pending.values() {
        if let Some(acknowledgement) = &pending.acknowledgement {
            acknowledgement.fail(error.to_owned());
        }
    }
}

fn evict_oldest_unprotected_pending(state: &mut DispatchState) -> bool {
    let oldest = state
        .pending
        .iter()
        .filter(|(_, pending)| {
            pending.file_generation.is_none()
                && pending.acknowledgement.is_none()
                && pending.terminal_contract.is_none()
        })
        .map(|(request_id, _)| *request_id)
        .min();
    if let Some(oldest) = oldest {
        state.pending.remove(&oldest);
        true
    } else {
        false
    }
}
