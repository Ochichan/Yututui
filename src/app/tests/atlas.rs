use super::*;
use crate::api::radio_browser::RadioStation;
use crate::app::atlas::{AtlasFocus, AtlasTarget};
use crate::atlas::fetch::{AtlasCmd, AtlasErrorKind, AtlasEvent};
use crate::atlas::raster::CellRect;

fn radio_app() -> App {
    let mut app = App::new(100);
    app.config.album_art = Some(true);
    let mut cmds = app.apply_radio_mode_confirm(RadioModeConfirm::Enter);
    admit_player_transition(&mut app, &mut cmds);
    assert!(app.radio_dedicated_mode);
    app
}

fn station_json(uuid: &str, name: &str, lat: f32, lon: f32) -> RadioStation {
    RadioStation::parse(&serde_json::json!({
        "stationuuid": uuid,
        "name": name,
        "url": format!("https://example.com/{uuid}.mp3"),
        "countrycode": "KR",
        "codec": "MP3",
        "bitrate": 128,
        "geo_lat": lat,
        "geo_long": lon,
    }))
    .expect("fixture parses")
}

/// Open Atlas and feed it two stations: one at the default view centre, one behind the globe.
fn atlas_app() -> App {
    let mut app = radio_app();
    let cmds = app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert!(app.radio_mode.atlas.open, "`a` opens Atlas in radio mode");
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Atlas(AtlasCmd::World { .. }))),
        "opening requests the world page"
    );
    let generation = app.radio_mode.atlas.generation;
    let centre = app.radio_mode.atlas.camera.centre;
    app.update(Msg::Atlas(AtlasMsg::Event(AtlasEvent::World {
        generation,
        page: 0,
        from_cache: true,
        refreshing: false,
        rows: vec![
            station_json("centre-1", "Centre FM", centre.lat, centre.lon),
            station_json("far-2", "Far FM", -centre.lat, centre.lon + 180.0),
        ],
    })));
    assert_eq!(app.radio_mode.atlas.catalog.len(), 2);
    // Render once so the globe rect bridge exists for hit-testing.
    render_app_buffer(&app, 120, 40);
    assert!(app.bridges.atlas_globe.get().is_some());
    app
}

fn globe_centre_cell(app: &App) -> (u16, u16) {
    let rect: CellRect = app.bridges.atlas_globe.get().expect("globe rect");
    (rect.x + rect.width / 2, rect.y + rect.height / 2)
}

#[test]
fn toggle_outside_radio_mode_only_toasts() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert!(!app.radio_mode.atlas.open);
    assert!(app.status.text.contains("Radio"));
}

#[test]
fn q_and_escape_close_atlas_and_restore_the_set_piece() {
    let mut app = atlas_app();
    let before = render_app_buffer(&radio_app(), 80, 24);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(!app.radio_mode.atlas.open);
    let after = render_app_buffer(&app, 80, 24);
    assert_eq!(
        before, after,
        "closing Atlas restores the radio set piece bytes"
    );

    app.update(Msg::Key(key(KeyCode::Char('a'))));
    app.radio_mode.atlas.highlight = Some(0);
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.radio_mode.atlas.open, "Esc first clears the highlight");
    assert!(app.radio_mode.atlas.highlight.is_none());
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.radio_mode.atlas.open);
}

#[test]
fn leaving_radio_mode_and_going_home_close_atlas() {
    let mut app = atlas_app();
    let mut cmds = app.apply_radio_mode_confirm(RadioModeConfirm::Exit);
    admit_player_transition(&mut app, &mut cmds);
    assert!(!app.radio_dedicated_mode);
    assert!(!app.radio_mode.atlas.open);

    let mut app = atlas_app();
    app.go_home();
    assert!(!app.radio_mode.atlas.open);
}

#[test]
fn mini_tier_never_routes_keys_into_atlas() {
    let mut app = atlas_app();
    app.bridges.ui_tier.set(crate::ui::layout::UiTier::Mini);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert!(!app.radio_mode.atlas.open, "entering Mini closes Atlas");
}

