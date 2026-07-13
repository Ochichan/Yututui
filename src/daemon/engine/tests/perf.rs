use super::*;

#[test]
fn configured_cache_values_flow_to_remote_projection_and_player_argv() {
    let mut engine = engine_with_queue(&["seed"]);
    engine.config.audio.mpv.output = Some(" pipewire ".to_owned());
    engine.config.audio.mpv.device = Some(" alsa/custom ".to_owned());
    engine.config.audio.mpv.cache_forward = "64MiB".to_owned();
    engine.config.audio.mpv.cache_back = "12MiB".to_owned();

    let settings = crate::remote::publish::settings_model(&engine.core_view(), 9);
    assert_eq!(settings.audio.mpv_output.as_deref(), Some("pipewire"));
    assert_eq!(settings.audio.mpv_device.as_deref(), Some("alsa/custom"));
    assert_eq!(settings.audio.mpv_cache_forward, "64MiB");
    assert_eq!(settings.audio.mpv_cache_back, "12MiB");
    assert_eq!(
        settings.audio.long_form_seek_optimization,
        Some(crate::config::LongFormSeekOptimization::Off)
    );
    assert_eq!(
        settings.audio.long_form_seek_effective,
        Some(crate::remote::proto::LongFormSeekEffective::NoMedia)
    );
    assert_eq!(
        settings.audio.long_form_seek_reason,
        Some(crate::remote::proto::LongFormSeekReason::NoMedia)
    );

    let runtime = engine.config.player_runtime(None);
    assert_eq!(
        crate::player::mpv::structured_audio_args(&runtime.audio.mpv),
        [
            "--audio-fallback-to-null=yes",
            "--audio-device=alsa/custom",
            "--demuxer-max-bytes=64MiB",
            "--demuxer-max-back-bytes=12MiB",
        ]
    );
}

#[test]
fn media_fingerprint_ignores_progress_but_tracks_projected_facets() {
    let mut engine = engine_with_queue(&["seed", "next"]);
    engine.loaded_video_id = Some("seed".to_owned());
    engine.playback.paused = false;
    engine.playback.time_pos = Some(1.0);
    engine.playback.time_pos_at = Some(Instant::now());
    let baseline = engine.media_fingerprint();

    engine.playback.time_pos = Some(42.0);
    engine.playback.time_pos_at = Some(Instant::now() - Duration::from_secs(1));
    assert_eq!(engine.media_fingerprint(), baseline);

    engine.playback.paused = true;
    assert_ne!(engine.media_fingerprint(), baseline);
    engine.playback.paused = false;
    assert_eq!(engine.media_fingerprint(), baseline);

    engine.library.toggle_favorite(&song("seed"));
    assert_ne!(engine.media_fingerprint(), baseline);
}

#[tokio::test]
async fn daemon_reset_all_keeps_its_origin_main_full_config_semantics() {
    let mut engine = engine_with_queue(&["seed"]);
    engine.config.audio.mpv.output = Some("pipewire".to_owned());
    engine.config.audio.mpv.device = Some("alsa/custom".to_owned());
    engine.config.audio.mpv.cache_forward = "64MiB".to_owned();
    engine.config.audio.mpv.cache_back = "12MiB".to_owned();
    engine.config.audio.mpv.cache_defaults_revision = u64::MAX;

    let (response, shutdown, effects) = engine.handle_remote(RemoteCommand::ResetAllSettings).await;

    assert!(response.ok);
    assert!(!shutdown);
    assert!(effects.is_empty());
    assert_eq!(engine.config.audio.mpv, Config::default().audio.mpv);
    assert_eq!(
        engine.config.audio.mpv.long_form_seek_optimization,
        crate::config::LongFormSeekOptimization::Off
    );
    assert_eq!(
        engine.config.audio.mpv.cache_defaults_revision,
        crate::config::MPV_CACHE_DEFAULTS_REVISION
    );
}
