use crate::util::delivery::DeliveryResult;
use crate::{player::PlayerCmd, queue::QueueSnapshot, remote::proto::RemoteResponse};

pub(super) const PERSISTENCE_UNAVAILABLE_REASON: &str = "persistence_unavailable";
pub(super) const DURABILITY_UNCONFIRMED_REASON: &str = "durability_unconfirmed";

pub(super) struct OutgoingRecord {
    video_id: String,
    artist_key: String,
    outcome: super::DaemonOutcome,
    completion: f32,
    recorded_at: i64,
}

#[derive(Debug)]
pub enum EngineError {
    Player(String),
    StartupRecovery(crate::persist::StartupRecoveryError),
    PersistenceWrite(String),
}

impl EngineError {
    pub(super) fn reason(&self) -> &'static str {
        match self {
            Self::Player(_) => "mpv_unavailable",
            Self::StartupRecovery(_) | Self::PersistenceWrite(_) => PERSISTENCE_UNAVAILABLE_REASON,
        }
    }
}

impl From<crate::persist::StartupRecoveryError> for EngineError {
    fn from(error: crate::persist::StartupRecoveryError) -> Self {
        Self::StartupRecovery(error)
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Player(message) => write!(f, "{message}"),
            Self::StartupRecovery(error) => write!(f, "persistence unavailable: {error}"),
            Self::PersistenceWrite(message) => write!(f, "persistence unavailable: {message}"),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StartupRecovery(error) => Some(error),
            Self::Player(_) | Self::PersistenceWrite(_) => None,
        }
    }
}

pub(super) fn record_player_delivery(command: &'static str, result: DeliveryResult) -> bool {
    match result {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(command, %error, "daemon player command was not accepted");
            false
        }
    }
}

pub(super) fn require_player_delivery(
    command: &'static str,
    result: DeliveryResult,
) -> Result<(), EngineError> {
    match result {
        Ok(receipt) => {
            tracing::trace!(command, ?receipt, "daemon player command accepted");
            Ok(())
        }
        Err(error) => Err(EngineError::Player(format!(
            "player {command} setup failed: {error}"
        ))),
    }
}

impl super::DaemonEngine {
    pub(super) fn record_outgoing(&mut self, full: bool) {
        if let Some(outgoing) = self.prepare_outgoing(full) {
            self.commit_outgoing(outgoing);
        }
    }

    pub(super) fn prepare_outgoing(&self, full: bool) -> Option<OutgoingRecord> {
        let song = self.queue.current()?.clone();
        if song.is_radio_station() {
            return None;
        }
        let artist_key = crate::signals::normalize_artist(&song.artist);
        let recorded_at = crate::signals::unix_now();
        let (outcome, completion) = if full {
            (super::DaemonOutcome::FullPlay, 1.0)
        } else {
            let completion = self.playback_completion();
            let outcome = if completion < crate::signals::STRONG_SKIP_FRAC {
                super::DaemonOutcome::QuickSkip
            } else {
                super::DaemonOutcome::Skip
            };
            (outcome, completion)
        };
        Some(OutgoingRecord {
            video_id: song.video_id,
            artist_key,
            outcome,
            completion,
            recorded_at,
        })
    }

    pub(super) fn commit_outgoing(&mut self, outgoing: OutgoingRecord) {
        if outgoing.outcome == super::DaemonOutcome::FullPlay {
            self.signals.record_play(
                &outgoing.video_id,
                &outgoing.artist_key,
                outgoing.completion,
                outgoing.recorded_at,
            );
        } else {
            self.signals.record_skip(
                &outgoing.video_id,
                &outgoing.artist_key,
                outgoing.completion,
                outgoing.recorded_at,
                0.6,
            );
        }
        self.record_session_event(&outgoing.artist_key, outgoing.outcome, outgoing.completion);
        self.save_signals("daemon outgoing playback signals");
    }

    pub(super) fn send_active_player_command(
        &self,
        command: &'static str,
        player_command: PlayerCmd,
    ) -> Result<(), EngineError> {
        let player = self
            .player
            .as_ref()
            .ok_or_else(|| EngineError::Player(format!("player {command} is unavailable")))?;
        require_player_delivery(command, player.handle.send(player_command))
    }

    /// Admit an ordered player transaction without exposing a prefix to mpv. This is required
    /// for state changes whose rollback would otherwise put the queue and transport on different
    /// tracks after a later command in the sequence is rejected.
    pub(super) fn send_active_player_batch(
        &self,
        command: &'static str,
        player_commands: Vec<PlayerCmd>,
    ) -> Result<(), EngineError> {
        let player = self
            .player
            .as_ref()
            .ok_or_else(|| EngineError::Player(format!("player {command} is unavailable")))?;
        require_player_delivery(command, player.handle.send_batch(player_commands))
    }

    pub(super) fn send_player_command_if_active(
        &self,
        command: &'static str,
        player_command: PlayerCmd,
    ) -> Result<(), EngineError> {
        match &self.player {
            Some(player) => require_player_delivery(command, player.handle.send(player_command)),
            None => Ok(()),
        }
    }

    pub(super) fn reject_player_command(&mut self, error: EngineError) -> RemoteResponse {
        tracing::warn!(%error, "daemon player command rejected without committing state");
        self.last_error = Some(error.to_string());
        RemoteResponse::err(error.reason())
    }

    pub(super) async fn load_current_or_restore_queue(
        &mut self,
        previous: QueueSnapshot,
    ) -> Result<(), EngineError> {
        match self.load_current().await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.queue.restore_snapshot(previous);
                self.last_error = Some(error.to_string());
                Err(error)
            }
        }
    }
}