#[test]
fn atlas_shadows_player_keys_but_passes_transport_through() {
    let mut app = atlas_app();
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(
        app.mode,
        Mode::Player,
        "`l` must not open the Library over the globe"
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Char(' '))));
    assert!(
        cmds.iter().any(|c| c.player_commands().next().is_some()),
        "Space still reaches the player transport"
    );
}

#[test]
fn arrows_rotate_and_plus_minus_zoom() {
    let mut app = atlas_app();
    let before = app.radio_mode.atlas.camera;
    app.update(Msg::Key(key(KeyCode::Right)));
    assert!(app.radio_mode.atlas.camera.centre.lon > before.centre.lon);
    app.update(Msg::Key(key(KeyCode::Char('h'))));
    assert!((app.radio_mode.atlas.camera.centre.lon - before.centre.lon).abs() < 1e-4);
    app.update(Msg::Key(key(KeyCode::Char('+'))));
    assert!(app.radio_mode.atlas.camera.scale > before.scale);
    app.update(Msg::Key(key(KeyCode::Char('0'))));
    assert_eq!(app.radio_mode.atlas.camera.scale, 1.0);
}

#[test]
fn next_signal_highlights_the_visible_station_and_enter_tunes_it() {
    let mut app = atlas_app();
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert_eq!(
        app.radio_mode.atlas.highlight,
        Some(0),
        "the far station is behind the globe"
    );
    assert!(app.radio_mode.atlas.context.contains("Centre FM"));
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.iter().any(|c| c.player_commands().next().is_some()));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        app.queue.current().map(|s| s.video_id.as_str()),
        Some("rad:centre-1")
    );
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Atlas(AtlasCmd::Click { uuid }) if uuid == "centre-1")),
        "a tune pings Radio Browser's click counter"
    );
    assert_eq!(app.radio_mode.atlas.selected, Some(0));
}

#[test]
fn click_on_a_marker_tunes_and_click_on_land_browses_the_country() {
    let mut app = atlas_app();
    let (col, row) = globe_centre_cell(&app);
    app.update(Msg::MouseClick {
        col,
        row,
        multi: false,
    });
    assert!(
        app.radio_mode.atlas.press.is_some(),
        "a press opens a session"
    );
    let mut cmds = app.update(Msg::MouseLeftUp);
    assert!(app.radio_mode.atlas.press.is_none());
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        app.queue.current().map(|s| s.video_id.as_str()),
        Some("rad:centre-1")
    );

    // Focus inland China (a cell spans ~2.5° on this small globe), then click the centre: no
    // marker there, so the country is browsed.
    app.radio_mode
        .atlas
        .camera
        .focus(crate::atlas::LatLon::new(35.0, 103.0));
    render_app_buffer(&app, 120, 40);
    let (col, row) = globe_centre_cell(&app);
    app.update(Msg::MouseClick {
        col,
        row,
        multi: false,
    });
    let cmds = app.update(Msg::MouseLeftUp);
    assert_eq!(app.radio_mode.atlas.active_country, Some(*b"CN"));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Atlas(AtlasCmd::Country { code, .. }) if code == "CN"))
    );
}

