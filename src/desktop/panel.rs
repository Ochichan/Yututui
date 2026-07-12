//! Shared model and WebView content for the desktop mini player panel.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::i18n::Language;
use crate::remote::proto::{RemoteCommand, RemoteSettingChange, ToggleState};
use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

use super::menu_model::MenuAction;
use super::status::PollUpdate;

#[path = "panel_payload.rs"]
mod payload;
pub(crate) use payload::track_identity;
pub use payload::{
    PanelOptionPayload, PanelPayload, PanelQueueItemPayload, PanelSettingsPayload,
    payload_for_update,
};
use payload::{control_error_label, language_code, payload_for_update_with_language};

/// A mini player skin. Presentation-only tray state: picked from the panel's own UI,
/// persisted in `desktop.json` (`DesktopState::mini_theme`), never sent to the core —
/// the remote protocol stays theme-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelTheme {
    Default,
    Minimal,
    Tamagotchi,
}

impl PanelTheme {
    pub const ALL: [PanelTheme; 3] = [
        PanelTheme::Default,
        PanelTheme::Minimal,
        PanelTheme::Tamagotchi,
    ];

    /// Stable snake_case id (same convention as the TUI's `ThemePreset::id`). Also
    /// spliced verbatim into the page's `data-theme` attribute, so ids must stay
    /// `[a-z_]` (a test guards this).
    pub fn id(self) -> &'static str {
        match self {
            PanelTheme::Default => "default",
            PanelTheme::Minimal => "minimal",
            PanelTheme::Tamagotchi => "tamagotchi",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.id() == id)
    }

    /// Logical window size, including the transparent margin the page reserves for
    /// its drop shadow (the same breathing room the 398×602 default always had).
    /// Only `Minimal` has an expanded (⋯) state; the host keeps the window's bottom
    /// edge anchored while it grows, so the extra controls unfold on-screen.
    pub fn window_size(self, expanded: bool) -> (f64, f64) {
        match (self, expanded) {
            (PanelTheme::Default, _) => (398.0, 602.0),
            (PanelTheme::Minimal, false) => (306.0, 90.0),
            (PanelTheme::Minimal, true) => (306.0, 276.0),
            (PanelTheme::Tamagotchi, _) => (290.0, 346.0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelCommand {
    PlayPause,
    Next,
    Previous,
    SeekBack,
    SeekForward,
    VolumeUp,
    VolumeDown,
    /// Absolute volume from the panel's volume bar (wheel / drag / click), `0..=100`.
    SetVolume(i64),
    /// Absolute seek from the panel's progress bar, in milliseconds.
    SeekTo(u64),
    ToggleShuffle,
    CycleRepeat,
    QueuePlay {
        position: usize,
        expected_rev: Option<u64>,
    },
    QueueRemove {
        position: usize,
        expected_rev: Option<u64>,
    },
    ToggleStreaming,
    SetStreaming(bool),
    SetStreamingMode(StreamingMode),
    SetStreamingSource(SearchSource),
    SetSpeed(u16),
    SetSeekSeconds(u16),
    SetNormalize(bool),
    SetGapless(bool),
    SetAiEnabled(bool),
    SetRadioMode(bool),
    StartDaemon,
    ResumeDaemon,
    StopDaemon,
    OpenTui,
    Refresh,
    Hide,
    /// Begin a native window drag (the frameless panel's header is the drag region).
    Drag,
    /// Switch the panel skin. Handled tray-locally: resize + persist, no socket traffic.
    SetTheme(PanelTheme),
    /// The minimal theme's ⋯ expansion toggled; the host resizes the window to match.
    SetExpanded(bool),
    /// Show the shared Queue/More sheet from compact skins. The host expands
    /// the native surface while the existing semantic panel is in use.
    SetSharedSheet(Option<PanelSheet>),
    /// Cache bounded, page-local focus and queue-scroll state across WebView teardown.
    PersistUi(PanelUiSnapshot),
    /// Keep the mini player visible on focus loss and above normal windows.
    /// Handled by the desktop shell and persisted locally, never sent to the core.
    SetPinned(bool),
    /// The page installed its handlers and is ready for the host's latest authoritative replay.
    FrontendReady,
    /// The browser failed to decode the currently supplied artwork data URI.
    ArtworkFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelSheet {
    Queue,
    More,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PanelUiSnapshot {
    pub queue_scroll_y: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_control: Option<String>,
}

impl PanelUiSnapshot {
    const MAX_QUEUE_SCROLL_Y: u32 = 10_000_000;
    const MAX_ACTIVE_CONTROL_BYTES: usize = 128;

    fn is_bounded(&self) -> bool {
        self.queue_scroll_y <= Self::MAX_QUEUE_SCROLL_Y
            && self
                .active_control
                .as_ref()
                .is_none_or(|id| id.len() <= Self::MAX_ACTIVE_CONTROL_BYTES)
    }
}

impl PanelCommand {
    pub fn menu_action(&self) -> Option<MenuAction> {
        match self {
            PanelCommand::PlayPause => Some(MenuAction::PlayPause),
            PanelCommand::Next => Some(MenuAction::Next),
            PanelCommand::Previous => Some(MenuAction::Previous),
            PanelCommand::SeekBack => Some(MenuAction::SeekBack),
            PanelCommand::SeekForward => Some(MenuAction::SeekForward),
            PanelCommand::VolumeUp => Some(MenuAction::VolumeUp),
            PanelCommand::VolumeDown => Some(MenuAction::VolumeDown),
            PanelCommand::ToggleStreaming => Some(MenuAction::ToggleStreaming),
            PanelCommand::ToggleShuffle
            | PanelCommand::CycleRepeat
            | PanelCommand::QueuePlay { .. }
            | PanelCommand::QueueRemove { .. }
            | PanelCommand::SetVolume(_)
            | PanelCommand::SeekTo(_)
            | PanelCommand::SetStreaming(_)
            | PanelCommand::SetStreamingMode(_)
            | PanelCommand::SetStreamingSource(_)
            | PanelCommand::SetSpeed(_)
            | PanelCommand::SetSeekSeconds(_)
            | PanelCommand::SetNormalize(_)
            | PanelCommand::SetGapless(_)
            | PanelCommand::SetAiEnabled(_)
            | PanelCommand::SetRadioMode(_) => None,
            PanelCommand::StartDaemon => Some(MenuAction::StartDaemon),
            PanelCommand::ResumeDaemon => Some(MenuAction::ResumeDaemon),
            PanelCommand::StopDaemon => Some(MenuAction::StopDaemon),
            PanelCommand::OpenTui => Some(MenuAction::OpenTui),
            PanelCommand::Refresh => Some(MenuAction::Refresh),
            PanelCommand::Hide
            | PanelCommand::Drag
            | PanelCommand::SetTheme(_)
            | PanelCommand::SetExpanded(_)
            | PanelCommand::SetSharedSheet(_)
            | PanelCommand::PersistUi(_)
            | PanelCommand::SetPinned(_)
            | PanelCommand::FrontendReady
            | PanelCommand::ArtworkFailed => None,
        }
    }

    pub fn remote_command(&self) -> Option<RemoteCommand> {
        match self {
            PanelCommand::SetStreaming(value) => Some(RemoteCommand::Streaming {
                state: if *value {
                    ToggleState::On
                } else {
                    ToggleState::Off
                },
            }),
            PanelCommand::SetStreamingMode(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::StreamingMode { value: *value },
            }),
            PanelCommand::SetStreamingSource(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::StreamingSource { value: *value },
            }),
            PanelCommand::SetSpeed(tenths) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::Speed { tenths: *tenths },
            }),
            PanelCommand::SetSeekSeconds(seconds) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::SeekSeconds { seconds: *seconds },
            }),
            PanelCommand::SetNormalize(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::Normalize { value: *value },
            }),
            PanelCommand::SetGapless(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::Gapless { value: *value },
            }),
            PanelCommand::SetAiEnabled(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::AiEnabled { value: *value },
            }),
            PanelCommand::SetRadioMode(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::RadioMode {
                    state: if *value {
                        ToggleState::On
                    } else {
                        ToggleState::Off
                    },
                },
            }),
            PanelCommand::ToggleShuffle => Some(RemoteCommand::ToggleShuffle),
            PanelCommand::CycleRepeat => Some(RemoteCommand::CycleRepeat),
            PanelCommand::QueuePlay {
                position,
                expected_rev: Some(expected_rev),
            } => Some(RemoteCommand::QueuePlayIfRevision {
                position: *position,
                expected_rev: *expected_rev,
            }),
            PanelCommand::QueuePlay {
                position,
                expected_rev: None,
            } => Some(RemoteCommand::QueuePlay {
                position: *position,
            }),
            PanelCommand::QueueRemove {
                position,
                expected_rev: Some(expected_rev),
            } => Some(RemoteCommand::QueueRemoveIfRevision {
                position: *position,
                expected_rev: *expected_rev,
            }),
            PanelCommand::QueueRemove {
                position,
                expected_rev: None,
            } => Some(RemoteCommand::QueueRemove {
                position: *position,
            }),
            PanelCommand::SetVolume(percent) => {
                Some(RemoteCommand::SetVolume { percent: *percent })
            }
            PanelCommand::SeekTo(ms) => Some(RemoteCommand::SeekTo { ms: *ms }),
            _ => self.menu_action().and_then(MenuAction::remote_command),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PanelIpcMessage {
    action: String,
    #[serde(default)]
    value: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PanelIpcV1 {
    v: u8,
    id: u64,
    command: PanelIpcMessage,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PanelIpcEnvelope {
    V1(PanelIpcV1),
    Legacy(PanelIpcMessage),
}

/// A versioned request from the panel. `id` is absent only for the legacy
/// boot fixtures retained while old embedded pages drain from development
/// builds; all current pages send v1 correlated requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelRequest {
    pub id: Option<u64>,
    pub command: PanelCommand,
}

pub fn parse_ipc_request(message: &str) -> Result<PanelRequest, serde_json::Error> {
    // IPC is local, but still crosses an untrusted WebView boundary. Keep malformed
    // or unexpectedly large messages out before allocating a serde tree.
    if message.len() > 4096 {
        return Err(serde::de::Error::custom("panel message is too large"));
    }
    match serde_json::from_str::<PanelIpcEnvelope>(message)? {
        PanelIpcEnvelope::V1(message) => {
            if message.v != 1 {
                return Err(serde::de::Error::custom("unsupported panel IPC version"));
            }
            if message.id == 0 {
                return Err(serde::de::Error::custom(
                    "panel request id must be non-zero",
                ));
            }
            Ok(PanelRequest {
                id: Some(message.id),
                command: parse_panel_command(message.command, true)?,
            })
        }
        PanelIpcEnvelope::Legacy(message) => Ok(PanelRequest {
            id: None,
            command: parse_panel_command(message, false)?,
        }),
    }
}

/// Compatibility helper used by existing parser tests and old callers.
pub fn parse_ipc_message(message: &str) -> Result<PanelCommand, serde_json::Error> {
    Ok(parse_ipc_request(message)?.command)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopCommandError {
    pub code: String,
    pub display_message: String,
    pub retryable: bool,
}

impl DesktopCommandError {
    pub fn new(
        code: impl Into<String>,
        display_message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            code: code.into(),
            display_message: display_message.into(),
            retryable,
        }
    }
}

pub fn command_error_from_control(error: &super::control::ControlError) -> DesktopCommandError {
    use super::control::ControlError;
    let (code, retryable) = match error {
        ControlError::NotRunning => ("offline", true),
        ControlError::StaleInstance => ("stale_instance", true),
        ControlError::Rejected(reason) => (reason.as_str(), reason == "stale_rev"),
        ControlError::MissingStatus => ("missing_status", true),
        ControlError::Transport(_) => ("transport", true),
    };
    DesktopCommandError::new(
        code,
        control_error_label(error, crate::i18n::current()),
        retryable,
    )
}

pub fn command_result_script(id: u64, error: Option<&DesktopCommandError>) -> String {
    let value = match error {
        Some(error) => serde_json::json!({ "id": id, "ok": false, "error": error }),
        None => serde_json::json!({ "id": id, "ok": true }),
    };
    format!(
        "window.ytmTuiCommandResult && window.ytmTuiCommandResult({});",
        json_for_script(&value)
    )
}

fn parse_panel_command(
    message: PanelIpcMessage,
    require_queue_revision: bool,
) -> Result<PanelCommand, serde_json::Error> {
    let value = message.value;
    let command = match message.action.as_str() {
        "play_pause" => PanelCommand::PlayPause,
        "next" => PanelCommand::Next,
        "previous" => PanelCommand::Previous,
        "seek_back" => PanelCommand::SeekBack,
        "seek_forward" => PanelCommand::SeekForward,
        "volume_up" => PanelCommand::VolumeUp,
        "volume_down" => PanelCommand::VolumeDown,
        "set_volume" => PanelCommand::SetVolume(i64::from(required_u16_in(value, 0, 100)?)),
        // Seven days is deliberately generous for long-form media while bounding
        // nonsensical values that cannot have originated from the panel slider.
        "seek_to" => PanelCommand::SeekTo(required_u64_in(value, 0, 7 * 24 * 60 * 60 * 1000)?),
        "toggle_shuffle" => PanelCommand::ToggleShuffle,
        "cycle_repeat" => PanelCommand::CycleRepeat,
        // The core queue hard cap is 999 items, hence valid zero-based positions
        // are 0..=998. Reject skewed/stale WebView values at the boundary.
        "queue_play" => {
            let value = required_queue_command(value, require_queue_revision)?;
            PanelCommand::QueuePlay {
                position: value.position,
                expected_rev: value.expected_rev,
            }
        }
        "queue_remove" => {
            let value = required_queue_command(value, require_queue_revision)?;
            PanelCommand::QueueRemove {
                position: value.position,
                expected_rev: value.expected_rev,
            }
        }
        "toggle_streaming" => PanelCommand::ToggleStreaming,
        "set_streaming" => PanelCommand::SetStreaming(required_bool(value)?),
        "set_streaming_mode" => {
            PanelCommand::SetStreamingMode(parse_streaming_mode(required_str(value)?)?)
        }
        "set_streaming_source" => {
            PanelCommand::SetStreamingSource(parse_search_source(required_str(value)?)?)
        }
        "set_speed" => PanelCommand::SetSpeed(required_u16_in(value, 5, 20)?),
        "set_seek_seconds" => PanelCommand::SetSeekSeconds(required_u16_in(value, 1, 60)?),
        "set_normalize" => PanelCommand::SetNormalize(required_bool(value)?),
        "set_gapless" => PanelCommand::SetGapless(required_bool(value)?),
        "set_ai_enabled" => PanelCommand::SetAiEnabled(required_bool(value)?),
        "set_radio_mode" => PanelCommand::SetRadioMode(required_bool(value)?),
        "start_daemon" => PanelCommand::StartDaemon,
        "resume_daemon" => PanelCommand::ResumeDaemon,
        "stop_daemon" => PanelCommand::StopDaemon,
        "open_tui" => PanelCommand::OpenTui,
        "refresh" => PanelCommand::Refresh,
        "hide" => PanelCommand::Hide,
        "drag" => PanelCommand::Drag,
        "set_theme" => PanelCommand::SetTheme(parse_panel_theme(required_str(value)?)?),
        "set_expanded" => PanelCommand::SetExpanded(required_bool(value)?),
        "set_shared_sheet" => PanelCommand::SetSharedSheet(required_panel_sheet(value)?),
        "persist_ui" => PanelCommand::PersistUi(required_panel_ui_snapshot(value)?),
        "set_pinned" => PanelCommand::SetPinned(required_bool(value)?),
        "frontend_ready" => PanelCommand::FrontendReady,
        "artwork_failed" => PanelCommand::ArtworkFailed,
        _ => return Err(serde::de::Error::custom("unknown panel action")),
    };
    Ok(command)
}

fn required_panel_ui_snapshot(value: Option<Value>) -> Result<PanelUiSnapshot, serde_json::Error> {
    let Some(value) = value else {
        return Err(serde::de::Error::custom("expected panel UI snapshot"));
    };
    let snapshot = serde_json::from_value::<PanelUiSnapshot>(value)?;
    if snapshot.is_bounded() {
        Ok(snapshot)
    } else {
        Err(serde::de::Error::custom(
            "panel UI snapshot is out of bounds",
        ))
    }
}

fn required_bool(value: Option<Value>) -> Result<bool, serde_json::Error> {
    match value {
        Some(Value::Bool(value)) => Ok(value),
        _ => Err(serde::de::Error::custom("expected boolean value")),
    }
}

fn required_panel_sheet(value: Option<Value>) -> Result<Option<PanelSheet>, serde_json::Error> {
    match value {
        Some(Value::Bool(false)) => Ok(None),
        // Compatibility with pages rendered before sheet identity became part of UiSnapshot.
        Some(Value::Bool(true)) => Ok(Some(PanelSheet::Queue)),
        Some(Value::String(value)) if value == "queue" => Ok(Some(PanelSheet::Queue)),
        Some(Value::String(value)) if value == "more" => Ok(Some(PanelSheet::More)),
        _ => Err(serde::de::Error::custom(
            "expected false, queue, or more for shared sheet",
        )),
    }
}

fn required_u16(value: Option<Value>) -> Result<u16, serde_json::Error> {
    match value {
        Some(Value::Number(value)) => value
            .as_u64()
            .and_then(|value| u16::try_from(value).ok())
            .ok_or_else(|| serde::de::Error::custom("expected u16 value")),
        _ => Err(serde::de::Error::custom("expected numeric value")),
    }
}

fn required_u16_in(value: Option<Value>, min: u16, max: u16) -> Result<u16, serde_json::Error> {
    let value = required_u16(value)?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(format!(
            "expected value in {min}..={max}"
        )))
    }
}

