use serde_json::Value;

use crate::player::PlayerCmd;
use crate::remote::proto::{RemoteResponse, RemoteSettingChange};

use super::{DaemonEngine, EngineEffect, EngineError};

impl DaemonEngine {
    /// Route one GUI `apply { group.field = value }` onto the live config. Fields that
    /// already have a [`RemoteSettingChange`] lane reuse it (live player/effect hooks
    /// included); the rest write config directly. Every accepted change is followed by
    /// a `settings_snapshot` push (the publisher diffs post-turn).
    pub(super) fn apply_gui_setting(
        &mut self,
        change: crate::remote::proto::GuiSettingChange,
    ) -> (RemoteResponse, Vec<EngineEffect>) {
        use RemoteSettingChange as S;
        let crate::remote::proto::GuiSettingChange {
            group,
            field,
            value,
        } = change;

        let as_bool = || value.as_bool();
        let as_u16 = || value.as_u64().and_then(|v| u16::try_from(v).ok());
        let as_str = || value.as_str().map(str::to_string);
        let as_optional_str = || match &value {
            Value::Null => Some(None),
            Value::String(s) => Some((!s.trim().is_empty()).then(|| s.trim().to_string())),
            _ => None,
        };
        let bad = || (RemoteResponse::err("bad_value"), Vec::new());
        let ok = |this: &Self| (RemoteResponse::status(this.status()), Vec::new());

        match (group.as_str(), field.as_str()) {
            ("playback", "speed_tenths") => match as_u16() {
                Some(tenths) => self.set_setting(S::Speed { tenths }),
                None => bad(),
            },
            ("playback", "seek_seconds") => match as_u16() {
                Some(seconds) => self.set_setting(S::SeekSeconds { seconds }),
                None => bad(),
            },
            ("playback", "gapless") => match as_bool() {
                Some(value) => self.set_setting(S::Gapless { value }),
                None => bad(),
            },
            ("playback", "enqueue_next") => match as_bool() {
                Some(v) => {
                    self.config.enqueue_next = Some(v);
                    self.save_config("daemon enqueue-next setting");
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "autoplay_on_start") => match as_bool() {
                Some(v) => {
                    self.config.autoplay_on_start = Some(v);
                    self.save_config("daemon autoplay-on-start setting");
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "mouse_wheel_volume") => match as_bool() {
                Some(v) => {
                    self.config.mouse_wheel_volume = Some(v);
                    self.save_config("daemon wheel-volume setting");
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "media_controls") => match as_bool() {
                Some(v) => {
                    // The OS session itself is created at daemon start; the toggle
                    // takes full effect on the next launch (same as the TUI).
                    self.config.media_controls = Some(v);
                    self.save_config("daemon media-controls setting");
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "volume") => match value.as_i64() {
                Some(v) => (self.set_volume(v), Vec::new()),
                None => bad(),
            },
            ("playback", "shuffle") => match as_bool() {
                Some(v) => {
                    if self.queue.shuffle != v {
                        self.queue.toggle_shuffle();
                        self.config.shuffle = Some(self.queue.shuffle);
                        self.save_config("daemon shuffle setting");
                        self.save_session();
                    }
                    ok(self)
                }
                None => bad(),
            },
            ("playback", "repeat") => {
                match serde_json::from_value::<crate::queue::Repeat>(value.clone()) {
                    // Music-mode invariant: can't enable repeat while autoplay streaming is on.
                    Ok(repeat) if repeat.set_blocked_by_streaming(self.streaming) => (
                        RemoteResponse::err("incompatible_playback_modes"),
                        Vec::new(),
                    ),
                    Ok(repeat) => {
                        self.queue.repeat = repeat;
                        self.config.repeat = repeat;
                        self.save_config("daemon repeat setting");
                        self.save_session();
                        ok(self)
                    }
                    Err(_) => bad(),
                }
            }
            ("audio", "backend") => match as_str().as_deref() {
                Some("mpv") => {
                    self.config.audio.backend = crate::config::AudioBackend::Mpv;
                    self.save_config("daemon audio backend setting");
                    ok(self)
                }
                _ => bad(),
            },
            ("audio", "mpv_output") => match as_optional_str() {
                Some(value) => {
                    self.config.audio.mpv.output = value;
                    self.save_config("daemon mpv output setting");
                    ok(self)
                }
                None => bad(),
            },
            ("audio", "mpv_device") => match as_optional_str() {
                Some(value) => {
                    self.config.audio.mpv.set_manual_device(value);
                    self.save_config("daemon mpv device setting");
                    ok(self)
                }
                None => bad(),
            },
            ("audio", "long_form_seek_optimization") => match as_str()
                .as_deref()
                .and_then(crate::config::LongFormSeekOptimization::from_id)
            {
                Some(mode) => {
                    if let Err(error) = self.send_player_command_if_active(
                        "set_long_form_seek_optimization",
                        PlayerCmd::SetLongFormSeekOptimization(mode),
                    ) {
                        return (self.reject_player_command(error), Vec::new());
                    }
                    self.config.audio.mpv.long_form_seek_optimization = mode;
                    self.save_config("daemon long-form seek optimization setting");
                    ok(self)
                }
                None => (RemoteResponse::err("bad_setting_value"), Vec::new()),
            },
            ("audio", "mpv_cache_forward") => match as_str() {
                Some(value) => {
                    self.config
                        .audio
                        .mpv
                        .set_cache_forward(crate::settings::blank_to_none(&value));
                    self.save_config("daemon mpv forward-cache setting");
                    ok(self)
                }
                None => bad(),
            },
            ("audio", "mpv_cache_back") => match as_str() {
                Some(value) => {
                    self.config
                        .audio
                        .mpv
                        .set_cache_back(crate::settings::blank_to_none(&value));
                    self.save_config("daemon mpv back-cache setting");
                    ok(self)
                }
                None => bad(),
            },
            ("eq", "preset") => match as_str()
                .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok())
            {
                Some(preset) => {
                    let previous_preset = self.config.eq_preset;
                    let previous_bands = self.config.eq_bands;
                    self.config.eq_preset = preset;
                    self.config.eq_bands = None; // preset gains take over
                    if let Err(error) = self.apply_audio_filter() {
                        self.config.eq_preset = previous_preset;
                        self.config.eq_bands = previous_bands;
                        return (self.reject_player_command(error), Vec::new());
                    }
                    self.save_config("daemon eq preset");
                    ok(self)
                }
                None => bad(),
            },
            ("eq", "bands") => match serde_json::from_value::<[f64; 10]>(value.clone()) {
                Ok(bands) => {
                    let previous_preset = self.config.eq_preset;
                    let previous_bands = self.config.eq_bands;
                    self.config.eq_bands = Some(bands);
                    self.config.eq_preset = crate::eq::EqPreset::Custom;
                    if let Err(error) = self.apply_audio_filter() {
                        self.config.eq_preset = previous_preset;
                        self.config.eq_bands = previous_bands;
                        return (self.reject_player_command(error), Vec::new());
                    }
                    self.save_config("daemon eq bands");
                    ok(self)
                }
                Err(_) => bad(),
            },
            ("eq", "normalize") => match as_bool() {
                Some(value) => self.set_setting(S::Normalize { value }),
                None => bad(),
            },
            ("streaming", "ai_enabled") => match as_bool() {
                Some(value) => self.set_setting(S::AiEnabled { value }),
                None => bad(),
            },
            ("streaming", "autoplay") => match as_bool() {
                Some(value) => self.set_setting(S::AutoplayStreaming { value }),
                None => bad(),
            },
            ("streaming", "mode") => match serde_json::from_value(value.clone()) {
                Ok(value) => self.set_setting(S::StreamingMode { value }),
                Err(_) => bad(),
            },
            ("streaming", "gemini_model") => {
                let parsed = as_str().and_then(|s| {
                    crate::ai::GeminiModel::CYCLE
                        .into_iter()
                        .find(|m| m.api_id() == s)
                        .or_else(|| {
                            serde_json::from_value(serde_json::Value::String(s.clone())).ok()
                        })
                });
                match parsed {
                    Some(model) => {
                        self.config.gemini_model = model;
                        self.save_config("daemon gemini model");
                        ok(self)
                    }
                    None => bad(),
                }
            }
            ("search", "default_source") => match serde_json::from_value(value.clone()) {
                Ok(source) => {
                    self.config.search.source = source;
                    self.save_config("daemon search source");
                    ok(self)
                }
                Err(_) => bad(),
            },
            (
                "search",
                flag @ ("soundcloud_enabled"
                | "audius_enabled"
                | "jamendo_enabled"
                | "internet_archive_enabled"
                | "radio_browser_enabled"),
            ) => match as_bool() {
                Some(v) => {
                    match flag {
                        "soundcloud_enabled" => self.config.search.soundcloud = v,
                        "audius_enabled" => self.config.search.audius = v,
                        "jamendo_enabled" => self.config.search.jamendo = v,
                        "internet_archive_enabled" => self.config.search.internet_archive = v,
                        _ => self.config.search.radio_browser = v,
                    }
                    self.save_config("daemon search catalogs");
                    ok(self)
                }
                None => bad(),
            },
            ("search", "audius_app_name") => match as_optional_str() {
                Some(value) => {
                    self.config.search.audius_app_name = value;
                    self.save_config("daemon audius app name");
                    ok(self)
                }
                None => bad(),
            },
            ("search", "jamendo_client_id") => match as_optional_str() {
                Some(value) => {
                    self.config.search.jamendo_client_id = value;
                    self.save_config("daemon jamendo client id");
                    ok(self)
                }
                None => bad(),
            },
            ("ui", "language") => match as_str().as_deref() {
                Some("en") => {
                    self.config.language = crate::i18n::Language::English;
                    self.save_config("daemon language");
                    ok(self)
                }
                Some("ko") => {
                    self.config.language = crate::i18n::Language::Korean;
                    self.save_config("daemon language");
                    ok(self)
                }
                _ => bad(),
            },
            ("ui", "mouse") => match as_bool() {
                Some(v) => {
                    self.config.mouse = Some(v);
                    self.save_config("daemon mouse setting");
                    ok(self)
                }
                None => bad(),
            },
            ("ui", "album_art") => match as_bool() {
                Some(v) => {
                    self.config.album_art = Some(v);
                    self.save_config("daemon album art setting");
                    ok(self)
                }
                None => bad(),
            },
            ("ui", "romanized_titles") => match as_bool() {
                Some(v) => {
                    self.config.romanized_titles = Some(v);
                    self.save_config("daemon romanized titles setting");
                    ok(self)
                }
                None => bad(),
            },
            ("storage", "download_dir") => match as_optional_str() {
                Some(value) => {
                    self.config.download_dir = value.map(std::path::PathBuf::from);
                    self.save_config("daemon download dir");
                    ok(self)
                }
                None => bad(),
            },
            ("storage", "cookies_file") => match as_optional_str() {
                Some(value) => {
                    self.config.cookies_file = value.map(std::path::PathBuf::from);
                    self.save_config("daemon cookies file");
                    ok(self)
                }
                None => bad(),
            },
            ("storage", "download_concurrency") => match value.as_u64() {
                Some(v @ 1..=16) => {
                    self.config.download_concurrency = Some(v as usize);
                    self.save_config("daemon download concurrency");
                    ok(self)
                }
                _ => bad(),
            },
            ("animations", field) => match self.apply_animation_field(field, &value) {
                true => {
                    self.save_config("daemon animations setting");
                    ok(self)
                }
                false => bad(),
            },
            ("theme", "preset") => match as_str() {
                Some(name) => {
                    // Preserve the existing wire behavior for unknown names (fall back to
                    // Default), but route recognized presets through the shared transition
                    // path so built-in overrides are discarded and Custom stays durable.
                    let preset = crate::theme::ThemePreset::from_id(&name)
                        .unwrap_or(crate::theme::ThemePreset::Default);
                    self.config.theme.set_preset(preset);
                    self.save_config("daemon theme preset");
                    ok(self)
                }
                None => bad(),
            },
            ("theme", "retro") => match as_bool() {
                Some(v) => {
                    self.config.retro_mode = v;
                    self.save_config("daemon retro mode");
                    ok(self)
                }
                None => bad(),
            },
            ("theme", role_id) => {
                let role = crate::theme::ThemeRole::ALL
                    .into_iter()
                    .find(|role| role.id() == role_id);
                match (role, as_str()) {
                    (Some(role), Some(hex)) => {
                        let mut theme = self.config.theme.clone();
                        match theme.set_override(role, &hex) {
                            Ok(()) => {
                                self.config.theme = theme;
                                self.save_config("daemon theme override");
                                ok(self)
                            }
                            Err(_) => bad(),
                        }
                    }
                    _ => (RemoteResponse::err("unknown_setting"), Vec::new()),
                }
            }
            _ => (RemoteResponse::err("unknown_setting"), Vec::new()),
        }
    }

    /// Set one [`AnimationsConfig`] field by its wire name; `false` = unknown field or
    /// wrong value type.
    fn apply_animation_field(&mut self, field: &str, value: &serde_json::Value) -> bool {
        let anim = &mut self.config.animations;
        if field == "fps" {
            let Some(fps) = value.as_u64().and_then(|v| u16::try_from(v).ok()) else {
                return false;
            };
            anim.fps = fps.clamp(crate::config::FPS_MIN, crate::config::FPS_MAX);
            return true;
        }
        let Some(v) = value.as_bool() else {
            return false;
        };
        let slot = match field {
            "master" => &mut anim.master,
            "pause_unfocused" => &mut anim.pause_unfocused,
            "title" => &mut anim.title,
            "heart" => &mut anim.heart,
            "seekbar" => &mut anim.seekbar,
            "spinner" => &mut anim.spinner,
            "eq_bars" => &mut anim.eq_bars,
            "controls" => &mut anim.controls,
            "border" => &mut anim.border,
            "track_intro" => &mut anim.track_intro,
            "lyrics" => &mut anim.lyrics,
            "toast" => &mut anim.toast,
            "volume_flash" => &mut anim.volume_flash,
            "like_burst" => &mut anim.like_burst,
            "seek_flash" => &mut anim.seek_flash,
            "selection" => &mut anim.selection,
            "stagger" => &mut anim.stagger,
            "caret" => &mut anim.caret,
            "tabs" => &mut anim.tabs,
            "popup_fade" => &mut anim.popup_fade,
            "activity" => &mut anim.activity,
            "about_fx" => &mut anim.about_fx,
            "time_glow" => &mut anim.time_glow,
            "progress_sparkle" => &mut anim.progress_sparkle,
            "border_chase" => &mut anim.border_chase,
            "pause_flash" => &mut anim.pause_flash,
            "error_shake" => &mut anim.error_shake,
            "visualizer" => &mut anim.visualizer,
            "rain" => &mut anim.rain,
            "donut" => &mut anim.donut,
            "starfield" => &mut anim.starfield,
            "bounce" => &mut anim.bounce,
            "comets" => &mut anim.comets,
            "snow" => &mut anim.snow,
            "fireflies" => &mut anim.fireflies,
            "cube" => &mut anim.cube,
            "aquarium" => &mut anim.aquarium,
            "waves" => &mut anim.waves,
            "fireworks" => &mut anim.fireworks,
            "life" => &mut anim.life,
            "pipes" => &mut anim.pipes,
            "plasma" => &mut anim.plasma,
            _ => return false,
        };
        *slot = v;
        true
    }

    /// Re-send the current audio filter chain (EQ + normalize) to the live player.
    pub(super) fn apply_audio_filter(&self) -> Result<(), EngineError> {
        let af = self.current_audio_filter();
        self.send_player_command_if_active("set_audio_filter", PlayerCmd::SetAudioFilter(af))
    }
}
