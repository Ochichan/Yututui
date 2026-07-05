use ytm_tui::{
    ai, api, app, artwork, auth_cli, config, daemon, deps, doctor, download, downloads, event,
    i18n, library, logging, lyrics, media, persist, player, playlists, remote, resolver, romanize,
    runtime, scrobble, session, signals, station, tools, transfer, tui, ui, zoom,
};

use anyhow::Result;
use app::{App, Msg};
use crossterm::event::EventStream;
use futures::StreamExt;
use player::PlayerHandle;
use ratatui_image::thread::ResizeRequest;
use runtime::RuntimeEvent;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

fn main() -> Result<()> {
    // Windows shell identity (media flyout, taskbar grouping). Before anything else —
    // the daemon path below inherits it, and it must precede any window/session.
    #[cfg(windows)]
    media::identity::adopt_process_identity();

    // `ytt --new-instance` deliberately launches a second, independent player even when one
    // is already running (bypassing the single-instance guard); threaded into `async_main`.
    let mut new_instance = false;
    if let Some(arg) = std::env::args_os().nth(1) {
        match arg.to_string_lossy().as_ref() {
            "--version" | "-V" => {
                println!("ytt {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!("ytt {}", env!("CARGO_PKG_VERSION"));
                println!();
                println!("Usage: ytt [OPTIONS]");
                println!("       ytt -r <command>     Control a running instance");
                println!("       ytt daemon <command> Manage the headless music daemon");
                println!("       ytt auth <service>   Connect Last.fm / ListenBrainz / Spotify");
                println!(
                    "       ytt transfer <cmd>   Import/export playlists (Spotify ↔ YTM ↔ files)"
                );
                println!("       ytt doctor           Check your environment and exit");
                println!(
                    "       ytt tools <cmd>      Manage the app-managed yt-dlp (status, update)"
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
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(remote::client::run(&rest));
            }
            // Headless daemon entrypoints must run before any TUI/terminal setup.
            "daemon" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(daemon::run_cli(&rest));
            }
            // One-shot account connections (Last.fm approval flow, ListenBrainz token,
            // Spotify PKCE) — usable without a terminal UI, e.g. for daemon-only setups.
            "auth" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(auth_cli::run(&rest));
            }
            // Playlist transfer (Spotify ↔ YTM ↔ files) — batch jobs, never the TUI.
            "transfer" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(transfer::cli::run(&rest));
            }
            "--new-instance" => new_instance = true,
            // One-shot environment diagnostic; never touches the terminal. Exits with its
            // own status code (non-zero if a required tool or directory is unusable).
            "doctor" => std::process::exit(doctor::run()),
            // Managed yt-dlp maintenance (status / forced update) — same one-shot,
            // no-terminal shape as `doctor`.
            "tools" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
                std::process::exit(tools::cli::run(&rest));
            }
            // Hidden maintenance command (run by install.ps1 / Scoop post_install): registers
            // the AppUserModelId so the Windows media flyout shows "YtmTui" + icon instead of
            // "Unknown app". Kept out of --help; errors out on other platforms.
            "register-media-identity" => {
                let rest: Vec<String> = std::env::args().skip(2).collect();
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
    // tty, queries, and restores the previous mode before returning — so it can't leave
    // `tui::init`'s crossterm raw-mode setup racing a half-restored terminal, and it spawns no
    // reader that could outlive it and steal input from the event loop. A failed/absent probe
    // falls back to halfblocks.
    //
    // Unconditional (not gated on `album_art`): the About card's embedded icon needs the
    // detected protocol to render at full resolution even when album art is off, and the probe
    // can't be deferred to when About opens (mid-run it would fight the event loop for the
    // terminal's query replies). Album art itself stays independently gated on
    // `effective_album_art()` — see `App::art_active` / `App::artwork_source` — so a present
    // picker never turns the feature on; it only means "a protocol is known." Repeat probes on
    // terminals without native graphics are cheap: `build_art_picker` short-circuits via the
    // 24h halfblocks cache.
    let art_picker = Some(build_art_picker());
    startup.mark("art_picker_ready");
    // Shared by the zoom backend (draw scaling), the event translator (mouse-cell
    // mapping), and the reducer (Ctrl+wheel / Ctrl+-/= steps).
    let zoom = zoom::ZoomHandle::default();
    let mut terminal = tui::init(mouse, zoom.clone())?;
    startup.mark("terminal_ready");
    let result = run(&mut terminal, cfg, art_picker, remote, startup, zoom).await;
    tui::restore(mouse);
    result
}

struct StartupTrace {
    enabled: bool,
    start: Instant,
    events: Vec<(&'static str, Duration)>,
    flushed: usize,
    logging_ready: bool,
}

impl StartupTrace {
    fn from_env() -> Self {
        Self {
            enabled: std::env::var_os("YTM_STARTUP_TRACE").is_some(),
            start: Instant::now(),
            events: Vec::new(),
            flushed: 0,
            logging_ready: false,
        }
    }

    fn mark(&mut self, label: &'static str) {
        if !self.enabled {
            return;
        }
        self.events.push((label, self.start.elapsed()));
        self.flush();
    }

    fn enable_logging(&mut self) {
        self.logging_ready = true;
        self.flush();
    }

    fn flush(&mut self) {
        if !self.enabled || !self.logging_ready {
            return;
        }
        for (label, elapsed) in &self.events[self.flushed..] {
            tracing::info!(
                target: "ytt::startup",
                label,
                elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                "startup"
            );
        }
        self.flushed = self.events.len();
    }
}

/// Build the terminal image picker, querying the terminal for its graphics protocol and
/// font size. Falls back to a halfblocks-only picker if the query fails (e.g. a terminal
/// that doesn't answer the control sequences), so album art still renders — just blocky.
fn build_art_picker() -> ratatui_image::picker::Picker {
    use ratatui_image::picker::{Picker, cap_parser::QueryStdioOptions};
    use std::io::IsTerminal;
    // Now that this runs for every launch (not just album-art-on), skip the probe entirely when
    // stdout isn't a terminal (`ytt > file`, a pipe, CI): the query bytes would otherwise be
    // written into the redirect target and the poll would stall the full timeout waiting for a
    // reply that can't come. Stdin-not-a-tty is already handled inside the probe (its
    // `tcgetattr` errors out to halfblocks); this closes the stdout-redirected-but-stdin-tty gap.
    if !std::io::stdout().is_terminal() {
        return Picker::halfblocks();
    }
    if image_protocol_override() == Some(ratatui_image::picker::ProtocolType::Halfblocks) {
        return Picker::halfblocks();
    }
    if let Some(picker) = cached_halfblocks_art_picker() {
        return picker;
    }

    let mut picker = match Picker::from_query_stdio_with_options(QueryStdioOptions {
        timeout: Duration::from_millis(250),
        ..QueryStdioOptions::default()
    }) {
        Ok(picker) => picker,
        Err(e) => {
            tracing::warn!(error = %e, "terminal graphics probe failed; album art falls back to halfblocks");
            Picker::halfblocks()
        }
    };
    if let Some(protocol) = image_protocol_override() {
        picker.set_protocol_type(protocol);
    }
    if picker.protocol_type() == ratatui_image::picker::ProtocolType::Halfblocks {
        store_halfblocks_art_picker_cache();
    }
    picker
}

fn cached_halfblocks_art_picker() -> Option<ratatui_image::picker::Picker> {
    if terminal_has_native_image_hint() {
        return None;
    }
    let path = art_picker_cache_path()?;
    let contents = std::fs::read_to_string(path).ok()?;
    let mut parts = contents.trim().split('\t');
    let version = parts.next()?;
    let key = parts.next()?;
    let protocol = parts.next()?;
    let stored_at = parts.next()?.parse::<u64>().ok()?;
    if version != "v1" || key != terminal_probe_cache_key().to_string() || protocol != "halfblocks"
    {
        return None;
    }
    let now = unix_seconds()?;
    if now.saturating_sub(stored_at) > Duration::from_secs(24 * 60 * 60).as_secs() {
        return None;
    }
    Some(ratatui_image::picker::Picker::halfblocks())
}

fn store_halfblocks_art_picker_cache() {
    if terminal_has_native_image_hint() {
        return;
    }
    let Some(path) = art_picker_cache_path() else {
        return;
    };
    let Some(now) = unix_seconds() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let contents = format!("v1\t{}\thalfblocks\t{now}\n", terminal_probe_cache_key());
    let _ = std::fs::write(path, contents);
}

fn art_picker_cache_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "ytm-tui")
        .map(|dirs| dirs.cache_dir().join("art-picker.cache"))
}

