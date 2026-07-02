//! Headless daemon mode.
//!
//! The daemon owns the primary remote descriptor and a headless mpv playback engine, so tray and
//! `ytt -r` clients can control playback without a terminal UI.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::remote::client::{self, ClientError};
use crate::remote::proto::{InstanceMode, RemoteCommand, StatusSnapshot};
use crate::remote::server::{self, BindOutcome, RemoteEvent};
use crate::util::process::{self, ProcessProfile};

mod engine;

const EXIT_OK: i32 = 0;
const EXIT_TRANSPORT: i32 = 1;
const EXIT_USAGE: i32 = 2;
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_REPLY_GRACE: Duration = Duration::from_millis(50);

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
                write!(f, "ytm-tui is already running in standalone TUI mode")
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

    let rt = match tokio::runtime::Builder::new_current_thread()
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
            println!("ytm-tui daemon is already running.");
            EXIT_OK
        }
        Ok(StartOutcome::Resumed) => {
            println!("ytm-tui daemon resumed the last session.");
            EXIT_OK
        }
        Ok(StartOutcome::Started) => {
            if resume {
                println!("ytm-tui daemon started and resumed the last session.");
            } else {
                println!("ytm-tui daemon started.");
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
    Signal,
}

async fn serve(_from_tray: bool, resume: bool) -> i32 {
    crate::player::lifetime::install_panic_hook();
    #[cfg(windows)]
    crate::player::lifetime::install_console_ctrl_handler();
    let _log_guard = init_daemon_logging();

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<DaemonEvent>();
    let player_event_tx = event_tx.clone();
    let mut engine =
        match engine::DaemonEngine::start(engine::EngineOptions { resume }, move |event| {
            let _ = player_event_tx.send(DaemonEvent::Player(event));
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

    let api_event_tx = event_tx.clone();
    let api = crate::api::spawn(engine.api_cookie(), move |event| {
        let _ = api_event_tx.send(DaemonEvent::Api(event));
    });

    let server = match server::bind_or_detect(false).await {
        BindOutcome::AlreadyRunning => {
            eprintln!("ytt daemon: ytm-tui is already running.");
            return EXIT_USAGE;
        }
        BindOutcome::Unavailable => {
            eprintln!("ytt daemon: could not bind remote control socket.");
            return EXIT_TRANSPORT;
        }
        BindOutcome::Bound(server) => *server,
    }
    .with_instance_metadata(InstanceMode::Daemon, daemon_capabilities());

    let remote_event_tx = event_tx.clone();
    let _guard =
        server.start(move |event| remote_event_tx.send(DaemonEvent::Remote(event)).is_ok());

    let signal_event_tx = event_tx.clone();
    crate::player::lifetime::spawn_signal_handlers(move |_| {
        let _ = signal_event_tx.send(DaemonEvent::Signal);
    });

    // OS media session: the headless daemon publishes Now Playing / SMTC / MPRIS too,
    // so media keys and OS widgets control background playback without a terminal.
    let media_cmd_tx = event_tx.clone();
    let media_art_tx = event_tx.clone();
    let mut media = crate::media::MediaSession::new(
        engine.media_controls_enabled(),
        move |cmd| {
            let _ = media_cmd_tx.send(DaemonEvent::Media(cmd));
        },
        move |ready| {
            let _ = media_art_tx.send(DaemonEvent::MediaArt(ready));
        },
    );
    // Scrobbler: same snapshot feed as the TUI loop, headless-safe (log-only events).
    // Config is read at daemon start — reconnecting via `ytt auth lastfm` needs a daemon
    // restart, which the CLI prints as a hint.
    let scrobble_event_tx = event_tx.clone();
    let mut scrobble = crate::scrobble::spawn(engine.scrobble_settings(), move |event| {
        let _ = scrobble_event_tx.send(DaemonEvent::Scrobble(event));
    });

    let startup_snapshot = engine.media_snapshot();
    scrobble.observe(&startup_snapshot);
    media.publish(startup_snapshot);
    // macOS delivers remote-command callbacks through the main run loop; pump it on a
    // short interval while the session is live (the guard parks the timer elsewhere).
    let mut media_pump = tokio::time::interval(Duration::from_millis(100));
    media_pump.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    dispatch_engine_effects(&api, engine.initial_effects());

    loop {
        let event = tokio::select! {
            maybe = event_rx.recv() => match maybe {
                Some(event) => event,
                None => break,
            },
            _ = media_pump.tick(), if media.wants_pump() => {
                media.pump();
                continue;
            },
        };
        match event {
            DaemonEvent::Remote(RemoteEvent::Command(command, reply)) => {
                let (response, shutdown, effects) = engine.handle_remote(command).await;
                let _ = reply.send(response);
                dispatch_engine_effects(&api, effects);
                if shutdown {
                    tokio::time::sleep(SHUTDOWN_REPLY_GRACE).await;
                    break;
                }
            }
            DaemonEvent::Player(event) => {
                let effects = engine.handle_player_event(event).await;
                dispatch_engine_effects(&api, effects);
            }
            DaemonEvent::Api(event) => {
                let effects = engine.handle_api_event(event).await;
                dispatch_engine_effects(&api, effects);
            }
            DaemonEvent::Media(command) => {
                let (shutdown, effects) = engine.handle_media(command).await;
                dispatch_engine_effects(&api, effects);
                if shutdown {
                    break;
                }
            }
            DaemonEvent::MediaArt(ready) => engine.set_media_art(ready),
            DaemonEvent::Scrobble(event) => log_scrobble_event(event),
            DaemonEvent::Signal => break,
        }
        // Mirror the post-event state to the OS session (diff-based inside); the
        // scrobbler taps the same snapshot first (it ignores media-controls enablement).
        let snapshot = engine.media_snapshot();
        scrobble.observe(&snapshot);
        media.publish(snapshot);
    }
    // Bounded best-effort delivery of queued scrobbles; leftovers flush next launch.
    let _ = tokio::time::timeout(Duration::from_millis(1500), scrobble.shutdown_flush()).await;
    EXIT_OK
}

/// The daemon's stand-in for the TUI's status toasts: scrobble notices go to the log.
fn log_scrobble_event(event: crate::scrobble::ScrobbleEvent) {
    use crate::scrobble::ScrobbleEvent;
    match event {
        ScrobbleEvent::SessionInvalid(kind) => {
            tracing::warn!(service = kind.label(), "scrobble session invalid; run `ytt auth`");
        }
        ScrobbleEvent::QueueStalled { pending } => {
            tracing::warn!(pending, "scrobble queue stalled");
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
            println!("ytm-tui daemon stopped.");
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
        "headless-playback".to_string(),
        "session-resume".to_string(),
        "autoplay-streaming".to_string(),
        "search-playback".to_string(),
    ]
}

fn dispatch_engine_effects(api: &crate::api::ApiHandle, effects: Vec<engine::EngineEffect>) {
    for effect in effects {
        match effect {
            engine::EngineEffect::StreamingFallback {
                seed,
                seed_video_id,
                exclude_ids,
                limit,
                mode,
                config,
            } => api.streaming(seed, seed_video_id, exclude_ids, limit, mode, config),
            engine::EngineEffect::StreamingPreflight {
                seed_video_id,
                picks,
                fallback,
                mode,
                config,
            } => api.streaming_preflight(seed_video_id, picks, fallback, mode, config),
        }
    }
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
    }

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| DaemonError::Spawn(format!("could not start daemon process: {e}")))
}

fn init_daemon_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dirs = directories::ProjectDirs::from("", "", "ytm-tui")?;
    let dir = dirs.cache_dir().join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let guard = crate::logging::init_named(&dir, "daemon.log");
    if guard.is_some() {
        tracing::info!(path = %dir.join("daemon.log").display(), "daemon logging initialized");
    }
    guard
}

async fn wait_until_ready() -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        let last_error = match current_status().await {
            Ok(status) if status.owner_mode == InstanceMode::Daemon => return Ok(()),
            Ok(_) => return Err("another ytm-tui owner appeared while starting daemon".to_string()),
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
mod tests {
    use super::*;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn parses_start_and_resume() {
        assert_eq!(
            parse(&owned(&["start", "--resume"])),
            Ok(DaemonCommand::Start { resume: true })
        );
        assert_eq!(
            parse(&owned(&["start"])),
            Ok(DaemonCommand::Start { resume: false })
        );
    }

    #[test]
    fn parses_serve_status_and_stop() {
        assert_eq!(
            parse(&owned(&["serve", "--from-tray", "--resume"])),
            Ok(DaemonCommand::Serve {
                from_tray: true,
                resume: true
            })
        );
        assert_eq!(
            parse(&owned(&["status", "--json"])),
            Ok(DaemonCommand::Status { json: true })
        );
        assert_eq!(parse(&owned(&["stop"])), Ok(DaemonCommand::Stop));
    }

    #[test]
    fn daemon_capabilities_advertise_headless_playback() {
        assert!(daemon_capabilities().contains(&"headless-playback".to_string()));
        assert!(daemon_capabilities().contains(&"queue-control".to_string()));
    }
}
