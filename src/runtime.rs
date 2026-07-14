//! Runtime event adapter between leaf actors and the app reducer.
//!
//! Leaf domain events cross this single orchestration boundary into reducer messages.

use ratatui_image::thread::ResizeResponse;

#[cfg(test)]
use crate::app::PersistCmd;
use crate::app::{
    AiMsg, App, Cmd, DataCmd, DownloadCmd, Msg, PersonalDataExportCmd, PlayerControl, PlayerMsg,
    ScrobbleCmd, StreamingMsg,
};
#[cfg(test)]
use crate::player::PlayerCmd;
use crate::player::PlayerHandle;
use crate::util::delivery::{DeliveryReceipt, DeliveryResult};
use crate::util::event_policy::{EventKey as Key, EventLane as Lane, EventPolicy};

mod delivery_reporting;
mod event_policy;
pub(crate) mod ingress;
mod persist_delivery;
pub(crate) mod player_delivery;
mod read_only;
mod recorder_recovery;
mod task_set;

#[cfg(test)]
use crate::util::delivery::DeliveryError;
use delivery_reporting::{ActorRejectionRecovery, recover_actor_rejection, report_actor_delivery};
#[cfg(test)]
use event_policy::player_msg_policy as app_player_msg_policy;
use event_policy::{app_msg_policy, player_event_policy, video_event_policy};
#[cfg(test)]
use player_delivery::{
    PENDING_PLAYER_CMDS_MAX, PENDING_PLAYER_INTENTS_MAX, PlayerRestartDecision,
    admit_player_intent, reject_pending_player_intents, settle_player_intent,
};
use player_delivery::{
    PendingPlayerCmds, PendingPlayerIntents, PlayerRestartGate, report_player_delivery,
};
use task_set::RuntimeTaskSet;

