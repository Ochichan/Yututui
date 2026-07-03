//! Windows notification-area backend for `ytt-desktop`.

use std::collections::HashMap;
use std::error::Error;
use std::panic;
use std::thread;
use std::time::{Duration, Instant};

use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::desktop::control;
use crate::desktop::launch;
use crate::desktop::menu_model::{self, MenuAction, MenuEntry, MenuItem as ModelItem, TrayState};
use crate::desktop::panel::{PanelCommand, PanelTheme};
use crate::desktop::platform::panel_window::MiniPlayerPanel;
use crate::desktop::startup::{self, StartupStatus};
use crate::desktop::status::{self, PollConfig, PollUpdate};
use crate::desktop::window_state::DesktopState;
use crate::remote::proto::{InstanceMode, RemoteCommand, StatusSnapshot};

const APP_ID: &str = "io.github.ochi.ytm-tui.tray";
const POLL_THREAD_NAME: &str = "ytt-desktop-status";
const COMMAND_THREAD_NAME: &str = "ytt-desktop-command";
const ICO_BYTES: &[u8] = include_bytes!("../../../assets/icons/ytm-tui.ico");

// Status carries the poll snapshot inline; events are dispatched one at a time
// (never queued in bulk), so boxing would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum UserEvent {
    Status(PollUpdate),
    ShowMenu,
    /// Show the mini player, optionally anchored near a tray click (physical px).
    ShowMiniPlayer(Option<(f64, f64)>),
    Panel(PanelCommand),
    Menu(MenuAction),
    /// The first daemon menu slot; whether it means Start or Stop depends on the
    /// state at click time, so the decision lives in the app, not in the menu id.
    DaemonPrimary,
    Refresh,
    StartupChanged,
    Quit,
}

pub fn run(open_main: bool) -> Result<(), Box<dyn Error>> {
    // The Windows main-window + gateway wiring mirrors the macOS path (docs/gui/03 §3) but
    // needs a Windows box to verify; it lands with the Windows M0 sign-off. Tray/mini are
    // unchanged for now.
    let _ = open_main;
    let log_guard = init_file_logging();
    install_tray_panic_hook();
    set_app_user_model_id();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

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

    let mut app = WindowsTrayApp::new(proxy.clone(), log_guard);

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Err(e) = app.init() {
                    report_error(e);
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Status(update)) => {
                app.apply_update(update);
            }
            Event::UserEvent(UserEvent::ShowMenu) => {
                app.show_menu();
            }
            Event::UserEvent(UserEvent::ShowMiniPlayer(anchor)) => {
                app.show_panel(target, anchor);
            }
            Event::UserEvent(UserEvent::Panel(command)) => {
                app.handle_panel_command(command);
            }
            Event::UserEvent(UserEvent::Refresh) => {
                app.request_status_now();
            }
            Event::UserEvent(UserEvent::StartupChanged) => {
                app.apply_startup_status();
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
                app.handle_window_close(window_id);
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Focused(false),
                ..
            } => {
                app.handle_panel_blur(window_id);
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Destroyed,
                ..
            } => {
                app.handle_window_destroyed(window_id);
            }
            Event::LoopDestroyed => {
                app.shutdown();
            }
            _ => {}
        }
    });
}

struct WindowsTrayApp {
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    menu: Option<WindowsMenu>,
    panel: Option<MiniPlayerPanel>,
    last_update: PollUpdate,
    poll_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    // Held until LoopDestroyed: tao's run() never returns (it exits the process), so
    // dropping this in shutdown() is the only chance the non-blocking appender gets
    // to flush the final log lines.
    log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    menu_dismissed_at: Option<Instant>,
    // When a focus loss auto-hid the minimal panel. A tray click arrives as
    // mousedown (blur → hide) then mouseup (ShowMiniPlayer); swallowing the show
    // shortly after a blur-hide makes that click read as toggle-close instead of
    // an instant reopen (mirrors menu_dismissed_at).
    panel_blur_hidden_at: Option<Instant>,
}

impl WindowsTrayApp {
    fn new(
        proxy: EventLoopProxy<UserEvent>,
        log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    ) -> Self {
        Self {
            proxy,
            tray: None,
            menu: None,
            panel: None,
            last_update: PollUpdate::disconnected(control::ControlError::NotRunning),
            poll_shutdown: None,
            log_guard,
            menu_dismissed_at: None,
            panel_blur_hidden_at: None,
        }
    }

