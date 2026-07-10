use super::*;

#[test]
fn slash_is_bound_to_library_filter() {
    let app = App::new(100);
    let slash = Chord::new(KeyCode::Char('/'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::Library, slash),
        Some(Action::LibraryFilter)
    );
}

#[test]
fn slash_opens_filter_and_typing_narrows_the_list() {
    let mut app = app_with_favorites(vec![
        fsong("a", "Lovely", "Billie Eilish"),
        fsong("b", "Bad Guy", "Billie Eilish"),
        fsong("c", "Anti-Hero", "Taylor Swift"),
    ]);
    assert_eq!(app.library_len(), 3);

    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.library_ui.filter_editing);
    for c in "lov".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.library_ui.filter_query, "lov");
    assert_eq!(row_ids(&app), vec!["a"]);
    assert_eq!(app.library_len(), 1);
    assert_eq!(app.library_ui.selected, 0);
}

#[test]
fn filter_matches_title_or_artist_case_insensitively() {
    let mut app = app_with_favorites(vec![
        fsong("a", "Lovely", "Billie Eilish"),
        fsong("b", "Bad Guy", "Billie Eilish"),
        fsong("c", "Anti-Hero", "Taylor Swift"),
    ]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "BILLIE".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    // Matched by artist, ignoring case.
    assert_eq!(row_ids(&app), vec!["a", "b"]);
}

#[test]
fn empty_filter_query_is_passthrough() {
    let mut app = app_with_favorites(vec![fsong("a", "One", "x"), fsong("b", "Two", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/')))); // open, but type nothing
    assert!(app.library_ui.filter_editing);
    assert_eq!(app.library_len(), 2); // full list while query is empty
}

#[test]
fn enter_commits_filter_and_esc_clears_it() {
    let mut app = app_with_favorites(vec![
        fsong("a", "Lovely", "Billie Eilish"),
        fsong("b", "Bad Guy", "Billie Eilish"),
    ]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Char('o'))));

    // Enter commits: stop editing, keep the filter applied.
    app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.library_ui.filter_editing);
    assert_eq!(app.library_ui.filter_query, "lo");
    assert_eq!(app.library_len(), 1);
    assert_eq!(app.mode, Mode::Library);

    // Esc with a committed filter clears it (full list back) and stays in the Library.
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(app.library_ui.filter_query.is_empty());
    assert_eq!(app.mode, Mode::Library);
    assert_eq!(app.library_len(), 2);
}

// --- search results-filter popup (`/`) -----------------------------------------

/// Search screen with three results loaded; results arrival focuses the list.

#[test]
fn slash_is_bound_to_search_filter() {
    let app = App::new(100);
    let slash = Chord::new(KeyCode::Char('/'), KeyModifiers::empty());
    assert_eq!(
        app.keymap.action(KeyContext::SearchResults, slash),
        Some(Action::SearchFilter)
    );
}

#[test]
fn slash_opens_the_filter_popup_and_typing_narrows_it() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.search_filter.open);
    // The popup opens fresh: empty query shows the full result set.
    assert_eq!(filter_row_ids(&app), vec!["a", "b", "c"]);

    // Case-insensitive title/artist matching, exactly like the Library filter.
    for c in "BILLIE".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.search_filter.query, "BILLIE");
    assert_eq!(filter_row_ids(&app), vec!["a", "b"]);
    assert_eq!(app.search_filter.cursor, 0);

    // A query with no matches leaves the popup open (still refining); Enter is a no-op.
    for c in " zzz".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert!(filter_row_ids(&app).is_empty());
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(cmds.is_empty());
    assert!(app.search_filter.open);
    assert_eq!(app.mode, Mode::Search);

    // Backspace re-widens.
    for _ in 0..4 {
        app.update(Msg::Key(key(KeyCode::Backspace)));
    }
    assert_eq!(filter_row_ids(&app), vec!["a", "b"]);
}

#[test]
fn filter_popup_does_not_capture_slash_in_the_input_focus() {
    let mut app = app_with_search_results();
    app.search.focus = SearchFocus::Input;
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    // While the query box is focused, `/` must type, not open the popup.
    assert!(!app.search_filter.open);
    assert_eq!(app.search.input, "/");
}

#[test]
fn filter_popup_enter_plays_the_highlighted_match_and_closes() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "anti".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(filter_row_ids(&app), vec!["c"]);
    let cmds = app.update(Msg::Key(key(KeyCode::Enter)));
    assert!(!app.search_filter.open);
    // The main cursor lands on the played row (original index), and playback starts.
    assert_eq!(app.search.selected, 2);
    assert_eq!(app.mode, Mode::Player);
    assert_loads_video(&cmds, "c");
}

#[test]
fn filter_popup_esc_closes_without_acting() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "anti".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    let cmds = app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(cmds.is_empty());
    assert!(!app.search_filter.open);
    assert!(app.search_filter.query.is_empty());
    assert_eq!(app.mode, Mode::Search);
    assert_eq!(app.queue.len(), 0);
}

