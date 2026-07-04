//! Shared model and WebView content for the desktop mini player panel.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::queue::Repeat;
use crate::remote::proto::{InstanceMode, RemoteCommand, RemoteSettingChange, ToggleState};
use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

use super::menu_model::{self, MenuAction, TrayState, TrayStateKind};
use super::status::PollUpdate;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    QueuePlay(usize),
    QueueRemove(usize),
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
}

impl PanelCommand {
    pub fn menu_action(self) -> Option<MenuAction> {
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
            | PanelCommand::QueuePlay(_)
            | PanelCommand::QueueRemove(_)
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
            | PanelCommand::SetExpanded(_) => None,
        }
    }

    pub fn remote_command(self) -> Option<RemoteCommand> {
        match self {
            PanelCommand::SetStreaming(value) => Some(RemoteCommand::Streaming {
                state: if value {
                    ToggleState::On
                } else {
                    ToggleState::Off
                },
            }),
            PanelCommand::SetStreamingMode(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::StreamingMode { value },
            }),
            PanelCommand::SetStreamingSource(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::StreamingSource { value },
            }),
            PanelCommand::SetSpeed(tenths) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::Speed { tenths },
            }),
            PanelCommand::SetSeekSeconds(seconds) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::SeekSeconds { seconds },
            }),
            PanelCommand::SetNormalize(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::Normalize { value },
            }),
            PanelCommand::SetGapless(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::Gapless { value },
            }),
            PanelCommand::SetAiEnabled(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::AiEnabled { value },
            }),
            PanelCommand::SetRadioMode(value) => Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::RadioMode {
                    state: if value {
                        ToggleState::On
                    } else {
                        ToggleState::Off
                    },
                },
            }),
            PanelCommand::ToggleShuffle => Some(RemoteCommand::ToggleShuffle),
            PanelCommand::CycleRepeat => Some(RemoteCommand::CycleRepeat),
            PanelCommand::QueuePlay(position) => Some(RemoteCommand::QueuePlay { position }),
            PanelCommand::QueueRemove(position) => Some(RemoteCommand::QueueRemove { position }),
            PanelCommand::SetVolume(percent) => Some(RemoteCommand::SetVolume { percent }),
            PanelCommand::SeekTo(ms) => Some(RemoteCommand::SeekTo { ms }),
            _ => self.menu_action().and_then(MenuAction::remote_command),
        }
    }
}

#[derive(Debug, Deserialize)]
struct PanelIpcMessage {
    action: String,
    #[serde(default)]
    value: Option<Value>,
}

pub fn parse_ipc_message(message: &str) -> Result<PanelCommand, serde_json::Error> {
    let message = serde_json::from_str::<PanelIpcMessage>(message)?;
    parse_panel_command(message)
}

fn parse_panel_command(message: PanelIpcMessage) -> Result<PanelCommand, serde_json::Error> {
    let value = message.value;
    let command = match message.action.as_str() {
        "play_pause" => PanelCommand::PlayPause,
        "next" => PanelCommand::Next,
        "previous" => PanelCommand::Previous,
        "seek_back" => PanelCommand::SeekBack,
        "seek_forward" => PanelCommand::SeekForward,
        "volume_up" => PanelCommand::VolumeUp,
        "volume_down" => PanelCommand::VolumeDown,
        "set_volume" => PanelCommand::SetVolume(i64::from(required_u16(value)?).min(100)),
        "seek_to" => PanelCommand::SeekTo(required_u64(value)?),
        "toggle_shuffle" => PanelCommand::ToggleShuffle,
        "cycle_repeat" => PanelCommand::CycleRepeat,
        "queue_play" => PanelCommand::QueuePlay(required_usize(value)?),
        "queue_remove" => PanelCommand::QueueRemove(required_usize(value)?),
        "toggle_streaming" => PanelCommand::ToggleStreaming,
        "set_streaming" => PanelCommand::SetStreaming(required_bool(value)?),
        "set_streaming_mode" => {
            PanelCommand::SetStreamingMode(parse_streaming_mode(required_str(value)?)?)
        }
        "set_streaming_source" => {
            PanelCommand::SetStreamingSource(parse_search_source(required_str(value)?)?)
        }
        "set_speed" => PanelCommand::SetSpeed(required_u16(value)?),
        "set_seek_seconds" => PanelCommand::SetSeekSeconds(required_u16(value)?),
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
        _ => return Err(serde::de::Error::custom("unknown panel action")),
    };
    Ok(command)
}

