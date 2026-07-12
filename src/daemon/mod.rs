//! Headless daemon mode.
//!
//! The daemon owns the primary remote descriptor and a headless mpv playback engine, so tray and
//! `ytt -r` clients can control playback without a terminal UI.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::remote::PERSONAL_EXPORT_CAPABILITY;
use crate::remote::client::{self, ClientError};
use crate::remote::proto::{
    InstanceMode, RETAINED_REQUEST_OUTCOMES_CAPABILITY, RemoteCommand, StatusSnapshot,
};
use crate::remote::server::{self, BindOutcome, RemoteEvent};
use crate::util::delivery::{DeliveryResult, OwnerEvent, OwnerEventIngress};
use crate::util::event_policy::{EventKey as Key, EventLane as Lane, EventPolicy};
use crate::util::process::{self, ProcessProfile};

mod effects;
mod engine;
mod gui_search_pending;
#[cfg(test)]
mod parity_tests;
mod personal_export;
mod shutdown_drain;

use effects::{DaemonEffectTasks, dispatch_engine_effects, dispatch_session_engine_effects};
use gui_search_pending::{GuiSearchPending, PendingGuiSearch};
use shutdown_drain::drain_daemon_shutdown_ingress;

const EXIT_OK: i32 = 0;
const EXIT_TRANSPORT: i32 = 1;
const EXIT_USAGE: i32 = 2;
const READY_TIMEOUT: Duration = Duration::from_secs(20);

const USAGE: &str = "\
Usage: ytt daemon <command> [flags]

Commands:
  start [--resume]       Start the headless playback daemon
  serve [--from-tray] [--resume]
                         Run the daemon in the foreground
  status [--json]        Print daemon/owner status
  stop                   Stop the daemon if it owns playback

Flags:
  -h, --help             Show this help
";

#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonCommand {
    Start { resume: bool },
    Serve { from_tray: bool, resume: bool },
    Status { json: bool },
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOptions {
    pub resume: bool,
    pub from_tray: bool,
    pub executable: Option<PathBuf>,
}

