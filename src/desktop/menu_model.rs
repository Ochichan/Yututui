//! OS-neutral menu model for desktop companion backends.

use crate::remote::proto::{InstanceMode, RemoteCommand, StatusSnapshot, ToggleState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayStateKind {
    ConnectedPlaying,
    ConnectedPaused,
    ConnectedIdle,
    Disconnected,
}

// Connected carries the full snapshot inline; one lives at a time (2s poll cadence,
// never collected in bulk), so boxing would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayState {
    Connected(StatusSnapshot),
    Disconnected { resume_available: bool },
}

impl TrayState {
    pub const fn disconnected(resume_available: bool) -> Self {
        Self::Disconnected { resume_available }
    }

    pub fn kind(&self) -> TrayStateKind {
        match self {
            TrayState::Disconnected { .. } => TrayStateKind::Disconnected,
            TrayState::Connected(status) if is_idle(status) => TrayStateKind::ConnectedIdle,
            TrayState::Connected(status) if status.paused => TrayStateKind::ConnectedPaused,
            TrayState::Connected(_) => TrayStateKind::ConnectedPlaying,
        }
    }

    pub fn status(&self) -> Option<&StatusSnapshot> {
        match self {
            TrayState::Connected(status) => Some(status),
            TrayState::Disconnected { .. } => None,
        }
    }

    pub fn resume_available(&self) -> bool {
        match self {
            TrayState::Connected(status) => !is_idle(status),
            TrayState::Disconnected { resume_available } => *resume_available,
        }
    }

    /// Merge a fresh projection into the desktop process's offline recovery capability cache.
    /// An idle connected snapshot cannot prove the persisted cache is empty, so it preserves the
    /// last known value; a playable snapshot proves resumability and an offline snapshot carries
    /// the disk-verified answer.
    pub fn update_known_resume_available(&self, known: &mut bool) {
        match self {
            TrayState::Connected(status) if !is_idle(status) => *known = true,
            TrayState::Disconnected { resume_available } => *known = *resume_available,
            TrayState::Connected(_) => {}
        }
    }
}

/// The exact subset of a poll state that the native menu **and** tooltip are derived from
/// (`build_menu` + each platform's `tooltip_for_state`). Equal signatures ⇒ a byte-identical
/// menu and tooltip, so a platform tray can skip the native rebuild — an allocating
/// `Vec<String>` model build plus a per-item ObjC/Win32 `set_text`/`set_enabled` walk and the
/// tooltip set — whenever this is unchanged between polls. During steady playback it *is*
/// unchanged on every 2s poll: `position`/`volume`/`elapsed`/`queue`/`shuffle`/`repeat`/
/// `artwork`/`settings` feed the mini-player panel, never the menu, so they are deliberately
/// excluded. Keep this in lockstep with what `build_menu` + `tooltip_for_state` read (the
/// `menu_signature_*` tests fail if a menu-relevant field is dropped or an irrelevant one is
/// added).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuSignature {
    Disconnected {
        resume_available: bool,
    },
    Connected {
        title: Option<String>,
        artist: Option<String>,
        paused: bool,
        total: usize,
        owner_mode: InstanceMode,
        streaming: bool,
    },
}

