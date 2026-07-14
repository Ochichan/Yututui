//! One-shot, position-preserving source-recovery contracts shared by App and daemon owners.

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RecoveryEpisodeId(u64);

impl RecoveryEpisodeId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportIntentEpoch(u64);

impl TransportIntentEpoch {
    pub const fn get(self) -> u64 {
        self.0
    }
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
    /// Emergency disk-safety recycle: the replacement actor must keep this one media in RAM
    /// even when the persisted/requested policy remains Auto or On.
    pub(crate) force_ram_only: bool,
}

impl LoadWithResume {
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
            // Emergency recycle is actor-owned rather than a source-resolution episode. The
            // zero identities are never admitted to RecoveryPlanner and exist only so the one
            // correlated file-loaded resume representation remains shared.
            episode_id: RecoveryEpisodeId(0),
            transport_epoch: TransportIntentEpoch(0),
            force_ram_only: true,
        }
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

/// URL-free error vocabulary used when mpv reports only `loading failed` (including mpv 0.32).
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

/// Owner-side episode and transport-intent arbiter.
#[derive(Default)]
pub struct RecoveryPlanner {
    next_episode: u64,
    transport_epoch: u64,
    attempted_item_generation: Option<u64>,
    active: Option<Episode>,
}

impl RecoveryPlanner {
    pub fn transport_epoch(&self) -> TransportIntentEpoch {
        TransportIntentEpoch(self.transport_epoch)
    }

    /// Call after admitting a newer user seek, play/pause, Load, Stop, or track change.
    pub fn supersede_transport(&mut self) -> TransportIntentEpoch {
        self.transport_epoch = self.transport_epoch.wrapping_add(1);
        self.active = None;
        TransportIntentEpoch(self.transport_epoch)
    }

    /// A logical queue-item change resets the one-attempt latch and invalidates pending work.
    pub fn begin_logical_item(&mut self, logical_item_generation: u64) {
        if self.attempted_item_generation != Some(logical_item_generation) {
            self.attempted_item_generation = None;
        }
        self.supersede_transport();
    }

    pub fn begin_episode(
        &mut self,
        error: &str,
        logical_item_generation: u64,
        origin_file_generation: u64,
    ) -> Option<(RecoveryEpisodeId, TransportIntentEpoch)> {
        classify_source_failure(error)?;
        if self.attempted_item_generation == Some(logical_item_generation) || self.active.is_some()
        {
            return None;
        }
        self.next_episode = self.next_episode.wrapping_add(1);
        let episode = Episode {
            id: RecoveryEpisodeId(self.next_episode),
            logical_item_generation,
            origin_file_generation,
            transport_epoch: TransportIntentEpoch(self.transport_epoch),
        };
        self.attempted_item_generation = Some(logical_item_generation);
        self.active = Some(episode);
        Some((episode.id, episode.transport_epoch))
    }

    /// Async resolution may complete only while every captured identity remains current.
    pub fn accepts_resolved_source(
        &self,
        episode_id: RecoveryEpisodeId,
        logical_item_generation: u64,
        origin_file_generation: u64,
        transport_epoch: TransportIntentEpoch,
    ) -> bool {
        self.active.is_some_and(|episode| {
            episode.id == episode_id
                && episode.logical_item_generation == logical_item_generation
                && episode.origin_file_generation == origin_file_generation
                && episode.transport_epoch == transport_epoch
                && transport_epoch == TransportIntentEpoch(self.transport_epoch)
        })
    }

    pub fn finish_episode(&mut self, episode_id: RecoveryEpisodeId) -> bool {
        if self.active.is_some_and(|episode| episode.id == episode_id) {
            self.active = None;
            true
        } else {
            false
        }
    }

    /// Roll back an episode whose semantic command never entered the player lane. No retry was
    /// attempted, so the same logical item remains eligible for a later admission attempt.
    pub fn cancel_unadmitted_episode(&mut self, episode_id: RecoveryEpisodeId) -> bool {
        if self.active.is_some_and(|episode| episode.id == episode_id) {
            self.active = None;
            self.attempted_item_generation = None;
            true
        } else {
            false
        }
    }

    pub fn active_episode(&self) -> Option<RecoveryEpisodeId> {
        self.active.map(|episode| episode.id)
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
        let (episode, epoch) = planner
            .begin_episode("HTTP 403", 4, 11)
            .expect("first failure should recover");
        assert!(planner.accepts_resolved_source(episode, 4, 11, epoch));
        assert!(planner.finish_episode(episode));
        assert!(planner.begin_episode("HTTP 410", 4, 12).is_none());

        planner.begin_logical_item(5);
        assert!(planner.begin_episode("HTTP 410", 5, 13).is_some());
    }

    #[test]
    fn newer_transport_intent_invalidates_async_resolution() {
        let mut planner = RecoveryPlanner::default();
        let (episode, epoch) = planner
            .begin_episode("connection reset", 9, 20)
            .expect("episode");
        planner.supersede_transport();
        assert!(!planner.accepts_resolved_source(episode, 9, 20, epoch));
        assert_eq!(planner.active_episode(), None);
    }

    #[test]
    fn stale_item_or_file_identity_is_rejected() {
        let mut planner = RecoveryPlanner::default();
        let (episode, epoch) = planner
            .begin_episode("premature EOF", 7, 30)
            .expect("episode");
        assert!(!planner.accepts_resolved_source(episode, 8, 30, epoch));
        assert!(!planner.accepts_resolved_source(episode, 7, 31, epoch));
    }

    #[test]
    fn rejected_admission_does_not_consume_the_item_attempt() {
        let mut planner = RecoveryPlanner::default();
        planner.begin_logical_item(7);
        let (episode, _) = planner
            .begin_episode("HTTP 403", 7, 30)
            .expect("first admission plan");
        assert!(planner.cancel_unadmitted_episode(episode));
        assert!(planner.begin_episode("HTTP 403", 7, 30).is_some());
    }
}
