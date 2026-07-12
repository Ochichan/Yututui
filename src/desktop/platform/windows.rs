//! Windows notification-area backend for `yututray`.

use std::collections::HashMap;
use std::error::Error;
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::desktop::app::{
    DesktopApp, DesktopEffect, DesktopEvent, DesktopTransition, DesktopWindowEvent, FrontendReplay,
    WindowKind,
};
use crate::desktop::control;
use crate::desktop::executor::{DesktopCommandExecutor, SubmitError};
use crate::desktop::launch;
use crate::desktop::menu_model::{self, MenuAction, MenuEntry, MenuItem as ModelItem, TrayState};
use crate::desktop::panel::{self, DesktopCommandError, PanelCommand, PanelRequest, PanelTheme};
use crate::desktop::platform::main_window::MainWindow;
use crate::desktop::platform::panel_window::MiniPlayerPanel;
use crate::desktop::single_instance::{self, Acquire, ActivationIntent};
use crate::desktop::startup::{self, StartupStatus};
use crate::desktop::status::{self, PollConfig, PollUpdate};
use crate::desktop::window_state::DesktopState;
use crate::desktop::{bridge, gateway};
use crate::remote::proto::{InstanceMode, PushEvent, RemoteCommand, Topic};

const APP_ID: &str = "io.github.ochi.yututui.tray";
const POLL_THREAD_NAME: &str = "yututray-status";
const COMMAND_THREAD_NAME: &str = "yututray-command";
const ICO_BYTES: &[u8] = include_bytes!("../../../assets/icons/yututui.ico");
const GEOMETRY_SAVE_DELAY: Duration = Duration::from_millis(500);
const MINI_WEBVIEW_GRACE: Duration = Duration::from_secs(15);
const MAIN_WEBVIEW_GRACE: Duration = Duration::from_secs(60);

/// Give the mini player a true tool-window identity. tao's skip-taskbar flag
/// removes the taskbar tab, while WS_EX_TOOLWINDOW also excludes the window
/// from Alt-Tab. Deliberately do not set WS_EX_NOACTIVATE: keyboard users must
/// still be able to focus and operate the panel.
pub(super) fn configure_mini_window(window: &tao::window::Window) -> Result<(), Box<dyn Error>> {
    use tao::platform::windows::WindowExtWindows;
    use windows::Win32::Foundation::{GetLastError, HWND, SetLastError, WIN32_ERROR};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };

    window.set_skip_taskbar(true)?;
    let hwnd = HWND(window.hwnd() as *mut core::ffi::c_void);
    // SAFETY: `hwnd` belongs to this live tao window and all calls run on the
    // owning event-loop thread. We preserve every unrelated extended-style bit.
    unsafe {
        SetLastError(WIN32_ERROR(0));
        let current = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let get_error = GetLastError();
        if window_long_call_failed(current, get_error.0) {
            return Err(windows::core::Error::from(get_error).into());
        }
        let role = (current & !(WS_EX_APPWINDOW.0 as isize)) | WS_EX_TOOLWINDOW.0 as isize;
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, role);
        let set_error = GetLastError();
        // SetWindowLongPtrW legitimately returns zero when the previous style was zero, so a
        // zero return is an error only when GetLastError was also set by this call.
        if window_long_call_failed(previous, set_error.0) {
            return Err(windows::core::Error::from(set_error).into());
        }
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
        )?;
    }
    Ok(())
}

fn window_long_call_failed(result: isize, last_error: u32) -> bool {
    result == 0 && last_error != 0
}

/// Physical work area for the monitor nearest a point, excluding taskbars and
/// other app bars. This is the native source of truth missing from tao 0.34.
pub(super) fn work_area_for_point(
    (x, y): (f64, f64),
) -> Option<crate::desktop::window_state::MonitorRect> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    };

    // SAFETY: MONITORINFO has the documented size and points to writable stack
    // storage for the duration of GetMonitorInfoW.
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: x.round() as i32,
                y: y.round() as i32,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let rect = info.rcWork;
        Some(crate::desktop::window_state::MonitorRect {
            x: rect.left,
            y: rect.top,
            w: rect.right.saturating_sub(rect.left) as u32,
            h: rect.bottom.saturating_sub(rect.top) as u32,
        })
    }
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDoubleClickTime() -> u32;
}

fn system_double_click_time() -> u32 {
    // SAFETY: the function has no arguments and only reads a user preference.
    unsafe { GetDoubleClickTime() }
}

fn set_process_app_id(app_id: &[u16]) -> i32 {
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    // SAFETY: the caller supplies a live NUL-terminated UTF-16 buffer.
    unsafe { SetCurrentProcessExplicitAppUserModelID(app_id.as_ptr()) }
}

