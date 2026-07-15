struct PendingLoadValidation {
    request_id: u64,
    file_generation: u64,
    task: tokio::task::JoinHandle<LoadValidationOutcome>,
    resume: resume::ResumeLoad,
    source_context: MediaSourceContext,
}

struct ValidatedLoad {
    request_id: u64,
    file_generation: u64,
    url: String,
    resume: resume::ResumeLoad,
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
        if let Self::Validated(load) = self {
            record_resume_outcome(
                load.resume.owned_request(),
                super::diagnostics::SourceRecoveryOutcome::Superseded,
            );
        }
    }
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
            validation.resume.owned_request(),
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
        let can_alias = validation.resume.is_restore_owned()
            && validation
                .resume
                .request()
                .is_some_and(super::recovery::LoadWithResume::is_source_recovery)
            && state.active_file_generation == Some(state.issued_file_generation)
            && state.file_loaded_generation == Some(state.issued_file_generation)
            && state.playback_ready_generation == Some(state.issued_file_generation);
        if can_alias {
            let validation = validating_load
                .take()
                .expect("aliasable recovery validation remains installed");
            rebase_cancelled_recovery(state, validation.file_generation, validation.source_context);
            record_resume_outcome(
                validation.resume.owned_request(),
                super::diagnostics::SourceRecoveryOutcome::Superseded,
            );
            validation.task.abort();
        } else {
            let merged = validation.resume.merge_transport(&cmd);
            if let resume::ResumeMerge::MergedOwned(purpose) = merged {
                record_resume_purpose(
                    purpose,
                    super::diagnostics::SourceRecoveryOutcome::Superseded,
                );
            }
            consumed_by_pending_resume = merged.is_merged();
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
    state.resume.rebase_file_generation(previous, generation);
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

fn merge_issued_pending_resume_command(state: &mut DispatchState, command: &PlayerCmd) -> bool {
    state.resume.merge_pending_command(command)
}

fn merge_post_load_resume_command(state: &mut DispatchState, command: &PlayerCmd) -> bool {
    let last_confirmed_time = state.last_confirmed_time;
    state
        .resume
        .merge_dispatching_command(command, last_confirmed_time)
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
        match load.resume.merge_transport(cmd) {
            resume::ResumeMerge::NotMerged => return false,
            resume::ResumeMerge::MergedOwned(purpose) => record_resume_purpose(
                purpose,
                super::diagnostics::SourceRecoveryOutcome::Superseded,
            ),
            resume::ResumeMerge::Merged => {}
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
            audio_output: None,
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
            audio_output: None,
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
            audio_output: None,
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
            audio_output: None,
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

fn evict_oldest_unprotected_pending(state: &mut DispatchState) -> bool {
    let oldest = state
        .pending
        .iter()
        .filter(|(_, pending)| {
            pending.file_generation.is_none()
                && pending.acknowledgement.is_none()
                && pending.audio_output.is_none()
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
