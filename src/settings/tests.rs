use super::*;
use crate::search_source::SearchSource;

/// A neutral draft the value/apply tests can tweak one field at a time.
fn base_draft() -> SettingsDraft {
    SettingsDraft {
        beginner_mode: false,
        restart_beginner_tutorial: false,
        cookies_file: String::new(),
        download_dir: String::new(),
        local_include_download_dir: true,
        local_music_root: String::new(),
        local_music_root_recursive: true,
        search: SearchConfig::default(),
        mouse: true,
        album_art: false,
        player_bar_position: crate::config::PlayerBarPosition::Bottom,
        autoplay_on_start: false,
        enqueue_next: false,
        update_check_enabled: true,
        speed: 1.0,
        seek_seconds: 10.0,
        big_text: false,
        big_text_percent: 150,
        mouse_wheel_volume: true,
        gapless: true,
        media_controls: true,
        auto_continue_videos: false,
        video_layout: crate::config::VideoOverlay::Compact,
        audio_backend: crate::config::AudioBackend::Mpv,
        audio_mpv_output: String::new(),
        audio_mpv_device: String::new(),
        audio_mpv_cache_forward: MPV_CACHE_FORWARD_DEFAULT.to_owned(),
        audio_mpv_cache_back: MPV_CACHE_BACK_DEFAULT.to_owned(),
        autoplay_streaming: false,
        curating_mode: CuratingMode::DjGem,
        streaming_mode: StreamingMode::Balanced,
        eq_preset: EqPreset::Flat,
        eq_bands: EqPreset::Flat.gains(),
        normalize: false,
        gemini_model: GeminiModel::default(),
        gemini_api_key: String::new(),
        ai_enabled: true,
        romanized_titles: false,
        dj_gem_language: crate::i18n::DjGemLanguage::Auto,
        theme: ThemeConfig::default(),
        retro_mode: false,
        language: Language::English,
        animations: AnimationsConfig::default(),
        lastfm_enabled: true,
        lastfm_love_sync: true,
        lastfm_session_key: String::new(),
        lastfm_username: String::new(),
        listenbrainz_enabled: true,
        listenbrainz_token: String::new(),
        scrobble_local_files: true,
        spotify_client_id: String::new(),
        spotify_redirect_port: String::new(),
        spotify_import_mode: SpotifyImportMode::FastPlaylist,
        spotify_connected: false,
        spotify_stale: false,
        spotify_username: String::new(),
        recording_mode: crate::recorder::RecordingMode::Nothing,
        recording_min_seconds: 30,
        recording_max_seconds: 900,
        recording_dir: String::new(),
        recording_past_tracks: 10,
        recording_notify: true,
    }
}

/// A `SettingsState` on the given tab for partition tests.
fn state_on(tab: SettingsTab, radio_mode: bool, retro: bool) -> SettingsState {
    let mut draft = base_draft();
    draft.retro_mode = retro;
    SettingsState {
        tab,
        row: 0,
        draft,
        editing_text: false,
        secret_restore: None,
        keymap: KeyMap::default(),
        mousemap: MouseMap::default(),
        capturing: None,
        spotify_import_mode_dropdown: None,
        personal_data_export: PersonalDataExportStatus::default(),
        radio_mode,
    }
}

#[test]
fn playback_sections_partition_in_both_radio_states() {
    let _guard = crate::i18n::lock_for_test();
    for radio in [false, true] {
        let st = state_on(SettingsTab::Playback, radio, false);
        let sum: usize = st.sections().iter().map(|(_, n)| n).sum();
        assert_eq!(sum, st.fields().len(), "radio={radio}");
        assert_eq!(st.fields().contains(&Field::RadioRecording), radio);
    }
}

#[test]
fn ai_sections_partition_under_retro() {
    let _guard = crate::i18n::lock_for_test();
    for retro in [false, true] {
        let st = state_on(SettingsTab::Ai, false, retro);
        let sum: usize = st.sections().iter().map(|(_, n)| n).sum();
        assert_eq!(sum, st.fields().len(), "retro={retro}");
    }
}

