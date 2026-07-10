use super::*;

#[test]
fn shift_down_extends_library_selection_without_collapsing_anchor() {
    let mut app = app_with_three_favorites();

    // Shift+Down twice grows the range from the anchor at row 0 to the cursor at row 2.
    app.update(Msg::Key(shift(KeyCode::Down)));
    app.update(Msg::Key(shift(KeyCode::Down)));
    assert_eq!(app.library_ui.anchor, 0, "anchor stays put while extending");
    assert_eq!(app.library_ui.selected, 2, "cursor advanced");

    // The whole span feeds the existing bulk consumers.
    let selected = app.selected_library_songs();
    let ids: Vec<&str> = selected.iter().map(|s| s.video_id.as_str()).collect();
    assert_eq!(ids, ["f0", "f1", "f2"]);
}

#[test]
fn plain_down_collapses_library_selection_onto_the_cursor() {
    let mut app = app_with_three_favorites();
    // Build a range with Shift, then a plain Down collapses it (anchor follows the cursor).
    app.update(Msg::Key(shift(KeyCode::Down)));
    assert_eq!((app.library_ui.anchor, app.library_ui.selected), (0, 1));
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!(app.library_ui.selected, 2);
    assert_eq!(app.library_ui.anchor, 2, "plain nav collapses the range");
}

#[test]
fn shift_home_and_end_extend_library_selection_to_the_edges() {
    let mut app = app_with_three_favorites();
    app.library_ui.selected = 1;
    app.library_ui.anchor = 1;

    app.update(Msg::Key(shift(KeyCode::End)));
    assert_eq!(app.library_ui.anchor, 1);
    assert_eq!(
        app.library_ui.selected, 2,
        "Shift+End extends to the bottom"
    );

    app.library_ui.selected = 1;
    app.library_ui.anchor = 1;
    app.update(Msg::Key(shift(KeyCode::Home)));
    assert_eq!(app.library_ui.anchor, 1);
    assert_eq!(app.library_ui.selected, 0, "Shift+Home extends to the top");
}

#[test]
fn shift_down_extends_queue_selection_and_delete_removes_the_range() {
    let mut app = app_playing(5, 0);
    app.update(Msg::Key(key(KeyCode::Char('c')))); // open the queue window at the playing row
    assert!(app.queue_popup.open);
    assert_eq!((app.queue_popup.anchor, app.queue_popup.cursor), (0, 0));

    app.update(Msg::Key(shift(KeyCode::Down)));
    app.update(Msg::Key(shift(KeyCode::Down)));
    assert_eq!(app.queue_popup.anchor, 0, "queue anchor stays put");
    assert_eq!(app.queue_popup.cursor, 2, "queue cursor advanced");

    // Delete acts on the inclusive anchor..=cursor span (rows 0..=2), leaving 2 of 5.
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert_eq!(app.queue.len(), 2);
}

#[test]
fn nav_step_for_hold_ramps_up_with_hold_duration() {
    use std::time::Duration;
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(0)), 1);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(399)), 1);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(400)), 2);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(999)), 2);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(1000)), 4);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(1999)), 4);
    assert_eq!(super::nav_step_for_hold(Duration::from_millis(2000)), 8);
    assert_eq!(super::nav_step_for_hold(Duration::from_secs(10)), 8);
}

#[test]
fn nav_repeat_step_accelerates_a_hold_but_resets_on_gap_or_direction_change() {
    use std::time::{Duration, Instant};
    let mut app = App::new(100);
    let t0 = Instant::now();
    // A steady cadence under NAV_REPEAT_GAP keeps one hold alive; cumulative hold time
    // (not per-event spacing) drives the ramp.
    let cadence = Duration::from_millis(150);

    // Fresh press moves one row.
    assert_eq!(app.nav_repeat_step_at(t0, Action::MoveDown), 1);
    let mut now = t0;
    let mut steps = Vec::new();
    for _ in 0..8 {
        now += cadence;
        steps.push(app.nav_repeat_step_at(now, Action::MoveDown));
    }
    // Hold grows 150,300,450,600,750,900,1050,1200 ms → ramps as it crosses 400ms and 1s.
    assert_eq!(steps, vec![1, 1, 2, 2, 2, 2, 4, 4]);

    // A gap longer than NAV_REPEAT_GAP restarts the hold (a deliberate fresh tap).
    let after_gap = now + super::NAV_REPEAT_GAP + Duration::from_millis(1);
    assert_eq!(app.nav_repeat_step_at(after_gap, Action::MoveDown), 1);

    // Switching direction also restarts, even within the window.
    assert_eq!(
        app.nav_repeat_step_at(after_gap + Duration::from_millis(10), Action::MoveDown),
        1
    );
    assert_eq!(
        app.nav_repeat_step_at(after_gap + Duration::from_millis(20), Action::MoveUp),
        1,
        "direction change resets the streak"
    );
}

