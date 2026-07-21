use yututui::{
    auth_cli,
    cli_capability::{OneShotCommand, collect_lossy_cli_args, interactive_persistence_capability},
    daemon, doctor, i18n, media, persist, player, remote, second_launch,
    terminal_runtime::{self, StartupTrace},
    tools, transfer, tui, update, zoom,
};

use anyhow::Result;
use std::time::{Duration, Instant};

/// The owner loop already gives tracked blocking work 3.5 seconds to finish. This small outer
/// grace only prevents a timed-out `spawn_blocking` closure from making `Runtime::drop` wait
/// without a bound; normal shutdown should have no work left by the time it starts.
const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

fn merge_terminal_restore(
    runtime_result: Result<()>,
    restore_result: std::io::Result<()>,
) -> Result<()> {
    match (runtime_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(runtime_error), Ok(())) => Err(runtime_error),
        (Ok(()), Err(restore_error)) => Err(restore_error.into()),
        (Err(runtime_error), Err(restore_error)) => Err(runtime_error.context(format!(
            "bounded terminal restore also failed: {restore_error}"
        ))),
    }
}

fn initialize_cli_persistence(command: OneShotCommand, args: &[String]) -> bool {
    let capability = command.persistence_capability(args);
    if !capability.requires_persistence() {
        return true;
    }
    let initialized = if capability.allows_writes() {
        persist::initialize_persistence_writer(false)
    } else {
        persist::initialize_persistence_reader()
    };
    match initialized {
        Ok(_) => {
            if capability.allows_writes()
                && let Err(error) = persist::preflight_all_startup_stores()
            {
                eprintln!("ytt {}: {error}", command.label());
                return false;
            }
            true
        }
        Err(error) => {
            eprintln!("ytt {}: {error}", command.label());
            false
        }
    }
}

fn initialize_interactive_persistence(
    read_only: bool,
) -> std::io::Result<persist::PersistenceAccess> {
    let capability = interactive_persistence_capability(read_only);
    debug_assert!(capability.requires_persistence());
    if capability.allows_writes() {
        persist::initialize_persistence_writer(false)
    } else {
        // `--new-instance` (and the chooser's read-only path) is an explicit observational
        // secondary, even when no primary happens to be alive. Never opportunistically promote
        // it to the shared-root writer: doing so makes the same command mutate or migrate
        // state depending on startup timing.
        persist::initialize_persistence_reader()
    }
}