#[test]
fn drag_rotates_and_a_flick_coasts_only_with_animations_on() {
    let mut app = atlas_app();
    app.config.animations.master = true;
    app.config.animations.radio_master = Some(true);
    let (col, row) = globe_centre_cell(&app);
    let before = app.radio_mode.atlas.camera.centre;
    app.update(Msg::MouseClick {
        col,
        row,
        multi: false,
    });
    app.update(Msg::MouseDrag { col: col + 6, row });
    assert!(
        app.radio_mode.atlas.camera.centre.lon < before.lon,
        "dragging right lowers the centre longitude"
    );
    assert!(app.radio_mode.atlas.press.is_some_and(|p| p.moved));
    // The release velocity is sampled against wall-clock time since the press; a stalled test
    // runner must not turn a flick into a slow drag, so pin the clock right before releasing.
    let press = app.radio_mode.atlas.press.as_mut().unwrap();
    press.started = std::time::Instant::now();
    app.radio_mode.atlas.drag_clock = 0.0;
    app.update(Msg::MouseLeftUp);
    assert!(
        app.radio_mode.atlas.kinetic.active(),
        "a fast release coasts"
    );
    assert!(app.animation_active(), "coasting requests animation ticks");
    assert!(app.animation_draw_fps() <= 20);
    assert_eq!(app.queue.current(), None, "a drag release never tunes");

    // A press during the coast only stops it.
    app.update(Msg::MouseClick {
        col,
        row,
        multi: false,
    });
    assert!(!app.radio_mode.atlas.kinetic.active());
    app.update(Msg::MouseLeftUp);
    assert_eq!(
        app.queue.current(),
        None,
        "the catching press does not activate"
    );
    assert!(
        !app.animation_active(),
        "the clock sleeps once motion stops"
    );

    // Animations off: the same gesture rotates but never coasts.
    app.config.animations.master = false;
    app.config.animations.radio_master = Some(false);
    app.update(Msg::MouseClick {
        col,
        row,
        multi: false,
    });
    app.update(Msg::MouseDrag { col: col + 6, row });
    app.update(Msg::MouseLeftUp);
    assert!(!app.radio_mode.atlas.kinetic.active());
    assert!(!app.animation_active());
}

#[test]
fn focus_loss_drops_the_press_session() {
    let mut app = atlas_app();
    let (col, row) = globe_centre_cell(&app);
    app.update(Msg::MouseClick {
        col,
        row,
        multi: false,
    });
    app.update(Msg::Focus(false));
    assert!(app.radio_mode.atlas.press.is_none());
    app.update(Msg::MouseLeftUp);
    assert_eq!(
        app.queue.current(),
        None,
        "a stale press never becomes a click"
    );
}

#[test]
fn wheel_over_the_globe_zooms() {
    let mut app = atlas_app();
    let (col, row) = globe_centre_cell(&app);
    let before = app.radio_mode.atlas.camera.scale;
    app.update(Msg::MouseScroll {
        up: true,
        col,
        row,
        ctrl: false,
    });
    assert!(app.radio_mode.atlas.camera.scale > before);
}

#[test]
fn set_piece_tick_sleeps_behind_atlas_while_a_station_plays() {
    let mut app = atlas_app();
    app.config.animations.master = true;
    app.config.animations.radio_master = Some(true);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    admit_player_transition(&mut app, &mut cmds);
    assert!(app.queue.current().is_some());
    assert!(
        !app.animation_active(),
        "with the set piece hidden and no motion, Atlas must not keep the clock awake"
    );
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert!(
        app.animation_active(),
        "the set piece resumes its sway once Atlas closes"
    );
}

#[test]
fn stale_generation_events_are_dropped_and_errors_surface() {
    let mut app = atlas_app();
    let stale = app.radio_mode.atlas.generation - 1;
    app.update(Msg::Atlas(AtlasMsg::Event(AtlasEvent::World {
        generation: stale,
        page: 1,
        from_cache: false,
        refreshing: false,
        rows: vec![station_json("stale-9", "Stale", 1.0, 1.0)],
    })));
    assert_eq!(app.radio_mode.atlas.catalog.len(), 2);
    let generation = app.radio_mode.atlas.generation;
    app.update(Msg::Atlas(AtlasMsg::Event(AtlasEvent::Error {
        generation,
        kind: AtlasErrorKind::Network,
        message: "boom".to_owned(),
    })));
    assert!(app.radio_mode.atlas.error.is_some());
    let buf = render_app_buffer(&app, 120, 40);
    assert!(buffer_contains(&buf, "Radio Browser"));
}

