//! Theme presets and user-editable color roles.
//!
//! The UI should not hard-code terminal colors. Views ask for semantic roles here, while
//! config stores a preset plus per-role `#RRGGBB` overrides.

use std::collections::BTreeMap;

use ratatui::style::{Color, Style};
use serde::{Deserialize, Serialize};

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
            .and_then(|s| normalize_hex(s))
            .unwrap_or_else(|| role.default_hex(self.preset_enum()).to_owned())
    }

    pub fn color(&self, role: ThemeRole) -> Color {
        color_from_hex(&self.effective_hex(role)).unwrap_or(Color::Reset)
    }

    pub fn style(&self, role: ThemeRole) -> Style {
        Style::default()
            .fg(self.color(role))
            .bg(self.color(ThemeRole::Background))
    }

    pub fn set_override(&mut self, role: ThemeRole, value: &str) -> Result<(), String> {
        let Some(hex) = normalize_hex(value) else {
            return Err(format!("Invalid color for {}: use #RRGGBB", role.label()));
        };
        if hex.eq_ignore_ascii_case(role.default_hex(self.preset_enum())) {
            self.overrides.remove(role.id());
        } else {
            self.overrides.insert(role.id().to_owned(), hex);
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
            if let Some(hex) = self
                .overrides
                .get(role.id())
                .and_then(|value| normalize_hex(value))
                && !hex.eq_ignore_ascii_case(role.default_hex(preset))
            {
                overrides.insert(role.id().to_owned(), hex);
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
}

impl ThemePreset {
    pub const ALL: [ThemePreset; 5] = [
        ThemePreset::Default,
        ThemePreset::Midnight,
        ThemePreset::Light,
        ThemePreset::HighContrast,
        ThemePreset::TerminalGreen,
    ];

    pub fn id(self) -> &'static str {
        match self {
            ThemePreset::Default => "default",
            ThemePreset::Midnight => "midnight",
            ThemePreset::Light => "light",
            ThemePreset::HighContrast => "high_contrast",
            ThemePreset::TerminalGreen => "terminal_green",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemePreset::Default => "Default",
            ThemePreset::Midnight => "Midnight",
            ThemePreset::Light => "Light",
            ThemePreset::HighContrast => "High Contrast",
            ThemePreset::TerminalGreen => "Terminal Green",
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
            ThemeRole::Background => "Background",
            ThemeRole::TextPrimary => "Text primary",
            ThemeRole::TextMuted => "Text muted",
            ThemeRole::TextSubtle => "Text subtle",
            ThemeRole::TextInverse => "Text inverse",
            ThemeRole::BorderPrimary => "Border primary",
            ThemeRole::BorderFocused => "Border focused",
            ThemeRole::BorderMuted => "Border muted",
            ThemeRole::Accent => "Accent",
            ThemeRole::AccentAlt => "Accent alt",
            ThemeRole::Success => "Success",
            ThemeRole::Warning => "Warning",
            ThemeRole::Error => "Error",
            ThemeRole::SelectionFg => "Selection text",
            ThemeRole::SelectionBg => "Selection background",
            ThemeRole::SelectionInactiveFg => "Inactive selection text",
            ThemeRole::SelectionInactiveBg => "Inactive selection background",
            ThemeRole::GaugeFilled => "Seekbar filled",
            ThemeRole::GaugeEmpty => "Seekbar empty",
            ThemeRole::PlayerControl => "Player controls",
            ThemeRole::PlayerLabel => "Player labels",
            ThemeRole::HelpGroup => "Help group",
            ThemeRole::HelpKey => "Help key",
            ThemeRole::HelpAction => "Help action",
            ThemeRole::SettingsGroup => "Settings group",
            ThemeRole::SettingsLabel => "Settings label",
            ThemeRole::SettingsValue => "Settings value",
            ThemeRole::SettingsValueFocused => "Settings focused value",
            ThemeRole::AiUser => "AI user",
            ThemeRole::AiAssistant => "AI assistant",
            ThemeRole::AiError => "AI error",
            ThemeRole::AiThinking => "AI thinking",
            ThemeRole::LyricsCurrent => "Lyrics current",
            ThemeRole::LyricsDim => "Lyrics dim",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            ThemeRole::Background => "screen and panel background",
            ThemeRole::TextPrimary => "normal foreground text",
            ThemeRole::TextMuted => "quiet hints and empty states",
            ThemeRole::TextSubtle => "secondary labels",
            ThemeRole::TextInverse => "text drawn on accent fills",
            ThemeRole::BorderPrimary => "main screen and popup borders",
            ThemeRole::BorderFocused => "focused input/list borders",
            ThemeRole::BorderMuted => "inactive input/list borders",
            ThemeRole::Accent => "cyan-style emphasis",
            ThemeRole::AccentAlt => "magenta-style emphasis",
            ThemeRole::Success => "positive state",
            ThemeRole::Warning => "warnings and loading",
            ThemeRole::Error => "errors",
            ThemeRole::SelectionFg => "focused selected row text",
            ThemeRole::SelectionBg => "focused selected row background",
            ThemeRole::SelectionInactiveFg => "unfocused selected row text",
            ThemeRole::SelectionInactiveBg => "unfocused selected row background",
            ThemeRole::GaugeFilled => "filled seekbar",
            ThemeRole::GaugeEmpty => "empty seekbar",
            ThemeRole::PlayerControl => "transport button text",
            ThemeRole::PlayerLabel => "player status labels",
            ThemeRole::HelpGroup => "help section headers",
            ThemeRole::HelpKey => "help key column",
            ThemeRole::HelpAction => "help action names",
            ThemeRole::SettingsGroup => "settings/key group names",
            ThemeRole::SettingsLabel => "settings row labels",
            ThemeRole::SettingsValue => "settings row values",
            ThemeRole::SettingsValueFocused => "focused settings value",
            ThemeRole::AiUser => "user messages",
            ThemeRole::AiAssistant => "assistant messages",
            ThemeRole::AiError => "assistant errors",
            ThemeRole::AiThinking => "assistant thinking",
            ThemeRole::LyricsCurrent => "current lyric line",
            ThemeRole::LyricsDim => "non-current lyric lines",
        }
    }

    pub fn default_hex(self, preset: ThemePreset) -> &'static str {
        match preset {
            ThemePreset::Default => self.default_dark(),
            ThemePreset::Midnight => self.midnight(),
            ThemePreset::Light => self.light(),
            ThemePreset::HighContrast => self.high_contrast(),
            ThemePreset::TerminalGreen => self.terminal_green(),
        }
    }

    fn default_dark(self) -> &'static str {
        match self {
            ThemeRole::Background => "#000000",
            ThemeRole::TextPrimary => "#FFFFFF",
            ThemeRole::TextMuted => "#555555",
            ThemeRole::TextSubtle => "#808080",
            ThemeRole::TextInverse => "#000000",
            ThemeRole::BorderPrimary | ThemeRole::BorderFocused | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#FF00FF",
            ThemeRole::BorderMuted | ThemeRole::GaugeEmpty | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#555555",
            ThemeRole::Accent | ThemeRole::PlayerLabel | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused | ThemeRole::AiUser | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#00FFFF",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#00FF00",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#FFFF00",
            ThemeRole::Error | ThemeRole::AiError => "#FF0000",
            ThemeRole::SelectionFg => "#000000",
            ThemeRole::SelectionInactiveFg | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue | ThemeRole::HelpAction => "#FFFFFF",
            ThemeRole::SettingsLabel => "#808080",
        }
    }

    fn midnight(self) -> &'static str {
        match self {
            ThemeRole::Background => "#0B1020",
            ThemeRole::TextPrimary => "#E6EDF7",
            ThemeRole::TextMuted => "#64748B",
            ThemeRole::TextSubtle => "#94A3B8",
            ThemeRole::TextInverse => "#07111F",
            ThemeRole::BorderPrimary | ThemeRole::BorderFocused | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#F472B6",
            ThemeRole::BorderMuted | ThemeRole::GaugeEmpty | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#243044",
            ThemeRole::Accent | ThemeRole::PlayerLabel | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused | ThemeRole::AiUser | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#38BDF8",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#22C55E",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#FACC15",
            ThemeRole::Error | ThemeRole::AiError => "#FB7185",
            ThemeRole::SelectionFg => "#0B1020",
            ThemeRole::SelectionInactiveFg | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue | ThemeRole::HelpAction => "#E6EDF7",
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
            ThemeRole::BorderPrimary | ThemeRole::BorderFocused | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#C026D3",
            ThemeRole::BorderMuted | ThemeRole::GaugeEmpty | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#D1D5DB",
            ThemeRole::Accent | ThemeRole::PlayerLabel | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused | ThemeRole::AiUser | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#0284C7",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#15803D",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#A16207",
            ThemeRole::Error | ThemeRole::AiError => "#DC2626",
            ThemeRole::SelectionFg => "#FFFFFF",
            ThemeRole::SelectionInactiveFg | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue | ThemeRole::HelpAction => "#16181D",
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
            ThemeRole::BorderPrimary | ThemeRole::BorderFocused | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#FFFF00",
            ThemeRole::BorderMuted | ThemeRole::GaugeEmpty | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#404040",
            ThemeRole::Accent | ThemeRole::PlayerLabel | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused | ThemeRole::AiUser | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#00FFFF",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#00FF00",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#FFFF00",
            ThemeRole::Error | ThemeRole::AiError => "#FF4040",
            ThemeRole::SelectionFg => "#000000",
            ThemeRole::SelectionInactiveFg | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue | ThemeRole::HelpAction => "#FFFFFF",
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
            ThemeRole::BorderPrimary | ThemeRole::BorderFocused | ThemeRole::AccentAlt
            | ThemeRole::SelectionBg => "#00FF66",
            ThemeRole::BorderMuted | ThemeRole::GaugeEmpty | ThemeRole::LyricsDim
            | ThemeRole::SelectionInactiveBg => "#134A2C",
            ThemeRole::Accent | ThemeRole::PlayerLabel | ThemeRole::HelpGroup
            | ThemeRole::SettingsValueFocused | ThemeRole::AiUser | ThemeRole::LyricsCurrent
            | ThemeRole::SettingsGroup => "#7CFF9B",
            ThemeRole::Success | ThemeRole::GaugeFilled | ThemeRole::AiAssistant => "#00FF66",
            ThemeRole::Warning | ThemeRole::HelpKey | ThemeRole::AiThinking => "#D6FF5C",
            ThemeRole::Error | ThemeRole::AiError => "#FF5C7A",
            ThemeRole::SelectionFg => "#001A10",
            ThemeRole::SelectionInactiveFg | ThemeRole::PlayerControl
            | ThemeRole::SettingsValue | ThemeRole::HelpAction => "#D7FFE4",
            ThemeRole::SettingsLabel => "#78B88F",
        }
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
        assert_eq!(cfg.effective_hex(ThemeRole::BorderPrimary), "#FF00FF");
        cfg.set_preset(ThemePreset::Light);
        assert_eq!(cfg.effective_hex(ThemeRole::BorderPrimary), "#C026D3");
        cfg.set_override(ThemeRole::BorderPrimary, "#123456").unwrap();
        assert_eq!(cfg.effective_hex(ThemeRole::BorderPrimary), "#123456");
    }

    #[test]
    fn invalid_overrides_are_dropped_when_normalized() {
        let mut cfg = ThemeConfig::default();
        cfg.overrides.insert("border_primary".to_owned(), "not-a-color".to_owned());
        cfg.overrides.insert("text_primary".to_owned(), "#eeeeee".to_owned());
        let normalized = cfg.normalized();
        assert_eq!(normalized.overrides.get("border_primary"), None);
        assert_eq!(normalized.overrides.get("text_primary").map(String::as_str), Some("#EEEEEE"));
    }
}
