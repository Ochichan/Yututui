mod ai;
mod api;
mod app;
mod artwork;
mod config;
mod deps;
mod download;
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
mod resolver;
mod settings;
mod signals;
mod theme;
mod tui;
mod ui;
mod util;

use anyhow::Result;
use app::{App, Cmd, Msg};
use crossterm::event::EventStream;
use futures::StreamExt;
use player::{PlayerCmd, PlayerHandle};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

fn main() -> Result<()> {
    if let Some(arg) = std::env::args_os().nth(1) {
        match arg.to_string_lossy().as_ref() {
            "--version" | "-V" => {
                println!("ytt {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!("ytt {}", env!("CARGO_PKG_VERSION"));
                println!();
                println!("Usage: ytt [--version]");
                println!();
                println!("Launch the terminal YouTube Music player.");
                return Ok(());
            }
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
    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    // Load config before terminal init so mouse capture reflects it.
    let cfg = config::Config::load();
    // Apply the saved UI language before anything renders, so the first frame is already
    // translated. The Settings dropdown updates this global live as the user changes it.
    i18n::set_language(cfg.effective_language());
    let mouse = cfg.effective_mouse();
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
    let result = run(&mut terminal, cfg, art_picker).await;
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

async fn run(
    terminal: &mut ratatui::DefaultTerminal,
    cfg: config::Config,
    art_picker: Option<ratatui_image::picker::Picker>,
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
    app.library_ui.downloaded = library::scan_downloads(&cfg.effective_download_dir());
    app.restore_last_played_from_library();
    // Load local playlists (the AI playlist tools read/write these).
    app.playlists = playlists::Playlists::load();
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

    // Signals (SIGINT/TERM/HUP) kill mpv and ask the loop to quit.
    player::lifetime::spawn_signal_handlers(worker_tx.clone());

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
                app.status.text = format!("{}: {e}", crate::t!("mpv unavailable", "mpv를 사용할 수 없음"));
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
    let mut anim_fps = app.config.animations.effective_fps();
    let mut anim_tick = anim_interval(anim_fps);
    tui::draw_synced(terminal, |f| ui::render(f, &app))?;

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
                tui::draw_synced(terminal, |f| ui::render(f, &app))?;
                app.dirty = false;
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
                Cmd::AskAi { prompt, context } => {
                    if let Some(h) = &ai_handle {
                        h.ask(prompt, context);
                    }
                }
                Cmd::AiRerank { seed_video_id, prompt } => {
                    if let Some(h) = &ai_handle {
                        h.rerank(seed_video_id, prompt);
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

        // The frame rate may have changed in Settings (committed to `config.animations` on close).
        // Rebuild the tick so the new rate applies without a relaunch — only when it actually
        // changed, so the common path costs one `u16` compare.
        let new_fps = app.config.animations.effective_fps();
        if new_fps != anim_fps {
            anim_fps = new_fps;
            anim_tick = anim_interval(anim_fps);
        }

        if app.dirty {
            // An art-covering popup (eq/radio dropdown or queue window) just opened, closed, or
            // switched: rebuild the art so the next draw re-transmits and re-emits the whole image,
            // overpainting any stale popup box. Cheaper and flicker-free vs a full-screen clear.
            let overlay_after = app.art_overlay_mask();
            if overlay_before != overlay_after && app.art_active() {
                app.refresh_art();
            }
            tui::draw_synced(terminal, |f| ui::render(f, &app))?;
            app.dirty = false;
        }
    }

    // Close the video overlay (if one is open) so it doesn't outlive the app. This is the single
    // cleanup chokepoint: every quit path just sets `should_quit` and falls out of the loop here.
    app.close_video();
    // Belt-and-suspenders: persist the library + signals on a clean exit too.
    let _ = app.library.save();
    let _ = app.signals.save();
    Ok(())
}
