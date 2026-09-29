use super::*;

#[cfg(test)]
pub(super) fn stage_actor_command(
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

pub(super) fn accept_actor_command(
    state: &mut DispatchState,
    cmd: PlayerCmd,
    validating_load: &mut Option<PendingLoadValidation>,
    backlog: &mut VecDeque<PlayerCmd>,
    seek_flight: &mut Option<SeekFlight>,
) {
    if cmd.invalidates_file_generation() {
        revoke_all_playback_routes(state);
    }
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
    if let Some(lease) = state.playback_route_leases.remove(&previous) {
        state.playback_route_leases.insert(generation, lease);
    }
    state.route_revocations.rebase(previous, generation);
    if let Some(cache) = state.cache.as_mut() {
        cache.rebase_file_generation(previous, generation);
    }
    for action in &mut state.cache_actions {
        action.rebase_file_generation(previous, generation);
    }
    publish_cache_status(state);
}
