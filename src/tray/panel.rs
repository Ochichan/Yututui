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
            PanelCommand::Hide => None,
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
            || (idle
                && status.is_some_and(|status| status.owner_mode == InstanceMode::Daemon)),
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
    :root {
      color-scheme: dark;
      --bg: #141619;
      --surface: #1d2024;
      --surface-2: #282d32;
      --surface-3: #32383f;
      --line: #444b54;
      --text: #f4f6f8;
      --muted: #b5bdc7;
      --soft: #8b95a1;
      --accent: #25d09b;
      --accent-2: #8fb4ff;
      --danger: #ff7468;
    }

    * {
      box-sizing: border-box;
    }

    html,
    body {
      width: 100%;
      height: 100%;
      margin: 0;
      overflow: hidden;
      background: var(--bg);
      color: var(--text);
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      letter-spacing: 0;
      user-select: none;
    }

    .shell {
      display: grid;
      grid-template-rows: auto auto 1fr auto;
      gap: 10px;
      width: 100%;
      height: 100%;
      padding: 14px;
      background: var(--surface);
    }

    header,
    .meta,
    .row {
      display: flex;
      align-items: center;
      min-width: 0;
    }

    header {
      justify-content: space-between;
      gap: 10px;
    }

    .brand {
      color: var(--muted);
      font-size: 13px;
      font-weight: 760;
    }

    .status {
      max-width: 190px;
      overflow: hidden;
      padding: 4px 9px;
      border: 1px solid var(--line);
      border-radius: 999px;
      color: var(--accent);
      font-size: 12px;
      font-weight: 680;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .tabs {
      display: grid;
      grid-template-columns: repeat(4, minmax(0, 1fr));
      gap: 6px;
    }

    .tab {
      height: 30px;
      background: transparent;
      color: var(--muted);
    }

    .tab.active {
      border-color: var(--accent-2);
      background: var(--surface-2);
      color: var(--text);
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
      gap: 10px;
      align-content: start;
    }

    .title {
      min-width: 0;
      overflow: hidden;
      color: var(--text);
      font-size: 21px;
      font-weight: 780;
      line-height: 1.14;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .artist {
      min-width: 0;
      overflow: hidden;
      color: var(--muted);
      font-size: 14px;
      font-weight: 540;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .meta {
      gap: 8px;
      overflow: hidden;
      color: var(--soft);
      font-size: 12px;
      white-space: nowrap;
    }

    .meta span {
      overflow: hidden;
      text-overflow: ellipsis;
    }

    .transport {
      display: grid;
      grid-template-columns: 44px 58px 44px;
      gap: 10px;
      align-items: center;
      justify-content: center;
      padding-top: 4px;
    }

    .mode-controls {
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 6px;
    }

    .secondary {
      display: grid;
      grid-template-columns: repeat(7, minmax(0, 1fr));
      gap: 6px;
    }

    .settings {
      display: grid;
      gap: 8px;
    }

    .row {
      justify-content: space-between;
      gap: 10px;
      min-height: 34px;
    }

    .label {
      color: var(--muted);
      font-size: 12px;
      font-weight: 680;
    }

    .value {
      color: var(--text);
      font-size: 12px;
      font-weight: 760;
    }

    .segmented,
    .stepper {
      display: grid;
      gap: 6px;
    }

    .segmented {
      grid-template-columns: repeat(3, minmax(0, 1fr));
    }

    .stepper {
      grid-template-columns: 34px 76px 34px;
      align-items: center;
    }

    select {
      width: 190px;
      height: 32px;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: var(--surface-2);
      color: var(--text);
      font: inherit;
      font-size: 12px;
      font-weight: 650;
      outline: none;
    }

    .daemon {
      display: grid;
      grid-template-columns: repeat(4, minmax(0, 1fr));
      gap: 6px;
    }

    .queue-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      min-width: 0;
    }

    .queue-list {
      display: grid;
      gap: 6px;
      min-height: 0;
      max-height: 244px;
      overflow-y: auto;
      padding-right: 2px;
    }

    .queue-item {
      display: grid;
      grid-template-columns: minmax(0, 1fr) 32px;
      gap: 6px;
      align-items: center;
      min-width: 0;
    }

    .queue-track {
      display: grid;
      grid-template-columns: 28px minmax(0, 1fr) auto;
      gap: 8px;
      align-items: center;
      width: 100%;
      text-align: left;
    }

    .queue-item.current .queue-track {
      border-color: var(--accent);
      background: var(--surface-3);
    }

    .queue-number {
      color: var(--accent);
      font-size: 12px;
      font-weight: 760;
      text-align: center;
    }

    .queue-text {
      display: grid;
      gap: 2px;
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
      font-weight: 760;
    }

    .queue-artist,
    .queue-duration {
      color: var(--muted);
      font-size: 11px;
      font-weight: 620;
    }

    .queue-remove {
      width: 32px;
      padding: 0;
      color: var(--danger);
    }

    .empty {
      display: grid;
      place-items: center;
      min-height: 120px;
      border: 1px solid var(--line);
      border-radius: 8px;
      color: var(--muted);
      font-size: 12px;
      font-weight: 680;
    }

    button {
      display: inline-grid;
      place-items: center;
      min-width: 0;
      height: 34px;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: var(--surface-2);
      color: var(--text);
      font: inherit;
      font-size: 12px;
      font-weight: 700;
      letter-spacing: 0;
      outline: none;
      white-space: nowrap;
    }

    button.queue-track {
      display: grid;
      grid-template-columns: 28px minmax(0, 1fr) auto;
      gap: 8px;
      place-items: initial;
      align-items: center;
      width: 100%;
      text-align: left;
    }

    .mode-controls button {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      gap: 6px;
    }

    button.icon {
      font-size: 15px;
    }

    button.primary {
      width: 58px;
      height: 44px;
      border-color: var(--accent);
      background: var(--accent);
      color: #07130f;
      font-size: 18px;
    }

    button.compact {
      height: 30px;
      font-size: 11px;
    }

    button:active:not(:disabled) {
      transform: translateY(1px);
    }

    button:hover:not(:disabled) {
      border-color: var(--accent);
    }

    button:disabled,
    select:disabled {
      color: #727b86;
      opacity: 0.48;
    }

    button.on {
      border-color: var(--accent);
      color: var(--accent);
    }

    button.mode.on {
      border-color: var(--accent-2);
      color: var(--accent-2);
    }

    .error {
      min-height: 16px;
      overflow: hidden;
      color: var(--danger);
      font-size: 12px;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
  </style>
</head>
<body>
  <div class="shell">
    <header>
      <div class="brand">YtmTui</div>
      <div class="status" id="stateLabel">Disconnected</div>
    </header>

    <div class="tabs" role="tablist">
      <button class="tab active" data-tab="now" id="tabNow">Now</button>
      <button class="tab" data-tab="queue" id="tabQueue">Queue</button>
      <button class="tab" data-tab="streaming" id="tabStreaming">Streaming</button>
      <button class="tab" data-tab="playback" id="tabPlayback">Playback</button>
    </div>

    <main>
      <section class="tab-panel active" data-panel="now">
        <div class="title" id="title">ytm-tui is not running</div>
        <div class="artist" id="artist">YtmTui</div>
        <div class="meta">
          <span id="ownerLabel">Offline</span>
          <span>&#183;</span>
          <span id="queueLabel">Queue unavailable</span>
          <span>&#183;</span>
          <span id="volumeLabel">--</span>
        </div>

        <div class="transport">
          <button class="icon" data-action="previous" id="previous" title="Previous" aria-label="Previous">&#9664;&#9664;</button>
          <button class="primary" data-action="play_pause" id="playPause" title="Play or pause" aria-label="Play or pause">&#9654;</button>
          <button class="icon" data-action="next" id="next" title="Next" aria-label="Next">&#9654;&#9654;</button>
        </div>

        <div class="mode-controls">
          <button data-action="toggle_shuffle" id="shuffle" title="Shuffle" aria-label="Toggle shuffle">&#8644; Shuffle</button>
          <button data-action="cycle_repeat" id="repeat" title="Repeat" aria-label="Cycle repeat">&#8635; <span id="repeatText">Repeat Off</span></button>
        </div>

        <div class="secondary">
          <button data-action="seek_back" id="seekBack" title="Seek back">-10</button>
          <button data-action="seek_forward" id="seekForward" title="Seek forward">+10</button>
          <button data-action="volume_down" id="volumeDown" title="Volume down">Vol-</button>
          <button data-action="volume_up" id="volumeUp" title="Volume up">Vol+</button>
          <button data-action="set_streaming" id="streaming" title="Streaming">Stream</button>
          <button data-action="refresh" id="refresh" title="Refresh">&#8635;</button>
          <button data-action="hide" id="hide" title="Hide">&#10005;</button>
        </div>

        <div class="error" id="error" hidden></div>
      </section>

      <section class="tab-panel" data-panel="queue">
        <div class="queue-head">
          <span class="label" id="queueSummary">Queue unavailable</span>
          <button class="compact" data-action="refresh" id="queueRefresh" title="Refresh">&#8635;</button>
        </div>
        <div class="queue-list" id="queueList"></div>
      </section>

      <section class="tab-panel" data-panel="streaming">
        <div class="settings">
          <div class="row">
            <span class="label">Streaming</span>
            <button data-action="set_streaming" id="streamingToggle">Off</button>
          </div>
          <div class="row">
            <span class="label">Mode</span>
            <div class="segmented">
              <button class="compact mode" data-action="set_streaming_mode" data-value="focused" id="modeFocused">Focused</button>
              <button class="compact mode" data-action="set_streaming_mode" data-value="balanced" id="modeBalanced">Balanced</button>
              <button class="compact mode" data-action="set_streaming_mode" data-value="discovery" id="modeDiscovery">Discovery</button>
            </div>
          </div>
          <div class="row">
            <span class="label">Source</span>
            <select id="streamingSource" data-action="set_streaming_source"></select>
          </div>
          <div class="row">
            <span class="label">DJ Gem</span>
            <button data-action="set_ai_enabled" id="aiEnabled">On</button>
          </div>
          <div class="row">
            <span class="label">Radio mode</span>
            <button data-action="set_radio_mode" id="radioMode">Off</button>
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
            <span class="label">Seek</span>
            <div class="stepper">
              <button data-action="seek_delta" data-delta="-1" id="seekDown">-</button>
              <span class="value" id="seekLabel">10s</span>
              <button data-action="seek_delta" data-delta="1" id="seekUp">+</button>
            </div>
          </div>
          <div class="row">
            <span class="label">Normalize</span>
            <button data-action="set_normalize" id="normalize">Off</button>
          </div>
          <div class="row">
            <span class="label">Gapless</span>
            <button data-action="set_gapless" id="gapless">On</button>
          </div>
        </div>
      </section>
    </main>

    <div class="daemon">
      <button data-action="start_daemon" id="startDaemon">Start</button>
      <button data-action="resume_daemon" id="resumeDaemon">Resume</button>
      <button data-action="stop_daemon" id="stopDaemon">Stop</button>
      <button data-action="open_tui" id="openTui">Open TUI</button>
    </div>
  </div>

  <script>
    window.__YTM_TUI_INITIAL__ = __INITIAL_PAYLOAD__;

    let currentPayload = window.__YTM_TUI_INITIAL__;

    const els = {
      stateLabel: document.getElementById("stateLabel"),
      title: document.getElementById("title"),
      artist: document.getElementById("artist"),
      ownerLabel: document.getElementById("ownerLabel"),
      queueLabel: document.getElementById("queueLabel"),
      volumeLabel: document.getElementById("volumeLabel"),
      error: document.getElementById("error"),
      previous: document.getElementById("previous"),
      playPause: document.getElementById("playPause"),
      next: document.getElementById("next"),
      shuffle: document.getElementById("shuffle"),
      repeat: document.getElementById("repeat"),
      repeatText: document.getElementById("repeatText"),
      queueSummary: document.getElementById("queueSummary"),
      queueList: document.getElementById("queueList"),
      queueRefresh: document.getElementById("queueRefresh"),
      seekBack: document.getElementById("seekBack"),
      seekForward: document.getElementById("seekForward"),
      volumeDown: document.getElementById("volumeDown"),
      volumeUp: document.getElementById("volumeUp"),
      streaming: document.getElementById("streaming"),
      streamingToggle: document.getElementById("streamingToggle"),
      modeFocused: document.getElementById("modeFocused"),
      modeBalanced: document.getElementById("modeBalanced"),
      modeDiscovery: document.getElementById("modeDiscovery"),
      streamingSource: document.getElementById("streamingSource"),
      aiEnabled: document.getElementById("aiEnabled"),
      radioMode: document.getElementById("radioMode"),
      speedDown: document.getElementById("speedDown"),
      speedUp: document.getElementById("speedUp"),
      speedLabel: document.getElementById("speedLabel"),
      seekDown: document.getElementById("seekDown"),
      seekUp: document.getElementById("seekUp"),
      seekLabel: document.getElementById("seekLabel"),
      normalize: document.getElementById("normalize"),
      gapless: document.getElementById("gapless"),
      startDaemon: document.getElementById("startDaemon"),
      resumeDaemon: document.getElementById("resumeDaemon"),
      stopDaemon: document.getElementById("stopDaemon")
    };

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

    function renderSourceOptions(settings) {
      const selected = settings.streamingSource;
      const html = settings.streamingSources.map(source => {
        const attr = source.value === selected ? " selected" : "";
        return `<option value="${source.value}"${attr}>${source.label}</option>`;
      }).join("");
      if (els.streamingSource.innerHTML !== html) {
        els.streamingSource.innerHTML = html;
      }
      els.streamingSource.value = selected;
    }

    function escapeHtml(value) {
      return String(value ?? "")
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;")
        .replace(/"/g, "&quot;")
        .replace(/'/g, "&#39;");
    }

    function renderQueue(payload) {
      els.queueSummary.textContent = payload.queueLabel;
      if (!payload.queue.length) {
        els.queueList.innerHTML = `<div class="empty">Queue empty</div>`;
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

    function apply(payload) {
      currentPayload = payload;
      const settings = payload.settings;

      els.stateLabel.textContent = payload.stateLabel;
      els.title.textContent = payload.title;
      els.artist.textContent = payload.artist;
      els.ownerLabel.textContent = payload.ownerLabel;
      els.queueLabel.textContent = payload.queueLabel;
      els.volumeLabel.textContent = payload.volumeLabel;
      els.playPause.innerHTML = payload.paused ? "&#9654;" : "&#10073;&#10073;";
      els.repeatText.textContent = "Repeat " + payload.repeatLabel;
      renderQueue(payload);

      els.shuffle.innerHTML = "&#8644; Shuffle";
      els.shuffle.classList.toggle("on", payload.shuffle);
      els.repeat.classList.toggle("on", payload.repeat !== "off");
      setToggle(els.streaming, settings.autoplayStreaming, "Stream", "Stream");
      setToggle(els.streamingToggle, settings.autoplayStreaming, "On", "Off");
      setToggle(els.aiEnabled, settings.aiEnabled, "On", "Off");
      setToggle(els.radioMode, settings.radioMode, "On", "Off");
      setToggle(els.normalize, settings.normalize, "On", "Off");
      setToggle(els.gapless, settings.gapless, "On", "Off");

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
      enabled(els.seekBack, payload.canPlayback);
      enabled(els.seekForward, payload.canPlayback);
      enabled(els.volumeDown, payload.canVolume);
      enabled(els.volumeUp, payload.canVolume);
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

      if (payload.error) {
        els.error.hidden = false;
        els.error.textContent = payload.error;
      } else {
        els.error.hidden = true;
        els.error.textContent = "";
      }
    }

    document.addEventListener("click", event => {
      const tab = event.target.closest("button[data-tab]");
      if (tab) {
        setTab(tab.dataset.tab);
        return;
      }

      const button = event.target.closest("button[data-action]");
      if (!button || button.disabled) return;
      const action = button.dataset.action;
      const settings = currentPayload.settings;

      if (action === "set_streaming") {
        send(action, !settings.autoplayStreaming);
      } else if (action === "toggle_shuffle" || action === "cycle_repeat") {
        send(action);
      } else if (action === "queue_play" || action === "queue_remove") {
        send(action, Number(button.dataset.position));
      } else if (action === "set_streaming_mode") {
        send(action, button.dataset.value);
      } else if (action === "set_ai_enabled") {
        send(action, !settings.aiEnabled);
      } else if (action === "set_radio_mode") {
        send(action, !settings.radioMode);
      } else if (action === "set_normalize") {
        send(action, !settings.normalize);
      } else if (action === "set_gapless") {
        send(action, !settings.gapless);
      } else if (action === "speed_delta") {
        const next = Math.max(5, Math.min(20, settings.speedTenths + Number(button.dataset.delta)));
        send("set_speed", next);
      } else if (action === "seek_delta") {
        const next = Math.max(1, Math.min(60, settings.seekSeconds + Number(button.dataset.delta)));
        send("set_seek_seconds", next);
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
    fn scripts_escape_html_script_endings() {
        let mut update = playing_update();
        if let TrayState::Connected(status) = &mut update.state {
            status.title = Some("</script><script>alert(1)</script>".to_string());
        }
        let html = html(&update);
        assert!(html.contains("<\\/script>"));
        assert!(!html.contains("</script><script>alert"));

        let script = update_script(&update);
        assert!(script.contains("<\\/script>"));
        assert!(!script.contains("</script><script>alert"));
    }

    #[test]
    fn panel_html_renames_primary_streaming_control() {
        assert!(PANEL_HTML.contains("Streaming"));
        assert!(!PANEL_HTML.contains(">Radio</button>"));
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
