use super::*;

#[test]
fn every_screen_renders_the_nav_bar() {
    for mode in [
        Mode::Player,
        Mode::Search,
        Mode::Library,
        Mode::Settings,
        Mode::Ai,
    ] {
        let mut app = app_playing(1, 0);
        app.navigate_to(mode);
        render_app(&app);
        let buttons = app.hits.regions();
        for nav in [
            Mode::Player,
            Mode::Search,
            Mode::Library,
            Mode::Settings,
            Mode::Ai,
        ] {
            assert!(
                buttons.iter().any(|b| b.target == MouseTarget::Nav(nav)),
                "screen {mode:?} is missing nav item {nav:?}"
            );
        }
    }
}

#[test]
fn clicking_a_nav_item_switches_screens() {
    let mut app = App::new(100);
    assert_eq!(app.mode, Mode::Player);
    click_target(&mut app, MouseTarget::Nav(Mode::Library));
    assert_eq!(app.mode, Mode::Library);
    click_target(&mut app, MouseTarget::Nav(Mode::Search));
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.search.focus, SearchFocus::Input);
}

#[test]
fn clicking_the_search_button_submits_the_query() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Input;
    app.search.input = "lofi beats".to_owned();
    let cmds = click_target(&mut app, MouseTarget::SearchSubmit);
    assert!(app.search.searching);
    assert!(matches!(
        cmds.as_slice(),
        [Cmd::Search { query, source, .. }]
            if query == "lofi beats" && *source == SearchSource::Youtube
    ));
}

#[test]
fn clicking_the_search_input_focuses_the_query_box() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(2);
    app.search.focus = SearchFocus::Results;
    app.search.select_all = true;
    app.dropdowns.search_source_open = true;

    click_target(&mut app, MouseTarget::SearchInput);

    assert_eq!(app.search.focus, SearchFocus::Input);
    assert!(!app.search.select_all);
    assert!(!app.dropdowns.search_source_open);
}

#[test]
fn clicking_a_library_tab_switches_it() {
    let mut app = App::new(100);
    app.mode = Mode::Library;
    assert_eq!(app.library_ui.tab, LibraryTab::All);
    click_target(&mut app, MouseTarget::LibraryTab(LibraryTab::Favorites));
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites);
}

#[test]
fn clicking_a_settings_tab_switches_it() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('o')))); // open settings
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::General);
    // SettingsTab::ALL[1] is Playback.
    click_target(&mut app, MouseTarget::SettingsTab(1));
    assert_eq!(app.settings.as_ref().unwrap().tab, SettingsTab::ALL[1]);
}

#[test]
fn single_click_on_a_result_row_selects_it() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(5);
    click_target(&mut app, MouseTarget::ListRow(2));
    assert_eq!(app.search.selected, 2);
    assert_eq!(app.search.focus, SearchFocus::Results);
}

#[test]
fn double_click_on_a_result_row_plays_it() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(5);
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::ListRow(3));
    let cmds = app.update(Msg::MouseDoubleClick { col, row });
    assert_eq!(current(&app), "id3");
    assert_loads_video(&cmds, "id3");
}

#[test]
fn clicking_the_position_label_opens_the_queue_window() {
    let mut app = app_playing(5, 2);
    assert!(!app.queue_popup.open);
    click_target(&mut app, MouseTarget::QueuePos);
    assert!(app.queue_popup.open);
    // It opens focused on the currently playing track.
    assert_eq!(app.queue_popup.cursor, 2);
    assert_eq!(app.queue_popup.anchor, 2);
}

#[test]
fn double_clicking_a_queue_row_jumps_to_it() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c')))); // open queue window
    assert!(app.queue_popup.open);
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::QueueRow(3));
    let cmds = app.update(Msg::MouseDoubleClick { col, row });
    assert_eq!(app.queue.cursor_pos(), 3);
    assert_eq!(current(&app), "id3");
    assert!(!app.queue_popup.open);
    assert_loads_video(&cmds, "id3");
}

#[test]
fn clicking_a_queue_delete_button_removes_that_track() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    click_target(&mut app, MouseTarget::QueueDel(2));
    assert_eq!(app.queue.len(), 4);
    assert!(
        app.queue.ordered().iter().all(|s| s.video_id != "id2"),
        "the removed track should be gone from the queue"
    );
}