    fn init(&mut self) -> Result<(), Box<dyn Error>> {
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
        self.start_polling();
        self.request_status_now();
        Ok(())
    }

    fn start_polling(&mut self) {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        self.poll_shutdown = Some(shutdown_tx);
        let proxy = self.proxy.clone();
        let builder = thread::Builder::new().name(POLL_THREAD_NAME.to_string());
        if let Err(e) = builder.spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    report_error(format_args!("could not start status runtime: {e}"));
                    return;
                }
            };
            rt.block_on(async move {
                let shutdown = async {
                    let _ = shutdown_rx.await;
                };
                status::run_until_shutdown(
                    PollConfig::default(),
                    move |update| {
                        let _ = proxy.send_event(UserEvent::Status(update));
                    },
                    shutdown,
                )
                .await;
            });
        }) {
            report_error(format_args!("could not start status polling thread: {e}"));
        }
    }

    fn apply_update(&mut self, update: PollUpdate) {
        if let Some(menu) = &self.menu {
            menu.apply_state(&update.state);
        }
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(tooltip_for_state(&update.state)));
        }
        if let Some(panel) = &self.panel {
            panel.apply_update(&update);
        }
        self.last_update = update;
    }

    fn request_status_now(&self) {
        let proxy = self.proxy.clone();
        spawn_async_command(move || async move {
            let update = status::poll_once().await;
            let _ = proxy.send_event(UserEvent::Status(update));
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

    fn handle_action(&self, action: MenuAction) {
        match action {
            MenuAction::ShowMiniPlayer => {
                let _ = self.proxy.send_event(UserEvent::ShowMiniPlayer(None));
            }
            MenuAction::OpenTui => match launch::open_tui() {
                Ok(_) => self.request_status_now(),
                Err(e) => report_error(e),
            },
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

    fn show_panel(
        &mut self,
        target: &EventLoopWindowTarget<UserEvent>,
        anchor: Option<(f64, f64)>,
    ) {
        // The tray click that requested this show may itself have blur-hidden the
        // minimal panel on mousedown; swallowing it turns the click into a toggle.
        if self
            .panel_blur_hidden_at
            .take()
            .is_some_and(|at| at.elapsed() < Duration::from_millis(300))
        {
            return;
        }
        if let Some(panel) = &self.panel {
            if let Some(anchor) = anchor {
                panel.position_near(anchor);
            }
            panel.show();
            panel.apply_update(&self.last_update);
            return;
        }

        // Unknown / corrupt persisted ids degrade to the default skin.
        let theme = DesktopState::load()
            .mini_theme
            .as_deref()
            .and_then(PanelTheme::from_id)
            .unwrap_or(PanelTheme::Default);
        let proxy = self.proxy.clone();
        match MiniPlayerPanel::create(target, &self.last_update, theme, move |command| {
            let _ = proxy.send_event(UserEvent::Panel(command));
        }) {
            Ok(panel) => {
                if let Some(anchor) = anchor {
                    panel.position_near(anchor);
                }
                panel.show();
                panel.apply_update(&self.last_update);
                self.panel = Some(panel);
            }
            Err(e) => report_error(e),
        }
    }

    fn handle_panel_command(&self, command: PanelCommand) {
        match command {
            PanelCommand::Hide => {
                if let Some(panel) = &self.panel {
                    panel.hide();
                }
            }
            PanelCommand::Drag => {
                if let Some(panel) = &self.panel {
                    panel.start_drag();
                }
            }
            PanelCommand::SetTheme(theme) => {
                if let Some(panel) = &self.panel {
                    panel.set_theme(theme);
                }
                // Same load-mutate-save as the main-window geometry persist, so
                // concurrent writes are never clobbered.
                let mut state = DesktopState::load();
                state.mini_theme = Some(theme.id().to_string());
                state.save();
            }
            PanelCommand::SetExpanded(expanded) => {
                if let Some(panel) = &self.panel {
                    panel.set_expanded(expanded);
                }
            }
            command => {
                if let Some(remote) = command.remote_command() {
                    self.send_panel_remote(
                        remote,
                        matches!(command, PanelCommand::SetStreaming(true)),
                    );
                } else if let Some(action) = command.menu_action() {
                    self.handle_action(action);
                }
            }
        }
    }

    /// The minimal skin dismisses on click-away like a real tray popup.
    fn handle_panel_blur(&mut self, window_id: tao::window::WindowId) {
        if let Some(panel) = &self.panel
            && panel.window_id() == window_id
            && panel.wants_blur_hide()
        {
            panel.hide();
            self.panel_blur_hidden_at = Some(Instant::now());
        }
    }

    fn handle_window_close(&self, window_id: tao::window::WindowId) {
        if let Some(panel) = &self.panel
            && panel.window_id() == window_id
        {
            panel.hide();
        }
    }

    fn handle_window_destroyed(&mut self, window_id: tao::window::WindowId) {
        if self
            .panel
            .as_ref()
            .is_some_and(|panel| panel.window_id() == window_id)
        {
            self.panel = None;
        }
    }

    fn send_remote(&self, command: RemoteCommand) {
        let proxy = self.proxy.clone();
        spawn_async_command(move || async move {
            if let Err(e) = control::send_remote(command).await {
                report_error(e);
            }
            let update = status::poll_once().await;
            let _ = proxy.send_event(UserEvent::Status(update));
        });
    }

    fn send_panel_remote(&self, command: RemoteCommand, resume_if_idle: bool) {
        let proxy = self.proxy.clone();
        spawn_async_command(move || async move {
            if let Err(e) = control::send_remote(command).await {
                report_error(e);
            }
            if resume_if_idle
                && let Ok(status) = control::status().await
                && status_is_idle(&status)
            {
                match control::send_remote(RemoteCommand::ResumeSession).await {
                    Ok(_) => {}
                    Err(control::ControlError::Rejected(reason)) if reason == "session_empty" => {}
                    Err(e) => report_error(e),
                }
            }
            let update = status::poll_once().await;
            let _ = proxy.send_event(UserEvent::Status(update));
        });
    }

    fn start_daemon(&self, resume: bool) {
        let proxy = self.proxy.clone();
        spawn_async_command(move || async move {
            if let Err(e) = control::start_daemon(resume).await {
                report_error(e);
            }
            let update = status::poll_once().await;
            let _ = proxy.send_event(UserEvent::Status(update));
        });
    }

    fn toggle_startup(&self) {
        let proxy = self.proxy.clone();
        spawn_async_command(move || async move {
            if let Err(e) = toggle_startup_entry() {
                report_error(e);
            }
            let _ = proxy.send_event(UserEvent::StartupChanged);
        });
    }

    fn apply_startup_status(&self) {
        if let Some(menu) = &self.menu {
            menu.apply_startup_status();
        }
    }

    fn stop_daemon(&self) {
        let proxy = self.proxy.clone();
        spawn_async_command(move || async move {
            if let Err(e) = control::stop_daemon().await {
                report_error(e);
            }
            let update = status::poll_once().await;
            let _ = proxy.send_event(UserEvent::Status(update));
        });
    }

    fn shutdown(&mut self) {
        if let Some(tx) = self.poll_shutdown.take() {
            let _ = tx.send(());
        }
        // Flush the log before tao exits the process. (muda/tray-icon handlers live
        // in set-once cells — unsetting them here was always a no-op, so we don't.)
        drop(self.log_guard.take());
    }
}

struct WindowsMenu {
    // The tray root must be a `Menu`. A `Submenu` root registers muda's window
    // subclass with a `MenuChild` payload, and muda's WM_NCACTIVATE/WM_NCPAINT arm
    // blindly casts that payload back to `Menu` — type confusion that access-violates
    // the process the moment the menu opens (SetForegroundWindow) or is dismissed.
    // See tray-icon#115 / tauri#11363.
    root: Menu,
    track: MenuItem,
    state: MenuItem,
    startup: CheckMenuItem,
    // Single handle for the first daemon slot. Its action flips between StartDaemon
    // and StopDaemon with the state, so an action-keyed handle map would strand it:
    // whichever variant was absent at build time could never appear afterwards.
    daemon_primary: MenuItem,
    actions: HashMap<MenuAction, MenuItem>,
}

impl WindowsMenu {
    fn new(state: &TrayState) -> Result<Self, Box<dyn Error>> {
        let model = menu_model::build_menu(state);
        let root = Menu::new();
        let mut action_items = HashMap::new();
        let mut track = None;
        let mut state_item = None;
        let mut startup_item = None;
        let mut daemon_primary = None;
        let mut disabled_index = 0usize;

        for entry in model.entries {
            match entry {
                MenuEntry::Separator => root.append(&PredefinedMenuItem::separator())?,
                MenuEntry::Item(item) => {
                    if item.action == Some(MenuAction::ToggleStartup) {
                        let menu_item = make_startup_menu_item(&item);
                        startup_item = Some(menu_item.clone());
                        root.append(&menu_item)?;
                        continue;
                    }
                    if is_daemon_primary(item.action) {
                        let menu_item = MenuItem::with_id(
                            MenuId::new(DAEMON_PRIMARY_ID),
                            &item.label,
                            item.enabled,
                            None,
                        );
                        daemon_primary = Some(menu_item.clone());
                        root.append(&menu_item)?;
                        continue;
                    }
                    let menu_item = make_menu_item(&item, disabled_index);
                    if let Some(action) = item.action {
                        action_items.insert(action, menu_item.clone());
                    } else {
                        match disabled_index {
                            1 => track = Some(menu_item.clone()),
                            2 => state_item = Some(menu_item.clone()),
                            _ => {}
                        }
                        disabled_index += 1;
                    }
                    root.append(&menu_item)?;
                }
            }
        }

        Ok(Self {
            root,
            track: track.ok_or("missing track menu item")?,
            state: state_item.ok_or("missing state menu item")?,
            startup: startup_item.ok_or("missing startup menu item")?,
            daemon_primary: daemon_primary.ok_or("missing daemon menu item")?,
            actions: action_items,
        })
    }

    fn apply_state(&self, state: &TrayState) {
        let model = menu_model::build_menu(state);
        let mut disabled_index = 0usize;
        for entry in model.entries {
            let MenuEntry::Item(item) = entry else {
                continue;
            };
            if let Some(action) = item.action {
                if action == MenuAction::ToggleStartup {
                    // Label only. Checked/enabled refresh on StartupChanged events —
                    // reading the registry on every status poll was wasted IO.
                    self.startup.set_text(item.label);
                    continue;
                }
                if is_daemon_primary(Some(action)) {
                    self.daemon_primary.set_text(item.label);
                    self.daemon_primary.set_enabled(item.enabled);
                    continue;
                }
                if let Some(handle) = self.actions.get(&action) {
                    handle.set_text(item.label);
                    handle.set_enabled(item.enabled);
                }
            } else {
                match disabled_index {
                    1 => {
                        self.track.set_text(item.label);
                        self.track.set_enabled(item.enabled);
                    }
                    2 => {
                        self.state.set_text(item.label);
                        self.state.set_enabled(item.enabled);
                    }
                    _ => {}
                }
                disabled_index += 1;
            }
        }
    }

    fn apply_startup_status(&self) {
        let (checked, enabled) = startup_menu_state();
        self.startup.set_checked(checked);
        self.startup.set_enabled(enabled);
    }
}

fn make_menu_item(item: &ModelItem, disabled_index: usize) -> MenuItem {
    if let Some(action) = item.action {
        MenuItem::with_id(action_menu_id(action), &item.label, item.enabled, None)
    } else {
        MenuItem::with_id(
            MenuId::new(format!("ytt-desktop:disabled:{disabled_index}")),
            &item.label,
            item.enabled,
            None,
        )
    }
}

fn make_startup_menu_item(item: &ModelItem) -> CheckMenuItem {
    let (checked, enabled) = startup_menu_state();
    CheckMenuItem::with_id(
        action_menu_id(MenuAction::ToggleStartup),
        &item.label,
        item.enabled && enabled,
        checked,
        None,
    )
}

const DAEMON_PRIMARY_ID: &str = "ytt-desktop:daemon_primary";

fn is_daemon_primary(action: Option<MenuAction>) -> bool {
    matches!(
        action,
        Some(MenuAction::StartDaemon) | Some(MenuAction::StopDaemon)
    )
}

fn user_event_from_menu_id(id: &MenuId) -> Option<UserEvent> {
    if id.as_ref() == DAEMON_PRIMARY_ID {
        return Some(UserEvent::DaemonPrimary);
    }
    let action = action_from_menu_id(id)?;
    Some(match action {
        MenuAction::ShowMiniPlayer => UserEvent::ShowMiniPlayer(None),
        MenuAction::Refresh => UserEvent::Refresh,
        MenuAction::QuitTray => UserEvent::Quit,
        other => UserEvent::Menu(other),
    })
}

fn action_menu_id(action: MenuAction) -> MenuId {
    MenuId::new(format!("ytt-desktop:{}", action_slug(action)))
}

fn action_from_menu_id(id: &MenuId) -> Option<MenuAction> {
    let slug = id.as_ref().strip_prefix("ytt-desktop:")?;
    match slug {
        "play_pause" => Some(MenuAction::PlayPause),
        "next" => Some(MenuAction::Next),
        "previous" => Some(MenuAction::Previous),
        "seek_back" => Some(MenuAction::SeekBack),
        "seek_forward" => Some(MenuAction::SeekForward),
        "volume_up" => Some(MenuAction::VolumeUp),
        "volume_down" => Some(MenuAction::VolumeDown),
        "toggle_streaming" => Some(MenuAction::ToggleStreaming),
        "start_daemon" => Some(MenuAction::StartDaemon),
        "resume_daemon" => Some(MenuAction::ResumeDaemon),
        "stop_daemon" => Some(MenuAction::StopDaemon),
        "show_mini_player" => Some(MenuAction::ShowMiniPlayer),
        "open_tui" => Some(MenuAction::OpenTui),
        "refresh" => Some(MenuAction::Refresh),
        "toggle_startup" => Some(MenuAction::ToggleStartup),
        "quit_player" => Some(MenuAction::QuitPlayer),
        "quit_tray" => Some(MenuAction::QuitTray),
        _ => None,
    }
}

fn action_slug(action: MenuAction) -> &'static str {
    match action {
        MenuAction::PlayPause => "play_pause",
        MenuAction::Next => "next",
        MenuAction::Previous => "previous",
        MenuAction::SeekBack => "seek_back",
        MenuAction::SeekForward => "seek_forward",
        MenuAction::VolumeUp => "volume_up",
        MenuAction::VolumeDown => "volume_down",
        MenuAction::ToggleStreaming => "toggle_streaming",
        MenuAction::StartDaemon => "start_daemon",
        MenuAction::ResumeDaemon => "resume_daemon",
        MenuAction::StopDaemon => "stop_daemon",
        MenuAction::ShowMiniPlayer => "show_mini_player",
        MenuAction::OpenTui => "open_tui",
        MenuAction::Refresh => "refresh",
        MenuAction::ToggleStartup => "toggle_startup",
        MenuAction::QuitPlayer => "quit_player",
        MenuAction::QuitTray => "quit_tray",
    }
}

fn toggle_startup_entry() -> Result<(), startup::StartupError> {
    match startup::status()? {
        StartupStatus::Enabled { .. } => startup::uninstall(),
        StartupStatus::Disabled => startup::install().map(|_| ()),
        StartupStatus::Unsupported => Err(startup::StartupError::Unsupported),
    }
}

fn startup_menu_state() -> (bool, bool) {
    match startup::status() {
        Ok(StartupStatus::Enabled { .. }) => (true, true),
        Ok(StartupStatus::Disabled) => (false, true),
        Ok(StartupStatus::Unsupported) => (false, false),
        Err(e) => {
            report_error(e);
            (false, true)
        }
    }
}

fn tooltip_for_state(state: &TrayState) -> String {
    let text = match state {
        TrayState::Disconnected => "ytm-tui is not running".to_string(),
        TrayState::Connected(status) => {
            if status.owner_mode == InstanceMode::Daemon
                && status.total == 0
                && status.title.as_deref().unwrap_or_default().is_empty()
            {
                return "ytm-tui daemon idle".to_string();
            }
            let prefix = if status.paused { "Paused" } else { "Playing" };
            match (status.artist.as_deref(), status.title.as_deref()) {
                (Some(artist), Some(title)) if !artist.is_empty() && !title.is_empty() => {
                    format!("{prefix}: {artist} - {title}")
                }
                (_, Some(title)) if !title.is_empty() => format!("{prefix}: {title}"),
                _ => "ytm-tui: nothing playing".to_string(),
            }
        }
    };
    truncate_tooltip(text)
}

fn truncate_tooltip(text: String) -> String {
    // The shell's szTip buffer holds 128 UTF-16 units including the NUL, so budget
    // in UTF-16 units, not chars — a title full of non-BMP codepoints (emoji, rare
    // CJK) would otherwise overflow into a garbled tooltip.
    const MAX_UTF16: usize = 124;
    if text.encode_utf16().count() <= MAX_UTF16 {
        return text;
    }
    let mut used = 0usize;
    let mut short = String::new();
    for ch in text.chars() {
        let units = ch.len_utf16();
        if used + units > MAX_UTF16 - 3 {
            break;
        }
        used += units;
        short.push(ch);
    }
    short.push_str("...");
    short
}

fn status_is_idle(status: &StatusSnapshot) -> bool {
    status.total == 0 && status.title.as_deref().unwrap_or_default().is_empty()
}

fn spawn_async_command<F, Fut>(make_future: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let builder = thread::Builder::new().name(COMMAND_THREAD_NAME.to_string());
    if let Err(e) = builder.spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                report_error(format_args!("could not start command runtime: {e}"));
                return;
            }
        };
        rt.block_on(make_future());
    }) {
        report_error(format_args!("could not start command thread: {e}"));
    }
}

