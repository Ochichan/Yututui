use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui_image::thread::ResizeRequest;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::app::{self, App, Cmd, Msg};
use crate::player::PlayerHandle;
use crate::runtime::RuntimeEvent;
use crate::{
    ai, api, artwork, config, deps, download, downloads, event, library, logging, lyrics, media,
    notify, persist, player, playlists, remote, resolver, romanize, runtime, scrobble, session,
    signals, station, tools, tui, ui, update, zoom,
};

use super::art::log_art_picker;
use super::startup::StartupTrace;

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
    anim_interval_at(tokio::time::Instant::now() + period, fps)
}

fn anim_interval_at(first_tick: tokio::time::Instant, fps: u16) -> tokio::time::Interval {
    let mut tick = tokio::time::interval_at(first_tick, anim_tick_period(fps));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    tick
}

/// Rebuild the long-lived interval only when the configured logical FPS changes. Animation
/// activation and draw-cadence changes deliberately do not call this path, preserving the
/// existing timer grid and reducer draw credit across a guarded (inactive) period.
fn sync_animation_interval(
    app: &mut App,
    anim_fps: &mut u16,
    anim_tick: &mut tokio::time::Interval,
) -> bool {
    let new_fps = app.animation_tick_fps();
    if new_fps == *anim_fps {
        return false;
    }
    *anim_fps = new_fps;
    app.reset_animation_cadence();
    *anim_tick = anim_interval(new_fps);
    true
}

const IME_SCRUB_PERIOD: Duration = Duration::from_millis(80);

/// Permanent low-rate repaint clock for terminal-owned IME preedit text. The select branch is
/// guarded while an application text field is active, but the clock itself never expires: some
/// terminals repaint preedit without sending any event that could re-arm a bounded timer.
fn ime_scrub_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(IME_SCRUB_PERIOD);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

/// Take only an immediately-adjacent time/cache pair. Both messages still pass through
/// `App::update` in their original order; sharing the post-update draw/observer work is safe while
/// skipping over any intervening message would not be.
fn take_adjacent_player_progress_pair(first: &Msg, pending: &mut VecDeque<Msg>) -> Option<Msg> {
    let complementary = matches!(
        (first, pending.front()),
        (
            Msg::Player(app::PlayerMsg::TimePos(_)),
            Some(Msg::Player(app::PlayerMsg::CacheTime(_)))
        ) | (
            Msg::Player(app::PlayerMsg::CacheTime(_)),
            Some(Msg::Player(app::PlayerMsg::TimePos(_)))
        )
    );
    complementary.then(|| pending.pop_front().expect("front checked above"))
}

/// Post-reducer observer work for one owner-loop turn. Progress is deliberately separate: mpv's
/// high-rate clocks still feed the 1 Hz scrobbler, but elapsed time is not an OS/remote change and
/// must not trigger media fingerprints, `CoreView`, topic hashes, models, or serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObserverPlan {
    project_state: bool,
    drive_scrobble_heartbeat: bool,
}

impl ObserverPlan {
    const INERT: Self = Self {
        project_state: false,
        drive_scrobble_heartbeat: false,
    };
    const PROGRESS: Self = Self {
        project_state: false,
        drive_scrobble_heartbeat: true,
    };
    const PROJECTED: Self = Self {
        project_state: true,
        drive_scrobble_heartbeat: true,
    };

    fn for_messages(first: &Msg, paired: Option<&Msg>) -> Self {
        let progress = |msg: &Msg| {
            matches!(
                msg,
                Msg::Player(app::PlayerMsg::TimePos(_)) | Msg::Player(app::PlayerMsg::CacheTime(_))
            )
        };
        if progress(first) && paired.is_none_or(progress) {
            Self::PROGRESS
        } else if paired.is_none() && matches!(first, Msg::AnimTick | Msg::StatusTick) {
            Self::INERT
        } else {
            Self::PROJECTED
        }
    }
}

struct PerfStats {
    enabled: bool,
    last_log: Instant,
    frames: u64,
    ime_fast_scrubs: u64,
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
            ime_fast_scrubs: 0,
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