#[test]
fn queue_row_context_menu_can_remove_that_track() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::QueueRow(2));

    let open_cmds = app.update(Msg::MouseRightClick { col, row });

    assert_no_load(&open_cmds);
    assert!(app.overlays.context_menu.is_some());
    assert_eq!(
        app.queue.len(),
        5,
        "opening the menu must not remove the row"
    );

    // Queue menus contain "Play now" followed by "Remove".
    let cmds = choose_context_menu_item(&mut app, 1);
    assert_no_load(&cmds);
    assert_eq!(app.queue.len(), 4);
    assert!(
        app.queue.ordered().iter().all(|s| s.video_id != "id2"),
        "the menu-selected track should be gone from the queue"
    );
}

#[test]
fn clicking_outside_the_queue_window_closes_it() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    render_app(&app); // publishes queue_popup.rect
    // Top-left corner is well outside the centered popup.
    let cmds = app.update(Msg::MouseClick {
        col: 1,
        row: 1,
        multi: false,
    });
    assert!(!app.queue_popup.open);
    assert!(cmds.is_empty());
}

#[test]
fn drag_selects_a_range_then_delete_removes_all_of_it() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c')))); // open, cursor = anchor = 0
    render_app(&app);
    let (start_col, start_row) = button_center(&app, MouseTarget::QueueRow(0));
    app.update(Msg::MouseClick {
        col: start_col,
        row: start_row,
        multi: false,
    });
    // Drag down to row 2: anchor stays at 0, so the selection spans 0..=2.
    let (col, row) = button_center(&app, MouseTarget::QueueRow(2));
    app.update(Msg::MouseDrag { col, row });
    assert_eq!(app.queue_popup.anchor, 0);
    assert_eq!(app.queue_popup.cursor, 2);
    // The Delete key removes the whole selected range at once.
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert_eq!(app.queue.len(), 2);
    let ids: Vec<&str> = app
        .queue
        .ordered()
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["id3", "id4"]);
}

#[test]
fn queue_delete_current_range_loads_next_after_removed_range() {
    let mut app = app_playing(5, 1);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    app.queue_popup.anchor = 1;
    app.queue_popup.cursor = 3;

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));

    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "id4");
    assert_loads_video(&cmds, "id4");
    let ids: Vec<&str> = app
        .queue
        .ordered()
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["id0", "id4"]);
}

#[test]
fn queue_delete_current_tail_range_stops_when_no_next_exists() {
    let mut app = app_playing(3, 1);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    app.queue_popup.anchor = 1;
    app.queue_popup.cursor = 2;

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));

    assert_eq!(app.queue.len(), 1);
    assert_eq!(current(&app), "id0");
    assert_no_load(&cmds);
    assert!(has_stop(&cmds), "mpv should stop the deleted current track");
    assert_eq!(app.prefetch.loaded_video_id, None);
    assert!(app.playback.paused);
}

#[test]
fn queue_delete_current_tail_wraps_under_repeat_all() {
    let mut app = app_playing(3, 2);
    app.queue.repeat = crate::queue::Repeat::All;
    app.update(Msg::Key(key(KeyCode::Char('c'))));

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));

    assert_eq!(app.queue.len(), 2);
    assert_eq!(current(&app), "id0");
    assert_loads_video(&cmds, "id0");
}

#[test]
fn queue_drag_after_release_starts_a_fresh_range() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::QueueRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::QueueRow(2));
    let (c4, r4) = button_center(&app, MouseTarget::QueueRow(4));

    app.update(Msg::MouseClick {
        col: c0,
        row: r0,
        multi: false,
    });
    assert_eq!((app.queue_popup.cursor, app.queue_popup.anchor), (0, 0));
    app.update(Msg::MouseLeftUp);

    app.update(Msg::MouseDrag { col: c2, row: r2 });
    assert_eq!(
        (app.queue_popup.cursor, app.queue_popup.anchor),
        (2, 2),
        "a new drag after release must not extend the old row-0 selection"
    );
    app.update(Msg::MouseDrag { col: c4, row: r4 });
    assert_eq!((app.queue_popup.cursor, app.queue_popup.anchor), (4, 2));
}

#[test]
fn enter_on_queue_drag_range_starts_at_range_beginning() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c'))));
    app.queue_popup.anchor = 1;
    app.queue_popup.cursor = 3;

    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));

    assert!(!app.queue_popup.open);
    assert_eq!(app.queue.cursor_pos(), 1);
    assert_eq!(current(&app), "id1");
    assert_loads_video(&cmds, "id1");
}
