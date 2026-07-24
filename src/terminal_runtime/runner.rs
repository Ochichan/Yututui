use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui_image::thread::ResizeRequest;
use tokio::time::MissedTickBehavior;

use crate::app::{self, App, Cmd, Msg};
use crate::runtime::RuntimeEvent;
use crate::{
    ai, api, artwork, config, deps, download, event, library, logging, lyrics, media, notify,
    persist, player, remote, resolver, runtime, scrobble, tools, tui, update, zoom,
};

use super::art::log_art_picker;
use super::persistence_shutdown::flush_owner_persistence;
use super::persistent_startup::{PersistentStartupState, TerminalStartupState};
use super::startup::StartupTrace;

mod buffered_events;
mod perf_stats;
mod player_startup;
mod recorder_status;
mod runtime_paths;
mod teardown;
#[cfg(test)]
mod teardown_tests;
mod terminal_events;
mod terminal_output;
use buffered_events::BufferedWorkerEvents;
use perf_stats::PerfStats;
#[cfg(test)]
use player_startup::spawn_player_startup;
use player_startup::{PlayerStartup, spawn_audio_player};
use recorder_status::recorder_capacity_blocked_status;
use runtime_paths::{TerminalRuntimePaths, resolve as terminal_runtime_paths};
use teardown::{OwnerIngressDrain, OwnerTeardown, complete_owner_teardown};
use terminal_events::TerminalEventWorker;
#[cfg(test)]
use terminal_output::finish_draw_cycle;
use terminal_output::{draw_app_frame, draw_full_app_frame};

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

fn owner_exit_requested(app: &App, shutdown: &player::lifetime::ShutdownLatch) -> bool {
    app.should_quit || shutdown.is_triggered()
}

/// The walkthrough changes persisted config on every transition, so a writer lease alone is not
/// sufficient: platforms without a resolvable config destination must keep it dormant too.
fn beginner_profile_persistable(
    persistence_read_only: bool,
    config_destination_available: bool,
) -> bool {
    !persistence_read_only && config_destination_available
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

/// Low-rate repaint clock for terminal-owned IME preedit text. The select branch is guarded
/// while an application text field is active and parks entirely once the event-armed burst
/// ([`app::IME_SCRUB_BURST_TICKS`] × 80 ms) runs out: preedit ghosts are created by terminal
/// activity, and every received terminal event re-arms the burst. Accepted trade-off (approved
/// in the CPU-plan UI/UX ledger): a terminal that repaints preedit without emitting any event
/// keeps its ghost until the next event, in exchange for zero periodic wakeups on an idle
/// screen — previously this clock woke the process ~12.5×/s forever.
fn ime_scrub_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(IME_SCRUB_PERIOD);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

fn lyrics_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval
}

/// Take only an immediately-adjacent time/cache pair. Both messages still pass through
/// `App::update` in their original order; sharing the post-update draw/observer work is safe while
/// skipping over any intervening message would not be.
fn take_adjacent_player_progress_pair(
    first: &Msg,
    pending: &mut BufferedWorkerEvents,
    mut player_is_current: impl FnMut(&crate::player::PlayerEvent) -> bool,
) -> Option<Msg> {
    let first_is_time = matches!(first, Msg::Player(app::PlayerMsg::TimePos(_)));
    let first_is_cache = matches!(first, Msg::Player(app::PlayerMsg::CacheTime(_)));
    if !first_is_time && !first_is_cache {
        return None;
    }

    let next = pending.pop_current(&mut player_is_current)?;
    let complementary = match &next {
        RuntimeEvent::Player(event) => {
            matches!(event.unscoped(), crate::player::PlayerEvent::CacheTime(_)) && first_is_time
                || matches!(event.unscoped(), crate::player::PlayerEvent::TimePos(_))
                    && first_is_cache
        }
        RuntimeEvent::App(Msg::Player(app::PlayerMsg::CacheTime(_))) => first_is_time,
        RuntimeEvent::App(Msg::Player(app::PlayerMsg::TimePos(_))) => first_is_cache,
        _ => false,
    };
    if complementary {
        Some(next.into())
    } else {
        pending.push_front(next);
        None
    }
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
        } else if paired.is_none()
            && matches!(first, Msg::AnimTick | Msg::StatusTick | Msg::LyricsTick)
        {
            Self::INERT
        } else {
            Self::PROJECTED
        }
    }
}