fn required_bool(value: Option<Value>) -> Result<bool, serde_json::Error> {
    match value {
        Some(Value::Bool(value)) => Ok(value),
        _ => Err(serde::de::Error::custom("expected boolean value")),
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

fn required_u64(value: Option<Value>) -> Result<u64, serde_json::Error> {
    match value {
        Some(Value::Number(value)) => value
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("expected u64 value")),
        _ => Err(serde::de::Error::custom("expected numeric value")),
    }
}

fn required_usize(value: Option<Value>) -> Result<usize, serde_json::Error> {
    match value {
        Some(Value::Number(value)) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| serde::de::Error::custom("expected usize value")),
        _ => Err(serde::de::Error::custom("expected numeric value")),
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelPayload {
    connected: bool,
    title: String,
    artist: String,
    state_label: String,
    owner_label: String,
    queue_label: String,
    volume_label: String,
    volume: i64,
    elapsed_ms: Option<u64>,
    duration_ms: Option<u64>,
    can_seek: bool,
    queue: Vec<PanelQueueItemPayload>,
    shuffle: bool,
    repeat: String,
    repeat_label: String,
    paused: bool,
    streaming: bool,
    settings: PanelSettingsPayload,
    error: Option<String>,
    can_playback: bool,
    can_volume: bool,
    can_manage_queue: bool,
    can_toggle_streaming: bool,
    can_start_daemon: bool,
    can_resume_daemon: bool,
    can_stop_daemon: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelSettingsPayload {
    autoplay_streaming: bool,
    streaming_mode: String,
    streaming_mode_label: String,
    streaming_source: String,
    streaming_source_label: String,
    streaming_sources: Vec<PanelOptionPayload>,
    speed_tenths: u16,
    speed_label: String,
    seek_seconds: u16,
    seek_label: String,
    normalize: bool,
    gapless: bool,
    ai_enabled: bool,
    radio_mode: bool,
    can_radio_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelOptionPayload {
    value: String,
    label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelQueueItemPayload {
    index: usize,
    title: String,
    artist: String,
    duration: String,
    current: bool,
}

pub fn payload_for_update(update: &PollUpdate) -> PanelPayload {
    let model = menu_model::build_menu(&update.state);
    let status = update.state.status();
    let connected = status.is_some();
    let idle = matches!(update.state.kind(), TrayStateKind::ConnectedIdle);
    let repeat = status.map(|status| status.repeat).unwrap_or_default();

    PanelPayload {
        connected,
        title: title_for_state(&update.state),
        artist: artist_for_state(&update.state),
        state_label: state_label(&update.state),
        owner_label: owner_label(&update.state),
        queue_label: queue_label(&update.state),
        volume_label: volume_label(&update.state),
        volume: status
            .map(|status| status.volume.clamp(0, 100))
            .unwrap_or(0),
        elapsed_ms: status.and_then(|status| status.elapsed_ms),
        duration_ms: status.and_then(|status| status.duration_ms),
        can_seek: action_enabled(&model, MenuAction::PlayPause)
            && status.is_some_and(|status| status.duration_ms.is_some()),
        queue: queue_payload(&update.state),
        shuffle: status.map(|status| status.shuffle).unwrap_or(false),
        repeat: repeat_id(repeat).to_string(),
        repeat_label: repeat_label(repeat).to_string(),
        paused: status.map(|status| status.paused).unwrap_or(true),
        streaming: status.map(|status| status.streaming).unwrap_or(false),
        settings: settings_payload(&update.state),
        error: update.error.as_ref().map(ToString::to_string),
        can_playback: action_enabled(&model, MenuAction::PlayPause),
        can_volume: action_enabled(&model, MenuAction::VolumeUp)
            || action_enabled(&model, MenuAction::VolumeDown),
        can_manage_queue: status.is_some_and(|status| !status.queue.is_empty()),
        can_toggle_streaming: action_enabled(&model, MenuAction::ToggleStreaming),
        can_start_daemon: action_enabled(&model, MenuAction::StartDaemon),
        // Trust the menu model: it already enables Resume for a disconnected player and
        // an idle daemon. Adding a blanket `|| idle` here used to light the button up for
        // an idle *standalone TUI*, where resume is always rejected (StandaloneOwner).
        can_resume_daemon: action_enabled(&model, MenuAction::ResumeDaemon)
            || (idle && status.is_some_and(|status| status.owner_mode == InstanceMode::Daemon)),
        can_stop_daemon: action_enabled(&model, MenuAction::StopDaemon),
    }
}

fn queue_payload(state: &TrayState) -> Vec<PanelQueueItemPayload> {
    state
        .status()
        .map(|status| {
            status
                .queue
                .iter()
                .enumerate()
                .map(|(index, item)| PanelQueueItemPayload {
                    index,
                    title: item.title.clone(),
                    artist: item.artist.clone(),
                    duration: item.duration.clone(),
                    current: item.current,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn settings_payload(state: &TrayState) -> PanelSettingsPayload {
    let status = state.status();
    let settings = status.map(|status| status.settings).unwrap_or_default();
    let source = settings.streaming_source;
    PanelSettingsPayload {
        autoplay_streaming: settings.autoplay_streaming,
        streaming_mode: streaming_mode_id(settings.streaming_mode).to_string(),
        streaming_mode_label: settings.streaming_mode.label().to_string(),
        streaming_source: search_source_id(source).to_string(),
        streaming_source_label: source.label().to_string(),
        streaming_sources: streaming_source_options(),
        speed_tenths: settings.speed_tenths,
        speed_label: format!("{:.1}x", f64::from(settings.speed_tenths) / 10.0),
        seek_seconds: settings.seek_seconds,
        seek_label: format!("{}s", settings.seek_seconds),
        normalize: settings.normalize,
        gapless: settings.gapless,
        ai_enabled: settings.ai_enabled,
        radio_mode: settings.radio_mode,
        can_radio_mode: status
            .is_some_and(|status| status.owner_mode == InstanceMode::StandaloneTui),
    }
}

fn streaming_source_options() -> Vec<PanelOptionPayload> {
    [
        SearchSource::Youtube,
        SearchSource::SoundCloud,
        SearchSource::Audius,
        SearchSource::Jamendo,
        SearchSource::InternetArchive,
        SearchSource::All,
    ]
    .into_iter()
    .map(|source| PanelOptionPayload {
        value: search_source_id(source).to_string(),
        label: source.label().to_string(),
    })
    .collect()
}

fn streaming_mode_id(mode: StreamingMode) -> &'static str {
    match mode {
        StreamingMode::Focused => "focused",
        StreamingMode::Balanced => "balanced",
        StreamingMode::Discovery => "discovery",
    }
}

fn search_source_id(source: SearchSource) -> &'static str {
    match source {
        SearchSource::Youtube => "youtube",
        SearchSource::SoundCloud => "soundcloud",
        SearchSource::Audius => "audius",
        SearchSource::Jamendo => "jamendo",
        SearchSource::InternetArchive => "internet_archive",
        SearchSource::RadioBrowser => "radio_browser",
        SearchSource::All => "all",
    }
}

fn repeat_id(repeat: Repeat) -> &'static str {
    match repeat {
        Repeat::Off => "off",
        Repeat::All => "all",
        Repeat::One => "one",
    }
}

fn repeat_label(repeat: Repeat) -> &'static str {
    match repeat {
        Repeat::Off => "Off",
        Repeat::All => "All",
        Repeat::One => "One",
    }
}

/// Render the page for a (re)build. `art_uri` is baked in rather than pushed after
/// load: `evaluate_script` against a still-loading page lands on the boot document
/// and vanishes, and the host's art memo would then suppress the natural 2s retry.
/// Theme ids are `[a-z_]` enum constants (guarded by a test), so splicing them into
/// the `data-theme` attribute needs no escaping.
pub fn html(initial: &PollUpdate, theme: PanelTheme, art_uri: Option<&str>) -> String {
    let art = match art_uri {
        Some(uri) => json_for_script(&uri),
        None => "null".to_string(),
    };
    PANEL_HTML
        .replace(
            "__INITIAL_PAYLOAD__",
            &json_for_script(&payload_for_update(initial)),
        )
        .replace("__PANEL_THEME__", theme.id())
        .replace("__INITIAL_ART__", &art)
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
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() <= MAX_PANEL_ART_BYTES => {}
        Ok(meta) => {
            tracing::debug!(target: "ytt_tray", path = %path.display(), len = meta.len(), "panel art too large");
            return None;
        }
        Err(e) => {
            tracing::debug!(target: "ytt_tray", path = %path.display(), error = %e, "panel art unreadable");
            return None;
        }
    }
    match std::fs::read(path) {
        Ok(bytes) => Some(art_data_uri(&bytes)),
        Err(e) => {
            tracing::debug!(target: "ytt_tray", path = %path.display(), error = %e, "panel art unreadable");
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

fn action_enabled(model: &menu_model::MenuModel, action: MenuAction) -> bool {
    model
        .action_item(action)
        .map(|item| item.enabled)
        .unwrap_or(false)
}

fn title_for_state(state: &TrayState) -> String {
    match state.status().and_then(|status| status.title.as_deref()) {
        Some(title) if !title.is_empty() => title.to_string(),
        _ if matches!(state, TrayState::Disconnected) => "ytm-tui is not running".to_string(),
        _ => "Nothing playing".to_string(),
    }
}

fn artist_for_state(state: &TrayState) -> String {
    match state.status().and_then(|status| status.artist.as_deref()) {
        Some(artist) if !artist.is_empty() => artist.to_string(),
        _ => "YPlayer".to_string(),
    }
}

fn state_label(state: &TrayState) -> String {
    match state.kind() {
        TrayStateKind::ConnectedPlaying => "Playing".to_string(),
        TrayStateKind::ConnectedPaused => "Paused".to_string(),
        TrayStateKind::ConnectedIdle => "Idle".to_string(),
        TrayStateKind::Disconnected => "Disconnected".to_string(),
    }
}

fn owner_label(state: &TrayState) -> String {
    match state.status().map(|status| status.owner_mode) {
        Some(InstanceMode::Daemon) => "Daemon".to_string(),
        Some(InstanceMode::StandaloneTui) => "Standalone TUI".to_string(),
        None => "Offline".to_string(),
    }
}

fn queue_label(state: &TrayState) -> String {
    let Some(status) = state.status() else {
        return "Queue unavailable".to_string();
    };
    if status.total == 0 {
        "Queue empty".to_string()
    } else {
        format!("{} / {}", status.position, status.total)
    }
}

fn volume_label(state: &TrayState) -> String {
    state
        .status()
        .map(|status| format!("{}%", status.volume))
        .unwrap_or_else(|| "--".to_string())
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

const PANEL_HTML: &str = include_str!("panel.html");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::control::ControlError;
    use crate::remote::proto::{InstanceMode, QueueItemSnapshot, StatusSnapshot};

    fn playing_update() -> PollUpdate {
        PollUpdate::connected(StatusSnapshot {
            title: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            paused: false,
            volume: 80,
            position: 1,
            total: 2,
            streaming: true,
            owner_mode: InstanceMode::StandaloneTui,
            settings: Default::default(),
            queue: vec![
                QueueItemSnapshot {
                    title: "Song".to_string(),
                    artist: "Artist".to_string(),
                    duration: "3:00".to_string(),
                    current: true,
                },
                QueueItemSnapshot {
                    title: "Next".to_string(),
                    artist: "Other".to_string(),
                    duration: "4:00".to_string(),
                    current: false,
                },
            ],
            shuffle: true,
            repeat: Repeat::All,
            elapsed_ms: Some(42_000),
            duration_ms: Some(180_000),
            artwork: None,
        })
    }

    #[test]
    fn playing_payload_enables_transport_and_volume() {
        let payload = payload_for_update(&playing_update());
        assert!(payload.connected);
        assert_eq!(payload.title, "Song");
        assert_eq!(payload.artist, "Artist");
        assert_eq!(payload.state_label, "Playing");
        assert_eq!(payload.owner_label, "Standalone TUI");
        assert_eq!(payload.queue_label, "1 / 2");
        assert_eq!(payload.volume_label, "80%");
        assert_eq!(payload.volume, 80);
        assert_eq!(payload.elapsed_ms, Some(42_000));
        assert_eq!(payload.duration_ms, Some(180_000));
        assert!(payload.can_seek);
        assert_eq!(payload.queue.len(), 2);
        assert_eq!(payload.queue[0].title, "Song");
        assert!(payload.shuffle);
        assert_eq!(payload.repeat, "all");
        assert_eq!(payload.repeat_label, "All");
        assert!(payload.can_playback);
        assert!(payload.can_volume);
        assert!(payload.can_manage_queue);
        assert!(payload.can_toggle_streaming);
        assert!(payload.settings.can_radio_mode);
        assert_eq!(payload.settings.streaming_mode, "balanced");
        assert_eq!(payload.settings.streaming_source, "youtube");
        assert!(!payload.can_start_daemon);
        assert!(!payload.can_resume_daemon);
        assert!(!payload.can_stop_daemon);
    }

    #[test]
    fn disconnected_payload_enables_daemon_start_and_resume() {
        let update = PollUpdate::disconnected(ControlError::NotRunning);
        let payload = payload_for_update(&update);
        assert!(!payload.connected);
        assert_eq!(payload.title, "ytm-tui is not running");
        assert_eq!(payload.owner_label, "Offline");
        assert_eq!(payload.queue_label, "Queue unavailable");
        assert!(!payload.can_playback);
        assert!(!payload.can_volume);
        assert!(!payload.can_seek);
        assert_eq!(payload.volume, 0);
        assert_eq!(payload.elapsed_ms, None);
        assert!(payload.can_start_daemon);
        assert!(payload.can_resume_daemon);
        assert!(!payload.can_stop_daemon);
        assert_eq!(payload.error, Some("ytm-tui is not running".to_string()));
    }

    #[test]
    fn idle_daemon_payload_enables_resume_and_stop() {
        let update = PollUpdate::connected(StatusSnapshot {
            title: None,
            artist: None,
            paused: true,
            volume: 70,
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
        let payload = payload_for_update(&update);
        assert_eq!(payload.title, "Nothing playing");
        assert_eq!(payload.state_label, "Idle");
        assert_eq!(payload.owner_label, "Daemon");
        assert!(!payload.can_playback);
        assert!(payload.can_volume);
        assert!(!payload.can_start_daemon);
        assert!(payload.can_resume_daemon);
        assert!(payload.can_stop_daemon);
        assert!(!payload.can_manage_queue);
        assert!(!payload.settings.can_radio_mode);
    }

    #[test]
    fn ipc_message_parses_panel_command() {
        assert_eq!(
            parse_ipc_message(r#"{"action":"play_pause"}"#).unwrap(),
            PanelCommand::PlayPause
        );
        assert_eq!(
            PanelCommand::PlayPause.menu_action(),
            Some(MenuAction::PlayPause)
        );
        assert_eq!(PanelCommand::Hide.menu_action(), None);
    }

    #[test]
    fn panel_setting_commands_map_to_remote_commands() {
        assert_eq!(
            PanelCommand::SetStreamingMode(StreamingMode::Focused).remote_command(),
            Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::StreamingMode {
                    value: StreamingMode::Focused
                }
            })
        );
        assert_eq!(
            PanelCommand::SetRadioMode(true).remote_command(),
            Some(RemoteCommand::SetSetting {
                change: RemoteSettingChange::RadioMode {
                    state: ToggleState::On
                }
            })
        );
        assert_eq!(
            PanelCommand::QueuePlay(2).remote_command(),
            Some(RemoteCommand::QueuePlay { position: 2 })
        );
        assert_eq!(
            PanelCommand::QueueRemove(1).remote_command(),
            Some(RemoteCommand::QueueRemove { position: 1 })
        );
    }

    #[test]
    fn ipc_message_parses_setting_commands() {
        assert_eq!(
            parse_ipc_message(r#"{"action":"set_streaming","value":true}"#).unwrap(),
            PanelCommand::SetStreaming(true)
        );
        assert_eq!(
            parse_ipc_message(r#"{"action":"set_streaming_mode","value":"discovery"}"#).unwrap(),
            PanelCommand::SetStreamingMode(StreamingMode::Discovery)
        );
        assert_eq!(
            parse_ipc_message(r#"{"action":"set_streaming_source","value":"jamendo"}"#).unwrap(),
            PanelCommand::SetStreamingSource(SearchSource::Jamendo)
        );
        assert_eq!(
            parse_ipc_message(r#"{"action":"set_speed","value":12}"#).unwrap(),
            PanelCommand::SetSpeed(12)
        );
        assert_eq!(
            parse_ipc_message(r#"{"action":"toggle_shuffle"}"#).unwrap(),
            PanelCommand::ToggleShuffle
        );
        assert_eq!(
            parse_ipc_message(r#"{"action":"cycle_repeat"}"#).unwrap(),
            PanelCommand::CycleRepeat
        );
        assert_eq!(
            parse_ipc_message(r#"{"action":"queue_play","value":2}"#).unwrap(),
            PanelCommand::QueuePlay(2)
        );
        assert_eq!(
            parse_ipc_message(r#"{"action":"queue_remove","value":1}"#).unwrap(),
            PanelCommand::QueueRemove(1)
        );
    }

    #[test]
    fn ipc_message_parses_volume_seek_and_drag() {
        assert_eq!(
            parse_ipc_message(r#"{"action":"set_volume","value":63}"#).unwrap(),
            PanelCommand::SetVolume(63)
        );
        // The panel clamps out-of-range wheel values defensively on the Rust side too.
        assert_eq!(
            parse_ipc_message(r#"{"action":"set_volume","value":100}"#).unwrap(),
            PanelCommand::SetVolume(100)
        );
        assert!(parse_ipc_message(r#"{"action":"set_volume","value":-4}"#).is_err());
        assert_eq!(
            parse_ipc_message(r#"{"action":"seek_to","value":91500}"#).unwrap(),
            PanelCommand::SeekTo(91_500)
        );
        assert_eq!(
            parse_ipc_message(r#"{"action":"drag"}"#).unwrap(),
            PanelCommand::Drag
        );

        assert_eq!(
            PanelCommand::SetVolume(63).remote_command(),
            Some(RemoteCommand::SetVolume { percent: 63 })
        );
        assert_eq!(
            PanelCommand::SeekTo(91_500).remote_command(),
            Some(RemoteCommand::SeekTo { ms: 91_500 })
        );
        assert_eq!(PanelCommand::Drag.remote_command(), None);
        assert_eq!(PanelCommand::Drag.menu_action(), None);
    }

    #[test]
    fn scripts_escape_html_script_endings() {
        let mut update = playing_update();
        if let TrayState::Connected(status) = &mut update.state {
            status.title = Some("</script><script>alert(1)</script><!--".to_string());
        }
        let html = html(&update, PanelTheme::Default, None);
        assert!(html.contains(r"\u003c/script>\u003cscript>alert(1)"));
        assert!(html.contains(r"\u003c!--"));
        assert!(!html.contains("</script><script>alert"));

        let script = update_script(&update);
        assert!(script.contains(r"\u003c/script>\u003cscript>alert(1)"));
        assert!(script.contains(r"\u003c!--"));
        assert!(!script.contains("</script><script>alert"));
    }

    #[test]
    fn panel_html_has_exactly_one_payload_slot() {
        assert_eq!(PANEL_HTML.matches("__INITIAL_PAYLOAD__").count(), 1);
    }

    #[test]
    fn idle_standalone_payload_cannot_resume() {
        let mut update = playing_update();
        if let TrayState::Connected(status) = &mut update.state {
            status.title = None;
            status.artist = None;
            status.total = 0;
            status.queue.clear();
        }
        let payload = payload_for_update(&update);
        assert_eq!(payload.state_label, "Idle");
        assert!(
            !payload.can_resume_daemon,
            "resume against a standalone TUI always dead-ends"
        );
        assert!(!payload.can_start_daemon);
    }

    #[test]
    fn panel_html_exposes_mode_switch_and_bars() {
        // The Music/Radio switch and both interactive bars must survive redesigns.
        assert!(PANEL_HTML.contains("data-mode=\"music\""));
        assert!(PANEL_HTML.contains("data-mode=\"radio\""));
        assert!(PANEL_HTML.contains("data-action=\"set_radio_mode\""));
        assert!(PANEL_HTML.contains("id=\"volumeBar\""));
        assert!(PANEL_HTML.contains("id=\"progressBar\""));
        assert!(PANEL_HTML.contains("addEventListener(\"wheel\""));
        assert!(PANEL_HTML.contains("\"set_volume\""));
        assert!(PANEL_HTML.contains("\"seek_to\""));
        assert!(PANEL_HTML.contains("send(\"drag\")"));
    }

    #[test]
    fn panel_html_exposes_queue_and_play_modes() {
        assert!(PANEL_HTML.contains("data-tab=\"queue\""));
        assert!(PANEL_HTML.contains("data-action=\"toggle_shuffle\""));
        assert!(PANEL_HTML.contains("data-action=\"cycle_repeat\""));
        assert!(PANEL_HTML.contains("data-action=\"queue_play\""));
        assert!(PANEL_HTML.contains("data-action=\"queue_remove\""));
    }

    #[test]
    fn theme_ids_round_trip() {
        for theme in PanelTheme::ALL {
            assert_eq!(PanelTheme::from_id(theme.id()), Some(theme));
        }
        assert_eq!(PanelTheme::from_id("bogus"), None);
    }

    #[test]
    fn theme_ids_are_substitution_safe() {
        // Ids are spliced verbatim into the page's data-theme attribute.
        for theme in PanelTheme::ALL {
            assert!(
                theme
                    .id()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_'),
                "{} is not attribute-safe",
                theme.id()
            );
        }
    }

    #[test]
    fn window_size_expands_only_minimal() {
        for theme in PanelTheme::ALL {
            let collapsed = theme.window_size(false);
            let expanded = theme.window_size(true);
            if theme == PanelTheme::Minimal {
                assert!(expanded.1 > collapsed.1);
                assert_eq!(expanded.0, collapsed.0, "expansion only grows downward");
            } else {
                assert_eq!(expanded, collapsed);
            }
        }
    }

    #[test]
    fn ipc_message_parses_theme_commands() {
        assert_eq!(
            parse_ipc_message(r#"{"action":"set_theme","value":"minimal"}"#).unwrap(),
            PanelCommand::SetTheme(PanelTheme::Minimal)
        );
        assert_eq!(
            parse_ipc_message(r#"{"action":"set_theme","value":"tamagotchi"}"#).unwrap(),
            PanelCommand::SetTheme(PanelTheme::Tamagotchi)
        );
        assert!(parse_ipc_message(r#"{"action":"set_theme","value":"bogus"}"#).is_err());
        assert!(parse_ipc_message(r#"{"action":"set_theme"}"#).is_err());
        assert_eq!(
            parse_ipc_message(r#"{"action":"set_expanded","value":true}"#).unwrap(),
            PanelCommand::SetExpanded(true)
        );
        assert!(parse_ipc_message(r#"{"action":"set_expanded","value":"yes"}"#).is_err());
    }

    #[test]
    fn theme_commands_stay_tray_local() {
        // Skin changes must never produce socket traffic or menu actions.
        for command in [
            PanelCommand::SetTheme(PanelTheme::Minimal),
            PanelCommand::SetExpanded(true),
        ] {
            assert_eq!(command.remote_command(), None);
            assert_eq!(command.menu_action(), None);
        }
    }

    #[test]
    fn panel_html_exposes_theme_switching() {
        // The CSS switch rules and the shared picker must survive redesigns.
        assert!(PANEL_HTML.contains(r#"html[data-theme="minimal"]"#));
        assert!(PANEL_HTML.contains(r#"html[data-theme="tamagotchi"]"#));
        assert!(PANEL_HTML.contains(r#"data-action="set_theme""#));
        for theme in PanelTheme::ALL {
            assert!(
                PANEL_HTML.contains(&format!(r#"data-value="{}""#, theme.id())),
                "picker misses {}",
                theme.id()
            );
        }
    }

    #[test]
    fn panel_html_exposes_minimal_theme_controls() {
        assert!(PANEL_HTML.contains(r#"id="minimalRoot""#));
        assert!(PANEL_HTML.contains(r#"id="mnCapsule""#));
        assert!(PANEL_HTML.contains(r#"id="mnExpand""#));
        assert!(PANEL_HTML.contains(r#"id="mnMore""#));
        assert!(PANEL_HTML.contains(r#"id="mnVolBar""#));
        assert!(PANEL_HTML.contains(r#"id="mnSeekBar""#));
        assert!(PANEL_HTML.contains(r#""set_expanded""#));
    }

    #[test]
    fn panel_html_has_exactly_one_theme_slot() {
        assert_eq!(PANEL_HTML.matches("__PANEL_THEME__").count(), 1);
        assert!(PANEL_HTML.contains(r#"data-theme="__PANEL_THEME__""#));
    }

    #[test]
    fn html_bakes_the_selected_theme() {
        let page = html(&playing_update(), PanelTheme::Minimal, None);
        assert!(page.contains(r#"data-theme="minimal""#));
        assert!(!page.contains("__PANEL_THEME__"));
    }

    #[test]
    fn panel_html_exposes_tamagotchi_pet_and_screen() {
        assert!(PANEL_HTML.contains(r#"id="tamaRoot""#));
        assert!(PANEL_HTML.contains(r#"id="tmScreen""#));
        assert!(PANEL_HTML.contains(r#"id="tmVolBar""#));
        assert!(PANEL_HTML.contains(r#"id="tmSeekBar""#));
        // The pet state machine and its LCD look must survive redesigns.
        assert!(PANEL_HTML.contains(r#"[data-pet="dance"]"#));
        assert!(PANEL_HTML.contains(r#"[data-pet="sleep"]"#));
        assert!(PANEL_HTML.contains(r#"[data-pet="off"]"#));
        assert!(PANEL_HTML.contains(r#"shape-rendering="crispEdges""#));
        assert!(PANEL_HTML.contains("image-rendering: pixelated"));
    }

    #[test]
    fn panel_html_has_exactly_one_art_slot() {
        assert_eq!(PANEL_HTML.matches("__INITIAL_ART__").count(), 1);
        assert!(PANEL_HTML.contains("ytmTuiApplyArt"));
    }

    #[test]
    fn html_bakes_initial_art() {
        let uri = "data:image/png;base64,iVBORw0KGgo=";
        let page = html(&playing_update(), PanelTheme::Minimal, Some(uri));
        assert!(page.contains(&format!("window.__YTM_TUI_INITIAL_ART__ = \"{uri}\";")));

        let artless = html(&playing_update(), PanelTheme::Minimal, None);
        assert!(artless.contains("window.__YTM_TUI_INITIAL_ART__ = null;"));
        assert!(!artless.contains("__INITIAL_ART__"));
    }

    #[test]
    fn art_data_uri_sniffs_common_formats() {
        assert!(art_data_uri(&[0xFF, 0xD8, 0xFF, 0x00]).starts_with("data:image/jpeg;base64,"));
        assert!(art_data_uri(b"\x89PNG\r\n\x1a\n").starts_with("data:image/png;base64,"));
        assert!(
            art_data_uri(b"RIFF\x00\x00\x00\x00WEBPVP8 ").starts_with("data:image/webp;base64,")
        );
        assert!(art_data_uri(b"not an image").starts_with("data:application/octet-stream;base64,"));
    }

    #[test]
    fn art_script_splices_or_clears() {
        assert_eq!(
            art_script(None),
            "window.ytmTuiApplyArt && window.ytmTuiApplyArt(null);"
        );
        let script = art_script(Some("data:image/png;base64,AA=="));
        assert_eq!(
            script,
            "window.ytmTuiApplyArt && window.ytmTuiApplyArt(\"data:image/png;base64,AA==\");"
        );
    }

    #[test]
    fn load_art_data_uri_handles_missing_and_oversized() {
        let dir = std::env::temp_dir();
        assert_eq!(
            load_art_data_uri(&dir.join("ytt-panel-art-missing.bin")),
            None
        );

        let ok_path = dir.join(format!("ytt-panel-art-ok-{}.bin", std::process::id()));
        std::fs::write(&ok_path, [0xFF, 0xD8, 0xFF, 0x00]).unwrap();
        let uri = load_art_data_uri(&ok_path);
        std::fs::remove_file(&ok_path).ok();
        assert!(uri.unwrap().starts_with("data:image/jpeg;base64,"));

        let big_path = dir.join(format!("ytt-panel-art-big-{}.bin", std::process::id()));
        std::fs::write(&big_path, vec![0u8; (MAX_PANEL_ART_BYTES + 1) as usize]).unwrap();
        let rejected = load_art_data_uri(&big_path);
        std::fs::remove_file(&big_path).ok();
        assert_eq!(rejected, None);
    }
}
