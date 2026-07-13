use super::*;
use crate::test_util::env::{with_var, with_vars};

#[test]
fn defaults_have_full_volume() {
    let c = Config::default();
    assert_eq!(c.volume, 100);
    assert!(c.cookie.is_none());
}

#[test]
fn load_from_preserves_unloadable_config_before_defaulting() {
    let dir = std::env::temp_dir().join(format!("ytm-cfg-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    const MARKER: &str = "SAPISID=super-secret-do-not-lose";

    // (1) Oversize file → moved to *.too-large.bak (secret preserved), fresh config written.
    let big = format!(
        "{{\"cookie\":\"{MARKER}\",\"pad\":\"{}\"}}",
        "x".repeat(1024 * 1024)
    );
    std::fs::write(&path, &big).unwrap();
    let _ = Config::load_from(&path);
    let too_large = path.with_extension("too-large.bak");
    assert!(
        too_large.exists(),
        "oversize config must be set aside, not clobbered"
    );
    assert!(
        std::fs::read_to_string(&too_large)
            .unwrap()
            .contains(MARKER),
        "the original secret is still recoverable in the backup",
    );
    assert!(path.exists(), "a fresh config is written in place");
    let _ = std::fs::remove_file(&too_large);

    // (2) Invalid UTF-8 → moved to *.corrupt.bak, original bytes preserved.
    std::fs::write(&path, [0xff, 0xfe, 0xfd, 0xfc]).unwrap();
    let _ = Config::load_from(&path);
    let corrupt = path.with_extension("corrupt.bak");
    assert!(corrupt.exists(), "invalid-UTF-8 config must be preserved");
    assert_eq!(
        std::fs::read(&corrupt).unwrap(),
        vec![0xff, 0xfe, 0xfd, 0xfc]
    );
    let _ = std::fs::remove_file(&corrupt);

    // (3) First run (missing) → defaults written, no backup created.
    std::fs::remove_file(&path).unwrap();
    let _ = Config::load_from(&path);
    assert!(path.exists());
    assert!(!path.with_extension("too-large.bak").exists());
    assert!(!path.with_extension("corrupt.bak").exists());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recording_defaults_and_clamps() {
    let c = Config::default();
    assert_eq!(c.recording.mode, crate::recorder::RecordingMode::Nothing);
    assert_eq!(c.effective_recording_min(), 30);
    assert_eq!(c.effective_recording_max(), 900);
    assert_eq!(c.effective_recording_past_tracks(), 10);

    let mut c = Config::default();
    c.recording.min_duration_secs = 1; // below floor
    c.recording.max_duration_secs = 999_999; // above ceil
    c.recording.past_tracks_count = 9_999; // above ceil
    assert_eq!(c.effective_recording_min(), RECORDING_MIN_SECONDS_MIN);
    assert_eq!(c.effective_recording_max(), RECORDING_MAX_SECONDS_MAX);
    assert_eq!(
        c.effective_recording_past_tracks(),
        RECORDING_PAST_TRACKS_MAX
    );

    // Max stays strictly above min even if hand-edited below it.
    let mut c = Config::default();
    c.recording.min_duration_secs = 600;
    c.recording.max_duration_secs = 60;
    assert!(c.effective_recording_max() > c.effective_recording_min());
}

#[test]
fn recording_config_round_trips_and_forward_migrates() {
    // An old config with no `recording` key loads with the defaults (opt-in Off).
    let old: Config = serde_json::from_str(r#"{"volume": 50}"#).unwrap();
    assert_eq!(old.recording.mode, crate::recorder::RecordingMode::Nothing);

    // A round-trip preserves an explicit recording block.
    let mut c = Config::default();
    c.recording.mode = crate::recorder::RecordingMode::Everything;
    c.recording.min_duration_secs = 45;
    let json = serde_json::to_string(&c).unwrap();
    let back: Config = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.recording.mode,
        crate::recorder::RecordingMode::Everything
    );
    assert_eq!(back.recording.min_duration_secs, 45);

    // A partial recording block fills the rest from defaults (nested serde default).
    let partial: Config =
        serde_json::from_str(r#"{"recording":{"min_duration_secs":45}}"#).unwrap();
    assert_eq!(partial.recording.min_duration_secs, 45);
    assert_eq!(partial.recording.past_tracks_count, 10);
    assert_eq!(
        partial.recording.mode,
        crate::recorder::RecordingMode::Nothing
    );
}

#[test]
fn drifted_enums_recover_instead_of_wiping_the_whole_config() {
    // A config written by a previous build whose `video_layout` value this build no
    // longer understands. Under the old strict load this reset the ENTIRE file (and
    // overwrote it) — the "settings reset after every install" bug. Now only the drifted
    // field falls back to default and every sibling survives.
    let mut original = Config {
        volume: 42,
        cookie: Some("SID=keepme".to_owned()),
        gemini_api_key: Some("AIzaKeep".to_owned()),
        ..Config::default()
    };
    original
        .theme
        .set_preset(crate::theme::ThemePreset::Midnight);

    let mut value = serde_json::to_value(&original).unwrap();
    value["video_layout"] = serde_json::Value::String("hologram".into());
    value["album_art_quality"] = serde_json::Value::String("future_ultra".into());

    // Strict parse fails outright (the behaviour that caused the reset)...
    assert!(
        serde_json::from_str::<Config>(&value.to_string()).is_err(),
        "a drifted enum must break the strict parse, or this test proves nothing",
    );

    // ...but recovery keeps everything except the one drifted field.
    let recovered = safe_fs::recover_lenient::<Config>(value);
    assert_eq!(recovered.volume, 42, "sibling scalar preserved");
    assert_eq!(
        recovered.cookie.as_deref(),
        Some("SID=keepme"),
        "login preserved",
    );
    assert_eq!(recovered.gemini_api_key.as_deref(), Some("AIzaKeep"));
    assert_eq!(recovered.theme.preset, "midnight", "theme preserved");
    assert_eq!(
        recovered.video_layout,
        VideoOverlay::default(),
        "only the drifted field falls back to default",
    );
    assert_eq!(
        recovered.album_art_quality,
        AlbumArtQuality::High,
        "the new drifted field also falls back without losing siblings",
    );
}

#[test]
fn json_round_trips() {
    let mut theme = ThemeConfig::default();
    theme.set_preset(crate::theme::ThemePreset::Midnight);
    theme
        .set_override(crate::theme::ThemeRole::BorderPrimary, "#123456")
        .unwrap();
    let mut radio_theme = ThemeConfig::default();
    radio_theme.set_preset(crate::theme::ThemePreset::RosePine);
    let c = Config {
        search_onboarding_seen: true,
        cookie: Some("SID=abc".to_owned()),
        cookies_file: Some(PathBuf::from("/tmp/cookies.txt")),
        volume: 70,
        download_dir: Some(PathBuf::from("/tmp/dl")),
        local: LocalConfig {
            include_download_dir: Some(false),
            roots: vec![LocalRootConfig {
                path: PathBuf::from("/music/library"),
                enabled: Some(true),
                recursive: Some(false),
            }],
            import_path_template: Some("{artist}/{title}".to_owned()),
        },
        download_concurrency: Some(2),
        mouse: Some(false),
        album_art: Some(true),
        album_art_quality: AlbumArtQuality::Original,
        player_bar_position: Some(PlayerBarPosition::Bottom),
        control_box_collapsed: Some(true),
        eq_preset: EqPreset::BassBoost,
        eq_bands: Some([1.0; eq::BANDS]),
        normalize: Some(true),
        speed: Some(1.5),
        seek_seconds: Some(15.0),
        mouse_wheel_volume: Some(false),
        text_zoom: Some(150),
        zoom_wheel_lock: Some(true),
        gapless: Some(false),
        shuffle: Some(true),
        repeat: Repeat::One,
        enqueue_next: Some(true),
        autoplay_streaming: Some(true),
        autoplay_on_start: Some(true),
        auto_continue_videos: Some(true),
        search: SearchConfig::default(),
        streaming: StreamingConfig::default(),
        animations: AnimationsConfig {
            master: true,
            radio_master: Some(false),
            rain: true,
            ..Default::default()
        },
        gemini_api_key: Some("AIzaSecret".to_owned()),
        gemini_model: GeminiModel::Latest,
        ai_enabled: Some(false),
        romanized_titles: Some(true),
        dj_gem_language: DjGemLanguage::ChineseTraditional,
        theme,
        radio_theme: Some(radio_theme),
        retro_mode: true,
        language: Language::Korean,
        keybindings: std::collections::BTreeMap::new(),
        mouse_bindings: std::collections::BTreeMap::new(),
        video_layout: VideoOverlay::Large,
        media_controls: Some(false),
        scrobble: ScrobbleConfig {
            lastfm: LastfmConfig {
                enabled: Some(true),
                session_key: Some("sk-123".to_owned()),
                username: Some("listener".to_owned()),
                love_sync: Some(false),
                api_key: None,
                api_secret: None,
            },
            listenbrainz: ListenBrainzConfig {
                enabled: None,
                token: Some("lb-token".to_owned()),
                api_url: None,
            },
            local_files: Some(false),
        },
        spotify: SpotifyConfig {
            client_id: Some("spotify-app-id".to_owned()),
            redirect_port: Some(9333),
            market: Some("KR".to_owned()),
            import_mode: SpotifyImportMode::StrictPlaylist,
        },
        tools: ToolsConfig {
            ytdlp_managed: Some(false),
            ytdlp_channel: Some(crate::tools::YtdlpChannel::Stable),
            ytdlp_path: Some(PathBuf::from("/opt/yt-dlp")),
            mpv_path: Some(PathBuf::from("/opt/mpv")),
        },
        audio: AudioConfig {
            backend: AudioBackend::Mpv,
            mpv: MpvAudioConfig {
                cache_defaults_revision: MPV_CACHE_DEFAULTS_REVISION,
                output: Some("pipewire".to_owned()),
                device: Some("alsa/default".to_owned()),
                cache_forward: "64MiB".to_owned(),
                cache_back: "16MiB".to_owned(),
                extra_args: vec!["--audio-exclusive=no".to_owned()],
            },
        },
        recording: RecordingConfig {
            mode: crate::recorder::RecordingMode::Decide,
            min_duration_secs: 20,
            max_duration_secs: 1200,
            track_directory: Some(PathBuf::from("/tmp/recs")),
            past_tracks_count: 25,
            notify: false,
        },
        update_check_enabled: false,
    };
    let s = serde_json::to_string(&c).unwrap();
    let back: Config = serde_json::from_str(&s).unwrap();
    assert!(!back.update_check_enabled);
    assert_eq!(back.recording.mode, crate::recorder::RecordingMode::Decide);
    assert_eq!(back.recording.min_duration_secs, 20);
    assert_eq!(
        back.recording.track_directory,
        Some(PathBuf::from("/tmp/recs"))
    );
    assert!(!back.recording.notify);
    assert_eq!(back.volume, 70);
    assert_eq!(back.cookie.as_deref(), Some("SID=abc"));
    assert_eq!(back.download_dir, Some(PathBuf::from("/tmp/dl")));
    assert!(!back.local.include_download_dir());
    assert_eq!(back.local.roots.len(), 1);
    assert_eq!(back.local.roots[0].path, PathBuf::from("/music/library"));
    assert!(back.local.roots[0].enabled());
    assert!(!back.local.roots[0].recursive());
    assert_eq!(back.local.import_path_template(), "{artist}/{title}");
    assert_eq!(back.mouse, Some(false));
    assert_eq!(back.album_art, Some(true));
    assert_eq!(back.album_art_quality, AlbumArtQuality::Original);
    assert_eq!(back.player_bar_position, Some(PlayerBarPosition::Bottom));
    assert_eq!(back.control_box_collapsed, Some(true));
    assert_eq!(back.eq_preset, EqPreset::BassBoost);
    assert_eq!(back.eq_bands, Some([1.0; eq::BANDS]));
    assert_eq!(back.normalize, Some(true));
    assert_eq!(back.speed, Some(1.5));
    assert_eq!(back.seek_seconds, Some(15.0));
    assert_eq!(back.mouse_wheel_volume, Some(false));
    assert_eq!(back.gapless, Some(false));
    assert_eq!(back.shuffle, Some(true));
    assert_eq!(back.repeat, Repeat::One);
    assert_eq!(back.enqueue_next, Some(true));
    assert_eq!(back.autoplay_streaming, Some(true));
    assert_eq!(back.autoplay_on_start, Some(true));
    assert_eq!(back.ai_enabled, Some(false));
    assert_eq!(back.romanized_titles, Some(true));
    assert_eq!(back.dj_gem_language, DjGemLanguage::ChineseTraditional);
    assert!(back.animations.master);
    assert_eq!(back.animations.radio_master, Some(false));
    assert!(back.animations.rain);
    assert!(!back.animations.donut);
    assert_eq!(back.gemini_api_key.as_deref(), Some("AIzaSecret"));
    assert_eq!(back.gemini_model, GeminiModel::Latest);
    assert!(back.retro_mode);
    assert_eq!(back.video_layout, VideoOverlay::Large);
    assert_eq!(back.media_controls, Some(false));
    assert_eq!(back.scrobble.lastfm.session_key.as_deref(), Some("sk-123"));
    assert_eq!(back.scrobble.lastfm.username.as_deref(), Some("listener"));
    assert_eq!(back.scrobble.lastfm.love_sync, Some(false));
    assert_eq!(
        back.scrobble.listenbrainz.token.as_deref(),
        Some("lb-token")
    );
    assert_eq!(back.scrobble.local_files, Some(false));
    assert!(back.scrobble.lastfm.is_active());
    assert_eq!(back.spotify.client_id.as_deref(), Some("spotify-app-id"));
    assert_eq!(back.effective_spotify_port(), 9333);
    assert_eq!(back.spotify.import_mode, SpotifyImportMode::StrictPlaylist);
    assert_eq!(back.tools.ytdlp_managed, Some(false));
    assert_eq!(
        back.tools.ytdlp_channel,
        Some(crate::tools::YtdlpChannel::Stable)
    );
    assert_eq!(back.tools.ytdlp_path, Some(PathBuf::from("/opt/yt-dlp")));
    assert_eq!(back.tools.mpv_path, Some(PathBuf::from("/opt/mpv")));
    assert_eq!(back.audio.backend, AudioBackend::Mpv);
    assert_eq!(back.audio.mpv.output.as_deref(), Some("pipewire"));
    assert_eq!(back.audio.mpv.device.as_deref(), Some("alsa/default"));
    assert_eq!(back.audio.mpv.cache_forward, "64MiB");
    assert_eq!(back.audio.mpv.cache_back, "16MiB");
    assert_eq!(back.audio.mpv.extra_args, ["--audio-exclusive=no"]);
    assert_eq!(back.theme.preset, "midnight");
    assert_eq!(
        back.theme
            .overrides
            .get("border_primary")
            .map(String::as_str),
        Some("#123456")
    );
    assert_eq!(
        back.radio_theme.as_ref().map(|t| t.preset.as_str()),
        Some("rose_pine")
    );
    assert_eq!(
        back.effective_radio_theme().map(|t| t.preset),
        Some("rose_pine".to_owned())
    );
    assert!(Config::default().radio_theme.is_none());
    assert!(Config::default().effective_radio_theme().is_none());
}

#[test]
fn search_onboarding_is_fresh_only_for_new_profiles() {
    assert!(!Config::default().search_onboarding_seen);
    let legacy: Config = serde_json::from_str("{\"volume\": 42}").unwrap();
    assert!(legacy.search_onboarding_seen);
}

#[test]
fn missing_local_config_includes_downloads_by_default() {
    let cfg: Config = serde_json::from_str(r#"{"download_dir":"/tmp/dl"}"#).unwrap();

    assert!(cfg.local.include_download_dir());
    assert!(cfg.local.roots.is_empty());
    assert_eq!(
        cfg.local.import_path_template(),
        LOCAL_IMPORT_PATH_TEMPLATE_DEFAULT
    );
    let blank: Config = serde_json::from_str(r#"{"local":{"import_path_template":"  "}}"#).unwrap();
    assert_eq!(
        blank.local.import_path_template(),
        LOCAL_IMPORT_PATH_TEMPLATE_DEFAULT
    );
}

#[test]
fn animations_effective_resolves_radio_master() {
    let inherit = AnimationsConfig {
        master: true,
        ..Default::default()
    };
    // `None` inherits the music master in radio mode (legacy behavior, one switch).
    assert!(inherit.effective(true).master);
    assert!(inherit.effective(false).master);

    let split = AnimationsConfig {
        master: true,
        radio_master: Some(false),
        ..Default::default()
    };
    assert!(
        !split.effective(true).master,
        "radio resolves to its own switch once pinned"
    );
    assert!(
        split.effective(false).master,
        "music mode ignores the radio override"
    );

    // Configs written before the split (no `radio_master` key) keep the inherit link.
    let legacy: AnimationsConfig = serde_json::from_str(r#"{"master":true}"#).unwrap();
    assert_eq!(legacy.radio_master, None);
    assert!(legacy.effective(true).master);
}

#[test]
fn keybindings_persist_through_config_json() {
    use crate::keymap::{Action, KeyContext, KeyMap, parse_chord};

    // Rebind a key, then capture it the way `close_settings` does on save.
    let mut km = KeyMap::default();
    km.rebind(
        KeyContext::Player,
        Action::TogglePause,
        parse_chord("x").unwrap(),
    )
    .unwrap();
    let cfg = Config {
        keybindings: km.to_overrides(),
        ..Config::default()
    };
    // Only the diff from defaults is persisted.
    assert_eq!(
        cfg.keybindings
            .get("player.toggle_pause")
            .map(String::as_str),
        Some("x")
    );

    // Round-trip through the exact serde path `Config::save`/`load` use (write JSON,
    // read it back) — proving the override survives a restart.
    let json = serde_json::to_string_pretty(&cfg).unwrap();
    let back: Config = serde_json::from_str(&json).unwrap();

    // On next launch the persisted override rebuilds into the live keymap.
    let restored = KeyMap::from_config(&back);
    assert_eq!(
        restored.action(KeyContext::Player, parse_chord("x").unwrap()),
        Some(Action::TogglePause)
    );
    assert_eq!(
        restored.action(KeyContext::Player, parse_chord("space").unwrap()),
        None
    );
}

#[test]
fn gemini_key_env_overrides_config() {
    let cfg = Config {
        gemini_api_key: Some("from_config".to_owned()),
        ..Config::default()
    };
    with_var("GEMINI_API_KEY", Some("  from_env  "), || {
        assert_eq!(cfg.effective_gemini_api_key().as_deref(), Some("from_env"));
    });
    // Empty/whitespace key reads as unset.
    let blank = Config {
        gemini_api_key: Some("   ".to_owned()),
        ..Config::default()
    };
    with_var("GEMINI_API_KEY", None, || {
        assert_eq!(
            cfg.effective_gemini_api_key().as_deref(),
            Some("from_config")
        );
        assert_eq!(blank.effective_gemini_api_key(), None);
    });
}

#[test]
fn ai_off_switch_gates_the_key_without_discarding_it() {
    // DJ Gem explicitly off: the key stays in config, but the *effective* key the assistant
    // spawns from is None — so DJ Gem stays down even with a key saved. (None regardless of any
    // `GEMINI_API_KEY` env var, since the disabled branch never consults the env.)
    let off = Config {
        gemini_api_key: Some("AIzaSaved".to_owned()),
        ai_enabled: Some(false),
        ..Config::default()
    };
    assert_eq!(off.gemini_api_key.as_deref(), Some("AIzaSaved")); // key retained
    assert!(!off.effective_ai_enabled());
    assert_eq!(off.effective_ai_key(), None); // but gated off

    // Enabled (or the default unset → on) passes the effective key straight through. Asserts
    // the *relationship* rather than a literal, so a concurrently-set env var can't flake it.
    let on = Config {
        gemini_api_key: Some("AIzaSaved".to_owned()),
        ai_enabled: Some(true),
        ..Config::default()
    };
    assert!(on.effective_ai_enabled());
    assert_eq!(on.effective_ai_key(), on.effective_gemini_api_key());

    let default_on = Config {
        ai_enabled: None,
        ..Config::default()
    };
    assert!(default_on.effective_ai_enabled()); // unset defaults to on
    assert_eq!(
        default_on.effective_ai_key(),
        default_on.effective_gemini_api_key()
    );
}

#[test]
fn playback_effective_defaults_and_overrides() {
    let d = Config::default();
    assert_eq!(d.effective_eq_bands(), [0.0; eq::BANDS]);
    assert!(!d.effective_normalize());
    assert_eq!(d.effective_speed(), 1.0);
    assert_eq!(d.effective_seek_seconds(), SEEK_SECONDS_DEFAULT);
    assert!(d.effective_mouse_wheel_volume());
    assert!(d.effective_gapless());
    assert!(!d.effective_shuffle());
    assert_eq!(d.effective_repeat(), Repeat::Off);
    assert!(!d.effective_enqueue_next());
    assert!(!d.effective_autoplay_streaming());
    assert!(!d.effective_autoplay_on_start());
    assert!(!d.effective_auto_continue_videos());

    // Preset gains feed through when no hand-tuned bands are set.
    let preset = Config {
        eq_preset: EqPreset::BassBoost,
        ..Config::default()
    };
    assert_eq!(preset.effective_eq_bands(), EqPreset::BassBoost.gains());

    // Speed is clamped to the supported range.
    let fast = Config {
        speed: Some(9.0),
        ..Config::default()
    };
    assert_eq!(fast.effective_speed(), SPEED_MAX);

    // Seek step is clamped to its supported range too.
    let big = Config {
        seek_seconds: Some(999.0),
        ..Config::default()
    };
    assert_eq!(big.effective_seek_seconds(), SEEK_SECONDS_MAX);
    let tiny = Config {
        seek_seconds: Some(0.0),
        ..Config::default()
    };
    assert_eq!(tiny.effective_seek_seconds(), SEEK_SECONDS_MIN);

    // A non-finite / corrupt persisted value never escapes the effective_* accessors:
    // it normalizes to a finite default instead of poisoning playback speed, the seek
    // step, or the mpv EQ filter.
    let corrupt = Config {
        speed: Some(f64::NAN),
        seek_seconds: Some(f64::INFINITY),
        eq_bands: Some([f64::NAN; eq::BANDS]),
        ..Config::default()
    };
    assert_eq!(corrupt.effective_speed(), 1.0, "NaN speed -> default 1.0");
    assert_eq!(
        corrupt.effective_seek_seconds(),
        SEEK_SECONDS_DEFAULT,
        "inf seek -> default"
    );
    assert!(corrupt.effective_eq_bands().iter().all(|g| g.is_finite()));
    assert_eq!(corrupt.effective_eq_bands(), [0.0; eq::BANDS]);

    let wheel_off = Config {
        mouse_wheel_volume: Some(false),
        ..Config::default()
    };
    assert!(!wheel_off.effective_mouse_wheel_volume());

    let enqueue_next = Config {
        enqueue_next: Some(true),
        ..Config::default()
    };
    assert!(enqueue_next.effective_enqueue_next());
}

#[test]
fn mouse_enabled_by_default_and_overridable() {
    assert!(Config::default().effective_mouse());
    let off = Config {
        mouse: Some(false),
        ..Config::default()
    };
    assert!(!off.effective_mouse());
}

#[test]
fn album_art_off_by_default_and_overridable() {
    assert!(!Config::default().effective_album_art()); // opt-in
    let on = Config {
        album_art: Some(true),
        ..Config::default()
    };
    assert!(on.effective_album_art());
}

#[test]
fn album_art_quality_defaults_to_legacy_high_and_cycles() {
    let legacy: Config = serde_json::from_str("{}").unwrap();
    assert_eq!(legacy.album_art_quality, AlbumArtQuality::High);
    assert_eq!(
        AlbumArtQuality::Standard.cycled(true),
        AlbumArtQuality::High
    );
    assert_eq!(
        AlbumArtQuality::High.cycled(true),
        AlbumArtQuality::Original
    );
    assert_eq!(
        AlbumArtQuality::Original.cycled(true),
        AlbumArtQuality::Standard
    );
    assert_eq!(
        AlbumArtQuality::Standard.cycled(false),
        AlbumArtQuality::Original
    );
    assert_eq!(
        serde_json::to_string(&AlbumArtQuality::Original).unwrap(),
        "\"original\""
    );
}

#[test]
fn player_bar_defaults_to_bottom_and_forward_migrates() {
    // Config files without the key (legacy or fresh) get the new docked layout;
    // Top stays selectable and round-trips.
    let back: Config = serde_json::from_str("{}").unwrap();
    assert_eq!(
        back.effective_player_bar_position(),
        PlayerBarPosition::Bottom
    );
    let top = Config {
        player_bar_position: Some(PlayerBarPosition::Top),
        ..Config::default()
    };
    assert_eq!(top.effective_player_bar_position(), PlayerBarPosition::Top);
    assert_eq!(PlayerBarPosition::Top.toggled(), PlayerBarPosition::Bottom);
    assert_eq!(PlayerBarPosition::Bottom.toggled(), PlayerBarPosition::Top);
}

#[test]
fn missing_fields_use_defaults() {
    let back: Config = serde_json::from_str("{}").unwrap();
    assert_eq!(back.volume, 100);
    assert_eq!(back.spotify.import_mode, SpotifyImportMode::FastPlaylist);

    let legacy: Config = serde_json::from_str(r#"{"spotify":{"client_id":"app"}}"#).unwrap();
    assert_eq!(legacy.spotify.import_mode, SpotifyImportMode::FastPlaylist);
}

#[test]
fn tools_config_defaults_and_overrides() {
    // A config written before the tools section existed forward-migrates to the
    // managed-on / nightly defaults.
    let back: Config = serde_json::from_str("{}").unwrap();
    assert!(back.tools.managed_enabled());
    assert_eq!(back.tools.channel(), crate::tools::YtdlpChannel::Nightly);
    assert_eq!(back.tools.mpv_program(), "mpv");

    let off = ToolsConfig {
        ytdlp_managed: Some(false),
        ..Default::default()
    };
    assert!(!off.managed_enabled());

    let pinned = ToolsConfig {
        ytdlp_path: Some(PathBuf::from("/pin/yt-dlp")),
        mpv_path: Some(PathBuf::from("/pin/mpv")),
        ..Default::default()
    };
    with_vars(&[("YTM_YTDLP", None), ("YTM_MPV", None)], || {
        assert_eq!(pinned.ytdlp_override(), Some(PathBuf::from("/pin/yt-dlp")));
        assert_eq!(pinned.mpv_program(), "/pin/mpv");
    });

    // Env vars beat the config paths.
    with_vars(
        &[
            ("YTM_YTDLP", Some("/env/yt-dlp")),
            ("YTM_MPV", Some("/env/mpv")),
        ],
        || {
            assert_eq!(pinned.ytdlp_override(), Some(PathBuf::from("/env/yt-dlp")));
            assert_eq!(pinned.mpv_program(), "/env/mpv");
        },
    );
}

#[test]
fn animations_off_by_default_and_active_logic() {
    let a = AnimationsConfig::default();
    assert!(!a.master);
    assert!(!a.any_effect());
    assert!(!a.active());

    // An effect on but master off → inactive (global kill-switch wins).
    let effect_only = AnimationsConfig {
        rain: true,
        ..Default::default()
    };
    assert!(effect_only.any_effect());
    assert!(!effect_only.active());

    // The UI-wide effects count as effects too — master + only `caret` (or `toast`)
    // must wake the clock, or the new toggles would silently never run.
    let ui_only = AnimationsConfig {
        master: true,
        caret: true,
        ..Default::default()
    };
    assert!(ui_only.any_effect());
    assert!(ui_only.active());
    let toast_only = AnimationsConfig {
        master: true,
        toast: true,
        ..Default::default()
    };
    assert!(toast_only.active());

    // Master on but no effect → still inactive (nothing to draw).
    let master_only = AnimationsConfig {
        master: true,
        ..Default::default()
    };
    assert!(!master_only.active());

    // Master + an effect → active.
    let on = AnimationsConfig {
        master: true,
        donut: true,
        ..Default::default()
    };
    assert!(on.active());

    // A missing "animations" key forward-migrates to all-off.
    let back: Config = serde_json::from_str("{}").unwrap();
    assert!(!back.animations.active());
}

#[test]
fn normalize_user_dir_trims_and_expands_tilde() {
    // Whitespace-only / empty → unset (falls through to the default), not a spaces-named dir.
    assert_eq!(normalize_user_dir("   "), None);
    assert_eq!(normalize_user_dir(""), None);
    // Surrounding whitespace trimmed.
    assert_eq!(
        normalize_user_dir("  /tmp/x  "),
        Some(PathBuf::from("/tmp/x"))
    );
    // Leading ~ / ~/ expands to home (was a literal `~` dir before).
    if let Some(base) = directories::BaseDirs::new() {
        let home = base.home_dir();
        assert_eq!(normalize_user_dir("~"), Some(home.to_path_buf()));
        assert_eq!(
            normalize_user_dir("~/Music/ytt"),
            Some(home.join("Music/ytt"))
        );
    }
    // A bare relative or absolute path is kept as-is.
    assert_eq!(
        normalize_user_dir("relative/dir"),
        Some(PathBuf::from("relative/dir"))
    );
}

#[test]
fn env_overrides_download_dir() {
    with_var("YTM_DOWNLOAD_DIR", Some("/tmp/ytm-dl-test"), || {
        assert_eq!(
            Config::default().effective_download_dir(),
            PathBuf::from("/tmp/ytm-dl-test")
        );
    });
}

#[test]
fn download_concurrency_defaults_clamps_and_honors_env() {
    let configured = |download_concurrency| {
        Config {
            download_concurrency,
            ..Config::default()
        }
        .effective_download_concurrency()
    };

    with_var("YTM_DOWNLOAD_CONCURRENCY", None, || {
        assert_eq!(configured(None), DOWNLOAD_CONCURRENCY_DEFAULT);
        assert_eq!(configured(Some(99)), DOWNLOAD_CONCURRENCY_MAX);
        assert_eq!(configured(Some(0)), DOWNLOAD_CONCURRENCY_MIN);
    });
    with_var("YTM_DOWNLOAD_CONCURRENCY", Some("99"), || {
        assert_eq!(configured(None), DOWNLOAD_CONCURRENCY_MAX);
    });
    with_var("YTM_DOWNLOAD_CONCURRENCY", Some("not-a-number"), || {
        assert_eq!(configured(Some(1)), 1);
    });
}

#[test]
fn imports_old_download_directory() {
    let dir = std::env::temp_dir().join(format!("ytm-old-dl-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("config.json");
    fs::write(&p, r#"{"downloadDirectory":"/music/dl"}"#).unwrap();
    let mut cfg = Config::default();
    import_old_from(&p, &mut cfg);
    assert_eq!(cfg.download_dir, Some(PathBuf::from("/music/dl")));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn imports_old_volume_and_cookie() {
    let dir = std::env::temp_dir().join(format!("ytm-old-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("config.json");
    fs::write(
        &p,
        r#"{"volume":42,"youtubeMusic":{"cookie":"SID=fromold"}}"#,
    )
    .unwrap();
    let mut cfg = Config::default();
    import_old_from(&p, &mut cfg);
    assert_eq!(cfg.volume, 42);
    assert_eq!(cfg.cookie.as_deref(), Some("SID=fromold"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn effective_dj_gem_language_resolves_retro_auto_and_concrete() {
    // A concrete choice is used as-is, regardless of UI language.
    let cfg = Config {
        dj_gem_language: DjGemLanguage::Japanese,
        language: Language::Korean,
        ..Config::default()
    };
    assert_eq!(cfg.effective_dj_gem_language(), DjGemLanguage::Japanese);

    // Auto + Korean UI → Korean (preserves the historical Korean-UI behavior).
    let cfg = Config {
        dj_gem_language: DjGemLanguage::Auto,
        language: Language::Korean,
        ..Config::default()
    };
    assert_eq!(cfg.effective_dj_gem_language(), DjGemLanguage::Korean);

    // Auto + non-Korean UI → stays Auto (no forced directive; model replies in kind).
    let cfg = Config {
        dj_gem_language: DjGemLanguage::Auto,
        language: Language::English,
        ..Config::default()
    };
    assert_eq!(cfg.effective_dj_gem_language(), DjGemLanguage::Auto);

    // Retro mode overrides any choice to English.
    let cfg = Config {
        dj_gem_language: DjGemLanguage::Korean,
        retro_mode: true,
        language: Language::Korean,
        ..Config::default()
    };
    assert_eq!(cfg.effective_dj_gem_language(), DjGemLanguage::English);
}
