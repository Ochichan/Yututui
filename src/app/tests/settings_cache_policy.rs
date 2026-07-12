use super::*;

#[test]
fn reset_all_preserves_tui_mpv_output_device_and_cache_like_origin_main() {
    // This is intentionally different from the daemon GUI's full-Config reset. The terminal
    // Settings reset has never included the mpv output/device/cache draft fields; keep that exact
    // behavior unless the user explicitly approves the visible settings-semantics change.
    let mut app = app_playing(1, 0);
    app.config.audio.mpv.output = Some("pipewire".to_owned());
    app.config.audio.mpv.device = Some("alsa/custom".to_owned());
    app.config.audio.mpv.cache_forward = "64MiB".to_owned();
    app.config.audio.mpv.cache_back = "12MiB".to_owned();
    app.config.audio.mpv.cache_defaults_revision = u64::MAX;

    app.update(Msg::Key(key(KeyCode::Char('o'))));
    let mut reset = app.settings_reset_all();
    admit_player_transition(&mut app, &mut reset);

    let draft = &app.settings.as_ref().unwrap().draft;
    assert_eq!(draft.audio_mpv_output, "pipewire");
    assert_eq!(draft.audio_mpv_device, "alsa/custom");
    assert_eq!(draft.audio_mpv_cache_forward, "64MiB");
    assert_eq!(draft.audio_mpv_cache_back, "12MiB");

    let cmds = super::settings_ui::update_and_admit(&mut app, Msg::Key(key(KeyCode::Char('q'))));
    let saved = save_config(&cmds).unwrap();
    assert_eq!(saved.audio.mpv.output.as_deref(), Some("pipewire"));
    assert_eq!(saved.audio.mpv.device.as_deref(), Some("alsa/custom"));
    assert_eq!(saved.audio.mpv.cache_forward, "64MiB");
    assert_eq!(saved.audio.mpv.cache_back, "12MiB");
    assert_eq!(saved.audio.mpv.cache_defaults_revision, u64::MAX);
}

#[test]
fn direct_tui_cache_edits_mark_current_without_lowering_future_revision() {
    let mut app = app_playing(1, 0);
    app.config.audio.mpv.cache_defaults_revision = 0;
    app.update(Msg::Key(key(KeyCode::Char('o'))));

    app.settings.as_mut().unwrap().draft.audio_mpv_cache_forward = "48MiB".to_owned();
    app.settings_persist_text_field(Field::AudioMpvCacheForward);
    assert_eq!(app.config.audio.mpv.cache_forward, "48MiB");
    assert_eq!(
        app.config.audio.mpv.cache_defaults_revision,
        crate::config::MPV_CACHE_DEFAULTS_REVISION
    );

    app.config.audio.mpv.cache_defaults_revision = u64::MAX;
    app.settings.as_mut().unwrap().draft.audio_mpv_cache_back = "6MiB".to_owned();
    app.settings_persist_text_field(Field::AudioMpvCacheBack);
    assert_eq!(app.config.audio.mpv.cache_back, "6MiB");
    assert_eq!(app.config.audio.mpv.cache_defaults_revision, u64::MAX);
}
