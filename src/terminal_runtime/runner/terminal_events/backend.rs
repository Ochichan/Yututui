use std::io;
use std::time::Duration;

use crossterm::event::Event;

use crate::terminal_policy::CPR_TOTAL_TIMEOUT;

use super::InputBackend;
use super::liveness::{EvidenceDomain, LivenessStage, ProbeOutcome};
use super::output_gate::{LivenessOutputError, OutputRole, TerminalOutputLock};
#[cfg(unix)]
use super::owner_probe::{OwnerProbeCheck, OwnerProbeDomain, TerminalOwnerProbe};

#[cfg(not(test))]
pub(super) fn terminal_output_operation() -> Option<crate::tui::OutputOperationSnapshot> {
    crate::tui::output_operation_snapshot()
}

// The active writer is process-global. Parallel TUI fixtures deliberately use that slot, so a
// liveness unit test must not diagnose another fixture's deliberately short operation.
#[cfg(test)]
pub(super) fn terminal_output_operation() -> Option<crate::tui::OutputOperationSnapshot> {
    None
}

pub(super) fn output_operation_stage(
    operation: crate::tui::OutputOperationSnapshot,
) -> LivenessStage {
    use crate::tui::OutputOperationPhase;

    if operation.label.contains("restore") {
        return LivenessStage::Restore;
    }
    if operation.label == "cursor-position probe" {
        return match operation.phase {
            OutputOperationPhase::Preparing => LivenessStage::CprWait,
            OutputOperationPhase::Writing | OutputOperationPhase::Flushing => {
                LivenessStage::CprWrite
            }
        };
    }
    match operation.phase {
        OutputOperationPhase::Preparing => LivenessStage::OwnerRender,
        OutputOperationPhase::Writing | OutputOperationPhase::Flushing => LivenessStage::OwnerFlush,
    }
}

pub(super) fn owner_output_operation_expired(
    operation: Option<crate::tui::OutputOperationSnapshot>,
    holder_role: Option<OutputRole>,
) -> bool {
    holder_role == Some(OutputRole::Owner) && operation.is_some_and(|value| value.expired)
}

pub(super) struct CrosstermInput {
    output_lock: TerminalOutputLock,
    heartbeat_generation: u64,
    owner_generation: u64,
    #[cfg(unix)]
    owner: TerminalOwnerProbe,
}

impl CrosstermInput {
    pub(super) fn detect(output_lock: TerminalOutputLock) -> Self {
        Self {
            output_lock,
            heartbeat_generation: 0,
            owner_generation: 0,
            #[cfg(unix)]
            owner: TerminalOwnerProbe::detect_with(|key| std::env::var_os(key)),
        }
    }
}

impl InputBackend for CrosstermInput {
    fn check_owner_attached(&mut self) -> Vec<ProbeOutcome> {
        self.owner_generation = self.owner_generation.wrapping_add(1).max(1);
        #[cfg(unix)]
        {
            match self.owner.check() {
                OwnerProbeCheck::Direct => vec![ProbeOutcome::NoEvidence],
                OwnerProbeCheck::Layers(checks) => checks
                    .into_iter()
                    .map(|check| {
                        let domain = owner_domain(check.domain);
                        match check.result {
                            Ok(()) => ProbeOutcome::Alive(domain),
                            Err(error) => ProbeOutcome::owner(domain, error, self.owner_generation),
                        }
                    })
                    .collect(),
            }
        }
        #[cfg(not(unix))]
        {
            vec![ProbeOutcome::NoEvidence]
        }
    }