pub(crate) fn show_native_error(title: &[u16], message: &[u16]) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::PCWSTR;

    // SAFETY: both pointers reference NUL-terminated UTF-16 buffers that remain alive for
    // the duration of the synchronous call; a null owner is explicitly supported.
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(message.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn run(
    mut initial_intent: ActivationIntent,
    secondary_intent: ActivationIntent,
) -> Result<(), Box<dyn Error>> {
    if initial_intent == ActivationIntent::ShowMain && !crate::desktop::assets::DIST_EMBEDDED {
        initial_intent = ActivationIntent::EnsureTray;
    }
    set_app_user_model_id();

    // Single GUI instance (docs/gui/03 §6): a second launch activates the first and exits.
    // Held until the process exits (tao's run() diverges, so this never drops early).
    let _instance = match single_instance::acquire()? {
        Acquire::Primary(guard) => Some(guard),
        Acquire::AlreadyRunning => {
            single_instance::signal_activation(secondary_intent)?;
            return Ok(());
        }
    };

    // Own the activation endpoint immediately after the process lock. Startup self-heal and
    // native event-loop creation happen afterward; requests arriving in that interval are kept
    // in a bounded FIFO and replayed once the proxy exists.
    let deferred_activations =
        single_instance::DeferredActivations::<EventLoopProxy<UserEvent>>::new();
    single_instance::spawn_activation_listener({
        let deferred_activations = deferred_activations.clone();
        move |intent| {
            if intent == ActivationIntent::ShowMain && !crate::desktop::assets::DIST_EMBEDDED {
                return false;
            }
            deferred_activations.deliver_or_defer(intent, |proxy, intent| {
                proxy.send_event(UserEvent::Activation(intent)).is_ok()
            })
        }
    })?;

    crate::desktop::persistence::initialize_writer()?;
    let log_guard = init_file_logging();
    install_tray_panic_hook();

    // Only the primary may mutate startup registration; concurrent secondaries must not race
    // the same registry value during login storms.
    startup::self_heal();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let rejected = deferred_activations.install(proxy.clone(), |proxy, intent| {
        proxy.send_event(UserEvent::Activation(intent)).is_ok()
    });
    if rejected != 0 {
        tracing::warn!(target: "ytt_desktop", rejected, "queued activations could not reach the event loop");
    }

    MenuEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |event: MenuEvent| {
            if let Some(message) = user_event_from_menu_id(event.id()) {
                let _ = proxy.send_event(message);
            }
        }
    }));

    TrayIconEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |event: TrayIconEvent| match event {
            // Left click is the media-companion convention: pop the mini player,
            // anchored near the icon. The context menu lives on right click.
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                position,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                position,
                ..
            } => {
                let _ = proxy.send_event(UserEvent::ShowMiniPlayer(Some((position.x, position.y))));
            }
            TrayIconEvent::Click {
                button: MouseButton::Right,
                button_state: MouseButtonState::Up,
                ..
            } => {
                let _ = proxy.send_event(UserEvent::ShowMenu);
            }
            _ => {}
        }
    }));

    let mut app = WindowsTrayApp::new(proxy.clone(), log_guard, initial_intent)?;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Err(e) = app.init(target) {
                    report_error(e);
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::Resumed => app.reconcile_window_placements(),
            Event::UserEvent(UserEvent::Gateway(ev)) => {
                app.handle_gateway(ev);
            }
            Event::UserEvent(UserEvent::Main(req)) => {
                app.handle_main(req, target);
            }
            Event::UserEvent(UserEvent::Activation(intent)) => {
                app.handle_activation(intent, target);
            }
            Event::UserEvent(UserEvent::Status {
                generation,
                update,
                restart_fallback,
            }) => {
                app.apply_poll_update(generation, update, restart_fallback);
            }
            Event::UserEvent(UserEvent::ShowMenu) => {
                app.show_menu();
            }
            Event::UserEvent(UserEvent::ShowMiniPlayer(anchor)) => {
                app.handle_tray_mini_request(target, anchor);
            }
            Event::UserEvent(UserEvent::Panel {
                generation,
                request,
            }) => {
                app.handle_panel_request(generation, request, target);
            }
            Event::UserEvent(UserEvent::PanelResult {
                generation,
                id,
                error,
            }) => {
                app.apply_panel_result(generation, id, error.as_ref());
            }
            Event::UserEvent(UserEvent::Refresh) => {
                app.request_status_now();
            }
            Event::UserEvent(UserEvent::StartupChanged { error }) => {
                app.finish_startup_toggle(error.as_deref());
            }
            Event::UserEvent(UserEvent::Quit) => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::Menu(action)) => {
                app.handle_action(action);
            }
            Event::UserEvent(UserEvent::DaemonPrimary) => {
                app.handle_daemon_primary();
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                app.handle_window_close(window_id, target);
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Focused(focused),
                ..
            } => {
                app.handle_panel_focus(window_id, focused, target);
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Moved(_) | WindowEvent::Resized(_),
                ..
            } => {
                app.handle_geometry_changed(window_id);
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::ScaleFactorChanged { .. },
                ..
            } => app.handle_scale_factor_changed(window_id),
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Destroyed,
                ..
            } => {
                app.handle_window_destroyed(window_id, target);
            }
            Event::LoopDestroyed => {
                app.shutdown();
            }
            _ => {}
        }
        app.process_deadlines(target);
        if !matches!(*control_flow, ControlFlow::Exit)
            && let Some(deadline) = app.next_deadline()
        {
            *control_flow = ControlFlow::WaitUntil(deadline);
        }
    });
}

struct WindowsTrayApp {
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    menu: Option<WindowsMenu>,
    panel: Option<MiniPlayerPanel>,
    main_window: Option<MainWindow>,
    gateway: Option<gateway::GatewayHandle>,
    desktop_app: DesktopApp,
    last_conn: gateway::ConnState,
    initial_intent: ActivationIntent,
    last_update: PollUpdate,
    /// Cached while the core is online so a later offline recovery surface does not need to
    /// parse the session/library files on the native event loop.
    resume_available: bool,
    // Menu/tooltip fingerprint of the last poll — lets `apply_update` skip the native menu
    // rebuild + tooltip set when nothing the menu shows changed (see `menu_signature`).
    last_menu_signature: menu_model::MenuSignature,
    poll_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    poll_thread: Option<thread::JoinHandle<()>>,
    poll_done: Option<std::sync::mpsc::Receiver<()>>,
    poll_generation: Arc<AtomicU64>,
    poll_lane: status::PollLane,
    // Held until LoopDestroyed: tao's run() never returns (it exits the process), so
    // dropping this in shutdown() is the only chance the non-blocking appender gets
    // to flush the final log lines.
    log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    menu_dismissed_at: Option<Instant>,
    geometry_dirty_at: Option<Instant>,
    last_tray_panel_click: Option<Instant>,
    command_executor: Option<DesktopCommandExecutor>,
    panel_teardown_at: Option<Instant>,
    main_teardown_at: Option<Instant>,
    panel_gateway_requests: HashMap<u64, (u64, u64)>,
    startup_toggle_pending: bool,
}

