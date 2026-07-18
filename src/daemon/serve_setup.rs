use super::engine;
use super::events::{DaemonEvent, DaemonEventSender, record_daemon_event};
use super::{EXIT_TRANSPORT, EXIT_USAGE, await_owner_handler, daemon_capabilities};
use crate::remote::proto::InstanceMode;
use crate::remote::server;

macro_rules! transport_or_return {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(error) => {
                eprintln!("ytt daemon: {error}");
                return EXIT_TRANSPORT;
            }
        }
    };
    ($result:expr, $context:literal) => {
        match $result {
            Ok(value) => value,
            Err(error) => {
                eprintln!(concat!("ytt daemon: ", $context, "{}"), error);
                return EXIT_TRANSPORT;
            }
        }
    };
}

pub(super) use transport_or_return;

pub(super) async fn bind_endpoint() -> Result<server::RemoteServer, i32> {
    let server = match server::bind_or_detect(false).await {
        server::BindOutcome::AlreadyRunning => {
            eprintln!("ytt daemon: YuTuTui! is already running.");
            return Err(EXIT_USAGE);
        }
        server::BindOutcome::Unavailable => {
            eprintln!("ytt daemon: could not bind remote control socket.");
            return Err(EXIT_TRANSPORT);
        }
        server::BindOutcome::Bound(server) => *server,
    };
    Ok(server.with_instance_metadata(InstanceMode::Daemon, daemon_capabilities()))
}

pub(super) fn initialize_persistence() -> Result<(), String> {
    crate::persist::initialize_persistence_writer(false).map_err(|error| error.to_string())?;
    crate::persist::ensure_startup_recovery_coherent().map_err(|error| error.to_string())
}

pub(super) fn spawn_signals(
    event_tx: &DaemonEventSender,
    shutdown: &crate::player::lifetime::ShutdownLatch,
) -> std::io::Result<crate::util::background_task::BackgroundTask> {
    let signal_event_tx = event_tx.clone();
    crate::player::lifetime::spawn_signal_handlers(
        shutdown.clone(),
        move |_| {
            // Compatibility/observability event only: the owner loop waits on `shutdown`
            // directly, so saturation here cannot delay teardown.
            record_daemon_event(&signal_event_tx, DaemonEvent::Signal);
        },
        // Second termination signal while the owner loop is wedged: mpv was already killed by
        // the cooperative path; there is no terminal to restore, so just refuse to hang.
        |code| {
            crate::player::lifetime::kill_mpv_now();
            std::process::exit(code);
        },
    )
}

pub(super) async fn start_engine(
    resume: bool,
    player_event_tx: DaemonEventSender,
    shutdown: &crate::player::lifetime::ShutdownLatch,
) -> Option<Result<engine::DaemonEngine, engine::EngineError>> {
    await_owner_handler(
        shutdown,
        engine::DaemonEngine::start(engine::EngineOptions { resume }, move |event| {
            record_daemon_event(&player_event_tx, DaemonEvent::Player(event));
        }),
    )
    .await
}
