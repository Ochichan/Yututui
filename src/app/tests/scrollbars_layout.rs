use super::*;

#[test]
fn library_scrollbar_shows_only_when_the_list_overflows() {
    // The thumb glyph appears on the right border column (79 in an 80-wide frame); the
    // plain vertical border is a different glyph, so its presence proves the scrollbar.
    let has_thumb = |app: &App| -> bool {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::ui::render(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..20).any(|y| buf.cell((79, y)).is_some_and(|c| c.symbol() == "█"))
    };

    let mut overflow = App::new(100);
    for i in 0..40 {
        overflow.library.record_play(&Song::remote(
            format!("id{i}"),
            format!("t{i}"),
            "x",
            "0:10",
        ));
    }
    overflow.mode = Mode::Library;
    overflow.library_ui.tab = LibraryTab::History;
    assert!(
        has_thumb(&overflow),
        "a long list should show a scrollbar thumb"
    );

    let mut fits = App::new(100);
    fits.library
        .record_play(&Song::remote("a", "ta", "x", "0:10"));
    fits.library
        .record_play(&Song::remote("b", "tb", "x", "0:10"));
    fits.mode = Mode::Library;
    fits.library_ui.tab = LibraryTab::History;
    assert!(
        !has_thumb(&fits),
        "a short list should not show a scrollbar"
    );
}

#[test]
fn library_scrollbar_thumb_tracks_the_actual_page_offset() {
    let mut app = App::new(100);
    for i in 0..40 {
        app.library.record_play(&Song::remote(
            format!("id{i}"),
            format!("t{i}"),
            "x",
            "0:10",
        ));
    }
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    app.bridges
        .library_scroll
        .wheel(false, 999, app.library_len());
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    let buf = terminal.backend().buffer();
    assert_eq!(
        buf.cell((79, 17)).map(|c| c.symbol()),
        Some("█"),
        "at the final page the scrollbar thumb should touch the list bottom"
    );
}

#[test]
fn dragging_library_scrollbar_moves_the_viewport() {
    let mut app = App::new(100);
    for i in 0..40 {
        app.library.record_play(&Song::remote(
            format!("id{i}"),
            format!("t{i}"),
            "x",
            "0:10",
        ));
    }
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::History;

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let bar = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::Scrollbar(ScrollSurface::Library))
        .map(|b| b.rect)
        .expect("rendered library scrollbar rect");

    app.update(Msg::MouseClick {
        col: bar.x,
        row: bar.y,
    });
    app.update(Msg::MouseDrag {
        col: bar.x,
        row: bar.y + bar.height - 1,
    });

    assert_eq!(
        app.bridges.library_scroll.offset(),
        app.library_len() - app.bridges.library_scroll.viewport()
    );
}

#[test]
fn dragging_search_scrollbar_moves_the_viewport() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(40);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let bar = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::Scrollbar(ScrollSurface::Search))
        .map(|b| b.rect)
        .expect("rendered search scrollbar rect");

    app.update(Msg::MouseClick {
        col: bar.x,
        row: bar.y,
    });
    app.update(Msg::MouseDrag {
        col: bar.x,
        row: bar.y + bar.height - 1,
    });

    assert_eq!(
        app.bridges.search_scroll.offset(),
        app.search.results.len() - app.bridges.search_scroll.viewport()
    );
}

#[test]
fn scrolled_search_rows_keep_absolute_hit_indices() {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.search.results = songs(40);
    app.search.selected = 0;

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    app.bridges
        .search_scroll
        .wheel(false, 8, app.search.results.len());
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    let offset = app.bridges.search_scroll.offset();
    assert!(offset > 0);
    let mut visible_rows: Vec<_> = app
        .hits
        .regions()
        .iter()
        .filter_map(|region| match region.target {
            MouseTarget::ListRow(index) => Some(index),
            _ => None,
        })
        .collect();
    visible_rows.sort_unstable();
    visible_rows.dedup();
    assert_eq!(visible_rows.first().copied(), Some(offset));
    assert!(!visible_rows.contains(&0));
}

#[test]
fn filtered_playlist_bottom_window_keeps_absolute_hit_indices() {
    let mut app = App::new(100);
    let playlist_id = app.playlists.create("Window").unwrap();
    for i in 0..40 {
        let title = if i % 2 == 0 {
            format!("Keep {i}")
        } else {
            format!("Skip {i}")
        };
        app.playlists.add(
            &playlist_id,
            Song::remote(format!("id{i}"), title, "Artist", "0:10"),
        );
    }
    app.mode = Mode::Library;
    app.library_ui.tab = LibraryTab::Playlists;
    app.library_ui.open_playlist = Some(playlist_id);
    app.library_ui.filter_query = "keep".to_owned();

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let len = app.library_len();
    app.bridges.library_scroll.wheel(false, 999, len);
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();

    let offset = app.bridges.library_scroll.offset();
    assert!(offset > 0);
    let mut visible_rows: Vec<_> = app
        .hits
        .regions()
        .iter()
        .filter_map(|region| match region.target {
            MouseTarget::ListRow(index) => Some(index),
            _ => None,
        })
        .collect();
    visible_rows.sort_unstable();
    visible_rows.dedup();
    assert_eq!(visible_rows.first().copied(), Some(offset));
    assert_eq!(visible_rows.last().copied(), Some(len - 1));
}

#[test]
fn ai_suggestions_scrollbar_renders_inside_borderless_list() {
    let mut app = App::new(100);
    app.mode = Mode::Ai;
    app.ai.messages.push(AiMessage {
        role: AiRole::User,
        text: "hide onboarding art".to_owned(),
    });
    app.ai.suggestions = songs(20);

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| crate::ui::render(f, &app)).unwrap();
    let bar = app
        .hits
        .regions()
        .iter()
        .find(|b| b.target == MouseTarget::Scrollbar(ScrollSurface::AiSuggestions))
        .map(|b| b.rect)
        .expect("rendered DJ Gem suggestions scrollbar rect");
    let buf = terminal.backend().buffer();

    assert!(bar.x < 80, "scrollbar should not be registered off-screen");
    assert!(
        (bar.y..bar.y + bar.height).any(|y| {
            buf.cell((bar.x, y))
                .is_some_and(|c| matches!(c.symbol(), "█" | "│"))
        }),
        "scrollbar should be drawn at its registered hit rect"
    );
}

#[test]
fn help_button_is_centered_on_footer_screens() {
    let _guard = crate::i18n::lock_for_test();
    let inner = Rect {
        x: 1,
        y: 1,
        width: 78,
        height: 18,
    };

    let player = App::new(100);
    assert_centered_in(rendered_help_cluster(&player, 80, 20), inner);

    let mut search = App::new(100);
    search.mode = Mode::Search;
    assert_centered_in(rendered_help_cluster(&search, 80, 20), inner);

    let mut library = App::new(100);
    library.mode = Mode::Library;
    assert_centered_in(rendered_help_cluster(&library, 80, 20), inner);

    let mut ai = App::new(100);
    ai.mode = Mode::Ai;
    assert_centered_in(rendered_help_cluster(&ai, 80, 20), inner);
}
