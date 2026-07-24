//! Daemon owner-loop event taxonomy: every event admitted into the serve loop, the
//! delivery policy that governs it, and the bounded ingress wrapper that enforces
//! those policies. The serve loop itself stays in `super` — this module owns only
//! the classification machinery.

use tokio::sync::mpsc;

use crate::owner_event_policy::{
    api_event_policy, download_event_policy, player_event_policy, remote_event_policy,
    scrobble_event_policy, transfer_event_policy,
};
use crate::remote::server::RemoteEvent;
use crate::util::delivery::{DeliveryResult, OwnerEvent, OwnerEventIngress};
use crate::util::event_policy::{EventKey as Key, EventLane as Lane, EventPolicy};

use super::personal_export;

pub(super) enum DaemonEvent {
    Remote(RemoteEvent),
    Player(crate::player::PlayerEvent),
    Api(crate::api::ApiEvent),
    /// A command from the OS media session (media keys / Now Playing / SMTC / MPRIS).
    Media(crate::media::MediaCommand),
    /// The media-artwork cache resolved a local file for a track.
    MediaArt(crate::media::artwork::MediaArtworkReady),
    /// Scrobble-actor notices. The daemon has no UI and never runs the interactive auth
    /// flow (`ytt auth lastfm` does), so these only reach the log.
    Scrobble(crate::scrobble::ScrobbleEvent),
    /// The lyrics actor resolved (or failed to find) lines for a track. Fetches are
    /// gated on a live `lyrics` subscriber; see [`super::lyrics_host::LyricsHost`].
    Lyrics(crate::lyrics::LyricsEvent),
    /// Download actor progress/results. Import-owned variants are transfer-lane work and
    /// are ignored by the daemon downloads host.
    Download(crate::download::DownloadEvent),
    /// Transfer actor progress/results, reduced by the daemon transfer host.
    Transfer(crate::transfer::actor::TransferEvent),
    /// DJ Gem actor output, reduced by the daemon AI host on the owner lane.
    Ai(crate::ai::AiEvent),
    /// A playback-self-heal yt-dlp update check finished (see
    /// [`super::engine::EngineEffect::YtdlpSelfHeal`]).
    YtdlpHeal {
        video_id: String,
        updated: bool,
    },
    /// Bounded, generation-tagged retry after an automatic player restart/replay failed.
    TransportRecoveryRetry {
        generation: u64,
    },
    /// An owned blocking personal-data projection finished and is ready to settle its retained
    /// remote request on the owner lane.
    PersonalExportFinished(personal_export::Finished),
    /// Detached WebDAV preparation completed; only the owner lane may revision-check and install
    /// this candidate.
    PersonalSyncFinished(Box<super::personal_sync::Finished>),
    Signal,
    TelemetryWake,
}

impl DaemonEvent {
    pub(super) fn policy(&self) -> EventPolicy {
        match self {
            DaemonEvent::Remote(event) => remote_event_policy(event),
            DaemonEvent::Player(event) => player_event_policy(event.unscoped()),
            DaemonEvent::Api(event) => api_event_policy(event),
            DaemonEvent::Media(_) => EventPolicy::MustDeliver {
                lane: Lane::Control,
            },
            DaemonEvent::MediaArt(_) => EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::MediaArtVideo,
            },
            DaemonEvent::Scrobble(event) => scrobble_event_policy(event),
            // The daemon lyrics host already gates fetches by subscriber and track generation.
            DaemonEvent::Lyrics(_) => EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            },
            DaemonEvent::Download(event) => download_event_policy(event),
            DaemonEvent::Transfer(event) => transfer_event_policy(event),
            DaemonEvent::Ai(crate::ai::AiEvent::Thinking(_)) => EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::AiThinking,
            },
            // The daemon AI host settles its own in-flight request state on the owner lane, so a
            // terminal pick result must arrive even when its seed is no longer current.
            DaemonEvent::Ai(crate::ai::AiEvent::StreamingPicks { .. }) => {
                EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                }
            }
            DaemonEvent::Ai(_) => EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            },
            DaemonEvent::YtdlpHeal { .. } => EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            },
            DaemonEvent::TransportRecoveryRetry { .. } => EventPolicy::MustDeliver {
                lane: Lane::Control,
            },
            DaemonEvent::PersonalExportFinished(_) | DaemonEvent::PersonalSyncFinished(_) => {
                EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                }
            }
            DaemonEvent::Signal => EventPolicy::MustDeliver {
                lane: Lane::Control,
            },
            DaemonEvent::TelemetryWake => EventPolicy::MustDeliver {
                lane: Lane::Control,
            },
        }
    }

    pub(super) fn kind(&self) -> &'static str {
        match self {
            DaemonEvent::Remote(_) => "remote",
            DaemonEvent::Player(_) => "player",
            DaemonEvent::Api(_) => "api",
            DaemonEvent::Media(_) => "media",
            DaemonEvent::MediaArt(_) => "media_art",
            DaemonEvent::Scrobble(_) => "scrobble",
            DaemonEvent::Lyrics(_) => "lyrics",
            DaemonEvent::Download(_) => "download",
            DaemonEvent::Transfer(_) => "transfer",
            DaemonEvent::Ai(_) => "ai",
            DaemonEvent::YtdlpHeal { .. } => "ytdlp_heal",
            DaemonEvent::TransportRecoveryRetry { .. } => "transport_recovery_retry",
            DaemonEvent::PersonalExportFinished(_) => "personal_export_finished",
            DaemonEvent::PersonalSyncFinished(_) => "personal_sync_finished",
            DaemonEvent::Signal => "signal",
            DaemonEvent::TelemetryWake => "telemetry_wake",
        }
    }

    pub(super) fn is_telemetry_wake(&self) -> bool {
        matches!(self, DaemonEvent::TelemetryWake)
    }

    pub(super) fn telemetry_slot(&self) -> Option<DaemonTelemetrySlot> {
        match self {
            DaemonEvent::MediaArt(ready) => Some(DaemonTelemetrySlot::MediaArt(ready.key.clone())),
            DaemonEvent::Download(crate::download::DownloadEvent::Progress {
                video_id, ..
            }) => Some(DaemonTelemetrySlot::DownloadProgress(video_id.clone())),
            DaemonEvent::Api(crate::api::ApiEvent::SearchResults { request_id, .. })
            | DaemonEvent::Api(crate::api::ApiEvent::SearchError { request_id, .. }) => {
                Some(DaemonTelemetrySlot::StaleSearch(*request_id))
            }
            DaemonEvent::Api(crate::api::ApiEvent::StreamingResults {
                request_id,
                seed_video_id,
                ..
            })
            | DaemonEvent::Api(crate::api::ApiEvent::StreamingPreflighted {
                request_id,
                seed_video_id,
                ..
            })
            | DaemonEvent::Api(crate::api::ApiEvent::StreamingError {
                request_id,
                seed_video_id,
                ..
            }) => Some(DaemonTelemetrySlot::StaleStreaming(
                *request_id,
                seed_video_id.clone(),
            )),
            _ => match self.policy() {
                EventPolicy::CoalesceLatest { key, .. } => Some(DaemonTelemetrySlot::Static(key)),
                _ => None,
            },
        }
    }
}

