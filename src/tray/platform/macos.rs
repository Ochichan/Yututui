//! macOS menu bar backend for `ytt-tray`.

use std::collections::HashMap;
use std::error::Error;
use std::thread;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::remote::proto::RemoteCommand;
use crate::tray::control;
use crate::tray::launch;
use crate::tray::menu_model::{self, MenuAction, MenuEntry, MenuItem as ModelItem, TrayState};
use crate::tray::status::{self, PollConfig, PollUpdate};

const POLL_THREAD_NAME: &str = "ytt-tray-status";
const COMMAND_THREAD_NAME: &str = "ytt-tray-command";

#[derive(Debug)]
enum UserEvent {
    Status(PollUpdate),
    Menu(MenuAction),
    Refresh,
    Quit,
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_dock_visibility(false);

    let proxy = event_loop.create_proxy();

    MenuEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |event: MenuEvent| {
            if let Some(action) = action_from_menu_id(event.id()) {
                let message = match action {
                    MenuAction::Refresh => UserEvent::Refresh,
                    MenuAction::QuitTray => UserEvent::Quit,
                    other => UserEvent::Menu(other),
                };
                let _ = proxy.send_event(message);
            }
        }
    }));

    let mut app = MacTrayApp::new(proxy.clone());

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => {
                if let Err(e) = app.init() {
                    eprintln!("ytt-tray: {e}");
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Status(update)) => {
                app.apply_update(update);
            }
            Event::UserEvent(UserEvent::Refresh) => {
                app.request_status_now();
            }
            Event::UserEvent(UserEvent::Quit) => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::Menu(action)) => {
                app.handle_action(action);
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
    poll_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl MacTrayApp {
    fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            proxy,
            tray: None,
            menu: None,
            poll_shutdown: None,
        }
    }

    fn init(&mut self) -> Result<(), Box<dyn Error>> {
        let initial = PollUpdate::disconnected(control::ControlError::NotRunning);
        let menu = MacMenu::new(&initial.state)?;
        let icon = template_icon()?;
        let tray = TrayIconBuilder::new()
            .with_id("io.github.ochi.ytm-tui.tray")
            .with_menu(Box::new(menu.root.clone()))
            .with_tooltip(tooltip_for_state(&initial.state))
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
                    eprintln!("ytt-tray: could not start status runtime: {e}");
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
            eprintln!("ytt-tray: could not start status polling thread: {e}");
        }
    }

    fn apply_update(&mut self, update: PollUpdate) {
        if let Some(menu) = &self.menu {
            menu.apply_state(&update.state);
        }
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(tooltip_for_state(&update.state)));
        }
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
            MenuAction::OpenTui => match launch::open_tui() {
                Ok(_) => self.request_status_now(),
                Err(e) => eprintln!("ytt-tray: {e}"),
            },
            MenuAction::Refresh => self.request_status_now(),
            MenuAction::QuitTray => {
                let _ = self.proxy.send_event(UserEvent::Quit);
            }
            action => {
                if let Some(command) = action.remote_command() {
                    self.send_remote(command);
                }
            }
        }
    }

    fn send_remote(&self, command: RemoteCommand) {
        let proxy = self.proxy.clone();
        spawn_async_command(move || async move {
            if let Err(e) = control::send_remote(command).await {
                eprintln!("ytt-tray: {e}");
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
    actions: HashMap<MenuAction, MenuItem>,
}

impl MacMenu {
    fn new(state: &TrayState) -> Result<Self, Box<dyn Error>> {
        let model = menu_model::build_menu(state);
        let root = Submenu::new("YtmTui", true);
        let mut action_items = HashMap::new();
        let mut track = None;
        let mut state_item = None;
        let mut disabled_index = 0usize;

        for entry in model.entries {
            match entry {
                MenuEntry::Separator => root.append(&PredefinedMenuItem::separator())?,
                MenuEntry::Item(item) => {
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
        "open_tui" => Some(MenuAction::OpenTui),
        "refresh" => Some(MenuAction::Refresh),
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
        MenuAction::OpenTui => "open_tui",
        MenuAction::Refresh => "refresh",
        MenuAction::QuitPlayer => "quit_player",
        MenuAction::QuitTray => "quit_tray",
    }
}

fn tooltip_for_state(state: &TrayState) -> String {
    match state {
        TrayState::Disconnected => "ytm-tui is not running".to_string(),
        TrayState::Connected(status) => {
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
                eprintln!("ytt-tray: could not start command runtime: {e}");
                return;
            }
        };
        rt.block_on(make_future());
    }) {
        eprintln!("ytt-tray: could not start command thread: {e}");
    }
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
    use crate::remote::proto::StatusSnapshot;

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
            MenuAction::OpenTui,
            MenuAction::Refresh,
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
        });
        assert_eq!(tooltip_for_state(&state), "Paused: Artist - Song");
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
