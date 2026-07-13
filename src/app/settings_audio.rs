//! Admission-atomic audio previews and Settings saves.
//!
//! Settings audio controls are audible previews, but their draft projection is valid only after
//! the corresponding mpv command enters the player lane. Saving follows the same rule for the
//! complete speed/filter batch: the screen stays open and config stays untouched on rejection.

use super::*;

#[derive(Clone, Copy, PartialEq)]
struct SettingsAudioSnapshot {
    speed: f64,
    seek_seconds: f64,
    preset: EqPreset,
    bands: [f64; eq::BANDS],
    normalize: bool,
}

impl SettingsAudioSnapshot {
    fn from_draft(draft: &SettingsDraft) -> Self {
        Self {
            speed: draft.speed,
            seek_seconds: draft.seek_seconds,
            preset: draft.eq_preset,
            bands: draft.eq_bands,
            normalize: draft.normalize,
        }
    }

    fn from_live(app: &App) -> Self {
        Self {
            speed: app.playback.speed,
            seek_seconds: app.audio.seek_seconds,
            preset: app.audio.preset,
            bands: app.audio.bands,
            normalize: app.audio.normalize,
        }
    }

    fn apply_to(self, draft: &mut SettingsDraft) {
        draft.speed = self.speed;
        draft.seek_seconds = self.seek_seconds;
        draft.eq_preset = self.preset;
        draft.eq_bands = self.bands;
        draft.normalize = self.normalize;
    }

    fn filter(self) -> String {
        eq::build_af_string(&self.bands, self.normalize).unwrap_or_default()
    }
}

#[derive(Clone)]
enum SettingsAudioPreviewApply {
    Audio(SettingsAudioSnapshot),
    Reset(Box<SettingsState>),
}

#[derive(Clone, PartialEq)]
struct SettingsResetGuard {
    projected: Vec<u8>,
    restart_beginner_tutorial: bool,
    tab: SettingsTab,
    row: usize,
    editing_text: bool,
    secret_restore: Option<String>,
    capturing: Option<(KeyContext, Action)>,
    spotify_import_mode_dropdown: Option<usize>,
    radio_mode: bool,
}

impl SettingsResetGuard {
    fn capture(config: &Config, state: &SettingsState) -> Self {
        Self {
            projected: settings_projection(config, state),
            restart_beginner_tutorial: state.draft.restart_beginner_tutorial,
            tab: state.tab,
            row: state.row,
            editing_text: state.editing_text,
            secret_restore: state.secret_restore.clone(),
            capturing: state.capturing,
            spotify_import_mode_dropdown: state.spotify_import_mode_dropdown,
            radio_mode: state.radio_mode,
        }
    }

    fn matches(&self, config: &Config, state: &SettingsState) -> bool {
        self.projected == settings_projection(config, state)
            && self.restart_beginner_tutorial == state.draft.restart_beginner_tutorial
            && self.tab == state.tab
            && self.row == state.row
            && self.editing_text == state.editing_text
            && self.secret_restore == state.secret_restore
            && self.capturing == state.capturing
            && self.spotify_import_mode_dropdown == state.spotify_import_mode_dropdown
            && self.radio_mode == state.radio_mode
    }
}

#[derive(Clone)]
pub struct SettingsAudioPreviewPlan {
    expected_draft: SettingsAudioSnapshot,
    expected_live: SettingsAudioSnapshot,
    expected_reset: Option<SettingsResetGuard>,
    apply: SettingsAudioPreviewApply,
}

#[derive(Clone)]
pub struct SettingsSavePlan {
    expected_draft: SettingsAudioSnapshot,
    expected_live: SettingsAudioSnapshot,
    expected_settings: Vec<u8>,
    settings: Box<SettingsState>,
    exit: SettingsSaveExit,
}

#[derive(Clone, Copy)]
enum SettingsSaveExit {
    Close,
    Home,
    Navigate(Mode),
    Quit,
}

impl App {
    pub(in crate::app) fn settings_preview_normalize(&self) -> Vec<Cmd> {
        let Some(expected) = self.settings_audio_draft() else {
            return Vec::new();
        };
        let next = SettingsAudioSnapshot {
            normalize: !expected.normalize,
            ..expected
        };
        self.settings_audio_intent(
            "settings_normalize_preview",
            PlayerCmd::SetAudioFilter(next.filter()),
            expected,
            SettingsAudioPreviewApply::Audio(next),
        )
    }