#[test]
fn tabs_step_and_wrap() {
    assert_eq!(SettingsTab::General.stepped(true), SettingsTab::Playback);
    assert_eq!(SettingsTab::Playback.stepped(true), SettingsTab::Keys);
    assert_eq!(SettingsTab::Keys.stepped(true), SettingsTab::Graphics);
    assert_eq!(SettingsTab::Graphics.stepped(true), SettingsTab::Ai);
    assert_eq!(SettingsTab::Ai.stepped(true), SettingsTab::Accounts);
    assert_eq!(SettingsTab::Accounts.stepped(true), SettingsTab::General); // wraps
    assert_eq!(SettingsTab::General.stepped(false), SettingsTab::Accounts); // wraps back
}

#[test]
fn accounts_tab_sections_partition_its_fields() {
    let _guard = crate::i18n::lock_for_test();
    let f = SettingsTab::Accounts.fields();
    let sections = SettingsTab::Accounts.sections();
    // Section counts must partition the field list exactly (the view walks them in lockstep).
    assert_eq!(sections.iter().map(|(_, n)| n).sum::<usize>(), f.len());
    assert_eq!(f[0], Field::LastfmEnabled);
    assert_eq!(Field::LastfmConnect.kind(), FieldKind::Button);
    assert_eq!(Field::ListenBrainzToken.kind(), FieldKind::Text);
    assert!(Field::ListenBrainzToken.is_secret());
    // The connect row doubles as the status line: it must reflect the session state.
    let mut d = base_draft();
    assert!(d.value_display(Field::LastfmConnect).contains('↵'));
    d.lastfm_session_key = "sk".to_owned();
    d.lastfm_username = "listener".to_owned();
    assert!(d.value_display(Field::LastfmConnect).contains("listener"));
}

#[test]
fn graphics_tab_groups_theme_colors_and_animations() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let f = SettingsTab::Graphics.fields();
    // Three base fields, every color role, and 43 animation fields.
    assert_eq!(f.len(), 3 + ThemeRole::ALL.len() + 43);
    assert_eq!(f[0], Field::RetroMode);
    assert_eq!(f[1], Field::ThemePreset);
    assert_eq!(f[2], Field::BackgroundNone);
    assert!(matches!(f[3], Field::ThemeColor(_)));
    let anim_start = 3 + ThemeRole::ALL.len();
    assert_eq!(f[anim_start], Field::AnimMaster);
    assert_eq!(f[anim_start + 1], Field::AnimPauseUnfocused);
    assert_eq!(f[anim_start + 2], Field::AnimFps);
    assert_eq!(Field::AnimFps.kind(), FieldKind::Slider);
    assert!(
        f[anim_start..]
            .iter()
            .filter(|fld| **fld != Field::AnimFps)
            .all(|fld| fld.kind() == FieldKind::Toggle)
    );

    let sections = SettingsTab::Graphics.sections();
    let total: usize = sections.iter().map(|(_, n)| n).sum();
    assert_eq!(total, f.len());
    assert_eq!(
        sections.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
        vec![3, ThemeRole::ALL.len(), 3, 7, 7, 11, 9, 6]
    );
    assert_eq!(
        sections
            .iter()
            .map(|(title, _)| *title)
            .collect::<Vec<_>>()
            .join(","),
        "Theme,Colors,Animation controls,Event feedback,Interface motion,Now playing,Ambient canvas,Canvas showpieces"
    );
    let order = f[anim_start..]
        .iter()
        .map(|field| format!("{field:?}"))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        order,
        "AnimMaster,AnimPauseUnfocused,AnimFps,\
         AnimErrorShake,AnimLikeBurst,AnimTrackIntro,AnimSeekFlash,AnimPauseFlash,AnimVolumeFlash,AnimToast,\
         AnimAboutFx,AnimPopupFade,AnimTabs,AnimStagger,AnimActivity,AnimCaret,AnimSelection,\
         AnimTimeGlow,AnimHeart,AnimSpinner,AnimControls,AnimEqBars,AnimSeekbar,AnimProgressSparkle,AnimTitle,AnimLyrics,AnimBorderChase,AnimBorder,\
         AnimBounce,AnimComets,AnimSnow,AnimStarfield,AnimFireflies,AnimCube,AnimAquarium,AnimWaves,AnimVisualizer,\
         AnimFireworks,AnimRain,AnimLife,AnimPipes,AnimDonut,AnimPlasma"
    );

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
    assert!(
        Field::Mouse
            .anim_flag(&mut AnimationsConfig::default())
            .is_none()
    );
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
    draft
        .theme
        .set_override(ThemeRole::Background, crate::theme::TRANSPARENT)
        .unwrap();
    assert!(draft.theme.is_role_transparent(ThemeRole::Background));
    assert_eq!(draft.value_display(Field::BackgroundNone), "[x]");
    let mut cfg = Config::default();
    draft.apply_to(&mut cfg);
    assert!(
        cfg.theme
            .normalized()
            .is_role_transparent(ThemeRole::Background)
    );
}

