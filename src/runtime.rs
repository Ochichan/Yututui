//! Runtime event adapter between leaf actors and the app reducer.
//!
//! Leaf domain events cross this single orchestration boundary into reducer messages.

use ratatui_image::thread::ResizeResponse;
use std::sync::atomic::AtomicU64;

use crate::app::PersistCmd;
use crate::app::{
    AiMsg, App, Cmd, DataCmd, DownloadCmd, Msg, PersonalDataExportCmd, PlayerControl, PlayerMsg,
    ScrobbleCmd, SearchCmd, SearchMsg, StreamingMsg,
};
use crate::owner_event_policy::{
    api_event_policy, download_event_policy, player_event_policy, remote_event_policy,
    scrobble_event_policy, transfer_event_policy,
};
#[cfg(test)]
use crate::player::PlayerCmd;
use crate::player::PlayerHandle;
use crate::util::delivery::{DeliveryReceipt, DeliveryResult};
use crate::util::event_policy::{EventKey as Key, EventLane as Lane, EventPolicy};

mod delivery_reporting;
mod dispatch;
mod event_policy;
pub(crate) mod ingress;
mod local_find;
mod persist_delivery;
pub(crate) mod player_delivery;
mod read_only;
mod recorder_recovery;
mod task_set;
mod transfer_commit;

#[cfg(test)]
use crate::util::delivery::DeliveryError;
use delivery_reporting::{ActorRejectionRecovery, recover_actor_rejection, report_actor_delivery};
#[cfg(test)]
use event_policy::player_msg_policy as app_player_msg_policy;
use event_policy::{app_msg_policy, video_event_policy};
#[cfg(test)]
use player_delivery::{
    PENDING_PLAYER_CMDS_MAX, PENDING_PLAYER_INTENTS_MAX, PlayerRestartDecision,
    admit_player_intent, reject_pending_player_intents, settle_player_intent,
};
use player_delivery::{PendingPlayerIntents, RuntimePlayerLifecycle, report_player_delivery};
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
                // The interactive reducer owns one in-flight rerank seed and can reject an old
                // result before it mutates the queue.
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
            RuntimeEvent::Api(event) => api_event_policy(event),
            RuntimeEvent::Artwork(_) => EventPolicy::DropIfStale {
                stale_key: Key::ArtworkVideo,
            },
            RuntimeEvent::ArtworkResized(_) => EventPolicy::CoalesceLatest {
                lane: Lane::Telemetry,
                key: Key::ArtResize,
            },
            RuntimeEvent::Download(event) => download_event_policy(event),
            // Interactive lyrics requests are keyed to the currently visible track.
            RuntimeEvent::Lyrics(_) => EventPolicy::DropIfStale {
                stale_key: Key::LyricsVideo,
            },
            RuntimeEvent::Player(event) => player_event_policy(event.unscoped()),
            RuntimeEvent::Persist(_) => EventPolicy::MustDeliver {
                lane: Lane::WorkResult,
            },
            RuntimeEvent::Remote(event) => remote_event_policy(event),
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
            RuntimeEvent::Scrobble(event) => scrobble_event_policy(event),
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
            RuntimeEvent::Transfer(event) => transfer_event_policy(event),
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
                    request_id,
                    seed_video_id,
                    picks,
                    conf,
                } => Msg::Streaming(StreamingMsg::AiPicks {
                    request_id,
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
                } => Msg::Search(SearchMsg::Results {
                    request_id,
                    query,
                    source,
                    songs,
                    timed_out,
                }),
                crate::api::ApiEvent::SearchError {
                    request_id,
                    source,
                    error,
                } => Msg::Search(SearchMsg::Error {
                    request_id,
                    source,
                    error,
                }),
                crate::api::ApiEvent::PlaylistTracks {
                    title,
                    intent,
                    songs,
                } => Msg::Search(SearchMsg::PlaylistTracks {
                    title,
                    intent,
                    songs,
                }),
                crate::api::ApiEvent::PlaylistTracksError { title, error } => {
                    Msg::Search(SearchMsg::PlaylistTracksError { title, error })
                }
                crate::api::ApiEvent::ArtistPage { page } => {
                    Msg::Search(SearchMsg::ArtistPage { page })
                }
                crate::api::ApiEvent::ArtistPageError { title, error } => {
                    Msg::Search(SearchMsg::ArtistPageError { title, error })
                }
                crate::api::ApiEvent::StreamingResults {
                    request_id,
                    seed_video_id,
                    candidates,
                } => Msg::Streaming(StreamingMsg::Results {
                    request_id,
                    seed_video_id,
                    candidates,
                }),
                crate::api::ApiEvent::StreamingPreflighted {
                    request_id,
                    seed_video_id,
                    songs,
                } => Msg::Streaming(StreamingMsg::Preflighted {
                    request_id,
                    seed_video_id,
                    songs,
                }),
                crate::api::ApiEvent::StreamingError {
                    request_id,
                    seed_video_id,
                    error,
                } => Msg::Streaming(StreamingMsg::Error {
                    request_id,
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
    /// Typed owner lifecycle keeps handle/guard ownership, restart budget, and the ordered
    /// restore batch in one reachable state.
    player: RuntimePlayerLifecycle<PlayerHandle, crate::player::Mpv>,
    pending_player_intents: PendingPlayerIntents,
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
    /// Latest admitted Local Find evaluation. Older blocking searches poll this epoch and retire
    /// without publishing, so fast typing cannot build an unbounded tail of obsolete work.
    local_find_query_epoch: std::sync::Arc<AtomicU64>,
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
            player: RuntimePlayerLifecycle::default(),
            pending_player_intents: PendingPlayerIntents::default(),
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
            local_find_query_epoch: std::sync::Arc::new(AtomicU64::new(0)),
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
}

#[cfg(test)]
mod tests;