    pub(in crate::app) fn settings_preview_speed(&self, dir: i32) -> Vec<Cmd> {
        let Some(expected) = self.settings_audio_draft() else {
            return Vec::new();
        };
        let speed = settings::clamp_speed(expected.speed + f64::from(dir) * settings::SPEED_STEP);
        let next = SettingsAudioSnapshot { speed, ..expected };
        self.settings_audio_intent(
            "settings_speed_preview",
            PlayerCmd::SetProperty {
                name: "speed".to_owned(),
                value: serde_json::Value::from(speed),
            },
            expected,
            SettingsAudioPreviewApply::Audio(next),
        )
    }

    pub(in crate::app) fn settings_preview_eq_preset(&self, dir: i32) -> Vec<Cmd> {
        let Some(expected) = self.settings_audio_draft() else {
            return Vec::new();
        };
        let preset = if expected.preset == EqPreset::Custom {
            EqPreset::Flat
        } else {
            let current = EqPreset::CYCLE
                .iter()
                .position(|preset| *preset == expected.preset)
                .unwrap_or(0);
            let count = EqPreset::CYCLE.len();
            let next = if dir >= 0 {
                (current + 1) % count
            } else {
                (current + count - 1) % count
            };
            EqPreset::CYCLE[next]
        };
        let next = SettingsAudioSnapshot {
            preset,
            bands: preset.gains(),
            ..expected
        };
        self.settings_audio_intent(
            "settings_eq_preset_preview",
            PlayerCmd::SetAudioFilter(next.filter()),
            expected,
            SettingsAudioPreviewApply::Audio(next),
        )
    }

    /// Adjust one EQ band. A labeled chain can use mpv's glitch-free `af-command`; crossing
    /// between a flat and active chain must rebuild the complete filter string.
    pub(in crate::app) fn settings_preview_band(&self, index: usize, dir: i32) -> Vec<Cmd> {
        let Some(expected) = self.settings_audio_draft() else {
            return Vec::new();
        };
        let Some(current_gain) = expected.bands.get(index).copied() else {
            return Vec::new();
        };
        let was_active = expected.bands.iter().any(|gain| gain.abs() > f64::EPSILON);
        let gain = settings::clamp_band(current_gain + f64::from(dir) * settings::BAND_GAIN_STEP);
        let mut bands = expected.bands;
        bands[index] = gain;
        let next = SettingsAudioSnapshot {
            preset: EqPreset::Custom,
            bands,
            ..expected
        };
        let now_active = bands.iter().any(|gain| gain.abs() > f64::EPSILON);
        let command = if was_active && now_active {
            PlayerCmd::AfCommand {
                label: eq::band_label(index),
                param: "gain".to_owned(),
                value: format!("{gain}"),
            }
        } else {
            PlayerCmd::SetAudioFilter(next.filter())
        };
        self.settings_audio_intent(
            "settings_eq_band_preview",
            command,
            expected,
            SettingsAudioPreviewApply::Audio(next),
        )
    }

    /// Reset the complete working settings state only after the matching default audio batch is
    /// admitted. This keeps a rejected reset from claiming that its audible preview succeeded.
    pub(in crate::app) fn settings_reset_all(&self) -> Vec<Cmd> {
        let Some(current) = self.settings.as_deref() else {
            return Vec::new();
        };
        let expected = SettingsAudioSnapshot::from_draft(&current.draft);
        let expected_reset = SettingsResetGuard::capture(&self.config, current);
        let mut next = current.clone();
        reset_settings_state(&mut next);
        let next_audio = SettingsAudioSnapshot::from_draft(&next.draft);
        self.settings_audio_batch_intent(
            "settings_reset_all_preview",
            vec![
                PlayerCmd::SetProperty {
                    name: "speed".to_owned(),
                    value: serde_json::Value::from(next_audio.speed),
                },
                PlayerCmd::SetAudioFilter(next_audio.filter()),
            ],
            expected,
            expected_reset,
            SettingsAudioPreviewApply::Reset(Box::new(next)),
        )
    }

    /// Prepare one indivisible save transaction. The accepted commit owns all visible/config
    /// changes and follow-up effects; rejection therefore leaves the Settings session intact.
    pub(in crate::app) fn close_settings(&mut self) -> Vec<Cmd> {
        self.prepare_settings_save(SettingsSaveExit::Close)
    }