#[test]
fn world_pages_keep_loading_until_the_station_limit() {
    let mut app = atlas_app();
    let generation = app.radio_mode.atlas.generation;
    let cmds = app.update(Msg::Atlas(AtlasMsg::Event(AtlasEvent::World {
        generation,
        page: 0,
        from_cache: false,
        refreshing: false,
        rows: vec![station_json("net-1", "Net", 2.0, 2.0)],
    })));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Atlas(AtlasCmd::More { .. }))),
        "below the limit the next random page is requested"
    );
    app.config.atlas.station_limit = 500;
    app.radio_mode.atlas.pages_fetched = 9;
    let cmds = app.update(Msg::Atlas(AtlasMsg::Event(AtlasEvent::World {
        generation,
        page: 9,
        from_cache: false,
        refreshing: false,
        rows: vec![],
    })));
    assert!(cmds.is_empty(), "the page budget stops progressive loading");
}

#[test]
fn search_box_captures_typed_keys_and_enter_requests_a_remote_search() {
    let mut app = atlas_app();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.radio_mode.atlas.search_editing);
    assert!(app.in_text_entry());
    for c in "cen".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.radio_mode.atlas.search_query, "cen");
    assert_eq!(
        app.mode,
        Mode::Player,
        "typed letters never trigger Player actions"
    );
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Atlas(AtlasCmd::Search { query, .. }) if query == "cen"))
    );
    assert_eq!(app.radio_mode.atlas.focus, AtlasFocus::Panel);
}

#[test]
fn panel_row_click_tunes_and_close_button_closes() {
    let mut app = atlas_app();
    let mut cmds = app.atlas_mouse_target(AtlasTarget::PanelRow(1), 0, 0);
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        app.queue.current().map(|s| s.video_id.as_str()),
        Some("rad:far-2")
    );
    app.atlas_mouse_target(AtlasTarget::Close, 0, 0);
    assert!(!app.radio_mode.atlas.open);
}

#[test]
fn braille_globe_renders_and_retro_falls_back_to_ascii() {
    let app = atlas_app();
    let buf = render_app_buffer(&app, 120, 40);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();
    assert!(
        text.chars().any(|c| ('\u{2801}'..='\u{28FF}').contains(&c)),
        "the globe is drawn with Braille dots"
    );
    assert!(
        buffer_contains(&buf, "◉") || buffer_contains(&buf, "•"),
        "a marker is drawn"
    );
    assert!(
        buf.content()
            .iter()
            .all(|c| unicode_width::UnicodeWidthStr::width(c.symbol()) <= 1),
        "every atlas cell is one column wide"
    );

    let mut retro = atlas_app();
    retro.config.retro_mode = true;
    let buf = render_app_buffer(&retro, 120, 40);
    let rect = retro.bridges.atlas_globe.get().expect("globe rect");
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            let sym = buf[(x, y)].symbol();
            assert!(
                crate::ui::retro::retro_supported(sym),
                "retro globe cell {sym:?} at ({x},{y}) is outside the console repertoire"
            );
        }
    }
}

#[test]
fn opening_and_closing_atlas_keeps_existing_radio_tests_honest() {
    // The set-piece tests render with Atlas closed; this pins that a closed Atlas is byte-inert.
    let mut app = radio_app();
    let before = render_app_buffer(&app, 80, 24);
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(before, render_app_buffer(&app, 80, 24));
}

#[test]
fn follow_playing_centres_the_globe_once_the_station_is_admitted() {
    let mut app = atlas_app();
    app.radio_mode
        .atlas
        .camera
        .focus(crate::atlas::LatLon::new(-40.0, 150.0));
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(app.radio_mode.atlas.follow_uuid.is_some());
    admit_player_transition(&mut app, &mut cmds);
    // Admission lands as its own message in the real loop; any next message triggers the follow.
    app.update(Msg::Noop);
    let centre = app.radio_mode.atlas.camera.centre;
    let tuned = app.radio_mode.atlas.selected.expect("a station was tuned");
    let station = app.radio_mode.atlas.catalog.get(tuned).unwrap().pos;
    assert!((centre.lat - station.lat).abs() < 1e-3 && (centre.lon - station.lon).abs() < 1e-3);
    assert!(
        app.radio_mode.atlas.follow_uuid.is_none(),
        "follow fires once"
    );
}