fn terminal_probe_cache_key() -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for name in [
        "TERM",
        "TERM_PROGRAM",
        "WT_SESSION",
        "TMUX",
        "KITTY_WINDOW_ID",
        "WEZTERM_EXECUTABLE",
        "KONSOLE_VERSION",
        "YTM_TUI_IMAGE_PROTOCOL",
    ] {
        name.hash(&mut hasher);
        std::env::var_os(name)
            .map(|value| value.to_string_lossy().into_owned())
            .hash(&mut hasher);
    }
    hasher.finish()
}

fn terminal_has_native_image_hint() -> bool {
    env_nonempty("KITTY_WINDOW_ID")
        || env_nonempty("WEZTERM_EXECUTABLE")
        || env_nonempty("KONSOLE_VERSION")
        || std::env::var("TERM").is_ok_and(|term| {
            term.contains("kitty") || term.contains("wezterm") || term.contains("ghostty")
        })
        || std::env::var("TERM_PROGRAM")
            .is_ok_and(|program| program.eq_ignore_ascii_case("iTerm.app"))
}

fn env_nonempty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn unix_seconds() -> Option<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn image_protocol_override() -> Option<ratatui_image::picker::ProtocolType> {
    let value = std::env::var("YTM_TUI_IMAGE_PROTOCOL").ok()?;
    parse_image_protocol_override(&value)
}

