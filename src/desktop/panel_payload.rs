//! Authoritative mini-player snapshot projection and localization.

use serde::Serialize;

use crate::i18n::Language;
use crate::queue::Repeat;
use crate::remote::proto::InstanceMode;
use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;

use super::super::menu_model::{self, MenuAction, TrayState, TrayStateKind};
use super::super::status::PollUpdate;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelPayload {
    pub(super) locale: &'static str,
    pub(super) connected: bool,
    pub(super) state: String,
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) state_label: String,
    pub(super) owner_label: String,
    pub(super) queue_label: String,
    pub(super) volume_label: String,
    pub(super) volume: i64,
    pub(super) elapsed_ms: Option<u64>,
    pub(super) duration_ms: Option<u64>,
    pub(super) is_live: bool,
    pub(super) queue_rev: Option<u64>,
    /// Stable-enough v7 fallback identity used to abandon a local seek preview
    /// when a track changes before the next status sample arrives.
    pub(super) track_identity: String,
    pub(super) can_seek: bool,
    pub(super) queue: Vec<PanelQueueItemPayload>,
    pub(super) shuffle: bool,
    pub(super) repeat: String,
    pub(super) repeat_label: String,
    pub(super) paused: bool,
    pub(super) streaming: bool,
    pub(super) settings: PanelSettingsPayload,
    pub(super) error: Option<String>,
    pub(super) can_playback: bool,
    pub(super) can_volume: bool,
    pub(super) can_manage_queue: bool,
    pub(super) can_toggle_streaming: bool,
    pub(super) can_start_daemon: bool,
    pub(super) can_resume_daemon: bool,
    pub(super) can_stop_daemon: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelSettingsPayload {
    pub(super) autoplay_streaming: bool,
    pub(super) streaming_mode: String,
    pub(super) streaming_mode_label: String,
    pub(super) streaming_source: String,
    pub(super) streaming_source_label: String,
    pub(super) streaming_sources: Vec<PanelOptionPayload>,
    pub(super) speed_tenths: u16,
    pub(super) speed_label: String,
    pub(super) seek_seconds: u16,
    pub(super) seek_label: String,
    pub(super) normalize: bool,
    pub(super) gapless: bool,
    pub(super) ai_enabled: bool,
    pub(super) radio_mode: bool,
    pub(super) can_radio_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelOptionPayload {
    pub(super) value: String,
    pub(super) label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelQueueItemPayload {
    pub(super) index: usize,
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) duration: String,
    pub(super) current: bool,
}

pub fn payload_for_update(update: &PollUpdate) -> PanelPayload {
    payload_for_update_with_language(update, crate::i18n::current())
}

pub(super) fn payload_for_update_with_language(
    update: &PollUpdate,
    language: Language,
) -> PanelPayload {
    let model = menu_model::build_menu(&update.state);
    let status = update.state.status();
    let connected = status.is_some();
    let idle = matches!(update.state.kind(), TrayStateKind::ConnectedIdle);
    let repeat = status.map(|status| status.repeat).unwrap_or_default();

    PanelPayload {
        locale: language_code(language),
        connected,
        state: state_id(&update.state).to_string(),
        title: title_for_state(&update.state, language),
        artist: artist_for_state(&update.state),
        state_label: state_label(&update.state, language),
        owner_label: owner_label(&update.state, language),
        queue_label: queue_label(&update.state, language),
        volume_label: volume_label(&update.state),
        volume: status
            .map(|status| status.volume.clamp(0, 100))
            .unwrap_or(0),
        elapsed_ms: status.and_then(|status| status.elapsed_ms),
        duration_ms: status.and_then(|status| status.duration_ms),
        is_live: status.is_some_and(|status| status.is_live),
        queue_rev: status.and_then(|status| status.queue_rev),
        track_identity: track_identity(&update.state),
        can_seek: action_enabled(&model, MenuAction::PlayPause)
            && status.is_some_and(|status| status.duration_ms.is_some()),
        queue: queue_payload(&update.state),
        shuffle: status.map(|status| status.shuffle).unwrap_or(false),
        repeat: repeat_id(repeat).to_string(),
        repeat_label: repeat_label(repeat, language).to_string(),
        paused: status.map(|status| status.paused).unwrap_or(true),
        streaming: status.map(|status| status.streaming).unwrap_or(false),
        settings: settings_payload(&update.state, language),
        error: update
            .error
            .as_ref()
            .map(|error| control_error_label(error, language)),
        can_playback: action_enabled(&model, MenuAction::PlayPause),
        can_volume: action_enabled(&model, MenuAction::VolumeUp)
            || action_enabled(&model, MenuAction::VolumeDown),
        // Queue mutation is only safe when the core supplied the revision token used by the
        // v8 checked commands.  A legacy v7 snapshot has no revision, so keep playback visible
        // but do not offer a racy play/remove action that could target a different row.
        can_manage_queue: status
            .is_some_and(|status| !status.queue.is_empty() && status.queue_rev.is_some()),
        can_toggle_streaming: action_enabled(&model, MenuAction::ToggleStreaming),
        can_start_daemon: action_enabled(&model, MenuAction::StartDaemon),
        // Trust the menu model: disconnected Resume is gated by the locally verified session
        // capability, while an idle daemon can ask its live owner to restore the cache. Adding a
        // blanket `|| idle` here would light the button up for an idle *standalone TUI*, where
        // resume is always rejected (StandaloneOwner).
        can_resume_daemon: action_enabled(&model, MenuAction::ResumeDaemon)
            || (idle && status.is_some_and(|status| status.owner_mode == InstanceMode::Daemon)),
        can_stop_daemon: action_enabled(&model, MenuAction::StopDaemon),
    }
}

pub(crate) fn track_identity(state: &TrayState) -> String {
    let Some(status) = state.status() else {
        return "disconnected".to_string();
    };
    if let Some(track_id) = status.track_id.as_deref() {
        return format!("v8\u{1f}{track_id}\u{1f}{}", status.position_epoch);
    }
    // Prefer the queue row over the display title: live streams may update their
    // ICY now-playing text without changing the underlying station/track.
    let current = status.queue.iter().find(|item| item.current);
    let title = current
        .map(|item| item.title.as_str())
        .or(status.title.as_deref())
        .unwrap_or_default();
    let artist = current
        .map(|item| item.artist.as_str())
        .or(status.artist.as_deref())
        .unwrap_or_default();
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        status.position,
        title,
        artist,
        status
            .duration_ms
            .map_or(String::new(), |value| value.to_string()),
        status.total
    )
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

fn settings_payload(state: &TrayState, language: Language) -> PanelSettingsPayload {
    let status = state.status();
    let settings = status.map(|status| status.settings).unwrap_or_default();
    let source = settings.streaming_source;
    PanelSettingsPayload {
        autoplay_streaming: settings.autoplay_streaming,
        streaming_mode: streaming_mode_id(settings.streaming_mode).to_string(),
        streaming_mode_label: streaming_mode_label(settings.streaming_mode, language).to_string(),
        streaming_source: search_source_id(source).to_string(),
        streaming_source_label: search_source_label(source, language).to_string(),
        streaming_sources: streaming_source_options(language),
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

fn streaming_source_options(language: Language) -> Vec<PanelOptionPayload> {
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
        label: search_source_label(source, language).to_string(),
    })
    .collect()
}

fn search_source_label(source: SearchSource, language: Language) -> &'static str {
    if source == SearchSource::All {
        tr(
            language,
            "All enabled",
            "활성화된 전체 소스",
            "有効なすべてのソース",
        )
    } else {
        source.label()
    }
}