fn required_u64(value: Option<Value>) -> Result<u64, serde_json::Error> {
    match value {
        Some(Value::Number(value)) => value
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("expected u64 value")),
        _ => Err(serde::de::Error::custom("expected numeric value")),
    }
}

fn required_u64_in(value: Option<Value>, min: u64, max: u64) -> Result<u64, serde_json::Error> {
    let value = required_u64(value)?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(format!(
            "expected value in {min}..={max}"
        )))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PanelQueueCommandValue {
    position: usize,
    #[serde(default)]
    expected_rev: Option<u64>,
}

fn required_queue_command(
    value: Option<Value>,
    require_revision: bool,
) -> Result<PanelQueueCommandValue, serde_json::Error> {
    let value = value.ok_or_else(|| serde::de::Error::custom("expected queue command value"))?;
    let value: PanelQueueCommandValue = serde_json::from_value(value)?;
    if value.position > 998 {
        return Err(serde::de::Error::custom(
            "expected queue position in 0..=998",
        ));
    }
    if require_revision && value.expected_rev.is_none() {
        return Err(serde::de::Error::custom(
            "v1 queue commands require expectedRev",
        ));
    }
    Ok(value)
}

fn required_str(value: Option<Value>) -> Result<String, serde_json::Error> {
    match value {
        Some(Value::String(value)) => Ok(value),
        _ => Err(serde::de::Error::custom("expected string value")),
    }
}

