//! Settings-screen state: tabs, editable fields, and a draft the user commits on close.
//!
//! The screen edits a [`SettingsDraft`] — a snapshot of the persisted [`Config`] plus the
//! live audio state — without touching `App`'s committed state until the screen closes.
//! Audio-affecting fields (speed, EQ, normalization) are applied to mpv *live* as they
//! change; closing the screen copies the draft into `App` and persists. Rendering lives in
//! [`crate::ui::views::settings`]; key handling in
//! [`crate::app`].

use std::path::{Path, PathBuf};

use crate::ai::GeminiModel;
use crate::config::{
    Config, SEEK_SECONDS_MAX, SEEK_SECONDS_MIN, SPEED_MAX, SPEED_MIN, default_cookies_file,
    default_download_dir,
};
use crate::eq::{self, EqPreset};
use crate::keymap::{Action, KeyContext, KeyMap};
use crate::theme::{ThemeConfig, ThemeRole};

/// Per-band gain limits and keyboard step (dB), for the EQ sliders.
pub const BAND_GAIN_MIN: f64 = -12.0;
pub const BAND_GAIN_MAX: f64 = 12.0;
pub const BAND_GAIN_STEP: f64 = 1.0;
/// Playback-speed step for the settings slider (←/→).
pub const SPEED_STEP: f64 = 0.1;
/// Seek-step slider step (seconds) for the Playback tab (←/→).
pub const SEEK_SECONDS_STEP: f64 = 1.0;

/// The settings tabs, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Playback,
    Eq,
    Ai,
    Theme,
    Colors,
    Keys,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 7] = [
        SettingsTab::General,
        SettingsTab::Playback,
        SettingsTab::Eq,
        SettingsTab::Ai,
        SettingsTab::Theme,
        SettingsTab::Colors,
        SettingsTab::Keys,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::General => "General",
            SettingsTab::Playback => "Playback",
            SettingsTab::Eq => "EQ",
            SettingsTab::Ai => "AI",
            SettingsTab::Theme => "Theme",
            SettingsTab::Colors => "Colors",
            SettingsTab::Keys => "Keys",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    /// The next/previous tab (wraps), for Tab / BackTab.
    pub fn stepped(self, forward: bool) -> Self {
        let n = Self::ALL.len();
        let i = self.index();
        let j = if forward { (i + 1) % n } else { (i + n - 1) % n };
        Self::ALL[j]
    }

    /// The ordered fields shown under this tab.
    pub fn fields(self) -> Vec<Field> {
        match self {
            SettingsTab::General => vec![
                Field::CookiesFile,
                Field::DownloadDir,
                Field::Mouse,
                Field::AlbumArt,
                Field::AutoplayOnStart,
                Field::ResetKeybindings,
                Field::ResetAll,
            ],
            SettingsTab::Playback => vec![Field::Speed, Field::SeekInterval, Field::Gapless],
            SettingsTab::Eq => {
                let mut f = vec![Field::EqPreset];
                f.extend((0..eq::BANDS).map(Field::Band));
                f.push(Field::Normalize);
                f
            }
            SettingsTab::Ai => vec![Field::GeminiModel, Field::ApiKey, Field::AutoplayRadio],
            SettingsTab::Theme => vec![Field::ThemePreset],
            SettingsTab::Colors => ThemeRole::ALL.iter().copied().map(Field::ThemeColor).collect(),
            // The Keys tab is a list of remappable bindings, not `Field`s; it has its own
            // navigation and rendering paths (see `crate::keymap::editable_entries`).
            SettingsTab::Keys => Vec::new(),
        }
    }
}

/// One editable setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    // General
    CookiesFile,
    DownloadDir,
    Mouse,
    AlbumArt,
    AutoplayOnStart,
    /// A button (not a value): restores all keybindings to their built-in defaults.
    ResetKeybindings,
    /// A button (not a value): activates "reset every setting to defaults".
    ResetAll,
    // Playback
    Speed,
    SeekInterval,
    Gapless,
    // EQ
    EqPreset,
    Band(usize),
    Normalize,
    // AI
    GeminiModel,
    ApiKey,
    AutoplayRadio,
    // Theme
    ThemePreset,
    ThemeColor(ThemeRole),
}

/// How a field is edited / rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Free text edited in-place (paths). Enter toggles edit mode.
    Text,
    /// On/off, flipped with ←/→ or Enter.
    Toggle,
    /// A value cycled through a set with ←/→.
    Select,
    /// A numeric value nudged with ←/→.
    Slider,
    /// A pressable action (no value); Enter/Confirm triggers it.
    Button,
}

