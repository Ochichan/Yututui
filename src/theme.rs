//! Theme presets and user-editable color roles.
//!
//! The UI should not hard-code terminal colors. Views ask for semantic roles here, while
//! config stores a preset plus per-role `#RRGGBB` overrides.

use std::collections::BTreeMap;

use ratatui::style::{Color, Style};
use serde::{Deserialize, Serialize};

use crate::t;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ThemeConfig {
    pub preset: String,
    pub overrides: BTreeMap<String, String>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            preset: ThemePreset::Default.id().to_owned(),
            overrides: BTreeMap::new(),
        }
    }
}

impl ThemeConfig {
    pub fn preset_enum(&self) -> ThemePreset {
        ThemePreset::from_id(&self.preset).unwrap_or(ThemePreset::Default)
    }

    pub fn set_preset(&mut self, preset: ThemePreset) {
        self.preset = preset.id().to_owned();
    }

    pub fn effective_hex(&self, role: ThemeRole) -> String {
        self.overrides
            .get(role.id())
            .and_then(|s| normalize_value(s))
            .unwrap_or_else(|| role.default_hex(self.preset_enum()).to_owned())
    }

    /// Whether `role` resolves to "no color" — i.e. [`Color::Reset`], letting the terminal's
    /// own background/foreground show through. Used to render a transparent swatch.
    pub fn is_role_transparent(&self, role: ThemeRole) -> bool {
        is_transparent(&self.effective_hex(role))
    }

    pub fn color(&self, role: ThemeRole) -> Color {
        let value = self.effective_hex(role);
        if is_transparent(&value) {
            Color::Reset
        } else {
            color_from_hex(&value).unwrap_or(Color::Reset)
        }
    }

    pub fn style(&self, role: ThemeRole) -> Style {
        Style::default()
            .fg(self.color(role))
            .bg(self.color(ThemeRole::Background))
    }

    pub fn set_override(&mut self, role: ThemeRole, value: &str) -> Result<(), String> {
        let Some(canonical) = normalize_value(value) else {
            return Err(if crate::i18n::is_korean() {
                format!(
                    "{} 색상이 올바르지 않습니다: #RRGGBB 또는 none 사용",
                    role.label()
                )
            } else {
                format!("Invalid color for {}: use #RRGGBB or none", role.label())
            });
        };
        if canonical.eq_ignore_ascii_case(role.default_hex(self.preset_enum())) {
            self.overrides.remove(role.id());
        } else {
            self.overrides.insert(role.id().to_owned(), canonical);
        }
        Ok(())
    }

    pub fn reset_role(&mut self, role: ThemeRole) {
        self.overrides.remove(role.id());
    }

    pub fn ensure_override_for_edit(&mut self, role: ThemeRole) {
        let value = self.effective_hex(role);
        self.overrides.entry(role.id().to_owned()).or_insert(value);
    }