impl WindowsTrayApp {
    fn new(
        proxy: EventLoopProxy<UserEvent>,
        log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
        initial_intent: ActivationIntent,
    ) -> Result<Self, Box<dyn Error>> {
        let mut desktop_app = DesktopApp::default();
        let desktop_state = DesktopState::load();
        desktop_app.restore_mini_pinned(desktop_state.mini_pinned);
        let resume_available = crate::session::resume_available();
        Ok(Self {
            proxy,
            tray: None,
            menu: None,
            panel: None,
            main_window: None,
            gateway: None,
            desktop_app,
            last_conn: gateway::ConnState::Connecting,
            initial_intent,
            last_update: PollUpdate::disconnected_with_resume(
                control::ControlError::NotRunning,
                resume_available,
            ),
            resume_available,
            // Matches the Disconnected menu built in `init`; the first real poll re-applies.
            last_menu_signature: menu_model::MenuSignature::Disconnected { resume_available },
            poll_shutdown: None,
            poll_thread: None,
            poll_done: None,
            poll_generation: Arc::new(AtomicU64::new(0)),
            poll_lane: status::PollLane::default(),
            log_guard,
            menu_dismissed_at: None,
            geometry_dirty_at: None,
            last_tray_panel_click: None,
            command_executor: Some(DesktopCommandExecutor::spawn(COMMAND_THREAD_NAME)?),
            panel_teardown_at: None,
            main_teardown_at: None,
            panel_gateway_requests: HashMap::new(),
            startup_toggle_pending: false,
        })
    }

    fn init(&mut self, target: &EventLoopWindowTarget<UserEvent>) -> Result<(), Box<dyn Error>> {
        let menu = WindowsMenu::new(&self.last_update.state)?;
        let icon = app_icon()?;
        let tray = TrayIconBuilder::new()
            .with_id(APP_ID)
            .with_menu(Box::new(menu.root.clone()))
            .with_tooltip(tooltip_for_state(&self.last_update.state))
            .with_icon(icon)
            // Show the menu from the tao event loop instead of tray-icon's Windows
            // WindowProc callback; this avoids click-time menu reentrancy crashes.
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(false)
            .build()?;

        self.tray = Some(tray);
        self.menu = Some(menu);
        self.start_gateway();
        self.handle_activation(self.initial_intent, target);
        Ok(())
    }

    /// Spawn the persistent v8 session thread; connection-state events route back to the
    /// loop as `UserEvent::Gateway` (docs/gui/03 §3.2).
    fn start_gateway(&mut self) {
        if self.gateway.is_some() {
            return;
        }
        let proxy = self.proxy.clone();
        self.gateway = Some(gateway::spawn(move |ev| {
            let _ = proxy.send_event(UserEvent::Gateway(ev));
        }));
    }

    fn handle_gateway(&mut self, ev: gateway::GatewayEvent) {
        match ev {
            gateway::GatewayEvent::Connection(state) => {
                let disconnected = match &state {
                    gateway::ConnState::Connecting => {
                        Some(control::ControlError::Transport("connecting".to_string()))
                    }
                    gateway::ConnState::Offline { reason } => {
                        Some(control::ControlError::Transport(reason.clone()))
                    }
                    gateway::ConnState::Online { .. } => None,
                };
                self.desktop_app.set_connection(&state);
                self.last_conn = state;
                if !matches!(self.last_conn, gateway::ConnState::Online { .. }) {
                    let error = DesktopCommandError::new(
                        "offline",
                        "The player connection was interrupted",
                        true,
                    );
                    let pending = self
                        .panel_gateway_requests
                        .drain()
                        .map(|(_, request)| request)
                        .collect::<Vec<_>>();
                    for (generation, request_id) in pending {
                        self.apply_panel_result(generation, request_id, Some(&error));
                    }
                }
                match &self.last_conn {
                    gateway::ConnState::Offline { reason } if reason == "no_v8" => {
                        self.start_polling();
                    }
                    // The compatibility poll belongs exclusively to the explicit
                    // no-v8 state.  Stop it while reconnecting and for every other
                    // offline reason so an old fallback result cannot outlive the
                    // core/session that authorized it.
                    _ => self.stop_polling(),
                }
                if let Some(error) = disconnected {
                    // Never leave stale track/actions interactive across a core disconnect.
                    self.apply_update(PollUpdate::disconnected_with_resume(
                        error,
                        self.resume_available,
                    ));
                }
                if let Some(main) = &self.main_window {
                    main.eval(&bridge::receive_script(&bridge::InEnvelope::conn(
                        self.last_conn.to_conn_payload(),
                    )));
                }
            }
            // A topic push or correlated reply from the session — hand it straight to the
            // page. Frames that arrive with no window open are dropped; the window re-subs
            // and gets fresh snapshots when it next loads (docs/gui/03 §3.2).
            gateway::GatewayEvent::Frame(env) => self.handle_gateway_frame(env, None),
            gateway::GatewayEvent::PageFrame {
                envelope: env,
                source_generation,
            } => self.handle_gateway_frame(env, source_generation),
            gateway::GatewayEvent::Push {
                sequence,
                topic,
                event,
                envelope,
            } => {
                if self.desktop_app.apply_push(sequence, topic, event) {
                    // Apply the shared sequence gate before any consumer sees the frame.
                    if let Some(main) = &self.main_window {
                        main.eval(&bridge::receive_script(&envelope));
                    }
                    self.apply_snapshot_locale();
                    if let Some(status) = self.desktop_app.status_projection() {
                        self.apply_update(PollUpdate::connected(status));
                    }
                }
            }
        }
    }

    fn handle_gateway_frame(&mut self, env: bridge::InEnvelope, source_generation: Option<u64>) {
        if source_generation.is_none() && gateway::is_native_request_id(env.id) {
            self.handle_native_command_result(env);
            return;
        }
        if let Some(main) = &self.main_window
            && source_generation.is_none_or(|generation| generation == main.page_generation())
        {
            main.eval(&bridge::receive_script(&env));
        }
    }

