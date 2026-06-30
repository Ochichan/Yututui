mod ai;
mod api;
mod app;
mod artwork;
mod config;
mod deps;
mod doctor;
mod download;
mod downloads;
mod eq;
mod event;
mod i18n;
mod keymap;
mod library;
mod logging;
mod lyrics;
mod player;
mod playlists;
mod queue;
mod remote;
mod resolver;
mod romanize;
mod search_source;
mod settings;
mod signals;
mod station;
mod streaming;
mod theme;
mod tui;
mod ui;
mod util;

use anyhow::Result;
use app::{App, Cmd, Msg};
use crossterm::event::EventStream;
use futures::StreamExt;
use player::{PlayerCmd, PlayerHandle};
use ratatui_image::thread::ResizeRequest;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

fn main() -> Result<()> {
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
                println!("       ytt doctor           Check your environment and exit");
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
            "--new-instance" => new_instance = true,
            // One-shot environment diagnostic; never touches the terminal. Exits with its
            // own status code (non-zero if a required tool or directory is unusable).
            "doctor" => std::process::exit(doctor::run()),
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
    // Album art is opt-in: only probe the terminal for its graphics protocol + font size
    // when the user enabled it, and do it BEFORE `tui::init` so the 1x1 probe image and its
    // cursor-position reports never land on the app's alternate screen. The probe is fully
    // synchronous (see `ratatui_image::picker`): it raw-modes the tty, queries, and restores the
    // previous mode before returning — so it can't leave `tui::init`'s crossterm raw-mode setup
    // racing a half-restored terminal, and it spawns no reader that could outlive it and steal
    // input from the event loop. A failed/absent probe falls back to halfblocks.
    let art_picker = if cfg.effective_album_art() {
        Some(build_art_picker())
    } else {
        None
    };
    startup.mark("art_picker_ready");
    let mut terminal = tui::init(mouse)?;
    startup.mark("terminal_ready");
    let result = run(&mut terminal, cfg, art_picker, remote, startup).await;
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
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    perf: &mut PerfStats,
) -> std::io::Result<()> {
    let start = perf.enabled.then(Instant::now);
    let clear_before = app.take_clear_before_draw();
    let synchronized = clear_before || app.synchronized_draw_active();
    let res = tui::draw_frame(terminal, synchronized, clear_before, |f| ui::render(f, app));
    if res.is_ok()
        && let Some(start) = start
    {
        perf.record_draw(start.elapsed());
    }
    res
}