fn streaming_mode_label(mode: StreamingMode, language: Language) -> &'static str {
    match mode {
        StreamingMode::Focused => tr(language, "Focused", "집중", "集中"),
        StreamingMode::Balanced => tr(language, "Balanced", "균형", "バランス"),
        StreamingMode::Discovery => tr(language, "Discovery", "발견", "発見"),
    }
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

fn repeat_label(repeat: Repeat, language: Language) -> &'static str {
    match repeat {
        Repeat::Off => tr(language, "Off", "끔", "オフ"),
        Repeat::All => tr(language, "All", "전체", "すべて"),
        Repeat::One => tr(language, "One", "한 곡", "1曲"),
    }
}

fn action_enabled(model: &menu_model::MenuModel, action: MenuAction) -> bool {
    model
        .action_item(action)
        .map(|item| item.enabled)
        .unwrap_or(false)
}

fn title_for_state(state: &TrayState, language: Language) -> String {
    match state.status().and_then(|status| status.title.as_deref()) {
        Some(title) if !title.is_empty() => title.to_string(),
        _ if matches!(state, TrayState::Disconnected { .. }) => tr(
            language,
            "YuTuTui! is not running",
            "YuTuTui!가 실행 중이 아닙니다",
            "YuTuTui!は実行されていません",
        )
        .to_string(),
        _ => tr(
            language,
            "Nothing playing",
            "재생 중인 곡 없음",
            "再生中の曲なし",
        )
        .to_string(),
    }
}

fn artist_for_state(state: &TrayState) -> String {
    match state.status().and_then(|status| status.artist.as_deref()) {
        Some(artist) if !artist.is_empty() => artist.to_string(),
        _ => "YuTuTray!".to_string(),
    }
}

fn state_id(state: &TrayState) -> &'static str {
    match state.kind() {
        TrayStateKind::ConnectedPlaying => "playing",
        TrayStateKind::ConnectedPaused => "paused",
        TrayStateKind::ConnectedIdle => "idle",
        TrayStateKind::Disconnected => "disconnected",
    }
}

