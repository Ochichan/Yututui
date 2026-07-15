//! One-shot, position-preserving source-recovery contracts shared by App and daemon owners.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RecoveryEpisodeId(u64);

impl RecoveryEpisodeId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TransportIntentEpoch(u64);

impl TransportIntentEpoch {
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Correlation captured when one source-recovery episode begins.
///
/// Keeping the episode and transport epoch in one value prevents callers from accidentally
/// pairing identities from different attempts.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RecoveryTicket {
    episode_id: RecoveryEpisodeId,
    transport_epoch: TransportIntentEpoch,
}

impl RecoveryTicket {
    pub(crate) const fn episode_id(self) -> RecoveryEpisodeId {
        self.episode_id
    }

    pub(crate) const fn transport_epoch(self) -> TransportIntentEpoch {
        self.transport_epoch
    }
}

/// Why one physical media replacement must restore transport state after `file-loaded`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ResumeOrigin {
    /// Same logical item with a freshly resolved source URL.
    SourceRecovery(RecoveryTicket),
    /// Cache-safety recycle which must force this replacement media to RAM only.
    CacheSafetyRamOnly,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadWithResume {
    /// Freshly resolved direct URL or the provider's safe watch URL. Never log this value.
    pub url: String,
    pub position_secs: f64,
    pub paused: bool,
    /// Owner-known semantics for this exact replacement; never inherited from actor state.
    pub source_context: super::MediaSourceContext,
    pub episode_id: RecoveryEpisodeId,
    pub transport_epoch: TransportIntentEpoch,
    origin: ResumeOrigin,
}

impl LoadWithResume {
    pub(crate) fn source_recovery(
        url: String,
        position_secs: f64,
        paused: bool,
        source_context: super::MediaSourceContext,
        ticket: RecoveryTicket,
    ) -> Self {
        Self {
            url,
            position_secs,
            paused,
            source_context,
            episode_id: ticket.episode_id(),
            transport_epoch: ticket.transport_epoch(),
            origin: ResumeOrigin::SourceRecovery(ticket),
        }
    }

    pub(crate) fn emergency(
        url: String,
        position_secs: f64,
        paused: bool,
        source_context: super::MediaSourceContext,
    ) -> Self {
        Self {
            url,
            position_secs,
            paused,
            source_context,
            // Retained public compatibility values. Semantics now come only from `origin`.
            episode_id: RecoveryEpisodeId(0),
            transport_epoch: TransportIntentEpoch(0),
            origin: ResumeOrigin::CacheSafetyRamOnly,
        }
    }

    #[cfg(test)]
    pub(crate) const fn recovery_ticket(&self) -> Option<RecoveryTicket> {
        match self.origin {
            ResumeOrigin::SourceRecovery(ticket) => Some(ticket),
            ResumeOrigin::CacheSafetyRamOnly => None,
        }
    }

    pub(crate) const fn is_source_recovery(&self) -> bool {
        matches!(self.origin, ResumeOrigin::SourceRecovery(_))
    }

    pub(crate) const fn forces_ram_only(&self) -> bool {
        matches!(self.origin, ResumeOrigin::CacheSafetyRamOnly)
    }
}

/// Ordered physical replay after an owner retires an unsafe or disconnected player.
///
/// The plan is shared by the standalone App and daemon so pause is always restored after the
/// replacement load, while a cache-safety resume remains one correlated command.
#[derive(Clone, PartialEq)]
pub(crate) enum TransportRestorePlan {
    Idle,
    Reload {
        load: super::PlaybackLoad,
        paused: bool,
    },
    ResumeRamOnly(LoadWithResume),
}

impl TransportRestorePlan {
    #[cfg(test)]
    pub(crate) const fn idle() -> Self {
        Self::Idle
    }

    pub(crate) fn reload_if_loaded(
        loaded_video_id: Option<&str>,
        current_video_id: &str,
        url: String,
        paused: bool,
        source_context: super::MediaSourceContext,
    ) -> Self {
        if loaded_video_id != Some(current_video_id) {
            return Self::Idle;
        }
        Self::Reload {
            load: super::PlaybackLoad::new(url, source_context),
            paused,
        }
    }