#[test]
fn playback_tab_groups_now_playing_and_eq() {
    let f = SettingsTab::Playback.fields();
    // Speed + SeekInterval + WheelVolume + Gapless + MediaControls + AutoContinueVideos +
    // VideoLayout + RadioRecording (radio-only), then audio backend, then EQ.
    assert_eq!(f.len(), 8 + 5 + 1 + eq::BANDS + 1);
    assert_eq!(f[0], Field::Speed);
    assert_eq!(f[1], Field::SeekInterval);
    assert_eq!(f[2], Field::MouseWheelVolume);
    assert_eq!(f[3], Field::Gapless);
    assert_eq!(f[4], Field::MediaControls);
    assert_eq!(f[5], Field::AutoContinueVideos);
    assert_eq!(f[6], Field::VideoLayout);
    assert_eq!(f[7], Field::RadioRecording);
    assert_eq!(f[8], Field::AudioBackend);
    assert_eq!(f[9], Field::AudioMpvOutput);
    assert_eq!(f[12], Field::AudioMpvCacheBack);
    assert_eq!(f[13], Field::EqPreset);
    assert_eq!(f[13 + eq::BANDS + 1], Field::Normalize);
    assert_eq!(Field::MouseWheelVolume.kind(), FieldKind::Toggle);
    assert_eq!(base_draft().value_display(Field::MouseWheelVolume), "[x]");
    assert_eq!(base_draft().value_display(Field::AudioBackend), "mpv");
    let total: usize = SettingsTab::Playback
        .sections()
        .iter()
        .map(|(_, n)| n)
        .sum();
    assert_eq!(total, f.len());
}

#[test]
fn general_tab_has_search_options_and_autoplay_toggle() {
    let _guard = crate::i18n::lock_for_test();
    let f = SettingsTab::General.fields();
    assert_eq!(
        f,
        vec![
            Field::BeginnerMode,
            Field::Language,
            Field::SearchSource,
            Field::StreamingSource,
            Field::SearchYoutube,
            Field::SearchSoundCloud,
            Field::SearchAudius,
            Field::AudiusAppName,
            Field::SearchJamendo,
            Field::JamendoClientId,
            Field::SearchInternetArchive,
            Field::SearchRadioBrowser,
            Field::CookiesFile,
            Field::DownloadDir,
            Field::LocalIncludeDownloadDir,
            Field::LocalMusicRoot,
            Field::LocalMusicRootRecursive,
            Field::Mouse,
            Field::AlbumArt,
            Field::PlayerBarPosition,
            Field::BigText,
            Field::AutoplayOnStart,
            Field::EnqueueNext,
            Field::UpdateCheck,
            Field::ExportPersonalData,
            Field::ResetKeybindings,
            Field::ResetAll,
        ]
    );
    assert_eq!(Field::ResetKeybindings.kind(), FieldKind::Button);
    assert_eq!(Field::ResetAll.kind(), FieldKind::Button);
    assert_eq!(Field::BeginnerMode.kind(), FieldKind::Toggle);
    assert_eq!(Field::SearchSource.kind(), FieldKind::Select);
    assert_eq!(Field::StreamingSource.kind(), FieldKind::Select);
    assert_eq!(Field::SearchSoundCloud.kind(), FieldKind::Toggle);
    assert_eq!(Field::JamendoClientId.kind(), FieldKind::Text);
    assert_eq!(Field::LocalIncludeDownloadDir.kind(), FieldKind::Toggle);
    assert_eq!(Field::LocalMusicRoot.kind(), FieldKind::Text);
    assert_eq!(Field::LocalMusicRootRecursive.kind(), FieldKind::Toggle);
    assert_eq!(Field::AutoplayOnStart.kind(), FieldKind::Toggle);
    assert_eq!(Field::EnqueueNext.kind(), FieldKind::Toggle);
    assert_eq!(Field::AlbumArt.kind(), FieldKind::Toggle);
    // Off by default, and the toggle renders as an empty checkbox.
    let draft = base_draft();
    assert_eq!(draft.value_display(Field::BeginnerMode), "[ ]");
    assert_eq!(
        draft.value_display(Field::ResetKeybindings),
        "↵ press Enter"
    );
    assert_eq!(draft.value_display(Field::SearchSource), "YouTube");
    assert_eq!(draft.value_display(Field::StreamingSource), "YouTube");
    assert_eq!(draft.value_display(Field::SearchSoundCloud), "[x]");
    assert_eq!(draft.value_display(Field::JamendoClientId), "(none)");
    assert_eq!(draft.value_display(Field::LocalIncludeDownloadDir), "[x]");
    assert_eq!(draft.value_display(Field::LocalMusicRoot), "(none)");
    assert_eq!(draft.value_display(Field::LocalMusicRootRecursive), "[x]");
    assert!(!draft.autoplay_on_start);
    assert_eq!(draft.value_display(Field::AutoplayOnStart), "[ ]");
    assert!(!draft.enqueue_next);
    assert_eq!(draft.value_display(Field::EnqueueNext), "[ ]");
}