    pub fn normalized(&self) -> Self {
        let preset = self.preset_enum();
        let mut overrides = BTreeMap::new();
        for role in ThemeRole::ALL {
            if let Some(value) = self
                .overrides
                .get(role.id())
                .and_then(|value| normalize_value(value))
                && !value.eq_ignore_ascii_case(role.default_hex(preset))
            {
                overrides.insert(role.id().to_owned(), value);
            }
        }
        Self {
            preset: preset.id().to_owned(),
            overrides,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePreset {
    Default,
    Midnight,
    Light,
    HighContrast,
    TerminalGreen,
    Gruvbox,
    Nord,
    Dracula,
    TokyoNight,
    Solarized,
    RosePine,
}

impl ThemePreset {
    pub const ALL: [ThemePreset; 11] = [
        ThemePreset::Default,
        ThemePreset::Midnight,
        ThemePreset::Light,
        ThemePreset::HighContrast,
        ThemePreset::TerminalGreen,
        ThemePreset::Gruvbox,
        ThemePreset::Nord,
        ThemePreset::Dracula,
        ThemePreset::TokyoNight,
        ThemePreset::Solarized,
        ThemePreset::RosePine,
    ];

    pub fn id(self) -> &'static str {
        match self {
            ThemePreset::Default => "default",
            ThemePreset::Midnight => "midnight",
            ThemePreset::Light => "light",
            ThemePreset::HighContrast => "high_contrast",
            ThemePreset::TerminalGreen => "terminal_green",
            ThemePreset::Gruvbox => "gruvbox",
            ThemePreset::Nord => "nord",
            ThemePreset::Dracula => "dracula",
            ThemePreset::TokyoNight => "tokyo_night",
            ThemePreset::Solarized => "solarized_dark",
            ThemePreset::RosePine => "rose_pine",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemePreset::Default => "Default",
            ThemePreset::Midnight => "Midnight",
            ThemePreset::Light => "Light",
            ThemePreset::HighContrast => "High Contrast",
            ThemePreset::TerminalGreen => "Terminal Green",
            ThemePreset::Gruvbox => "Gruvbox",
            ThemePreset::Nord => "Nord",
            ThemePreset::Dracula => "Dracula",
            ThemePreset::TokyoNight => "Tokyo Night",
            ThemePreset::Solarized => "Solarized Dark",
            ThemePreset::RosePine => "Rosé Pine",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.id() == id)
    }

    pub fn stepped(self, dir: i32) -> Self {
        let n = Self::ALL.len();
        let i = Self::ALL.iter().position(|&p| p == self).unwrap_or(0);
        let next = if dir >= 0 {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        Self::ALL[next]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeRole {
    Background,
    TextPrimary,
    TextMuted,
    TextSubtle,
    TextInverse,
    BorderPrimary,
    BorderFocused,
    BorderMuted,
    Accent,
    AccentAlt,
    Success,
    Warning,
    Error,
    SelectionFg,
    SelectionBg,
    SelectionInactiveFg,
    SelectionInactiveBg,
    GaugeFilled,
    GaugeEmpty,
    PlayerControl,
    PlayerLabel,
    HelpGroup,
    HelpKey,
    HelpAction,
    SettingsGroup,
    SettingsLabel,
    SettingsValue,
    SettingsValueFocused,
    AiUser,
    AiAssistant,
    AiError,
    AiThinking,
    LyricsCurrent,
    LyricsDim,
}

impl ThemeRole {
    pub const ALL: [ThemeRole; 34] = [
        ThemeRole::Background,
        ThemeRole::TextPrimary,
        ThemeRole::TextMuted,
        ThemeRole::TextSubtle,
        ThemeRole::TextInverse,
        ThemeRole::BorderPrimary,
        ThemeRole::BorderFocused,
        ThemeRole::BorderMuted,
        ThemeRole::Accent,
        ThemeRole::AccentAlt,
        ThemeRole::Success,
        ThemeRole::Warning,
        ThemeRole::Error,
        ThemeRole::SelectionFg,
        ThemeRole::SelectionBg,
        ThemeRole::SelectionInactiveFg,
        ThemeRole::SelectionInactiveBg,
        ThemeRole::GaugeFilled,
        ThemeRole::GaugeEmpty,
        ThemeRole::PlayerControl,
        ThemeRole::PlayerLabel,
        ThemeRole::HelpGroup,
        ThemeRole::HelpKey,
        ThemeRole::HelpAction,
        ThemeRole::SettingsGroup,
        ThemeRole::SettingsLabel,
        ThemeRole::SettingsValue,
        ThemeRole::SettingsValueFocused,
        ThemeRole::AiUser,
        ThemeRole::AiAssistant,
        ThemeRole::AiError,
        ThemeRole::AiThinking,
        ThemeRole::LyricsCurrent,
        ThemeRole::LyricsDim,
    ];

    pub fn id(self) -> &'static str {
        match self {
            ThemeRole::Background => "background",
            ThemeRole::TextPrimary => "text_primary",
            ThemeRole::TextMuted => "text_muted",
            ThemeRole::TextSubtle => "text_subtle",
            ThemeRole::TextInverse => "text_inverse",
            ThemeRole::BorderPrimary => "border_primary",
            ThemeRole::BorderFocused => "border_focused",
            ThemeRole::BorderMuted => "border_muted",
            ThemeRole::Accent => "accent",
            ThemeRole::AccentAlt => "accent_alt",
            ThemeRole::Success => "success",
            ThemeRole::Warning => "warning",
            ThemeRole::Error => "error",
            ThemeRole::SelectionFg => "selection_fg",
            ThemeRole::SelectionBg => "selection_bg",
            ThemeRole::SelectionInactiveFg => "selection_inactive_fg",
            ThemeRole::SelectionInactiveBg => "selection_inactive_bg",
            ThemeRole::GaugeFilled => "gauge_filled",
            ThemeRole::GaugeEmpty => "gauge_empty",
            ThemeRole::PlayerControl => "player_control",
            ThemeRole::PlayerLabel => "player_label",
            ThemeRole::HelpGroup => "help_group",
            ThemeRole::HelpKey => "help_key",
            ThemeRole::HelpAction => "help_action",
            ThemeRole::SettingsGroup => "settings_group",
            ThemeRole::SettingsLabel => "settings_label",
            ThemeRole::SettingsValue => "settings_value",
            ThemeRole::SettingsValueFocused => "settings_value_focused",
            ThemeRole::AiUser => "ai_user",
            ThemeRole::AiAssistant => "ai_assistant",
            ThemeRole::AiError => "ai_error",
            ThemeRole::AiThinking => "ai_thinking",
            ThemeRole::LyricsCurrent => "lyrics_current",
            ThemeRole::LyricsDim => "lyrics_dim",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemeRole::Background => t!("Background", "배경"),
            ThemeRole::TextPrimary => t!("Text primary", "기본 텍스트"),
            ThemeRole::TextMuted => t!("Text muted", "흐린 텍스트"),
            ThemeRole::TextSubtle => t!("Text subtle", "보조 텍스트"),
            ThemeRole::TextInverse => t!("Text inverse", "반전 텍스트"),
            ThemeRole::BorderPrimary => t!("Border primary", "기본 테두리"),
            ThemeRole::BorderFocused => t!("Border focused", "포커스 테두리"),
            ThemeRole::BorderMuted => t!("Border muted", "흐린 테두리"),
            ThemeRole::Accent => t!("Accent", "강조"),
            ThemeRole::AccentAlt => t!("Accent alt", "보조 강조"),
            ThemeRole::Success => t!("Success", "성공"),
            ThemeRole::Warning => t!("Warning", "경고"),
            ThemeRole::Error => t!("Error", "오류"),
            ThemeRole::SelectionFg => t!("Selection text", "선택 텍스트"),
            ThemeRole::SelectionBg => t!("Selection background", "선택 배경"),
            ThemeRole::SelectionInactiveFg => t!("Inactive selection text", "비활성 선택 텍스트"),
            ThemeRole::SelectionInactiveBg => {
                t!("Inactive selection background", "비활성 선택 배경")
            }
            ThemeRole::GaugeFilled => t!("Seekbar filled", "탐색바 채움"),
            ThemeRole::GaugeEmpty => t!("Seekbar empty", "탐색바 빈 부분"),
            ThemeRole::PlayerControl => t!("Player controls", "플레이어 컨트롤"),
            ThemeRole::PlayerLabel => t!("Player labels", "플레이어 라벨"),
            ThemeRole::HelpGroup => t!("Help group", "도움말 그룹"),
            ThemeRole::HelpKey => t!("Help key", "도움말 키"),
            ThemeRole::HelpAction => t!("Help action", "도움말 동작"),
            ThemeRole::SettingsGroup => t!("Settings group", "설정 그룹"),
            ThemeRole::SettingsLabel => t!("Settings label", "설정 라벨"),
            ThemeRole::SettingsValue => t!("Settings value", "설정 값"),
            ThemeRole::SettingsValueFocused => t!("Settings focused value", "설정 포커스 값"),
            ThemeRole::AiUser => t!("AI user", "AI 사용자"),
            ThemeRole::AiAssistant => t!("AI assistant", "AI 어시스턴트"),
            ThemeRole::AiError => t!("AI error", "AI 오류"),
            ThemeRole::AiThinking => t!("AI thinking", "AI 생각 중"),
            ThemeRole::LyricsCurrent => t!("Lyrics current", "현재 가사"),
            ThemeRole::LyricsDim => t!("Lyrics dim", "흐린 가사"),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            ThemeRole::Background => t!("screen and panel background", "화면 및 패널 배경"),
            ThemeRole::TextPrimary => t!("normal foreground text", "일반 전경 텍스트"),
            ThemeRole::TextMuted => t!("quiet hints and empty states", "조용한 힌트와 빈 상태"),
            ThemeRole::TextSubtle => t!("secondary labels", "보조 라벨"),
            ThemeRole::TextInverse => t!("text drawn on accent fills", "강조 채움 위 텍스트"),
            ThemeRole::BorderPrimary => {
                t!("main screen and popup borders", "주 화면 및 팝업 테두리")
            }
            ThemeRole::BorderFocused => {
                t!("focused input/list borders", "포커스된 입력/목록 테두리")
            }
            ThemeRole::BorderMuted => t!("inactive input/list borders", "비활성 입력/목록 테두리"),
            ThemeRole::Accent => t!("cyan-style emphasis", "청록 계열 강조"),
            ThemeRole::AccentAlt => t!("magenta-style emphasis", "자홍 계열 강조"),
            ThemeRole::Success => t!("positive state", "긍정 상태"),
            ThemeRole::Warning => t!("warnings and loading", "경고 및 로딩"),
            ThemeRole::Error => t!("errors", "오류"),
            ThemeRole::SelectionFg => t!("focused selected row text", "포커스된 선택 행 텍스트"),
            ThemeRole::SelectionBg => {
                t!("focused selected row background", "포커스된 선택 행 배경")
            }
            ThemeRole::SelectionInactiveFg => {
                t!("unfocused selected row text", "비포커스 선택 행 텍스트")
            }
            ThemeRole::SelectionInactiveBg => {
                t!("unfocused selected row background", "비포커스 선택 행 배경")
            }
            ThemeRole::GaugeFilled => t!("filled seekbar", "채워진 탐색바"),
            ThemeRole::GaugeEmpty => t!("empty seekbar", "빈 탐색바"),
            ThemeRole::PlayerControl => t!("transport button text", "재생 버튼 텍스트"),
            ThemeRole::PlayerLabel => t!("player status labels", "플레이어 상태 라벨"),
            ThemeRole::HelpGroup => t!("help section headers", "도움말 섹션 헤더"),
            ThemeRole::HelpKey => t!("help key column", "도움말 키 열"),
            ThemeRole::HelpAction => t!("help action names", "도움말 동작 이름"),
            ThemeRole::SettingsGroup => t!("settings/key group names", "설정/키 그룹 이름"),
            ThemeRole::SettingsLabel => t!("settings row labels", "설정 행 라벨"),
            ThemeRole::SettingsValue => t!("settings row values", "설정 행 값"),
            ThemeRole::SettingsValueFocused => t!("focused settings value", "포커스된 설정 값"),
            ThemeRole::AiUser => t!("user messages", "사용자 메시지"),
            ThemeRole::AiAssistant => t!("assistant messages", "어시스턴트 메시지"),
            ThemeRole::AiError => t!("assistant errors", "어시스턴트 오류"),
            ThemeRole::AiThinking => t!("assistant thinking", "어시스턴트 생각 중"),
            ThemeRole::LyricsCurrent => t!("current lyric line", "현재 가사 줄"),
            ThemeRole::LyricsDim => t!("non-current lyric lines", "그 외 가사 줄"),
        }
    }

    pub fn default_hex(self, preset: ThemePreset) -> &'static str {
        match preset {
            ThemePreset::Default => self.default_dark(),
            ThemePreset::Midnight => self.midnight(),
            ThemePreset::Light => self.light(),
            ThemePreset::HighContrast => self.high_contrast(),
            ThemePreset::TerminalGreen => self.terminal_green(),
            ThemePreset::Gruvbox => self.gruvbox(),
            ThemePreset::Nord => self.nord(),
            ThemePreset::Dracula => self.dracula(),
            ThemePreset::TokyoNight => self.tokyo_night(),
            ThemePreset::Solarized => self.solarized(),
            ThemePreset::RosePine => self.rose_pine(),
        }
    }

    fn default_dark(self) -> &'static str {
        // Soft pastel dark palette (Catppuccin Mocha) — low-saturation accents on a
        // muted base so nothing glares; replaces the old pure-neon defaults.
        match self {
            // Transparent by default: inherit the terminal's own background so the app blends
            // with the user's color scheme / wallpaper / opacity. Other roles still carry the
            // Mocha base (`#1E1E2E`) where a concrete dark is needed (e.g. text on accents).
            ThemeRole::Background => "none",
            ThemeRole::TextPrimary => "#CDD6F4",
            // Lifted from overlay0 to overlay1 so quiet hints/empty states stay legible.
            ThemeRole::TextMuted => "#7F849C",
            ThemeRole::TextSubtle => "#A6ADC8",
            ThemeRole::TextInverse => "#1E1E2E",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#CBA6F7",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#45475A",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#89DCEB",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#A6E3A1",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#F9E2AF",
            ThemeRole::Error | ThemeRole::AiError => "#F38BA8",
            ThemeRole::SelectionFg => "#1E1E2E",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#CDD6F4",
            ThemeRole::SettingsLabel => "#A6ADC8",
        }
    }

    fn midnight(self) -> &'static str {
        match self {
            ThemeRole::Background => "#0B1020",
            ThemeRole::TextPrimary => "#E6EDF7",
            // Nudged brighter so muted hints don't disappear against the very dark base.
            ThemeRole::TextMuted => "#7689A3",
            ThemeRole::TextSubtle => "#94A3B8",
            ThemeRole::TextInverse => "#07111F",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#F472B6",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#243044",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#38BDF8",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#22C55E",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#FACC15",
            ThemeRole::Error | ThemeRole::AiError => "#FB7185",
            ThemeRole::SelectionFg => "#0B1020",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#E6EDF7",
            ThemeRole::SettingsLabel => "#94A3B8",
        }
    }

    fn light(self) -> &'static str {
        match self {
            ThemeRole::Background => "#F7F7F2",
            ThemeRole::TextPrimary => "#16181D",
            ThemeRole::TextMuted => "#6B7280",
            ThemeRole::TextSubtle => "#4B5563",
            ThemeRole::TextInverse => "#FFFFFF",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#C026D3",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#D1D5DB",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#0284C7",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#15803D",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#A16207",
            ThemeRole::Error | ThemeRole::AiError => "#DC2626",
            ThemeRole::SelectionFg => "#FFFFFF",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#16181D",
            ThemeRole::SettingsLabel => "#4B5563",
        }
    }

