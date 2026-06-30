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
    AnimationsConfig, Config, SEEK_SECONDS_MAX, SEEK_SECONDS_MIN, SPEED_MAX, SPEED_MIN,
    default_cookies_file, default_download_dir,
};
use crate::eq::{self, EqPreset};
use crate::i18n::Language;
use crate::keymap::{Action, KeyContext, KeyMap};
use crate::radio::RadioMode;
use crate::t;
use crate::theme::{ThemeConfig, ThemeRole};

/// Per-band gain limits and keyboard step (dB), for the EQ sliders.
pub const BAND_GAIN_MIN: f64 = -12.0;
pub const BAND_GAIN_MAX: f64 = 12.0;
pub const BAND_GAIN_STEP: f64 = 1.0;
/// Playback-speed step for the settings slider (←/→).
pub const SPEED_STEP: f64 = 0.1;
/// Seek-step slider step (seconds) for the Playback tab (←/→).
pub const SEEK_SECONDS_STEP: f64 = 1.0;
/// Animation frame-rate slider step (fps) for the Graphics tab (←/→). Values are clamped to
/// [`crate::config::FPS_MIN`]..=[`crate::config::FPS_MAX`] on change.
pub const ANIM_FPS_STEP: u16 = 5;

/// The settings tabs, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    /// Playback controls + the EQ, shown as two sub-categories (see [`Self::sections`]).
    Playback,
    /// The remappable-keybinding editor (its own render/navigation path).
    Keys,
    /// Theme preset, color roles, and animation toggles, as three sub-categories.
    Graphics,
    Ai,
}

