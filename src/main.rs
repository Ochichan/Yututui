use yututui::{
    auth_cli,
    cli_capability::{OneShotCommand, collect_lossy_cli_args, interactive_persistence_capability},
    daemon, doctor, i18n, media, persist, remote,
    terminal_runtime::{self, StartupTrace},
    tools, transfer, tui, update, zoom,
};

use anyhow::Result;
use std::time::Duration;

/// The owner loop already gives tracked blocking work 3.5 seconds to finish. This small outer
/// grace only prevents a timed-out `spawn_blocking` closure from making `Runtime::drop` wait
/// without a bound; normal shutdown should have no work left by the time it starts.
const RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
const ALREADY_RUNNING_NOTICE: &str = "ytt is already running.\n  \
                                      Control it:  ytt -r <command>   (e.g. `ytt -r pp`, `ytt -r next`)\n  \
                                      Stop it:     ytt -r quit";

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
    new_instance: bool,
) -> std::io::Result<persist::PersistenceAccess> {
    let capability = interactive_persistence_capability(new_instance);
    debug_assert!(capability.requires_persistence());
    if capability.allows_writes() {
        persist::initialize_persistence_writer(false)
    } else {
        // `--new-instance` is an explicit observational secondary, even when no primary happens
        // to be alive. Never opportunistically promote it to the shared-root writer: doing so
        // makes the same command mutate or migrate state depending on startup timing.
        persist::initialize_persistence_reader()
    }
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
    // Single-instance guard + control socket. Done BEFORE the terminal is touched so the
    // "already running" notice prints to a clean screen and, critically, before any config
    // loader can migrate or repair state. `--new-instance` skips the guard and binds a private
    // socket; a bind failure degrades to running without remote control rather than refusing.
    let remote = match remote::bind_or_detect(new_instance).await {
        remote::BindOutcome::AlreadyRunning => {
            eprintln!("{ALREADY_RUNNING_NOTICE}");
            return Ok(());
        }
        remote::BindOutcome::Bound(server) => Some(*server),
        remote::BindOutcome::Unavailable => None,
    };
    startup.mark("remote_bound");

    // The primary owner must hold one process-wide writer lease before Config::load can migrate
    // anything. A deliberate secondary player remains available, but only as strict read-only;
    // normal owners and mutating CLI paths fail rather than accepting state a newer epoch discards.
    let persistence_access = initialize_interactive_persistence(new_instance)?;
    let (cfg, persistent_state) = persist::load_verified_startup_state()?;
    startup.mark("config_loaded");
    // Apply the saved UI language before anything renders, so the first frame is already
    // translated. The Settings dropdown updates this global live as the user changes it.
    i18n::set_language(cfg.effective_language());
    let mouse = cfg.effective_mouse();
    // Probe the terminal for its graphics protocol + font size, and do it BEFORE `tui::init`
    // so the 1x1 probe image and its cursor-position reports never land on the app's alternate
    // screen. The probe is fully synchronous (see `ratatui_image::picker`): it raw-modes the
    // tty, queries, and restores the previous mode before returning - so it can't leave
    // `tui::init`'s crossterm raw-mode setup racing a half-restored terminal, and it spawns no
    // reader that could outlive it and steal input from the event loop. A failed/absent probe
    // falls back to halfblocks.
    //
    // Unconditional (not gated on `album_art`): the About card's embedded icon needs the
    // detected protocol to render at full resolution even when album art is off, and the probe
    // can't be deferred to when About opens (mid-run it would fight the event loop for the
    // terminal's query replies). Album art itself stays independently gated on
    // `effective_album_art()` - see `App::art_active` / `App::artwork_source` - so a present
    // picker never turns the feature on; it only means "a protocol is known." Repeat probes on
    // terminals without native graphics are cheap: `build_art_picker` short-circuits via the
    // 24h halfblocks cache.
    let art_picker = Some(terminal_runtime::build_art_picker_with_access(
        &persistence_access,
    ));
    startup.mark("art_picker_ready");
    // Shared by the zoom backend (draw scaling), the event translator (mouse-cell
    // mapping), and the reducer (Ctrl+wheel / Ctrl+-/= steps).
    let zoom = zoom::ZoomHandle::default();
    let mut terminal = tui::init(mouse, zoom.clone())?;
    startup.mark("terminal_ready");
    let result = terminal_runtime::run(
        &mut terminal,
        terminal_runtime::TerminalStartupState::new(cfg, persistent_state, persistence_access),
        art_picker,
        remote,
        startup,
        zoom,
        cli_identity().1,
    )
    .await;
    // Keep a failed frame's buffered remainder on the alternate screen during normal teardown.
    // Successful draws flush at ratatui's normal frame boundary; the panic-safe writer separately
    // discards its remainder while unwinding, after ratatui's panic hook has already restored.
    drop(terminal);
    tui::restore(mouse);
    result
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
    fn already_running_notice_keeps_controls_without_advertising_new_instance() {
        assert!(ALREADY_RUNNING_NOTICE.contains("Control it:"));
        assert!(ALREADY_RUNNING_NOTICE.contains("ytt -r <command>"));
        assert!(ALREADY_RUNNING_NOTICE.contains("Stop it:"));
        assert!(ALREADY_RUNNING_NOTICE.contains("ytt -r quit"));
        assert!(!ALREADY_RUNNING_NOTICE.contains("--new-instance"));
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
