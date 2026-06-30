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
mod radio;
mod remote;
mod resolver;
mod settings;
mod signals;
mod station;
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

    // Custom runtime: 2 workers + 512 KB stacks keeps stack RSS ~1.5 MB (vs ~4.5 MB
    // at the 2 MB default). The render loop runs on the main task; actors run on the
    // worker threads so a blocked IPC read never stalls rendering.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(512 * 1024)
        .enable_all()
        .build()?;
    rt.block_on(async_main(new_instance))
}

async fn async_main(new_instance: bool) -> Result<()> {
    // Load config before terminal init so mouse capture reflects it.
    let cfg = config::Config::load();
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
    let mut terminal = tui::init(mouse)?;
    let result = run(&mut terminal, cfg, art_picker, remote).await;
    tui::restore(mouse);
    result
}

/// Build the terminal image picker, querying the terminal for its graphics protocol and
/// font size. Falls back to a halfblocks-only picker if the query fails (e.g. a terminal
/// that doesn't answer the control sequences), so album art still renders — just blocky.
fn build_art_picker() -> ratatui_image::picker::Picker {
    use ratatui_image::picker::Picker;
    match Picker::from_query_stdio() {
        Ok(picker) => picker,
        Err(e) => {
            tracing::warn!(error = %e, "terminal graphics probe failed; album art falls back to halfblocks");
            Picker::halfblocks()
        }
    }
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
    app: &App,
    perf: &mut PerfStats,
    clear_before: bool,
) -> std::io::Result<()> {
    let start = perf.enabled.then(Instant::now);
    let res = tui::draw_frame(
        terminal,
        app.synchronized_draw_active() || clear_before,
        clear_before,
        |f| ui::render(f, app),
    );
    if res.is_ok()
        && let Some(start) = start
    {
        perf.record_draw(start.elapsed());
    }
    res
}

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    cfg: config::Config,
    art_picker: Option<ratatui_image::picker::Picker>,
    remote: Option<remote::RemoteServer>,
) -> Result<()> {
    // Resolve cross-platform dirs; logging + PID registry degrade gracefully if absent.
    let dirs = directories::ProjectDirs::from("", "", "ytm-tui");
    let _log_guard = dirs.as_ref().and_then(|d| {
        let dir = d.cache_dir();
        std::fs::create_dir_all(dir).ok()?;
        logging::init(dir)
    });
    let data_dir = dirs.as_ref().map(|d| d.data_dir().to_path_buf());
    if let Some(dir) = &data_dir {
        let _ = std::fs::create_dir_all(dir);
        // Reap any mpv leaked by a prior run that died uncatchably (SIGKILL/power loss).
        player::lifetime::reap_orphans(dir);
    }

    // Wrap the terminal-restoring panic hook so a panic also kills mpv (matters under
    // `panic = "abort"`, where Drop never runs). Install after `tui::init`.
    player::lifetime::install_panic_hook();
    // Windows: kill mpv promptly on the console close button / logoff / shutdown (the
    // Job Object guarantees it regardless; this just makes teardown immediate).
    #[cfg(windows)]
    player::lifetime::install_console_ctrl_handler();

    // Config is loaded in `async_main` (so mouse capture can reflect it) and passed in.
    let cookie = cfg.effective_cookie();
    let had_cookie = cookie.is_some();
    // Only hand mpv/yt-dlp a cookies file that actually exists: a configured/default
    // path that has not been exported yet would make yt-dlp error and break anonymous
    // playback.
    let cookies_file = cfg.effective_cookies_file().filter(|p| p.exists());

    let mut app = App::new(cfg.volume);
    // Hand over the terminal image picker (present only when album art is enabled).
    app.art.picker = art_picker;
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
    // Load local playlists (the AI playlist tools read/write these).
    app.playlists = playlists::Playlists::load();
    // Load the active natural-language station profile (explore level + avoided artists), if any.
    app.station = station::StationStore::load();
    app.apply_station_profile();
    // Push persisted playback/EQ settings (preset, bands, normalize, speed, autoplay).
    app.apply_config(&cfg);

    // Preflight the external tools. If mpv/yt-dlp are missing, show an install hint up
    // front rather than surfacing an opaque spawn failure later.
    let missing = deps::missing();
    if !missing.is_empty() {
        tracing::warn!(missing = ?missing, "required external tools not found on PATH");
        app.status.text = deps::install_hint(&missing);
    }

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

    // Spawn mpv. `_mpv_guard` must outlive the loop: dropping it kills mpv on quit.
    let mut player_handle: Option<PlayerHandle> = None;
    let _mpv_guard;
    match player::spawn(
        worker_tx.clone(),
        data_dir.clone(),
        cookies_file.clone(),
        cfg.effective_gapless(),
    )
    .await
    {
        Ok((handle, guard)) => {
            handle.send(PlayerCmd::SetVolume(cfg.volume));
            // Apply persisted playback speed and the EQ/normalization chain up front.
            if (app.playback.speed - 1.0).abs() > f64::EPSILON {
                handle.send(PlayerCmd::SetProperty {
                    name: "speed".to_owned(),
                    value: serde_json::Value::from(app.playback.speed),
                });
            }
            if let Some(af) = crate::eq::build_af_string(&app.audio.bands, app.audio.normalize) {
                handle.send(PlayerCmd::SetAudioFilter(af));
            }
            // The M1 demo track is now opt-in; normal startup is idle until the user
            // searches and plays something.
            if let Ok(url) = std::env::var("YTM_PLAY_URL") {
                handle.load(url);
            }
            player_handle = Some(handle);
            _mpv_guard = Some(guard);
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to start mpv");
            // Keep the richer preflight hint if we already set one.
            if app.status.text.is_empty() {
                app.status.text = format!(
                    "{}: {e}",
                    crate::t!("mpv unavailable", "mpv를 사용할 수 없음")
                );
            }
            _mpv_guard = None;
        }
    }

    // Spawn the API actor: signed in if the cookie works, else anonymous (yt-dlp search
    // + public playback still work). Anonymous never fails, so we always get a handle.
    let (api_handle, api_mode) = api::spawn(cookie, worker_tx.clone()).await;
    app.authenticated = api_mode == api::ApiMode::Authenticated;

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
    if api_mode == api::ApiMode::Anonymous && had_cookie {
        app.status.text = crate::t!(
            "Cookie rejected — anonymous mode (search & play only)",
            "쿠키가 거부됨 — 익명 모드 (검색·재생만 가능)"
        )
        .to_owned();
    }

    // AI assistant actor: spawned only when a Gemini key is configured *and* AI is enabled.
    // `effective_ai_key()` returns None when the user has switched AI off, so the actor stays
    // down even with a key saved. Keep `ai_available` in lockstep with whether the actor
    // actually started (a malformed key makes `spawn` return None even though one was set).
    let mut ai_handle = cfg
        .effective_ai_key()
        .and_then(|key| ai::spawn(&key, cfg.effective_gemini_model(), worker_tx.clone()));
    app.ai.available = ai_handle.is_some();

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
    // playing, focused, no full-screen overlay). With every animation toggle off (the default) the
    // guard is false, this timer never wakes, and the loop stays exactly as light as before.
    // `Skip` drops missed frames so a busy moment can't build up a backlog of redraws. The period
    // is rebuilt below whenever the user changes the rate in Settings.
    let mut anim_fps = app.animation_tick_fps();
    let mut anim_tick = anim_interval(anim_fps);
    let mut perf = PerfStats::from_env();
    draw_app_frame(terminal, &app, &mut perf, false)?;

    // Every actor is up and the reducer loop is one statement away from draining `worker_rx`:
    // now start the control server and publish the instance descriptor. Doing it here (rather
    // than during setup) means a `ytt -r` that discovers the descriptor is guaranteed something
    // is accepting and the reducer will answer promptly. `_remote_guard` lives to end of `run`;
    // its Drop removes the descriptor (and best-effort the socket) on exit. `None` => remote
    // control unavailable this run; the app still works as a normal player.
    let _remote_guard = remote.map(|server| server.start(worker_tx.clone()));

    while !app.should_quit {
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
            _ = ime_scrub.tick(), if app.should_scrub_ime_preedit() => {
                draw_app_frame(terminal, &app, &mut perf, false)?;
                app.dirty = false;
                perf.maybe_log(&app);
                continue;
            },
            _ = status_tick.tick(), if app.status_visible() => Msg::StatusTick,
            _ = anim_tick.tick(), if app.animation_active() => Msg::AnimTick,
        };

        // The `eq:`/`radio:` dropdowns and the queue window paint a `Clear` box over part of the
        // album art, but graphics-protocol art only re-emits when its render *area* changes — so a
        // closed popup would leave a stale box where it was. Snapshot which art-covering popups are
        // open across dispatch and, on any change, rebuild the art so it repaints cleanly (see
        // `App::refresh_art`) — the same clean appear/disappear the full-width `?` overlay gets free.
        let overlay_before = app.art_overlay_mask();

        let resized_artwork = matches!(&msg, Msg::ArtworkResized(_));
        for cmd in app.update(msg) {
            match cmd {
                Cmd::Player(pc) => {
                    if let Some(p) = &player_handle {
                        p.send(pc);
                    }
                }
                Cmd::Search(query) => api_handle.search(query),
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
                Cmd::RadioFallback {
                    seed,
                    seed_video_id,
                    exclude_ids,
                } => {
                    api_handle.radio(seed, seed_video_id, exclude_ids, app::RADIO_POOL_COUNT);
                }
                Cmd::SetAiModel(model) => {
                    if let Some(h) = &ai_handle {
                        h.set_model(model);
                    }
                }
                Cmd::ReloadAi { key, model } => {
                    // Drop the old actor (closing its channel ends its task) and bring up a
                    // fresh one for the new key, so a key edited in Settings works at once.
                    ai_handle = key.and_then(|k| ai::spawn(&k, model, worker_tx.clone()));
                    app.ai.available = ai_handle.is_some();
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
            let overlay_after = app.art_overlay_mask();
            // Native terminal graphics (Sixel/iTerm2/Kitty) live outside ratatui's text buffer.
            // When a popup/modal/screen covers album art, clear the terminal graphics layer and
            // force a full redraw. While the overlay remains visible, the player view suppresses
            // art rendering so the image cannot reappear above the popup.
            let clear_for_art_overlay = overlay_before != overlay_after
                && app.art_active()
                && app.art_uses_terminal_graphics();
            if clear_for_art_overlay && overlay_after == 0 {
                app.refresh_art();
            }
            draw_app_frame(terminal, &app, &mut perf, clear_for_art_overlay)?;
            app.dirty = false;
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
