//! Settings-screen state: tabs, editable fields, and a draft the user commits or reverts.
//!
//! The screen edits a [`SettingsDraft`] — a snapshot of the persisted [`Config`] plus the
//! live audio state — without touching `App`'s committed state. Audio-affecting fields
//! (speed, EQ, normalization) are applied to mpv *live* as they change; on cancel the
//! reducer restores mpv from the committed state, on save it copies the draft into `App`
//! and persists. Rendering lives in [`crate::ui::views::settings`]; key handling in
//! [`crate::app`].

use std::path::PathBuf;

use crate::ai::GeminiModel;
use crate::config::{Config, SPEED_MAX, SPEED_MIN};
use crate::eq::{self, EqPreset};
use crate::keymap::{Action, KeyContext, KeyMap};

/// Per-band gain limits and keyboard step (dB), for the EQ sliders.
pub const BAND_GAIN_MIN: f64 = -12.0;
pub const BAND_GAIN_MAX: f64 = 12.0;
pub const BAND_GAIN_STEP: f64 = 1.0;
/// Playback-speed step for the settings slider (←/→).
pub const SPEED_STEP: f64 = 0.1;

/// The settings tabs, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Playback,
    Eq,
    Ai,
    Keys,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 5] = [
        SettingsTab::General,
        SettingsTab::Playback,
        SettingsTab::Eq,
        SettingsTab::Ai,
        SettingsTab::Keys,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::General => "General",
            SettingsTab::Playback => "Playback",
            SettingsTab::Eq => "EQ",
            SettingsTab::Ai => "AI",
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
            SettingsTab::General => vec![Field::CookiesFile, Field::DownloadDir, Field::Mouse],
            SettingsTab::Playback => vec![Field::Speed, Field::Gapless],
            SettingsTab::Eq => {
                let mut f = vec![Field::EqPreset];
                f.extend((0..eq::BANDS).map(Field::Band));
                f.push(Field::Normalize);
                f
            }
            SettingsTab::Ai => vec![Field::GeminiModel, Field::ApiKey, Field::AutoplayRadio],
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
    // Playback
    Speed,
    Gapless,
    // EQ
    EqPreset,
    Band(usize),
    Normalize,
    // AI
    GeminiModel,
    ApiKey,
    AutoplayRadio,
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
}

impl Field {
    pub fn kind(self) -> FieldKind {
        match self {
            Field::CookiesFile | Field::DownloadDir | Field::ApiKey => FieldKind::Text,
            Field::Mouse | Field::Gapless | Field::AutoplayRadio | Field::Normalize => FieldKind::Toggle,
            Field::EqPreset | Field::GeminiModel => FieldKind::Select,
            Field::Speed | Field::Band(_) => FieldKind::Slider,
        }
    }