fn parse_streaming_mode(value: String) -> Result<StreamingMode, serde_json::Error> {
    match value.as_str() {
        "focused" => Ok(StreamingMode::Focused),
        "balanced" => Ok(StreamingMode::Balanced),
        "discovery" => Ok(StreamingMode::Discovery),
        _ => Err(serde::de::Error::custom("unknown streaming mode")),
    }
}

fn parse_panel_theme(value: String) -> Result<PanelTheme, serde_json::Error> {
    PanelTheme::from_id(&value).ok_or_else(|| serde::de::Error::custom("unknown panel theme"))
}

fn parse_search_source(value: String) -> Result<SearchSource, serde_json::Error> {
    match value.as_str() {
        "youtube" => Ok(SearchSource::Youtube),
        "soundcloud" => Ok(SearchSource::SoundCloud),
        "audius" => Ok(SearchSource::Audius),
        "jamendo" => Ok(SearchSource::Jamendo),
        "internet_archive" => Ok(SearchSource::InternetArchive),
        // "radio_browser" is deliberately absent: live radio streams are not valid
        // autoplay/DJ Gem candidates (see SearchConfig::normalized_streaming_source).
        "all" => Ok(SearchSource::All),
        _ => Err(serde::de::Error::custom("unknown streaming source")),
    }
}