#[test]
fn fresh_results_close_the_filter_popup() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.search_filter.open);
    // A search that was already in flight lands: the rows the popup indexed are gone.
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "y".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![fsong("z", "Zeta", "Nobody")],
    });
    assert!(!app.search_filter.open);
}

#[test]
fn filter_button_opens_the_popup_and_clicking_rows_selects_and_plays() {
    let mut app = app_with_search_results();
    // The `⌕ Filter` button is registered on render and opens the popup.
    click_target(&mut app, MouseTarget::SearchFilterOpen);
    assert!(app.search_filter.open);
    // Single-click a popup row: the popup cursor moves there, nothing plays yet.
    let cmds = click_target(&mut app, MouseTarget::SearchFilterRow(1));
    assert!(cmds.is_empty());
    assert!(app.search_filter.open);
    assert_eq!(app.search_filter.cursor, 1);
    // Double-click plays it and closes the popup, landing the main cursor on the row.
    let cmds = double_click_target(&mut app, MouseTarget::SearchFilterRow(1));
    assert!(!app.search_filter.open);
    assert_eq!(app.search.selected, 1);
    assert_eq!(app.mode, Mode::Player);
    assert_loads_video(&cmds, "b");
}

#[test]
fn filter_popup_right_click_menu_can_enqueue_without_closing() {
    // Something is already playing, so opening the menu and enqueueing must not interrupt it.
    let mut app = app_playing(2, 0);
    app.mode = Mode::Search;
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs: vec![fsong("r0", "R0", "A"), fsong("r1", "R1", "B")],
    });
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    assert!(app.search_filter.open);
    render_app(&app);
    let (col, row) = button_center(&app, MouseTarget::SearchFilterRow(1));
    let before = app.queue.len();
    let open_cmds = app.update(Msg::MouseRightClick { col, row });
    assert!(open_cmds.is_empty());
    assert!(app.overlays.context_menu.is_some());
    assert_eq!(
        app.queue.len(),
        before,
        "opening the menu is side-effect free"
    );

    // Search menus contain "Play now" followed by "Add to queue".
    let cmds = choose_context_menu_item(&mut app, 1);
    assert_no_load(&cmds);
    assert_eq!(app.queue.len(), before + 1);
    assert!(app.search_filter.open);
    assert_eq!(app.search_filter.cursor, 1);
}

#[test]
fn filter_popup_click_outside_closes_it() {
    let mut app = app_with_search_results();
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    render_app(&app);
    // (0, 0) is the screen corner, well outside the centered popup.
    app.update(Msg::MouseClick { col: 0, row: 0 });
    assert!(!app.search_filter.open);
    assert_eq!(app.mode, Mode::Search);
}

#[test]
fn filter_popup_wheel_scrolls_its_own_viewport() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    let songs = (0..40)
        .map(|i| fsong(&format!("id{i}"), &format!("Song {i}"), "A"))
        .collect();
    app.update(Msg::SearchResults {
        request_id: app.search.request_id,
        query: "x".to_owned(),
        source: SearchSource::Youtube,
        timed_out: false,
        songs,
    });
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    render_app(&app); // records the popup viewport + rect
    let (col, row) = app
        .search_filter
        .rect
        .get()
        .map(|r| (r.x + r.width / 2, r.y + r.height / 2))
        .expect("an open popup rect");
    app.update(Msg::MouseScroll {
        up: false,
        col,
        row,
        ctrl: false,
    });
    assert!(app.search_filter.scroll.offset() > 0);
    // The main results viewport underneath did not move.
    assert_eq!(app.bridges.search_scroll.offset(), 0);
}

#[test]
fn esc_while_editing_cancels_the_filter() {
    let mut app = app_with_favorites(vec![fsong("a", "One", "x"), fsong("b", "Two", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('o'))));
    app.update(Msg::Key(key(KeyCode::Char('n')))); // "on" matches "One" only
    assert_eq!(app.library_len(), 1);
    app.update(Msg::Key(key(KeyCode::Esc)));
    assert!(!app.library_ui.filter_editing);
    assert!(app.library_ui.filter_query.is_empty());
    assert_eq!(app.library_len(), 2);
}

#[test]
fn delete_under_filter_removes_the_matched_track_by_identity() {
    // Raw order [k0, m, k1]: a naive index-based delete of filtered-row-0 would wrongly
    // remove k0; the identity-based delete must remove only the matched track m.
    let mut app = app_with_favorites(vec![
        fsong("k0", "Alpha", "x"),
        fsong("m", "Zebra", "x"),
        fsong("k1", "Beta", "x"),
    ]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "zeb".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.library_len(), 1);
    app.update(Msg::Key(key(KeyCode::Enter))); // commit; selection on filtered row 0

    let cmds = app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Cmd::Persist(PersistCmd::Library)))
    );
    let ids: Vec<&str> = app
        .library
        .favorites
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["k0", "k1"]);
}