    fn high_contrast(self) -> &'static str {
        match self {
            ThemeRole::Background => "#000000",
            ThemeRole::TextPrimary => "#FFFFFF",
            ThemeRole::TextMuted => "#BDBDBD",
            ThemeRole::TextSubtle => "#E0E0E0",
            ThemeRole::TextInverse => "#000000",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#FFFF00",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#404040",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#00FFFF",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#00FF00",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#FFFF00",
            ThemeRole::Error | ThemeRole::AiError => "#FF4040",
            ThemeRole::SelectionFg => "#000000",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#FFFFFF",
            ThemeRole::SettingsLabel => "#E0E0E0",
        }
    }

    fn terminal_green(self) -> &'static str {
        match self {
            ThemeRole::Background => "#001A10",
            ThemeRole::TextPrimary => "#D7FFE4",
            ThemeRole::TextMuted => "#4E8F68",
            ThemeRole::TextSubtle => "#78B88F",
            ThemeRole::TextInverse => "#001A10",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#00FF66",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#134A2C",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#7CFF9B",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#00FF66",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#D6FF5C",
            ThemeRole::Error | ThemeRole::AiError => "#FF5C7A",
            ThemeRole::SelectionFg => "#001A10",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#D7FFE4",
            ThemeRole::SettingsLabel => "#78B88F",
        }
    }

    fn gruvbox(self) -> &'static str {
        // Retro warm palette: soft cream text on a dark roast base, earthy accents.
        match self {
            ThemeRole::Background => "#282828",
            ThemeRole::TextPrimary => "#EBDBB2",
            ThemeRole::TextMuted => "#928374",
            ThemeRole::TextSubtle => "#A89984",
            ThemeRole::TextInverse => "#282828",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#D3869B",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#504945",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#8EC07C",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#B8BB26",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#FABD2F",
            ThemeRole::Error | ThemeRole::AiError => "#FB4934",
            ThemeRole::SelectionFg => "#282828",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#EBDBB2",
            ThemeRole::SettingsLabel => "#A89984",
        }
    }

    fn nord(self) -> &'static str {
        // Arctic palette: cool desaturated frost accents on a slate base.
        match self {
            ThemeRole::Background => "#2E3440",
            ThemeRole::TextPrimary => "#ECEFF4",
            ThemeRole::TextMuted => "#616E88",
            ThemeRole::TextSubtle => "#D8DEE9",
            ThemeRole::TextInverse => "#2E3440",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#B48EAD",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#434C5E",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#88C0D0",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#A3BE8C",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#EBCB8B",
            ThemeRole::Error | ThemeRole::AiError => "#BF616A",
            ThemeRole::SelectionFg => "#2E3440",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#ECEFF4",
            ThemeRole::SettingsLabel => "#D8DEE9",
        }
    }

    fn dracula(self) -> &'static str {
        // High-energy dark: pink, cyan, and green pops on a deep grey-violet base.
        match self {
            ThemeRole::Background => "#282A36",
            ThemeRole::TextPrimary => "#F8F8F2",
            ThemeRole::TextMuted => "#6272A4",
            ThemeRole::TextSubtle => "#9EA2C9",
            ThemeRole::TextInverse => "#282A36",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#FF79C6",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#44475A",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#8BE9FD",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#50FA7B",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#F1FA8C",
            ThemeRole::Error | ThemeRole::AiError => "#FF5555",
            ThemeRole::SelectionFg => "#282A36",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#F8F8F2",
            ThemeRole::SettingsLabel => "#9EA2C9",
        }
    }

    fn tokyo_night(self) -> &'static str {
        // Calm modern dark: blue-leaning text and a violet accent on near-black indigo.
        match self {
            ThemeRole::Background => "#1A1B26",
            ThemeRole::TextPrimary => "#C0CAF5",
            ThemeRole::TextMuted => "#565F89",
            ThemeRole::TextSubtle => "#A9B1D6",
            ThemeRole::TextInverse => "#1A1B26",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#BB9AF7",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#3B4261",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#7DCFFF",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#9ECE6A",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#E0AF68",
            ThemeRole::Error | ThemeRole::AiError => "#F7768E",
            ThemeRole::SelectionFg => "#1A1B26",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#C0CAF5",
            ThemeRole::SettingsLabel => "#A9B1D6",
        }
    }

    fn solarized(self) -> &'static str {
        // Precision-tuned classic: low-glare teal/blue base with muted ANSI accents.
        match self {
            ThemeRole::Background => "#002B36",
            ThemeRole::TextPrimary => "#93A1A1",
            ThemeRole::TextMuted => "#586E75",
            ThemeRole::TextSubtle => "#839496",
            ThemeRole::TextInverse => "#002B36",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#D33682",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#073642",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#2AA198",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#859900",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#B58900",
            ThemeRole::Error | ThemeRole::AiError => "#DC322F",
            ThemeRole::SelectionFg => "#002B36",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#93A1A1",
            ThemeRole::SettingsLabel => "#839496",
        }
    }

    fn rose_pine(self) -> &'static str {
        // Soho-vibe muted palette: iris/foam/gold pastels on a dusky plum base.
        match self {
            ThemeRole::Background => "#191724",
            ThemeRole::TextPrimary => "#E0DEF4",
            ThemeRole::TextMuted => "#6E6A86",
            ThemeRole::TextSubtle => "#908CAA",
            ThemeRole::TextInverse => "#191724",
            ThemeRole::BorderPrimary
            | ThemeRole::BorderFocused
            | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#C4A7E7",
            ThemeRole::BorderMuted
            | ThemeRole::GaugeEmpty
            | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#403D52",
            ThemeRole::Accent
            | ThemeRole::PlayerLabel
            | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused
            | ThemeRole::AiUser
            | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#9CCFD8",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#31748F",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#F6C177",
            ThemeRole::Error | ThemeRole::AiError => "#EB6F92",
            ThemeRole::SelectionFg => "#191724",
            ThemeRole::SelectionInactiveFg
            | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue
            | ThemeRole::HelpAction => "#E0DEF4",
            ThemeRole::SettingsLabel => "#908CAA",
        }
    }
}

