//! Typed boundary for the GUI's intentionally loose `group.field = JSON` settings wire.
//!
//! The protocol shape stays open so older clients and frozen frames remain compatible. Owners,
//! however, should never have to interpret raw strings or JSON: this module translates every
//! accepted field into a domain-typed mutation and preserves the daemon's historical error
//! reasons for rejected values.

use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::ai::GeminiModel;
use crate::config::{AnimationsConfig, AudioBackend, FPS_MAX, FPS_MIN, LongFormSeekOptimization};
use crate::eq::EqPreset;
use crate::i18n::Language;
use crate::queue::Repeat;
use crate::remote::proto::GuiSettingChange;
use crate::search_source::SearchSource;
use crate::streaming::StreamingMode;
use crate::theme::{ThemeConfig, ThemePreset, ThemeRole};

use super::blank_to_none;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GuiSettingMutation {
    Playback(PlaybackSettingMutation),
    Audio(AudioSettingMutation),
    Eq(EqSettingMutation),
    Streaming(StreamingSettingMutation),
    Search(SearchSettingMutation),
    Ui(UiSettingMutation),
    Storage(StorageSettingMutation),
    Animation(AnimationSettingMutation),
    Theme(ThemeSettingMutation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlaybackSettingMutation {
    SpeedTenths(u16),
    SeekSeconds(u16),
    Gapless(bool),
    EnqueueNext(bool),
    AutoplayOnStart(bool),
    MouseWheelVolume(bool),
    MediaControls(bool),
    Volume(i64),
    Shuffle(bool),
    Repeat(Repeat),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AudioSettingMutation {
    Backend(AudioBackend),
    MpvOutput(Option<String>),
    MpvDevice(Option<String>),
    LongFormSeekOptimization(LongFormSeekOptimization),
    MpvCacheForward(Option<String>),
    MpvCacheBack(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EqSettingMutation {
    Preset(EqPreset),
    Bands([f64; 10]),
    Normalize(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StreamingSettingMutation {
    AiEnabled(bool),
    Autoplay(bool),
    Mode(StreamingMode),
    GeminiModel(GeminiModel),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchCatalog {
    SoundCloud,
    Audius,
    Jamendo,
    InternetArchive,
    RadioBrowser,
}

impl SearchCatalog {
    fn from_field(field: &str) -> Option<Self> {
        match field {
            "soundcloud_enabled" => Some(Self::SoundCloud),
            "audius_enabled" => Some(Self::Audius),
            "jamendo_enabled" => Some(Self::Jamendo),
            "internet_archive_enabled" => Some(Self::InternetArchive),
            "radio_browser_enabled" => Some(Self::RadioBrowser),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchSettingMutation {
    DefaultSource(SearchSource),
    Catalog {
        catalog: SearchCatalog,
        enabled: bool,
    },
    AudiusAppName(Option<String>),
    JamendoClientId(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiSettingMutation {
    Language(Language),
    Mouse(bool),
    AlbumArt(bool),
    RomanizedTitles(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StorageSettingMutation {
    DownloadDir(Option<PathBuf>),
    CookiesFile(Option<PathBuf>),
    DownloadConcurrency(usize),
}

macro_rules! animation_fields {
    ($($variant:ident => ($id:literal, $slot:ident)),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum AnimationField {
            $($variant),+
        }

        impl AnimationField {
            #[cfg(test)]
            pub(crate) const ALL: [Self; animation_fields!(@count $($variant),+)] = [
                $(Self::$variant),+
            ];

            #[cfg(test)]
            pub(crate) const fn id(self) -> &'static str {
                match self {
                    $(Self::$variant => $id),+
                }
            }

            fn from_id(id: &str) -> Option<Self> {
                match id {
                    $($id => Some(Self::$variant)),+,
                    _ => None,
                }
            }

            fn apply(self, animations: &mut AnimationsConfig, enabled: bool) {
                match self {
                    $(Self::$variant => animations.$slot = enabled),+
                }
            }

            #[cfg(test)]
            fn read(self, animations: &AnimationsConfig) -> bool {
                match self {
                    $(Self::$variant => animations.$slot),+
                }
            }
        }
    };
    (@count $($item:ident),*) => {
        <[()]>::len(&[$(animation_fields!(@replace $item)),*])
    };
    (@replace $_item:ident) => { () };
}

// This is exactly the public animation-setting surface. `radio_master` is intentionally absent:
// it is an internal mode-scoped override with no settings row or GUI wire field.
animation_fields! {
    Master => ("master", master),
    PauseUnfocused => ("pause_unfocused", pause_unfocused),
    Title => ("title", title),
    Heart => ("heart", heart),
    Seekbar => ("seekbar", seekbar),
    Spinner => ("spinner", spinner),
    EqBars => ("eq_bars", eq_bars),
    Controls => ("controls", controls),
    Border => ("border", border),
    TrackIntro => ("track_intro", track_intro),
    Lyrics => ("lyrics", lyrics),
    Toast => ("toast", toast),
    VolumeFlash => ("volume_flash", volume_flash),
    LikeBurst => ("like_burst", like_burst),
    SeekFlash => ("seek_flash", seek_flash),
    Selection => ("selection", selection),
    Stagger => ("stagger", stagger),
    Caret => ("caret", caret),
    Tabs => ("tabs", tabs),
    PopupFade => ("popup_fade", popup_fade),
    Activity => ("activity", activity),
    AboutFx => ("about_fx", about_fx),
    TimeGlow => ("time_glow", time_glow),
    ProgressSparkle => ("progress_sparkle", progress_sparkle),
    BorderChase => ("border_chase", border_chase),
    PauseFlash => ("pause_flash", pause_flash),
    ErrorShake => ("error_shake", error_shake),
    Visualizer => ("visualizer", visualizer),
    Rain => ("rain", rain),
    Donut => ("donut", donut),
    Starfield => ("starfield", starfield),
    Bounce => ("bounce", bounce),
    Comets => ("comets", comets),
    Snow => ("snow", snow),
    Fireflies => ("fireflies", fireflies),
    Cube => ("cube", cube),
    Aquarium => ("aquarium", aquarium),
    Waves => ("waves", waves),
    Fireworks => ("fireworks", fireworks),
    Life => ("life", life),
    Pipes => ("pipes", pipes),
    Plasma => ("plasma", plasma),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnimationSettingMutation {
    Fps(u16),
    Toggle {
        field: AnimationField,
        enabled: bool,
    },
}

impl AnimationSettingMutation {
    pub(crate) fn apply(self, animations: &mut AnimationsConfig) {
        match self {
            Self::Fps(fps) => animations.fps = fps,
            Self::Toggle { field, enabled } => field.apply(animations, enabled),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThemeOverrideValue(String);

impl ThemeOverrideValue {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThemeSettingMutation {
    Preset(ThemePreset),
    Retro(bool),
    Override {
        role: ThemeRole,
        value: ThemeOverrideValue,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuiSettingParseError {
    BadValue,
    BadSettingValue,
    UnknownSetting,
}

impl GuiSettingParseError {
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::BadValue => "bad_value",
            Self::BadSettingValue => "bad_setting_value",
            Self::UnknownSetting => "unknown_setting",
        }
    }
}

impl TryFrom<GuiSettingChange> for GuiSettingMutation {
    type Error = GuiSettingParseError;

    fn try_from(change: GuiSettingChange) -> Result<Self, Self::Error> {
        let GuiSettingChange {
            group,
            field,
            value,
        } = change;
        let bad = GuiSettingParseError::BadValue;

        let mutation = match (group.as_str(), field.as_str()) {
            ("playback", "speed_tenths") => {
                PlaybackSettingMutation::SpeedTenths(parse_u16(&value).ok_or(bad)?).into()
            }
            ("playback", "seek_seconds") => {
                PlaybackSettingMutation::SeekSeconds(parse_u16(&value).ok_or(bad)?).into()
            }
            ("playback", "gapless") => {
                PlaybackSettingMutation::Gapless(value.as_bool().ok_or(bad)?).into()
            }
            ("playback", "enqueue_next") => {
                PlaybackSettingMutation::EnqueueNext(value.as_bool().ok_or(bad)?).into()
            }
            ("playback", "autoplay_on_start") => {
                PlaybackSettingMutation::AutoplayOnStart(value.as_bool().ok_or(bad)?).into()
            }
            ("playback", "mouse_wheel_volume") => {
                PlaybackSettingMutation::MouseWheelVolume(value.as_bool().ok_or(bad)?).into()
            }
            ("playback", "media_controls") => {
                PlaybackSettingMutation::MediaControls(value.as_bool().ok_or(bad)?).into()
            }
            ("playback", "volume") => {
                PlaybackSettingMutation::Volume(value.as_i64().ok_or(bad)?).into()
            }
            ("playback", "shuffle") => {
                PlaybackSettingMutation::Shuffle(value.as_bool().ok_or(bad)?).into()
            }
            ("playback", "repeat") => {
                PlaybackSettingMutation::Repeat(parse_json(value).ok_or(bad)?).into()
            }

            ("audio", "backend") => {
                let backend = match value.as_str() {
                    Some("mpv") => AudioBackend::Mpv,
                    _ => return Err(bad),
                };
                AudioSettingMutation::Backend(backend).into()
            }
            ("audio", "mpv_output") => {
                AudioSettingMutation::MpvOutput(parse_optional_string(value).ok_or(bad)?).into()
            }
            ("audio", "mpv_device") => {
                AudioSettingMutation::MpvDevice(parse_optional_string(value).ok_or(bad)?).into()
            }
            ("audio", "long_form_seek_optimization") => {
                let mode = value
                    .as_str()
                    .and_then(LongFormSeekOptimization::from_id)
                    .ok_or(GuiSettingParseError::BadSettingValue)?;
                AudioSettingMutation::LongFormSeekOptimization(mode).into()
            }
            ("audio", "mpv_cache_forward") => {
                AudioSettingMutation::MpvCacheForward(parse_trimmed_string(value).ok_or(bad)?)
                    .into()
            }
            ("audio", "mpv_cache_back") => {
                AudioSettingMutation::MpvCacheBack(parse_trimmed_string(value).ok_or(bad)?).into()
            }

            ("eq", "preset") => EqSettingMutation::Preset(parse_json(value).ok_or(bad)?).into(),
            ("eq", "bands") => EqSettingMutation::Bands(parse_json(value).ok_or(bad)?).into(),
            ("eq", "normalize") => EqSettingMutation::Normalize(value.as_bool().ok_or(bad)?).into(),

            ("streaming", "ai_enabled") => {
                StreamingSettingMutation::AiEnabled(value.as_bool().ok_or(bad)?).into()
            }
            ("streaming", "autoplay") => {
                StreamingSettingMutation::Autoplay(value.as_bool().ok_or(bad)?).into()
            }
            ("streaming", "mode") => {
                StreamingSettingMutation::Mode(parse_json(value).ok_or(bad)?).into()
            }
            ("streaming", "gemini_model") => {
                let raw = value.as_str().ok_or(bad)?;
                let model = GeminiModel::CYCLE
                    .into_iter()
                    .find(|model| model.api_id() == raw)
                    .or_else(|| parse_json(Value::String(raw.to_owned())))
                    .ok_or(bad)?;
                StreamingSettingMutation::GeminiModel(model).into()
            }

            ("search", "default_source") => {
                SearchSettingMutation::DefaultSource(parse_json(value).ok_or(bad)?).into()
            }
            ("search", "audius_app_name") => {
                SearchSettingMutation::AudiusAppName(parse_optional_string(value).ok_or(bad)?)
                    .into()
            }
            ("search", "jamendo_client_id") => {
                SearchSettingMutation::JamendoClientId(parse_optional_string(value).ok_or(bad)?)
                    .into()
            }
            ("search", field) => SearchSettingMutation::Catalog {
                catalog: SearchCatalog::from_field(field)
                    .ok_or(GuiSettingParseError::UnknownSetting)?,
                enabled: value.as_bool().ok_or(bad)?,
            }
            .into(),

            ("ui", "language") => {
                let language = match value.as_str() {
                    Some("en") => Language::English,
                    Some("ko") => Language::Korean,
                    _ => return Err(bad),
                };
                UiSettingMutation::Language(language).into()
            }
            ("ui", "mouse") => UiSettingMutation::Mouse(value.as_bool().ok_or(bad)?).into(),
            ("ui", "album_art") => UiSettingMutation::AlbumArt(value.as_bool().ok_or(bad)?).into(),
            ("ui", "romanized_titles") => {
                UiSettingMutation::RomanizedTitles(value.as_bool().ok_or(bad)?).into()
            }

            ("storage", "download_dir") => StorageSettingMutation::DownloadDir(
                parse_optional_string(value).ok_or(bad)?.map(PathBuf::from),
            )
            .into(),
            ("storage", "cookies_file") => StorageSettingMutation::CookiesFile(
                parse_optional_string(value).ok_or(bad)?.map(PathBuf::from),
            )
            .into(),
            ("storage", "download_concurrency") => {
                let concurrency = value.as_u64().filter(|value| (1..=16).contains(value));
                StorageSettingMutation::DownloadConcurrency(concurrency.ok_or(bad)? as usize).into()
            }

            ("animations", "fps") => {
                let fps = parse_u16(&value).ok_or(bad)?.clamp(FPS_MIN, FPS_MAX);
                GuiSettingMutation::Animation(AnimationSettingMutation::Fps(fps))
            }
            ("animations", field) => {
                let field = AnimationField::from_id(field).ok_or(bad)?;
                GuiSettingMutation::Animation(AnimationSettingMutation::Toggle {
                    field,
                    enabled: value.as_bool().ok_or(bad)?,
                })
            }

            ("theme", "preset") => {
                let name = value.as_str().ok_or(bad)?;
                let preset = ThemePreset::from_id(name).unwrap_or(ThemePreset::Default);
                ThemeSettingMutation::Preset(preset).into()
            }
            ("theme", "retro") => ThemeSettingMutation::Retro(value.as_bool().ok_or(bad)?).into(),
            ("theme", role_id) => {
                let role = ThemeRole::ALL
                    .into_iter()
                    .find(|role| role.id() == role_id)
                    .ok_or(GuiSettingParseError::UnknownSetting)?;
                // Historical contract: a recognized theme role with a non-string value is also
                // `unknown_setting`, while a malformed color string is `bad_value`.
                let raw = value.as_str().ok_or(GuiSettingParseError::UnknownSetting)?;
                let mut validator = ThemeConfig::default();
                validator.set_override(role, raw).map_err(|_| bad)?;
                ThemeSettingMutation::Override {
                    role,
                    value: ThemeOverrideValue(raw.to_owned()),
                }
                .into()
            }
            _ => return Err(GuiSettingParseError::UnknownSetting),
        };
        Ok(mutation)
    }
}

macro_rules! mutation_from {
    ($source:ty, $variant:ident) => {
        impl From<$source> for GuiSettingMutation {
            fn from(value: $source) -> Self {
                Self::$variant(value)
            }
        }
    };
}

mutation_from!(PlaybackSettingMutation, Playback);
mutation_from!(AudioSettingMutation, Audio);
mutation_from!(EqSettingMutation, Eq);
mutation_from!(StreamingSettingMutation, Streaming);
mutation_from!(SearchSettingMutation, Search);
mutation_from!(UiSettingMutation, Ui);
mutation_from!(StorageSettingMutation, Storage);
mutation_from!(ThemeSettingMutation, Theme);

fn parse_u16(value: &Value) -> Option<u16> {
    value.as_u64().and_then(|value| u16::try_from(value).ok())
}

fn parse_json<T: DeserializeOwned>(value: Value) -> Option<T> {
    serde_json::from_value(value).ok()
}

fn parse_optional_string(value: Value) -> Option<Option<String>> {
    match value {
        Value::Null => Some(None),
        Value::String(value) => Some(blank_to_none(&value)),
        _ => None,
    }
}

fn parse_trimmed_string(value: Value) -> Option<Option<String>> {
    match value {
        Value::String(value) => Some(blank_to_none(&value)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;

    use super::*;

    fn change(group: &str, field: &str, value: Value) -> GuiSettingChange {
        GuiSettingChange {
            group: group.to_owned(),
            field: field.to_owned(),
            value,
        }
    }

    fn parse(group: &str, field: &str, value: Value) -> GuiSettingMutation {
        GuiSettingMutation::try_from(change(group, field, value)).unwrap()
    }

    #[test]
    fn every_fixed_field_has_a_typed_parser_lane() {
        let cases = [
            ("playback", "speed_tenths", json!(10)),
            ("playback", "seek_seconds", json!(5)),
            ("playback", "gapless", json!(true)),
            ("playback", "enqueue_next", json!(true)),
            ("playback", "autoplay_on_start", json!(true)),
            ("playback", "mouse_wheel_volume", json!(true)),
            ("playback", "media_controls", json!(true)),
            ("playback", "volume", json!(50)),
            ("playback", "shuffle", json!(true)),
            ("playback", "repeat", json!("all")),
            ("audio", "backend", json!("mpv")),
            ("audio", "mpv_output", json!("auto")),
            ("audio", "mpv_device", json!("speakers")),
            ("audio", "long_form_seek_optimization", json!("auto")),
            ("audio", "mpv_cache_forward", json!("32MiB")),
            ("audio", "mpv_cache_back", json!("8MiB")),
            ("eq", "preset", json!("rock")),
            (
                "eq",
                "bands",
                json!([0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            ),
            ("eq", "normalize", json!(true)),
            ("streaming", "ai_enabled", json!(true)),
            ("streaming", "autoplay", json!(true)),
            ("streaming", "mode", json!("Balanced")),
            ("streaming", "gemini_model", json!("flash_lite")),
            ("search", "default_source", json!("youtube")),
            ("search", "soundcloud_enabled", json!(true)),
            ("search", "audius_enabled", json!(true)),
            ("search", "jamendo_enabled", json!(true)),
            ("search", "internet_archive_enabled", json!(true)),
            ("search", "radio_browser_enabled", json!(true)),
            ("search", "audius_app_name", json!("app")),
            ("search", "jamendo_client_id", json!("client")),
            ("ui", "language", json!("en")),
            ("ui", "mouse", json!(true)),
            ("ui", "album_art", json!(true)),
            ("ui", "romanized_titles", json!(true)),
            ("storage", "download_dir", json!("/tmp/downloads")),
            ("storage", "cookies_file", json!("/tmp/cookies")),
            ("storage", "download_concurrency", json!(2)),
            ("theme", "preset", json!("default")),
            ("theme", "retro", json!(true)),
        ];
        let mut fields = HashSet::new();
        for (group, field, value) in cases {
            assert!(
                fields.insert((group, field)),
                "duplicate parser fixture for {group}.{field}"
            );
            GuiSettingMutation::try_from(change(group, field, value))
                .unwrap_or_else(|error| panic!("{group}.{field}: {}", error.reason()));
        }
        assert_eq!(fields.len(), 40);
    }

    #[test]
    fn every_animation_field_is_typed_and_updates_only_its_named_slot() {
        let mut ids = HashSet::new();
        for field in AnimationField::ALL {
            assert!(ids.insert(field.id()), "duplicate animation id");
            let mutation = parse("animations", field.id(), json!(true));
            assert_eq!(
                mutation,
                GuiSettingMutation::Animation(AnimationSettingMutation::Toggle {
                    field,
                    enabled: true,
                })
            );
            let mut animations = AnimationsConfig::default();
            let GuiSettingMutation::Animation(mutation) = mutation else {
                unreachable!()
            };
            mutation.apply(&mut animations);
            assert!(field.read(&animations));
            assert_eq!(animations.radio_master, None);
        }
        assert_eq!(ids.len(), 42);
    }

    #[test]
    fn every_theme_role_is_typed_before_it_reaches_the_owner() {
        let mut ids = HashSet::new();
        for role in ThemeRole::ALL {
            assert!(ids.insert(role.id()), "duplicate theme role id");
            assert_eq!(
                parse("theme", role.id(), json!("#123456")),
                GuiSettingMutation::Theme(ThemeSettingMutation::Override {
                    role,
                    value: ThemeOverrideValue("#123456".to_owned()),
                })
            );
        }
        assert_eq!(ids.len(), ThemeRole::ALL.len());
    }

    #[test]
    fn optional_strings_and_cache_values_keep_historical_trimming() {
        assert_eq!(
            parse("audio", "mpv_output", json!("  pulse  ")),
            GuiSettingMutation::Audio(AudioSettingMutation::MpvOutput(Some("pulse".to_owned())))
        );
        assert_eq!(
            parse("audio", "mpv_device", json!("   ")),
            GuiSettingMutation::Audio(AudioSettingMutation::MpvDevice(None))
        );
        assert_eq!(
            parse("search", "audius_app_name", json!(" \t ")),
            GuiSettingMutation::Search(SearchSettingMutation::AudiusAppName(None))
        );
        assert_eq!(
            parse("search", "jamendo_client_id", json!("  client  ")),
            GuiSettingMutation::Search(SearchSettingMutation::JamendoClientId(Some(
                "client".to_owned()
            )))
        );
        assert_eq!(
            parse("storage", "download_dir", Value::Null),
            GuiSettingMutation::Storage(StorageSettingMutation::DownloadDir(None))
        );
        assert_eq!(
            parse("storage", "cookies_file", json!("  /tmp/cookies  ")),
            GuiSettingMutation::Storage(StorageSettingMutation::CookiesFile(Some(PathBuf::from(
                "/tmp/cookies"
            ))))
        );
        assert_eq!(
            parse("audio", "mpv_cache_forward", json!("  64MiB  ")),
            GuiSettingMutation::Audio(AudioSettingMutation::MpvCacheForward(Some(
                "64MiB".to_owned()
            )))
        );
    }

    #[test]
    fn compatibility_fallbacks_and_aliases_are_resolved_at_the_boundary() {
        assert_eq!(
            parse("theme", "preset", json!("future-theme")),
            GuiSettingMutation::Theme(ThemeSettingMutation::Preset(ThemePreset::Default))
        );
        for (raw, expected) in [
            ("gemini-2.5-flash", GeminiModel::Flash),
            ("flash", GeminiModel::Flash),
            ("gemini-flash-latest", GeminiModel::Latest),
            ("latest", GeminiModel::Latest),
        ] {
            assert_eq!(
                parse("streaming", "gemini_model", json!(raw)),
                GuiSettingMutation::Streaming(StreamingSettingMutation::GeminiModel(expected))
            );
        }
        assert_eq!(
            parse("animations", "fps", json!(999)),
            GuiSettingMutation::Animation(AnimationSettingMutation::Fps(FPS_MAX))
        );
        assert_eq!(
            parse("animations", "fps", json!(0)),
            GuiSettingMutation::Animation(AnimationSettingMutation::Fps(FPS_MIN))
        );
    }

    #[test]
    fn parser_preserves_the_three_daemon_error_contracts() {
        let cases = [
            ("playback", "speed_tenths", json!("fast"), "bad_value"),
            ("eq", "preset", json!("future"), "bad_value"),
            ("animations", "missing", json!(true), "bad_value"),
            ("animations", "master", json!(1), "bad_value"),
            ("theme", "accent", json!("bad-hex"), "bad_value"),
            (
                "audio",
                "long_form_seek_optimization",
                json!("future"),
                "bad_setting_value",
            ),
            (
                "audio",
                "long_form_seek_optimization",
                json!(false),
                "bad_setting_value",
            ),
            ("theme", "missing_role", json!("#ffffff"), "unknown_setting"),
            ("theme", "accent", json!(false), "unknown_setting"),
            ("playback", "missing", json!(true), "unknown_setting"),
            ("missing", "field", json!(true), "unknown_setting"),
        ];
        for (group, field, value, reason) in cases {
            let error = GuiSettingMutation::try_from(change(group, field, value)).unwrap_err();
            assert_eq!(error.reason(), reason, "{group}.{field}");
        }
    }
}
