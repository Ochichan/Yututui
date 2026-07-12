use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui_image::thread::ResizeRequest;
use tokio::time::MissedTickBehavior;

use crate::app::{self, App, Cmd, Msg};
use crate::player::PlayerHandle;
use crate::runtime::RuntimeEvent;
use crate::{
    ai, api, artwork, config, deps, download, event, library, logging, lyrics, media, notify,
    persist, player, remote, resolver, runtime, scrobble, tools, tui, ui, update, zoom,
};

use super::art::log_art_picker;
use super::persistence_shutdown::flush_owner_persistence;
use super::persistent_startup::{PersistentStartupState, TerminalStartupState};
use super::startup::StartupTrace;

mod buffered_events;
mod runtime_paths;
mod teardown;
#[cfg(test)]
mod teardown_tests;
use buffered_events::BufferedWorkerEvents;
use runtime_paths::{TerminalRuntimePaths, resolve as terminal_runtime_paths};
use teardown::{OwnerIngressDrain, OwnerTeardown, complete_owner_teardown};

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

fn owner_exit_requested(app: &App, shutdown: &player::lifetime::ShutdownLatch) -> bool {
    app.should_quit || shutdown.is_triggered()
}

type PlayerReadyResult = Result<(PlayerHandle, player::Mpv), String>;
const PLAYER_START_TIMEOUT: Duration = Duration::from_secs(5);

struct PlayerStartup {
    ready_rx: Option<tokio::sync::oneshot::Receiver<PlayerReadyResult>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl PlayerStartup {
    async fn recv(
        &mut self,
    ) -> std::result::Result<PlayerReadyResult, tokio::sync::oneshot::error::RecvError> {
        let result = self
            .ready_rx
            .as_mut()
            .expect("player startup receiver is consumed only once")
            .await;
        self.ready_rx = None;
        result
    }

    async fn join_finished(&mut self) {
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "player startup task failed unexpectedly");
        }
    }

    /// Close the result slot first, then abort and reap the producer. A startup which has already
    /// created mpv cannot leave its `(handle, guard)` buffered in an unpolled oneshot during the
    /// slower actor and persistence shutdown which follows owner-loop exit.
    async fn cancel_and_join(&mut self) {
        if let Some(mut ready_rx) = self.ready_rx.take() {
            ready_rx.close();
            drop(ready_rx);
        }
        if let Some(task) = self.task.take() {
            task.abort();
            match task.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {}
                Err(error) => tracing::warn!(%error, "player startup task failed during shutdown"),
            }
        }
    }
}

impl Drop for PlayerStartup {
    fn drop(&mut self) {
        if let Some(ready_rx) = self.ready_rx.as_mut() {
            ready_rx.close();
        }
        if let Some(task) = self.task.as_ref() {
            task.abort();
        }
    }
}

fn spawn_player_startup<F>(future: F) -> PlayerStartup
where
    F: std::future::Future<Output = PlayerReadyResult> + Send + 'static,
{
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let result = future.await;
        if ready_tx.send(result).is_err() {
            // Closing/replacing the result slot is the cancellation boundary for obsolete work.
            // Dropping the rejected value also retires its handle/guard in the safe order.
            tracing::debug!("player startup completed after its readiness slot was closed");
        }
    });
    PlayerStartup {
        ready_rx: Some(ready_rx),
        task: Some(task),
    }
}

fn spawn_audio_player(
    worker_tx: runtime::RuntimeSender,
    data_dir: Option<std::path::PathBuf>,
    cfg: &config::PlayerRuntimeConfig,
) -> PlayerStartup {
    let cookies_file = cfg.cookies_file.clone();
    let gapless = cfg.gapless;
    let audio = cfg.audio.clone();
    spawn_player_startup(async move {
        match tokio::time::timeout(
            PLAYER_START_TIMEOUT,
            player::spawn(
                runtime::sink(worker_tx, RuntimeEvent::Player),
                data_dir,
                cookies_file,
                gapless,
                audio,
            ),
        )
        .await
        {
            Ok(result) => result.map_err(|error| format!("{error:#}")),
            Err(_) => Err(format!(
                "player startup timed out after {} seconds",
                PLAYER_START_TIMEOUT.as_secs()
            )),
        }
    })
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

/// Preserve the first fatal owner-loop I/O error so resource teardown can run before it is
/// returned to the terminal owner. Callers leave the owner loop immediately on `None`.
fn capture_owner_io_result<T>(
    result: std::io::Result<T>,
    owner_error: &mut Option<anyhow::Error>,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            debug_assert!(owner_error.is_none(), "owner loop keeps the first error");
            if owner_error.is_none() {
                *owner_error = Some(error.into());
            }
            None
        }
    }
}

struct TerminalBackgroundTasks {
    art_resize: crate::util::background_task::BackgroundTask,
    signal_handlers: crate::util::background_task::BackgroundTask,
    ytdlp_maintainer: crate::util::background_task::BackgroundTask,
    update_check: crate::util::background_task::BackgroundTask,
}

impl TerminalBackgroundTasks {
    async fn shutdown(&mut self) {
        tokio::join!(
            self.art_resize.shutdown(),
            self.signal_handlers.shutdown(),
            self.ytdlp_maintainer.shutdown(),
            self.update_check.shutdown(),
        );
    }
}