#[test]
fn escape_and_reset_clear_the_browsed_country_and_a_slow_clock_keeps_coasting() {
    let mut app = atlas_app();
    app.config.animations.master = true;
    app.config.animations.radio_master = Some(true);
    app.radio_mode
        .atlas
        .camera
        .focus(crate::atlas::LatLon::new(35.0, 103.0));
    render_app_buffer(&app, 120, 40);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    assert_eq!(app.radio_mode.atlas.active_country, Some(*b"CN"));
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(
        app.radio_mode.atlas.open,
        "Esc first drops the country filter"
    );
    assert!(app.radio_mode.atlas.active_country.is_none());
    assert!(app.radio_mode.atlas.active_mask.is_none());

    app.update(Msg::Key(key(KeyCode::Char('c'))));
    app.update(Msg::Key(key(KeyCode::Char('0'))));
    assert!(
        app.radio_mode.atlas.active_country.is_none(),
        "reset view returns to the world"
    );

    // A 5 fps animation clock delivers 200 ms ticks; the coast must survive them.
    app.radio_mode.atlas.kinetic = crate::atlas::view::Kinetic::launch(600.0, 0.0).unwrap();
    app.radio_mode.atlas.last_tick =
        Some(std::time::Instant::now() - std::time::Duration::from_millis(200));
    app.update(Msg::AnimTick);
    assert!(
        app.radio_mode.atlas.kinetic.active(),
        "a slow tick integrates instead of aborting"
    );
    app.radio_mode.atlas.last_tick =
        Some(std::time::Instant::now() - std::time::Duration::from_secs(3));
    app.update(Msg::AnimTick);
    assert!(
        !app.radio_mode.atlas.kinetic.active(),
        "a parked clock ends the coast"
    );
}

#[test]
fn panel_tabs_cycle_with_left_and_right_and_a_toggles_closed() {
    let mut app = atlas_app();
    app.update(Msg::Key(key(KeyCode::Tab)));
    assert_eq!(app.radio_mode.atlas.focus, AtlasFocus::Panel);
    app.update(Msg::Key(key(KeyCode::Right)));
    assert_eq!(
        app.radio_mode.atlas.panel_tab,
        crate::app::atlas::PanelTab::Favorites
    );
    app.update(Msg::Key(key(KeyCode::Left)));
    assert_eq!(
        app.radio_mode.atlas.panel_tab,
        crate::app::atlas::PanelTab::World
    );
    app.update(Msg::Key(key(KeyCode::Char('a'))));
    assert!(!app.radio_mode.atlas.open, "`a` toggles Atlas closed again");
}

#[test]
fn mouse_move_over_a_pin_sets_hover_without_keyboard_highlight() {
    let mut app = atlas_app();
    app.config.animations.master = true;
    app.config.animations.radio_master = Some(true);
    let (col, row) = globe_centre_cell(&app);
    app.update(Msg::MouseMove { col, row });
    assert_eq!(app.radio_mode.atlas.hover, Some(0));
    assert!(app.radio_mode.atlas.highlight.is_none());
    let buf = render_app_buffer(&app, 120, 40);
    assert!(buffer_contains(&buf, "Centre FM"));
    assert!(buffer_contains(&buf, "Centre FM · KR"));
}

#[test]
fn mouse_move_with_animations_off_latches_motion_but_skips_hover_chrome() {
    let mut app = atlas_app();
    app.config.animations.master = false;
    app.config.animations.radio_master = Some(false);
    let (col, row) = globe_centre_cell(&app);
    app.update(Msg::MouseMove { col, row });
    assert!(app.interaction.pointer_motion);
    assert!(app.radio_mode.atlas.hover.is_none());
    let buf = render_app_buffer(&app, 120, 40);
    assert!(
        !buffer_contains(&buf, "Centre FM · KR"),
        "animations off must not paint the hover tip"
    );
}

