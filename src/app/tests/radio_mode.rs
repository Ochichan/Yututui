use super::*;

#[test]
fn track_load_carries_owner_known_live_context_without_duration_inference() {
    let mut app = App::new(50);
    app.queue.set(vec![radio_station("finite-dvr")], 0);
    let radio = app.load_song(app.queue.current().cloned());
    assert_eq!(
        load_source_context(&radio),
        Some(crate::player::MediaSourceContext::Live)
    );

    app.queue.set(songs(1), 0);
    let on_demand = app.load_song(app.queue.current().cloned());
    assert_eq!(
        load_source_context(&on_demand),
        Some(crate::player::MediaSourceContext::OnDemand)
    );
}

fn apply_radio_mode_and_admit(app: &mut App, confirm: RadioModeConfirm) -> Vec<Cmd> {
    let mut cmds = app.apply_radio_mode_confirm(confirm);
    admit_player_transition(app, &mut cmds);
    cmds
}

fn confirm_pending_radio_mode_and_admit(app: &mut App) -> Vec<Cmd> {
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    admit_player_transition(app, &mut cmds);
    cmds
}

fn ordered_player_commands(cmds: &[Cmd]) -> Vec<&PlayerCmd> {
    cmds.iter().flat_map(Cmd::player_commands).collect()
}

#[test]
fn alt_shift_r_confirms_dedicated_radio_mode() {
    let mut app = app_playing(1, 0);
    assert!(!app.radio_dedicated_mode);
    assert!(
        !app.search_config_for_mode()
            .selectable_sources()
            .contains(&SearchSource::RadioBrowser)
    );
    assert_eq!(app.library_tabs(), &LibraryTab::NORMAL);

    let cmds = app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert!(cmds.is_empty());
    assert_eq!(
        app.radio_mode.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
    assert!(!app.radio_dedicated_mode);

    let mut enter = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        !app.radio_dedicated_mode,
        "the visible mode must wait for player admission"
    );
    assert_eq!(
        app.radio_mode.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
    admit_player_transition(&mut app, &mut enter);
    assert!(app.radio_dedicated_mode);
    assert!(app.radio_mode.pending_radio_mode_confirm.is_none());
    assert_eq!(app.theme.preset, "dario");
    assert_eq!(
        app.theme.effective_hex(crate::theme::ThemeRole::Background),
        "none"
    );
    assert_eq!(
        app.search_config_for_mode().selectable_sources(),
        vec![SearchSource::RadioBrowser]
    );
    assert_eq!(app.search.source, SearchSource::RadioBrowser);
    assert_eq!(app.library_tabs(), &LibraryTab::RADIO_MODE);

    app.update(Msg::Key(key(KeyCode::Char('g'))));
    assert_eq!(app.mode, Mode::Ai, "DJ Gem remains available in Radio mode");
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Player);

    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert_eq!(
        app.radio_mode.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Exit)
    );
    confirm_pending_radio_mode_and_admit(&mut app);
    assert!(!app.radio_dedicated_mode);
    assert_eq!(app.theme.preset, "default");
    assert!(
        !app.search_config_for_mode()
            .selectable_sources()
            .contains(&SearchSource::RadioBrowser)
    );
}