/// Bounded retry over [`initialize_interactive_persistence`] for the restart takeover.
/// `initialize_writer_state` leaves the process lease `Uninitialized` on a failed acquire,
/// so retrying is safe.
fn initialize_interactive_persistence_with_retry(
    read_only: bool,
    attempts: u32,
) -> std::io::Result<persist::PersistenceAccess> {
    let mut last = None;
    for attempt in 0..attempts.max(1) {
        match initialize_interactive_persistence(read_only) {
            Ok(access) => return Ok(access),
            Err(error) => {
                last = Some(error);
                if attempt + 1 < attempts {
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
    }
    Err(last.expect("at least one attempt always runs"))
}

mod data_cli;

fn cli_identity() -> (&'static str, &'static str) {
    match option_env!("CARGO_BIN_NAME") {
        Some("ytt-dev") => ("ytt-dev", concat!(env!("CARGO_PKG_VERSION"), "-dev")),
        _ => ("ytt", env!("CARGO_PKG_VERSION")),
    }
}

fn main() -> Result<()> {
    #[cfg(windows)]
    let pause_after_exit = explorer_double_click_launch();
    let result = run();
    #[cfg(windows)]
    if pause_after_exit {
        pause_explorer_console(&result);
        if result.is_err() {
            std::process::exit(1);
        }
    }
    result
}

fn run() -> Result<()> {
    // The private mpv guardian must run before process identity, persistence, a Tokio runtime,
    // or terminal setup. Its only authority comes from the one-shot protocol on inherited
    // stdio; it is intentionally absent from help and is never a user-facing subcommand.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__mpv-guardian")) {
        std::process::exit(player::guardian::run_cli());
    }

    // Windows shell identity (media flyout, taskbar grouping). Before anything else:
    // the daemon path below inherits it, and it must precede any window/session.
    #[cfg(windows)]
    media::identity::adopt_process_identity();

    // `ytt --new-instance` deliberately launches a second, independent player even when one
    // is already running (bypassing the single-instance guard); threaded into `async_main`.
    let mut new_instance = false;
    if let Some(arg) = std::env::args_os().nth(1) {
        match arg.to_string_lossy().as_ref() {
            "--version" | "-V" => {
                let (bin, version) = cli_identity();
                println!("{bin} {version}");
                return Ok(());
            }
            "--help" | "-h" => {
                let (bin, version) = cli_identity();
                println!("{bin} {version}");
                println!();
                println!("Usage: {bin} [OPTIONS]");
                println!("       {bin} -r <command>     Control a running instance");
                println!("       {bin} daemon <command> Manage the headless music daemon");
                println!("       {bin} auth <service>   Connect Last.fm / ListenBrainz / Spotify");
                println!(
                    "       {bin} transfer <cmd>   Import/export playlists (Spotify ↔ YTM ↔ files)"
                );
                println!("       {bin} data <cmd>       Export portable personal data");
                println!("       {bin} doctor [-v]      Check your environment and exit");
                println!("       {bin} doctor audio [-v]");
                println!("       {bin} doctor privacy [--cleanup]");
                println!("       {bin} doctor terminal --json");
                println!("                             Report terminal capability hints");
                println!(
                    "       {bin} tools <cmd>      Manage the app-managed yt-dlp (status, update)"
                );
                println!();
                println!("Launch the terminal YouTube Music player.");
                println!();
                println!("Options:");
                println!(
                    "  -r, --remote <command>   Send a command to a running instance (see `ytt -r --help`)"
                );
                println!(
                    "      --new-instance       Start a second player even if one is already running"
                );
                println!("  -V, --version            Print version and exit");
                println!("  -h, --help               Print this help and exit");
                return Ok(());
            }
            // Remote-control client: connect to a running instance, print the result, exit
            // with its status code. This path never builds the multi-thread runtime and
            // never touches terminal raw mode / the alternate screen.
            "-r" | "--remote" => {
                let rest = collect_lossy_cli_args(std::env::args_os().skip(2));
                std::process::exit(remote::client::run(&rest));
            }
            // Headless daemon entrypoints must run before any TUI/terminal setup.
            "daemon" => {
                let rest = collect_lossy_cli_args(std::env::args_os().skip(2));
                std::process::exit(daemon::run_cli(&rest));
            }
            // One-shot account connections (Last.fm approval flow, ListenBrainz token,
            // Spotify PKCE) - usable without a terminal UI, e.g. for daemon-only setups.
            "auth" => {
                let rest = collect_lossy_cli_args(std::env::args_os().skip(2));
                if !initialize_cli_persistence(OneShotCommand::Auth, &rest) {
                    std::process::exit(1);
                }
                std::process::exit(auth_cli::run(&rest));
            }
            // Playlist transfer (Spotify ↔ YTM ↔ files) - batch jobs, never the TUI.
            "transfer" => {
                let rest = collect_lossy_cli_args(std::env::args_os().skip(2));
                if !initialize_cli_persistence(OneShotCommand::Transfer, &rest) {
                    std::process::exit(1);
                }
                std::process::exit(transfer::cli::run(&rest));
            }
            // Portable settings/library export. This one-shot path never initializes the TUI;
            // it delegates to a capable live owner or snapshots persisted state when offline.
            "data" => {
                let rest = collect_lossy_cli_args(std::env::args_os().skip(2));
                std::process::exit(data_cli::run(&rest));
            }
            "--new-instance" => new_instance = true,
            // One-shot environment diagnostic; never touches the terminal. Exits with its
            // own status code (non-zero if a required tool or directory is unusable).
            "doctor" => {
                let rest = collect_lossy_cli_args(std::env::args_os().skip(2));
                if !initialize_cli_persistence(OneShotCommand::Doctor, &rest) {
                    std::process::exit(1);
                }
                std::process::exit(doctor::run_with_args(&rest));
            }
            // Managed yt-dlp maintenance (status / forced update) - same one-shot,
            // no-terminal shape as `doctor`.
            "tools" => {
                let rest = collect_lossy_cli_args(std::env::args_os().skip(2));
                if !initialize_cli_persistence(OneShotCommand::Tools, &rest) {
                    std::process::exit(1);
                }
                std::process::exit(tools::cli::run(&rest));
            }
            // App update check - reports whether a newer YuTuTui! release exists and how to
            // upgrade for this install method. One-shot, no terminal, like `doctor`/`tools`.
            "update" => {
                let rest = collect_lossy_cli_args(std::env::args_os().skip(2));
                if !initialize_cli_persistence(OneShotCommand::Update, &rest) {
                    std::process::exit(1);
                }
                std::process::exit(update::cli::run(&rest));
            }
            // Hidden maintenance command (run by install.ps1 / Scoop post_install): registers
            // the AppUserModelId so the Windows media flyout shows "YuTuTui!" + icon instead of
            // "Unknown app". Kept out of --help; errors out on other platforms.
            "register-media-identity" => {
                let rest = collect_lossy_cli_args(std::env::args_os().skip(2));
                std::process::exit(media::identity::register_cli(&rest));
            }
            _ => {}
        }
    }

    let mut startup = StartupTrace::from_env();
    startup.mark("args_parsed");

    // Custom runtime: 2 workers + 512 KB stacks keeps stack RSS ~1.5 MB (vs ~4.5 MB
    // at the 2 MB default). The render loop runs on the main task; actors run on the
    // worker threads so a blocked IPC read never stalls rendering.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(512 * 1024)
        .enable_all()
        .build()?;
    startup.mark("runtime_built");
    let result = rt.block_on(async_main(new_instance, startup));
    rt.shutdown_timeout(RUNTIME_SHUTDOWN_GRACE);
    result
}

#[cfg(windows)]
fn explorer_double_click_launch() -> bool {
    use sysinfo::{Pid, ProcessesToUpdate, System};

    if std::env::args_os().count() != 1 {
        return false;
    }
    let mut system = System::new();
    let current = Pid::from_u32(std::process::id());
    system.refresh_processes(ProcessesToUpdate::Some(&[current]), true);
    let Some(parent) = system.process(current).and_then(sysinfo::Process::parent) else {
        return false;
    };
    system.refresh_processes(ProcessesToUpdate::Some(&[parent]), true);
    let parent_name = system
        .process(parent)
        .map(|process| process.name().to_string_lossy().into_owned());
    should_pause_explorer_launch(1, parent_name.as_deref())
}

#[cfg(any(windows, test))]
fn should_pause_explorer_launch(argument_count: usize, parent_name: Option<&str>) -> bool {
    argument_count == 1 && parent_name.is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"))
}

#[cfg(windows)]
fn pause_explorer_console(result: &Result<()>) {
    use std::io::Write;

    if let Err(error) = result {
        eprintln!("YuTuTui! could not start / 시작하지 못했습니다: {error:#}");
    } else {
        println!("YuTuTui! closed / YuTuTui!가 종료되었습니다.");
    }
    print!("Press Enter to close this window / 창을 닫으려면 Enter를 누르세요: ");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    let _ = std::io::stdin().read_line(&mut input);
}

async fn async_main(new_instance: bool, mut startup: StartupTrace) -> Result<()> {
    // Register termination streams before any pre-TUI chooser or capability probe can enter raw
    // mode. Main retains this same owner through final terminal restoration so a first signal
    // cannot be forgotten by re-registering or by stopping escalation early in teardown.
    let terminal_signals = terminal_runtime::InteractiveSignals::install()?;
    let early_shutdown = terminal_signals.shutdown_latch();

    // Single-instance guard + control socket. Done BEFORE the terminal is touched so the
    // second-launch UX renders on a clean screen and, critically, before any config loader
    // can migrate or repair state. `--new-instance` skips the guard and binds a private
    // socket; a bind failure degrades to running without remote control rather than refusing.
    let (remote, read_only, via_restart) = match remote::bind_or_detect(new_instance).await {
        remote::BindOutcome::AlreadyRunning => {
            match second_launch::handle_already_running(&early_shutdown).await {
                second_launch::Outcome::Exit => {
                    terminal_signals.shutdown().await;
                    return Ok(());
                }
                second_launch::Outcome::Continue { remote, read_only } => {
                    (remote.map(|server| *server), read_only, !read_only)
                }
            }
        }
        remote::BindOutcome::Bound(server) => (Some(*server), new_instance, false),
        remote::BindOutcome::Unavailable => (None, new_instance, false),
    };
    if terminal_signals.shutdown_requested() {
        terminal_signals.shutdown().await;
        return Ok(());
    }
    // Advertise where this TUI lives so a later second launch can focus this terminal.
    // Only the interactive player: the daemon path never attaches a hint, and a private
    // `--new-instance` socket never publishes the descriptor anyway.
    let remote = remote.map(|server| {
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut hint = remote::proto::HostTerminalHint::capture_from_env();
        #[cfg(windows)]
        {
            hint.console_hwnd = second_launch::focus_windows::console_hwnd();
            hint.terminal_host_pid = second_launch::focus_windows::terminal_host_pid();
        }
        server.with_host_terminal(hint)
    });
    startup.mark("remote_bound");

    // The primary owner must hold one process-wide writer lease before Config::load can migrate
    // anything. A deliberate secondary player remains available, but only as strict read-only;
    // normal owners and mutating CLI paths fail rather than accepting state a newer epoch discards.
    // A restart takeover retries for a few seconds: the restart sequence waits for the
    // owner's pid to exit when the descriptor names one, but a stale/foreign descriptor
    // degrades that wait to a fixed grace — the flock lease is then the only arbiter, and
    // the old owner may still be flushing behind its already-released socket.
    let persistence_access = if via_restart {
        initialize_interactive_persistence_with_retry(read_only, 10)?
    } else {
        initialize_interactive_persistence(read_only)?
    };
    let (cfg, persistent_state) = persist::load_verified_startup_state()?;
    startup.mark("config_loaded");
    // Apply the saved UI language before anything renders, so the first frame is already
    // translated. The Settings dropdown updates this global live as the user changes it.
    i18n::set_language(cfg.effective_language());
    let mouse = cfg.effective_mouse();
    terminal_signals.set_mouse(mouse);
    if terminal_signals.shutdown_requested() {
        terminal_signals.shutdown().await;
        return Ok(());
    }
    // Validate stdin/stdout identity before a probe can raw-mode one TTY and write queries to
    // another. This is read-only and opens no shared/blocking descriptor.
    tui::preflight_interactive_terminal()?;
    // Probe the terminal for its graphics protocol + font size, and do it BEFORE `tui::init`
    // so the 1x1 probe image and its cursor-position reports never land on the app's alternate
    // screen. The probe is fully synchronous (see `ratatui_image::picker`): it raw-modes the
    // tty, queries, and restores the previous mode before returning. The whole synchronous
    // transaction runs on a bounded worker; timeout/shutdown restores the pre-probe termios and
    // fails startup instead of letting `tui::init` race a detached raw-mode owner. Ordinary
    // unsupported/absent capabilities still fall back to halfblocks.
    //
    // Unconditional (not gated on `album_art`): the About card's embedded icon needs the
    // detected protocol to render at full resolution even when album art is off, and the probe
    // can't be deferred to when About opens (mid-run it would fight the event loop for the
    // terminal's query replies). Album art itself stays independently gated on
    // `effective_album_art()` - see `App::art_active` / `App::artwork_source` - so a present
    // picker never turns the feature on; it only means "a protocol is known." Repeat probes on
    // terminals without native graphics are cheap: `build_art_picker` short-circuits via the
    // 24h halfblocks cache.
    let terminal_negotiation_deadline = Instant::now() + terminal_runtime::STARTUP_OUTPUT_TIMEOUT;
    let art_picker = match terminal_runtime::build_art_picker_with_access_until_bounded(
        &persistence_access,
        terminal_negotiation_deadline,
        terminal_signals.shutdown_latch(),
        terminal_signals.query_cancellation(),
    )
    .await
    {
        Ok(picker) => Some(picker),
        Err(_) if terminal_signals.shutdown_requested() => {
            terminal_signals.shutdown().await;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    startup.mark("art_picker_ready");
    if terminal_signals.shutdown_requested() {
        terminal_signals.shutdown().await;
        return Ok(());
    }
    // Shared by the zoom backend (draw scaling), the event translator (mouse-cell
    // mapping), and the reducer (Ctrl+wheel / Ctrl+-/= steps).
    let zoom = zoom::ZoomHandle::default();
    let (mut terminal, keyboard_input_mode) =
        match tui::init_until(mouse, zoom.clone(), terminal_negotiation_deadline) {
            Ok(initialized) => initialized,
            Err(_) if terminal_signals.shutdown_requested() => {
                terminal_signals.shutdown().await;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
    startup.mark("terminal_ready");
    if terminal_signals.shutdown_requested() {
        drop(terminal);
        let restored = tui::restore(mouse);
        terminal_signals.shutdown().await;
        return restored.map_err(Into::into);
    }
    let runtime_shutdown = terminal_signals.shutdown_latch();
    let result = terminal_runtime::run(
        &mut terminal,
        terminal_runtime::TerminalStartupState::new(
            cfg,
            persistent_state,
            persistence_access,
            keyboard_input_mode,
            runtime_shutdown,
        ),
        art_picker,
        remote,
        startup,
        zoom,
        cli_identity().1,
    )
    .await;
    // Successful draws flush at ratatui's normal frame boundary. AppTerminal arms its per-instance
    // drop fence here, so any failed-frame remainder and Ratatui's destructor-time cursor write are
    // discarded before the bounded restore becomes the sole physical terminal-output owner.
    drop(terminal);
    let restored = tui::restore(mouse);
    terminal_signals.shutdown().await;
    merge_terminal_restore(result, restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use yututui::cli_capability::CliPersistenceCapability;

    #[test]
    fn explicit_new_instance_is_always_an_observational_reader() {
        assert_eq!(
            interactive_persistence_capability(true),
            CliPersistenceCapability::ReadOnly
        );
    }

    #[test]
    fn ordinary_interactive_owner_requests_the_writer_capability() {
        assert_eq!(
            interactive_persistence_capability(false),
            CliPersistenceCapability::Writer
        );
    }

    #[test]
    fn terminal_restore_failure_is_returned_without_hiding_a_runtime_failure() {
        let restore_only = merge_terminal_restore(
            Ok(()),
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "normal terminal restore timed out at leave-alt",
            )),
        )
        .unwrap_err();
        assert!(restore_only.to_string().contains("leave-alt"));

        let combined = merge_terminal_restore(
            Err(anyhow::anyhow!("primary terminal liveness failure")),
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "restore phase timed out",
            )),
        )
        .unwrap_err();
        assert_eq!(
            combined.to_string(),
            "bounded terminal restore also failed: restore phase timed out"
        );
        assert!(format!("{combined:#}").contains("primary terminal liveness failure"));
    }

    #[test]
    fn explorer_double_click_pause_policy_is_narrow() {
        assert!(should_pause_explorer_launch(1, Some("explorer.exe")));
        assert!(should_pause_explorer_launch(1, Some("EXPLORER.EXE")));
        assert!(!should_pause_explorer_launch(2, Some("explorer.exe")));
        assert!(!should_pause_explorer_launch(
            1,
            Some("WindowsTerminal.exe")
        ));
        assert!(!should_pause_explorer_launch(1, None));
    }
}