#[test]
fn keyboard_next_and_enter_tune_without_mouse_move_or_hover() {
    let mut app = atlas_app();
    assert!(!app.interaction.pointer_motion);
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    assert_eq!(app.radio_mode.atlas.highlight, Some(0));
    assert!(app.radio_mode.atlas.hover.is_none());
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        app.queue.current().map(|s| s.video_id.as_str()),
        Some("rad:centre-1")
    );
    assert!(app.radio_mode.atlas.hover.is_none());
    assert!(!app.interaction.pointer_motion);
}

#[test]
fn right_click_pin_opens_a_four_item_menu_without_prior_motion() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = atlas_app();
    let (col, row) = globe_centre_cell(&app);
    app.update(Msg::MouseRightClick { col, row });
    assert!(
        !app.interaction.pointer_motion,
        "right-click is not hover chrome"
    );
    let menu = app.overlays.context_menu.as_ref().expect("atlas pin menu");
    let labels: Vec<String> = menu
        .items
        .iter()
        .map(|item| item.label(menu.target_count()))
        .collect();
    assert_eq!(
        labels,
        vec![
            "Play now",
            "Favorite / unfavorite",
            "Copy stream url",
            "Open country list",
        ]
    );

    let mut play = atlas_app();
    let (col, row) = globe_centre_cell(&play);
    play.update(Msg::MouseRightClick { col, row });
    let mut cmds = choose_context_menu_item(&mut play, 0);
    admit_player_transition(&mut play, &mut cmds);
    assert_eq!(
        play.queue.current().map(|s| s.video_id.as_str()),
        Some("rad:centre-1")
    );

    let mut favorite = atlas_app();
    let (col, row) = globe_centre_cell(&favorite);
    favorite.update(Msg::MouseRightClick { col, row });
    let cmds = choose_context_menu_item(&mut favorite, 1);
    assert!(favorite.library.is_radio_favorite("rad:centre-1"));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );

    let mut copy = atlas_app();
    let (col, row) = globe_centre_cell(&copy);
    copy.update(Msg::MouseRightClick { col, row });
    choose_context_menu_item(&mut copy, 2);
    assert_eq!(copy.status.text, "✓ Stream URL copied to clipboard");

    let mut country = atlas_app();
    let (col, row) = globe_centre_cell(&country);
    country.update(Msg::MouseRightClick { col, row });
    let cmds = choose_context_menu_item(&mut country, 3);
    assert!(cmds.iter().any(|c| matches!(
        c,
        Cmd::Atlas(AtlasCmd::Country { code, .. }) if code.eq_ignore_ascii_case("KR")
    )));
}

#[test]
fn atlas_pin_menu_ignores_search_mouse_bindings() {
    let mut app = atlas_app();
    app.mousemap
        .set(
            crate::mousemap::MouseContext::Search,
            crate::mousemap::MouseGesture::RightClick,
            crate::mousemap::MouseAction::Enqueue,
        )
        .unwrap();
    let (col, row) = globe_centre_cell(&app);
    app.update(Msg::MouseRightClick { col, row });
    assert_eq!(
        app.overlays
            .context_menu
            .as_ref()
            .map(|menu| menu.items.len()),
        Some(4),
        "Search mouse remaps must not steal the Atlas pin menu"
    );
    assert_eq!(app.queue.len(), 0);
}

#[test]
fn enter_and_f_still_work_with_the_atlas_menu_closed() {
    let mut app = atlas_app();
    assert!(app.overlays.context_menu.is_none());
    app.update(Msg::Key(key(KeyCode::Char('n'))));
    let mut cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(
        app.queue.current().map(|s| s.video_id.as_str()),
        Some("rad:centre-1")
    );

    let mut fav = atlas_app();
    fav.update(Msg::Key(key(KeyCode::Char('n'))));
    let cmds = fav.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(fav.library.is_radio_favorite("rad:centre-1"));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
}