impl StartOptions {
    fn cli(resume: bool) -> Self {
        Self {
            resume,
            from_tray: false,
            executable: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    Started,
    Resumed,
    AlreadyRunning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    StandaloneOwner,
    InspectOwner(String),
    ResolveExecutable(String),
    Spawn(String),
    NotReady(String),
    NotRunning(String),
    ResumeRejected(String),
    StopRejected(String),
    Transport(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonError::StandaloneOwner => {
                write!(f, "YuTuTui! is already running in standalone TUI mode")
            }
            DaemonError::InspectOwner(message) => {
                write!(f, "could not inspect current owner: {message}")
            }
            DaemonError::ResolveExecutable(message) => write!(f, "{message}"),
            DaemonError::Spawn(message) => write!(f, "{message}"),
            DaemonError::NotReady(message) => write!(f, "{message}"),
            DaemonError::NotRunning(message) => write!(f, "{message}"),
            DaemonError::ResumeRejected(reason) => write!(f, "resume rejected: {reason}"),
            DaemonError::StopRejected(reason) => write!(f, "stop rejected: {reason}"),
            DaemonError::Transport(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for DaemonError {}

pub fn run_cli(args: &[String]) -> i32 {
    let command = match parse(args) {
        Ok(command) => command,
        Err(ParseOutcome::Usage) => {
            print!("{USAGE}");
            return EXIT_OK;
        }
        Err(ParseOutcome::Invalid(message)) => {
            eprintln!("ytt daemon: {message}");
            return EXIT_USAGE;
        }
    };

    // Callback actors apply bounded backpressure when both owner-delivery lanes are full.
    // Keep the owner schedulable while Tokio replaces a `block_in_place` producer worker.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ytt daemon: could not start runtime: {e}");
            return EXIT_TRANSPORT;
        }
    };
    rt.block_on(run_command(command))
}

async fn run_command(command: DaemonCommand) -> i32 {
    match command {
        DaemonCommand::Start { resume } => start_cli(resume).await,
        DaemonCommand::Serve { from_tray, resume } => serve(from_tray, resume).await,
        DaemonCommand::Status { json } => status(json).await,
        DaemonCommand::Stop => stop_cli().await,
    }
}

async fn start_cli(resume: bool) -> i32 {
    match start_daemon(StartOptions::cli(resume)).await {
        Ok(StartOutcome::AlreadyRunning) => {
            println!("YuTuTui! daemon is already running.");
            EXIT_OK
        }
        Ok(StartOutcome::Resumed) => {
            println!("YuTuTui! daemon resumed the last session.");
            EXIT_OK
        }
        Ok(StartOutcome::Started) => {
            if resume {
                println!("YuTuTui! daemon started and resumed the last session.");
            } else {
                println!("YuTuTui! daemon started.");
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!("ytt daemon: {e}");
            daemon_error_exit_code(&e)
        }
    }
}

enum DaemonEvent {
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
    /// A playback-self-heal yt-dlp update check finished (see
    /// [`engine::EngineEffect::YtdlpSelfHeal`]).
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
    fn policy(&self) -> EventPolicy {
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

    fn kind(&self) -> &'static str {
        match self {
            DaemonEvent::Remote(_) => "remote",
            DaemonEvent::Player(_) => "player",
            DaemonEvent::Api(_) => "api",
            DaemonEvent::Media(_) => "media",
            DaemonEvent::MediaArt(_) => "media_art",
            DaemonEvent::Scrobble(_) => "scrobble",
            DaemonEvent::YtdlpHeal { .. } => "ytdlp_heal",
            DaemonEvent::TransportRecoveryRetry { .. } => "transport_recovery_retry",
            DaemonEvent::PersonalExportFinished(_) => "personal_export_finished",
            DaemonEvent::Signal => "signal",
            DaemonEvent::TelemetryWake => "telemetry_wake",
        }
    }

    fn is_telemetry_wake(&self) -> bool {
        matches!(self, DaemonEvent::TelemetryWake)
    }

    fn telemetry_slot(&self) -> Option<DaemonTelemetrySlot> {
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
struct DaemonEventSender {
    ingress: OwnerEventIngress<DaemonEvent>,
}

impl DaemonEventSender {
    fn new(tx: mpsc::Sender<DaemonEvent>) -> Self {
        Self {
            ingress: OwnerEventIngress::new("daemon", tx, DAEMON_TELEMETRY_SLOTS),
        }
    }

    fn drain_coalesced(&self) -> Vec<DaemonEvent> {
        self.ingress.drain_coalesced()
    }

    fn close_admission(&self) -> bool {
        self.ingress.close_admission()
    }

    fn deferred_is_idle(&self) -> bool {
        self.ingress.deferred_is_idle()
    }

    fn emit_terminal_owned(
        &self,
        event: DaemonEvent,
    ) -> Result<
        crate::util::delivery::DeliveryReceipt,
        (crate::util::delivery::DeliveryError, Box<DaemonEvent>),
    > {
        self.ingress.emit_must_deliver_owned(event)
    }

    #[cfg(test)]
    fn with_deferred_capacity(tx: mpsc::Sender<DaemonEvent>, capacity: usize) -> Self {
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

fn emit_daemon_event(tx: &DaemonEventSender, event: DaemonEvent) -> DeliveryResult {
    tx.ingress.emit(event)
}

#[cfg(test)]
fn emit_daemon_callback_result(tx: &DaemonEventSender, event: DaemonEvent) -> DeliveryResult {
    tx.ingress.emit_callback_blocking(event)
}

#[cfg(any(windows, test))]
fn emit_daemon_callback_result_until(
    tx: &DaemonEventSender,
    event: DaemonEvent,
    cancellation: &crate::util::delivery::CallbackCancellation,
) -> DeliveryResult {
    tx.ingress.emit_callback_blocking_until(event, cancellation)
}

fn record_daemon_event(tx: &DaemonEventSender, event: DaemonEvent) {
    if let Err(error) = tx.ingress.emit_callback_blocking(event) {
        tracing::debug!(%error, "daemon event sink rejected event");
    }
}

/// Poll shutdown before an owner handler on every wake, and discard a result if the latch won
/// immediately after the handler completed. Callers must still check the latch before applying
/// synchronous follow-up effects because shutdown can arrive between any two owner operations.
async fn await_owner_handler<T>(
    shutdown: &crate::player::lifetime::ShutdownLatch,
    handler: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = shutdown.wait() => None,
        output = handler => (!shutdown.is_triggered()).then_some(output),
    }
}

async fn serve(_from_tray: bool, resume: bool) -> i32 {
    crate::player::lifetime::install_panic_hook();
    #[cfg(windows)]
    crate::player::lifetime::install_console_ctrl_handler();

    // Own the public endpoint first, then the complete persistence root set, before any loader,
    // orphan reaper, logger, or actor can touch disk. An early lease/recovery failure drops the
    // unstarted server and removes its endpoint through RemoteServer's identity-safe cleanup.
    let server = match server::bind_or_detect(false).await {
        BindOutcome::AlreadyRunning => {
            eprintln!("ytt daemon: YuTuTui! is already running.");
            return EXIT_USAGE;
        }
        BindOutcome::Unavailable => {
            eprintln!("ytt daemon: could not bind remote control socket.");
            return EXIT_TRANSPORT;
        }
        BindOutcome::Bound(server) => *server,
    }
    .with_instance_metadata(InstanceMode::Daemon, daemon_capabilities());
    if let Err(error) = crate::persist::initialize_persistence_writer(false) {
        eprintln!("ytt daemon: {error}");
        return EXIT_TRANSPORT;
    }
    if let Err(error) = crate::persist::ensure_startup_recovery_coherent() {
        eprintln!("ytt daemon: {error}");
        return EXIT_TRANSPORT;
    }
    let (raw_event_tx, mut event_rx) = crate::util::backpressure::bounded_channel::<DaemonEvent>(
        crate::util::backpressure::DAEMON_EVENT_QUEUE,
    );
    let event_tx = DaemonEventSender::new(raw_event_tx);
    let player_event_tx = event_tx.clone();
    let mut engine =
        match engine::DaemonEngine::start(engine::EngineOptions { resume }, move |event| {
            record_daemon_event(&player_event_tx, DaemonEvent::Player(event));
        })
        .await
        {
            Ok(engine) => engine,
            Err(e) => {
                eprintln!("ytt daemon: {e}");
                return EXIT_TRANSPORT;
            }
        };
    if resume && engine.status().title.is_none() {
        eprintln!("ytt daemon: resume rejected: session_empty");
        return EXIT_USAGE;
    }
    // Logging creates cache artifacts, so it begins only after every durable store has passed
    // the engine's coherent recovery load. Invalid recovery must abort byte-for-byte fail-closed.
    let _log_guard = init_daemon_logging();

    let api_event_tx = event_tx.clone();
    let api = crate::api::spawn(engine.api_cookie(), move |event| {
        record_daemon_event(&api_event_tx, DaemonEvent::Api(event));
    });

    let remote_event_tx = event_tx.clone();
    let (mut remote_guard, session_hub) = server.start(move |event| {
        emit_daemon_event(&remote_event_tx, DaemonEvent::Remote(event)).is_ok()
    });
    // The v8 publisher runs on this loop (the owner lane), next to the media/scrobble
    // post-event observers below.
    let mut publisher = crate::remote::publish::Publisher::new(session_hub);

    let shutdown = crate::player::lifetime::ShutdownLatch::new();
    let signal_event_tx = event_tx.clone();
    let mut signal_handlers =
        crate::player::lifetime::spawn_signal_handlers(shutdown.clone(), move |_| {
            // Compatibility/observability event only: the owner loop waits on `shutdown` directly,
            // so saturation here cannot delay teardown.
            record_daemon_event(&signal_event_tx, DaemonEvent::Signal);
        });

    // OS media session: the headless daemon publishes Now Playing / SMTC / MPRIS too,
    // so media keys and OS widgets control background playback without a terminal.
    //
    // `YTM_NO_MEDIA_SESSION` force-disables it. Escape hatch for GUI-less contexts —
    // CI smoke tests, `ssh`, a launchd daemon — where macOS has no login/Aqua session
    // to attach MPNowPlayingInfoCenter/MPRemoteCommandCenter to; there the activation
    // wedges the daemon's event loop (Linux MPRIS degrades gracefully, macOS does not).
    let media_cmd_tx = event_tx.clone();
    let media_art_tx = event_tx.clone();
    let media_session_allowed = std::env::var_os("YTM_NO_MEDIA_SESSION").is_none();
    let media_enabled = daemon_media_enabled(&engine, media_session_allowed);
    let mut media = crate::media::MediaSession::new_cancellable(
        media_enabled,
        move |cmd, callback_cancellation| {
            let event = DaemonEvent::Media(cmd);
            #[cfg(windows)]
            {
                // SMTC callbacks run on a dedicated thread and cannot report a busy result.
                // Preserve the exact command until owner admission or retirement of this
                // backend generation during a live media-controls toggle.
                emit_daemon_callback_result_until(&media_cmd_tx, event, callback_cancellation)
            }
            #[cfg(not(windows))]
            {
                if callback_cancellation.is_cancelled() {
                    return Err(crate::util::delivery::DeliveryError::Closed);
                }
                emit_daemon_event(&media_cmd_tx, event)
            }
        },
        move |ready| {
            record_daemon_event(&media_art_tx, DaemonEvent::MediaArt(ready));
        },
    );
    // Scrobbler: same snapshot feed as the TUI loop, headless-safe (log-only events).
    // Config is read at daemon start — reconnecting via `ytt auth lastfm` needs a daemon
    // restart, which the CLI prints as a hint.
    let scrobble_event_tx = event_tx.clone();
    let mut scrobble = crate::scrobble::spawn(engine.scrobble_settings(), move |event| {
        record_daemon_event(&scrobble_event_tx, DaemonEvent::Scrobble(event));
    });

    if !shutdown.is_triggered() {
        let startup_snapshot = engine.media_snapshot();
        if !shutdown.is_triggered() {
            let _ = scrobble.observe(&startup_snapshot);
        }
        if !shutdown.is_triggered() {
            media.publish(startup_snapshot);
        }
    }
    // macOS delivers remote-command callbacks through the main run loop; pump it on a
    // short interval while the session is live (the guard parks the timer elsewhere).
    let mut media_pump = tokio::time::interval(Duration::from_millis(100));
    media_pump.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut scrobble_retry_tick = tokio::time::interval(Duration::from_millis(250));
    scrobble_retry_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut effect_tasks = DaemonEffectTasks::new();
    let mut gui_search_pending = GuiSearchPending::default();
    let mut personal_export = personal_export::PersonalExport::default();
    let mut pending_events: VecDeque<DaemonEvent> = VecDeque::new();
    if !shutdown.is_triggered() {
        let initial_effects = engine.initial_effects();
        pending_events.extend(dispatch_engine_effects(
            &api,
            &event_tx,
            &shutdown,
            &mut effect_tasks,
            &mut gui_search_pending,
            initial_effects,
        ));
    }

    'owner: loop {
        effect_tasks.reap_finished();
        gui_search_pending.prune_closed();
        if shutdown.is_triggered() {
            engine.suppress_transport_recovery_for_shutdown();
            break;
        }
        let event = if let Some(event) = pending_events.pop_front() {
            event
        } else {
            tokio::select! {
                biased;
                _ = shutdown.wait() => {
                    engine.suppress_transport_recovery_for_shutdown();
                    break 'owner;
                },
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                    media.retry_deadline().unwrap_or_else(Instant::now)
                )), if media.retry_deadline().is_some() => {
                    media.publish(engine.media_snapshot());
                    continue;
                },
                maybe = event_rx.recv() => match maybe {
                    Some(event) => event,
                    None => {
                        shutdown.trigger();
                        break 'owner;
                    },
                },
                _ = media_pump.tick(), if media.wants_pump() => {
                    if shutdown.is_triggered() {
                        engine.suppress_transport_recovery_for_shutdown();
                        break 'owner;
                    }
                    media.pump();
                    continue;
                },
                _ = scrobble_retry_tick.tick(), if scrobble.retry_needed() => {
                    let _ = scrobble.observe(&engine.media_snapshot());
                    continue;
                },
            }
        };
        // A queued TransportClosed/Signal may have won the same scheduler turn. The monotonic
        // latch still takes precedence before the event can mutate the engine or spawn mpv.
        if shutdown.is_triggered() {
            engine.suppress_transport_recovery_for_shutdown();
            break;
        }
        if event.is_telemetry_wake() {
            pending_events.extend(event_tx.drain_coalesced());
            continue;
        }
        match event {
            DaemonEvent::Remote(RemoteEvent::Command(command, reply)) => match command {
                RemoteCommand::ExportPersonalData { directory } => {
                    personal_export.start_engine(
                        directory,
                        reply,
                        &engine,
                        &event_tx,
                        &shutdown,
                        &mut effect_tasks,
                    );
                }
                command => {
                    let Some((response, wants_shutdown, effects)) =
                        await_owner_handler(&shutdown, engine.handle_remote(command)).await
                    else {
                        engine.suppress_transport_recovery_for_shutdown();
                        break;
                    };
                    if shutdown.is_triggered() {
                        engine.suppress_transport_recovery_for_shutdown();
                        break;
                    }
                    let _ = reply.send(response);
                    if wants_shutdown {
                        shutdown.trigger();
                        break;
                    }
                    pending_events.extend(dispatch_engine_effects(
                        &api,
                        &event_tx,
                        &shutdown,
                        &mut effect_tasks,
                        &mut gui_search_pending,
                        effects,
                    ));
                }
            },
            DaemonEvent::Remote(RemoteEvent::SessionCommand {
                command,
                origin,
                reply,
            }) => match command {
                RemoteCommand::ExportPersonalData { directory } => {
                    personal_export.start_engine(
                        directory,
                        reply,
                        &engine,
                        &event_tx,
                        &shutdown,
                        &mut effect_tasks,
                    );
                }
                command => {
                    let requester_key = engine::RequesterKey::new(
                        origin.session_id(),
                        origin.page_id().map(str::to_owned),
                    );
                    let Some((response, wants_shutdown, effects)) = await_owner_handler(
                        &shutdown,
                        engine.handle_session_remote(command, requester_key),
                    )
                    .await
                    else {
                        engine.suppress_transport_recovery_for_shutdown();
                        break;
                    };
                    if shutdown.is_triggered() {
                        engine.suppress_transport_recovery_for_shutdown();
                        break;
                    }
                    let _ = reply.send(response);
                    if wants_shutdown {
                        shutdown.trigger();
                        break;
                    }
                    pending_events.extend(dispatch_session_engine_effects(
                        &api,
                        &event_tx,
                        &shutdown,
                        &mut effect_tasks,
                        &mut gui_search_pending,
                        &origin,
                        effects,
                    ));
                }
            },
            // Owner lane (docs/gui/02 §8/§14): initial snapshots + reply from current
            // engine state, in order, into this session's queue.
            DaemonEvent::Remote(RemoteEvent::SessionSubscribe {
                session,
                frame_id,
                page_id,
                topics,
                settlement,
            }) => {
                if shutdown.is_triggered() {
                    engine.suppress_transport_recovery_for_shutdown();
                    break;
                }
                publisher.handle_tracked_subscribe(
                    &engine.core_view(),
                    &session,
                    page_id.as_deref(),
                    frame_id,
                    &topics,
                    settlement,
                );
                continue;
            }
            DaemonEvent::Player(event) => {
                let Some(effects) =
                    await_owner_handler(&shutdown, engine.handle_player_event(event)).await
                else {
                    engine.suppress_transport_recovery_for_shutdown();
                    break;
                };
                pending_events.extend(dispatch_engine_effects(
                    &api,
                    &event_tx,
                    &shutdown,
                    &mut effect_tasks,
                    &mut gui_search_pending,
                    effects,
                ));
            }
            // GUI-search answers are owner-lane fan-out. A completion is admitted only while
            // its requester page is current and subscribed; only then expose those exact rows
            // to follow-up play_tracks/enqueue_tracks commands from that requester.
            DaemonEvent::Api(crate::api::ApiEvent::GuiSearchCompleted { request_id, groups }) => {
                if shutdown.is_triggered() {
                    engine.suppress_transport_recovery_for_shutdown();
                    break;
                }
                if let Some(pending) = gui_search_pending.take(request_id) {
                    route_gui_search_completion(&mut engine, &publisher, &pending, &groups);
                } else {
                    tracing::debug!(
                        ?request_id,
                        "ignored unknown or stale GUI search completion"
                    );
                }
            }
            DaemonEvent::Api(event) => {
                let Some(effects) =
                    await_owner_handler(&shutdown, engine.handle_api_event(event)).await
                else {
                    engine.suppress_transport_recovery_for_shutdown();
                    break;
                };
                pending_events.extend(dispatch_engine_effects(
                    &api,
                    &event_tx,
                    &shutdown,
                    &mut effect_tasks,
                    &mut gui_search_pending,
                    effects,
                ));
            }
            DaemonEvent::Media(command) => {
                let Some((wants_shutdown, effects)) =
                    await_owner_handler(&shutdown, engine.handle_media(command)).await
                else {
                    engine.suppress_transport_recovery_for_shutdown();
                    break;
                };
                if wants_shutdown {
                    shutdown.trigger();
                    engine.suppress_transport_recovery_for_shutdown();
                    break;
                }
                pending_events.extend(dispatch_engine_effects(
                    &api,
                    &event_tx,
                    &shutdown,
                    &mut effect_tasks,
                    &mut gui_search_pending,
                    effects,
                ));
            }
            DaemonEvent::MediaArt(ready) => {
                if shutdown.is_triggered() {
                    engine.suppress_transport_recovery_for_shutdown();
                    break;
                }
                engine.set_media_art(ready);
            }
            DaemonEvent::YtdlpHeal { video_id, updated } => {
                let Some(effects) =
                    await_owner_handler(&shutdown, engine.handle_heal_result(video_id, updated))
                        .await
                else {
                    engine.suppress_transport_recovery_for_shutdown();
                    break;
                };
                pending_events.extend(dispatch_engine_effects(
                    &api,
                    &event_tx,
                    &shutdown,
                    &mut effect_tasks,
                    &mut gui_search_pending,
                    effects,
                ));
            }
            DaemonEvent::TransportRecoveryRetry { generation } => {
                let Some(effects) =
                    await_owner_handler(&shutdown, engine.attempt_transport_recovery(generation))
                        .await
                else {
                    engine.suppress_transport_recovery_for_shutdown();
                    break;
                };
                pending_events.extend(dispatch_engine_effects(
                    &api,
                    &event_tx,
                    &shutdown,
                    &mut effect_tasks,
                    &mut gui_search_pending,
                    effects,
                ));
            }
            DaemonEvent::PersonalExportFinished(finished) => personal_export.finish(finished),
            DaemonEvent::Scrobble(event) => log_scrobble_event(event),
            DaemonEvent::Signal => {
                shutdown.trigger();
                engine.suppress_transport_recovery_for_shutdown();
                break;
            }
            DaemonEvent::TelemetryWake => unreachable!("telemetry wake is handled before dispatch"),
        }
        if shutdown.is_triggered() {
            engine.suppress_transport_recovery_for_shutdown();
            break;
        }
        // Mirror the post-event state to the OS session (diff-based inside); the
        // scrobbler taps the same snapshot first (it ignores media-controls enablement).
        // Reconcile the persisted live setting on every owner turn so GUI/remote changes tear
        // down or create the platform generation before this turn's snapshot is published.
        media.set_enabled(daemon_media_enabled(&engine, media_session_allowed));
        let snapshot = engine.media_snapshot();
        if shutdown.is_triggered() {
            engine.suppress_transport_recovery_for_shutdown();
            break;
        }
        let _ = scrobble.observe(&snapshot);
        if shutdown.is_triggered() {
            engine.suppress_transport_recovery_for_shutdown();
            break;
        }
        media.publish(snapshot);
        if shutdown.is_triggered() {
            engine.suppress_transport_recovery_for_shutdown();
            break;
        }
        // v8 push: fingerprint-diffed; time-tick events change nothing it watches.
        publisher.observe(&engine.core_view());
    }
    shutdown.trigger();
    engine.suppress_transport_recovery_for_shutdown();
    // Token creation and this monotonic transition share the hub registry lock. Close remote
    // admission before the generic owner ingress so no accepted request can appear without a
    // wire-settlement token beyond the drain frontier.
    publisher.quiesce_owner_admission();
    // Reject callback producers before awaiting any task that may itself be inside a callback.
    // This breaks the owner-waits-producer / producer-waits-owner shutdown cycle under saturation.
    event_tx.close_admission();
    // Remove the OS media surface before the slower task barrier. Its callbacks now see a closed
    // ingress, and a fast successor must not compete with a stale Now Playing/MPRIS/SMTC target.
    let _ = media.set_enabled(false);
    let drain = drain_daemon_shutdown_ingress(
        &event_tx,
        &mut event_rx,
        &mut pending_events,
        &publisher,
        &mut personal_export,
    )
    .await;
    // A worker that completed before the admission frontier was settled by the drain above.
    // Anything still retained cannot re-enter now, so release its wire settlement explicitly.
    personal_export.shutdown();
    tracing::debug!(
        remote_requests = drain.remote_requests,
        subscribe_requests = drain.subscribe_requests,
        terminal_events = drain.terminal_events,
        personal_export_completions = drain.personal_export_completions,
        coalesced_events = drain.coalesced_events,
        retired_events = drain.retired_events,
        "daemon shutdown ingress drained"
    );
    if !publisher.wait_for_wire_settlements().await {
        // Only a last scheduler margin after the structural writer budget timed out (and logged).
        crate::remote::await_shutdown_reply_grace().await;
    }
    // Keep the endpoint lease through settlement so a retry cannot reach a successor and bypass
    // this process-local dedupe frontier. Release while the old listener still owns the path;
    // every later cleanup is then inert toward the successor socket.
    remote_guard.release_endpoint();
    gui_search_pending.clear();
    publisher.shutting_down();
    remote_guard.shutdown().await;
    signal_handlers.shutdown().await;
    engine.shutdown_background().await;
    effect_tasks.shutdown().await;
    // The deadline reports slow shutdown but never cancels accepted local durability. A failed
    // final frontier is a transport failure, not a clean daemon exit.
    match scrobble
        .shutdown_and_join(Duration::from_millis(1500))
        .await
    {
        Ok(()) => EXIT_OK,
        Err(error) => {
            tracing::warn!(%error, "scrobble shutdown durability was not confirmed");
            EXIT_TRANSPORT
        }
    }
}

fn daemon_media_enabled(engine: &engine::DaemonEngine, media_session_allowed: bool) -> bool {
    media_session_allowed && engine.media_controls_enabled()
}

/// Route an async search answer only while both generations still match: the engine's latest
/// requester ticket and the socket hub's live session/page subscription. Indexing happens only
/// after the targeted push is admitted, so a closed or replaced page cannot leave actionable rows.
fn route_gui_search_completion(
    engine: &mut engine::DaemonEngine,
    publisher: &crate::remote::publish::Publisher,
    pending: &PendingGuiSearch,
    groups: &[crate::api::GuiSearchGroup],
) -> bool {
    if !engine.gui_search_is_current(&pending.requester_key, pending.ticket)
        || !publisher.search_completed(
            &pending.requester,
            pending.ticket,
            &pending.query,
            pending.source,
            groups,
        )
    {
        return false;
    }
    let completed = engine.complete_gui_search(&pending.requester_key, pending.ticket, groups);
    debug_assert!(completed);
    completed
}

/// The daemon's stand-in for the TUI's status toasts: scrobble notices go to the log.
fn log_scrobble_event(event: crate::scrobble::ScrobbleEvent) {
    use crate::scrobble::ScrobbleEvent;
    match event {
        ScrobbleEvent::SessionInvalid(kind) => {
            tracing::warn!(
                service = kind.label(),
                "scrobble session invalid; run `ytt auth`"
            );
        }
        ScrobbleEvent::QueueStalled { pending } => {
            if pending == 0 {
                tracing::info!("scrobble storage recovered; retained listens were saved");
            } else {
                tracing::warn!(pending, "scrobble queue stalled");
            }
        }
        ScrobbleEvent::QueueDropped { dropped } => {
            tracing::warn!(dropped, "scrobble queue over cap; dropped oldest entries");
        }
        // The daemon never starts the interactive flow, so these are unexpected.
        ScrobbleEvent::AuthUrl(_) | ScrobbleEvent::AuthDone { .. } => {
            tracing::info!("unexpected scrobble auth event in daemon");
        }
        ScrobbleEvent::AuthFailed(error) => {
            tracing::warn!(error = %crate::util::sanitize::sanitize_error_text(error), "scrobble auth failed");
        }
    }
}

async fn status(json: bool) -> i32 {
    let response = match client::send(RemoteCommand::Status).await {
        Ok(response) => response,
        Err(e) => {
            eprintln!("ytt daemon: {}", e.human_message());
            return EXIT_TRANSPORT;
        }
    };

    if json {
        match serde_json::to_string(&response) {
            Ok(line) => println!("{line}"),
            Err(e) => {
                eprintln!("ytt daemon: could not encode status: {e}");
                return EXIT_TRANSPORT;
            }
        }
    } else if let Some(status) = response.status {
        let owner = match status.owner_mode {
            InstanceMode::StandaloneTui => "standalone TUI",
            InstanceMode::Daemon => "daemon",
        };
        println!("{owner}: {}", status.human_line());
    } else if let Some(message) = response.message {
        println!("{message}");
    }

    EXIT_OK
}

async fn stop_cli() -> i32 {
    match stop_daemon().await {
        Ok(()) => {
            println!("YuTuTui! daemon stopped.");
            EXIT_OK
        }
        Err(e) => {
            eprintln!("ytt daemon: {e}");
            daemon_error_exit_code(&e)
        }
    }
}

pub async fn start_daemon(options: StartOptions) -> Result<StartOutcome, DaemonError> {
    match current_status().await {
        Ok(status) if status.owner_mode == InstanceMode::Daemon => {
            if options.resume {
                resume_running_daemon().await?;
                return Ok(StartOutcome::Resumed);
            }
            return Ok(StartOutcome::AlreadyRunning);
        }
        Ok(_) => return Err(DaemonError::StandaloneOwner),
        Err(ClientError::NoRunningInstance | ClientError::ConnectFailed) => {}
        Err(e) => return Err(DaemonError::InspectOwner(e.human_message())),
    }

    let mut spawn_options = options.clone();
    spawn_options.resume = false;
    spawn_daemon_process(&spawn_options)?;
    wait_until_ready().await.map_err(DaemonError::NotReady)?;
    if options.resume
        && let Err(e) = resume_running_daemon().await
    {
        let _ = stop_daemon().await;
        return Err(e);
    }
    Ok(StartOutcome::Started)
}

async fn resume_running_daemon() -> Result<(), DaemonError> {
    match client::send(RemoteCommand::ResumeSession).await {
        Ok(response) if response.ok => Ok(()),
        Ok(response) => Err(DaemonError::ResumeRejected(
            response.reason.unwrap_or_else(|| "rejected".to_string()),
        )),
        Err(e) => Err(DaemonError::Transport(e.human_message())),
    }
}

pub async fn stop_daemon() -> Result<(), DaemonError> {
    let status = current_status().await.map_err(|e| match e {
        ClientError::NoRunningInstance => DaemonError::NotRunning(e.human_message()),
        other => DaemonError::Transport(other.human_message()),
    })?;

    if status.owner_mode != InstanceMode::Daemon {
        return Err(DaemonError::StandaloneOwner);
    }

    match client::send(RemoteCommand::Quit).await {
        Ok(response) if response.ok => Ok(()),
        Ok(response) => Err(DaemonError::StopRejected(
            response.reason.unwrap_or_else(|| "rejected".to_string()),
        )),
        Err(e) => Err(DaemonError::Transport(e.human_message())),
    }
}

fn daemon_error_exit_code(error: &DaemonError) -> i32 {
    match error {
        DaemonError::StandaloneOwner
        | DaemonError::ResumeRejected(_)
        | DaemonError::StopRejected(_) => EXIT_USAGE,
        _ => EXIT_TRANSPORT,
    }
}

async fn current_status() -> Result<StatusSnapshot, ClientError> {
    let response = client::send(RemoteCommand::Status).await?;
    response.status.ok_or(ClientError::MalformedResponse)
}

fn daemon_capabilities() -> Vec<String> {
    vec![
        "remote-control".to_string(),
        "status".to_string(),
        "queue-control".to_string(),
        RETAINED_REQUEST_OUTCOMES_CAPABILITY.to_string(),
        "headless-playback".to_string(),
        "session-resume".to_string(),
        "autoplay-streaming".to_string(),
        "search-playback".to_string(),
        // v8 sessions with live push (docs/gui/02 §10).
        "events-v8".to_string(),
        PERSONAL_EXPORT_CAPABILITY.to_string(),
    ]
}

fn spawn_daemon_process(options: &StartOptions) -> Result<(), DaemonError> {
    let exe = match &options.executable {
        Some(path) => path.clone(),
        None => std::env::current_exe().map_err(|e| {
            DaemonError::ResolveExecutable(format!("could not resolve current exe: {e}"))
        })?,
    };
    let mut cmd = std::process::Command::new(exe);
    process::apply_std_env(&mut cmd, ProcessProfile::Daemon);
    cmd.args(["daemon", "serve"]);
    if options.from_tray {
        cmd.arg("--from-tray");
    }
    if options.resume {
        cmd.arg("--resume");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Become a real background daemon on Unix/macOS too. Without a new session, the child
        // stays tied to the launching shell and can receive SIGHUP when that shell exits.
        // SAFETY: `pre_exec` runs in the child after fork and before exec; `setsid` is
        // an async-signal-safe syscall and reports failure through errno.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
        // The Stdio::null() above only sets the daemon's OWN std handles; the spawn
        // still runs with bInheritHandles=TRUE (std needs it to pass those NULs), which
        // leaks every *inheritable* handle in this client into the daemon — including
        // the write end of whatever pipe captures `ytt daemon start`'s output. A shell
        // reading that pipe then never sees EOF while the daemon lives (`$out = ytt
        // daemon start | Out-String` hung forever; the CI smoke's Invoke-Checked hit
        // the same). The client is about to exit and spawns nothing else, so stripping
        // the inherit flag from its std handles closes the leak at the source.
        // SAFETY: `GetStdHandle` returns process-owned pseudo/real handles; invalid or
        // null handles are skipped, and clearing HANDLE_FLAG_INHERIT is best-effort.
        unsafe {
            use windows_sys::Win32::Foundation::{
                HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
            };
            use windows_sys::Win32::System::Console::{
                GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
            };
            for kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let handle = GetStdHandle(kind);
                if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                    let _ = SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
                }
            }
        }
    }

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| DaemonError::Spawn(format!("could not start daemon process: {e}")))
}

fn init_daemon_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dir = daemon_log_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let guard = crate::logging::init_named(&dir, "daemon.log");
    if guard.is_some() {
        tracing::info!(dir = %dir.display(), prefix = "daemon.log", "daemon logging initialized");
    }
    guard
}