impl Field {
    pub fn kind(self) -> FieldKind {
        match self {
            Field::CookiesFile | Field::DownloadDir | Field::ApiKey | Field::ThemeColor(_) => FieldKind::Text,
            Field::Mouse | Field::AlbumArt | Field::AutoplayOnStart | Field::Gapless
            | Field::AutoplayRadio | Field::Normalize => FieldKind::Toggle,
            Field::EqPreset | Field::GeminiModel | Field::ThemePreset => FieldKind::Select,
            Field::Speed | Field::SeekInterval | Field::Band(_) => FieldKind::Slider,
            Field::ResetKeybindings | Field::ResetAll => FieldKind::Button,
        }
    }

    pub fn label(self) -> String {
        match self {
            Field::CookiesFile => "Cookies file".to_owned(),
            Field::DownloadDir => "Download dir".to_owned(),
            Field::Mouse => "Mouse (next launch)".to_owned(),
            Field::AlbumArt => "Album art (next launch)".to_owned(),
            Field::AutoplayOnStart => "Autoplay on launch".to_owned(),
            Field::ResetKeybindings => "Reset keybindings".to_owned(),
            Field::ResetAll => "Reset all settings".to_owned(),
            Field::Speed => "Playback speed".to_owned(),
            Field::SeekInterval => "Seek interval".to_owned(),
            Field::Gapless => "Gapless (next launch)".to_owned(),
            Field::AutoplayRadio => "Autoplay radio".to_owned(),
            Field::EqPreset => "Preset".to_owned(),
            Field::Band(i) => format!("{:>5}", freq_label(i)),
            Field::Normalize => "Normalize (loudness)".to_owned(),
            Field::GeminiModel => "Model".to_owned(),
            Field::ApiKey => "API key".to_owned(),
            Field::ThemePreset => "Preset".to_owned(),
            Field::ThemeColor(role) => role.label().to_owned(),
        }
    }

    /// Whether the field's value must be hidden when displayed (the API key).
    pub fn is_secret(self) -> bool {
        matches!(self, Field::ApiKey)
    }
}

/// A human label for band `i`'s center frequency (e.g. `1 kHz`).
pub fn freq_label(i: usize) -> String {
    let hz = eq::BAND_FREQS.get(i).copied().unwrap_or(0);
    if hz >= 1000 {
        format!("{} kHz", hz / 1000)
    } else {
        format!("{hz} Hz")
    }
}

/// The user-editable snapshot.
#[derive(Debug, Clone)]
pub struct SettingsDraft {
    pub cookies_file: String,
    pub download_dir: String,
    pub mouse: bool,
    pub album_art: bool,
    pub autoplay_on_start: bool,
    pub speed: f64,
    /// Seek step (seconds) for the seek-back/-forward keys.
    pub seek_seconds: f64,
    pub gapless: bool,
    pub autoplay_radio: bool,
    pub eq_preset: EqPreset,
    pub eq_bands: [f64; eq::BANDS],
    pub normalize: bool,
    pub gemini_model: GeminiModel,
    /// The Gemini key as stored in config (env `GEMINI_API_KEY` still overrides at launch).
    pub gemini_api_key: String,
    /// Color theme preset plus role overrides.
    pub theme: ThemeConfig,
}

impl SettingsDraft {
    /// Render the current value of `field` for display.
    pub fn value_display(&self, field: Field) -> String {
        match field {
            Field::CookiesFile => {
                if self.cookies_file.is_empty() {
                    default_cookies_file()
                        .map(|p| format!("(default: {})", display_path(&p)))
                        .unwrap_or_else(|| "(none)".to_owned())
                } else {
                    self.cookies_file.clone()
                }
            }
            Field::DownloadDir => {
                if self.download_dir.is_empty() {
                    format!("(default: {})", display_path(&default_download_dir()))
                } else {
                    self.download_dir.clone()
                }
            }
            Field::Mouse => toggle_str(self.mouse),
            Field::AlbumArt => toggle_str(self.album_art),
            Field::AutoplayOnStart => toggle_str(self.autoplay_on_start),
            Field::Speed => format!("{:.1}x", self.speed),
            Field::SeekInterval => format!("{:.0}s", self.seek_seconds),
            // Buttons, not values: these rows show how to trigger them.
            Field::ResetKeybindings | Field::ResetAll => "↵ press Enter".to_owned(),
            Field::Gapless => toggle_str(self.gapless),
            Field::AutoplayRadio => toggle_str(self.autoplay_radio),
            Field::EqPreset => self.eq_preset.label().to_owned(),
            Field::Band(i) => format!("{:+.0} dB", self.eq_bands[i]),
            Field::Normalize => toggle_str(self.normalize),
            Field::GeminiModel => self.gemini_model.label().to_owned(),
            Field::ThemePreset => self.theme.preset_enum().label().to_owned(),
            Field::ThemeColor(role) => self.theme.effective_hex(role),
            // Never echo the key. Editing shows a masked buffer (handled in the view); this
            // is the at-rest summary.
            Field::ApiKey => {
                if self.gemini_api_key.trim().is_empty() {
                    "(none)".to_owned()
                } else {
                    "***configured***".to_owned()
                }
            }
        }
    }

