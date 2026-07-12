//! macOS menu bar backend for `yututray`.

use std::collections::HashMap;
use std::error::Error;
use std::panic;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS, EventLoopWindowTargetExtMacOS};
use tray_icon::menu::MenuEvent;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::desktop::app::{
    DesktopApp, DesktopEffect, DesktopEvent, DesktopTransition, DesktopWindowEvent, FrontendReplay,
    WindowKind,
};
use crate::desktop::control;
use crate::desktop::executor::{DesktopCommandExecutor, SubmitError};
use crate::desktop::launch;
use crate::desktop::menu_model::{self, MenuAction};
use crate::desktop::panel::{self, DesktopCommandError, PanelCommand, PanelRequest, PanelTheme};
use crate::desktop::platform::main_window::MainWindow;
use crate::desktop::platform::panel_window::MiniPlayerPanel;
use crate::desktop::single_instance::{self, Acquire, ActivationIntent};
use crate::desktop::status::{self, PollConfig, PollUpdate};
use crate::desktop::window_state::DesktopState;
use crate::desktop::{bridge, gateway};
use crate::remote::proto::{InstanceMode, PushEvent, RemoteCommand, Topic};

#[path = "macos_geometry.rs"]
mod geometry;
#[path = "macos_menu.rs"]
mod native_menu;

pub(crate) use geometry::work_area_for_point;
use native_menu::{MacMenu, toggle_startup_entry, tooltip_for_state, user_event_from_menu_id};

const POLL_THREAD_NAME: &str = "yututray-status";
const COMMAND_THREAD_NAME: &str = "yututray-command";
const GEOMETRY_SAVE_DELAY: Duration = Duration::from_millis(500);
const MINI_WEBVIEW_GRACE: Duration = Duration::from_secs(15);
const MAIN_WEBVIEW_GRACE: Duration = Duration::from_secs(60);

// Status carries the poll snapshot inline; events are dispatched one at a time
// (never queued in bulk), so boxing would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum UserEvent {
    Status {
        generation: u64,
        update: PollUpdate,
        restart_fallback: bool,
    },
    ShowMiniPlayer,
    Panel {
        generation: u64,
        request: PanelRequest,
    },
    PanelResult {
        generation: u64,
        id: u64,
        error: Option<DesktopCommandError>,
    },
    Menu(MenuAction),
    /// The first daemon menu slot; whether it means Start or Stop depends on the
    /// state at click time, so the decision lives in the app, not in the menu id.
    DaemonPrimary,
    Refresh,
    StartupChanged {
        error: Option<String>,
    },
    /// Live connection state from the persistent v8 gateway thread (docs/gui/03 §3.2).
    Gateway(gateway::GatewayEvent),
    /// A request concerning the main window: an IPC line from its webview, or an activate.
    Main(MainRequest),
    Activation(ActivationIntent),
    Quit,
}

#[derive(Debug)]
enum MainRequest {
    /// One IPC envelope line posted by the main window's webview.
    Ipc { generation: u64, body: String },
}