#[test]
fn alt_shift_r_radio_mode_switch_only_works_on_player() {
    let mut app = App::new(100);

    app.mode = Mode::Search;
    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert!(app.radio_mode.pending_radio_mode_confirm.is_none());
    assert!(!app.radio_dedicated_mode);

    app.mode = Mode::Library;
    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert!(app.radio_mode.pending_radio_mode_confirm.is_none());
    assert!(!app.radio_dedicated_mode);

    app.mode = Mode::Player;
    app.update(Msg::Key(alt_shift(KeyCode::Char('r'))));
    assert_eq!(
        app.radio_mode.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
}

#[test]
fn radio_mode_switch_stops_playback_restores_cached_queues_and_themes() {
    let mut app = app_playing(3, 1);
    app.theme.set_preset(crate::theme::ThemePreset::Midnight);
    app.config.theme = app.theme.clone();
    app.playback.paused = false;
    app.streaming.pending = true;
    app.streaming.pending_rerank = Some(PendingRerank {
        seed_video_id: "id1".to_owned(),
        shortlist: Vec::new(),
        local_pick: Vec::new(),
        cid_map: Vec::new(),
        mode: crate::streaming::config::StreamingMode::Balanced,
        cache_key: 42,
    });

    let mut enter = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);

    assert!(has_stop(&enter), "entering Radio mode should stop mpv");
    assert!(!app.radio_dedicated_mode);
    assert_eq!(app.queue.len(), 3);
    assert!(!app.playback.paused);
    assert!(load_url(&enter).is_none());
    assert!(app.streaming.pending);
    assert!(app.streaming.pending_rerank.is_some());
    assert_eq!(app.theme.preset, "midnight");

    admit_player_transition(&mut app, &mut enter);
    assert!(app.radio_dedicated_mode);
    assert!(app.queue.is_empty());
    assert!(app.playback.paused);
    assert!(!app.streaming.pending);
    assert!(app.streaming.pending_rerank.is_none());
    assert_eq!(app.theme.preset, "dario");

    app.queue.set(
        vec![radio_station("station-a"), radio_station("station-b")],
        1,
    );
    let mut radio_load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut radio_load);
    app.playback.paused = false;
    app.theme.set_preset(crate::theme::ThemePreset::RosePine);
    let mut exit = app.apply_radio_mode_confirm(RadioModeConfirm::Exit);

    assert!(has_stop(&exit), "leaving Radio mode should stop mpv");
    assert!(app.radio_dedicated_mode);
    assert_eq!(app.queue.len(), 2);
    let exit_player = ordered_player_commands(&exit);
    let stop = exit_player
        .iter()
        .position(|command| matches!(command, PlayerCmd::Stop))
        .expect("mode switch Stop");
    let load = exit_player
        .iter()
        .position(|command| matches!(command, PlayerCmd::Load(_)))
        .expect("restored queue Load");
    assert!(stop < load, "mode switch must stop before replacement load");
    admit_player_transition(&mut app, &mut exit);

    assert!(!app.radio_dedicated_mode);
    assert_eq!(app.queue.len(), 3);
    assert_eq!(current(&app), "id1");
    assert!(
        load_url(&exit)
            .expect("restored normal track load")
            .contains("id1")
    );
    assert!(!app.playback.paused);
    assert_eq!(app.theme.preset, "midnight");

    app.theme.set_preset(crate::theme::ThemePreset::Light);
    app.queue.set(songs(2), 0);
    let mut normal_load = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut normal_load);
    let mut reenter = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);

    assert!(has_stop(&reenter));
    assert!(!app.radio_dedicated_mode);
    admit_player_transition(&mut app, &mut reenter);

    assert!(app.radio_dedicated_mode);
    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "rad:station-b");
    assert!(
        load_url(&reenter)
            .expect("restored Radio station load")
            .contains("station-b.mp3")
    );
    assert!(!app.playback.paused);
    assert_eq!(
        app.theme.preset, "rose_pine",
        "Radio mode should remember the last Radio theme"
    );

    let mut second_exit = app.apply_radio_mode_confirm(RadioModeConfirm::Exit);

    assert!(has_stop(&second_exit));
    assert!(app.radio_dedicated_mode);
    admit_player_transition(&mut app, &mut second_exit);

    assert!(!app.radio_dedicated_mode);
    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "id0");
    assert!(
        load_url(&second_exit)
            .expect("updated normal queue load")
            .contains("id0")
    );
    assert_eq!(app.theme.preset, "light");
}

