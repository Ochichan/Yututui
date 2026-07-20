use tokio::sync::mpsc::{Receiver, Sender};

use super::RuntimeEvent;
use super::event_policy::app_msg_policy;
use crate::app::{Msg, PlayerMsg, StreamingMsg};
use crate::util::delivery::{DeliveryResult, OwnerEvent, OwnerEventIngress};
use crate::util::event_policy::{EventKey as Key, EventPolicy};

const RUNTIME_TELEMETRY_SLOTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum RuntimeTelemetrySlot {
    Static(Key),
    DownloadProgress(String),
    MediaArt(String),
    StaleArtwork(String),
    StaleLyrics(String),
    StaleResolver(String),
    StaleSearch(u64),
    StaleStreamingRequest(u64, String),
    StaleStreamingResolved(String),
    StaleTrackResolve(u64),
    TransferProgress(String),
    VideoPaused(u64),
}

impl RuntimeEvent {
    pub(crate) fn telemetry_slot(&self) -> Option<RuntimeTelemetrySlot> {
        match self {
            RuntimeEvent::App(Msg::Download(crate::app::DownloadMsg::Progress {
                video_id,
                ..
            })) => Some(RuntimeTelemetrySlot::DownloadProgress(video_id.clone())),
            RuntimeEvent::App(Msg::Download(crate::app::DownloadMsg::ImportProgress {
                context,
                ..
            })) => Some(RuntimeTelemetrySlot::DownloadProgress(
                context.tracking_key(),
            )),
            RuntimeEvent::App(Msg::MediaArtworkReady(ready)) => {
                Some(RuntimeTelemetrySlot::MediaArt(ready.key.clone()))
            }
            RuntimeEvent::App(Msg::Transfer(crate::transfer::actor::TransferEvent::Progress(
                progress,
            ))) => Some(RuntimeTelemetrySlot::TransferProgress(
                progress.job_id.clone(),
            )),
            RuntimeEvent::App(Msg::Player(PlayerMsg::VideoOverlay { generation, event })) => {
                video_slot(*generation, event)
            }
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id, ..
            }) => Some(RuntimeTelemetrySlot::DownloadProgress(video_id.clone())),
            RuntimeEvent::Download(crate::download::DownloadEvent::ImportProgress {
                context,
                ..
            }) => Some(RuntimeTelemetrySlot::DownloadProgress(
                context.tracking_key(),
            )),
            RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::Progress(progress)) => {
                Some(RuntimeTelemetrySlot::TransferProgress(
                    progress.job_id.clone(),
                ))
            }
            RuntimeEvent::App(msg) => {
                app_stale_slot(msg).or_else(|| static_policy_slot(app_msg_policy(msg)))
            }
            RuntimeEvent::Ai(crate::ai::AiEvent::StreamingPicks {
                request_id,
                seed_video_id,
                ..
            }) => Some(RuntimeTelemetrySlot::StaleStreamingRequest(
                *request_id,
                seed_video_id.clone(),
            )),
            RuntimeEvent::Api(event) => api_stale_slot(event),
            RuntimeEvent::Artwork(crate::artwork::ArtworkEvent::Result { video_id, .. }) => {
                Some(RuntimeTelemetrySlot::StaleArtwork(video_id.clone()))
            }
            RuntimeEvent::Lyrics(crate::lyrics::LyricsEvent::Result { video_id, .. }) => {
                Some(RuntimeTelemetrySlot::StaleLyrics(video_id.clone()))
            }
            RuntimeEvent::Resolver(event) => {
                let (video_id, purpose) = match event {
                    crate::resolver::ResolverEvent::Resolved {
                        video_id, purpose, ..
                    }
                    | crate::resolver::ResolverEvent::Failed { video_id, purpose } => {
                        (video_id, purpose)
                    }
                };
                (*purpose == crate::resolver::ResolvePurpose::Prefetch)
                    .then(|| RuntimeTelemetrySlot::StaleResolver(video_id.as_str().to_owned()))
            }
            RuntimeEvent::Video { generation, event } => video_slot(*generation, event),
            _ => static_policy_slot(self.policy()),
        }
    }
}

fn static_policy_slot(policy: EventPolicy) -> Option<RuntimeTelemetrySlot> {
    match policy {
        EventPolicy::CoalesceLatest { key, .. } => Some(RuntimeTelemetrySlot::Static(key)),
        _ => None,
    }
}

fn video_slot(
    generation: u64,
    event: &crate::player::video::VideoEvent,
) -> Option<RuntimeTelemetrySlot> {
    match event {
        crate::player::video::VideoEvent::Paused(_) => {
            Some(RuntimeTelemetrySlot::VideoPaused(generation))
        }
        crate::player::video::VideoEvent::Eof
        | crate::player::video::VideoEvent::Failed(_)
        | crate::player::video::VideoEvent::Quit
        | crate::player::video::VideoEvent::Closed => None,
        crate::player::video::VideoEvent::Next
        | crate::player::video::VideoEvent::Prev
        | crate::player::video::VideoEvent::TogglePause
        | crate::player::video::VideoEvent::Close
        | crate::player::video::VideoEvent::ToggleFullscreen
        | crate::player::video::VideoEvent::ToggleMute => None,
    }
}

fn api_stale_slot(event: &crate::api::ApiEvent) -> Option<RuntimeTelemetrySlot> {
    match event {
        crate::api::ApiEvent::SearchResults { request_id, .. }
        | crate::api::ApiEvent::SearchError { request_id, .. } => {
            Some(RuntimeTelemetrySlot::StaleSearch(*request_id))
        }
        crate::api::ApiEvent::StreamingResults {
            request_id,
            seed_video_id,
            ..
        }
        | crate::api::ApiEvent::StreamingPreflighted {
            request_id,
            seed_video_id,
            ..
        }
        | crate::api::ApiEvent::StreamingError {
            request_id,
            seed_video_id,
            ..
        } => Some(RuntimeTelemetrySlot::StaleStreamingRequest(
            *request_id,
            seed_video_id.clone(),
        )),
        _ => None,
    }
}