pub fn run(
    initial_intent: ActivationIntent,
    secondary_intent: ActivationIntent,
) -> Result<(), Box<dyn Error>> {
    let log_guard = init_file_logging();
    install_tray_panic_hook();

    // Single GUI instance (docs/gui/03 §6): a second launch activates the first and exits.
    // Held until the process exits (tao's run() diverges, so this never drops early).
    let _instance = match single_instance::acquire()? {
        Acquire::Primary(guard) => Some(guard),
        Acquire::AlreadyRunning => {
            single_instance::signal_activation(secondary_intent)?;
            return Ok(());
        }
    };

    // Bind while the primary lock is already held but before startup repair or tao setup.
    // A bounded FIFO acknowledges and preserves activation intents until the proxy is installed.
    let deferred_activations =
        single_instance::DeferredActivations::<EventLoopProxy<UserEvent>>::new();
    single_instance::spawn_activation_listener({
        let deferred_activations = deferred_activations.clone();
        move |intent| {
            deferred_activations.deliver_or_defer(intent, |proxy, intent| {
                proxy.send_event(UserEvent::Activation(intent)).is_ok()
            })
        }
    })?;

    // Only the primary may mutate the LaunchAgent; concurrent secondaries must not race the
    // same plist/temp file during login storms.
    crate::desktop::startup::self_heal();

    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_dock_visibility(false);

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

    let mut app = MacTrayApp::new(proxy.clone(), log_guard, initial_intent)?;

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
            Event::UserEvent(UserEvent::ShowMiniPlayer) => {
                let _ = app.dispatch_desktop_event(
                    DesktopEvent::WindowVisibility {
                        kind: WindowKind::Mini,
                        visible: true,
                    },
                    target,
                );
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

struct MacTrayApp {
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    menu: Option<MacMenu>,
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
    geometry_dirty_at: Option<Instant>,
    command_executor: Option<DesktopCommandExecutor>,
    panel_teardown_at: Option<Instant>,
    main_teardown_at: Option<Instant>,
    panel_gateway_requests: HashMap<u64, (u64, u64)>,
    startup_toggle_pending: bool,
}

impl MacTrayApp {
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
            geometry_dirty_at: None,
            command_executor: Some(DesktopCommandExecutor::spawn(COMMAND_THREAD_NAME)?),
            panel_teardown_at: None,
            main_teardown_at: None,
            panel_gateway_requests: HashMap::new(),
            startup_toggle_pending: false,
        })
    }

    fn init(&mut self, target: &EventLoopWindowTarget<UserEvent>) -> Result<(), Box<dyn Error>> {
        let menu = MacMenu::new(&self.last_update.state)?;
        let icon = template_icon()?;
        let tray = TrayIconBuilder::new()
            .with_id("io.github.ochi.yututui.tray")
            .with_menu(Box::new(menu.root.clone()))
            .with_tooltip(tooltip_for_state(&self.last_update.state))
            .with_icon(icon)
            .with_icon_as_template(true)
            .with_menu_on_left_click(true)
            .with_menu_on_right_click(true)
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

    fn dispatch_desktop_event(
        &mut self,
        event: DesktopEvent,
        target: &EventLoopWindowTarget<UserEvent>,
    ) -> Option<FrontendReplay> {
        let transition = self.desktop_app.handle_event(event);
        self.apply_desktop_transition(transition, target)
    }

    fn apply_desktop_transition(
        &mut self,
        transition: DesktopTransition,
        target: &EventLoopWindowTarget<UserEvent>,
    ) -> Option<FrontendReplay> {
        let DesktopTransition { effects, replay } = transition;
        let mut failed_window = None;
        for effect in effects {
            match effect {
                DesktopEffect::EnsureTray => debug_assert!(self.tray.is_some()),
                DesktopEffect::EnsureMiniSurface => {
                    if !self.ensure_panel(target) {
                        failed_window = Some(WindowKind::Mini);
                    }
                }
                DesktopEffect::ShowMini => {
                    if !self.show_panel(target) {
                        failed_window = Some(WindowKind::Mini);
                    }
                }
                DesktopEffect::HideMini => self.hide_panel(),
                DesktopEffect::EnsureMainSurface => {
                    if !self.ensure_main_window(target) {
                        failed_window = Some(WindowKind::Main);
                    }
                }
                DesktopEffect::ShowMain => {
                    if !self.show_main_window(target) {
                        failed_window = Some(WindowKind::Main);
                    }
                }
                DesktopEffect::HideMain => self.hide_main_window(),
                DesktopEffect::UseRegularActivation => set_main_activation(target, true),
                DesktopEffect::UseAccessoryActivation => set_main_activation(target, false),
                DesktopEffect::ApplyWindowPolicy {
                    kind: WindowKind::Mini,
                    policy,
                } => {
                    if let Some(panel) = &self.panel {
                        panel.set_pinned(policy.always_on_top);
                    }
                }
                DesktopEffect::ApplyWindowPolicy {
                    kind: WindowKind::Main,
                    ..
                } => {}
            }
        }
        if let Some(kind) = failed_window {
            let correction = self
                .desktop_app
                .handle_event(DesktopEvent::WindowEvent(DesktopWindowEvent::Hidden(kind)));
            let _ = self.apply_desktop_transition(correction, target);
        }
        replay.map(|(_, replay)| replay)
    }

    fn ensure_main_window(&mut self, target: &EventLoopWindowTarget<UserEvent>) -> bool {
        self.main_teardown_at = None;
        if let Some(main) = &self.main_window {
            return main.ensure_surface();
        }
        let boot = boot_json(&self.last_conn);
        let proxy = self.proxy.clone();
        match MainWindow::create(target, boot, None, move |generation, body| {
            let _ = proxy.send_event(UserEvent::Main(MainRequest::Ipc { generation, body }));
        }) {
            Ok(main) => {
                self.main_window = Some(main);
                true
            }
            Err(e) => {
                crate::desktop::native_error::show(
                    "YuTuTui! Desktop",
                    &format!("Could not create the main window: {e}"),
                );
                report_error(e);
                false
            }
        }
    }

    fn show_main_window(&mut self, target: &EventLoopWindowTarget<UserEvent>) -> bool {
        self.ensure_main_window(target) && self.main_window.as_ref().is_some_and(MainWindow::show)
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
                    // The v7 poll is an exclusive compatibility fallback, not a
                    // general offline loop.  Cancel it as soon as the connection
                    // leaves the explicit no-v8 state.
                    _ => self.stop_polling(),
                }
                if let Some(error) = disconnected {
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
            gateway::GatewayEvent::Frame {
                envelope: env,
                source_generation,
            } => {
                if source_generation.is_none() && gateway::is_native_request_id(env.id) {
                    self.handle_native_command_result(env);
                    return;
                }
                if let Some(main) = &self.main_window
                    && source_generation
                        .is_none_or(|generation| generation == main.page_generation())
                {
                    main.eval(&bridge::receive_script(&env));
                }
            }
            gateway::GatewayEvent::Push {
                sequence,
                topic,
                event,
            } => {
                let topic_name = topic.wire_str().to_string();
                let payload = serde_json::to_value(&event).unwrap_or(serde_json::Value::Null);
                if self.desktop_app.apply_push(sequence, topic, event) {
                    if let Some(main) = &self.main_window {
                        main.eval(&bridge::receive_script(&bridge::InEnvelope::event(
                            &topic_name,
                            payload,
                        )));
                    }
                    self.apply_snapshot_locale();
                    if let Some(status) = self.desktop_app.status_projection() {
                        self.apply_update(PollUpdate::connected(status));
                    }
                }
            }
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
                        let error = match &self.gateway {
                            Some(gateway) => {
                                gateway.send_from_generation(env, Some(generation)).err()
                            }
                            None => Some(gateway::GatewaySendError::Offline(env)),
                        };
                        if let Some(error) = error {
                            let code = error.code();
                            let rejected = error.into_envelope();
                            if rejected.kind == bridge::OutKind::Req
                                && let Some(id) = rejected.id
                                && let Some(main) = &self.main_window
                            {
                                main.eval(&bridge::receive_script(&bridge::InEnvelope::err(
                                    id,
                                    serde_json::json!({ "reason": code }),
                                )));
                            }
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
        let _ = self.dispatch_desktop_event(DesktopEvent::Activation(intent), target);
    }

    fn handle_win_op(&mut self, op: bridge::WinOp, target: &EventLoopWindowTarget<UserEvent>) {
        match op {
            bridge::WinOp::FrontendReady => self.replay_main_frontend(target),
            bridge::WinOp::Hide => {
                if self.main_window.is_some() {
                    self.persist_main_geometry();
                    let _ = self.dispatch_desktop_event(
                        DesktopEvent::WindowVisibility {
                            kind: WindowKind::Main,
                            visible: false,
                        },
                        target,
                    );
                }
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
        self.apply_desktop_transition(transition, target);
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
                let _ = self.proxy.send_event(UserEvent::ShowMiniPlayer);
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

    fn ensure_panel(&mut self, target: &EventLoopWindowTarget<UserEvent>) -> bool {
        self.panel_teardown_at = None;
        if let Some(panel) = &self.panel {
            if !panel.is_pinned() {
                panel.position_near_cursor();
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
                if !panel.is_pinned() {
                    panel.position_near_cursor();
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

    fn show_panel(&mut self, target: &EventLoopWindowTarget<UserEvent>) -> bool {
        self.ensure_panel(target) && self.panel.as_ref().is_some_and(MiniPlayerPanel::show)
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
                let _ = self.apply_desktop_transition(transition, target);
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
                );
            }
            PanelCommand::Drag => {
                if let Some(panel) = &self.panel {
                    panel.start_drag();
                }
                self.complete_panel_request(correlated, None);
            }
            PanelCommand::SetTheme(theme) => {
                // Same load-mutate-save as persist_main_geometry, so concurrent
                // geometry writes are never clobbered.
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
                let _ = self.dispatch_desktop_event(DesktopEvent::MiniPinned(pinned), target);
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
            let _ = self.dispatch_desktop_event(DesktopEvent::WindowEvent(event), target);
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
        let _ = self.apply_desktop_transition(transition, target);
        if self.panel_teardown_at.is_some_and(|due| now >= due) {
            self.panel_teardown_at = None;
            if let Some(panel) = &self.panel {
                panel.teardown_webview();
                let _ = self.dispatch_desktop_event(
                    DesktopEvent::FrontendTornDown(WindowKind::Mini),
                    target,
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
            );
            let _ = self
                .dispatch_desktop_event(DesktopEvent::FrontendTornDown(WindowKind::Mini), target);
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
            );
            let _ = self
                .dispatch_desktop_event(DesktopEvent::FrontendTornDown(WindowKind::Main), target);
        }
    }

    fn send_remote(&self, command: RemoteCommand) {
        if let Some(gateway) = &self.gateway
            && gateway.is_online()
        {
            if let Err(error) = gateway.send_remote(command) {
                report_command_error(format_args!(
                    "could not queue desktop command: {}",
                    error.code()
                ));
            }
            return;
        }
        if !matches!(
            self.last_conn,
            gateway::ConnState::Offline { ref reason } if reason == "no_v8"
        ) {
            report_command_error("player is offline");
            return;
        }
        let proxy = self.proxy.clone();
        self.submit_command(move |generation| async move {
            if let Err(e) = control::send_remote(command).await {
                report_command_error(e);
            }
            let update = status::poll_once_exclusive().await;
            let _ = proxy.send_event(UserEvent::Status {
                generation,
                update,
                restart_fallback: true,
            });
        });
    }

    fn send_panel_remote(&mut self, command: RemoteCommand, request: Option<(u64, u64)>) {
        if let Some(gateway) = &self.gateway
            && gateway.is_online()
        {
            match gateway.send_remote(command) {
                Ok(gateway_id) => {
                    if let Some(request) = request {
                        self.panel_gateway_requests.insert(gateway_id, request);
                    }
                }
                Err(error) => {
                    let error = DesktopCommandError::new(
                        error.code(),
                        format!("Could not queue command: {}", error.code()),
                        true,
                    );
                    self.complete_panel_request(request, Some(&error));
                }
            }
            return;
        }
        if !matches!(
            self.last_conn,
            gateway::ConnState::Offline { ref reason } if reason == "no_v8"
        ) {
            let error = DesktopCommandError::new("offline", "Player is offline", true);
            self.complete_panel_request(request, Some(&error));
            return;
        }
        let proxy = self.proxy.clone();
        self.submit_panel_command(request, move |generation| async move {
            let result = control::send_remote(command).await;
            if let Some((page_generation, id)) = request {
                let error = result.as_ref().err().map(panel::command_error_from_control);
                let _ = proxy.send_event(UserEvent::PanelResult {
                    generation: page_generation,
                    id,
                    error,
                });
            }
            let update = status::poll_once_exclusive().await;
            let _ = proxy.send_event(UserEvent::Status {
                generation,
                update,
                restart_fallback: true,
            });
        });
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

include!("macos_support.rs");
