//! OS-neutral menu model for desktop companion backends.

use crate::remote::proto::{InstanceMode, RemoteCommand, StatusSnapshot, ToggleState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayStateKind {
    ConnectedPlaying,
    ConnectedPaused,
    ConnectedIdle,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayState {
    Connected(StatusSnapshot),
    Disconnected,
}

impl TrayState {
    pub fn kind(&self) -> TrayStateKind {
        match self {
            TrayState::Disconnected => TrayStateKind::Disconnected,
            TrayState::Connected(status) if is_idle(status) => TrayStateKind::ConnectedIdle,
            TrayState::Connected(status) if status.paused => TrayStateKind::ConnectedPaused,
            TrayState::Connected(_) => TrayStateKind::ConnectedPlaying,
        }
    }

    pub fn status(&self) -> Option<&StatusSnapshot> {
        match self {
            TrayState::Connected(status) => Some(status),
            TrayState::Disconnected => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuAction {
    PlayPause,
    Next,
    Previous,
    SeekBack,
    SeekForward,
    VolumeUp,
    VolumeDown,
    ToggleStreaming,
    StartDaemon,
    ResumeDaemon,
    StopDaemon,
    ShowMiniPlayer,
    OpenTui,
    Refresh,
    ToggleStartup,
    QuitPlayer,
    QuitTray,
}

impl MenuAction {
    pub fn remote_command(self) -> Option<RemoteCommand> {
        match self {
            MenuAction::PlayPause => Some(RemoteCommand::TogglePause),
            MenuAction::Next => Some(RemoteCommand::Next),
            MenuAction::Previous => Some(RemoteCommand::Prev),
            MenuAction::SeekBack => Some(RemoteCommand::SeekBack),
            MenuAction::SeekForward => Some(RemoteCommand::SeekForward),
            MenuAction::VolumeUp => Some(RemoteCommand::VolumeUp),
            MenuAction::VolumeDown => Some(RemoteCommand::VolumeDown),
            MenuAction::ToggleStreaming => Some(RemoteCommand::Streaming {
                state: ToggleState::Toggle,
            }),
            MenuAction::QuitPlayer => Some(RemoteCommand::Quit),
            MenuAction::StartDaemon
            | MenuAction::ResumeDaemon
            | MenuAction::StopDaemon
            | MenuAction::ShowMiniPlayer
            | MenuAction::OpenTui
            | MenuAction::Refresh
            | MenuAction::ToggleStartup
            | MenuAction::QuitTray => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub label: String,
    pub enabled: bool,
    pub action: Option<MenuAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    pub state: TrayStateKind,
    pub primary_action: MenuAction,
    pub entries: Vec<MenuEntry>,
}

impl MenuModel {
    pub fn summary_line(&self) -> String {
        let track = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                MenuEntry::Item(item) => Some(item.label.as_str()),
                MenuEntry::Separator => None,
            })
            .nth(1)
            .unwrap_or("ytm-tui");
        format!("{:?}: {track}", self.state)
    }

    pub fn action_item(&self, action: MenuAction) -> Option<&MenuItem> {
        self.entries.iter().find_map(|entry| match entry {
            MenuEntry::Item(item) if item.action == Some(action) => Some(item),
            _ => None,
        })
    }
}

pub fn build_menu(state: &TrayState) -> MenuModel {
    let kind = state.kind();
    let connected = !matches!(kind, TrayStateKind::Disconnected);
    let has_track = state.status().is_some_and(|status| !is_idle(status));
    let primary_action = if matches!(
        kind,
        TrayStateKind::Disconnected | TrayStateKind::ConnectedIdle
    ) {
        MenuAction::OpenTui
    } else {
        MenuAction::PlayPause
    };

    let entries = vec![
        item("YtmTui", false, None),
        item(track_label(state), false, None),
        item(state_label(state), false, None),
        MenuEntry::Separator,
        daemon_action_item(state, 0),
        daemon_action_item(state, 1),
        MenuEntry::Separator,
        item("Play / Pause", has_track, Some(MenuAction::PlayPause)),
        item("Next", has_track, Some(MenuAction::Next)),
        item("Previous", has_track, Some(MenuAction::Previous)),
        item("Seek Back", has_track, Some(MenuAction::SeekBack)),
        item("Seek Forward", has_track, Some(MenuAction::SeekForward)),
        MenuEntry::Separator,
        item("Volume Up", connected, Some(MenuAction::VolumeUp)),
        item("Volume Down", connected, Some(MenuAction::VolumeDown)),
        item(
            streaming_label(state),
            connected,
            Some(MenuAction::ToggleStreaming),
        ),
        MenuEntry::Separator,
        item("Show Mini Player", true, Some(MenuAction::ShowMiniPlayer)),
        item("Open TUI", true, Some(MenuAction::OpenTui)),
        item("Refresh", true, Some(MenuAction::Refresh)),
        item("Open at Login", true, Some(MenuAction::ToggleStartup)),
        item("Quit Player", connected, Some(MenuAction::QuitPlayer)),
        item("Quit Tray", true, Some(MenuAction::QuitTray)),
    ];

    MenuModel {
        state: kind,
        primary_action,
        entries,
    }
}

fn item(label: impl Into<String>, enabled: bool, action: Option<MenuAction>) -> MenuEntry {
    MenuEntry::Item(MenuItem {
        label: label.into(),
        enabled,
        action,
    })
}

fn is_idle(status: &StatusSnapshot) -> bool {
    status.total == 0 && status.title.as_deref().unwrap_or_default().is_empty()
}

fn track_label(state: &TrayState) -> String {
    let Some(status) = state.status() else {
        return "ytm-tui is not running".to_string();
    };
    match (status.artist.as_deref(), status.title.as_deref()) {
        (Some(artist), Some(title)) if !artist.is_empty() && !title.is_empty() => {
            format!("{artist} - {title}")
        }
        (_, Some(title)) if !title.is_empty() => title.to_string(),
        _ => "Nothing playing".to_string(),
    }
}

fn daemon_action_item(state: &TrayState, index: usize) -> MenuEntry {
    let daemon_owner = state
        .status()
        .is_some_and(|status| status.owner_mode == InstanceMode::Daemon);
    let daemon_idle = daemon_owner && state.status().is_some_and(is_idle);
    let disconnected = matches!(state, TrayState::Disconnected);
    match (daemon_owner, daemon_idle, disconnected, index) {
        (true, _, _, 0) => item("Stop Music Daemon", true, Some(MenuAction::StopDaemon)),
        (true, true, _, _) => item("Resume Last Session", true, Some(MenuAction::ResumeDaemon)),
        (true, false, _, _) => item("Resume Last Session", false, Some(MenuAction::ResumeDaemon)),
        (false, _, true, 0) => item("Start Music Daemon", true, Some(MenuAction::StartDaemon)),
        (false, _, true, _) => item("Resume Last Session", true, Some(MenuAction::ResumeDaemon)),
        (false, _, false, 0) => item("Start Music Daemon", false, Some(MenuAction::StartDaemon)),
        (false, _, false, _) => item("Resume Last Session", false, Some(MenuAction::ResumeDaemon)),
    }
}

fn state_label(state: &TrayState) -> String {
    match state {
        TrayState::Disconnected => "Disconnected".to_string(),
        TrayState::Connected(status) => {
            let owner = match status.owner_mode {
                InstanceMode::StandaloneTui => "Standalone TUI",
                InstanceMode::Daemon => "Daemon",
            };
            let state = match state.kind() {
                TrayStateKind::ConnectedPlaying => "Playing",
                TrayStateKind::ConnectedPaused => "Paused",
                TrayStateKind::ConnectedIdle => "Idle",
                TrayStateKind::Disconnected => "Disconnected",
            };
            format!("{owner}: {state}")
        }
    }
}

fn streaming_label(state: &TrayState) -> String {
    let on = state
        .status()
        .map(|status| status.streaming)
        .unwrap_or(false);
    if on {
        "Streaming: On".to_string()
    } else {
        "Streaming: Off".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn playing_status() -> StatusSnapshot {
        StatusSnapshot {
            title: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            paused: false,
            volume: 80,
            position: 1,
            total: 3,
            streaming: true,
            owner_mode: InstanceMode::StandaloneTui,
            settings: Default::default(),
            queue: Vec::new(),
            shuffle: false,
            repeat: Default::default(),
        }
    }

    #[test]
    fn playing_menu_has_expected_labels_and_primary_action() {
        let model = build_menu(&TrayState::Connected(playing_status()));
        assert_eq!(model.state, TrayStateKind::ConnectedPlaying);
        assert_eq!(model.primary_action, MenuAction::PlayPause);
        assert_eq!(model.summary_line(), "ConnectedPlaying: Artist - Song");
        assert_eq!(
            model
                .action_item(MenuAction::ToggleStreaming)
                .unwrap()
                .label,
            "Streaming: On"
        );
        assert!(model.action_item(MenuAction::Next).unwrap().enabled);
    }

    #[test]
    fn disconnected_menu_keeps_open_and_quit_tray_available() {
        let model = build_menu(&TrayState::Disconnected);
        assert_eq!(model.state, TrayStateKind::Disconnected);
        assert_eq!(model.primary_action, MenuAction::OpenTui);
        assert!(!model.action_item(MenuAction::PlayPause).unwrap().enabled);
        assert!(!model.action_item(MenuAction::QuitPlayer).unwrap().enabled);
        assert!(model.action_item(MenuAction::StartDaemon).unwrap().enabled);
        assert!(model.action_item(MenuAction::ResumeDaemon).unwrap().enabled);
        assert!(model.action_item(MenuAction::OpenTui).unwrap().enabled);
        assert!(
            model
                .action_item(MenuAction::ToggleStartup)
                .unwrap()
                .enabled
        );
        assert!(model.action_item(MenuAction::QuitTray).unwrap().enabled);
    }

    #[test]
    fn idle_connected_menu_prefers_open_tui() {
        let mut status = playing_status();
        status.title = None;
        status.artist = None;
        status.total = 0;
        let model = build_menu(&TrayState::Connected(status));
        assert_eq!(model.state, TrayStateKind::ConnectedIdle);
        assert_eq!(model.primary_action, MenuAction::OpenTui);
        assert!(!model.action_item(MenuAction::Next).unwrap().enabled);
        assert!(model.action_item(MenuAction::VolumeUp).unwrap().enabled);
    }

    #[test]
    fn daemon_connected_menu_identifies_daemon_owner() {
        let mut status = playing_status();
        status.owner_mode = InstanceMode::Daemon;
        status.title = None;
        status.artist = None;
        status.total = 0;
        let model = build_menu(&TrayState::Connected(status));
        assert_eq!(model.state, TrayStateKind::ConnectedIdle);
        assert!(
            model.entries.iter().any(
                |entry| matches!(entry, MenuEntry::Item(item) if item.label == "Daemon: Idle")
            )
        );
        assert!(model.action_item(MenuAction::StopDaemon).unwrap().enabled);
        assert!(model.action_item(MenuAction::ResumeDaemon).unwrap().enabled);
        assert!(!model.action_item(MenuAction::PlayPause).unwrap().enabled);
        assert!(!model.action_item(MenuAction::Next).unwrap().enabled);
    }

    #[test]
    fn standalone_connected_menu_does_not_start_a_second_owner() {
        let model = build_menu(&TrayState::Connected(playing_status()));
        assert!(!model.action_item(MenuAction::StartDaemon).unwrap().enabled);
        assert!(!model.action_item(MenuAction::ResumeDaemon).unwrap().enabled);
    }

    #[test]
    fn actions_map_to_remote_commands() {
        assert_eq!(
            MenuAction::PlayPause.remote_command(),
            Some(RemoteCommand::TogglePause)
        );
        assert_eq!(MenuAction::Next.remote_command(), Some(RemoteCommand::Next));
        assert_eq!(
            MenuAction::ToggleStreaming.remote_command(),
            Some(RemoteCommand::Streaming {
                state: ToggleState::Toggle
            })
        );
        assert_eq!(MenuAction::OpenTui.remote_command(), None);
        assert_eq!(MenuAction::ShowMiniPlayer.remote_command(), None);
        assert_eq!(MenuAction::StartDaemon.remote_command(), None);
        assert_eq!(MenuAction::StopDaemon.remote_command(), None);
        assert_eq!(MenuAction::ToggleStartup.remote_command(), None);
        assert_eq!(MenuAction::QuitTray.remote_command(), None);
    }
}
