use crate::player::PlayerCmd;
use crate::remote::proto::{RemoteResponse, RemoteSettingChange};
use crate::settings::gui_mutation::{
    AudioSettingMutation, EqSettingMutation, GuiSettingMutation, PlaybackSettingMutation,
    SearchCatalog, SearchSettingMutation, StorageSettingMutation, StreamingSettingMutation,
    ThemeSettingMutation, UiSettingMutation,
};

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
        let mutation = match GuiSettingMutation::try_from(change) {
            Ok(mutation) => mutation,
            Err(error) => return (RemoteResponse::err(error.reason()), Vec::new()),
        };
        let ok = |this: &Self| (RemoteResponse::status(this.status()), Vec::new());

        match mutation {
            GuiSettingMutation::Playback(mutation) => match mutation {
                PlaybackSettingMutation::SpeedTenths(tenths) => {
                    self.set_setting(S::Speed { tenths })
                }
                PlaybackSettingMutation::SeekSeconds(seconds) => {
                    self.set_setting(S::SeekSeconds { seconds })
                }
                PlaybackSettingMutation::Gapless(value) => self.set_setting(S::Gapless { value }),
                PlaybackSettingMutation::EnqueueNext(value) => {
                    self.config.enqueue_next = Some(value);
                    self.save_config("daemon enqueue-next setting");
                    ok(self)
                }
                PlaybackSettingMutation::AutoplayOnStart(value) => {
                    self.config.autoplay_on_start = Some(value);
                    self.save_config("daemon autoplay-on-start setting");
                    ok(self)
                }
                PlaybackSettingMutation::MouseWheelVolume(value) => {
                    self.config.mouse_wheel_volume = Some(value);
                    self.save_config("daemon wheel-volume setting");
                    ok(self)
                }
                PlaybackSettingMutation::MediaControls(value) => {
                    // The OS session itself is created at daemon start; the toggle takes full
                    // effect on the next launch (same as the TUI).
                    self.config.media_controls = Some(value);
                    self.save_config("daemon media-controls setting");
                    ok(self)
                }
                PlaybackSettingMutation::Volume(value) => (self.set_volume(value), Vec::new()),
                PlaybackSettingMutation::Shuffle(value) => {
                    if self.queue.shuffle != value {
                        self.queue.toggle_shuffle();
                        self.config.shuffle = Some(self.queue.shuffle);
                        self.save_config("daemon shuffle setting");
                        self.save_session();
                    }
                    ok(self)
                }
                PlaybackSettingMutation::Repeat(repeat) => {
                    let transition = crate::playback_policy::PlaybackModeState::new(
                        self.queue.repeat,
                        self.streaming,
                    )
                    .transition(
                        crate::playback_policy::PlaybackModeAction::SetRepeat(repeat),
                    );
                    let Ok(transition) = transition else {
                        return (
                            RemoteResponse::err("incompatible_playback_modes"),
                            Vec::new(),
                        );
                    };
                    self.queue.repeat = transition.state.repeat;
                    self.config.repeat = transition.state.repeat;
                    self.save_config("daemon repeat setting");
                    self.save_session();
                    ok(self)
                }
            },
            GuiSettingMutation::Audio(mutation) => match mutation {
                AudioSettingMutation::Backend(backend) => {
                    self.config.audio.backend = backend;
                    self.save_config("daemon audio backend setting");
                    ok(self)
                }
                AudioSettingMutation::MpvOutput(value) => {
                    self.config.audio.mpv.output = value;
                    self.save_config("daemon mpv output setting");
                    ok(self)
                }
                AudioSettingMutation::MpvDevice(value) => {
                    self.config.audio.mpv.set_manual_device(value);
                    self.save_config("daemon mpv device setting");
                    ok(self)
                }
                AudioSettingMutation::LongFormSeekOptimization(mode) => {
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
                AudioSettingMutation::MpvCacheForward(value) => {
                    self.config.audio.mpv.set_cache_forward(value);
                    self.save_config("daemon mpv forward-cache setting");
                    ok(self)
                }
                AudioSettingMutation::MpvCacheBack(value) => {
                    self.config.audio.mpv.set_cache_back(value);
                    self.save_config("daemon mpv back-cache setting");
                    ok(self)
                }
            },
            GuiSettingMutation::Eq(mutation) => match mutation {
                EqSettingMutation::Preset(preset) => {
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
                EqSettingMutation::Bands(bands) => {
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
                EqSettingMutation::Normalize(value) => self.set_setting(S::Normalize { value }),
            },
            GuiSettingMutation::Streaming(mutation) => match mutation {
                StreamingSettingMutation::AiEnabled(value) => {
                    self.set_setting(S::AiEnabled { value })
                }
                StreamingSettingMutation::Autoplay(value) => {
                    self.set_setting(S::AutoplayStreaming { value })
                }
                StreamingSettingMutation::Mode(value) => {
                    self.set_setting(S::StreamingMode { value })
                }
                StreamingSettingMutation::GeminiModel(model) => {
                    self.config.gemini_model = model;
                    self.save_config("daemon gemini model");
                    ok(self)
                }
            },
            GuiSettingMutation::Search(mutation) => match mutation {
                SearchSettingMutation::DefaultSource(source) => {
                    self.config.search.source = source;
                    self.save_config("daemon search source");
                    ok(self)
                }
                SearchSettingMutation::Catalog { catalog, enabled } => {
                    match catalog {
                        SearchCatalog::SoundCloud => self.config.search.soundcloud = enabled,
                        SearchCatalog::Audius => self.config.search.audius = enabled,
                        SearchCatalog::Jamendo => self.config.search.jamendo = enabled,
                        SearchCatalog::InternetArchive => {
                            self.config.search.internet_archive = enabled;
                        }
                        SearchCatalog::RadioBrowser => self.config.search.radio_browser = enabled,
                    }
                    self.save_config("daemon search catalogs");
                    ok(self)
                }
                SearchSettingMutation::AudiusAppName(value) => {
                    self.config.search.audius_app_name = value;
                    self.save_config("daemon audius app name");
                    ok(self)
                }
                SearchSettingMutation::JamendoClientId(value) => {
                    self.config.search.jamendo_client_id = value;
                    self.save_config("daemon jamendo client id");
                    ok(self)
                }
            },
            GuiSettingMutation::Ui(mutation) => match mutation {
                UiSettingMutation::Language(language) => {
                    self.config.language = language;
                    self.save_config("daemon language");
                    ok(self)
                }
                UiSettingMutation::Mouse(value) => {
                    self.config.mouse = Some(value);
                    self.save_config("daemon mouse setting");
                    ok(self)
                }
                UiSettingMutation::AlbumArt(value) => {
                    self.config.album_art = Some(value);
                    self.save_config("daemon album art setting");
                    ok(self)
                }
                UiSettingMutation::RomanizedTitles(value) => {
                    self.config.romanized_titles = Some(value);
                    self.save_config("daemon romanized titles setting");
                    ok(self)
                }
            },
            GuiSettingMutation::Storage(mutation) => match mutation {
                StorageSettingMutation::DownloadDir(value) => {
                    self.config.download_dir = value;
                    self.save_config("daemon download dir");
                    ok(self)
                }
                StorageSettingMutation::CookiesFile(value) => {
                    self.config.cookies_file = value;
                    self.save_config("daemon cookies file");
                    ok(self)
                }
                StorageSettingMutation::DownloadConcurrency(value) => {
                    self.config.download_concurrency = Some(value);
                    self.save_config("daemon download concurrency");
                    ok(self)
                }
            },
            GuiSettingMutation::Animation(mutation) => {
                mutation.apply(&mut self.config.animations);
                self.save_config("daemon animations setting");
                ok(self)
            }
            GuiSettingMutation::Theme(mutation) => match mutation {
                ThemeSettingMutation::Preset(preset) => {
                    // Preserve the existing wire behavior for unknown names (fall back to
                    // Default), but route recognized presets through the shared transition
                    // path so built-in overrides are discarded and Custom stays durable.
                    self.config.theme.set_preset(preset);
                    self.save_config("daemon theme preset");
                    ok(self)
                }
                ThemeSettingMutation::Retro(value) => {
                    self.config.retro_mode = value;
                    self.save_config("daemon retro mode");
                    ok(self)
                }
                ThemeSettingMutation::Override { role, value } => {
                    let mut theme = self.config.theme.clone();
                    match theme.set_override(role, value.as_str()) {
                        Ok(()) => {
                            self.config.theme = theme;
                            self.save_config("daemon theme override");
                            ok(self)
                        }
                        // The typed boundary already validated this value. Keep the old defensive
                        // rollback/error behavior if the theme validator ever gains context.
                        Err(_) => (RemoteResponse::err("bad_value"), Vec::new()),
                    }
                }
            },
        }
    }

    /// Re-send the current audio filter chain (EQ + normalize) to the live player.
    pub(super) fn apply_audio_filter(&self) -> Result<(), EngineError> {
        let af = self.current_audio_filter();
        self.send_player_command_if_active("set_audio_filter", PlayerCmd::SetAudioFilter(af))
    }
}