fn set_app_user_model_id() {
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

    let app_id = APP_ID
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let hr = unsafe { SetCurrentProcessExplicitAppUserModelID(app_id.as_ptr()) };
    if hr < 0 {
        report_error(format_args!(
            "could not set AppUserModelID (HRESULT {hr:#x})"
        ));
    }
}

fn init_file_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dirs = directories::ProjectDirs::from("", "", "ytm-tui")?;
    let dir = dirs.cache_dir();
    if let Err(e) = std::fs::create_dir_all(dir) {
        report_error(format_args!(
            "could not create log directory {}: {e}",
            dir.display()
        ));
        return None;
    }

    let guard = crate::logging::init(dir);
    if guard.is_some() {
        tracing::info!(
            target: "ytt_tray",
            path = %dir.join("ytm-tui.log").display(),
            "ytt-desktop logging initialized"
        );
    }
    guard
}

fn install_tray_panic_hook() {
    // panic = "abort" kills the process before tracing-appender's worker thread can
    // flush, so mirror every panic synchronously into a plain file next to the log.
    let panic_log = directories::ProjectDirs::from("", "", "ytm-tui")
        .map(|dirs| dirs.cache_dir().join("ytt-desktop-panic.log"));
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "ytt_tray", panic = %info, "ytt-desktop panic");
        if let Some(path) = &panic_log
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            use std::io::Write;
            let unix_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or_default();
            let _ = writeln!(file, "[unix {unix_secs}] ytt-desktop panic: {info}");
        }
        previous(info);
    }));
}

