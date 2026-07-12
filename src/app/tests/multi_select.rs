use super::*;

// --- Library: Ctrl/Cmd+click toggle ---------------------------------------

fn app_with_four_favorites() -> App {
    let mut app = App::new(100);
    app.library.favorites = vec![
        Song::remote("a", "ta", "x", "0:10"),
        Song::remote("b", "tb", "x", "0:10"),
        Song::remote("c", "tc", "x", "0:10"),
        Song::remote("d", "td", "x", "0:10"),
    ];
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    app
}

#[test]
fn ctrl_click_toggles_discontiguous_library_rows() {
    let mut app = app_with_four_favorites();
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    // Plain click selects row 0; Ctrl+click on row 2 adds it to the selection.
    app.update(Msg::MouseClick {
        col: c0,
        row: r0,
        multi: false,
    });
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: true,
    });
    assert_eq!(
        app.library_ui.selected, 2,
        "cursor follows the toggle click"
    );
    let ids: Vec<String> = app
        .selected_library_songs()
        .iter()
        .map(|s| s.video_id.clone())
        .collect();
    assert_eq!(ids, ["a", "c"], "rows 0 and 2 are selected, row 1 is not");

    // A second Ctrl+click on the same row deselects it again.
    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: true,
    });
    let ids: Vec<String> = app
        .selected_library_songs()
        .iter()
        .map(|s| s.video_id.clone())
        .collect();
    assert_eq!(ids, ["a"], "toggling row 2 off leaves only row 0");
}

#[test]
fn plain_click_collapses_the_picked_selection() {
    let mut app = app_with_four_favorites();
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));
    let (c1, r1) = button_center(&app, MouseTarget::ListRow(1));

    app.update(Msg::MouseClick {
        col: c0,
        row: r0,
        multi: false,
    });
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: true,
    });
    assert_eq!(app.selected_library_songs().len(), 2);

    app.update(Msg::MouseClick {
        col: c1,
        row: r1,
        multi: false,
    });
    assert!(
        app.library_ui.picked.is_empty(),
        "plain click drops the picks"
    );
    let ids: Vec<String> = app
        .selected_library_songs()
        .iter()
        .map(|s| s.video_id.clone())
        .collect();
    assert_eq!(ids, ["b"], "selection collapsed onto the clicked row");
}

#[test]
fn drag_after_a_toggle_returns_to_range_mode() {
    let mut app = app_with_four_favorites();
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));
    let (c3, r3) = button_center(&app, MouseTarget::ListRow(3));

    app.update(Msg::MouseClick {
        col: c0,
        row: r0,
        multi: false,
    });
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: true,
    });
    assert_eq!(app.selected_library_songs().len(), 2);

    // Dragging onto another row is a range gesture: picks yield to a fresh range
    // anchored at the first dragged-over row (a toggle click arms no drag session).
    app.update(Msg::MouseDrag { col: c3, row: r3 });
    assert!(app.library_ui.picked.is_empty(), "drag clears the picks");
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (3, 3));
    let ids: Vec<String> = app
        .selected_library_songs()
        .iter()
        .map(|s| s.video_id.clone())
        .collect();
    assert_eq!(ids, ["d"], "the fresh drag range replaces the picks");
}

#[test]
fn delete_removes_exactly_the_picked_rows() {
    let mut app = app_with_four_favorites();
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    app.update(Msg::MouseClick {
        col: c0,
        row: r0,
        multi: false,
    });
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: true,
    });

    app.update(Msg::Key(key(KeyCode::Delete)));
    let remaining: Vec<&str> = app
        .library
        .favorites
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(
        remaining,
        ["b", "d"],
        "only the two picked rows were removed"
    );
    assert!(
        app.library_ui.picked.is_empty(),
        "the consumed picks are dropped"
    );
}

// --- Search results: drag range + toggle parity ---------------------------

fn app_with_search_results(n: usize) -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(n);
    app.search.focus = SearchFocus::Results;
    app
}

fn app_with_one_search_pick_off_cursor() -> App {
    let mut app = app_with_search_results(3);
    // Toggle row 2 on and back off. The remaining picked row is 0, while the cursor stays
    // where the last toggle happened (row 2).
    app.search_toggle_pick(2);
    app.search_toggle_pick(2);
    assert_eq!(app.search_selection_indices(), vec![0]);
    assert_eq!(app.search.selected, 2);
    app
}