#[test]
fn beginner_mode_projection_restarts_only_when_requested() {
    let mut cfg = Config {
        beginner_mode: true,
        beginner_tutorial: BeginnerTutorialProgress {
            content_version: crate::config::BEGINNER_TUTORIAL_VERSION,
            next_step: "finish".to_owned(),
        },
        ..Config::default()
    };

    let mut draft = base_draft();
    draft.beginner_mode = true;
    draft.apply_to(&mut cfg);
    assert_eq!(cfg.beginner_tutorial.next_step, "finish");

    draft.restart_beginner_tutorial = true;
    draft.apply_to(&mut cfg);
    assert_eq!(cfg.beginner_tutorial, BeginnerTutorialProgress::welcome());
}

#[test]
fn ai_tab_has_model_key_autoplay_and_streaming_mode() {
    let _guard = crate::i18n::lock_for_test();
    let f = SettingsTab::Ai.fields();
    assert_eq!(
        f,
        vec![
            Field::AiEnabled,
            Field::GeminiModel,
            Field::ApiKey,
            Field::DjGemLanguage,
            Field::RomanizedTitles,
            Field::ClearRomanizedTitleCache,
            Field::AutoplayStreaming,
            Field::CuratingMode,
            Field::StreamingMode,
        ]
    );
    // Section header counts must partition the fields exactly, or the renderer drops the tail.
    let secs: usize = SettingsTab::Ai.sections().iter().map(|(_, n)| n).sum();
    assert_eq!(secs, SettingsTab::Ai.fields().len());
    assert_eq!(Field::AiEnabled.kind(), FieldKind::Toggle);
    assert!(!Field::AiEnabled.is_secret());
    // Enabled by default in a fresh draft.
    assert_eq!(base_draft().value_display(Field::AiEnabled), "[x]");
    assert_eq!(Field::GeminiModel.kind(), FieldKind::Select);
    assert_eq!(Field::ApiKey.kind(), FieldKind::Text);
    assert!(Field::ApiKey.is_secret());
    assert!(!Field::GeminiModel.is_secret());
    // Reply language is a non-secret cycle field defaulting to Auto.
    assert_eq!(Field::DjGemLanguage.kind(), FieldKind::Select);
    assert!(!Field::DjGemLanguage.is_secret());
    assert_eq!(
        base_draft().value_display(Field::DjGemLanguage),
        "Auto (interface)"
    );
    assert_eq!(Field::RomanizedTitles.kind(), FieldKind::Toggle);
    assert_eq!(base_draft().value_display(Field::RomanizedTitles), "[ ]");
    assert_eq!(Field::ClearRomanizedTitleCache.kind(), FieldKind::Button);
    assert_eq!(
        base_draft().value_display(Field::ClearRomanizedTitleCache),
        "↵ press Enter"
    );
    // Curating mode + style are non-secret cycle fields; both default to DJ Gem / Balanced.
    assert_eq!(Field::CuratingMode.kind(), FieldKind::Select);
    assert!(!Field::CuratingMode.is_secret());
    assert_eq!(base_draft().value_display(Field::CuratingMode), "DJ Gem");
    assert_eq!(Field::StreamingMode.kind(), FieldKind::Select);
    assert!(!Field::StreamingMode.is_secret());
    assert_eq!(base_draft().value_display(Field::StreamingMode), "Balanced");
}

