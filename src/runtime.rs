//! Runtime event adapter between leaf actors and the app reducer.
//!
//! Actors emit domain-specific events so they do not depend on `crate::app::Msg`.
//! This module is the single orchestration boundary that maps those events back into
//! reducer messages.

use std::sync::{Arc, Mutex};

use ratatui_image::thread::ResizeResponse;
use tokio::sync::mpsc::{Receiver, Sender, error::TrySendError};

use crate::app::{AiMsg, App, Cmd, Msg, PersistCmd, PlayerMsg, StreamingMsg};
use crate::config::PlayerRuntimeConfig;
use crate::player::{PlayerCmd, PlayerHandle};
use crate::util::event_policy::{
    EventKey as Key, EventLane as Lane, EventPolicy, LatestEventBuffer,
};

pub enum RuntimeEvent {
    App(Msg),
    Ai(crate::ai::AiEvent),
    Api(crate::api::ApiEvent),
    Artwork(crate::artwork::ArtworkEvent),
    ArtworkResized(ResizeResponse),
    Download(crate::download::DownloadEvent),
    Lyrics(crate::lyrics::LyricsEvent),
    Player(crate::player::PlayerEvent),
    Persist(crate::persist::PersistEvent),
    Remote(crate::remote::server::RemoteEvent),
    /// From the video-overlay mpv's IPC client, tagged with its spawn generation.
    Video {
        generation: u64,
        event: crate::player::video::VideoEvent,
    },
    Resolver(crate::resolver::ResolverEvent),
    Scrobble(crate::scrobble::ScrobbleEvent),
    Signal(crate::player::lifetime::SignalEvent),
    /// Managed yt-dlp maintenance progress (download %, installed, failed).
    Tools(crate::tools::ToolsEvent),
    /// Background app-update check result (newer release available + install method).
    Update(crate::update::UpdateEvent),
    Transfer(crate::transfer::actor::TransferEvent),
    TelemetryWake,
}

impl RuntimeEvent {
    pub fn policy(&self) -> EventPolicy {
        match self {
            RuntimeEvent::App(msg) => app_msg_policy(msg),
            RuntimeEvent::Ai(event) => match event {
                crate::ai::AiEvent::Thinking(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::AiThinking,
                },
                crate::ai::AiEvent::StreamingPicks { .. } => EventPolicy::DropIfStale {
                    stale_key: Key::StreamingSeed,
                },
                crate::ai::AiEvent::Chat(_)
                | crate::ai::AiEvent::Error(_)
                | crate::ai::AiEvent::PlayTracks(_)
                | crate::ai::AiEvent::Enqueue(_)
                | crate::ai::AiEvent::Suggestions(_)
                | crate::ai::AiEvent::SetAutoplay(_)
                | crate::ai::AiEvent::SetStationProfile { .. }
                | crate::ai::AiEvent::CreatePlaylist(_)
                | crate::ai::AiEvent::AddToPlaylist { .. }
                | crate::ai::AiEvent::PlayPlaylist(_)
                | crate::ai::AiEvent::StationPatch { .. }
                | crate::ai::AiEvent::RomanizedTitles { .. } => EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                },
            },
            RuntimeEvent::Api(event) => match event {
                crate::api::ApiEvent::ModeResolved { .. }
                | crate::api::ApiEvent::TrackResolved { .. } => EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                },
                crate::api::ApiEvent::SearchResults { .. }
                | crate::api::ApiEvent::SearchError { .. } => EventPolicy::DropIfStale {
                    stale_key: Key::SearchRequest,
                },
                crate::api::ApiEvent::PlaylistTracks { .. }
                | crate::api::ApiEvent::PlaylistTracksError { .. } => {
                    EventPolicy::MustReplyOrBusy {
                        lane: Lane::WorkResult,
                    }
                }
                crate::api::ApiEvent::StreamingResults { .. }
                | crate::api::ApiEvent::StreamingPreflighted { .. }
                | crate::api::ApiEvent::StreamingError { .. } => EventPolicy::DropIfStale {
                    stale_key: Key::StreamingSeed,
                },
                crate::api::ApiEvent::GuiSearchCompleted { .. } => EventPolicy::DropIfStale {
                    stale_key: Key::GuiSearchTicket,
                },
            },
            RuntimeEvent::Artwork(_) => EventPolicy::DropIfStale {
                stale_key: Key::ArtworkVideo,
            },
            RuntimeEvent::ArtworkResized(_) => EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::ArtResize,
            },
            RuntimeEvent::Download(event) => match event {
                crate::download::DownloadEvent::Progress { .. } => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::DownloadProgress,
                },
                crate::download::DownloadEvent::Done { .. }
                | crate::download::DownloadEvent::Error { .. } => EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                },
            },
            RuntimeEvent::Lyrics(_) => EventPolicy::DropIfStale {
                stale_key: Key::LyricsVideo,
            },
            RuntimeEvent::Player(event) => match event {
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
                crate::player::PlayerEvent::Eof | crate::player::PlayerEvent::Error(_) => {
                    EventPolicy::MustDeliver {
                        lane: Lane::Control,
                    }
                }
            },
            RuntimeEvent::Persist(_) => EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            },
            RuntimeEvent::Remote(
                crate::remote::server::RemoteEvent::Command(_, _)
                | crate::remote::server::RemoteEvent::SessionSubscribe { .. },
            ) => EventPolicy::MustReplyOrBusy {
                lane: Lane::RemoteCommand,
            },
            RuntimeEvent::Video { .. } => EventPolicy::DropIfStale {
                stale_key: Key::VideoOverlayGeneration,
            },
            RuntimeEvent::Resolver(_) => EventPolicy::DropIfStale {
                stale_key: Key::ResolverVideo,
            },
            RuntimeEvent::Scrobble(_) => EventPolicy::BestEffort {
                reason: "scrobble UI notices are secondary to the durable scrobble queue",
            },
            RuntimeEvent::Signal(_) => EventPolicy::MustDeliver {
                lane: Lane::Control,
            },
            RuntimeEvent::Tools(event) => match event {
                crate::tools::ToolsEvent::Progress { .. } => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::ToolProgress,
                },
                crate::tools::ToolsEvent::Installed { .. }
                | crate::tools::ToolsEvent::Failed { .. } => EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                },
            },
            RuntimeEvent::Update(_) => EventPolicy::CoalesceLatest {
                lane: Lane::WorkResult,
                key: Key::UpdateCheck,
            },
            RuntimeEvent::Transfer(event) => match event {
                crate::transfer::actor::TransferEvent::Progress(_) => EventPolicy::CoalesceLatest {
                    lane: Lane::Telemetry,
                    key: Key::TransferJob,
                },
                crate::transfer::actor::TransferEvent::AuthUrl(_)
                | crate::transfer::actor::TransferEvent::AuthDone { .. }
                | crate::transfer::actor::TransferEvent::AuthError(_)
                | crate::transfer::actor::TransferEvent::Disconnected => EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                },
                crate::transfer::actor::TransferEvent::SpotifyPlaylists(_)
                | crate::transfer::actor::TransferEvent::JobDone(_)
                | crate::transfer::actor::TransferEvent::JobFailed { .. } => {
                    EventPolicy::MustDeliver {
                        lane: Lane::WorkResult,
                    }
                }
            },
            RuntimeEvent::TelemetryWake => EventPolicy::MustDeliver {
                lane: Lane::Control,
            },
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            RuntimeEvent::App(_) => "app",
            RuntimeEvent::Ai(_) => "ai",
            RuntimeEvent::Api(_) => "api",
            RuntimeEvent::Artwork(_) => "artwork",
            RuntimeEvent::ArtworkResized(_) => "artwork_resized",
            RuntimeEvent::Download(_) => "download",
            RuntimeEvent::Lyrics(_) => "lyrics",
            RuntimeEvent::Player(_) => "player",
            RuntimeEvent::Persist(_) => "persist",
            RuntimeEvent::Remote(_) => "remote",
            RuntimeEvent::Video { .. } => "video",
            RuntimeEvent::Resolver(_) => "resolver",
            RuntimeEvent::Scrobble(_) => "scrobble",
            RuntimeEvent::Signal(_) => "signal",
            RuntimeEvent::Tools(_) => "tools",
            RuntimeEvent::Update(_) => "update",
            RuntimeEvent::Transfer(_) => "transfer",
            RuntimeEvent::TelemetryWake => "telemetry_wake",
        }
    }

    pub fn is_telemetry_wake(&self) -> bool {
        matches!(self, RuntimeEvent::TelemetryWake)
    }

    fn telemetry_slot(&self) -> Option<RuntimeTelemetrySlot> {
        match self {
            RuntimeEvent::App(Msg::DownloadProgress { video_id, .. }) => {
                Some(RuntimeTelemetrySlot::DownloadProgress(video_id.clone()))
            }
            RuntimeEvent::App(Msg::MediaArtworkReady(ready)) => {
                Some(RuntimeTelemetrySlot::MediaArt(ready.key.clone()))
            }
            RuntimeEvent::App(Msg::Transfer(crate::transfer::actor::TransferEvent::Progress(
                progress,
            ))) => Some(RuntimeTelemetrySlot::TransferProgress(
                progress.job_id.clone(),
            )),
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id, ..
            }) => Some(RuntimeTelemetrySlot::DownloadProgress(video_id.clone())),
            RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::Progress(progress)) => {
                Some(RuntimeTelemetrySlot::TransferProgress(
                    progress.job_id.clone(),
                ))
            }
            _ => match self.policy() {
                EventPolicy::CoalesceLatest { key, .. } => Some(RuntimeTelemetrySlot::Static(key)),
                _ => None,
            },
        }
    }
}