    fn heartbeat(&mut self) -> ProbeOutcome {
        self.heartbeat_generation = self.heartbeat_generation.wrapping_add(1).max(1);
        match self
            .output_lock
            .run_liveness_io(|| crate::tui::probe_cursor_position(CPR_TOTAL_TIMEOUT))
        {
            Ok(crossterm::cursor::CursorPositionProbe::Position(_, _)) => {
                ProbeOutcome::Alive(EvidenceDomain::Transport)
            }
            Ok(crossterm::cursor::CursorPositionProbe::DeferredForPendingInput) => {
                ProbeOutcome::PendingInput
            }
            Ok(crossterm::cursor::CursorPositionProbe::DeferredForRecentInput) => {
                ProbeOutcome::RecentInput
            }
            Ok(_) => ProbeOutcome::InternalFatal {
                domain: Some(EvidenceDomain::Transport),
                stage: LivenessStage::CprWait,
                error: io::Error::other("unknown cursor-position probe result"),
            },
            Err(LivenessOutputError::Gate(gate)) => {
                // A legal frame/OSC operation can hold the owner permit for its whole shorter
                // deadline. Defer CPR without manufacturing two terminal-loss observations; the
                // independent gate deadline diagnoses a genuinely wedged owner precisely.
                tracing::debug!(
                    holder_role = ?gate.holder_role,
                    holder_generation = gate.holder_generation,
                    holder_held_ms = gate.held_for.as_millis(),
                    "deferred terminal liveness probe while owner output was busy"
                );
                ProbeOutcome::OwnerOutputBusy
            }
            Err(LivenessOutputError::Operation(error)) => {
                let stage = cpr_error_stage(&error);
                ProbeOutcome::transport(error, self.heartbeat_generation, stage)
            }
        }
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        crossterm::event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        crossterm::event::read()
    }
}

#[cfg(unix)]
fn owner_domain(domain: OwnerProbeDomain) -> EvidenceDomain {
    match domain {
        OwnerProbeDomain::Environment => EvidenceDomain::OwnerEnvironment,
        OwnerProbeDomain::Tmux => EvidenceDomain::OwnerTmux,
        OwnerProbeDomain::Screen => EvidenceDomain::OwnerScreen,
        OwnerProbeDomain::Zellij => EvidenceDomain::OwnerZellij,
    }
}

fn cpr_error_stage(error: &io::Error) -> LivenessStage {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("write") || message.contains("terminal output") {
        LivenessStage::CprWrite
    } else {
        LivenessStage::CprWait
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{OutputOperationPhase, OutputOperationSnapshot};

    fn operation(label: &'static str, phase: OutputOperationPhase) -> OutputOperationSnapshot {
        OutputOperationSnapshot {
            label,
            generation: 7,
            elapsed: Duration::from_secs(1),
            phase,
            expired: false,
        }
    }

    #[test]
    fn output_operation_phases_map_to_actionable_liveness_stages() {
        assert_eq!(
            output_operation_stage(operation(
                "cursor-position probe",
                OutputOperationPhase::Preparing,
            )),
            LivenessStage::CprWait
        );
        assert_eq!(
            output_operation_stage(operation(
                "cursor-position probe",
                OutputOperationPhase::Writing,
            )),
            LivenessStage::CprWrite
        );
        assert_eq!(
            output_operation_stage(operation("terminal frame", OutputOperationPhase::Preparing)),
            LivenessStage::OwnerRender
        );
        assert_eq!(
            output_operation_stage(operation("terminal frame", OutputOperationPhase::Flushing)),
            LivenessStage::OwnerFlush
        );
        assert_eq!(
            output_operation_stage(operation("terminal restore", OutputOperationPhase::Writing)),
            LivenessStage::Restore
        );
    }

    #[test]
    fn only_an_owner_operation_uses_its_shorter_deadline_as_fatal() {
        let mut expired = operation("terminal frame", OutputOperationPhase::Preparing);
        expired.expired = true;
        assert!(owner_output_operation_expired(
            Some(expired),
            Some(OutputRole::Owner)
        ));
        assert!(
            !owner_output_operation_expired(Some(expired), Some(OutputRole::Liveness)),
            "a CPR write timeout must return through the two-observation ambiguity policy"
        );
        assert!(!owner_output_operation_expired(Some(expired), None));
    }
}
