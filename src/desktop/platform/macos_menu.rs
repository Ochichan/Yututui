//! Native macOS menu construction and semantic menu-id dispatch.

use std::collections::HashMap;
use std::error::Error;

use tray_icon::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};

use super::{UserEvent, report_error};
use crate::desktop::menu_model::{
    self, MenuAction, MenuEntry, MenuItem as ModelItem, MenuSubmenuId, TrayState,
};
use crate::desktop::single_instance::ActivationIntent;
use crate::desktop::startup::{self, StartupStatus};
use crate::remote::proto::InstanceMode;

pub(super) struct MacMenu {
    // `Menu` root for parity with the Windows backend, where a `Submenu` root is an
    // outright crash (muda WM_NCACTIVATE type confusion); on macOS both map to an
    // NSMenu, so this is behaviour-neutral and keeps one shape to reason about.
    pub(super) root: Menu,
    track: MenuItem,
    state: MenuItem,
    startup: CheckMenuItem,
    // Single handle for the first daemon slot. Its action flips between StartDaemon
    // and StopDaemon with the state, so an action-keyed handle map would strand it:
    // whichever variant was absent at build time could never appear afterwards.
    daemon_primary: MenuItem,
    actions: HashMap<MenuAction, MenuItem>,
    submenus: HashMap<MenuSubmenuId, Submenu>,
}

impl MacMenu {
    pub(super) fn new(state: &TrayState) -> Result<Self, Box<dyn Error>> {
        let model = menu_model::build_menu(state);
        let root = Menu::new();
        let mut handles = NativeMenuHandles::default();
        append_native_entries(NativeMenuParent::Root(&root), &model.entries, &mut handles)?;

        Ok(Self {
            root,
            track: handles.track.ok_or("missing track menu item")?,
            state: handles.state.ok_or("missing state menu item")?,
            startup: handles.startup.ok_or("missing startup menu item")?,
            daemon_primary: handles.daemon_primary.ok_or("missing daemon menu item")?,
            actions: handles.actions,
            submenus: handles.submenus,
        })
    }

    pub(super) fn apply_state(&self, state: &TrayState) {
        let model = menu_model::build_menu(state);
        for (id, handle) in &self.submenus {
            if let Some(submenu) = model.submenu(*id) {
                handle.set_text(&submenu.label);
                handle.set_enabled(submenu.enabled);
            }
        }
        let mut disabled_index = 0usize;
        model.visit_items(|item| match item.action {
            Some(MenuAction::ToggleStartup) => {
                // Label only. Checked/enabled refresh on StartupChanged events —
                // hitting the LaunchAgent plist on every status poll was main-
                // thread disk IO plus report_error spam on persistent failure.
                self.startup.set_text(&item.label);
            }
            Some(action) if is_daemon_primary(Some(action)) => {
                self.daemon_primary.set_text(&item.label);
                self.daemon_primary.set_enabled(item.enabled);
            }
            Some(action) => {
                if let Some(handle) = self.actions.get(&action) {
                    handle.set_text(&item.label);
                    handle.set_enabled(item.enabled);
                }
            }
            None => {
                match disabled_index {
                    0 => {
                        self.track.set_text(&item.label);
                        self.track.set_enabled(item.enabled);
                    }
                    1 => {
                        self.state.set_text(&item.label);
                        self.state.set_enabled(item.enabled);
                    }
                    _ => {}
                }
                disabled_index += 1;
            }
        });
    }

    pub(super) fn apply_startup_status(&self) {
        let (checked, enabled) = startup_menu_state();
        self.startup.set_checked(checked);
        self.startup.set_enabled(enabled);
    }

    pub(super) fn set_startup_pending(&self, pending: bool) {
        if pending {
            self.startup.set_enabled(false);
        } else {
            self.apply_startup_status();
        }
    }
}

#[derive(Default)]
struct NativeMenuHandles {
    track: Option<MenuItem>,
    state: Option<MenuItem>,
    startup: Option<CheckMenuItem>,
    daemon_primary: Option<MenuItem>,
    actions: HashMap<MenuAction, MenuItem>,
    submenus: HashMap<MenuSubmenuId, Submenu>,
    disabled_index: usize,
}

#[derive(Clone, Copy)]
enum NativeMenuParent<'a> {
    Root(&'a Menu),
    Submenu(&'a Submenu),
}

impl NativeMenuParent<'_> {
    fn append(self, item: &dyn IsMenuItem) -> tray_icon::menu::Result<()> {
        match self {
            Self::Root(menu) => menu.append(item),
            Self::Submenu(menu) => menu.append(item),
        }
    }
}

fn append_native_entries(
    parent: NativeMenuParent<'_>,
    entries: &[MenuEntry],
    handles: &mut NativeMenuHandles,
) -> tray_icon::menu::Result<()> {
    for entry in entries {
        match entry {
            MenuEntry::Separator => parent.append(&PredefinedMenuItem::separator())?,
            MenuEntry::Submenu(model) => {
                let menu = Submenu::with_id(submenu_menu_id(model.id), &model.label, model.enabled);
                append_native_entries(NativeMenuParent::Submenu(&menu), &model.entries, handles)?;
                parent.append(&menu)?;
                handles.submenus.insert(model.id, menu);
            }
            MenuEntry::Item(item) => append_native_item(parent, item, handles)?,
        }
    }
    Ok(())
}