struct LiveOwnerTeardown<'a> {
    app: &'a mut App,
    handles: &'a mut runtime::RuntimeHandles,
    player_startup: &'a mut PlayerStartup,
    terminal_background: &'a mut TerminalBackgroundTasks,
    media: &'a mut media::MediaSession,
    publisher: Option<&'a remote::publish::Publisher>,
    remote_guard: Option<&'a mut remote::server::InstanceGuard>,
    persist: &'a persist::PersistHandle,
    pending_worker_events: &'a mut BufferedWorkerEvents,
    pending_shutdown_events: &'a mut VecDeque<RuntimeEvent>,
    worker_rx: &'a mut tokio::sync::mpsc::Receiver<RuntimeEvent>,
}

impl LiveOwnerTeardown<'_> {
    fn reduce_runtime_shutdown_event(
        &mut self,
        event: RuntimeEvent,
        drain: &mut OwnerIngressDrain,
    ) {
        match event {
            RuntimeEvent::Remote(
                remote_event @ (remote::server::RemoteEvent::Command(_, _)
                | remote::server::RemoteEvent::SessionCommand { .. }),
            ) => {
                drain.remote_requests += 1;
                self.handles
                    .reduce_shutdown_event(self.app, RuntimeEvent::Remote(remote_event));
            }
            RuntimeEvent::Remote(remote::server::RemoteEvent::SessionSubscribe {
                session,
                frame_id,
                page_id,
                topics: _,
                settlement,
            }) => {
                drain.subscribe_requests += 1;
                if !self.publisher.is_some_and(|publisher| {
                    publisher.reject_subscribe_for_shutdown(
                        &session,
                        page_id.as_deref(),
                        frame_id,
                        settlement,
                    )
                }) {
                    tracing::debug!(
                        frame_id,
                        ?page_id,
                        "retired queued session subscribe during owner shutdown"
                    );
                }
            }
            RuntimeEvent::TelemetryWake => {
                for coalesced in self.handles.drain_background_coalesced() {
                    self.reduce_runtime_shutdown_event(coalesced, drain);
                }
            }
            event => self.handles.reduce_shutdown_event(self.app, event),
        }
    }
}

impl OwnerTeardown for LiveOwnerTeardown<'_> {
    fn quiesce_remote(&mut self) {
        if let Some(guard) = self.remote_guard.as_deref_mut() {
            guard.quiesce_owner_admission();
        } else if let Some(publisher) = self.publisher {
            publisher.quiesce_owner_admission();
        }
    }

    fn retire_player(&mut self) {
        self.handles.begin_player_shutdown(self.app);
    }

    fn close_ingress(&mut self) {
        // Reject new producers without closing the receiver: callback retries are released, while
        // already accepted main/deferred/coalesced events remain available to the final drain.
        self.handles.close_event_ingress();
    }

    fn deactivate_media(&mut self) {
        let _ = self.media.set_enabled(false);
    }

    async fn drain_owner_ingress(&mut self) -> OwnerIngressDrain {
        let mut drain = OwnerIngressDrain::default();
        while let Some(event) = self.pending_worker_events.pop_front() {
            self.reduce_runtime_shutdown_event(event, &mut drain);
        }
        while let Some(event) = self.pending_shutdown_events.pop_front() {
            self.reduce_runtime_shutdown_event(event, &mut drain);
        }
        loop {
            while let Ok(event) = self.worker_rx.try_recv() {
                self.reduce_runtime_shutdown_event(event, &mut drain);
            }
            if self.handles.background_ingress_is_idle() {
                match self.worker_rx.try_recv() {
                    Ok(event) => {
                        self.reduce_runtime_shutdown_event(event, &mut drain);
                        continue;
                    }
                    Err(
                        tokio::sync::mpsc::error::TryRecvError::Empty
                        | tokio::sync::mpsc::error::TryRecvError::Disconnected,
                    ) => break,
                }
            }
            // The drainer may publish after the empty try-receive or may have completed its final
            // send without retiring in-flight accounting yet. Yield and recheck both predicates;
            // never wait on a receiver whose sender handles deliberately remain alive.
            tokio::task::yield_now().await;
        }
        for coalesced in self.handles.drain_background_coalesced() {
            self.reduce_runtime_shutdown_event(coalesced, &mut drain);
        }
        self.worker_rx.close();
        drain
    }

    async fn await_remote_reply_flush(&mut self) {
        if let Some(publisher) = self.publisher
            && !publisher.wait_for_wire_settlements().await
        {
            // The structural barrier owns the normal path. This tiny final window is only a
            // scheduler fallback after the bounded writer budget was exhausted and logged.
            remote::await_shutdown_reply_grace().await;
        }
    }

    async fn shutdown_remote(&mut self) {
        // The endpoint must be unlinked while this guard still owns the listener. If the hub were
        // latched first, the listener could drop and a successor could rebind before our late
        // path cleanup, letting the old process delete the new socket.
        if let Some(guard) = self.remote_guard.as_deref_mut() {
            guard.release_endpoint();
        }
        if let Some(publisher) = self.publisher {
            publisher.shutting_down();
        }
        if let Some(guard) = self.remote_guard.as_deref_mut() {
            guard.shutdown().await;
        }
    }

    async fn reap_player_startup(&mut self) {
        self.player_startup.cancel_and_join().await;
    }

    fn close_video(&mut self) {
        self.app.close_video();
    }

    async fn shutdown_terminal_background(&mut self) {
        self.terminal_background.shutdown().await;
    }

    async fn shutdown_resolver(&mut self) {
        self.handles
            .resolver_shutdown(Duration::from_millis(3500))
            .await;
    }

    async fn shutdown_runtime_background(&mut self) -> runtime::BackgroundShutdown {
        // Runtime-local jobs close admission together with the player barrier. Cancellable work
        // is aborted; real blocking work gets a bounded join window and reports exact leftovers.
        // The teardown driver preserves a timeout and invokes this once more after transfer and
        // download actors stop, before persistence flush.
        self.handles
            .background_shutdown(Duration::from_millis(3500))
            .await
    }

    async fn shutdown_transfer(&mut self) {
        // Transfer owns auth, playlist, and import child tasks. Stop it while the runtime ingress
        // still exists so the actor can interrupt reliable retries, then reap every child.
        self.handles
            .transfer_shutdown(Duration::from_millis(3500))
            .await;
    }

    async fn shutdown_downloads(&mut self) {
        // Stop yt-dlp/ffmpeg process groups before slower persistence/scrobble work.
        self.handles
            .download_shutdown(Duration::from_millis(3500))
            .await;
    }

    async fn finalize_runtime_background(&mut self) {
        let fallback = self.handles.finalize_background().await;
        // Main/deferred/coalesced work was settled before the remote/session owner closed. Only
        // exact terminal completions which crossed the closed-ingress boundary remain here.
        for event in fallback {
            self.handles.reduce_shutdown_event(self.app, event);
        }
    }

    async fn flush_persistence(&mut self) -> Result<()> {
        // Publish all authoritative quit snapshots, then drain the actor. A timeout retries every
        // still-owned journal frontier and reports any operation whose durability is unconfirmed.
        flush_owner_persistence(self.app, self.persist).await
    }

    async fn shutdown_scrobble(&mut self) -> Result<()> {
        // The deadline is diagnostic only. Accepted local durability is joined before the
        // terminal returns, while the actor's network flush remains internally bounded.
        self.handles
            .scrobble_shutdown(Duration::from_millis(1500))
            .await
            .map_err(|error| anyhow::anyhow!("scrobble shutdown durability failed: {error}"))
    }
}