#[test]
fn theme_and_colors_are_editable_and_persistent() {
    // Retro mode + theme preset + transparent-bg toggle lead the Graphics tab; colors follow.
    let f = SettingsTab::Graphics.fields();
    assert_eq!(f[0], Field::RetroMode);
    assert_eq!(f[1], Field::ThemePreset);
    assert_eq!(f[2], Field::BackgroundNone);
    let color_fields: Vec<Field> = f
        .into_iter()
        .filter(|fld| matches!(fld, Field::ThemeColor(_)))
        .collect();
    assert_eq!(color_fields.len(), ThemeRole::ALL.len());
    assert!(matches!(
        color_fields[0],
        Field::ThemeColor(ThemeRole::Background)
    ));

    let mut draft = base_draft();
    draft.theme.set_preset(crate::theme::ThemePreset::Midnight);
    draft
        .theme
        .set_override(ThemeRole::Accent, "#123456")
        .unwrap();
    let mut cfg = Config::default();
    draft.apply_to(&mut cfg);
    assert_eq!(cfg.theme.preset, "midnight");
    assert_eq!(
        cfg.theme.overrides.get("accent").map(String::as_str),
        Some("#123456")
    );
}

#[test]
fn retro_mode_commits_the_edited_theme_but_keeps_english() {
    // Retro used to overwrite the committed theme with a fresh Retro preset (wiping
    // the user's preset + color overrides on disk). Now the theme commits as edited;
    // only the UI language stays forced to English.
    let mut draft = base_draft();
    draft.retro_mode = true;
    draft.language = crate::i18n::Language::Korean;
    draft.theme.set_preset(crate::theme::ThemePreset::Nord);
    draft
        .theme
        .set_override(ThemeRole::Accent, "#ABCDEF")
        .unwrap();
    let mut cfg = Config::default();
    draft.apply_to(&mut cfg);
    assert!(cfg.retro_mode);
    assert_eq!(cfg.theme.preset, "nord");
    assert_eq!(
        cfg.theme.overrides.get("accent").map(String::as_str),
        Some("#ABCDEF")
    );
    assert_eq!(cfg.language, crate::i18n::Language::English);
    // And the runtime theme honors the user's choice under retro mode too.
    assert_eq!(cfg.effective_theme().preset, "nord");
}

