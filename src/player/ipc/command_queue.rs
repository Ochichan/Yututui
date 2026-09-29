use super::{diagnostics, recovery};

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
    route_lease: Option<crate::playback_target::PlaybackRouteLease>,
    resume: resume::ResumeLoad,
    source_context: MediaSourceContext,
    wait_for_cache_reset: bool,
}

enum PendingLoadBoundary {
    Validated(Box<ValidatedLoad>),
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
    Validated {
        url: String,
        route_lease: Option<crate::playback_target::PlaybackRouteLease>,
    },
    Rejected(String),
    Superseded,
}

#[path = "command_queue/staging.rs"]
mod staging;

use staging::accept_actor_command;
#[cfg(test)]
use staging::stage_actor_command;

fn revoke_playback_route(state: &mut DispatchState, file_generation: u64) {
    state.route_revocations.revoke(file_generation);
    state.playback_route_leases.remove(&file_generation);
}

fn revoke_all_playback_routes(state: &mut DispatchState) {
    state.route_revocations.revoke_all();
    state.playback_route_leases.clear();
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
