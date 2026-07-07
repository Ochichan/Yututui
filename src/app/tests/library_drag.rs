use super::*;

#[test]
fn library_mouse_drag_selects_range_then_delete_removes_it() {
    let mut app = App::new(100);
    app.library
        .toggle_favorite(&Song::remote("a", "ta", "x", "0:10"));
    app.library
        .toggle_favorite(&Song::remote("b", "tb", "x", "0:10"));
    app.library
        .toggle_favorite(&Song::remote("c", "tc", "x", "0:10")); // [c, b, a]
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;

    // Render so the per-row hit rects are published.
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let row_rect = |i: usize| {
        app.hits
            .regions()
            .iter()
            .find(|b| b.target == MouseTarget::ListRow(i))
            .map(|b| b.rect)
            .expect("a rendered library row rect")
    };
    let r0 = row_rect(0);
    let r2 = row_rect(2);

    // Click row 0 (anchors the range), then drag onto row 2 (extends it).
    app.update(Msg::MouseClick {
        col: r0.x,
        row: r0.y,
    });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (0, 0));
    app.update(Msg::MouseDrag {
        col: r2.x,
        row: r2.y,
    });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (2, 0));

    // Delete removes the whole selected 0..=2 range.
    app.update(Msg::Key(key(KeyCode::Delete)));
    assert!(app.library.favorites.is_empty());
}

#[test]
fn library_tabs_mark_favorite_rows_with_a_heart() {
    let song = Song::remote("shared", "Shared Song", "Artist", "0:10");
    let mut app = App::new(100);
    app.library.record_play(&song);
    app.library.toggle_favorite(&song);
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;

    let buf = render_app_buffer(&app, 80, 24);
    let text: String = buf
        .content()
        .iter()
        .map(|c| c.symbol().to_owned())
        .collect();

    assert!(
        text.contains("♥ Shared Song"),
        "favorite rows should show a heart in non-Favorites library tabs too"
    );
}

#[test]
fn library_drag_after_release_starts_a_fresh_range() {
    let mut app = App::new(100);
    app.library.favorites = vec![
        Song::remote("a", "ta", "x", "0:10"),
        Song::remote("b", "tb", "x", "0:10"),
        Song::remote("c", "tc", "x", "0:10"),
        Song::remote("d", "td", "x", "0:10"),
    ];
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Favorites;
    render_app(&app);

    let (c0, r0) = button_center(&app, MouseTarget::ListRow(0));
    let (c2, r2) = button_center(&app, MouseTarget::ListRow(2));
    let (c3, r3) = button_center(&app, MouseTarget::ListRow(3));

    app.update(Msg::MouseClick { col: c0, row: r0 });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (0, 0));
    app.update(Msg::MouseLeftUp);

    app.update(Msg::MouseDrag { col: c2, row: r2 });
    assert_eq!(
        (app.library_ui.selected, app.library_ui.anchor),
        (2, 2),
        "a new drag after release must not extend the old row-0 selection"
    );
    app.update(Msg::MouseDrag { col: c3, row: r3 });
    assert_eq!((app.library_ui.selected, app.library_ui.anchor), (3, 2));
}

// --- M6: lyrics ---------------------------------------------------------
