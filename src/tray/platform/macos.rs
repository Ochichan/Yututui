//! macOS menu bar backend for `ytt-tray`.

use std::collections::HashMap;
use std::error::Error;
use std::panic;
use std::thread;

use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{CheckMenuItem, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::remote::proto::{InstanceMode, RemoteCommand, StatusSnapshot};
use crate::tray::control;
use crate::tray::launch;
use crate::tray::menu_model::{self, MenuAction, MenuEntry, MenuItem as ModelItem, TrayState};
use crate::tray::panel::PanelCommand;
use crate::tray::platform::panel_window::MiniPlayerPanel;
use crate::tray::startup::{self, StartupStatus};
use crate::tray::status::{self, PollConfig, PollUpdate};

const POLL_THREAD_NAME: &str = "ytt-tray-status";
const COMMAND_THREAD_NAME: &str = "ytt-tray-command";

#[derive(Debug)]
enum UserEvent {
    Status(PollUpdate),
    ShowMiniPlayer,
    Panel(PanelCommand),
    Menu(MenuAction),
    Refresh,
    StartupChanged,
    Quit,
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let _log_guard = init_file_logging();
    install_tray_panic_hook();

    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_dock_visibility(false);

    let proxy = event_loop.create_proxy();

    MenuEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |event: MenuEvent| {
            if let Some(action) = action_from_menu_id(event.id()) {
                let message = match action {
                    MenuAction::ShowMiniPlayer => UserEvent::ShowMiniPlayer,
                    MenuAction::Refresh => UserEvent::Refresh,
                    MenuAction::QuitTray => UserEvent::Quit,
                    other => UserEvent::Menu(other),
                };
                let _ = proxy.send_event(message);
            }
        }
    }));

    let mut app = MacTrayApp::new(proxy.clone());

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
            Event::UserEvent(UserEvent::ShowMiniPlayer) => {
                app.show_panel(target);
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
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                app.handle_window_close(window_id);
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

struct MacTrayApp {
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    menu: Option<MacMenu>,
    panel: Option<MiniPlayerPanel>,
    last_update: PollUpdate,
    poll_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MacTrayApp {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
            tray: None,
            menu: None,
            panel: None,
            last_update: PollUpdate::disconnected(control::ControlError::NotRunning),
            poll_shutdown: None,
        }
    }

    fn init(&mut self) -> Result<(), Box<dyn Error>> {
        let menu = MacMenu::new(&self.last_update.state)?;
        let icon = template_icon()?;
        let tray = TrayIconBuilder::new()
            .with_id("io.github.ochi.ytm-tui.tray")
            .with_menu(Box::new(menu.root.clone()))
            .with_tooltip(tooltip_for_state(&self.last_update.state))
            .with_icon(icon)
            .with_icon_as_template(true)
            .with_menu_on_left_click(true)
            .with_menu_on_right_click(true)
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

    fn handle_action(&self, action: MenuAction) {
        match action {
            MenuAction::ShowMiniPlayer => {
                let _ = self.proxy.send_event(UserEvent::ShowMiniPlayer);
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

    fn show_panel(&mut self, target: &EventLoopWindowTarget<UserEvent>) {
        if let Some(panel) = &self.panel {
            panel.show();
            panel.apply_update(&self.last_update);
            return;
        }

        let proxy = self.proxy.clone();
        match MiniPlayerPanel::create(target, &self.last_update, move |command| {
            let _ = proxy.send_event(UserEvent::Panel(command));
        }) {
            Ok(panel) => {
                panel.apply_update(&self.last_update);
                self.panel = Some(panel);
            }
            Err(e) => report_error(e),
        }
    }

    fn handle_panel_command(&self, command: PanelCommand) {
        if command == PanelCommand::Hide {
            if let Some(panel) = &self.panel {
                panel.hide();
            }
            return;
        }
        if let Some(remote) = command.remote_command() {
            self.send_panel_remote(remote, matches!(command, PanelCommand::SetStreaming(true)));
        } else if let Some(action) = command.menu_action() {
            self.handle_action(action);
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
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
    }
}

struct MacMenu {
    root: Submenu,
    track: MenuItem,
    state: MenuItem,
    startup: CheckMenuItem,
    actions: HashMap<MenuAction, MenuItem>,
}

impl MacMenu {
    fn new(state: &TrayState) -> Result<Self, Box<dyn Error>> {
        let model = menu_model::build_menu(state);
        let root = Submenu::new("YtmTui", true);
        let mut action_items = HashMap::new();
        let mut track = None;
        let mut state_item = None;
        let mut startup_item = None;
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
                    self.startup.set_text(item.label);
                    self.apply_startup_status();
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
            MenuId::new(format!("ytt-tray:disabled:{disabled_index}")),
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

fn action_menu_id(action: MenuAction) -> MenuId {
    MenuId::new(format!("ytt-tray:{}", action_slug(action)))
}

fn action_from_menu_id(id: &MenuId) -> Option<MenuAction> {
    let slug = id.as_ref().strip_prefix("ytt-tray:")?;
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
    match state {
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
    }
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
            "ytt-tray logging initialized"
        );
    }
    guard
}

fn install_tray_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        tracing::error!(target: "ytt_tray", panic = %info, "ytt-tray panic");
        previous(info);
    }));
}

fn report_error(message: impl std::fmt::Display) {
    let message = message.to_string();
    tracing::error!(target: "ytt_tray", "ytt-tray: {message}");
    #[cfg(debug_assertions)]
    eprintln!("ytt-tray: {message}");
}

fn template_icon() -> Result<Icon, tray_icon::BadIcon> {
    const SIZE: u32 = 18;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            let idx = ((y * SIZE + x) * 4) as usize;
            let ring = (6.1..=8.0).contains(&dist);
            let stem = (8..=10).contains(&x) && (3..=14).contains(&y);
            let dot = dist <= 2.1;
            if ring || stem || dot {
                rgba[idx] = 0;
                rgba[idx + 1] = 0;
                rgba[idx + 2] = 0;
                rgba[idx + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE)
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
    fn tooltip_tracks_playback_state() {
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
        });
        assert_eq!(tooltip_for_state(&idle_daemon), "ytm-tui daemon idle");
        assert_eq!(
            tooltip_for_state(&TrayState::Disconnected),
            "ytm-tui is not running"
        );
    }

    #[test]
    fn template_icon_is_valid_rgba() {
        assert!(template_icon().is_ok());
    }
}