fn parse_image_protocol_override(value: &str) -> Option<ratatui_image::picker::ProtocolType> {
    use ratatui_image::picker::ProtocolType;
    match value.trim().to_ascii_lowercase().as_str() {
        "halfblocks" | "halfblock" | "blocks" | "block" => Some(ProtocolType::Halfblocks),
        "sixel" => Some(ProtocolType::Sixel),
        "kitty" => Some(ProtocolType::Kitty),
        "iterm2" | "iterm" => Some(ProtocolType::Iterm2),
        _ => None,
    }
}

fn log_art_picker(picker: Option<&ratatui_image::picker::Picker>) {
    let override_value = std::env::var("YTM_TUI_IMAGE_PROTOCOL").ok();
    let term = std::env::var("TERM").ok();
    let term_program = std::env::var("TERM_PROGRAM").ok();
    let wt_session = std::env::var("WT_SESSION").ok();
    if let Some(value) = override_value.as_deref()
        && parse_image_protocol_override(value).is_none()
    {
        tracing::warn!(
            image_protocol_override = value,
            "ignored unsupported terminal image protocol override"
        );
    }
    let Some(picker) = picker else {
        tracing::info!(
            image_protocol_override = override_value.as_deref().unwrap_or(""),
            term = term.as_deref().unwrap_or(""),
            term_program = term_program.as_deref().unwrap_or(""),
            wt_session = wt_session.as_deref().unwrap_or(""),
            "terminal graphics disabled"
        );
        return;
    };
    let font = picker.font_size();
    tracing::info!(
        protocol = ?picker.protocol_type(),
        cell_width = font.width,
        cell_height = font.height,
        capabilities = ?picker.capabilities(),
        image_protocol_override = override_value.as_deref().unwrap_or(""),
        term = term.as_deref().unwrap_or(""),
        term_program = term_program.as_deref().unwrap_or(""),
        wt_session = wt_session.as_deref().unwrap_or(""),
        "terminal graphics picker"
    );
}

/// The animation tick period for a given frame rate. `fps` is expected pre-clamped (via
/// [`config::AnimationsConfig::effective_fps`]); the `.max(1)` is a divide-by-zero guard only.
fn anim_tick_period(fps: u16) -> Duration {
    Duration::from_millis((1000 / u64::from(fps.max(1))).max(1))
}

/// Build the animation tick for `fps`. The first tick is scheduled one full period out (via
/// `interval_at`) instead of firing immediately, so rebuilding the interval when the rate changes
/// in Settings doesn't emit a spurious extra frame on the very next loop iteration. `Skip` drops
/// missed frames so a busy moment can't back up a redraw backlog.
fn anim_interval(fps: u16) -> tokio::time::Interval {
    let period = anim_tick_period(fps);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick
}

struct PerfStats {
    enabled: bool,
    last_log: Instant,
    frames: u64,
    draw_total: Duration,
    draw_max: Duration,
    art_resizes: u64,
}

impl PerfStats {
    fn from_env() -> Self {
        let enabled = std::env::var_os("YTM_PERF").is_some();
        Self {
            enabled,
            last_log: Instant::now(),
            frames: 0,
            draw_total: Duration::ZERO,
            draw_max: Duration::ZERO,
            art_resizes: 0,
        }
    }

    fn record_draw(&mut self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        self.frames += 1;
        self.draw_total += elapsed;
        self.draw_max = self.draw_max.max(elapsed);
    }

    fn record_art_resize(&mut self) {
        if self.enabled {
            self.art_resizes += 1;
        }
    }

    fn maybe_log(&mut self, app: &App) {
        if !self.enabled || self.last_log.elapsed() < Duration::from_secs(5) {
            return;
        }
        let avg_draw_ms = if self.frames == 0 {
            0.0
        } else {
            self.draw_total.as_secs_f64() * 1000.0 / self.frames as f64
        };
        let a = app.animations();
        let active_effects = [
            a.title,
            a.heart,
            a.seekbar,
            a.spinner,
            a.eq_bars,
            a.controls,
            a.border,
            a.rain,
            a.donut,
            a.visualizer,
            a.starfield,
            a.bounce,
        ]
        .into_iter()
        .filter(|on| *on)
        .count();
        tracing::info!(
            target: "ytt::perf",
            frames = self.frames,
            avg_draw_ms,
            max_draw_ms = self.draw_max.as_secs_f64() * 1000.0,
            art_resizes = self.art_resizes,
            active_effects,
            tick_fps = app.animation_tick_fps(),
            draw_fps = app.animation_draw_fps(),
            dirty = app.dirty,
            "perf window"
        );
        self.last_log = Instant::now();
        self.frames = 0;
        self.draw_total = Duration::ZERO;
        self.draw_max = Duration::ZERO;
        self.art_resizes = 0;
    }
}