fn app_msg_policy(msg: &Msg) -> EventPolicy {
    match msg {
        Msg::Quit => EventPolicy::MustDeliver {
            lane: Lane::Control,
        },
        Msg::Media(_) => EventPolicy::MustDeliver {
            lane: Lane::Control,
        },
        Msg::Remote(_, _) => EventPolicy::MustReplyOrBusy {
            lane: Lane::RemoteCommand,
        },
        Msg::Player(player) => app_player_msg_policy(player),
        Msg::ArtworkResized(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::ArtResize,
        },
        Msg::DownloadProgress { .. } => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::DownloadProgress,
        },
        Msg::MediaArtworkReady(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::MediaArtVideo,
        },
        Msg::Tools(crate::tools::ToolsEvent::Progress { .. }) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::ToolProgress,
        },
        Msg::UpdateChecked(_) => EventPolicy::CoalesceLatest {
            lane: Lane::WorkResult,
            key: Key::UpdateCheck,
        },
        Msg::Transfer(crate::transfer::actor::TransferEvent::Progress(_)) => {
            EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::TransferJob,
            }
        }
        Msg::SearchResults { .. } | Msg::SearchError { .. } => EventPolicy::DropIfStale {
            stale_key: Key::SearchRequest,
        },
        Msg::ArtworkResult { .. } => EventPolicy::DropIfStale {
            stale_key: Key::ArtworkVideo,
        },
        Msg::LyricsResult { .. } => EventPolicy::DropIfStale {
            stale_key: Key::LyricsVideo,
        },
        Msg::Streaming(_) => EventPolicy::DropIfStale {
            stale_key: Key::StreamingSeed,
        },
        Msg::TrackResolved { .. } => EventPolicy::DropIfStale {
            stale_key: Key::ResolverVideo,
        },
        Msg::ResolveFailed { .. } => EventPolicy::DropIfStale {
            stale_key: Key::ResolverVideo,
        },
        Msg::Noop | Msg::StatusTick | Msg::AnimTick | Msg::RecordingTick => {
            EventPolicy::BestEffort {
                reason: "loop-owned ticks and inert messages are redraw/status hints",
            }
        }
        Msg::Key(_)
        | Msg::MouseClick { .. }
        | Msg::MouseDoubleClick { .. }
        | Msg::MouseRightClick { .. }
        | Msg::MouseDrag { .. }
        | Msg::MouseLeftUp
        | Msg::MouseScroll { .. }
        | Msg::Resize
        | Msg::Focus(_)
        | Msg::Autoplay
        | Msg::ApiModeResolved { .. }
        | Msg::Recorder(_)
        | Msg::PlaylistTracks { .. }
        | Msg::PlaylistTracksError { .. }
        | Msg::DownloadsScanned(_)
        | Msg::Local(_)
        | Msg::DownloadDone { .. }
        | Msg::DownloadError { .. }
        | Msg::DownloadDirError { .. }
        | Msg::PersistFailed { .. }
        | Msg::Ai(_)
        | Msg::Scrobble(_)
        | Msg::Tools(
            crate::tools::ToolsEvent::Installed { .. } | crate::tools::ToolsEvent::Failed { .. },
        )
        | Msg::YtdlpHealResult { .. }
        | Msg::Transfer(
            crate::transfer::actor::TransferEvent::AuthUrl(_)
            | crate::transfer::actor::TransferEvent::AuthDone { .. }
            | crate::transfer::actor::TransferEvent::AuthError(_)
            | crate::transfer::actor::TransferEvent::Disconnected
            | crate::transfer::actor::TransferEvent::SpotifyPlaylists(_)
            | crate::transfer::actor::TransferEvent::JobDone(_)
            | crate::transfer::actor::TransferEvent::JobFailed { .. },
        ) => EventPolicy::MustDeliver {
            lane: Lane::WorkResult,
        },
    }
}

fn app_player_msg_policy(msg: &PlayerMsg) -> EventPolicy {
    match msg {
        PlayerMsg::TimePos(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerTimePos,
        },
        PlayerMsg::Duration(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerDuration,
        },
        PlayerMsg::Paused(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerPaused,
        },
        PlayerMsg::Volume(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerVolume,
        },
        PlayerMsg::Metadata(_) => EventPolicy::CoalesceLatest {
            lane: Lane::WorkResult,
            key: Key::PlayerMetadata,
        },
        PlayerMsg::CacheTime(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerCacheTime,
        },
        PlayerMsg::AudioCodec(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerAudioCodec,
        },
        PlayerMsg::FileFormat(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::PlayerFileFormat,
        },
        PlayerMsg::Eof | PlayerMsg::Error(_) => EventPolicy::MustDeliver {
            lane: Lane::Control,
        },
        PlayerMsg::VideoOverlay { .. } => EventPolicy::DropIfStale {
            stale_key: Key::VideoOverlayGeneration,
        },
    }
}