fn ime_scrub_state_requires_full_draw(
    reducer_turn_unrendered: bool,
    dirty: bool,
    clear_before_draw_pending: bool,
    radio_stream_active: bool,
) -> bool {
    reducer_turn_unrendered || dirty || clear_before_draw_pending || radio_stream_active
}

fn ime_scrub_requires_full_draw(app: &App, reducer_turn_unrendered: bool) -> bool {
    // Animation ticks own animation redraw cadence. The IME clock may scrub the current terminal
    // buffer between them, but must not turn its independent 80 ms period into extra full frames.
    ime_scrub_state_requires_full_draw(
        reducer_turn_unrendered,
        app.dirty,
        app.clear_before_draw_pending(),
        // Live-radio rendering reads `cache_time_at.elapsed()` for its stale-edge verdict, even
        // when the stream was started outside dedicated Radio mode.
        app.current_is_radio_stream(),
    ) || !app.ime_scrub_local_projection_fresh()
}

/// Remember reducer turns whose state has not reached a successful full draw yet. Animation ticks
/// are the exception: their draw credit deliberately leaves some ticks unrendered, and `dirty`
/// already sends due ticks through [`draw_full_app_frame`]. Preserve an older pending turn, but do
/// not let a skipped animation tick make the independent IME scrub clock bypass that cadence.
fn arm_unrendered_reducer_turn(reducer_turn_unrendered: &mut bool, msg: &Msg) {
    if !matches!(msg, Msg::AnimTick) {
        *reducer_turn_unrendered = true;
    }
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
    terminal_events: TerminalEventWorker,
    art_resize: crate::util::background_task::BackgroundTask,
    ytdlp_maintainer: crate::util::background_task::BackgroundTask,
    update_check: crate::util::background_task::BackgroundTask,
}