#[test]
fn search_click_anchors_and_drag_extends_the_range() {
    let mut app = app_with_search_results(4);
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    app.update(Msg::MouseClick {
        col: c0,
        row: r0,
        multi: false,
    });
    assert_eq!((app.search.selected, app.search.anchor), (0, 0));
    app.update(Msg::MouseDrag { col: c2, row: r2 });
    assert_eq!(
        (app.search.selected, app.search.anchor),
        (2, 0),
        "drag extends while the anchor stays put"
    );
    assert_eq!(app.selected_search_songs().len(), 3);

    // Release, then a fresh drag starts its own range (no stale anchor).
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseDrag { col: c2, row: r2 });
    assert_eq!((app.search.selected, app.search.anchor), (2, 2));
}

#[test]
fn search_shift_nav_extends_like_the_library() {
    let mut app = app_with_search_results(4);
    app.update(Msg::Key(shift(KeyCode::Down)));
    app.update(Msg::Key(shift(KeyCode::Down)));
    assert_eq!((app.search.selected, app.search.anchor), (2, 0));
    assert_eq!(app.selected_search_songs().len(), 3);

    // Plain nav collapses the range onto the cursor.
    app.update(Msg::Key(key(KeyCode::Down)));
    assert_eq!((app.search.selected, app.search.anchor), (3, 3));
    assert_eq!(app.selected_search_songs().len(), 1);
}

#[test]
fn search_ctrl_click_toggles_and_enter_plays_the_selection() {
    let mut app = app_with_search_results(4);
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    app.update(Msg::MouseClick {
        col: c0,
        row: r0,
        multi: false,
    });
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: true,
    });
    let ids: Vec<String> = app
        .selected_search_songs()
        .iter()
        .map(|s| s.video_id.clone())
        .collect();
    assert_eq!(ids.len(), 2, "rows 0 and 2 are selected");

    // Enter plays the whole multi-selection (the Library's Enter semantics).
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert_eq!(app.queue.len(), 2, "both selected results were queued");
    let queued: Vec<String> = app
        .queue
        .ordered_iter()
        .map(|s| s.video_id.clone())
        .collect();
    assert_eq!(queued, ids);
}

#[test]
fn every_single_search_action_uses_the_lone_picked_row_instead_of_the_cursor() {
    let mut app = app_with_one_search_pick_off_cursor();
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert_loads_video(&cmds, "id0");

    let mut app = app_with_one_search_pick_off_cursor();
    app.update(Msg::Key(key(KeyCode::Char('\\'))));
    assert_eq!(current(&app), "id0", "enqueue must use the picked row");

    let mut app = app_with_one_search_pick_off_cursor();
    let cmds = app.update(Msg::Key(key(KeyCode::Char('d'))));
    assert!(
        matches!(cmds.as_slice(), [Cmd::Download(song)] if song.video_id == "id0"),
        "download must use the picked row"
    );

    let mut app = app_with_one_search_pick_off_cursor();
    app.update(Msg::Key(key(KeyCode::Char('p'))));
    assert_eq!(
        app.playlist_picker.as_ref().expect("playlist picker").songs[0].video_id,
        "id0"
    );

    let mut app = app_with_one_search_pick_off_cursor();
    app.update(Msg::Key(key(KeyCode::Char('f'))));
    assert!(
        app.library.is_favorite("id0"),
        "favorite must use the picked row"
    );
    assert!(
        !app.library.is_favorite("id2"),
        "cursor row must remain untouched"
    );
}

#[test]
fn fresh_search_results_drop_the_old_selection() {
    let mut app = app_with_search_results(4);
    render_app(&app);
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));
    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: true,
    });
    assert!(!app.search.picked.is_empty());

    app.search.input = "abc".to_owned();
    let _ = app.submit_search_query();
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "abc".to_owned(),
        songs: songs(2),
        source: SearchSource::Youtube,
        timed_out: false,
    });
    assert!(app.search.picked.is_empty(), "new results clear the picks");
    assert_eq!((app.search.selected, app.search.anchor), (0, 0));
}

#[test]
fn mouse_cheatsheet_documents_the_new_multi_select_gestures() {
    let mut app = App::new(100);
    app.overlays.mouse_help_visible = true;
    let buf = render_app_buffer(&app, 220, 160);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    // The platform-native modifier label (⌘ on macOS, Ctrl elsewhere) appears for the
    // toggle rows added to the Search and Library groups.
    let modk = if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    };
    let label = format!("{modk} + click");
    assert!(
        text.matches(&label).count() >= 2,
        "expected the '{label}' row in both the Search and Library groups"
    );
    // The Search group gained a drag-select row (Library already had one).
    assert!(
        text.matches("Drag rows").count() >= 2,
        "expected 'Drag rows' in the Search group as well as the Library group"
    );
}

