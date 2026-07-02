//! Shared model and WebView content for the desktop mini player panel.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::queue::Repeat;
use crate::remote::proto::{InstanceMode, RemoteCommand, RemoteSettingChange, ToggleState};
use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

use super::menu_model::{self, MenuAction, TrayState, TrayStateKind};
use super::status::PollUpdate;

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
            PanelCommand::Hide | PanelCommand::Drag => None,
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

pub fn html(initial: &PollUpdate) -> String {
    PANEL_HTML.replace(
        "__INITIAL_PAYLOAD__",
        &json_for_script(&payload_for_update(initial)),
    )
}

pub fn update_script(update: &PollUpdate) -> String {
    format!(
        "window.ytmTuiApply && window.ytmTuiApply({});",
        json_for_script(&payload_for_update(update))
    )
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
        _ => "YtmTui".to_string(),
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

const PANEL_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <style>
    /* Catppuccin Macchiato — official palette (github.com/catppuccin/catppuccin) */
    :root {
      color-scheme: dark;
      --rosewater: #f4dbd6;
      --flamingo: #f0c6c6;
      --pink: #f5bde6;
      --mauve: #c6a0f6;
      --red: #ed8796;
      --maroon: #ee99a0;
      --peach: #f5a97f;
      --yellow: #eed49f;
      --green: #a6da95;
      --teal: #8bd5ca;
      --sky: #91d7e3;
      --sapphire: #7dc4e4;
      --blue: #8aadf4;
      --lavender: #b7bdf8;
      --text: #cad3f5;
      --subtext1: #b8c0e0;
      --subtext0: #a5adcb;
      --overlay2: #939ab7;
      --overlay1: #8087a2;
      --overlay0: #6e738d;
      --surface2: #5b6078;
      --surface1: #494d64;
      --surface0: #363a4f;
      --base: #24273a;
      --mantle: #1e2030;
      --crust: #181926;
    }

    * {
      box-sizing: border-box;
      -webkit-tap-highlight-color: transparent;
    }

    html,
    body {
      width: 100%;
      height: 100%;
      margin: 0;
      overflow: hidden;
      background: transparent;
      color: var(--text);
      font-family: ui-rounded, "SF Pro Rounded", -apple-system, "Segoe UI Variable Text",
        "Segoe UI", system-ui, sans-serif;
      font-size: 12px;
      user-select: none;
      cursor: default;
    }

    body {
      padding: 13px;
    }

    /* The whole player is one soft rounded cushion floating on a transparent window. */
    .cushion {
      display: grid;
      grid-template-rows: auto auto auto minmax(0, 1fr) auto auto;
      gap: 9px;
      width: 100%;
      height: 100%;
      padding: 13px 13px 11px;
      border: 1.5px solid rgba(91, 96, 120, 0.6);
      border-radius: 26px;
      background:
        radial-gradient(130% 80% at 50% 0%, rgba(198, 160, 246, 0.14) 0%, rgba(198, 160, 246, 0) 46%),
        linear-gradient(180deg, #262a40 0%, var(--base) 42%, var(--mantle) 100%);
      box-shadow:
        0 18px 44px rgba(12, 12, 20, 0.55),
        0 2px 10px rgba(12, 12, 20, 0.35);
      overflow: hidden;
    }

    /* ---------- header ---------- */

    header {
      display: flex;
      align-items: center;
      gap: 8px;
      min-width: 0;
      padding: 1px 2px;
      cursor: grab;
    }

    header:active {
      cursor: grabbing;
    }

    .brand {
      display: inline-flex;
      flex: none;
      align-items: center;
      gap: 7px;
      color: var(--lavender);
      font-size: 13.5px;
      font-weight: 800;
      letter-spacing: 0.2px;
      white-space: nowrap;
    }

    .brand .cat {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      height: 22px;
      padding: 0 9px;
      border-radius: 999px;
      background: linear-gradient(135deg, rgba(198, 160, 246, 0.28), rgba(245, 189, 230, 0.24));
      color: var(--pink);
      font-size: 10px;
      font-weight: 800;
      letter-spacing: 0.5px;
    }

    .chip {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      min-width: 0;
      max-width: 100%;
      margin-left: auto;
      overflow: hidden;
      padding: 4px 11px;
      border: 1.5px solid var(--surface1);
      border-radius: 999px;
      background: var(--mantle);
      color: var(--subtext1);
      font-size: 11px;
      font-weight: 750;
      white-space: nowrap;
      text-overflow: ellipsis;
    }

    .chip::before {
      content: "";
      flex: none;
      width: 7px;
      height: 7px;
      border-radius: 50%;
      background: var(--overlay0);
    }

    .chip[data-state="playing"] {
      border-color: rgba(166, 218, 149, 0.5);
      color: var(--green);
    }

    .chip[data-state="playing"]::before {
      background: var(--green);
      box-shadow: 0 0 8px rgba(166, 218, 149, 0.9);
      animation: pulse 2.2s ease-in-out infinite;
    }

    .chip[data-state="paused"] {
      border-color: rgba(245, 169, 127, 0.5);
      color: var(--peach);
    }

    .chip[data-state="paused"]::before {
      background: var(--peach);
    }

    .chip[data-state="idle"] {
      border-color: rgba(145, 215, 227, 0.45);
      color: var(--sky);
    }

    .chip[data-state="idle"]::before {
      background: var(--sky);
    }

    .chip[data-state="disconnected"] {
      border-color: rgba(237, 135, 150, 0.45);
      color: var(--red);
    }

    .chip[data-state="disconnected"]::before {
      background: var(--red);
    }

    @keyframes pulse {
      0%, 100% { opacity: 1; }
      50% { opacity: 0.45; }
    }

    .icon-btn {
      flex: none;
      width: 26px;
      height: 26px;
      padding: 0;
      border-radius: 50%;
      font-size: 12px;
    }

    button.icon-btn#hide {
      color: var(--overlay1);
    }

    button.icon-btn#hide:hover:not(:disabled) {
      border-color: rgba(237, 135, 150, 0.55);
      background: rgba(237, 135, 150, 0.14);
      color: var(--red);
    }

    /* ---------- mode switch (Music / Radio) ---------- */

    .mode-switch {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 4px;
      padding: 4px;
      border-radius: 999px;
      background: var(--crust);
    }

    .mode-switch button {
      height: 30px;
      border: none;
      border-radius: 999px;
      background: transparent;
      color: var(--subtext0);
      font-size: 12px;
      font-weight: 750;
    }

    .mode-switch button.on {
      background: linear-gradient(135deg, var(--mauve), var(--pink));
      color: var(--crust);
      box-shadow: 0 3px 12px rgba(198, 160, 246, 0.35);
    }

    .mode-switch button#modeRadio.on {
      background: linear-gradient(135deg, var(--teal), var(--sky));
      box-shadow: 0 3px 12px rgba(139, 213, 202, 0.35);
    }

    .mode-switch button:disabled {
      opacity: 0.45;
    }

    /* ---------- tabs ---------- */

    .tabs {
      display: grid;
      grid-template-columns: repeat(4, minmax(0, 1fr));
      gap: 4px;
      padding: 4px;
      border-radius: 999px;
      background: var(--mantle);
    }

    .tab {
      height: 26px;
      padding: 0;
      border: none;
      border-radius: 999px;
      background: transparent;
      color: var(--subtext0);
      font-size: 11.5px;
    }

    .tab.active {
      background: var(--surface0);
      color: var(--mauve);
      box-shadow: 0 2px 8px rgba(24, 25, 38, 0.55);
    }

    main {
      min-height: 0;
    }

    .tab-panel {
      display: none;
      height: 100%;
      min-height: 0;
    }

    .tab-panel.active {
      display: grid;
      gap: 8px;
      align-content: start;
    }

    /* Spread the Now sections over the full height instead of pooling slack at
       the bottom; let the queue list flex to whatever height is left. */
    .tab-panel[data-panel="now"].active {
      align-content: space-between;
    }

    .tab-panel[data-panel="queue"].active {
      grid-template-rows: auto minmax(0, 1fr);
      align-content: stretch;
    }

    /* ---------- now playing ---------- */

    .now-card {
      display: grid;
      gap: 3px;
      padding: 11px 13px 12px;
      border-radius: 20px;
      background: rgba(30, 32, 48, 0.75);
    }

    .title {
      min-width: 0;
      overflow: hidden;
      color: var(--text);
      font-size: 18px;
      font-weight: 800;
      line-height: 1.2;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .artist {
      min-width: 0;
      overflow: hidden;
      color: var(--subtext1);
      font-size: 12.5px;
      font-weight: 650;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .meta {
      display: flex;
      align-items: center;
      gap: 6px;
      min-width: 0;
      overflow: hidden;
      margin-top: 3px;
      color: var(--overlay1);
      font-size: 10.5px;
      font-weight: 650;
      white-space: nowrap;
    }

    .meta span {
      overflow: hidden;
      text-overflow: ellipsis;
    }

    .meta .onair {
      display: inline-flex;
      flex: none;
      align-items: center;
      gap: 4px;
      padding: 1px 8px;
      border-radius: 999px;
      background: rgba(139, 213, 202, 0.14);
      color: var(--teal);
    }

    /* The explicit display above beats the UA's [hidden] rule — restore it. */
    .meta .onair[hidden] {
      display: none;
    }

    .meta .onair::before {
      content: "";
      width: 5px;
      height: 5px;
      border-radius: 50%;
      background: var(--teal);
      animation: pulse 1.6s ease-in-out infinite;
    }

    /* progress row: [-N] [bar] [+N] */
    .progress-row {
      display: grid;
      grid-template-columns: 30px minmax(0, 1fr) 30px;
      gap: 8px;
      align-items: center;
      margin-top: 8px;
    }

    .progress-row .step {
      width: 30px;
      height: 30px;
      padding: 0;
      border-radius: 50%;
      color: var(--subtext0);
      font-size: 9.5px;
      font-weight: 800;
    }

    .bar {
      position: relative;
      height: 18px;
      border-radius: 999px;
      touch-action: none;
    }

    .bar .track {
      position: absolute;
      inset: 6px 0;
      overflow: hidden;
      border-radius: 999px;
      background: var(--surface0);
    }

    .bar .fill {
      position: absolute;
      inset: 0 auto 0 0;
      width: 0%;
      border-radius: 999px;
      background: linear-gradient(90deg, var(--mauve), var(--pink));
      transition: width 0.15s linear;
    }

    .bar .knob {
      position: absolute;
      top: 50%;
      left: 0%;
      width: 13px;
      height: 13px;
      border-radius: 50%;
      background: var(--rosewater);
      box-shadow: 0 1px 6px rgba(12, 12, 20, 0.6);
      transform: translate(-50%, -50%);
      transition: left 0.15s linear;
    }

    .bar.dragging .fill,
    .bar.dragging .knob {
      transition: none;
    }

    #progressBar.disabled {
      opacity: 0.4;
    }

    #progressBar.live .fill {
      width: 100% !important;
      background: repeating-linear-gradient(
        -45deg,
        rgba(139, 213, 202, 0.85) 0 10px,
        rgba(145, 215, 227, 0.55) 10px 20px
      );
      animation: crawl 1.2s linear infinite;
    }

    #progressBar.live .knob {
      display: none;
    }

    @keyframes crawl {
      from { background-position: 0 0; }
      to { background-position: 28px 0; }
    }

    .times {
      display: flex;
      justify-content: space-between;
      padding: 0 40px;
      color: var(--overlay1);
      font-size: 10px;
      font-weight: 700;
      font-variant-numeric: tabular-nums;
    }

    .times .live-tag {
      color: var(--teal);
      letter-spacing: 1px;
    }

    /* transport */
    .transport {
      display: flex;
      align-items: center;
      justify-content: center;
      gap: 18px;
      padding: 2px 0 0;
    }

    .transport .skip {
      width: 42px;
      height: 42px;
      padding: 0;
      border-radius: 50%;
      color: var(--lavender);
      font-size: 12px;
    }

    .transport .primary {
      width: 56px;
      height: 56px;
      padding: 0;
      border: none;
      border-radius: 50%;
      background: linear-gradient(135deg, var(--mauve), var(--pink));
      color: var(--crust);
      font-size: 19px;
      box-shadow: 0 6px 20px rgba(198, 160, 246, 0.4);
    }

    .transport .primary:hover:not(:disabled) {
      border-color: transparent;
      box-shadow: 0 6px 24px rgba(245, 189, 230, 0.55);
    }

    /* shuffle / repeat / streaming pills */
    .pill-row {
      display: grid;
      grid-template-columns: repeat(3, minmax(0, 1fr));
      gap: 6px;
    }

    .pill-row button {
      height: 30px;
      border-radius: 999px;
      font-size: 11px;
    }

    button#shuffle.on {
      border-color: rgba(139, 213, 202, 0.6);
      background: rgba(139, 213, 202, 0.13);
      color: var(--teal);
    }

    button#repeat.on {
      border-color: rgba(245, 189, 230, 0.6);
      background: rgba(245, 189, 230, 0.12);
      color: var(--pink);
    }

    button#streaming.on {
      border-color: rgba(166, 218, 149, 0.6);
      background: rgba(166, 218, 149, 0.12);
      color: var(--green);
    }

    /* volume row */
    .volume-row {
      display: grid;
      grid-template-columns: auto minmax(0, 1fr) 40px;
      gap: 9px;
      align-items: center;
      padding: 7px 12px;
      border-radius: 999px;
      background: var(--mantle);
    }

    .volume-row .vol-ico {
      color: var(--subtext0);
      font-size: 13px;
    }

    #volumeBar .fill {
      background: linear-gradient(90deg, var(--teal), var(--sky));
    }

    #volumeBar.disabled {
      opacity: 0.4;
    }

    #volumePct {
      color: var(--subtext1);
      font-size: 11px;
      font-weight: 800;
      font-variant-numeric: tabular-nums;
      text-align: right;
    }

    /* ---------- queue ---------- */

    .queue-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      min-width: 0;
      padding: 0 2px;
    }

    .queue-list {
      display: grid;
      gap: 6px;
      align-content: start;
      min-height: 0;
      overflow-y: auto;
      padding-right: 4px;
    }

    .queue-list::-webkit-scrollbar {
      width: 6px;
    }

    .queue-list::-webkit-scrollbar-thumb {
      border-radius: 999px;
      background: var(--surface1);
    }

    .queue-item {
      display: grid;
      grid-template-columns: minmax(0, 1fr) 30px;
      gap: 6px;
      align-items: center;
      min-width: 0;
    }

    button.queue-track {
      display: grid;
      grid-template-columns: 26px minmax(0, 1fr) auto;
      gap: 9px;
      align-items: center;
      width: 100%;
      height: auto;
      padding: 7px 11px 7px 8px;
      border-color: transparent;
      border-radius: 16px;
      background: var(--mantle);
      text-align: left;
    }

    button.queue-track:hover:not(:disabled) {
      border-color: var(--surface2);
    }

    .queue-item.current button.queue-track {
      border-color: rgba(198, 160, 246, 0.55);
      background: rgba(198, 160, 246, 0.12);
      box-shadow: 0 2px 10px rgba(198, 160, 246, 0.18);
    }

    .queue-number {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 24px;
      height: 24px;
      border-radius: 50%;
      background: var(--surface0);
      color: var(--subtext0);
      font-size: 10px;
      font-weight: 800;
    }

    .queue-item.current .queue-number {
      background: var(--mauve);
      color: var(--crust);
    }

    .queue-text {
      display: grid;
      gap: 1px;
      min-width: 0;
    }

    .queue-title,
    .queue-artist {
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .queue-title {
      color: var(--text);
      font-size: 12px;
      font-weight: 750;
    }

    .queue-artist {
      color: var(--subtext0);
      font-size: 10.5px;
      font-weight: 600;
    }

    .queue-duration {
      color: var(--overlay1);
      font-size: 10.5px;
      font-weight: 650;
      font-variant-numeric: tabular-nums;
    }

    button.queue-remove {
      width: 30px;
      height: 30px;
      padding: 0;
      border-color: transparent;
      border-radius: 50%;
      background: transparent;
      color: var(--red);
      font-size: 11px;
    }

    button.queue-remove:hover:not(:disabled) {
      border-color: rgba(237, 135, 150, 0.4);
      background: rgba(237, 135, 150, 0.14);
    }

    .empty {
      display: grid;
      gap: 4px;
      place-items: center;
      min-height: 150px;
      border: 2px dashed var(--surface1);
      border-radius: 20px;
      color: var(--subtext0);
      font-size: 12px;
      font-weight: 700;
    }

    .empty .kaomoji {
      color: var(--overlay1);
      font-size: 16px;
      letter-spacing: 1px;
    }

    /* ---------- settings rows ---------- */

    .settings {
      display: grid;
      gap: 7px;
      align-content: start;
    }

    .row {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 10px;
      min-height: 40px;
      padding: 4px 5px 4px 12px;
      border-radius: 16px;
      background: var(--mantle);
    }

    .label {
      color: var(--subtext1);
      font-size: 12px;
      font-weight: 750;
    }

    .sub-label {
      color: var(--overlay0);
      font-size: 10px;
      font-weight: 650;
    }

    .value {
      min-width: 44px;
      color: var(--text);
      font-size: 12px;
      font-weight: 800;
      font-variant-numeric: tabular-nums;
      text-align: center;
    }

    .segmented {
      display: inline-flex;
      gap: 4px;
      padding: 3px;
      border-radius: 999px;
      background: var(--crust);
    }

    .segmented button {
      height: 26px;
      padding: 0 10px;
      border: none;
      border-radius: 999px;
      background: transparent;
      color: var(--subtext0);
      font-size: 11px;
    }

    .segmented button.on {
      background: var(--mauve);
      color: var(--crust);
    }

    .stepper {
      display: inline-flex;
      align-items: center;
      gap: 4px;
    }

    .stepper button {
      width: 30px;
      height: 30px;
      padding: 0;
      border-radius: 50%;
      font-size: 14px;
    }

    .toggle {
      min-width: 58px;
      height: 30px;
      border-radius: 999px;
    }

    .toggle.on {
      border-color: rgba(166, 218, 149, 0.6);
      background: rgba(166, 218, 149, 0.14);
      color: var(--green);
    }

    select {
      height: 32px;
      max-width: 190px;
      padding: 0 10px;
      border: 1.5px solid var(--surface1);
      border-radius: 14px;
      background: var(--surface0);
      color: var(--text);
      font: inherit;
      font-size: 12px;
      font-weight: 650;
      outline: none;
    }

    /* ---------- dock ---------- */

    .dock {
      display: grid;
      grid-template-columns: repeat(4, minmax(0, 1fr));
      gap: 6px;
      padding: 6px;
      border-radius: 18px;
      background: var(--mantle);
    }

    .dock button {
      height: 30px;
      border-color: transparent;
      border-radius: 12px;
      font-size: 11px;
    }

    button#startDaemon {
      color: var(--green);
    }

    button#startDaemon:hover:not(:disabled) {
      border-color: rgba(166, 218, 149, 0.6);
    }

    button#resumeDaemon {
      color: var(--teal);
    }

    button#resumeDaemon:hover:not(:disabled) {
      border-color: rgba(139, 213, 202, 0.6);
    }

    button#stopDaemon {
      color: var(--red);
    }

    button#stopDaemon:hover:not(:disabled) {
      border-color: rgba(237, 135, 150, 0.6);
    }

    button#openTui {
      color: var(--lavender);
    }

    button#openTui:hover:not(:disabled) {
      border-color: rgba(183, 189, 248, 0.6);
    }

    /* ---------- error toast ---------- */

    /* Inline toast row between the tabs' content and the dock: when hidden its
       grid row collapses, so it never overlaps the controls. */
    .alert {
      overflow: hidden;
      padding: 7px 14px;
      border: 1.5px solid rgba(237, 135, 150, 0.5);
      border-radius: 999px;
      background: rgba(237, 135, 150, 0.1);
      color: var(--red);
      font-size: 11px;
      font-weight: 650;
      text-align: center;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .alert[hidden] {
      display: none;
    }

    /* ---------- shared button base ---------- */

    button {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      gap: 6px;
      min-width: 0;
      height: 34px;
      padding: 0 10px;
      border: 1.5px solid var(--surface1);
      border-radius: 16px;
      background: var(--surface0);
      color: var(--text);
      font: inherit;
      font-size: 12px;
      font-weight: 700;
      white-space: nowrap;
      outline: none;
      transition: transform 0.12s ease, background 0.15s ease, border-color 0.15s ease,
        color 0.15s ease, opacity 0.15s ease, box-shadow 0.15s ease;
    }

    button:hover:not(:disabled) {
      border-color: var(--mauve);
    }

    button:active:not(:disabled) {
      transform: scale(0.92);
    }

    button:focus-visible {
      border-color: var(--lavender);
      box-shadow: 0 0 0 3px rgba(183, 189, 248, 0.25);
    }

    button:disabled,
    select:disabled {
      opacity: 0.38;
    }
  </style>
</head>
<body>
  <div class="cushion">
    <header id="dragRegion">
      <div class="brand"><span class="cat">=^..^=</span> YtmTui</div>
      <div class="chip" id="stateLabel" data-state="disconnected">Disconnected</div>
      <button class="icon-btn" data-action="refresh" id="refreshTop" title="Refresh">&#8635;</button>
      <button class="icon-btn" data-action="hide" id="hide" title="Hide">&#10005;</button>
    </header>

    <div class="mode-switch" id="modeSwitch" title="Switch between normal playback and the dedicated radio mode">
      <button id="modeMusic" data-mode="music">&#9834; Music</button>
      <button id="modeRadio" data-mode="radio">&#128251; Radio</button>
    </div>

    <div class="tabs" role="tablist">
      <button class="tab active" data-tab="now" id="tabNow">Now</button>
      <button class="tab" data-tab="queue" id="tabQueue">Queue</button>
      <button class="tab" data-tab="streaming" id="tabStreaming">Stream</button>
      <button class="tab" data-tab="playback" id="tabPlayback">Tune</button>
    </div>

    <main>
      <section class="tab-panel active" data-panel="now">
        <div class="now-card">
          <div class="title" id="title">ytm-tui is not running</div>
          <div class="artist" id="artist">YtmTui</div>
          <div class="meta">
            <span id="ownerLabel">Offline</span>
            <span>&#183;</span>
            <span id="queueLabel">Queue unavailable</span>
            <span class="onair" id="onAir" hidden>on air &#9834;</span>
          </div>

          <div class="progress-row">
            <button class="step" data-action="seek_back" id="seekBack" title="Seek back">-10</button>
            <div class="bar" id="progressBar">
              <div class="track"><div class="fill" id="progressFill"></div></div>
              <div class="knob" id="progressKnob"></div>
            </div>
            <button class="step" data-action="seek_forward" id="seekForward" title="Seek forward">+10</button>
          </div>
          <div class="times">
            <span id="timeElapsed">-:--</span>
            <span id="timeTotal">-:--</span>
          </div>
        </div>

        <div class="transport">
          <button class="skip" data-action="previous" id="previous" title="Previous" aria-label="Previous">&#9664;&#9664;</button>
          <button class="primary" data-action="play_pause" id="playPause" title="Play or pause" aria-label="Play or pause">&#9654;</button>
          <button class="skip" data-action="next" id="next" title="Next" aria-label="Next">&#9654;&#9654;</button>
        </div>

        <div class="pill-row">
          <button data-action="toggle_shuffle" id="shuffle" title="Shuffle" aria-label="Toggle shuffle">&#8644; Shuffle</button>
          <button data-action="cycle_repeat" id="repeat" title="Repeat" aria-label="Cycle repeat">&#8635; <span id="repeatText">Off</span></button>
          <button data-action="set_streaming" id="streaming" title="Autoplay streaming">&#9835; Stream</button>
        </div>

        <div class="volume-row">
          <span class="vol-ico">&#9834;</span>
          <div class="bar" id="volumeBar" title="Scroll or drag to change the volume">
            <div class="track"><div class="fill" id="volumeFill"></div></div>
            <div class="knob" id="volumeKnob"></div>
          </div>
          <span id="volumePct">--</span>
        </div>
      </section>

      <section class="tab-panel" data-panel="queue">
        <div class="queue-head">
          <span class="label" id="queueSummary">Queue unavailable</span>
          <button class="icon-btn" data-action="refresh" id="queueRefresh" title="Refresh">&#8635;</button>
        </div>
        <div class="queue-list" id="queueList"></div>
      </section>

      <section class="tab-panel" data-panel="streaming">
        <div class="settings">
          <div class="row">
            <span class="label">Streaming</span>
            <button class="toggle" data-action="set_streaming" id="streamingToggle">Off</button>
          </div>
          <div class="row">
            <span class="label">Mode</span>
            <div class="segmented">
              <button class="mode" data-action="set_streaming_mode" data-value="focused" id="modeFocused">Focused</button>
              <button class="mode" data-action="set_streaming_mode" data-value="balanced" id="modeBalanced">Balanced</button>
              <button class="mode" data-action="set_streaming_mode" data-value="discovery" id="modeDiscovery">Discovery</button>
            </div>
          </div>
          <div class="row">
            <span class="label">Source</span>
            <select id="streamingSource" data-action="set_streaming_source"></select>
          </div>
          <div class="row">
            <span class="label">DJ Gem</span>
            <button class="toggle" data-action="set_ai_enabled" id="aiEnabled">On</button>
          </div>
          <div class="row">
            <span class="label">Radio mode <span class="sub-label" id="radioHint"></span></span>
            <button class="toggle" data-action="set_radio_mode" id="radioMode">Off</button>
          </div>
        </div>
      </section>

      <section class="tab-panel" data-panel="playback">
        <div class="settings">
          <div class="row">
            <span class="label">Speed</span>
            <div class="stepper">
              <button data-action="speed_delta" data-delta="-1" id="speedDown">-</button>
              <span class="value" id="speedLabel">1.0x</span>
              <button data-action="speed_delta" data-delta="1" id="speedUp">+</button>
            </div>
          </div>
          <div class="row">
            <span class="label">Seek step</span>
            <div class="stepper">
              <button data-action="seek_delta" data-delta="-1" id="seekDown">-</button>
              <span class="value" id="seekLabel">10s</span>
              <button data-action="seek_delta" data-delta="1" id="seekUp">+</button>
            </div>
          </div>
          <div class="row">
            <span class="label">Normalize</span>
            <button class="toggle" data-action="set_normalize" id="normalize">Off</button>
          </div>
          <div class="row">
            <span class="label">Gapless</span>
            <button class="toggle" data-action="set_gapless" id="gapless">On</button>
          </div>
        </div>
      </section>
    </main>

    <div class="alert" id="error" hidden></div>

    <div class="dock">
      <button data-action="start_daemon" id="startDaemon">Start</button>
      <button data-action="resume_daemon" id="resumeDaemon">Resume</button>
      <button data-action="stop_daemon" id="stopDaemon">Stop</button>
      <button data-action="open_tui" id="openTui">Open TUI</button>
    </div>
  </div>

  <script>
    window.__YTM_TUI_INITIAL__ = __INITIAL_PAYLOAD__;

    let currentPayload = window.__YTM_TUI_INITIAL__;
    let lastQueueKey = null;
    let lastSourceOptionsKey = null;

    const els = {};
    [
      "stateLabel", "title", "artist", "ownerLabel", "queueLabel", "onAir", "error",
      "previous", "playPause", "next", "shuffle", "repeat", "repeatText", "streaming",
      "seekBack", "seekForward", "progressBar", "progressFill", "progressKnob",
      "timeElapsed", "timeTotal", "volumeBar", "volumeFill", "volumeKnob", "volumePct",
      "modeMusic", "modeRadio", "radioHint",
      "queueSummary", "queueList", "queueRefresh", "refreshTop", "hide",
      "streamingToggle", "modeFocused", "modeBalanced", "modeDiscovery", "streamingSource",
      "aiEnabled", "radioMode", "speedDown", "speedUp", "speedLabel",
      "seekDown", "seekUp", "seekLabel", "normalize", "gapless",
      "startDaemon", "resumeDaemon", "stopDaemon", "openTui", "dragRegion"
    ].forEach(id => { els[id] = document.getElementById(id); });

    function send(action, value) {
      if (window.ipc && window.ipc.postMessage) {
        const message = value === undefined ? { action } : { action, value };
        window.ipc.postMessage(JSON.stringify(message));
      }
    }

    function enabled(button, value) {
      button.disabled = !value;
    }

    function setTab(tab) {
      document.querySelectorAll("[data-tab]").forEach(button => {
        button.classList.toggle("active", button.dataset.tab === tab);
      });
      document.querySelectorAll("[data-panel]").forEach(panel => {
        panel.classList.toggle("active", panel.dataset.panel === tab);
      });
    }

    function setToggle(button, on, onText, offText) {
      button.textContent = on ? onText : offText;
      button.classList.toggle("on", on);
    }

    function chipState(payload) {
      if (!payload.connected) return "disconnected";
      const state = String(payload.stateLabel || "").toLowerCase();
      return state === "playing" || state === "paused" ? state : "idle";
    }

    function escapeHtml(value) {
      return String(value ?? "")
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;")
        .replace(/'/g, "&#39;");
    }

    function fmtTime(ms) {
      if (ms == null || !isFinite(ms)) return "-:--";
      const total = Math.max(0, Math.floor(ms / 1000));
      const h = Math.floor(total / 3600);
      const m = Math.floor((total % 3600) / 60);
      const s = total % 60;
      const two = n => String(n).padStart(2, "0");
      return h > 0 ? h + ":" + two(m) + ":" + two(s) : m + ":" + two(s);
    }

    /* ---------- window drag (frameless) ---------- */

    els.dragRegion.addEventListener("mousedown", event => {
      if (event.button !== 0) return;
      if (event.target.closest("button")) return;
      send("drag");
    });

    /* ---------- progress bar: live interpolation + click/drag seek ---------- */

    const prog = {
      anchorMs: null,
      anchorAt: 0,
      playing: false,
      speed: 1,
      durationMs: null,
      live: false,
      canSeek: false,
      holdUntil: 0,
      dragging: false
    };

    function shownElapsed() {
      if (prog.anchorMs == null) return null;
      let value = prog.anchorMs;
      if (prog.playing && !prog.dragging) {
        value += (performance.now() - prog.anchorAt) * prog.speed;
      }
      if (prog.durationMs != null) value = Math.min(value, prog.durationMs);
      return Math.max(0, value);
    }

    function renderProgress() {
      const elapsed = shownElapsed();
      const isLive = prog.live;
      els.progressBar.classList.toggle("live", isLive);
      els.progressBar.classList.toggle("disabled", !prog.canSeek && !isLive);
      if (isLive) {
        els.timeElapsed.textContent = "on air";
        els.timeTotal.innerHTML = '<span class="live-tag">LIVE</span>';
        return;
      }
      const pct = prog.durationMs && elapsed != null
        ? Math.min(100, (elapsed / prog.durationMs) * 100)
        : 0;
      els.progressFill.style.width = pct + "%";
      els.progressKnob.style.left = pct + "%";
      els.timeElapsed.textContent = fmtTime(elapsed);
      els.timeTotal.textContent = prog.durationMs != null ? fmtTime(prog.durationMs) : "-:--";
    }

    function syncProgress(payload) {
      if (performance.now() < prog.holdUntil || prog.dragging) return;
      prog.anchorMs = payload.elapsedMs;
      prog.anchorAt = performance.now();
      prog.playing = payload.connected && !payload.paused && payload.elapsedMs != null;
      prog.speed = (payload.settings.speedTenths || 10) / 10;
      prog.durationMs = payload.durationMs;
      prog.live = payload.connected
        && payload.durationMs == null
        && payload.elapsedMs != null;
      prog.canSeek = payload.canSeek;
      renderProgress();
    }

    setInterval(renderProgress, 250);

    function barFraction(bar, clientX) {
      const rect = bar.getBoundingClientRect();
      if (rect.width <= 0) return 0;
      return Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
    }

    els.progressBar.addEventListener("pointerdown", event => {
      if (!prog.canSeek || prog.durationMs == null) return;
      prog.dragging = true;
      els.progressBar.classList.add("dragging");
      els.progressBar.setPointerCapture(event.pointerId);
      prog.anchorMs = barFraction(els.progressBar, event.clientX) * prog.durationMs;
      renderProgress();
    });

    els.progressBar.addEventListener("pointermove", event => {
      if (!prog.dragging) return;
      prog.anchorMs = barFraction(els.progressBar, event.clientX) * prog.durationMs;
      renderProgress();
    });

    function endSeekDrag(event) {
      if (!prog.dragging) return;
      prog.dragging = false;
      els.progressBar.classList.remove("dragging");
      const target = Math.round(barFraction(els.progressBar, event.clientX) * prog.durationMs);
      prog.anchorMs = target;
      prog.anchorAt = performance.now();
      prog.holdUntil = performance.now() + 1500;
      send("seek_to", target);
      renderProgress();
    }

    els.progressBar.addEventListener("pointerup", endSeekDrag);
    els.progressBar.addEventListener("pointercancel", () => {
      prog.dragging = false;
      els.progressBar.classList.remove("dragging");
    });

    /* ---------- volume bar: wheel + click/drag ---------- */

    const vol = {
      local: null,
      localUntil: 0,
      canVolume: false,
      dragging: false,
      sendTimer: null,
      pending: null
    };

    function currentVolume() {
      return vol.local != null ? vol.local : currentPayload.volume;
    }

    function renderVolume() {
      const value = currentVolume();
      const pct = Math.min(100, Math.max(0, value));
      els.volumeFill.style.width = pct + "%";
      els.volumeKnob.style.left = pct + "%";
      els.volumePct.textContent = currentPayload.connected ? pct + "%" : "--";
      els.volumeBar.classList.toggle("disabled", !vol.canVolume);
    }

    function queueVolumeSend(value) {
      vol.pending = value;
      if (vol.sendTimer) return;
      vol.sendTimer = setTimeout(() => {
        vol.sendTimer = null;
        send("set_volume", vol.pending);
      }, 70);
    }

    function setVolumeLocal(value) {
      if (!vol.canVolume) return;
      const next = Math.min(100, Math.max(0, Math.round(value)));
      vol.local = next;
      vol.localUntil = performance.now() + 1800;
      queueVolumeSend(next);
      renderVolume();
    }

    els.volumeBar.addEventListener("wheel", event => {
      event.preventDefault();
      if (!vol.canVolume) return;
      const step = event.shiftKey ? 1 : 5;
      const dir = (event.deltaY || event.deltaX) < 0 ? 1 : -1;
      setVolumeLocal(currentVolume() + dir * step);
    }, { passive: false });

    els.volumeBar.addEventListener("pointerdown", event => {
      if (!vol.canVolume) return;
      vol.dragging = true;
      els.volumeBar.classList.add("dragging");
      els.volumeBar.setPointerCapture(event.pointerId);
      setVolumeLocal(barFraction(els.volumeBar, event.clientX) * 100);
    });

    els.volumeBar.addEventListener("pointermove", event => {
      if (!vol.dragging) return;
      setVolumeLocal(barFraction(els.volumeBar, event.clientX) * 100);
    });

    function endVolumeDrag() {
      vol.dragging = false;
      els.volumeBar.classList.remove("dragging");
    }

    els.volumeBar.addEventListener("pointerup", endVolumeDrag);
    els.volumeBar.addEventListener("pointercancel", endVolumeDrag);

    /* ---------- streaming source select ---------- */

    function renderSourceOptions(settings) {
      const selected = settings.streamingSource;
      const sources = settings.streamingSources.slice();
      // Protocol skew safety net: an unknown current value still shows up instead
      // of leaving the select blank.
      if (!sources.some(source => source.value === selected)) {
        sources.push({ value: selected, label: settings.streamingSourceLabel || selected });
      }
      // Rebuild <option>s only when the option list itself changes; comparing against
      // innerHTML never matched (browsers re-serialize attributes), which used to
      // rebuild + reset this select on every poll.
      const optionsKey = JSON.stringify(sources);
      if (optionsKey !== lastSourceOptionsKey) {
        lastSourceOptionsKey = optionsKey;
        els.streamingSource.innerHTML = sources.map(source =>
          `<option value="${escapeHtml(source.value)}">${escapeHtml(source.label)}</option>`
        ).join("");
      }
      if (els.streamingSource.value !== selected) {
        els.streamingSource.value = selected;
      }
    }

    /* ---------- queue ---------- */

    function renderQueue(payload) {
      els.queueSummary.textContent = payload.queueLabel;
      // Skip the innerHTML rebuild unless the queue actually changed — rebuilding on
      // every 2s poll ate in-flight clicks and thrashed layout on large queues.
      const queueKey = JSON.stringify(payload.queue);
      if (queueKey === lastQueueKey) return;
      lastQueueKey = queueKey;

      if (!payload.queue.length) {
        els.queueList.innerHTML = `<div class="empty"><span class="kaomoji">=^..^=</span><span>queue is napping&#8230;</span></div>`;
        return;
      }

      els.queueList.innerHTML = payload.queue.map(item => {
        const number = item.current ? "&#9654;" : String(item.index + 1);
        const duration = item.duration ? escapeHtml(item.duration) : "";
        const current = item.current ? " current" : "";
        return `
          <div class="queue-item${current}">
            <button class="queue-track" data-action="queue_play" data-position="${item.index}" title="Play ${escapeHtml(item.title)}">
              <span class="queue-number">${number}</span>
              <span class="queue-text">
                <span class="queue-title">${escapeHtml(item.title)}</span>
                <span class="queue-artist">${escapeHtml(item.artist)}</span>
              </span>
              <span class="queue-duration">${duration}</span>
            </button>
            <button class="queue-remove" data-action="queue_remove" data-position="${item.index}" title="Remove" aria-label="Remove">&#10005;</button>
          </div>`;
      }).join("");
    }

    /* ---------- apply a status payload ---------- */

    function apply(payload) {
      currentPayload = payload;
      const settings = payload.settings;

      els.stateLabel.textContent = payload.stateLabel;
      els.stateLabel.dataset.state = chipState(payload);
      els.title.textContent = payload.title;
      els.artist.textContent = payload.artist;
      els.ownerLabel.textContent = payload.ownerLabel;
      els.queueLabel.textContent = payload.queueLabel;
      els.onAir.hidden = !payload.streaming;
      els.playPause.innerHTML = payload.paused ? "&#9654;" : "&#10073;&#10073;";
      els.repeatText.textContent = payload.repeatLabel;
      renderQueue(payload);

      els.shuffle.classList.toggle("on", payload.shuffle);
      els.repeat.classList.toggle("on", payload.repeat !== "off");
      els.streaming.classList.toggle("on", settings.autoplayStreaming);
      setToggle(els.streamingToggle, settings.autoplayStreaming, "On", "Off");
      setToggle(els.aiEnabled, settings.aiEnabled, "On", "Off");
      setToggle(els.radioMode, settings.radioMode, "On", "Off");
      setToggle(els.normalize, settings.normalize, "On", "Off");
      setToggle(els.gapless, settings.gapless, "On", "Off");

      els.modeMusic.classList.toggle("on", !settings.radioMode);
      els.modeRadio.classList.toggle("on", settings.radioMode);
      enabled(els.modeMusic, settings.canRadioMode && settings.radioMode);
      enabled(els.modeRadio, settings.canRadioMode && !settings.radioMode);
      els.radioHint.textContent = settings.canRadioMode ? "" : "(TUI only)";

      els.modeFocused.classList.toggle("on", settings.streamingMode === "focused");
      els.modeBalanced.classList.toggle("on", settings.streamingMode === "balanced");
      els.modeDiscovery.classList.toggle("on", settings.streamingMode === "discovery");
      renderSourceOptions(settings);
      els.speedLabel.textContent = settings.speedLabel;
      els.seekBack.textContent = "-" + settings.seekSeconds;
      els.seekForward.textContent = "+" + settings.seekSeconds;
      els.seekLabel.textContent = settings.seekLabel;

      enabled(els.previous, payload.canPlayback);
      enabled(els.playPause, payload.canPlayback);
      enabled(els.next, payload.canPlayback);
      enabled(els.shuffle, payload.connected);
      enabled(els.repeat, payload.connected);
      enabled(els.queueRefresh, payload.connected);
      enabled(els.seekBack, payload.canSeek);
      enabled(els.seekForward, payload.canSeek);
      enabled(els.streaming, payload.canToggleStreaming);
      enabled(els.streamingToggle, payload.canToggleStreaming);
      enabled(els.modeFocused, payload.connected);
      enabled(els.modeBalanced, payload.connected);
      enabled(els.modeDiscovery, payload.connected);
      els.streamingSource.disabled = !payload.connected;
      enabled(els.aiEnabled, payload.connected);
      enabled(els.radioMode, settings.canRadioMode);
      enabled(els.speedDown, payload.connected && settings.speedTenths > 5);
      enabled(els.speedUp, payload.connected && settings.speedTenths < 20);
      enabled(els.seekDown, payload.connected && settings.seekSeconds > 1);
      enabled(els.seekUp, payload.connected && settings.seekSeconds < 60);
      enabled(els.normalize, payload.connected);
      enabled(els.gapless, payload.connected);
      enabled(els.startDaemon, payload.canStartDaemon);
      enabled(els.resumeDaemon, payload.canResumeDaemon);
      enabled(els.stopDaemon, payload.canStopDaemon);

      vol.canVolume = payload.canVolume;
      if (performance.now() > vol.localUntil) {
        vol.local = null;
      }
      renderVolume();
      syncProgress(payload);

      if (payload.error) {
        els.error.hidden = false;
        els.error.textContent = payload.error;
      } else {
        els.error.hidden = true;
        els.error.textContent = "";
      }
    }

    /* ---------- clicks ---------- */

    document.addEventListener("click", event => {
      const tab = event.target.closest("button[data-tab]");
      if (tab) {
        setTab(tab.dataset.tab);
        return;
      }

      const mode = event.target.closest("button[data-mode]");
      if (mode && !mode.disabled) {
        const settings = currentPayload.settings;
        const wantRadio = mode.dataset.mode === "radio";
        if (wantRadio !== settings.radioMode) {
          settings.radioMode = wantRadio;
          send("set_radio_mode", wantRadio);
          apply(currentPayload);
        }
        return;
      }

      const button = event.target.closest("button[data-action]");
      if (!button || button.disabled) return;
      const action = button.dataset.action;
      const settings = currentPayload.settings;

      // Setter clicks update currentPayload optimistically so rapid clicks compute
      // from the newest value instead of the last poll; the next status payload
      // remains authoritative and overwrites everything.
      if (action === "set_streaming") {
        settings.autoplayStreaming = !settings.autoplayStreaming;
        send(action, settings.autoplayStreaming);
        apply(currentPayload);
      } else if (action === "toggle_shuffle" || action === "cycle_repeat") {
        send(action);
      } else if (action === "queue_play" || action === "queue_remove") {
        send(action, Number(button.dataset.position));
      } else if (action === "set_streaming_mode") {
        settings.streamingMode = button.dataset.value;
        send(action, button.dataset.value);
        apply(currentPayload);
      } else if (action === "set_ai_enabled") {
        settings.aiEnabled = !settings.aiEnabled;
        send(action, settings.aiEnabled);
        apply(currentPayload);
      } else if (action === "set_radio_mode") {
        settings.radioMode = !settings.radioMode;
        send(action, settings.radioMode);
        apply(currentPayload);
      } else if (action === "set_normalize") {
        settings.normalize = !settings.normalize;
        send(action, settings.normalize);
        apply(currentPayload);
      } else if (action === "set_gapless") {
        settings.gapless = !settings.gapless;
        send(action, settings.gapless);
        apply(currentPayload);
      } else if (action === "speed_delta") {
        const next = Math.max(5, Math.min(20, settings.speedTenths + Number(button.dataset.delta)));
        if (next !== settings.speedTenths) {
          settings.speedTenths = next;
          settings.speedLabel = (next / 10).toFixed(1) + "x";
          send("set_speed", next);
          apply(currentPayload);
        }
      } else if (action === "seek_delta") {
        const next = Math.max(1, Math.min(60, settings.seekSeconds + Number(button.dataset.delta)));
        if (next !== settings.seekSeconds) {
          settings.seekSeconds = next;
          settings.seekLabel = next + "s";
          send("set_seek_seconds", next);
          apply(currentPayload);
        }
      } else {
        send(action);
      }
    });

    els.streamingSource.addEventListener("change", event => {
      send("set_streaming_source", event.target.value);
    });

    window.ytmTuiApply = apply;
    apply(window.__YTM_TUI_INITIAL__);
  </script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::{InstanceMode, QueueItemSnapshot, StatusSnapshot};
    use crate::tray::control::ControlError;

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
        let html = html(&update);
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
}