pub async fn run(
    terminal: &mut tui::AppTerminal,
    startup_state: TerminalStartupState,
    art_picker: Option<ratatui_image::picker::Picker>,
    remote: Option<remote::RemoteServer>,
    mut startup: StartupTrace,
    zoom: zoom::ZoomHandle,
    current_version: &'static str,
) -> Result<()> {
    let TerminalStartupState {
        config: cfg,
        persistent,
        persistence_access,
    } = startup_state;
    let persistence_read_only = persistence_access.is_read_only();
    // Resolve every runtime store through the same override-aware roots covered by the writer
    // lease. Observational (`--new-instance`) owners retain read access but receive no mutation
    // destinations at all.
    let TerminalRuntimePaths {
        data_dir,
        writable_data_dir: player_registry_dir,
        writable_cache_dir,
        recorder_temp_dir,
    } = terminal_runtime_paths(persistence_read_only);
    let _log_guard = writable_cache_dir.as_deref().and_then(|dir| {
        std::fs::create_dir_all(dir).ok()?;
        logging::init(dir)
    });
    startup.mark("logging_init_called");
    startup.enable_logging();
    if let Some(dir) = &player_registry_dir {
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
    if let Some(temp_dir) = recorder_temp_dir.as_ref() {
        app.recorder.temp_dir = temp_dir.clone();
    }
    // Zoom wiring before `apply_config`, which restores the persisted scale (the handle
    // carries the probed mode, so an unsupported terminal never sees a scale above 1).
    app.zoom = zoom.clone();
    // Hand over the terminal image picker (present only when album art is enabled).
    app.art.picker = art_picker;
    log_art_picker(app.art.picker.as_ref());
    let PersistentStartupState {
        library,
        session_cache,
        signals,
        download_store,
        playlists: loaded_playlists,
        playlist_repair,
        station,
        romanization,
    } = persistent;
    app.library = library;
    app.signals = signals;
    app.download_store = download_store;
    // Enrich the bare disk scan with each track's remembered
    // YouTube identity (+ real artist) so a downloaded-and-online track keeps its share link
    // after a restart; files the manifest doesn't know are recovered from their `[id]` filename.
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
    let playlists_repaired_at_startup = playlist_repair.changed();
    app.playlists = loaded_playlists;
    if playlist_repair.changed() {
        tracing::warn!(?playlist_repair, "playlists file was repaired on load");
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
    app.station = station;
    app.apply_station_profile();
    // Load Latin-script display overlays for CJK titles. Source metadata stays in the library.
    app.romanization.cache = romanization;

    if let Some(reason) = persistence_access.read_only_reason() {
        tracing::warn!(%reason, "secondary player is running with read-only persistence");
        app.status.kind = app::StatusKind::Error;
        app.status.text = if crate::i18n::is_korean() {
            format!("보조 인스턴스 — 변경사항이 저장되지 않습니다: {reason}")
        } else {
            format!("Secondary instance — changes are not saved: {reason}")
        };
        // Keep the first-frame warning persistent rather than arming the ordinary toast expiry.
        app.dirty = true;
    }

    // Start persistence only after the binary-level typed preflight accepted every startup load.
    // No actor or direct writer can run with a stale read-only fallback snapshot.
    let persist = persist::spawn();
    persist::install_panic_flush(persist.pending());
    if playlists_repaired_at_startup
        && let Err(error) = persist.save(persist::Snapshot::Playlists(app.playlists.clone()))
    {
        tracing::warn!(%error, "startup playlist repair remains read-only");
    }
    // Push persisted playback/EQ settings (preset, bands, normalize, speed, autoplay).
    app.apply_config(&cfg);
    app.restore_last_session_from_cache(&session_cache);
    startup.mark("app_state_loaded");

    let mut perf = PerfStats::from_env();
    if let Err(error) = draw_app_frame(terminal, &mut app, &mut perf) {
        let owner_error = anyhow::Error::from(error);
        return match flush_owner_persistence(&app, &persist).await {
            Ok(()) => Err(owner_error),
            Err(persistence_error) => Err(owner_error.context(format!(
                "persistence shutdown also failed: {persistence_error:#}"
            ))),
        };
    }
    startup.mark("first_draw");
    if std::env::var_os("YTM_EXIT_AFTER_FIRST_DRAW").is_some() {
        startup.mark("exit_after_first_draw");
        flush_owner_persistence(&app, &persist).await?;
        return Ok(());
    }

    // Resolve which yt-dlp (managed vs system vs override) and mpv this process runs.
    // After first draw (a cold probe spawns `yt-dlp --version`, several hundred ms),
    // but before the deps preflight and the mpv spawn below, which both consume it.
    tools::init(&cfg.tools).await;
    startup.mark("tools_selected");

    // Probe after `tools::init` so it hits the selected mpv. Recovery-aware temp cleanup is
    // deferred until the tracked runtime task set exists below.
    app.recorder.supported = !persistence_read_only && player::mpv::stream_record_supported();
    // Warm the media-controls probe here too (same one-time subprocess cost as the line above)
    // so the mid-session mpv respawn path hits the cache instead of blocking a worker on the
    // first play. Both probes are cached per (process, flag) after this.
    let _ = player::mpv::media_controls_flag_supported();

    // Preflight the external tools. If mpv/yt-dlp are missing, show an install hint up
    // front rather than surfacing an opaque spawn failure later.
    let mut missing = deps::missing();
    if !missing.is_empty() {
        tracing::warn!(missing = ?missing, "required external tools not found on PATH");
        // A missing yt-dlp is about to be fetched by the maintainer spawned below -
        // its "Downloading yt-dlp..." status supersedes the install nag. mpv stays a
        // hard nag; there is no managed download for it.
        if !persistence_read_only
            && cfg.tools.managed_enabled()
            && tools::ytdlp::asset_name().is_some()
        {
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
    let art_resize_task =
        crate::util::background_task::BackgroundTask::spawn("artwork resize worker", async move {
            while let Some(mut request) = art_resize_rx.recv().await {
                while let Ok(newer) = art_resize_rx.try_recv() {
                    request = newer;
                }
                match crate::util::blocking::spawn_cpu(move || request.resize_encode()).await {
                    Ok(Ok(response)) => {
                        runtime::emit_callback_observed(
                            &art_resize_msg_tx,
                            RuntimeEvent::ArtworkResized(response),
                        );
                    }
                    Ok(Err(e)) => tracing::warn!(error = ?e, "artwork resize failed"),
                    Err(e) => tracing::warn!(error = ?e, "artwork resize task failed"),
                }
            }
        });

    // External shutdown has an out-of-band latch because the bounded worker lane can be full.
    // The compatibility RuntimeEvent is still emitted for observers, but loop termination and
    // transport-restart suppression never depend on admitting it.
    let shutdown = player::lifetime::ShutdownLatch::new();
    let signal_handlers = player::lifetime::spawn_signal_handlers(
        shutdown.clone(),
        runtime::sink(worker_tx.clone(), RuntimeEvent::Signal),
    );

    // Keep the managed yt-dlp present and fresh in the background (the fix for
    // distro-frozen yt-dlp breaking playback). Never blocks startup or playback;
    // download progress and outcomes surface on the status line via Msg::Tools.
    let ytdlp_maintainer = if persistence_read_only {
        crate::util::background_task::BackgroundTask::disabled(
            "managed yt-dlp maintainer (read-only)",
        )
    } else {
        tools::ytdlp::spawn_maintainer(
            cfg.tools.clone(),
            runtime::sink(worker_tx.clone(), RuntimeEvent::Tools),
        )
    };

    // Background app-update check: resolve the latest GitHub release and, if we're behind,
    // surface it in the About card (+ nav-brand dot + one-time toast). Never blocks startup;
    // no-ops entirely when disabled or on a development build.
    let update_check = if persistence_read_only {
        crate::util::background_task::BackgroundTask::disabled("update check (read-only)")
    } else {
        update::spawn_update_check(
            current_version,
            cfg.update_check_enabled,
            runtime::sink(worker_tx.clone(), RuntimeEvent::Update),
        )
    };
    let mut terminal_background = TerminalBackgroundTasks {
        art_resize: art_resize_task,
        signal_handlers,
        ytdlp_maintainer,
        update_check,
    };

    // The remote-control accept loop is started - and the instance descriptor published - just
    // before the reducer loop below (see `remote.map(..server.start..)`), NOT here. Publishing
    // only once the app can actually service commands avoids a cold-start race where `ytt -r`
    // found a live descriptor but nothing was accepting/answering yet. The socket itself is
    // already bound (in `bind_or_detect`), so the single-instance guard is in force from launch.

    // Spawn mpv off the startup path. Until the IPC actor is ready, reducer-emitted
    // PlayerCmds are buffered by RuntimeHandles and replayed in order.
    let mut player_startup = spawn_audio_player(
        worker_tx.clone(),
        player_registry_dir.clone(),
        &player_runtime,
    );
    let mut player_ready_pending = true;
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
    let download_tx = worker_tx.clone();
    let download_handle = download::spawn(
        move |event| runtime::emit(&download_tx, RuntimeEvent::Download(event)),
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

    // An explicit Save is journaled in the stable recorder cache before its copy worker is
    // admitted. Recover those intents off the owner task and wait for the tracked job before
    // deleting ordinary incomplete/Decide temps or admitting any live recorder command. The
    // journal retains its originally accepted destination even if configuration changed.
    if recorder_temp_dir.is_some() {
        let report = handles
            .recover_recordings(app.recorder.temp_dir.clone(), cfg.effective_recording_dir())
            .await;
        app.recorder.capacity_blocked = report.capacity_blocked();
        if report.capacity_blocked() {
            app.status.kind = app::StatusKind::Error;
            app.status.text = recorder_capacity_blocked_status(&report);
            app.recorder.health_warning = Some(app.status.text.clone());
            app.recorder.health_sticky = true;
            app.dirty = true;
        } else if !report.warnings.is_empty() {
            tracing::warn!(
                recovered = report.recovered,
                pending = report.pending,
                warnings = ?report.warnings,
                "recording crash recovery completed with warnings"
            );
            app.status.kind = app::StatusKind::Error;
            app.status.text = if crate::i18n::is_korean() {
                format!(
                    "녹음 복구: {}개 복구, {}개 대기 — {}",
                    report.recovered, report.pending, report.warnings[0]
                )
            } else {
                format!(
                    "Recording recovery: {} recovered, {} pending — {}",
                    report.recovered, report.pending, report.warnings[0]
                )
            };
            app.recorder.health_warning = Some(app.status.text.clone());
            app.recorder.health_sticky = true;
            app.dirty = true;
        } else if report.recovered > 0 {
            app.status.kind = app::StatusKind::Info;
            app.status.text = if crate::i18n::is_korean() {
                format!("중단된 녹음 저장 {}개를 복구했어요", report.recovered)
            } else {
                format!("Recovered {} interrupted recording saves", report.recovered)
            };
            app.dirty = true;
        }
    }
    startup.mark("recorder_ready");

    // Opt-in autoplay-on-launch: every actor handle now exists, so reduce and dispatch this
    // owner-local startup action directly. Sending it through the bounded worker ingress before
    // the owner loop starts could reject the action behind an early burst of actor callbacks.
    if cfg.effective_autoplay_on_start() {
        for cmd in app.update(Msg::Autoplay) {
            handles.dispatch(&mut app, cmd);
        }
    }

    // OS media session (macOS Now Playing / Windows SMTC / Linux MPRIS): commands from
    // media keys and OS widgets come back through the normal message pump as
    // `Msg::Media`; artwork-cache results as `Msg::MediaArtworkReady`. Non-fatal when
    // the platform session can't initialize.
    let media_cmd_tx = worker_tx.clone();
    let media_art_tx = worker_tx.clone();
    let mut media = media::MediaSession::new_cancellable(
        cfg.effective_media_controls(),
        move |cmd, callback_cancellation| {
            let event = RuntimeEvent::App(Msg::Media(cmd));
            #[cfg(windows)]
            {
                // SMTC invokes this on its dedicated worker and has no busy return surface.
                // Retain the exact user command until admission, but let backend-generation
                // cancellation release it before a live Settings disable joins the worker.
                runtime::ingress::emit_callback_result_until(
                    &media_cmd_tx,
                    event,
                    callback_cancellation,
                )
            }
            #[cfg(not(windows))]
            {
                if callback_cancellation.is_cancelled() {
                    return Err(crate::util::delivery::DeliveryError::Closed);
                }
                // MPRIS returns the error to D-Bus; macOS translates it to CommandFailed and is
                // pumped by the owner thread, where blocking would self-deadlock.
                runtime::emit(&media_cmd_tx, event)
            }
        },
        move |ready| {
            runtime::emit_callback_observed(
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
    // A saturated scrobble lane can reject the final pause/stop after playback heartbeats cease.
    // Keep one parked retry clock so the current terminal snapshot is retried without requiring
    // another player event; the handle disables the guard immediately after admission.
    let mut scrobble_retry_tick = tokio::time::interval(Duration::from_millis(250));
    scrobble_retry_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
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
    // its owned shutdown removes the descriptor/socket and joins the accept tree. `None` =>
    // remote control unavailable this run; the app still works as a normal player.
    let (mut publisher, mut remote_guard) = match remote {
        Some(server) => {
            let (guard, hub) = server.start(runtime::remote_sink(worker_tx.clone()));
            // The v8 publisher shares the server's session hub; it runs on this loop
            // (the owner lane) right next to the media/scrobble post-turn observers.
            (Some(remote::publish::Publisher::new(hub)), Some(guard))
        }
        None => (None, None),
    };

    let mut pending_worker_events = BufferedWorkerEvents::default();
    let mut pending_shutdown_events: VecDeque<RuntimeEvent> = VecDeque::new();
    let mut owner_error = None;

    enum OwnerTurnInput {
        Local(Msg),
        BufferedWorker(RuntimeEvent),
        Worker(RuntimeEvent),
    }

    'owner: while !app.should_quit {
        if shutdown.is_triggered() {
            handles.begin_player_shutdown(&mut app);
            break;
        }
        if app.dirty {
            let Some(drew) = capture_owner_io_result(
                draw_app_frame(terminal, &mut app, &mut perf),
                &mut owner_error,
            ) else {
                break 'owner;
            };
            if drew {
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
        let input = if let Some(pending) =
            pending_worker_events.pop_current(|event| handles.player_event_is_current(event))
        {
            // Once a keyed wake has been accepted, finish that bounded owner batch before a
            // newer input mutation. Otherwise a terminal event buffered before `Next` could be
            // reduced against the track selected by `Next` and double-advance the queue.
            OwnerTurnInput::BufferedWorker(pending)
        } else {
            tokio::select! {
                biased;
                _ = shutdown.wait() => {
                    handles.begin_player_shutdown(&mut app);
                    break 'owner;
                },
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(
                    media.retry_deadline().unwrap_or_else(Instant::now)
                )), if media.retry_deadline().is_some() => {
                    media.publish(app.media_snapshot());
                    continue;
                },
                maybe = events.next() => match maybe {
                    // The translator maps physical mouse cells onto the zoom backend's
                    // virtual grid, so hit-testing (and double-click identity) stay correct
                    // while the UI is scaled.
                    Some(Ok(ev)) => {
                        let (cs, rs) = zoom.mouse_scale();
                        match input.translate(ev, cs, rs) {
                            Some(m) => OwnerTurnInput::Local(m),
                            None => continue,
                        }
                    }
                    Some(Err(_)) => continue,
                    None => break,
                },
                Some(event) = worker_rx.recv() => {
                    if let RuntimeEvent::Player(player_event) = &event
                        && !handles.player_event_is_current(player_event)
                    {
                        continue;
                    }
                    if event.is_telemetry_wake() {
                        pending_worker_events.extend(worker_tx.drain_coalesced());
                        continue;
                    } else {
                        OwnerTurnInput::Worker(event)
                    }
                },
                result = player_startup.recv(), if player_ready_pending => {
                    if shutdown.is_triggered() {
                        handles.begin_player_shutdown(&mut app);
                        break 'owner;
                    }
                    player_ready_pending = false;
                    let result = result.unwrap_or_else(|_| {
                        Err("player startup task stopped before reporting readiness".to_owned())
                    });
                    player_startup.join_finished().await;
                    if shutdown.is_triggered() {
                        // A signal can race the tiny send/join boundary on the multi-thread
                        // runtime. Drop a successful result here instead of installing a fresh
                        // process after the shutdown latch has won.
                        handles.begin_player_shutdown(&mut app);
                        drop(result);
                        break 'owner;
                    }
                    handles.handle_player_ready(result, &mut app);
                    if app.dirty {
                        let Some(drew) = capture_owner_io_result(
                            draw_app_frame(terminal, &mut app, &mut perf),
                            &mut owner_error,
                        ) else {
                            break 'owner;
                        };
                        if drew {
                            finish_draw_cycle(&mut app);
                        }
                        perf.maybe_log(&app);
                    }
                    continue;
                },
                _ = ime_scrub.tick(), if app.should_scrub_ime_preedit() => {
                    let Some(drew) = capture_owner_io_result(
                        draw_app_frame(terminal, &mut app, &mut perf),
                        &mut owner_error,
                    ) else {
                        break 'owner;
                    };
                    if drew {
                        finish_draw_cycle(&mut app);
                    }
                    perf.maybe_log(&app);
                    continue;
                },
                _ = status_tick.tick(), if app.status_visible() => OwnerTurnInput::Local(Msg::StatusTick),
                _ = anim_tick.tick(), if app.animation_active() => OwnerTurnInput::Local(Msg::AnimTick),
                _ = recording_tick.tick(), if app.recorder_active() => OwnerTurnInput::Local(Msg::RecordingTick),
                _ = scrobble_retry_tick.tick(), if handles.scrobble_retry_needed() => {
                    let snapshot = app.media_snapshot();
                    if let Err(error) = handles.scrobble_observe(&snapshot) {
                        tracing::debug!(%error, "scrobble terminal snapshot remains pending");
                    }
                    continue;
                },
                _ = media_pump.tick(), if media.wants_pump() => {
                    media.pump();
                    continue;
                },
            }
        };

        // The latch wins even if a queued TransportClosed or compatibility Signal event was
        // selected in the same scheduler turn.
        if shutdown.is_triggered() {
            match input {
                OwnerTurnInput::BufferedWorker(event) => pending_worker_events.push_front(event),
                OwnerTurnInput::Worker(event) => pending_shutdown_events.push_back(event),
                OwnerTurnInput::Local(_) => {}
            }
            handles.begin_player_shutdown(&mut app);
            break;
        }

        let msg = match input {
            OwnerTurnInput::Local(msg) => msg,
            OwnerTurnInput::BufferedWorker(event) => event.into(),
            // Owner lane (docs/gui/02 §8/§14): session subscribe ops run here, between reducer
            // turns, and never become a Msg. Keeping it as RuntimeEvent through the shutdown
            // latch check preserves the correlated request if shutdown wins this turn.
            OwnerTurnInput::Worker(RuntimeEvent::Remote(
                remote::server::RemoteEvent::SessionSubscribe {
                    session,
                    frame_id,
                    page_id,
                    topics,
                    settlement,
                },
            )) => {
                if let Some(publisher) = publisher.as_mut() {
                    publisher.handle_tracked_subscribe(
                        &app.core_view(),
                        &session,
                        page_id.as_deref(),
                        frame_id,
                        &topics,
                        settlement,
                    );
                }
                continue;
            }
            OwnerTurnInput::Worker(event) => event.into(),
        };

        let resized_artwork = matches!(&msg, Msg::ArtworkResized(_));
        // AnimTick/StatusTick only advance animations / expire the toast - they can't touch
        // anything a media snapshot reads, so skip rebuilding it (snapshot construction
        // allocates ~10 Strings, and AnimTick fires at up to the configured FPS).
        let media_inert = matches!(&msg, Msg::AnimTick | Msg::StatusTick);
        let media_before = (!media_inert).then(|| app.media_fingerprint());
        let cmds = app.update(msg);
        if owner_exit_requested(&app, &shutdown) {
            // Normal Quit has no follow-up effects. If an external signal raced the reducer,
            // retire transport before any effects from that turn can enter background actors.
            handles.begin_player_shutdown(&mut app);
            break;
        }
        for cmd in cmds {
            if shutdown.is_triggered() {
                handles.begin_player_shutdown(&mut app);
                break 'owner;
            }
            // Desktop notifications are handled here (not in `RuntimeHandles`) because the OSC
            // path writes to the terminal's stdout, which this scope owns; do it between frames
            // (before the draw below) so it never interleaves with a partial frame.
            if let Cmd::DesktopNotify { title, body } = cmd {
                notifier.emit(&title, &body);
                continue;
            }
            handles.dispatch(&mut app, cmd);
        }
        if owner_exit_requested(&app, &shutdown) {
            // A normal Quit or a signal can share a reducer turn with a queued TransportClosed.
            // Revoke the replacement before the sole runner spawn point below; relying on the
            // next `while` condition would leave this turn able to recreate mpv during teardown.
            handles.begin_player_shutdown(&mut app);
            break;
        }
        if handles.take_player_restart_request() {
            // The terminal event is the old actor's final emission. Reap the completed readiness
            // producer before starting the only successor, so no startup task is ever detached.
            player_startup.cancel_and_join().await;
            player_startup = spawn_audio_player(
                worker_tx.clone(),
                player_registry_dir.clone(),
                &player_runtime,
            );
            player_ready_pending = true;
        }
        if resized_artwork {
            perf.record_art_resize();
        }

        // The frame rate may have changed in Settings (committed to `config.animations` on close).
        // Rebuild the tick so the new rate applies without a relaunch - only when it actually
        // changed, so the common path costs one `u16` compare. Kept before the draw so the new
        // cadence is in force for this iteration.
        let new_fps = app.animation_tick_fps();
        if new_fps != anim_fps {
            anim_fps = new_fps;
            app.reset_animation_cadence();
            anim_tick = anim_interval(anim_fps);
        }

        // Draw first, so the keypress lands on screen with the least latency. Everything below
        // is pure output bookkeeping - it reads post-update state but feeds no rendering - so
        // running it *after* the frame lags the OS/remote surfaces by well under one frame while
        // leaving the resting on-screen output identical.
        if app.dirty {
            let Some(drew) = capture_owner_io_result(
                draw_app_frame(terminal, &mut app, &mut perf),
                &mut owner_error,
            ) else {
                break 'owner;
            };
            if drew {
                finish_draw_cycle(&mut app);
            }
        }

        // Mirror the post-update state to the OS media session: the facade diffs, so
        // this is one comparison when nothing media-visible changed. The enabled flag
        // tracks the Settings toggle live. The scrobbler taps the same snapshot first -
        // it must keep working when media controls are disabled (publish early-returns).
        // Scrobble cadence is unaffected by the inert skip: elapsed time is credited from
        // the 1 Hz PlayerTimePos observations, which always take this path.
        let media_enabled = app.config.effective_media_controls();
        let media_enabled_changed = media.set_enabled(media_enabled);
        let media_observed_turn = media_before.is_some();
        let media_changed = media_before.is_some_and(|before| before != app.media_fingerprint());
        let scrobble_due = media_observed_turn
            && app.media_scrobble_heartbeat_active()
            && handles.scrobble_heartbeat_due();
        let publish_due =
            media_changed || (media_enabled_changed && media_enabled) || media.retry_due();
        if media_changed || scrobble_due || publish_due {
            let snapshot = app.media_snapshot();
            if (media_changed || scrobble_due)
                && let Err(error) = handles.scrobble_observe(&snapshot)
            {
                let message = if error == crate::util::delivery::DeliveryError::Busy {
                    "Scrobble delivery is busy; retrying the current playback state.".to_owned()
                } else {
                    format!("Scrobble delivery unavailable: {}.", error.reason())
                };
                app.set_status_error(message);
            }
            if publish_due {
                media.publish(snapshot);
            }
        }
        // v8 push: fingerprint-diffed internally. Avoid building the borrowed core view
        // on idle standalone turns after the publisher has primed its baselines.
        if !media_inert
            && let Some(publisher) = publisher.as_mut()
            && publisher.should_observe(media_changed || media_enabled_changed)
        {
            publisher.observe(&app.core_view());
        }
        perf.maybe_log(&app);
    }

    // Every loop exit, including a fatal draw error, reaches the same ownership barrier before
    // any result is returned. The remote publisher is notified before slower actor/persistence
    // work so clients can begin reconnect/daemon handling promptly (docs/gui/02 §7).
    let mut teardown = LiveOwnerTeardown {
        app: &mut app,
        handles: &mut handles,
        player_startup: &mut player_startup,
        terminal_background: &mut terminal_background,
        media: &mut media,
        publisher: publisher.as_ref(),
        remote_guard: remote_guard.as_mut(),
        persist: &persist,
        pending_worker_events: &mut pending_worker_events,
        pending_shutdown_events: &mut pending_shutdown_events,
        worker_rx: &mut worker_rx,
    };
    complete_owner_teardown(&mut teardown, owner_error).await
}

fn recorder_capacity_blocked_status(report: &crate::recorder::job::RecoveryReport) -> String {
    if report.admission_uncertain {
        let detail = report
            .warnings
            .first()
            .map(String::as_str)
            .unwrap_or("recovery inventory could not be verified");
        if crate::i18n::is_korean() {
            format!("자동 녹음 일시 중지: 복구 저장 목록을 확인할 수 없음 — {detail}")
        } else {
            format!("Automatic recording paused: recovery inventory is uncertain — {detail}")
        }
    } else if crate::i18n::is_korean() {
        format!(
            "자동 녹음 일시 중지: 저장 대기 {}개 / {}바이트",
            report.pending, report.pending_bytes
        )
    } else {
        format!(
            "Automatic recording paused: {} pending / {} bytes",
            report.pending, report.pending_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    use crate::app::App;

    use super::*;

    #[test]
    fn uncertain_recorder_inventory_has_an_actionable_startup_status() {
        let _guard = crate::i18n::lock_for_test();
        let report = crate::recorder::job::RecoveryReport {
            admission_uncertain: true,
            warnings: vec!["registry enumeration failed".to_owned()],
            ..Default::default()
        };

        let status = recorder_capacity_blocked_status(&report);

        assert!(status.contains("inventory is uncertain"));
        assert!(status.contains("registry enumeration failed"));
        assert!(!status.contains("0 pending / 0 bytes"));
    }

    #[test]
    fn normal_quit_requests_owner_exit_without_an_external_signal() {
        let mut app = App::new(50);
        let shutdown = player::lifetime::ShutdownLatch::new();

        assert!(!owner_exit_requested(&app, &shutdown));
        app.should_quit = true;
        assert!(owner_exit_requested(&app, &shutdown));
    }

    #[tokio::test]
    async fn quit_during_player_startup_aborts_and_reaps_the_producer() {
        struct DropFlag(Arc<AtomicBool>);

        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let future_dropped = Arc::new(AtomicBool::new(false));
        let future_completed = Arc::new(AtomicBool::new(false));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let drop_flag = Arc::clone(&future_dropped);
        let completed = Arc::clone(&future_completed);
        let mut startup = spawn_player_startup(async move {
            let _drop_flag = DropFlag(drop_flag);
            let _ = started_tx.send(());
            let _ = release_rx.await;
            completed.store(true, Ordering::SeqCst);
            Err("late player startup".to_owned())
        });

        started_rx.await.expect("startup future entered");
        startup.cancel_and_join().await;

        assert!(future_dropped.load(Ordering::SeqCst));
        assert!(!future_completed.load(Ordering::SeqCst));
        assert!(startup.ready_rx.is_none());
        assert!(startup.task.is_none());
        assert!(release_tx.send(()).is_err());
    }

    #[test]
    fn perf_stats_track_draws_and_reset_after_log_window() {
        let mut stats = PerfStats {
            enabled: false,
            last_log: Instant::now() - Duration::from_secs(10),
            frames: 0,
            draw_total: Duration::ZERO,
            draw_max: Duration::ZERO,
            art_resizes: 0,
        };
        stats.record_draw(Duration::from_millis(7));
        stats.record_art_resize();
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.art_resizes, 0);

        stats.enabled = true;
        stats.record_draw(Duration::from_millis(7));
        stats.record_draw(Duration::from_millis(11));
        stats.record_art_resize();
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.draw_total, Duration::from_millis(18));
        assert_eq!(stats.draw_max, Duration::from_millis(11));
        assert_eq!(stats.art_resizes, 1);

        stats.maybe_log(&App::new(100));
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.draw_total, Duration::ZERO);
        assert_eq!(stats.draw_max, Duration::ZERO);
        assert_eq!(stats.art_resizes, 0);
    }

    #[tokio::test]
    async fn animation_tick_period_clamps_extreme_frame_rates() {
        assert_eq!(anim_tick_period(0), Duration::from_millis(1000));
        assert_eq!(anim_tick_period(1), Duration::from_millis(1000));
        assert_eq!(anim_tick_period(60), Duration::from_millis(16));
        assert_eq!(anim_tick_period(2_000), Duration::from_millis(1));

        let interval = anim_interval(30);
        assert_eq!(interval.period(), Duration::from_millis(33));
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