#[test]
fn search_row_context_menu_can_add_it_to_the_queue() {
    // Something is already playing, so opening the menu and enqueueing must not interrupt it.
    let mut app = app_playing(2, 0);
    let playing = app.prefetch.loaded_video_id.clone();
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![
            Song::remote("r0", "R0", "A", "3:00"),
            Song::remote("r1", "R1", "B", "3:00"),
        ],
    });
    app.search.focus = SearchFocus::Results;

    // Render so the per-row hit rects are published, then right-click row 1.
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let row1 = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::ListRow(1))
        .map(|b| b.rect)
        .expect("a rendered search row rect");

    let open_cmds = app.update(Msg::MouseRightClick {
        col: row1.x,
        row: row1.y,
    });
    assert_no_load(&open_cmds);
    assert!(app.overlays.context_menu.is_some());
    assert_eq!(app.search.selected, 1);
    assert!(!app.queue.video_ids().any(|v| v == "r1"));

    // Search menus contain "Play now" followed by "Add to queue".
    let cmds = choose_context_menu_item(&mut app, 1);
    assert_no_load(&cmds);
    assert_eq!(app.prefetch.loaded_video_id, playing);
    assert!(app.queue.video_ids().any(|v| v == "r1"));
    assert_eq!(app.status.kind, StatusKind::Info);
}

#[test]
fn library_delete_cell_context_menu_can_add_that_row_to_the_queue() {
    let mut app = app_playing(1, 0);
    let playing = app.prefetch.loaded_video_id.clone();
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    app.library.favorites = vec![Song::remote("fav0", "Favorite Zero", "A", "3:00")];

    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::LibraryDel(0));
    let open_cmds = app.update(Msg::MouseRightClick { col, row });

    assert_eq!(app.library_ui.selected, 0);
    assert_no_load(&open_cmds);
    assert!(app.overlays.context_menu.is_some());
    assert!(!app.queue.video_ids().any(|v| v == "fav0"));

    // Library-song menus contain "Play now" followed by "Add to queue".
    let cmds = choose_context_menu_item(&mut app, 1);
    assert_no_load(&cmds);
    assert_eq!(app.prefetch.loaded_video_id, playing);
    assert!(app.queue.video_ids().any(|v| v == "fav0"));
    assert!(
        app.library.favorites.iter().any(|s| s.video_id == "fav0"),
        "choosing enqueue from the delete-cell menu must not delete the favorite"
    );
}

#[test]
fn q_closes_search_results_without_quitting_app() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(1);
    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn ctrl_q_quits_from_search_results() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.focus = SearchFocus::Results;
    app.search.results = songs(1);
    app.update(Msg::Key(ctrl(KeyCode::Char('q'))));
    assert!(app.should_quit);
}

#[test]
fn ctrl_h_goes_home_from_search_input_without_typing() {
    let mut app = App::new(100);
    app.update(Msg::Key(key(KeyCode::Char('s'))));
    app.search.input = "abc".to_owned();
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.search.input, "abc");
    assert!(!app.should_quit);
}

#[test]
fn korean_ctrl_h_goes_home_from_library() {
    let mut app = app_playing(1, 0);
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.mode, Mode::Library);
    app.update(Msg::Key(ctrl(KeyCode::Char('ㅗ'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.should_quit);
}

#[test]
fn ctrl_h_goes_home_from_help_overlay() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.overlays.help_visible = true;
    app.update(Msg::Key(ctrl(KeyCode::Char('h'))));
    assert_eq!(app.mode, Mode::Player);
    assert!(!app.overlays.help_visible);
    assert!(!app.should_quit);
}

// --- M4: queue / shuffle / repeat / auto-advance ------------------------