#[test]
fn search_right_click_inside_the_selection_targets_all_picked_rows() {
    let mut app = app_with_search_results(4);
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    app.update(Msg::MouseClick {
        col: c0,
        row: r0,
        multi: false,
    });
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: true,
    });
    app.update(Msg::MouseRightClick { col: c2, row: r2 });

    let menu = app
        .overlays
        .context_menu
        .as_ref()
        .expect("a context menu over the picked rows");
    assert_eq!(
        menu.target_count(),
        2,
        "the menu acts on both picked rows, not just the clicked one"
    );
}

#[test]
fn search_context_menu_preserves_mixed_selection_when_target_filters_playlists() {
    let mut app = app_with_search_results(0);
    app.search.results = vec![
        Song::remote("id0", "t0", "a", "0:10"),
        Song::remote("ytpl:PLabcdefgh1234", "Mix", "a", "10 tracks"),
        Song::remote("id2", "t2", "a", "0:10"),
    ];
    render_app(&app);
    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c1, r1) = button_center(&app, MouseTarget::ListRow(1));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    app.update(Msg::MouseClick {
        col: c0,
        row: r0,
        multi: false,
    });
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseClick {
        col: c1,
        row: r1,
        multi: true,
    });
    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: true,
    });
    assert_eq!(app.search_selection_indices(), vec![0, 1, 2]);

    app.update(Msg::MouseRightClick { col: c2, row: r2 });
    let menu = app
        .overlays
        .context_menu
        .as_ref()
        .expect("a context menu over the picked rows");
    assert_eq!(menu.target_count(), 2, "playlist rows are not bulk targets");
    assert_eq!(
        app.search_selection_indices(),
        vec![0, 1, 2],
        "opening the menu must not shrink the visible selection"
    );
}

#[test]
fn search_plain_click_inside_multi_selection_collapses_immediately() {
    let mut app = app_with_search_results(4);
    app.search_toggle_pick(2);
    render_app(&app);
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: false,
    });
    assert_eq!(app.search_selection_indices(), vec![2]);
    assert!(app.search.picked.is_empty());
}

#[test]
fn search_matching_double_click_restores_and_plays_pre_click_selection() {
    let mut app = app_with_search_results(4);
    app.search_toggle_pick(2);
    render_app(&app);
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: false,
    });
    assert_eq!(app.search_selection_indices(), vec![2]);
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseDoubleClick { col: c2, row: r2 });
    let queued: Vec<&str> = app
        .queue
        .ordered_iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(queued, ["id0", "id2"]);
}

#[test]
fn search_double_click_outside_selection_plays_only_clicked_row() {
    let mut app = app_with_search_results(4);
    app.search_toggle_pick(2);
    render_app(&app);
    let (c1, r1) = button_center(&app, MouseTarget::ListRow(1));

    app.update(Msg::MouseClick {
        col: c1,
        row: r1,
        multi: false,
    });
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseDoubleClick { col: c1, row: r1 });
    let queued: Vec<&str> = app
        .queue
        .ordered_iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(queued, ["id1"]);
}

#[test]
fn library_plain_click_inside_multi_selection_collapses_immediately() {
    let mut app = app_with_four_favorites();
    app.library_toggle_pick(2);
    render_app(&app);
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: false,
    });
    assert_eq!(app.library_selection_indices(), vec![2]);
    assert!(app.library_ui.picked.is_empty());
}

#[test]
fn library_matching_double_click_restores_and_plays_pre_click_selection() {
    let mut app = app_with_four_favorites();
    app.library_toggle_pick(2);
    render_app(&app);
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));

    app.update(Msg::MouseClick {
        col: c2,
        row: r2,
        multi: false,
    });
    assert_eq!(app.library_selection_indices(), vec![2]);
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseDoubleClick { col: c2, row: r2 });
    let queued: Vec<&str> = app
        .queue
        .ordered_iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(queued, ["a", "c"]);
}

#[test]
fn library_double_click_outside_selection_plays_only_clicked_row() {
    let mut app = app_with_four_favorites();
    app.library_toggle_pick(2);
    render_app(&app);
    let (c1, r1) = button_center(&app, MouseTarget::ListRow(1));

    app.update(Msg::MouseClick {
        col: c1,
        row: r1,
        multi: false,
    });
    app.update(Msg::MouseLeftUp);
    app.update(Msg::MouseDoubleClick { col: c1, row: r1 });
    let queued: Vec<&str> = app
        .queue
        .ordered_iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(queued, ["b"]);
}