impl TerminalBackgroundTasks {
    async fn shutdown(&mut self) -> Option<std::io::Error> {
        let (terminal_error, _, _, _) = tokio::join!(
            self.terminal_events.shutdown(),
            self.art_resize.shutdown(),
            self.ytdlp_maintainer.shutdown(),
            self.update_check.shutdown(),
        );
        terminal_error
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

    async fn shutdown_terminal_background(&mut self) -> Option<std::io::Error> {
        self.terminal_background.shutdown().await
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

async fn finish_interrupted_startup(
    app: &App,
    persist: &persist::PersistHandle,
    terminal_error: Option<std::io::Error>,
) -> Result<()> {
    match (terminal_error, flush_owner_persistence(app, persist).await) {
        (None, Ok(())) => Ok(()),
        (None, Err(persistence_error)) => Err(persistence_error),
        (Some(terminal_error), Ok(())) => Err(terminal_error.into()),
        (Some(terminal_error), Err(persistence_error)) => Err(anyhow::Error::from(terminal_error)
            .context(format!(
                "persistence shutdown also failed: {persistence_error:#}"
            ))),
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
        keyboard_input_mode,
        shutdown,
    } = startup_state;
    if shutdown.is_triggered() {
        return Ok(());
    }
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
    app.terminal_keyboard_mode = keyboard_input_mode;
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
        personal_state,
        library,
        session_cache,
        signals,
        download_store,
        playlists: loaded_playlists,
        playlist_repair,
        station,
        romanization,
    } = persistent;
    app.personal_state = personal_state;
    app.library = Arc::new(library);
    app.signals = Arc::new(signals);
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
            crate::t!("Showing first", "처음", "先頭"),
            crate::t!(
                "download files; more are hidden",
                "개 다운로드 파일만 표시됨; 일부는 숨김",
                "件のダウンロードファイルのみ表示; 一部は非表示"
            )
        );
    }
    // Load local playlists (the DJ Gem playlist tools read/write these). Hand-edited or old
    // files are count-repaired on load; persist the repaired snapshot so startup does not keep
    // redoing the same repair every run.
    app.playlists = Arc::new(loaded_playlists);
    if playlist_repair.changed() {
        tracing::warn!(?playlist_repair, "playlists file was repaired on load");
        if playlist_repair.truncated() && app.status.text.is_empty() {
            app.status.kind = app::StatusKind::Info;
            app.status.text = format!(
                "{} ({} {}, {} {})",
                crate::t!(
                    "Saved playlists repaired",
                    "저장된 플레이리스트를 정리했어요",
                    "保存済みプレイリストを修復しました"
                ),
                playlist_repair.playlists_removed,
                crate::t!("lists removed", "개 목록 제거", "件のリストを削除"),
                playlist_repair.songs_removed,
                crate::t!("tracks removed", "곡 제거", "曲を削除")
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
        app.status.text = match crate::i18n::current() {
            crate::i18n::Language::Korean => {
                format!("보조 인스턴스 — 변경사항이 저장되지 않습니다: {reason}")
            }
            crate::i18n::Language::Japanese => {
                format!("セカンダリインスタンス — 変更は保存されません: {reason}")
            }
            _ => format!("Secondary instance — changes are not saved: {reason}"),
        };
        // Keep the first-frame warning persistent rather than arming the ordinary toast expiry.
        app.dirty = true;
    }

    // Start persistence only after the binary-level typed preflight accepted every startup load.
    // No actor or direct writer can run with a stale read-only fallback snapshot.
    let persist = persist::spawn();
    persist::install_panic_flush(persist.pending());
    // Push persisted playback/EQ settings (preset, bands, normalize, speed, autoplay).
    app.apply_config(&cfg);
    app.restore_last_session_from_cache(&session_cache);
    startup.mark("app_state_loaded");

    if shutdown.is_triggered() {
        return finish_interrupted_startup(&app, &persist, None).await;
    }
    let mut perf = PerfStats::from_env();
    if let Err(error) = draw_app_frame(terminal, &mut app, &mut perf, None) {
        if shutdown.is_triggered() {
            return finish_interrupted_startup(&app, &persist, None).await;
        }
        let owner_error = anyhow::Error::from(error);
        return match flush_owner_persistence(&app, &persist).await {
            Ok(()) => Err(owner_error),
            Err(persistence_error) => Err(owner_error.context(format!(
                "persistence shutdown also failed: {persistence_error:#}"
            ))),
        };
    }
    startup.mark("first_draw");
    if shutdown.is_triggered() {
        return finish_interrupted_startup(&app, &persist, None).await;
    }
    if std::env::var_os("YTM_EXIT_AFTER_FIRST_DRAW").is_some() {
        startup.mark("exit_after_first_draw");
        flush_owner_persistence(&app, &persist).await?;
        return Ok(());
    }

    // Establish exclusive input ownership and prove that the interactive terminal client is
    // still present before any mpv process (including capability probes) may be spawned. Terminal
    // liveness has its own out-of-band latch so a full worker queue can never delay teardown.
    let (mut events, mut terminal_event_worker, terminal_output) =
        match terminal_events::start(shutdown.clone()) {
            Ok(source) => source,
            Err(error) => {
                if shutdown.was_triggered_by_signal() {
                    return finish_interrupted_startup(&app, &persist, None).await;
                }
                let owner_error = anyhow::Error::from(error);
                return match flush_owner_persistence(&app, &persist).await {
                    Ok(()) => Err(owner_error),
                    Err(persistence_error) => Err(owner_error.context(format!(
                        "persistence shutdown also failed: {persistence_error:#}"
                    ))),
                };
            }
        };
    startup.mark("terminal_liveness_ready");

    if shutdown.is_triggered() {
        let terminal_error = terminal_event_worker
            .shutdown()
            .await
            .or_else(|| events.take_failure());
        return finish_interrupted_startup(&app, &persist, terminal_error).await;
    }

    // Build the owner lane after terminal readiness. The process-signal owner and this latch were
    // installed before any raw-mode startup work and remain owned by main through final restore.
    let (worker_tx, mut worker_rx) = runtime::channel(crate::util::backpressure::OWNER_EVENT_QUEUE);
    persist.set_event_sink(runtime::sink(worker_tx.clone(), RuntimeEvent::Persist));

    // Resolve which yt-dlp (managed vs system vs override) and mpv this process runs.
    // After first draw (a cold probe spawns `yt-dlp --version`, several hundred ms),
    // but before the deps preflight and the mpv spawn below, which both consume it.
    tools::init(&cfg.tools).await;
    startup.mark("tools_selected");
    if shutdown.is_triggered() {
        // The terminal watchdog or an OS signal may win while tool discovery is awaiting. Close
        // the terminal worker and return before the first mpv capability probe is admitted.
        player::lifetime::kill_mpv_now();
        let terminal_error = terminal_event_worker
            .shutdown()
            .await
            .or_else(|| events.take_failure());
        return finish_interrupted_startup(&app, &persist, terminal_error).await;
    }

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
    app.enable_runtime_tool_checks();
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
        app.show_tool_setup(app::ToolSetupContext::Startup, missing);
    }
    app.prepare_beginner_onboarding(beginner_profile_persistable(
        persistence_read_only,
        config::config_path().is_some(),
    ));
    startup.mark("deps_checked");

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
        terminal_events: terminal_event_worker,
        art_resize: art_resize_task,
        ytdlp_maintainer,
        update_check,
    };

