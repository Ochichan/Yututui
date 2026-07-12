//! Daemon owner-loop event taxonomy: every event admitted into the serve loop, the
//! delivery policy that governs it, and the bounded ingress wrapper that enforces
//! those policies. The serve loop itself stays in `super` — this module owns only
//! the classification machinery.

use tokio::sync::mpsc;

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
    Signal,
    TelemetryWake,
}

impl DaemonEvent {
    pub(super) fn policy(&self) -> EventPolicy {
        match self {
            DaemonEvent::Remote(
                RemoteEvent::Command(_, _)
                | RemoteEvent::SessionCommand { .. }
                | RemoteEvent::SessionSubscribe { .. },
            ) => EventPolicy::MustReplyOrBusy {
                lane: Lane::RemoteCommand,
            },
            DaemonEvent::Player(event) => match event.unscoped() {
                crate::player::PlayerEvent::TimePos(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::PlayerTimePos,
                },
                crate::player::PlayerEvent::Duration(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::PlayerDuration,
                },
                crate::player::PlayerEvent::Paused(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::PlayerPaused,
                },
                crate::player::PlayerEvent::Volume(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::PlayerVolume,
                },
                crate::player::PlayerEvent::Metadata(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::WorkResult,
                    key: Key::PlayerMetadata,
                },
                crate::player::PlayerEvent::CacheTime(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::PlayerCacheTime,
                },
                crate::player::PlayerEvent::AudioCodec(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::PlayerAudioCodec,
                },
                crate::player::PlayerEvent::FileFormat(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::PlayerFileFormat,
                },
                crate::player::PlayerEvent::Eof
                | crate::player::PlayerEvent::Error(_)
                | crate::player::PlayerEvent::TransportClosed(_) => EventPolicy::MustDeliver {
                    lane: Lane::Control,
                },
                crate::player::PlayerEvent::FileScoped { .. } => {
                    unreachable!("daemon audio event was unscoped before policy lookup")
                }
            },
            DaemonEvent::Api(event) => match event {
                crate::api::ApiEvent::ModeResolved { .. }
                | crate::api::ApiEvent::TrackResolved { .. }
                | crate::api::ApiEvent::PlaylistTracks { .. }
                | crate::api::ApiEvent::PlaylistTracksError { .. } => EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                },
                crate::api::ApiEvent::SearchResults { .. }
                | crate::api::ApiEvent::SearchError { .. } => EventPolicy::DropIfStale {
                    stale_key: Key::SearchRequest,
                },
                crate::api::ApiEvent::StreamingResults { .. }
                | crate::api::ApiEvent::StreamingPreflighted { .. }
                | crate::api::ApiEvent::StreamingError { .. } => EventPolicy::DropIfStale {
                    stale_key: Key::StreamingSeed,
                },
                crate::api::ApiEvent::GuiSearchCompleted { .. } => EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                },
            },
            DaemonEvent::Media(_) => EventPolicy::MustDeliver {
                lane: Lane::Control,
            },
            DaemonEvent::MediaArt(_) => EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::MediaArtVideo,
            },
            DaemonEvent::Scrobble(event) => match event {
                crate::scrobble::ScrobbleEvent::AuthUrl(_)
                | crate::scrobble::ScrobbleEvent::AuthDone { .. }
                | crate::scrobble::ScrobbleEvent::AuthFailed(_)
                | crate::scrobble::ScrobbleEvent::SessionInvalid(_)
                | crate::scrobble::ScrobbleEvent::QueueDropped { .. } => EventPolicy::MustDeliver {
                    lane: Lane::Control,
                },
                crate::scrobble::ScrobbleEvent::QueueStalled { .. } => {
                    EventPolicy::CoalesceLatest {
                        lane: Lane::Telemetry,
                        key: Key::ScrobbleQueueStalled,
                    }
                }
            },
            DaemonEvent::Lyrics(_) => EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            },
            DaemonEvent::YtdlpHeal { .. } => EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            },
            DaemonEvent::TransportRecoveryRetry { .. } => EventPolicy::MustDeliver {
                lane: Lane::Control,
            },
            DaemonEvent::PersonalExportFinished(_) => EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            },
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
            DaemonEvent::YtdlpHeal { .. } => "ytdlp_heal",
            DaemonEvent::TransportRecoveryRetry { .. } => "transport_recovery_retry",
            DaemonEvent::PersonalExportFinished(_) => "personal_export_finished",
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
            DaemonEvent::Api(crate::api::ApiEvent::SearchResults { request_id, .. })
            | DaemonEvent::Api(crate::api::ApiEvent::SearchError { request_id, .. }) => {
                Some(DaemonTelemetrySlot::StaleSearch(*request_id))
            }
            DaemonEvent::Api(crate::api::ApiEvent::StreamingResults { seed_video_id, .. })
            | DaemonEvent::Api(crate::api::ApiEvent::StreamingPreflighted {
                seed_video_id, ..
            })
            | DaemonEvent::Api(crate::api::ApiEvent::StreamingError { seed_video_id, .. }) => {
                Some(DaemonTelemetrySlot::StaleStreaming(seed_video_id.clone()))
            }
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
    StaleSearch(u64),
    StaleStreaming(String),
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
