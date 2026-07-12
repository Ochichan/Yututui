//! The v8 `settings` topic read model (docs/gui/05 §5.2).
//!
//! A full-config projection pushed as `settings_snapshot` — on subscribe and after any
//! mutation. Field names and value shapes are the contract the GUI settings store binds
//! (`gui/src/lib/stores/settings.svelte.ts`); the frontend's optimistic pending overlay
//! clears an entry only when a push agrees with it, so every accepted `apply` MUST be
//! followed by a snapshot that reflects the new value.
//!
//! Pure wire structs only — the Config→model projection lives in `remote::publish`.

use serde::{Deserialize, Serialize};

use crate::queue::Repeat;
use crate::search_source::SearchSource;

use super::model_player::EqModel;

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsModelV8 {
    /// Monotonic per-owner-run change counter; the GUI uses it only for staleness cues.
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub rev: u64,
    pub playback: PlaybackSettingsModel,
    pub eq: EqModel,
    pub streaming: StreamingSettingsModel,
    pub search: SearchSettingsModel,
    pub ui: UiSettingsModel,
    pub storage: StorageSettingsModel,
    pub audio: AudioSettingsModel,
    pub animations: AnimationsModel,
    pub theme: ThemeSettingsModel,
    pub keymap: KeymapSettingsModel,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaybackSettingsModel {
    /// Playback speed in tenths: `10` = 1.0×, `15` = 1.5× (same unit the panel uses).
    pub speed_tenths: u16,
    pub seek_seconds: u16,
    pub gapless: bool,
    pub enqueue_next: bool,
    pub autoplay_on_start: bool,
    pub mouse_wheel_volume: bool,
    pub media_controls: bool,
    /// Carried for display; live changes ride the player topic.
    pub volume: i64,
    pub shuffle: bool,
    pub repeat: Repeat,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamingSettingsModel {
    pub ai_enabled: bool,
    /// Gemini REST model id (e.g. `gemini-2.5-flash-lite`).
    pub gemini_model: String,
    /// Auto-extend the queue when it runs low (`autoplay_streaming`).
    pub autoplay: bool,
    /// Streaming engine profile: `focused` | `balanced` | `discovery`.
    pub mode: String,
    /// Key presence only — the key itself never crosses the wire.
    pub has_gemini_key: bool,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchSettingsModel {
    pub default_source: SearchSource,
    pub soundcloud_enabled: bool,
    pub audius_enabled: bool,
    pub jamendo_enabled: bool,
    pub internet_archive_enabled: bool,
    pub radio_browser_enabled: bool,
    pub audius_app_name: Option<String>,
    pub jamendo_client_id: Option<String>,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiSettingsModel {
    /// BCP-47-ish short code the GUI i18n catalog keys off: `en` | `ko`.
    pub language: String,
    pub mouse: bool,
    pub album_art: bool,
    pub romanized_titles: bool,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageSettingsModel {
    /// Display strings (paths), never validated shell-side.
    pub download_dir: Option<String>,
    pub cookies_file: Option<String>,
    pub download_concurrency: u32,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioSettingsModel {
    /// v1 supports only `mpv`; exposed so the GUI can name the backend explicitly.
    pub backend: String,
    pub mpv_output: Option<String>,
    pub mpv_device: Option<String>,
    pub mpv_cache_forward: String,
    pub mpv_cache_back: String,
}

/// Mirrors [`crate::config::AnimationsConfig`] field-for-field (minus the TUI-only
/// `radio_master` scope selector): master/behaviour knobs + the 25 effect flags.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnimationsModel {
    pub master: bool,
    pub pause_unfocused: bool,
    pub fps: u16,
    pub title: bool,
    pub heart: bool,
    pub seekbar: bool,
    pub spinner: bool,
    pub eq_bars: bool,
    pub controls: bool,
    pub border: bool,
    pub track_intro: bool,
    pub lyrics: bool,
    pub toast: bool,
    pub volume_flash: bool,
    pub like_burst: bool,
    pub seek_flash: bool,
    pub selection: bool,
    pub stagger: bool,
    pub caret: bool,
    pub tabs: bool,
    pub popup_fade: bool,
    pub activity: bool,
    pub about_fx: bool,
    pub visualizer: bool,
    pub rain: bool,
    pub donut: bool,
    pub starfield: bool,
    pub bounce: bool,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeSettingsModel {
    pub preset: String,
    /// The fully resolved 34-role palette as `role → #RRGGBB` (preset + overrides).
    pub roles: std::collections::BTreeMap<String, String>,
    /// Only the user's per-role overrides.
    pub overrides: std::collections::BTreeMap<String, String>,
    pub background_none: bool,
    pub retro: bool,
    /// Preset gallery: stable name, display label, and a small swatch of representative roles.
    pub presets: Vec<ThemePresetModel>,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemePresetModel {
    /// Stable preset id used by setting-change commands.
    pub name: String,
    /// Human-readable preset name for UI display.
    pub label: String,
    pub swatch: std::collections::BTreeMap<String, String>,
}

/// B1-minimal keymap block: the user's persisted overrides (`"<context>.<action>" →
/// chord`). The full live keymap + action catalog (docs/gui/05 §8) lands with the
/// hotkeys milestone; the GUI tab degrades to override rows until then.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeymapSettingsModel {
    pub bindings: std::collections::BTreeMap<String, String>,
    pub actions: Vec<ActionInfoModel>,
}

#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionInfoModel {
    pub context: String,
    pub id: String,
    pub label: String,
    /// The factory chord (the per-row reset target); the live chord rides `bindings`.
    pub default_chord: String,
}