impl From<RuntimeEvent> for Msg {
    fn from(event: RuntimeEvent) -> Self {
        match event {
            RuntimeEvent::App(msg) => msg,
            RuntimeEvent::Ai(event) => match event {
                crate::ai::AiEvent::Thinking(on) => Msg::Ai(AiMsg::Thinking(on)),
                crate::ai::AiEvent::Chat(text) => Msg::Ai(AiMsg::Chat(text)),
                crate::ai::AiEvent::Error(text) => Msg::Ai(AiMsg::Error(text)),
                crate::ai::AiEvent::PlayTracks(songs) => Msg::Ai(AiMsg::PlayTracks(songs)),
                crate::ai::AiEvent::Enqueue(songs) => Msg::Ai(AiMsg::Enqueue(songs)),
                crate::ai::AiEvent::Suggestions(songs) => Msg::Ai(AiMsg::Suggestions(songs)),
                crate::ai::AiEvent::SetAutoplay(on) => Msg::Ai(AiMsg::SetAutoplay(on)),
                crate::ai::AiEvent::SetStationProfile {
                    query,
                    explore,
                    avoid_artists,
                } => Msg::Ai(AiMsg::SetStationProfile {
                    query,
                    explore,
                    avoid_artists,
                }),
                crate::ai::AiEvent::CreatePlaylist(name) => Msg::Ai(AiMsg::CreatePlaylist(name)),
                crate::ai::AiEvent::AddToPlaylist { playlist, songs } => {
                    Msg::Ai(AiMsg::AddToPlaylist { playlist, songs })
                }
                crate::ai::AiEvent::PlayPlaylist(key) => Msg::Ai(AiMsg::PlayPlaylist(key)),
                crate::ai::AiEvent::StreamingPicks {
                    seed_video_id,
                    picks,
                    conf,
                } => Msg::Streaming(StreamingMsg::AiPicks {
                    seed_video_id,
                    picks,
                    conf,
                }),
                crate::ai::AiEvent::StationPatch {
                    down_artists,
                    boost_artists,
                } => Msg::Ai(AiMsg::StationPatch {
                    down_artists,
                    boost_artists,
                }),
                crate::ai::AiEvent::RomanizedTitles {
                    request_id,
                    keys,
                    entries,
                } => Msg::Ai(AiMsg::RomanizedTitles {
                    request_id,
                    keys,
                    entries,
                }),
            },
            RuntimeEvent::Api(event) => match event {
                crate::api::ApiEvent::ModeResolved { mode, had_cookie } => {
                    Msg::ApiModeResolved { mode, had_cookie }
                }
                crate::api::ApiEvent::TrackResolved { seq, result } => {
                    Msg::TrackResolved { seq, result }
                }
                crate::api::ApiEvent::SearchResults {
                    request_id,
                    query,
                    source,
                    songs,
                    timed_out,
                } => Msg::SearchResults {
                    request_id,
                    query,
                    source,
                    songs,
                    timed_out,
                },
                crate::api::ApiEvent::SearchError {
                    request_id,
                    source,
                    error,
                } => Msg::SearchError {
                    request_id,
                    source,
                    error,
                },
                crate::api::ApiEvent::PlaylistTracks {
                    title,
                    intent,
                    songs,
                } => Msg::PlaylistTracks {
                    title,
                    intent,
                    songs,
                },
                crate::api::ApiEvent::PlaylistTracksError { title, error } => {
                    Msg::PlaylistTracksError { title, error }
                }
                crate::api::ApiEvent::StreamingResults {
                    seed_video_id,
                    candidates,
                } => Msg::Streaming(StreamingMsg::Results {
                    seed_video_id,
                    candidates,
                }),
                crate::api::ApiEvent::StreamingPreflighted {
                    seed_video_id,
                    songs,
                } => Msg::Streaming(StreamingMsg::Preflighted {
                    seed_video_id,
                    songs,
                }),
                crate::api::ApiEvent::StreamingError {
                    seed_video_id,
                    error,
                } => Msg::Streaming(StreamingMsg::Error {
                    seed_video_id,
                    error,
                }),
                // Daemon-owner lane only: the standalone TUI rejects `run_search`
                // (`daemon_required`), so its api actor never produces this.
                crate::api::ApiEvent::GuiSearchCompleted { .. } => Msg::Noop,
            },
            RuntimeEvent::Artwork(crate::artwork::ArtworkEvent::Result { video_id, image }) => {
                Msg::ArtworkResult { video_id, image }
            }
            RuntimeEvent::ArtworkResized(response) => Msg::ArtworkResized(response),
            RuntimeEvent::Download(event) => match event {
                crate::download::DownloadEvent::Progress { video_id, percent } => {
                    Msg::DownloadProgress { video_id, percent }
                }
                crate::download::DownloadEvent::Done { video_id, path } => {
                    Msg::DownloadDone { video_id, path }
                }
                crate::download::DownloadEvent::Error { video_id, error } => {
                    Msg::DownloadError { video_id, error }
                }
            },
            RuntimeEvent::Lyrics(crate::lyrics::LyricsEvent::Result { video_id, lines }) => {
                Msg::LyricsResult { video_id, lines }
            }
            RuntimeEvent::Player(event) => match event {
                crate::player::PlayerEvent::TimePos(t) => Msg::Player(PlayerMsg::TimePos(t)),
                crate::player::PlayerEvent::Duration(d) => Msg::Player(PlayerMsg::Duration(d)),
                crate::player::PlayerEvent::Paused(paused) => {
                    Msg::Player(PlayerMsg::Paused(paused))
                }
                crate::player::PlayerEvent::Volume(volume) => {
                    Msg::Player(PlayerMsg::Volume(volume))
                }
                crate::player::PlayerEvent::Metadata(metadata) => {
                    Msg::Player(PlayerMsg::Metadata(metadata))
                }
                crate::player::PlayerEvent::CacheTime(t) => Msg::Player(PlayerMsg::CacheTime(t)),
                crate::player::PlayerEvent::AudioCodec(c) => Msg::Player(PlayerMsg::AudioCodec(c)),
                crate::player::PlayerEvent::FileFormat(f) => Msg::Player(PlayerMsg::FileFormat(f)),
                crate::player::PlayerEvent::Eof => Msg::Player(PlayerMsg::Eof),
                crate::player::PlayerEvent::Error(error) => Msg::Player(PlayerMsg::Error(error)),
            },
            RuntimeEvent::Persist(crate::persist::PersistEvent::WriteFailed { store, error }) => {
                Msg::PersistFailed { store, error }
            }
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::Command(cmd, reply)) => {
                Msg::Remote(cmd, reply)
            }
            RuntimeEvent::Video { generation, event } => {
                Msg::Player(PlayerMsg::VideoOverlay { generation, event })
            }
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::SessionSubscribe {
                ..
            }) => {
                // Session ops are intercepted in the run loop (the Publisher's owner
                // lane) before Msg conversion — the reducer never sees sessions
                // (docs/gui/02 §14). Reaching here means a host forgot the intercept.
                unreachable!("SessionSubscribe must be handled in the owner loop, not the reducer")
            }
            RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Resolved {
                video_id,
                stream_url,
            }) => {
                let video_id = video_id.into_string();
                let stream_url = stream_url.into_string();
                match crate::api::validate_playable_url(
                    crate::search_source::SearchSource::Youtube,
                    &stream_url,
                ) {
                    Ok(stream_url) => Msg::Streaming(StreamingMsg::Resolved {
                        video_id,
                        stream_url,
                    }),
                    Err(error) => {
                        tracing::warn!(%video_id, %error, "dropping invalid resolved stream URL");
                        Msg::ResolveFailed { video_id }
                    }
                }
            }
            RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Failed { video_id }) => {
                Msg::ResolveFailed {
                    video_id: video_id.into_string(),
                }
            }
            RuntimeEvent::Scrobble(event) => Msg::Scrobble(event),
            RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit) => Msg::Quit,
            RuntimeEvent::Tools(event) => Msg::Tools(event),
            RuntimeEvent::Update(crate::update::UpdateEvent::Checked(status)) => {
                Msg::UpdateChecked(status)
            }
            RuntimeEvent::Transfer(event) => Msg::Transfer(event),
            RuntimeEvent::TelemetryWake => {
                unreachable!(
                    "TelemetryWake must be drained by the owner loop before Msg conversion"
                )
            }
        }
    }
}

const RUNTIME_TELEMETRY_SLOTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RuntimeTelemetrySlot {
    Static(Key),
    DownloadProgress(String),
    MediaArt(String),
    TransferProgress(String),
}

#[derive(Clone)]
pub struct RuntimeSender {
    tx: Sender<RuntimeEvent>,
    telemetry: Arc<Mutex<LatestEventBuffer<RuntimeTelemetrySlot, RuntimeEvent>>>,
}

impl RuntimeSender {
    pub fn new(tx: Sender<RuntimeEvent>) -> Self {
        Self {
            tx,
            telemetry: Arc::new(Mutex::new(LatestEventBuffer::new(RUNTIME_TELEMETRY_SLOTS))),
        }
    }

    pub fn drain_coalesced(&self) -> Vec<RuntimeEvent> {
        self.telemetry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
    }
}

pub fn channel(
    policy: crate::util::backpressure::QueuePolicy,
) -> (RuntimeSender, Receiver<RuntimeEvent>) {
    let (tx, rx) = crate::util::backpressure::bounded_channel(policy);
    (RuntimeSender::new(tx), rx)
}

pub fn emit(tx: &RuntimeSender, event: RuntimeEvent) -> bool {
    let policy = event.policy();
    let event_kind = event.kind();
    if matches!(policy, EventPolicy::CoalesceLatest { .. }) {
        return emit_coalesced(tx, event, event_kind, policy);
    }
    emit_direct(&tx.tx, event, event_kind, policy)
}

fn emit_direct(
    tx: &Sender<RuntimeEvent>,
    event: RuntimeEvent,
    event_kind: &'static str,
    policy: EventPolicy,
) -> bool {
    match tx.try_send(event) {
        Ok(()) => true,
        Err(TrySendError::Full(event)) if matches!(policy, EventPolicy::MustDeliver { .. }) => {
            tracing::warn!(
                event_policy = policy.name(),
                event_lane = policy.lane().map(Lane::name).unwrap_or("none"),
                event_kind,
                coalesce_key = policy.key().map(Key::name).unwrap_or("none"),
                drop_reason = "must_deliver_delayed",
                "runtime owner event queue full; deferring must-deliver event"
            );
            defer_must_deliver(tx.clone(), event, event_kind, policy);
            true
        }
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                event_policy = policy.name(),
                event_lane = policy.lane().map(Lane::name).unwrap_or("none"),
                event_kind,
                coalesce_key = policy.key().map(Key::name).unwrap_or("none"),
                drop_reason = full_queue_reason(policy),
                "runtime owner event queue full; dropping event"
            );
            false
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

fn emit_coalesced(
    tx: &RuntimeSender,
    event: RuntimeEvent,
    event_kind: &'static str,
    policy: EventPolicy,
) -> bool {
    let Some(slot) = event.telemetry_slot() else {
        return emit_direct(&tx.tx, event, event_kind, policy);
    };
    let insert = tx
        .telemetry
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(slot, event);
    if insert.replaced_existing || insert.evicted_oldest {
        tracing::trace!(
            event_policy = policy.name(),
            event_lane = policy.lane().map(Lane::name).unwrap_or("none"),
            event_kind,
            coalesce_key = policy.key().map(Key::name).unwrap_or("none"),
            drop_reason = if insert.evicted_oldest {
                "coalesced_evicted_oldest"
            } else {
                "coalesced"
            },
            "runtime telemetry event coalesced"
        );
    }
    if insert.should_wake {
        emit_direct(
            &tx.tx,
            RuntimeEvent::TelemetryWake,
            RuntimeEvent::TelemetryWake.kind(),
            RuntimeEvent::TelemetryWake.policy(),
        )
    } else {
        true
    }
}

fn full_queue_reason(policy: EventPolicy) -> &'static str {
    match policy {
        EventPolicy::MustReplyOrBusy { .. } => "busy",
        EventPolicy::BestEffort { .. } => "dropped_best_effort",
        EventPolicy::DropIfStale { .. } => "stale_or_full",
        EventPolicy::CoalesceLatest { .. } => "coalesced_wake_full",
        EventPolicy::MustDeliver { .. } => "must_deliver_failed",
    }
}