    /// The raw buffer backing a text field, for the view's caret/masking math (so a secret
    /// field is masked by *its own* length rather than a hardcoded one). `None` for
    /// non-text fields. Mirrors the editable routing in `App::settings_text_buf`.
    pub fn text_value(&self, field: Field) -> Option<&str> {
        match field {
            Field::CookiesFile => Some(&self.cookies_file),
            Field::DownloadDir => Some(&self.download_dir),
            Field::ApiKey => Some(&self.gemini_api_key),
            Field::ThemeColor(role) => self.theme.overrides.get(role.id()).map(String::as_str),
            _ => None,
        }
    }

    /// Merge the draft's persisted fields into `cfg` (called on save). Live audio fields
    /// are also written so they survive a restart.
    pub fn apply_to(&self, cfg: &mut Config) {
        cfg.cookies_file = blank_to_none(&self.cookies_file).map(PathBuf::from);
        cfg.download_dir = blank_to_none(&self.download_dir).map(PathBuf::from);
        cfg.mouse = Some(self.mouse);
        cfg.album_art = Some(self.album_art);
        cfg.autoplay_on_start = Some(self.autoplay_on_start);
        cfg.speed = Some(self.speed);
        cfg.seek_seconds = Some(self.seek_seconds);
        cfg.gapless = Some(self.gapless);
        cfg.autoplay_radio = Some(self.autoplay_radio);
        cfg.eq_preset = self.eq_preset;
        // Store the explicit band array only when it diverges from the preset's gains, so
        // a plain preset choice stays compact in config.json.
        cfg.eq_bands = if self.eq_bands == self.eq_preset.gains() { None } else { Some(self.eq_bands) };
        cfg.normalize = Some(self.normalize);
        cfg.gemini_model = self.gemini_model;
        cfg.gemini_api_key = blank_to_none(&self.gemini_api_key);
        cfg.theme = self.theme.normalized();
    }
}

fn display_path(path: &Path) -> String {
    if let Some(user_dirs) = directories::UserDirs::new()
        && let Ok(stripped) = path.strip_prefix(user_dirs.home_dir())
    {
        let mut shortened = PathBuf::from("~");
        shortened.push(stripped);
        return shortened.display().to_string();
    }
    path.display().to_string()
}

/// The whole settings-screen state, boxed in `App` so it costs nothing when closed.
#[derive(Debug)]
pub struct SettingsState {
    pub tab: SettingsTab,
    pub row: usize,
    pub draft: SettingsDraft,
    /// Whether the focused text field is in character-entry mode.
    pub editing_text: bool,
    /// The masked secret's value captured when its editor opened. The buffer is cleared
    /// on edit-start (blind-paste of a whole new key), so this lets a commit that typed
    /// nothing restore the prior key instead of wiping it. `None` outside a secret edit.
    pub secret_restore: Option<String>,
    /// A working copy of the keymap edited on the Keys tab; committed to `App.keymap` and
    /// persisted when settings closes.
    pub keymap: KeyMap,
    /// The binding being rebound (Keys tab), while waiting to capture its new key.
    pub capturing: Option<(KeyContext, Action)>,
}

impl SettingsState {
    /// The field the cursor is on (clamped defensively; `saturating_sub` matches the
    /// view's row clamp and stays panic-free if a future tab ever has no fields).
    pub fn current_field(&self) -> Field {
        let fields = self.tab.fields();
        fields[self.row.min(fields.len().saturating_sub(1))]
    }
}

fn toggle_str(on: bool) -> String {
    if on { "[x]".to_owned() } else { "[ ]".to_owned() }
}

pub(crate) fn blank_to_none(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_owned()) }
}

/// Clamp a band gain to range and round to a whole dB.
pub fn clamp_band(g: f64) -> f64 {
    g.clamp(BAND_GAIN_MIN, BAND_GAIN_MAX).round()
}