#[test]
fn apply_to_persists_every_settings_field() {
    let mut bands = EqPreset::Flat.gains();
    bands[2] = 4.0;
    let mut theme = ThemeConfig::default();
    theme.set_preset(crate::theme::ThemePreset::HighContrast);
    theme
        .set_override(ThemeRole::BorderPrimary, "#123456")
        .unwrap();

    let draft = SettingsDraft {
        beginner_mode: true,
        restart_beginner_tutorial: true,
        cookies_file: "/tmp/cookies.txt".to_owned(),
        download_dir: "/tmp/downloads".to_owned(),
        local_include_download_dir: false,
        local_music_root: "/Users/listener/Music".to_owned(),
        local_music_root_recursive: false,
        search: SearchConfig {
            source: SearchSource::SoundCloud,
            streaming_source: SearchSource::All,
            jamendo_client_id: Some("jam-id".to_owned()),
            ..SearchConfig::default()
        },
        mouse: false,
        album_art: true,
        player_bar_position: crate::config::PlayerBarPosition::Top,
        autoplay_on_start: true,
        enqueue_next: true,
        update_check_enabled: false,
        speed: 1.7,
        seek_seconds: 25.0,
        big_text: false,
        big_text_percent: 150,
        mouse_wheel_volume: false,
        gapless: false,
        media_controls: false,
        auto_continue_videos: true,
        video_layout: crate::config::VideoOverlay::Fullscreen,
        audio_backend: crate::config::AudioBackend::Mpv,
        audio_mpv_output: "pipewire".to_owned(),
        audio_mpv_device: "alsa/default".to_owned(),
        audio_mpv_cache_forward: "64MiB".to_owned(),
        audio_mpv_cache_back: "16MiB".to_owned(),
        autoplay_streaming: true,
        curating_mode: CuratingMode::YtNative,
        streaming_mode: StreamingMode::Discovery,
        eq_preset: EqPreset::Custom,
        eq_bands: bands,
        normalize: true,
        gemini_model: GeminiModel::Latest,
        gemini_api_key: "  AIzaPersist  ".to_owned(),
        ai_enabled: false,
        romanized_titles: true,
        dj_gem_language: crate::i18n::DjGemLanguage::Japanese,
        theme,
        retro_mode: false,
        language: Language::Korean,
        animations: AnimationsConfig {
            master: true,
            border: true,
            fps: 45,
            pause_unfocused: false,
            ..Default::default()
        },
        lastfm_enabled: false,
        lastfm_love_sync: false,
        lastfm_session_key: "sk-abc".to_owned(),
        lastfm_username: "listener".to_owned(),
        listenbrainz_enabled: true,
        listenbrainz_token: "lb-tok".to_owned(),
        scrobble_local_files: false,
        spotify_client_id: "  spotify-cid  ".to_owned(),
        spotify_redirect_port: "9333".to_owned(),
        spotify_import_mode: crate::config::SpotifyImportMode::StrictPlaylist,
        spotify_connected: true,
        spotify_stale: false,
        spotify_username: "listener".to_owned(),
        recording_mode: crate::recorder::RecordingMode::Decide,
        recording_min_seconds: 20,
        recording_max_seconds: 1200,
        recording_dir: "/tmp/recs".to_owned(),
        recording_past_tracks: 25,
        recording_notify: false,
    };

    let mut cfg = Config::default();
    cfg.beginner_tutorial.next_step = "finish".to_owned();
    draft.apply_to(&mut cfg);
    assert!(cfg.beginner_mode);
    assert_eq!(cfg.beginner_tutorial, BeginnerTutorialProgress::welcome());
    assert_eq!(cfg.recording.mode, crate::recorder::RecordingMode::Decide);
    assert_eq!(cfg.recording.min_duration_secs, 20);
    assert_eq!(cfg.recording.max_duration_secs, 1200);
    assert_eq!(
        cfg.recording.track_directory,
        Some(PathBuf::from("/tmp/recs"))
    );
    assert_eq!(cfg.recording.past_tracks_count, 25);
    assert!(!cfg.recording.notify);
    assert_eq!(cfg.language, Language::Korean);
    // The raw pick round-trips (not retro-forced here); `Auto` would too.
    assert_eq!(cfg.dj_gem_language, crate::i18n::DjGemLanguage::Japanese);
    assert_eq!(cfg.ai_enabled, Some(false));
    assert_eq!(cfg.romanized_titles, Some(true));
    assert!(cfg.animations.master);
    assert!(cfg.animations.border);
    assert!(!cfg.animations.rain);
    assert_eq!(cfg.animations.fps, 45);
    assert!(!cfg.animations.pause_unfocused);
    assert_eq!(cfg.cookies_file, Some(PathBuf::from("/tmp/cookies.txt")));
    assert_eq!(cfg.download_dir, Some(PathBuf::from("/tmp/downloads")));
    assert!(!cfg.local.include_download_dir());
    assert_eq!(cfg.local.roots.len(), 1);
    assert_eq!(
        cfg.local.roots[0].path,
        PathBuf::from("/Users/listener/Music")
    );
    assert!(cfg.local.roots[0].enabled());
    assert!(!cfg.local.roots[0].recursive());
    assert_eq!(cfg.search.source, SearchSource::SoundCloud);
    assert_eq!(cfg.search.streaming_source, SearchSource::All);
    assert_eq!(cfg.search.jamendo_client_id.as_deref(), Some("jam-id"));
    assert_eq!(cfg.mouse, Some(false));
    assert_eq!(cfg.album_art, Some(true));
    assert_eq!(
        cfg.player_bar_position,
        Some(crate::config::PlayerBarPosition::Top)
    );
    assert_eq!(cfg.autoplay_on_start, Some(true));
    assert_eq!(cfg.enqueue_next, Some(true));
    assert!(!cfg.update_check_enabled);
    assert_eq!(cfg.speed, Some(1.7));
    assert_eq!(cfg.seek_seconds, Some(25.0));
    assert_eq!(cfg.mouse_wheel_volume, Some(false));
    assert_eq!(cfg.gapless, Some(false));
    assert_eq!(cfg.media_controls, Some(false));
    assert_eq!(cfg.auto_continue_videos, Some(true));
    assert_eq!(cfg.video_layout, crate::config::VideoOverlay::Fullscreen);
    assert_eq!(cfg.audio.backend, crate::config::AudioBackend::Mpv);
    assert_eq!(cfg.audio.mpv.output.as_deref(), Some("pipewire"));
    assert_eq!(cfg.audio.mpv.device.as_deref(), Some("alsa/default"));
    assert_eq!(cfg.audio.mpv.cache_forward, "64MiB");
    assert_eq!(cfg.audio.mpv.cache_back, "16MiB");
    assert_eq!(cfg.autoplay_streaming, Some(true));
    assert_eq!(cfg.streaming.mode, StreamingMode::Discovery);
    // Curating mode = YT Native → the AI rerank flag persists as false.
    assert!(!cfg.streaming.ai.enabled);
    assert_eq!(cfg.scrobble.lastfm.enabled, Some(false));
    assert_eq!(cfg.scrobble.lastfm.love_sync, Some(false));
    assert_eq!(cfg.scrobble.lastfm.session_key.as_deref(), Some("sk-abc"));
    assert_eq!(cfg.scrobble.lastfm.username.as_deref(), Some("listener"));
    assert_eq!(cfg.scrobble.listenbrainz.enabled, Some(true));
    assert_eq!(cfg.scrobble.listenbrainz.token.as_deref(), Some("lb-tok"));
    assert_eq!(cfg.scrobble.local_files, Some(false));
    assert_eq!(cfg.spotify.client_id.as_deref(), Some("spotify-cid"));
    assert_eq!(cfg.spotify.redirect_port, Some(9333));
    assert_eq!(
        cfg.spotify.import_mode,
        crate::config::SpotifyImportMode::StrictPlaylist
    );
    assert_eq!(cfg.eq_preset, EqPreset::Custom);
    assert_eq!(cfg.eq_bands, Some(bands));
    assert_eq!(cfg.normalize, Some(true));
    assert_eq!(cfg.gemini_model, GeminiModel::Latest);
    assert_eq!(cfg.gemini_api_key.as_deref(), Some("AIzaPersist"));
    assert_eq!(cfg.theme.preset, "high_contrast");
    assert_eq!(
        cfg.theme
            .overrides
            .get("border_primary")
            .map(String::as_str),
        Some("#123456")
    );
}

#[test]
fn big_text_toggle_maps_to_the_zoom_level() {
    // ON with no prior zoom: enables the mode-preferred level.
    let mut draft = base_draft();
    draft.big_text = true;
    draft.big_text_percent = 200;
    let mut cfg = Config::default();
    draft.apply_to(&mut cfg);
    assert_eq!(cfg.text_zoom, Some(200));

    // ON with a custom wheel-set level already in the config: keeps it.
    let mut cfg = Config {
        text_zoom: Some(250),
        ..Config::default()
    };
    draft.apply_to(&mut cfg);
    assert_eq!(cfg.text_zoom, Some(250));

    // OFF always returns to normal size.
    draft.big_text = false;
    draft.apply_to(&mut cfg);
    assert_eq!(cfg.text_zoom, Some(100));
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
        autoplay_streaming: true,
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
    assert_eq!(clamp_speed(9.0), crate::config::SPEED_MAX);
    assert_eq!(clamp_speed(0.0), crate::config::SPEED_MIN);
}

#[test]
fn freq_labels_read_naturally() {
    assert_eq!(freq_label(0), "31 Hz");
    assert_eq!(freq_label(5), "1 kHz");
    assert_eq!(freq_label(9), "16 kHz");
}