    pub(in crate::app) fn close_settings_for_home(&mut self) -> Vec<Cmd> {
        self.prepare_settings_save(SettingsSaveExit::Home)
    }

    pub(in crate::app) fn close_settings_for_navigation(&mut self, mode: Mode) -> Vec<Cmd> {
        self.prepare_settings_save(SettingsSaveExit::Navigate(mode))
    }

    pub(in crate::app) fn close_settings_for_quit(&mut self) -> Vec<Cmd> {
        self.prepare_settings_save(SettingsSaveExit::Quit)
    }

    fn prepare_settings_save(&mut self, exit: SettingsSaveExit) -> Vec<Cmd> {
        let Some(current) = self.settings.as_deref() else {
            self.mode = Mode::Player;
            self.dirty = true;
            return match exit {
                SettingsSaveExit::Close => Vec::new(),
                SettingsSaveExit::Home => self.go_home(),
                SettingsSaveExit::Navigate(mode) => self.navigate_to(mode),
                SettingsSaveExit::Quit => self.quit_app(),
            };
        };
        let draft = SettingsAudioSnapshot::from_draft(&current.draft);
        let plan = SettingsSavePlan {
            expected_draft: draft,
            expected_live: SettingsAudioSnapshot::from_live(self),
            expected_settings: settings_projection(&self.config, current),
            settings: Box::new(current.clone()),
            exit,
        };
        vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
            PlayerIntent::batch(
                "settings_save",
                vec![
                    PlayerCmd::SetProperty {
                        name: "speed".to_owned(),
                        value: serde_json::Value::from(draft.speed),
                    },
                    PlayerCmd::SetAudioFilter(draft.filter()),
                ],
                PlayerCommit::SettingsSave(Box::new(plan)),
            ),
        )))]
    }

    pub(in crate::app) fn commit_settings_audio_preview(
        &mut self,
        plan: SettingsAudioPreviewPlan,
    ) -> Vec<Cmd> {
        if !self.settings_audio_preview_is_current(&plan) {
            tracing::warn!("ignored stale Settings audio preview commit");
            return Vec::new();
        }
        match plan.apply {
            SettingsAudioPreviewApply::Audio(next) => {
                if let Some(current) = self.settings.as_deref_mut() {
                    next.apply_to(&mut current.draft);
                }
            }
            SettingsAudioPreviewApply::Reset(next) => {
                self.settings = Some(next);
                if let Some(current) = self.settings.as_deref() {
                    self.theme = current.draft.theme.normalized();
                    crate::i18n::set_language(current.draft.language);
                }
                self.status.text = t!(
                    "All settings reset to defaults",
                    "모든 설정을 기본값으로 되돌렸어요"
                )
                .to_owned();
            }
        }
        self.dirty = true;
        Vec::new()
    }

    pub(in crate::app) fn commit_settings_save_plan(&mut self, plan: SettingsSavePlan) -> Vec<Cmd> {
        if !self.settings_save_is_current(&plan) {
            tracing::warn!("ignored stale Settings save commit");
            return Vec::new();
        }
        let mut follow_ups = self.apply_settings_save(*plan.settings);
        follow_ups.extend(match plan.exit {
            SettingsSaveExit::Close => Vec::new(),
            SettingsSaveExit::Home => self.go_home(),
            SettingsSaveExit::Navigate(mode) => self.navigate_to(mode),
            SettingsSaveExit::Quit => self.quit_app(),
        });
        follow_ups
    }

    pub(in crate::app) fn settings_audio_preview_is_current(
        &self,
        plan: &SettingsAudioPreviewPlan,
    ) -> bool {
        self.settings_audio_plan_is_current(plan.expected_draft, plan.expected_live)
            && plan.expected_reset.as_ref().is_none_or(|expected| {
                self.settings
                    .as_deref()
                    .is_some_and(|current| expected.matches(&self.config, current))
            })
    }

    pub(in crate::app) fn settings_save_is_current(&self, plan: &SettingsSavePlan) -> bool {
        self.mode == Mode::Settings
            && self.settings.as_deref().is_some_and(|current| {
                SettingsAudioSnapshot::from_draft(&current.draft) == plan.expected_draft
                    && SettingsAudioSnapshot::from_live(self) == plan.expected_live
                    && settings_projection(&self.config, current) == plan.expected_settings
                    && current.draft.restart_beginner_tutorial
                        == plan.settings.draft.restart_beginner_tutorial
            })
    }

    fn settings_audio_intent(
        &self,
        label: &'static str,
        command: PlayerCmd,
        expected_draft: SettingsAudioSnapshot,
        apply: SettingsAudioPreviewApply,
    ) -> Vec<Cmd> {
        vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
            PlayerIntent::one(
                label,
                command,
                PlayerCommit::SettingsAudioPreview(Box::new(SettingsAudioPreviewPlan {
                    expected_draft,
                    expected_live: SettingsAudioSnapshot::from_live(self),
                    expected_reset: None,
                    apply,
                })),
            ),
        )))]
    }

    fn settings_audio_batch_intent(
        &self,
        label: &'static str,
        commands: Vec<PlayerCmd>,
        expected_draft: SettingsAudioSnapshot,
        expected_reset: SettingsResetGuard,
        apply: SettingsAudioPreviewApply,
    ) -> Vec<Cmd> {
        vec![Cmd::PlayerControl(PlayerControl::Intent(Box::new(
            PlayerIntent::batch(
                label,
                commands,
                PlayerCommit::SettingsAudioPreview(Box::new(SettingsAudioPreviewPlan {
                    expected_draft,
                    expected_live: SettingsAudioSnapshot::from_live(self),
                    expected_reset: Some(expected_reset),
                    apply,
                })),
            ),
        )))]
    }

    fn settings_audio_draft(&self) -> Option<SettingsAudioSnapshot> {
        self.settings
            .as_deref()
            .map(|state| SettingsAudioSnapshot::from_draft(&state.draft))
    }

    fn settings_audio_plan_is_current(
        &self,
        expected_draft: SettingsAudioSnapshot,
        expected_live: SettingsAudioSnapshot,
    ) -> bool {
        self.mode == Mode::Settings
            && self.settings_audio_draft() == Some(expected_draft)
            && SettingsAudioSnapshot::from_live(self) == expected_live
    }
}