/// Clamp/round a speed value the way the controls do.
pub fn clamp_speed(s: f64) -> f64 {
    ((s * 10.0).round() / 10.0).clamp(SPEED_MIN, SPEED_MAX)
}

/// Clamp/round a seek step to whole seconds within the supported range.
pub fn clamp_seek_seconds(s: f64) -> f64 {
    s.round().clamp(SEEK_SECONDS_MIN, SEEK_SECONDS_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A neutral draft the value/apply tests can tweak one field at a time.
    fn base_draft() -> SettingsDraft {
        SettingsDraft {
            cookies_file: String::new(),
            download_dir: String::new(),
            mouse: true,
            album_art: false,
            autoplay_on_start: false,
            speed: 1.0,
            seek_seconds: 10.0,
            gapless: true,
            autoplay_radio: false,
            eq_preset: EqPreset::Flat,
            eq_bands: EqPreset::Flat.gains(),
            normalize: false,
            gemini_model: GeminiModel::default(),
            gemini_api_key: String::new(),
            theme: ThemeConfig::default(),
        }
    }

    #[test]
    fn tabs_step_and_wrap() {
        assert_eq!(SettingsTab::General.stepped(true), SettingsTab::Playback);
        assert_eq!(SettingsTab::Eq.stepped(true), SettingsTab::Ai);
        assert_eq!(SettingsTab::Ai.stepped(true), SettingsTab::Theme);
        assert_eq!(SettingsTab::Theme.stepped(true), SettingsTab::Colors);
        assert_eq!(SettingsTab::Colors.stepped(true), SettingsTab::Keys);
        assert_eq!(SettingsTab::Keys.stepped(true), SettingsTab::General); // wraps
        assert_eq!(SettingsTab::General.stepped(false), SettingsTab::Keys); // wraps back
    }

    #[test]
    fn general_tab_has_autoplay_on_start_toggle() {
        let f = SettingsTab::General.fields();
        assert_eq!(
            f,
            vec![
                Field::CookiesFile,
                Field::DownloadDir,
                Field::Mouse,
                Field::AlbumArt,
                Field::AutoplayOnStart,
                Field::ResetKeybindings,
                Field::ResetAll,
            ]
        );
        assert_eq!(Field::ResetKeybindings.kind(), FieldKind::Button);
        assert_eq!(Field::ResetAll.kind(), FieldKind::Button);
        assert_eq!(Field::AutoplayOnStart.kind(), FieldKind::Toggle);
        assert_eq!(Field::AlbumArt.kind(), FieldKind::Toggle);
        // Off by default, and the toggle renders as an empty checkbox.
        let draft = base_draft();
        assert_eq!(draft.value_display(Field::ResetKeybindings), "↵ press Enter");
        assert!(!draft.autoplay_on_start);
        assert_eq!(draft.value_display(Field::AutoplayOnStart), "[ ]");
    }

    #[test]
    fn ai_tab_has_model_key_and_autoplay() {
        let f = SettingsTab::Ai.fields();
        assert_eq!(f, vec![Field::GeminiModel, Field::ApiKey, Field::AutoplayRadio]);
        assert_eq!(Field::GeminiModel.kind(), FieldKind::Select);
        assert_eq!(Field::ApiKey.kind(), FieldKind::Text);
        assert!(Field::ApiKey.is_secret());
        assert!(!Field::GeminiModel.is_secret());
    }

    #[test]
    fn theme_and_colors_tabs_are_editable_and_persistent() {
        assert_eq!(SettingsTab::Theme.fields(), vec![Field::ThemePreset]);
        let color_fields = SettingsTab::Colors.fields();
        assert_eq!(color_fields.len(), ThemeRole::ALL.len());
        assert!(matches!(color_fields[0], Field::ThemeColor(ThemeRole::Background)));

        let mut draft = base_draft();
        draft.theme.set_preset(crate::theme::ThemePreset::Midnight);
        draft.theme.set_override(ThemeRole::Accent, "#123456").unwrap();
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.theme.preset, "midnight");
        assert_eq!(cfg.theme.overrides.get("accent").map(String::as_str), Some("#123456"));
    }

    #[test]
    fn apply_to_persists_every_settings_field() {
        let mut bands = EqPreset::Flat.gains();
        bands[2] = 4.0;
        let mut theme = ThemeConfig::default();
        theme.set_preset(crate::theme::ThemePreset::HighContrast);
        theme.set_override(ThemeRole::BorderPrimary, "#123456").unwrap();

        let draft = SettingsDraft {
            cookies_file: "/tmp/cookies.txt".to_owned(),
            download_dir: "/tmp/downloads".to_owned(),
            mouse: false,
            album_art: true,
            autoplay_on_start: true,
            speed: 1.7,
            seek_seconds: 25.0,
            gapless: false,
            autoplay_radio: true,
            eq_preset: EqPreset::Custom,
            eq_bands: bands,
            normalize: true,
            gemini_model: GeminiModel::Latest,
            gemini_api_key: "  AIzaPersist  ".to_owned(),
            theme,
        };

        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.cookies_file, Some(PathBuf::from("/tmp/cookies.txt")));
        assert_eq!(cfg.download_dir, Some(PathBuf::from("/tmp/downloads")));
        assert_eq!(cfg.mouse, Some(false));
        assert_eq!(cfg.album_art, Some(true));
        assert_eq!(cfg.autoplay_on_start, Some(true));
        assert_eq!(cfg.speed, Some(1.7));
        assert_eq!(cfg.seek_seconds, Some(25.0));
        assert_eq!(cfg.gapless, Some(false));
        assert_eq!(cfg.autoplay_radio, Some(true));
        assert_eq!(cfg.eq_preset, EqPreset::Custom);
        assert_eq!(cfg.eq_bands, Some(bands));
        assert_eq!(cfg.normalize, Some(true));
        assert_eq!(cfg.gemini_model, GeminiModel::Latest);
        assert_eq!(cfg.gemini_api_key.as_deref(), Some("AIzaPersist"));
        assert_eq!(cfg.theme.preset, "high_contrast");
        assert_eq!(
            cfg.theme.overrides.get("border_primary").map(String::as_str),
            Some("#123456")
        );
    }

    #[test]
    fn api_key_display_is_masked() {
        let mut draft = base_draft();
        assert_eq!(draft.value_display(Field::ApiKey), "(none)");
        draft.gemini_api_key = "AIzaSuperSecret".to_owned();
        assert_eq!(draft.value_display(Field::ApiKey), "***configured***");
    }

    #[test]
    fn apply_to_persists_ai_fields() {
        let mut draft = base_draft();
        draft.gemini_model = GeminiModel::Latest;
        draft.gemini_api_key = "  AIzaKey  ".to_owned();
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.gemini_model, GeminiModel::Latest);
        assert_eq!(cfg.gemini_api_key.as_deref(), Some("AIzaKey")); // trimmed
    }

    #[test]
    fn eq_tab_has_preset_ten_bands_and_normalize() {
        let f = SettingsTab::Eq.fields();
        assert_eq!(f.len(), eq::BANDS + 2);
        assert_eq!(f[0], Field::EqPreset);
        assert_eq!(f[eq::BANDS + 1], Field::Normalize);
    }

    #[test]
    fn apply_to_stores_preset_without_band_array_when_unmodified() {
        let draft = SettingsDraft {
            download_dir: "  ".to_owned(),
            eq_preset: EqPreset::BassBoost,
            eq_bands: EqPreset::BassBoost.gains(),
            ..base_draft()
        };
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.eq_preset, EqPreset::BassBoost);
        assert_eq!(cfg.eq_bands, None); // matches preset → not stored explicitly
        assert_eq!(cfg.download_dir, None); // whitespace → none
        assert_eq!(cfg.cookies_file, None);
    }

    #[test]
    fn apply_to_stores_custom_band_array() {
        let mut bands = EqPreset::Flat.gains();
        bands[0] = 5.0;
        let draft = SettingsDraft {
            cookies_file: "/c.txt".to_owned(),
            mouse: false,
            autoplay_on_start: true,
            speed: 1.5,
            gapless: false,
            autoplay_radio: true,
            eq_preset: EqPreset::Custom,
            eq_bands: bands,
            normalize: true,
            ..base_draft()
        };
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.eq_bands, Some(bands));
        assert_eq!(cfg.speed, Some(1.5));
        assert_eq!(cfg.cookies_file, Some(PathBuf::from("/c.txt")));
        assert_eq!(cfg.mouse, Some(false));
        assert_eq!(cfg.autoplay_on_start, Some(true));
    }

    #[test]
    fn band_and_speed_clamps() {
        assert_eq!(clamp_band(99.0), BAND_GAIN_MAX);
        assert_eq!(clamp_band(-99.0), BAND_GAIN_MIN);
        assert_eq!(clamp_speed(9.0), SPEED_MAX);
        assert_eq!(clamp_speed(0.0), SPEED_MIN);
    }

    #[test]
    fn freq_labels_read_naturally() {
        assert_eq!(freq_label(0), "31 Hz");
        assert_eq!(freq_label(5), "1 kHz");
        assert_eq!(freq_label(9), "16 kHz");
    }
}