    if shutdown.is_triggered() {
        // Capability probes and dependency preflight are bounded, but shutdown may arrive during
        // either. Re-check after all background ownership has been assembled and immediately
        // before creating the long-lived player startup task.
        player::lifetime::kill_mpv_now();
        let terminal_error = terminal_background
            .shutdown()
            .await
            .or_else(|| events.take_failure());
        return finish_interrupted_startup(&app, &persist, terminal_error).await;
    }

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
        shutdown.clone(),
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

    if let Some(cmd) = app.take_beginner_startup_persist() {
        handles.dispatch(&mut app, cmd);
    }

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
            app.status.text = match crate::i18n::current() {
                crate::i18n::Language::Korean => format!(
                    "녹음 복구: {}개 복구, {}개 대기 — {}",
                    report.recovered, report.pending, report.warnings[0]
                ),
                crate::i18n::Language::Japanese => format!(
                    "録音復旧: {}件復旧, {}件待機 — {}",
                    report.recovered, report.pending, report.warnings[0]
                ),
                _ => format!(
                    "Recording recovery: {} recovered, {} pending — {}",
                    report.recovered, report.pending, report.warnings[0]
                ),
            };
            app.recorder.health_warning = Some(app.status.text.clone());
            app.recorder.health_sticky = true;
            app.dirty = true;
        } else if report.recovered > 0 {
            app.status.kind = app::StatusKind::Info;
            app.status.text = match crate::i18n::current() {
                crate::i18n::Language::Korean => {
                    format!("중단된 녹음 저장 {}개를 복구했어요", report.recovered)
                }
                crate::i18n::Language::Japanese => {
                    format!("中断された録音保存を{}件復旧しました", report.recovered)
                }
                _ => format!("Recovered {} interrupted recording saves", report.recovered),
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

    let mut input = event::Translator::default();
    let mut ime_scrub = ime_scrub_interval();
    // Polled only while transient status or the lyric-delay OSD has a deadline; lets the reducer
    // restore the title and collapse the control after their respective TTLs. Idle otherwise.
    let mut status_tick = tokio::time::interval(Duration::from_millis(250));
    status_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut lyrics_tick = lyrics_interval();

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
    // Startup mutates App after the first paint. Until a later successful full render proves the
    // terminal matches all reducer-owned state, the IME clock must take the normal draw path.
    let mut reducer_turn_unrendered = true;

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
            let Some(_drew) = capture_owner_io_result(
                terminal_output.run_io(|deadline| {
                    draw_full_app_frame(
                        terminal,
                        &mut app,
                        &mut perf,
                        &mut reducer_turn_unrendered,
                        deadline,
                    )
                }),
                &mut owner_error,
            ) else {
                break 'owner;
            };
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
                maybe = events.recv() => match maybe {
                    // The translator maps physical mouse cells onto the zoom backend's
                    // virtual grid, so hit-testing (and double-click identity) stay correct
                    // while the UI is scaled.
                    Some(ev) => {
                        // Terminal activity is what creates terminal-owned preedit ghosts, so
                        // every received event — even one that translates to no message (focus,
                        // key release) — re-arms the bounded IME scrub burst.
                        app.arm_ime_scrub_burst();
                        let (cs, rs) = zoom.mouse_scale();
                        match input.translate_with_keymap(ev, cs, rs, &app.keymap) {
                            Some(m) => OwnerTurnInput::Local(m),
                            None => continue,
                        }
                    }
                    None => {
                        let error = events.take_failure().unwrap_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::BrokenPipe,
                                "terminal input worker stopped unexpectedly",
                            )
                        });
                        if owner_error.is_none() {
                            owner_error = Some(error.into());
                        }
                        shutdown.trigger();
                        player::lifetime::kill_mpv_now();
                        handles.begin_player_shutdown(&mut app);
                        break 'owner;
                    }
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
                    reducer_turn_unrendered = true;
                    if app.dirty {
                        let Some(_drew) = capture_owner_io_result(
                            terminal_output.run_io(|deadline| {
                                draw_full_app_frame(
                                    terminal,
                                    &mut app,
                                    &mut perf,
                                    &mut reducer_turn_unrendered,
                                    deadline,
                                )
                            }),
                            &mut owner_error,
                        ) else {
                            break 'owner;
                        };
                        perf.maybe_log(&app);
                    }
                    continue;
                },
                _ = ime_scrub.tick(), if app.should_scrub_ime_preedit() => {
                    app.consume_ime_scrub_tick();
                    let fast_succeeded = if ime_scrub_requires_full_draw(
                        &app,
                        reducer_turn_unrendered,
                    ) {
                        false
                    } else {
                        match terminal_output.run_io(|deadline| {
                            tui::scrub_ime_preedit_until(
                                terminal,
                                app.synchronized_draw_active(),
                                deadline,
                            )
                        }) {
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
                        let Some(_drew) = capture_owner_io_result(
                            terminal_output.run_io(|deadline| {
                                draw_full_app_frame(
                                    terminal,
                                    &mut app,
                                    &mut perf,
                                    &mut reducer_turn_unrendered,
                                    deadline,
                                )
                            }),
                            &mut owner_error,
                        ) else {
                            break 'owner;
                        };
                    }
                    perf.maybe_log(&app);
                    continue;
                },
                _ = status_tick.tick(), if app.status_visible() || app.lyrics.delay_osd_until.is_some() => OwnerTurnInput::Local(Msg::StatusTick),
                _ = lyrics_tick.tick(), if app.lyrics_clock_active() => OwnerTurnInput::Local(Msg::LyricsTick),
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
            OwnerTurnInput::BufferedWorker(mut event) | OwnerTurnInput::Worker(mut event) => {
                if let RuntimeEvent::Player(player_event) = &mut event {
                    handles.reconcile_cache_safety_event(player_event);
                }
                event.into()
            }
        };

        let paired_progress =
            take_adjacent_player_progress_pair(&msg, &mut pending_worker_events, |event| {
                handles.player_event_is_current(event)
            });
        let observer_plan = ObserverPlan::for_messages(&msg, paired_progress.as_ref());

        let resized_artwork = matches!(&msg, Msg::ArtworkResized(_))
            || matches!(&paired_progress, Some(Msg::ArtworkResized(_)));
        // Progress updates only interpolation/live-sync clocks. Neither clock is a projected
        // facet: OS media and remote clients interpolate elapsed independently, while seeks bump
        // `position_epoch` through a different message. Skip both hashes on this high-rate path.
        let media_before = observer_plan.project_state.then(|| app.media_fingerprint());
        arm_unrendered_reducer_turn(&mut reducer_turn_unrendered, &msg);
        for msg in std::iter::once(msg).chain(paired_progress) {
            if shutdown.is_triggered() {
                handles.begin_player_shutdown(&mut app);
                break 'owner;
            }
            let cmds = app.update(msg);
            if owner_exit_requested(&app, &shutdown) {
                // Quit has no follow-up effects. Retire transport before commands from the same
                // reducer turn can enter background actors.
                handles.begin_player_shutdown(&mut app);
                break 'owner;
            }
            for cmd in cmds {
                if shutdown.is_triggered() {
                    handles.begin_player_shutdown(&mut app);
                    break 'owner;
                }
                // Desktop notifications are handled here (not in `RuntimeHandles`) because the
                // OSC path writes to the terminal's stdout, which this scope owns; do it between
                // frames (before the draw below) so it never interleaves with a partial frame.
                if let Cmd::DesktopNotify { title, body } = cmd {
                    if capture_owner_io_result(
                        terminal_output.run_io_for(
                            crate::terminal_policy::NOTIFICATION_OUTPUT_TIMEOUT,
                            |deadline| notifier.emit_until(&title, &body, deadline),
                        ),
                        &mut owner_error,
                    )
                    .is_none()
                    {
                        handles.begin_player_shutdown(&mut app);
                        break 'owner;
                    }
                    continue;
                }
                handles.dispatch(&mut app, cmd);
            }
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
                shutdown.clone(),
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
        let _fps_changed = sync_animation_interval(&mut app, &mut anim_fps, &mut anim_tick);

        // Draw first, so the keypress lands on screen with the least latency. Everything below
        // is pure output bookkeeping - it reads post-update state but feeds no rendering - so
        // running it *after* the frame lags the OS/remote surfaces by well under one frame while
        // leaving the resting on-screen output identical.
        if app.dirty {
            let Some(_drew) = capture_owner_io_result(
                terminal_output.run_io(|deadline| {
                    draw_full_app_frame(
                        terminal,
                        &mut app,
                        &mut perf,
                        &mut reducer_turn_unrendered,
                        deadline,
                    )
                }),
                &mut owner_error,
            ) else {
                break 'owner;
            };
        }

        // Only a turn that can mutate projected state may toggle/rebuild OS metadata. Progress
        // still checks the independent heartbeat gate below, so scrobbling remains ~1 Hz even
        // when media controls are disabled or no remote topic is subscribed.
        // Reconcile the live setting on every owner turn. The common progress path is a borrowed
        // boolean comparison, while a re-enable must force one full current snapshot.
        let media_enabled = app.config.effective_media_controls();
        let media_enabled_changed = media.set_enabled(media_enabled);
        let media_enable_publish_due = media_enabled_changed && media_enabled;
        let media_changed = media_before.is_some_and(|before| before != app.media_fingerprint());
        let scrobble_due = observer_plan.drive_scrobble_heartbeat
            && app.media_scrobble_heartbeat_active()
            && handles.scrobble_heartbeat_due();
        let publish_due = media_changed || media_enable_publish_due || media.retry_due();
        if scrobble_due || publish_due {
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

    // Stop terminal probing and input delivery at the owner-loop boundary, not several actor
    // joins later. Otherwise a clean quit leaves an unconsumed event queue and live heartbeats
    // which can manufacture a terminal failure while orderly teardown is already in progress.
    shutdown.trigger();

    // A terminal failure is stored before its out-of-band latch is triggered. Preserve that cause
    // even when the biased shutdown branch won the select turn and no channel item was consumed.
    owner_error = teardown::prefer_terminal_failure(owner_error, events.take_failure());

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

#[cfg(test)]
mod tests;