fn finish_draw_cycle(app: &mut App) {
    app.dirty = app.clear_before_draw_pending();
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    cfg: config::Config,
    art_picker: Option<ratatui_image::picker::Picker>,
    remote: Option<remote::RemoteServer>,
    mut startup: StartupTrace,
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

    // Config is loaded in `async_main` (so mouse capture can reflect it) and passed in.
    let cookie = cfg.effective_cookie();
    // Only hand mpv/yt-dlp a cookies file that actually exists: a configured/default
    // path that has not been exported yet would make yt-dlp error and break anonymous
    // playback.
    let cookies_file = cfg.effective_cookies_file().filter(|p| p.exists());
    startup.mark("cookies_resolved");

    let mut app = App::new(cfg.volume);
    // Hand over the terminal image picker (present only when album art is enabled).
    app.art.picker = art_picker;
    log_art_picker(app.art.picker.as_ref());
    // Load the local library (favorites + history); an absent/corrupt file → empty.
    app.library = library::Library::load();
    // Load per-track preference signals (plays/skips/dislikes); absent → empty.
    app.signals = signals::Signals::load();
    // Load the downloads manifest, then enrich the bare disk scan with each track's remembered
    // YouTube identity (+ real artist) so a downloaded-and-online track keeps its share link
    // after a restart; files the manifest doesn't know are recovered from their `[id]` filename.
    app.download_store = downloads::DownloadStore::load();
    let scanned = library::scan_downloads(&cfg.effective_download_dir());
    app.library_ui.downloaded = app.enrich_downloads(scanned);
    app.restore_last_played_from_library();
    // Load local playlists (the DJ Gem playlist tools read/write these).
    app.playlists = playlists::Playlists::load();
    // Load the active natural-language station profile (explore level + avoided artists), if any.
    app.station = station::StationStore::load();
    app.apply_station_profile();
    // Load Latin-script display overlays for CJK titles. Source metadata stays in the library.
    app.romanization.cache = romanize::RomanizeCache::load();
    // Push persisted playback/EQ settings (preset, bands, normalize, speed, autoplay).
    app.apply_config(&cfg);
    startup.mark("app_state_loaded");

    let mut perf = PerfStats::from_env();
    draw_app_frame(terminal, &mut app, &mut perf)?;
    startup.mark("first_draw");
    if std::env::var_os("YTM_EXIT_AFTER_FIRST_DRAW").is_some() {
        startup.mark("exit_after_first_draw");
        return Ok(());
    }

    // Preflight the external tools. If mpv/yt-dlp are missing, show an install hint up
    // front rather than surfacing an opaque spawn failure later.
    let missing = deps::missing();
    if !missing.is_empty() {
        tracing::warn!(missing = ?missing, "required external tools not found on PATH");
        app.status.text = deps::install_hint(&missing);
    }
    startup.mark("deps_checked");

    // Worker -> UI channel. Actors hold clones; the original stays alive so the
    // select! branch never resolves to `None`.
    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel::<Msg>();

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
                    let _ = art_resize_msg_tx.send(Msg::ArtworkResized(response));
                }
                Ok(Err(e)) => tracing::warn!(error = ?e, "artwork resize failed"),
                Err(e) => tracing::warn!(error = ?e, "artwork resize task failed"),
            }
        }
    });

    // Signals (SIGINT/TERM/HUP) kill mpv and ask the loop to quit.
    player::lifetime::spawn_signal_handlers(worker_tx.clone());

    // The remote-control accept loop is started — and the instance descriptor published — just
    // before the reducer loop below (see `remote.map(..server.start..)`), NOT here. Publishing
    // only once the app can actually service commands avoids a cold-start race where `ytt -r`
    // found a live descriptor but nothing was accepting/answering yet. The socket itself is
    // already bound (in `bind_or_detect`), so the single-instance guard is in force from launch.

    // Spawn mpv off the startup path. Until the IPC actor is ready, reducer-emitted
    // PlayerCmds are buffered below and replayed in order.
    let mut player_handle: Option<PlayerHandle> = None;
    let mut pending_player_cmds: Vec<PlayerCmd> = Vec::new();
    let mut player_failed = false;
    let mut _mpv_guard: Option<player::Mpv> = None;
    let (player_ready_tx, mut player_ready_rx) =
        mpsc::unbounded_channel::<Result<(PlayerHandle, player::Mpv), String>>();
    let player_msg_tx = worker_tx.clone();
    let player_data_dir = data_dir.clone();
    let player_cookies_file = cookies_file.clone();
    let player_gapless = cfg.effective_gapless();
    tokio::spawn(async move {
        let result = player::spawn(
            player_msg_tx,
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
    let api_handle = api::spawn(cookie, worker_tx.clone());
    startup.mark("api_spawned");

    // Lyrics actor: fetches synced lyrics from lrclib on demand, cached per track.
    let lyrics_handle = lyrics::spawn(worker_tx.clone());

    // Artwork actor: fetches + decodes album art / thumbnails on demand (only used when
    // album art is enabled; the reducer simply never emits a fetch otherwise).
    let artwork_handle = artwork::spawn(worker_tx.clone());

    // Download actor: yt-dlp best-audio + tags + cover art, capped concurrency.
    let download_handle = download::spawn(
        worker_tx.clone(),
        cfg.effective_download_dir(),
        cookies_file.clone(),
        cfg.effective_download_concurrency(),
    );

    // Resolver actor: pre-resolves the next track's stream URL for instant skip.
    let resolver_handle = resolver::spawn(worker_tx.clone(), cookies_file);

    // Gemini actor: spawned when any Gemini-backed feature is active. DJ Gem can be off while
    // title romanization is on, so UI assistant availability is tracked separately below.
    let mut ai_handle = cfg
        .effective_ai_service_key()
        .and_then(|key| ai::spawn(&key, cfg.effective_gemini_model(), worker_tx.clone()));
    app.ai.available = cfg.effective_ai_enabled() && ai_handle.is_some();

    // Opt-in autoplay-on-launch: now that every player/actor handle exists, ask the loop to
    // start the restored track. Routed through the message pump so the resulting load/save
    // commands flow through the normal dispatch below (no-op when the setting is off or
    // nothing was restored).
    if cfg.effective_autoplay_on_start() {
        let _ = worker_tx.send(Msg::Autoplay);
    }

    let mut events = EventStream::new();
    let mut input = event::Translator::default();
    let mut ime_scrub = tokio::time::interval(Duration::from_millis(80));
    ime_scrub.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Only polled while a transient status is covering the song title; lets the reducer
    // expire it (and restore the title) ~3s after it was shown. Idle otherwise.
    let mut status_tick = tokio::time::interval(Duration::from_millis(250));
    status_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
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
    let _remote_guard = remote.map(|server| server.start(worker_tx.clone()));

    while !app.should_quit {
        if app.dirty {
            draw_app_frame(terminal, &mut app, &mut perf)?;
            finish_draw_cycle(&mut app);
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
                Some(Ok(ev)) => match input.translate(ev) {
                    Some(m) => m,
                    None => continue,
                },
                Some(Err(_)) => continue,
                None => break,
            },
            Some(m) = worker_rx.recv() => m,
            Some(result) = player_ready_rx.recv() => {
                match result {
                    Ok((handle, guard)) => {
                        handle.send(PlayerCmd::SetVolume(cfg.volume));
                        // Apply persisted playback speed and the EQ/normalization chain up front.
                        if (app.playback.speed - 1.0).abs() > f64::EPSILON {
                            handle.send(PlayerCmd::SetProperty {
                                name: "speed".to_owned(),
                                value: serde_json::Value::from(app.playback.speed),
                            });
                        }
                        if let Some(af) =
                            crate::eq::build_af_string(&app.audio.bands, app.audio.normalize)
                        {
                            handle.send(PlayerCmd::SetAudioFilter(af));
                        }
                        // The M1 demo track is opt-in; keep its ordering before queued startup
                        // commands, matching the old synchronous setup path.
                        if let Ok(url) = std::env::var("YTM_PLAY_URL") {
                            handle.load(url);
                        }
                        for cmd in pending_player_cmds.drain(..) {
                            handle.send(cmd);
                        }
                        player_handle = Some(handle);
                        _mpv_guard = Some(guard);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to start mpv");
                        player_failed = true;
                        pending_player_cmds.clear();
                        // Keep the richer preflight hint if we already set one.
                        if app.status.text.is_empty() {
                            app.status.text = format!(
                                "{}: {e}",
                                crate::t!("mpv unavailable", "mpv를 사용할 수 없음")
                            );
                            app.dirty = true;
                        }
                    }
                }
                if app.dirty {
                    draw_app_frame(terminal, &mut app, &mut perf)?;
                    finish_draw_cycle(&mut app);
                    perf.maybe_log(&app);
                }
                continue;
            },
            _ = ime_scrub.tick(), if app.should_scrub_ime_preedit() => {
                draw_app_frame(terminal, &mut app, &mut perf)?;
                finish_draw_cycle(&mut app);
                perf.maybe_log(&app);
                continue;
            },
            _ = status_tick.tick(), if app.status_visible() => Msg::StatusTick,
            _ = anim_tick.tick(), if app.animation_active() => Msg::AnimTick,
        };

        let resized_artwork = matches!(&msg, Msg::ArtworkResized(_));
        for cmd in app.update(msg) {
            match cmd {
                Cmd::Player(pc) => {
                    if let Some(p) = &player_handle {
                        p.send(pc);
                    } else if !player_failed {
                        pending_player_cmds.push(pc);
                    }
                }
                Cmd::Search {
                    query,
                    source,
                    config,
                } => api_handle.search(query, source, config),
                Cmd::SaveLibrary => {
                    if let Err(e) = app.library.save() {
                        tracing::warn!(error = %e, "failed to save library");
                    }
                }
                Cmd::SaveDownloads => {
                    if let Err(e) = app.download_store.save() {
                        tracing::warn!(error = %e, "failed to save downloads manifest");
                    }
                }
                Cmd::SaveSignals => {
                    if let Err(e) = app.signals.save() {
                        tracing::warn!(error = %e, "failed to save signals");
                    }
                }
                Cmd::SaveRomanizedTitles => {
                    if let Err(e) = app.romanization.cache.save() {
                        tracing::warn!(error = %e, "failed to save romanized title cache");
                    }
                }
                Cmd::ClearRomanizedTitles => {
                    if let Err(e) = romanize::RomanizeCache::delete_saved() {
                        tracing::warn!(error = %e, "failed to delete romanized title cache");
                    }
                }
                Cmd::ScanDownloads(dir) => {
                    let songs = library::scan_downloads(&dir);
                    let _ = worker_tx.send(Msg::DownloadsScanned(songs));
                }
                Cmd::FetchLyrics {
                    video_id,
                    artist,
                    title,
                } => {
                    lyrics_handle.fetch(video_id, artist, title);
                }
                Cmd::FetchArtwork { video_id, source } => {
                    artwork_handle.fetch(video_id, source);
                }
                Cmd::Download(song) => download_handle.start(song),
                Cmd::SetDownloadDir(dir) => download_handle.set_dir(dir),
                Cmd::Resolve {
                    video_id,
                    watch_url,
                } => {
                    resolver_handle.resolve(video_id, watch_url);
                }
                Cmd::SaveConfig(cfg) => {
                    if let Err(e) = cfg.save() {
                        tracing::warn!(error = %e, "failed to save config");
                    }
                }
                Cmd::SavePlaylists => {
                    if let Err(e) = app.playlists.save() {
                        tracing::warn!(error = %e, "failed to save playlists");
                    }
                }
                Cmd::SaveStationProfile => {
                    if let Err(e) = app.station.save() {
                        tracing::warn!(error = %e, "failed to save station profile");
                    }
                }
                Cmd::AskAi { prompt, context } => {
                    if let Some(h) = &ai_handle {
                        h.ask(prompt, context);
                    }
                }
                Cmd::AiRerank {
                    seed_video_id,
                    prompt,
                } => {
                    if let Some(h) = &ai_handle {
                        h.rerank(seed_video_id, prompt);
                    }
                }
                Cmd::SummarizeFeedback { digest } => {
                    if let Some(h) = &ai_handle {
                        h.summarize_feedback(digest);
                    }
                }
                Cmd::RomanizeTitles { request_id, items } => {
                    let keys: Vec<String> = items.iter().map(|item| item.key.clone()).collect();
                    if let Some(h) = &ai_handle {
                        h.romanize(request_id, items);
                    } else {
                        let _ = worker_tx.send(Msg::RomanizedTitles {
                            request_id,
                            keys,
                            entries: Vec::new(),
                        });
                    }
                }
                Cmd::StreamingFallback {
                    seed,
                    seed_video_id,
                    exclude_ids,
                    mode,
                    config,
                } => {
                    api_handle.streaming(
                        seed,
                        seed_video_id,
                        exclude_ids,
                        app::STREAMING_POOL_COUNT,
                        mode,
                        config,
                    );
                }
                Cmd::StreamingPreflight {
                    seed_video_id,
                    picks,
                    fallback,
                    mode,
                    config,
                } => {
                    api_handle.streaming_preflight(seed_video_id, picks, fallback, mode, config);
                }
                Cmd::SetAiModel(model) => {
                    if let Some(h) = &ai_handle {
                        h.set_model(model);
                    }
                }
                Cmd::ReloadAi {
                    key,
                    model,
                    assistant_enabled,
                } => {
                    // Drop the old actor (closing its channel ends its task) and bring up a
                    // fresh one for the new key, so a key edited in Settings works at once.
                    ai_handle = key.and_then(|k| ai::spawn(&k, model, worker_tx.clone()));
                    app.ai.available = assistant_enabled && ai_handle.is_some();
                }
            }
        }
        if resized_artwork {
            perf.record_art_resize();
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

        if app.dirty {
            draw_app_frame(terminal, &mut app, &mut perf)?;
            finish_draw_cycle(&mut app);
        }
        perf.maybe_log(&app);
    }

    // Close the video overlay (if one is open) so it doesn't outlive the app. This is the single
    // cleanup chokepoint: every quit path just sets `should_quit` and falls out of the loop here.
    app.close_video();
    // Belt-and-suspenders: persist the library + signals + downloads manifest on a clean exit too.
    let _ = app.library.save();
    let _ = app.signals.save();
    let _ = app.download_store.save();
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