/// Canonical spelling of the "no color" value: the role resolves to [`Color::Reset`] so the
/// terminal's own background/foreground shows through. Mainly used for a transparent base
/// background that inherits the terminal's wallpaper/opacity.
pub const TRANSPARENT: &str = "none";

/// Whether `value` means "no color" (transparent). Accepts a few friendly spellings so the
/// Colors tab can take `none`, `transparent`, or `-` interchangeably.
pub fn is_transparent(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "none" | "transparent" | "-"
    )
}

/// Normalize a user-entered color value: either the transparent sentinel (`none`) or a
/// `#RRGGBB` hex. Returns `None` for anything else.
pub fn normalize_value(value: &str) -> Option<String> {
    if is_transparent(value) {
        Some(TRANSPARENT.to_owned())
    } else {
        normalize_hex(value)
    }
}

pub fn normalize_hex(value: &str) -> Option<String> {
    let raw = value.trim();
    let raw = raw.strip_prefix('#').unwrap_or(raw);
    if raw.len() != 6 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", raw.to_ascii_uppercase()))
}

fn color_from_hex(value: &str) -> Option<Color> {
    let raw = normalize_hex(value)?;
    let hex = raw.trim_start_matches('#');
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors_normalize() {
        assert_eq!(normalize_hex("#ff00aa").as_deref(), Some("#FF00AA"));
        assert_eq!(normalize_hex("00ff66").as_deref(), Some("#00FF66"));
        assert_eq!(normalize_hex("#fff"), None);
        assert_eq!(normalize_hex("#zz00aa"), None);
    }

    #[test]
    fn theme_config_uses_preset_and_overrides() {
        let mut cfg = ThemeConfig::default();
        assert_eq!(cfg.effective_hex(ThemeRole::BorderPrimary), "#CBA6F7");
        cfg.set_preset(ThemePreset::Light);
        assert_eq!(cfg.effective_hex(ThemeRole::BorderPrimary), "#C026D3");
        cfg.set_override(ThemeRole::BorderPrimary, "#123456")
            .unwrap();
        assert_eq!(cfg.effective_hex(ThemeRole::BorderPrimary), "#123456");
    }

    #[test]
    fn every_preset_role_has_a_valid_value() {
        for preset in ThemePreset::ALL {
            for role in ThemeRole::ALL {
                let value = role.default_hex(preset);
                assert!(
                    normalize_value(value).is_some(),
                    "{}/{} has an invalid value {value}",
                    preset.id(),
                    role.id()
                );
            }
        }
    }

    #[test]
    fn background_can_be_transparent() {
        // The Default preset ships with a transparent background.
        let cfg = ThemeConfig::default();
        assert_eq!(cfg.effective_hex(ThemeRole::Background), "none");
        assert!(cfg.is_role_transparent(ThemeRole::Background));
        assert_eq!(cfg.color(ThemeRole::Background), Color::Reset);

        // A preset with a solid base can be overridden to transparent, and back.
        let mut cfg = ThemeConfig::default();
        cfg.set_preset(ThemePreset::Midnight);
        assert!(!cfg.is_role_transparent(ThemeRole::Background));
        cfg.set_override(ThemeRole::Background, "none").unwrap();
        assert!(cfg.is_role_transparent(ThemeRole::Background));
        assert_eq!(cfg.color(ThemeRole::Background), Color::Reset);
        // "transparent" / "-" are accepted spellings too.
        cfg.set_override(ThemeRole::Background, "TRANSPARENT")
            .unwrap();
        assert_eq!(cfg.effective_hex(ThemeRole::Background), "none");
    }

    #[test]
    fn invalid_overrides_are_dropped_when_normalized() {
        let mut cfg = ThemeConfig::default();
        cfg.overrides
            .insert("border_primary".to_owned(), "not-a-color".to_owned());
        cfg.overrides
            .insert("text_primary".to_owned(), "#eeeeee".to_owned());
        let normalized = cfg.normalized();
        assert_eq!(normalized.overrides.get("border_primary"), None);
        assert_eq!(
            normalized.overrides.get("text_primary").map(String::as_str),
            Some("#EEEEEE")
        );
    }
}
