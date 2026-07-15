use std::collections::VecDeque;
use std::time::Instant;

use super::super::recovery::LoadWithResume;
use super::PlayerCmd;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResumePurpose {
    SourceRecovery,
    Other,
}

impl ResumePurpose {
    fn from_request(request: &LoadWithResume) -> Self {
        if request.is_source_recovery() {
            Self::SourceRecovery
        } else {
            Self::Other
        }
    }

    pub(super) const fn is_source_recovery(self) -> bool {
        matches!(self, Self::SourceRecovery)
    }
}

/// Transport state retained while a replacement load crosses asynchronous validation.
///
/// Keeping ownership in the variant prevents the invalid combinations that were possible with
/// an optional request plus a separate ownership flag. Once user transport is
/// merged, the physical replacement and its cache-safety metadata remain, but recovery no longer
/// owns the transport intent.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum ResumeLoad {
    #[default]
    None,
    RestoreOwned(LoadWithResume),
    UserTransportMerged(LoadWithResume),
}

impl ResumeLoad {
    pub(super) fn from_request(request: Option<LoadWithResume>) -> Self {
        request.map_or(Self::None, Self::RestoreOwned)
    }

    pub(super) const fn request(&self) -> Option<&LoadWithResume> {
        match self {
            Self::RestoreOwned(request) | Self::UserTransportMerged(request) => Some(request),
            Self::None => None,
        }
    }

    pub(super) const fn owned_request(&self) -> Option<&LoadWithResume> {
        match self {
            Self::RestoreOwned(request) => Some(request),
            Self::None | Self::UserTransportMerged(_) => None,
        }
    }

