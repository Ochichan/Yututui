//! Runtime event adapter between leaf actors and the app reducer.
//!
//! Actors emit domain-specific events so they do not depend on `crate::app::Msg`.
//! This module is the single orchestration boundary that maps those events back into
//! reducer messages.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ratatui_image::thread::ResizeResponse;
use tokio::sync::mpsc::{Receiver, Sender, error::TrySendError};

use crate::app::{AiMsg, App, Cmd, Msg, PersistCmd, PlayerMsg, StreamingMsg};
use crate::config::PlayerRuntimeConfig;
use crate::player::{PlayerCmd, PlayerHandle};
use crate::util::event_policy::{
    EventKey as Key, EventLane as Lane, EventPolicy, LatestEventBuffer,
};

mod must_deliver;

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
            RuntimeEvent::Video { event, .. } => video_event_policy(event),
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
        Msg::Local(crate::app::LocalMsg::ScanProgress(_)) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::LocalScanProgress,
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
        PlayerMsg::VideoOverlay { event, .. } => video_event_policy(event),
    }
}

fn video_event_policy(event: &crate::player::video::VideoEvent) -> EventPolicy {
    match event {
        crate::player::video::VideoEvent::Paused(_) => EventPolicy::CoalesceLatest {
            lane: Lane::Telemetry,
            key: Key::VideoOverlayPaused,
        },
        _ => EventPolicy::MustDeliver {
            lane: Lane::Control,
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
    must_deliver_overflow: Arc<must_deliver::MustDeliverOverflow>,
}

impl RuntimeSender {
    pub fn new(tx: Sender<RuntimeEvent>) -> Self {
        Self {
            tx,
            telemetry: Arc::new(Mutex::new(LatestEventBuffer::new(RUNTIME_TELEMETRY_SLOTS))),
            must_deliver_overflow: Arc::new(must_deliver::MustDeliverOverflow::new()),
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
    emit_direct(tx, event, event_kind, policy)
}

fn emit_direct(
    tx: &RuntimeSender,
    event: RuntimeEvent,
    event_kind: &'static str,
    policy: EventPolicy,
) -> bool {
    match tx.tx.try_send(event) {
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
            tx.must_deliver_overflow
                .push(tx.tx.clone(), event, event_kind, policy);
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
        return emit_direct(tx, event, event_kind, policy);
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
            tx,
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

const PENDING_PLAYER_CMDS_MAX: usize = 64;

#[derive(Default)]
struct PendingPlayerCmds {
    cmds: VecDeque<PlayerCmd>,
}

impl PendingPlayerCmds {
    fn push(&mut self, cmd: PlayerCmd) {
        match &cmd {
            PlayerCmd::Load(_) => {
                self.cmds
                    .retain(|existing| !matches!(existing, PlayerCmd::Load(_)));
            }
            PlayerCmd::SetVolume(_) => {
                self.cmds
                    .retain(|existing| !matches!(existing, PlayerCmd::SetVolume(_)));
            }
            PlayerCmd::SetAudioFilter(_) => {
                self.cmds
                    .retain(|existing| !matches!(existing, PlayerCmd::SetAudioFilter(_)));
            }
            PlayerCmd::SetProperty { name, .. } => {
                self.cmds.retain(|existing| {
                    !matches!(existing, PlayerCmd::SetProperty { name: existing_name, .. } if existing_name == name)
                });
            }
            PlayerCmd::Stop
            | PlayerCmd::CyclePause
            | PlayerCmd::SeekRelative(_)
            | PlayerCmd::SeekAbsolute(_)
            | PlayerCmd::AfCommand { .. } => {}
        }
        self.cmds.push_back(cmd);
        while self.cmds.len() > PENDING_PLAYER_CMDS_MAX {
            let idx = self
                .cmds
                .iter()
                .position(|cmd| !matches!(cmd, PlayerCmd::Load(_)))
                .unwrap_or(0);
            if let Some(dropped) = self.cmds.remove(idx) {
                tracing::warn!(
                    kind = player_cmd_kind(&dropped),
                    "pending player command buffer full; dropping oldest queued command"
                );
            } else {
                break;
            }
        }
    }

    fn drain(&mut self) -> Vec<PlayerCmd> {
        self.cmds.drain(..).collect()
    }

    fn clear(&mut self) {
        self.cmds.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.cmds.len()
    }
}

fn player_cmd_kind(cmd: &PlayerCmd) -> &'static str {
    match cmd {
        PlayerCmd::Load(_) => "load",
        PlayerCmd::Stop => "stop",
        PlayerCmd::CyclePause => "cycle_pause",
        PlayerCmd::SeekRelative(_) => "seek_relative",
        PlayerCmd::SeekAbsolute(_) => "seek_absolute",
        PlayerCmd::SetVolume(_) => "set_volume",
        PlayerCmd::SetAudioFilter(_) => "set_audio_filter",
        PlayerCmd::AfCommand { .. } => "af_command",
        PlayerCmd::SetProperty { .. } => "set_property",
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
    pending_player_cmds: PendingPlayerCmds,
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
            pending_player_cmds: PendingPlayerCmds::default(),
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

    pub fn scrobble_heartbeat_due(&self) -> bool {
        self.scrobble_handle.heartbeat_due()
    }

    /// Best-effort queue flush on quit, bounded by `budget`.
    pub async fn scrobble_shutdown(&self, budget: std::time::Duration) {
        let done = self.scrobble_handle.shutdown_flush();
        let _ = tokio::time::timeout(budget, done).await;
    }

    fn emit_api_enqueue_error(&self, msg: Msg) {
        emit(&self.worker_tx, RuntimeEvent::App(msg));
    }

    fn send_video_cmd(&self, cmd: crate::player::video::VideoCmd, label: &'static str) {
        let Some(video) = &self.video_handle else {
            tracing::warn!(%label, "video overlay command requested with no IPC client");
            return;
        };
        if video.try_send(cmd).is_err() {
            tracing::warn!(%label, "video overlay command queue full or closed; dropping command");
        }
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
                for cmd in self.pending_player_cmds.drain() {
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
                bindings,
            } => {
                let tx = self.worker_tx.clone();
                self.video_handle = Some(crate::player::video::connect(
                    ipc_path,
                    generation,
                    bindings,
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
            Cmd::VideoTogglePause => {
                self.send_video_cmd(crate::player::video::VideoCmd::CyclePause, "pause");
            }
            Cmd::VideoToggleFullscreen => {
                self.send_video_cmd(
                    crate::player::video::VideoCmd::CycleFullscreen,
                    "fullscreen",
                );
            }
            Cmd::VideoToggleMute => {
                self.send_video_cmd(crate::player::video::VideoCmd::CycleMute, "mute");
            }
            Cmd::UpdateSeen { tag } => crate::update::mark_notified(&tag),
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
                tokio::spawn(async move {
                    if let Err(error) = crate::util::blocking::spawn_io(move || {
                        let scan = crate::library::scan_downloads(&dir);
                        emit(&tx, RuntimeEvent::App(Msg::DownloadsScanned(scan)));
                    })
                    .await
                    {
                        tracing::warn!(%error, "download scan task failed");
                    }
                });
            }
            Cmd::Local(cmd) => match cmd {
                crate::app::LocalCmd::LoadIndex { index_path } => {
                    let tx = self.worker_tx.clone();
                    tokio::spawn(async move {
                        if let Err(error) = crate::util::blocking::spawn_io(move || {
                            let load = index_path
                                .as_deref()
                                .map(crate::local::LocalIndex::load_with_diagnostics)
                                .unwrap_or_default();
                            let warnings = load
                                .warnings
                                .into_iter()
                                .map(|warning| crate::local::ScanError {
                                    path: warning.path,
                                    message: warning.message,
                                })
                                .collect();
                            emit(
                                &tx,
                                RuntimeEvent::App(Msg::Local(crate::app::LocalMsg::IndexLoaded {
                                    index_path,
                                    index: load.index,
                                    warnings,
                                })),
                            );
                        })
                        .await
                        {
                            tracing::warn!(%error, "local index load task failed");
                        }
                    });
                }
                crate::app::LocalCmd::ScanRoots {
                    roots,
                    index_path,
                    previous,
                } => {
                    let tx = self.worker_tx.clone();
                    tokio::spawn(async move {
                        if let Err(error) = crate::util::blocking::spawn_io(move || {
                            let progress_tx = tx.clone();
                            let mut result = crate::local::scan_roots_with_progress(
                                &roots,
                                &previous,
                                |progress| {
                                    emit(
                                        &progress_tx,
                                        RuntimeEvent::App(Msg::Local(
                                            crate::app::LocalMsg::ScanProgress(progress),
                                        )),
                                    );
                                },
                            );
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
                        })
                        .await
                        {
                            tracing::warn!(%error, "local root scan task failed");
                        }
                    });
                }
            },
            Cmd::Recorder(job) => {
                // Copy/tag/delete are blocking IO — keep them off the loop task. A `Save`
                // reports back; `Discard`/`WipeTemp` are fire-and-forget.
                let tx = self.worker_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = crate::util::blocking::spawn_io(move || {
                        if let Some(event) = crate::recorder::job::run(job) {
                            emit(&tx, RuntimeEvent::App(Msg::Recorder(event)));
                        }
                    })
                    .await
                    {
                        tracing::warn!(%error, "recorder file task failed");
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
                let result = if let Some(request) = crate::download::import_request_for_song(&song)
                {
                    self.download_handle.start_for_import(request)
                } else {
                    self.download_handle.start(*song)
                };
                if let Err(error) = result {
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
                    let outcome = crate::tools::ytdlp::rollback_or_check_and_update(
                        &tools,
                        &move |event| {
                            emit(&progress_tx, RuntimeEvent::Tools(event));
                        },
                        "playback self-heal",
                    )
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
mod tests;