const DAEMON_TELEMETRY_SLOTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum DaemonTelemetrySlot {
    Static(Key),
    MediaArt(String),
    DownloadProgress(String),
    StaleSearch(u64),
    StaleStreaming(u64, String),
}

impl OwnerEvent for DaemonEvent {
    type CoalesceKey = DaemonTelemetrySlot;

    fn policy(&self) -> EventPolicy {
        Self::policy(self)
    }

    fn kind(&self) -> &'static str {
        Self::kind(self)
    }

    fn coalesce_key(&self) -> Option<Self::CoalesceKey> {
        self.telemetry_slot()
    }

    fn wake_event() -> Self {
        Self::TelemetryWake
    }
}

#[derive(Clone)]
pub(super) struct DaemonEventSender {
    ingress: OwnerEventIngress<DaemonEvent>,
}

impl DaemonEventSender {
    pub(super) fn new(tx: mpsc::Sender<DaemonEvent>) -> Self {
        Self {
            ingress: OwnerEventIngress::new("daemon", tx, DAEMON_TELEMETRY_SLOTS),
        }
    }

    pub(super) fn drain_coalesced(&self) -> Vec<DaemonEvent> {
        self.ingress.drain_coalesced()
    }

    pub(super) fn close_admission(&self) -> bool {
        self.ingress.close_admission()
    }

    pub(super) fn deferred_is_idle(&self) -> bool {
        self.ingress.deferred_is_idle()
    }

    pub(super) fn emit_terminal_owned(
        &self,
        event: DaemonEvent,
    ) -> Result<
        crate::util::delivery::DeliveryReceipt,
        (crate::util::delivery::DeliveryError, Box<DaemonEvent>),
    > {
        self.ingress.emit_must_deliver_owned(event)
    }

    #[cfg(test)]
    pub(super) fn with_deferred_capacity(tx: mpsc::Sender<DaemonEvent>, capacity: usize) -> Self {
        Self {
            ingress: OwnerEventIngress::with_deferred_capacity(
                "daemon",
                tx,
                DAEMON_TELEMETRY_SLOTS,
                capacity,
            ),
        }
    }
}

pub(super) fn emit_daemon_event(tx: &DaemonEventSender, event: DaemonEvent) -> DeliveryResult {
    tx.ingress.emit(event)
}

#[cfg(test)]
pub(super) fn emit_daemon_callback_result(
    tx: &DaemonEventSender,
    event: DaemonEvent,
) -> DeliveryResult {
    tx.ingress.emit_callback_blocking(event)
}

#[cfg(any(windows, test))]
pub(super) fn emit_daemon_callback_result_until(
    tx: &DaemonEventSender,
    event: DaemonEvent,
    cancellation: &crate::util::delivery::CallbackCancellation,
) -> DeliveryResult {
    tx.ingress.emit_callback_blocking_until(event, cancellation)
}

pub(super) fn record_daemon_event(tx: &DaemonEventSender, event: DaemonEvent) {
    if let Err(error) = tx.ingress.emit_callback_blocking(event) {
        tracing::debug!(%error, "daemon event sink rejected event");
    }
}