pub fn menu_signature(state: &TrayState) -> MenuSignature {
    match state {
        TrayState::Disconnected { resume_available } => MenuSignature::Disconnected {
            resume_available: *resume_available,
        },
        TrayState::Connected(s) => MenuSignature::Connected {
            title: s.title.clone(),
            artist: s.artist.clone(),
            paused: s.paused,
            total: s.total,
            owner_mode: s.owner_mode,
            streaming: s.streaming,
        },
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
    OpenMainWindow,
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
            | MenuAction::OpenMainWindow
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuSubmenuId {
    Session,
    Playback,
}

impl MenuSubmenuId {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Playback => "playback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuSubmenu {
    pub id: MenuSubmenuId,
    pub label: String,
    pub enabled: bool,
    pub entries: Vec<MenuEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item(MenuItem),
    Submenu(MenuSubmenu),
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
            .find_map(|entry| match entry {
                MenuEntry::Item(item) => Some(item.label.as_str()),
                MenuEntry::Submenu(_) | MenuEntry::Separator => None,
            })
            .unwrap_or("yututui");
        format!("{:?}: {track}", self.state)
    }

    pub fn action_item(&self, action: MenuAction) -> Option<&MenuItem> {
        find_action_item(&self.entries, action)
    }

    pub fn submenu(&self, id: MenuSubmenuId) -> Option<&MenuSubmenu> {
        find_submenu(&self.entries, id)
    }

    pub fn visit_items(&self, mut visitor: impl FnMut(&MenuItem)) {
        visit_items(&self.entries, &mut visitor);
    }
}

fn find_action_item(entries: &[MenuEntry], action: MenuAction) -> Option<&MenuItem> {
    entries.iter().find_map(|entry| match entry {
        MenuEntry::Item(item) if item.action == Some(action) => Some(item),
        MenuEntry::Submenu(submenu) => find_action_item(&submenu.entries, action),
        MenuEntry::Item(_) | MenuEntry::Separator => None,
    })
}

fn find_submenu(entries: &[MenuEntry], id: MenuSubmenuId) -> Option<&MenuSubmenu> {
    entries.iter().find_map(|entry| match entry {
        MenuEntry::Submenu(submenu) if submenu.id == id => Some(submenu),
        MenuEntry::Submenu(submenu) => find_submenu(&submenu.entries, id),
        MenuEntry::Item(_) | MenuEntry::Separator => None,
    })
}

fn visit_items(entries: &[MenuEntry], visitor: &mut impl FnMut(&MenuItem)) {
    for entry in entries {
        match entry {
            MenuEntry::Item(item) => visitor(item),
            MenuEntry::Submenu(submenu) => visit_items(&submenu.entries, visitor),
            MenuEntry::Separator => {}
        }
    }
}

pub fn build_menu(state: &TrayState) -> MenuModel {
    build_menu_with_main_window(state, crate::desktop::assets::DIST_EMBEDDED)
}

fn build_menu_with_main_window(state: &TrayState, main_window_available: bool) -> MenuModel {
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

    let session = submenu(
        MenuSubmenuId::Session,
        crate::t!("Session", "세션", "セッション"),
        true,
        vec![daemon_action_item(state, 0), daemon_action_item(state, 1)],
    );
    let playback = submenu(
        MenuSubmenuId::Playback,
        crate::t!("Playback", "재생", "再生"),
        connected,
        vec![
            item(
                crate::t!("Seek Back", "뒤로 탐색", "シークで戻る"),
                has_track,
                Some(MenuAction::SeekBack),
            ),
            item(
                crate::t!("Seek Forward", "앞으로 탐색", "シークで進む"),
                has_track,
                Some(MenuAction::SeekForward),
            ),
            MenuEntry::Separator,
            item(
                crate::t!("Volume Down", "볼륨 낮추기", "音量を下げる"),
                connected,
                Some(MenuAction::VolumeDown),
            ),
            item(
                crate::t!("Volume Up", "볼륨 높이기", "音量を上げる"),
                connected,
                Some(MenuAction::VolumeUp),
            ),
            item(
                autoplay_label(state),
                connected,
                Some(MenuAction::ToggleStreaming),
            ),
        ],
    );

    let mut entries = vec![
        item(track_label(state), false, None),
        item(state_label(state), false, None),
        MenuEntry::Separator,
        item(
            crate::t!("Previous", "이전 곡", "前の曲"),
            has_track,
            Some(MenuAction::Previous),
        ),
        item(
            crate::t!("Play / Pause", "재생 / 일시정지", "再生 / 一時停止"),
            has_track,
            Some(MenuAction::PlayPause),
        ),
        item(
            crate::t!("Next", "다음 곡", "次の曲"),
            has_track,
            Some(MenuAction::Next),
        ),
        MenuEntry::Separator,
        item(
            crate::t!(
                "Show Mini Player",
                "미니플레이어 열기",
                "ミニプレイヤーを表示"
            ),
            true,
            Some(MenuAction::ShowMiniPlayer),
        ),
        session,
        playback,
        MenuEntry::Separator,
        item(
            crate::t!("Open Player", "플레이어 열기", "プレイヤーを開く"),
            true,
            Some(MenuAction::OpenTui),
        ),
        item(
            crate::t!("Open Main Window", "메인 창 열기", "メインウィンドウを開く"),
            true,
            Some(MenuAction::OpenMainWindow),
        ),
        item(
            crate::t!("Open at Login", "로그인 시 열기", "ログイン時に起動"),
            true,
            Some(MenuAction::ToggleStartup),
        ),
        MenuEntry::Separator,
        item(
            crate::t!("Quit Player", "플레이어 종료", "プレイヤーを終了"),
            connected,
            Some(MenuAction::QuitPlayer),
        ),
        item(
            crate::t!("Quit Tray", "트레이 종료", "トレイを終了"),
            true,
            Some(MenuAction::QuitTray),
        ),
    ];

    // Packaged releases intentionally ship the native tray + mini player without the full
    // web-GUI application. Do not expose an action that would open build.rs's missing-frontend
    // stub; developer builds with an embedded dist retain the explicit main-window surface.
    if !main_window_available {
        entries.retain(|entry| {
            !matches!(
                entry,
                MenuEntry::Item(MenuItem {
                    action: Some(MenuAction::OpenMainWindow),
                    ..
                })
            )
        });
    }

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

fn submenu(
    id: MenuSubmenuId,
    label: impl Into<String>,
    enabled: bool,
    entries: Vec<MenuEntry>,
) -> MenuEntry {
    MenuEntry::Submenu(MenuSubmenu {
        id,
        label: label.into(),
        enabled,
        entries,
    })
}

fn is_idle(status: &StatusSnapshot) -> bool {
    status.total == 0 && status.title.as_deref().unwrap_or_default().is_empty()
}

fn track_label(state: &TrayState) -> String {
    let Some(status) = state.status() else {
        return crate::t!(
            "YuTuTui! is not running",
            "YuTuTui!가 실행 중이 아닙니다",
            "YuTuTui!は実行されていません"
        )
        .to_string();
    };
    match (status.artist.as_deref(), status.title.as_deref()) {
        (Some(artist), Some(title)) if !artist.is_empty() && !title.is_empty() => {
            format!("{artist} - {title}")
        }
        (_, Some(title)) if !title.is_empty() => title.to_string(),
        _ => crate::t!("Nothing playing", "재생 중인 곡 없음", "再生中の曲なし").to_string(),
    }
}

fn daemon_action_item(state: &TrayState, index: usize) -> MenuEntry {
    let daemon_owner = state
        .status()
        .is_some_and(|status| status.owner_mode == InstanceMode::Daemon);
    let daemon_idle = daemon_owner && state.status().is_some_and(is_idle);
    let disconnected = matches!(state, TrayState::Disconnected { .. });
    let resume_available = state.resume_available();
    match (daemon_owner, daemon_idle, disconnected, index) {
        (true, _, _, 0) => item(
            crate::t!("Stop Music Daemon", "음악 데몬 중지", "音楽デーモンを停止"),
            true,
            Some(MenuAction::StopDaemon),
        ),
        (true, true, _, _) => item(
            crate::t!(
                "Resume Last Session",
                "이전 세션 재개",
                "前回のセッションを再開"
            ),
            true,
            Some(MenuAction::ResumeDaemon),
        ),
        (true, false, _, _) => item(
            crate::t!(
                "Resume Last Session",
                "이전 세션 재개",
                "前回のセッションを再開"
            ),
            false,
            Some(MenuAction::ResumeDaemon),
        ),
        (false, _, true, 0) => item(
            crate::t!("Start Music Daemon", "음악 데몬 시작", "音楽デーモンを開始"),
            true,
            Some(MenuAction::StartDaemon),
        ),
        (false, _, true, _) => item(
            crate::t!(
                "Resume Last Session",
                "이전 세션 재개",
                "前回のセッションを再開"
            ),
            resume_available,
            Some(MenuAction::ResumeDaemon),
        ),
        (false, _, false, 0) => item(
            crate::t!("Start Music Daemon", "음악 데몬 시작", "音楽デーモンを開始"),
            false,
            Some(MenuAction::StartDaemon),
        ),
        (false, _, false, _) => item(
            crate::t!(
                "Resume Last Session",
                "이전 세션 재개",
                "前回のセッションを再開"
            ),
            false,
            Some(MenuAction::ResumeDaemon),
        ),
    }
}

fn state_label(state: &TrayState) -> String {
    match state {
        TrayState::Disconnected { .. } => {
            crate::t!("Disconnected", "연결 끊김", "未接続").to_string()
        }
        TrayState::Connected(status) => {
            let owner = match status.owner_mode {
                InstanceMode::StandaloneTui => {
                    crate::t!("Standalone TUI", "독립형 TUI", "スタンドアロンTUI")
                }
                InstanceMode::Daemon => crate::t!("Daemon", "데몬", "デーモン"),
            };
            let state = match state.kind() {
                TrayStateKind::ConnectedPlaying => crate::t!("Playing", "재생 중", "再生中"),
                TrayStateKind::ConnectedPaused => crate::t!("Paused", "일시정지", "一時停止"),
                TrayStateKind::ConnectedIdle => crate::t!("Idle", "대기 중", "待機中"),
                TrayStateKind::Disconnected => crate::t!("Disconnected", "연결 끊김", "未接続"),
            };
            format!("{owner}: {state}")
        }
    }
}

fn autoplay_label(state: &TrayState) -> String {
    let on = state
        .status()
        .map(|status| status.streaming)
        .unwrap_or(false);
    if on {
        crate::t!("Autoplay: On", "자동재생: 켬", "自動再生: オン").to_string()
    } else {
        crate::t!("Autoplay: Off", "자동재생: 끔", "自動再生: オフ").to_string()
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
            elapsed_ms: None,
            duration_ms: None,
            is_live: false,
            queue_rev: None,
            track_id: None,
            position_epoch: 0,
            artwork: None,
            personal_sync: None,
        }
    }

    #[test]
    fn menu_signature_ignores_playback_only_fields() {
        // These fields feed the mini-player panel, never the menu/tooltip — changing them must
        // NOT change the signature, or the tray would rebuild the native menu on every poll
        // during playback (the exact churn the guard removes).
        let base = playing_status();
        let sig = menu_signature(&TrayState::Connected(base.clone()));
        let mut s = base.clone();
        s.position += 1;
        s.volume += 1;
        s.elapsed_ms = Some(1_000);
        s.duration_ms = Some(200_000);
        s.queue = Vec::new();
        s.shuffle = !base.shuffle;
        s.repeat = Default::default();
        assert_eq!(sig, menu_signature(&TrayState::Connected(s)));
    }

    #[test]
    fn menu_signature_reflects_every_menu_field() {
        // Every field `build_menu` + `tooltip_for_state` read must move the signature, so the
        // guard can never suppress a real menu change.
        let base = playing_status();
        let sig = menu_signature(&TrayState::Connected(base.clone()));
        let mut variants = Vec::new();
        let mut s = base.clone();
        s.title = Some("Other".to_string());
        variants.push(s);
        let mut s = base.clone();
        s.artist = Some("Other".to_string());
        variants.push(s);
        let mut s = base.clone();
        s.paused = !base.paused;
        variants.push(s);
        let mut s = base.clone();
        s.total = base.total + 5;
        variants.push(s);
        let mut s = base.clone();
        s.owner_mode = InstanceMode::Daemon;
        variants.push(s);
        let mut s = base.clone();
        s.streaming = !base.streaming;
        variants.push(s);
        for s in variants {
            assert_ne!(sig, menu_signature(&TrayState::Connected(s)));
        }
        // Disconnected is its own signature.
        assert_ne!(sig, menu_signature(&TrayState::disconnected(false)));
        assert_ne!(
            menu_signature(&TrayState::disconnected(false)),
            menu_signature(&TrayState::disconnected(true))
        );
    }

    #[test]
    fn resume_capability_cache_survives_idle_but_accepts_offline_disk_truth() {
        let mut known = false;
        TrayState::Connected(playing_status()).update_known_resume_available(&mut known);
        assert!(known);

        let mut idle = playing_status();
        idle.title = None;
        idle.artist = None;
        idle.total = 0;
        TrayState::Connected(idle).update_known_resume_available(&mut known);
        assert!(known, "an idle push cannot disprove the persisted session");

        TrayState::disconnected(false).update_known_resume_available(&mut known);
        assert!(!known, "the offline disk check is authoritative");
    }

    #[test]
    fn playing_menu_has_expected_labels_and_primary_action() {
        let _guard = crate::i18n::lock_for_test();
        let model = build_menu_with_main_window(&TrayState::Connected(playing_status()), true);
        assert_eq!(model.state, TrayStateKind::ConnectedPlaying);
        assert_eq!(model.primary_action, MenuAction::PlayPause);
        assert_eq!(model.summary_line(), "ConnectedPlaying: Artist - Song");
        assert_eq!(
            model
                .action_item(MenuAction::ToggleStreaming)
                .unwrap()
                .label,
            "Autoplay: On"
        );
        assert!(model.action_item(MenuAction::Next).unwrap().enabled);
    }

    #[test]
    fn native_menu_uses_the_product_structure_and_nested_sections() {
        let _guard = crate::i18n::lock_for_test();
        let model = build_menu_with_main_window(&TrayState::Connected(playing_status()), true);
        assert_eq!(model.entries.len(), 17);
        assert!(matches!(
            &model.entries[0],
            MenuEntry::Item(item) if item.label == "Artist - Song" && item.action.is_none()
        ));
        assert!(matches!(
            &model.entries[1],
            MenuEntry::Item(item) if item.label == "Standalone TUI: Playing" && item.action.is_none()
        ));
        for index in [2, 6, 10, 14] {
            assert!(matches!(&model.entries[index], MenuEntry::Separator));
        }
        assert_eq!(
            model
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    MenuEntry::Item(item) => item.action,
                    MenuEntry::Submenu(_) | MenuEntry::Separator => None,
                })
                .collect::<Vec<_>>(),
            vec![
                MenuAction::Previous,
                MenuAction::PlayPause,
                MenuAction::Next,
                MenuAction::ShowMiniPlayer,
                MenuAction::OpenTui,
                MenuAction::OpenMainWindow,
                MenuAction::ToggleStartup,
                MenuAction::QuitPlayer,
                MenuAction::QuitTray,
            ]
        );
        assert!(matches!(
            &model.entries[8],
            MenuEntry::Submenu(submenu) if submenu.id == MenuSubmenuId::Session
        ));
        assert!(matches!(
            &model.entries[9],
            MenuEntry::Submenu(submenu) if submenu.id == MenuSubmenuId::Playback
        ));

        let session = model.submenu(MenuSubmenuId::Session).unwrap();
        assert_eq!(session.label, "Session");
        assert_eq!(
            session
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    MenuEntry::Item(item) => item.action,
                    MenuEntry::Submenu(_) | MenuEntry::Separator => None,
                })
                .collect::<Vec<_>>(),
            vec![MenuAction::StartDaemon, MenuAction::ResumeDaemon]
        );

        let playback = model.submenu(MenuSubmenuId::Playback).unwrap();
        assert_eq!(playback.label, "Playback");
        assert_eq!(
            playback
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    MenuEntry::Item(item) => item.action,
                    MenuEntry::Submenu(_) | MenuEntry::Separator => None,
                })
                .collect::<Vec<_>>(),
            vec![
                MenuAction::SeekBack,
                MenuAction::SeekForward,
                MenuAction::VolumeDown,
                MenuAction::VolumeUp,
                MenuAction::ToggleStreaming,
            ]
        );
        assert!(model.action_item(MenuAction::Refresh).is_none());
    }

    #[test]
    fn non_gui_package_keeps_tray_mini_and_hides_only_the_main_window() {
        let _guard = crate::i18n::lock_for_test();
        let model = build_menu_with_main_window(&TrayState::Connected(playing_status()), false);
        assert!(
            model
                .action_item(MenuAction::ShowMiniPlayer)
                .is_some_and(|item| item.enabled),
            "the native mini player remains a release surface"
        );
        assert!(model.action_item(MenuAction::OpenMainWindow).is_none());
        assert!(model.action_item(MenuAction::OpenTui).is_some());
        assert!(model.action_item(MenuAction::QuitTray).is_some());
    }

    #[test]
    fn model_lookup_and_visit_recurse_through_nested_submenus() {
        let nested = MenuModel {
            state: TrayStateKind::Disconnected,
            primary_action: MenuAction::OpenTui,
            entries: vec![submenu(
                MenuSubmenuId::Session,
                "Session",
                true,
                vec![submenu(
                    MenuSubmenuId::Playback,
                    "Playback",
                    true,
                    vec![item(
                        "Nested action",
                        true,
                        Some(MenuAction::ToggleStreaming),
                    )],
                )],
            )],
        };

        assert_eq!(
            nested
                .action_item(MenuAction::ToggleStreaming)
                .map(|item| item.label.as_str()),
            Some("Nested action")
        );
        assert_eq!(
            nested
                .submenu(MenuSubmenuId::Playback)
                .map(|submenu| submenu.label.as_str()),
            Some("Playback")
        );
        let mut visited = Vec::new();
        nested.visit_items(|item| visited.push(item.action));
        assert_eq!(visited, vec![Some(MenuAction::ToggleStreaming)]);
    }

    #[test]
    fn disconnected_menu_only_offers_resume_when_a_session_is_available() {
        let _guard = crate::i18n::lock_for_test();
        let model = build_menu_with_main_window(&TrayState::disconnected(false), true);
        assert_eq!(model.state, TrayStateKind::Disconnected);
        assert_eq!(model.primary_action, MenuAction::OpenTui);
        assert!(!model.action_item(MenuAction::PlayPause).unwrap().enabled);
        assert!(!model.action_item(MenuAction::QuitPlayer).unwrap().enabled);
        assert!(model.action_item(MenuAction::StartDaemon).unwrap().enabled);
        assert!(!model.action_item(MenuAction::ResumeDaemon).unwrap().enabled);
        assert!(model.action_item(MenuAction::OpenTui).unwrap().enabled);
        assert!(
            model
                .action_item(MenuAction::OpenMainWindow)
                .unwrap()
                .enabled
        );
        assert!(
            model
                .action_item(MenuAction::ToggleStartup)
                .unwrap()
                .enabled
        );
        assert!(model.action_item(MenuAction::QuitTray).unwrap().enabled);

        let resumable = build_menu_with_main_window(&TrayState::disconnected(true), true);
        assert!(
            resumable
                .action_item(MenuAction::ResumeDaemon)
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn idle_connected_menu_prefers_open_tui() {
        let _guard = crate::i18n::lock_for_test();
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
        let _guard = crate::i18n::lock_for_test();
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
        let _guard = crate::i18n::lock_for_test();
        let model = build_menu(&TrayState::Connected(playing_status()));
        assert!(!model.action_item(MenuAction::StartDaemon).unwrap().enabled);
        assert!(!model.action_item(MenuAction::ResumeDaemon).unwrap().enabled);
    }

    #[test]
    fn korean_language_localizes_native_recovery_and_playback_labels() {
        let _guard = crate::i18n::lock_for_test();
        crate::i18n::set_language(crate::i18n::Language::Korean);
        let disconnected = build_menu(&TrayState::disconnected(true));
        assert_eq!(
            disconnected.submenu(MenuSubmenuId::Session).unwrap().label,
            "세션"
        );
        assert_eq!(
            disconnected
                .action_item(MenuAction::ResumeDaemon)
                .unwrap()
                .label,
            "이전 세션 재개"
        );
        let playing = build_menu(&TrayState::Connected(playing_status()));
        assert_eq!(
            playing.submenu(MenuSubmenuId::Playback).unwrap().label,
            "재생"
        );
        assert_eq!(
            playing
                .action_item(MenuAction::ToggleStreaming)
                .unwrap()
                .label,
            "자동재생: 켬"
        );
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
        assert_eq!(MenuAction::OpenMainWindow.remote_command(), None);
        assert_eq!(MenuAction::StartDaemon.remote_command(), None);
        assert_eq!(MenuAction::StopDaemon.remote_command(), None);
        assert_eq!(MenuAction::ToggleStartup.remote_command(), None);
        assert_eq!(MenuAction::QuitTray.remote_command(), None);
    }
}
