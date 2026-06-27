mod ai;
mod api;
mod app;
mod artwork;
mod config;
mod deps;
mod download;
mod eq;
mod event;
mod keymap;
mod library;
mod logging;
mod lyrics;
mod player;
mod playlists;
mod queue;
mod resolver;
mod settings;
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
    let mouse = cfg.effective_mouse();
    // Album art is opt-in: only probe the terminal for its graphics protocol + font size
    // when the user enabled it, and do it BEFORE `tui::init` (the query reads/writes stdio,
    // so running it ahead of the alternate screen + event stream avoids racing them). A
    // failed probe falls back to halfblocks so the feature still shows something.
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
    app.art_picker = art_picker;
    // Load the local library (favorites + history); an absent/corrupt file → empty.
    app.library = library::Library::load();
    app.downloaded_tracks = library::scan_downloads(&cfg.effective_download_dir());
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
        app.status = deps::install_hint(&missing);
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
            if (app.speed - 1.0).abs() > f64::EPSILON {
                handle.send(PlayerCmd::SetProperty {
                    name: "speed".to_owned(),
                    value: serde_json::Value::from(app.speed),
                });
            }
            if let Some(af) = crate::eq::build_af_string(&app.eq_bands, app.normalize) {
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
            if app.status.is_empty() {
                app.status = format!("mpv unavailable: {e}");
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
        app.status = "Cookie rejected — anonymous mode (search & play only)".to_owned();
    }

    // AI assistant actor: spawned only when a Gemini key is configured. Keep
    // `ai_available` in lockstep with whether the actor actually started (a malformed key
    // makes `spawn` return None even though one was set).
    let mut ai_handle = cfg
        .effective_gemini_api_key()
        .and_then(|key| ai::spawn(&key, cfg.effective_gemini_model(), worker_tx.clone()));
    app.ai_available = ai_handle.is_some();

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
    terminal.draw(|f| ui::render(f, &app))?;

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
                terminal.draw(|f| ui::render(f, &app))?;
                app.dirty = false;
                continue;
            },
        };

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
                Cmd::SetAiModel(model) => {
                    if let Some(h) = &ai_handle {
                        h.set_model(model);
                    }
                }
                Cmd::ReloadAi { key, model } => {
                    // Drop the old actor (closing its channel ends its task) and bring up a
                    // fresh one for the new key, so a key edited in Settings works at once.
                    ai_handle = key.and_then(|k| ai::spawn(&k, model, worker_tx.clone()));
                    app.ai_available = ai_handle.is_some();
                }
            }
        }

        if app.dirty {
            terminal.draw(|f| ui::render(f, &app))?;
            app.dirty = false;
        }
    }

    // Belt-and-suspenders: persist the library on a clean exit too.
    let _ = app.library.save();
    Ok(())
}