    fn apply_snapshot_locale(&mut self) {
        let Some(settings) = &self.desktop_app.snapshot().settings else {
            return;
        };
        let language = if settings.ui.language == "ko" {
            crate::i18n::Language::Korean
        } else {
            crate::i18n::Language::English
        };
        if crate::i18n::current() != language {
            crate::i18n::set_language(language);
            // Force one native menu rebuild even when playback fields did not change.
            self.last_menu_signature = menu_model::MenuSignature::Disconnected {
                resume_available: !self.resume_available,
            };
        }
    }

    fn handle_native_command_result(&mut self, env: bridge::InEnvelope) {
        let panel_request = env
            .id
            .and_then(|gateway_id| self.panel_gateway_requests.remove(&gateway_id));
        if panel_request.is_some_and(|(generation, _)| {
            self.panel
                .as_ref()
                .is_none_or(|panel| panel.page_generation() != generation)
        }) {
            // The source page no longer exists. Drop the whole result—including its native
            // error projection—so a rejected gen-1 command cannot toast over a rebuilt gen-2.
            return;
        }
        match env.kind {
            bridge::InKind::Res => {
                // v8 push snapshots are authoritative. A response status can be older than a
                // push already accepted by DesktopApp, so it must never overwrite projection.
                self.complete_panel_request(panel_request, None);
            }
            bridge::InKind::Err => {
                let reason = env
                    .payload
                    .as_ref()
                    .and_then(|payload| {
                        payload
                            .get("reason")
                            .or_else(|| payload.get("code"))
                            .and_then(|value| value.as_str())
                    })
                    .unwrap_or("rejected")
                    .to_string();
                let update = PollUpdate {
                    state: self.last_update.state.clone(),
                    error: Some(control::ControlError::Rejected(reason.clone())),
                };
                let command_error = panel::command_error_from_control(
                    update.error.as_ref().expect("rejection error was set"),
                );
                if panel_request.is_none() {
                    report_command_error(&command_error.display_message);
                }
                self.apply_update(update);
                self.complete_panel_request(panel_request, Some(&command_error));
                if reason == "stale_rev"
                    && let Some(gateway) = &self.gateway
                {
                    let _ = gateway.refresh_topic(Topic::Queue);
                }
            }
            _ => self.complete_panel_request(panel_request, None),
        }
    }

    fn handle_main(&mut self, req: MainRequest, target: &EventLoopWindowTarget<UserEvent>) {
        match req {
            MainRequest::Ipc { generation, body } => {
                if self
                    .main_window
                    .as_ref()
                    .is_none_or(|main| generation != main.page_generation())
                {
                    return;
                }
                match bridge::dispatch(&body) {
                    bridge::BridgeAction::Reply(env) => {
                        if let Some(main) = &self.main_window {
                            main.eval(&bridge::receive_script(&env));
                        }
                    }
                    bridge::BridgeAction::Win(op) => self.handle_win_op(op, target),
                    // Commands/requests/subscriptions go to the live v8 session; its replies and
                    // topic pushes come back as `UserEvent::Gateway(Frame(..))`.
                    bridge::BridgeAction::ToGateway(env) => {
                        if let Some(rejection) = gateway::send_or_reject_from_generation(
                            self.gateway.as_ref(),
                            env,
                            Some(generation),
                        ) && let Some(main) = &self.main_window
                            && main.page_generation() == generation
                        {
                            main.eval(&bridge::receive_script(&rejection));
                        }
                    }
                    bridge::BridgeAction::Ignore => {}
                }
            }
        }
    }

    fn handle_activation(
        &mut self,
        intent: ActivationIntent,
        target: &EventLoopWindowTarget<UserEvent>,
    ) {
        if intent == ActivationIntent::ShowMain && !crate::desktop::assets::DIST_EMBEDDED {
            return;
        }
        let _ = self.dispatch_desktop_event(DesktopEvent::Activation(intent), target, None);
    }

    fn handle_win_op(&mut self, op: bridge::WinOp, target: &EventLoopWindowTarget<UserEvent>) {
        match op {
            bridge::WinOp::FrontendReady => self.replay_main_frontend(target),
            bridge::WinOp::Hide => {
                self.persist_main_geometry();
                let _ = self.dispatch_desktop_event(
                    DesktopEvent::WindowVisibility {
                        kind: WindowKind::Main,
                        visible: false,
                    },
                    target,
                    None,
                );
            }
            bridge::WinOp::Drag => {
                if let Some(main) = &self.main_window {
                    main.start_drag();
                }
            }
            bridge::WinOp::StartDaemon => self.start_daemon(false),
            bridge::WinOp::CopyText(text) => {
                if let Err(error) = crate::desktop::clipboard::copy_text(&text) {
                    report_error(format_args!("could not copy text: {error}"));
                }
            }
            bridge::WinOp::OpenUrl(url) if !url.trim().is_empty() => {
                let opened = crate::util::browser::open_in_browser_checked(&url);
                if !opened.launched() {
                    report_error(format_args!(
                        "could not open URL in browser: {}",
                        opened.failure_summary()
                    ));
                }
            }
            bridge::WinOp::PersistUi(snapshot) => {
                if let Some(main) = &self.main_window {
                    main.cache_ui_snapshot(snapshot);
                }
            }
            _ => {}
        }
    }