/// Render the page for a (re)build. `art_uri` is baked in rather than pushed after
/// load: `evaluate_script` against a still-loading page lands on the boot document
/// and vanishes, and the host's art memo would then suppress the natural 2s retry.
/// Theme ids are `[a-z_]` enum constants (guarded by a test), so splicing them into
/// the `data-theme` attribute needs no escaping.
pub fn html(initial: &PollUpdate, theme: PanelTheme, art_uri: Option<&str>) -> String {
    html_with_pinned(initial, theme, art_uri, false)
}

/// Render with the desktop shell's persisted pin state. Kept separate from
/// [`html`] so older call sites remain source-compatible while platform wiring
/// migrates to the explicit state.
pub fn html_with_pinned(
    initial: &PollUpdate,
    theme: PanelTheme,
    art_uri: Option<&str>,
    pinned: bool,
) -> String {
    html_with_state(initial, theme, art_uri, pinned, false, None)
}

/// Render a rebuilt panel with the small host-owned UI snapshot that must survive the
/// WebView idle grace. Theme, pinning, and authoritative player state remain separate host
/// inputs; only transient expansion/sheet state is restored here.
pub fn html_with_state(
    initial: &PollUpdate,
    theme: PanelTheme,
    art_uri: Option<&str>,
    pinned: bool,
    expanded: bool,
    shared_sheet: Option<PanelSheet>,
) -> String {
    html_with_panel_ui_state(
        initial,
        theme,
        art_uri,
        pinned,
        expanded,
        shared_sheet,
        &PanelUiSnapshot::default(),
    )
}