#[test]
fn radio_mode_busy_and_closed_preserve_the_complete_switch_for_retry() {
    use crate::util::delivery::DeliveryError;

    for error in [DeliveryError::Busy, DeliveryError::Closed] {
        let mut app = app_playing(3, 1);
        app.theme.set_preset(crate::theme::ThemePreset::Midnight);
        app.config.theme = app.theme.clone();
        app.playback.paused = false;
        app.streaming.pending = true;
        app.streaming.pending_rerank = Some(PendingRerank {
            seed_video_id: "id1".to_owned(),
            shortlist: Vec::new(),
            local_pick: Vec::new(),
            cid_map: Vec::new(),
            mode: crate::streaming::config::StreamingMode::Balanced,
            cache_key: 7,
        });
        app.radio_mode.pending_radio_mode_confirm = Some(RadioModeConfirm::Enter);

        let before_queue = serde_json::to_vec(&app.queue.snapshot()).unwrap();
        let before_rev = app.queue.rev();
        let before_epoch = app.playback.position_epoch;
        let before_theme = serde_json::to_vec(&app.theme).unwrap();
        let before_art_clear = app.art.force_clear_next_frame;

        let cmds = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);

        assert!(has_stop(&cmds));
        assert!(!app.radio_dedicated_mode);
        assert_eq!(
            app.radio_mode.pending_radio_mode_confirm,
            Some(RadioModeConfirm::Enter)
        );
        assert_eq!(
            serde_json::to_vec(&app.queue.snapshot()).unwrap(),
            before_queue
        );
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(app.playback.position_epoch, before_epoch);
        assert!(!app.playback.paused);
        assert!(app.streaming.pending);
        assert!(app.streaming.pending_rerank.is_some());
        assert_eq!(serde_json::to_vec(&app.theme).unwrap(), before_theme);
        assert_eq!(app.art.force_clear_next_frame, before_art_clear);

        assert!(reject_player_transition(&mut app, cmds, error).is_empty());
        assert!(!app.radio_dedicated_mode);
        assert_eq!(
            app.radio_mode.pending_radio_mode_confirm,
            Some(RadioModeConfirm::Enter),
            "a rejected confirmation must remain retryable"
        );
        assert_eq!(
            serde_json::to_vec(&app.queue.snapshot()).unwrap(),
            before_queue
        );
        assert_eq!(app.queue.rev(), before_rev);
        assert_eq!(app.playback.position_epoch, before_epoch);
        assert!(!app.playback.paused);
        assert!(app.streaming.pending);
        assert!(app.streaming.pending_rerank.is_some());
        assert_eq!(serde_json::to_vec(&app.theme).unwrap(), before_theme);
        assert_eq!(app.art.force_clear_next_frame, before_art_clear);
        assert_eq!(app.status.kind, StatusKind::Error);
        assert!(!app.status.text.is_empty(), "rejection must be visible");

        let mut retry = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
        admit_player_transition(&mut app, &mut retry);
        assert!(app.radio_dedicated_mode);
        assert!(app.radio_mode.pending_radio_mode_confirm.is_none());
        assert!(app.queue.is_empty());
        assert!(app.playback.paused);
        assert_eq!(app.playback.position_epoch, before_epoch + 1);
        assert!(!app.streaming.pending);
        assert!(app.streaming.pending_rerank.is_none());
        assert_eq!(app.theme.preset, "dario");
    }
}

#[test]
fn radio_mode_theme_edits_do_not_overwrite_normal_config_theme() {
    let mut app = App::new(100);
    app.theme.set_preset(crate::theme::ThemePreset::Midnight);
    app.config.theme = app.theme.clone();
    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);

    app.open_settings();
    {
        let st = app.settings.as_mut().expect("settings open");
        st.draft
            .theme
            .set_preset(crate::theme::ThemePreset::RosePine);
    }
    let mut cmds = app.close_settings();
    admit_player_transition(&mut app, &mut cmds);

    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
    assert_eq!(app.theme.preset, "rose_pine");
    assert_eq!(
        app.config.theme.preset, "midnight",
        "normal theme in config should survive Radio-mode theme edits"
    );
    assert_eq!(
        app.config.radio_theme.as_ref().map(|t| t.preset.as_str()),
        Some("rose_pine"),
        "a Radio-mode theme edit should persist into its own config slot"
    );

    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Exit);
    assert_eq!(app.theme.preset, "midnight");
    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);
    assert_eq!(app.theme.preset, "rose_pine");
}

#[test]
fn persisted_radio_theme_survives_restart_into_radio_session() {
    // Quit while in radio mode with a saved radio theme, relaunch: the session restore
    // must find the persisted radio theme instead of falling back to Radio.
    let mut cfg = crate::config::Config::default();
    let mut radio_theme = crate::theme::ThemeConfig::default();
    radio_theme.set_preset(crate::theme::ThemePreset::RosePine);
    cfg.radio_theme = Some(radio_theme);

    let mut app = App::new(100);
    app.apply_config(&cfg);
    app.library.record_play(&radio_station("latest"));
    app.restore_last_session_from_library(true);

    assert!(app.radio_dedicated_mode);
    assert_eq!(app.theme.preset, "rose_pine");
}