    fn replay_main_frontend(&mut self, target: &EventLoopWindowTarget<UserEvent>) {
        let mut transition = self
            .desktop_app
            .handle_event(DesktopEvent::FrontendReady(WindowKind::Main));
        let Some((WindowKind::Main, replay)) = transition.replay.take() else {
            return;
        };
        let Some(main) = &self.main_window else {
            return;
        };
        main.mark_frontend_ready();
        gateway::refresh_ready_main_frontend(self.gateway.as_ref());
        main.eval(&bridge::receive_script(&bridge::InEnvelope::conn(
            self.last_conn.to_conn_payload(),
        )));
        if let Some(model) = replay.snapshot.player {
            main.eval(&bridge::receive_script(&bridge::InEnvelope::event(
                "player",
                serde_json::to_value(PushEvent::PlayerSnapshot {
                    model: Box::new(model),
                })
                .unwrap_or(serde_json::Value::Null),
            )));
        }
        if let Some(model) = replay.snapshot.queue {
            main.eval(&bridge::receive_script(&bridge::InEnvelope::event(
                "queue",
                serde_json::to_value(PushEvent::QueueSnapshot { model })
                    .unwrap_or(serde_json::Value::Null),
            )));
        }
        if let Some(model) = replay.snapshot.settings {
            main.eval(&bridge::receive_script(&bridge::InEnvelope::event(
                "settings",
                serde_json::to_value(PushEvent::SettingsSnapshot {
                    model: Box::new(model),
                })
                .unwrap_or(serde_json::Value::Null),
            )));
        }
        self.apply_desktop_transition(transition, target, None);
    }

    /// Persist the main window's current geometry so relaunch restores it (docs/gui/03 §8).
    fn persist_main_geometry(&self) {
        let mut state = DesktopState::load();
        if let Some(main) = &self.main_window
            && let Some(rect) = main.geometry()
        {
            state.main = Some(rect);
            state.placement_v2.main = main.placement();
        }
        if let Some(panel) = &self.panel
            && panel.is_pinned()
            && let Some(placement) = panel.placement()
        {
            state.mini = Some(crate::desktop::window_state::Point {
                x: placement.work_area.x + placement.origin.x,
                y: placement.work_area.y + placement.origin.y,
            });
            state.placement_v2.mini = Some(placement);
        }
        state.save();
    }