pub(crate) fn html_with_panel_ui_state(
    initial: &PollUpdate,
    theme: PanelTheme,
    art_uri: Option<&str>,
    pinned: bool,
    expanded: bool,
    shared_sheet: Option<PanelSheet>,
    ui_snapshot: &PanelUiSnapshot,
) -> String {
    html_with_language_and_ui(
        initial,
        theme,
        art_uri,
        pinned,
        expanded,
        shared_sheet,
        ui_snapshot,
        crate::i18n::current(),
    )
}

#[allow(clippy::too_many_arguments)]
fn html_with_language_and_ui(
    initial: &PollUpdate,
    theme: PanelTheme,
    art_uri: Option<&str>,
    pinned: bool,
    expanded: bool,
    shared_sheet: Option<PanelSheet>,
    ui_snapshot: &PanelUiSnapshot,
    language: Language,
) -> String {
    render_html(
        initial,
        theme,
        art_uri,
        pinned,
        expanded,
        shared_sheet,
        ui_snapshot,
        language,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_html(
    initial: &PollUpdate,
    theme: PanelTheme,
    art_uri: Option<&str>,
    pinned: bool,
    expanded: bool,
    shared_sheet: Option<PanelSheet>,
    ui_snapshot: &PanelUiSnapshot,
    language: Language,
) -> String {
    let nonce = csp_nonce();
    let art = match art_uri {
        Some(uri) => json_for_script(&uri),
        None => "null".to_string(),
    };
    PANEL_HTML
        .replace(
            "__INITIAL_PAYLOAD__",
            &json_for_script(&payload_for_update_with_language(initial, language)),
        )
        .replace("__PANEL_LANG__", language_code(language))
        .replace("__PANEL_LOCALE__", language_code(language))
        .replace("__PANEL_THEME__", theme.id())
        .replace("__INITIAL_ART__", &art)
        .replace("__INITIAL_PINNED__", if pinned { "true" } else { "false" })
        .replace(
            "__INITIAL_EXPANDED__",
            if expanded { "true" } else { "false" },
        )
        .replace(
            "__INITIAL_SHARED_SHEET__",
            match shared_sheet {
                Some(PanelSheet::Queue) => "\"queue\"",
                Some(PanelSheet::More) => "\"more\"",
                None => "null",
            },
        )
        .replace(
            "__INITIAL_QUEUE_SCROLL_Y__",
            &ui_snapshot.queue_scroll_y.to_string(),
        )
        .replace(
            "__INITIAL_ACTIVE_CONTROL__",
            &json_for_script(&ui_snapshot.active_control),
        )
        .replace("__CSP_NONCE__", &nonce)
}

#[cfg(test)]
fn html_with_language(
    initial: &PollUpdate,
    theme: PanelTheme,
    art_uri: Option<&str>,
    pinned: bool,
    expanded: bool,
    shared_sheet: Option<PanelSheet>,
    language: Language,
) -> String {
    html_with_language_and_ui(
        initial,
        theme,
        art_uri,
        pinned,
        expanded,
        shared_sheet,
        &PanelUiSnapshot::default(),
        language,
    )
}

fn csp_nonce() -> String {
    use base64::Engine;

    let mut bytes = [0_u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // Entropy failure should not make the tray unusable. This fallback remains
        // unique per render and process; the page also blocks every external origin.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |value| value.as_nanos());
        bytes[..4].copy_from_slice(&std::process::id().to_le_bytes());
        bytes[4..].copy_from_slice(&stamp.to_le_bytes()[..12]);
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn update_script(update: &PollUpdate) -> String {
    format!(
        "window.ytmTuiApply && window.ytmTuiApply({});",
        json_for_script(&payload_for_update(update))
    )
}

/// Cap for panel art files. The media-art cache stores small covers (≤512²,
/// tens of KB); anything bigger is likely not ours and not worth base64-ing
/// into the webview on every track change.
pub const MAX_PANEL_ART_BYTES: u64 = 2 * 1024 * 1024;

/// `data:<mime>;base64,…` for raw image bytes (mime sniffed from magic bytes).
pub fn art_data_uri(bytes: &[u8]) -> String {
    use base64::Engine;
    format!(
        "data:{};base64,{}",
        super::assets::sniff_image(bytes),
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

/// Read + encode an artwork cache file; `None` when missing, unreadable, or
/// oversized (best-effort — the panel just keeps its placeholder).
pub fn load_art_data_uri(path: &std::path::Path) -> Option<String> {
    // One bounded, symlink-rejecting read instead of metadata-then-read: closes the TOCTOU
    // window (the file can't grow between the size check and the read) and refuses to follow a
    // symlink out of the cache dir. Oversized/missing/unreadable all fall back to the placeholder.
    match crate::util::safe_fs::read_no_symlink_limited(path, MAX_PANEL_ART_BYTES) {
        Ok(bytes) if super::assets::sniff_image(&bytes).starts_with("image/") => {
            Some(art_data_uri(&bytes))
        }
        Ok(_) => {
            tracing::debug!(target: "ytt_tray", path = %path.display(), "panel art has an unsupported image format");
            None
        }
        Err(e) => {
            tracing::debug!(target: "ytt_tray", path = %path.display(), error = %e, "panel art unreadable or oversized");
            None
        }
    }
}

/// Push artwork into the live page, off the status-payload path: art is only
/// (re)sent when it changes, never on the 2s poll. `None` clears to placeholders.
pub fn art_script(data_uri: Option<&str>) -> String {
    match data_uri {
        Some(uri) => format!(
            "window.ytmTuiApplyArt && window.ytmTuiApplyArt({});",
            json_for_script(&uri)
        ),
        None => "window.ytmTuiApplyArt && window.ytmTuiApplyArt(null);".to_string(),
    }
}

fn json_for_script<T: Serialize>(value: &T) -> String {
    // The payload is spliced into a <script> block (html()) and into evaluate_script
    // (update_script()). Escaping every `<` as the JSON escape `<` neutralises HTML parser
    // re-entry sequences — `</script`, `<script`, and `<!--` alike — while staying
    // valid JSON/JS (a bare `</`-only rewrite left the latter two open). U+2028/29
    // are legal JSON but broke JS string literals before ES2019 webviews.
    serde_json::to_string(value)
        .expect("panel payload serialization should not fail")
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

// The panel ships as one nonce-protected, network-isolated document, but its source stays
// reviewable by concern. `concat!` assembles the exact inline document at compile time: no
// runtime filesystem access, package, or WebView network allowance is introduced.
const PANEL_HTML: &str = concat!(
    include_str!("panel_assets/document_start.html"),
    include_str!("panel_assets/common.css"),
    include_str!("panel_assets/cushion.css"),
    include_str!("panel_assets/shared.css"),
    include_str!("panel_assets/minimal.css"),
    include_str!("panel_assets/tamagotchi.css"),
    include_str!("panel_assets/accessibility.css"),
    include_str!("panel_assets/body_start.html"),
    include_str!("panel.html"),
    include_str!("panel_assets/script_start.html"),
    include_str!("panel_assets/ipc-state.js"),
    include_str!("panel_assets/document_end.html"),
);

#[cfg(test)]
#[path = "panel_tests.rs"]
mod tests;