#[test]
fn filter_clears_when_switching_tabs() {
    let mut app = app_with_favorites(vec![fsong("a", "Lovely", "x"), fsong("b", "Other", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Enter))); // commit so Tab is dispatched as a tab switch
    assert!(!app.library_ui.filter_query.is_empty());

    app.update(Msg::Key(key(KeyCode::Tab))); // Favorites -> History, drops the filter
    assert_eq!(app.library_ui.tab, LibraryTab::History);
    assert!(app.library_ui.filter_query.is_empty());
    assert!(!app.library_ui.filter_editing);
}

#[test]
fn delete_after_navigating_then_filtering_removes_only_the_cursor_row() {
    // Regression: moving the cursor down plants the multi-select anchor at that row. Opening
    // the filter and typing must snap the anchor back to 0, or a single Delete on the
    // committed filter would silently wipe the whole anchor..=cursor range of matches.
    let mut app = app_with_favorites(vec![
        fsong("a", "Alpha", "x"),
        fsong("b", "Beta", "x"),
        fsong("c", "Carol", "x"),
        fsong("d", "Delta", "x"),
    ]);
    app.update(Msg::Key(key(KeyCode::Down)));
    app.update(Msg::Key(key(KeyCode::Down))); // cursor + anchor now at raw row 2
    assert_eq!(app.library_ui.anchor, 2);

    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('a')))); // matches all four (every title has 'a')
    assert_eq!(app.library_len(), 4);
    assert_eq!(app.library_ui.selected, 0);
    assert_eq!(app.library_ui.anchor, 0); // the fix: anchor followed the cursor to 0
    app.update(Msg::Key(key(KeyCode::Enter))); // commit

    app.update(Msg::Key(key(KeyCode::Delete)));
    let ids: Vec<&str> = app
        .library
        .favorites
        .iter()
        .map(|s| s.video_id.as_str())
        .collect();
    assert_eq!(ids, vec!["b", "c", "d"]); // only the cursor row (Alpha) went, not a range
}

#[test]
fn playing_the_whole_tab_under_a_filter_queues_only_the_filtered_subset() {
    // WYSIWYG: with a filter applied, "play whole tab" (P) plays — and queues — only the
    // visible matches, consistent with delete/favorite/download all operating on the filtered set.
    let mut app = app_with_favorites(vec![
        fsong("a", "Lovely", "Billie Eilish"),
        fsong("b", "Bad Guy", "Billie Eilish"),
        fsong("c", "Anti-Hero", "Taylor Swift"),
    ]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    for c in "billie".chars() {
        app.update(Msg::Key(key(KeyCode::Char(c))));
    }
    app.update(Msg::Key(key(KeyCode::Enter))); // commit; two matches
    assert_eq!(app.library_len(), 2);

    app.update(Msg::Key(key(KeyCode::Char('a')))); // play the whole filtered tab
    assert_eq!(app.mode, Mode::Player);
    let ids: Vec<&str> = app.queue.video_ids().collect();
    assert_eq!(ids, vec!["a", "b"]); // only the matched tracks were queued
}

#[test]
fn q_closes_the_library_in_one_press_even_with_a_filter_applied() {
    // `q` is the one-press "close Library"; clearing the filter is Esc's lighter gesture.
    let mut app = app_with_favorites(vec![fsong("a", "Lovely", "x"), fsong("b", "Other", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Enter))); // commit the filter
    assert!(!app.library_ui.filter_query.is_empty());

    app.update(Msg::Key(key(KeyCode::Char('q'))));
    assert_eq!(app.mode, Mode::Player); // closed on the first press
}

#[test]
fn reopening_the_library_clears_a_leftover_filter() {
    // Leaving with a filter applied is harmless: the next Library open starts clean.
    let mut app = app_with_favorites(vec![fsong("a", "Lovely", "x"), fsong("b", "Other", "x")]);
    app.update(Msg::Key(key(KeyCode::Char('/'))));
    app.update(Msg::Key(key(KeyCode::Char('l'))));
    app.update(Msg::Key(key(KeyCode::Enter))); // commit
    app.update(Msg::Key(key(KeyCode::Char('q')))); // leave; filter state lingers in Player
    assert!(!app.library_ui.filter_query.is_empty());

    app.update(Msg::Key(key(KeyCode::Char('l')))); // re-open the Library
    assert_eq!(app.mode, Mode::Library);
    assert!(app.library_ui.filter_query.is_empty()); // OpenLibrary reset it
    assert_eq!(app.library_ui.tab, LibraryTab::Favorites); // tab persists, only the filter resets
    assert_eq!(app.library_len(), 2);
}

// --- copy link (`y`): only internet-sourced tracks expose a URL --------------