fn draw_app_frame(
    terminal: &mut tui::AppTerminal,
    app: &mut App,
    perf: &mut PerfStats,
) -> std::io::Result<bool> {
    let start = perf.enabled.then(Instant::now);
    let clear_before = app.take_clear_before_draw();
    let synchronized = clear_before || app.synchronized_draw_active();
    let res = tui::draw_frame(terminal, synchronized, clear_before, |f| ui::render(f, app));
    match res {
        Ok(()) => {
            if let Some(start) = start {
                perf.record_draw(start.elapsed());
            }
            Ok(true)
        }
        Err(e) if is_transient_terminal_draw_error(&e) => {
            tracing::warn!(
                error = %e,
                "ignored transient terminal draw failure; waiting for the next input or resize"
            );
            app.dirty = false;
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

fn finish_draw_cycle(app: &mut App) {
    app.dirty = app.clear_before_draw_pending();
}

#[cfg(windows)]
fn is_transient_terminal_draw_error(error: &std::io::Error) -> bool {
    // Windows Terminal can briefly reject console writes while its taskbar/window state is
    // changing. Treat only these known Win32 console transition errors as recoverable.
    matches!(error.raw_os_error(), Some(6 | 87 | 995))
}

#[cfg(not(windows))]
fn is_transient_terminal_draw_error(_: &std::io::Error) -> bool {
    false
}

async fn run(
    terminal: &mut tui::AppTerminal,
    cfg: config::Config,
    art_picker: Option<ratatui_image::picker::Picker>,
    remote: Option<remote::RemoteServer>,
    mut startup: StartupTrace,
    zoom: zoom::ZoomHandle,
) -> Result<()> {
    // Resolve cross-platform dirs; logging + PID registry degrade gracefully if absent.
    let dirs = directories::ProjectDirs::from("", "", "ytm-tui");
    let _log_guard = dirs.as_ref().and_then(|d| {
        let dir = d.cache_dir();
        std::fs::create_dir_all(dir).ok()?;
        logging::init(dir)
    });
    startup.mark("logging_init_called");
    startup.enable_logging();
    let data_dir = dirs.as_ref().map(|d| d.data_dir().to_path_buf());
    if let Some(dir) = &data_dir {
        let _ = std::fs::create_dir_all(dir);
        // Reap any mpv leaked by a prior run that died uncatchably (SIGKILL/power loss).
        player::lifetime::reap_orphans(dir);
    }
    startup.mark("dirs_ready");

    // Wrap the terminal-restoring panic hook so a panic also kills mpv (matters under
    // `panic = "abort"`, where Drop never runs). Install after `tui::init`.
    player::lifetime::install_panic_hook();
    // Windows: kill mpv promptly on the console close button / logoff / shutdown (the
    // Job Object guarantees it regardless; this just makes teardown immediate).
    #[cfg(windows)]
    player::lifetime::install_console_ctrl_handler();

    // Background persistence actor: Save* commands hand it owned snapshots and it does
    // the debounced atomic writes off the loop task. The flush hook is installed after
    // the mpv panic hook above (last installed runs first), so on a panic pending
    // snapshots land on disk, then mpv dies, then the terminal is restored.
    let persist = persist::spawn();
    persist::install_panic_flush(persist.pending());

    // Config is loaded in `async_main` (so mouse capture can reflect it) and passed in.
    let cookie = cfg.effective_cookie();
    // Only hand mpv/yt-dlp a cookies file that actually exists: a configured/default
    // path that has not been exported yet would make yt-dlp error and break anonymous
    // playback.
    let cookies_file = cfg.effective_cookies_file().filter(|p| p.exists());
    let player_runtime = cfg.player_runtime(cookies_file.clone());
    let download_runtime = cfg.download_runtime(cookies_file.clone());
    let ai_runtime = cfg.ai_runtime();
    startup.mark("cookies_resolved");

    let mut app = App::new(player_runtime.volume);
    // Radio recorder: point it at its temp dir (wiped now — only explicitly-saved files
    // persist across runs) and probe whether this mpv supports `stream-record`.
    if let Some(d) = dirs.as_ref() {
        app.recorder.temp_dir = d.cache_dir().join("recordings");
        let _ = std::fs::remove_dir_all(&app.recorder.temp_dir);
        let _ = std::fs::create_dir_all(&app.recorder.temp_dir);
    }
    app.recorder.supported = player::mpv::stream_record_supported();
    // Zoom wiring before `apply_config`, which restores the persisted scale (the handle
    // carries the probed mode, so an unsupported terminal never sees a scale above 1).
    app.zoom = zoom.clone();
    // Hand over the terminal image picker (present only when album art is enabled).
    app.art.picker = art_picker;
    log_art_picker(app.art.picker.as_ref());
    // Load the local library (favorites + history); an absent/corrupt file → empty.
    app.library = library::Library::load();
    let session_cache = session::SessionCache::load();
    // Load per-track preference signals (plays/skips/dislikes); absent → empty.
    app.signals = signals::Signals::load();
    // Load the downloads manifest, then enrich the bare disk scan with each track's remembered
    // YouTube identity (+ real artist) so a downloaded-and-online track keeps its share link
    // after a restart; files the manifest doesn't know are recovered from their `[id]` filename.
    app.download_store = downloads::DownloadStore::load();
    let scanned = library::scan_downloads(&download_runtime.dir);
    app.library_ui.downloaded_rev = app.library_ui.downloaded_rev.wrapping_add(1);
    app.library_ui.downloaded = app.enrich_downloads(scanned);
    // Load local playlists (the DJ Gem playlist tools read/write these).
    app.playlists = playlists::Playlists::load();
    // Load the active natural-language station profile (explore level + avoided artists), if any.
    app.station = station::StationStore::load();
    app.apply_station_profile();
    // Load Latin-script display overlays for CJK titles. Source metadata stays in the library.
    app.romanization.cache = romanize::RomanizeCache::load();
    // Push persisted playback/EQ settings (preset, bands, normalize, speed, autoplay).
    app.apply_config(&cfg);
    app.restore_last_session_from_cache(&session_cache);
    startup.mark("app_state_loaded");

    let mut perf = PerfStats::from_env();
    draw_app_frame(terminal, &mut app, &mut perf)?;
    startup.mark("first_draw");
    if std::env::var_os("YTM_EXIT_AFTER_FIRST_DRAW").is_some() {
        startup.mark("exit_after_first_draw");
        return Ok(());
    }

    // Resolve which yt-dlp (managed vs system vs override) and mpv this process runs.
    // After first draw (a cold probe spawns `yt-dlp --version`, several hundred ms),
    // but before the deps preflight and the mpv spawn below, which both consume it.
    tools::init(&cfg.tools).await;
    startup.mark("tools_selected");

    // Preflight the external tools. If mpv/yt-dlp are missing, show an install hint up
    // front rather than surfacing an opaque spawn failure later.
    let mut missing = deps::missing();
    if !missing.is_empty() {
        tracing::warn!(missing = ?missing, "required external tools not found on PATH");
        // A missing yt-dlp is about to be fetched by the maintainer spawned below —
        // its "Downloading yt-dlp…" status supersedes the install nag. mpv stays a
        // hard nag; there is no managed download for it.
        if cfg.tools.managed_enabled() && tools::ytdlp::asset_name().is_some() {
            missing.retain(|bin| *bin != "yt-dlp");
        }
    }
    if !missing.is_empty() {
        app.status.text = deps::install_hint(&missing);
    }
    startup.mark("deps_checked");

    // Worker -> UI channel. Actors hold clones; the original stays alive so the
    // select! branch never resolves to `None`. Intentionally unbounded: single-session
    // event bus, and the one high-frequency producer (mpv time-pos) is coalesced to
    // ~1/sec inside the IPC actor before it ever reaches this channel.
    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel::<RuntimeEvent>();

    // Latest-only in behavior: the drain loop below always skips to the newest request.
    let (art_resize_tx, mut art_resize_rx) = mpsc::unbounded_channel::<ResizeRequest>();
    app.set_art_resize_tx(art_resize_tx);
    let art_resize_msg_tx = worker_tx.clone();
    tokio::spawn(async move {
        while let Some(mut request) = art_resize_rx.recv().await {
            while let Ok(newer) = art_resize_rx.try_recv() {
                request = newer;
            }
            match tokio::task::spawn_blocking(move || request.resize_encode()).await {
                Ok(Ok(response)) => {
                    let _ = art_resize_msg_tx.send(RuntimeEvent::ArtworkResized(response));
                }
                Ok(Err(e)) => tracing::warn!(error = ?e, "artwork resize failed"),
                Err(e) => tracing::warn!(error = ?e, "artwork resize task failed"),
            }
        }
    });

    // Signals (SIGINT/TERM/HUP) kill mpv and ask the loop to quit.
    player::lifetime::spawn_signal_handlers(runtime::sink(worker_tx.clone(), RuntimeEvent::Signal));

    // Keep the managed yt-dlp present and fresh in the background (the fix for
    // distro-frozen yt-dlp breaking playback). Never blocks startup or playback;
    // download progress and outcomes surface on the status line via Msg::Tools.
    tools::ytdlp::spawn_maintainer(
        cfg.tools.clone(),
        runtime::sink(worker_tx.clone(), RuntimeEvent::Tools),
    );

    // The remote-control accept loop is started — and the instance descriptor published — just
    // before the reducer loop below (see `remote.map(..server.start..)`), NOT here. Publishing
    // only once the app can actually service commands avoids a cold-start race where `ytt -r`
    // found a live descriptor but nothing was accepting/answering yet. The socket itself is
    // already bound (in `bind_or_detect`), so the single-instance guard is in force from launch.

    // Spawn mpv off the startup path. Until the IPC actor is ready, reducer-emitted
    // PlayerCmds are buffered by RuntimeHandles and replayed in order.
    let (player_ready_tx, mut player_ready_rx) =
        mpsc::unbounded_channel::<Result<(PlayerHandle, player::Mpv), String>>();
    let player_msg_tx = worker_tx.clone();
    let player_data_dir = data_dir.clone();
    let player_cookies_file = player_runtime.cookies_file.clone();
    let player_gapless = player_runtime.gapless;
    tokio::spawn(async move {
        let result = player::spawn(
            runtime::sink(player_msg_tx, RuntimeEvent::Player),
            player_data_dir,
            player_cookies_file,
            player_gapless,
        )
        .await
        .map_err(|e| format!("{e:#}"));
        let _ = player_ready_tx.send(result);
    });
    startup.mark("mpv_spawned");

    // Spawn the API actor. Cookie auth runs inside the actor so first render and player setup
    // don't wait on the network; commands sent before it settles stay queued in the channel.
    let api_handle = api::spawn(cookie, runtime::sink(worker_tx.clone(), RuntimeEvent::Api));
    startup.mark("api_spawned");

    // Lyrics actor: fetches synced lyrics from lrclib on demand, cached per track.
    let lyrics_handle = lyrics::spawn(runtime::sink(worker_tx.clone(), RuntimeEvent::Lyrics));

    // Artwork actor: fetches + decodes album art / thumbnails on demand (only used when
    // album art is enabled; the reducer simply never emits a fetch otherwise).
    let artwork_handle = artwork::spawn(runtime::sink(worker_tx.clone(), RuntimeEvent::Artwork));

    // Download actor: yt-dlp best-audio + tags + cover art, capped concurrency.
    let download_handle = download::spawn(
        runtime::sink(worker_tx.clone(), RuntimeEvent::Download),
        download_runtime.dir.clone(),
        download_runtime.cookies_file.clone(),
        download_runtime.max_concurrent,
    );

    // Resolver actor: pre-resolves the next track's stream URL for instant skip.
    let resolver_handle = resolver::spawn(
        runtime::sink(worker_tx.clone(), RuntimeEvent::Resolver),
        player_runtime.cookies_file.clone(),
    );

    // Gemini actor: spawned when any Gemini-backed feature is active. DJ Gem can be off while
    // title romanization is on, so UI assistant availability is tracked separately below.
    let ai_handle = ai_runtime.key.as_deref().and_then(|key| {
        ai::spawn(
            key,
            ai_runtime.model,
            runtime::sink(worker_tx.clone(), RuntimeEvent::Ai),
        )
    });
    app.ai.available = ai_runtime.assistant_enabled && ai_handle.is_some();

    // Scrobble actor (Last.fm / ListenBrainz): fed playback snapshots from the loop below,
    // delivers via a durable queue. Idles when no account is connected.
    let scrobble_handle = scrobble::spawn(
        cfg.scrobble_settings(),
        runtime::sink(worker_tx.clone(), RuntimeEvent::Scrobble),
    );

    let mut handles = runtime::RuntimeHandles::new(
        worker_tx.clone(),
        api_handle,
        lyrics_handle,
        artwork_handle,
        download_handle,
        resolver_handle,
        ai_handle,
        scrobble_handle,
        persist.clone(),
    );

    // Opt-in autoplay-on-launch: now that every player/actor handle exists, ask the loop to
    // start the restored track. Routed through the message pump so the resulting load/save
    // commands flow through the normal dispatch below (no-op when the setting is off or
    // nothing was restored).
    if cfg.effective_autoplay_on_start() {
        let _ = worker_tx.send(RuntimeEvent::App(Msg::Autoplay));
    }

    // OS media session (macOS Now Playing / Windows SMTC / Linux MPRIS): commands from
    // media keys and OS widgets come back through the normal message pump as
    // `Msg::Media`; artwork-cache results as `Msg::MediaArtworkReady`. Non-fatal when
    // the platform session can't initialize.
    let media_cmd_tx = worker_tx.clone();
    let media_art_tx = worker_tx.clone();
    let mut media = media::MediaSession::new(
        cfg.effective_media_controls(),
        move |cmd| {
            let _ = media_cmd_tx.send(RuntimeEvent::App(Msg::Media(cmd)));
        },
        move |ready| {
            let _ = media_art_tx.send(RuntimeEvent::App(Msg::MediaArtworkReady(ready)));
        },
    );
    media.publish(app.media_snapshot());
    // macOS delivers remote-command callbacks through the main run loop, which a TUI
    // never spins on its own — pump it briefly on a short interval while the session
    // is live (no-op elsewhere; the `if` guard keeps the timer parked).
    let mut media_pump = tokio::time::interval(Duration::from_millis(100));
    media_pump.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut events = EventStream::new();
    let mut input = event::Translator::default();
    let mut ime_scrub = tokio::time::interval(Duration::from_millis(80));
    ime_scrub.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Only polled while a transient status is covering the song title; lets the reducer
    // expire it (and restore the title) ~3s after it was shown. Idle otherwise.
    let mut status_tick = tokio::time::interval(Duration::from_millis(250));
    status_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // 1 Hz while a radio recording is in progress; parked otherwise (like the other guarded
    // ticks). Drives the max-duration force-split.
    let mut recording_tick = tokio::time::interval(Duration::from_secs(1));
    recording_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Drives the optional player-view animations at the configured frame rate — but only ticks
    // while `app.animation_active()` holds (player view, master + an effect enabled, a track
    // playing, focused unless the user opted out of focus-pausing). With every animation toggle
    // off (the default) the guard is false, this timer never wakes, and the loop stays exactly as
    // light as before.
    // `Skip` drops missed frames so a busy moment can't build up a backlog of redraws. The period
    // is rebuilt below whenever the user changes the rate in Settings.
    let mut anim_fps = app.animation_tick_fps();
    let mut anim_tick = anim_interval(anim_fps);
    // Every actor is up and the reducer loop is one statement away from draining `worker_rx`:
    // now start the control server and publish the instance descriptor. Doing it here (rather
    // than during setup) means a `ytt -r` that discovers the descriptor is guaranteed something
    // is accepting and the reducer will answer promptly. `_remote_guard` lives to end of `run`;
    // its Drop removes the descriptor (and best-effort the socket) on exit. `None` => remote
    // control unavailable this run; the app still works as a normal player.
    let (mut publisher, _remote_guard) = match remote {
        Some(server) => {
            let (guard, hub) = server.start(runtime::remote_sink(worker_tx.clone()));
            // The v8 publisher shares the server's session hub; it runs on this loop
            // (the owner lane) right next to the media/scrobble post-turn observers.
            (Some(remote::publish::Publisher::new(hub)), Some(guard))
        }
        None => (None, None),
    };

    while !app.should_quit {
        if app.dirty {
            if draw_app_frame(terminal, &mut app, &mut perf)? {
                finish_draw_cycle(&mut app);
            }
            perf.maybe_log(&app);
            if app.dirty {
                continue;
            }
        }

        // Mostly blocks until input or a worker message arrives. Outside text-entry fields,
        // a low-rate redraw scrubs IME preedit text that some terminals paint without
        // sending an input event to the app.
        let msg = tokio::select! {
            maybe = events.next() => match maybe {
                // The translator maps physical mouse cells onto the zoom backend's
                // virtual grid, so hit-testing (and double-click identity) stay correct
                // while the UI is scaled.
                Some(Ok(ev)) => match input.translate(ev, zoom.scale()) {
                    Some(m) => m,
                    None => continue,
                },
                Some(Err(_)) => continue,
                None => break,
            },
            Some(event) = worker_rx.recv() => {
                // Owner lane (docs/gui/02 §8/§14): session subscribe ops run here,
                // between reducer turns, and never become a Msg — the Publisher emits
                // the initial snapshots + reply from current state.
                if let RuntimeEvent::Remote(remote::server::RemoteEvent::SessionSubscribe {
                    session, frame_id, topics,
                }) = event {
                    if let Some(publisher) = publisher.as_mut() {
                        publisher.handle_subscribe(&app.core_view(), &session, frame_id, &topics);
                    }
                    continue;
                }
                event.into()
            },
            Some(result) = player_ready_rx.recv() => {
                handles.handle_player_ready(result, &player_runtime, &mut app);
                if app.dirty {
                    if draw_app_frame(terminal, &mut app, &mut perf)? {
                        finish_draw_cycle(&mut app);
                    }
                    perf.maybe_log(&app);
                }
                continue;
            },
            _ = ime_scrub.tick(), if app.should_scrub_ime_preedit() => {
                if draw_app_frame(terminal, &mut app, &mut perf)? {
                    finish_draw_cycle(&mut app);
                }
                perf.maybe_log(&app);
                continue;
            },
            _ = status_tick.tick(), if app.status_visible() => Msg::StatusTick,
            _ = anim_tick.tick(), if app.animation_active() => Msg::AnimTick,
            _ = recording_tick.tick(), if app.recorder_active() => Msg::RecordingTick,
            _ = media_pump.tick(), if media.wants_pump() => {
                media.pump();
                continue;
            },
        };

        let resized_artwork = matches!(&msg, Msg::ArtworkResized(_));
        // AnimTick/StatusTick only advance animations / expire the toast — they can't touch
        // anything a media snapshot reads, so skip rebuilding it (snapshot construction
        // allocates ~10 Strings, and AnimTick fires at up to the configured FPS).
        let media_inert = matches!(&msg, Msg::AnimTick | Msg::StatusTick);
        for cmd in app.update(msg) {
            handles.dispatch(&mut app, cmd);
        }
        if resized_artwork {
            perf.record_art_resize();
        }

        // Mirror the post-update state to the OS media session: the facade diffs, so
        // this is one comparison when nothing media-visible changed. The enabled flag
        // tracks the Settings toggle live. The scrobbler taps the same snapshot first —
        // it must keep working when media controls are disabled (publish early-returns).
        // Scrobble cadence is unaffected by the inert skip: elapsed time is credited from
        // the 1 Hz PlayerTimePos observations, which always take this path.
        media.set_enabled(app.config.effective_media_controls());
        if !media_inert {
            let snapshot = app.media_snapshot();
            handles.scrobble_observe(&snapshot);
            media.publish(snapshot);
            // v8 push: fingerprint-diffed, so this is a cheap compare when nothing
            // session-visible changed. Shares the inert gate — AnimTick/StatusTick
            // turns can't change anything the publisher watches (audited by tests).
            if let Some(publisher) = publisher.as_mut() {
                publisher.observe(&app.core_view());
            }
        }

        // The frame rate may have changed in Settings (committed to `config.animations` on close).
        // Rebuild the tick so the new rate applies without a relaunch — only when it actually
        // changed, so the common path costs one `u16` compare.
        let new_fps = app.animation_tick_fps();
        if new_fps != anim_fps {
            anim_fps = new_fps;
            app.reset_animation_cadence();
            anim_tick = anim_interval(anim_fps);
        }

        if app.dirty && draw_app_frame(terminal, &mut app, &mut perf)? {
            finish_draw_cycle(&mut app);
        }
        perf.maybe_log(&app);
    }

    // Tell v8 sessions the owner is going away (`system` event + Goodbye) before the
    // guard below removes the descriptor — clients start their reconnect/daemon logic
    // off this, not off a bare EOF (docs/gui/02 §7).
    if let Some(publisher) = publisher.as_ref() {
        publisher.shutting_down();
    }
    // Close the video overlay (if one is open) so it doesn't outlive the app. This is the single
    // cleanup chokepoint: every quit path just sets `should_quit` and falls out of the loop here.
    app.close_video();
    // Belt-and-suspenders: persist the last UI mode + library/signals/downloads on a clean exit
    // too. Send fresh quit-time snapshots, then drain the actor — the flush also lands any still-
    // debounced config/playlists/romanize writes. Do NOT save directly here instead: an older
    // pending snapshot in the actor could then overwrite the newer direct write.
    persist.save(persist::Snapshot::Session(app.session_cache_snapshot()));
    persist.save(persist::Snapshot::Library(app.library.clone()));
    persist.save(persist::Snapshot::Signals(app.signals.clone()));
    persist.save(persist::Snapshot::Downloads(app.download_store.clone()));
    if !persist.flush(Duration::from_secs(5)).await {
        // Timed out (disk hung?). Direct fallback is safe: the actor holds the same
        // quit-time snapshots, so a late actor write can't clobber anything newer.
        tracing::warn!("persist flush timed out at quit; writing directly");
        let _ = app.session_cache_snapshot().save();
        let _ = app.library.save();
        let _ = app.signals.save();
        let _ = app.download_store.save();
    }
    // Give queued scrobbles one bounded delivery attempt (they're already durable on
    // disk either way — leftovers flush on the next launch).
    handles.scrobble_shutdown(Duration::from_millis(1500)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use ratatui_image::picker::ProtocolType;

    use super::parse_image_protocol_override;

    #[test]
    fn image_protocol_override_accepts_supported_protocol_names() {
        assert_eq!(
            parse_image_protocol_override("halfblocks"),
            Some(ProtocolType::Halfblocks)
        );
        assert_eq!(
            parse_image_protocol_override("sixel"),
            Some(ProtocolType::Sixel)
        );
        assert_eq!(
            parse_image_protocol_override("kitty"),
            Some(ProtocolType::Kitty)
        );
        assert_eq!(
            parse_image_protocol_override("iterm2"),
            Some(ProtocolType::Iterm2)
        );
        assert_eq!(
            parse_image_protocol_override("  SIXEL  "),
            Some(ProtocolType::Sixel)
        );
        assert_eq!(parse_image_protocol_override("bad"), None);
    }
}