fn defer_must_deliver(
    tx: Sender<RuntimeEvent>,
    event: RuntimeEvent,
    event_kind: &'static str,
    policy: EventPolicy,
) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            if tx.send(event).await.is_err() {
                tracing::error!(
                    event_policy = policy.name(),
                    event_lane = policy.lane().map(Lane::name).unwrap_or("none"),
                    event_kind,
                    drop_reason = "must_deliver_failed",
                    "runtime owner event queue closed before must-deliver event was accepted"
                );
            }
        });
    } else {
        std::thread::spawn(move || {
            if tx.blocking_send(event).is_err() {
                tracing::error!(
                    event_policy = policy.name(),
                    event_lane = policy.lane().map(Lane::name).unwrap_or("none"),
                    event_kind,
                    drop_reason = "must_deliver_failed",
                    "runtime owner event queue closed before must-deliver event was accepted"
                );
            }
        });
    }
}

pub fn sink<T, F>(tx: RuntimeSender, wrap: F) -> impl Fn(T) + Send + Sync + 'static
where
    T: 'static,
    F: Fn(T) -> RuntimeEvent + Send + Sync + 'static,
{
    move |event| {
        emit(&tx, wrap(event));
    }
}

pub fn remote_sink(
    tx: RuntimeSender,
) -> impl Fn(crate::remote::server::RemoteEvent) -> bool + Send + Sync + 'static {
    move |event| emit(&tx, RuntimeEvent::Remote(event))
}

pub struct RuntimeHandles {
    worker_tx: RuntimeSender,
    player_handle: Option<PlayerHandle>,
    pending_player_cmds: Vec<PlayerCmd>,
    player_failed: bool,
    _mpv_guard: Option<crate::player::Mpv>,
    /// Command sender for the *current* video overlay's IPC client. Replaced wholesale
    /// on every `Cmd::VideoConnect` (each spawn generation gets a fresh client); sends
    /// to a dead client are silent no-ops.
    video_handle: Option<Sender<crate::player::video::VideoCmd>>,
    api_handle: crate::api::ApiHandle,
    lyrics_handle: crate::lyrics::LyricsHandle,
    artwork_handle: crate::artwork::ArtworkHandle,
    download_handle: crate::download::DownloadHandle,
    resolver_handle: crate::resolver::ResolverHandle,
    ai_handle: Option<crate::ai::AiHandle>,
    scrobble_handle: crate::scrobble::ScrobbleHandle,
    /// Spawned on the first transfer command — costs nothing until the feature is used.
    transfer_handle: Option<crate::transfer::actor::TransferHandle>,
    /// Debounced background store writes (the `Cmd::Persist` family).
    persist: crate::persist::PersistHandle,
}

impl RuntimeHandles {
    #[allow(clippy::too_many_arguments)] // one-time construction in `run()`
    pub fn new(
        worker_tx: RuntimeSender,
        api_handle: crate::api::ApiHandle,
        lyrics_handle: crate::lyrics::LyricsHandle,
        artwork_handle: crate::artwork::ArtworkHandle,
        download_handle: crate::download::DownloadHandle,
        resolver_handle: crate::resolver::ResolverHandle,
        ai_handle: Option<crate::ai::AiHandle>,
        scrobble_handle: crate::scrobble::ScrobbleHandle,
        persist: crate::persist::PersistHandle,
    ) -> Self {
        Self {
            worker_tx,
            player_handle: None,
            pending_player_cmds: Vec::new(),
            player_failed: false,
            _mpv_guard: None,
            video_handle: None,
            api_handle,
            lyrics_handle,
            artwork_handle,
            download_handle,
            resolver_handle,
            ai_handle,
            scrobble_handle,
            transfer_handle: None,
            persist,
        }
    }

    /// Feed the scrobbler the same snapshot the loop is about to publish to the OS media
    /// session. Deliberately independent of that session's enabled state — scrobbling
    /// must survive `media_controls: false`.
    pub fn scrobble_observe(&mut self, snapshot: &crate::media::MediaSnapshot) {
        self.scrobble_handle.observe(snapshot);
    }

    /// Best-effort queue flush on quit, bounded by `budget`.
    pub async fn scrobble_shutdown(&self, budget: std::time::Duration) {
        let done = self.scrobble_handle.shutdown_flush();
        let _ = tokio::time::timeout(budget, done).await;
    }

    fn emit_api_enqueue_error(&self, msg: Msg) {
        emit(&self.worker_tx, RuntimeEvent::App(msg));
    }

    pub fn handle_player_ready(
        &mut self,
        result: Result<(PlayerHandle, crate::player::Mpv), String>,
        cfg: &PlayerRuntimeConfig,
        app: &mut App,
    ) {
        match result {
            Ok((handle, guard)) => {
                handle.send(PlayerCmd::SetVolume(cfg.volume));
                if (app.playback.speed - 1.0).abs() > f64::EPSILON {
                    handle.send(PlayerCmd::SetProperty {
                        name: "speed".to_owned(),
                        value: serde_json::Value::from(app.playback.speed),
                    });
                }
                if let Some(af) = crate::eq::build_af_string(&app.audio.bands, app.audio.normalize)
                {
                    handle.send(PlayerCmd::SetAudioFilter(af));
                }
                if let Ok(url) = std::env::var("YTM_PLAY_URL") {
                    handle.load(url);
                }
                for cmd in self.pending_player_cmds.drain(..) {
                    handle.send(cmd);
                }
                self.player_handle = Some(handle);
                self._mpv_guard = Some(guard);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to start mpv");
                self.player_failed = true;
                self.pending_player_cmds.clear();
                if app.status.text.is_empty() {
                    app.set_status_error(format!(
                        "{}: {e}",
                        crate::t!("mpv unavailable", "mpv를 사용할 수 없음")
                    ));
                }
            }
        }
    }

