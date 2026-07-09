use yututui::{
    auth_cli, config, daemon, doctor, i18n, media, remote,
    terminal_runtime::{self, StartupTrace},
    tools, transfer, tui, update, zoom,
};

use anyhow::Result;

fn cli_identity() -> (&'static str, &'static str) {
    match option_env!("CARGO_BIN_NAME") {
        Some("ytt-dev") => ("ytt-dev", concat!(env!("CARGO_PKG_VERSION"), "-dev")),
        _ => ("ytt", env!("CARGO_PKG_VERSION")),
    }
}

fn main() -> Result<()> {
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
                println!("       {bin} doctor [-v]      Check your environment and exit");
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
                let rest: Vec<String> = std::env::args_os()
                    .skip(2)
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                std::process::exit(remote::client::run(&rest));
            }
            // Headless daemon entrypoints must run before any TUI/terminal setup.
            "daemon" => {
                let rest: Vec<String> = std::env::args_os()
                    .skip(2)
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                std::process::exit(daemon::run_cli(&rest));
            }
            // One-shot account connections (Last.fm approval flow, ListenBrainz token,
            // Spotify PKCE) - usable without a terminal UI, e.g. for daemon-only setups.
            "auth" => {
                let rest: Vec<String> = std::env::args_os()
                    .skip(2)
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                std::process::exit(auth_cli::run(&rest));
            }
            // Playlist transfer (Spotify ↔ YTM ↔ files) - batch jobs, never the TUI.
            "transfer" => {
                let rest: Vec<String> = std::env::args_os()
                    .skip(2)
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                std::process::exit(transfer::cli::run(&rest));
            }
            "--new-instance" => new_instance = true,
            // One-shot environment diagnostic; never touches the terminal. Exits with its
            // own status code (non-zero if a required tool or directory is unusable).
            "doctor" => {
                let rest: Vec<String> = std::env::args_os()
                    .skip(2)
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                std::process::exit(doctor::run_with_args(&rest));
            }
            // Managed yt-dlp maintenance (status / forced update) - same one-shot,
            // no-terminal shape as `doctor`.
            "tools" => {
                let rest: Vec<String> = std::env::args_os()
                    .skip(2)
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                std::process::exit(tools::cli::run(&rest));
            }
            // App update check - reports whether a newer YuTuTui! release exists and how to
            // upgrade for this install method. One-shot, no terminal, like `doctor`/`tools`.
            "update" => {
                let rest: Vec<String> = std::env::args_os()
                    .skip(2)
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
                std::process::exit(update::cli::run(&rest));
            }
            // Hidden maintenance command (run by install.ps1 / Scoop post_install): registers
            // the AppUserModelId so the Windows media flyout shows "YuTuTui!" + icon instead of
            // "Unknown app". Kept out of --help; errors out on other platforms.
            "register-media-identity" => {
                let rest: Vec<String> = std::env::args_os()
                    .skip(2)
                    .map(|s| s.to_string_lossy().into_owned())
                    .collect();
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
    rt.block_on(async_main(new_instance, startup))
}

async fn async_main(new_instance: bool, mut startup: StartupTrace) -> Result<()> {
    // Load config before terminal init so mouse capture reflects it.
    let cfg = config::Config::load();
    startup.mark("config_loaded");
    // Apply the saved UI language before anything renders, so the first frame is already
    // translated. The Settings dropdown updates this global live as the user changes it.
    i18n::set_language(cfg.effective_language());
    let mouse = cfg.effective_mouse();

    // Single-instance guard + control socket. Done BEFORE the terminal is touched so the
    // "already running" notice prints to a clean screen and we never enter the alternate
    // screen just to bow out. `--new-instance` skips the guard and binds a private socket;
    // a bind failure degrades to running without remote control rather than refusing to start.
    let remote = match remote::bind_or_detect(new_instance).await {
        remote::BindOutcome::AlreadyRunning => {
            eprintln!(
                "ytt is already running.\n  \
                 Control it:  ytt -r <command>   (e.g. `ytt -r pp`, `ytt -r next`)\n  \
                 Stop it:     ytt -r quit\n  \
                 New player:  ytt --new-instance"
            );
            return Ok(());
        }
        remote::BindOutcome::Bound(server) => Some(*server),
        remote::BindOutcome::Unavailable => None,
    };
    startup.mark("remote_bound");
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
    let art_picker = Some(terminal_runtime::build_art_picker());
    startup.mark("art_picker_ready");
    // Shared by the zoom backend (draw scaling), the event translator (mouse-cell
    // mapping), and the reducer (Ctrl+wheel / Ctrl+-/= steps).
    let zoom = zoom::ZoomHandle::default();
    let mut terminal = tui::init(mouse, zoom.clone())?;
    startup.mark("terminal_ready");
    let result = terminal_runtime::run(
        &mut terminal,
        cfg,
        art_picker,
        remote,
        startup,
        zoom,
        cli_identity().1,
    )
    .await;
    tui::restore(mouse);
    result
}