fn report_error(message: impl std::fmt::Display) {
    let message = message.to_string();
    tracing::error!(target: "ytt_tray", "ytt-desktop: {message}");
    #[cfg(debug_assertions)]
    eprintln!("ytt-desktop: {message}");
}

fn app_icon() -> Result<Icon, Box<dyn Error>> {
    let entry = find_ico_entry(ICO_BYTES, 32)
        .or_else(|| find_ico_entry(ICO_BYTES, 48))
        .or_else(|| find_ico_entry(ICO_BYTES, 256))
        .ok_or("ytm-tui.ico has no usable tray icon image")?;
    let image = image::load_from_memory(&ICO_BYTES[entry.offset..entry.end()])?.to_rgba8();
    let (width, height) = image.dimensions();
    Ok(Icon::from_rgba(image.into_raw(), width, height)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IcoEntry {
    width: u32,
    height: u32,
    bytes: usize,
    offset: usize,
}

impl IcoEntry {
    fn end(self) -> usize {
        self.offset + self.bytes
    }
}

fn ico_entries(bytes: &[u8]) -> Result<Vec<IcoEntry>, &'static str> {
    if bytes.len() < 6 {
        return Err("ICO header is too short");
    }
    let reserved = u16::from_le_bytes([bytes[0], bytes[1]]);
    let kind = u16::from_le_bytes([bytes[2], bytes[3]]);
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    if reserved != 0 || kind != 1 || count == 0 {
        return Err("invalid ICO header");
    }
    let dir_len = 6usize
        .checked_add(count.checked_mul(16).ok_or("ICO entry count overflows")?)
        .ok_or("ICO directory overflows")?;
    if bytes.len() < dir_len {
        return Err("ICO directory is truncated");
    }

    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let base = 6 + index * 16;
        let width = if bytes[base] == 0 {
            256
        } else {
            bytes[base] as u32
        };
        let height = if bytes[base + 1] == 0 {
            256
        } else {
            bytes[base + 1] as u32
        };
        let bytes_len = u32::from_le_bytes([
            bytes[base + 8],
            bytes[base + 9],
            bytes[base + 10],
            bytes[base + 11],
        ]) as usize;
        let offset = u32::from_le_bytes([
            bytes[base + 12],
            bytes[base + 13],
            bytes[base + 14],
            bytes[base + 15],
        ]) as usize;
        let end = offset
            .checked_add(bytes_len)
            .ok_or("ICO image range overflows")?;
        if end > bytes.len() {
            return Err("ICO image range is outside the file");
        }
        entries.push(IcoEntry {
            width,
            height,
            bytes: bytes_len,
            offset,
        });
    }
    Ok(entries)
}