#[test]
fn persisted_radio_theme_applies_on_radio_reentry_after_relaunch() {
    // Quit in NORMAL mode (radio theme saved earlier), relaunch, then re-enter radio
    // mode: the stash seeded from config must win over the Radio fallback, and exiting
    // must return to the normal theme untouched.
    let mut cfg = crate::config::Config::default();
    let mut normal = crate::theme::ThemeConfig::default();
    normal.set_preset(crate::theme::ThemePreset::Midnight);
    cfg.theme = normal;
    let mut radio_theme = crate::theme::ThemeConfig::default();
    radio_theme.set_preset(crate::theme::ThemePreset::RosePine);
    cfg.radio_theme = Some(radio_theme);

    let mut app = App::new(100);
    app.apply_config(&cfg);
    assert_eq!(app.theme.preset, "midnight");

    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);
    assert_eq!(app.theme.preset, "rose_pine");
    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Exit);
    assert_eq!(app.theme.preset, "midnight");
}

#[test]
fn settings_enqueue_next_toggle_persists_on_close() {
    let mut app = App::new(100);
    app.open_settings();
    let row = SettingsTab::General
        .fields()
        .iter()
        .position(|f| *f == Field::EnqueueNext)
        .expect("enqueue-next setting");
    app.settings.as_mut().unwrap().row = row;

    app.settings_change(1);
    assert!(app.settings.as_ref().unwrap().draft.enqueue_next);
    let mut cmds = app.close_settings();
    admit_player_transition(&mut app, &mut cmds);

    assert!(app.config.effective_enqueue_next());
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Config(_))))
    );
}

#[test]
fn radio_mode_nav_labels_player_as_radio_without_shifting_tabs() {
    let _guard = crate::i18n::lock_for_test();
    let mut app = App::new(100);

    let normal = render_app_buffer(&app, 80, 24);
    let normal_text: String = normal
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        normal_text.contains("Player"),
        "normal nav should show Player"
    );
    let normal_buttons = app.hits.regions();
    let normal_player = normal_buttons
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Player))
        .expect("normal Player tab")
        .rect;
    let normal_search = normal_buttons
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Search))
        .expect("normal Search tab")
        .rect;
    drop(normal_buttons);

    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);
    let radio = render_app_buffer(&app, 80, 24);
    let radio_text: String = radio
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        radio_text.contains("Radio"),
        "Radio nav should replace Player"
    );
    let radio_buttons = app.hits.regions();
    let radio_player = radio_buttons
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Player))
        .expect("Radio Player tab")
        .rect;
    let radio_search = radio_buttons
        .iter()
        .find(|b| b.target == MouseTarget::Nav(Mode::Search))
        .expect("Radio Search tab")
        .rect;

    assert_eq!(radio_player.width, normal_player.width);
    assert_eq!(
        radio_search.x, normal_search.x,
        "Search tab should not shift when Player becomes Radio"
    );
    assert!(
        radio_buttons
            .iter()
            .any(|b| b.target == MouseTarget::Nav(Mode::Ai)),
        "DJ Gem tab stays visible in Radio mode"
    );
}

#[test]
fn radio_mode_renders_custom_radio_art() {
    let mut app = App::new(100);
    // The set piece rides the album-art toggle (off by default).
    app.config.album_art = Some(true);

    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);
    let radio = render_app_buffer(&app, 80, 24);
    let radio_text: String = radio
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        radio_text.contains("⢸⣿⣿⣉⣉⣉⣹⣿"),
        "radio mode should render the custom radio art"
    );
}

#[test]
fn radio_separator_renders_only_in_radio_mode() {
    let mut app = app_playing(1, 0);
    make_test_art_active(&mut app, ratatui_image::picker::ProtocolType::Halfblocks);

    let normal = render_app_buffer(&app, 100, 36);
    let normal_text: String = normal
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        !normal_text.contains("♫♪.ılılı"),
        "normal player mode should not render the radio separator"
    );

    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);
    let radio = render_app_buffer(&app, 80, 24);
    let radio_text: String = radio
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        radio_text.contains("♫♪.ılılı"),
        "radio mode should render the separator inside the player border"
    );
}

#[test]
fn radio_art_animates_when_animation_master_is_on() {
    let mut app = App::new(100);
    // The set piece rides the album-art toggle (off by default).
    app.config.album_art = Some(true);
    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);
    app.queue.set(vec![radio_station("moving")], 0);
    app.playback.paused = false;
    app.config.animations.master = true;

    assert!(
        app.animation_active(),
        "radio art should wake the animation clock when the master switch is on"
    );

    app.anim.anim_frame = 0;
    let first = render_app_buffer(&app, 80, 24);
    let first_text: String = first
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    app.anim.anim_frame = 24;
    let later = render_app_buffer(&app, 80, 24);
    let later_text: String = later
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert_ne!(
        first_text, later_text,
        "radio mode art should move on a slower animation phase"
    );
}