fn daemon_log_dir() -> Option<PathBuf> {
    crate::paths::cache_dir().map(|cache_dir| cache_dir.join("logs"))
}

async fn wait_until_ready() -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let last_error = match current_status().await {
            Ok(status) if status.owner_mode == InstanceMode::Daemon => return Ok(()),
            Ok(_) => {
                return Err("another YuTuTui! owner appeared while starting daemon".to_string());
            }
            Err(e) => e.human_message(),
        };

        if Instant::now() >= deadline {
            return Err(format!("daemon did not become ready: {last_error}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParseOutcome {
    Usage,
    Invalid(String),
}

fn parse(args: &[String]) -> Result<DaemonCommand, ParseOutcome> {
    let Some((verb, rest)) = args.split_first() else {
        return Err(ParseOutcome::Usage);
    };
    if matches!(verb.as_str(), "-h" | "--help") {
        return Err(ParseOutcome::Usage);
    }

    match verb.as_str() {
        "start" => {
            let mut resume = false;
            for arg in rest {
                match arg.as_str() {
                    "--resume" => resume = true,
                    "-h" | "--help" => return Err(ParseOutcome::Usage),
                    other => {
                        return Err(ParseOutcome::Invalid(format!(
                            "start: unknown flag `{other}`"
                        )));
                    }
                }
            }
            Ok(DaemonCommand::Start { resume })
        }
        "serve" => {
            let mut from_tray = false;
            let mut resume = false;
            for arg in rest {
                match arg.as_str() {
                    "--from-tray" => from_tray = true,
                    "--resume" => resume = true,
                    "-h" | "--help" => return Err(ParseOutcome::Usage),
                    other => {
                        return Err(ParseOutcome::Invalid(format!(
                            "serve: unknown flag `{other}`"
                        )));
                    }
                }
            }
            Ok(DaemonCommand::Serve { from_tray, resume })
        }
        "status" => {
            let mut json = false;
            for arg in rest {
                match arg.as_str() {
                    "--json" => json = true,
                    "-h" | "--help" => return Err(ParseOutcome::Usage),
                    other => {
                        return Err(ParseOutcome::Invalid(format!(
                            "status: unknown flag `{other}`"
                        )));
                    }
                }
            }
            Ok(DaemonCommand::Status { json })
        }
        "stop" => {
            if !rest.is_empty() {
                return Err(ParseOutcome::Invalid(
                    "stop: unexpected arguments".to_string(),
                ));
            }
            Ok(DaemonCommand::Stop)
        }
        other => Err(ParseOutcome::Invalid(format!(
            "unknown command `{other}` (try `ytt daemon --help`)"
        ))),
    }
}

#[cfg(test)]
mod tests;