fn append_native_item(
    parent: NativeMenuParent<'_>,
    item: &ModelItem,
    handles: &mut NativeMenuHandles,
) -> tray_icon::menu::Result<()> {
    if item.action == Some(MenuAction::ToggleStartup) {
        let menu_item = make_startup_menu_item(item);
        parent.append(&menu_item)?;
        handles.startup = Some(menu_item);
        return Ok(());
    }
    if is_daemon_primary(item.action) {
        let menu_item = MenuItem::with_id(
            MenuId::new(DAEMON_PRIMARY_ID),
            &item.label,
            item.enabled,
            None,
        );
        parent.append(&menu_item)?;
        handles.daemon_primary = Some(menu_item);
        return Ok(());
    }

    let menu_item = make_menu_item(item, handles.disabled_index);
    if let Some(action) = item.action {
        handles.actions.insert(action, menu_item.clone());
    } else {
        match handles.disabled_index {
            0 => handles.track = Some(menu_item.clone()),
            1 => handles.state = Some(menu_item.clone()),
            _ => {}
        }
        handles.disabled_index += 1;
    }
    parent.append(&menu_item)
}

fn make_menu_item(item: &ModelItem, disabled_index: usize) -> MenuItem {
    if let Some(action) = item.action {
        MenuItem::with_id(action_menu_id(action), &item.label, item.enabled, None)
    } else {
        MenuItem::with_id(
            MenuId::new(format!("yututray:disabled:{disabled_index}")),
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

const DAEMON_PRIMARY_ID: &str = "yututray:daemon_primary";

fn submenu_menu_id(id: MenuSubmenuId) -> MenuId {
    MenuId::new(format!("yututray:submenu:{}", id.slug()))
}

fn is_daemon_primary(action: Option<MenuAction>) -> bool {
    matches!(
        action,
        Some(MenuAction::StartDaemon) | Some(MenuAction::StopDaemon)
    )
}

pub(super) fn user_event_from_menu_id(id: &MenuId) -> Option<UserEvent> {
    if id.as_ref() == DAEMON_PRIMARY_ID {
        return Some(UserEvent::DaemonPrimary);
    }
    let action = action_from_menu_id(id)?;
    Some(match action {
        MenuAction::ShowMiniPlayer => UserEvent::ShowMiniPlayer,
        MenuAction::OpenMainWindow => UserEvent::Activation(ActivationIntent::ShowMain),
        MenuAction::Refresh => UserEvent::Refresh,
        MenuAction::QuitTray => UserEvent::Quit,
        other => UserEvent::Menu(other),
    })
}

fn action_menu_id(action: MenuAction) -> MenuId {
    MenuId::new(format!("yututray:{}", action_slug(action)))
}

fn action_from_menu_id(id: &MenuId) -> Option<MenuAction> {
    let slug = id.as_ref().strip_prefix("yututray:")?;
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
        "open_main_window" => Some(MenuAction::OpenMainWindow),
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
        MenuAction::OpenMainWindow => "open_main_window",
        MenuAction::OpenTui => "open_tui",
        MenuAction::Refresh => "refresh",
        MenuAction::ToggleStartup => "toggle_startup",
        MenuAction::QuitPlayer => "quit_player",
        MenuAction::QuitTray => "quit_tray",
    }
}

pub(super) fn toggle_startup_entry() -> Result<(), startup::StartupError> {
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

pub(super) fn tooltip_for_state(state: &TrayState) -> String {
    match state {
        TrayState::Disconnected { .. } => "YuTuTui! is not running".to_string(),
        TrayState::Connected(status) => {
            if status.owner_mode == InstanceMode::Daemon
                && status.total == 0
                && status.title.as_deref().unwrap_or_default().is_empty()
            {
                return "YuTuTui! daemon idle".to_string();
            }
            let prefix = if status.paused { "Paused" } else { "Playing" };
            match (status.artist.as_deref(), status.title.as_deref()) {
                (Some(artist), Some(title)) if !artist.is_empty() && !title.is_empty() => {
                    format!("{prefix}: {artist} - {title}")
                }
                (_, Some(title)) if !title.is_empty() => format!("{prefix}: {title}"),
                _ => "YuTuTui!: nothing playing".to_string(),
            }
        }
    }
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
            MenuAction::StartDaemon,
            MenuAction::ResumeDaemon,
            MenuAction::StopDaemon,
            MenuAction::ShowMiniPlayer,
            MenuAction::OpenMainWindow,
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
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
            elapsed_ms: None,
            duration_ms: None,
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
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
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
        });
        assert_eq!(tooltip_for_state(&idle_daemon), "YuTuTui! daemon idle");
        assert_eq!(
            tooltip_for_state(&TrayState::disconnected(false)),
            "YuTuTui! is not running"
        );
    }

    #[test]
    fn daemon_primary_menu_id_dispatches_state_dependent_event() {
        assert!(matches!(
            user_event_from_menu_id(&MenuId::new(DAEMON_PRIMARY_ID)),
            Some(UserEvent::DaemonPrimary)
        ));
    }

    #[test]
    fn submenu_ids_are_stable_and_semantic() {
        assert_eq!(
            submenu_menu_id(MenuSubmenuId::Session).as_ref(),
            "yututray:submenu:session"
        );
        assert_eq!(
            submenu_menu_id(MenuSubmenuId::Playback).as_ref(),
            "yututray:submenu:playback"
        );
    }
}