fn app_stale_slot(msg: &Msg) -> Option<RuntimeTelemetrySlot> {
    match msg {
        Msg::SearchResults { request_id, .. } | Msg::SearchError { request_id, .. } => {
            Some(RuntimeTelemetrySlot::StaleSearch(*request_id))
        }
        Msg::ArtworkResult { video_id, .. } => {
            Some(RuntimeTelemetrySlot::StaleArtwork(video_id.clone()))
        }
        Msg::LyricsResult { video_id, .. } => {
            Some(RuntimeTelemetrySlot::StaleLyrics(video_id.clone()))
        }
        Msg::Streaming(StreamingMsg::Resolved {
            self_heal: true, ..
        }) => None,
        Msg::Streaming(StreamingMsg::Resolved { video_id, .. }) => Some(
            RuntimeTelemetrySlot::StaleStreamingResolved(video_id.clone()),
        ),
        Msg::Streaming(
            StreamingMsg::Results {
                request_id,
                seed_video_id,
                ..
            }
            | StreamingMsg::Preflighted {
                request_id,
                seed_video_id,
                ..
            }
            | StreamingMsg::PreflightError {
                request_id,
                seed_video_id,
                ..
            }
            | StreamingMsg::Error {
                request_id,
                seed_video_id,
                ..
            }
            | StreamingMsg::AiPicks {
                request_id,
                seed_video_id,
                ..
            },
        ) => Some(RuntimeTelemetrySlot::StaleStreamingRequest(
            *request_id,
            seed_video_id.clone(),
        )),
        Msg::TrackResolved { seq, .. } => Some(RuntimeTelemetrySlot::StaleTrackResolve(*seq)),
        _ => None,
    }
}

impl OwnerEvent for RuntimeEvent {
    type CoalesceKey = RuntimeTelemetrySlot;

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
pub struct RuntimeSender {
    ingress: OwnerEventIngress<RuntimeEvent>,
}

impl RuntimeSender {
    pub fn new(tx: Sender<RuntimeEvent>) -> Self {
        Self {
            ingress: OwnerEventIngress::new("runtime", tx, RUNTIME_TELEMETRY_SLOTS),
        }
    }

    pub fn drain_coalesced(&self) -> Vec<RuntimeEvent> {
        self.ingress.drain_coalesced()
    }

    pub(crate) fn close_admission(&self) -> bool {
        self.ingress.close_admission()
    }

    pub(crate) fn deferred_is_idle(&self) -> bool {
        self.ingress.deferred_is_idle()
    }

    #[cfg(test)]
    pub(crate) fn with_deferred_capacity(
        tx: Sender<RuntimeEvent>,
        deferred_capacity: usize,
    ) -> Self {
        Self {
            ingress: OwnerEventIngress::with_deferred_capacity(
                "runtime",
                tx,
                RUNTIME_TELEMETRY_SLOTS,
                deferred_capacity,
            ),
        }
    }
}

pub fn channel(
    policy: crate::util::backpressure::QueuePolicy,
) -> (RuntimeSender, Receiver<RuntimeEvent>) {
    let (tx, rx) = crate::util::backpressure::bounded_channel(policy);
    (RuntimeSender::new(tx), rx)
}

pub fn emit(tx: &RuntimeSender, event: RuntimeEvent) -> DeliveryResult {
    tx.ingress.emit(event)
}

/// Apply callback-owned backpressure while preserving the delivery result for platforms whose
/// callback runs off the owner thread. In particular, Windows SMTC has no return channel for a
/// busy media button, so the dedicated callback thread retains the exact command until admitted.
pub fn emit_callback_result(tx: &RuntimeSender, event: RuntimeEvent) -> DeliveryResult {
    tx.ingress.emit_callback_blocking(event)
}

/// Retain a callback-owned terminal event until owner admission or retirement of the platform
/// producer generation. Unlike closing the runtime receiver, generation cancellation is safe for
/// a live Settings toggle because unrelated producers and the owner loop stay open.
#[cfg(any(windows, test))]
pub(crate) fn emit_callback_result_until(
    tx: &RuntimeSender,
    event: RuntimeEvent,
    cancellation: &crate::util::delivery::CallbackCancellation,
) -> DeliveryResult {
    tx.ingress.emit_callback_blocking_until(event, cancellation)
}

/// Deliver from a callback which cannot return an owned saturation rejection. Terminal events
/// retain bounded producer-side ownership until admitted; coalesced/busy policies stay nonblocking.
pub fn emit_callback_observed(tx: &RuntimeSender, event: RuntimeEvent) {
    if let Err(error) = emit_callback_result(tx, event) {
        tracing::debug!(%error, "runtime callback event sink rejected event");
    }
}

pub(super) fn emit_terminal_owned(
    tx: &RuntimeSender,
    event: RuntimeEvent,
) -> Result<
    crate::util::delivery::DeliveryReceipt,
    (crate::util::delivery::DeliveryError, Box<RuntimeEvent>),
> {
    tx.ingress.emit_must_deliver_owned(event)
}

/// Submit an event from a callback that cannot return delivery status to its caller.
/// Rejections are still explicit in structured logs rather than being silently ignored.
pub fn emit_observed(tx: &RuntimeSender, event: RuntimeEvent) {
    if let Err(error) = emit(tx, event) {
        tracing::debug!(%error, "runtime event sink rejected event");
    }
}