fn state_label(state: &TrayState, language: Language) -> String {
    match state.kind() {
        TrayStateKind::ConnectedPlaying => tr(language, "Playing", "재생 중", "再生中"),
        TrayStateKind::ConnectedPaused => tr(language, "Paused", "일시 정지", "一時停止"),
        TrayStateKind::ConnectedIdle => tr(language, "Idle", "대기 중", "待機中"),
        TrayStateKind::Disconnected => tr(language, "Disconnected", "연결 안 됨", "未接続"),
    }
    .to_string()
}

fn owner_label(state: &TrayState, language: Language) -> String {
    match state.status().map(|status| status.owner_mode) {
        Some(InstanceMode::Daemon) => tr(language, "Daemon", "데몬", "デーモン").to_string(),
        Some(InstanceMode::StandaloneTui) => "Standalone TUI".to_string(),
        None => tr(language, "Offline", "오프라인", "オフライン").to_string(),
    }
}

fn queue_label(state: &TrayState, language: Language) -> String {
    let Some(status) = state.status() else {
        return tr(
            language,
            "Queue unavailable",
            "대기열 사용 불가",
            "キュー利用不可",
        )
        .to_string();
    };
    if status.total == 0 {
        tr(
            language,
            "Queue empty",
            "대기열 비어 있음",
            "キューは空です",
        )
        .to_string()
    } else {
        format!("{} / {}", status.position, status.total)
    }
}

pub(super) fn control_error_label(
    error: &super::super::control::ControlError,
    language: Language,
) -> String {
    use super::super::control::ControlError;

    if let ControlError::Rejected(reason) = error {
        let translated = match (reason.as_str(), language) {
            ("stale_rev", Language::English) => {
                Some("The queue changed. It was refreshed; try again.")
            }
            ("stale_rev", Language::Korean) => {
                Some("대기열이 변경되어 새로고침했습니다. 다시 시도해 주세요.")
            }
            ("incompatible_playback_modes", Language::English) => {
                Some("Autoplay and repeat cannot be used together. Turn one off and try again.")
            }
            ("incompatible_playback_modes", Language::Korean) => {
                Some("자동 재생과 반복은 함께 사용할 수 없습니다. 하나를 끄고 다시 시도해 주세요.")
            }
            ("backpressure", Language::English) => {
                Some("The player is busy. Wait a moment and try again.")
            }
            ("backpressure", Language::Korean) => {
                Some("플레이어가 바쁩니다. 잠시 후 다시 시도해 주세요.")
            }
            ("stale_rev", Language::Japanese) => {
                Some("キューが変更されたため更新しました。もう一度お試しください。")
            }
            ("incompatible_playback_modes", Language::Japanese) => Some(
                "自動再生とリピートは同時に使用できません。どちらかをオフにしてもう一度お試しください。",
            ),
            ("backpressure", Language::Japanese) => {
                Some("プレイヤーがビジー状態です。しばらくしてからもう一度お試しください。")
            }
            _ => None,
        };
        if let Some(message) = translated {
            return message.to_string();
        }
    }

    match language {
        Language::English => error.to_string(),
        Language::Korean => match error {
            ControlError::NotRunning => "YuTuTui!가 실행 중이 아닙니다".to_string(),
            ControlError::StaleInstance => "저장된 YuTuTui! 인스턴스가 만료되었습니다".to_string(),
            ControlError::Rejected(reason) => format!("명령이 거부되었습니다: {reason}"),
            ControlError::MissingStatus => "상태 응답을 받지 못했습니다".to_string(),
            ControlError::Transport(message) => message.clone(),
        },
        Language::Japanese => match error {
            ControlError::NotRunning => "YuTuTui!は実行されていません".to_string(),
            ControlError::StaleInstance => {
                "保存されたYuTuTui!インスタンスは無効になりました".to_string()
            }
            ControlError::Rejected(reason) => format!("コマンドが拒否されました: {reason}"),
            ControlError::MissingStatus => "ステータス応答を受信できませんでした".to_string(),
            ControlError::Transport(message) => message.clone(),
        },
    }
}

fn volume_label(state: &TrayState) -> String {
    state
        .status()
        .map(|status| format!("{}%", status.volume))
        .unwrap_or_else(|| "--".to_string())
}

pub(super) fn language_code(language: Language) -> &'static str {
    match language {
        Language::English => "en",
        Language::Korean => "ko",
        Language::Japanese => "ja",
    }
}

fn tr(
    language: Language,
    english: &'static str,
    korean: &'static str,
    japanese: &'static str,
) -> &'static str {
    match language {
        Language::Korean => korean,
        Language::Japanese => japanese,
        Language::English => english,
    }
}
