use super::*;

#[test]
fn space_toggles_pause_and_emits_cmd() {
    let mut app = App::new(100);
    let cmds = app.update(Msg::Key(key(KeyCode::Char(' '))));
    assert!(app.playback.paused);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::CyclePause)]
    ));
}

#[test]
fn restores_last_history_track_without_autoplaying() {
    let mut app = App::new(100);
    app.library.record_play(&songs(2)[0]);
    app.library.record_play(&songs(2)[1]);
    app.restore_last_played_from_library();
    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "id1");
    assert!(app.playback.paused);
    assert!(app.prefetch.loaded_video_id.is_none());
}

#[test]
fn play_loads_restored_history_track() {
    let mut app = App::new(100);
    app.library.record_play(&songs(1)[0]);
    app.restore_last_played_from_library();
    let cmds = app.update(Msg::Key(key(KeyCode::Char(' '))));
    assert_loads_video(&cmds, "id0");
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));
    assert!(!app.playback.paused);
}

#[test]
fn autoplay_on_start_plays_restored_track_when_enabled() {
    let mut app = App::new(100);
    app.library.record_play(&songs(1)[0]);
    app.restore_last_played_from_library();
    app.config.autoplay_on_start = Some(true);
    // The launch trigger loads the restored track and starts it (no key press).
    let cmds = app.update(Msg::Autoplay);
    assert_loads_video(&cmds, "id0");
    assert_eq!(app.prefetch.loaded_video_id.as_deref(), Some("id0"));
    assert!(!app.playback.paused);
}

#[test]
fn autoplay_on_start_is_noop_when_disabled() {
    let mut app = App::new(100);
    app.library.record_play(&songs(1)[0]);
    app.restore_last_played_from_library();
    // Default (opt-in off): the trigger does nothing; the track stays paused and unloaded.
    assert!(!app.config.effective_autoplay_on_start());
    let cmds = app.update(Msg::Autoplay);
    assert_no_load(&cmds);
    assert!(app.prefetch.loaded_video_id.is_none());
    assert!(app.playback.paused);
}

#[test]
fn restores_radio_session_mode_and_last_station_without_autoplaying() {
    let mut app = App::new(100);
    app.library.record_play(&radio_station("older"));
    app.library.record_play(&radio_station("latest"));

    app.restore_last_session_from_library(true);

    assert!(app.radio_dedicated_mode);
    assert_eq!(app.theme.preset, "dario");
    assert_eq!(app.search.source, SearchSource::RadioBrowser);
    assert_eq!(app.library_tabs(), &LibraryTab::RADIO_MODE);
    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "rad:latest");
    assert!(app.playback.paused);
    assert!(app.prefetch.loaded_video_id.is_none());
}

#[test]
fn restores_cached_radio_mode_even_without_recent_station() {
    let mut app = App::new(100);

    app.restore_last_session_from_library(true);

    assert!(app.radio_dedicated_mode);
    assert_eq!(app.search.source, SearchSource::RadioBrowser);
    assert!(app.queue.is_empty());
}

#[test]
fn restores_normal_session_mode_from_song_history() {
    let mut app = App::new(100);
    app.library.record_play(&radio_station("station"));
    app.library.record_play(&songs(1)[0]);

    app.restore_last_session_from_library(false);

    assert!(!app.radio_dedicated_mode);
    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "id0");
    assert!(app.playback.paused);
}

#[test]
fn up_down_adjust_volume_in_player_mode() {
    let mut app = App::new(50);
    let cmds = app.update(Msg::Key(key(KeyCode::Up)));
    assert_eq!(app.playback.volume, 55);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(55))]
    ));

    let cmds = app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.playback.volume, 50);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Player(PlayerCmd::SetVolume(50))]
    ));
}

#[test]
fn time_pos_redraws_only_on_second_change() {
    let mut app = App::new(100);
    app.dirty = false;
    app.update(PlayerMsg::TimePos(1.1));
    assert!(app.dirty);
    app.dirty = false;
    app.update(PlayerMsg::TimePos(1.9)); // same whole second
    assert!(!app.dirty);
    app.update(PlayerMsg::TimePos(2.0)); // new second
    assert!(app.dirty);
}