    fn start_polling(&mut self) {
        if self.poll_shutdown.is_some() {
            return;
        }
        let generation = self.next_poll_generation();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        self.poll_shutdown = Some(shutdown_tx);
        let proxy = self.proxy.clone();
        let poll_lane = self.poll_lane.clone();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let builder = thread::Builder::new().name(POLL_THREAD_NAME.to_string());
        match builder.spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    report_error(format_args!("could not start status runtime: {e}"));
                    let _ = done_tx.send(());
                    return;
                }
            };
            rt.block_on(async move {
                let shutdown = async {
                    let _ = shutdown_rx.await;
                };
                status::run_until_shutdown_with_lane(
                    PollConfig::default(),
                    poll_lane,
                    move |update| {
                        let _ = proxy.send_event(UserEvent::Status {
                            generation,
                            update,
                            restart_fallback: false,
                        });
                    },
                    shutdown,
                )
                .await;
            });
            let _ = done_tx.send(());
        }) {
            Ok(thread) => {
                self.poll_thread = Some(thread);
                self.poll_done = Some(done_rx);
            }
            Err(e) => {
                self.poll_shutdown = None;
                self.poll_done = None;
                report_error(format_args!("could not start status polling thread: {e}"));
            }
        }
    }

    fn stop_polling(&mut self) {
        // Invalidate already-queued results before asking the worker to stop.
        self.next_poll_generation();
        if let Some(tx) = self.poll_shutdown.take() {
            let _ = tx.send(());
        }
        let stopped = self
            .poll_done
            .take()
            .is_none_or(|done| done.recv_timeout(Duration::from_secs(2)).is_ok());
        if stopped {
            if let Some(thread) = self.poll_thread.take()
                && thread.join().is_err()
            {
                report_error("status polling thread panicked during shutdown");
            }
        } else {
            report_error("status polling thread did not stop before deadline");
            self.poll_thread.take();
        }
    }

    fn apply_poll_update(&mut self, generation: u64, update: PollUpdate, restart_fallback: bool) {
        if generation != self.poll_generation.load(Ordering::Acquire) {
            return;
        }
        // A fallback result already in the event queue when v8 came online is stale by
        // definition. Typed pushes remain authoritative for that whole session.
        if !matches!(self.last_conn, gateway::ConnState::Online { .. }) {
            self.apply_update(update);
        }
        if restart_fallback
            && matches!(
                self.last_conn,
                gateway::ConnState::Offline { ref reason } if reason == "no_v8"
            )
        {
            self.stop_polling();
            self.start_polling();
        }
    }

    fn next_poll_generation(&self) -> u64 {
        self.poll_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn apply_update(&mut self, update: PollUpdate) {
        update
            .state
            .update_known_resume_available(&mut self.resume_available);
        // The native menu + tooltip derive from only a small field subset (see
        // `menu_model::menu_signature`); skip the allocating menu-model rebuild + the native
        // per-item set_text/set_enabled walk + the tooltip set whenever that subset is
        // unchanged from the last poll — which, during steady playback, is every poll. The
        // mini-player panel consumes the full update (elapsed, artwork, …) and is always
        // refreshed, so it is deliberately outside the guard.
        let signature = menu_model::menu_signature(&update.state);
        if signature != self.last_menu_signature {
            if let Some(menu) = &self.menu {
                menu.apply_state(&update.state);
            }
            if let Some(tray) = &self.tray {
                let _ = tray.set_tooltip(Some(tooltip_for_state(&update.state)));
            }
            self.last_menu_signature = signature;
        }
        if let Some(panel) = &self.panel {
            panel.apply_update(&update);
        }
        self.last_update = update;
    }

    fn request_status_now(&self) {
        if self
            .gateway
            .as_ref()
            .is_some_and(gateway::GatewayHandle::is_online)
        {
            return;
        }
        let restart_fallback = matches!(
            self.last_conn,
            gateway::ConnState::Offline { ref reason } if reason == "no_v8"
        );
        let proxy = self.proxy.clone();
        self.submit_command(move |generation| async move {
            let update = status::poll_once_exclusive().await;
            let _ = proxy.send_event(UserEvent::Status {
                generation,
                update,
                restart_fallback,
            });
        });
    }

    fn show_menu(&mut self) {
        // A click on the tray icon while the menu is open dismisses it, but tao
        // buffers that click during the modal menu loop and delivers it as another
        // ShowMenu right after show_menu() returns — treat it as "close", not
        // "reopen".
        if self
            .menu_dismissed_at
            .is_some_and(|at| at.elapsed() < Duration::from_millis(300))
        {
            return;
        }
        if let Some(tray) = &self.tray {
            tray.show_menu();
            self.menu_dismissed_at = Some(Instant::now());
        }
    }

    fn handle_daemon_primary(&self) {
        // One physical menu item whose meaning tracks the menu model: "Stop Music
        // Daemon" while a daemon owns playback, otherwise "Start Music Daemon".
        let daemon_owner = self
            .last_update
            .state
            .status()
            .is_some_and(|status| status.owner_mode == InstanceMode::Daemon);
        if daemon_owner {
            self.stop_daemon();
        } else {
            self.start_daemon(false);
        }
    }

    fn handle_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::ShowMiniPlayer => {
                let _ = self.proxy.send_event(UserEvent::ShowMiniPlayer(None));
            }
            MenuAction::OpenTui => self.open_tui(),
            MenuAction::Refresh => self.request_status_now(),
            MenuAction::QuitTray => {
                let _ = self.proxy.send_event(UserEvent::Quit);
            }
            MenuAction::StartDaemon => self.start_daemon(false),
            MenuAction::ResumeDaemon => self.start_daemon(true),
            MenuAction::StopDaemon => self.stop_daemon(),
            MenuAction::ToggleStartup => self.toggle_startup(),
            action => {
                if let Some(command) = action.remote_command() {
                    self.send_remote(command);
                }
            }
        }
    }

    fn handle_tray_mini_request(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        anchor: Option<(f64, f64)>,
    ) {
        if anchor.is_some() {
            let now = Instant::now();
            if self
                .last_tray_panel_click
                .is_some_and(|at| now.duration_since(at) < tray_click_coalesce())
            {
                return;
            }
            self.last_tray_panel_click = Some(now);
        }
        let visible = !(self.desktop_app.is_window_visible(WindowKind::Mini)
            && !self.desktop_app.mini_pinned());
        let _ = self.dispatch_desktop_event(
            DesktopEvent::WindowVisibility {
                kind: WindowKind::Mini,
                visible,
            },
            target,
            if visible { anchor } else { None },
        );
    }

    fn ensure_panel(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        anchor: Option<(f64, f64)>,
    ) -> bool {
        self.panel_teardown_at = None;
        if let Some(panel) = &self.panel {
            if let Some(anchor) = anchor.filter(|_| !panel.is_pinned()) {
                panel.position_near(anchor);
            }
            panel.apply_update(&self.last_update);
            return panel.ensure_surface();
        }

        // Unknown / corrupt persisted ids degrade to the default skin.
        let state = DesktopState::load();
        let theme = state
            .mini_theme
            .as_deref()
            .and_then(PanelTheme::from_id)
            .unwrap_or(PanelTheme::Default);
        let proxy = self.proxy.clone();
        match MiniPlayerPanel::create(
            target,
            &self.last_update,
            theme,
            state.mini_pinned,
            move |generation, request| {
                let _ = proxy.send_event(UserEvent::Panel {
                    generation,
                    request,
                });
            },
        ) {
            Ok(panel) => {
                if let Some(anchor) = anchor.filter(|_| !panel.is_pinned()) {
                    panel.position_near(anchor);
                }
                panel.apply_update(&self.last_update);
                self.panel = Some(panel);
                true
            }
            Err(e) => {
                crate::desktop::native_error::show(
                    "YuTuTui! Mini Player",
                    &format!("Could not create the mini player: {e}"),
                );
                report_error(e);
                false
            }
        }
    }

    fn show_panel(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        anchor: Option<(f64, f64)>,
    ) -> bool {
        self.ensure_panel(target, anchor) && self.panel.as_ref().is_some_and(MiniPlayerPanel::show)
    }

    fn handle_panel_request(
        &mut self,
        generation: u64,
        request: PanelRequest,
        target: &EventLoopWindowTarget<UserEvent>,
    ) {
        if self
            .panel
            .as_ref()
            .is_none_or(|panel| panel.page_generation() != generation)
        {
            return;
        }
        let PanelRequest { id, command } = request;
        let correlated = id.map(|id| (generation, id));
        match command {
            PanelCommand::FrontendReady => {
                let mut transition = self
                    .desktop_app
                    .handle_event(DesktopEvent::FrontendReady(WindowKind::Mini));
                let replay = transition.replay.take();
                if let Some(panel) = &self.panel {
                    panel.mark_frontend_ready();
                    panel.apply_update(&self.last_update);
                }
                debug_assert!(matches!(replay, Some((WindowKind::Mini, _))));
                let _ = self.apply_desktop_transition(transition, target, None);
                self.complete_panel_request(correlated, None);
            }
            PanelCommand::Hide => {
                self.complete_panel_request(correlated, None);
                let _ = self.dispatch_desktop_event(
                    DesktopEvent::WindowVisibility {
                        kind: WindowKind::Mini,
                        visible: false,
                    },
                    target,
                    None,
                );
            }
            PanelCommand::Drag => {
                if let Some(panel) = &self.panel {
                    panel.start_drag();
                }
                self.complete_panel_request(correlated, None);
            }
            PanelCommand::SetTheme(theme) => {
                // Same load-mutate-save as the main-window geometry persist, so
                // concurrent writes are never clobbered.
                let mut state = DesktopState::load();
                state.mini_theme = Some(theme.id().to_string());
                let error = state.save_checked().err().map(|error| {
                    DesktopCommandError::new("persist_failed", error.to_string(), true)
                });
                if error.is_none()
                    && let Some(panel) = &self.panel
                {
                    panel.set_theme(theme);
                }
                self.complete_panel_request(correlated, error.as_ref());
            }
            PanelCommand::SetExpanded(expanded) => {
                if let Some(panel) = &self.panel {
                    panel.set_expanded(expanded);
                }
                self.complete_panel_request(correlated, None);
            }
            PanelCommand::SetSharedSheet(open) => {
                if let Some(panel) = &self.panel {
                    panel.set_shared_sheet(open);
                }
                self.complete_panel_request(correlated, None);
            }
            PanelCommand::PersistUi(snapshot) => {
                if let Some(panel) = &self.panel {
                    panel.persist_ui(snapshot);
                }
                self.complete_panel_request(correlated, None);
            }
            PanelCommand::SetPinned(pinned) => {
                let mut state = DesktopState::load();
                state.mini_pinned = pinned;
                if let Some(panel) = &self.panel
                    && let Some(placement) = panel.placement()
                {
                    state.mini = Some(crate::desktop::window_state::Point {
                        x: placement.work_area.x + placement.origin.x,
                        y: placement.work_area.y + placement.origin.y,
                    });
                    state.placement_v2.mini = Some(placement);
                }
                let error = state.save_checked().err().map(|error| {
                    DesktopCommandError::new("persist_failed", error.to_string(), true)
                });
                if let Some(error) = error.as_ref() {
                    self.complete_panel_request(correlated, Some(error));
                    return;
                }
                let _ = self.dispatch_desktop_event(DesktopEvent::MiniPinned(pinned), target, None);
                self.complete_panel_request(correlated, None);
            }
            PanelCommand::ArtworkFailed => {
                if let Some(panel) = &self.panel {
                    panel.artwork_failed();
                }
                self.complete_panel_request(correlated, None);
            }
            PanelCommand::StartDaemon => self.start_daemon_for_panel(false, correlated),
            PanelCommand::ResumeDaemon => self.start_daemon_for_panel(true, correlated),
            PanelCommand::StopDaemon => self.stop_daemon_for_panel(correlated),
            PanelCommand::OpenTui => self.open_tui_for_panel(correlated),
            PanelCommand::Refresh => {
                self.request_status_now();
                self.complete_panel_request(correlated, None);
            }
            command => {
                if let Some(remote) = command.remote_command() {
                    self.send_panel_remote(remote, correlated);
                } else if let Some(action) = command.menu_action() {
                    self.handle_action(action);
                    self.complete_panel_request(correlated, None);
                }
            }
        }
    }

    fn complete_panel_request(
        &self,
        request: Option<(u64, u64)>,
        error: Option<&DesktopCommandError>,
    ) {
        if let (Some((generation, id)), Some(panel)) = (request, &self.panel)
            && panel.page_generation() == generation
        {
            panel.apply_command_result(id, error);
        }
    }

    fn apply_panel_result(&self, generation: u64, id: u64, error: Option<&DesktopCommandError>) {
        if let Some(panel) = &self.panel
            && panel.page_generation() == generation
        {
            panel.apply_command_result(id, error);
        }
    }

    fn handle_panel_focus(
        &mut self,
        window_id: tao::window::WindowId,
        focused: bool,
        target: &EventLoopWindowTarget<UserEvent>,
    ) {
        if let Some(panel) = &self.panel
            && panel.window_id() == window_id
        {
            let event = if focused {
                DesktopWindowEvent::Focused(WindowKind::Mini)
            } else {
                DesktopWindowEvent::Blurred(WindowKind::Mini)
            };
            let _ = self.dispatch_desktop_event(DesktopEvent::WindowEvent(event), target, None);
        }
    }

    fn hide_panel(&mut self) {
        if let Some(panel) = &self.panel {
            panel.hide();
            self.panel_teardown_at = if DesktopState::load().keep_webview_alive {
                None
            } else {
                Some(Instant::now() + MINI_WEBVIEW_GRACE)
            };
        }
    }

    fn hide_main_window(&mut self) {
        if let Some(main) = &self.main_window {
            main.hide();
            self.main_teardown_at = if DesktopState::load().keep_webview_alive {
                None
            } else {
                Some(Instant::now() + MAIN_WEBVIEW_GRACE)
            };
        }
    }

    fn handle_geometry_changed(&mut self, window_id: tao::window::WindowId) {
        if let Some(main) = &self.main_window
            && main.window_id() == window_id
        {
            main.record_geometry();
            self.geometry_dirty_at = Some(Instant::now());
        }
        if let Some(panel) = &self.panel
            && panel.window_id() == window_id
            && panel.is_pinned()
            && !panel.consume_programmatic_geometry_event()
        {
            self.geometry_dirty_at = Some(Instant::now());
        }
    }

    fn process_deadlines(&mut self, target: &EventLoopWindowTarget<UserEvent>) {
        let now = Instant::now();
        let transition = self
            .desktop_app
            .handle_event_at(DesktopEvent::Deadline, now);
        let _ = self.apply_desktop_transition(transition, target, None);
        if self.panel_teardown_at.is_some_and(|due| now >= due) {
            self.panel_teardown_at = None;
            if let Some(panel) = &self.panel {
                panel.teardown_webview();
                let _ = self.dispatch_desktop_event(
                    DesktopEvent::FrontendTornDown(WindowKind::Mini),
                    target,
                    None,
                );
            }
        }
        if self.main_teardown_at.is_some_and(|due| now >= due) {
            self.main_teardown_at = None;
            if let Some(main) = &self.main_window {
                main.teardown_webview();
                let _ = self.dispatch_desktop_event(
                    DesktopEvent::FrontendTornDown(WindowKind::Main),
                    target,
                    None,
                );
            }
        }
        if self
            .geometry_dirty_at
            .is_some_and(|dirty| now.duration_since(dirty) >= GEOMETRY_SAVE_DELAY)
        {
            self.geometry_dirty_at = None;
            self.persist_main_geometry();
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let blur = self.desktop_app.mini_dismiss_deadline();
        let geometry = self
            .geometry_dirty_at
            .map(|dirty| dirty + GEOMETRY_SAVE_DELAY);
        [
            blur,
            geometry,
            self.panel_teardown_at,
            self.main_teardown_at,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn handle_window_close(
        &mut self,
        window_id: tao::window::WindowId,
        target: &EventLoopWindowTarget<UserEvent>,
    ) {
        if let Some(panel) = &self.panel
            && panel.window_id() == window_id
        {
            let _ = self.dispatch_desktop_event(
                DesktopEvent::WindowVisibility {
                    kind: WindowKind::Mini,
                    visible: false,
                },
                target,
                None,
            );
        }
        // Main window close button → hide to tray (docs/gui/03 §4), saving geometry first.
        if let Some(main) = &self.main_window
            && main.window_id() == window_id
        {
            self.persist_main_geometry();
            if DesktopState::load().close_to_tray {
                let _ = self.dispatch_desktop_event(
                    DesktopEvent::WindowVisibility {
                        kind: WindowKind::Main,
                        visible: false,
                    },
                    target,
                    None,
                );
            } else {
                let _ = self.proxy.send_event(UserEvent::Quit);
            }
        }
    }

    fn handle_window_destroyed(
        &mut self,
        window_id: tao::window::WindowId,
        target: &EventLoopWindowTarget<UserEvent>,
    ) {
        if self
            .panel
            .as_ref()
            .is_some_and(|panel| panel.window_id() == window_id)
        {
            self.panel = None;
            self.panel_teardown_at = None;
            let _ = self.dispatch_desktop_event(
                DesktopEvent::WindowEvent(DesktopWindowEvent::Hidden(WindowKind::Mini)),
                target,
                None,
            );
            let _ = self.dispatch_desktop_event(
                DesktopEvent::FrontendTornDown(WindowKind::Mini),
                target,
                None,
            );
        }
        if self
            .main_window
            .as_ref()
            .is_some_and(|main| main.window_id() == window_id)
        {
            self.main_window = None;
            self.main_teardown_at = None;
            let _ = self.dispatch_desktop_event(
                DesktopEvent::WindowEvent(DesktopWindowEvent::Hidden(WindowKind::Main)),
                target,
                None,
            );
            let _ = self.dispatch_desktop_event(
                DesktopEvent::FrontendTornDown(WindowKind::Main),
                target,
                None,
            );
        }
    }

    fn start_daemon_for_panel(&self, resume: bool, request: Option<(u64, u64)>) {
        let proxy = self.proxy.clone();
        self.submit_panel_lifecycle_command(request, move |_| async move {
            let result = control::start_daemon(resume).await;
            if let Some((page_generation, id)) = request {
                let error = result.as_ref().err().map(panel::command_error_from_control);
                let _ = proxy.send_event(UserEvent::PanelResult {
                    generation: page_generation,
                    id,
                    error,
                });
            }
            let _ = proxy.send_event(UserEvent::Refresh);
        });
    }

    fn stop_daemon_for_panel(&self, request: Option<(u64, u64)>) {
        let proxy = self.proxy.clone();
        self.submit_panel_lifecycle_command(request, move |_| async move {
            let result = control::stop_daemon().await;
            if let Some((page_generation, id)) = request {
                let error = result.as_ref().err().map(panel::command_error_from_control);
                let _ = proxy.send_event(UserEvent::PanelResult {
                    generation: page_generation,
                    id,
                    error,
                });
            }
            let _ = proxy.send_event(UserEvent::Refresh);
        });
    }

    fn start_daemon(&self, resume: bool) {
        let proxy = self.proxy.clone();
        self.submit_lifecycle_command(move |_| async move {
            if let Err(e) = control::start_daemon(resume).await {
                report_command_error(e);
            }
            let _ = proxy.send_event(UserEvent::Refresh);
        });
    }

    fn toggle_startup(&mut self) {
        if self.startup_toggle_pending {
            return;
        }
        self.startup_toggle_pending = true;
        if let Some(menu) = &self.menu {
            menu.set_startup_pending(true);
        }
        let proxy = self.proxy.clone();
        let submit = self
            .command_executor
            .as_ref()
            .ok_or(SubmitError::Closed)
            .and_then(|executor| {
                executor.submit_lifecycle(move || async move {
                    let error = tokio::task::spawn_blocking(|| {
                        toggle_startup_entry().err().map(|error| error.to_string())
                    })
                    .await
                    .unwrap_or_else(|error| Some(format!("startup worker failed: {error}")));
                    let _ = proxy.send_event(UserEvent::StartupChanged { error });
                })
            });
        if let Err(error) = submit {
            self.finish_startup_toggle(Some(&error.to_string()));
        }
    }

    fn finish_startup_toggle(&mut self, error: Option<&str>) {
        self.startup_toggle_pending = false;
        if let Some(error) = error {
            report_error(format_args!("could not update login startup: {error}"));
            crate::desktop::native_error::show(
                "YuTuTui! Desktop",
                &format!("Could not update login startup: {error}"),
            );
        }
        if let Some(menu) = &self.menu {
            menu.apply_startup_status();
        }
    }

    fn stop_daemon(&self) {
        let proxy = self.proxy.clone();
        self.submit_lifecycle_command(move |_| async move {
            if let Err(e) = control::stop_daemon().await {
                report_command_error(e);
            }
            let _ = proxy.send_event(UserEvent::Refresh);
        });
    }
}

// Kept as an include so the platform support code and its tests retain access
// to the backend's private event types without widening their visibility.
include!("windows_desktop_app.rs");
include!("windows_support.rs");