    pub fn label(self) -> String {
        match self {
            Field::CookiesFile => "Cookies file".to_owned(),
            Field::DownloadDir => "Download dir".to_owned(),
            Field::Mouse => "Mouse (next launch)".to_owned(),
            Field::Speed => "Playback speed".to_owned(),
            Field::Gapless => "Gapless (next launch)".to_owned(),
            Field::AutoplayRadio => "Autoplay radio".to_owned(),
            Field::EqPreset => "Preset".to_owned(),
            Field::Band(i) => format!("{:>5}", freq_label(i)),
            Field::Normalize => "Normalize (loudness)".to_owned(),
            Field::GeminiModel => "Model".to_owned(),
            Field::ApiKey => "API key".to_owned(),
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

/// The user-editable snapshot. `Clone` so the screen can keep a pristine copy for revert.
#[derive(Debug, Clone)]
pub struct SettingsDraft {
    pub cookies_file: String,
    pub download_dir: String,
    pub mouse: bool,
    pub speed: f64,
    pub gapless: bool,
    pub autoplay_radio: bool,
    pub eq_preset: EqPreset,
    pub eq_bands: [f64; eq::BANDS],
    pub normalize: bool,
    pub gemini_model: GeminiModel,
    /// The Gemini key as stored in config (env `GEMINI_API_KEY` still overrides at launch).
    pub gemini_api_key: String,
}

impl SettingsDraft {
    /// Render the current value of `field` for display.
    pub fn value_display(&self, field: Field) -> String {
        match field {
            Field::CookiesFile => {
                if self.cookies_file.is_empty() { "(none)".to_owned() } else { self.cookies_file.clone() }
            }
            Field::DownloadDir => {
                if self.download_dir.is_empty() { "(default)".to_owned() } else { self.download_dir.clone() }
            }
            Field::Mouse => toggle_str(self.mouse),
            Field::Speed => format!("{:.1}x", self.speed),
            Field::Gapless => toggle_str(self.gapless),
            Field::AutoplayRadio => toggle_str(self.autoplay_radio),
            Field::EqPreset => self.eq_preset.label().to_owned(),
            Field::Band(i) => format!("{:+.0} dB", self.eq_bands[i]),
            Field::Normalize => toggle_str(self.normalize),
            Field::GeminiModel => self.gemini_model.label().to_owned(),
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
            _ => None,
        }
    }

    /// Merge the draft's persisted fields into `cfg` (called on save). Live audio fields
    /// are also written so they survive a restart.
    pub fn apply_to(&self, cfg: &mut Config) {
        cfg.cookies_file = blank_to_none(&self.cookies_file).map(PathBuf::from);
        cfg.download_dir = blank_to_none(&self.download_dir).map(PathBuf::from);
        cfg.mouse = Some(self.mouse);
        cfg.speed = Some(self.speed);
        cfg.gapless = Some(self.gapless);
        cfg.autoplay_radio = Some(self.autoplay_radio);
        cfg.eq_preset = self.eq_preset;
        // Store the explicit band array only when it diverges from the preset's gains, so
        // a plain preset choice stays compact in config.json.
        cfg.eq_bands = if self.eq_bands == self.eq_preset.gains() { None } else { Some(self.eq_bands) };
        cfg.normalize = Some(self.normalize);
        cfg.gemini_model = self.gemini_model;
        cfg.gemini_api_key = blank_to_none(&self.gemini_api_key);
    }
}

/// The whole settings-screen state, boxed in `App` so it costs nothing when closed.
#[derive(Debug)]
pub struct SettingsState {
    pub tab: SettingsTab,
    pub row: usize,
    pub draft: SettingsDraft,
    /// Pristine copy captured on open, used to revert live changes on cancel.
    pub original: SettingsDraft,
    /// Whether the focused text field is in character-entry mode.
    pub editing_text: bool,
    /// The masked secret's value captured when its editor opened. The buffer is cleared
    /// on edit-start (blind-paste of a whole new key), so this lets a commit that typed
    /// nothing restore the prior key instead of wiping it. `None` outside a secret edit.
    pub secret_restore: Option<String>,
    /// A working copy of the keymap edited on the Keys tab; committed to `App.keymap` and
    /// persisted on save, discarded on cancel.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A neutral draft the value/apply tests can tweak one field at a time.
    fn base_draft() -> SettingsDraft {
        SettingsDraft {
            cookies_file: String::new(),
            download_dir: String::new(),
            mouse: true,
            speed: 1.0,
            gapless: true,
            autoplay_radio: false,
            eq_preset: EqPreset::Flat,
            eq_bands: EqPreset::Flat.gains(),
            normalize: false,
            gemini_model: GeminiModel::default(),
            gemini_api_key: String::new(),
        }
    }

    #[test]
    fn tabs_step_and_wrap() {
        assert_eq!(SettingsTab::General.stepped(true), SettingsTab::Playback);
        assert_eq!(SettingsTab::Eq.stepped(true), SettingsTab::Ai);
        assert_eq!(SettingsTab::Ai.stepped(true), SettingsTab::Keys);
        assert_eq!(SettingsTab::Keys.stepped(true), SettingsTab::General); // wraps
        assert_eq!(SettingsTab::General.stepped(false), SettingsTab::Keys); // wraps back
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
