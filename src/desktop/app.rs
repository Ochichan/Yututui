//! OS-neutral desktop shell state and policy.
//!
//! Platform adapters own only native event loops/windows.  This module owns the state that must
//! behave identically on Windows and macOS: v8 snapshot ordering, activation effects, window-role
//! policy, frontend readiness, and delayed transient-panel dismissal.

use std::time::{Duration, Instant};

use crate::remote::proto::{
    InstanceMode, PlayerModel, PushEvent, QueueItemSnapshot, QueueModel, SettingsModelV8,
    SettingsSnapshot, StatusSnapshot, Topic,
};

use super::gateway::ConnState;
use super::single_instance::ActivationIntent;

pub const PANEL_BLUR_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowKind {
    Main,
    Mini,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRole {
    MainApplication,
    MiniPopover,
    MiniPinned,
}

/// Native policy derived from role, never from visual skin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPolicy {
    pub role: WindowRole,
    pub show_in_taskbar: bool,
    pub show_in_app_switcher: bool,
    pub always_on_top: bool,
    pub dismiss_on_blur: bool,
}

impl WindowPolicy {
    pub fn for_window(kind: WindowKind, mini_pinned: bool) -> Self {
        match (kind, mini_pinned) {
            (WindowKind::Main, _) => Self {
                role: WindowRole::MainApplication,
                show_in_taskbar: true,
                show_in_app_switcher: true,
                always_on_top: false,
                dismiss_on_blur: false,
            },
            (WindowKind::Mini, false) => Self {
                role: WindowRole::MiniPopover,
                show_in_taskbar: false,
                show_in_app_switcher: false,
                always_on_top: false,
                dismiss_on_blur: true,
            },
            (WindowKind::Mini, true) => Self {
                role: WindowRole::MiniPinned,
                show_in_taskbar: false,
                show_in_app_switcher: false,
                always_on_top: true,
                dismiss_on_blur: false,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopConnection {
    Connecting,
    Online {
        protocol_version: u8,
        capabilities: Vec<String>,
        owner_mode: InstanceMode,
    },
    Offline {
        reason: String,
    },
}

impl From<&ConnState> for DesktopConnection {
    fn from(value: &ConnState) -> Self {
        match value {
            ConnState::Connecting => Self::Connecting,
            ConnState::Online {
                protocol_version,
                capabilities,
                owner_mode,
            } => Self::Online {
                protocol_version: *protocol_version,
                capabilities: capabilities.clone(),
                owner_mode: *owner_mode,
            },
            ConnState::Offline { reason } => Self::Offline {
                reason: reason.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DesktopSnapshot {
    pub connection: DesktopConnection,
    pub player: Option<PlayerModel>,
    pub queue: Option<QueueModel>,
    pub settings: Option<SettingsModelV8>,
    pub capabilities: Vec<String>,
    /// Last accepted v8 session sequence. Zero means no snapshot has arrived in this session.
    pub sequence: u64,
}

impl Default for DesktopSnapshot {
    fn default() -> Self {
        Self {
            connection: DesktopConnection::Connecting,
            player: None,
            queue: None,
            settings: None,
            capabilities: Vec::new(),
            sequence: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopCommandError {
    pub code: String,
    pub display_message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DesktopEvent {
    SnapshotChanged(Box<DesktopSnapshot>),
    CommandCompleted {
        id: u64,
        result: Result<serde_json::Value, DesktopCommandError>,
    },
    FrontendReady(WindowKind),
    FrontendTornDown(WindowKind),
    WindowEvent(DesktopWindowEvent),
    WindowVisibility {
        kind: WindowKind,
        visible: bool,
    },
    MiniPinned(bool),
    Deadline,
    Activation(ActivationIntent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopWindowEvent {
    Shown(WindowKind),
    Hidden(WindowKind),
    Focused(WindowKind),
    Blurred(WindowKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopEffect {
    EnsureTray,
    EnsureMiniSurface,
    ShowMini,
    HideMini,
    EnsureMainSurface,
    ShowMain,
    HideMain,
    ApplyWindowPolicy {
        kind: WindowKind,
        policy: WindowPolicy,
    },
    UseRegularActivation,
    UseAccessoryActivation,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrontendReplay {
    pub connection: DesktopConnection,
    pub snapshot: DesktopSnapshot,
}

/// One deterministic reducer decision. Native adapters execute `effects` in order and use
/// `replay` only after the matching page generation has completed its ready handshake.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub struct DesktopTransition {
    pub effects: Vec<DesktopEffect>,
    pub replay: Option<(WindowKind, FrontendReplay)>,
}

impl DesktopTransition {
    fn none() -> Self {
        Self {
            effects: Vec::new(),
            replay: None,
        }
    }
}

/// Generation-based delayed dismiss. Callers supply `now`, so tests exercise it with a fake
/// clock and production event loops do not need a timer thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FocusDismiss {
    generation: u64,
    pending: Option<(u64, Instant)>,
}

impl FocusDismiss {
    pub fn blur(&mut self, now: Instant) -> Instant {
        self.generation = self.generation.wrapping_add(1);
        let deadline = now + PANEL_BLUR_DELAY;
        self.pending = Some((self.generation, deadline));
        deadline
    }

    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = None;
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending.map(|(_, deadline)| deadline)
    }

    pub fn take_due(&mut self, now: Instant) -> bool {
        let Some((generation, deadline)) = self.pending else {
            return false;
        };
        if now < deadline {
            return false;
        }
        self.pending = None;
        generation == self.generation
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DesktopApp {
    snapshot: DesktopSnapshot,
    mini_pinned: bool,
    main_visible: bool,
    mini_visible: bool,
    mini_focused: bool,
    mini_ready: bool,
    main_ready: bool,
    dismiss: FocusDismiss,
}

impl DesktopApp {
    pub fn snapshot(&self) -> &DesktopSnapshot {
        &self.snapshot
    }

    pub fn set_connection(&mut self, connection: &ConnState) {
        let next = DesktopConnection::from(connection);
        // A Connecting transition starts a new session generation. Never retain models whose
        // sequence belongs to the previous owner/session.
        if matches!(next, DesktopConnection::Connecting) {
            self.snapshot.player = None;
            self.snapshot.queue = None;
            self.snapshot.settings = None;
            self.snapshot.sequence = 0;
        }
        self.snapshot.capabilities = match &next {
            DesktopConnection::Online { capabilities, .. } => capabilities.clone(),
            _ => Vec::new(),
        };
        self.snapshot.connection = next;
    }

    /// Apply a typed v8 push. Stale/duplicate sequences and topic/event mismatches are discarded.
    pub fn apply_push(&mut self, sequence: u64, topic: Topic, event: PushEvent) -> bool {
        if sequence <= self.snapshot.sequence {
            return false;
        }
        let applied = match (topic, event) {
            (Topic::Player, PushEvent::PlayerSnapshot { model }) => {
                self.snapshot.player = Some(*model);
                true
            }
            (Topic::Queue, PushEvent::QueueSnapshot { model }) => {
                self.snapshot.queue = Some(model);
                true
            }
            (Topic::Settings, PushEvent::SettingsSnapshot { model }) => {
                self.snapshot.settings = Some(*model);
                true
            }
            (Topic::System, PushEvent::OwnerChanged { mode }) => {
                if let DesktopConnection::Online { owner_mode, .. } = &mut self.snapshot.connection
                {
                    *owner_mode = mode;
                }
                true
            }
            (Topic::System, PushEvent::ShuttingDown) => {
                self.snapshot.connection = DesktopConnection::Offline {
                    reason: "shutting_down".to_string(),
                };
                true
            }
            (Topic::Search, PushEvent::SearchCompleted { .. }) => true,
            _ => false,
        };
        if applied {
            self.snapshot.sequence = sequence;
        }
        applied
    }

    /// Restore host-owned persisted policy before any native mini window exists.
    pub fn restore_mini_pinned(&mut self, pinned: bool) {
        self.mini_pinned = pinned;
        if pinned {
            self.dismiss.cancel();
        }
    }

    pub fn window_policy(&self, kind: WindowKind) -> WindowPolicy {
        WindowPolicy::for_window(kind, self.mini_pinned)
    }

    pub fn handle_event(&mut self, event: DesktopEvent) -> DesktopTransition {
        self.handle_event_at(event, Instant::now())
    }

    /// Deterministic reducer entry point used by event loops and fake-clock parity tests.
    pub fn handle_event_at(&mut self, event: DesktopEvent, now: Instant) -> DesktopTransition {
        let mut transition = DesktopTransition::none();
        match event {
            DesktopEvent::SnapshotChanged(snapshot) => self.snapshot = *snapshot,
            DesktopEvent::CommandCompleted { .. } => {}
            DesktopEvent::FrontendReady(kind) => {
                match kind {
                    WindowKind::Main => self.main_ready = true,
                    WindowKind::Mini => self.mini_ready = true,
                }
                transition.replay = Some((
                    kind,
                    FrontendReplay {
                        connection: self.snapshot.connection.clone(),
                        snapshot: self.snapshot.clone(),
                    },
                ));
                if self.is_window_visible(kind) {
                    transition.effects.extend(self.show_effects(kind));
                }
            }
            DesktopEvent::FrontendTornDown(kind) => match kind {
                WindowKind::Main => self.main_ready = false,
                WindowKind::Mini => self.mini_ready = false,
            },
            DesktopEvent::WindowEvent(event) => match event {
                DesktopWindowEvent::Shown(WindowKind::Main) => {
                    self.main_visible = true;
                    transition.effects.push(DesktopEffect::UseRegularActivation);
                }
                DesktopWindowEvent::Shown(WindowKind::Mini) => {
                    self.mini_visible = true;
                    self.dismiss.cancel();
                }
                DesktopWindowEvent::Hidden(WindowKind::Main) => {
                    self.main_visible = false;
                    transition
                        .effects
                        .push(DesktopEffect::UseAccessoryActivation);
                }
                DesktopWindowEvent::Hidden(WindowKind::Mini) => {
                    self.mini_visible = false;
                    self.mini_focused = false;
                    self.dismiss.cancel();
                }
                DesktopWindowEvent::Focused(WindowKind::Mini) => {
                    self.mini_focused = true;
                    self.dismiss.cancel();
                }
                DesktopWindowEvent::Blurred(WindowKind::Mini) => {
                    self.mini_focused = false;
                    if self.mini_visible && self.window_policy(WindowKind::Mini).dismiss_on_blur {
                        self.dismiss.blur(now);
                    }
                }
                DesktopWindowEvent::Focused(WindowKind::Main)
                | DesktopWindowEvent::Blurred(WindowKind::Main) => {}
            },
            DesktopEvent::WindowVisibility { kind, visible } => {
                if visible || self.is_window_visible(kind) {
                    transition
                        .effects
                        .extend(self.set_visibility(kind, visible));
                }
            }
            DesktopEvent::MiniPinned(pinned) => {
                self.mini_pinned = pinned;
                if pinned {
                    self.dismiss.cancel();
                } else if self.mini_visible && !self.mini_focused {
                    self.dismiss.blur(now);
                }
                transition.effects.push(DesktopEffect::ApplyWindowPolicy {
                    kind: WindowKind::Mini,
                    policy: self.window_policy(WindowKind::Mini),
                });
            }
            DesktopEvent::Deadline => {
                if self.dismiss.take_due(now)
                    && self.mini_visible
                    && !self.mini_focused
                    && !self.mini_pinned
                {
                    transition
                        .effects
                        .extend(self.set_visibility(WindowKind::Mini, false));
                }
            }
            DesktopEvent::Activation(intent) => {
                transition.effects.push(DesktopEffect::EnsureTray);
                match intent {
                    ActivationIntent::EnsureTray => {}
                    ActivationIntent::ShowMini => transition
                        .effects
                        .extend(self.set_visibility(WindowKind::Mini, true)),
                    ActivationIntent::ShowMain => transition
                        .effects
                        .extend(self.set_visibility(WindowKind::Main, true)),
                }
            }
        }
        transition
    }

    fn set_visibility(&mut self, kind: WindowKind, visible: bool) -> Vec<DesktopEffect> {
        match (kind, visible) {
            (WindowKind::Main, true) => {
                self.main_visible = true;
                self.show_effects(kind)
            }
            (WindowKind::Main, false) => {
                self.main_visible = false;
                vec![
                    DesktopEffect::HideMain,
                    DesktopEffect::UseAccessoryActivation,
                ]
            }
            (WindowKind::Mini, true) => {
                self.mini_visible = true;
                self.dismiss.cancel();
                self.show_effects(kind)
            }
            (WindowKind::Mini, false) => {
                self.mini_visible = false;
                self.mini_focused = false;
                self.dismiss.cancel();
                vec![DesktopEffect::HideMini]
            }
        }
    }

    fn show_effects(&self, kind: WindowKind) -> Vec<DesktopEffect> {
        match (kind, self.is_frontend_ready(kind)) {
            (WindowKind::Main, true) => {
                vec![DesktopEffect::UseRegularActivation, DesktopEffect::ShowMain]
            }
            (WindowKind::Main, false) => vec![DesktopEffect::EnsureMainSurface],
            (WindowKind::Mini, true) => vec![DesktopEffect::ShowMini],
            (WindowKind::Mini, false) => vec![DesktopEffect::EnsureMiniSurface],
        }
    }

    pub fn is_window_visible(&self, kind: WindowKind) -> bool {
        match kind {
            WindowKind::Main => self.main_visible,
            WindowKind::Mini => self.mini_visible,
        }
    }

    pub fn mini_pinned(&self) -> bool {
        self.mini_pinned
    }

    pub fn is_frontend_ready(&self, kind: WindowKind) -> bool {
        match kind {
            WindowKind::Main => self.main_ready,
            WindowKind::Mini => self.mini_ready,
        }
    }

    pub fn mini_dismiss_deadline(&self) -> Option<Instant> {
        self.dismiss.deadline()
    }

    /// Compatibility projection for the native menu and existing panel while both migrate to
    /// the typed DesktopSnapshot. Returns `None` until the first player snapshot arrives.
    pub fn status_projection(&self) -> Option<StatusSnapshot> {
        let player = self.snapshot.player.as_ref()?;
        let queue = self.snapshot.queue.as_ref();
        let track = player.track.as_ref();
        let settings = self.snapshot.settings.as_ref();
        let queue_len = queue.map_or(player.queue_len, |model| model.items.len());
        Some(StatusSnapshot {
            title: track.map(|track| {
                track
                    .display_title
                    .clone()
                    .unwrap_or_else(|| track.title.clone())
            }),
            artist: track.map(|track| {
                track
                    .display_artist
                    .clone()
                    .unwrap_or_else(|| track.artist.clone())
            }),
            paused: player.paused,
            volume: player.volume,
            position: if queue_len > 0 {
                player.queue_pos.saturating_add(1)
            } else {
                0
            },
            total: queue_len,
            streaming: player.streaming,
            owner_mode: player.owner_mode,
            settings: compact_settings(player, settings),
            queue: queue
                .map(|model| {
                    model
                        .items
                        .iter()
                        .enumerate()
                        .map(|(index, track)| QueueItemSnapshot {
                            title: track
                                .display_title
                                .clone()
                                .unwrap_or_else(|| track.title.clone()),
                            artist: track
                                .display_artist
                                .clone()
                                .unwrap_or_else(|| track.artist.clone()),
                            duration: format_duration(track.duration_ms),
                            current: index == player.queue_pos,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            shuffle: player.shuffle,
            repeat: player.repeat,
            elapsed_ms: player.elapsed_ms,
            duration_ms: player.duration_ms,
            is_live: track.is_some_and(|track| track.is_live),
            queue_rev: queue.map(|queue| queue.rev),
            track_id: track.map(|track| track.video_id.clone()),
            position_epoch: player.position_epoch,
            artwork: track.and_then(|track| track.artwork.clone()),
            // This compatibility projection is reconstructed from player/settings session topics,
            // which do not carry the one-shot owner's optional personal-sync snapshot.
            personal_sync: None,
        })
    }
}

fn compact_settings(player: &PlayerModel, settings: Option<&SettingsModelV8>) -> SettingsSnapshot {
    let mut compact = SettingsSnapshot {
        speed_tenths: player.speed_tenths,
        normalize: player.eq.normalize,
        radio_mode: player.radio_mode,
        autoplay_streaming: player.streaming,
        ..SettingsSnapshot::default()
    };
    if let Some(settings) = settings {
        compact.autoplay_streaming = settings.streaming.autoplay;
        compact.streaming_mode = match settings.streaming.mode.as_str() {
            "focused" => crate::streaming::StreamingMode::Focused,
            "discovery" => crate::streaming::StreamingMode::Discovery,
            _ => crate::streaming::StreamingMode::Balanced,
        };
        compact.streaming_source = settings.search.default_source;
        compact.speed_tenths = settings.playback.speed_tenths;
        compact.seek_seconds = settings.playback.seek_seconds;
        compact.normalize = settings.eq.normalize;
        compact.gapless = settings.playback.gapless;
        compact.ai_enabled = settings.streaming.ai_enabled;
    }
    compact
}

fn format_duration(duration_ms: Option<u64>) -> String {
    let Some(total_seconds) = duration_ms.map(|value| value / 1_000) else {
        return String::new();
    };
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::{EqModel, TrackModel};
    use crate::search_source::SearchSource;

    fn track(id: &str) -> TrackModel {
        TrackModel {
            video_id: id.to_string(),
            title: format!("Track {id}"),
            artist: "Artist".to_string(),
            album: None,
            duration_ms: Some(65_000),
            source: SearchSource::Youtube,
            is_local: false,
            downloaded: false,
            favorite: false,
            disliked: false,
            display_title: None,
            display_artist: None,
            artwork: None,
            watch_url: None,
            is_live: false,
        }
    }

    fn player(id: &str) -> PlayerModel {
        PlayerModel {
            track: Some(track(id)),
            paused: false,
            volume: 63,
            speed_tenths: 10,
            elapsed_ms: Some(1_000),
            duration_ms: Some(65_000),
            position_epoch: 1,
            shuffle: false,
            repeat: crate::queue::Repeat::Off,
            streaming: false,
            radio_mode: false,
            stream_now_playing: None,
            owner_mode: InstanceMode::StandaloneTui,
            eq: EqModel {
                preset: "flat".to_string(),
                bands: [0.0; 10],
                normalize: false,
            },
            queue_pos: 0,
            queue_len: 1,
        }
    }

    #[test]
    fn window_roles_match_the_product_contract() {
        let transient = WindowPolicy::for_window(WindowKind::Mini, false);
        assert!(!transient.show_in_taskbar);
        assert!(!transient.show_in_app_switcher);
        assert!(!transient.always_on_top);
        assert!(transient.dismiss_on_blur);

        let pinned = WindowPolicy::for_window(WindowKind::Mini, true);
        assert!(pinned.always_on_top);
        assert!(!pinned.dismiss_on_blur);

        let main = WindowPolicy::for_window(WindowKind::Main, false);
        assert!(main.show_in_taskbar);
        assert!(main.show_in_app_switcher);
        assert!(!main.always_on_top);
    }

    #[test]
    fn stale_sequences_never_overwrite_the_latest_snapshot() {
        let mut app = DesktopApp::default();
        assert!(app.apply_push(
            8,
            Topic::Player,
            PushEvent::PlayerSnapshot {
                model: Box::new(player("new")),
            },
        ));
        assert!(!app.apply_push(
            7,
            Topic::Player,
            PushEvent::PlayerSnapshot {
                model: Box::new(player("stale")),
            },
        ));
        assert_eq!(
            app.snapshot()
                .player
                .as_ref()
                .unwrap()
                .track
                .as_ref()
                .unwrap()
                .video_id,
            "new"
        );
        assert_eq!(app.snapshot().sequence, 8);
    }

    #[test]
    fn push_events_only_advance_their_declared_topic() {
        let owner = crate::app::App::new(50);
        let settings = crate::remote::publish::settings_model(&owner.core_view(), 1);
        let cases = vec![
            (
                Topic::Player,
                PushEvent::PlayerSnapshot {
                    model: Box::new(player("topic")),
                },
            ),
            (
                Topic::Queue,
                PushEvent::QueueSnapshot {
                    model: QueueModel {
                        rev: 1,
                        items: vec![track("topic")],
                    },
                },
            ),
            (
                Topic::Settings,
                PushEvent::SettingsSnapshot {
                    model: Box::new(settings),
                },
            ),
            (
                Topic::System,
                PushEvent::OwnerChanged {
                    mode: InstanceMode::Daemon,
                },
            ),
            (Topic::System, PushEvent::ShuttingDown),
            (
                Topic::Search,
                PushEvent::SearchCompleted {
                    ticket: 1,
                    page_id: None,
                    query: "topic".to_string(),
                    source: SearchSource::Youtube,
                    groups: Vec::new(),
                },
            ),
        ];

        for (expected_topic, event) in cases {
            for topic in Topic::ALL {
                let mut app = DesktopApp::default();
                let expected = topic == expected_topic;
                assert_eq!(
                    app.apply_push(1, topic, event.clone()),
                    expected,
                    "event {event:?} on topic {topic:?}"
                );
                assert_eq!(
                    app.snapshot().sequence,
                    u64::from(expected),
                    "event {event:?} on topic {topic:?}"
                );
            }
        }
    }

    #[test]
    fn focus_dismiss_uses_generation_and_a_fake_clock() {
        let start = Instant::now();
        let mut app = DesktopApp::default();
        let _ = app.handle_event_at(
            DesktopEvent::WindowVisibility {
                kind: WindowKind::Mini,
                visible: true,
            },
            start,
        );
        let _ = app.handle_event_at(
            DesktopEvent::WindowEvent(DesktopWindowEvent::Blurred(WindowKind::Mini)),
            start,
        );
        let first = app.mini_dismiss_deadline().unwrap();
        assert!(
            app.handle_event_at(DesktopEvent::Deadline, first - Duration::from_millis(1))
                .effects
                .is_empty()
        );
        let _ = app.handle_event_at(
            DesktopEvent::WindowEvent(DesktopWindowEvent::Focused(WindowKind::Mini)),
            first,
        );
        assert!(
            app.handle_event_at(DesktopEvent::Deadline, first)
                .effects
                .is_empty()
        );

        let second_start = start + Duration::from_secs(1);
        let _ = app.handle_event_at(
            DesktopEvent::WindowEvent(DesktopWindowEvent::Blurred(WindowKind::Mini)),
            second_start,
        );
        let second = app.mini_dismiss_deadline().unwrap();
        assert_eq!(
            app.handle_event_at(DesktopEvent::Deadline, second).effects,
            vec![DesktopEffect::HideMini]
        );
        assert!(!app.is_window_visible(WindowKind::Mini));

        let _ = app.handle_event_at(
            DesktopEvent::WindowVisibility {
                kind: WindowKind::Mini,
                visible: true,
            },
            second,
        );
        let _ = app.handle_event_at(DesktopEvent::MiniPinned(true), second);
        let _ = app.handle_event_at(
            DesktopEvent::WindowEvent(DesktopWindowEvent::Blurred(WindowKind::Mini)),
            second,
        );
        assert_eq!(app.mini_dismiss_deadline(), None);
    }

    #[test]
    fn frontend_ready_replays_the_latest_connection_and_snapshot() {
        let mut app = DesktopApp::default();
        app.set_connection(&ConnState::Online {
            protocol_version: 8,
            capabilities: vec!["events-v8".to_string()],
            owner_mode: InstanceMode::Daemon,
        });
        app.apply_push(
            3,
            Topic::Player,
            PushEvent::PlayerSnapshot {
                model: Box::new(player("ready")),
            },
        );
        let activation = app.handle_event(DesktopEvent::Activation(ActivationIntent::ShowMain));
        assert_eq!(
            activation.effects,
            vec![DesktopEffect::EnsureTray, DesktopEffect::EnsureMainSurface]
        );
        assert!(!activation.effects.contains(&DesktopEffect::ShowMain));
        let ready = app.handle_event(DesktopEvent::FrontendReady(WindowKind::Main));
        let replay = ready.replay.unwrap().1;
        assert!(app.is_frontend_ready(WindowKind::Main));
        assert_eq!(replay.snapshot.sequence, 3);
        assert!(matches!(
            replay.connection,
            DesktopConnection::Online { .. }
        ));
        assert_eq!(
            ready.effects,
            vec![DesktopEffect::UseRegularActivation, DesktopEffect::ShowMain]
        );

        let _ = app.handle_event(DesktopEvent::FrontendReady(WindowKind::Mini));
        let _ = app.handle_event(DesktopEvent::FrontendTornDown(WindowKind::Main));
        assert!(!app.is_frontend_ready(WindowKind::Main));
        assert!(app.is_frontend_ready(WindowKind::Mini));
    }

    #[test]
    fn identical_event_traces_produce_platform_independent_effects() {
        let start = Instant::now();
        let trace = vec![
            DesktopEvent::Activation(ActivationIntent::EnsureTray),
            DesktopEvent::Activation(ActivationIntent::ShowMini),
            DesktopEvent::FrontendReady(WindowKind::Mini),
            DesktopEvent::WindowEvent(DesktopWindowEvent::Focused(WindowKind::Mini)),
            DesktopEvent::WindowEvent(DesktopWindowEvent::Blurred(WindowKind::Mini)),
            DesktopEvent::MiniPinned(true),
            DesktopEvent::Deadline,
            DesktopEvent::MiniPinned(false),
            DesktopEvent::Deadline,
            DesktopEvent::Activation(ActivationIntent::ShowMain),
            DesktopEvent::FrontendReady(WindowKind::Main),
            DesktopEvent::WindowVisibility {
                kind: WindowKind::Main,
                visible: false,
            },
        ];
        let mut windows = DesktopApp::default();
        let mut macos = DesktopApp::default();
        for (index, event) in trace.into_iter().enumerate() {
            let now = start + Duration::from_millis(index as u64 * 400);
            let windows_transition = windows.handle_event_at(event.clone(), now);
            let macos_transition = macos.handle_event_at(event, now);
            assert_eq!(windows_transition, macos_transition, "event {index}");
            assert_eq!(windows, macos, "state after event {index}");
        }
        assert!(!windows.is_window_visible(WindowKind::Mini));
        assert!(!windows.is_window_visible(WindowKind::Main));
    }

    #[test]
    fn failed_hidden_surface_clears_visibility_intent_without_a_show() {
        let mut app = DesktopApp::default();
        let activation = app.handle_event(DesktopEvent::Activation(ActivationIntent::ShowMain));
        assert_eq!(
            activation.effects,
            vec![DesktopEffect::EnsureTray, DesktopEffect::EnsureMainSurface]
        );

        let correction = app.handle_event(DesktopEvent::WindowEvent(DesktopWindowEvent::Hidden(
            WindowKind::Main,
        )));
        assert_eq!(
            correction.effects,
            vec![DesktopEffect::UseAccessoryActivation]
        );
        assert!(!app.is_window_visible(WindowKind::Main));

        let ready = app.handle_event(DesktopEvent::FrontendReady(WindowKind::Main));
        assert!(ready.effects.is_empty());
        assert!(ready.replay.is_some());
    }

    #[test]
    fn mini_activation_ensures_hidden_surface_until_frontend_ready() {
        let mut app = DesktopApp::default();
        let activation = app.handle_event(DesktopEvent::Activation(ActivationIntent::ShowMini));
        assert_eq!(
            activation.effects,
            vec![DesktopEffect::EnsureTray, DesktopEffect::EnsureMiniSurface]
        );
        assert!(!activation.effects.contains(&DesktopEffect::ShowMini));

        let ready = app.handle_event(DesktopEvent::FrontendReady(WindowKind::Mini));
        assert_eq!(ready.effects, vec![DesktopEffect::ShowMini]);
        assert!(matches!(ready.replay, Some((WindowKind::Mini, _))));
    }

    #[test]
    fn typed_v8_projection_keeps_queue_cursor_and_duration() {
        let mut app = DesktopApp::default();
        app.apply_push(
            1,
            Topic::Queue,
            PushEvent::QueueSnapshot {
                model: QueueModel {
                    rev: 4,
                    items: vec![track("a")],
                },
            },
        );
        app.apply_push(
            2,
            Topic::Player,
            PushEvent::PlayerSnapshot {
                model: Box::new(player("a")),
            },
        );
        let status = app.status_projection().unwrap();
        assert_eq!(status.position, 1);
        assert_eq!(status.total, 1);
        assert_eq!(status.queue[0].duration, "1:05");
        assert!(status.queue[0].current);
        assert!(!status.is_live);
        assert_eq!(status.queue_rev, Some(4));
        assert_eq!(status.track_id.as_deref(), Some("a"));
        assert_eq!(status.position_epoch, 1);
    }

    #[test]
    fn typed_v8_projection_preserves_explicit_live_signal() {
        let mut app = DesktopApp::default();
        let mut unknown = player("loading");
        unknown.duration_ms = None;
        unknown.track.as_mut().unwrap().duration_ms = None;
        app.apply_push(
            1,
            Topic::Player,
            PushEvent::PlayerSnapshot {
                model: Box::new(unknown),
            },
        );
        let status = app.status_projection().unwrap();
        assert_eq!(status.duration_ms, None);
        assert!(!status.is_live);
        assert_eq!(status.track_id.as_deref(), Some("loading"));
        assert_eq!(status.position_epoch, 1);

        let mut live = player("station");
        live.duration_ms = None;
        let live_track = live.track.as_mut().unwrap();
        live_track.duration_ms = None;
        live_track.is_live = true;
        app.apply_push(
            2,
            Topic::Player,
            PushEvent::PlayerSnapshot {
                model: Box::new(live),
            },
        );
        let status = app.status_projection().unwrap();
        assert_eq!(status.duration_ms, None);
        assert!(status.is_live);
        assert_eq!(status.track_id.as_deref(), Some("station"));
        assert_eq!(status.position_epoch, 1);
    }
}