/// Read one buffer row as a string of cell symbols (index == column, one symbol per cell).

#[test]
fn radio_art_hidden_when_album_art_disabled() {
    let mut app = App::new(100);
    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);
    app.queue.set(vec![radio_station("plain")], 0);
    app.playback.paused = false;
    app.config.animations.master = true;

    let radio = render_app_buffer(&app, 80, 24);
    let text: String = radio
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        !text.contains("⢸⣿⣿⣉⣉⣉⣹⣿"),
        "album art off must hide the radio set piece"
    );
    assert!(
        !text.contains("♫♪.ılılı"),
        "album art off must hide the one-line art too"
    );
    assert!(
        !app.animation_active(),
        "with the set piece hidden and no effects enabled the clock must stay asleep"
    );
}

#[test]
fn radio_mode_keeps_gap_and_animates_canvas_below_separator() {
    let mut app = App::new(100);
    app.config.album_art = Some(true);
    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);
    app.queue.set(vec![radio_station("canvas")], 0);
    app.playback.paused = false;
    app.config.animations.master = true;
    app.config.animations.rain = true;

    app.anim.anim_frame = 0;
    let first = render_app_buffer(&app, 100, 36);
    let sep_y = (0..36)
        .find(|&y| buffer_row(&first, y).contains("ılılı"))
        .expect("one-line art row");

    // Two luxury rows sit between the set piece's bottom edge and the one-line art
    // (the art's own blank-braille pad row is ⠀ glyphs, not spaces, so a collapsed gap
    // would show up here).
    for dy in 1..=2u16 {
        let interior: String = buffer_row(&first, sep_y - dy)
            .chars()
            .skip(1)
            .take(97)
            .collect();
        assert!(
            interior.trim().is_empty(),
            "row {dy} above the one-line art should be blank, got: {interior:?}"
        );
    }

    // The music-mode canvas (rain) animates in the blank band below the one-line art.
    app.anim.anim_frame = 40;
    let later = render_app_buffer(&app, 100, 36);
    let below = |buf: &ratatui::buffer::Buffer| -> String {
        (sep_y + 1..34).map(|y| buffer_row(buf, y)).collect()
    };
    assert_ne!(
        below(&first),
        below(&later),
        "the filler canvas below the one-line art should animate in radio mode"
    );
}

#[test]
fn toggle_animations_in_radio_mode_flips_radio_master_not_master() {
    let mut app = App::new(100);
    app.config.animations.master = true;
    apply_radio_mode_and_admit(&mut app, RadioModeConfirm::Enter);
    assert!(
        app.animations().master,
        "radio inherits the music master until first toggled"
    );

    let cmds = app.toggle_animations();

    assert!(
        app.config.animations.master,
        "the music-mode switch must stay untouched"
    );
    assert_eq!(app.config.animations.radio_master, Some(false));
    assert!(
        !app.animations().master,
        "radio mode now resolves to its own switch"
    );
    // The raw config (music master intact) is what gets persisted, never the resolved copy.
    assert!(matches!(
        &cmds[..],
        [Cmd::Persist(PersistCmd::Config(c))] if c.animations.master && c.animations.radio_master == Some(false)
    ));

    app.radio_dedicated_mode = false;
    assert!(
        app.animations().master,
        "music mode keeps animating independently of the radio switch"
    );
}

#[test]
fn double_clicking_active_player_tab_confirms_radio_mode() {
    let mut app = App::new(100);

    let cmds = double_click_target(&mut app, MouseTarget::Nav(Mode::Player));

    assert!(cmds.is_empty());
    assert_eq!(
        app.radio_mode.pending_radio_mode_confirm,
        Some(RadioModeConfirm::Enter)
    );
}

#[test]
fn autoplay_streaming_does_not_extend_from_radio_browser_streams() {
    let mut app = App::new(100);
    app.autoplay_streaming = true;
    app.queue.set(vec![radio_station("station-seed")], 0);
    app.mode = Mode::Player;

    let mut cmds = app.load_song(app.queue.current().cloned());
    admit_player_transition(&mut app, &mut cmds);

    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Cmd::StreamingFallback { .. }))
    );
    assert!(app.library.history.is_empty());
    assert_eq!(app.library.radios.len(), 1);
}