impl SettingsTab {
    pub const ALL: [SettingsTab; 5] = [
        SettingsTab::General,
        SettingsTab::Playback,
        SettingsTab::Keys,
        SettingsTab::Graphics,
        SettingsTab::Ai,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsTab::General => t!("General", "일반"),
            SettingsTab::Playback => t!("Playback", "재생"),
            SettingsTab::Keys => t!("Hotkeys", "핫키"),
            SettingsTab::Graphics => t!("Graphics", "그래픽"),
            SettingsTab::Ai => t!("AI", "AI"),
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

    /// The ordered fields shown under this tab. Merged tabs concatenate their sub-categories'
    /// fields in display order; [`Self::sections`] supplies the headers drawn between them.
    pub fn fields(self) -> Vec<Field> {
        match self {
            SettingsTab::General => vec![
                Field::Language,
                Field::CookiesFile,
                Field::DownloadDir,
                Field::Mouse,
                Field::AlbumArt,
                Field::AutoplayOnStart,
                Field::ResetKeybindings,
                Field::ResetAll,
            ],
            // "Now Playing" controls then the EQ (preset + ten bands + normalize).
            SettingsTab::Playback => {
                let mut f = vec![
                    Field::Speed,
                    Field::SeekInterval,
                    Field::MouseWheelVolume,
                    Field::Gapless,
                    Field::EqPreset,
                ];
                f.extend((0..eq::BANDS).map(Field::Band));
                f.push(Field::Normalize);
                f
            }
            // Theme (preset + transparent-bg toggle), then every color role, then the
            // animation toggles (master kill-switch first, then per-element effects).
            SettingsTab::Graphics => {
                let mut f = vec![Field::ThemePreset, Field::BackgroundNone];
                f.extend(ThemeRole::ALL.iter().copied().map(Field::ThemeColor));
                f.extend([
                    Field::AnimMaster,
                    Field::AnimFps,
                    Field::AnimPauseUnfocused,
                    Field::AnimTitle,
                    Field::AnimHeart,
                    Field::AnimSeekbar,
                    Field::AnimSpinner,
                    Field::AnimEqBars,
                    Field::AnimControls,
                    Field::AnimBorder,
                    Field::AnimRain,
                    Field::AnimDonut,
                    Field::AnimVisualizer,
                    Field::AnimStarfield,
                    Field::AnimBounce,
                ]);
                f
            }
            SettingsTab::Ai => vec![
                Field::AiEnabled,
                Field::GeminiModel,
                Field::ApiKey,
                Field::AutoplayRadio,
                Field::RadioMode,
            ],
            // The Keys tab is a list of remappable bindings, not `Field`s; it has its own
            // navigation and rendering paths (see `crate::keymap::editable_entries`).
            SettingsTab::Keys => Vec::new(),
        }
    }

    /// Sub-category headers for tabs that group their fields, each as `(title, field_count)`
    /// in field order; the counts partition [`Self::fields`] exactly. An empty result means the
    /// tab renders as a flat list with no headers.
    pub fn sections(self) -> Vec<(&'static str, usize)> {
        match self {
            SettingsTab::Playback => vec![
                (t!("Now Playing", "현재 재생"), 4),
                (t!("EQ", "EQ"), eq::BANDS + 2),
            ],
            SettingsTab::Graphics => vec![
                (t!("Theme", "테마"), 2),
                (t!("Colors", "색상"), ThemeRole::ALL.len()),
                (t!("Animations", "애니메이션"), 15),
            ],
            _ => Vec::new(),
        }
    }
}

/// One editable setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    // General
    /// The UI language (English / 한국어), cycled like any other Select field.
    Language,
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
    MouseWheelVolume,
    Gapless,
    // EQ
    EqPreset,
    Band(usize),
    Normalize,
    // AI
    /// Master on/off for the AI assistant. Turning it off keeps the saved key but tears the
    /// assistant down, so AI can be disabled without discarding the API key.
    AiEnabled,
    GeminiModel,
    ApiKey,
    AutoplayRadio,
    /// The radio station's adventurousness (Focused / Balanced / Discovery).
    RadioMode,
    // Theme
    ThemePreset,
    /// Toggle the background to "no color" (transparent), letting the terminal show through.
    BackgroundNone,
    ThemeColor(ThemeRole),
    // Animations — `AnimMaster` is the global enable and `AnimFps` is the frame-rate slider; the
    // rest are per-effect on/off toggles, each mapping to a flag in [`AnimationsConfig`] via
    // `anim_flag`. `AnimFps` is the one non-toggle here (a `Slider`), so it is excluded from
    // `anim_flag` and handled on its own in `kind`/`value_display`/`settings_change`.
    AnimMaster,
    AnimFps,
    /// Behaviour knob (a toggle): pause the animation tick while the terminal is unfocused.
    /// Maps to `AnimationsConfig::pause_unfocused`, handled explicitly (not via `anim_flag`,
    /// which is for visual effects only).
    AnimPauseUnfocused,
    AnimTitle,
    AnimHeart,
    AnimSeekbar,
    AnimSpinner,
    AnimEqBars,
    AnimControls,
    AnimBorder,
    AnimRain,
    AnimDonut,
    AnimVisualizer,
    AnimStarfield,
    AnimBounce,
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
            Field::Mouse | Field::AlbumArt | Field::AutoplayOnStart | Field::MouseWheelVolume
            | Field::Gapless
            | Field::AutoplayRadio | Field::Normalize | Field::AiEnabled | Field::BackgroundNone
            | Field::AnimPauseUnfocused
            | Field::AnimMaster | Field::AnimTitle | Field::AnimHeart | Field::AnimSeekbar
            | Field::AnimSpinner | Field::AnimEqBars | Field::AnimControls | Field::AnimBorder
            | Field::AnimRain | Field::AnimDonut | Field::AnimVisualizer | Field::AnimStarfield
            | Field::AnimBounce => FieldKind::Toggle,
            Field::Language
            | Field::EqPreset
            | Field::GeminiModel
            | Field::ThemePreset
            | Field::RadioMode => FieldKind::Select,
            Field::Speed | Field::SeekInterval | Field::Band(_) | Field::AnimFps => {
                FieldKind::Slider
            }
            Field::ResetKeybindings | Field::ResetAll => FieldKind::Button,
        }
    }

    /// For an animation toggle field, a mutable handle to its flag inside an
    /// [`AnimationsConfig`]; `None` for any non-animation field. This single mapping is the
    /// source of truth used for both rendering the checkbox and flipping it on input — so the
    /// 13 effects never drift out of sync across the display / toggle / persist paths.
    pub(crate) fn anim_flag(self, a: &mut AnimationsConfig) -> Option<&mut bool> {
        Some(match self {
            Field::AnimMaster => &mut a.master,
            Field::AnimTitle => &mut a.title,
            Field::AnimHeart => &mut a.heart,
            Field::AnimSeekbar => &mut a.seekbar,
            Field::AnimSpinner => &mut a.spinner,
            Field::AnimEqBars => &mut a.eq_bars,
            Field::AnimControls => &mut a.controls,
            Field::AnimBorder => &mut a.border,
            Field::AnimRain => &mut a.rain,
            Field::AnimDonut => &mut a.donut,
            Field::AnimVisualizer => &mut a.visualizer,
            Field::AnimStarfield => &mut a.starfield,
            Field::AnimBounce => &mut a.bounce,
            _ => return None,
        })
    }

    pub fn label(self) -> String {
        match self {
            Field::Language => t!("Language", "언어").to_owned(),
            Field::CookiesFile => t!("Cookies file", "쿠키 파일").to_owned(),
            Field::DownloadDir => t!("Download dir", "다운로드 폴더").to_owned(),
            Field::Mouse => t!("Mouse (next launch)", "마우스 (재시작 후 적용)").to_owned(),
            Field::AlbumArt => t!("Album art (next launch)", "앨범 아트 (재시작 후 적용)").to_owned(),
            Field::AutoplayOnStart => t!("Autoplay on launch", "앱 시작 시 자동재생").to_owned(),
            Field::ResetKeybindings => t!("Reset keybindings", "단축키 초기화").to_owned(),
            Field::ResetAll => t!("Reset all settings", "모든 설정 초기화").to_owned(),
            Field::Speed => t!("Playback speed", "재생 속도").to_owned(),
            Field::SeekInterval => t!("Seek interval", "탐색 간격").to_owned(),
            Field::MouseWheelVolume => t!("Wheel volume", "휠 볼륨 조절").to_owned(),
            Field::Gapless => t!("Gapless (next launch)", "갭리스 (재시작 후 적용)").to_owned(),
            Field::AutoplayRadio => t!("Autoplay radio", "자동재생 라디오").to_owned(),
            Field::RadioMode => t!("Radio mode", "라디오 모드").to_owned(),
            Field::EqPreset => t!("Preset", "프리셋").to_owned(),
            Field::Band(i) => format!("{:>5}", freq_label(i)),
            Field::Normalize => t!("Normalize (loudness)", "음량 평준화").to_owned(),
            Field::AiEnabled => t!("Enable AI", "AI 사용").to_owned(),
            Field::GeminiModel => t!("Model", "모델").to_owned(),
            Field::ApiKey => t!("API key", "API 키").to_owned(),
            Field::ThemePreset => t!("Preset", "프리셋").to_owned(),
            Field::BackgroundNone => t!("Background: None", "배경 없음").to_owned(),
            Field::ThemeColor(role) => role.label().to_owned(),
            Field::AnimMaster => t!("Enable animations", "애니메이션 켜기").to_owned(),
            Field::AnimFps => t!("Frame rate", "프레임 레이트").to_owned(),
            Field::AnimPauseUnfocused => {
                t!("Pause when unfocused", "포커스 없을 때 정지").to_owned()
            }
            Field::AnimTitle => t!("Title shimmer", "제목 반짝임").to_owned(),
            Field::AnimHeart => t!("Beating heart", "하트 박동").to_owned(),
            Field::AnimSeekbar => t!("Seekbar glow", "탐색바 반짝임").to_owned(),
            Field::AnimSpinner => t!("Now-playing spinner", "재생 스피너").to_owned(),
            Field::AnimEqBars => t!("EQ bars", "EQ 막대").to_owned(),
            Field::AnimControls => t!("Control pulse", "컨트롤 펄스").to_owned(),
            Field::AnimBorder => t!("Breathing border", "테두리 호흡").to_owned(),
            Field::AnimRain => t!("Matrix rain", "매트릭스 비").to_owned(),
            Field::AnimDonut => t!("Spinning donut", "회전 도넛").to_owned(),
            Field::AnimVisualizer => t!("Visualizer", "비주얼라이저").to_owned(),
            Field::AnimStarfield => t!("Starfield / notes", "별·음표").to_owned(),
            Field::AnimBounce => t!("Bouncing logo", "튕기는 로고").to_owned(),
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
// No `Debug`: holds the plaintext `gemini_api_key`, so it must not be `{:?}`-printable (see `Config`).
#[derive(Clone)]
pub struct SettingsDraft {
    pub cookies_file: String,
    pub download_dir: String,
    pub mouse: bool,
    pub album_art: bool,
    pub autoplay_on_start: bool,
    pub speed: f64,
    /// Seek step (seconds) for the seek-back/-forward keys.
    pub seek_seconds: f64,
    /// Whether wheel events over the player volume cluster nudge volume.
    pub mouse_wheel_volume: bool,
    pub gapless: bool,
    pub autoplay_radio: bool,
    /// The radio station's adventurousness (drives MMR λ, sampling temperature, artist spacing).
    pub radio_mode: RadioMode,
    pub eq_preset: EqPreset,
    pub eq_bands: [f64; eq::BANDS],
    pub normalize: bool,
    pub gemini_model: GeminiModel,
    /// The Gemini key as stored in config (env `GEMINI_API_KEY` still overrides at launch).
    pub gemini_api_key: String,
    /// Whether the AI assistant is enabled. Lets the user keep the key saved but turn AI off.
    pub ai_enabled: bool,
    /// Color theme preset plus role overrides.
    pub theme: ThemeConfig,
    /// UI language. Applied live (via [`crate::i18n::set_language`]) as the user cycles the
    /// dropdown, and persisted on close.
    pub language: Language,
    /// Player-view animation toggles (the Animations tab). Edited in place; the whole struct
    /// is copied into `Config` on save. See [`AnimationsConfig`].
    pub animations: AnimationsConfig,
}

impl SettingsDraft {
    /// Render the current value of `field` for display.
    pub fn value_display(&self, field: Field) -> String {
        match field {
            // Each language names itself, so this value is the same regardless of the active
            // UI language (English / 한국어).
            Field::Language => self.language.native_name().to_owned(),
            Field::CookiesFile => {
                if self.cookies_file.is_empty() {
                    default_cookies_file()
                        .map(|p| format!("({}: {})", t!("default", "기본값"), display_path(&p)))
                        .unwrap_or_else(|| t!("(none)", "(없음)").to_owned())
                } else {
                    self.cookies_file.clone()
                }
            }
            Field::DownloadDir => {
                if self.download_dir.is_empty() {
                    format!(
                        "({}: {})",
                        t!("default", "기본값"),
                        display_path(&default_download_dir())
                    )
                } else {
                    self.download_dir.clone()
                }
            }
            Field::Mouse => toggle_str(self.mouse),
            Field::AlbumArt => toggle_str(self.album_art),
            Field::AutoplayOnStart => toggle_str(self.autoplay_on_start),
            Field::Speed => format!("{:.1}x", self.speed),
            Field::SeekInterval => format!("{:.0}s", self.seek_seconds),
            Field::MouseWheelVolume => toggle_str(self.mouse_wheel_volume),
            // Buttons, not values: these rows show how to trigger them.
            Field::ResetKeybindings | Field::ResetAll => {
                t!("↵ press Enter", "↵ Enter로 실행").to_owned()
            }
            Field::Gapless => toggle_str(self.gapless),
            Field::AutoplayRadio => toggle_str(self.autoplay_radio),
            Field::RadioMode => self.radio_mode.label().to_owned(),
            Field::EqPreset => self.eq_preset.label().to_owned(),
            Field::Band(i) => format!("{:+.0} dB", self.eq_bands[i]),
            Field::Normalize => toggle_str(self.normalize),
            Field::AiEnabled => toggle_str(self.ai_enabled),
            Field::GeminiModel => self.gemini_model.label().to_owned(),
            Field::ThemePreset => self.theme.preset_enum().label().to_owned(),
            Field::BackgroundNone => {
                toggle_str(self.theme.is_role_transparent(ThemeRole::Background))
            }
            Field::ThemeColor(role) => self.theme.effective_hex(role),
            // The lone numeric animation field: shown as "<n> fps" (clamped to the valid range).
            Field::AnimFps => format!("{} fps", self.animations.effective_fps()),
            // Behaviour knob, rendered as a checkbox (handled explicitly, not via `anim_flag`).
            Field::AnimPauseUnfocused => toggle_str(self.animations.pause_unfocused),
            // All 13 animation toggles render as a checkbox; one mapping (`anim_flag`) reads
            // the live value out of the draft's `animations`, so display never drifts from
            // the toggle/persist paths. (`field` is the value being matched here.)
            Field::AnimMaster
            | Field::AnimTitle
            | Field::AnimHeart
            | Field::AnimSeekbar
            | Field::AnimSpinner
            | Field::AnimEqBars
            | Field::AnimControls
            | Field::AnimBorder
            | Field::AnimRain
            | Field::AnimDonut
            | Field::AnimVisualizer
            | Field::AnimStarfield
            | Field::AnimBounce => {
                let mut a = self.animations;
                toggle_str(field.anim_flag(&mut a).map(|b| *b).unwrap_or(false))
            }
            // Never echo the key. Editing shows a masked buffer (handled in the view); this
            // is the at-rest summary.
            Field::ApiKey => {
                if self.gemini_api_key.trim().is_empty() {
                    t!("(none)", "(없음)").to_owned()
                } else {
                    t!("***configured***", "***저장됨***").to_owned()
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
        cfg.mouse_wheel_volume = Some(self.mouse_wheel_volume);
        cfg.gapless = Some(self.gapless);
        cfg.autoplay_radio = Some(self.autoplay_radio);
        cfg.radio.mode = self.radio_mode;
        cfg.eq_preset = self.eq_preset;
        // Store the explicit band array only when it diverges from the preset's gains, so
        // a plain preset choice stays compact in config.json.
        cfg.eq_bands = if self.eq_bands == self.eq_preset.gains() { None } else { Some(self.eq_bands) };
        cfg.normalize = Some(self.normalize);
        cfg.gemini_model = self.gemini_model;
        cfg.gemini_api_key = blank_to_none(&self.gemini_api_key);
        cfg.ai_enabled = Some(self.ai_enabled);
        cfg.theme = self.theme.normalized();
        cfg.language = self.language;
        cfg.animations = self.animations;
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
// No `Debug`: carries `secret_restore` (a captured copy of the API key) and the `draft` above,
// so it must not be `{:?}`-printable.
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
    /// The field the cursor is on, or `None` when the tab has no `Field`s (the Keys tab, which
    /// edits bindings instead). `saturating_sub` matches the view's row clamp; `get` keeps this
    /// panic-free even for an empty tab, so callers reached on any tab (e.g. the per-keystroke
    /// "is this a color row?" check) stay sound.
    pub fn current_field(&self) -> Option<Field> {
        let fields = self.tab.fields();
        fields.get(self.row.min(fields.len().saturating_sub(1))).copied()
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
            mouse_wheel_volume: true,
            gapless: true,
            autoplay_radio: false,
            radio_mode: RadioMode::Balanced,
            eq_preset: EqPreset::Flat,
            eq_bands: EqPreset::Flat.gains(),
            normalize: false,
            gemini_model: GeminiModel::default(),
            gemini_api_key: String::new(),
            ai_enabled: true,
            theme: ThemeConfig::default(),
            language: Language::English,
            animations: AnimationsConfig::default(),
        }
    }

    #[test]
    fn tabs_step_and_wrap() {
        assert_eq!(SettingsTab::General.stepped(true), SettingsTab::Playback);
        assert_eq!(SettingsTab::Playback.stepped(true), SettingsTab::Keys);
        assert_eq!(SettingsTab::Keys.stepped(true), SettingsTab::Graphics);
        assert_eq!(SettingsTab::Graphics.stepped(true), SettingsTab::Ai);
        assert_eq!(SettingsTab::Ai.stepped(true), SettingsTab::General); // wraps
        assert_eq!(SettingsTab::General.stepped(false), SettingsTab::Ai); // wraps back
    }

    #[test]
    fn graphics_tab_groups_theme_colors_and_animations() {
        let _guard = crate::i18n::lock_for_test();
        let f = SettingsTab::Graphics.fields();
        // ThemePreset + BackgroundNone + every color role + 15 animation fields
        // (master enable + fps slider + pause-when-unfocused toggle + 12 per-effect toggles).
        assert_eq!(f.len(), 2 + ThemeRole::ALL.len() + 15);
        assert_eq!(f[0], Field::ThemePreset);
        assert_eq!(f[1], Field::BackgroundNone);
        assert!(matches!(f[2], Field::ThemeColor(_)));
        let anim_start = 2 + ThemeRole::ALL.len();
        assert_eq!(f[anim_start], Field::AnimMaster);
        // The fps slider sits right after the master enable; it's the one non-toggle here.
        assert_eq!(f[anim_start + 1], Field::AnimFps);
        assert_eq!(Field::AnimFps.kind(), FieldKind::Slider);
        assert!(
            f[anim_start..]
                .iter()
                .filter(|fld| **fld != Field::AnimFps)
                .all(|fld| fld.kind() == FieldKind::Toggle)
        );

        // Section counts partition the field list exactly.
        let total: usize = SettingsTab::Graphics.sections().iter().map(|(_, n)| n).sum();
        assert_eq!(total, f.len());

        // Default draft: every animation toggle reads as off.
        let mut draft = base_draft();
        assert_eq!(draft.value_display(Field::AnimMaster), "[ ]");
        assert_eq!(draft.value_display(Field::AnimRain), "[ ]");

        // Flipping a couple through the shared mapping shows + persists.
        *Field::AnimMaster.anim_flag(&mut draft.animations).unwrap() = true;
        *Field::AnimDonut.anim_flag(&mut draft.animations).unwrap() = true;
        assert_eq!(draft.value_display(Field::AnimMaster), "[x]");
        assert_eq!(draft.value_display(Field::AnimDonut), "[x]");
        assert_eq!(draft.value_display(Field::AnimRain), "[ ]");

        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert!(cfg.animations.master);
        assert!(cfg.animations.donut);
        assert!(!cfg.animations.rain);
        assert!(cfg.animations.active());

        // Non-animation fields map to no flag.
        assert!(Field::Mouse.anim_flag(&mut AnimationsConfig::default()).is_none());
    }

    #[test]
    fn background_none_toggle_tracks_transparency() {
        let _guard = crate::i18n::lock_for_test();
        assert_eq!(Field::BackgroundNone.kind(), FieldKind::Toggle);
        let mut draft = base_draft();
        // A preset with a concrete background reads as "not none".
        draft.theme.set_preset(crate::theme::ThemePreset::Midnight);
        assert!(!draft.theme.is_role_transparent(ThemeRole::Background));
        assert_eq!(draft.value_display(Field::BackgroundNone), "[ ]");
        // Forcing the override transparent flips the toggle on and persists.
        draft.theme.set_override(ThemeRole::Background, crate::theme::TRANSPARENT).unwrap();
        assert!(draft.theme.is_role_transparent(ThemeRole::Background));
        assert_eq!(draft.value_display(Field::BackgroundNone), "[x]");
        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert!(cfg.theme.normalized().is_role_transparent(ThemeRole::Background));
    }

    #[test]
    fn playback_tab_groups_now_playing_and_eq() {
        let f = SettingsTab::Playback.fields();
        // Speed + SeekInterval + WheelVolume + Gapless, then EqPreset + ten bands + Normalize.
        assert_eq!(f.len(), 4 + 1 + eq::BANDS + 1);
        assert_eq!(f[0], Field::Speed);
        assert_eq!(f[1], Field::SeekInterval);
        assert_eq!(f[2], Field::MouseWheelVolume);
        assert_eq!(f[3], Field::Gapless);
        assert_eq!(f[4], Field::EqPreset);
        assert_eq!(f[4 + eq::BANDS + 1], Field::Normalize);
        assert_eq!(Field::MouseWheelVolume.kind(), FieldKind::Toggle);
        assert_eq!(base_draft().value_display(Field::MouseWheelVolume), "[x]");
        let total: usize = SettingsTab::Playback.sections().iter().map(|(_, n)| n).sum();
        assert_eq!(total, f.len());
    }

    #[test]
    fn general_tab_has_autoplay_on_start_toggle() {
        let _guard = crate::i18n::lock_for_test();
        let f = SettingsTab::General.fields();
        assert_eq!(
            f,
            vec![
                Field::Language,
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
    fn ai_tab_has_model_key_autoplay_and_radio_mode() {
        let _guard = crate::i18n::lock_for_test();
        let f = SettingsTab::Ai.fields();
        assert_eq!(
            f,
            vec![
                Field::AiEnabled,
                Field::GeminiModel,
                Field::ApiKey,
                Field::AutoplayRadio,
                Field::RadioMode,
            ]
        );
        assert_eq!(Field::AiEnabled.kind(), FieldKind::Toggle);
        assert!(!Field::AiEnabled.is_secret());
        // Enabled by default in a fresh draft.
        assert_eq!(base_draft().value_display(Field::AiEnabled), "[x]");
        assert_eq!(Field::GeminiModel.kind(), FieldKind::Select);
        assert_eq!(Field::ApiKey.kind(), FieldKind::Text);
        assert!(Field::ApiKey.is_secret());
        assert!(!Field::GeminiModel.is_secret());
        // The radio mode is a non-secret cycle field.
        assert_eq!(Field::RadioMode.kind(), FieldKind::Select);
        assert!(!Field::RadioMode.is_secret());
        assert_eq!(base_draft().value_display(Field::RadioMode), "Balanced");
    }

    #[test]
    fn theme_and_colors_are_editable_and_persistent() {
        // Theme preset + transparent-bg toggle lead the Graphics tab; the color roles follow.
        let f = SettingsTab::Graphics.fields();
        assert_eq!(f[0], Field::ThemePreset);
        let color_fields: Vec<Field> =
            f.into_iter().filter(|fld| matches!(fld, Field::ThemeColor(_))).collect();
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
            mouse_wheel_volume: false,
            gapless: false,
            autoplay_radio: true,
            radio_mode: RadioMode::Discovery,
            eq_preset: EqPreset::Custom,
            eq_bands: bands,
            normalize: true,
            gemini_model: GeminiModel::Latest,
            gemini_api_key: "  AIzaPersist  ".to_owned(),
            ai_enabled: false,
            theme,
            language: Language::Korean,
            animations: AnimationsConfig {
                master: true,
                border: true,
                fps: 45,
                pause_unfocused: false,
                ..Default::default()
            },
        };

        let mut cfg = Config::default();
        draft.apply_to(&mut cfg);
        assert_eq!(cfg.language, Language::Korean);
        assert_eq!(cfg.ai_enabled, Some(false));
        assert!(cfg.animations.master);
        assert!(cfg.animations.border);
        assert!(!cfg.animations.rain);
        assert_eq!(cfg.animations.fps, 45);
        assert!(!cfg.animations.pause_unfocused);
        assert_eq!(cfg.cookies_file, Some(PathBuf::from("/tmp/cookies.txt")));
        assert_eq!(cfg.download_dir, Some(PathBuf::from("/tmp/downloads")));
        assert_eq!(cfg.mouse, Some(false));
        assert_eq!(cfg.album_art, Some(true));
        assert_eq!(cfg.autoplay_on_start, Some(true));
        assert_eq!(cfg.speed, Some(1.7));
        assert_eq!(cfg.seek_seconds, Some(25.0));
        assert_eq!(cfg.mouse_wheel_volume, Some(false));
        assert_eq!(cfg.gapless, Some(false));
        assert_eq!(cfg.autoplay_radio, Some(true));
        assert_eq!(cfg.radio.mode, RadioMode::Discovery);
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
        let _guard = crate::i18n::lock_for_test();
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
    fn eq_section_lives_under_playback() {
        // EQ moved under Playback: preset, ten bands, then normalize, contiguous.
        let f = SettingsTab::Playback.fields();
        let preset_at = f.iter().position(|fld| *fld == Field::EqPreset).unwrap();
        for (k, fld) in f[preset_at + 1..=preset_at + eq::BANDS].iter().enumerate() {
            assert_eq!(*fld, Field::Band(k));
        }
        assert_eq!(f[preset_at + eq::BANDS + 1], Field::Normalize);
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