    pub fn dispatch(&mut self, app: &mut App, cmd: Cmd) {
        match cmd {
            Cmd::Player(pc) => {
                if let Some(p) = &self.player_handle {
                    p.send(pc);
                } else if !self.player_failed {
                    self.pending_player_cmds.push(pc);
                }
            }
            // dispatch runs synchronously right after each update, so the connect for a
            // spawn generation is always installed before any VideoLoad that follows it.
            Cmd::VideoConnect {
                ipc_path,
                generation,
            } => {
                let tx = self.worker_tx.clone();
                self.video_handle = Some(crate::player::video::connect(
                    ipc_path,
                    generation,
                    move |generation, event| {
                        emit(&tx, RuntimeEvent::Video { generation, event });
                    },
                ));
            }
            Cmd::VideoLoad(url) => {
                if let Some(v) = &self.video_handle
                    && v.try_send(crate::player::video::VideoCmd::Load(url))
                        .is_err()
                {
                    tracing::warn!("video overlay command queue full or closed; dropping load");
                }
            }
            Cmd::Search {
                request_id,
                query,
                source,
                config,
            } => {
                if let Err(error) = self.api_handle.search(request_id, query, source, config) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.emit_api_enqueue_error(Msg::SearchError {
                        request_id,
                        source,
                        error: error.to_string(),
                    });
                }
            }
            Cmd::SearchPlaylists { request_id, query } => {
                if let Err(error) = self.api_handle.search_playlists(request_id, query) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.emit_api_enqueue_error(Msg::SearchError {
                        request_id,
                        source: crate::search_source::SearchSource::Youtube,
                        error: error.to_string(),
                    });
                }
            }
            Cmd::FetchPlaylistTracks {
                playlist_id,
                title,
                intent,
            } => {
                if let Err(error) =
                    self.api_handle
                        .playlist_tracks(playlist_id, title.clone(), intent)
                {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.emit_api_enqueue_error(Msg::PlaylistTracksError {
                        title,
                        error: error.to_string(),
                    });
                }
            }
            // Persist: hand the persistence actor an owned snapshot (or clear one). Cloning a
            // store is a couple ms of memcpy at worst; the fsync it replaces on this task was
            // 5-50ms. The marker variants clone the live snapshot from `app` here; `Config`
            // carries its own owned snapshot.
            Cmd::Persist(p) => match p {
                PersistCmd::Library => self
                    .persist
                    .save(crate::persist::Snapshot::Library(app.library.clone())),
                PersistCmd::Downloads => self.persist.save(crate::persist::Snapshot::Downloads(
                    app.download_store.clone(),
                )),
                PersistCmd::Signals => self
                    .persist
                    .save(crate::persist::Snapshot::Signals(app.signals.clone())),
                PersistCmd::RomanizedTitles => self.persist.save(
                    crate::persist::Snapshot::RomanizedTitles(app.romanization.cache.clone()),
                ),
                PersistCmd::ClearRomanizedTitles => self.persist.delete_romanized_titles(),
                PersistCmd::Config(cfg) => self.persist.save(crate::persist::Snapshot::Config(cfg)),
                PersistCmd::Playlists => self
                    .persist
                    .save(crate::persist::Snapshot::Playlists(app.playlists.clone())),
                PersistCmd::StationProfile => self
                    .persist
                    .save(crate::persist::Snapshot::Station(app.station.clone())),
            },
            Cmd::ScanDownloads(dir) => {
                // Directory scan does per-file IO — keep it off the loop task too.
                let tx = self.worker_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let scan = crate::library::scan_downloads(&dir);
                    emit(&tx, RuntimeEvent::App(Msg::DownloadsScanned(scan)));
                });
            }
            Cmd::Local(cmd) => match cmd {
                crate::app::LocalCmd::LoadIndex { index_path } => {
                    let tx = self.worker_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let index = index_path
                            .as_deref()
                            .map(crate::local::LocalIndex::load)
                            .unwrap_or_default();
                        emit(
                            &tx,
                            RuntimeEvent::App(Msg::Local(crate::app::LocalMsg::IndexLoaded {
                                index_path,
                                index,
                            })),
                        );
                    });
                }
                crate::app::LocalCmd::ScanRoots {
                    roots,
                    index_path,
                    previous,
                } => {
                    let tx = self.worker_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut result = crate::local::scan_roots(&roots, &previous);
                        if let Some(path) = index_path.as_deref()
                            && let Err(error) = result.index.save(path)
                        {
                            result.errors.push(crate::local::ScanError {
                                path: path.to_path_buf(),
                                message: format!("could not save local index: {error}"),
                            });
                            result.summary.errors = result.errors.len();
                        }
                        emit(
                            &tx,
                            RuntimeEvent::App(Msg::Local(crate::app::LocalMsg::ScanFinished {
                                index_path,
                                result,
                            })),
                        );
                    });
                }
            },
            Cmd::Recorder(job) => {
                // Copy/tag/delete are blocking IO — keep them off the loop task. A `Save`
                // reports back; `Discard`/`WipeTemp` are fire-and-forget.
                let tx = self.worker_tx.clone();
                tokio::task::spawn_blocking(move || {
                    if let Some(event) = crate::recorder::job::run(job) {
                        emit(&tx, RuntimeEvent::App(Msg::Recorder(event)));
                    }
                });
            }
            Cmd::FetchLyrics {
                video_id,
                artist,
                title,
            } => {
                self.lyrics_handle.fetch(video_id, artist, title);
            }
            Cmd::FetchArtwork { video_id, source } => {
                self.artwork_handle.fetch(video_id, source);
            }
            Cmd::Download(song) => {
                if let Err(error) = self.download_handle.start(song) {
                    tracing::warn!(video_id = %error.video_id, "download queue full; dropping request");
                    emit(
                        &self.worker_tx,
                        RuntimeEvent::App(Msg::DownloadError {
                            video_id: error.video_id,
                            error: "Download queue is full; try again in a moment.".to_owned(),
                        }),
                    );
                }
            }
            Cmd::SetDownloadDir(dir) => {
                if let Err(error) = self.download_handle.set_dir(dir) {
                    tracing::warn!(dir = %error.dir().display(), %error, "could not update download directory");
                    emit(
                        &self.worker_tx,
                        RuntimeEvent::App(Msg::DownloadDirError {
                            error: error.to_string(),
                        }),
                    );
                }
            }
            Cmd::Resolve {
                video_id,
                watch_url,
            } => {
                self.resolver_handle.resolve_or_log(video_id, watch_url);
            }
            Cmd::YtdlpSelfHeal { video_id, tools } => {
                // Off-loop: an update check downloads up to ~40 MiB. Progress rides the
                // same Tools status-line events as the maintainer; the verdict returns
                // as Msg::YtdlpHealResult for the reducer's retry-or-skip decision.
                let tx = self.worker_tx.clone();
                tokio::spawn(async move {
                    let progress_tx = tx.clone();
                    crate::tools::ytdlp::clear_probe_cache();
                    let outcome = crate::tools::ytdlp::check_and_update(&tools, &move |event| {
                        emit(&progress_tx, RuntimeEvent::Tools(event));
                    })
                    .await;
                    let updated = matches!(
                        outcome,
                        crate::tools::ytdlp::UpdateOutcome::Installed { .. }
                    );
                    emit(
                        &tx,
                        RuntimeEvent::App(Msg::YtdlpHealResult { video_id, updated }),
                    );
                });
            }
            Cmd::AskAi { prompt, context } => {
                if let Some(h) = &self.ai_handle {
                    h.ask(prompt, context);
                }
            }
            Cmd::ResolveTrack { seq, query, config } => {
                if let Err(error) = self.api_handle.resolve_track(seq, query, config) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.emit_api_enqueue_error(Msg::TrackResolved {
                        seq,
                        result: Err(error.to_string()),
                    });
                }
            }
            Cmd::AiRerank {
                seed_video_id,
                prompt,
            } => {
                if let Some(h) = &self.ai_handle {
                    h.rerank(seed_video_id, prompt);
                }
            }
            Cmd::SummarizeFeedback { digest } => {
                if let Some(h) = &self.ai_handle {
                    h.summarize_feedback(digest);
                }
            }
            Cmd::RomanizeTitles { request_id, items } => {
                let keys: Vec<String> = items.iter().map(|item| item.key.clone()).collect();
                if let Some(h) = &self.ai_handle {
                    h.romanize(request_id, items);
                } else {
                    emit(
                        &self.worker_tx,
                        RuntimeEvent::App(Msg::Ai(AiMsg::RomanizedTitles {
                            request_id,
                            keys,
                            entries: Vec::new(),
                        })),
                    );
                }
            }
            Cmd::StreamingFallback {
                seed,
                seed_video_id,
                exclude_ids,
                mode,
                config,
            } => {
                if let Err(error) = self.api_handle.streaming(
                    seed,
                    seed_video_id.clone(),
                    exclude_ids,
                    crate::app::STREAMING_POOL_COUNT,
                    mode,
                    config,
                ) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.emit_api_enqueue_error(Msg::Streaming(StreamingMsg::Error {
                        seed_video_id,
                        error: error.to_string(),
                    }));
                }
            }
            Cmd::StreamingPreflight {
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => {
                if let Err(error) = self.api_handle.streaming_preflight(
                    seed_video_id.clone(),
                    picks,
                    fallback,
                    mode,
                    config,
                ) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.emit_api_enqueue_error(Msg::Streaming(StreamingMsg::Error {
                        seed_video_id,
                        error: error.to_string(),
                    }));
                }
            }
            Cmd::SetAiModel(model) => {
                if let Some(h) = &self.ai_handle {
                    h.set_model(model);
                }
            }
            Cmd::ReloadAi {
                key,
                model,
                assistant_enabled,
            } => {
                self.ai_handle = key.and_then(|k| {
                    crate::ai::spawn(&k, model, sink(self.worker_tx.clone(), RuntimeEvent::Ai))
                });
                app.ai.available = assistant_enabled && self.ai_handle.is_some();
            }
            Cmd::ScrobbleAuthStart => self.scrobble_handle.auth_start(),
            Cmd::ScrobbleReconfigure(settings) => self.scrobble_handle.reconfigure(*settings),
            Cmd::Transfer(cmd) => {
                let handle = self.transfer_handle.get_or_insert_with(|| {
                    crate::transfer::actor::spawn(sink(
                        self.worker_tx.clone(),
                        RuntimeEvent::Transfer,
                    ))
                });
                handle.send(cmd);
            }
            // Handled in the main loop (the OSC path writes to the terminal this scope doesn't
            // own); never reaches here. Listed for exhaustiveness.
            Cmd::DesktopNotify { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::RemoteCommand;
    use crate::util::event_policy::{EventKey, EventLane, EventPolicy};

    fn song(video_id: &str) -> crate::api::Song {
        crate::api::Song::from_search(
            video_id,
            format!("Title {video_id}"),
            "Artist",
            "3:21",
            Some("Album".to_owned()),
        )
    }

    fn transfer_progress(job_id: &str) -> crate::transfer::TransferProgress {
        crate::transfer::TransferProgress {
            job_id: job_id.to_owned(),
            stage: crate::transfer::Stage::Matching,
            done: 1,
            total: 2,
            matched: 1,
            ambiguous: 0,
            not_found: 0,
            current: "Artist - Title".to_owned(),
        }
    }

    fn update_status() -> crate::update::UpdateStatus {
        crate::update::UpdateStatus {
            current: "1.0.0".to_owned(),
            latest: "v1.0.1".to_owned(),
            available: true,
            first_seen: true,
            method: crate::update::InstallMethod::Cargo,
        }
    }

    fn assert_policy(event: RuntimeEvent, expected: EventPolicy) {
        assert_eq!(event.policy(), expected);
    }

    #[test]
    fn runtime_event_policy_covers_representative_events() {
        assert_eq!(
            RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit).policy(),
            EventPolicy::MustDeliver {
                lane: EventLane::Control
            }
        );
        assert_eq!(
            RuntimeEvent::Player(crate::player::PlayerEvent::Eof).policy(),
            EventPolicy::MustDeliver {
                lane: EventLane::Control
            }
        );
        assert_eq!(
            RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(12.0)).policy(),
            EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::PlayerTimePos
            }
        );

        let (reply, _rx) = tokio::sync::oneshot::channel();
        assert_eq!(
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::Command(
                RemoteCommand::TogglePause,
                reply,
            ))
            .policy(),
            EventPolicy::MustReplyOrBusy {
                lane: EventLane::RemoteCommand
            }
        );

        assert_eq!(
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id: "v".to_owned(),
                percent: 50.0,
            })
            .policy(),
            EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::DownloadProgress
            }
        );
        assert_eq!(
            RuntimeEvent::Download(crate::download::DownloadEvent::Done {
                video_id: "v".to_owned(),
                path: "song.m4a".to_owned(),
            })
            .policy(),
            EventPolicy::MustDeliver {
                lane: EventLane::WorkResult
            }
        );
        assert_eq!(
            RuntimeEvent::Api(crate::api::ApiEvent::StreamingError {
                seed_video_id: "seed".to_owned(),
                error: "nope".to_owned(),
            })
            .policy(),
            EventPolicy::DropIfStale {
                stale_key: EventKey::StreamingSeed
            }
        );
        assert!(matches!(
            RuntimeEvent::Scrobble(crate::scrobble::ScrobbleEvent::QueueStalled { pending: 1 })
                .policy(),
            EventPolicy::BestEffort { .. }
        ));
    }

    #[test]
    fn runtime_event_policy_covers_leaf_event_classes() {
        use crate::api::{ApiEvent, ApiMode, PlaylistIntent};
        use crate::search_source::SearchSource;

        assert_policy(
            RuntimeEvent::Ai(crate::ai::AiEvent::Thinking(true)),
            EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::AiThinking,
            },
        );
        assert_policy(
            RuntimeEvent::Ai(crate::ai::AiEvent::StreamingPicks {
                seed_video_id: "seed".to_owned(),
                picks: Vec::new(),
                conf: None,
            }),
            EventPolicy::DropIfStale {
                stale_key: EventKey::StreamingSeed,
            },
        );
        assert_policy(
            RuntimeEvent::Ai(crate::ai::AiEvent::Chat("ok".to_owned())),
            EventPolicy::MustDeliver {
                lane: EventLane::WorkResult,
            },
        );

        assert_policy(
            RuntimeEvent::Api(ApiEvent::ModeResolved {
                mode: ApiMode::Anonymous,
                had_cookie: false,
            }),
            EventPolicy::MustDeliver {
                lane: EventLane::WorkResult,
            },
        );
        assert_policy(
            RuntimeEvent::Api(ApiEvent::SearchResults {
                request_id: 7,
                query: "q".to_owned(),
                source: SearchSource::Youtube,
                songs: Vec::new(),
                timed_out: false,
            }),
            EventPolicy::DropIfStale {
                stale_key: EventKey::SearchRequest,
            },
        );
        assert_policy(
            RuntimeEvent::Api(ApiEvent::PlaylistTracks {
                title: "mix".to_owned(),
                intent: PlaylistIntent::Play,
                songs: Vec::new(),
            }),
            EventPolicy::MustReplyOrBusy {
                lane: EventLane::WorkResult,
            },
        );
        assert_policy(
            RuntimeEvent::Api(ApiEvent::GuiSearchCompleted {
                ticket: 9,
                query: "q".to_owned(),
                source: SearchSource::All,
                groups: Vec::new(),
            }),
            EventPolicy::DropIfStale {
                stale_key: EventKey::GuiSearchTicket,
            },
        );

        assert_policy(
            RuntimeEvent::Artwork(crate::artwork::ArtworkEvent::Result {
                video_id: "v".to_owned(),
                image: None,
            }),
            EventPolicy::DropIfStale {
                stale_key: EventKey::ArtworkVideo,
            },
        );
        assert_policy(
            RuntimeEvent::Lyrics(crate::lyrics::LyricsEvent::Result {
                video_id: "v".to_owned(),
                lines: Vec::new(),
            }),
            EventPolicy::DropIfStale {
                stale_key: EventKey::LyricsVideo,
            },
        );
        assert_policy(
            RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Failed {
                video_id: crate::ids::VideoId::from("v"),
            }),
            EventPolicy::DropIfStale {
                stale_key: EventKey::ResolverVideo,
            },
        );
        assert_policy(
            RuntimeEvent::Video {
                generation: 1,
                event: crate::player::video::VideoEvent::Next,
            },
            EventPolicy::DropIfStale {
                stale_key: EventKey::VideoOverlayGeneration,
            },
        );
        assert_policy(
            RuntimeEvent::Tools(crate::tools::ToolsEvent::Progress {
                channel: crate::tools::YtdlpChannel::Nightly,
                percent: Some(20),
            }),
            EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::ToolProgress,
            },
        );
        assert_policy(
            RuntimeEvent::Tools(crate::tools::ToolsEvent::Failed {
                error: "offline".to_owned(),
            }),
            EventPolicy::MustDeliver {
                lane: EventLane::WorkResult,
            },
        );
        assert_policy(
            RuntimeEvent::Update(crate::update::UpdateEvent::Checked(update_status())),
            EventPolicy::CoalesceLatest {
                lane: EventLane::WorkResult,
                key: EventKey::UpdateCheck,
            },
        );
        assert_policy(
            RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::Progress(
                transfer_progress("job"),
            )),
            EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::TransferJob,
            },
        );
        assert_policy(
            RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::JobFailed {
                job_id: "job".to_owned(),
                error: "failed".to_owned(),
                resumable: true,
            }),
            EventPolicy::MustDeliver {
                lane: EventLane::WorkResult,
            },
        );
        assert_policy(
            RuntimeEvent::TelemetryWake,
            EventPolicy::MustDeliver {
                lane: EventLane::Control,
            },
        );
    }

    #[test]
    fn runtime_event_kind_and_telemetry_slots_are_stable() {
        assert_eq!(
            RuntimeEvent::Ai(crate::ai::AiEvent::Chat("hi".to_owned())).kind(),
            "ai"
        );
        assert_eq!(
            RuntimeEvent::Api(crate::api::ApiEvent::StreamingError {
                seed_video_id: "seed".to_owned(),
                error: "e".to_owned(),
            })
            .kind(),
            "api"
        );
        assert_eq!(
            RuntimeEvent::Download(crate::download::DownloadEvent::Done {
                video_id: "v".to_owned(),
                path: "v.m4a".to_owned(),
            })
            .kind(),
            "download"
        );
        assert_eq!(
            RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(1.0)).kind(),
            "player"
        );
        assert_eq!(
            RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit).kind(),
            "signal"
        );
        assert_eq!(RuntimeEvent::TelemetryWake.kind(), "telemetry_wake");
        assert!(RuntimeEvent::TelemetryWake.is_telemetry_wake());
        assert!(!RuntimeEvent::Player(crate::player::PlayerEvent::Eof).is_telemetry_wake());

        assert_eq!(
            RuntimeEvent::App(Msg::DownloadProgress {
                video_id: "a".to_owned(),
                percent: 1.0,
            })
            .telemetry_slot(),
            Some(RuntimeTelemetrySlot::DownloadProgress("a".to_owned()))
        );
        assert_eq!(
            RuntimeEvent::App(Msg::MediaArtworkReady(
                crate::media::artwork::MediaArtworkReady {
                    key: "cover-key".to_owned(),
                    path: "cover.jpg".into(),
                },
            ))
            .telemetry_slot(),
            Some(RuntimeTelemetrySlot::MediaArt("cover-key".to_owned()))
        );
        assert_eq!(
            RuntimeEvent::Transfer(crate::transfer::actor::TransferEvent::Progress(
                transfer_progress("import-1"),
            ))
            .telemetry_slot(),
            Some(RuntimeTelemetrySlot::TransferProgress(
                "import-1".to_owned()
            ))
        );
        assert_eq!(
            RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(1.0)).telemetry_slot(),
            Some(RuntimeTelemetrySlot::Static(EventKey::PlayerTimePos))
        );
        assert_eq!(
            RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit).telemetry_slot(),
            None
        );
    }

    #[test]
    fn app_message_policy_covers_backpressure_lanes() {
        let (reply, _reply_rx) = tokio::sync::oneshot::channel();
        assert_eq!(
            app_msg_policy(&Msg::Remote(RemoteCommand::TogglePause, reply)),
            EventPolicy::MustReplyOrBusy {
                lane: EventLane::RemoteCommand,
            }
        );
        assert_eq!(
            app_msg_policy(&Msg::DownloadProgress {
                video_id: "v".to_owned(),
                percent: 12.0,
            }),
            EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::DownloadProgress,
            }
        );
        assert_eq!(
            app_msg_policy(&Msg::MediaArtworkReady(
                crate::media::artwork::MediaArtworkReady {
                    key: "v".to_owned(),
                    path: "cover.jpg".into(),
                },
            )),
            EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::MediaArtVideo,
            }
        );
        assert_eq!(
            app_msg_policy(&Msg::SearchError {
                request_id: 1,
                source: crate::search_source::SearchSource::Youtube,
                error: "offline".to_owned(),
            }),
            EventPolicy::DropIfStale {
                stale_key: EventKey::SearchRequest,
            }
        );
        assert_eq!(
            app_msg_policy(&Msg::TrackResolved {
                seq: 1,
                result: Ok(Vec::new()),
            }),
            EventPolicy::DropIfStale {
                stale_key: EventKey::ResolverVideo,
            }
        );
        assert_eq!(
            app_msg_policy(&Msg::Streaming(StreamingMsg::Error {
                seed_video_id: "seed".to_owned(),
                error: "empty".to_owned(),
            })),
            EventPolicy::DropIfStale {
                stale_key: EventKey::StreamingSeed,
            }
        );
        assert!(matches!(
            app_msg_policy(&Msg::Noop),
            EventPolicy::BestEffort { .. }
        ));
        assert_eq!(
            app_msg_policy(&Msg::Media(crate::media::MediaCommand::Play)),
            EventPolicy::MustDeliver {
                lane: EventLane::Control,
            }
        );
        assert_eq!(
            app_msg_policy(&Msg::Transfer(
                crate::transfer::actor::TransferEvent::Disconnected,
            )),
            EventPolicy::MustDeliver {
                lane: EventLane::WorkResult,
            }
        );
    }

    #[test]
    fn player_message_policy_covers_each_property_lane() {
        assert_eq!(
            app_player_msg_policy(&PlayerMsg::TimePos(1.0)),
            EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::PlayerTimePos,
            }
        );
        assert_eq!(
            app_player_msg_policy(&PlayerMsg::Metadata(serde_json::json!({"title":"Song"}))),
            EventPolicy::CoalesceLatest {
                lane: EventLane::WorkResult,
                key: EventKey::PlayerMetadata,
            }
        );
        assert_eq!(
            app_player_msg_policy(&PlayerMsg::AudioCodec(Some("aac".to_owned()))),
            EventPolicy::CoalesceLatest {
                lane: EventLane::Telemetry,
                key: EventKey::PlayerAudioCodec,
            }
        );
        assert_eq!(
            app_player_msg_policy(&PlayerMsg::VideoOverlay {
                generation: 2,
                event: crate::player::video::VideoEvent::Closed,
            }),
            EventPolicy::DropIfStale {
                stale_key: EventKey::VideoOverlayGeneration,
            }
        );
        assert_eq!(
            app_player_msg_policy(&PlayerMsg::Error("boom".to_owned())),
            EventPolicy::MustDeliver {
                lane: EventLane::Control,
            }
        );
    }

    #[test]
    fn runtime_event_to_msg_preserves_ai_api_and_transport_payloads() {
        let msg = Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::SetStationProfile {
            query: "city pop".to_owned(),
            explore: Some("wide".to_owned()),
            avoid_artists: vec!["skip".to_owned()],
        }));
        assert!(matches!(
            msg,
            Msg::Ai(AiMsg::SetStationProfile {
                query,
                explore: Some(explore),
                avoid_artists,
            }) if query == "city pop" && explore == "wide" && avoid_artists == ["skip"]
        ));

        let msg = Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::StreamingPicks {
            seed_video_id: "seed".to_owned(),
            picks: vec![crate::app::AiPick {
                cid: "c1".to_owned(),
                role: Some("bridge".to_owned()),
                reasons: vec!["tr".to_owned()],
            }],
            conf: Some(0.75),
        }));
        assert!(matches!(
            msg,
            Msg::Streaming(StreamingMsg::AiPicks {
                seed_video_id,
                picks,
                conf: Some(conf),
            }) if seed_video_id == "seed" && picks[0].cid == "c1" && (conf - 0.75).abs() < f32::EPSILON
        ));

        let msg = Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::SearchResults {
            request_id: 3,
            query: "query".to_owned(),
            source: crate::search_source::SearchSource::SoundCloud,
            songs: vec![song("v1")],
            timed_out: true,
        }));
        assert!(matches!(
            msg,
            Msg::SearchResults {
                request_id: 3,
                query,
                source: crate::search_source::SearchSource::SoundCloud,
                songs,
                timed_out: true,
            } if query == "query" && songs[0].video_id == "v1"
        ));

        let msg = Msg::from(RuntimeEvent::Api(
            crate::api::ApiEvent::PlaylistTracksError {
                title: "mix".to_owned(),
                error: "denied".to_owned(),
            },
        ));
        assert!(matches!(
            msg,
            Msg::PlaylistTracksError { title, error } if title == "mix" && error == "denied"
        ));

        let msg = Msg::from(RuntimeEvent::Download(
            crate::download::DownloadEvent::Error {
                video_id: "v2".to_owned(),
                error: "disk".to_owned(),
            },
        ));
        assert!(matches!(
            msg,
            Msg::DownloadError { video_id, error } if video_id == "v2" && error == "disk"
        ));

        let msg = Msg::from(RuntimeEvent::Player(
            crate::player::PlayerEvent::FileFormat(Some("mp4".to_owned())),
        ));
        assert!(matches!(
            msg,
            Msg::Player(PlayerMsg::FileFormat(Some(format))) if format == "mp4"
        ));

        let (reply, _reply_rx) = tokio::sync::oneshot::channel();
        let msg = Msg::from(RuntimeEvent::Remote(
            crate::remote::server::RemoteEvent::Command(RemoteCommand::Next, reply),
        ));
        assert!(matches!(msg, Msg::Remote(RemoteCommand::Next, _)));

        let msg = Msg::from(RuntimeEvent::Video {
            generation: 42,
            event: crate::player::video::VideoEvent::Failed("403".to_owned()),
        });
        assert!(matches!(
            msg,
            Msg::Player(PlayerMsg::VideoOverlay {
                generation: 42,
                event: crate::player::video::VideoEvent::Failed(error),
            }) if error == "403"
        ));
    }

    #[test]
    fn runtime_event_to_msg_validates_resolver_urls_and_side_channels() {
        let msg = Msg::from(RuntimeEvent::Resolver(
            crate::resolver::ResolverEvent::Resolved {
                video_id: crate::ids::VideoId::from("v1"),
                stream_url: crate::ids::StreamUrl::from("https://rr1---sn.test/video.m4a"),
            },
        ));
        assert!(matches!(
            msg,
            Msg::Streaming(StreamingMsg::Resolved {
                video_id,
                stream_url,
            }) if video_id == "v1" && stream_url.starts_with("https://")
        ));

        let msg = Msg::from(RuntimeEvent::Resolver(
            crate::resolver::ResolverEvent::Resolved {
                video_id: crate::ids::VideoId::from("v2"),
                stream_url: crate::ids::StreamUrl::from("file:///etc/passwd"),
            },
        ));
        assert!(matches!(msg, Msg::ResolveFailed { video_id } if video_id == "v2"));

        let msg = Msg::from(RuntimeEvent::Api(
            crate::api::ApiEvent::GuiSearchCompleted {
                ticket: 1,
                query: "ignored".to_owned(),
                source: crate::search_source::SearchSource::All,
                groups: Vec::new(),
            },
        ));
        assert!(matches!(msg, Msg::Noop));

        let msg = Msg::from(RuntimeEvent::Signal(
            crate::player::lifetime::SignalEvent::Quit,
        ));
        assert!(matches!(msg, Msg::Quit));

        let msg = Msg::from(RuntimeEvent::Update(crate::update::UpdateEvent::Checked(
            update_status(),
        )));
        assert!(matches!(
            msg,
            Msg::UpdateChecked(status) if status.latest == "v1.0.1" && status.available
        ));

        let msg = Msg::from(RuntimeEvent::Transfer(
            crate::transfer::actor::TransferEvent::Progress(transfer_progress("job-2")),
        ));
        assert!(matches!(
            msg,
            Msg::Transfer(crate::transfer::actor::TransferEvent::Progress(progress))
                if progress.job_id == "job-2"
        ));
    }

    #[test]
    fn runtime_event_to_msg_preserves_ai_payload_variants() {
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Thinking(true))),
            Msg::Ai(AiMsg::Thinking(true))
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Error(
                "no key".to_owned()
            ))),
            Msg::Ai(AiMsg::Error(error)) if error == "no key"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::PlayTracks(vec![song(
                "play1234567"
            )]))),
            Msg::Ai(AiMsg::PlayTracks(songs)) if songs[0].video_id == "play1234567"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Enqueue(vec![song(
                "queue123456"
            )]))),
            Msg::Ai(AiMsg::Enqueue(songs)) if songs[0].video_id == "queue123456"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::Suggestions(vec![
                song("suggest1234")
            ]))),
            Msg::Ai(AiMsg::Suggestions(songs)) if songs[0].video_id == "suggest1234"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::SetAutoplay(false))),
            Msg::Ai(AiMsg::SetAutoplay(false))
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::CreatePlaylist(
                "Road".to_owned()
            ))),
            Msg::Ai(AiMsg::CreatePlaylist(name)) if name == "Road"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::AddToPlaylist {
                playlist: "Road".to_owned(),
                songs: vec![song("add12345678")],
            })),
            Msg::Ai(AiMsg::AddToPlaylist { playlist, songs })
                if playlist == "Road" && songs[0].video_id == "add12345678"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::PlayPlaylist(
                "Favorites".to_owned()
            ))),
            Msg::Ai(AiMsg::PlayPlaylist(key)) if key == "Favorites"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::StationPatch {
                down_artists: vec!["Skip".to_owned()],
                boost_artists: vec!["Boost".to_owned()],
            })),
            Msg::Ai(AiMsg::StationPatch {
                down_artists,
                boost_artists,
            }) if down_artists == ["Skip"] && boost_artists == ["Boost"]
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Ai(crate::ai::AiEvent::RomanizedTitles {
                request_id: 77,
                keys: vec!["k1".to_owned()],
                entries: vec![crate::romanize::RomanizedResult {
                    key: "k1".to_owned(),
                    title: "Title".to_owned(),
                    artist: "Artist".to_owned(),
                    confidence: Some(0.9),
                }],
            })),
            Msg::Ai(AiMsg::RomanizedTitles {
                request_id: 77,
                keys,
                entries,
            }) if keys == ["k1"] && entries[0].title == "Title"
        ));
    }

    #[test]
    fn runtime_event_to_msg_preserves_api_player_and_service_payloads() {
        assert!(matches!(
            Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::ModeResolved {
                mode: crate::api::ApiMode::Authenticated,
                had_cookie: true,
            })),
            Msg::ApiModeResolved {
                mode: crate::api::ApiMode::Authenticated,
                had_cookie: true,
            }
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::TrackResolved {
                seq: 12,
                result: Err("not found".to_owned()),
            })),
            Msg::TrackResolved {
                seq: 12,
                result: Err(error),
            } if error == "not found"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::PlaylistTracks {
                title: "Mix".to_owned(),
                intent: crate::api::PlaylistIntent::Enqueue,
                songs: vec![song("plist123456")],
            })),
            Msg::PlaylistTracks {
                title,
                intent: crate::api::PlaylistIntent::Enqueue,
                songs,
            } if title == "Mix" && songs[0].video_id == "plist123456"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::StreamingResults {
                seed_video_id: "seed".to_owned(),
                candidates: vec![(song("cand1234567"), crate::streaming::CandidateSource::YtdlpStreaming)],
            })),
            Msg::Streaming(StreamingMsg::Results {
                seed_video_id,
                candidates,
            }) if seed_video_id == "seed"
                && candidates[0].0.video_id == "cand1234567"
                && candidates[0].1 == crate::streaming::CandidateSource::YtdlpStreaming
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Api(crate::api::ApiEvent::StreamingPreflighted {
                seed_video_id: "seed".to_owned(),
                songs: vec![song("pre12345678")],
            })),
            Msg::Streaming(StreamingMsg::Preflighted { seed_video_id, songs })
                if seed_video_id == "seed" && songs[0].video_id == "pre12345678"
        ));

        for (event, assert_msg) in [
            (crate::player::PlayerEvent::Duration(Some(88.0)), "duration"),
            (crate::player::PlayerEvent::Paused(true), "paused"),
            (crate::player::PlayerEvent::Volume(42.0), "volume"),
            (crate::player::PlayerEvent::CacheTime(Some(91.0)), "cache"),
            (
                crate::player::PlayerEvent::AudioCodec(Some("aac".to_owned())),
                "codec",
            ),
            (crate::player::PlayerEvent::Eof, "eof"),
            (
                crate::player::PlayerEvent::Error("decode".to_owned()),
                "error",
            ),
        ] {
            let msg = Msg::from(RuntimeEvent::Player(event));
            match assert_msg {
                "duration" => assert!(matches!(
                    msg,
                    Msg::Player(PlayerMsg::Duration(Some(d))) if (d - 88.0).abs() < f64::EPSILON
                )),
                "paused" => assert!(matches!(msg, Msg::Player(PlayerMsg::Paused(true)))),
                "volume" => assert!(matches!(
                    msg,
                    Msg::Player(PlayerMsg::Volume(v)) if (v - 42.0).abs() < f64::EPSILON
                )),
                "cache" => assert!(matches!(
                    msg,
                    Msg::Player(PlayerMsg::CacheTime(Some(t))) if (t - 91.0).abs() < f64::EPSILON
                )),
                "codec" => assert!(matches!(
                    msg,
                    Msg::Player(PlayerMsg::AudioCodec(Some(codec))) if codec == "aac"
                )),
                "eof" => assert!(matches!(msg, Msg::Player(PlayerMsg::Eof))),
                "error" => assert!(matches!(
                    msg,
                    Msg::Player(PlayerMsg::Error(error)) if error == "decode"
                )),
                _ => unreachable!(),
            }
        }

        assert!(matches!(
            Msg::from(RuntimeEvent::Scrobble(
                crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 4 },
            )),
            Msg::Scrobble(crate::scrobble::ScrobbleEvent::QueueDropped { dropped: 4 })
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Tools(crate::tools::ToolsEvent::Installed {
                version: "2026.07.07".to_owned(),
            })),
            Msg::Tools(crate::tools::ToolsEvent::Installed { version })
                if version == "2026.07.07"
        ));
        assert!(matches!(
            Msg::from(RuntimeEvent::Transfer(
                crate::transfer::actor::TransferEvent::AuthDone {
                    display_name: "Tester".to_owned(),
                },
            )),
            Msg::Transfer(crate::transfer::actor::TransferEvent::AuthDone { display_name })
                if display_name == "Tester"
        ));
    }

    #[tokio::test]
    async fn must_deliver_runtime_event_waits_when_owner_lane_is_full() {
        let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
        let tx = RuntimeSender::new(raw_tx.clone());
        assert!(
            raw_tx
                .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
                    1.0
                )))
                .is_ok()
        );

        assert!(emit(
            &tx,
            RuntimeEvent::Signal(crate::player::lifetime::SignalEvent::Quit)
        ));
        assert!(matches!(
            rx.recv().await,
            Some(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(_)))
        ));
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await,
            Ok(Some(RuntimeEvent::Signal(
                crate::player::lifetime::SignalEvent::Quit
            )))
        ));
    }

    #[test]
    fn remote_runtime_event_reports_full_to_callers() {
        let (raw_tx, _rx) = tokio::sync::mpsc::channel(1);
        let tx = RuntimeSender::new(raw_tx.clone());
        assert!(
            raw_tx
                .try_send(RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(
                    1.0
                )))
                .is_ok()
        );
        let (reply, _reply_rx) = tokio::sync::oneshot::channel();

        assert!(!emit(
            &tx,
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::Command(
                RemoteCommand::TogglePause,
                reply,
            ))
        ));
    }

    #[test]
    fn runtime_telemetry_coalesces_time_pos_to_one_wake() {
        let (raw_tx, mut rx) = tokio::sync::mpsc::channel(1);
        let tx = RuntimeSender::new(raw_tx);

        for tick in 0..10_000 {
            assert!(emit(
                &tx,
                RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(tick as f64))
            ));
        }

        assert!(matches!(rx.try_recv(), Ok(RuntimeEvent::TelemetryWake)));
        assert!(rx.try_recv().is_err());
        let drained = tx.drain_coalesced();
        assert_eq!(drained.len(), 1);
        assert!(matches!(
            &drained[0],
            RuntimeEvent::Player(crate::player::PlayerEvent::TimePos(t)) if (*t - 9999.0).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn runtime_download_progress_coalesces_without_displacing_final_event() {
        let (raw_tx, mut rx) = tokio::sync::mpsc::channel(4);
        let tx = RuntimeSender::new(raw_tx);

        assert!(emit(
            &tx,
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id: "a".to_owned(),
                percent: 10.0,
            })
        ));
        assert!(emit(
            &tx,
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id: "a".to_owned(),
                percent: 70.0,
            })
        ));
        assert!(emit(
            &tx,
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id: "b".to_owned(),
                percent: 40.0,
            })
        ));
        assert!(emit(
            &tx,
            RuntimeEvent::Download(crate::download::DownloadEvent::Done {
                video_id: "a".to_owned(),
                path: "a.m4a".to_owned(),
            })
        ));

        assert!(matches!(rx.try_recv(), Ok(RuntimeEvent::TelemetryWake)));
        assert!(matches!(
            rx.try_recv(),
            Ok(RuntimeEvent::Download(crate::download::DownloadEvent::Done {
                video_id,
                ..
            })) if video_id == "a"
        ));
        let drained = tx.drain_coalesced();
        assert_eq!(drained.len(), 2);
        assert!(drained.iter().any(|event| matches!(
            event,
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id,
                percent,
            }) if video_id == "a" && (*percent - 70.0).abs() < f64::EPSILON
        )));
        assert!(drained.iter().any(|event| matches!(
            event,
            RuntimeEvent::Download(crate::download::DownloadEvent::Progress {
                video_id,
                percent,
            }) if video_id == "b" && (*percent - 40.0).abs() < f64::EPSILON
        )));
    }
}