pub use ingress::{
    RuntimeSender, channel, emit, emit_callback_observed, emit_callback_result, emit_observed,
};
pub use task_set::BackgroundShutdown;

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
                | crate::api::ApiEvent::PlaylistTracksError { .. } => EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
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
            RuntimeEvent::Artwork(_) => EventPolicy::DropIfStale {
                stale_key: Key::ArtworkVideo,
            },
            RuntimeEvent::ArtworkResized(_) => EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::ArtResize,
            },
            RuntimeEvent::Download(event) => match event {
                crate::download::DownloadEvent::Progress { .. }
                | crate::download::DownloadEvent::ImportProgress { .. } => {
                    EventPolicy::CoalesceLatest {
                        lane: Lane::Telemetry,
                        key: Key::DownloadProgress,
                    }
                }
                crate::download::DownloadEvent::Done { .. }
                | crate::download::DownloadEvent::ImportDone { .. }
                | crate::download::DownloadEvent::Error { .. }
                | crate::download::DownloadEvent::ImportError { .. } => EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                },
            },
            RuntimeEvent::Lyrics(_) => EventPolicy::DropIfStale {
                stale_key: Key::LyricsVideo,
            },
            RuntimeEvent::Player(event) => player_event_policy(event.unscoped()),
            RuntimeEvent::Persist(_) => EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            },
            RuntimeEvent::Remote(
                crate::remote::server::RemoteEvent::Command(_, _)
                | crate::remote::server::RemoteEvent::SessionCommand { .. }
                | crate::remote::server::RemoteEvent::SessionSubscribe { .. },
            ) => EventPolicy::MustReplyOrBusy {
                lane: Lane::RemoteCommand,
            },
            RuntimeEvent::Video { event, .. } => video_event_policy(event),
            RuntimeEvent::Resolver(
                crate::resolver::ResolverEvent::Resolved { purpose, .. }
                | crate::resolver::ResolverEvent::Failed { purpose, .. },
            ) if *purpose == crate::resolver::ResolvePurpose::SelfHeal => {
                EventPolicy::MustDeliver {
                    lane: Lane::WorkResult,
                }
            }
            RuntimeEvent::Resolver(_) => EventPolicy::CoalesceLatest {
                lane: Lane::WorkResult,
                key: Key::ResolverVideo,
            },
            RuntimeEvent::Scrobble(event) => match event {
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
                | crate::transfer::actor::TransferEvent::JobRejected { .. }
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
            RuntimeEvent::Artwork(crate::artwork::ArtworkEvent::Result {
                video_id,
                quality,
                image,
            }) => Msg::ArtworkResult {
                video_id,
                quality,
                image,
            },
            RuntimeEvent::ArtworkResized(response) => Msg::ArtworkResized(response),
            RuntimeEvent::Download(event) => match event {
                crate::download::DownloadEvent::Progress { video_id, percent } => {
                    Msg::Download(crate::app::DownloadMsg::Progress { video_id, percent })
                }
                crate::download::DownloadEvent::ImportProgress { context, percent } => {
                    Msg::Download(crate::app::DownloadMsg::ImportProgress { context, percent })
                }
                crate::download::DownloadEvent::Done { video_id, path } => {
                    Msg::Download(crate::app::DownloadMsg::Done { video_id, path })
                }
                crate::download::DownloadEvent::ImportDone { context, path } => {
                    Msg::Download(crate::app::DownloadMsg::ImportDone { context, path })
                }
                crate::download::DownloadEvent::Error { video_id, error } => {
                    Msg::Download(crate::app::DownloadMsg::Error { video_id, error })
                }
                crate::download::DownloadEvent::ImportError { context, error } => {
                    Msg::Download(crate::app::DownloadMsg::ImportError { context, error })
                }
            },
            RuntimeEvent::Lyrics(crate::lyrics::LyricsEvent::Result { video_id, lines }) => {
                Msg::LyricsResult { video_id, lines }
            }
            RuntimeEvent::Player(event) => match event.into_unscoped() {
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
                crate::player::PlayerEvent::AudioDeviceList(devices) => {
                    Msg::Player(PlayerMsg::AudioDeviceList(devices))
                }
                crate::player::PlayerEvent::AudioDeviceRefreshFailed(error) => {
                    Msg::Player(PlayerMsg::AudioDeviceRefreshFailed(error))
                }
                crate::player::PlayerEvent::AudioDeviceChanged(device) => {
                    Msg::Player(PlayerMsg::AudioDeviceChanged(device))
                }
                crate::player::PlayerEvent::CurrentAudioOutput(output) => {
                    Msg::Player(PlayerMsg::CurrentAudioOutput(output))
                }
                crate::player::PlayerEvent::AudioDeviceSelectionResult {
                    correlation_id,
                    device,
                    result,
                } => Msg::Player(PlayerMsg::AudioDeviceSelectionResult {
                    correlation_id,
                    device,
                    result,
                }),
                crate::player::PlayerEvent::Eof => Msg::Player(PlayerMsg::Eof),
                crate::player::PlayerEvent::Error(error) => Msg::Player(PlayerMsg::Error(error)),
                crate::player::PlayerEvent::TransportClosed(reason) => {
                    Msg::Player(PlayerMsg::TransportClosed(reason))
                }
                crate::player::PlayerEvent::CacheEmergency {
                    file_generation: _,
                    position_secs,
                    paused,
                    reason,
                } => Msg::Player(PlayerMsg::CacheEmergency {
                    position_secs,
                    paused,
                    reason,
                }),
                crate::player::PlayerEvent::CacheReplacementEmergency { reason } => {
                    Msg::Player(PlayerMsg::CacheReplacementEmergency { reason })
                }
                crate::player::PlayerEvent::FileScoped { .. } => {
                    unreachable!("audio file event was unscoped before conversion")
                }
            },
            RuntimeEvent::Persist(crate::persist::PersistEvent::WriteFailed { store, error }) => {
                Msg::PersistFailed { store, error }
            }
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::Command(cmd, reply)) => {
                Msg::Remote(cmd, reply)
            }
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::SessionCommand {
                command,
                reply,
                ..
            }) => Msg::Remote(command, reply),
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
                purpose,
            }) => {
                let video_id = video_id.into_string();
                let stream_url = stream_url.into_string();
                let self_heal = purpose == crate::resolver::ResolvePurpose::SelfHeal;
                match crate::api::validate_playable_url(
                    crate::search_source::SearchSource::Youtube,
                    &stream_url,
                ) {
                    Ok(stream_url) => Msg::Streaming(StreamingMsg::Resolved {
                        video_id,
                        stream_url,
                        self_heal,
                    }),
                    Err(error) => {
                        tracing::warn!(%video_id, %error, "dropping invalid resolved stream URL");
                        if self_heal {
                            Msg::ResolveFailed { video_id }
                        } else {
                            Msg::Noop
                        }
                    }
                }
            }
            RuntimeEvent::Resolver(crate::resolver::ResolverEvent::Failed {
                video_id,
                purpose,
            }) => {
                if purpose == crate::resolver::ResolvePurpose::SelfHeal {
                    Msg::ResolveFailed {
                        video_id: video_id.into_string(),
                    }
                } else {
                    Msg::Noop
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

fn settle_resolver_admission(app: &mut App, video_id: String, result: DeliveryResult) -> Vec<Cmd> {
    match result {
        Ok(DeliveryReceipt::Coalesced { .. }) => {
            tracing::trace!(%video_id, "resolver request coalesced");
            Vec::new()
        }
        Ok(DeliveryReceipt::Enqueued | DeliveryReceipt::Deferred) => Vec::new(),
        Err(error) => {
            tracing::debug!(%video_id, %error, "resolver request was not accepted");
            // Dispatch already owns `&mut App`, so reduce the failure directly. Sending it
            // back through the bounded owner ingress could itself saturate or evict another
            // keyed completion and leave a self-heal retry latch stuck forever.
            app.update(Msg::ResolveFailed { video_id })
        }
    }
}

fn recover_download_admission(
    app: &mut App,
    error: crate::download::DownloadStartError,
) -> Vec<Cmd> {
    // Reduce directly so saturated owner ingress cannot strand `downloads.dispatched`.
    let message = "Download queue is full; try again in a moment.".to_owned();
    if let Some(context) = error.import_context
        && let Err(record_error) =
            crate::transfer::session::record_import_download_error(&context.claim, &message)
    {
        tracing::warn!(error = %record_error, "could not record rejected import download");
    }
    app.update(Msg::Download(crate::app::DownloadMsg::Rejected {
        tracking_key: error.tracking_key,
        error: message,
    }))
}

pub fn sink<T, F>(tx: RuntimeSender, wrap: F) -> impl Fn(T) + Send + Sync + 'static
where
    T: 'static,
    F: Fn(T) -> RuntimeEvent + Send + Sync + 'static,
{
    move |event| {
        emit_callback_observed(&tx, wrap(event));
    }
}

pub fn remote_sink(
    tx: RuntimeSender,
) -> impl Fn(crate::remote::server::RemoteEvent) -> bool + Send + Sync + 'static {
    move |event| emit(&tx, RuntimeEvent::Remote(event)).is_ok()
}

pub struct RuntimeHandles {
    worker_tx: RuntimeSender,
    player_handle: Option<PlayerHandle>,
    /// Runtime-owned restore commands are kept apart from user intents so restore is always
    /// admitted first when a replacement actor becomes ready.
    pending_player_cmds: PendingPlayerCmds,
    pending_player_intents: PendingPlayerIntents,
    player_failed: bool,
    player_restart: PlayerRestartGate,
    _mpv_guard: Option<crate::player::Mpv>,
    /// Command sender for the *current* video overlay's IPC client. Replaced wholesale
    /// on every `Cmd::VideoConnect` (each spawn generation gets a fresh client); rejected
    /// sends are surfaced through the common player-delivery status path.
    video_handle: Option<crate::player::video::VideoHandle>,
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
    /// Runtime-local blocking jobs and cancellable maintenance work.
    background_tasks: RuntimeTaskSet,
    /// A deliberate secondary player may use playback/network actors, but every command capable
    /// of durable mutation is rejected before it reaches an actor or blocking worker.
    persistence_read_only: Option<std::sync::Arc<str>>,
}

fn settle_video_load_delivery(app: &mut App, result: DeliveryResult) -> Vec<Cmd> {
    report_player_delivery(app, "video_load", result);
    if result.is_err() {
        app.compensate_video_load_rejection()
    } else {
        Vec::new()
    }
}

/// Return the durable-mutation reason which is authoritative *now*.
///
/// `persistence_read_only` captures the writer-lease decision made at process startup. Recovery
/// can still discover an unverifiable journal later (for example when the lazily-spawned transfer
/// actor loads config), so every durable admission also consults the typed process latch. The
/// lower-level `safe_fs` revoke remains the final race barrier if recovery fails after admission.
fn durable_mutation_rejection_reason(
    persistence_read_only: Option<&std::sync::Arc<str>>,
) -> Option<String> {
    let recovery_reason = crate::persist::ensure_startup_recovery_coherent()
        .err()
        .map(|error| error.to_string());
    persistence_read_only
        .map(|reason| reason.to_string())
        .or(recovery_reason)
}

fn shutdown_event_is_retired(event: &RuntimeEvent) -> bool {
    matches!(
        event,
        RuntimeEvent::TelemetryWake | RuntimeEvent::Signal(_) | RuntimeEvent::Player(_)
    )
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
            pending_player_intents: PendingPlayerIntents::default(),
            player_failed: false,
            player_restart: PlayerRestartGate::default(),
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
            background_tasks: RuntimeTaskSet::new(),
            persistence_read_only: match crate::persist::persistence_access() {
                crate::persist::PersistenceAccess::Writable => None,
                crate::persist::PersistenceAccess::ReadOnly { reason } => Some(reason),
            },
        }
    }

    /// Feed the scrobbler the same snapshot the loop is about to publish to the OS media
    /// session. Deliberately independent of that session's enabled state — scrobbling
    /// must survive `media_controls: false`.
    pub fn scrobble_observe(
        &mut self,
        snapshot: &crate::media::MediaSnapshot,
    ) -> crate::util::delivery::DeliveryResult {
        self.scrobble_handle.observe(snapshot)
    }

    pub fn scrobble_heartbeat_due(&self) -> bool {
        self.scrobble_handle.heartbeat_due()
    }

    pub fn scrobble_retry_needed(&self) -> bool {
        self.scrobble_handle.retry_needed()
    }

    /// Cancel and join every accepted yt-dlp task before the terminal runtime exits.
    pub async fn download_shutdown(&self, budget: std::time::Duration) {
        match tokio::time::timeout(budget, self.download_handle.shutdown()).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!("download actor stopped before confirming shutdown"),
            Err(_) => tracing::warn!("download shutdown timed out"),
        }
    }

    /// Close resolver admission and join the actor which owns every active yt-dlp resolve.
    pub async fn resolver_shutdown(&mut self, budget: std::time::Duration) {
        match tokio::time::timeout(budget, self.resolver_handle.shutdown()).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!("resolver actor stopped before confirming shutdown"),
            Err(_) => tracing::warn!("resolver shutdown timed out"),
        }
    }

    /// Close background admission, abort cancellable work, and wait boundedly for real blocking
    /// jobs. A timeout keeps the blocking joins owned so a later call can observe their completion.
    pub async fn background_shutdown(&mut self, budget: std::time::Duration) -> BackgroundShutdown {
        let outcome = self.background_tasks.shutdown(budget).await;
        match outcome {
            BackgroundShutdown::Drained => {
                tracing::debug!("runtime background tasks drained during shutdown")
            }
            BackgroundShutdown::TimedOut {
                blocking_remaining,
                cancellable_remaining,
            } => tracing::warn!(
                blocking_remaining,
                cancellable_remaining,
                "runtime background task shutdown timed out"
            ),
        }
        outcome
    }

    /// Stop new owner-event producers without closing the receiver. This releases callback retry
    /// loops while preserving every already accepted main/deferred/coalesced event for teardown.
    pub fn close_event_ingress(&self) -> bool {
        self.worker_tx.close_admission()
    }

    pub fn background_ingress_is_idle(&self) -> bool {
        self.worker_tx.deferred_is_idle()
    }

    pub fn drain_background_coalesced(&self) -> Vec<RuntimeEvent> {
        self.worker_tx.drain_coalesced()
    }

    /// Final ownership barrier for non-abortable runtime jobs. Unlike the diagnostic shutdown
    /// windows, this deliberately has no timeout.
    pub async fn finalize_background(&mut self) -> Vec<RuntimeEvent> {
        self.background_tasks.finalize().await
    }

    /// Apply one event accepted before producer shutdown (or retained in the shutdown outbox).
    /// Remote request lifetimes are settled explicitly; state completions use the ordinary
    /// reducer/dispatcher so persistence follow-ups cross the same durable actor boundary. Player
    /// events are retired because teardown has already ended that generation and suppresses every
    /// follow-up player command; applying a queued EOF here would corrupt the final queue/session.
    pub fn reduce_shutdown_event(&mut self, app: &mut App, event: RuntimeEvent) {
        if shutdown_event_is_retired(&event) {
            return;
        }
        match event {
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::Command(_, reply))
            | RuntimeEvent::Remote(crate::remote::server::RemoteEvent::SessionCommand {
                reply,
                ..
            }) => {
                let _ = reply.send(crate::remote::proto::RemoteResponse::err("shutting_down"));
            }
            RuntimeEvent::Remote(crate::remote::server::RemoteEvent::SessionSubscribe {
                ..
            }) => {
                tracing::debug!("retired queued session subscribe during owner shutdown");
            }
            event => self.reduce_owner_msg(app, event.into()),
        }
    }

    /// Abort and join any active transfer/auth/playlist work before the owner runtime exits.
    pub async fn transfer_shutdown(&mut self, budget: std::time::Duration) {
        let Some(handle) = self.transfer_handle.as_mut() else {
            return;
        };
        match tokio::time::timeout(budget, handle.shutdown()).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!("transfer actor stopped before confirming shutdown"),
            Err(_) => tracing::warn!("transfer shutdown timed out"),
        }
        self.transfer_handle = None;
    }

    /// Confirm the durable scrobble frontier and join its isolated owner thread. `budget` is a
    /// diagnostic warning deadline, not permission to cancel an accepted append/fsync.
    pub async fn scrobble_shutdown(
        &mut self,
        budget: std::time::Duration,
    ) -> Result<(), crate::util::delivery::DeliveryError> {
        self.scrobble_handle.shutdown_and_join(budget).await
    }

    /// Settle a synchronous actor rejection on the owner which already holds `App`.
    /// Re-enqueueing this terminal message through the same bounded ingress could reject it a
    /// second time and leave request flags such as `searching` or `thinking` stuck forever.
    fn reduce_owner_msg(&mut self, app: &mut App, msg: Msg) {
        for follow_up in app.update(msg) {
            self.dispatch(app, follow_up);
        }
    }

    fn send_video_cmd(
        &self,
        cmd: crate::player::video::VideoCmd,
        label: &'static str,
    ) -> DeliveryResult {
        match &self.video_handle {
            Some(video) => video.send(cmd),
            None => {
                tracing::warn!(%label, "video overlay command requested with no IPC client");
                Err(crate::util::delivery::DeliveryError::Closed)
            }
        }
    }

    pub fn dispatch(&mut self, app: &mut App, cmd: Cmd) {
        self.background_tasks.reap_finished();
        if let Some(component) = read_only::durable_mutation_component(&cmd) {
            let reason = durable_mutation_rejection_reason(self.persistence_read_only.as_ref());
            if let Some(reason) = reason {
                for follow_up in read_only::reject_mutation(app, &cmd, component, &reason) {
                    self.dispatch(app, follow_up);
                }
                return;
            }
        }
        match cmd {
            Cmd::PlayerControl(PlayerControl::Restart { restore }) => {
                let restart_started = self.handle_player_transport_closed(app);
                if restart_started && !restore.is_empty() {
                    let result = self.admit_player_restore_batch(restore);
                    report_player_delivery(app, "transport_restore", result);
                }
            }
            Cmd::PlayerControl(PlayerControl::Intent(intent)) => {
                self.dispatch_player_intent(app, intent);
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
                        emit_callback_observed(&tx, RuntimeEvent::Video { generation, event });
                    },
                ));
            }
            Cmd::VideoLoad(url) => {
                let result =
                    self.send_video_cmd(crate::player::video::VideoCmd::Load(url), "video_load");
                if result.is_err() {
                    // Drop the rejected generation before closing its process so no stale
                    // pending load can later reach an overlay which no longer represents state.
                    self.video_handle = None;
                }
                for follow_up in settle_video_load_delivery(app, result) {
                    self.dispatch(app, follow_up);
                }
            }
            Cmd::VideoTogglePause => {
                let result =
                    self.send_video_cmd(crate::player::video::VideoCmd::CyclePause, "video_pause");
                report_player_delivery(app, "video_pause", result);
            }
            Cmd::VideoToggleFullscreen => {
                let result = self.send_video_cmd(
                    crate::player::video::VideoCmd::CycleFullscreen,
                    "video_fullscreen",
                );
                report_player_delivery(app, "video_fullscreen", result);
            }
            Cmd::VideoToggleMute => {
                let result =
                    self.send_video_cmd(crate::player::video::VideoCmd::CycleMute, "video_mute");
                report_player_delivery(app, "video_mute", result);
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
                    self.reduce_owner_msg(
                        app,
                        Msg::SearchError {
                            request_id,
                            source,
                            error: error.to_string(),
                        },
                    );
                }
            }
            Cmd::SearchPlaylists { request_id, query } => {
                if let Err(error) = self.api_handle.search_playlists(request_id, query) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::SearchError {
                            request_id,
                            source: crate::search_source::SearchSource::Youtube,
                            error: error.to_string(),
                        },
                    );
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
                    self.reduce_owner_msg(
                        app,
                        Msg::PlaylistTracksError {
                            title,
                            error: error.to_string(),
                        },
                    );
                }
            }
            // Persist: hand the persistence actor an owned snapshot (or clear one). Cloning a
            // store is a couple ms of memcpy at worst; the fsync it replaces on this task was
            // 5-50ms. The marker variants clone the live snapshot from `app` here; `Config`
            // carries its own owned snapshot.
            Cmd::Persist(p) => {
                let result = persist_delivery::admit(&self.persist, app, p);
                report_actor_delivery(app, "persistence", result);
            }
            Cmd::Data(cmd) => match cmd {
                DataCmd::PersonalDataExport(PersonalDataExportCmd::Export {
                    directory,
                    sources,
                    reply,
                }) => {
                    let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                    self.background_tasks
                        .spawn_blocking("personal_data_export", move || {
                            let snapshot = crate::data_export::ExportSnapshot::new(
                                &sources.config,
                                &sources.library,
                                &sources.playlists,
                                &sources.signals,
                                &sources.station,
                            );
                            drop(sources);
                            let result = crate::data_export::export_snapshot(&directory, &snapshot)
                                .map_err(|error| {
                                    crate::util::sanitize::sanitize_error_text(error.to_string())
                                });
                            emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Data(
                                crate::app::DataMsg::PersonalDataExport(
                                    crate::app::PersonalDataExportMsg::Finished { result, reply },
                                ),
                            )));
                        });
                }
                DataCmd::ScanDownloads(dir) => {
                    let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                    self.background_tasks
                        .spawn_blocking("scan_downloads_data", move || {
                            let scan = crate::library::scan_downloads(&dir);
                            emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Data(
                                crate::app::DataMsg::DownloadsScanned(scan),
                            )));
                        });
                }
            },
            Cmd::Download(DownloadCmd::Scan(dir)) => {
                // Directory scan does per-file IO — keep it off the loop task too.
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("scan_downloads", move || {
                        let scan = crate::library::scan_downloads(&dir);
                        emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Data(
                            crate::app::DataMsg::DownloadsScanned(scan),
                        )));
                    });
            }
            Cmd::Download(DownloadCmd::Delete { paths, root }) => {
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_blocking("delete_downloads", move || {
                        let (deleted, failures) =
                            crate::download::delete_download_files(paths, &root);
                        for (path, error) in &failures {
                            tracing::warn!(
                                path = %crate::util::sanitize::sanitize_error_text(path.display().to_string()),
                                error = %crate::util::sanitize::sanitize_error_text(error.to_string()),
                                "refused or failed to delete downloaded file"
                            );
                        }
                        emitter.emit_terminal_blocking(RuntimeEvent::App(
                            Msg::DownloadsDeleted {
                                root,
                                deleted,
                                failed: failures.len(),
                            },
                        ));
                    });
            }
            Cmd::Local(cmd) => match cmd {
                crate::app::LocalCmd::LoadIndex { index_path } => {
                    let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                    self.background_tasks
                        .spawn_blocking("local_load_index", move || {
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
                            emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                                crate::app::LocalMsg::IndexLoaded {
                                    index_path,
                                    index: load.index,
                                    warnings,
                                },
                            )));
                        });
                }
                crate::app::LocalCmd::ScanRoots {
                    roots,
                    index_path,
                    previous,
                } => {
                    let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                    self.background_tasks
                        .spawn_blocking("local_scan_roots", move || {
                            let progress_emitter = emitter.clone();
                            let mut result = crate::local::scan_roots_with_progress(
                                &roots,
                                &previous,
                                |progress| {
                                    progress_emitter.emit(RuntimeEvent::App(Msg::Local(
                                        crate::app::LocalMsg::ScanProgress(progress),
                                    )));
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
                            emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                                crate::app::LocalMsg::ScanFinished { index_path, result },
                            )));
                        });
                }
                crate::app::LocalCmd::ReviewImport {
                    op_id,
                    session_id,
                    source_order,
                    action,
                } => {
                    let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                    self.background_tasks
                        .spawn_blocking("review_import", move || {
                            let t0 = std::time::Instant::now();
                            let result = match action {
                                crate::app::ImportReviewAction::AcceptFirst => {
                                    crate::transfer::review_action::accept_first_candidate(
                                        &session_id,
                                        source_order,
                                    )
                                }
                                crate::app::ImportReviewAction::ChooseNext => {
                                    crate::transfer::review_action::choose_next_candidate(
                                        &session_id,
                                        source_order,
                                    )
                                }
                                crate::app::ImportReviewAction::Reject => {
                                    crate::transfer::review_action::reject_row(
                                        &session_id,
                                        source_order,
                                    )
                                }
                                crate::app::ImportReviewAction::Skip => {
                                    crate::transfer::review_action::skip_row(
                                        &session_id,
                                        source_order,
                                    )
                                }
                            }
                            .map_err(|error| format!("{error:#}"));
                            let elapsed_ms = t0.elapsed().as_millis();
                            tracing::debug!(
                                session_id = %session_id,
                                source_order,
                                ?action,
                                elapsed_ms,
                                "finished import review action"
                            );
                            emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                                crate::app::LocalMsg::ImportReviewFinished {
                                    op_id,
                                    session_id,
                                    source_order,
                                    action,
                                    result,
                                    elapsed_ms,
                                },
                            )));
                        });
                }
                crate::app::LocalCmd::ReviewImportAcceptAll { op_id, session_id } => {
                    let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                    self.background_tasks
                        .spawn_blocking("review_import_accept_all", move || {
                            let t0 = std::time::Instant::now();
                            let result =
                                crate::transfer::review_action::accept_all_candidates(&session_id)
                                    .map_err(|error| format!("{error:#}"));
                            let elapsed_ms = t0.elapsed().as_millis();
                            tracing::debug!(
                                session_id = %session_id,
                                elapsed_ms,
                                "finished import review accept all"
                            );
                            emitter.emit_terminal_blocking(RuntimeEvent::App(Msg::Local(
                                crate::app::LocalMsg::ImportReviewAcceptAllFinished {
                                    op_id,
                                    session_id,
                                    result,
                                    elapsed_ms,
                                },
                            )));
                        });
                }
            },
            Cmd::Recorder(job) => {
                self.dispatch_recorder(app, job);
            }
            Cmd::FetchLyrics {
                video_id,
                artist,
                title,
            } => {
                if !report_actor_delivery(
                    app,
                    "lyrics",
                    self.lyrics_handle.fetch(video_id, artist, title),
                ) {
                    recover_actor_rejection(app, ActorRejectionRecovery::Lyrics);
                }
            }
            Cmd::FetchArtwork { video_id, source } => {
                if !report_actor_delivery(
                    app,
                    "artwork",
                    self.artwork_handle.fetch(video_id, source),
                ) {
                    recover_actor_rejection(app, ActorRejectionRecovery::Artwork);
                }
            }
            Cmd::Download(DownloadCmd::Start(song)) => {
                let import_metadata_present =
                    song.import_session_id.is_some() || song.import_source_order.is_some();
                let result = match crate::download::import_request_for_song(&song) {
                    Ok(Some(request)) => Some(self.download_handle.start_for_import(request)),
                    Ok(None) if import_metadata_present => {
                        let follow_ups =
                            app.update(Msg::Download(crate::app::DownloadMsg::Rejected {
                                tracking_key: crate::download::download_tracking_key(&song),
                                error: "Import session row is unavailable; refresh and retry."
                                    .to_owned(),
                            }));
                        for follow_up in follow_ups {
                            self.dispatch(app, follow_up);
                        }
                        None
                    }
                    Err(error) if import_metadata_present => {
                        tracing::warn!(%error, "import download admission failed");
                        let follow_ups =
                            app.update(Msg::Download(crate::app::DownloadMsg::Rejected {
                                tracking_key: crate::download::download_tracking_key(&song),
                                error: format!("Import download was not admitted: {error:#}"),
                            }));
                        for follow_up in follow_ups {
                            self.dispatch(app, follow_up);
                        }
                        None
                    }
                    Ok(None) => Some(self.download_handle.start(*song)),
                    Err(error) => {
                        tracing::warn!(%error, "ordinary download metadata admission failed");
                        Some(self.download_handle.start(*song))
                    }
                };
                if let Some(Err(error)) = result {
                    tracing::warn!(video_id = %error.video_id, "download request rejected; surfacing retry status");
                    for follow_up in recover_download_admission(app, error) {
                        self.dispatch(app, follow_up);
                    }
                }
            }
            Cmd::Download(DownloadCmd::SetDir(dir)) => {
                if let Err(error) = self.download_handle.set_dir(dir) {
                    tracing::warn!(dir = %error.dir().display(), %error, "could not update download directory");
                    let follow_ups = app.update(Msg::Download(crate::app::DownloadMsg::DirError {
                        error: error.to_string(),
                    }));
                    for follow_up in follow_ups {
                        self.dispatch(app, follow_up);
                    }
                }
            }
            Cmd::Resolve {
                video_id,
                watch_url,
            } => {
                let result = self.resolver_handle.resolve(video_id.clone(), watch_url);
                for follow_up in settle_resolver_admission(app, video_id, result) {
                    self.dispatch(app, follow_up);
                }
            }
            Cmd::ResolveForSelfHeal {
                video_id,
                watch_url,
            } => {
                let result = self
                    .resolver_handle
                    .resolve_for_self_heal(video_id.clone(), watch_url);
                for follow_up in settle_resolver_admission(app, video_id, result) {
                    self.dispatch(app, follow_up);
                }
            }
            Cmd::YtdlpSelfHeal { video_id, tools } => {
                // Off-loop: an update check downloads up to ~40 MiB. Progress rides the
                // same Tools status-line events as the maintainer; the verdict returns
                // as Msg::YtdlpHealResult for the reducer's retry-or-skip decision.
                let emitter = self.background_tasks.emitter(self.worker_tx.clone());
                self.background_tasks
                    .spawn_cancellable("ytdlp_self_heal", async move {
                        let progress_emitter = emitter.clone();
                        crate::tools::ytdlp::clear_probe_cache();
                        let outcome = crate::tools::ytdlp::rollback_or_check_and_update(
                            &tools,
                            &move |event| {
                                progress_emitter.emit(RuntimeEvent::Tools(event));
                            },
                            "playback self-heal",
                        )
                        .await;
                        let updated = matches!(
                            outcome,
                            crate::tools::ytdlp::UpdateOutcome::Installed { .. }
                        );
                        emitter
                            .emit_terminal(RuntimeEvent::App(Msg::YtdlpHealResult {
                                video_id,
                                updated,
                            }))
                            .await;
                    });
            }
            Cmd::AskAi { prompt, context } => {
                let result = self.ai_handle.as_ref().map_or_else(
                    || Err(crate::util::delivery::DeliveryError::Closed),
                    |handle| handle.ask(prompt, context),
                );
                if !report_actor_delivery(app, "ai.ask", result) {
                    recover_actor_rejection(app, ActorRejectionRecovery::AiTurn);
                }
            }
            Cmd::ResolveTrack { seq, query, config } => {
                if let Err(error) = self.api_handle.resolve_track(seq, query, config) {
                    tracing::warn!(%error, "api command enqueue failed");
                    self.reduce_owner_msg(
                        app,
                        Msg::TrackResolved {
                            seq,
                            result: Err(error.to_string()),
                        },
                    );
                }
            }
            Cmd::AiRerank {
                seed_video_id,
                prompt,
            } => {
                let recovery_seed = seed_video_id.clone();
                let result = self.ai_handle.as_ref().map_or_else(
                    || Err(crate::util::delivery::DeliveryError::Closed),
                    |handle| handle.rerank(seed_video_id, prompt),
                );
                if !report_actor_delivery(app, "ai.rerank", result)
                    && let Some(msg) = recover_actor_rejection(
                        app,
                        ActorRejectionRecovery::AiRerank(recovery_seed),
                    )
                {
                    self.reduce_owner_msg(app, msg);
                }
            }
            Cmd::SummarizeFeedback { digest } => {
                let result = self.ai_handle.as_ref().map_or_else(
                    || Err(crate::util::delivery::DeliveryError::Closed),
                    |handle| handle.summarize_feedback(digest),
                );
                if !report_actor_delivery(app, "ai.feedback", result) {
                    recover_actor_rejection(app, ActorRejectionRecovery::AiFeedback);
                }
            }
            Cmd::RomanizeTitles { request_id, items } => {
                let keys: Vec<String> = items.iter().map(|item| item.key.clone()).collect();
                if let Some(h) = &self.ai_handle {
                    if !report_actor_delivery(app, "ai.romanize", h.romanize(request_id, items)) {
                        self.reduce_owner_msg(
                            app,
                            Msg::Ai(AiMsg::RomanizedTitles {
                                request_id,
                                keys,
                                entries: Vec::new(),
                            }),
                        );
                    }
                } else {
                    self.reduce_owner_msg(
                        app,
                        Msg::Ai(AiMsg::RomanizedTitles {
                            request_id,
                            keys,
                            entries: Vec::new(),
                        }),
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
                    self.reduce_owner_msg(
                        app,
                        Msg::Streaming(StreamingMsg::Error {
                            seed_video_id,
                            error: error.to_string(),
                        }),
                    );
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
                    self.reduce_owner_msg(
                        app,
                        Msg::Streaming(StreamingMsg::Error {
                            seed_video_id,
                            error: error.to_string(),
                        }),
                    );
                }
            }
            Cmd::SetAiModel(model) => {
                if let Some(h) = &self.ai_handle {
                    report_actor_delivery(app, "ai.model", h.set_model(model));
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
            Cmd::Scrobble(scrobble) => match scrobble {
                ScrobbleCmd::AuthStart => {
                    report_actor_delivery(app, "scrobble.auth", self.scrobble_handle.auth_start());
                }
                ScrobbleCmd::Reconfigure(settings) => {
                    report_actor_delivery(
                        app,
                        "scrobble.reconfigure",
                        self.scrobble_handle.reconfigure(*settings),
                    );
                }
            },
            Cmd::Transfer(cmd) => {
                let recovery = match &cmd {
                    crate::transfer::actor::TransferCmd::StartJob(_)
                    | crate::transfer::actor::TransferCmd::WriteReviewedLocal { .. } => {
                        Some(ActorRejectionRecovery::TransferStart)
                    }
                    crate::transfer::actor::TransferCmd::CancelJob => {
                        Some(ActorRejectionRecovery::TransferCancel)
                    }
                    crate::transfer::actor::TransferCmd::AuthStart { .. }
                    | crate::transfer::actor::TransferCmd::Disconnect
                    | crate::transfer::actor::TransferCmd::ListSpotifyPlaylists => None,
                };
                let transfer_tx = self.worker_tx.clone();
                let handle = self.transfer_handle.get_or_insert_with(|| {
                    crate::transfer::actor::spawn(move |event| {
                        emit(&transfer_tx, RuntimeEvent::Transfer(event))
                    })
                });
                if !report_actor_delivery(app, "transfer", handle.send(cmd))
                    && let Some(recovery) = recovery
                {
                    recover_actor_rejection(app, recovery);
                }
            }
            // Handled in the main loop (the OSC path writes to the terminal this scope doesn't
            // own); never reaches here. Listed for exhaustiveness.
            Cmd::DesktopNotify { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests;