    fn record_ime_fast_scrub(&mut self) {
        if self.enabled {
            self.ime_fast_scrubs += 1;
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
            full_frames = self.frames,
            ime_fast_scrubs = self.ime_fast_scrubs,
            avg_draw_ms,
            max_draw_ms = self.draw_max.as_secs_f64() * 1000.0,
            art_resizes = self.art_resizes,
            dropped_log_lines = logging::dropped_lines(),
            active_effects,
            tick_fps = app.animation_tick_fps(),
            draw_fps = app.animation_draw_fps(),
            dirty = app.dirty,
            "perf window"
        );
        self.last_log = Instant::now();
        self.frames = 0;
        self.ime_fast_scrubs = 0;
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
            app.mark_local_rows_rendered();
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

/// Attempt the normal render lifecycle while keeping IME freshness fail-closed. A transient full
/// draw failure may clear `app.dirty`, so the separate flag is set before touching the terminal and
/// is cleared only after both a successful draw and the normal finish step.
fn draw_full_app_frame(
    terminal: &mut tui::AppTerminal,
    app: &mut App,
    perf: &mut PerfStats,
    reducer_turn_unrendered: &mut bool,
) -> std::io::Result<bool> {
    *reducer_turn_unrendered = true;
    let rendered = draw_app_frame(terminal, app, perf)?;
    if rendered {
        finish_draw_cycle(app);
        *reducer_turn_unrendered = false;
    }
    Ok(rendered)
}

fn ime_scrub_state_requires_full_draw(
    reducer_turn_unrendered: bool,
    dirty: bool,
    clear_before_draw_pending: bool,
    animation_active: bool,
    radio_stream_active: bool,
) -> bool {
    reducer_turn_unrendered
        || dirty
        || clear_before_draw_pending
        || animation_active
        || radio_stream_active
}

fn ime_scrub_requires_full_draw(app: &App, reducer_turn_unrendered: bool) -> bool {
    ime_scrub_state_requires_full_draw(
        reducer_turn_unrendered,
        app.dirty,
        app.clear_before_draw_pending(),
        // Active animation renders can depend on wall-clock interpolation between reducer ticks.
        // Keep origin's 80 ms full redraw in those windows so visible timing remains exact.
        app.animation_active(),
        // Live-radio rendering reads `cache_time_at.elapsed()` for its stale-edge verdict, even
        // when the stream was started outside dedicated Radio mode.
        app.current_is_radio_stream(),
    ) || !app.ime_scrub_local_projection_fresh()
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

pub async fn run(
    terminal: &mut tui::AppTerminal,
    cfg: config::Config,
    art_picker: Option<ratatui_image::picker::Picker>,
    remote: Option<remote::RemoteServer>,
    mut startup: StartupTrace,
    zoom: zoom::ZoomHandle,
    current_version: &'static str,
) -> Result<()> {
    // Use the centralized paths so verification/perf runs honor YTM_* overrides all the way
    // through logging and the mpv lifetime registry. Both degrade gracefully if unavailable.
    let cache_dir = crate::paths::cache_dir();
    let _log_guard = cache_dir.as_ref().and_then(|dir| {
        std::fs::create_dir_all(dir).ok()?;
        logging::init(dir)
    });
    startup.mark("logging_init_called");
    startup.enable_logging();
    let data_dir = crate::paths::data_dir();
    if let Some(dir) = &data_dir {
        let _ = std::fs::create_dir_all(dir);
        // Reap any mpv leaked by a prior run that died uncatchably (SIGKILL/power loss).
        player::lifetime::reap_orphans(dir);
    }
    startup.mark("dirs_ready");

    // Detect the terminal's desktop-notification support once (env-only). Used by the
    // `Cmd::DesktopNotify` arm in the loop below to pick OSC vs. the native fallback.
    let notifier = notify::Notifier::detect();

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
    let (cookies_file, cookies_warning) =
        cfg.cookies_file_for_external_tools_with_warning(data_dir.as_deref());
    let player_runtime = cfg.player_runtime(cookies_file.clone());
    let download_runtime = cfg.download_runtime(cookies_file.clone());
    let ai_runtime = cfg.ai_runtime();
    startup.mark("cookies_resolved");

    let mut app = App::new(player_runtime.volume);
    if let Some(warning) = cookies_warning {
        app.status.text = warning;
    }
    // Radio recorder: point it at its temp dir. The dir wipe (only explicitly-saved files
    // persist across runs) and the `stream-record` capability probe are deferred to just
    // after `tools::init` below — off the pre-first-frame path. Neither is read by the first
    // render (`recorder.supported` is only consulted when the user opens/uses the recorder),
    // and the probe is an `mpv --version` fork/exec (~40-60ms) that must run *after*
    // `tools::init` sets `mpv_program()`, so it probes the selected mpv, not the fallback.
    if let Some(dir) = cache_dir.as_ref() {
        app.recorder.temp_dir = dir.join("recordings");
    }
    // Zoom wiring before `apply_config`, which restores the persisted scale (the handle
    // carries the probed mode, so an unsupported terminal never sees a scale above 1).
    app.zoom = zoom.clone();
    // Hand over the terminal image picker (present only when album art is enabled).
    app.art.picker = art_picker;
    log_art_picker(app.art.picker.as_ref());
    // Load the local library (favorites + history); an absent/corrupt file -> empty.
    app.library = library::Library::load();
    let session_cache = session::SessionCache::load();
    // Load per-track preference signals (plays/skips/dislikes); absent -> empty.
    app.signals = signals::Signals::load();
    // Load the downloads manifest, then enrich the bare disk scan with each track's remembered
    // YouTube identity (+ real artist) so a downloaded-and-online track keeps its share link
    // after a restart; files the manifest doesn't know are recovered from their `[id]` filename.
    app.download_store = downloads::DownloadStore::load();
    let scanned = library::scan_downloads(&download_runtime.dir);
    let scan_truncated = scanned.truncated;
    let scan_limit = scanned.limit;
    app.library_ui.downloaded_rev = app.library_ui.downloaded_rev.wrapping_add(1);
    app.library_ui.downloaded = app.enrich_downloads(scanned.songs);
    if scan_truncated {
        app.status.text = format!(
            "{} {scan_limit} {}",
            crate::t!("Showing first", "처음"),
            crate::t!(
                "download files; more are hidden",
                "개 다운로드 파일만 표시됨; 일부는 숨김"
            )
        );
    }
    // Load local playlists (the DJ Gem playlist tools read/write these). Hand-edited or old
    // files are count-repaired on load; persist the repaired snapshot so startup does not keep
    // redoing the same repair every run.
    let (loaded_playlists, playlist_repair) = playlists::Playlists::load_with_repair_report();
    app.playlists = loaded_playlists;
    if playlist_repair.changed() {
        tracing::warn!(?playlist_repair, "playlists file was repaired on load");
        persist.save(persist::Snapshot::Playlists(app.playlists.clone()));
        if playlist_repair.truncated() && app.status.text.is_empty() {
            app.status.kind = app::StatusKind::Info;
            app.status.text = format!(
                "{} ({} {}, {} {})",
                crate::t!(
                    "Saved playlists repaired",
                    "저장된 플레이리스트를 정리했어요"
                ),
                playlist_repair.playlists_removed,
                crate::t!("lists removed", "개 목록 제거"),
                playlist_repair.songs_removed,
                crate::t!("tracks removed", "곡 제거")
            );
        }
    }
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

    // Radio recorder setup, deferred off the pre-first-frame path (see `App::new` above).
    // Runs after `tools::init` so the capability probe hits the *selected* mpv (via
    // `mpv_program()`), and before the event loop starts, so a recording (an explicit user
    // action) can never race the temp-dir wipe.
    if cache_dir.is_some() {
        let temp_dir = app.recorder.temp_dir.clone();
        let _ = std::fs::remove_dir_all(&temp_dir);
        let _ = std::fs::create_dir_all(&temp_dir);
    }
    app.recorder.supported = player::mpv::stream_record_supported();
    // Warm the media-controls probe here too (same one-time subprocess cost as the line above)
    // so the mid-session mpv respawn path hits the cache instead of blocking a worker on the
    // first play. Both probes are cached per (process, flag) after this.
    let _ = player::mpv::media_controls_flag_supported();
    startup.mark("recorder_ready");

    // Preflight the external tools. If mpv/yt-dlp are missing, show an install hint up
    // front rather than surfacing an opaque spawn failure later.
    let mut missing = deps::missing();
    if !missing.is_empty() {
        tracing::warn!(missing = ?missing, "required external tools not found on PATH");
        // A missing yt-dlp is about to be fetched by the maintainer spawned below -
        // its "Downloading yt-dlp..." status supersedes the install nag. mpv stays a
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
    // select! branch never resolves to `None`. Bounded to keep a burst of valid actor
    // events from becoming unbounded memory growth; high-frequency producers coalesce
    // before this boundary where possible.
    let (worker_tx, mut worker_rx) = runtime::channel(crate::util::backpressure::OWNER_EVENT_QUEUE);
    persist.set_event_sink(runtime::sink(worker_tx.clone(), RuntimeEvent::Persist));

    // Latest-only in behavior: the bounded inbox caps memory and the drain loop below
    // skips to the newest request whenever multiple resizes are already waiting.
    let (art_resize_tx, mut art_resize_rx) = crate::util::backpressure::bounded_channel::<
        ResizeRequest,
    >(crate::util::backpressure::ART_RESIZE_QUEUE);
    app.set_art_resize_tx(art_resize_tx);
    let art_resize_msg_tx = worker_tx.clone();
    tokio::spawn(async move {
        while let Some(mut request) = art_resize_rx.recv().await {
            while let Ok(newer) = art_resize_rx.try_recv() {
                request = newer;
            }
            match crate::util::blocking::spawn_cpu(move || request.resize_encode()).await {
                Ok(Ok(response)) => {
                    runtime::emit(&art_resize_msg_tx, RuntimeEvent::ArtworkResized(response));
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

    // Background app-update check: resolve the latest GitHub release and, if we're behind,
    // surface it in the About card (+ nav-brand dot + one-time toast). Never blocks startup;
    // no-ops entirely when disabled or on a development build.
    update::spawn_update_check(
        current_version,
        cfg.update_check_enabled,
        runtime::sink(worker_tx.clone(), RuntimeEvent::Update),
    );

    // The remote-control accept loop is started - and the instance descriptor published - just
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
    let player_audio = player_runtime.audio.clone();
    tokio::spawn(async move {
        let result = player::spawn(
            runtime::sink(player_msg_tx, RuntimeEvent::Player),
            player_data_dir,
            player_cookies_file,
            player_gapless,
            player_audio,
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
        runtime::emit(&worker_tx, RuntimeEvent::App(Msg::Autoplay));
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
            runtime::emit(&media_cmd_tx, RuntimeEvent::App(Msg::Media(cmd)));
        },
        move |ready| {
            runtime::emit(
                &media_art_tx,
                RuntimeEvent::App(Msg::MediaArtworkReady(ready)),
            );
        },
    );
    media.publish(app.media_snapshot());
    // macOS delivers remote-command callbacks through the main run loop, which a TUI
    // never spins on its own - pump it briefly on a short interval while the session
    // is live (no-op elsewhere; the `if` guard keeps the timer parked).
    let mut media_pump = tokio::time::interval(Duration::from_millis(100));
    media_pump.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut events = EventStream::new();
    let mut input = event::Translator::default();
    let mut ime_scrub = ime_scrub_interval();
    // Only polled while a transient status is covering the song title; lets the reducer
    // expire it (and restore the title) ~3s after it was shown. Idle otherwise.
    let mut status_tick = tokio::time::interval(Duration::from_millis(250));
    status_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // 1 Hz while a radio recording is in progress; parked otherwise (like the other guarded
    // ticks). Drives the max-duration force-split.
    let mut recording_tick = tokio::time::interval(Duration::from_secs(1));
    recording_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Drives the optional player-view animations at the configured frame rate - but only ticks
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

    let mut pending_worker_msgs: VecDeque<Msg> = VecDeque::new();
    // Startup mutates App after the first paint. Until a later successful full render proves the
    // terminal matches all reducer-owned state, the IME clock must take the normal draw path.
    let mut reducer_turn_unrendered = true;

    while !app.should_quit {
        if app.dirty {
            draw_full_app_frame(terminal, &mut app, &mut perf, &mut reducer_turn_unrendered)?;
            perf.maybe_log(&app);
            if app.dirty {
                continue;
            }
        }

        // Mostly blocks until input or a worker message arrives. Outside text-entry fields,
        // a low-rate redraw scrubs IME preedit text that some terminals paint without
        // sending an input event to the app.
        let msg = if !pending_worker_msgs.is_empty() {
            let mut polled_input = None;
            match event::poll_terminal_input_now(&mut events, &mut input, &zoom, &mut polled_input)
            {
                event::InputPoll::Ready => polled_input.expect("ready input message"),
                event::InputPoll::Empty => pending_worker_msgs
                    .pop_front()
                    .expect("pending worker message"),
                event::InputPoll::Closed => break,
            }
        } else {
            tokio::select! {
                maybe = events.next() => match maybe {
                    // The translator maps physical mouse cells onto the zoom backend's
                    // virtual grid, so hit-testing (and double-click identity) stay correct
                    // while the UI is scaled.
                    Some(Ok(ev)) => {
                        let (cs, rs) = zoom.mouse_scale();
                        match input.translate(ev, cs, rs) {
                            Some(m) => m,
                            None => continue,
                        }
                    }
                    Some(Err(_)) => continue,
                    None => break,
                },
                Some(event) = worker_rx.recv() => {
                    if event.is_telemetry_wake() {
                        for event in worker_tx.drain_coalesced() {
                            pending_worker_msgs.push_back(event.into());
                        }
                        continue;
                    } else {
                        // Owner lane (docs/gui/02 §8/§14): session subscribe ops run here,
                        // between reducer turns, and never become a Msg - the Publisher emits
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
                    }
                },
                Some(result) = player_ready_rx.recv() => {
                    handles.handle_player_ready(result, &player_runtime, &mut app);
                    reducer_turn_unrendered = true;
                    if app.dirty {
                        draw_full_app_frame(
                            terminal,
                            &mut app,
                            &mut perf,
                            &mut reducer_turn_unrendered,
                        )?;
                        perf.maybe_log(&app);
                    }
                    continue;
                },
                _ = ime_scrub.tick(), if app.should_scrub_ime_preedit() => {
                    let fast_succeeded = if ime_scrub_requires_full_draw(
                        &app,
                        reducer_turn_unrendered,
                    ) {
                        false
                    } else {
                        match tui::scrub_ime_preedit(
                            terminal,
                            app.synchronized_draw_active(),
                        ) {
                            Ok(tui::ImeScrubResult::Fast) => {
                                perf.record_ime_fast_scrub();
                                true
                            }
                            Ok(tui::ImeScrubResult::Resized) => false,
                            Err(error) => {
                                tracing::warn!(
                                    error = %error,
                                    "IME terminal scrub failed; retrying with a full draw"
                                );
                                false
                            }
                        }
                    };
                    if !fast_succeeded {
                        draw_full_app_frame(
                            terminal,
                            &mut app,
                            &mut perf,
                            &mut reducer_turn_unrendered,
                        )?;
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
            }
        };

        let paired_progress = take_adjacent_player_progress_pair(&msg, &mut pending_worker_msgs);
        let observer_plan = ObserverPlan::for_messages(&msg, paired_progress.as_ref());

        let resized_artwork = matches!(&msg, Msg::ArtworkResized(_))
            || matches!(&paired_progress, Some(Msg::ArtworkResized(_)));
        // Progress updates only interpolation/live-sync clocks. Neither clock is a projected
        // facet: OS media and remote clients interpolate elapsed independently, while seeks bump
        // `position_epoch` through a different message. Skip both hashes on this high-rate path.
        let media_before = observer_plan.project_state.then(|| app.media_fingerprint());
        reducer_turn_unrendered = true;
        for msg in std::iter::once(msg).chain(paired_progress) {
            for cmd in app.update(msg) {
                // Desktop notifications are handled here (not in `RuntimeHandles`) because the
                // OSC path writes to the terminal's stdout, which this scope owns; do it between
                // frames (before the draw below) so it never interleaves with a partial frame.
                if let Cmd::DesktopNotify { title, body } = cmd {
                    notifier.emit(&title, &body);
                    continue;
                }
                handles.dispatch(&mut app, cmd);
            }
        }
        if resized_artwork {
            perf.record_art_resize();
        }

        // The frame rate may have changed in Settings (committed to `config.animations` on close).
        // Rebuild the tick so the new rate applies without a relaunch - only when it actually
        // changed, so the common path costs one `u16` compare. Kept before the draw so the new
        // cadence is in force for this iteration.
        let _fps_changed = sync_animation_interval(&mut app, &mut anim_fps, &mut anim_tick);

        // Draw first, so the keypress lands on screen with the least latency. Everything below
        // is pure output bookkeeping - it reads post-update state but feeds no rendering - so
        // running it *after* the frame lags the OS/remote surfaces by well under one frame while
        // leaving the resting on-screen output identical.
        if app.dirty {
            draw_full_app_frame(terminal, &mut app, &mut perf, &mut reducer_turn_unrendered)?;
        }

        // Only a turn that can mutate projected state may toggle/rebuild OS metadata. Progress
        // still checks the independent heartbeat gate below, so scrobbling remains ~1 Hz even
        // when media controls are disabled or no remote topic is subscribed.
        let (media_enabled_changed, media_enable_publish_due) = if observer_plan.project_state {
            let media_enabled = app.config.effective_media_controls();
            let changed = media.set_enabled(media_enabled);
            (changed, changed && media_enabled)
        } else {
            (false, false)
        };
        let media_changed = media_before.is_some_and(|before| before != app.media_fingerprint());
        let scrobble_due = observer_plan.drive_scrobble_heartbeat
            && app.media_scrobble_heartbeat_active()
            && handles.scrobble_heartbeat_due();
        let publish_due = media_changed || media_enable_publish_due;
        if scrobble_due || publish_due {
            let snapshot = app.media_snapshot();
            if media_changed || scrobble_due {
                handles.scrobble_observe(&snapshot);
            }
            if publish_due {
                media.publish(snapshot);
            }
        }
        // Remote elapsed remains outside the player fingerprint. Match origin's observer call
        // gates so late subscriptions and follow-up snapshot ordering stay byte-for-byte stable;
        // unchanged turns perform only borrowed comparisons and never build owned models.
        if observer_plan != ObserverPlan::INERT
            && let Some(publisher) = publisher.as_mut()
            && publisher.should_observe(media_changed || media_enabled_changed)
        {
            publisher.observe(&app.core_view());
        }
        perf.maybe_log(&app);
    }

    // Tell v8 sessions the owner is going away (`system` event + Goodbye) before the
    // guard below removes the descriptor - clients start their reconnect/daemon logic
    // off this, not off a bare EOF (docs/gui/02 §7).
    if let Some(publisher) = publisher.as_ref() {
        publisher.shutting_down();
    }
    // Close the video overlay (if one is open) so it doesn't outlive the app. This is the single
    // cleanup chokepoint: every quit path just sets `should_quit` and falls out of the loop here.
    app.close_video();
    // Belt-and-suspenders: persist the last UI mode + library/signals/downloads on a clean exit
    // too. Send fresh quit-time snapshots, then drain the actor - the flush also lands any still-
    // debounced config/playlists/romanize writes. Do NOT save directly here instead: an older
    // pending snapshot in the actor could then overwrite the newer direct write.
    persist.save(persist::Snapshot::Session(app.session_cache_snapshot()));
    persist.save(persist::Snapshot::Library(app.library.clone()));
    persist.save(persist::Snapshot::Signals(app.signals.clone()));
    persist.save(persist::Snapshot::Downloads(app.download_store.clone()));
    if !persist.flush(Duration::from_secs(5)).await {
        // Timed out or still dirty after a write failure. Direct fallback is safe: the actor
        // holds the same quit-time snapshots, so a late actor write can't clobber anything newer.
        tracing::warn!("persist flush failed or timed out at quit; writing directly");
        if let Err(e) = app.session_cache_snapshot().save() {
            tracing::warn!(error = %e, "failed to save session cache at quit");
        }
        if let Err(e) = app.library.save() {
            tracing::warn!(error = %e, "failed to save library at quit");
        }
        if let Err(e) = app.signals.save() {
            tracing::warn!(error = %e, "failed to save signals at quit");
        }
        if let Err(e) = app.download_store.save() {
            tracing::warn!(error = %e, "failed to save downloads manifest at quit");
        }
    }
    // Give queued scrobbles one bounded delivery attempt (they're already durable on
    // disk either way - leftovers flush on the next launch).
    handles.scrobble_shutdown(Duration::from_millis(1500)).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::{App, Mode, Msg, PlayerMsg};

    use super::*;

    #[test]
    fn perf_stats_track_draws_and_reset_after_log_window() {
        let mut stats = PerfStats {
            enabled: false,
            last_log: Instant::now() - Duration::from_secs(10),
            frames: 0,
            ime_fast_scrubs: 0,
            draw_total: Duration::ZERO,
            draw_max: Duration::ZERO,
            art_resizes: 0,
        };
        stats.record_draw(Duration::from_millis(7));
        stats.record_art_resize();
        stats.record_ime_fast_scrub();
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.art_resizes, 0);
        assert_eq!(stats.ime_fast_scrubs, 0);

        stats.enabled = true;
        stats.record_draw(Duration::from_millis(7));
        stats.record_draw(Duration::from_millis(11));
        stats.record_art_resize();
        stats.record_ime_fast_scrub();
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.draw_total, Duration::from_millis(18));
        assert_eq!(stats.draw_max, Duration::from_millis(11));
        assert_eq!(stats.art_resizes, 1);
        assert_eq!(stats.ime_fast_scrubs, 1);

        stats.maybe_log(&App::new(100));
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.draw_total, Duration::ZERO);
        assert_eq!(stats.draw_max, Duration::ZERO);
        assert_eq!(stats.art_resizes, 0);
        assert_eq!(stats.ime_fast_scrubs, 0);
    }

    #[tokio::test]
    async fn animation_interval_uses_the_legacy_period_and_skip_policy() {
        assert_eq!(anim_tick_period(0), Duration::from_millis(1000));
        assert_eq!(anim_tick_period(1), Duration::from_millis(1000));
        assert_eq!(anim_tick_period(60), Duration::from_millis(16));
        assert_eq!(anim_tick_period(2_000), Duration::from_millis(1));

        let interval = anim_interval(30);
        assert_eq!(interval.period(), Duration::from_millis(33));
        assert_eq!(
            interval.missed_tick_behavior(),
            MissedTickBehavior::Skip,
            "busy periods must collapse into one delivered tick"
        );

        // A 1 Hz interval whose first deadline is 2.5 s overdue has two missed deadlines behind
        // it. Skip delivers the first poll immediately, then jumps to the next future grid point;
        // Burst would make this second poll immediately ready too.
        let first_due = tokio::time::Instant::now() - Duration::from_millis(2_500);
        let mut delayed = anim_interval_at(first_due, 1);
        assert_eq!(delayed.tick().await, first_due);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), delayed.tick())
                .await
                .is_err(),
            "Skip must drop the overdue backlog instead of replaying a second tick"
        );
    }

    #[tokio::test]
    async fn delayed_200ms_first_poll_advances_exactly_one_frame() {
        let first_due = tokio::time::Instant::now() - Duration::from_millis(200);
        let mut interval = anim_interval_at(first_due, 30);
        let mut app = App::new(100);

        let delivered_due = interval.tick().await;
        assert_eq!(delivered_due, first_due);
        app.update(Msg::AnimTick);

        assert_eq!(
            app.anim_frame(),
            1,
            "missed deadlines are never batch-applied"
        );
    }

    fn ambient_animation_app() -> App {
        let mut app = App::new(100);
        app.mode = Mode::Search;
        app.config.animations.master = true;
        app.config.animations.caret = true;
        app.config.animations.fps = 30;
        app
    }

    #[tokio::test]
    async fn inactive_overdue_tick_is_immediate_and_retains_draw_credit() {
        let mut app = ambient_animation_app();
        assert!(app.animation_active());
        assert_eq!(app.animation_draw_fps(), 12);

        // At 30→12 fps, two delivered ticks bank 24/30 credit without redrawing.
        for _ in 0..2 {
            app.dirty = false;
            app.update(Msg::AnimTick);
            assert!(!app.dirty);
        }

        // Construct one already-overdue interval before parking. The exact same object survives
        // both focus transitions and is polled only after reactivation.
        let first_due = tokio::time::Instant::now() - Duration::from_millis(2_500);
        let mut interval = anim_interval_at(first_due, 30);
        app.update(Msg::Focus(false));
        assert!(!app.animation_active());

        // The interval remained overdue while its select branch was guarded. Reactivation polls
        // the same interval and therefore gets exactly one immediate Skip tick on existing state.
        app.update(Msg::Focus(true));
        app.dirty = false;
        let delivered_due = tokio::time::timeout(Duration::from_millis(50), interval.tick())
            .await
            .expect("an overdue interval tick must be immediately ready");
        assert_eq!(delivered_due, first_due);
        app.update(Msg::AnimTick);

        assert_eq!(app.anim_frame(), 3);
        assert!(app.dirty, "retained 24/30 credit makes the third tick draw");
    }

    #[tokio::test]
    async fn reactivation_before_deadline_keeps_the_existing_interval_grid() {
        let mut app = ambient_animation_app();
        // A distant first deadline makes this structural test independent of wall-clock load.
        let first_due = tokio::time::Instant::now() + Duration::from_secs(3_600);
        let mut interval = anim_interval_at(first_due, 30);
        let mut fps = 30;

        app.update(Msg::Focus(false));
        assert!(!sync_animation_interval(&mut app, &mut fps, &mut interval));
        app.update(Msg::Focus(true));
        assert!(!sync_animation_interval(&mut app, &mut fps, &mut interval));
        assert_eq!(interval.period(), Duration::from_millis(33));
        assert_eq!(app.anim_frame(), 0);

        // The same synchronization seam does rebuild on the sole approved trigger.
        app.config.animations.fps = 60;
        assert!(sync_animation_interval(&mut app, &mut fps, &mut interval));
        assert_eq!(fps, 60);
        assert_eq!(interval.period(), Duration::from_millis(16));
    }

    #[tokio::test]
    async fn input_without_a_delivered_timer_does_not_advance_animation() {
        let mut app = ambient_animation_app();
        let before = app.anim_frame();
        // Even an overdue clock has no reducer effect until its `tick()` future is delivered by
        // select. This is the exact ordering that the removed input-time phase sync violated.
        let _undelivered_interval =
            anim_interval_at(tokio::time::Instant::now() - Duration::from_millis(200), 30);

        app.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));

        assert_eq!(
            app.anim_frame(),
            before,
            "input handling must not pre-apply elapsed animation ticks"
        );
    }

    #[tokio::test]
    async fn ime_scrub_clock_retains_the_permanent_origin_period() {
        let mut interval = ime_scrub_interval();
        assert_eq!(IME_SCRUB_PERIOD, Duration::from_millis(80));
        assert_eq!(interval.period(), Duration::from_millis(80));

        // The removed burst stopped after eight ticks. Reaching ten ticks without any event-based
        // re-arming proves the scrub clock remains capable of repainting terminal-owned preedit.
        for _ in 0..10 {
            tokio::time::timeout(Duration::from_millis(250), interval.tick())
                .await
                .expect("permanent IME scrub clock must not expire");
        }
    }

    #[test]
    fn ime_scrub_gate_fails_closed_for_state_and_wall_clock_rendering() {
        assert!(!ime_scrub_state_requires_full_draw(
            false, false, false, false, false
        ));
        assert!(ime_scrub_state_requires_full_draw(
            true, false, false, false, false
        ));
        assert!(ime_scrub_state_requires_full_draw(
            false, true, false, false, false
        ));
        assert!(
            ime_scrub_state_requires_full_draw(false, false, true, false, false),
            "a pending native-image clear must go through the consuming full-draw path"
        );
        assert!(
            ime_scrub_state_requires_full_draw(false, false, false, true, false),
            "animation-active views retain origin's wall-clock full draws"
        );
        assert!(
            ime_scrub_state_requires_full_draw(false, false, false, false, true),
            "live-radio stale-edge rendering retains origin's wall-clock full draws"
        );
    }

    #[test]
    fn reducer_turn_gate_catches_visible_updates_that_do_not_set_dirty() {
        let mut app = App::new(100);
        app.dirty = false;
        let status = crate::update::UpdateStatus {
            current: "1.0.0".to_owned(),
            latest: "v1.0.1".to_owned(),
            available: true,
            first_seen: false,
            method: crate::update::InstallMethod::Cargo,
        };

        let _ = app.update(Msg::UpdateChecked(status));

        assert!(
            !app.dirty,
            "this persistent surface update intentionally skips dirty"
        );
        assert!(app.overlays.update_status.is_some());
        assert!(ime_scrub_requires_full_draw(&app, true));
        assert!(
            !ime_scrub_requires_full_draw(&app, false),
            "after a successful full draw the same stable non-Local state may use the fast path"
        );
    }

    #[test]
    fn only_adjacent_time_and_cache_messages_share_an_owner_turn() {
        let first = Msg::Player(PlayerMsg::TimePos(7.0));
        let mut adjacent = VecDeque::from([Msg::Player(PlayerMsg::CacheTime(Some(9.0)))]);
        assert!(matches!(
            take_adjacent_player_progress_pair(&first, &mut adjacent),
            Some(Msg::Player(PlayerMsg::CacheTime(Some(9.0))))
        ));
        assert!(adjacent.is_empty());

        let mut intervening = VecDeque::from([
            Msg::Player(PlayerMsg::Duration(Some(180.0))),
            Msg::Player(PlayerMsg::CacheTime(Some(9.0))),
        ]);
        assert!(take_adjacent_player_progress_pair(&first, &mut intervening).is_none());
        assert!(matches!(
            intervening.front(),
            Some(Msg::Player(PlayerMsg::Duration(Some(180.0))))
        ));

        let reverse = Msg::Player(PlayerMsg::CacheTime(Some(9.0)));
        let mut adjacent = VecDeque::from([Msg::Player(PlayerMsg::TimePos(7.0))]);
        assert!(matches!(
            take_adjacent_player_progress_pair(&reverse, &mut adjacent),
            Some(Msg::Player(PlayerMsg::TimePos(7.0)))
        ));
    }

    #[test]
    fn progress_turns_skip_media_and_remote_projection_but_keep_scrobble_heartbeat() {
        for first in [
            Msg::Player(PlayerMsg::TimePos(7.0)),
            Msg::Player(PlayerMsg::CacheTime(Some(9.0))),
        ] {
            let plan = ObserverPlan::for_messages(&first, None);
            assert!(!plan.project_state, "progress must skip projection");
            assert!(
                plan.drive_scrobble_heartbeat,
                "progress must retain the 1 Hz scrobble clock"
            );
        }

        let first = Msg::Player(PlayerMsg::TimePos(7.0));
        let paired = Msg::Player(PlayerMsg::CacheTime(Some(9.0)));
        assert_eq!(
            ObserverPlan::for_messages(&first, Some(&paired)),
            ObserverPlan::PROGRESS,
            "coalescing the two clocks must not re-enable projection"
        );

        assert_eq!(
            ObserverPlan::for_messages(&Msg::StatusTick, None),
            ObserverPlan::INERT
        );
        assert_eq!(
            ObserverPlan::for_messages(&Msg::Player(PlayerMsg::Duration(Some(180.0))), None,),
            ObserverPlan::PROJECTED,
            "a real media facet still runs media/remote observers"
        );
    }

    #[test]
    fn draw_cycle_and_transient_error_helpers_are_stable() {
        let mut app = App::new(100);
        finish_draw_cycle(&mut app);
        assert!(!app.dirty);

        let error = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(!is_transient_terminal_draw_error(&error));
    }
}