    pub(crate) fn resume_ram_only_if_loaded(
        loaded_video_id: Option<&str>,
        current_video_id: &str,
        url: String,
        position_secs: f64,
        paused: bool,
        source_context: super::MediaSourceContext,
    ) -> Self {
        if loaded_video_id != Some(current_video_id) {
            return Self::Idle;
        }
        Self::ResumeRamOnly(LoadWithResume::emergency(
            url,
            position_secs,
            paused,
            source_context,
        ))
    }

    /// Materialize the only legal replay ordering. Owner-specific setup may supply its current
    /// audio filter, which stays between the load and a separate pause restoration.
    pub(crate) fn into_commands(self, audio_filter: Option<String>) -> Vec<super::PlayerCmd> {
        let (mut commands, restore_pause) = match self {
            Self::Idle => return Vec::new(),
            Self::Reload { load, paused } => (vec![super::PlayerCmd::Load(load)], paused),
            Self::ResumeRamOnly(request) => {
                (vec![super::PlayerCmd::LoadWithResume(request)], false)
            }
        };
        if let Some(audio_filter) = audio_filter {
            commands.push(super::PlayerCmd::SetAudioFilter(audio_filter));
        }
        if restore_pause {
            commands.push(super::PlayerCmd::CyclePause);
        }
        commands
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoverableSourceFailure {
    HttpForbidden,
    HttpGone,
    ExpiredSource,
    PrematureEof,
    ConnectionReset,
    GenericLoadingFailure,
}

impl RecoverableSourceFailure {
    pub const fn id(self) -> &'static str {
        match self {
            Self::HttpForbidden => "http_forbidden",
            Self::HttpGone => "http_gone",
            Self::ExpiredSource => "expired_source",
            Self::PrematureEof => "premature_eof",
            Self::ConnectionReset => "connection_reset",
            Self::GenericLoadingFailure => "generic_loading_failure",
        }
    }
}

/// URL-free error vocabulary used when mpv reports only `loading failed` (including legacy 0.32).
pub const GENERIC_LOADING_FAILURE: &str = "mpv loading failed";

/// Conservative v1 classifier. Authentication/provider denials outside these exact families keep
/// the existing breaker/skip behavior rather than entering a retry loop.
pub fn classify_source_failure(error: &str) -> Option<RecoverableSourceFailure> {
    let normalized = error.to_ascii_lowercase();
    if normalized.trim() == GENERIC_LOADING_FAILURE {
        return Some(RecoverableSourceFailure::GenericLoadingFailure);
    }
    if has_http_status(&normalized, 403) {
        return Some(RecoverableSourceFailure::HttpForbidden);
    }
    if has_http_status(&normalized, 410) {
        return Some(RecoverableSourceFailure::HttpGone);
    }
    if [
        "url has expired",
        "signed url expired",
        "signature expired",
        "expired stream url",
        "source url expired",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return Some(RecoverableSourceFailure::ExpiredSource);
    }
    if [
        "premature eof",
        "unexpected eof",
        "end of file before",
        "connection closed before end",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return Some(RecoverableSourceFailure::PrematureEof);
    }
    if [
        "connection reset",
        "connection was reset",
        "econnreset",
        "network connection was lost",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
    {
        return Some(RecoverableSourceFailure::ConnectionReset);
    }
    None
}

fn has_http_status(error: &str, status: u16) -> bool {
    let status = status.to_string();
    error
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| part == status)
        && (error.contains("http")
            || error.contains("server returned")
            || error.contains("status code"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Episode {
    id: RecoveryEpisodeId,
    logical_item_generation: u64,
    origin_file_generation: u64,
    transport_epoch: TransportIntentEpoch,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RecoveryEpisodeState {
    #[default]
    Eligible,
    Active(Episode),
    Consumed {
        logical_item_generation: u64,
    },
}

impl RecoveryEpisodeState {
    const fn attempted_generation(self) -> Option<u64> {
        match self {
            Self::Eligible => None,
            Self::Active(episode) => Some(episode.logical_item_generation),
            Self::Consumed {
                logical_item_generation,
            } => Some(logical_item_generation),
        }
    }
}

/// Owner-side episode and transport-intent arbiter.
#[derive(Default)]
pub struct RecoveryPlanner {
    next_episode: u64,
    transport_epoch: u64,
    episode: RecoveryEpisodeState,
}

impl RecoveryPlanner {
    pub fn transport_epoch(&self) -> TransportIntentEpoch {
        TransportIntentEpoch(self.transport_epoch)
    }

    /// Call after admitting a newer user seek, play/pause, Load, Stop, or track change.
    pub fn supersede_transport(&mut self) -> TransportIntentEpoch {
        self.transport_epoch = self.transport_epoch.saturating_add(1);
        if let RecoveryEpisodeState::Active(active) = self.episode {
            self.episode = RecoveryEpisodeState::Consumed {
                logical_item_generation: active.logical_item_generation,
            };
        }
        TransportIntentEpoch(self.transport_epoch)
    }

    /// A logical queue-item change resets the one-attempt latch and invalidates pending work.
    pub fn begin_logical_item(&mut self, logical_item_generation: u64) {
        if self.episode.attempted_generation() != Some(logical_item_generation) {
            self.episode = RecoveryEpisodeState::Eligible;
        }
        self.supersede_transport();
    }

    pub fn begin_episode(
        &mut self,
        error: &str,
        logical_item_generation: u64,
        origin_file_generation: u64,
    ) -> Option<(RecoveryEpisodeId, TransportIntentEpoch)> {
        self.begin_ticket(error, logical_item_generation, origin_file_generation)
            .map(|ticket| (ticket.episode_id, ticket.transport_epoch))
    }

    pub(crate) fn begin_ticket(
        &mut self,
        error: &str,
        logical_item_generation: u64,
        origin_file_generation: u64,
    ) -> Option<RecoveryTicket> {
        classify_source_failure(error)?;
        match self.episode {
            RecoveryEpisodeState::Active(_) => return None,
            RecoveryEpisodeState::Consumed {
                logical_item_generation: attempted,
            } if attempted == logical_item_generation => return None,
            RecoveryEpisodeState::Eligible | RecoveryEpisodeState::Consumed { .. } => {}
        }
        self.next_episode = self.next_episode.checked_add(1)?;
        let episode_id = RecoveryEpisodeId(self.next_episode);
        let episode = Episode {
            id: episode_id,
            logical_item_generation,
            origin_file_generation,
            transport_epoch: TransportIntentEpoch(self.transport_epoch),
        };
        self.episode = RecoveryEpisodeState::Active(episode);
        Some(RecoveryTicket {
            episode_id,
            transport_epoch: episode.transport_epoch,
        })
    }

    /// Async resolution may complete only while every captured identity remains current.
    pub fn accepts_resolved_source(
        &self,
        episode_id: RecoveryEpisodeId,
        logical_item_generation: u64,
        origin_file_generation: u64,
        transport_epoch: TransportIntentEpoch,
    ) -> bool {
        self.accepts_ticket(
            RecoveryTicket {
                episode_id,
                transport_epoch,
            },
            logical_item_generation,
            origin_file_generation,
        )
    }

    pub(crate) fn accepts_ticket(
        &self,
        ticket: RecoveryTicket,
        logical_item_generation: u64,
        origin_file_generation: u64,
    ) -> bool {
        matches!(self.episode, RecoveryEpisodeState::Active(episode) if {
            episode.id == ticket.episode_id
                && episode.logical_item_generation == logical_item_generation
                && episode.origin_file_generation == origin_file_generation
                && episode.transport_epoch == ticket.transport_epoch
                && ticket.transport_epoch == TransportIntentEpoch(self.transport_epoch)
        })
    }

    pub fn finish_episode(&mut self, episode_id: RecoveryEpisodeId) -> bool {
        let Some(ticket) = self.active_ticket() else {
            return false;
        };
        if ticket.episode_id != episode_id {
            return false;
        }
        self.finish_ticket(ticket)
    }

    pub(crate) fn finish_ticket(&mut self, ticket: RecoveryTicket) -> bool {
        let RecoveryEpisodeState::Active(active) = self.episode else {
            return false;
        };
        if active.id != ticket.episode_id || active.transport_epoch != ticket.transport_epoch {
            return false;
        }
        self.episode = RecoveryEpisodeState::Consumed {
            logical_item_generation: active.logical_item_generation,
        };
        true
    }

    /// Roll back an episode whose semantic command never entered the player lane. No retry was
    /// attempted, so the same logical item remains eligible for a later admission attempt.
    pub fn cancel_unadmitted_episode(&mut self, episode_id: RecoveryEpisodeId) -> bool {
        let Some(ticket) = self.active_ticket() else {
            return false;
        };
        if ticket.episode_id != episode_id {
            return false;
        }
        self.cancel_unadmitted_ticket(ticket)
    }

    pub(crate) fn cancel_unadmitted_ticket(&mut self, ticket: RecoveryTicket) -> bool {
        let RecoveryEpisodeState::Active(active) = self.episode else {
            return false;
        };
        if active.id != ticket.episode_id || active.transport_epoch != ticket.transport_epoch {
            return false;
        }
        self.episode = RecoveryEpisodeState::Eligible;
        true
    }

    pub fn active_episode(&self) -> Option<RecoveryEpisodeId> {
        self.active_ticket().map(|ticket| ticket.episode_id)
    }

    pub(crate) fn active_ticket(&self) -> Option<RecoveryTicket> {
        match self.episode {
            RecoveryEpisodeState::Active(episode) => Some(RecoveryTicket {
                episode_id: episode.id,
                transport_epoch: episode.transport_epoch,
            }),
            RecoveryEpisodeState::Eligible | RecoveryEpisodeState::Consumed { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifier_is_conservative() {
        for (error, expected) in [
            (
                "HTTP error 403 Forbidden",
                RecoverableSourceFailure::HttpForbidden,
            ),
            (
                "server returned 410 Gone",
                RecoverableSourceFailure::HttpGone,
            ),
            (
                "signed URL expired while reading",
                RecoverableSourceFailure::ExpiredSource,
            ),
            (
                "premature EOF in stream",
                RecoverableSourceFailure::PrematureEof,
            ),
            (
                "read failed: ECONNRESET",
                RecoverableSourceFailure::ConnectionReset,
            ),
            (
                GENERIC_LOADING_FAILURE,
                RecoverableSourceFailure::GenericLoadingFailure,
            ),
        ] {
            assert_eq!(classify_source_failure(error), Some(expected), "{error}");
        }
        for error in [
            "403 songs in the playlist",
            "authentication required",
            "permission denied",
            "unsupported codec",
            "timed out",
        ] {
            assert_eq!(classify_source_failure(error), None, "{error}");
        }
    }

    #[test]
    fn one_episode_survives_retry_generation_but_not_a_second_failure() {
        let mut planner = RecoveryPlanner::default();
        planner.begin_logical_item(4);
        let ticket = planner
            .begin_ticket("HTTP 403", 4, 11)
            .expect("first failure should recover");
        assert!(planner.accepts_ticket(ticket, 4, 11));
        assert!(planner.finish_ticket(ticket));
        assert!(planner.begin_episode("HTTP 410", 4, 12).is_none());

        planner.begin_logical_item(5);
        assert!(planner.begin_episode("HTTP 410", 5, 13).is_some());
    }

    #[test]
    fn newer_transport_intent_invalidates_async_resolution() {
        let mut planner = RecoveryPlanner::default();
        let ticket = planner
            .begin_ticket("connection reset", 9, 20)
            .expect("episode");
        planner.supersede_transport();
        assert!(!planner.accepts_ticket(ticket, 9, 20));
        assert_eq!(planner.active_episode(), None);
        assert!(
            planner.begin_episode("connection reset", 9, 21).is_none(),
            "a newer transport intent invalidates but does not rearm the same logical item"
        );
    }

    #[test]
    fn stale_item_or_file_identity_is_rejected() {
        let mut planner = RecoveryPlanner::default();
        let ticket = planner
            .begin_ticket("premature EOF", 7, 30)
            .expect("episode");
        assert!(!planner.accepts_ticket(ticket, 8, 30));
        assert!(!planner.accepts_ticket(ticket, 7, 31));
    }

    #[test]
    fn rejected_admission_does_not_consume_the_item_attempt() {
        let mut planner = RecoveryPlanner::default();
        planner.begin_logical_item(7);
        let ticket = planner
            .begin_ticket("HTTP 403", 7, 30)
            .expect("first admission plan");
        assert!(planner.cancel_unadmitted_ticket(ticket));
        assert!(planner.begin_episode("HTTP 403", 7, 30).is_some());
    }

    #[test]
    fn stale_ticket_mutators_cannot_finish_or_cancel_the_new_active_episode() {
        let mut planner = RecoveryPlanner::default();
        let stale = planner
            .begin_ticket("HTTP 403", 7, 30)
            .expect("first ticket");
        assert!(planner.cancel_unadmitted_ticket(stale));
        let current = planner
            .begin_ticket("HTTP 403", 7, 30)
            .expect("replacement ticket after rejected admission");
        assert_ne!(stale, current);

        assert!(!planner.finish_ticket(stale));
        assert!(!planner.cancel_unadmitted_ticket(stale));
        assert_eq!(planner.active_ticket(), Some(current));
        assert!(planner.accepts_ticket(current, 7, 30));
    }

    #[test]
    fn public_episode_api_preserves_finish_and_unadmitted_cancel_semantics() {
        let mut planner = RecoveryPlanner::default();
        let (episode, epoch) = planner
            .begin_episode("HTTP 403", 4, 10)
            .expect("public begin wrapper");
        assert!(planner.accepts_resolved_source(episode, 4, 10, epoch));
        assert_eq!(planner.active_episode(), Some(episode));
        assert!(planner.finish_episode(episode));
        assert!(planner.begin_episode("HTTP 410", 4, 11).is_none());

        planner.begin_logical_item(5);
        let (episode, _) = planner
            .begin_episode("HTTP 410", 5, 12)
            .expect("new logical item is eligible");
        assert!(planner.cancel_unadmitted_episode(episode));
        assert!(planner.begin_episode("HTTP 410", 5, 12).is_some());
    }

    #[test]
    fn episode_identity_exhaustion_never_wraps_to_the_emergency_sentinel() {
        let mut planner = RecoveryPlanner {
            next_episode: u64::MAX,
            ..RecoveryPlanner::default()
        };

        assert!(planner.begin_ticket("HTTP 403", 1, 1).is_none());
        assert_eq!(planner.active_ticket(), None);
    }

    #[test]
    fn resume_origin_replaces_zero_sentinels_and_ram_only_flags() {
        let mut planner = RecoveryPlanner::default();
        let ticket = planner
            .begin_ticket("HTTP 403", 1, 2)
            .expect("source recovery ticket");
        let source = LoadWithResume::source_recovery(
            "source".to_owned(),
            12.0,
            false,
            super::super::MediaSourceContext::OnDemand,
            ticket,
        );
        assert_eq!(source.recovery_ticket(), Some(ticket));
        assert!(source.is_source_recovery());
        assert!(!source.forces_ram_only());
        assert_ne!(ticket.episode_id().get(), 0);

        let cache = LoadWithResume::emergency(
            "cache".to_owned(),
            13.0,
            true,
            super::super::MediaSourceContext::OnDemand,
        );
        assert_eq!(cache.recovery_ticket(), None);
        assert!(!cache.is_source_recovery());
        assert!(cache.forces_ram_only());
    }

    #[test]
    fn transport_restore_plan_has_one_exhaustive_command_ordering() {
        let filter = || Some("lavfi=[volume=1]".to_owned());
        assert!(
            TransportRestorePlan::idle()
                .into_commands(filter())
                .is_empty()
        );

        let playing = TransportRestorePlan::reload_if_loaded(
            Some("playing"),
            "playing",
            "playing".to_owned(),
            false,
            super::super::MediaSourceContext::OnDemand,
        )
        .into_commands(None);
        assert!(matches!(
            playing.as_slice(),
            [super::super::PlayerCmd::Load(_)]
        ));

        let paused = TransportRestorePlan::reload_if_loaded(
            Some("paused"),
            "paused",
            "paused".to_owned(),
            true,
            super::super::MediaSourceContext::OnDemand,
        )
        .into_commands(filter());
        assert!(matches!(
            paused.as_slice(),
            [
                super::super::PlayerCmd::Load(_),
                super::super::PlayerCmd::SetAudioFilter(value),
                super::super::PlayerCmd::CyclePause,
            ] if value == "lavfi=[volume=1]"
        ));

        let ram_only = TransportRestorePlan::resume_ram_only_if_loaded(
            Some("resume"),
            "resume",
            "resume".to_owned(),
            42.0,
            true,
            super::super::MediaSourceContext::OnDemand,
        )
        .into_commands(filter());
        assert!(matches!(
            ram_only.as_slice(),
            [
                super::super::PlayerCmd::LoadWithResume(request),
                super::super::PlayerCmd::SetAudioFilter(value),
            ] if request.forces_ram_only() && request.paused && value == "lavfi=[volume=1]"
        ));

        assert!(
            TransportRestorePlan::reload_if_loaded(
                None,
                "stopped",
                "must-not-load".to_owned(),
                false,
                super::super::MediaSourceContext::OnDemand,
            )
            .into_commands(None)
            .is_empty()
        );
    }
}