fn settings_projection(config: &Config, state: &SettingsState) -> Vec<u8> {
    let mut projected = config.clone();
    state.draft.apply_to(&mut projected);
    projected.keybindings = state.keymap.to_overrides();
    projected.mouse_bindings = state.mousemap.to_overrides();
    serde_json::to_vec(&projected).expect("Settings config projection must serialize")
}

fn reset_settings_state(state: &mut SettingsState) {
    let defaults = Config::default();
    let draft = &mut state.draft;
    // Reset All is a user-facing factory reset, unlike `Config::default()`'s conservative
    // legacy/recovery baseline: it opts into Beginner Mode and schedules a Welcome restart.
    draft.beginner_mode = true;
    draft.restart_beginner_tutorial = true;
    draft.cookies_file = String::new();
    draft.download_dir = String::new();
    draft.search = defaults.effective_search();
    draft.mouse = defaults.effective_mouse();
    draft.album_art = defaults.effective_album_art();
    draft.autoplay_on_start = defaults.effective_autoplay_on_start();
    draft.enqueue_next = defaults.effective_enqueue_next();
    draft.speed = defaults.effective_speed();
    draft.seek_seconds = defaults.effective_seek_seconds();
    draft.gapless = defaults.effective_gapless();
    draft.autoplay_streaming = defaults.effective_autoplay_streaming();
    draft.curating_mode = crate::streaming::CuratingMode::from_ai(defaults.streaming.ai.enabled);
    draft.streaming_mode = defaults.streaming.mode;
    draft.eq_preset = defaults.eq_preset;
    draft.eq_bands = defaults.effective_eq_bands();
    draft.normalize = defaults.effective_normalize();
    draft.gemini_model = defaults.effective_gemini_model();
    draft.gemini_api_key = String::new();
    draft.ai_enabled = defaults.effective_ai_enabled();
    draft.romanized_titles = defaults.effective_romanized_titles();
    draft.dj_gem_language = defaults.dj_gem_language;
    draft.theme = defaults.effective_theme();
    draft.retro_mode = defaults.effective_retro_mode();
    draft.language = defaults.effective_language();
    draft.animations = defaults.animations;
    draft.lastfm_enabled = true;
    draft.lastfm_love_sync = true;
    draft.listenbrainz_enabled = true;
    draft.scrobble_local_files = true;
    draft.spotify_redirect_port = String::new();
    draft.spotify_import_mode = defaults.spotify.import_mode;
    draft.recording_mode = defaults.recording.mode;
    draft.recording_min_seconds = defaults.effective_recording_min();
    draft.recording_max_seconds = defaults.effective_recording_max();
    draft.recording_dir = String::new();
    draft.recording_past_tracks = defaults.effective_recording_past_tracks();
    draft.recording_notify = defaults.recording.notify;
    state.keymap = KeyMap::default();
    state.mousemap.reset_all();
    state.editing_text = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::delivery::{DeliveryError, DeliveryReceipt};

    fn app_with_settings() -> App {
        let mut app = App::new(100);
        app.open_settings();
        app
    }

    fn reject(app: &mut App, cmds: Vec<Cmd>, error: DeliveryError) -> Vec<Cmd> {
        let intent = cmds
            .into_iter()
            .find_map(|cmd| match cmd {
                Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(*intent),
                _ => None,
            })
            .expect("Settings player intent");
        crate::runtime::player_delivery::settle_player_intent(app, intent, Err(error))
    }

    fn accept(app: &mut App, cmds: &[Cmd]) -> Vec<Cmd> {
        app.admit_player_intents_with_followups_for_test(cmds)
    }

    #[test]
    fn speed_preview_commits_only_after_admission() {
        for error in [DeliveryError::Busy, DeliveryError::Closed] {
            let mut app = app_with_settings();
            let before = app.settings.as_deref().unwrap().draft.speed;
            let config_before = app.config.speed;
            let cmds = app.settings_preview_speed(1);

            assert_eq!(app.settings.as_deref().unwrap().draft.speed, before);
            assert!(reject(&mut app, cmds, error).is_empty());
            assert_eq!(app.settings.as_deref().unwrap().draft.speed, before);
            assert_eq!(app.playback.speed, before);
            assert_eq!(app.config.speed, config_before);
        }

        let mut app = app_with_settings();
        let cmds = app.settings_preview_speed(1);
        assert!(matches!(
            cmds[0].player_command(),
            Some(PlayerCmd::SetProperty { name, value })
                if name == "speed" && value.as_f64() == Some(1.1)
        ));
        assert!(accept(&mut app, &cmds).is_empty());
        assert_eq!(app.settings.as_deref().unwrap().draft.speed, 1.1);
        assert_eq!(app.playback.speed, 1.0);
        assert_eq!(app.config.speed, None);
    }

    #[test]
    fn normalize_preview_commits_only_after_admission() {
        for error in [DeliveryError::Busy, DeliveryError::Closed] {
            let mut app = app_with_settings();
            let cmds = app.settings_preview_normalize();

            assert!(!app.settings.as_deref().unwrap().draft.normalize);
            assert!(reject(&mut app, cmds, error).is_empty());
            assert!(!app.settings.as_deref().unwrap().draft.normalize);
            assert!(!app.audio.normalize);
            assert!(!app.config.effective_normalize());
        }

        let mut app = app_with_settings();
        let cmds = app.settings_preview_normalize();
        assert!(matches!(
            cmds[0].player_command(),
            Some(PlayerCmd::SetAudioFilter(filter)) if filter.contains("dynaudnorm")
        ));
        assert!(accept(&mut app, &cmds).is_empty());
        assert!(app.settings.as_deref().unwrap().draft.normalize);
        assert!(!app.audio.normalize);
        assert!(!app.config.effective_normalize());
    }

    #[test]
    fn preset_preview_commits_only_after_admission() {
        for error in [DeliveryError::Busy, DeliveryError::Closed] {
            let mut app = app_with_settings();
            let cmds = app.settings_preview_eq_preset(1);

            assert_eq!(
                app.settings.as_deref().unwrap().draft.eq_preset,
                EqPreset::Flat
            );
            assert!(reject(&mut app, cmds, error).is_empty());
            assert_eq!(
                app.settings.as_deref().unwrap().draft.eq_preset,
                EqPreset::Flat
            );
            assert_eq!(app.audio.preset, EqPreset::Flat);
            assert_eq!(app.config.eq_preset, EqPreset::Flat);
        }

        let mut app = app_with_settings();
        let cmds = app.settings_preview_eq_preset(1);
        assert!(matches!(
            cmds[0].player_command(),
            Some(PlayerCmd::SetAudioFilter(filter)) if filter.contains("equalizer")
        ));
        assert!(accept(&mut app, &cmds).is_empty());
        let draft = &app.settings.as_deref().unwrap().draft;
        assert_eq!(draft.eq_preset, EqPreset::BassBoost);
        assert_eq!(draft.eq_bands, EqPreset::BassBoost.gains());
        assert_eq!(app.audio.preset, EqPreset::Flat);
        assert_eq!(app.config.eq_preset, EqPreset::Flat);
    }

    #[test]
    fn band_preview_commits_only_after_admission_and_selects_af_command() {
        for error in [DeliveryError::Busy, DeliveryError::Closed] {
            let mut app = app_with_settings();
            let cmds = app.settings_preview_band(0, 1);

            assert_eq!(app.settings.as_deref().unwrap().draft.eq_bands[0], 0.0);
            assert!(reject(&mut app, cmds, error).is_empty());
            assert_eq!(app.settings.as_deref().unwrap().draft.eq_bands[0], 0.0);
            assert_eq!(app.audio.bands[0], 0.0);
            assert_eq!(app.config.effective_eq_bands()[0], 0.0);
        }

        let mut app = app_with_settings();
        let first = app.settings_preview_band(0, 1);
        assert!(matches!(
            first[0].player_command(),
            Some(PlayerCmd::SetAudioFilter(filter)) if filter.contains("@eq0")
        ));
        assert!(accept(&mut app, &first).is_empty());
        assert_eq!(
            app.settings.as_deref().unwrap().draft.eq_preset,
            EqPreset::Custom
        );
        assert_eq!(app.settings.as_deref().unwrap().draft.eq_bands[0], 1.0);

        let second = app.settings_preview_band(0, 1);
        assert!(matches!(
            second[0].player_command(),
            Some(PlayerCmd::AfCommand { label, param, value })
                if label == "eq0" && param == "gain" && value == "2"
        ));
        assert!(accept(&mut app, &second).is_empty());
        assert_eq!(app.settings.as_deref().unwrap().draft.eq_bands[0], 2.0);
        assert_eq!(app.audio.bands[0], 0.0);
    }

    #[test]
    fn reset_all_audio_batch_is_admission_atomic() {
        let _guard = crate::i18n::lock_for_test();
        for error in [DeliveryError::Busy, DeliveryError::Closed] {
            let mut app = app_with_settings();
            {
                let draft = &mut app.settings.as_deref_mut().unwrap().draft;
                draft.speed = 1.8;
                draft.eq_bands[0] = 6.0;
                draft.eq_preset = EqPreset::Custom;
                draft.gemini_api_key = "keep-me".to_owned();
            }
            let cmds = app.settings_reset_all();
            assert_eq!(cmds[0].player_commands().count(), 2);

            assert!(reject(&mut app, cmds, error).is_empty());
            let draft = &app.settings.as_deref().unwrap().draft;
            assert_eq!(draft.speed, 1.8);
            assert_eq!(draft.eq_bands[0], 6.0);
            assert_eq!(draft.gemini_api_key, "keep-me");
            assert!(!draft.beginner_mode);
            assert!(!draft.restart_beginner_tutorial);
            assert_eq!(app.playback.speed, 1.0);
        }

        let mut app = app_with_settings();
        app.settings.as_deref_mut().unwrap().draft.speed = 1.8;
        app.settings.as_deref_mut().unwrap().draft.gemini_api_key = "clear-me".to_owned();
        let cmds = app.settings_reset_all();
        let commands: Vec<&PlayerCmd> = cmds.iter().flat_map(Cmd::player_commands).collect();
        assert!(matches!(
            commands.as_slice(),
            [
                PlayerCmd::SetProperty { name, value },
                PlayerCmd::SetAudioFilter(_)
            ] if name == "speed" && value.as_f64() == Some(1.0)
        ));
        assert!(accept(&mut app, &cmds).is_empty());
        let draft = &app.settings.as_deref().unwrap().draft;
        assert_eq!(draft.speed, 1.0);
        assert!(draft.gemini_api_key.is_empty());
        assert!(draft.beginner_mode);
        assert!(draft.restart_beginner_tutorial);
        assert_eq!(app.playback.speed, 1.0);
        assert_eq!(app.config.speed, None);
    }

    #[test]
    fn settings_save_rejection_keeps_screen_live_state_and_config_unchanged() {
        for error in [DeliveryError::Busy, DeliveryError::Closed] {
            let mut app = app_with_settings();
            app.settings.as_deref_mut().unwrap().draft.speed = 1.6;
            app.settings.as_deref_mut().unwrap().draft.normalize = true;
            let config_speed = app.config.speed;
            let config_normalize = app.config.normalize;
            let cmds = app.close_settings();

            assert!(
                cmds.iter()
                    .all(|cmd| !matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
            );
            assert!(reject(&mut app, cmds, error).is_empty());
            assert_eq!(app.mode, Mode::Settings);
            assert!(app.settings.is_some());
            assert_eq!(app.settings.as_deref().unwrap().draft.speed, 1.6);
            assert!(app.settings.as_deref().unwrap().draft.normalize);
            assert_eq!(app.playback.speed, 1.0);
            assert!(!app.audio.normalize);
            assert_eq!(app.config.speed, config_speed);
            assert_eq!(app.config.normalize, config_normalize);
        }
    }

    #[test]
    fn settings_save_admits_speed_and_filter_then_closes_and_persists_once() {
        let mut app = app_with_settings();
        {
            let draft = &mut app.settings.as_deref_mut().unwrap().draft;
            draft.speed = 1.4;
            draft.eq_preset = EqPreset::BassBoost;
            draft.eq_bands = EqPreset::BassBoost.gains();
            draft.normalize = true;
        }
        let cmds = app.close_settings();
        let commands: Vec<&PlayerCmd> = cmds.iter().flat_map(Cmd::player_commands).collect();
        assert!(matches!(
            commands.as_slice(),
            [
                PlayerCmd::SetProperty { name, value },
                PlayerCmd::SetAudioFilter(filter)
            ] if name == "speed"
                && value.as_f64() == Some(1.4)
                && filter.contains("dynaudnorm")
                && filter.contains("equalizer")
        ));
        assert_eq!(app.mode, Mode::Settings);
        assert!(app.settings.is_some());

        let follow_ups = crate::runtime::player_delivery::settle_player_intent(
            &mut app,
            cmds.into_iter()
                .find_map(|cmd| match cmd {
                    Cmd::PlayerControl(PlayerControl::Intent(intent)) => Some(*intent),
                    _ => None,
                })
                .expect("Settings save intent"),
            Ok(DeliveryReceipt::Enqueued),
        );
        assert_eq!(app.mode, Mode::Player);
        assert!(app.settings.is_none());
        assert_eq!(app.playback.speed, 1.4);
        assert_eq!(app.audio.preset, EqPreset::BassBoost);
        assert_eq!(app.audio.bands, EqPreset::BassBoost.gains());
        assert!(app.audio.normalize);
        assert_eq!(app.config.speed, Some(1.4));
        assert_eq!(app.config.normalize, Some(true));
        assert_eq!(
            follow_ups
                .iter()
                .filter(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
                .count(),
            1
        );
        assert!(
            follow_ups
                .iter()
                .flat_map(Cmd::player_commands)
                .next()
                .is_none()
        );
    }

    #[test]
    fn settings_navigation_and_quit_wait_for_save_admission() {
        let mut app = app_with_settings();
        let cmds = app.go_home();
        assert!(reject(&mut app, cmds, DeliveryError::Busy).is_empty());
        assert_eq!(app.mode, Mode::Settings);
        assert!(app.settings.is_some());

        let cmds = app.navigate_to(Mode::Search);
        assert!(reject(&mut app, cmds, DeliveryError::Closed).is_empty());
        assert_eq!(app.mode, Mode::Settings);
        assert!(app.settings.is_some());

        let cmds = app.quit_app();
        assert!(reject(&mut app, cmds, DeliveryError::Busy).is_empty());
        assert!(!app.should_quit);
        assert_eq!(app.mode, Mode::Settings);
        assert!(app.settings.is_some());

        let cmds = app.navigate_to(Mode::Search);
        let follow_ups = accept(&mut app, &cmds);
        assert_eq!(app.mode, Mode::Search);
        assert!(app.settings.is_none());
        assert_eq!(
            follow_ups
                .iter()
                .filter(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Config(_))))
                .count(),
            1
        );
    }
}