    pub(super) const fn is_some(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub(super) const fn is_restore_owned(&self) -> bool {
        matches!(self, Self::RestoreOwned(_))
    }

    pub(super) fn merge_transport(&mut self, command: &PlayerCmd) -> ResumeMerge {
        let current = std::mem::take(self);
        match current {
            Self::RestoreOwned(mut request) => {
                if merge_pending_request(&mut request, command) {
                    let purpose = ResumePurpose::from_request(&request);
                    *self = Self::UserTransportMerged(request);
                    ResumeMerge::MergedOwned(purpose)
                } else {
                    *self = Self::RestoreOwned(request);
                    ResumeMerge::NotMerged
                }
            }
            Self::UserTransportMerged(mut request) => {
                let merged = merge_pending_request(&mut request, command);
                *self = Self::UserTransportMerged(request);
                if merged {
                    ResumeMerge::Merged
                } else {
                    ResumeMerge::NotMerged
                }
            }
            Self::None => {
                *self = Self::None;
                ResumeMerge::NotMerged
            }
        }
    }

    pub(super) fn into_request(self) -> Option<LoadWithResume> {
        match self {
            Self::RestoreOwned(request) | Self::UserTransportMerged(request) => Some(request),
            Self::None => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResumeMerge {
    NotMerged,
    Merged,
    MergedOwned(ResumePurpose),
}

impl ResumeMerge {
    pub(super) const fn is_merged(self) -> bool {
        !matches!(self, Self::NotMerged)
    }
}

#[derive(Clone, Debug)]
pub(super) enum ResumeTelemetryPhase {
    Prepared,
    AwaitingSeekReply {
        request_id: u64,
        buffered_near_target: Option<f64>,
    },
    AwaitingPosition {
        deadline: Instant,
    },
}

#[derive(Clone, Debug)]
struct ResumeTelemetry {
    file_generation: u64,
    target_secs: f64,
    purpose: ResumePurpose,
    phase: ResumeTelemetryPhase,
}

impl ResumeTelemetry {
    fn new(file_generation: u64, target_secs: f64, purpose: ResumePurpose) -> Self {
        Self {
            file_generation,
            target_secs,
            purpose,
            phase: ResumeTelemetryPhase::Prepared,
        }
    }

    fn reset_target(&mut self, target_secs: f64) {
        self.target_secs = target_secs;
        self.phase = ResumeTelemetryPhase::Prepared;
    }
}

#[derive(Clone, Debug)]
struct PendingResume {
    file_generation: u64,
    request: LoadWithResume,
}

#[derive(Clone, Default)]
enum ResumeStage {
    #[default]
    Idle,
    AwaitingFileLoaded(PendingResume),
    AwaitingCorrelation(PendingResume),
    Dispatching {
        file_generation: u64,
        commands: VecDeque<PlayerCmd>,
    },
}

#[derive(Default)]
pub(super) struct ResumeCoordinator {
    stage: ResumeStage,
    telemetry: Option<ResumeTelemetry>,
}

pub(super) enum ResumeCancellation {
    NotOwned,
    Owned(ResumePurpose),
}

pub(super) enum TelemetryCompletion {
    Ignored,
    AwaitingPosition,
    Completed {
        position: f64,
        purpose: ResumePurpose,
    },
}

pub(super) enum PositionDisposition {
    Forward,
    Hold,
    Completed(ResumePurpose),
}

impl ResumeCoordinator {
    pub(super) fn install(&mut self, file_generation: u64, request: LoadWithResume) {
        self.install_telemetry(file_generation, &request);
        self.stage = ResumeStage::AwaitingFileLoaded(PendingResume {
            file_generation,
            request,
        });
    }

    pub(super) fn install_telemetry(&mut self, file_generation: u64, request: &LoadWithResume) {
        self.telemetry = (request.position_secs > 0.0).then(|| {
            ResumeTelemetry::new(
                file_generation,
                request.position_secs,
                ResumePurpose::from_request(request),
            )
        });
    }

    pub(super) fn reset_post_load(&mut self) {
        self.telemetry = None;
        if matches!(self.stage, ResumeStage::Dispatching { .. }) {
            self.stage = ResumeStage::Idle;
        }
    }

    pub(super) fn reset_post_load_preserving(&mut self, preserve_generation: Option<u64>) {
        if self
            .telemetry
            .as_ref()
            .is_some_and(|telemetry| Some(telemetry.file_generation) != preserve_generation)
        {
            self.telemetry = None;
        }
        if matches!(
            &self.stage,
            ResumeStage::Dispatching {
                file_generation,
                ..
            } if Some(*file_generation) != preserve_generation
        ) {
            self.stage = ResumeStage::Idle;
        }
    }

    pub(super) fn cancel_if_owned(&mut self) -> ResumeCancellation {
        if matches!(self.stage, ResumeStage::Idle) {
            return ResumeCancellation::NotOwned;
        }
        let source_recovery = self
            .pending_request()
            .is_some_and(LoadWithResume::is_source_recovery)
            || self
                .telemetry
                .as_ref()
                .is_some_and(|telemetry| telemetry.purpose.is_source_recovery());
        self.stage = ResumeStage::Idle;
        self.telemetry = None;
        ResumeCancellation::Owned(if source_recovery {
            ResumePurpose::SourceRecovery
        } else {
            ResumePurpose::Other
        })
    }

    pub(super) fn rebase_file_generation(&mut self, previous: u64, generation: u64) {
        match &mut self.stage {
            ResumeStage::AwaitingFileLoaded(pending)
            | ResumeStage::AwaitingCorrelation(pending)
                if pending.file_generation == previous =>
            {
                pending.file_generation = generation;
            }
            ResumeStage::Dispatching {
                file_generation, ..
            } if *file_generation == previous => *file_generation = generation,
            ResumeStage::Idle
            | ResumeStage::AwaitingFileLoaded(_)
            | ResumeStage::AwaitingCorrelation(_)
            | ResumeStage::Dispatching { .. } => {}
        }
        if let Some(telemetry) = self
            .telemetry
            .as_mut()
            .filter(|telemetry| telemetry.file_generation == previous)
        {
            telemetry.file_generation = generation;
        }
    }

    pub(super) fn mark_file_loaded(
        &mut self,
        event_generation: Option<u64>,
        active_entry_matches: bool,
        active_generation: Option<u64>,
    ) {
        let stage = std::mem::take(&mut self.stage);
        self.stage = match stage {
            ResumeStage::AwaitingFileLoaded(pending) => {
                let event_matches = event_generation == Some(pending.file_generation)
                    || (event_generation.is_none() && active_entry_matches);
                if !event_matches {
                    ResumeStage::AwaitingFileLoaded(pending)
                } else if active_generation == Some(pending.file_generation) {
                    Self::dispatching(pending)
                } else {
                    ResumeStage::AwaitingCorrelation(pending)
                }
            }
            ResumeStage::AwaitingCorrelation(pending) => {
                let event_matches = event_generation == Some(pending.file_generation)
                    || (event_generation.is_none() && active_entry_matches);
                if event_matches && active_generation == Some(pending.file_generation) {
                    Self::dispatching(pending)
                } else {
                    ResumeStage::AwaitingCorrelation(pending)
                }
            }
            stage => stage,
        };
    }

    pub(super) fn release_if_correlated(&mut self, active_generation: Option<u64>) {
        let stage = std::mem::take(&mut self.stage);
        self.stage = match stage {
            ResumeStage::AwaitingCorrelation(pending)
                if active_generation == Some(pending.file_generation) =>
            {
                Self::dispatching(pending)
            }
            stage => stage,
        };
    }

    fn dispatching(pending: PendingResume) -> ResumeStage {
        let mut commands = VecDeque::new();
        if pending.request.position_secs > 0.0 {
            commands.push_back(PlayerCmd::exact_seek(pending.request.position_secs));
        }
        commands.push_back(PlayerCmd::SetProperty {
            name: "pause".to_owned(),
            value: serde_json::Value::Bool(pending.request.paused),
        });
        ResumeStage::Dispatching {
            file_generation: pending.file_generation,
            commands,
        }
    }

    pub(super) fn cancel_pending_for_generation(&mut self, generation: Option<u64>) {
        let should_cancel = match &self.stage {
            ResumeStage::AwaitingFileLoaded(pending)
            | ResumeStage::AwaitingCorrelation(pending) => {
                generation.is_none_or(|generation| generation == pending.file_generation)
            }
            ResumeStage::Idle | ResumeStage::Dispatching { .. } => false,
        };
        if should_cancel {
            self.stage = ResumeStage::Idle;
        }
    }

    pub(super) fn is_awaiting_boundary(&self) -> bool {
        matches!(
            self.stage,
            ResumeStage::AwaitingFileLoaded(_) | ResumeStage::AwaitingCorrelation(_)
        )
    }

    pub(super) fn pending_request(&self) -> Option<&LoadWithResume> {
        match &self.stage {
            ResumeStage::AwaitingFileLoaded(pending)
            | ResumeStage::AwaitingCorrelation(pending) => Some(&pending.request),
            ResumeStage::Idle | ResumeStage::Dispatching { .. } => None,
        }
    }

    pub(super) fn pending_request_mut(&mut self) -> Option<&mut LoadWithResume> {
        match &mut self.stage {
            ResumeStage::AwaitingFileLoaded(pending)
            | ResumeStage::AwaitingCorrelation(pending) => Some(&mut pending.request),
            ResumeStage::Idle | ResumeStage::Dispatching { .. } => None,
        }
    }

    pub(super) fn merge_pending_command(&mut self, command: &PlayerCmd) -> bool {
        self.pending_request_mut()
            .is_some_and(|request| merge_pending_request(request, command))
    }

    pub(super) fn merge_dispatching_command(
        &mut self,
        command: &PlayerCmd,
        last_confirmed_time: f64,
    ) -> bool {
        let (file_generation, commands) = match &mut self.stage {
            ResumeStage::Dispatching {
                file_generation,
                commands,
            } => (*file_generation, commands),
            ResumeStage::Idle
            | ResumeStage::AwaitingFileLoaded(_)
            | ResumeStage::AwaitingCorrelation(_) => return false,
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
                commands.iter_mut().any(|queued| {
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
                let queued_target = commands.iter().find_map(|queued| match queued {
                    PlayerCmd::SeekAbsolute { seconds, .. } => Some(*seconds),
                    _ => None,
                });
                let current_target = queued_target
                    .or_else(|| {
                        self.telemetry
                            .as_ref()
                            .filter(|telemetry| telemetry.file_generation == file_generation)
                            .map(|telemetry| telemetry.target_secs)
                    })
                    .unwrap_or(last_confirmed_time);
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
                for queued in commands.iter_mut() {
                    if let PlayerCmd::SeekAbsolute { seconds, .. } = queued {
                        *seconds = target;
                        replaced_queued = true;
                        break;
                    }
                }
                if !replaced_queued {
                    let before_pause = commands
                        .iter()
                        .position(|queued| {
                            matches!(queued, PlayerCmd::SetProperty { name, .. } if name == "pause")
                        })
                        .unwrap_or(commands.len());
                    commands.insert(before_pause, PlayerCmd::exact_seek(target));
                }
                if let Some(telemetry) = self
                    .telemetry
                    .as_mut()
                    .filter(|telemetry| telemetry.file_generation == file_generation)
                {
                    telemetry.reset_target(target);
                } else {
                    self.telemetry = Some(ResumeTelemetry::new(
                        file_generation,
                        target,
                        ResumePurpose::Other,
                    ));
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn front_command(&self) -> Option<&PlayerCmd> {
        match &self.stage {
            ResumeStage::Dispatching { commands, .. } => commands.front(),
            ResumeStage::Idle
            | ResumeStage::AwaitingFileLoaded(_)
            | ResumeStage::AwaitingCorrelation(_) => None,
        }
    }

    #[cfg(test)]
    pub(super) fn back_command(&self) -> Option<&PlayerCmd> {
        match &self.stage {
            ResumeStage::Dispatching { commands, .. } => commands.back(),
            ResumeStage::Idle
            | ResumeStage::AwaitingFileLoaded(_)
            | ResumeStage::AwaitingCorrelation(_) => None,
        }
    }

    pub(super) fn pop_command(&mut self) -> Option<PlayerCmd> {
        match &mut self.stage {
            ResumeStage::Dispatching { commands, .. } => commands.pop_front(),
            ResumeStage::Idle
            | ResumeStage::AwaitingFileLoaded(_)
            | ResumeStage::AwaitingCorrelation(_) => None,
        }
    }

    #[cfg(test)]
    pub(super) fn commands_is_empty(&self) -> bool {
        self.front_command().is_none()
    }

    #[cfg(test)]
    pub(super) fn command_count(&self) -> usize {
        match &self.stage {
            ResumeStage::Dispatching { commands, .. } => commands.len(),
            ResumeStage::Idle
            | ResumeStage::AwaitingFileLoaded(_)
            | ResumeStage::AwaitingCorrelation(_) => 0,
        }
    }

    pub(super) fn dispatch_generation(&self) -> Option<u64> {
        match self.stage {
            ResumeStage::Dispatching {
                file_generation, ..
            } => Some(file_generation),
            ResumeStage::Idle
            | ResumeStage::AwaitingFileLoaded(_)
            | ResumeStage::AwaitingCorrelation(_) => None,
        }
    }

    pub(super) fn finish_dispatch(&mut self) {
        if matches!(self.stage, ResumeStage::Dispatching { .. }) {
            self.stage = ResumeStage::Idle;
        }
    }

    pub(super) fn clear_dispatch_for_generation(&mut self, file_generation: u64) {
        if self.dispatch_generation() == Some(file_generation) {
            self.stage = ResumeStage::Idle;
        }
    }

    pub(super) fn begin_seek_observation(&mut self, file_generation: u64, request_id: u64) {
        if let Some(telemetry) = self
            .telemetry
            .as_mut()
            .filter(|telemetry| telemetry.file_generation == file_generation)
        {
            telemetry.phase = ResumeTelemetryPhase::AwaitingSeekReply {
                request_id,
                buffered_near_target: None,
            };
        }
    }

    pub(super) fn complete_seek_observation(
        &mut self,
        file_generation: u64,
        request_id: u64,
        deadline: Instant,
    ) -> TelemetryCompletion {
        let Some(telemetry) = self
            .telemetry
            .as_mut()
            .filter(|telemetry| telemetry.file_generation == file_generation)
        else {
            return TelemetryCompletion::Ignored;
        };
        let ResumeTelemetryPhase::AwaitingSeekReply {
            request_id: expected_request_id,
            buffered_near_target,
        } = &mut telemetry.phase
        else {
            return TelemetryCompletion::Ignored;
        };
        if *expected_request_id != request_id {
            return TelemetryCompletion::Ignored;
        }
        if let Some(position) = buffered_near_target.take() {
            let purpose = telemetry.purpose;
            self.telemetry = None;
            TelemetryCompletion::Completed { position, purpose }
        } else {
            telemetry.phase = ResumeTelemetryPhase::AwaitingPosition { deadline };
            TelemetryCompletion::AwaitingPosition
        }
    }

    pub(super) fn observe_position(
        &mut self,
        active_file_generation: Option<u64>,
        position: f64,
        floor_tolerance_secs: f64,
    ) -> PositionDisposition {
        let Some(telemetry) = self.telemetry.as_mut() else {
            return PositionDisposition::Forward;
        };
        if active_file_generation.is_some()
            && active_file_generation != Some(telemetry.file_generation)
        {
            return PositionDisposition::Forward;
        }
        let near_target = (telemetry.target_secs == 0.0 || position > 0.0)
            && (position - telemetry.target_secs).abs() <= floor_tolerance_secs;
        match &mut telemetry.phase {
            ResumeTelemetryPhase::Prepared => PositionDisposition::Hold,
            ResumeTelemetryPhase::AwaitingSeekReply {
                buffered_near_target,
                ..
            } => {
                if near_target {
                    *buffered_near_target = Some(position);
                }
                PositionDisposition::Hold
            }
            ResumeTelemetryPhase::AwaitingPosition { .. } if !near_target => {
                PositionDisposition::Hold
            }
            ResumeTelemetryPhase::AwaitingPosition { .. } => {
                let purpose = telemetry.purpose;
                self.telemetry = None;
                PositionDisposition::Completed(purpose)
            }
        }
    }

    pub(super) fn terminal_deadline(&self) -> Option<Instant> {
        match self.telemetry.as_ref().map(|telemetry| &telemetry.phase) {
            Some(ResumeTelemetryPhase::AwaitingPosition { deadline }) => Some(*deadline),
            Some(
                ResumeTelemetryPhase::Prepared | ResumeTelemetryPhase::AwaitingSeekReply { .. },
            )
            | None => None,
        }
    }

    pub(super) fn pause_waits_for_position(&self) -> bool {
        matches!(
            (&self.stage, &self.telemetry),
            (
                ResumeStage::Dispatching {
                    file_generation: dispatch_generation,
                    ..
                },
                Some(ResumeTelemetry {
                    file_generation: telemetry_generation,
                    phase: ResumeTelemetryPhase::AwaitingPosition { .. },
                    ..
                })
            ) if dispatch_generation == telemetry_generation
        )
    }

    pub(super) fn take_telemetry_for_generation(
        &mut self,
        file_generation: u64,
    ) -> Option<ResumePurpose> {
        self.telemetry
            .take()
            .filter(|telemetry| telemetry.file_generation == file_generation)
            .map(|telemetry| telemetry.purpose)
    }

    pub(super) fn source_recovery_matches(&self, file_generation: u64) -> bool {
        self.telemetry.as_ref().is_some_and(|telemetry| {
            telemetry.file_generation == file_generation && telemetry.purpose.is_source_recovery()
        })
    }

    pub(super) fn telemetry_is_source_recovery(&self) -> bool {
        self.telemetry
            .as_ref()
            .is_some_and(|telemetry| telemetry.purpose.is_source_recovery())
    }

    pub(super) fn telemetry_target_for_generation(&self, file_generation: u64) -> Option<f64> {
        self.telemetry
            .as_ref()
            .filter(|telemetry| telemetry.file_generation == file_generation)
            .map(|telemetry| telemetry.target_secs)
    }

    #[cfg(test)]
    pub(super) fn telemetry_is_some(&self) -> bool {
        self.telemetry.is_some()
    }

    #[cfg(test)]
    pub(super) fn telemetry_is_prepared_for(&self, file_generation: u64, target_secs: f64) -> bool {
        self.telemetry.as_ref().is_some_and(|telemetry| {
            telemetry.file_generation == file_generation
                && (telemetry.target_secs - target_secs).abs() < f64::EPSILON
                && matches!(telemetry.phase, ResumeTelemetryPhase::Prepared)
        })
    }

    #[cfg(test)]
    pub(super) fn install_dispatching_for_test(
        &mut self,
        file_generation: u64,
        commands: VecDeque<PlayerCmd>,
    ) {
        self.stage = ResumeStage::Dispatching {
            file_generation,
            commands,
        };
    }
}

pub(super) fn merge_pending_request(request: &mut LoadWithResume, command: &PlayerCmd) -> bool {
    match command {
        PlayerCmd::CyclePause => {
            request.paused = !request.paused;
            true
        }
        PlayerCmd::SetProperty { name, value } if name == "pause" => {
            value.as_bool().is_some_and(|paused| {
                request.paused = paused;
                true
            })
        }
        PlayerCmd::SeekAbsolute { seconds, .. } => {
            request.position_secs = crate::playback_policy::norm_position(*seconds);
            true
        }
        PlayerCmd::SeekRelative(delta) => {
            request.position_secs =
                crate::playback_policy::norm_position(request.position_secs + delta);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LoadWithResume {
        LoadWithResume::emergency(
            "https://example.invalid/recovery".to_owned(),
            42.0,
            true,
            crate::player::MediaSourceContext::OnDemand,
        )
    }

    fn source_recovery_request(position_secs: f64) -> LoadWithResume {
        let mut planner = crate::player::recovery::RecoveryPlanner::default();
        let ticket = planner
            .begin_ticket("HTTP error 403 Forbidden", 11, 7)
            .expect("fixture is recoverable");
        LoadWithResume::source_recovery(
            "https://example.invalid/recovery".to_owned(),
            position_secs,
            true,
            crate::player::MediaSourceContext::OnDemand,
            ticket,
        )
    }

    #[test]
    fn file_loaded_before_correlation_dispatches_once_after_correlation() {
        let mut coordinator = ResumeCoordinator::default();
        coordinator.install(8, request());

        coordinator.mark_file_loaded(Some(8), true, None);

        assert!(matches!(
            &coordinator.stage,
            ResumeStage::AwaitingCorrelation(_)
        ));
        assert_eq!(coordinator.dispatch_generation(), None);

        coordinator.release_if_correlated(Some(8));
        coordinator.release_if_correlated(Some(8));

        assert_eq!(coordinator.dispatch_generation(), Some(8));
        assert_eq!(coordinator.command_count(), 2);
    }

    #[test]
    fn legacy_file_loaded_without_generation_waits_for_correlation_and_dispatches_once() {
        let mut coordinator = ResumeCoordinator::default();
        coordinator.install(8, request());

        coordinator.mark_file_loaded(None, true, None);
        coordinator.mark_file_loaded(None, true, None);

        assert!(matches!(
            &coordinator.stage,
            ResumeStage::AwaitingCorrelation(_)
        ));
        assert_eq!(coordinator.command_count(), 0);

        coordinator.release_if_correlated(Some(8));
        coordinator.release_if_correlated(Some(8));
        coordinator.mark_file_loaded(None, true, Some(8));

        assert_eq!(coordinator.dispatch_generation(), Some(8));
        assert_eq!(coordinator.command_count(), 2);
    }

    #[test]
    fn correlation_before_file_loaded_dispatches_once_after_file_loaded() {
        let mut coordinator = ResumeCoordinator::default();
        coordinator.install(8, request());

        coordinator.release_if_correlated(Some(8));

        assert!(matches!(
            &coordinator.stage,
            ResumeStage::AwaitingFileLoaded(_)
        ));
        assert_eq!(coordinator.dispatch_generation(), None);

        coordinator.mark_file_loaded(Some(8), true, Some(8));
        coordinator.mark_file_loaded(Some(8), true, Some(8));

        assert_eq!(coordinator.dispatch_generation(), Some(8));
        assert_eq!(coordinator.command_count(), 2);
    }

    #[test]
    fn stale_file_loaded_and_correlation_generations_cannot_advance_resume() {
        let mut coordinator = ResumeCoordinator::default();
        coordinator.install(8, request());

        coordinator.mark_file_loaded(Some(7), false, Some(7));
        coordinator.release_if_correlated(Some(7));

        assert!(matches!(
            &coordinator.stage,
            ResumeStage::AwaitingFileLoaded(_)
        ));

        coordinator.mark_file_loaded(Some(8), true, None);
        coordinator.release_if_correlated(Some(7));

        assert!(matches!(
            &coordinator.stage,
            ResumeStage::AwaitingCorrelation(_)
        ));
        assert_eq!(coordinator.dispatch_generation(), None);

        coordinator.release_if_correlated(Some(8));
        assert_eq!(coordinator.dispatch_generation(), Some(8));
        assert_eq!(coordinator.command_count(), 2);
    }

    #[test]
    fn staged_resume_ownership_transitions_once_without_dropping_request() {
        let mut load = ResumeLoad::from_request(Some(request()));

        assert!(load.is_restore_owned());
        assert_eq!(
            load.merge_transport(&PlayerCmd::SetVolume(25)),
            ResumeMerge::NotMerged
        );
        assert_eq!(
            load.merge_transport(&PlayerCmd::CyclePause),
            ResumeMerge::MergedOwned(ResumePurpose::Other)
        );
        assert!(!load.is_restore_owned());
        assert_eq!(
            load.merge_transport(&PlayerCmd::CyclePause),
            ResumeMerge::Merged
        );
        assert_eq!(load.into_request(), Some(request()));
    }

    #[test]
    fn staged_source_recovery_reports_ownership_only_on_the_first_transport_merge() {
        let mut load = ResumeLoad::from_request(Some(source_recovery_request(42.0)));

        assert_eq!(
            load.merge_transport(&PlayerCmd::interactive_seek(20.0)),
            ResumeMerge::MergedOwned(ResumePurpose::SourceRecovery)
        );
        assert_eq!(
            load.merge_transport(&PlayerCmd::CyclePause),
            ResumeMerge::Merged
        );
        assert!(load.owned_request().is_none());
        assert!(
            load.request()
                .is_some_and(|request| { request.position_secs == 20.0 && !request.paused })
        );
    }

    #[test]
    fn cancellation_reports_only_owned_source_recovery_and_preserves_in_flight_telemetry() {
        let mut owned = ResumeCoordinator::default();
        owned.install(8, source_recovery_request(0.0));
        assert!(!owned.telemetry_is_some());
        assert!(matches!(
            owned.cancel_if_owned(),
            ResumeCancellation::Owned(ResumePurpose::SourceRecovery)
        ));
        assert!(!owned.telemetry_is_some());
        assert!(!owned.is_awaiting_boundary());
        assert_eq!(owned.dispatch_generation(), None);

        let mut other = ResumeCoordinator::default();
        other.install(8, request());
        assert!(matches!(
            other.cancel_if_owned(),
            ResumeCancellation::Owned(ResumePurpose::Other)
        ));

        let mut in_flight = ResumeCoordinator::default();
        in_flight.install(8, source_recovery_request(42.0));
        in_flight.mark_file_loaded(Some(8), true, Some(8));
        in_flight.finish_dispatch();
        assert!(in_flight.source_recovery_matches(8));
        assert!(matches!(
            in_flight.cancel_if_owned(),
            ResumeCancellation::NotOwned
        ));
        assert!(in_flight.source_recovery_matches(8));
    }
}