fn find_ico_entry(bytes: &[u8], size: u32) -> Option<IcoEntry> {
    ico_entries(bytes)
        .ok()?
        .into_iter()
        .find(|entry| entry.width == size && entry.height == size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::{InstanceMode, StatusSnapshot};

    #[test]
    fn menu_ids_round_trip_actions() {
        for action in [
            MenuAction::PlayPause,
            MenuAction::Next,
            MenuAction::Previous,
            MenuAction::SeekBack,
            MenuAction::SeekForward,
            MenuAction::VolumeUp,
            MenuAction::VolumeDown,
            MenuAction::ToggleStreaming,
            MenuAction::StartDaemon,
            MenuAction::ResumeDaemon,
            MenuAction::StopDaemon,
            MenuAction::ShowMiniPlayer,
            MenuAction::OpenTui,
            MenuAction::Refresh,
            MenuAction::ToggleStartup,
            MenuAction::QuitPlayer,
            MenuAction::QuitTray,
        ] {
            assert_eq!(action_from_menu_id(&action_menu_id(action)), Some(action));
        }
    }

    #[test]
    fn tooltip_tracks_playback_state_and_shell_limit() {
        let state = TrayState::Connected(StatusSnapshot {
            title: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            paused: true,
            volume: 60,
            position: 1,
            total: 2,
            streaming: false,
            owner_mode: InstanceMode::StandaloneTui,
            settings: Default::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            artwork: None,
        });
        assert_eq!(tooltip_for_state(&state), "Paused: Artist - Song");
        let idle_daemon = TrayState::Connected(StatusSnapshot {
            title: None,
            artist: None,
            paused: true,
            volume: 80,
            position: 0,
            total: 0,
            streaming: false,
            owner_mode: InstanceMode::Daemon,
            settings: Default::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            artwork: None,
        });
        assert_eq!(tooltip_for_state(&idle_daemon), "ytm-tui daemon idle");
        assert_eq!(
            tooltip_for_state(&TrayState::Disconnected),
            "ytm-tui is not running"
        );

        let long = tooltip_for_state(&TrayState::Connected(StatusSnapshot {
            title: Some("T".repeat(200)),
            artist: Some("Artist".to_string()),
            paused: false,
            volume: 60,
            position: 1,
            total: 2,
            streaming: false,
            owner_mode: InstanceMode::StandaloneTui,
            settings: Default::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            artwork: None,
        }));
        assert_eq!(long.chars().count(), 124);
        assert!(long.ends_with("..."));
    }

    #[test]
    fn daemon_primary_menu_id_dispatches_state_dependent_event() {
        assert!(matches!(
            user_event_from_menu_id(&MenuId::new(DAEMON_PRIMARY_ID)),
            Some(UserEvent::DaemonPrimary)
        ));
    }

    #[test]
    fn tooltip_truncates_by_utf16_units() {
        let long = truncate_tooltip("\u{1F3B5}".repeat(100));
        assert!(long.encode_utf16().count() <= 124);
        assert!(long.ends_with("..."));
    }

    #[test]
    fn ico_resource_has_tray_and_shortcut_sizes() {
        let sizes = ico_entries(ICO_BYTES)
            .unwrap()
            .into_iter()
            .map(|entry| entry.width)
            .collect::<Vec<_>>();
        for expected in [16, 20, 24, 32, 40, 48, 64, 128, 256] {
            assert!(
                sizes.contains(&expected),
                "missing {expected}x{expected} icon in ytm-tui.ico; found {sizes:?}"
            );
        }
    }

    #[test]
    fn app_icon_decodes_from_ico() {
        assert!(app_icon().is_ok());
    }
}
